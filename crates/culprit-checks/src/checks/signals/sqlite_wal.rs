use super::*;
use crate::evidence::{Observation, Requirement as Evidence};
use culprit_core::Actionability;
pub fn sqlite_wal_info() -> CheckInfo {
    CheckInfo{id:"sqlite-wal".into(),title:"SQLite WAL file size and growth".into(),category:Category::Maintenance,engines:vec![Engine::Sqlite],requires:vec![],description:"Read-only WAL sidecar metadata and changes between scans; retained allocation is not inferred to be a checkpoint backlog or a blocked reader.".into()}
}
check!(SqliteWalCheck: sqlite_wal_info, sqlite_wal, &[Evidence::Observed(Observation::WalSize, &["wal_file"])]);
pub fn sqlite_wal(s: &Snapshot) -> Vec<Finding> {
    let Some(w) = s.operations.sqlite.as_ref() else { return vec![] };
    let Some(bytes) = w.wal_bytes.filter(|b| b.is_finite() && *b >= 0.0) else { return vec![] };
    let before = w
        .before
        .as_ref()
        .filter(|b| parse_epoch_seconds(&b.at).zip(parse_epoch_seconds(&s.taken_at)).is_some_and(|(a, b)| a < b));
    let growth = before.map(|p| bytes - p.count);
    if bytes < 64.0 * 1024.0 * 1024.0 && growth.is_none_or(|g| g < 16.0 * 1024.0 * 1024.0) {
        return vec![];
    }
    let mut f = finding(
        "sqlite-wal",
        &s.server.database,
        Category::Maintenance,
        subject(SubjectKind::Database, &s.server.database),
    );
    f.title = format!("The SQLite WAL file occupies {}", format::bytes(bytes));
    f.summary = format!(
        "The sidecar's allocated file length was read without checkpointing or inspecting contents. {} A WAL can retain space after checkpointing and reuse it. Size or growth alone does not establish uncheckpointed work, checkpoint starvation, a long reader or disk exhaustion.",
        before
            .map(|p| format!(
                "It changed by {}{} since {}.",
                if growth.unwrap_or(0.0) < 0.0 { "−" } else { "" },
                format::bytes(growth.unwrap_or(0.0).abs()),
                p.at
            ))
            .unwrap_or_else(|| "No earlier comparable size was recorded.".into())
    );
    f.severity = Severity::Low;
    f.actionability = Actionability::Information;
    f.confidence = Confidence::High;
    f.impact.score = base_score(f.severity);
    f.impact.label = "File allocation context".into();
    f.metrics = vec![Metric::new("WAL file length", bytes, Unit::Bytes)];
    f.fix=Some(Fix{summary:"If this accompanies capacity or latency trouble, inspect the application's checkpoint results and open read transactions on its own connections. Confirm filesystem space and checkpoint policy. Do not delete the WAL or force a checkpoint just to reduce this number; live backups must preserve a consistent database/WAL state.".into(),sql:None,cost:None});
    f.verify=vec!["Compare WAL allocation across scans and correlate it with application checkpoint progress, latency and filesystem headroom. Reuse of a stable allocated WAL can be healthy; shrinking is not required.".into()];
    vec![f]
}
