//! A running statement on another backend: pg_stat_activity for state, waits and elapsed time, the pg_stat_progress_*
//! views for the commands that report progress, and the statement's estimated plan (EXPLAIN, never ANALYZE).

use culprit_core::{Engine, Error, QueryProgress, Result, session_number};

use crate::connect::{Connector, INTERACTIVE};
use crate::rows::{Row, iso, query_one};

/// Postgres doesn't count rows per operator while a query runs; only these commands report how far along they are.
pub(crate) const NO_OPERATOR_PROGRESS: &str = "Postgres doesn't report how far a running query has got, operator by \
operator. Culprit shows the plan Postgres estimated for it and how long it has been running. CREATE INDEX, VACUUM, \
ANALYZE, CLUSTER and COPY report their own progress.";

struct Phase {
    view: &'static str,
    sql: &'static str,
    describe: fn(&Row) -> (String, Option<f64>),
}

fn ratio(done: Option<f64>, total: Option<f64>) -> Option<f64> {
    match (done, total) {
        (Some(d), Some(t)) if t > 0.0 => Some((d / t).clamp(0.0, 1.0)),
        _ => None,
    }
}

fn phase_text(r: &Row, command: &str) -> String {
    match r.text("phase").filter(|p| !p.is_empty()) {
        Some(phase) => format!("{command}: {phase}"),
        None => command.to_string(),
    }
}

const PHASES: &[Phase] = &[
    Phase {
        view: "pg_stat_progress_create_index",
        sql: "SELECT command, phase, blocks_total, blocks_done, tuples_total, tuples_done FROM pg_stat_progress_create_index WHERE pid = {pid}",
        describe: |r| {
            // Scanning reports blocks, loading reports tuples; sorting reports nothing, so it gets no number.
            let phase = r.text("phase").unwrap_or_default();
            let fraction = if phase.contains("scanning") {
                ratio(r.number("blocks_done"), r.number("blocks_total"))
            } else {
                ratio(r.number("tuples_done"), r.number("tuples_total")).filter(|f| *f > 0.0)
            };
            (phase_text(r, &r.text("command").unwrap_or_else(|| "CREATE INDEX".into())), fraction)
        },
    },
    Phase {
        view: "pg_stat_progress_vacuum",
        sql: "SELECT phase, heap_blks_total, heap_blks_scanned FROM pg_stat_progress_vacuum WHERE pid = {pid}",
        describe: |r| (phase_text(r, "VACUUM"), ratio(r.number("heap_blks_scanned"), r.number("heap_blks_total"))),
    },
    Phase {
        view: "pg_stat_progress_analyze",
        sql: "SELECT phase, sample_blks_total, sample_blks_scanned FROM pg_stat_progress_analyze WHERE pid = {pid}",
        describe: |r| (phase_text(r, "ANALYZE"), ratio(r.number("sample_blks_scanned"), r.number("sample_blks_total"))),
    },
    Phase {
        view: "pg_stat_progress_cluster",
        sql: "SELECT command, phase, heap_blks_total, heap_blks_scanned FROM pg_stat_progress_cluster WHERE pid = {pid}",
        describe: |r| {
            (
                phase_text(r, &r.text("command").unwrap_or_else(|| "CLUSTER".into())),
                ratio(r.number("heap_blks_scanned"), r.number("heap_blks_total")),
            )
        },
    },
    Phase {
        view: "pg_stat_progress_copy",
        sql: "SELECT command, type, bytes_processed, bytes_total, tuples_processed FROM pg_stat_progress_copy WHERE pid = {pid}",
        describe: |r| {
            let what = format!(
                "{} {}",
                r.text("command").unwrap_or_else(|| "COPY".into()),
                r.text("type").unwrap_or_default()
            );
            let rows = r.number("tuples_processed").map(|t| format!(" · {} rows so far", t as u64)).unwrap_or_default();
            (format!("{}{rows}", what.trim()), ratio(r.number("bytes_processed"), r.number("bytes_total")))
        },
    },
];

/// State, waits, elapsed time and command progress. The caller adds the estimated plan when asked for it.
pub(crate) async fn query_progress(connector: &Connector, session: &str) -> Result<QueryProgress> {
    let pid =
        i32::try_from(session_number(session)?).map_err(|_| Error::invalid(format!("'{session}' is not a pid")))?;
    let client = connector.connect(INTERACTIVE).await?;
    let now = query_one(&client, &format!("SELECT {} AS now", iso("now()")))
        .await?
        .map(|r| r.string("now"))
        .unwrap_or_default();
    let mut progress = QueryProgress::idle(session, Engine::Postgres, now);
    let Some(activity) = query_one(
        &client,
        &format!(
            "SELECT state, wait_event_type, wait_event, query, backend_type,
                    extract(epoch FROM now() - query_start) * 1000 AS elapsed_ms
               FROM pg_stat_activity WHERE pid = {pid}"
        ),
    )
    .await?
    else {
        return Ok(progress);
    };
    progress.state = activity.text("state").or_else(|| activity.text("backend_type"));
    progress.running = activity.text("state").as_deref() == Some("active");
    progress.statement = activity.text("query").map(|q| q.trim().to_string()).filter(|q| !q.is_empty());
    if !progress.running {
        return Ok(progress);
    }
    progress.elapsed_ms = activity.number("elapsed_ms");
    progress.wait = match (activity.text("wait_event_type"), activity.text("wait_event")) {
        (Some(kind), Some(event)) => Some(format!("{kind}: {event}")),
        (Some(kind), None) => Some(kind),
        _ => None,
    };
    for phase in PHASES {
        // Views that don't exist on this version (pg_stat_progress_copy before 14…) simply don't apply.
        if let Ok(Some(row)) = query_one(&client, &phase.sql.replace("{pid}", &pid.to_string())).await {
            let (text, fraction) = (phase.describe)(&row);
            progress.phase = Some(text);
            progress.phase_percent = fraction;
            progress.percent = fraction;
            progress.percent_basis =
                fraction.map(|_| format!("How far through this step, from Postgres ({})", phase.view));
            break;
        }
    }
    if progress.phase.is_none() {
        progress.note = Some(NO_OPERATOR_PROGRESS.to_string());
    }
    Ok(progress)
}
