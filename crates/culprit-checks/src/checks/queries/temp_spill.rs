//! The `temp-spill` check: Statements whose sorts, hashes or temporary tables outgrow memory and spill to disk.

use super::*;
use crate::evidence::{QueryMeasure, Requirement as Evidence};

pub fn temp_spill_info() -> CheckInfo {
    CheckInfo {
        id: "temp-spill".into(),
        title: "Queries writing temp files".into(),
        category: Category::Queries,
        engines: vec![Engine::Postgres, Engine::Mysql],
        requires: vec![],
        description: "Statements whose sorts, hashes or temporary tables outgrow memory and spill to disk.".into(),
    }
}

check!(TempSpillCheck: temp_spill_info, temp_spill, &[Evidence::Query(QueryMeasure::TempWrites)]);

pub fn temp_spill(snapshot: &Snapshot) -> Vec<Finding> {
    match snapshot.server.engine {
        Engine::Postgres => {}
        Engine::Mysql => return mysql_disk_temp_tables(snapshot),
        Engine::Mssql | Engine::Sqlite => return Vec::new(),
    }
    let Some(workload) = snapshot.workload.as_ref() else { return Vec::new() };
    let work_mem_text = snapshot.setting("work_mem").map(|s| s.value.clone());
    let work_mem = work_mem_text.as_deref().and_then(pg_size_bytes).unwrap_or(4.0 * MB);
    let block = snapshot.setting("block_size").and_then(|s| s.value.trim().parse::<f64>().ok()).unwrap_or(8192.0);
    app_queries(snapshot)
        .filter(|q| q.temp_writes.is_some_and(|t| t > 0.0))
        .filter_map(|q| {
            let impact = query_impact(snapshot, &[q])?;
            if impact.share < 0.01 {
                return None;
            }
            let written = q.temp_writes? * block;
            let per_run = written / q.executions.max(1.0);
            // pg_stat_statements counts the blocks written, not the time: what writing them and reading them back can
            // take at a slow disk's pace, at most the run itself. A spill too small to matter to the run isn't why it's
            // slow (and mustn't stand in for the heavy-query finding that would say what is).
            let spill_ms = (per_run * 2.0 / SPILL_BYTES_PER_MS).min(q.mean_time_ms);
            if spill_ms < q.mean_time_ms * SPILL_MIN_SHARE_OF_RUN {
                return None;
            }
            let spill_share = workload.share_of(spill_ms * q.executions, q);
            let mut f = finding("temp-spill", &q.id, Category::Queries, subject(SubjectKind::Query, &q.id));
            apply_impact(snapshot, &mut f, &impact);
            f.severity = severity_for_time_of(snapshot, &[q], spill_ms * q.executions, spill_share);
            f.impact.score = score_for_share(spill_share);
            f.confidence = Confidence::High;
            f.title = format!(
                "{} {} about {} of temp files per run",
                capitalize(&query_handle(q)),
                if crate::all_stopped(snapshot, &[q]) { "wrote" } else { "writes" },
                format::bytes(per_run)
            );
            // work_mem as Culprit's session has it: a role or database setting (ALTER ROLE … SET) gives the
            // application's sessions their own, which the scan doesn't read.
            let (needs, writes, spills) = if crate::all_stopped(snapshot, &[q]) {
                ("needed", "wrote", "spilled")
            } else {
                ("needs", "writes", "spills")
            };
            // The spill is kept only in the totals: where the statement's share and average are Culprit's scans', what
            // it wrote is said over what it is counted over.
            let basis = crate::support::totals_basis(snapshot, q);
            f.summary = format!(
                "A sort or hash in this statement {needs} more memory than work_mem ({} in Culprit's session; a setting on the application's role or database can differ) and {writes} the overflow to temporary files: {} over {} runs{}. pg_stat_statements counts the total, so whether every run {spills} or only some is unknown. Writing temp files and reading them back is many times slower than sorting in memory.",
                work_mem_text.as_deref().unwrap_or("4MB, the default"),
                format::bytes(written),
                format::count(q.executions),
                basis.as_ref().map(|b| format!(" in {b}")).unwrap_or_default()
            );
            f.queries = vec![QueryRef { id: q.id.clone(), plan_node: spilling_step(q, false), note: None }];
            let label = match &basis {
                Some(b) => format!("Temp written per run over {b}"),
                None => "Temp written per run".to_string(),
            };
            f.metrics.push(Metric::new(label, per_run, Unit::Bytes));
            f.metrics.extend(crate::queries::average_metrics(snapshot, q));
            f.fix = Some(work_mem_fix(work_mem, work_mem_text.as_deref(), per_run));
            f.verify = vec![spill_verify(
                snapshot,
                "Once the application's connections use the new setting (a role's setting reaches the connections it opens after the change)",
                "temp files",
                format!("{} over {} runs", format::bytes(written), format::count(q.executions)),
            )];
            Some(f)
        })
        .collect()
}

