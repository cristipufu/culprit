//! Plan insights about how a statement is written: predicates an index can't use, conversions, joins, loops and
//! functions the plan runs row by row.

use super::*;

pub(super) fn implicit_conversion(cx: &Cx) -> Vec<Draft> {
    if cx.engine() != Engine::Mssql {
        return Vec::new();
    }
    // Only the warnings about a converted column the table still has (see `current_conversions`); older scans flagged
    // every conversion, and a plan captured before a column changed type still shows its old conversion.
    let plan_lines: Vec<String> = cx
        .prop("PlanAffectingConvert")
        .map(|v| v.lines().filter(|l| !current_conversions(cx.plan, l).is_empty()).map(str::to_string).collect())
        .unwrap_or_default();
    let node = cx
        .nodes
        .iter()
        // A converted column is the problem; a converted parameter, variable or literal costs nothing.
        .filter(|n| !current_conversions(cx.plan, &conditions(n)).is_empty())
        .max_by(|a, b| a.self_cost.total_cmp(&b.self_cost))
        .copied();
    if plan_lines.is_empty() && node.is_none() {
        return Vec::new();
    }
    let seek_line = plan_lines.iter().find(|l| l.starts_with("Seek Plan"));
    let text =
        seek_line.cloned().or_else(|| plan_lines.first().cloned()).or_else(|| node.map(conditions)).unwrap_or_default();
    // What the conversion costs is SQL Server's own verdict, never inferred from the plan's shape: a scan may have no
    // index on the column to seek, or be the right plan for a small table. "Seek Plan" means it kept SQL Server from
    // seeking an index; "Cardinality Estimate" that it swayed the row estimates; no warning, that it changed nothing.
    // SQL Server words it "may affect": an operator that seeks all the same (a range through the conversion) wasn't
    // stopped.
    let blocks_seek = seek_line.is_some() && node.is_none_or(|n| n.kind != NodeKind::Seek);
    let converted = current_conversions(cx.plan, &text).into_iter().next();
    let column = converted.as_ref().map(|(c, _)| c.clone()).or_else(|| convert_column(&text));
    let target = converted.map(|(_, t)| t);
    let impact = node.map_or(0.05, |n| cx.share(n));
    let mut d =
        Draft::new("implicit-conversion", InsightArea::Query, impact, node.map(|n| vec![n.id]).unwrap_or_default());
    let col = column.clone().unwrap_or_else(|| "a column".into());
    let to = target.as_deref().map(|t| format!(" to {t}")).unwrap_or_default();
    let expression = format::predicate(&convert_expression(&text));
    if blocks_seek {
        d.floor = Severity::Medium;
        d.title = format!("A type mismatch on {col} stops SQL Server from seeking an index");
        d.detail = format!(
            "The value it's compared with has a different type, so SQL Server converts {col}{to} on every row (`{expression}`) and, by its own warning, can't seek the index."
        );
    } else if !plan_lines.is_empty() {
        d.cap = Severity::Low;
        d.title = format!("A type mismatch on {col} turns row estimates into guesses");
        d.detail = format!(
            "SQL Server converts {col}{to} before comparing it (`{expression}`), which by its own warning only affects how many rows it expects here; an index can still be used."
        );
    } else {
        d.cap = Severity::Low;
        d.title = format!("A type mismatch on {col} adds a conversion to every row");
        d.detail = format!(
            "SQL Server converts {col}{to} on every row it reads to compare it with a value of that type (`{expression}`). It didn't warn that this changes the plan, so the cost is the conversion itself."
        );
    }
    d.fix = mismatch_fix(column.as_deref(), target.as_deref());
    d.verify = if blocks_seek {
        "Explain again: no CONVERT_IMPLICIT in the plan, and the scan became a seek.".into()
    } else {
        format!("Explain again: no CONVERT_IMPLICIT on {col} in the plan.")
    };
    vec![d]
}

/// `CONVERT_IMPLICIT(nvarchar(256),[db].[dbo].[Users].[Email],0)` → "Users.Email".
pub(super) fn convert_column(text: &str) -> Option<String> {
    let start = text.find("CONVERT_IMPLICIT(")? + "CONVERT_IMPLICIT(".len();
    let rest = &text[start..];
    let after_type = rest.find("),").map(|i| &rest[i + 2..]).or_else(|| rest.split_once(',').map(|(_, r)| r))?;
    let expr = after_type.split(',').next()?;
    let parts: Vec<String> = expr.split('.').map(clean_identifier).collect();
    match parts.as_slice() {
        [.., table, column] if !column.is_empty() && !column.starts_with('@') => Some(format!("{table}.{column}")),
        [column] if !column.is_empty() && !column.starts_with('@') => Some(column.clone()),
        _ => None,
    }
}

pub(super) fn convert_expression(text: &str) -> String {
    text.split_once(": ").map(|(_, e)| e.to_string()).unwrap_or_else(|| text.to_string())
}

