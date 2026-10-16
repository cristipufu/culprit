//! The `scan-heavy` check: Large tables that queries read in full many times a day, from the table scan counters.

use super::*;

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

check!(ScanHeavyCheck: scan_heavy_info, scan_heavy);

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

/// The captured statements that read a table in full: their plan scans it, or, without a plan, they read or change
/// rows of it (SELECT, UPDATE, DELETE, MERGE) and name it. A statement that seeks the table through an index, or only
/// inserts into it, isn't behind its full reads, so its time isn't theirs.
pub(crate) fn scanning_queries<'a>(snapshot: &'a Snapshot, schema: &str, table: &str) -> Vec<&'a QueryStat> {
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
        let measured = per_day_is_measured(days);
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
        let scanning = scanning_queries(snapshot, &t.schema, &t.name);
        // A plan that scans it shows the full read; a statement without one may as well seek it (most reads by key
        // do): listed to explain, but its time isn't counted as the scans' cost.
        let (proven, unplanned): (Vec<&QueryStat>, Vec<&QueryStat>) = scanning.iter().partition(|q| q.plan.is_some());
        let mut f = finding("scan-heavy", &table_name, Category::Indexes, subject(SubjectKind::Table, &table_name));
        let rows_text = if measured {
            format!("{} rows a day", format::count(rows_read))
        } else {
            format!("{} rows", format::count(read_total))
        };
        f.title = format!(
            "{} is read in full {} ({rows_text})",
            t.name,
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
                    "{} of the captured statements read it in full by their plans{}; explain them to see which filter has no index.",
                    proven.len(),
                    if unplanned.is_empty() {
                        String::new()
                    } else {
                        format!(", and {} more without a captured plan may", unplanned.len())
                    }
                )
            } else if !unplanned.is_empty() {
                format!(
                    "{} of the captured statements read or change it without a captured plan; explain them to see whether they read it in full.",
                    unplanned.len()
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
                let time_ms: f64 = proven.iter().map(|q| q.total_time_ms).sum();
                f.severity = severity_for_time(snapshot, time_ms, i.share);
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
        f.fix = Some(Fix {
            summary: match snapshot.server.engine {
                Engine::Postgres => "Explain the statements below (Culprit can show a generic plan) and index the \
                    columns in their WHERE clause; small lookup tables are fine to scan."
                    .into(),
                _ => "Explain the statements below and index the columns in their WHERE clause; small lookup tables \
                    are fine to scan."
                    .into(),
            },
            sql: None,
            cost: crate::indexes::write_cost(snapshot, &t.schema, &t.name),
        });
        f.verify =
            vec![format!("Re-scan after a day: full reads of {} per day drop, and its index scans rise.", t.name)];
        f.caveats.push(
            "Counters accumulate since statistics were last reset; one nightly report can make up much of them.".into(),
        );
        out.push(f);
    }
    out
}
