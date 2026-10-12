//! The digest: one report, compared with the previous one when there is one, in the few facts a scheduled job posts.

use culprit_core::{
    Digest, DigestChanges, DigestCounts, DigestFinding, Environment, Finding, Report, Severity, TileKey,
};

/// What a report says, and what changed since `previous` (an earlier report of the same database). `top` caps every
/// list of findings.
pub fn digest(report: &Report, previous: Option<&Report>, top: usize) -> Digest {
    let baseline = previous.map(|p| {
        let findings = p.findings.iter().filter(|f| !f.inactive).map(|f| item(f, None)).collect::<Vec<_>>();
        (p.snapshot.taken_at.clone(), findings)
    });
    build(report, baseline, top)
}

/// The same, compared with a previous digest instead of a report: what a scheduled job keeps between runs, since a
/// digest holds no statement text or parameter values and a report does.
pub fn digest_since(report: &Report, previous: &Digest, top: usize) -> Digest {
    build(report, Some((previous.taken_at.clone(), previous.findings.clone())), top)
}

fn build(report: &Report, baseline: Option<(String, Vec<DigestFinding>)>, top: usize) -> Digest {
    let board = crate::dashboard(report);
    let active: Vec<&Finding> = report.findings.iter().filter(|f| !f.inactive).collect();
    let count = |s: Severity| active.iter().filter(|f| f.severity == s).count() as u32;
    let since = baseline.map(|(taken_at, before)| {
        let was = |id: &str| before.iter().find(|f| f.id == id);
        DigestChanges {
            taken_at,
            new: active.iter().filter(|f| was(&f.id).is_none()).take(top).map(|f| item(f, None)).collect(),
            fixed: before.iter().filter(|f| !active.iter().any(|a| a.id == f.id)).take(top).cloned().collect(),
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
        findings: active.iter().map(|f| item(f, None)).collect(),
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
    fn a_previous_digest_is_as_good_a_baseline_as_its_report() {
        let now = report();
        let mut before = now.clone();
        before.snapshot.taken_at = "2026-10-10T06:00:00Z".into();
        let gone = before.findings.remove(0);
        let from_report = digest(&now, Some(&before), 10).since.expect("changes");
        let previous = digest(&before, None, 3);
        assert_eq!(previous.findings.len(), before.findings.iter().filter(|f| !f.inactive).count());
        let from_digest = digest_since(&now, &previous, 10).since.expect("changes");
        assert_eq!(from_digest, from_report);
        assert_eq!(from_digest.new[0].id, gone.id);
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