pub(super) fn non_sargable(cx: &Cx) -> Vec<Draft> {
    let mut out = Vec::new();
    for n in cx.nodes.iter().filter(|n| index_could_pay(cx, n)) {
        let Some(filter) = n.predicate.as_deref() else { continue };
        let Some(Blocker::Function(func, col)) = blocker(cx, n, filter) else { continue };
        let table = what(n);
        let impact = cx.share(n).max(cx.subtree(n) * 0.5);
        let mut d = Draft::new("non-sargable", InsightArea::Query, impact, vec![n.id]);
        let cast = func.starts_with("::") || func == "CAST" || func == "CONVERT";
        d.title = if cast {
            format!("A cast on {col} keeps the database from using an index on {}", table_leaf(&table))
        } else {
            format!("{func}() around {col} keeps the database from using an index on {}", table_leaf(&table))
        };
        d.detail = format!(
            "Each row's {col} is computed before it can be compared (`{}`), so an index on {col} can't narrow the search: {} reads {} rows to keep {}. {} of this plan's work.",
            format::predicate(filter),
            n.op,
            format::count(rows_read(n)),
            format::percent(filter_keeps(cx, n).unwrap_or(1.0)),
            capitalize(&format::percent(impact))
        );
        let lower = func.to_ascii_lowercase();
        d.fix = if matches!(
            lower.as_str(),
            "year"
                | "month"
                | "day"
                | "date"
                | "datepart"
                | "date_trunc"
                | "date_part"
                | "extract"
                | "date_format"
                | "to_char"
        ) || lower == "::date"
        {
            format!(
                "Compare {col} itself with a range instead, e.g. `{col} >= '2026-03-01' AND {col} < '2026-04-01'`; then an index on {col} is used."
            )
        } else if matches!(lower.as_str(), "isnull" | "ifnull" | "coalesce" | "nvl") {
            format!("Write the condition on {col} directly, e.g. `({col} = @value OR {col} IS NULL)`.")
        } else if cast {
            format!("Compare {col} with a value of its own type, so nothing has to be converted per row.")
        } else {
            match cx.engine() {
                Engine::Postgres => format!(
                    "Store {col} in the form you search for, or index the expression itself, e.g. `CREATE INDEX ON {} (({}({col})))`.",
                    table_leaf(&table),
                    lower
                ),
                Engine::Mysql => format!(
                    "Store {col} in the form you search for, or add a functional index, e.g. `CREATE INDEX ... ON {} (({}({col})))` (MySQL 8.0.13+).",
                    table_leaf(&table),
                    lower
                ),
                Engine::Mssql => format!(
                    "Store {col} in the form you search for, or add a computed column with {func}({col}) and index it."
                ),
                Engine::Sqlite => format!(
                    "Store {col} in the form you search for, or index the expression itself, e.g. `CREATE INDEX ... ON {} ({}({col}))`.",
                    table_leaf(&table),
                    lower
                ),
            }
        };
        d.verify = format!("Explain again: {} is read with a seek or index condition on {col}.", table_leaf(&table));
        out.push(d);
    }
    out
}

pub(super) fn leading_wildcard(cx: &Cx) -> Vec<Draft> {
    let mut out = Vec::new();
    for n in cx.nodes.iter().filter(|n| index_could_pay(cx, n)) {
        let Some(filter) = n.predicate.as_deref() else { continue };
        let Some(Blocker::Wildcard(at)) = blocker(cx, n, filter) else { continue };
        let column = identifiers(&filter[..at]).into_iter().last().unwrap_or_else(|| "the column".into());
        let table = what(n);
        let impact = cx.share(n).max(cx.subtree(n) * 0.5);
        let mut d = Draft::new("leading-wildcard", InsightArea::Query, impact, vec![n.id]);
        d.title = format!("LIKE '%…' on {column} reads all of {}", table_leaf(&table));
        d.detail = format!(
            "A pattern that starts with a wildcard can't use an index, so {} checks every row's {column} ({} rows) to keep {} of them.",
            n.op,
            format::count(rows_read(n)),
            format::percent(filter_keeps(cx, n).unwrap_or(1.0))
        );
        d.fix = match cx.engine() {
            Engine::Postgres => format!(
                "A trigram index answers '%…%' searches: enable pg_trgm and index {column} with gin_trgm_ops. If you only need prefixes, drop the leading %."
            ),
            Engine::Mssql => format!(
                "Use full-text search for word searches, or drop the leading % if a prefix search is enough. For suffix searches, store the reversed value of {column} and index it."
            ),
            Engine::Mysql => {
                "Use a FULLTEXT index with MATCH … AGAINST for word searches, or drop the leading % if a prefix search is enough."
                    .to_string()
            }
            Engine::Sqlite => {
                "Use an FTS5 virtual table with MATCH for word searches, or drop the leading % if a prefix search is enough."
                    .to_string()
            }
        };
        d.fix_sql = (cx.engine() == Engine::Postgres).then(|| {
            format!(
                "CREATE EXTENSION IF NOT EXISTS pg_trgm;\nCREATE INDEX CONCURRENTLY ix_{}_{}_trgm\n    ON {} USING gin ({} gin_trgm_ops);",
                table_leaf(&table).to_ascii_lowercase(),
                column.to_ascii_lowercase(),
                qualified(Engine::Postgres, &table),
                quote(Engine::Postgres, &column)
            )
        });
        d.verify = format!(
            "Explain again: {} is read through an index (a Bitmap Index Scan on PostgreSQL).",
            table_leaf(&table)
        );
        out.push(d);
    }
    out
}

/// Joins that combine every row with every row: a NoJoinPredicate warning, or a Postgres/MySQL join with no condition.
pub(super) fn cross_joins(cx: &Cx) -> Vec<u32> {
    let mut ids: Vec<u32> =
        cx.nodes.iter().filter(|n| n.warnings.contains(&PlanWarning::NoJoinPredicate)).map(|n| n.id).collect();
    if ids.is_empty() && cx.plan.warnings.contains(&PlanWarning::NoJoinPredicate) {
        ids.extend(cx.nodes.iter().filter(|n| n.kind == NodeKind::Join).take(1).map(|n| n.id));
    }
    if cx.engine() == Engine::Postgres {
        for n in cx.nodes.iter().filter(|n| n.op == "Nested Loop" && n.children.len() == 2) {
            let inner = &n.children[1];
            // Anywhere under the inner side: a LATERAL subquery's condition sits below its Limit and Sort
            // (Limit → Sort → Index Scan with the Index Cond).
            fn conditioned(n: &PlanNode) -> bool {
                !conditions(n).is_empty() || n.props.contains_key("Join Filter") || n.children.iter().any(conditioned)
            }
            let no_condition = !conditioned(inner) && !n.props.contains_key("Join Filter");
            let product = n.children[0].est_rows * inner.est_rows.max(1.0);
            if no_condition && n.est_rows >= 0.5 * product && product >= 100.0 {
                ids.push(n.id);
            }
        }
    }
    if cx.engine() == Engine::Mysql {
        ids.extend(
            cx.nodes
                .iter()
                .filter(|n| {
                    (n.props.contains_key("using_join_buffer") && n.predicate.is_none() && n.seek_predicate.is_none())
                        || n.op.to_ascii_lowercase().contains("(no condition)")
                })
                .map(|n| n.id),
        );
    }
    ids
}

