//! Plan insights about parallel plans: skewed threads, and parallelism a statement didn't need or couldn't get.

use super::*;

/// What a spilling operator wrote to disk, when the plan says: tempdb pages (SQL Server), the sort's or hash
/// aggregate's own disk use, or the temporary blocks the operator itself wrote (PostgreSQL with BUFFERS).
pub(super) fn spilled_bytes(n: &PlanNode) -> Option<f64> {
    let disk_kb = n
        .facts()
        .sort_space_kb
        .filter(|_| n.facts().sort_on_disk == Some(true))
        .or_else(|| n.facts().disk_usage_kb.filter(|kb| *kb > 0.0));
    disk_kb.map(|kb| kb * 1024.0).or_else(|| crate::plan_facts::spill_pages(n).map(|p| p * 8192.0))
}

pub(super) fn spill(cx: &Cx) -> Vec<Draft> {
    let mut out = Vec::new();
    for n in cx.nodes.iter().filter(|n| n.facts().exchange_spill == Some(true)) {
        let mut d = Draft::new("spill", InsightArea::Parallelism, cx.share(n), vec![n.id]);
        d.title = match spilled_bytes(n) {
            Some(b) => format!(
                "The {} exchange wrote {} to tempdb to keep its threads from waiting on each other",
                n.op,
                format::bytes(b)
            ),
            None => format!("The {} exchange wrote to tempdb to keep its threads from waiting on each other", n.op),
        };
        d.detail = "SQL Server spills an exchange's buffers to tempdb when its parallel threads would otherwise wait on each other for good, usually at an exchange that keeps rows in order (merging streams) fed unevenly. It isn't a memory grant problem.".into();
        d.fix = "Remove the need for order at that exchange (an ORDER BY or merge join above it that an index or a hash join could serve), and check whether one value holds most of the rows; comparing a run with OPTION (MAXDOP 1) shows what parallelism costs here.".into();
        d.verify = "Run with an actual plan: no ExchangeSpillDetails on the Parallelism operator.".into();
        out.push(d);
    }
    // Underestimates by the rule behind the Misestimate warning: 30 rows where 2 were expected sized no grant.
    let misestimated: HashMap<u32, f64> = cx.plan.root.misestimates().into_iter().collect();
    for n in cx.nodes.iter().filter(|n| n.warnings.contains(&PlanWarning::Spill)) {
        let mut d = Draft::new("spill", InsightArea::Memory, cx.share(n), vec![n.id]);
        // The plan's costs were estimated before the spill happened; its size is the evidence of what it cost.
        match spilled_bytes(n) {
            Some(bytes) if bytes < SPILL_MATERIAL_BYTES => d.cap = Severity::Low,
            Some(_) => d.floor = Severity::Medium,
            None => {}
        }
        let underestimated = {
            let mut below = Vec::new();
            n.walk(&mut below);
            below.iter().any(|b| misestimated.get(&b.id).is_some_and(|m| *m > 0.0))
        };
        match cx.engine() {
            // EXPLAIN QUERY PLAN reports no spills.
            Engine::Sqlite => continue,
            Engine::Postgres => {
                let disk_kb = n.facts().sort_space_kb.filter(|_| n.facts().sort_on_disk == Some(true));
                let batches = n.facts().hash_batches.map(f64::from).filter(|b| *b > 1.0);
                let agg_batches = n.facts().hash_aggregate_batches.map(f64::from).filter(|b| *b > 1.0);
                d.title = match (disk_kb, batches, agg_batches, spilled_bytes(n)) {
                    (Some(kb), _, _, _) => format!(
                        "The sort writes {} to disk because it didn't fit in work_mem",
                        format::bytes(kb * 1024.0)
                    ),
                    (None, Some(b), _, _) => format!(
                        "The hash split its input into {} batches on disk because it didn't fit in work_mem",
                        format::count(b)
                    ),
                    (None, None, _, Some(bytes)) => format!(
                        "{} writes {} of temporary files because it didn't fit in work_mem",
                        n.op,
                        format::bytes(bytes)
                    ),
                    (None, None, Some(b), None) => format!(
                        "{} split its groups into {} batches on disk because they didn't fit in work_mem",
                        n.op,
                        format::count(b)
                    ),
                    (None, None, None, None) => {
                        format!("{} wrote temporary files because it didn't fit in work_mem", n.op)
                    }
                };
                d.detail = format!(
                    "Disk is many times slower than memory for sorting and hashing.{}",
                    if underestimated {
                        " The rows feeding it were badly underestimated, so it was planned with too little memory."
                    } else {
                        ""
                    }
                );
                d.fix = "Give this workload more memory for sorts and hashes, per role or per session rather than server-wide, or add an index that returns rows already ordered so the sort disappears.".into();
                d.fix_sql = Some("-- Try it in your session first:\nSET work_mem = '64MB';\n-- Then for the application's role:\n-- ALTER ROLE app_user SET work_mem = '64MB';".into());
                d.verify =
                    "Explain analyze again: Sort Method is quicksort/top-N (in memory) and Hash Batches is 1.".into();
            }
            // MariaDB's ANALYZE counts a sort's merge passes: it didn't fit in sort_buffer_size.
            Engine::Mysql => {
                let passes = n.facts().sort_merge_passes.map(f64::from).unwrap_or(1.0);
                d.title = format!(
                    "The sort needed {} through temporary files because it didn't fit in sort_buffer_size",
                    plural(passes, "merge pass", "merge passes")
                );
                d.detail = format!(
                    "Each merge pass writes the sorted runs to a temporary file and reads them back.{}",
                    if underestimated { " The rows feeding it were badly underestimated." } else { "" }
                );
                d.fix = "Raise sort_buffer_size for the session that runs it rather than server-wide (it's allocated per sort), or add an index that returns rows already in order so the sort disappears.".into();
                d.fix_sql =
                    Some("-- Try it in your session first:\nSET SESSION sort_buffer_size = 8 * 1024 * 1024;".into());
                d.verify = "Run ANALYZE again: r_sort_passes is 0.".into();
            }
            Engine::Mssql => {
                d.title = match crate::plan_facts::spill_pages(n) {
                    Some(pages) => {
                        format!("{} ran out of memory and wrote {} to tempdb", n.op, format::bytes(pages * 8192.0))
                    }
                    None => format!("{} ran out of memory and wrote to tempdb", n.op),
                };
                d.detail = format!(
                    "The memory granted to {} was too small for the rows it got, so part of the work went to disk in tempdb.{}",
                    n.op,
                    if underestimated {
                        " The rows feeding it were badly underestimated; the grant was sized for the estimate."
                    } else {
                        ""
                    }
                );
                d.fix = "Fix the row estimate first (update statistics on the tables feeding it); an index that returns rows in the needed order removes the sort altogether. SQL Server 2019+ also grows the grant on later runs (memory grant feedback, compatibility level 150).".into();
                d.verify = "Run with an actual plan: no spill warning on that operator.".into();
            }
        }
        out.push(d);
    }
    out
}

