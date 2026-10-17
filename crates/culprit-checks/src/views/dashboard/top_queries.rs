//! The dashboard's top queries tile.

use super::*;

pub(super) fn top_queries(s: &Snapshot) -> Tile {
    let Some(w) = &s.workload else { return no_workload(TileKey::TopQueries, "there is no query list") };
    let total = w.total_time_ms;
    // Each one's share as every surface states it (over Culprit's scans where they measured it), and one whose cost is
    // behind it (stopped, or ran once) never takes the place of one that runs.
    let share = |q: &QueryStat| w.statement_share(q).min(1.0);
    let mut queries: Vec<(&QueryStat, bool)> =
        app_queries(s).filter(|q| q.total_time_ms > 0.0).map(|q| (q, crate::query_is_history(s, q))).collect();
    queries.sort_by(|(a, a_history), (b, b_history)| a_history.cmp(b_history).then(share(b).total_cmp(&share(a))));
    let queries: Vec<&QueryStat> = queries.into_iter().map(|(q, _)| q).collect();
    if queries.is_empty() || total <= 0.0 {
        let mut t = Tile::new(
            TileKey::TopQueries,
            TileStatus::Info,
            format!("No application queries ran over {}.", window_phrase(w)),
        );
        t.detail = Some(format!("{} recorded {} statements in all.", w.source, format::count(w.query_count)));
        return t;
    }
    let top = &queries[..queries.len().min(5)];
    let first = share(top[0]);
    let three: f64 = top.iter().take(3).map(|q| share(q)).sum();
    let five: f64 = top.iter().map(|q| share(q)).sum();
    let takeaway = if first >= 0.3 {
        format!("One query is {} of database time: {}.", format::percent(first), query_handle(top[0]))
    } else if three >= 0.6 && top.len() >= 3 {
        format!("Three queries make up {} of database time.", format::percent(three))
    } else if top.len() >= 5 {
        format!("Time is spread out: the top five queries are {} of database time.", format::percent(five))
    } else {
        format!(
            "{} {} of database time.",
            capitalized(&plural(top.len(), "query is", "queries are")),
            format::percent(five)
        )
    };
    let mut tile = Tile::new(TileKey::TopQueries, TileStatus::Info, takeaway);
    let stopped = top.iter().filter(|q| crate::query_stopped(s, q).is_some()).count();
    // What the shares are over, where Culprit's scans measured every one of them.
    let scans: Option<Vec<culprit_core::ScanSpan>> = top.iter().map(|q| w.scans_of(q)).collect();
    let over = scans
        .and_then(|spans| spans.into_iter().max_by(|a, b| a.hours.total_cmp(&b.hours)))
        .map(|s| format!("; their shares are over {}", s.phrase()))
        .unwrap_or_default();
    tile.detail = Some(format!(
        "{} tracked over {}{over}.{}",
        capitalized(&plural(w.query_count as usize, "query", "queries")),
        window_phrase(w),
        match stopped {
            0 => String::new(),
            n if n == top.len() =>
                " All of these stopped running before the window ended: a deploy may have changed them.".into(),
            n => format!(" {n} of these stopped running before the window ended: a deploy may have changed them."),
        }
    ));
    tile.metrics = vec![
        TileMetric::new("top-share", "Top query", Some(first), format::percent(first)),
        TileMetric::new("top5-share", "Top five", Some(five.min(1.0)), format::percent(five.min(1.0))),
        TileMetric::new("queries", "Queries", Some(w.query_count), format::count(w.query_count)),
    ];
    tile.items = top
        .iter()
        .map(|q| {
            let label = if crate::ran_once(q) {
                format!("{} (ran once)", query_handle(q))
            } else if crate::query_stopped(s, q).is_some() {
                format!("{} (stopped running)", query_handle(q))
            } else {
                query_handle(q)
            };
            let mut item = TileItem::new(ItemKind::Query, q.id.clone(), label);
            item.value = Some(format::percent(share(q)));
            item.share = Some(share(q));
            item.query_id = Some(q.id.clone());
            item
        })
        .collect();
    tile
}
