//! A plan's structural observations, checked against the scan's catalog and measured workload. The shape remains
//! visible when cheap or historical; urgency and executable changes require current supporting evidence.

use culprit_core::insights::{PlanInsight, PlanInsights};
use culprit_core::{Plan, QueryStat, Severity, Snapshot};

use crate::{indexes::*, support::*};

pub fn annotate_plans(snapshot: &mut Snapshot) {
    let readings: Vec<_> =
        snapshot.queries().iter().map(|q| q.plan.as_ref().map(|p| query_plan_insights(snapshot, q, p))).collect();
    if let Some(w) = &mut snapshot.workload {
        for (q, reading) in w.queries.iter_mut().zip(readings) {
            if let Some(p) = &mut q.plan {
                p.insights = reading;
            }
        }
    }
}

/// Read a captured or fetched plan in the context of the selected scan. Structural concerns remain useful for
/// growing tables even when the selected workload does not justify treating them as current incidents.
pub fn query_plan_insights(snapshot: &Snapshot, query: &QueryStat, plan: &Plan) -> PlanInsights {
    if let Some(note) = culprit_core::insights::statistics_maintenance_note(plan.engine, &query.text) {
        return PlanInsights { items: Vec::new(), note: Some(note.into()) };
    }
    let mut items = catalog_insights(snapshot, query, plan);
    if let Some(rewrite) = crate::queries::late_filter_rewrite(snapshot, query, plan) {
        items.push(rewrite);
    }
    let history = crate::query_is_history(snapshot, query);
    let material = index_work_is_material(snapshot, &[query]);
    let context = if history {
        Some("This statement ran once or stopped running in the selected workload. These are observations about its captured plan, not a current incident. Recheck if the workload returns.".to_string())
    } else if !material && !items.is_empty() {
        Some("The selected workload does not establish material execution cost. Structural concerns remain relevant as table size or call volume grows; compare reads and time under representative load before changing an index.".to_string())
    } else {
        None
    };
    for i in &mut items {
        if history || !material {
            i.severity = Severity::Low;
            i.fix_sql = None;
            i.fix = format!("{} {}", context.as_deref().unwrap_or_default(), i.fix);
        }
        if i.rule == "missing-statistics" {
            i.severity = Severity::Low;
            i.fix_sql = None;
        }
    }
    let growth = plan.nodes().into_iter().any(|n| {
        n.kind == culprit_core::NodeKind::Scan && n.predicate.is_some()
            && n.table_rows.is_some_and(|rows| rows > 0.0 && rows < culprit_core::plan::SCAN_ROWS)
            && !n.props.contains_key(culprit_core::plan::NOT_READ_IN_FULL)
    }).then(|| "A filtered scan of a small table can be appropriate today. Track rows read and execution time as the table grows; low current cost does not establish that the access path will scale.".to_string());
    let notes: Vec<_> = [culprit_core::insights::coverage_note(plan), context, growth].into_iter().flatten().collect();
    let note = (!notes.is_empty()).then(|| notes.join(" "));
    PlanInsights { items, note }
}

