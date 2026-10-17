//! Plan insights about memory: sorts and hashes that spill, and grants far larger or smaller than the run used.

use super::*;

pub(super) fn memory_grant(cx: &Cx) -> Vec<Draft> {
    if cx.engine() != Engine::Mssql {
        return Vec::new();
    }
    let kb = |key: &str| cx.prop(key).and_then(|v| v.parse::<f64>().ok());
    let mut out = Vec::new();
    let root = cx.plan.root.id;
    if let (Some(granted), Some(used)) = (kb("MemoryGrantedKB"), kb("MemoryMaxUsedKB"))
        && has_actuals(cx.plan)
        && granted >= 10.0 * 1024.0
        && used < 0.25 * granted
    {
        let mut d = Draft::new("memory-grant", InsightArea::Memory, 0.15, vec![memory_node(cx).unwrap_or(root)]);
        d.fixed_weight = true;
        d.subtree = true;
        d.floor = if granted >= 1024.0 * 1024.0 { Severity::High } else { Severity::Medium };
        d.title = format!(
            "Reserves {} of memory per run and uses {}",
            format::bytes(granted * 1024.0),
            format::bytes(used * 1024.0)
        );
        d.detail = format!(
            "The grant is sized from row estimates before the query runs; when they're too high, memory sits reserved and other queries wait for it (RESOURCE_SEMAPHORE).{}",
            cx.prop("MemoryGrantWarning").map(|w| format!(" SQL Server flags it too: {w}.")).unwrap_or_default()
        );
        d.fix = "Fix the estimates (update statistics, avoid functions on filtered columns). SQL Server 2019+ shrinks grants on later runs (memory grant feedback); as a last resort cap it with OPTION (MAX_GRANT_PERCENT = 5).".into();
        d.verify = "Run with an actual plan: GrantedMemory is close to MaxUsedMemory.".into();
        out.push(d);
    } else if let Some(desired) = kb("MemoryDesiredKB")
        && !has_actuals(cx.plan)
        && desired >= 1024.0 * 1024.0
    {
        let mut d = Draft::new("memory-grant", InsightArea::Memory, 0.1, vec![memory_node(cx).unwrap_or(root)]);
        d.fixed_weight = true;
        // A gigabyte or more asked for each run is enough on its own to make others queue.
        d.floor = Severity::Medium;
        d.subtree = true;
        d.title = format!("Asks for {} of memory per run", format::bytes(desired * 1024.0));
        d.detail =
            "Large grants are reserved for the whole run; a few of these at once make other queries queue for memory."
                .into();
        d.fix = "Check the row estimates of the sorts and hashes; sort fewer rows or columns, or cap it with OPTION (MAX_GRANT_PERCENT = n).".into();
        d.verify = "Explain again: the desired memory is lower.".into();
        out.push(d);
    }
    if let Some(wait) = kb("MemoryGrantWaitMs").filter(|w| *w >= 100.0) {
        let mut d = Draft::new("memory-grant", InsightArea::Memory, 0.2, vec![root]);
        d.fixed_weight = true;
        d.cost_ms = Some(wait);
        d.floor = Severity::Medium;
        d.title = format!("Waited {} for memory before it could start", format::duration(wait));
        d.detail = "Other queries held the memory it needed. The run was slow before it did any work.".into();
        d.fix = "Find the queries with oversized grants (they show up with this same insight) and shrink them; adding memory only postpones it.".into();
        d.verify = "Run with an actual plan: GrantWaitTime is 0.".into();
        out.push(d);
    }
    out
}

pub(super) fn memory_node(cx: &Cx) -> Option<u32> {
    cx.nodes
        .iter()
        .filter(|n| n.kind == NodeKind::Sort || n.op.contains("Hash"))
        .max_by(|a, b| a.self_cost.total_cmp(&b.self_cost))
        .map(|n| n.id)
}

pub(super) fn lossy_bitmap(cx: &Cx) -> Vec<Draft> {
    cx.nodes
        .iter()
        .filter_map(|n| {
            let lossy = prop_f64(n, "Lossy Heap Blocks")?;
            // A BRIN index only ever gives lossy pages: no exact ones means nothing was lost for want of work_mem.
            let exact = prop_f64(n, "Exact Heap Blocks").unwrap_or(0.0);
            (lossy > 0.0 && exact > 0.0).then_some((n, lossy))
        })
        .take(1)
        .map(|(n, lossy)| {
            // PostgreSQL gives it per loop, like its actual rows.
            let rechecked = prop_f64(n, "Rows Removed by Index Recheck").unwrap_or(0.0)
                * n.act_executions.unwrap_or(1.0).max(1.0);
            // A BRIN index only ever gives pages: one in the bitmap (the catalog marks its kind) explains lossy pages
            // with work_mem to spare. Bitmaps combined (BitmapOr, BitmapAnd) on a plan the catalog didn't mark may hold
            // one, so work_mem is then one cause, not the cause.
            fn walk<'a>(n: &'a PlanNode, out: &mut Vec<&'a PlanNode>) {
                out.push(n);
                n.children.iter().for_each(|c| walk(c, out));
            }
            let mut below = Vec::new();
            walk(n, &mut below);
            let index_scans: Vec<&&PlanNode> = below.iter().filter(|x| x.op == "Bitmap Index Scan").collect();
            let marked = !index_scans.is_empty() && index_scans.iter().all(|x| x.props.contains_key(crate::plan::INDEX_KIND));
            let combined = below.iter().any(|x| matches!(x.op.as_str(), "BitmapOr" | "BitmapAnd"));
            let maybe_brin = if marked { index_scans.iter().any(|x| x.reads_brin()) } else { combined };
            let mut d = Draft::new("lossy-bitmap", InsightArea::Memory, cx.share(n), vec![n.id]);
            d.cap = if maybe_brin { Severity::Low } else { Severity::Medium };
            d.title = if maybe_brin {
                format!("The bitmap scan on {} kept {} pages without their rows", table_leaf(&what(n)), format::count(lossy))
            } else {
                format!("The bitmap scan on {} ran out of work_mem and went lossy", table_leaf(&what(n)))
            };
            d.detail = format!(
                "For {} pages it only remembered the page, not the rows, so every row on them was re-checked{}.{}",
                format::count(lossy),
                if rechecked > 0.0 {
                    format!(" ({} rows thrown away)", format::count(rechecked))
                } else {
                    String::new()
                },
                if maybe_brin {
                    " A bitmap outgrowing work_mem does that, and so does a BRIN index combined into it, which only ever gives pages: check the indexes it combines before raising work_mem."
                } else {
                    ""
                }
            );
            d.fix =
                "Raise work_mem for this workload (per role or session), or make the condition more selective.".into();
            d.fix_sql = Some("SET work_mem = '32MB';  -- test in your session first".into());
            d.verify = "Explain analyze again: no Lossy Heap Blocks.".into();
            d
        })
        .collect()
}