/// A NOT IN that checks the subquery row by row, and the operators that work exists for (what NOT EXISTS removes).
/// NOT IN that is one build and a probe per row isn't one: PostgreSQL's hashed SubPlan, MySQL's materialized subquery
/// and its antijoin (`Not exists`, MySQL 8.0.17+).
pub(super) fn not_in_work<'a>(cx: &Cx<'a>, n: &'a PlanNode) -> Option<Vec<&'a PlanNode>> {
    let statement = cx.statement();
    // The same plan shapes come from NOT EXISTS under an OR; when the statement is known, it must say NOT IN.
    let says_not_in = statement.is_empty() || statement.contains("not in");
    match cx.engine() {
        // EXPLAIN QUERY PLAN shows no per-row work to weigh.
        Engine::Sqlite => None,
        Engine::Mssql => {
            let spools: Vec<&PlanNode> = n
                .children
                .iter()
                .flat_map(|c| std::iter::once(c).chain(c.children.iter()))
                .filter(|c| c.op == "Row Count Spool")
                .collect();
            // NOT EXISTS gets the same spool under an anti semi join (a correlated condition the inner side can't use).
            (n.detail.as_deref().is_some_and(|d| d.contains("Anti Semi Join")) && !spools.is_empty() && says_not_in)
                .then_some(spools)
        }
        Engine::Postgres => {
            let l = n.predicate.as_deref()?.to_ascii_lowercase();
            let per_row = l.contains("not (") && l.contains("subplan") && !l.contains("hashed subplan");
            let subplans: Vec<&PlanNode> = n
                .children
                .iter()
                .filter(|c| c.props.get("Parent Relationship").is_some_and(|r| r == "SubPlan"))
                .collect();
            (per_row && says_not_in).then_some(subplans)
        }
        Engine::Mysql => {
            let l = n.predicate.as_deref()?.to_ascii_lowercase();
            let negated = l.contains("<not>(<in_optimizer>") || l.contains("not(<in_optimizer>");
            // `<exists>` is the subquery run for each row; `<materialize>` is built once and looked up.
            let per_row = negated && l.contains("<exists>") && !l.contains("<materialize>");
            (per_row && says_not_in && !is_true(n.props.get("not_exists"))).then(|| n.children.iter().collect())
        }
    }
}

pub(super) fn not_in(cx: &Cx) -> Vec<Draft> {
    let mut out = Vec::new();
    for n in &cx.nodes {
        let Some(work) = not_in_work(cx, n) else { continue };
        // NOT IN's own work: the operator testing each row and the NULL handling or subquery it repeats, not the
        // input it filters, which NOT EXISTS reads all the same.
        let impact = (cx.share(n) + work.iter().map(|w| cx.subtree(w)).sum::<f64>()).min(1.0);
        let mut nodes = vec![n.id];
        nodes.extend(work.iter().map(|w| w.id));
        let mut d = Draft::new("not-in", InsightArea::Query, impact, nodes);
        d.title = match cx.engine() {
            Engine::Mssql => {
                "NOT IN against a column that can be NULL makes SQL Server check for NULLs row by row".into()
            }
            Engine::Postgres => {
                "NOT IN (subquery) can't run as an anti-join, so PostgreSQL tests every row against the subquery".into()
            }
            Engine::Sqlite => {
                "NOT IN against a subquery whose column can be NULL returns no rows once it meets a NULL".into()
            }
            Engine::Mysql => "NOT IN (subquery) makes MySQL run the subquery once per row".into(),
        };
        d.detail = match cx.engine() {
            Engine::Mssql => format!("Because the subquery's column allows NULL, NOT IN must prove no NULL exists before it can return anything, which adds the Row Count Spool under {}.", n.op),
            Engine::Postgres => "If the subquery ever returns NULL, NOT IN returns nothing at all; that rule stops PostgreSQL from using a Hash Anti Join, and it expected the subquery's rows too many to hash in work_mem, so it searches them again for each row.".into(),
            Engine::Sqlite => "If the subquery ever returns NULL, NOT IN returns nothing at all.".into(),
            Engine::Mysql => "NOT IN has to treat a NULL from the subquery specially, and here MySQL could neither turn it into an antijoin nor materialize the subquery once, so it runs the subquery for each row.".into(),
        };
        d.fix = "Rewrite as NOT EXISTS, which means what you almost certainly intend and runs as an anti-join: `WHERE NOT EXISTS (SELECT 1 FROM orders o WHERE o.customer_id = c.id)`. Declaring the column NOT NULL also helps.".into();
        d.verify = match cx.engine() {
            Engine::Mssql => "Explain again: an Anti Semi Join without a Row Count Spool.".into(),
            Engine::Postgres => {
                "Explain again: a Hash Anti Join (or Nested Loop Anti Join) instead of a SubPlan filter.".into()
            }
            Engine::Sqlite => "Explain again: the subquery runs as a correlated NOT EXISTS.".into(),
            Engine::Mysql => "Explain again: an antijoin (Not exists) instead of a dependent subquery.".into(),
        };
        out.push(d);
    }
    out
}

