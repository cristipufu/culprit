//! The `large-rows` check: Tables whose rows average many kilobytes, most of it in large values kept apart from the rows (SQL Server's LOB pages, PostgreSQL's TOAST): every statement that reads whole rows pulls them in, every write that sets them writes them again. From the catalog's page counts; no data is read.

use super::*;

pub fn large_rows_info() -> CheckInfo {
    CheckInfo {
        id: "large-rows".into(),
        title: "Tables whose rows are mostly large values".into(),
        category: Category::Maintenance,
        engines: vec![Engine::Mssql, Engine::Postgres],
        requires: vec![],
        description: "Tables whose rows average many kilobytes, most of it in large values kept apart from the rows (SQL Server's LOB pages, PostgreSQL's TOAST): every statement that reads whole rows pulls them in, every write that sets them writes them again. From the catalog's page counts; no data is read.".into(),
    }
}

check!(LargeRowsCheck: large_rows_info, large_rows);

/// A row this big on average is mostly a payload, not a record.
pub(super) const LARGE_ROW_BYTES: f64 = 16.0 * 1024.0;
/// Below this the large values cost too little to be worth a finding.
const LARGE_ROWS_TABLE_BYTES: f64 = 256.0 * MB;
/// The share of a table's data in large-value pages that makes them, not the rows, its size.
const LARGE_VALUE_SHARE: f64 = 0.5;
/// Pages a statement reads a run, and for each row it returns or changes, that mean it pulls whole rows: 16 pages is
/// 128 KB a row, far more than any index or record needs.
const WHOLE_ROW_PAGES: f64 = 50.0;
const WHOLE_ROW_PAGES_PER_ROW: f64 = 16.0;
/// Whole-row reads this often a day cost more than the table's size alone.
const WHOLE_ROW_RUNS_PER_DAY: f64 = 1_000.0;
/// Growth that matters: a tenth of the table a day, and at least this much.
const FAST_GROWTH_SHARE: f64 = 0.1;
const FAST_GROWTH_BYTES: f64 = 100.0 * MB;
/// A table this big is costly to keep, back up and restore whatever reads it.
const BIG_TABLE_BYTES: f64 = 10.0 * GB;
/// A data page, on SQL Server and PostgreSQL.
const PAGE_BYTES: f64 = 8_192.0;

/// A table's own data and the part of it in large-value pages: its in-row, large-value and row-overflow pages when the
/// engine splits them. `None` when it doesn't.
pub(super) fn split_bytes(t: &TableStat) -> Option<(f64, f64)> {
    let large = t.lob_bytes? + t.overflow_bytes.unwrap_or(0.0);
    let total = match t.in_row_bytes {
        Some(in_row) => in_row + large,
        None => t.data_bytes?,
    };
    (total > 0.0).then_some((total, large))
}

/// A column type whose values the engine keeps apart from the row once they're large.
pub(super) fn large_value_type(engine: Engine, data_type: &str) -> bool {
    let t = data_type.to_ascii_lowercase();
    match engine {
        Engine::Mssql => {
            t.ends_with("(max)") || matches!(t.as_str(), "xml" | "text" | "ntext" | "image" | "geography" | "geometry")
        }
        Engine::Postgres => {
            matches!(t.as_str(), "text" | "bytea" | "json" | "jsonb" | "xml" | "varchar" | "character varying")
        }
        Engine::Mysql | Engine::Sqlite => false,
    }
}