/// How a later scan shows the spill gone. A windowed source drops the runs from before the change once its window has
/// passed them. A source of running totals keeps them, and this finding with them: what shows it is the statement's
/// count staying as it is while its runs go on, between a scan taken when the change is live and a later one, said
/// with this scan's numbers (`now`) ([`verify_counts_between_scans`]).
fn spill_verify(snapshot: &Snapshot, live: &str, what: &str, now: String) -> String {
    match verify_counts_between_scans(snapshot, &format!("{what} and runs"), &now) {
        Some(compare) => format!("{live}, its {what} should stop growing while its runs go on: {compare}."),
        None => {
            format!("{live}, scan again once the scan's window holds only runs from after it: this finding is gone.")
        }
    }
}

/// The plan's step that spills, to point at: the one the plan saw spill, else the only one that can. PostgreSQL's temp
/// files come from a sort, a hash join's hash or a hash aggregate; MySQL's on-disk tables from an internal temporary
/// table (`temp_tables`), which no plan marks as spilled (MariaDB's spill mark is a sort's, not the table's). None when
/// the plan shows no such step, or several and nothing says which.
fn spilling_step(q: &QueryStat, temp_tables: bool) -> Option<u32> {
    let nodes = q.plan.as_ref()?.nodes();
    if !temp_tables && let Some(n) = nodes.iter().find(|n| n.warnings.contains(&PlanWarning::Spill)) {
        return Some(n.id);
    }
    let can_spill = |n: &&&PlanNode| {
        if temp_tables {
            n.op == "Temporary table" || n.detail.as_deref().is_some_and(|d| d.contains("temporary table"))
        } else {
            n.kind == NodeKind::Sort
                || n.op == "Hash"
                || n.props.get("Strategy").is_some_and(|s| s == "Hashed" || s == "Mixed")
        }
    };
    let mut steps = nodes.iter().filter(can_spill);
    match (steps.next(), steps.next()) {
        (Some(only), None) => Some(only.id),
        _ => None,
    }
}

pub(super) const MB: f64 = 1024.0 * 1024.0;
/// A slow disk's pace for temp files (100 MB/s), in bytes per millisecond.
const SPILL_BYTES_PER_MS: f64 = 100.0 * MB / 1000.0;
/// A spill that can take less than this share of a run isn't what makes the statement slow.
const SPILL_MIN_SHARE_OF_RUN: f64 = 0.1;
/// The most sort memory Culprit suggests for one role: past it, the fix is to sort less.
const WORK_MEM_MAX: f64 = 1024.0 * MB;

/// More work_mem than the spill needed, for the application's role or one transaction: never less than it has now.
pub(super) fn work_mem_fix(current: f64, current_text: Option<&str>, per_run: f64) -> Fix {
    let index = "add an index that returns rows already in the order the sort needs, or sort fewer rows";
    let now = current_text.map(str::to_string).unwrap_or_else(|| format::bytes(current));
    let wanted = (current + per_run * 2.0).max(16.0 * MB);
    let proposed = 2f64.powf(wanted.log2().ceil());
    if proposed > WORK_MEM_MAX || proposed <= current {
        return Fix {
            summary: format!(
                "work_mem is already {now}; more sort memory per operation is no longer a reasonable fix, so {index}."
            ),
            sql: None,
            cost: None,
        };
    }
    let value = if proposed >= 1024.0 * MB {
        format!("{:.0}GB", proposed / (1024.0 * MB))
    } else {
        format!("{:.0}MB", proposed / MB)
    };
    Fix {
        summary: format!(
            "Give this statement more sort memory ({value} instead of {now}, if the application's role doesn't already set more), for the role the application connects as or only around this statement, rather than server-wide; or {index}."
        ),
        sql: Some(format!(
            "-- For the role the application connects as (takes effect on its next connections):\nALTER ROLE <app_role> SET work_mem = '{value}';\n\n-- Or only for this statement, inside its transaction:\nSET LOCAL work_mem = '{value}';"
        )),
        cost: Some("work_mem is per sort or hash, per connection: size it with your connection count in mind.".into()),
    }
}