pub(super) fn scalar_udf(cx: &Cx) -> Vec<Draft> {
    if cx.engine() != Engine::Mssql {
        return Vec::new();
    }
    let blocks_parallel = cx.prop("NonParallelPlanReason") == Some("TSQLUserDefinedFunctionsNotParallelizable");
    let mut out = Vec::new();
    // Time spent in scalar functions, measured apart from the plan's costs (SQL Server 2016 SP2+ actual plans).
    let measured = match (cx.prop("UdfElapsedMs"), elapsed_ms(cx.plan)) {
        (Some(udf), Some(total)) if total > 0.0 => udf.parse::<f64>().ok().map(|u| (u, (u / total).min(1.0))),
        _ => None,
    };
    for n in cx.nodes.iter().filter(|n| n.props.contains_key("UserDefinedFunctions")) {
        let names = n.props["UserDefinedFunctions"].clone();
        // In a seek's keys a function is worked out once per seek, not per row.
        let in_seek = is_true(n.props.get(crate::plan::UDF_IN_SEEK_ONLY));
        let calls = if in_seek { n.runs() } else { rows_out(n).max(n.children.first().map_or(0.0, rows_out)) };
        // The plan's costs leave the function's body out, so its own share says little: the measured time does, and
        // without it the number of calls is the evidence.
        let impact = measured.map_or_else(|| cx.share(n), |(_, share)| share);
        let mut d = Draft::new("scalar-udf", InsightArea::Query, impact, vec![n.id]);
        if measured.is_none() && calls >= MANY_ROWS {
            d.floor = Severity::Medium;
        }
        d.title = if in_seek {
            format!("{names} runs once for every seek (about {} calls per run)", format::count(calls))
        } else {
            format!("{names} runs once for every row (about {} calls per run)", format::count(calls))
        };
        d.detail = format!(
            "A scalar function runs like a small separate query for each row{}. {}",
            if blocks_parallel { ", and it keeps the whole plan on one CPU" } else { "" },
            match measured {
                Some((ms, share)) => format!(
                    "This run spent {} in it, {} of the statement's time.",
                    format::duration(ms),
                    format::percent(share)
                ),
                None => "Its cost doesn't show in the plan's numbers, which makes it easy to miss.".into(),
            }
        );
        d.fix = "Inline it: rewrite the function as an inline table-valued function or a plain expression in the query. SQL Server 2019+ inlines scalar functions automatically unless they use things like GETDATE(), table variables or loops.".into();
        d.verify = "Explain again: no UserDefinedFunction in the plan, and no NonParallelPlanReason.".into();
        out.push(d);
    }
    if out.is_empty() && blocks_parallel {
        let mut d = Draft::new("scalar-udf", InsightArea::Query, 0.1, vec![cx.plan.root.id]);
        d.fixed_weight = true;
        d.subtree = true;
        d.floor = Severity::Medium;
        d.title = "A scalar function keeps this plan on one CPU and runs once per row".into();
        d.detail = "SQL Server reports TSQLUserDefinedFunctionsNotParallelizable: a T-SQL scalar function in the query can't run in parallel, and it's called for each row.".into();
        d.fix = "Inline the function as an inline table-valued function or an expression; on SQL Server 2019+ remove what prevents automatic inlining (GETDATE(), table variables, loops).".into();
        d.verify = "Explain again: NonParallelPlanReason is gone.".into();
        out.push(d);
    }
    out
}

pub(super) fn table_variable(cx: &Cx) -> Vec<Draft> {
    if cx.engine() != Engine::Mssql {
        return Vec::new();
    }
    let mut out = Vec::new();
    for n in cx.nodes.iter().filter(|n| table_variable_of(n).is_some()) {
        let name = table_variable_of(n).unwrap_or_default();
        let actual = n.act_rows;
        if n.est_rows_total() > 1.5 && actual.is_none_or(|a| a < 10.0 * n.est_rows_for_runs()) {
            continue;
        }
        let mut d = Draft::new("table-variable", InsightArea::Query, cx.subtree(n), vec![n.id]);
        match actual {
            Some(a) if a >= 1_000.0 => {
                d.floor = Severity::Medium;
                d.title =
                    format!("SQL Server planned for 1 row in table variable {name}, which held {}", format::count(a));
            }
            _ => {
                d.cap = Severity::Low;
                d.title = format!("SQL Server plans for 1 row in table variable {name}");
            }
        }
        d.detail = "Table variables have no statistics, so everything joined to it is planned as if it held a single row. That's fine for a handful of rows and badly wrong for thousands.".into();
        d.fix = format!(
            "If {name} can hold more than a few hundred rows, use a temporary table (#{}) instead; it gets statistics. Compatibility level 150+ also defers its compilation so the first run's row count is used.",
            name.trim_start_matches('@')
        );
        d.verify = "Run with an actual plan: the estimate for that table matches its actual rows.".into();
        out.push(d);
    }
    out
}

