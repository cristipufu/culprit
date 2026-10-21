//! Forced plan inspection: scans in forced plans that an existing index could seek, active forced plans summary, and
//! forcing failure reason hints.

use super::*;
use crate::indexes::SCAN_ROWS;

/// A scan in a query's forced plan of a table an existing index could seek, the way the scan filters it.
pub(super) struct SeekableScan<'a> {
    node: u32,
    schema: String,
    table: String,
    columns: Vec<String>,
    rows: f64,
    index: &'a IndexStat,
    /// The share of the rows read that the scan keeps.
    kept: f64,
    /// Columns the scan returns that the index doesn't hold, so a seek would look each row up in the table; empty when
    /// it covers them.
    lookups: Vec<String>,
}

/// An index that doesn't hold every column the query reads costs a lookup per row it finds: worth it over a scan only
/// for a filter far narrower than an index's usual bar.
const LOOKUPS_KEEP_AT_MOST: f64 = 0.01;

pub(super) fn seekable_scan<'a>(snapshot: &'a Snapshot, q: &QueryStat, plan_id: &str) -> Option<SeekableScan<'a>> {
    // The captured plan is the one that took the most time: the forced one, once the forcing has taken hold.
    let dominant = q.plans.iter().max_by(|a, b| a.total_time_ms.total_cmp(&b.total_time_ms))?;
    if dominant.id != plan_id {
        return None;
    }
    q.plan.as_ref()?.nodes().into_iter().find_map(|node| {
        if node.kind != NodeKind::Scan {
            return None;
        }
        let (schema, table) = split_object(node.object.as_deref()?)?;
        let rows = crate::indexes::scan_rows(snapshot, node, &schema, &table)?;
        if rows < SCAN_ROWS {
            return None;
        }
        // The bar every index claim uses (`PlanNode::kept_share`): a scan that keeps much of what it reads is the right
        // plan, forced or not. Rows read as `scan_rows` counts them: a whole table, unless the index is filtered.
        let kept =
            (node.act_rows.unwrap_or_else(|| node.est_rows_total()) / (rows * node.est_executions.max(1.0))).min(1.0);
        if kept > culprit_core::INDEX_KEEPS_AT_MOST {
            return None;
        }
        let cols = predicate_columns(node.predicate.as_deref()?);
        let mut columns = cols.equality.clone();
        columns.extend(cols.range.into_iter().take(1));
        if columns.is_empty() {
            return None;
        }
        let index = leading_index(snapshot, &schema, &table, &cols.equality, &columns)?;
        // Without the plan's output columns, coverage is unknown: count it as lookups.
        let lookups = match node.facts().output_columns.as_ref() {
            Some(list) => uncovered(snapshot, index, list),
            None => vec!["the columns it returns".to_string()],
        };
        if !lookups.is_empty() && kept > LOOKUPS_KEEP_AT_MOST {
            return None;
        }
        Some(SeekableScan { node: node.id, schema, table, columns, rows, index, kept, lookups })
    })
}

/// Columns of a plan's output list the index doesn't hold: not a key, not included, and not the clustered key every
/// nonclustered index carries.
pub(super) fn uncovered(snapshot: &Snapshot, index: &IndexStat, output: &[String]) -> Vec<String> {
    if index.is_clustered {
        return Vec::new();
    }
    let clustered = snapshot.indexes_on(&index.schema, &index.table).find(|i| i.is_clustered);
    let held: Vec<String> = index
        .key_names()
        .into_iter()
        .map(str::to_string)
        .chain(index.include.iter().cloned())
        .chain(clustered.map(|c| c.key_names().into_iter().map(str::to_string).collect::<Vec<_>>()).unwrap_or_default())
        .map(|c| c.to_ascii_lowercase())
        .collect();
    output
        .iter()
        .filter(|c| !c.is_empty() && !c.starts_with("Expr") && !held.contains(&c.to_ascii_lowercase()))
        .cloned()
        .collect()
}

