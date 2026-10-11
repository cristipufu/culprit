//! Table sizes for plans. PostgreSQL's estimated plans say how many rows a scan expects to return, not how many it
//! reads: a sequential scan reads the whole table. The planner's own statistics (`pg_class.reltuples`) say how big
//! that is, so a scan expecting five rows of a two-million-row table reads as what it is.

use std::collections::HashMap;

use culprit_core::{NodeKind, Plan, PlanNode};
use tokio_postgres::Client;

use crate::rows::tagged;

const SIZES_SQL: &str =
    "SELECT n.nspname || '.' || c.relname AS qualified, c.relname AS name, c.reltuples::float8 AS rows
  FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
 WHERE c.relkind IN ('r', 'm', 'p')
   AND ((n.nspname || '.' || c.relname) = ANY($1) OR (c.relname = ANY($1) AND pg_table_is_visible(c.oid)))";

/// Fill `table_rows` on the plan's scans, and `rows_read` on sequential scans an estimated plan left without one.
/// Best effort: a failed lookup leaves the plan as it was.
pub async fn fill(client: &Client, plan: &mut Plan) {
    let mut names: Vec<String> = plan
        .nodes()
        .into_iter()
        .filter(|n| matches!(n.kind, NodeKind::Scan | NodeKind::Seek))
        .filter_map(|n| n.object.clone())
        .collect();
    names.sort();
    names.dedup();
    if names.is_empty() {
        return;
    }
    // The names are bound as one text[] parameter, never spliced into the SQL.
    let Ok(rows) = client.query(tagged(SIZES_SQL).as_str(), &[&names]).await else { return };
    let mut sizes: HashMap<String, f64> = HashMap::new();
    for row in rows {
        let (Ok(qualified), Ok(name), Ok(count)) =
            (row.try_get::<_, String>(0), row.try_get::<_, String>(1), row.try_get::<_, f64>(2))
        else {
            continue;
        };
        // -1 means never analyzed: no estimate to give.
        if count >= 0.0 {
            sizes.insert(qualified, count);
            sizes.entry(name).or_insert(count);
        }
    }
    apply(&mut plan.root, &sizes);
}

fn apply(node: &mut PlanNode, sizes: &HashMap<String, f64>) {
    if let Some(rows) = node.object.as_ref().and_then(|o| sizes.get(o)).copied() {
        node.table_rows = Some(rows);
        if node.kind == NodeKind::Scan
            && node.op.contains("Seq Scan")
            && node.rows_read.is_none()
            && node.act_rows.is_none()
        {
            node.rows_read = Some(rows);
        }
    }
    for child in node.children.iter_mut() {
        apply(child, sizes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use culprit_core::PlanKind;

    #[test]
    fn a_sequential_scan_reads_the_whole_table() {
        let json = r#"[{"Plan": {"Node Type": "Seq Scan", "Relation Name": "orders", "Schema": "public",
            "Plan Rows": 5, "Total Cost": 4500.0, "Filter": "(customer_id = $1)"}}]"#;
        let mut plan = crate::plan_json::parse(json, PlanKind::Generic, Some("SELECT 1")).unwrap();
        let sizes = HashMap::from([("public.orders".to_string(), 2_000_000.0)]);
        apply(&mut plan.root, &sizes);
        assert_eq!(plan.root.table_rows, Some(2_000_000.0));
        assert_eq!(plan.root.rows_read, Some(2_000_000.0));
        // And the plan's insights now see a scan that throws almost everything away.
        let found = culprit_core::insights::insights(&plan);
        assert!(
            found.iter().any(|i| i.rule == "scan-filter"),
            "{:?}",
            found.iter().map(|i| &i.rule).collect::<Vec<_>>()
        );
    }
}
