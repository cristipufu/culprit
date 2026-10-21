//! The `scan-heavy` check: Large tables that queries read in full many times a day, from the table scan counters.

use super::*;
use crate::evidence::Requirement as Evidence;

pub fn scan_heavy_info() -> CheckInfo {
    CheckInfo {
        id: "scan-heavy".into(),
        title: "Tables read in full over and over".into(),
        category: Category::Indexes,
        engines: vec![Engine::Postgres, Engine::Mysql],
        requires: vec![Capability::TableStats],
        description: "Large tables that queries read in full many times a day, from the table scan counters.".into(),
    }
}

check!(ScanHeavyCheck: scan_heavy_info, scan_heavy, &[Evidence::Tables(crate::evidence::TableMeasure::Scans)]);

/// Full-table reads and rows read without an index since the counters started, and the days that covers. A full read
/// is the table's rows read once: PostgreSQL's `seq_scan` counts a scan once for every process that runs it (each
/// parallel worker and the leader) and again on every rescan, and counts a scan a LIMIT stopped early as a whole one,
/// so the rows it read, divided by the table's rows, is the count to say when it is lower.
pub(crate) fn full_scans_measured(snapshot: &Snapshot, t: &culprit_core::TableStat) -> Option<(f64, f64, f64)> {
    let days = counter_days_for_rate(snapshot)?;
    let rows = t.rows.filter(|r| *r >= 50_000.0)?;
    let scanned = t.seq_rows_read?;
    let full_reads = (scanned / rows).round();
    let scans = t.seq_scans.map_or(full_reads, |n| n.min(full_reads));
    Some((scans, scanned, days))
}

/// A statement with no WHERE: a GROUP BY or a report over the whole table, an UPDATE of every row. It reads the table
/// in full by what it asks for; its sort, grouping or result size is another finding's.
fn reads_every_row(q: &QueryStat) -> bool {
    !mentions_word(&q.text, "where")
}

/// The captured statements that read a table in full: their plan scans it, or, without a plan, they read or change rows
/// of it (SELECT, UPDATE, DELETE, MERGE) and name it. A statement that seeks the table through an index, or only
/// inserts into it, isn't behind its full reads, so its time isn't theirs. The check sets apart those with no WHERE
/// ([`reads_every_row`]), which read every row on purpose, and lists the rest; the index advisor lists what it lists.
fn reading_in_full<'a>(snapshot: &'a Snapshot, schema: &str, table: &str) -> Vec<&'a QueryStat> {
    let qualified = format!("{schema}.{table}").to_ascii_lowercase();
    let names_table = |object: &str| {
        let o = object.to_ascii_lowercase().replace(['[', ']', '"', '`'], "");
        o == qualified || o.ends_with(&format!(".{qualified}")) || o == table.to_ascii_lowercase()
    };
    queries_touching(snapshot, schema, table)
        .into_iter()
        .filter(|q| !is_monitoring_query(q))
        .filter(|q| match &q.plan {
            Some(plan) => plan
                .nodes()
                .iter()
                .any(|n| n.kind == culprit_core::NodeKind::Scan && n.object.as_deref().is_some_and(names_table)),
            None => {
                let verb = q.text.trim_start_matches(|c: char| c.is_whitespace() || c == '(').to_ascii_lowercase();
                ["select", "with", "update", "delete", "merge"].iter().any(|v| verb.starts_with(v))
            }
        })
        .collect()
}

/// What a statement's runs say about reading a table of `table_rows` rows in full, where the engine counts the rows
/// each statement examined (MySQL and MariaDB): under a tenth of the table a run, it reaches the rows through an index
/// (`Some(false)`); half of it or more a run, from a statement that names no other table, it reads it in full
/// (`Some(true)`). `None` where the counts can't say: another engine, no count, a join (its rows examined are every
/// table's), or anything in between.
fn examined_reads_whole(snapshot: &Snapshot, q: &QueryStat, table_rows: f64) -> Option<bool> {
    if snapshot.server.engine != Engine::Mysql || table_rows <= 0.0 {
        return None;
    }
    let examined = q.reads_per_execution()?;
    if examined < table_rows * 0.1 {
        return Some(false);
    }
    let one_table = culprit_core::sql::tables_in(&q.text, Engine::Mysql).len() == 1;
    (examined >= table_rows * 0.5 && one_table).then_some(true)
}

/// The sentence for statements with no WHERE that read the table in full: on purpose, and nothing an index on a
/// filter changes. They aren't listed with the finding (its fix is about filters), so it names them.
fn on_purpose(whole: &[&QueryStat]) -> String {
    let ids: Vec<String> = whole.iter().map(|q| q.id.clone()).collect();
    match whole.len() {
        0 => String::new(),
        1 => {
            let handle = query_handle(whole[0]);
            let mut chars = handle.chars();
            let handle =
                chars.next().map(|c| c.to_uppercase().collect::<String>() + chars.as_str()).unwrap_or_default();
            format!(
                " {handle} (query {}) reads it in full too, on purpose: it has no WHERE clause, so no index on a filter changes that.",
                ids[0]
            )
        }
        n => format!(
            " {n} statements with no WHERE clause (queries {}) read it in full too, on purpose: no index on a filter changes that.",
            format::list(&ids)
        ),
    }
}

