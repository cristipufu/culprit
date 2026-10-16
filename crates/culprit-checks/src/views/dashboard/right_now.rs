//! The dashboard's right now tile.

use super::*;

pub(super) fn not_read(s: &Snapshot) -> Tile {
    if !s.server.engine.has_server() {
        let mut t = Tile::new(
            TileKey::Now,
            TileStatus::Unavailable,
            format!("{} has no server, so there are no sessions to show.", s.server.engine),
        );
        t.detail = Some(
            "A writer that has to wait for the lock gets SQLITE_BUSY in the application: its logs say when.".into(),
        );
        return t;
    }
    let mut t = Tile::new(TileKey::Now, TileStatus::Info, "Sessions weren't read for this dashboard.");
    t.detail = Some("The app reads them every few seconds; `culprit live` reads them once.".into());
    t
}

pub(super) fn idle_in_transaction(s: &culprit_core::Session) -> bool {
    s.transaction_ms.is_some() && (s.state.starts_with("idle in transaction") || s.state == "sleeping")
}

pub(super) fn running(s: &culprit_core::Session) -> bool {
    s.state != "idle" && s.state != "sleeping" && !s.state.starts_with("idle in transaction")
}

/// Sessions right now: blocking first, then an idle open transaction, then what runs. The app words its polled tile the
/// same way (`app/src/lib/dashboard.ts`).
pub fn now(live: &LiveState) -> Tile {
    let active: Vec<&culprit_core::Session> = live.sessions.iter().filter(|s| running(s)).collect();
    let blocked = live.sessions.iter().filter(|s| s.blocked_by.is_some()).count();
    let idle_tx: Vec<&culprit_core::Session> = live.sessions.iter().filter(|s| idle_in_transaction(s)).collect();
    let longest = active
        .iter()
        .filter(|s| s.blocked_by.is_none())
        .max_by(|a, b| a.elapsed_ms.unwrap_or(0.0).total_cmp(&b.elapsed_ms.unwrap_or(0.0)));
    let oldest_idle =
        idle_tx.iter().max_by(|a, b| a.transaction_ms.unwrap_or(0.0).total_cmp(&b.transaction_ms.unwrap_or(0.0)));
    let chains = live.blocking.as_ref().map(|b| b.chains.as_slice()).unwrap_or_default();

    let (status, takeaway) = if let Some(chain) = chains.first() {
        let severe = chain.waiters >= 5 || chain.longest_wait_ms.is_some_and(|ms| ms >= 30_000.0);
        (if severe { TileStatus::Bad } else { TileStatus::Watch }, chain.headline.clone())
    } else if blocked > 0 {
        (
            TileStatus::Watch,
            format!(
                "{} waiting on another session's lock.",
                capitalized(&plural(blocked, "session is", "sessions are"))
            ),
        )
    } else if let Some(s) = oldest_idle.filter(|s| s.transaction_ms.unwrap_or(0.0) >= 60_000.0) {
        (
            TileStatus::Watch,
            format!(
                "Session {} has been idle in an open transaction for {}.",
                s.id,
                format::duration(s.transaction_ms.unwrap_or(0.0))
            ),
        )
    } else if active.is_empty() {
        (TileStatus::Good, "Nothing is running right now.".to_string())
    } else {
        let tail = match longest.and_then(|s| s.elapsed_ms) {
            Some(ms) if ms >= 1_000.0 => format!("; the longest has run {}", format::duration(ms)),
            _ => String::new(),
        };
        (
            TileStatus::Good,
            format!("{} running, none blocked{tail}.", capitalized(&plural(active.len(), "session", "sessions"))),
        )
    };
    let mut tile = Tile::new(TileKey::Now, status, takeaway);
    tile.metrics = vec![
        TileMetric::new("active", "Running", Some(active.len() as f64), active.len().to_string()),
        TileMetric::new("blocked", "Blocked", Some(blocked as f64), blocked.to_string()),
        TileMetric::new(
            "idle-in-transaction",
            "Idle in transaction",
            Some(idle_tx.len() as f64),
            idle_tx.len().to_string(),
        ),
        TileMetric::new("sessions", "Sessions", Some(live.sessions.len() as f64), live.sessions.len().to_string()),
    ];
    let mut items: Vec<TileItem> = chains
        .iter()
        .take(3)
        .map(|c| {
            let mut item = TileItem::new(ItemKind::Session, c.head.clone(), c.headline.clone());
            item.value = Some(format!("{} waiting", c.waiters));
            item.status = Some(TileStatus::Bad);
            item
        })
        .collect();
    let mut longest_first = active.clone();
    longest_first.sort_by(|a, b| b.elapsed_ms.unwrap_or(0.0).total_cmp(&a.elapsed_ms.unwrap_or(0.0)));
    items.extend(longest_first.iter().filter(|s| !chains.iter().any(|c| c.head == s.id)).take(3).map(|s| {
        let label = s
            .sql
            .as_deref()
            .map(statement_handle)
            .unwrap_or_else(|| s.command.clone().unwrap_or_else(|| s.state.clone()));
        let mut item = TileItem::new(ItemKind::Session, s.id.clone(), format!("Session {} · {label}", s.id));
        item.value = s.elapsed_ms.map(format::duration);
        item.status = Some(if s.blocked_by.is_some() { TileStatus::Watch } else { TileStatus::Good });
        item
    }));
    tile.items = items;
    tile.detail = Some(format!("Read {}.", culprit_core::changes::short_time(&live.taken_at)));
    tile
}