/// A PostgreSQL size setting ("4MB", "64kB", "1GB", or a bare number of kB) in bytes.
pub(super) fn pg_size_bytes(text: &str) -> Option<f64> {
    let t = text.trim();
    let split = t.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(t.len());
    let number: f64 = t[..split].parse().ok()?;
    let unit = match t[split..].trim().to_ascii_lowercase().as_str() {
        "" | "kb" => 1024.0,
        "b" => 1.0,
        "mb" => MB,
        "gb" => 1024.0 * MB,
        "tb" => 1024.0 * 1024.0 * MB,
        _ => return None,
    };
    Some(number * unit)
}

/// MySQL counts on-disk temporary tables per digest: a GROUP BY, DISTINCT or ORDER BY whose temporary table outgrew
/// memory, or held a value the in-memory engine can't (TEXT and BLOB, before MySQL 8.0's TempTable engine).
pub(super) fn mysql_disk_temp_tables(snapshot: &Snapshot) -> Vec<Finding> {
    let Some(workload) = snapshot.workload.as_ref() else { return Vec::new() };
    let limits = TempLimits::of(snapshot);
    app_queries(snapshot)
        .filter(|q| q.temp_writes.is_some_and(|t| t > 0.0))
        .filter_map(|q| {
            let impact = query_impact(snapshot, &[q])?;
            if impact.share < 0.01 {
                return None;
            }
            let tables = q.temp_writes?;
            let per_run = tables / q.executions.max(1.0);
            // MySQL counts tables, not runs: a run can build more than one, so the runs that spilled are at most this.
            let runs_share = per_run.min(1.0);
            let spilled_share = workload.share_of(q.total_time_ms * runs_share, q);
            let mut f = finding("temp-spill", &q.id, Category::Queries, subject(SubjectKind::Query, &q.id));
            apply_impact(snapshot, &mut f, &impact);
            let large = if limits.blob_bound { large_value_columns(snapshot, q) } else { Vec::new() };
            // The time of the runs that can have spilled, not the whole statement's. A table that outgrew the memory
            // limit is at least that big, work that costs. One sent to disk for a TEXT or BLOB value can be a few
            // rows, cheap where the file stays in the operating system's cache: nothing says it is what the run
            // spends its time on, so it is Low.
            f.severity = if large.is_empty() {
                severity_for_time_of(snapshot, &[q], q.total_time_ms * runs_share, spilled_share)
            } else {
                Severity::Low
            };
            f.impact.score = score_for_share(if large.is_empty() { spilled_share } else { spilled_share * 0.25 });
            f.confidence = Confidence::High;
            let handle = capitalize(&query_handle(q));
            // A statement that stopped running is told in the past.
            let stopped = crate::all_stopped(snapshot, &[q]);
            let builds = if stopped { "built" } else { "builds" };
            let (outgrows, moves, holds) =
                if stopped { ("outgrew", "moved", "held") } else { ("outgrows", "moves", "holds") };
            f.title = if per_run >= 0.95 && per_run.round() <= 1.0 {
                format!("{handle} {builds} an on-disk temporary table about once per run")
            } else if per_run >= 0.95 {
                format!("{handle} {builds} about {} on-disk temporary tables per run", format::count(per_run.round()))
            } else {
                format!("{handle} {builds} on-disk temporary tables in up to {} of its runs", format::percent(runs_share))
            };
            let name = snapshot.server.name();
            // Counted only in the totals: said over what they are counted over where the share is Culprit's scans'.
            let basis = crate::support::totals_basis(snapshot, q);
            let over = basis.as_ref().map(|b| format!(" over {b}")).unwrap_or_default();
            f.summary = if large.is_empty() {
                format!(
                    "The temporary table behind a GROUP BY, DISTINCT or ORDER BY in this statement {outgrows} {} and {moves} to disk, which is many times slower: {} on-disk tables in {} runs{over}.",
                    limits.describe(),
                    format::count(tables),
                    format::count(q.executions)
                )
            } else {
                format!(
                    "The temporary table behind a GROUP BY, DISTINCT or ORDER BY in this statement {holds} {}, and {name} keeps a temporary table with TEXT or BLOB values on disk whatever its size: {} on-disk tables in {} runs{over}. Raising the memory limit won't change that.",
                    format::list(&large),
                    format::count(tables),
                    format::count(q.executions)
                )
            };
            f.queries = vec![QueryRef { id: q.id.clone(), plan_node: spilling_step(q, true), note: None }];
            f.verify = vec![spill_verify(
                snapshot,
                if large.is_empty() && !limits.caps.is_empty() {
                    "Once the application's connections use the new limits"
                } else {
                    "Once the change is live"
                },
                "on-disk temporary tables",
                format!("{} in {} runs", format::count(tables), format::count(q.executions)),
            )];
            let label = match &basis {
                Some(b) => format!("On-disk temporary tables per run over {b}"),
                None => "On-disk temporary tables per run".to_string(),
            };
            f.metrics.push(Metric::new(label, per_run, Unit::Count));
            f.metrics.extend(crate::queries::average_metrics(snapshot, q));
            f.fix = Some(if large.is_empty() {
                limits.fix(snapshot)
            } else {
                Fix {
                    summary: format!(
                        "Leave {} out of the grouped, distinct or sorted rows (group by the key, then fetch the large columns for the rows you keep), or add an index that delivers rows already grouped or ordered so no temporary table is needed.",
                        format::list(&large)
                    ),
                    sql: None,
                    cost: None,
                }
            });
            Some(f)
        })
        .collect()
}