pub(super) fn nested_loops(cx: &Cx) -> Vec<Draft> {
    let cross = cross_joins(cx);
    let not_in_ids: Vec<u32> = not_in(cx).iter().flat_map(|d| d.nodes.clone()).collect();
    let mut out = Vec::new();
    for n in
        cx.nodes.iter().filter(|n| n.kind == NodeKind::Join && n.op.to_ascii_lowercase().starts_with("nested loop"))
    {
        if n.children.len() < 2 {
            continue;
        }
        let outer = &n.children[0];
        let inner = &n.children[n.children.len() - 1];
        // Said better elsewhere: key lookups, NOT IN's row count spool, a cross join. A join buffer, MySQL's or
        // MariaDB's (`block-nl-join`), is no repeat: the table is read once per buffer fill, not once per row.
        let mut inner_nodes = Vec::new();
        inner.walk(&mut inner_nodes);
        let buffered =
            |x: &PlanNode| x.props.contains_key("using_join_buffer") || x.props.contains_key("join_buffer_join_type");
        if inner.kind == NodeKind::Lookup
            || inner_nodes.iter().any(|x| x.op == "Row Count Spool" || buffered(x))
            || cross.contains(&n.id)
            || inner_nodes.iter().any(|x| not_in_ids.contains(&x.id))
        {
            continue;
        }
        // An eager index spool is the eager-spool rule's: it is built once per run, not repeated per row.
        if inner_nodes.iter().any(|x| x.op == "Index Spool" && x.detail.as_deref() == Some("Eager Spool")) {
            continue;
        }
        // Times the inner side is asked for rows: once per outer row.
        let asked =
            if inner.act_executions.is_some() || inner.est_executions > 1.0 { inner.runs() } else { rows_out(outer) };
        // A cache in front of the inner side (PostgreSQL's Memoize and Materialize, SQL Server's lazy spools, MySQL's
        // materialized subqueries) replays rows it holds: its input runs again only on a cache miss or a rebind, which
        // is how often that input ran.
        let materializes = |x: &PlanNode| {
            matches!(x.op.as_str(), "Memoize" | "Table Spool" | "Index Spool" | "Query block" | "Materialized subquery")
                || x.op.starts_with("Materialize")
        };
        let cached_input = if materializes(inner) {
            inner.children.first()
        } else if inner.object.is_none() {
            inner.children.iter().find(|c| materializes(c))
        } else {
            None
        };
        let cache = materializes(inner) || cached_input.is_some();
        let reads = if cache { cached_input.map_or(0.0, |c| c.runs()) } else { asked };
        let outer_name = table_leaf(&first_object(outer).unwrap_or_else(|| outer.op.clone()));
        let inner_name = table_leaf(&first_object(inner).unwrap_or_else(|| inner.op.clone()));
        let hinted = {
            let s = cx.statement();
            s.contains("loop join") || s.contains("option (loop")
        };
        // The optimizer's choice stands unless the plan shows it was made on wrong numbers, or is forced, or each run
        // rereads much of a table: an accurate outer estimate means a loop was costed against a hash join and won.
        let expected = outer.est_rows_for_runs();
        let misjudged = outer.act_rows.is_some_and(|a| a >= MISESTIMATE_FACTOR * expected.max(1.0));
        let leaf = inner_nodes.iter().find(|x| x.object.is_some() && x.kind != NodeKind::Other).copied();
        let per_read = leaf.map(|l| rows_read(l) / l.runs().max(1.0));
        let table = leaf.and_then(|l| l.table_rows).filter(|t| *t > 0.0);
        let rereads = !cache
            && per_read.is_some_and(|per| {
                per >= 100.0
                    && match table {
                        Some(t) => per >= t * INDEX_KEEPS_AT_MOST,
                        None => leaf.is_some_and(|l| l.kind == NodeKind::Scan),
                    }
            });
        // Each outer row meets every row the cache replays, and the join tests its condition on every pair.
        let per_replay = rows_out(inner) / asked.max(1.0);
        let pairs = cache && n.predicate.is_some() && asked >= MANY_LOOPS && per_replay >= 100.0;
        let repeated = reads >= MANY_LOOPS && (hinted || misjudged || rereads);
        if !repeated && !pairs {
            continue;
        }
        let impact = (cx.subtree(inner) + cx.share(n)).min(1.0);
        let mut d = Draft::new("nested-loops", InsightArea::Query, impact, vec![n.id, inner.id]);
        d.cap = Severity::Medium;
        let mysql = cx.engine() == Engine::Mysql;
        d.title = if repeated {
            format!("Repeats its read of {inner_name} {} times, once per row of {outer_name}", format::count(reads))
        } else {
            format!(
                "Compares each of {} rows of {outer_name} with {} rows the {} replays ({} pairs)",
                format::count(asked),
                format::count(per_replay),
                inner.op,
                format::count(asked * per_replay)
            )
        };
        let forced = "The query forces a loop join, so the optimizer didn't weigh a hash or merge join.";
        d.detail = if hinted && repeated {
            forced.to_string()
        } else if misjudged && repeated {
            format!(
                "The optimizer expected {} rows from {outer_name} and chose a nested loop; it got {}. {}",
                format::count(expected),
                rows_text(rows_out(outer)),
                if mysql {
                    "MySQL joins through an index whenever one fits; the estimate decides the join order, so a wrong one can start from the bigger side."
                } else {
                    "At that size one pass with a hash join is usually much cheaper, and the optimizer would have weighed one."
                }
            )
        } else if repeated {
            format!(
                "Each run reads about {} rows of {inner_name}{}, so the loop reads it {} times over. {} of this plan's work.",
                format::count(per_read.unwrap_or(0.0)),
                table
                    .map(|t| format!(" ({} of the table)", format::percent((per_read.unwrap_or(0.0) / t).min(1.0))))
                    .unwrap_or_default(),
                format::count(reads),
                capitalize(&format::percent(impact))
            )
        } else {
            format!(
                "The {} reads {inner_name} once and replays its {} rows for each row of {outer_name}; the join then checks `{}` on every pair. {} of this plan's work.{}",
                inner.op,
                format::count(per_replay),
                format::predicate(n.predicate.as_deref().unwrap_or_default()),
                capitalize(&format::percent(impact)),
                if hinted { format!(" {forced}") } else { String::new() }
            )
        };
        d.fix = if hinted {
            "The query forces a loop join (LOOP JOIN / OPTION (LOOP JOIN)); remove the hint and let the optimizer choose.".into()
        } else if misjudged && repeated {
            if mysql {
                format!(
                    "Fix the estimate for {outer_name}: ANALYZE TABLE, and a histogram on its filtered columns, so MySQL plans the join for the real row count; filtering {outer_name} earlier means fewer rows reach the loop."
                )
            } else {
                format!(
                    "Fix the estimate for {outer_name}: update its statistics. With the right row count the optimizer can choose a hash join by itself."
                )
            }
        } else if repeated && leaf.is_some_and(|l| l.kind == NodeKind::Seek) {
            // It already seeks an index, one that narrows each run too little.
            format!(
                "Give {inner_name} an index that leads with the join column and the columns the inner side filters on, so each run reads only its matching rows; or filter {outer_name} earlier so fewer rows reach the loop."
            )
        } else if repeated {
            format!(
                "Index {inner_name} on the join column so each run reads only its matching rows, or filter {outer_name} earlier so fewer rows reach the loop."
            )
        } else {
            format!(
                "Give the join a condition it can seek or hash on: compare stored columns with = (no functions or computed values around them), indexed on the inner side, or filter {outer_name} earlier so fewer rows reach the loop."
            )
        };
        d.verify = if mysql {
            format!("Explain again: {inner_name} is read with ref or eq_ref access, and far fewer rows reach the loop.")
        } else if hinted {
            "Explain again without the hint: a hash or merge join instead of the loop.".to_string()
        } else if repeated {
            format!("Run with actual rows: a hash join, or far fewer rows read per execution of {inner_name}.")
        } else {
            format!("Explain again: no {} under the join, or a join that seeks its matches.", inner.op)
        };
        out.push(d);
    }
    out
}

