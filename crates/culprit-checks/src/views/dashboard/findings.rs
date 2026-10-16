//! The dashboard's findings tile.

use super::*;

pub(super) fn findings(report: &Report) -> Tile {
    let active: Vec<&Finding> = report.findings.iter().filter(|f| !f.inactive).collect();
    let count = |sev: Severity| active.iter().filter(|f| f.severity == sev).count();
    let (high, medium, low) = (count(Severity::High), count(Severity::Medium), count(Severity::Low));
    let stopped = report.findings.len() - active.len();
    let span = report.snapshot.workload.as_ref().map(window_phrase);
    let status = if high > 0 {
        TileStatus::Bad
    } else if medium > 0 {
        TileStatus::Watch
    } else if low > 0 {
        TileStatus::Info
    } else {
        TileStatus::Good
    };
    let top: Vec<&Finding> = active.iter().take(3).copied().collect();
    // The same number the findings summary states: the top three's distinct queries, each counted once.
    let touched = top_findings_share(&report.snapshot, active.iter().copied()).unwrap_or(0.0);
    let takeaway = if active.is_empty() {
        match &span {
            Some(span) => format!("Nothing stands out over {span}."),
            None => "Nothing stands out.".into(),
        }
    } else if touched >= 0.05 {
        format!(
            "{}; the top {} touch {} of database time.",
            capitalized(&plural(active.len(), "opportunity", "opportunities")),
            if top.len() == 1 { "one".to_string() } else { top.len().to_string() },
            format::percent(touched)
        )
    } else if high > 0 {
        format!("{}, {high} of them high.", capitalized(&plural(active.len(), "opportunity", "opportunities")))
    } else {
        format!("{}, none of them high.", capitalized(&plural(active.len(), "opportunity", "opportunities")))
    };
    let mut tile = Tile::new(TileKey::Findings, status, takeaway);
    tile.metrics = vec![
        TileMetric::new("high", "High", Some(high as f64), high.to_string()),
        TileMetric::new("medium", "Medium", Some(medium as f64), medium.to_string()),
        TileMetric::new("low", "Low", Some(low as f64), low.to_string()),
    ];
    if stopped > 0 {
        tile.metrics.push(TileMetric::new("stopped", "Stopped running", Some(stopped as f64), stopped.to_string()));
        tile.detail = Some(format!(
            "{} about queries that stopped running, most likely changed by a deploy.",
            capitalized(&plural(stopped, "more is", "more are"))
        ));
    }
    tile.items = top
        .iter()
        .map(|f| {
            let mut item = TileItem::new(ItemKind::Finding, f.id.clone(), f.title.clone());
            item.value = Some(f.impact.label.clone());
            item.share = f.impact.db_time_share;
            item.status = Some(severity_status(f.severity));
            item.finding_id = Some(f.id.clone());
            item
        })
        .collect();
    tile
}
