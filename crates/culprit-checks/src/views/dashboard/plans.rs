//! The dashboard's plans tile.

use super::*;

pub(super) fn plans(s: &Snapshot) -> Tile {
    let Some(w) = &s.workload else { return no_workload(TileKey::Plans, "there are no plans to read") };
    let total = w.total_time_ms.max(1.0);
    // Plans of queries that stopped running (most likely changed by a deploy) would describe code that is gone, and a
    // statement that ran once isn't what the application runs.
    let with_plans: Vec<&QueryStat> =
        app_queries(s).filter(|q| q.plan.is_some() && !crate::query_is_history(s, q)).collect();
    if with_plans.is_empty() {
        let reason = [Capability::QueryPlans, Capability::Showplan, Capability::GenericPlans]
            .into_iter()
            .find_map(|c| missing_reason(s, c))
            .map(|m| sentence(&m.reason));
        let mut t = Tile::new(
            TileKey::Plans,
            TileStatus::Unavailable,
            match reason {
                Some(reason) => format!("No plans were captured: {reason}"),
                None => "No plans were captured in this scan.".to_string(),
            },
        );
        t.detail = Some("Open a query to plan it on demand, or paste a plan into Plans.".into());
        return t;
    }
    struct Group<'a> {
        plans: usize,
        share: f64,
        severity: Severity,
        top: &'a QueryStat,
    }
    let mut groups: HashMap<String, Group> = HashMap::new();
    let mut flagged = 0;
    let mut note = None;
    for q in &with_plans {
        let Some(plan) = &q.plan else { continue };
        let found = match &plan.insights {
            Some(i) => i.items.clone(),
            None => insights(plan),
        };
        if note.is_none() {
            note = plan.insights.as_ref().and_then(|i| i.note.clone()).or_else(|| coverage_note(plan));
        }
        if found.is_empty() {
            continue;
        }
        flagged += 1;
        let share = q.total_time_ms / total;
        let mut rules = HashSet::new();
        for i in &found {
            if !rules.insert(i.rule.clone()) {
                continue;
            }
            let g =
                groups.entry(i.rule.clone()).or_insert(Group { plans: 0, share: 0.0, severity: i.severity, top: q });
            g.plans += 1;
            g.share += share;
            g.severity = g.severity.max(i.severity);
            if q.total_time_ms > g.top.total_time_ms {
                g.top = q;
            }
        }
    }
    let captured = with_plans.len();
    let mut ranked: Vec<(String, Group)> = groups.into_iter().collect();
    ranked.sort_by(|a, b| b.1.severity.cmp(&a.1.severity).then(b.1.share.total_cmp(&a.1.share)).then(a.0.cmp(&b.0)));
    let (status, takeaway) = match ranked.first() {
        None => (
            TileStatus::Good,
            format!("None of the {} shows a known problem.", plural(captured, "captured plan", "captured plans")),
        ),
        Some((rule, g)) => {
            // Worth watching where the plan-insights check would raise it: a share of database time behind it.
            let status = if (g.severity == Severity::High && g.share >= crate::queries::MIN_SHARE)
                || (g.severity == Severity::Medium && g.share >= 0.1)
            {
                TileStatus::Watch
            } else {
                TileStatus::Info
            };
            let what = short_rule(rule);
            let share = format::percent(g.share.min(1.0));
            let takeaway = if flagged == 1 {
                format!(
                    "One of the {} has something to fix: {what} ({share} of database time).",
                    plural(captured, "captured plan", "captured plans")
                )
            } else if g.plans == 1 {
                format!(
                    "{flagged} of {captured} captured plans have something to fix; the costliest is {what} ({share} of database time)."
                )
            } else {
                format!(
                    "{flagged} of {captured} captured plans have something to fix; {what} shows up in {} ({share} of database time).",
                    plural(g.plans, "plan", "plans")
                )
            };
            (status, takeaway)
        }
    };
    let mut tile = Tile::new(TileKey::Plans, status, takeaway);
    tile.detail = note;
    tile.metrics = vec![
        TileMetric::new("captured", "Captured", Some(captured as f64), captured.to_string()),
        TileMetric::new("flagged", "With problems", Some(flagged as f64), flagged.to_string()),
    ];
    tile.items = ranked
        .iter()
        .take(4)
        .map(|(rule, g)| {
            let mut item = TileItem::new(ItemKind::Rule, rule.clone(), capitalized(&short_rule(rule)));
            item.value = Some(plural(g.plans, "plan", "plans"));
            item.share = Some(g.share.min(1.0));
            item.status = Some(severity_status(g.severity));
            item.query_id = Some(g.top.id.clone());
            item
        })
        .collect();
    tile
}

/// A rule as a few words for a sentence: "a large sort", "key lookups".
pub(super) fn short_rule(rule: &str) -> String {
    match rule {
        "key-lookups" => "key lookups",
        "scan-filter" => "a scan reading far more rows than it keeps",
        "missing-index-hint" => "a missing index",
        "implicit-conversion" => "an implicit conversion",
        "non-sargable" => "a function around a filtered column",
        "leading-wildcard" => "a leading wildcard",
        "not-in" => "a NOT IN",
        "scalar-udf" => "a scalar function called per row",
        "table-variable" => "a table variable",
        "parallelism-blocked" => "a plan that can't go parallel",
        "parallel-skew" => "uneven parallel threads",
        "eager-spool" => "an eager spool",
        "spill" => "a spill to disk",
        "memory-grant" => "an oversized memory grant",
        "misestimate" => "a row misestimate",
        "nested-loops" => "a nested loop repeating thousands of times",
        "big-sort" => "a large sort",
        "no-join-predicate" => "a join without a condition",
        "missing-statistics" => "missing statistics",
        "optimizer-timeout" => "an optimizer timeout",
        "legacy-ce" => "the legacy cardinality estimator",
        "slow-compile" => "slow planning",
        "many-to-many" => "a many-to-many merge join",
        "remote-query" => "a remote query",
        "heap-fetches" => "heap fetches",
        "lossy-bitmap" => "a lossy bitmap scan",
        "workers-short" => "missing parallel workers",
        "jit" => "JIT compilation",
        "correlated-subquery" => "a correlated subquery",
        "join-buffer" => "a join without an index",
        "temp-table-sort" => "a temporary table and filesort",
        "index-ignored" => "an ignored index",
        other => return other.replace('-', " "),
    }
    .to_string()
}