pub(super) fn parallelism_blocked(cx: &Cx) -> Vec<Draft> {
    // The plan doesn't carry the server's cost threshold for parallelism; the snapshot's settings may
    // ([`COST_THRESHOLD`]). Without them, below its default (5) no setting would make the plan parallel, and only above
    // the 50 it's commonly raised to does running serially say much.
    const DEFAULT_THRESHOLD: f64 = 5.0;
    const COMMON_THRESHOLD: f64 = 50.0;
    let facts = cx.plan.facts();
    let Some(reason) = facts.parallelism_blocker.as_deref() else { return Vec::new() };
    let cost = cx.plan.total_cost;
    let threshold = facts.cost_threshold_for_parallelism;
    // A threshold still at its default is one Culprit's own advice raises to the common 50
    // (config:cost-threshold-for-parallelism): a plan under that would stay on one CPU once it is raised, so removing
    // what keeps it serial now is no advice to give beside it. One answer for both findings.
    let bar = match threshold {
        Some(t) if t <= DEFAULT_THRESHOLD => COMMON_THRESHOLD,
        Some(t) => t,
        None => DEFAULT_THRESHOLD,
    };
    if reason == "TSQLUserDefinedFunctionsNotParallelizable" || cost < bar {
        return Vec::new();
    }
    // Which MAXDOP is 1: the statement's own hint, or the database's or the server's setting as the snapshot read them.
    let statement = cx.statement();
    let hinted = statement.match_indices("maxdop").any(|(at, _)| {
        statement[at + 6..]
            .trim_start()
            .strip_prefix('1')
            .is_some_and(|rest| !rest.starts_with(|c: char| c.is_ascii_digit()))
    });
    let (database, server) = (facts.database_maxdop, facts.server_maxdop);
    let maxdop_one = if hinted {
        "its statement says OPTION (MAXDOP 1)".to_string()
    } else if database == Some(1) {
        "the database's MAXDOP setting is 1".to_string()
    } else if database.is_none_or(|d| d == 0) && server == Some(1) {
        "the server's max degree of parallelism is 1".to_string()
    } else if database.is_some() && server.is_some() {
        "MAXDOP is 1, though neither its statement nor the database's or server's setting says so now (a Query Store \
         hint, a Resource Governor workload group, or a setting changed since it was compiled)"
            .to_string()
    } else {
        "MAXDOP is 1 (a query hint, or the database or server setting)".to_string()
    };
    let plain = match reason {
        "MaxDOPSetToOne" => maxdop_one.as_str(),
        "EstimatedDOPIsOne" => "only one CPU is available to it",
        "NoParallelPlansInDesktopOrExpressEdition" => "this SQL Server edition doesn't run parallel plans",
        "CouldNotGenerateValidParallelPlan" => "SQL Server couldn't build a valid parallel plan for it",
        "NonParallelizableIntrinsicFunction" => "the query calls a built-in function that can't run in parallel",
        "TableVariableTransactionsDoNotSupportParallelNestedTransaction" => "it writes to a table variable",
        "NoParallelForMemoryOptimizedTables" => "it touches memory-optimized tables",
        "UpdatingWritebackVariable" => "it assigns to a variable",
        "CLRUserDefinedFunctionRequiresDataAccess" => "it calls a CLR function that reads data",
        _ => "",
    };
    let mut d = Draft::new("parallelism-blocked", InsightArea::Parallelism, 0.1, vec![cx.plan.root.id]);
    d.fixed_weight = true;
    d.subtree = true;
    d.cap = if cost >= COMMON_THRESHOLD { Severity::Medium } else { Severity::Low };
    // SQL Server's own reason on a plan expensive enough to go parallel is Medium; a cheap one's is Low.
    d.floor = d.cap;
    d.title = if plain.is_empty() {
        format!("This plan runs on one CPU: SQL Server reports {reason}")
    } else {
        format!("This plan runs on one CPU because {plain}")
    };
    d.detail = match threshold {
        Some(t) if t <= DEFAULT_THRESHOLD => format!(
            "Its estimated cost is {cost:.0}, above this server's cost threshold for parallelism ({t:.0}) and the \
             {COMMON_THRESHOLD:.0} it's commonly raised to, so SQL Server would consider a parallel plan for it either \
             way; the whole run happens on a single core instead."
        ),
        Some(t) => format!(
            "Its estimated cost is {cost:.0}, above this server's cost threshold for parallelism ({t:.0}), so SQL Server \
             would consider a parallel plan for it; the whole run happens on a single core instead."
        ),
        None => format!(
            "Its estimated cost is {cost:.0}. SQL Server considers a parallel plan above the server's cost threshold for parallelism (5 by default, often raised to 50){}; the whole run happens on a single core instead.",
            if cost >= COMMON_THRESHOLD {
                ", which this plan passes either way"
            } else {
                ", so whether it would split depends on that setting"
            }
        ),
    };
    d.fix = match reason {
        "MaxDOPSetToOne" if hinted => {
            "Remove OPTION (MAXDOP 1) from the statement, unless something needs it to run on one CPU.".into()
        }
        "MaxDOPSetToOne" if database == Some(1) => {
            "Raise the database's MAXDOP (ALTER DATABASE SCOPED CONFIGURATION SET MAXDOP), or give this statement \
             OPTION (MAXDOP n) where it should run in parallel."
                .into()
        }
        "MaxDOPSetToOne" if server == Some(1) => {
            "Raise the server's max degree of parallelism, or set the database's MAXDOP (ALTER DATABASE SCOPED \
             CONFIGURATION SET MAXDOP), which overrides it for this database."
                .into()
        }
        "MaxDOPSetToOne" if database.is_some() && server.is_some() => {
            "Look for what sets it: a Query Store hint on this query (sys.query_store_query_hints), or the workload \
             group its session runs in (MAX_DOP)."
                .into()
        }
        "MaxDOPSetToOne" => "Remove OPTION (MAXDOP 1) if the query has it; otherwise check MAXDOP for the database (ALTER DATABASE SCOPED CONFIGURATION) and the server.".into(),
        _ => "Remove what prevents parallelism if the query allows it; otherwise make the single-threaded run cheaper (indexes, fewer rows).".into(),
    };
    d.verify = "Explain again: no NonParallelPlanReason, and Parallelism operators appear.".into();
    vec![d]
}

