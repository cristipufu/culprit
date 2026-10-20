//! Plan insights about the server around the plan: settings, compiles and resources a plan reports.

use super::*;

pub(super) fn workers_short(cx: &Cx) -> Vec<Draft> {
    cx.nodes
        .iter()
        // A Gather that never ran launched no workers because it never asked for any.
        .filter(|n| n.act_executions.is_none_or(|runs| runs > 0.0))
        .filter_map(|n| {
            let planned = prop_f64(n, "Workers Planned")?;
            let launched = prop_f64(n, "Workers Launched")?;
            (launched < planned).then_some((n, planned, launched))
        })
        .take(1)
        .map(|(n, planned, launched)| {
            let mut d = Draft::new("workers-short", InsightArea::Server, cx.subtree(n), vec![n.id]);
            d.subtree = true;
            d.cap = Severity::Medium;
            d.title = format!("Planned {} parallel workers, only {} started", format::count(planned), format::count(launched));
            d.detail = "The server had no free parallel workers when this ran (max_parallel_workers / max_worker_processes), so it ran with less help than it was planned for.".into();
            d.fix = "If the server has idle CPUs, raise max_parallel_workers (and max_worker_processes, which needs a restart); otherwise reduce how many queries run in parallel at once.".into();
            d.verify = "Explain analyze again: Workers Launched equals Workers Planned.".into();
            d
        })
        .collect()
}

pub(super) fn heap_fetches(cx: &Cx) -> Vec<Draft> {
    cx.nodes
        .iter()
        .filter(|n| n.op == "Index Only Scan")
        .filter_map(|n| {
            let fetches = prop_f64(n, "Heap Fetches")?;
            (fetches >= 1_000.0 && fetches >= 0.1 * rows_out(n)).then_some((n, fetches))
        })
        .take(1)
        .map(|(n, fetches)| {
            let table = what(n);
            let mut d = Draft::new("heap-fetches", InsightArea::Server, cx.share(n), vec![n.id]);
            d.cap = Severity::Medium;
            d.title = format!("The index-only scan on {} still visits the table {} times", table_leaf(&table), rows_text(fetches));
            d.detail = "PostgreSQL must visit table pages that the visibility map cannot certify as all-visible. Recent writes, an older snapshot or maintenance that has not yet caught up can explain this; heap fetches alone do not establish a vacuum failure.".into();
            d.fix = format!("Check recent writes, vacuum activity and cleanup holders on {}. If vacuum can advance visibility and the saved reads justify it, test VACUUM; a frequently updated table can legitimately keep requiring heap fetches.", table_leaf(&table));
            d.fix_sql = Some(format!("VACUUM (ANALYZE) {};", qualified(Engine::Postgres, &table)));
            d.verify = "Compare heap fetches, buffers and execution time with equivalent parameters and write activity after maintenance. Fewer heap fetches may help; zero is not required on a table that keeps changing.".into();
            d
        })
        .collect()
}

pub(super) fn jit(cx: &Cx) -> Vec<Draft> {
    if cx.engine() != Engine::Postgres {
        return Vec::new();
    }
    let Some(raw) = cx.prop("JIT") else { return Vec::new() };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(raw) else { return Vec::new() };
    let Some(total) = value.pointer("/Timing/Total").and_then(|v| v.as_f64()) else { return Vec::new() };
    let execution = cx.prop("Execution Time").and_then(|v| v.parse::<f64>().ok());
    if total < 10.0 || execution.is_some_and(|e| total < 0.2 * e) {
        return Vec::new();
    }
    let share = execution.map(|e| (total / e.max(total)).min(1.0));
    // Execution Time includes the compiling. When compiling took at least as long as everything else, the compiled
    // code can't have won that back; below that it may have, and only a run without JIT can tell.
    let dominates = share.is_some_and(|s| s >= 0.5);
    let mut d = Draft::new("jit", InsightArea::Server, share.unwrap_or(0.2), vec![cx.plan.root.id]);
    d.fixed_weight = share.is_none();
    d.cost_ms = Some(total);
    d.cap = if dominates { Severity::Medium } else { Severity::Low };
    d.title = match execution {
        Some(e) => {
            format!("JIT compilation took {} of the {} this query ran", format::duration(total), format::duration(e))
        }
        None => format!("JIT compilation took {}", format::duration(total)),
    };
    d.detail = format!(
        "PostgreSQL compiled parts of this query to machine code because its estimated cost crossed jit_above_cost. {}",
        if dominates {
            "Compiling took longer than everything else the query did, so it can't have paid for itself here."
        } else {
            "Compiled code evaluates each row faster, so on a long run it can pay for itself; only a run without it tells."
        }
    );
    d.fix = if dominates {
        "Check with JIT off in your session; if it's faster, turn JIT off for this workload's role, or raise jit_above_cost so only long analytical queries use it.".into()
    } else {
        "Compare with JIT off in your session before changing anything.".into()
    };
    d.fix_sql = Some(if dominates {
        "-- In your session, then EXPLAIN ANALYZE again and compare:\nSET jit = off;\n-- If it's faster, for the application's role:\n-- ALTER ROLE app_user SET jit = off;".into()
    } else {
        "-- In your session only, then EXPLAIN ANALYZE again and compare:\nSET jit = off;".into()
    });
    d.verify = "Explain analyze again: no JIT section, and a shorter execution time.".into();
    vec![d]
}
