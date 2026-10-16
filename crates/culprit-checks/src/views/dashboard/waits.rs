//! The dashboard's waits tile.

use super::*;

/// Waiting of `ms` is a real amount over the stretch the report covers: about a minute a day, over at least an hour of
/// it. A sample taken just now needs a second of waiting in it.
fn material(s: &Snapshot, scope: WaitScope, ms: f64) -> bool {
    let hours = match scope {
        WaitScope::Window | WaitScope::Statements => s.workload.as_ref().map(|w| w.window_hours),
        WaitScope::SinceStartup => s.pressure.as_ref().and_then(|p| p.uptime_hours),
        WaitScope::Sampled => return ms >= 1_000.0,
    };
    hours.filter(|h| *h >= culprit_core::RATE_MIN_HOURS).is_some_and(|h| ms / h * 24.0 >= 60_000.0)
}

pub(super) fn waits(s: &Snapshot) -> Tile {
    let report = crate::waits::explain(s).filter(|r| !r.categories.is_empty());
    let Some(r) = report else { return waits_missing(s) };
    let top = &r.categories[0];
    let named = r.categories.iter().find(|c| c.key != "other");
    // Without performance_schema, MySQL and MariaDB time no waits: the slow query log keeps only each statement's lock
    // time, so "100% locks" would be 100% of the little that was measured. Say what is known, against database time.
    if r.scope == WaitScope::Statements && s.server.engine == Engine::Mysql {
        let database_ms = s.workload.as_ref().map(|w| w.total_time_ms).unwrap_or(0.0);
        let share = if database_ms > 0.0 { r.total_ms / database_ms } else { 0.0 };
        let takeaway = format!(
            "{} times no waits without performance_schema; logged statements waited {} on locks ({} of their time).",
            family(&s.server),
            format::duration(r.total_ms),
            format::percent(share)
        );
        let watch = share >= 0.25 && material(s, r.scope, r.total_ms);
        let mut tile = Tile::new(TileKey::Waits, if watch { TileStatus::Watch } else { TileStatus::Info }, takeaway);
        tile.detail = top.detail.clone();
        tile.metrics = vec![TileMetric::new("total", "Lock time", Some(r.total_ms), format::duration(r.total_ms))];
        tile.items = r
            .categories
            .iter()
            .take(6)
            .map(|c| {
                let mut item = TileItem::new(ItemKind::Wait, c.key.clone(), c.label.clone());
                item.value = Some(format::duration(c.wait_ms));
                item.query_id = c.queries.first().map(|q| q.id.clone());
                item
            })
            .collect();
        return tile;
    }
    let takeaway = if top.key == "other" {
        // "Everything else" leading says nothing; say what is known and how much isn't.
        match named {
            Some(c) => format!(
                "{} take {}; the other {} isn't broken down by {}.",
                c.label,
                format::percent(c.share),
                format::percent(top.share),
                family(&s.server)
            ),
            None => format!("{} doesn't break this waiting down by kind.", family(&s.server)),
        }
    } else {
        match r.categories.get(1).filter(|c| c.share >= 0.05) {
            Some(next) => format!(
                "Most time goes to {} ({}), then {} ({}).",
                lower_label(&top.label),
                format::percent(top.share),
                lower_label(&next.label),
                format::percent(next.share)
            ),
            None => format!("Most time goes to {} ({}).", lower_label(&top.label), format::percent(top.share)),
        }
    };
    let contended: f64 =
        r.categories.iter().filter(|c| matches!(c.key.as_str(), "lock" | "latch" | "memory")).map(|c| c.share).sum();
    // Contention worth watching is a real amount of it, and this database's: totals since startup are the whole
    // server's (every database on it), except on Azure SQL Database where the server is the database.
    let shared = r.scope == WaitScope::SinceStartup
        && !s.server.platform().is_some_and(|p| p.is_azure_sql_database())
        && s.server.engine.has_server();
    let watch = contended >= 0.25 && !shared && material(s, r.scope, contended * r.total_ms);
    let status = if watch { TileStatus::Watch } else { TileStatus::Info };
    let mut tile = Tile::new(TileKey::Waits, status, takeaway);
    let lead = if top.key == "other" { named.unwrap_or(top) } else { top };
    // Totals since startup cover a different stretch than database time: say which, rather than set them side by side.
    let since_start = r.scope == WaitScope::SinceStartup;
    tile.detail = if since_start {
        Some(format!("{}.", r.source.trim_end_matches('.')))
    } else {
        lead.detail.clone().or_else(|| first_sentence(&lead.meaning))
    };
    let label = if since_start { "Waiting since start" } else { "Waiting" };
    tile.metrics = vec![TileMetric::new("total", label, Some(r.total_ms), format::duration(r.total_ms))];
    tile.items = r
        .categories
        .iter()
        .take(6)
        .map(|c| {
            let mut item = TileItem::new(ItemKind::Wait, c.key.clone(), c.label.clone());
            item.value = Some(format::percent(c.share));
            item.share = Some(c.share);
            item.query_id = c.queries.first().map(|q| q.id.clone());
            item
        })
        .collect();
    tile
}