/// Catalog safety is also used during detection, before the pipeline decides whether a finding is history.
pub(crate) fn catalog_insights(snapshot: &Snapshot, query: &QueryStat, plan: &Plan) -> Vec<PlanInsight> {
    if culprit_core::insights::statistics_maintenance_note(plan.engine, &query.text).is_some() {
        return Vec::new();
    }
    let mut found = culprit_core::insights::insights(plan);
    for i in &mut found {
        if i.area == culprit_core::insights::InsightArea::Index && crate::evidence::index_catalog(snapshot).is_some() {
            i.severity = Severity::Low;
            i.fix_sql = None;
            i.fix = "Current index definitions were unavailable in this scan. Inspect the current keys, filters and included columns before choosing an index change; the captured structure can still pose a scaling risk.".into();
            continue;
        }
        if i.rule == "key-lookups" {
            lookup_advice(snapshot, query, plan, i);
            continue;
        }
        let request = match i.rule.as_str() {
            "missing-index-hint" => {
                let hint = i.index_hint.as_ref();
                hint.and_then(|h| {
                    split_object(&h.table)
                        .map(|(s, t)| (s, t, h.equality.clone(), h.inequality.clone(), h.include.clone()))
                })
            }
            "scan-filter" => {
                i.nodes.first().and_then(|id| plan.nodes().into_iter().find(|n| n.id == *id)).and_then(|n| {
                    let (s, t) = split_object(n.object.as_deref()?)?;
                    let cols = predicate_columns(n.predicate.as_deref()?);
                    Some((s, t, cols.equality, cols.range.into_iter().take(1).collect(), Vec::new()))
                })
            }
            _ => continue,
        };
        let Some((schema, table, equality, inequality, mut include)) = request else {
            i.fix_sql = None;
            i.fix = "Check the complete predicate and current index definitions before choosing index keys.".into();
            continue;
        };
        let note = drop_large_includes(snapshot, &schema, &table, &mut include);
        let wide = include.len() > MAX_INCLUDE;
        if wide {
            include.clear();
        }
        // Only the definition is used here. Request counters belong to the server's missing-index view, not XML.
        let candidate = Candidate {
            schema: schema.clone(),
            table: table.clone(),
            equality,
            inequality,
            include,
            seeks: 0.0,
            scans: 0.0,
            weight: 0.0,
            impact_pct: 0.0,
            query_hashes: vec![],
            requests: vec![],
        };
        if candidate.keys().is_empty() {
            i.fix_sql = None;
            i.fix = "Inspect the complete predicate and current index definitions; these conditions do not establish a safe choice of index keys.".into();
            continue;
        }
        if table_gone(snapshot, &table, &[query]) {
            i.severity = Severity::Low;
            i.fix_sql = None;
            i.fix = "The captured table is absent from this scan's catalog. Confirm the current schema before acting on this historical plan.".into();
            continue;
        }
        let fixed = candidate_fix(snapshot, &candidate, &[query]);
        if fixed.fix.sql.is_none() && fixed.widens.is_some() {
            i.severity = Severity::Low;
            i.title = format!(
                "The captured {} on {schema}.{table} has matching index keys now",
                if i.rule == "scan-filter" { "scan" } else { "request" }
            );
        }
        i.fix = fixed.fix.summary;
        i.fix_sql = fixed.fix.sql;
        if let Some(note) = note {
            i.fix.push_str(&format!(" {note}"));
        }
        if wide {
            i.fix.push_str(" The wide INCLUDE list is omitted. Select fewer columns and test a narrow index before copying the payload into an index.");
        }
        i.verify = "Obtain a newly compiled plan and compare reads and execution time with equivalent parameters and load. Check index usage and write cost; a retained scan or old stored suggestion alone does not establish success or failure.".into();
    }
    found
}