pub(super) fn parallel_skew(cx: &Cx) -> Vec<Draft> {
    let mut out = Vec::new();
    for n in &cx.nodes {
        let Some((workers, total, max)) = crate::plan_facts::parallel_distribution(n, cx.engine()) else {
            continue;
        };
        let avg = total / workers as f64;
        if total < 10_000.0 || avg <= 0.0 || max < 3.0 * avg {
            continue;
        }
        // Skew costs time only in proportion to the work involved, and a TOP makes uneven threads normal.
        let has_top = cx.nodes.iter().any(|x| x.op == "Top" || x.op == "Limit");
        let mut d = Draft::new("parallel-skew", InsightArea::Parallelism, cx.share(n) * 0.3, vec![n.id]);
        d.cap = if has_top || total < 100_000.0 { Severity::Low } else { Severity::Medium };
        let who = if cx.engine() == Engine::Postgres { "process" } else { "thread" };
        d.title = format!("One {who} of {} handled {} of the rows in {}", workers, format::percent(max / total), n.op);
        d.detail = format!("Parallel work is only as fast as its busiest {who}; the others finished early and waited.");
        d.fix = "Uneven work usually means one key value holds most of the rows (check the distribution of the join or filter column) or a parallel scan over a small range; update statistics and consider filtering that value separately.".into();
        d.verify = format!("Run with actual rows: each {who} handles a similar share.");
        out.push(d);
    }
    out.sort_by(|a, b| b.impact.total_cmp(&a.impact));
    out.into_iter().take(1).collect()
}