pub(super) fn no_join_predicate(cx: &Cx) -> Vec<Draft> {
    let ids = cross_joins(cx);
    cx.nodes
        .iter()
        .filter(|n| ids.contains(&n.id))
        .copied()
        .collect::<Vec<&PlanNode>>()
        .into_iter()
        // A side of one row (a total, a setting) multiplies nothing: joining to it without a condition is harmless.
        .filter(|n| {
            // Under a TOP, SQL Server estimates each side for the rows it expects to need; the side itself is bigger.
            let per_run = |c: &PlanNode| {
                (rows_out(c) / c.runs().max(1.0)).max(prop_f64(c, "EstimateRowsWithoutRowGoal").unwrap_or(0.0))
            };
            let sides: Vec<f64> = if n.children.len() >= 2 {
                n.children.iter().map(per_run).collect()
            } else {
                // MySQL's table joined through a join buffer: the table, and the rows joined before it.
                let before = cx.parent(n).and_then(|p| {
                    let at = p.children.iter().position(|c| c.id == n.id)?;
                    (at > 0).then(|| rows_out(&p.children[at - 1]))
                });
                vec![rows_read(n) / n.runs().max(1.0), before.unwrap_or(0.0)]
            };
            sides.iter().all(|rows| *rows >= 2.0)
        })
        .take(1)
        .map(|n| {
            let sides: Vec<String> = n.children.iter().filter_map(first_object).map(|o| table_leaf(&o)).collect();
            let impact = cx.subtree(n);
            let mut d = Draft::new("no-join-predicate", InsightArea::Query, impact, vec![n.id]);
            d.title = if sides.len() >= 2 {
                format!("{} and {} are joined without a condition: every row with every row", sides[0], sides[1])
            } else {
                "Two inputs are joined without a condition: every row with every row".into()
            };
            d.detail = format!("{} has no join condition, so its output is the product of both inputs ({} rows planned). That's almost always a missing ON clause.", n.op, format::count(rows_out(n)));
            d.fix = "Add the missing join condition; if you really want every combination, write CROSS JOIN so the intent is clear.".into();
            d.verify = "Explain again: the join has a condition and its row count is reasonable.".into();
            d
        })
        .collect()
}

pub(super) fn optimizer_limits(cx: &Cx) -> Vec<Draft> {
    let mut out = Vec::new();
    if let Some(reason) =
        cx.prop("StatementOptmEarlyAbortReason").filter(|r| matches!(*r, "TimeOut" | "MemoryLimitExceeded"))
    {
        let mut d = Draft::new("optimizer-timeout", InsightArea::Query, 0.1, vec![cx.plan.root.id]);
        // The optimizer says it stopped searching, not that the plan it kept is a bad one: Low on its word alone.
        d.fixed_weight = true;
        d.subtree = true;
        d.cap = Severity::Medium;
        let memory = reason.eq_ignore_ascii_case("MemoryLimitExceeded");
        d.title = if memory {
            "The optimizer ran out of memory before it finished choosing a plan".into()
        } else {
            "The optimizer stopped searching before it found the best plan (timeout)".into()
        };
        d.detail = format!(
            "SQL Server reports StatementOptmEarlyAbortReason = {reason}: the query is complex enough that the plan in use is the best one found in the time allowed, not necessarily the best one."
        );
        d.fix = "Make it simpler to optimize: split it into steps with a temporary table, remove joins the result doesn't need, or replace nested views with direct table references.".into();
        d.verify = "Explain again: StatementOptmEarlyAbortReason is gone.".into();
        out.push(d);
    }
    if cx.prop("CardinalityEstimationModelVersion") == Some("70") {
        let mut d = Draft::new("legacy-ce", InsightArea::Server, 0.02, vec![cx.plan.root.id]);
        d.fixed_weight = true;
        d.subtree = true;
        d.cap = Severity::Low;
        d.title = "Planned with the legacy cardinality estimator (the SQL Server 2012 model)".into();
        d.detail = "The database has LEGACY_CARDINALITY_ESTIMATION on, a compatibility level below 120, or the query uses the FORCE_LEGACY_CARDINALITY_ESTIMATION hint. The newer estimator handles correlated filters and joins better.".into();
        d.fix = "Test the query with the current estimator (OPTION (USE HINT('FORCE_DEFAULT_CARDINALITY_ESTIMATION'))) and, if it's faster, plan the switch for the database.".into();
        d.verify = "Explain again: CardinalityEstimationModelVersion is 120 or higher.".into();
        out.push(d);
    }
    out
}

pub(super) fn slow_compile(cx: &Cx) -> Vec<Draft> {
    let (planning, running) = match cx.engine() {
        Engine::Mssql => (cx.prop("CompileTime").and_then(|v| v.parse::<f64>().ok()), None),
        Engine::Postgres => (
            cx.prop("Planning Time").and_then(|v| v.parse::<f64>().ok()),
            cx.prop("Execution Time").and_then(|v| v.parse::<f64>().ok()),
        ),
        Engine::Mysql | Engine::Sqlite => (None, None),
    };
    let Some(planning) = planning else { return Vec::new() };
    let slower_than_running = running.is_some_and(|r| planning > r && planning >= 20.0);
    if planning < 500.0 && !slower_than_running {
        return Vec::new();
    }
    let running = running.or_else(|| elapsed_ms(cx.plan));
    let impact = running.map_or(0.1, |r| planning / (planning + r).max(1.0));
    let mut d = Draft::new("slow-compile", InsightArea::Query, impact, vec![cx.plan.root.id]);
    // A cached plan records the one compile that produced it: without a run to weigh it against, nothing says it
    // recompiles, and it is Low.
    d.fixed_weight = running.is_none();
    d.cost_ms = Some(planning);
    d.cap = Severity::Medium;
    // PostgreSQL's first planning on a connection also loads the tables' metadata into the backend's caches, and
    // Culprit's EXPLAIN runs on a fresh connection: a few tens of milliseconds may never recur.
    let cold = cx.engine() == Engine::Postgres;
    if cold && planning < 500.0 {
        d.cap = Severity::Low;
    }
    d.title = match running {
        Some(r) if slower_than_running => {
            format!("Planning takes {}, longer than running it ({})", format::duration(planning), format::duration(r))
        }
        _ => format!("Compiling this plan took {}", format::duration(planning)),
    };
    d.detail = format!(
        "When the plan isn't reused, every run pays this again. Many joins, many partitions or long IN lists make planning expensive.{}",
        if cold {
            " This is one measurement, and the first planning on a new connection also reads the tables' metadata: explain it twice in one session to see the time a warm connection pays."
        } else {
            ""
        }
    );
    d.fix = match cx.engine() {
        Engine::Postgres => "Reuse the plan with prepared statements, keep IN lists short (use = ANY($1) with an array), and make sure partition pruning can happen at plan time.".into(),
        _ => "Make sure the plan is reused: send the query parameterized rather than with literal values, and avoid OPTION (RECOMPILE) on frequent queries.".into(),
    };
    d.verify = "Explain again: compile/planning time is a small fraction of the run.".into();
    vec![d]
}