fn lookup_advice(snapshot: &Snapshot, query: &QueryStat, plan: &Plan, i: &mut PlanInsight) {
    let nodes = plan.nodes();
    let Some(lookup) = i.nodes.first().and_then(|id| nodes.iter().find(|n| n.id == *id)) else { return };
    let Some((schema, table)) = lookup.object.as_deref().and_then(split_object) else { return };
    let mut include: Vec<String> = lookup
        .props
        .get("OutputList")
        .into_iter()
        .flat_map(|s| s.split(", "))
        .map(|s| s.rsplit('.').next().unwrap_or(s).trim_matches(['[', ']', '"']).to_string())
        .filter(|s| !s.is_empty() && !s.starts_with("Expr"))
        .collect();
    let note = drop_large_includes(snapshot, &schema, &table, &mut include);
    let unknown = include.iter().any(|name| {
        !snapshot.columns.iter().any(|c| {
            c.schema.eq_ignore_ascii_case(&schema)
                && c.table.eq_ignore_ascii_case(&table)
                && c.name.eq_ignore_ascii_case(name)
        })
    });
    let wide = include.len() > MAX_INCLUDE;
    // A lookup can be appropriate for large payloads; never recommend copying the entire row blindly.
    i.fix = "Select fewer columns where possible. Test adding only the narrow columns needed by frequent reads as included columns, balancing saved lookups against index size and write cost.".into();
    i.fix_sql = None;
    if let Some(note) = note {
        i.fix.push_str(&format!(" {note}"));
    }
    if wide {
        i.fix.push_str(" The remaining projection is also wide; a covering index is not automatically worthwhile.");
    }
    if unknown {
        i.fix.push_str(" Confirm the projection against current column definitions before choosing INCLUDE columns.");
    }
    if !unknown && !wide && !include.is_empty() && index_work_is_material(snapshot, &[query]) {
        let index = i
            .nodes
            .iter()
            .skip(1)
            .filter_map(|id| nodes.iter().find(|n| n.id == *id))
            .filter_map(|n| n.index.as_deref())
            .find_map(|name| {
                snapshot.indexes.iter().find(|ix| {
                    ix.schema.eq_ignore_ascii_case(&schema)
                        && ix.table.eq_ignore_ascii_case(&table)
                        && ix.name.eq_ignore_ascii_case(name.trim_matches(['[', ']', '"']))
                        && ix.filter.as_deref().is_none_or(|f| fits_filter(&table, f, &[query]))
                })
            });
        if let Some(index) = index
            && let Widening::Sql(fix) = widen_index(snapshot, index, &include)
        {
            i.fix.push_str(&format!(" {}", fix.summary));
            i.fix_sql = fix.sql;
        }
    }
    i.verify = "Compare a newly compiled plan, lookup counts, reads and time under equivalent parameters and load. Retain a covering index only if its read benefit justifies its storage and write cost.".into();
}

#[cfg(test)]
mod tests {
    use super::*;
    use culprit_core::{Engine, NodeKind, PlanIndexHint};

    fn workload(engine: Engine, hours: f64, mean: f64) -> Snapshot {
        let mut s: Snapshot = serde_json::from_str(include_str!("../../../../fixtures/mssql-demo.json")).unwrap();
        s.server.engine = engine;
        s.indexes.clear();
        s.missing_indexes.clear();
        let w = s.workload.as_mut().unwrap();
        let mut q = w.queries.iter().find(|q| q.id == "9").unwrap().clone();
        q.first_seen = None;
        q.last_seen = None;
        q.trend = None;
        q.executions = 100.0;
        q.mean_time_ms = mean;
        q.total_time_ms = mean * q.executions;
        q.text = "SELECT id FROM shop.orders WHERE status = 1".into();
        let p = q.plan.as_mut().unwrap();
        p.engine = engine;
        p.statement = Some(q.text.clone());
        p.index_hints.clear();
        p.warnings.clear();
        p.props.clear();
        p.insights = None;
        p.total_cost = 100_000.0;
        let n = &mut p.root;
        n.op = "Table Scan".into();
        n.kind = NodeKind::Scan;
        n.object = Some("shop.orders".into());
        n.index = None;
        n.predicate = Some("status = 1".into());
        n.seek_predicate = None;
        n.est_rows = 100.0;
        n.est_executions = 1.0;
        n.table_rows = Some(1_000_000.0);
        n.rows_read = Some(1_000_000.0);
        n.act_rows = None;
        n.act_executions = None;
        n.act_time_ms = None;
        n.self_cost = p.total_cost;
        n.subtree_cost = p.total_cost;
        n.children.clear();
        n.props.clear();
        n.warnings.clear();
        w.windowed = true;
        w.window_hours = hours;
        w.hourly_time_ms = None;
        w.total_time_ms = q.total_time_ms;
        w.total_executions = q.executions;
        w.queries = vec![q];
        s
    }