pub(super) fn forced_scan_finding(
    snapshot: &Snapshot,
    p: &ForcedPlan,
    q: &QueryStat,
    how: &str,
    forced_at: Option<&str>,
    scan: SeekableScan,
) -> Finding {
    let table_name = format!("{}.{}", scan.schema, scan.table);
    let index = &scan.index.name;
    let columns = format::list(&scan.columns);
    let mut f = finding(
        "tuning-advice",
        &format!("forced-scan:{}", p.query_id),
        Category::Queries,
        subject(SubjectKind::Query, p.query_id.clone()),
    );
    f.title = format!("A forced plan makes {} scan {} though {index} could seek it", query_handle(q), scan.table);
    let lookups = if scan.lookups.is_empty() {
        String::new()
    } else {
        format!(
            " {index} doesn't hold {}, so a seek would look each row it finds up in the table: still far less than \
             reading every row for the {} it keeps.",
            format::list(&scan.lookups),
            format::percent(scan.kept)
        )
    };
    // "Forced before the index existed" only where something dates the index as new (a scan saw it appear, or the
    // database dated a change to its table), and not to before the forcing. SQL Server doesn't say which time zone the
    // tuning view's times are in, so within the widest UTC offset (14 hours) of each other their order is unknown.
    const ZONE_UNKNOWN_SECONDS: f64 = 14.0 * 3600.0;
    let before_forcing = |at: &str| match (parse_epoch_seconds(at), forced_at.and_then(parse_epoch_seconds)) {
        (Some(created), Some(forced)) => created <= forced + ZONE_UNKNOWN_SECONDS,
        _ => false,
    };
    let newer = match scan.index.created_at_latest.as_deref().filter(|at| !before_forcing(at)) {
        Some(at) => format!(
            " {index} may be new (Culprit dates it to {} at the latest), so this plan was most likely forced before it \
             existed.",
            format::friendly_date(at, &snapshot.taken_at)
        ),
        None => String::new(),
    };
    f.summary = format!(
        "Plan {} is {how} for query {}. SQL Server runs a forced plan as it is, even after an index is added, so this \
         one reads all of {table_name} ({} rows) and keeps about {} of them to find the rows by {columns}, while \
         {index} leads with {} and could seek them.{lookups}{newer}",
        p.plan_id,
        p.query_id,
        format::count(scan.rows),
        format::percent(scan.kept),
        if scan.columns.len() == 1 { "it" } else { "them" },
    );
    match query_impact(snapshot, &[q]) {
        Some(impact) => {
            apply_query_impact(&mut f, &impact);
            f.severity = severity_for_impact(snapshot, &[q], &impact);
        }
        None => {
            f.severity = Severity::Medium;
            f.impact.score = base_score(f.severity);
            f.impact.label = "Forced plan".into();
        }
    }
    f.confidence = Confidence::Medium;
    f.metrics.push(Metric::text("Forced plan", p.plan_id.clone()));
    f.metrics.push(Metric::text("Index that could seek", index.clone()));
    f.metrics.push(Metric::new("Rows in table", scan.rows, Unit::Rows));
    f.queries = vec![QueryRef {
        id: q.id.clone(),
        plan_node: Some(scan.node),
        note: Some(format!("Scans {} under the forced plan.", scan.table)),
    }];
    f.last_seen = q.last_seen.clone();
    f.fix = Some(Fix {
        summary: format!(
            "Unforce it, so SQL Server compiles a plan that can seek {index}. If automatic plan correction is on and the \
             new plan turns out slower, it forces a plan again, chosen among the plans it has seen since."
        ),
        sql: Some(format!("EXEC sp_query_store_unforce_plan @query_id = {}, @plan_id = {};", p.query_id, p.plan_id)),
        cost: None,
    });
    f.verify = vec![
        format!("Re-scan: query {} seeks {index}, and its reads and average duration drop.", p.query_id),
        format!(
            "If it gets slower, force the plan again: EXEC sp_query_store_force_plan @query_id = {}, @plan_id = {};",
            p.query_id, p.plan_id
        ),
    ];
    f
}

/// Plans listed one by one in the forced-plans finding; the rest are counted.
pub(super) const FORCED_LISTED: usize = 12;