pub(super) fn remote_query(cx: &Cx) -> Vec<Draft> {
    cx.nodes
        .iter()
        .filter(|n| n.op.starts_with("Remote"))
        .take(1)
        .map(|n| {
            let mut d = Draft::new("remote-query", InsightArea::Query, cx.subtree(n), vec![n.id]);
            d.cap = Severity::Medium;
            // Remote Insert, Update and Delete send rows the other way: each one is a round trip to the other server.
            let writes = ["Remote Insert", "Remote Update", "Remote Delete"].contains(&n.op.as_str());
            if writes {
                let rows = n.children.first().map_or_else(|| rows_out(n), rows_out);
                d.title = format!("Sends {} to a linked server ({})", plural(rows, "row", "rows"), n.op);
                d.detail = "Each row written through a linked server crosses the network, usually one statement per row on the other side.".into();
                d.fix = "Run the change on the remote server itself (EXEC (…) AT the linked server, or a procedure there) with the rows it needs, rather than row by row from here.".into();
                d.verify = "Explain again: no Remote Insert, Update or Delete over many rows.".into();
            } else {
                d.title = format!("Pulls {} from a linked server", plural(rows_out(n), "row", "rows"));
                d.detail = "Rows from another server travel over the network, and filters that can't be sent along are applied locally after the transfer.".into();
                d.fix = "Filter on the remote side (OPENQUERY with the WHERE clause inside), or copy the data you need locally on a schedule.".into();
                d.verify = "Explain again: far fewer rows come back from the Remote operator.".into();
            }
            d
        })
        .collect()
}

pub(super) fn correlated_subquery(cx: &Cx) -> Vec<Draft> {
    let mut out = Vec::new();
    for n in &cx.nodes {
        let Some(parent) = cx.parent(n) else { continue };
        // NOT IN explains its own subquery and has the better fix.
        if not_in_work(cx, parent).is_some() {
            continue;
        }
        // Without measured runs, the parent's rows bound them from above: "up to" that many.
        let mut at_most = false;
        let (runs, outer, impact) = match cx.engine() {
            Engine::Postgres => {
                if n.props.get("Parent Relationship").map(String::as_str) != Some("SubPlan") {
                    continue;
                }
                // A hashed SubPlan runs once and is probed per row: `(hashed SubPlan 1)` in the parent's condition.
                let name = n.props.get("Subplan Name").map_or("subplan".to_string(), |s| s.to_ascii_lowercase());
                let parent_text =
                    parent.props.values().chain(parent.predicate.iter()).cloned().collect::<Vec<_>>().join(" ");
                if parent_text.to_ascii_lowercase().contains(&format!("hashed {name}")) {
                    continue;
                }
                // Measured runs. Else, in one of the parent's conditions, it runs for each row the parent tests, at most
                // the rows it reads (another condition may reject a row first); anywhere else (the select list), once
                // for each row the parent returns, never for each row of a table the parent scans to return a few.
                let runs = match n.act_executions {
                    Some(runs) => runs,
                    None if names_subplan(parent, &name) => {
                        at_most = true;
                        rows_read(parent)
                    }
                    None => rows_out(parent),
                };
                // PostgreSQL charges every run of the SubPlan to the parent's own cost.
                (runs, first_object(parent), (cx.share(parent) + cx.subtree(n)).min(1.0))
            }
            Engine::Mysql => {
                if !is_true(n.props.get("dependent")) || is_true(n.props.get("cacheable")) {
                    continue;
                }
                let runs = if n.props.contains_key("cache_state") {
                    // MariaDB's expression cache answers repeated outer values: only measured misses are runs.
                    let Some(runs) = n.act_executions else { continue };
                    runs
                } else if parent.object.is_some() {
                    // Attached to a table's condition: run for each row the table reads.
                    rows_read(parent)
                } else {
                    // In the select list: run for each row the query block returns.
                    parent.est_rows
                };
                let outer = if parent.object.is_some() {
                    parent.object.clone()
                } else {
                    parent
                        .children
                        .iter()
                        .find(|c| !matches!(c.op.as_str(), "Subquery" | "Dependent subquery"))
                        .and_then(first_object)
                };
                // MySQL costs one run of the subquery; the plan's row count says how many there are.
                let one = n.subtree_cost.max(0.0);
                let all = one * runs.max(1.0);
                let impact = if all > 0.0 { all / (cx.total - one + all) } else { 0.0 };
                (runs, outer, impact.clamp(0.0, 1.0))
            }
            Engine::Mssql | Engine::Sqlite => continue,
        };
        if runs < MANY_RUNS {
            continue;
        }
        let mut d = Draft::new("correlated-subquery", InsightArea::Query, impact, vec![n.id]);
        let outer_name = outer.as_deref().map(table_leaf).unwrap_or_else(|| "the outer query".into());
        d.title = format!(
            "A subquery runs {}{} times, once per row of {outer_name}",
            if at_most { "up to " } else { "" },
            format::count(runs)
        );
        d.detail = format!(
            "{} the subquery for every row instead of once for the whole set{}.",
            if cx.engine() == Engine::Mysql {
                "MySQL marks it DEPENDENT and not cacheable, so it re-runs"
            } else {
                "PostgreSQL runs"
            },
            first_object(n).map(|t| format!(", reading {} each time", table_leaf(&t))).unwrap_or_default()
        );
        d.fix = "Rewrite it as a join (or LEFT JOIN … GROUP BY for counts and sums) so it runs once; if it must stay correlated, index the columns it filters on so each run is a quick lookup.".into();
        d.verify =
            "Explain again: the subquery appears once, joined, instead of as a per-row SubPlan / DEPENDENT SUBQUERY."
                .into();
        out.push(d);
    }
    out.into_iter().take(2).collect()
}