/// Statements that read or rewrite whole rows of `t`: many pages a run, and many for each row they return or change.
/// One-off statements (a single run) aren't the application's. A scan of the rows' own pages reads about as many
/// pages as the table keeps in-row, and returns few rows (a count, a filter on an unindexed column): many pages a row
/// without a single large value. A statement reading about that many pages a run is left out; one reading far fewer
/// (a few rows by key) or more (the large values too) isn't.
pub(super) fn whole_row_statements<'a>(
    snapshot: &'a Snapshot,
    t: &TableStat,
    size: f64,
    large: f64,
) -> Vec<&'a QueryStat> {
    let in_row_pages = t.in_row_bytes.unwrap_or(size - large).max(0.0) / PAGE_BYTES;
    let scans_in_row = |pages: f64| pages >= in_row_pages * 0.5 && pages <= in_row_pages * 1.2;
    let mut found: Vec<&QueryStat> = queries_touching(snapshot, &t.schema, &t.name)
        .into_iter()
        .filter(|q| {
            if is_monitoring_query(q) || q.executions < 2.0 {
                return false;
            }
            let Some(reads) = q.logical_reads else { return false };
            let per_run = reads / q.executions;
            let rows = q.rows.map(|r| r / q.executions).unwrap_or(1.0).max(1.0);
            per_run >= WHOLE_ROW_PAGES && per_run / rows >= WHOLE_ROW_PAGES_PER_ROW && !scans_in_row(per_run)
        })
        .collect();
    found.sort_by(|a, b| b.logical_reads.unwrap_or(0.0).total_cmp(&a.logical_reads.unwrap_or(0.0)));
    found.truncate(5);
    found
}

/// The sentence about the statements a large-rows finding lists, with a space in front ("The 3 statements below read
/// or write them 696 times in 2 hours, going through about 294 pages (2.3 MB) a run."), or nothing without one. The
/// pages a run are said only when every statement counted its reads. Refine says it again once a writer joins them.
pub(crate) fn statements_below(snapshot: &Snapshot, statements: &[&QueryStat]) -> String {
    if statements.is_empty() {
        return String::new();
    }
    let runs: f64 = statements.iter().map(|q| q.executions).sum();
    let reads: Vec<f64> = statements.iter().filter_map(|q| q.logical_reads).collect();
    let writes = statements.iter().filter(|q| writes_rows(&q.text)).count();
    let one = statements.len() == 1;
    let verb = match (writes, one) {
        (0, true) => "reads them",
        (0, false) => "read them",
        (1, true) => "writes them",
        (w, false) if w == statements.len() => "write them",
        _ => "read or write them",
    };
    let pages = (reads.len() == statements.len()).then(|| (reads.iter().sum::<f64>() / runs.max(1.0)).round());
    format!(
        " {} below {verb} {}{}",
        if one { "The statement".to_string() } else { format!("The {} statements", statements.len()) },
        runs_of_all(snapshot, statements),
        match pages {
            Some(p) =>
                format!(", going through about {} pages ({}) a run.", format::count(p), format::bytes(p * 8192.0)),
            None => ".".into(),
        }
    )
}

/// Whether a statement writes rows rather than reads them: its first statement keyword, past any parameter
/// declarations and comments the engine kept with the text.
pub(super) fn writes_rows(text: &str) -> bool {
    first_verb(text).is_some_and(|(_, k)| k != "select")
}

/// A table's average row and the share of its data in large-value pages, when its rows are mostly large values (the
/// bar large-rows holds them to, whatever the table's size): an insert or an update that sets them logs them in full.
pub(super) fn large_values(t: &TableStat) -> Option<(f64, f64)> {
    let rows = table_rows(t)?;
    let (size, large) = split_bytes(t)?;
    let versions = rows + t.dead_rows.unwrap_or(0.0).max(0.0);
    (size / versions >= LARGE_ROW_BYTES && large / size >= LARGE_VALUE_SHARE).then(|| (size / rows, large / size))
}

/// What shrinks large values that must stay: large-rows' fix, which log-volume points to for the log they make.
pub(super) fn compress_advice(engine: Engine) -> &'static str {
    match engine {
        Engine::Postgres => {
            "LZ4 compresses them faster than the default (ALTER TABLE … ALTER COLUMN … SET COMPRESSION lz4, PostgreSQL 14+), or keep them in object storage and store a reference"
        }
        _ => {
            "COMPRESS() them as they're written (GZip; DECOMPRESS() reads them back), or keep them in object storage and store a reference"
        }
    }
}

/// The rows a table holds, as far as its size per row goes. PostgreSQL's live-row counter starts again from zero after
/// a statistics reset, a crash or a failover, while the planner's count (reltuples) doesn't: the larger of the two.
pub(super) fn table_rows(t: &TableStat) -> Option<f64> {
    let planner = t.storage.as_ref().and_then(|s| s.planner_rows);
    match (t.rows, planner) {
        (Some(live), Some(planner)) => Some(live.max(planner)),
        (rows, planner) => rows.or(planner),
    }
    .filter(|r| *r >= 1.0)
}