    #[test]
    fn cheap_plans_keep_the_scaling_concern_without_current_incident_urgency() {
        for engine in [Engine::Mssql, Engine::Postgres, Engine::Mysql] {
            for hours in [1.0, 24.0, 168.0, 720.0] {
                let s = workload(engine, hours, 0.1);
                let q = &s.queries()[0];
                let p = q.plan.as_ref().unwrap();
                assert!(!culprit_core::insights::insights(p).is_empty(), "{engine:?}");
                let reading = query_plan_insights(&s, q, p);
                assert!(!reading.items.is_empty(), "a structural warning must remain visible");
                assert!(reading.items.iter().all(|i| i.severity == Severity::Low && i.fix_sql.is_none()));
                assert!(reading.note.unwrap().contains("table size or call volume grows"));
                assert!(reading.items.iter().any(|i| i.detail.contains("status")), "retain the mechanism");
            }
        }
    }

    #[test]
    fn current_cost_can_justify_advice_and_once_only_work_remains_history() {
        let mut s = workload(Engine::Mssql, 24.0, 500.0);
        let read = |s: &Snapshot| {
            let q = &s.queries()[0];
            query_plan_insights(s, q, q.plan.as_ref().unwrap())
        };
        assert!(read(&s).items.iter().any(|i| i.severity > Severity::Low && i.fix_sql.is_some()));
        let q = &mut s.workload.as_mut().unwrap().queries[0];
        q.executions = 1.0;
        assert!(read(&s).items.iter().all(|i| i.severity == Severity::Low && i.fix_sql.is_none()));
        assert!(read(&s).note.unwrap().contains("ran once or stopped"));
    }

    #[test]
    fn a_small_table_scan_keeps_growth_guidance_before_it_becomes_expensive() {
        let mut s = workload(Engine::Mssql, 24.0, 0.1);
        let n = &mut s.workload.as_mut().unwrap().queries[0].plan.as_mut().unwrap().root;
        n.table_rows = Some(200.0);
        n.rows_read = Some(200.0);
        n.est_rows = 1.0;
        let q = &s.queries()[0];
        let reading = query_plan_insights(&s, q, q.plan.as_ref().unwrap());
        assert!(reading.note.unwrap().contains("low current cost does not establish that the access path will scale"));
        assert!(reading.items.iter().all(|i| i.fix_sql.is_none()));
        let q = &mut s.workload.as_mut().unwrap().queries[0];
        q.mean_time_ms = 500.0;
        q.total_time_ms = q.mean_time_ms * q.executions;
        let n = &mut q.plan.as_mut().unwrap().root;
        n.table_rows = Some(1_000_000.0);
        n.rows_read = Some(1_000_000.0);
        let q = &s.queries()[0];
        assert!(query_plan_insights(&s, q, q.plan.as_ref().unwrap()).items.iter().any(|i| i.severity > Severity::Low));
    }

    #[test]
    fn current_index_coverage_and_large_payloads_override_an_old_hint() {
        let mut s = workload(Engine::Mssql, 24.0, 500.0);
        let fixture: Snapshot = serde_json::from_str(include_str!("../../../../fixtures/mssql-demo.json")).unwrap();
        let mut index = fixture.indexes[0].clone();
        index.schema = "shop".into();
        index.table = "orders".into();
        index.name = "ix_orders_status".into();
        index.columns = vec!["status".into()];
        index.include.clear();
        index.filter = None;
        index.is_disabled = false;
        s.indexes.push(index);
        let mut col = s.columns[0].clone();
        col.schema = "shop".into();
        col.table = "orders".into();
        col.name = "payload".into();
        col.data_type = "nvarchar(max)".into();
        s.columns.push(col);
        let p = s.workload.as_mut().unwrap().queries[0].plan.as_mut().unwrap();
        p.index_hints.push(PlanIndexHint {
            table: "shop.orders".into(),
            equality: vec!["status".into()],
            inequality: vec![],
            include: vec!["payload".into()],
            impact_pct: 99.0,
        });
        let q = &s.queries()[0];
        let reading = query_plan_insights(&s, q, q.plan.as_ref().unwrap());
        let i = reading.items.iter().find(|i| i.rule == "missing-index-hint").unwrap();
        assert!(i.fix_sql.is_none());
        assert_eq!(i.severity, Severity::Low);
        assert!(i.fix.contains("ix_orders_status") && i.fix.contains("payload"), "{}", i.fix);
        s.indexes[0].filter = Some("is_deleted = 0".into());
        let q = &s.queries()[0];
        let reading = query_plan_insights(&s, q, q.plan.as_ref().unwrap());
        let i = reading.items.iter().find(|i| i.rule == "missing-index-hint").unwrap();
        assert!(i.fix_sql.as_deref().unwrap().contains("CREATE"), "unproven filtered coverage cannot suppress advice");
        assert!(!i.fix_sql.as_deref().unwrap().contains("payload"));
    }