/// What keeps a MySQL or MariaDB internal temporary table in memory, as this server is set up.
pub(super) struct TempLimits {
    /// The variables that cap one table in memory, with their values (bytes) where the scan read them.
    caps: Vec<(&'static str, Option<f64>)>,
    /// MySQL 8.0's TempTable engine also stops at temptable_max_ram, for all its tables together: its value where the
    /// scan read it.
    shared_ram: Option<Option<f64>>,
    /// A TEXT or BLOB value sends the table to disk whatever its size (the MEMORY engine: MariaDB, MySQL 5.7).
    blob_bound: bool,
    /// MySQL 8.0 keeps these tables in TempTable or MEMORY (`internal_tmp_mem_storage_engine`), and the scan didn't
    /// read which: both variables of MEMORY's cap are raised, and nothing is said of TempTable's limit but its name.
    engine_unknown: bool,
}

impl TempLimits {
    fn of(snapshot: &Snapshot) -> TempLimits {
        let value = |name: &str| snapshot.setting(name).and_then(|s| s.value.trim().parse::<f64>().ok());
        let version = &snapshot.server.version;
        let mysql8 = snapshot.server.name() == "MySQL" && culprit_core::version_at_least(version, [8, 0, 0]);
        let engine = snapshot.setting("internal_tmp_mem_storage_engine").map(|s| s.value.trim().to_ascii_lowercase());
        let both =
            vec![("tmp_table_size", value("tmp_table_size")), ("max_heap_table_size", value("max_heap_table_size"))];
        if mysql8 && engine.as_deref() == Some("temptable") {
            // TempTable ignores max_heap_table_size; tmp_table_size caps each table from 8.0.28.
            let caps = if culprit_core::version_at_least(version, [8, 0, 28]) {
                vec![("tmp_table_size", value("tmp_table_size"))]
            } else {
                Vec::new()
            };
            return TempLimits {
                caps,
                shared_ram: Some(value("temptable_max_ram")),
                blob_bound: false,
                engine_unknown: false,
            };
        }
        if mysql8 && engine.is_none() {
            return TempLimits { caps: both, shared_ram: None, blob_bound: false, engine_unknown: true };
        }
        TempLimits {
            caps: both,
            shared_ram: None,
            // MySQL 8.0's MEMORY engine, MySQL 5.7 and MariaDB can't hold TEXT or BLOB values in memory.
            blob_bound: true,
            engine_unknown: false,
        }
    }

