//! The dashboard's indexes tile.

use super::*;

pub(super) fn indexes(report: &Report) -> Tile {
    let advice = crate::advisor::index_advice(report);
    // A query that asks for two indexes still only takes its share of time once. A foreign key's index is for deletes on
    // the parent: the queries that merely name the key wouldn't necessarily get faster, so they don't count here.
    let mut seen = HashSet::new();
    let mut share = 0.0;
    let measured = advice.candidates.iter().filter(|c| c.source != culprit_core::CandidateSource::ForeignKey);
    for q in measured.flat_map(|c| &c.queries) {
        if seen.insert(q.id.as_str()) {
            share += q.share.unwrap_or(0.0);
        }
    }
    let share = share.min(1.0);
    let adds = advice.candidates.len();
    let cleanups = advice.cleanups.len();
    let reclaim: f64 = advice.cleanups.iter().filter_map(|c| c.size_bytes).sum();
    let free = if reclaim > 0.0 { format!(" ({})", format::bytes(reclaim)) } else { String::new() };
    let new = |n: usize| if n == 1 { "One new index".to_string() } else { format!("{n} new indexes") };
    let help = if share >= 0.01 {
        format!("would help queries that take {} of database time", format::percent(share))
    } else if !seen.is_empty() {
        format!("would help {}", plural(seen.len(), "query", "queries"))
    } else if adds == 1 {
        "is worth a look".to_string()
    } else {
        "are worth a look".to_string()
    };
    // New indexes come from what queries need: without query history, none can be suggested, which says nothing about
    // whether any is needed.
    let history = report.snapshot.workload.is_some();
    let takeaway = match (adds, cleanups) {
        (0, 0) if !history => "No query history to suggest new indexes from, and nothing to clean up.".to_string(),
        (0, c) if !history => format!(
            "No query history to suggest new indexes from; {} can go{free}.",
            plural(c, "existing index", "existing indexes")
        ),
        (0, 0) => "No index changes stand out.".to_string(),
        (0, c) => format!("No new indexes needed; {} can go{free}.", plural(c, "existing index", "existing indexes")),
        (a, 0) => format!("{} {help}.", new(a)),
        (a, c) => format!("{} {help}; {} can go{free}.", new(a), plural(c, "index", "indexes")),
    };
    let status = if adds > 0 && share >= 0.1 {
        TileStatus::Watch
    } else if adds + cleanups > 0 || !history {
        TileStatus::Info
    } else {
        TileStatus::Good
    };
    let mut tile = Tile::new(TileKey::Indexes, status, takeaway);
    tile.detail = advice
        .notes
        .first()
        .cloned()
        .or_else(|| (!advice.counters.is_empty()).then(|| format!("Index usage counted {}.", advice.counters)));
    tile.metrics = vec![
        TileMetric::new("add", "To add", Some(adds as f64), adds.to_string()),
        TileMetric::new("cleanup", "To clean up", Some(cleanups as f64), cleanups.to_string()),
        TileMetric::new("reclaim", "Space back", Some(reclaim), format::bytes(reclaim)),
        TileMetric::new(
            "tables",
            "Tables to look at",
            Some(advice.tables.len() as f64),
            advice.tables.len().to_string(),
        ),
    ];
    let mut items: Vec<TileItem> = advice
        .candidates
        .iter()
        .take(3)
        .map(|c| {
            let mut item = TileItem::new(
                ItemKind::IndexAdd,
                format!("{}.{}({})", c.schema, c.table, c.keys.join(", ")),
                format!("{} ({})", c.table, c.keys.join(", ")),
            );
            item.value = c.db_time_share.map(format::percent);
            item.share = c.db_time_share;
            item.query_id = c.queries.first().map(|q| q.id.clone());
            item.finding_id = c.finding_id.clone();
            item
        })
        .collect();
    items.extend(advice.cleanups.iter().take(2).map(|c| {
        let mut item = TileItem::new(
            ItemKind::IndexCleanup,
            format!("{}.{}.{}", c.schema, c.table, c.index),
            format!("{} on {}", c.index, c.table),
        );
        item.value = c.size_bytes.map(format::bytes);
        item.finding_id = c.finding_id.clone();
        item
    }));
    tile.items = items;
    tile
}
