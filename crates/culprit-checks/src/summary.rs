//! The report's summary: counts by category and severity, the share of database time the top findings touch, and the
//! headline sentence.

use culprit_core::{Category, CategoryCount, Finding, Severity, SeverityCount, Snapshot, Summary, format};

use crate::support;

/// The report's summary for these findings: counts, the share the top ones touch and the headline.
pub fn summarize(snapshot: &Snapshot, findings: &[Finding]) -> Summary {
    let mut by_category: Vec<CategoryCount> = Vec::new();
    for category in [Category::Indexes, Category::Queries, Category::Maintenance, Category::Configuration] {
        let count = findings.iter().filter(|f| f.is_actionable() && f.category == category).count() as u32;
        if count > 0 {
            by_category.push(CategoryCount { category, count });
        }
    }
    let by_severity = [Severity::High, Severity::Medium, Severity::Low]
        .into_iter()
        .map(|severity| SeverityCount {
            severity,
            count: findings.iter().filter(|f| f.is_actionable() && f.severity == severity).count() as u32,
        })
        .filter(|c| c.count > 0)
        .collect();

    let top_share = support::top_findings_share(snapshot, findings.iter());
    // Over Culprit's scans where they measured every statement behind it, as the findings say it.
    let scans = support::top_findings_scans(snapshot, findings.iter());

    let cost = support::top_findings_cost(snapshot, findings.iter());
    let n = findings.iter().filter(|f| f.is_actionable()).count();
    let window = support::summary_window(snapshot);
    let headline = match (n, top_share, &window) {
        (0, _, Some(window)) => format!("Nothing stands out over {window}."),
        // No statements were examined: say what was.
        (0, _, None) if snapshot.workload.is_none() => {
            "Nothing stands out in the tables, indexes and settings; this scan has no query history.".to_string()
        }
        (0, _, None) => "Nothing stands out.".to_string(),
        (n, Some(share), Some(_)) if share >= 0.05 && cost.is_some() => format!(
            "{} {}. The top ones touch {}.",
            n,
            plural(n, "opportunity", "opportunities"),
            cost.as_deref().unwrap()
        ),
        (n, Some(share), Some(window)) if share >= 0.05 => format!(
            "{} {}. The top ones touch {} of database time over {}.",
            n,
            plural(n, "opportunity", "opportunities"),
            format::percent(share),
            scans.as_deref().unwrap_or(window)
        ),
        (n, _, _) => format!("{} {}.", n, plural(n, "opportunity", "opportunities")),
    };
    let stopped = Stopped::of(findings);
    let headline = match (stopped.statements, stopped.unnamed) {
        (0, 0) => headline,
        (s, 0) => format!(
            "{headline} {s} more {} stopped running: a deploy may have changed {}.",
            plural(s, "statement", "statements"),
            plural(s, "it", "them")
        ),
        (0, u) => format!(
            "{headline} {u} more {} on statements that stopped running: a deploy may have changed them.",
            plural(u, "finding rests", "findings rest")
        ),
        (s, u) => format!(
            "{headline} {s} more {} stopped running, and {u} more {} on statements that did: a deploy may have \
             changed them.",
            plural(s, "statement", "statements"),
            plural(u, "finding rests", "findings rest")
        ),
    };
    Summary {
        opportunities: n as u32,
        by_category,
        by_severity,
        top_share,
        headline,
        no_workload: snapshot.no_workload_reason(),
        runs_said: runs_said(snapshot),
        over_scans: over_scans(snapshot),
    }
}

/// What Culprit's scans measured of the statements they did (`Summary::over_scans`), as every surface states it.
fn over_scans(snapshot: &Snapshot) -> Vec<culprit_core::StatementCost> {
    let Some(w) = snapshot.workload.as_ref() else { return Vec::new() };
    w.queries
        .iter()
        .filter(|q| w.scans_of(q).is_some() || w.observed_activity(q).is_some())
        .map(|q| w.cost_of(q))
        .collect()
}

/// The statements whose runs a rate a day would misstate (`Summary::runs_said`), said as the brief says them: a burst
/// by when it ran, fewer runs than the window has days as a count.
fn runs_said(snapshot: &Snapshot) -> Vec<culprit_core::RunsSaid> {
    let Some(w) = snapshot.workload.as_ref() else { return Vec::new() };
    w.queries
        .iter()
        .filter(|q| {
            w.cadence(q).is_burst()
                || (w.window_hours >= culprit_core::PER_DAY_MIN_HOURS && w.runs_per_day_of(q).is_none())
        })
        .map(|q| culprit_core::RunsSaid { query: q.id.clone(), runs: w.cost_of(q).runs })
        .collect()
}

fn plural<'a>(n: usize, one: &'a str, many: &'a str) -> &'a str {
    if n == 1 { one } else { many }
}

/// The statements behind findings that stopped running, counted as statements: one statement with a heavy-query and an
/// unstable-query finding is one. A finding that names none (an index the optimizer stopped asking for) is counted on
/// its own.
pub(crate) struct Stopped {
    /// Distinct statements the inactive findings name.
    pub statements: usize,
    /// Inactive findings that name no statement.
    pub unnamed: usize,
}

impl Stopped {
    pub(crate) fn of(findings: &[Finding]) -> Stopped {
        let inactive: Vec<&Finding> = findings.iter().filter(|f| f.inactive).collect();
        let mut ids: Vec<&str> = inactive.iter().flat_map(|f| f.queries.iter().map(|r| r.id.as_str())).collect();
        ids.sort_unstable();
        ids.dedup();
        Stopped { statements: ids.len(), unnamed: inactive.iter().filter(|f| f.queries.is_empty()).count() }
    }
}

#[cfg(test)]
mod window_tests {
    use culprit_core::Engine;

    use super::summarize;
    use crate::waits::snapshot_with;

    /// A scan with `--window 6h` reads about six hours of Query Store intervals, and the headline says so.
    #[test]
    fn the_headline_names_an_hours_window() {
        let mut snapshot = snapshot_with(Engine::Mssql);
        let workload = snapshot.workload.as_mut().expect("fixture has a workload");
        workload.windowed = true;
        workload.window_hours = 6.0;
        assert_eq!(summarize(&snapshot, &[]).headline, "Nothing stands out over the last 6 hours.");
    }

    /// Query Store keeps whole hourly intervals, so `--window 6h` reads the interval it starts in too: the headline names
    /// the window asked for, and the rounding.
    #[test]
    fn the_headline_names_the_window_asked_for_and_the_rounding() {
        let mut snapshot = snapshot_with(Engine::Mssql);
        snapshot.options = Some(culprit_core::SnapshotOptions::default().with_window_minutes(360));
        let workload = snapshot.workload.as_mut().expect("fixture has a workload");
        workload.windowed = true;
        workload.window_hours = 6.9;
        assert_eq!(
            summarize(&snapshot, &[]).headline,
            "Nothing stands out over the last 6 hours (Query Store keeps whole intervals, so this covers 7)."
        );
        // Rounding that doesn't show isn't mentioned.
        snapshot.workload.as_mut().unwrap().window_hours = 6.3;
        assert_eq!(summarize(&snapshot, &[]).headline, "Nothing stands out over the last 6 hours.");
        // Less data than asked for: what it covers.
        snapshot.workload.as_mut().unwrap().window_hours = 2.0;
        assert_eq!(summarize(&snapshot, &[]).headline, "Nothing stands out over the last 2 hours.");
    }
}
