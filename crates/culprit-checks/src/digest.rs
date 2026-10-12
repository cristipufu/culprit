//! The digest: one report, compared with the previous one when there is one, in the few facts a scheduled job posts.

use culprit_core::{
    Digest, DigestChanges, DigestCounts, DigestFinding, Environment, Finding, Report, Severity, TileKey,
};

/// What a report says, and what changed since `previous` (an earlier report of the same database). `top` caps every
/// list of findings.
pub fn digest(report: &Report, previous: Option<&Report>, top: usize) -> Digest {
    let board = crate::dashboard(report);
    let active: Vec<&Finding> = report.findings.iter().filter(|f| !f.inactive).collect();
    let count = |s: Severity| active.iter().filter(|f| f.severity == s).count() as u32;
    let since = previous.map(|p| {
        let before: Vec<&Finding> = p.findings.iter().filter(|f| !f.inactive).collect();
        let was = |id: &str| before.iter().find(|f| f.id == id).copied();
        DigestChanges {
            taken_at: p.snapshot.taken_at.clone(),
            new: active.iter().filter(|f| was(&f.id).is_none()).take(top).map(|f| item(f, None)).collect(),
            fixed: before
                .iter()
                .filter(|f| !active.iter().any(|a| a.id == f.id))
                .take(top)
                .map(|f| item(f, None))
                .collect(),
            worse: active
                .iter()
                .filter_map(|f| was(&f.id).filter(|b| f.severity > b.severity).map(|b| item(f, Some(b.severity))))
                .take(top)
                .collect(),
        }
    });
    Digest {
        connection: report.snapshot.connection.clone(),
        server: board.product.clone(),
        database: board.database.clone(),
        environment: Some(environment_name(board.environment).to_string()),
        taken_at: report.snapshot.taken_at.clone(),
        window: board.coverage.clone(),
        headline: report.summary.headline.clone(),
        database_time: board.tiles.iter().find(|t| t.key == TileKey::DbTime).map(|t| t.takeaway.clone()),
        counts: DigestCounts {
            active: active.len() as u32,
            high: count(Severity::High),
            medium: count(Severity::Medium),
            low: count(Severity::Low),
            stopped: report.findings.iter().filter(|f| f.inactive).count() as u32,
        },
        since,
        top: active.iter().take(top).map(|f| item(f, None)).collect(),
    }
}

/// As connections spell it (`--env staging`).
fn environment_name(environment: Environment) -> &'static str {
    match environment {
        Environment::Production => "production",
        Environment::Staging => "staging",
        Environment::Development => "development",
        Environment::Local => "local",
    }
}

fn item(f: &Finding, was: Option<Severity>) -> DigestFinding {
    DigestFinding {
        id: f.id.clone(),
        title: f.title.clone(),
        severity: f.severity,
        db_time_share: f.impact.db_time_share,
        impact: f.impact.label.clone(),
        was,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report() -> Report {
        let text = std::fs::read_to_string(format!("{}/../../fixtures/mssql-demo.json", env!("CARGO_MANIFEST_DIR")))
            .expect("fixture");
        crate::analyze(serde_json::from_str(&text).expect("snapshot"))
    }

    #[test]
    fn a_first_digest_counts_and_ranks_without_changes() {
        let r = report();
        let d = digest(&r, None, 3);
        let active = r.findings.iter().filter(|f| !f.inactive).count() as u32;
        assert_eq!(d.counts.active, active);
        assert_eq!(d.counts.high + d.counts.medium + d.counts.low, active);
        assert_eq!(d.top.len(), 3.min(active as usize));
        assert_eq!(d.top[0].id, r.findings.iter().find(|f| !f.inactive).unwrap().id);
        assert!(d.since.is_none());
        assert_eq!(d.headline, r.summary.headline);
        assert!(d.database_time.is_some());
    }

    #[test]
    fn a_digest_reports_what_is_new_fixed_and_worse_since_the_previous_one() {
        let now = report();
        let mut before = now.clone();
        before.snapshot.taken_at = "2026-10-10T06:00:00Z".into();
        let gone = before.findings.remove(0);
        let mut old = now.findings[1].clone();
        old.id = "unused-index:shop.old_table".into();
        before.findings.push(old.clone());
        // A finding that is medium or high now was low before.
        let rising = before.findings.iter().position(|f| !f.inactive && f.severity > Severity::Low).expect("one");
        before.findings[rising].severity = Severity::Low;
        let rising = before.findings[rising].id.clone();

        let changes = digest(&now, Some(&before), 10).since.expect("changes");
        assert_eq!(changes.taken_at, "2026-10-10T06:00:00Z");
        assert_eq!(changes.new.iter().map(|f| f.id.as_str()).collect::<Vec<_>>(), [gone.id.as_str()]);
        assert_eq!(changes.fixed.iter().map(|f| f.id.as_str()).collect::<Vec<_>>(), [old.id.as_str()]);
        assert_eq!(changes.worse.len(), 1);
        assert_eq!(changes.worse[0].id, rising);
        assert_eq!(changes.worse[0].was, Some(Severity::Low));
    }
}