pub(super) fn forced_plans_finding(snapshot: &Snapshot, kept: &[(&ForcedPlan, Option<&QueryStat>, String)]) -> Finding {
    let n = kept.len();
    let automatic = kept.iter().filter(|(_, _, how)| how.contains("automatic tuning")).count();
    let mut f = finding(
        "tuning-advice",
        "forced-plans",
        Category::Queries,
        subject(SubjectKind::Database, snapshot.server.database.clone()),
    );
    f.title = match (n, automatic) {
        (1, 1) => "Automatic tuning forced a plan in Query Store".to_string(),
        (1, _) => "A plan is forced in Query Store".to_string(),
        (n, a) if a == n => format!("{n} plans are forced in Query Store, all by automatic tuning"),
        (n, 0) => format!("{n} plans are forced in Query Store"),
        (n, a) => format!("{n} plans are forced in Query Store, {a} by automatic tuning"),
    };
    let automatic_sentence = match (n, automatic) {
        (_, 0) => String::new(),
        (1, _) => " Automatic plan correction forced it after the query got slower with a new plan.".to_string(),
        (n, a) if a == n => {
            " Automatic plan correction forced all of them after their queries got slower with a new plan.".to_string()
        }
        (_, 1) => {
            " Automatic plan correction forced one of them after its query got slower with a new plan.".to_string()
        }
        (_, a) => {
            format!(" Automatic plan correction forced {a} of them after their queries got slower with a new plan.")
        }
    };
    // Automatic plan correction keeps checking the plans it forced and unforces one that turns out slower, while it is
    // on; a plan forced by hand (or once it's off) stays forced until someone unforces it.
    let correcting = crate::tuning::automatic_plan_correction(snapshot) == Some(true);
    let checked = if correcting { automatic } else { 0 };
    let unchecked = match (n - checked, checked) {
        (0, _) => " It is still on, so it keeps checking the plans it forced and unforces one that turns out slower."
            .to_string(),
        (_, 0) => " Once the query or its indexes change, a forced plan can become the slower one without anything \
                   saying so."
            .to_string(),
        (1, _) => " It is still on, so it keeps checking the plans it forced and unforces one that turns out slower. \
                   The other plan stays forced until someone unforces it: once its query or indexes change, it can \
                   become the slower one without anything saying so."
            .to_string(),
        (rest, _) => format!(
            " It is still on, so it keeps checking the plans it forced and unforces one that turns out slower. The \
             other {rest} stay forced until someone unforces them: once their queries or indexes change, one can \
             become the slower one without anything saying so."
        ),
    };
    f.summary = format!(
        "SQL Server runs {} with the plan forced for {} instead of choosing one, even after an index is added or the \
         data grows. A forced plan keeps a query on the plan it ran fast with, so a new plan can't slow it \
         down.{automatic_sentence}{unchecked}",
        if n == 1 { "this query" } else { "these queries" },
        if n == 1 { "it" } else { "each" },
    );
    f.severity = Severity::Low;
    f.impact.score = base_score(f.severity) + (n as f64).min(10.0) * 0.2;
    f.impact.label = format!("{n} forced {}", if n == 1 { "plan" } else { "plans" });
    f.confidence = Confidence::High;
    f.metrics.push(Metric::new("Forced plans", n as f64, Unit::Count));
    if automatic > 0 {
        f.metrics.push(Metric::new("Forced by automatic tuning", automatic as f64, Unit::Count));
    }
    let mut lines: Vec<String> = kept
        .iter()
        .take(FORCED_LISTED)
        .map(|(p, q, how)| {
            let what = q.map(query_handle).map(|h| format!(" ({h})")).unwrap_or_default();
            let ran = match (q, quiet_since(snapshot, p.last_execution.as_deref())) {
                (None, Some((_, last))) => {
                    format!("; not run since {}", format::friendly_date(last, &snapshot.taken_at))
                }
                _ => String::new(),
            };
            format!("Query {}{what}: plan {}, {how}{ran}.", p.query_id, p.plan_id)
        })
        .collect();
    if n > FORCED_LISTED {
        lines.push(format!("And {} more.", n - FORCED_LISTED));
    }
    f.caveats.push(format!("Forced plans:\n{}", lines.join("\n")));
    f.queries = kept
        .iter()
        .filter_map(|(p, q, _)| q.map(|q| (p, q)))
        .take(FORCED_LISTED)
        .map(|(p, q)| QueryRef {
            id: q.id.clone(),
            plan_node: None,
            note: Some(format!("Plan {} is forced.", p.plan_id)),
        })
        .collect();
    f.informational();
    f.fix = Some(Fix {
        summary: "Nothing to do while they help. Unforce one whose query or indexes changed since it was forced and \
            compare its duration before and after: SQL Server chooses a plan again."
            .into(),
        sql: Some(
            std::iter::once("-- One at a time, comparing the query's duration before and after".to_string())
                .chain(kept.iter().take(FORCED_LISTED).map(|(p, _, _)| {
                    format!("-- EXEC sp_query_store_unforce_plan @query_id = {}, @plan_id = {};", p.query_id, p.plan_id)
                }))
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        cost: None,
    });
    f.verify = vec!["Re-scan after unforcing one: its query's average duration stays the same or drops.".into()];
    f
}

pub(super) fn failure_hint(reason: &str) -> &'static str {
    let r = reason.to_ascii_uppercase();
    if r.contains("NO_INDEX") {
        "An index the plan uses no longer exists."
    } else if r.contains("NO_PLAN") || r.contains("GENERAL_FAILURE") {
        "The query or schema changed so the plan can no longer be produced."
    } else if r.contains("TIME_OUT") || r.contains("TIMEOUT") {
        "Compiling with the forced plan took too long."
    } else {
        ""
    }
}

#[cfg(test)]
mod output_tests {
    use super::*;
    #[test]
    fn captured_output_column_names_are_not_split_into_qualifiers_or_csv_fields() {
        let mut snapshot: Snapshot =
            serde_json::from_str(include_str!("../../../../../../fixtures/mssql-demo.json")).unwrap();
        let mut index = snapshot.indexes[0].clone();
        index.is_clustered = false;
        index.columns = vec!["id".into()];
        index.include = vec!["odd.column".into(), "a, b".into(), "two words".into()];
        snapshot.indexes = vec![index.clone()];
        assert!(
            uncovered(&snapshot, &index, &["id".into(), "odd.column".into(), "a, b".into(), "two words".into()])
                .is_empty()
        );
        assert_eq!(uncovered(&snapshot, &index, &["uncovered.column".into()]), vec!["uncovered.column"]);
    }
}
