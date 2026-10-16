//! The `hot-rows` check: Indexes where sessions waited a long time for row and page locks, and where locks escalated to the whole table (sys.dm_db_index_operational_stats). MySQL, MariaDB and PostgreSQL keep no lock waits per table; see lock-time and Live.

use super::*;

pub fn hot_rows_info() -> CheckInfo {
    CheckInfo {
        id: "hot-rows".into(),
        title: "Rows that many sessions fight over".into(),
        category: Category::Indexes,
        engines: vec![Engine::Mssql],
        requires: vec![],
        description: "Indexes where sessions waited a long time for row and page locks, and where locks escalated to the whole table (sys.dm_db_index_operational_stats). MySQL, MariaDB and PostgreSQL keep no lock waits per table; see lock-time and Live.".into(),
    }
}

check!(HotRowsCheck: hot_rows_info, hot_rows);

/// Lock waits a day on one table that make hot-rows High: ten minutes. A minute a day of waits a second or longer each
/// is Medium.
pub(super) const HOT_ROWS_HIGH_MS_PER_DAY: f64 = 600_000.0;
const HOT_ROWS_MEDIUM_MS_PER_DAY: f64 = 60_000.0;

pub fn hot_rows(snapshot: &Snapshot) -> Vec<Finding> {
    let rcsi = snapshot.setting("is_read_committed_snapshot_on").map(|s| crate::config::is_on(&s.value));
    // One finding per table: its indexes' waits are the same rows fought over, whichever index the lock was taken on.
    let mut by_table: BTreeMap<(String, String), Vec<(&IndexStat, &culprit_core::IndexContention)>> = BTreeMap::new();
    for i in &snapshot.indexes {
        if let Some(c) = i.contention.as_ref().filter(|c| c.lock_waits() >= 1.0 && c.lock_wait_ms() > 0.0) {
            by_table.entry((i.schema.clone(), i.table.clone())).or_default().push((i, c));
        }
    }
    let mut findings: Vec<Finding> = by_table
        .into_iter()
        .filter_map(|((schema, table), mut indexes)| {
            let waits: f64 = indexes.iter().map(|(_, c)| c.lock_waits()).sum();
            let wait_ms: f64 = indexes.iter().map(|(_, c)| c.lock_wait_ms()).sum();
            (wait_ms >= 10_000.0).then(|| {
                indexes.sort_by(|a, b| b.1.lock_wait_ms().total_cmp(&a.1.lock_wait_ms()));
                (schema, table, indexes, waits, wait_ms)
            })
        })
        .map(|(schema, table, indexes, waits, wait_ms)| {
            let name = format!("{schema}.{table}");
            // The subject is the index most of the waiting was on; the finding is the table's.
            let (top, _) = indexes[0];
            let mut f = finding("hot-rows", &name, Category::Indexes, subject(SubjectKind::Index, format!("{}.{}", index_label(top), top.name)));
            let escalations: f64 = indexes.iter().map(|(_, c)| c.lock_escalations).sum();
            let avg = per_run(wait_ms, waits);
            f.title = format!("Sessions wait {} each on row locks in {table}", format::duration(avg));
            let names: Vec<String> = indexes.iter().map(|(i, _)| i.name.clone()).collect();
            let on = match names.as_slice() {
                [one] => format!("Since {one} was loaded into memory, sessions waited"),
                _ => format!("Since its indexes were loaded into memory, sessions waited on {}", format::list(&names)),
            };
            let escalated = if escalations > 0.0 {
                format!(" {} times SQL Server escalated many row locks to a lock on the whole table, which blocks every other session.", format::count(escalations))
            } else {
                String::new()
            };
            f.summary = format!(
                "{on} {} times for locks on the rows and pages of {table}, {} in all, {} each on average.{escalated} \
                 Waits like these come from transactions that hold rows for long, or from writes whose filter isn't indexed and so lock more rows than they change.",
                format::count(waits),
                format::duration(wait_ms),
                format::duration(avg)
            );
            let touching = queries_touching(snapshot, &schema, &table);
            let mut locked: Vec<&QueryStat> = touching.into_iter().filter(|q| lock_ms(q) > 0.0).collect();
            locked.sort_by(|a, b| lock_ms(b).total_cmp(&lock_ms(a)));
            locked.truncate(3);
            f.queries = query_refs(&locked);
            // Counted since the indexes were loaded, with no time for each wait: graded on a rate measured over an
            // hour or more, never on the total.
            let rate = counter_rate_per_day(snapshot, wait_ms);
            f.severity = match rate {
                Some(r) if r >= HOT_ROWS_HIGH_MS_PER_DAY => Severity::High,
                Some(r) if r >= HOT_ROWS_MEDIUM_MS_PER_DAY && avg >= 1_000.0 => Severity::Medium,
                _ => Severity::Low,
            };
            severity_score(&mut f);
            f.impact.label = format!("{} waiting on locks", format::duration(wait_ms));
            f.confidence = Confidence::High;
            f.metrics = vec![
                Metric::new("Lock waits", waits, Unit::Count),
                Metric::new("Time waiting", wait_ms, Unit::Ms),
                Metric::new("Average wait", avg, Unit::Ms),
            ];
            if let Some(r) = rate.filter(|_| counter_days(snapshot).is_some_and(per_day_is_measured)) {
                f.metrics.push(Metric::new("Waiting a day", r, Unit::Ms));
            }
            if escalations > 0.0 {
                f.metrics.push(Metric::new(ESCALATIONS_METRIC, escalations, Unit::Count));
            }
            f.fix = Some(Fix {
                summary: format!(
                    "Keep the transactions that change {table} short and commit before slow work; make their WHERE use an index so they lock only the rows they change.{}",
                    if rcsi == Some(false) {
                        " If readers are the ones waiting, READ_COMMITTED_SNAPSHOT lets them read the last committed version instead: it changes what readers see and uses tempdb, so test it first."
                    } else {
                        ""
                    }
                ),
                sql: (rcsi == Some(false)).then(|| "-- Readers stop waiting for writers; test first\nALTER DATABASE CURRENT SET READ_COMMITTED_SNAPSHOT ON WITH ROLLBACK IMMEDIATE;".into()),
                cost: None,
            });
            f.verify = vec![format!("Re-scan a day later: lock waits on {table} should stop growing.")];
            f.caveats.push("Counted since the indexes' metadata was loaded into memory, usually since SQL Server started.".into());
            if rate.is_none() {
                f.caveats.push(history_caveat(snapshot, "lock waits"));
            }
            f
        })
        .collect();
    findings.sort_by(|a, b| b.impact.score.total_cmp(&a.impact.score));
    findings.truncate(5);
    findings
}
