//! The dashboard's deadlocks tile.

use super::*;

pub(super) fn deadlocks(report: &Report) -> Tile {
    let s = &report.snapshot;
    let family = family(&s.server);
    let Some(d) = &s.deadlocks else {
        let headline = if s.server.engine.has_server() {
            "This scan didn't read deadlocks.".to_string()
        } else {
            format!("{} has no deadlocks to read.", s.server.engine)
        };
        let mut t = Tile::new(TileKey::Deadlocks, TileStatus::Unavailable, headline);
        t.detail = Some(match s.notes.iter().find(|n| n.collector == "deadlocks") {
            Some(note) => note.message.clone(),
            None if !s.server.engine.has_server() => {
                format!("{} records no deadlocks: one writer at a time holds the file.", s.server.engine)
            }
            None => "Scan again to read the last day of deadlocks.".into(),
        });
        return t;
    };
    let finding_id = report.findings.iter().find(|f| f.check == "deadlocks").map(|f| f.id.clone());
    // This database's, as the check counts them: the server-wide reports carry other databases' too.
    let here: Vec<&culprit_core::Deadlock> =
        d.deadlocks.iter().filter(|dl| crate::deadlocks::in_this_database(dl, s)).collect();
    // Only a dated deadlock is "in the last day": InnoDB keeps its latest one however old it is.
    let dated = here.iter().filter(|dl| dl.at.is_some()).count();
    if !here.is_empty() && dated == 0 {
        let mut tile = Tile::new(
            TileKey::Deadlocks,
            TileStatus::Info,
            "The server's latest deadlock report doesn't say when it happened.".to_string(),
        );
        tile.detail = d.note.clone().or_else(|| Some(format!("Read from {}.", d.source)));
        tile.metrics =
            vec![TileMetric::new("count", "Undated reports", Some(here.len() as f64), here.len().to_string())];
        return tile;
    }
    if !here.is_empty() {
        // Same pattern on the same tables is the same problem.
        let mut groups: BTreeMap<(String, String), Vec<&culprit_core::Deadlock>> = BTreeMap::new();
        for dl in here.iter().filter(|dl| dl.at.is_some()) {
            groups.entry((dl.pattern.label().to_string(), dl.objects.join(", "))).or_default().push(dl);
        }
        let mut groups: Vec<((String, String), Vec<&culprit_core::Deadlock>)> = groups.into_iter().collect();
        groups.sort_by_key(|g| std::cmp::Reverse(g.1.len()));
        let n = dated;
        let ((pattern, objects), biggest) = &groups[0];
        let on = if objects.is_empty() { String::new() } else { format!(" on {objects}") };
        let when = last(d.since_hours);
        let takeaway = if n == 1 {
            format!("One deadlock in {when}: {pattern}{on}.")
        } else if biggest.len() == n {
            format!("{n} deadlocks in {when}, all {pattern}{on}.")
        } else if groups.len() == n {
            format!("{n} deadlocks in {when}, each a different kind.")
        } else {
            format!("{n} deadlocks in {when}; {} of them the same {pattern}{on}.", biggest.len())
        };
        // Bad where the check says High: by how often, not how many.
        let status = report
            .findings
            .iter()
            .filter(|f| f.check == "deadlocks" && !f.inactive)
            .map(|f| severity_status(f.severity))
            .max()
            .unwrap_or(TileStatus::Info);
        let mut tile = Tile::new(TileKey::Deadlocks, status, takeaway);
        tile.detail = d.note.clone().or_else(|| Some(format!("Read from {}.", d.source)));
        tile.metrics = vec![TileMetric::new("count", "Deadlocks", Some(n as f64), n.to_string())];
        tile.items = groups
            .iter()
            .take(3)
            .map(|((pattern, objects), list)| {
                let label = if objects.is_empty() {
                    capitalized(pattern)
                } else {
                    format!("{} on {objects}", capitalized(pattern))
                };
                let mut item = TileItem::new(ItemKind::Deadlock, list[0].id.clone(), label);
                item.value = Some(format!("{}×", list.len()));
                item.at = list.iter().filter_map(|dl| dl.at.clone()).max();
                item.status = Some(TileStatus::Watch);
                item.finding_id = finding_id.clone();
                item
            })
            .collect();
        return tile;
    }
    if let Some(counted) = d.counted {
        let since = d.counted_since.as_deref().and_then(|from| format::age_between(from, &s.taken_at));
        let over =
            since.as_ref().map(|age| format!(" in the {age} since its statistics were reset")).unwrap_or_default();
        let mut tile = if counted > 0.0 {
            // A total with no reset on record is from an unknown span: said, not graded.
            Tile::new(
                TileKey::Deadlocks,
                report
                    .findings
                    .iter()
                    .filter(|f| f.check == "deadlocks" && !f.inactive)
                    .map(|f| severity_status(f.severity))
                    .max()
                    .unwrap_or(TileStatus::Info),
                format!(
                    "{family} counted {} deadlock{}{over}; it keeps no details to explain them.",
                    format::count(counted),
                    if counted == 1.0 { "" } else { "s" }
                ),
            )
        } else {
            Tile::new(TileKey::Deadlocks, TileStatus::Good, format!("No deadlocks counted{over}."))
        };
        tile.detail = d.note.clone();
        tile.metrics = vec![TileMetric::new("counted", "Counted", Some(counted), format::count(counted))];
        return tile;
    }
    let mut tile = Tile::new(TileKey::Deadlocks, TileStatus::Good, format!("No deadlocks in {}.", last(d.since_hours)));
    tile.detail = d.note.clone();
    tile.metrics = vec![TileMetric::new("count", "Deadlocks", Some(0.0), "0")];
    tile
}