    #[test]
    fn multiple_requests_for_one_operator_keep_the_selected_requests_keys() {
        let mut s = workload(Engine::Mssql, 24.0, 500.0);
        let p = s.workload.as_mut().unwrap().queries[0].plan.as_mut().unwrap();
        for (key, impact_pct) in [("customer_id", 55.0), ("status", 95.0)] {
            p.index_hints.push(PlanIndexHint {
                table: "shop.orders".into(),
                equality: vec![key.into()],
                inequality: vec![],
                include: vec![],
                impact_pct,
            });
        }
        let q = &s.queries()[0];
        let found = query_plan_insights(&s, q, q.plan.as_ref().unwrap());
        let hints: Vec<_> = found.items.iter().filter(|i| i.rule == "missing-index-hint").collect();
        // Core keeps the strongest request per operator. Its identity must survive that choice, rather than
        // retrieving the first request that happens to name the same table.
        assert_eq!(hints.len(), 1);
        assert_eq!(hints[0].index_hint.as_ref().unwrap().equality, vec!["status"]);
        for i in hints {
            let key = &i.index_hint.as_ref().unwrap().equality[0];
            assert!(i.fix_sql.as_deref().unwrap().contains(&format!("([{key}])")), "{i:?}");
        }
    }

    #[test]
    fn redaction_preserves_workload_priority_and_withheld_index_sql() {
        let s = workload(Engine::Mssql, 24.0, 0.1);
        let q = &s.queries()[0];
        let mut p = q.plan.clone().unwrap();
        p.insights = Some(query_plan_insights(&s, q, &p));
        let redacted = culprit_core::redact::redact_plan(&p);
        let reading = redacted.insights.unwrap();
        assert!(!reading.items.is_empty());
        assert!(reading.items.iter().all(|i| i.severity == Severity::Low && i.fix_sql.is_none()));
        assert!(reading.note.unwrap().contains("scaling risks"));
    }

    #[test]
    fn a_failed_index_source_is_unknown_not_evidence_that_no_index_exists() {
        let mut s = workload(Engine::Mssql, 24.0, 500.0);
        s.server.capabilities.retain(|c| *c != culprit_core::Capability::IndexUsage);
        s.notes.push(culprit_core::CollectorNote {
            collector: "indexes".into(),
            message: "Index definitions unavailable".into(),
        });
        let q = &s.queries()[0];
        let found = query_plan_insights(&s, q, q.plan.as_ref().unwrap());
        assert!(!found.items.is_empty());
        assert!(found.items.iter().all(|i| i.severity == Severity::Low && i.fix_sql.is_none()));
        assert!(found.items.iter().any(|i| i.fix.contains("definitions were unavailable")));
    }

    #[test]
    fn advisor_catalog_rows_are_distinct_from_captured_cardinality() {
        let mut s = workload(Engine::Mssql, 24.0, 500.0);
        let t = s.tables.iter_mut().find(|t| t.schema == "shop" && t.name == "orders").unwrap();
        t.rows = Some(2_000_000.0);
        let report = crate::analyze(s);
        let advice = crate::index_advice(&report);
        let c = advice.candidates.iter().find(|c| c.table == "orders").unwrap();
        assert_eq!(c.table_rows, Some(2_000_000.0));
        assert!(c.reason.contains("1.0M") && c.reason.contains("2.0M"), "{}", c.reason);
        assert!(!c.verify.iter().any(|v| v.contains("becomes a seek") || v.contains("share of database time drops")));
    }
}
