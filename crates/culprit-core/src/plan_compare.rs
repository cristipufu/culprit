//! Why two plans of the same statement differ, in plain language: the conditions the optimizer saw (estimator version,
//! parameter values, statistics, memory, parallelism, plan kind) rather than the operators it picked. Works on plans
//! from every engine; each scenario only fires when both plans carry the evidence for it.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::format;
use crate::model::{Engine, StatisticsInfo};
use crate::plan::{NodeKind, Plan, PlanKind, PlanNode, PlanWarning};
use crate::plan_diff::PlanDiff;
use crate::redact::redact_sql;

/// Estimates must differ by this factor, and the larger reach this many rows, to be a scenario.
const ESTIMATE_FACTOR: f64 = 10.0;
const ESTIMATE_MIN_ROWS: f64 = 100.0;
/// Memory grants must differ by this factor, and the larger reach this many KB.
const MEMORY_FACTOR: f64 = 4.0;
const MEMORY_MIN_KB: f64 = 10.0 * 1024.0;
/// Statistics this far behind their table's data are worth naming.
const STALE_SHARE: f64 = 0.1;
const MAX_ESTIMATES: usize = 3;

#[derive(Serialize, Deserialize, TS, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
#[ts(export)]
pub enum ScenarioKind {
    /// The plans are for different statement text.
    Statement,
    /// One plan is estimated, the other actual (or generic).
    PlanKind,
    /// SQL Server's cardinality estimator version differs (compatibility level or hint).
    Estimator,
    /// The plans were compiled for different parameter values.
    Parameters,
    /// Row estimates differ between the plans, or from what actually happened.
    Estimates,
    /// The two plans read the same table through different indexes.
    Index,
    /// One plan converts a column's type before comparing it, so it can't seek an index on it.
    Conversion,
    /// One plan runs in parallel and the other doesn't.
    Parallelism,
    /// Memory grants differ a lot, or one plan spills.
    Memory,
}

/// One condition that differs between the plans, with what it does to the plan.
#[derive(Serialize, Deserialize, TS, Clone, Debug)]
#[ts(export)]
pub struct Scenario {
    pub kind: ScenarioKind,
    pub title: String,
    pub detail: Option<String>,
    /// Node in plan A this is about, when it is about one.
    pub a: Option<u32>,
    pub b: Option<u32>,
}

/// What Culprit knows about the database the plans ran on, from its latest scan: optimizer statistics, so a scenario
/// can say "statistics on orders are stale". Empty when the plans came from files.
#[derive(Serialize, Deserialize, TS, Clone, Debug, Default)]
#[ts(export)]
pub struct CompareContext {
    #[serde(default)]
    pub statistics: Vec<StatisticsInfo>,
    /// When the statistics were read (the scan time), so ages are honest.
    #[serde(default)]
    pub taken_at: Option<String>,
}