pub fn large_rows(snapshot: &Snapshot) -> Vec<Finding> {
    let mut findings: Vec<Finding> = snapshot
        .tables
        .iter()
        .filter_map(|t| {
            let rows = table_rows(t)?;
            let (size, large) = split_bytes(t)?;
            // Dead row versions (not yet vacuumed) keep their large values on disk too: a queue whose deleted payloads
            // wait for VACUUM isn't a table of large rows. Per version, they must still be large.
            let versions = rows + t.dead_rows.unwrap_or(0.0).max(0.0);
            (size >= LARGE_ROWS_TABLE_BYTES && size / versions >= LARGE_ROW_BYTES && large / size >= LARGE_VALUE_SHARE)
                .then(|| large_rows_finding(snapshot, t, rows, size, large))
        })
        .collect();
    findings.sort_by(|a, b| b.impact.score.total_cmp(&a.impact.score));
    findings.truncate(5);
    findings
}

pub(super) fn large_rows_finding(snapshot: &Snapshot, t: &TableStat, rows: f64, size: f64, large: f64) -> Finding {
    let engine = snapshot.server.engine;
    let name = t.qualified();
    let average = size / rows;
    let share = large / size;
    let mut f = finding("large-rows", &name, Category::Maintenance, subject(SubjectKind::Table, &name));
    f.title = format!(
        "{} holds {} in {} rows: {} a row, mostly large values",
        t.name,
        format::bytes(size),
        format::count(rows),
        format::bytes(average)
    );

    // The columns that can hold them, from the catalog: nothing is sampled.
    let mut columns: Vec<_> = snapshot
        .columns
        .iter()
        .filter(|c| {
            c.schema.eq_ignore_ascii_case(&t.schema)
                && c.table.eq_ignore_ascii_case(&t.name)
                && large_value_type(engine, &c.data_type)
        })
        .collect();
    columns.sort_by_key(|c| c.ordinal);
    let column_names: Vec<String> = columns.iter().take(6).map(|c| c.name.clone()).collect();
    // Which of them hold the bytes isn't known without reading the data: these are the columns that can.
    let held_in = match (column_names.as_slice(), columns.len()) {
        ([], _) => String::new(),
        ([one], 1) => format!(" (the column that can hold them: {one})"),
        (names, n) if n > names.len() => {
            format!(" (columns that can hold them: {} and {} more)", names.join(", "), n - names.len())
        }
        (names, _) => format!(" (columns that can hold them: {})", format::list(names)),
    };
    let mut summary = format!(
        "The {} rows of {} take {}: {} each on average, and {} of that is large values kept apart from the rows{held_in}. Every statement that reads whole rows pulls them in, every insert or update that sets them writes them again, and they keep their space until the rows are deleted and purged.",
        format::count(rows),
        t.name,
        format::bytes(size),
        format::bytes(average),
        share_words(share)
    );
    if engine == Engine::Postgres {
        summary.push_str(" PostgreSQL has already compressed them: these are the sizes on disk.");
    }

    // Dead versions hold their values until VACUUM: say how much of the table they are.
    if let Some(dead) = dead_share(t).filter(|d| *d >= 0.2) {
        summary.push_str(&format!(
            " {} of its row versions are dead rows not yet vacuumed, and their large values still take space until VACUUM clears them.",
            format::percent(dead)
        ));
    }

    // Statements reading whole rows: what makes the size cost on every run, not just on disk.
    let statements = whole_row_statements(snapshot, t, size, large);
    let runs: f64 = statements.iter().map(|q| q.executions).sum();
    let hot = runs_at_least(snapshot, &statements, WHOLE_ROW_RUNS_PER_DAY);
    // A day's worth only where every statement's runs came that way.
    let runs_a_day = snapshot
        .workload
        .as_ref()
        .filter(|w| statements.iter().all(|q| w.runs_per_day_of(q).is_some()))
        .and_then(|w| w.per_day_measured(runs));
    summary.push_str(&statements_below(snapshot, &statements));

    // Growth since the newest scan at least 20 hours back, when there is one: named by its time, since it is not
    // always the scan just before.
    let growth = snapshot.sizes_before.as_ref().and_then(|b| {
        let then = b.bytes(&name)?;
        let days = days_between(&b.taken_at, &snapshot.taken_at).filter(|d| *d >= 1.0 / 24.0)?;
        Some((b, size - then, days))
    });
    let mut fast = false;
    if let Some((before, delta, days)) = growth {
        let when = crate::queries::when_phrase(&before.taken_at);
        let a_day = delta / days;
        fast = a_day >= FAST_GROWTH_BYTES && a_day >= FAST_GROWTH_SHARE * size;
        summary.push(' ');
        summary.push_str(&if delta >= MB && days >= 20.0 / 24.0 {
            format!("It grew by {} since the scan {when}, about {} a day.", format::bytes(delta), format::bytes(a_day))
        } else if delta >= MB {
            format!(
                "It grew by {} in the {} since the scan {when}.",
                format::bytes(delta),
                culprit_core::span_words(days * 24.0)
            )
        } else if delta <= -MB {
            format!("It's {} smaller than at the scan {when}.", format::bytes(-delta))
        } else {
            format!("It hasn't grown since the scan {when}.")
        });
        if delta >= MB && days >= 20.0 / 24.0 {
            f.metrics.push(Metric::new("Growth a day", a_day, Unit::Bytes));
        }
    }
    f.summary = summary;

    let weight = usize::from(hot) + usize::from(fast) + usize::from(size >= BIG_TABLE_BYTES);
    f.severity = match weight {
        0 => Severity::Low,
        1 => Severity::Medium,
        _ => Severity::High,
    };
    f.confidence = Confidence::High;
    if let Some(impact) = query_impact(snapshot, &statements) {
        f.impact.db_time_share = Some(impact.share);
        f.impact.time_ms_per_day = impact.per_day_ms;
    }
    severity_score(&mut f);
    f.impact.label = format!("{}, {} a row", format::bytes(size), format::bytes(average));
    let mut metrics = vec![
        Metric::new("Table data", size, Unit::Bytes),
        Metric::new("Rows", rows, Unit::Rows),
        Metric::new("Average row", average, Unit::Bytes),
        Metric::new("In large-value pages", share, Unit::Ratio),
    ];
    if let Some(runs_a_day) = runs_a_day.filter(|_| !statements.is_empty()) {
        metrics.push(Metric::new("Whole-row statements a day", runs_a_day, Unit::Count));
    }
    metrics.append(&mut f.metrics);
    f.metrics = metrics;
    f.queries = query_refs(&statements);
    f.last_seen = statements.iter().filter_map(|q| q.last_seen.clone()).max();

    let compress = compress_advice(engine);
    f.fix = Some(Fix {
        summary: format!(
            "Read only the columns a statement needs: a SELECT * or an ORM loading whole rows pulls every large value. Keep a value that many rows repeat in one place and point to it. Clear values a row no longer needs once its work is done. Check how long deleted rows are kept before they're purged: until then they keep their values. For values that must stay, {compress}."
        ),
        sql: None,
        cost: None,
    });
    // On running totals a statement's pages a run hold every run from before the change; What changed shows its time
    // a run between two scans, never its reads.
    let statements_read_less = match &snapshot.workload {
        _ if statements.is_empty() => String::new(),
        Some(w) if !w.windowed => format!(
            ", and the statements below should take less time a run between a scan taken when it ships and one a day or more later (What changed, `{}`)",
            crate::queries::WHAT_CHANGED_COMMAND
        ),
        _ => ", and the statements below should read fewer pages a run".to_string(),
    };
    f.verify = vec![
        format!(
            "Re-scan after the change: the table's average row and its share of large values should drop{statements_read_less}."
        ),
        "A later scan says whether it still grows: it compares the table's size with this scan's.".into(),
    ];
    f.caveats = vec!["Sizes are the pages the table has reserved, from the catalog: they include space allocated but not yet filled.".into()];
    f
}