    /// The binding in-memory limit, in words: "the in-memory limit (1 MB, the smaller of tmp_table_size and max_heap_table_size)".
    fn describe(&self) -> String {
        let known: Vec<f64> = self.caps.iter().filter_map(|(_, v)| *v).collect();
        let smallest = known.iter().copied().reduce(f64::min);
        let ram = |ram: &Option<f64>| match ram {
            Some(v) => format!("temptable_max_ram, {}", format::bytes(*v)),
            None => "temptable_max_ram".to_string(),
        };
        match (self.caps.as_slice(), smallest, &self.shared_ram) {
            ([(name, Some(v))], _, Some(shared)) => format!(
                "the in-memory limit ({name}, {}; and {}, for all such tables together)",
                format::bytes(*v),
                ram(shared)
            ),
            ([], _, Some(Some(v))) => {
                format!("temptable_max_ram ({}, for all temporary tables together)", format::bytes(*v))
            }
            ([], _, Some(None)) => "temptable_max_ram (the memory for all temporary tables together)".to_string(),
            (caps, Some(v), _) if caps.len() > 1 && self.engine_unknown => format!(
                "the in-memory limit ({}, the smaller of {}, where these tables use the MEMORY engine; the TempTable \
                 engine stops at tmp_table_size and temptable_max_ram instead)",
                format::bytes(v),
                format::list(&caps.iter().map(|(n, _)| n.to_string()).collect::<Vec<_>>())
            ),
            (caps, Some(v), _) if caps.len() > 1 => format!(
                "the in-memory limit ({}, the smaller of {})",
                format::bytes(v),
                format::list(&caps.iter().map(|(n, _)| n.to_string()).collect::<Vec<_>>())
            ),
            _ => "the in-memory limit".to_string(),
        }
    }

    /// Raise every cap below the proposal to it, through the server's own way of changing settings; never lower one.
    fn fix(&self, snapshot: &Snapshot) -> Fix {
        let index = "Check whether an index matching the GROUP BY or ORDER BY reduces temporary-table work (an ORDER BY that differs from the grouping can still need a temporary table and sort)";
        let (names, current): (Vec<&str>, Option<f64>) = if self.caps.is_empty() {
            (vec!["temptable_max_ram"], self.shared_ram.flatten())
        } else {
            (self.caps.iter().map(|(n, _)| *n).collect(), self.caps.iter().filter_map(|(_, v)| *v).reduce(f64::min))
        };
        let floor = if self.caps.is_empty() { 2048.0 * MB } else { 64.0 * MB };
        let proposed = current.map_or(floor, |c| (c * 2.0).max(floor));
        let now = |name: &str| {
            if self.caps.is_empty() {
                self.shared_ram.flatten()
            } else {
                self.caps.iter().find(|(n, _)| *n == name).and_then(|(_, v)| *v)
            }
        };
        // A value the scan didn't read could be higher than the proposal: no SQL that might lower it.
        if names.iter().any(|n| now(n).is_none()) {
            return Fix {
                summary: format!(
                    "{index}, or raise {} (the scan didn't read {}).{}",
                    format::list(&names.iter().map(|n| n.to_string()).collect::<Vec<_>>()),
                    if names.len() == 1 { "its current value" } else { "their current values" },
                    if self.caps.is_empty() { "" } else { NEW_CONNECTION_LIMITS }
                ),
                sql: None,
                cost: None,
            };
        }
        let changes: Vec<SettingChange> = names
            .iter()
            .filter(|name| now(name).is_some_and(|v| v < proposed))
            .map(|name| setting_change(&snapshot.server, name, &format!("{proposed:.0}")))
            .collect();
        if changes.is_empty() {
            return Fix { summary: format!("{index}."), sql: None, cost: None };
        }
        let sql: Vec<String> = changes.iter().filter_map(|c| c.sql.clone()).collect();
        let how: Vec<String> = changes.iter().map(|c| c.how.clone()).collect();
        Fix {
            summary: format!(
                "{index}, or raise the in-memory limit to {}{}. {}{}",
                format::bytes(proposed),
                match self.caps.len() {
                    0 | 1 => "",
                    _ if self.engine_unknown => " (both variables: with the MEMORY engine the smaller one wins)",
                    _ => " (both variables: the smaller one wins)",
                },
                how.join(" "),
                if self.caps.is_empty() { "" } else { NEW_CONNECTION_LIMITS }
            ),
            sql: (!sql.is_empty()).then(|| sql.join("\n")),
            cost: Some(if self.caps.is_empty() {
                "temptable_max_ram is memory the server may use for all temporary tables together.".into()
            } else {
                "The limit is per temporary table per connection; size it with your connection count in mind.".into()
            }),
        }
    }
}

/// The per-table caps are session variables on both MySQL and MariaDB; the shared TempTable RAM cap isn't.
const NEW_CONNECTION_LIMITS: &str = " Global values apply to new connections: reopen the application's existing connections, or have the application set these session variables on them.";

/// TEXT and BLOB columns of the statement's tables that its text names (or all of them, for `SELECT *`).
pub(super) fn large_value_columns(snapshot: &Snapshot, q: &QueryStat) -> Vec<String> {
    let text = squeeze(&q.text);
    let star = text.contains("select *") || text.contains(".*");
    let mut out: Vec<String> = Vec::new();
    for c in &snapshot.columns {
        let t = c.data_type.to_ascii_lowercase();
        let large = t.contains("text") || t.contains("blob") || t == "json" || t == "longtext";
        if large
            && mentions_word(&text, &c.table.to_ascii_lowercase())
            && (star || mentions_word(&text, &c.name.to_ascii_lowercase()))
            && !out.contains(&c.name)
        {
            out.push(c.name.clone());
        }
    }
    out
}

#[cfg(test)]
pub(super) mod wording_tests {
    use super::*;
    use culprit_core::{NodeKind, Properties};