pub(crate) fn scenarios(
    a: &Plan,
    b: &Plan,
    diff: &PlanDiff,
    name_a: &str,
    name_b: &str,
    ctx: &CompareContext,
) -> Vec<Scenario> {
    let mut out = Vec::new();
    let nodes_a: HashMap<u32, &PlanNode> = a.nodes().into_iter().map(|n| (n.id, n)).collect();
    let nodes_b: HashMap<u32, &PlanNode> = b.nodes().into_iter().map(|n| (n.id, n)).collect();
    let pairs: Vec<(&PlanNode, &PlanNode)> =
        diff.matches.iter().filter_map(|m| Some((*nodes_a.get(&m.a)?, *nodes_b.get(&m.b)?))).collect();

    if let (Some(sa), Some(sb)) = (a.statement.as_deref(), b.statement.as_deref())
        && a.engine == b.engine
        && shape(sa, a.engine) != shape(sb, b.engine)
    {
        out.push(Scenario {
            kind: ScenarioKind::Statement,
            title: "These plans are for different statements.".into(),
            detail: Some(
                "Some differences may come from the SQL itself (another filter, join or column), not from the optimizer."
                    .into(),
            ),
            a: None,
            b: None,
        });
    }

    if a.kind != b.kind {
        let (actual, other) = if a.kind == PlanKind::Actual { (name_a, name_b) } else { (name_b, name_a) };
        let title = if a.kind == PlanKind::Actual || b.kind == PlanKind::Actual {
            format!("{} is an actual plan and {} isn't.", cap(actual), other)
        } else {
            format!("{} is a {} plan and {} a {} plan.", cap(name_a), kind_word(a.kind), name_b, kind_word(b.kind))
        };
        out.push(Scenario {
            kind: ScenarioKind::PlanKind,
            title,
            detail: Some(
                "Compare estimates with estimates; actual rows and times only exist on the executed plan.".into(),
            ),
            a: None,
            b: None,
        });
    }

    let ce = |p: &Plan| p.facts().cardinality_estimator.map(|v| v.to_string());
    if let (Some(x), Some(y)) = (ce(a), ce(b))
        && x != y
    {
        out.push(Scenario {
            kind: ScenarioKind::Estimator,
            title: format!("Different cardinality estimators: version {x} in {name_a}, {y} in {name_b}."),
            detail: Some(
                "SQL Server estimates rows differently per estimator version. It follows the database's compatibility \
                 level unless a hint or LEGACY_CARDINALITY_ESTIMATION overrides it, so a compatibility-level change \
                 alone can change the plan."
                    .into(),
            ),
            a: None,
            b: None,
        });
    }

    let params = |p: &Plan| p.facts().compiled_parameters.clone();
    let parameters_differ = matches!((params(a), params(b)), (Some(x), Some(y)) if x != y);
    if let (Some(x), Some(y)) = (params(a), params(b))
        && x != y
    {
        out.push(Scenario {
            kind: ScenarioKind::Parameters,
            title: format!("Compiled for different parameter values: {name_a} for {x}; {name_b} for {y}."),
            detail: Some(
                "A plan is optimized for the values it was first compiled with. When some values match a few rows and \
                 others many, the same query gets very different plans (parameter sensitivity)."
                    .into(),
            ),
            a: None,
            b: None,
        });
    }

    estimate_scenarios(&mut out, &pairs, a, b, name_a, name_b, parameters_differ, ctx);

    // Per table, the indexes each plan reads it through (a seek plus a lookup uses two; a scan of the table uses one).
    let mut lines: Vec<(String, String)> = Vec::new();
    for (table_name, (in_a, in_b)) in index_use(a, b) {
        if in_a == in_b {
            continue;
        }
        let uses = |list: &[String]| if list.is_empty() { "no index".to_string() } else { format::list(list) };
        lines.push((
            table_name.clone(),
            format!("{table_name}: {name_a} uses {}, {name_b} uses {}.", uses(&in_a), uses(&in_b)),
        ));
    }
    if !lines.is_empty() {
        let tables: Vec<String> = lines.iter().map(|(t, _)| t.clone()).collect();
        let detail: Vec<String> = lines.iter().map(|(_, l)| l.clone()).collect();
        out.push(Scenario {
            kind: ScenarioKind::Index,
            title: format!("Different indexes on {}.", format::list(&tables)),
            detail: Some(format!(
                "{} If an index was added, dropped or rebuilt between the two plans, that explains it; otherwise the \
                 optimizer chose differently because it expected a different number of rows.",
                detail.join(" ")
            )),
            a: None,
            b: None,
        });
    }

    let converts = |p: &Plan| {
        p.facts()
            .conversions
            .iter()
            .find(|c| !crate::insights::current_conversions(p, &c.expression).is_empty())
            .map(|c| c.text())
            .or_else(|| {
                p.nodes()
                    .into_iter()
                    .filter(|n| n.warnings.contains(&PlanWarning::ImplicitConversion))
                    .flat_map(|n| [n.predicate.as_ref(), n.seek_predicate.as_ref()])
                    .flatten()
                    .find(|text| !crate::insights::current_conversions(p, text).is_empty())
                    .cloned()
            })
    };
    let (ca, cb) = (converts(a), converts(b));
    if ca.is_some() != cb.is_some() {
        let (who, other, what) = if cb.is_some() { (name_b, name_a, cb) } else { (name_a, name_b, ca) };
        out.push(Scenario {
            kind: ScenarioKind::Conversion,
            title: format!("{} converts a column's type before comparing it; {other} doesn't.", cap(who)),
            detail: Some(format!(
                "A converted column can't be searched through its index, so the plan reads every entry instead.{} \
                 This usually means the application sent the parameter with a different type (e.g. nvarchar for a \
                 varchar column).",
                what.map(|w| {
                    // SQL Server prefixes each issue with its effect ("Seek Plan: …", "Cardinality Estimate: …").
                    let expr = w.split_once(": ").filter(|(k, _)| k.len() < 30).map_or(w.as_str(), |(_, e)| e);
                    format!(" Here: {}.", format::predicate(expr))
                })
                .unwrap_or_default()
            )),
            a: None,
            b: None,
        });
    }

    let (pa, pb) = (parallel(a), parallel(b));
    if pa.is_some() != pb.is_some() {
        let (yes, no, plan_no, dop) =
            if let Some(d) = pa { (name_a, name_b, b, d) } else { (name_b, name_a, a, pb.unwrap_or(0)) };
        let reason = plan_no.facts().parallelism_blocker.as_ref().map(|r| humanize(r));
        let dop = if dop > 1 { format!(" ({dop} threads)") } else { String::new() };
        out.push(Scenario {
            kind: ScenarioKind::Parallelism,
            title: format!("{} runs in parallel{dop}; {no} runs on one thread.", cap(yes)),
            detail: Some(match reason {
                Some(r) => format!("{} couldn't go parallel: {r}.", cap(no)),
                None => "Parallelism helps large scans and joins and costs more CPU; the optimizer only chooses it \
                         when the estimated cost passes the server's threshold."
                    .into(),
            }),
            a: None,
            b: None,
        });
    }

    let grant = |p: &Plan| p.facts().memory_granted_kb.or(p.facts().memory_desired_kb);
    if let (Some(ga), Some(gb)) = (grant(a), grant(b))
        && ga.max(gb) >= MEMORY_MIN_KB
        && ratio(ga, gb) >= MEMORY_FACTOR
    {
        let (more, less, big, small) = if gb > ga { (name_b, name_a, gb, ga) } else { (name_a, name_b, ga, gb) };
        out.push(Scenario {
            kind: ScenarioKind::Memory,
            title: format!("{} reserves {} of memory per run; {less} {}.", cap(more), kb(big), kb(small)),
            detail: Some(
                "Sorts and hashes get memory sized from the row estimates. A large grant on a busy server makes other \
                 queries wait for memory; a small one spills to tempdb."
                    .into(),
            ),
            a: None,
            b: None,
        });
    }
    let spills = |p: &Plan| p.has_warning(PlanWarning::Spill);
    if spills(a) != spills(b) {
        let (yes, no) = if spills(b) { (name_b, name_a) } else { (name_a, name_b) };
        out.push(Scenario {
            kind: ScenarioKind::Memory,
            title: format!("{} spills to disk; {no} doesn't.", cap(yes)),
            detail: Some(
                "Its sort or hash got less memory than the rows needed, usually because it expected fewer rows than it \
                 read."
                    .into(),
            ),
            a: None,
            b: None,
        });
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn estimate_scenarios(
    out: &mut Vec<Scenario>,
    pairs: &[(&PlanNode, &PlanNode)],
    a: &Plan,
    b: &Plan,
    name_a: &str,
    name_b: &str,
    parameters_differ: bool,
    ctx: &CompareContext,
) {
    let mut found: Vec<(f64, Scenario)> = Vec::new();
    // Between the plans: the same step expecting very different row counts.
    for (x, y) in pairs {
        let (ta, tb) = (x.est_rows_total(), y.est_rows_total());
        // A step run once per outer row (a nested loop's inner side) is estimated per execution: two seeks that each
        // expect one row differ only in how often they run, which their outer sides' own estimates say. And a seek
        // per row against one scan of the table expects as many rows overall: a different plan, not another estimate.
        let repeats = x.est_executions > 1.0 || y.est_executions > 1.0;
        let (ea, eb) = if repeats { (x.est_rows, y.est_rows) } else { (ta, tb) };
        if ta.max(tb) < ESTIMATE_MIN_ROWS
            || ratio(ta, tb) < ESTIMATE_FACTOR
            || ratio(ea, eb) < ESTIMATE_FACTOR
            || x.object.is_none()
        {
            continue;
        }
        let why = if parameters_differ {
            Some("The parameter values they were compiled for likely match very different numbers of rows.".to_string())
        } else {
            stale(ctx, x)
                .map(|s| format!("{s} The two plans may have been compiled before and after an update."))
                .or_else(|| {
                    Some(
                        "Different row estimates lead the optimizer to different joins and access paths. They come from \
                         different parameter values, newer statistics or a different estimator."
                            .into(),
                    )
                })
        };
        found.push((
            ratio(ea, eb),
            Scenario {
                kind: ScenarioKind::Estimates,
                title: format!(
                    "On {}, {name_a} expects {} and {name_b} expects {}{}.",
                    table(y),
                    rows(ea),
                    rows(eb),
                    if repeats { " per execution" } else { "" }
                ),
                detail: why,
                a: Some(x.id),
                b: Some(y.id),
            },
        ));
    }
    // Within an executed plan: estimated vs actual, by the rule behind the Misestimate warning
    // (`PlanNode::misestimates`): the estimate for the runs that happened, and an operator a Limit, a semi join or a
    // merge join stopped early isn't blamed for the rows it never had to read.
    for (plan, name, side_a) in [(a, name_a, true), (b, name_b, false)] {
        let nodes: HashMap<u32, &PlanNode> = plan.nodes().into_iter().map(|n| (n.id, n)).collect();
        for (id, factor) in plan.root.misestimates() {
            let Some(node) = nodes.get(&id) else { continue };
            // Data access only: the operators above repeat the same misestimate.
            let (Some(actual), Some(_)) = (node.act_rows, &node.object) else { continue };
            let estimated = node.est_rows_for_runs();
            let title = format!(
                "{}: estimated {} from {}, got {}.",
                cap(name),
                rows(estimated),
                table(node),
                format::count(actual)
            );
            let detail = stale(ctx, node).or_else(|| {
                Some("Estimates this far off lead to the wrong join type, too little memory or a needless scan.".into())
            });
            let (ia, ib) = if side_a { (Some(node.id), None) } else { (None, Some(node.id)) };
            found.push((factor.abs(), Scenario { kind: ScenarioKind::Estimates, title, detail, a: ia, b: ib }));
        }
    }
    found.sort_by(|x, y| y.0.total_cmp(&x.0));
    out.extend(found.into_iter().take(MAX_ESTIMATES).map(|(_, s)| s));
}

/// A table, the indexes plan A reads it through and the indexes plan B reads it through.
type IndexUse = (String, (Vec<String>, Vec<String>));

/// How each plan reads the tables both plans read.
fn index_use(a: &Plan, b: &Plan) -> Vec<IndexUse> {
    let collect = |p: &Plan| {
        let mut map: Vec<(String, Vec<String>)> = Vec::new();
        for n in p.nodes() {
            if n.object.is_none() {
                continue;
            }
            let t = table(n);
            let entry = match map.iter_mut().position(|(k, _)| k.eq_ignore_ascii_case(&t)) {
                Some(i) => &mut map[i].1,
                None => {
                    map.push((t, Vec::new()));
                    &mut map.last_mut().expect("just pushed").1
                }
            };
            if let Some(index) = n.index.as_deref() {
                let name = unquote(index);
                if !entry.iter().any(|e| e.eq_ignore_ascii_case(&name)) {
                    entry.push(name);
                }
            }
        }
        map
    };
    let (ma, mb) = (collect(a), collect(b));
    ma.into_iter()
        .filter_map(|(t, mut ia)| {
            let mut ib = mb.iter().find(|(k, _)| k.eq_ignore_ascii_case(&t))?.1.clone();
            ia.sort_by_key(|x| x.to_lowercase());
            ib.sort_by_key(|x| x.to_lowercase());
            let same = ia.len() == ib.len() && ia.iter().zip(&ib).all(|(x, y)| x.eq_ignore_ascii_case(y));
            Some((t, if same { (ia.clone(), ia) } else { (ia, ib) }))
        })
        .collect()
}

/// "Statistics on orders are stale: 23% of the table changed since they were updated, 6 days before the scan." for
/// the node's table, when Culprit knows.
fn stale(ctx: &CompareContext, node: &PlanNode) -> Option<String> {
    let object = node.object.as_deref()?;
    let name = bare(object);
    let schema = object.rsplit('.').nth(1).map(bare);
    let worst = ctx
        .statistics
        .iter()
        .filter(|s| bare(&s.table) == name && schema.as_deref().is_none_or(|sc| bare(&s.schema) == sc))
        .filter_map(|s| Some((s.modifications? / s.rows?.max(1.0), s)))
        .max_by(|x, y| x.0.total_cmp(&y.0))?;
    if worst.0 < STALE_SHARE {
        return None;
    }
    let age = match (&worst.1.last_updated, &ctx.taken_at) {
        (Some(updated), Some(now)) => format::age_between(updated, now).map(|a| format!(", {a} before the scan")),
        _ => None,
    }
    .unwrap_or_default();
    Some(format!(
        "Statistics on {} are stale: {} of the table changed since they were updated{age}.",
        unquote(object.rsplit('.').next().unwrap_or(object)),
        format::percent(worst.0.min(9.99))
    ))
}

/// The statement with literals replaced and whitespace and case folded, so the same query with other values compares
/// equal.
fn shape(sql: &str, engine: Engine) -> String {
    redact_sql(sql, engine).split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

/// Threads when the plan runs in parallel: SQL Server's DegreeOfParallelism, or parallelism operators (Gather).
fn parallel(plan: &Plan) -> Option<u32> {
    let dop = plan.facts().degree_of_parallelism;
    if let Some(d) = dop {
        return (d > 1).then_some(d);
    }
    plan.nodes().iter().any(|n| n.kind == NodeKind::Parallelism).then_some(0)
}

/// "CouldNotGenerateValidParallelPlan" → "could not generate valid parallel plan".
fn humanize(reason: &str) -> String {
    let mut out = String::new();
    for (i, c) in reason.chars().enumerate() {
        if c.is_uppercase() && i > 0 {
            out.push(' ');
        }
        out.extend(c.to_lowercase());
    }
    out
}

fn rows(n: f64) -> String {
    let rounded = n.round();
    format!("{} row{}", format::count(n), if rounded == 1.0 { "" } else { "s" })
}

fn kind_word(kind: PlanKind) -> &'static str {
    match kind {
        PlanKind::Estimated => "estimated",
        PlanKind::Generic => "generic",
        PlanKind::Actual => "actual",
    }
}

fn table(node: &PlanNode) -> String {
    let object = node.object.as_deref().unwrap_or("?");
    unquote(object.rsplit('.').next().unwrap_or(object))
}

fn bare(name: &str) -> String {
    unquote(name.rsplit('.').next().unwrap_or(name)).to_lowercase()
}

fn unquote(name: &str) -> String {
    name.chars().filter(|c| !matches!(c, '[' | ']' | '"' | '`')).collect()
}

fn ratio(a: f64, b: f64) -> f64 {
    let (a, b) = (a.max(1.0), b.max(1.0));
    if a > b { a / b } else { b / a }
}

fn kb(value: f64) -> String {
    format::bytes(value * 1024.0)
}

/// Capitalize a generic name at the start of a sentence ("plan A" → "Plan A"); names people chose keep their case.
fn cap(s: &str) -> String {
    if !["plan ", "query ", "these "].iter().any(|w| s.starts_with(w)) {
        return s.to_string();
    }
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Engine, Properties};
    use crate::plan_diff::plan_diff_in;

    fn node(
        op: &str,
        kind: NodeKind,
        object: Option<&str>,
        index: Option<&str>,
        est: f64,
        children: Vec<PlanNode>,
    ) -> PlanNode {
        PlanNode {
            facts: None,
            id: 0,
            op: op.into(),
            kind,
            detail: None,
            object: object.map(str::to_string),
            index: index.map(str::to_string),
            seek_predicate: None,
            predicate: None,
            est_rows: est,
            est_executions: 1.0,
            act_rows: None,
            act_executions: None,
            rows_read: None,
            table_rows: None,
            subtree_cost: 1.0 + children.iter().map(|c| c.subtree_cost).sum::<f64>(),
            self_cost: 0.0,
            act_time_ms: None,
            warnings: Vec::new(),
            props: Properties::new(),
            children,
            runtime: None,
        }
    }

    fn plan(root: PlanNode, props: &[(&str, &str)]) -> Plan {
        let mut plan = Plan {
            facts: None,
            view: None,
            engine: Engine::Mssql,
            kind: PlanKind::Estimated,
            statement: Some("SELECT * FROM orders WHERE status = @p1".into()),
            total_cost: root.subtree_cost,
            root,
            warnings: Vec::new(),
            index_hints: Vec::new(),
            props: props.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            raw: None,
            insights: None,
        };
        fn number(n: &mut PlanNode, next: &mut u32) {
            n.id = *next;
            *next += 1;
            n.children.iter_mut().for_each(|c| number(c, next));
        }
        number(&mut plan.root, &mut 0);
        plan.compute_self_costs();
        plan
    }

    #[test]
    fn database_names_are_ignored_when_lining_plans_up() {
        let a = plan(
            node("Index Seek", NodeKind::Seek, Some("shop_prod.dbo.orders"), Some("ix_status"), 12.0, vec![]),
            &[],
        );
        let b = plan(
            node("Index Seek", NodeKind::Seek, Some("shop_test.dbo.orders"), Some("ix_status"), 12.0, vec![]),
            &[],
        );
        let diff = plan_diff_in(&a, "prod", &b, "test", &CompareContext::default());
        assert!(diff.same_shape, "{:#?}", diff.changes);
        assert_eq!(diff.regions.len(), 1);
    }

    #[test]
    fn stale_statistics_and_parameters_explain_different_estimates() {
        let a = plan(
            node("Index Seek", NodeKind::Seek, Some("dbo.orders"), Some("ix_status"), 12.0, vec![]),
            &[("Parameters", "@p1 = N'open'"), ("CardinalityEstimationModelVersion", "150")],
        );
        let b = plan(
            node("Clustered Index Scan", NodeKind::Scan, Some("dbo.orders"), Some("pk_orders"), 140_000.0, vec![]),
            &[("Parameters", "@p1 = N'open'"), ("CardinalityEstimationModelVersion", "160")],
        );
        let ctx = CompareContext {
            statistics: vec![StatisticsInfo {
                schema: "dbo".into(),
                table: "orders".into(),
                name: "ix_status".into(),
                last_updated: Some("2026-10-05T00:00:00Z".into()),
                rows: Some(1_000_000.0),
                rows_sampled: None,
                modifications: Some(230_000.0),
                auto_created: false,
            }],
            taken_at: Some("2026-10-11T00:00:00Z".into()),
        };
        let diff = plan_diff_in(&a, "Plan 1", &b, "Plan 2", &ctx);
        let kinds: Vec<ScenarioKind> = diff.scenarios.iter().map(|s| s.kind).collect();
        assert!(kinds.contains(&ScenarioKind::Estimator), "{kinds:?}");
        assert!(!kinds.contains(&ScenarioKind::Parameters), "same parameters are not a scenario");
        let estimates =
            diff.scenarios.iter().find(|s| s.kind == ScenarioKind::Estimates).expect("an estimates scenario");
        assert_eq!(estimates.title, "On orders, Plan 1 expects 12 rows and Plan 2 expects 140K rows.");
        let detail = estimates.detail.as_deref().unwrap();
        assert!(
            detail.starts_with(
                "Statistics on orders are stale: 23% of the table changed since they were updated, 6.0 days before the \
                 scan."
            ),
            "{detail}"
        );
        assert!(
            diff.scenarios.iter().any(|s| s.kind == ScenarioKind::Index && s.title == "Different indexes on orders.")
        );
    }

    #[test]
    fn different_parameter_values_are_named() {
        let a = plan(
            node("Index Seek", NodeKind::Seek, Some("dbo.orders"), Some("ix_status"), 12.0, vec![]),
            &[("Parameters", "@p1 = N'rare'")],
        );
        let b = plan(
            node("Clustered Index Scan", NodeKind::Scan, Some("dbo.orders"), Some("pk_orders"), 140_000.0, vec![]),
            &[("Parameters", "@p1 = N'common'")],
        );
        let diff = plan_diff_in(&a, "plan A", &b, "plan B", &CompareContext::default());
        let p = diff.scenarios.iter().find(|s| s.kind == ScenarioKind::Parameters).expect("parameters");
        assert_eq!(
            p.title,
            "Compiled for different parameter values: plan A for @p1 = N'rare'; plan B for @p1 = N'common'."
        );
        let e = diff.scenarios.iter().find(|s| s.kind == ScenarioKind::Estimates).unwrap();
        assert!(e.detail.as_deref().unwrap().contains("parameter values"));
    }

    fn estimate_titles(a: &Plan, b: &Plan) -> Vec<String> {
        let diff = plan_diff_in(a, "plan A", b, "plan B", &CompareContext::default());
        diff.scenarios.into_iter().filter(|s| s.kind == ScenarioKind::Estimates).map(|s| s.title).collect()
    }

    #[test]
    fn inner_sides_compare_their_estimate_per_execution() {
        // Plan A expects one order and plan B 140K; each seeks one customer per order. The customers seek expects one
        // row per execution in both: only how often it runs differs, and the orders estimates say that.
        let join = |orders: f64, per_order: f64| {
            let mut customers =
                node("Clustered Index Seek", NodeKind::Seek, Some("dbo.customers"), Some("pk"), per_order, vec![]);
            customers.est_executions = orders;
            let outer = node("Index Seek", NodeKind::Seek, Some("dbo.orders"), Some("ix_status"), orders, vec![]);
            plan(node("Nested Loops", NodeKind::Join, None, None, orders * per_order, vec![outer, customers]), &[])
        };
        assert_eq!(
            estimate_titles(&join(1.0, 1.0), &join(140_000.0, 1.0)),
            vec!["On orders, plan A expects 1 row and plan B expects 140K rows."]
        );
        // Another estimate per execution is a difference in the customers estimate itself.
        let titles = estimate_titles(&join(100.0, 1.0), &join(100.0, 1_000.0));
        assert_eq!(titles.len(), 1, "{titles:?}");
        assert!(titles[0].starts_with("On customers, plan A expects 1 row and plan B expects"), "{titles:?}");
        assert!(titles[0].ends_with(" per execution."), "{titles:?}");
    }

    #[test]
    fn actual_rows_are_held_to_the_estimate_for_the_runs_that_happened() {
        let ran = |mut n: PlanNode, rows: f64, runs: f64| {
            n.act_rows = Some(rows);
            n.act_executions = Some(runs);
            n
        };
        let actual = |root: PlanNode| {
            let mut p = plan(root, &[]);
            p.kind = PlanKind::Actual;
            p
        };
        // SQL Server: the orders seek expected 100 rows and got 10K, so the customers seek ran 10K times where 100 were
        // planned, and found the one row per run it expected. Only orders is misestimated.
        let mut customers =
            node("Clustered Index Seek", NodeKind::Seek, Some("dbo.customers"), Some("pk_customers"), 1.0, vec![]);
        customers.est_executions = 100.0;
        let orders = node("Index Seek", NodeKind::Seek, Some("dbo.orders"), Some("ix_status"), 100.0, vec![]);
        let join = node(
            "Nested Loops",
            NodeKind::Join,
            None,
            None,
            100.0,
            vec![ran(orders, 10_000.0, 1.0), ran(customers, 10_000.0, 10_000.0)],
        );
        let nested = actual(ran(join, 10_000.0, 1.0));
        let titles = estimate_titles(&nested, &nested);
        assert!(!titles.is_empty() && titles.iter().all(|t| t.contains("from orders")), "{titles:?}");

        // PostgreSQL: a scan's estimate is for a full run, and the Limit above stopped it after 10 rows.
        let scan = node("Seq Scan", NodeKind::Scan, Some("public.orders"), None, 5_000_000.0, vec![]);
        let limited =
            actual(ran(node("Limit", NodeKind::Other, None, None, 10.0, vec![ran(scan, 10.0, 1.0)]), 10.0, 1.0));
        assert_eq!(estimate_titles(&limited, &limited), Vec::<String>::new());
    }
}