pub fn scan_heavy(snapshot: &Snapshot) -> Vec<Finding> {
    let planned: Vec<(String, String)> = scan_groups(snapshot)
        .into_iter()
        .map(|g| (g.schema.to_ascii_lowercase(), g.table.to_ascii_lowercase()))
        .collect();
    let mut out = Vec::new();
    // A statement that reads every partition reads each one in full: judged per partition it would be one finding per
    // partition for one cause.
    for t in snapshot.tables.iter().filter(|t| t.partition_of.is_none()) {
        let Some((scans_total, read_total, days)) = full_scans_measured(snapshot, t) else { continue };
        // Judged per day; said per day only over 20 hours of counters, otherwise as counted.
        let (scans, rows_read) = (scans_total / days, read_total / days);
        let measured = counters_measure_a_day(snapshot, days);
        if scans < 50.0 || rows_read < 5_000_000.0 {
            continue;
        }
        // A captured plan already names the filter and the index to add: the filtered-scan finding says it better.
        if planned.contains(&(t.schema.to_ascii_lowercase(), t.name.to_ascii_lowercase())) {
            continue;
        }
        let table_name = t.qualified();
        let touching: Vec<&QueryStat> =
            queries_touching(snapshot, &t.schema, &t.name).into_iter().filter(|q| !is_monitoring_query(q)).collect();
        let (whole, scanning): (Vec<&QueryStat>, Vec<&QueryStat>) =
            reading_in_full(snapshot, &t.schema, &t.name).into_iter().partition(|q| reads_every_row(q));
        // A plan that scans it shows the full read, and so do the rows a statement examined on MySQL when they are about
        // the whole table. A statement without either may as well seek it (most reads by key do): listed to explain,
        // but its time isn't counted as the scans' cost; and one whose rows examined are a sliver of the table reaches
        // them through an index, so it isn't listed at all.
        let table_rows = t.rows.unwrap_or(0.0);
        let (by_plan, rest): (Vec<&QueryStat>, Vec<&QueryStat>) = scanning.iter().partition(|q| q.plan.is_some());
        let (by_rows, unplanned): (Vec<&QueryStat>, Vec<&QueryStat>) = rest
            .into_iter()
            .filter(|q| examined_reads_whole(snapshot, q, table_rows) != Some(false))
            .partition(|q| examined_reads_whole(snapshot, q, table_rows) == Some(true));
        let proven: Vec<&QueryStat> = by_plan.iter().chain(&by_rows).copied().collect();
        // How the proven ones show it: their plans, the rows they examine, or both.
        let shown = match (by_plan.is_empty(), by_rows.as_slice()) {
            (false, []) => if proven.len() == 1 { "its plan scans it" } else { "their plans scan it" }.to_string(),
            (true, [q]) => format!(
                "it examines {} rows a run, about the whole table",
                format::count(q.reads_per_execution().unwrap_or(0.0))
            ),
            (true, _) => "each examines half its rows or more a run".to_string(),
            (false, _) => "by their plans, or by the rows they examine a run".to_string(),
        };
        // Statements with no WHERE are behind full reads too, but on purpose. When the query history has nothing else
        // that reads it in full and their plans read it whole as often as it was read in full, the reads are theirs, and
        // no index on a filter has anything to remove.
        let whole_runs: f64 = whole.iter().filter(|q| q.plan.is_some()).map(|q| q.executions).sum();
        if scanning.is_empty() && !whole.is_empty() && whole_runs >= scans_total {
            continue;
        }
        let mut f = finding("scan-heavy", &table_name, Category::Indexes, subject(SubjectKind::Table, &table_name));
        let rows_text = if measured {
            format!("{} rows a day", format::count(rows_read))
        } else {
            format!("{} rows", format::count(read_total))
        };
        // Read in full by statements that all stopped running: told in the past, as the finding then is.
        let listed: Vec<&QueryStat> = proven.iter().chain(&unplanned).copied().take(5).collect();
        f.title = format!(
            "{} {} read in full {} ({rows_text})",
            t.name,
            if crate::all_stopped(snapshot, &listed) { "was" } else { "is" },
            how_many(snapshot, scans_total, days, "time", "times")
        );
        f.summary = format!(
            "{} has {} rows, and queries have read all of it {}{}: {rows_text} read without an index. {}",
            table_name,
            format::count(t.rows.unwrap_or(0.0)),
            how_many(snapshot, scans_total, days, "time", "times"),
            if measured { format!(" {}", since_phrase(snapshot)) } else { String::new() },
            if !proven.is_empty() {
                format!(
                    "{} of the captured statements {} it in full ({shown}){}; explain {} to check the access path, then measure rows kept in representative runs.{}",
                    proven.len(),
                    if proven.len() == 1 { "reads" } else { "read" },
                    if unplanned.is_empty() {
                        String::new()
                    } else {
                        format!(", and {} more without a captured plan may", unplanned.len())
                    },
                    if proven.len() + unplanned.len() == 1 { "it" } else { "them" },
                    on_purpose(&whole)
                )
            } else if !unplanned.is_empty() {
                format!(
                    "{} of the captured statements {} or {} it without a captured plan; explain {} to see whether {} it in full.{}",
                    unplanned.len(),
                    if unplanned.len() == 1 { "reads" } else { "read" },
                    if unplanned.len() == 1 { "changes" } else { "change" },
                    if unplanned.len() == 1 { "it" } else { "them" },
                    if unplanned.len() == 1 { "it reads" } else { "they read" },
                    on_purpose(&whole)
                )
            } else if !whole.is_empty() {
                format!(
                    "{} The rest come from statements outside the top of the workload.",
                    on_purpose(&whole).trim_start()
                )
            } else if !touching.is_empty() {
                "The captured statements that name it reach it through an index, so the full reads come from \
                 statements outside the top of the workload."
                    .to_string()
            } else {
                "None of the captured statements names it, so the scans come from statements outside the top of the workload."
                    .to_string()
            }
        );
        // Only the statements whose plans read it in full are behind the scans, and their time is the cost.
        let impact = query_impact(snapshot, &proven);
        match &impact {
            Some(i) if i.share >= 0.005 => {
                apply_query_impact(&mut f, i);
                // Their share and the amount it comes to, on one basis (Culprit's scans where they measured them).
                f.severity = severity_for_statement_share(snapshot, &proven, i.share);
            }
            _ => {
                f.severity = if rows_read >= 1e9 { Severity::Medium } else { Severity::Low };
                f.impact.score = base_score(f.severity) + (rows_read.log10() - 6.0).max(0.0) * 3.0;
                f.impact.label = if measured {
                    format!("{} rows scanned/day", format::count(rows_read))
                } else {
                    format!("{} rows scanned", format::count(read_total))
                };
            }
        }
        f.confidence = if proven.is_empty() { Confidence::Low } else { Confidence::Medium };
        f.queries = query_refs(&proven.iter().chain(&unplanned).copied().take(5).collect::<Vec<_>>());
        if measured {
            f.metrics.push(Metric::new("Full reads per day", scans, Unit::Count));
            f.metrics.push(Metric::new("Rows read without an index per day", rows_read, Unit::Rows));
        } else {
            f.metrics.push(Metric::new("Full reads", scans_total, Unit::Count));
            f.metrics.push(Metric::new("Rows read without an index", read_total, Unit::Rows));
        }
        if let Some(rows) = t.rows {
            f.metrics.push(Metric::new("Rows in table", rows, Unit::Rows));
        }
        let explain = if f.queries.is_empty() {
            "Find the statements that read it in full (they aren't among the heaviest this scan captured), explain them"
        } else {
            "Explain the statements listed with this finding"
        };
        f.fix = Some(Fix {
            summary: format!(
                "{explain}{} to check their access paths. To measure each filter, capture an actual plan with \
                 representative values in a controlled environment: this runs the statement. Compare rows kept \
                 with rows read. If a filter keeps at most {} of the rows read, consider an index on the columns \
                 it searches by; if it keeps more, read fewer rows or columns instead. Small lookup tables are fine to scan.",
                if snapshot.server.engine == Engine::Postgres {
                    " (Culprit can show a generic plan, which does not measure actual rows)"
                } else {
                    ""
                },
                format::percent(culprit_core::plan::INDEX_KEEPS_AT_MOST)
            ),
            sql: None,
            // No SQL creates an index here: the cost line names the one the reader would add.
            cost: crate::indexes::write_cost_of(snapshot, &t.schema, &t.name, "any index you add"),
        });
        // The counters keep totals since they started, so a day moves a rate over weeks by a hair: what shows the
        // change is what they grow by between two scans.
        f.verify = vec![format!(
            "After adding an index for a selective filter, the full reads of {} should grow far slower. Compare how much its scan counters grow between a scan taken once the change is live and one a day or more later ({} full reads{} in this scan).",
            t.name,
            format::count(scans_total),
            t.index_scans.map(|n| format!(" and {} index scans", format::count(n))).unwrap_or_default()
        )];
        if !f.queries.is_empty() {
            let (queries, their) = if f.queries.len() == 1 { ("the query", "its") } else { ("the queries", "their") };
            let cheaper = format!("{their} time per run should drop");
            let verify = crate::queries::what_changed_between_scans(snapshot, queries, &cheaper)
                .unwrap_or_else(|| format!("Re-scan after the workload has run: {cheaper}."));
            f.verify.push(format!("Whether the fix adds an index or reads fewer rows or columns: {verify}"));
        }
        f.caveats.push(
            "Counters accumulate since statistics were last reset; one nightly report can make up much of them.".into(),
        );
        out.push(f);
    }
    out
}