    #[test]
    fn plan_steps_take_the_right_article() {
        let mut n = PlanNode {
            id: 1,
            op: "Index Scan".into(),
            kind: NodeKind::Scan,
            detail: None,
            object: Some("orders".into()),
            index: None,
            seek_predicate: None,
            predicate: None,
            est_rows: 1.0,
            est_executions: 1.0,
            act_rows: None,
            act_executions: None,
            rows_read: None,
            table_rows: None,
            subtree_cost: 1.0,
            self_cost: 1.0,
            act_time_ms: None,
            warnings: vec![],
            props: Properties::new(),
            children: vec![],
            runtime: None,
        };
        let plan = |n: &PlanNode| Plan {
            view: None,
            engine: Engine::Postgres,
            kind: culprit_core::PlanKind::Estimated,
            statement: None,
            total_cost: 1.0,
            root: n.clone(),
            warnings: vec![],
            index_hints: vec![],
            props: Properties::new(),
            raw: None,
            insights: None,
        };
        assert_eq!(describe_node(&plan(&n), &n, None), "an index scan of orders");
        n.op = "Seq Scan".into();
        assert_eq!(describe_node(&plan(&n), &n, None), "a seq scan of orders");
    }

    #[test]
    fn rows_a_step_reads_are_said_as_measured_or_as_estimated() {
        let mut n = PlanNode {
            id: 1,
            op: "Index range scan".into(),
            kind: NodeKind::Seek,
            detail: None,
            object: Some("shop.orders".into()),
            index: Some("ix_orders_status_created".into()),
            seek_predicate: None,
            predicate: None,
            est_rows: 29_126.0,
            est_executions: 1.0,
            act_rows: None,
            act_executions: None,
            rows_read: Some(29_126.0),
            table_rows: None,
            subtree_cost: 1.0,
            self_cost: 1.0,
            act_time_ms: None,
            warnings: vec![],
            props: Properties::new(),
            children: vec![],
            runtime: None,
        };
        let plan = |n: &PlanNode| Plan {
            view: None,
            engine: Engine::Mysql,
            kind: culprit_core::PlanKind::Estimated,
            statement: None,
            total_cost: 1.0,
            root: n.clone(),
            warnings: vec![],
            index_hints: vec![],
            props: Properties::new(),
            raw: None,
            insights: None,
        };
        // An estimated plan's rows read are an estimate, not what the runs read.
        assert_eq!(
            describe_node(&plan(&n), &n, None),
            "an index range scan of shop.orders, estimated to read 29K rows"
        );
        // MySQL's rows examined per run are this step's when it reads the plan's only table.
        assert_eq!(
            describe_node(&plan(&n), &n, Some(10_715.0)),
            "an index range scan of shop.orders that examines 11K rows a run"
        );
        // Measured by the plan itself.
        n.act_rows = Some(20_000.0);
        n.rows_read = Some(20_000.0);
        assert_eq!(describe_node(&plan(&n), &n, None), "an index range scan of shop.orders that reads 20K rows");
    }
}