/// Whether one of a PostgreSQL operator's conditions runs the SubPlan `name` ("subplan 1", lowercase): `(total >
/// (SubPlan 1))` in its Filter, never `SubPlan 12`.
fn names_subplan(n: &PlanNode, name: &str) -> bool {
    const CONDITIONS: [&str; 6] = ["Filter", "Join Filter", "Index Cond", "Recheck Cond", "Hash Cond", "Merge Cond"];
    [n.predicate.as_deref(), n.seek_predicate.as_deref()]
        .into_iter()
        .chain(CONDITIONS.iter().map(|k| n.props.get(*k).map(String::as_str)))
        .flatten()
        .any(|text| {
            let text = text.to_ascii_lowercase();
            text.match_indices(name).any(|(at, _)| !text[at + name.len()..].starts_with(|c: char| c.is_ascii_digit()))
        })
}

pub(super) fn temp_table_sort(cx: &Cx) -> Vec<Draft> {
    let server = if cx.mariadb() { "MariaDB" } else { "MySQL" };
    let in_temp =
        |n: &PlanNode| n.op == "Temporary table" || n.detail.as_deref().is_some_and(|d| d.contains("temporary table"));
    // MySQL marks the grouping step (`grouping_operation`, op Group) or the ordering step (`ordering_operation`, op
    // Sort); MariaDB wraps a "Temporary table" node, sorted when a Sort wraps that.
    let grouping = |n: &PlanNode| matches!(n.op.as_str(), "Group" | "Distinct" | "Temporary table");
    let sorted = |n: &PlanNode| {
        n.detail.as_deref().is_some_and(|d| d.contains("filesort"))
            || cx.nodes.iter().any(|p| p.kind == NodeKind::Sort && p.children.iter().any(|c| c.id == n.id))
    };
    fn grouped_below(n: &PlanNode, in_temp: &dyn Fn(&PlanNode) -> bool) -> bool {
        n.children.iter().any(|c| {
            (matches!(c.op.as_str(), "Group" | "Distinct" | "Temporary table") && in_temp(c))
                || grouped_below(c, in_temp)
        })
    }
    // A temporary table for an ORDER BY alone (on a table the join doesn't read first) has no grouping to explain.
    let statement = cx.statement();
    let groups = statement.is_empty() || statement.contains("group by") || statement.contains("distinct");
    let mut out = Vec::new();
    for n in cx.nodes.iter().filter(|n| in_temp(n)) {
        let rows = n.children.iter().map(rows_out).fold(0.0, f64::max).max(rows_out(n));
        if rows < MANY_ROWS {
            continue;
        }
        if !groups {
            let sort_above = cx.parent(n).filter(|p| p.kind == NodeKind::Sort).map_or(0.0, |p| cx.share(p));
            let mut d =
                Draft::new("temp-table-sort", InsightArea::Query, (cx.share(n) + sort_above).min(1.0), vec![n.id]);
            d.cap = Severity::Low;
            d.title =
                format!("Collects {} rows in an internal temporary table to sort them (filesort)", format::count(rows));
            d.detail = format!(
                "The ORDER BY isn't on the table the join reads first, so {server} collects the joined rows in a temporary table (on disk once it outgrows tmp_table_size) and sorts them."
            );
            d.fix = "Order by columns of the table the join reads first, with an index that returns them in that order; or sort fewer rows with a more selective WHERE or a LIMIT.".into();
            d.verify = "Explain again: no \"using temporary\" on the sort.".into();
            out.push(d);
            continue;
        }
        // The temporary table's own work and the sort of it, not the reads below it, which happen either way. MySQL
        // leaves most of it uncosted, so this is often small next to the scan feeding it.
        let sort_above = cx.parent(n).filter(|p| p.kind == NodeKind::Sort).map_or(0.0, |p| cx.share(p));
        let impact = (cx.share(n) + sort_above).min(1.0);
        if grouping(n) {
            let mut d = Draft::new("temp-table-sort", InsightArea::Index, impact, vec![n.id]);
            d.title = if sorted(n) {
                format!(
                    "Groups {} rows in an internal temporary table, then sorts them (filesort)",
                    format::count(rows)
                )
            } else {
                format!("Collects {} rows in an internal temporary table", format::count(rows))
            };
            d.detail = format!(
                "The rows don't arrive in the order the GROUP BY / DISTINCT needs, so {server} builds a temporary table (on disk once it outgrows tmp_table_size) and sorts the result."
            );
            d.fix = format!(
                "An index whose leading columns match the GROUP BY lets {server} group in index order with no temporary table; ordering by an aggregate (COUNT(*) DESC) always needs a sort, so keep the grouped set small."
            );
            d.verify = "Explain again: no \"using temporary table\" on the grouping.".into();
            out.push(d);
        } else if !grouped_below(n, &in_temp) {
            // The ordering step's temporary table: the grouping below already reads an index in order, so only the
            // groups are sorted. No index changes that when the order is by an aggregate.
            let mut d = Draft::new("temp-table-sort", InsightArea::Query, impact, vec![n.id]);
            d.cap = Severity::Low;
            d.title = "Sorts the groups in a temporary table (filesort)".into();
            d.detail = format!(
                "Grouping already reads an index in order; {server} then sorts the grouped result, because the query orders by an aggregate or by columns the index doesn't provide."
            );
            d.fix = "Ordering by an aggregate (COUNT(*) DESC) always needs this sort: keep the grouped set small with a selective WHERE, or add a LIMIT so only the top groups are kept.".into();
            d.verify = "Explain again: the sort's input is the groups you expect, not every row.".into();
            out.push(d);
        }
    }
    out
}
