//! The dashboard's health tile.

use super::*;

pub(super) fn health(report: &Report) -> Tile {
    let s = &report.snapshot;
    let Some(p) = &s.pressure else {
        let o = &s.operations;
        if !o.spaces.is_empty()
            || !o.counters.is_empty()
            || !o.sequences.is_empty()
            || !o.multixacts.is_empty()
            || o.sqlite.is_some()
            || o.cluster.is_some()
        {
            return operational(
                report,
                Tile::new(
                    TileKey::Health,
                    TileStatus::Info,
                    "Operational measurements were recorded; each retains its source scope and period.",
                ),
            );
        }
        let headline = if s.server.engine.has_server() {
            "This scan recorded no health counters.".to_string()
        } else {
            format!("{} has no server counters.", s.server.engine)
        };
        let mut t = Tile::new(TileKey::Health, TileStatus::Unavailable, headline);
        t.detail = Some(match s.notes.iter().find(|n| n.collector.contains("pressure")) {
            Some(note) => note.message.clone(),
            None if !s.server.engine.has_server() => format!(
                "{} has no server, so there are no compiles, memory, cache or connection counters to read.",
                s.server.engine
            ),
            None => "Scan again to read compiles, memory, cache and connection counters.".into(),
        });
        return t;
    };
    let mut signals: Vec<&Signal> = p.signals.iter().collect();
    // Worst first, then the healthy ones, then the ones shown without a verdict: an ungraded counter must never push a
    // healthy signal out of the squares the tile shows.
    signals.sort_by_key(|s| match s.status {
        SignalStatus::Bad => 0,
        SignalStatus::Watch => 1,
        SignalStatus::Good => 2,
        SignalStatus::Info => 3,
    });
    let graded = signals.iter().filter(|s| s.status != SignalStatus::Info).count();
    let bad = signals.iter().filter(|s| s.status == SignalStatus::Bad).count();
    let watch = signals.iter().filter(|s| s.status == SignalStatus::Watch).count();
    let mut status = if bad > 0 {
        TileStatus::Bad
    } else if watch > 0 {
        TileStatus::Watch
    } else if graded > 0 {
        TileStatus::Good
    } else {
        TileStatus::Info
    };
    let mut takeaway = match signals.first().filter(|s| matches!(s.status, SignalStatus::Bad | SignalStatus::Watch)) {
        Some(worst) => match &worst.healthy {
            Some(healthy) => format!("{}: {}; {}.", worst.label, signal_value(worst), healthy.trim_end_matches('.')),
            None => format!("{}: {}.", worst.label, signal_value(worst)),
        },
        None if graded == 1 => "The one graded signal looks healthy.".to_string(),
        None if graded > 0 => format!("All {graded} graded signals look healthy."),
        None => format!(
            "{} recorded; none has a threshold to grade.",
            capitalized(&plural(signals.len(), "signal", "signals"))
        ),
    };
    let mut items: Vec<TileItem> = Vec::new();
    // An old open transaction holds back cleanup on every engine; it earns a row of its own.
    if let Some(tx) = p.oldest_transaction.as_ref() {
        let assessed = report.findings.iter().find(|f| f.check == "held-cleanup" && f.is_actionable());
        let tx_status = assessed.map(|f| severity_status(f.severity)).unwrap_or(TileStatus::Info);
        let mut item =
            TileItem::new(ItemKind::Signal, culprit_core::signal_id::OLDEST_TRANSACTION, "Oldest open transaction");
        item.value = Some(format!(
            "{}{}",
            format::duration(tx.seconds * 1000.0),
            tx.state.as_deref().map(|s| format!(" · {s}")).unwrap_or_default()
        ));
        item.status = Some(tx_status);
        if tx_status >= TileStatus::Watch && status < tx_status {
            status = tx_status;
            takeaway = format!("{}.", assessed.unwrap().title.trim_end_matches('.'));
        }
        items.push(item);
    }
    items.extend(signals.iter().take(8).map(|s| {
        let mut item = TileItem::new(ItemKind::Signal, s.key.clone(), s.label.clone());
        item.value = Some(signal_value(s));
        item.status = Some(signal_status(s.status));
        item
    }));
    let mut tile = Tile::new(TileKey::Health, status, takeaway);
    let more = (bad + watch).saturating_sub(1);
    tile.detail = if more > 0 {
        Some(format!(
            "{} also {} a look.",
            capitalized(&plural(more, "more signal", "more signals")),
            if more == 1 { "needs" } else { "need" }
        ))
    } else {
        Some("Each signal states the period it measures.".into())
    };
    tile.metrics = vec![
        TileMetric::new("bad", "Bad", Some(bad as f64), bad.to_string()),
        TileMetric::new("watch", "Watch", Some(watch as f64), watch.to_string()),
        TileMetric::new("healthy", "Healthy", Some((graded - bad - watch) as f64), (graded - bad - watch).to_string()),
    ];
    tile.items = items;
    operational(report, tile)
}

fn operational(report: &Report, mut tile: Tile) -> Tile {
    let findings: Vec<_> = report
        .findings
        .iter()
        .filter(|f| {
            matches!(
                f.check.as_str(),
                "storage-capacity"
                    | "sequence-capacity"
                    | "multixact-age"
                    | "operational-errors"
                    | "cluster-health"
                    | "sqlite-wal"
            )
        })
        .collect();
    for f in findings.iter().take(3) {
        let status = if f.is_actionable() { severity_status(f.severity) } else { TileStatus::Info };
        if status >= TileStatus::Watch && status > tile.status {
            tile.status = status;
            tile.takeaway = f.title.clone();
        }
        let mut item = TileItem::new(ItemKind::Finding, f.id.clone(), f.title.clone());
        item.finding_id = Some(f.id.clone());
        item.status = Some(status);
        tile.items.push(item);
    }
    tile
}
