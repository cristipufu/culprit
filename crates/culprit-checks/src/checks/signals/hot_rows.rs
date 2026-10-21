//! The `hot-rows` check: Indexes where sessions waited a long time for row and page locks, and where locks escalated to the whole table (sys.dm_db_index_operational_stats). MySQL, MariaDB and PostgreSQL keep no lock waits per table; see lock-time and Live.

use super::*;
use crate::evidence::Requirement as Evidence;

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

check!(HotRowsCheck: hot_rows_info, hot_rows, &[Evidence::Source("index contention")]);

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
            f.verification_basis = culprit_core::VerificationBasis::CounterChange;
            let escalations: f64 = indexes.iter().map(|(_, c)| c.lock_escalations).sum();
            let measured = snapshot.contention_before.as_ref().and_then(|b| {
                let hours = days_between(&b.taken_at, &snapshot.taken_at)? * 24.0;
                if hours < 0.0 { return None; }
                let delta = snapshot.indexes_on(&schema, &table).filter(|i| i.contention.is_some())
                    .try_fold((0.0, 0.0), |(n, ms), i| b.since(i).map(|d| (n + d.waits, ms + d.wait_ms)))?;
                Some((b, hours, delta.0, delta.1))
            });
            let in_window = measured.is_some_and(|(_, hours, _, _)| {
                snapshot.options.as_ref().is_none_or(|o| hours <= f64::from(o.window_in_minutes()) / 60.0 + 1.0 / 3600.0)
            });
            let unchanged = measured.is_some_and(|(_, _, n, ms)| n == 0.0 && ms == 0.0);
            let (shown_waits, shown_ms) = measured.map_or((waits, wait_ms), |(_, _, n, ms)| (n, ms));
            let avg = per_run(shown_ms, shown_waits);
            f.title = if let Some((b, _, n, ms)) = measured {
                if unchanged {
                    format!("No new lock waits on {table} since {}", crate::queries::short_when(&b.taken_at))
                } else if n == 0.0 {
                    format!("Recorded lock wait time on {table} grew by {} since {}", format::duration(ms), crate::queries::short_when(&b.taken_at))
                } else if n == 1.0 {
                    format!("One lock wait on {table} took {} since {}", format::duration(ms), crate::queries::short_when(&b.taken_at))
                } else {
                    format!("{} lock waits on {table} averaged {} since {}", format::count(n), format::duration(avg), crate::queries::short_when(&b.taken_at))
                }
            } else if waits == 1.0 {
                format!("One historical lock wait on {table} took {}", format::duration(wait_ms))
            } else {
                format!("{table} has {} recorded historical lock waits", format::count(waits))
            };
            let names: Vec<String> = indexes.iter().map(|(i, _)| i.name.clone()).collect();
            let on = match names.as_slice() {
                [one] => format!("Since its index {one} was loaded into memory, sessions waited"),
                _ => format!("Since its indexes were loaded into memory, sessions waited on {}", format::list(&names)),
            };
            let escalated = if escalations > 0.0 {
                format!(" {} times SQL Server escalated many row locks to a lock on the whole table, which can block sessions needing incompatible locks.", format::count(escalations))
            } else {
                String::new()
            };
            f.summary = format!(
                "{on} {} times for locks on the rows and pages of {table}, {} in all, {} each on average.{escalated} \
                 Waits like these come from transactions that hold rows for long, or from writes whose filter isn't indexed and so lock more rows than they change.",
                format::count(waits),
                total_wait(wait_ms),
                format::duration(per_run(wait_ms, waits))
            );
            if let Some((b, _, n, ms)) = measured {
                f.summary = format!("Between {} and this scan, {} lock waits totaling {} were recorded on {table}. {}{}",
                    crate::queries::when_phrase(&b.taken_at), format::count(n), format::duration(ms),
                    if unchanged { "The earlier waits do not establish contention in the selected window. " } else { "" },
                    f.summary);
                if !in_window && !unchanged {
                    f.summary.push_str(" This counter interval extends beyond the selected window, so the scan cannot place these waits inside that window.");
                }
            } else {
                f.summary.push_str(" No comparable earlier counters place these waits in the selected window or establish that contention continues.");
            }
            let touching = queries_touching(snapshot, &schema, &table);
            let mut locked: Vec<&QueryStat> = touching.into_iter().filter(|q| lock_ms(q) > 0.0).collect();
            locked.sort_by(|a, b| lock_ms(b).total_cmp(&lock_ms(a)));
            locked.truncate(3);
            f.queries = query_refs(&locked);
            // Counted since the indexes were loaded, with no time for each wait: graded on a rate measured over an
            // hour or more, never on the total.
            let rate = measured.filter(|(_, h, _, _)| in_window && *h >= culprit_core::RATE_MIN_HOURS)
                .map(|(_, h, _, ms)| ms * 24.0 / h);
            let amount = measured.filter(|_| in_window).map(|(_, h, _, ms)| {
                if h >= culprit_core::RATE_MIN_HOURS { ms * 24.0 / h } else { ms }
            });
            f.severity = match amount {
                Some(r) if r >= HOT_ROWS_HIGH_MS_PER_DAY => Severity::High,
                Some(r) if r >= HOT_ROWS_MEDIUM_MS_PER_DAY && avg >= 1_000.0 => Severity::Medium,
                _ => Severity::Low,
            };
            severity_score(&mut f);
            f.impact.label = format!("{} waiting on locks", total_wait(shown_ms));
            if unchanged {
                f.actionability = culprit_core::Actionability::History;
                f.impact.label = "No new lock waits".into();
            } else if !in_window {
                f.informational();
            }
            f.confidence = Confidence::High;
            f.metrics = vec![
                Metric::new("Historical lock waits", waits, Unit::Count),
                Metric::new("Historical time waiting", wait_ms, Unit::Ms),
                Metric::new("Historical average wait", per_run(wait_ms, waits), Unit::Ms),
            ];
            if let Some((_, h, n, ms)) = measured {
                f.metrics.push(Metric::new("Lock waits since earlier scan", n, Unit::Count));
                f.metrics.push(Metric::new("Time waiting since earlier scan", ms, Unit::Ms));
                f.metrics.push(Metric::new("Counter interval", h / 24.0, Unit::Days));
            }
            if let Some(r) = rate.filter(|_| measured.is_some_and(|(_, h, _, _)| h >= culprit_core::PER_DAY_MIN_HOURS)) {
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
            if !f.is_actionable() {
                f.fix = Some(Fix { summary: "These counters alone justify no change. If the application experiences blocking, capture the current blocked statement and its blocker under representative load before changing transactions or indexes.".into(), sql: None, cost: None });
            }
            f.verify = vec![format!("Compare new lock waits and their time between scans under comparable load on {table}. An unchanged counter reports no new waits; an idle interval alone does not verify a fix.")];
            f.caveats.push("Counted since the indexes' metadata was loaded into memory, usually since SQL Server started.".into());
            if measured.is_some_and(|(_, h, _, _)| h < culprit_core::RATE_MIN_HOURS) {
                f.caveats.push("The comparison covers less than an hour: only its measured total is graded, without extrapolating a daily rate.".into());
            }
            f
        })
        .collect();
    findings.sort_by(|a, b| b.impact.score.total_cmp(&a.impact.score));
    findings.truncate(5);
    findings
}
