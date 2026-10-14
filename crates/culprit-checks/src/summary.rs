//! The report's summary: counts by category and severity, the share of database time the top findings touch, and the
//! headline sentence.

use culprit_core::{Category, CategoryCount, Finding, Severity, SeverityCount, Snapshot, Summary, format};

use crate::support;

/// The report's summary for these findings: counts, the share the top ones touch and the headline.
pub fn summarize(snapshot: &Snapshot, findings: &[Finding]) -> Summary {
    let mut by_category: Vec<CategoryCount> = Vec::new();
    for category in [Category::Indexes, Category::Queries, Category::Maintenance, Category::Configuration] {
        let count = findings.iter().filter(|f| f.category == category).count() as u32;
        if count > 0 {
            by_category.push(CategoryCount { category, count });
        }
    }
    let by_severity = [Severity::High, Severity::Medium, Severity::Low]
        .into_iter()
        .map(|severity| SeverityCount {
            severity,
            count: findings.iter().filter(|f| f.severity == severity).count() as u32,
        })
        .filter(|c| c.count > 0)
        .collect();

    let top_share = support::top_findings_share(snapshot, findings.iter());

    let n = findings.iter().filter(|f| !f.inactive).count();
    let window = support::summary_window(snapshot);
    let headline = match (n, top_share, &window) {
        (0, _, Some(window)) => format!("Nothing stands out over {window}."),
        (0, _, None) => "Nothing stands out.".to_string(),
        (n, Some(share), Some(window)) if share >= 0.05 => format!(
            "{} {}. The top ones touch {} of database time over {window}.",
            n,
            plural(n, "opportunity", "opportunities"),
            format::percent(share)
        ),
        (n, _, _) => format!("{} {}.", n, plural(n, "opportunity", "opportunities")),
    };
    let fixed = findings.iter().filter(|f| f.inactive).count();
    let headline = match fixed {
        0 => headline,
        1 => format!("{headline} 1 more stopped running, likely changed by a deploy."),
        k => format!("{headline} {k} more stopped running, likely changed by a deploy."),
    };
    Summary { opportunities: n as u32, by_category, by_severity, top_share, headline }
}

fn plural<'a>(n: usize, one: &'a str, many: &'a str) -> &'a str {
    if n == 1 { one } else { many }
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