/// Labels like "CPU" keep their case; "Disk reads" reads as "disk reads" mid-sentence.
pub(super) fn lower_label(label: &str) -> String {
    let mut words = label.split(' ');
    let first = words.next().unwrap_or_default();
    let first = if first.len() > 1 && first.chars().all(|c| c.is_ascii_uppercase()) {
        first.to_string()
    } else {
        first.to_lowercase()
    };
    std::iter::once(first).chain(words.map(str::to_string)).collect::<Vec<_>>().join(" ")
}

pub(super) fn first_sentence(text: &str) -> Option<String> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    Some(match text.find(". ") {
        Some(end) => text[..=end].to_string(),
        None => text.to_string(),
    })
}

pub(super) fn waits_missing(s: &Snapshot) -> Tile {
    let family = family(&s.server);
    let mut tile = match s.server.engine {
        Engine::Sqlite => Tile::new(
            TileKey::Waits,
            TileStatus::Unavailable,
            "SQLite keeps no record of waits: it has no server, and a writer that has to wait gets SQLITE_BUSY in the application.",
        ),
        Engine::Postgres => {
            let mut t = Tile::new(
                TileKey::Waits,
                TileStatus::Unavailable,
                "PostgreSQL keeps no history of what sessions wait on.",
            );
            if is_off(setting(s, "track_io_timing")) {
                // Where it is set: ALTER SYSTEM on a server of one's own, the platform's settings on a managed one.
                let change = crate::support::setting_change(&s.server, "track_io_timing", "on");
                t.detail = Some(format!(
                    "Turn on track_io_timing to measure disk time per statement, or sample what it waits on right now. {}",
                    change.how
                ));
                t.fix_sql = change.sql;
            } else {
                t.detail = Some("Sample what it waits on right now to see where time goes.".into());
            }
            t
        }
        Engine::Mysql => match missing_reason(s, Capability::PerformanceSchema)
            .or_else(|| missing_reason(s, Capability::WaitStats))
        {
            Some(m) => {
                let mut t = Tile::new(
                    TileKey::Waits,
                    TileStatus::Unavailable,
                    format!("{family} isn't counting waits: {}", sentence(&m.reason)),
                );
                t.detail = m.fix.clone();
                t
            }
            None => {
                Tile::new(TileKey::Waits, TileStatus::Unavailable, format!("{family} recorded no waits in this scan."))
            }
        },
        Engine::Mssql => match missing_reason(s, Capability::WaitStats) {
            Some(m) => {
                let mut t = Tile::new(
                    TileKey::Waits,
                    TileStatus::Unavailable,
                    format!("Waits aren't readable here: {}", sentence(&m.reason)),
                );
                t.detail = m.fix.clone();
                t
            }
            None => Tile::new(TileKey::Waits, TileStatus::Unavailable, "This scan recorded no waits."),
        },
    };
    if tile.detail.is_none() && s.server.engine.has_server() {
        tile.detail = Some("Sample what it waits on right now instead.".into());
    }
    tile
}
