//! Compare two plans for the same statement: which steps changed, where the cost moved, which estimates shifted.
//! Pure, so it works on two Query Store plans, a plan before and after an index, or the same query in two environments.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::format;
use crate::plan::{NodeKind, Plan, PlanNode, PlanWarning};

/// A cost-share move smaller than this (10 points of the plan's cost) is noise.
const SHARE_SHIFT: f64 = 0.10;
/// Row counts must differ by this factor, and the larger must reach `ROWS_MIN`, to be worth a line.
const ROWS_FACTOR: f64 = 10.0;
const ROWS_MIN: f64 = 100.0;
/// Total cost or time must differ by this factor before a headline mentions it.
const COST_RATIO: f64 = 1.25;
/// Actual times must differ by this factor and the slower must reach this many milliseconds.
const TIME_FACTOR: f64 = 2.0;
const TIME_MIN_MS: f64 = 10.0;
/// Structural changes touching less than this share of either plan's cost don't make the headlines.
const HEADLINE_WEIGHT: f64 = 0.02;
const MAX_HEADLINES: usize = 3;

#[derive(Serialize, Deserialize, TS, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
#[ts(export)]
pub enum ChangeKind {
    /// The same table is read a different way (Index Seek → Index Scan, another index).
    Access,
    /// The same step uses a different operator (Nested Loops → Hash Match).
    Operator,
    /// A step only plan B has.
    Added,
    /// A step only plan A has.
    Removed,
    /// The step's share of the plan's cost, or its actual time, moved a lot.
    Cost,
    /// Estimated or actual rows changed by an order of magnitude.
    Rows,
    /// A plan warning appeared or went away.
    Warning,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug)]
#[ts(export)]
pub struct PlanChange {
    pub kind: ChangeKind,
    /// Node id in plan A, when the change involves a node there.
    pub a: Option<u32>,
    /// Node id in plan B, when the change involves a node there.
    pub b: Option<u32>,
    pub title: String,
    pub detail: Option<String>,
    /// How much this matters: roughly the share of plan cost involved, with changed data access ranked above other
    /// structural changes. Changes are sorted by it.
    pub weight: f64,
}

#[derive(Serialize, Deserialize, TS, Clone, Copy, Debug, PartialEq, Eq)]
#[ts(export)]
pub struct NodeMatch {
    pub a: u32,
    pub b: u32,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug)]
#[ts(export)]
pub struct PlanDiff {
    /// One to three plain-language sentences, most important first.
    pub headlines: Vec<String>,
    pub cost_a: f64,
    pub cost_b: f64,
    /// `cost_b / cost_a` when both plans come from the same engine and have a cost.
    pub cost_ratio: Option<f64>,
    /// Both root operators' actual time, when both plans were executed.
    pub time_a_ms: Option<f64>,
    pub time_b_ms: Option<f64>,
    /// Costs are only comparable within one engine.
    pub comparable: bool,
    /// Every operator matched with the same operator, object and index.
    pub same_shape: bool,
    pub changes: Vec<PlanChange>,
    /// Operators that correspond across the two plans.
    pub matches: Vec<NodeMatch>,
    pub only_a: Vec<u32>,
    pub only_b: Vec<u32>,
    pub warnings_added: Vec<PlanWarning>,
    pub warnings_removed: Vec<PlanWarning>,
}

/// Compare plan `b` against plan `a` (the baseline), naming them "plan A" and "plan B" in the headlines.
pub fn plan_diff(a: &Plan, b: &Plan) -> PlanDiff {
    plan_diff_named(a, "plan A", b, "plan B")
}

/// Compare plan `b` against plan `a` (the baseline); headlines refer to them by the given names ("Plan 815", "staging").
pub fn plan_diff_named(a: &Plan, name_a: &str, b: &Plan, name_b: &str) -> PlanDiff {
    let fa = Flat::new(a);
    let fb = Flat::new(b);
    let mut m = Matching { a: vec![None; fa.nodes.len()], b: vec![None; fb.nodes.len()] };
    match_anchors(&fa, &fb, &mut m);
    match_bottom_up(&fa, &fb, &mut m, true);
    match_bottom_up(&fa, &fb, &mut m, false);
    match_top_down(&fa, &fb, &mut m);

    let comparable = a.engine == b.engine;
    let mut changes = Vec::new();
    let mut same_shape = true;
    for (i, j) in m.a.iter().enumerate().filter_map(|(i, j)| j.map(|j| (i, j))) {
        let (na, nb) = (fa.nodes[i], fb.nodes[j]);
        let (sa, sb) = (fa.share(na), fb.share(nb));
        let reshaped = na.op != nb.op || !same_name(na.index.as_deref(), nb.index.as_deref());
        if reshaped {
            same_shape = false;
            changes.push(reshape_change(na, nb, sa, sb, comparable));
        } else if comparable && (sb - sa).abs() >= SHARE_SHIFT {
            changes.push(PlanChange {
                kind: ChangeKind::Cost,
                a: Some(na.id),
                b: Some(nb.id),
                title: format!("{}: {} → {} of the plan's cost", label(nb), format::percent(sa), format::percent(sb)),
                detail: None,
                weight: (sb - sa).abs(),
            });
        }
        if let (Some(ta), Some(tb)) = (na.act_time_ms, nb.act_time_ms)
            && ta.max(tb) >= TIME_MIN_MS
            && ratio(ta, tb) >= TIME_FACTOR
            && !reshaped
        {
            changes.push(PlanChange {
                kind: ChangeKind::Cost,
                a: Some(na.id),
                b: Some(nb.id),
                title: format!("{}: {} → {}", label(nb), format::duration(ta), format::duration(tb)),
                detail: Some("Actual time, across all executions".into()),
                weight: (fa.time_share(na) - fb.time_share(nb)).abs().max(0.05),
            });
        }
        if !reshaped || na.object.is_some() {
            rows_change(&mut changes, na, nb, sa.max(sb));
        }
        for w in nb.warnings.iter().filter(|w| !na.warnings.contains(w)) {
            changes.push(warning_change(*w, Some(na.id), Some(nb.id), nb, true, sb));
        }
        for w in na.warnings.iter().filter(|w| !nb.warnings.contains(w)) {
            changes.push(warning_change(*w, Some(na.id), Some(nb.id), nb, false, sa));
        }
    }

    let only_a: Vec<usize> = (0..fa.nodes.len()).filter(|i| m.a[*i].is_none()).collect();
    let only_b: Vec<usize> = (0..fb.nodes.len()).filter(|j| m.b[*j].is_none()).collect();
    same_shape &= only_a.is_empty() && only_b.is_empty();
    // A whole new branch is one change: report its top operator and count the rest.
    for &i in only_a.iter().filter(|&&i| fa.parent[i].is_none_or(|p| m.a[p].is_some())) {
        changes.push(presence_change(ChangeKind::Removed, &fa, i, &m.a));
    }
    for &j in only_b.iter().filter(|&&j| fb.parent[j].is_none_or(|p| m.b[p].is_some())) {
        changes.push(presence_change(ChangeKind::Added, &fb, j, &m.b));
    }

    let warnings_a = all_warnings(a);
    let warnings_b = all_warnings(b);
    let warnings_added: Vec<PlanWarning> = warnings_b.iter().filter(|w| !warnings_a.contains(w)).copied().collect();
    let warnings_removed: Vec<PlanWarning> = warnings_a.iter().filter(|w| !warnings_b.contains(w)).copied().collect();
    for w in b.warnings.iter().filter(|w| !a.warnings.contains(w)) {
        changes.push(plan_warning_change(*w, true));
    }
    for w in a.warnings.iter().filter(|w| !b.warnings.contains(w)) {
        changes.push(plan_warning_change(*w, false));
    }
    changes.sort_by(|x, y| y.weight.total_cmp(&x.weight).then(rank(x.kind).cmp(&rank(y.kind))));

    let cost_ratio = (comparable && a.total_cost > 0.0 && b.total_cost > 0.0).then(|| b.total_cost / a.total_cost);
    let (time_a_ms, time_b_ms) = (a.root.act_time_ms, b.root.act_time_ms);
    let mut diff = PlanDiff {
        headlines: Vec::new(),
        cost_a: a.total_cost,
        cost_b: b.total_cost,
        cost_ratio,
        time_a_ms,
        time_b_ms,
        comparable,
        same_shape,
        changes,
        matches: m
            .a
            .iter()
            .enumerate()
            .filter_map(|(i, j)| j.map(|j| NodeMatch { a: fa.nodes[i].id, b: fb.nodes[j].id }))
            .collect(),
        only_a: only_a.iter().map(|&i| fa.nodes[i].id).collect(),
        only_b: only_b.iter().map(|&j| fb.nodes[j].id).collect(),
        warnings_added,
        warnings_removed,
    };
    diff.headlines = headlines(&diff, a, b, &fa, &fb, name_a, name_b);
    diff
}

/// What operator shares are measured against. Usually the plan's cost, but under a Postgres `Limit` the children's
/// costs assume they run to completion, so their self costs add up to more than the root's early-stop cost.
pub fn plan_cost_basis(plan: &Plan) -> f64 {
    let own: f64 = plan.nodes().iter().map(|n| n.self_cost).sum();
    [plan.total_cost, plan.root.subtree_cost, own].into_iter().fold(0.0, f64::max).max(f64::MIN_POSITIVE)
}

/// The plan as a pre-order list, so a node's descendants are the `size - 1` entries after it.
struct Flat<'a> {
    nodes: Vec<&'a PlanNode>,
    parent: Vec<Option<usize>>,
    children: Vec<Vec<usize>>,
    size: Vec<usize>,
    total: f64,
    total_time: f64,
}

impl<'a> Flat<'a> {
    fn new(plan: &'a Plan) -> Flat<'a> {
        let mut flat = Flat {
            nodes: Vec::new(),
            parent: Vec::new(),
            children: Vec::new(),
            size: Vec::new(),
            total: plan_cost_basis(plan),
            total_time: plan.root.act_time_ms.filter(|t| *t > 0.0).unwrap_or(1.0),
        };
        flat.push(&plan.root, None);
        flat
    }

    fn push(&mut self, node: &'a PlanNode, parent: Option<usize>) -> usize {
        let index = self.nodes.len();
        self.nodes.push(node);
        self.parent.push(parent);
        self.children.push(Vec::new());
        self.size.push(1);
        for child in &node.children {
            let c = self.push(child, Some(index));
            self.children[index].push(c);
            self.size[index] += self.size[c];
        }
        index
    }

    fn share(&self, node: &PlanNode) -> f64 {
        node.self_cost / self.total
    }

    /// The operator and the unmatched operators under it: a branch only one plan has.
    fn branch(&self, index: usize, matched: &[Option<usize>]) -> Vec<usize> {
        (index..index + self.size[index]).filter(|d| matched[*d].is_none()).collect()
    }

    fn time_share(&self, node: &PlanNode) -> f64 {
        node.act_time_ms.unwrap_or(0.0) / self.total_time
    }

    fn position(&self, index: usize) -> f64 {
        index as f64 / self.nodes.len().max(1) as f64
    }
}

struct Matching {
    a: Vec<Option<usize>>,
    b: Vec<Option<usize>>,
}

impl Matching {
    fn pair(&mut self, i: usize, j: usize) {
        self.a[i] = Some(j);
        self.b[j] = Some(i);
    }
}

/// Data access is the backbone of a plan: pair the operators that read the same table first.
fn match_anchors(fa: &Flat, fb: &Flat, m: &mut Matching) {
    let mut candidates = Vec::new();
    for (i, na) in fa.nodes.iter().enumerate() {
        let Some(oa) = na.object.as_deref() else { continue };
        for (j, nb) in fb.nodes.iter().enumerate() {
            let Some(ob) = nb.object.as_deref() else { continue };
            if !same_object(oa, ob) {
                continue;
            }
            let mut score = 0.0;
            if same_name(na.index.as_deref(), nb.index.as_deref()) {
                score += 4.0;
            }
            if na.op == nb.op {
                score += 2.0;
            }
            if na.kind == nb.kind {
                score += 1.0;
            }
            score -= (fa.position(i) - fb.position(j)).abs();
            candidates.push((score, i, j));
        }
    }
    candidates.sort_by(|x, y| y.0.total_cmp(&x.0));
    for (_, i, j) in candidates {
        if m.a[i].is_none() && m.b[j].is_none() {
            m.pair(i, j);
        }
    }
}

/// An operator above matched operators corresponds to the operator above their counterparts (joins, sorts, aggregates).
/// The first pass only pairs identical operators, so a Sort can't take the Limit that belongs to the other Limit.
fn match_bottom_up(fa: &Flat, fb: &Flat, m: &mut Matching, same_op: bool) {
    for i in (0..fa.nodes.len()).rev() {
        if m.a[i].is_some() || fa.size[i] == 1 {
            continue;
        }
        let mut common: HashMap<usize, usize> = HashMap::new();
        for d in i + 1..i + fa.size[i] {
            let Some(e) = m.a[d] else { continue };
            let mut x = fb.parent[e];
            while let Some(up) = x {
                if m.b[up].is_none() {
                    *common.entry(up).or_default() += 1;
                }
                x = fb.parent[up];
            }
        }
        let na = fa.nodes[i];
        let best = common
            .into_iter()
            .filter(|(x, _)| !same_op || fb.nodes[*x].op == na.op)
            .map(|(x, shared)| {
                let dice = 2.0 * shared as f64 / ((fa.size[i] - 1) + (fb.size[x] - 1)).max(1) as f64;
                let nb = fb.nodes[x];
                let likeness =
                    f64::from(u8::from(na.op == nb.op)) * 0.2 + f64::from(u8::from(na.kind == nb.kind)) * 0.1;
                (dice + likeness, dice, shared, x)
            })
            .max_by(|x, y| x.0.total_cmp(&y.0).then(y.3.cmp(&x.3)));
        if let Some((_, dice, shared, x)) = best
            && (dice >= 0.4 || (shared >= 1 && na.op == fb.nodes[x].op))
        {
            m.pair(i, x);
        }
    }
}

/// What's left pairs up under matched parents: same operator first, then same kind, then a lone child on each side.
fn match_top_down(fa: &Flat, fb: &Flat, m: &mut Matching) {
    if m.a[0].is_none() && m.b[0].is_none() {
        m.pair(0, 0);
    }
    for i in 0..fa.nodes.len() {
        let Some(j) = m.a[i] else { continue };
        let open_a = |m: &Matching| fa.children[i].iter().copied().filter(|c| m.a[*c].is_none()).collect::<Vec<_>>();
        let open_b = |m: &Matching| fb.children[j].iter().copied().filter(|c| m.b[*c].is_none()).collect::<Vec<_>>();
        let rules: [PairRule; 2] = [same_step, same_kind];
        for rule in rules {
            for c in open_a(m) {
                if let Some(d) = open_b(m).into_iter().find(|d| rule(fa.nodes[c], fb.nodes[*d])) {
                    m.pair(c, d);
                }
            }
        }
        if let ([c], [d]) = (open_a(m).as_slice(), open_b(m).as_slice()) {
            m.pair(*c, *d);
        }
    }
}

type PairRule = fn(&PlanNode, &PlanNode) -> bool;

fn same_step(x: &PlanNode, y: &PlanNode) -> bool {
    x.op == y.op && same_name(x.object.as_deref(), y.object.as_deref())
}

fn same_kind(x: &PlanNode, y: &PlanNode) -> bool {
    x.kind == y.kind && x.kind != NodeKind::Other
}

fn reshape_change(na: &PlanNode, nb: &PlanNode, sa: f64, sb: f64, comparable: bool) -> PlanChange {
    let access = na.object.is_some() && nb.object.is_some();
    let title =
        if access { format!("{}: {} → {}", table(nb), how(na), how(nb)) } else { format!("{} → {}", na.op, nb.op) };
    let detail =
        comparable.then(|| format!("{} of the plan's cost in A, {} in B", format::percent(sa), format::percent(sb)));
    PlanChange {
        kind: if access { ChangeKind::Access } else { ChangeKind::Operator },
        a: Some(na.id),
        b: Some(nb.id),
        title,
        detail,
        weight: sa.max(sb) + if access { 0.35 } else { 0.2 },
    }
}

fn rows_change(changes: &mut Vec<PlanChange>, na: &PlanNode, nb: &PlanNode, share: f64) {
    let (ea, eb) = (na.est_rows_total(), nb.est_rows_total());
    if ea.max(eb) >= ROWS_MIN && ratio(ea, eb) >= ROWS_FACTOR {
        changes.push(PlanChange {
            kind: ChangeKind::Rows,
            a: Some(na.id),
            b: Some(nb.id),
            title: format!("{}: estimated rows {} → {}", label(nb), format::count(ea), format::count(eb)),
            detail: None,
            weight: share * 0.5 + 0.01,
        });
    }
    if let (Some(ra), Some(rb)) = (na.act_rows, nb.act_rows)
        && ra.max(rb) >= ROWS_MIN
        && ratio(ra, rb) >= ROWS_FACTOR
    {
        changes.push(PlanChange {
            kind: ChangeKind::Rows,
            a: Some(na.id),
            b: Some(nb.id),
            title: format!("{}: actual rows {} → {}", label(nb), format::count(ra), format::count(rb)),
            detail: None,
            weight: share * 0.5 + 0.01,
        });
    }
}

fn warning_change(
    w: PlanWarning,
    a: Option<u32>,
    b: Option<u32>,
    node: &PlanNode,
    added: bool,
    share: f64,
) -> PlanChange {
    PlanChange {
        kind: ChangeKind::Warning,
        a,
        b,
        title: if added {
            format!("{} on {}", w.label(), label(node))
        } else {
            format!("No longer: {} on {}", w.label().to_lowercase(), label(node))
        },
        detail: None,
        weight: share * 0.5 + if added { 0.03 } else { 0.015 },
    }
}

fn plan_warning_change(w: PlanWarning, added: bool) -> PlanChange {
    PlanChange {
        kind: ChangeKind::Warning,
        a: None,
        b: None,
        title: if added {
            format!("Plan warning: {}", w.label())
        } else {
            format!("No longer: {}", w.label().to_lowercase())
        },
        detail: None,
        weight: if added { 0.03 } else { 0.015 },
    }
}

fn presence_change(kind: ChangeKind, flat: &Flat, index: usize, matched: &[Option<usize>]) -> PlanChange {
    let node = flat.nodes[index];
    let branch = flat.branch(index, matched);
    let share = branch.iter().map(|d| flat.nodes[*d].self_cost).sum::<f64>() / flat.total;
    let (a, b, side) = match kind {
        ChangeKind::Removed => (Some(node.id), None, "Only in A"),
        _ => (None, Some(node.id), "Only in B"),
    };
    let names: Vec<String> = branch.iter().map(|d| label(flat.nodes[*d])).collect();
    let title = if names.len() <= 3 {
        format!("{side}: {}", format::list(&names))
    } else {
        format!("{side}: {} and {} operators below it", names[0], names.len() - 1)
    };
    PlanChange {
        kind,
        a,
        b,
        title,
        detail: Some(format!("{} of that plan's cost", format::percent(share))),
        weight: share + 0.05,
    }
}

fn rank(kind: ChangeKind) -> u8 {
    match kind {
        ChangeKind::Access => 0,
        ChangeKind::Operator => 1,
        ChangeKind::Added => 2,
        ChangeKind::Removed => 3,
        ChangeKind::Cost => 4,
        ChangeKind::Warning => 5,
        ChangeKind::Rows => 6,
    }
}

fn headlines(diff: &PlanDiff, a: &Plan, b: &Plan, fa: &Flat, fb: &Flat, name_a: &str, name_b: &str) -> Vec<String> {
    let mut out = Vec::new();
    if !diff.comparable {
        out.push(format!(
            "These plans come from different engines ({} and {}), so their costs aren't comparable.",
            a.engine.label(),
            b.engine.label()
        ));
    }
    if let Some(sentence) = magnitude(diff, name_a, name_b) {
        out.push(sentence);
    }
    let node_a = |id: Option<u32>| id.and_then(|id| fa.nodes.iter().find(|n| n.id == id).copied());
    let node_b = |id: Option<u32>| id.and_then(|id| fb.nodes.iter().find(|n| n.id == id).copied());
    let mut explained: Vec<String> = Vec::new();
    for change in diff.changes.iter().filter(|c| c.weight >= HEADLINE_WEIGHT) {
        if out.len() >= MAX_HEADLINES {
            break;
        }
        let sentence = match (change.kind, node_a(change.a), node_b(change.b)) {
            (ChangeKind::Access, Some(x), Some(y)) => {
                explained.push(normalize(&table(x)));
                format!("{name_b} {} where {name_a} {}.", verb(y, &fb.nodes), verb(x, &fa.nodes))
            }
            (ChangeKind::Added | ChangeKind::Removed, x, y)
                if {
                    let (flat, node) = if change.kind == ChangeKind::Added { (fb, y) } else { (fa, x) };
                    let tables = node
                        .map(|n| {
                            branch_tables(
                                flat,
                                n.id,
                                if change.kind == ChangeKind::Added { &diff.only_b } else { &diff.only_a },
                            )
                        })
                        .unwrap_or_default();
                    !tables.is_empty() && tables.iter().all(|t| explained.contains(t))
                } =>
            {
                continue;
            }
            (ChangeKind::Operator, Some(x), Some(y)) if x.kind == NodeKind::Join || y.kind == NodeKind::Join => {
                format!("{name_b} joins with {} where {name_a} uses {}.", y.op, x.op)
            }
            (ChangeKind::Operator, Some(x), Some(y)) => format!("{name_b} uses {} where {name_a} uses {}.", y.op, x.op),
            (ChangeKind::Added, _, Some(y)) => {
                let what = change.title.trim_start_matches("Only in B: ");
                let share = change.detail.as_deref().and_then(|d| d.split(' ').next()).unwrap_or("");
                format!("{name_b} adds {} {what} ({share} of its cost).", article(&y.op))
            }
            (ChangeKind::Removed, Some(_), _) => {
                format!("{name_b} has no {} ({name_a} has).", change.title.trim_start_matches("Only in A: "))
            }
            _ => continue,
        };
        out.push(sentence);
    }
    if out.len() < MAX_HEADLINES && !diff.warnings_added.is_empty() {
        let names: Vec<String> = diff.warnings_added.iter().map(|w| w.label().to_lowercase()).collect();
        let noun = if names.len() == 1 { "a plan warning" } else { "plan warnings" };
        out.push(format!("{name_b} has {noun} {name_a} doesn't: {}.", format::list(&names)));
    }
    if out.is_empty() || (out.len() == 1 && !diff.comparable) {
        out.push(if diff.same_shape {
            format!("{name_b} runs the same steps as {name_a}.")
        } else {
            format!("{name_b} differs from {name_a} only in minor steps.")
        });
    }
    out.into_iter().map(capitalize).collect()
}

fn magnitude(diff: &PlanDiff, name_a: &str, name_b: &str) -> Option<String> {
    if let (Some(ta), Some(tb)) = (diff.time_a_ms, diff.time_b_ms)
        && ta > 0.0
        && tb > 0.0
        && ratio(ta, tb) >= COST_RATIO
    {
        let (word, r) = if tb > ta { ("longer", tb / ta) } else { ("less time", ta / tb) };
        return Some(format!(
            "{name_b} took {} {word} than {name_a} ({} → {}).",
            times(r),
            format::duration(ta),
            format::duration(tb)
        ));
    }
    let r = diff.cost_ratio?;
    if ratio(1.0, r) < COST_RATIO {
        return None;
    }
    let (word, r) = if r > 1.0 { ("more expensive", r) } else { ("cheaper", 1.0 / r) };
    Some(format!(
        "{name_b} is estimated {} {word} than {name_a} (cost {} → {}).",
        times(r),
        cost(diff.cost_a),
        cost(diff.cost_b)
    ))
}

/// How an operator reads its table, as a verb phrase: "seeks ix_orders_customer", "scans orders". A lookup names the
/// seek on the same table that feeds it.
fn verb(node: &PlanNode, plan: &[&PlanNode]) -> String {
    let table = table(node);
    match node.kind {
        NodeKind::Seek => match node.index.as_deref() {
            Some(index) => format!("seeks {index}"),
            None => format!("seeks into {table}"),
        },
        NodeKind::Scan => format!("scans {table}"),
        NodeKind::Lookup => {
            let feeder = plan.iter().find(|n| {
                n.kind == NodeKind::Seek && n.index.is_some() && same_name(n.object.as_deref(), node.object.as_deref())
            });
            match feeder.and_then(|n| n.index.as_deref()) {
                Some(index) => format!("seeks {index} and looks up the rows in {table}"),
                None => format!("looks up rows in {table}"),
            }
        }
        _ => format!("uses {} on {table}", node.op),
    }
}

/// Tables read inside a branch only one plan has (the operator with `id` and the unmatched operators under it).
fn branch_tables(flat: &Flat, id: u32, only: &[u32]) -> Vec<String> {
    let Some(start) = flat.nodes.iter().position(|n| n.id == id) else { return Vec::new() };
    (start..start + flat.size[start])
        .map(|d| flat.nodes[d])
        .filter(|n| only.contains(&n.id) && n.object.is_some())
        .map(|n| normalize(&table(n)))
        .collect()
}

/// "Index Seek (ix_customers_email)" for the change list, where the table is already named.
fn how(node: &PlanNode) -> String {
    match node.index.as_deref() {
        Some(index) => format!("{} ({index})", node.op),
        None => node.op.clone(),
    }
}

/// "Key Lookup on customers", "Sort".
fn label(node: &PlanNode) -> String {
    match node.object.as_deref() {
        Some(_) => format!("{} on {}", node.op, table(node)),
        None => node.op.clone(),
    }
}

fn table(node: &PlanNode) -> String {
    let object = node.object.as_deref().unwrap_or("?");
    unquote(object.rsplit('.').next().unwrap_or(object))
}

fn all_warnings(plan: &Plan) -> Vec<PlanWarning> {
    let mut out = Vec::new();
    for w in plan.warnings.iter().chain(plan.nodes().iter().flat_map(|n| n.warnings.iter())) {
        if !out.contains(w) {
            out.push(*w);
        }
    }
    out
}

fn same_object(a: &str, b: &str) -> bool {
    let (a, b) = (normalize(a), normalize(b));
    if a == b {
        return true;
    }
    // One side may name the schema and the other not (an imported plan, another tool's output).
    let (sa, sb) = (a.split('.').collect::<Vec<_>>(), b.split('.').collect::<Vec<_>>());
    (sa.len() == 1 || sb.len() == 1) && sa.last() == sb.last()
}

fn same_name(a: Option<&str>, b: Option<&str>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => normalize(a) == normalize(b),
        (None, None) => true,
        _ => false,
    }
}

fn normalize(name: &str) -> String {
    name.chars().filter(|c| !matches!(c, '[' | ']' | '"' | '`')).collect::<String>().to_lowercase()
}

fn unquote(name: &str) -> String {
    name.chars().filter(|c| !matches!(c, '[' | ']' | '"' | '`')).collect()
}

fn ratio(a: f64, b: f64) -> f64 {
    let (a, b) = (a.max(1e-9), b.max(1e-9));
    if a > b { a / b } else { b / a }
}

fn times(r: f64) -> String {
    if r >= 10.0 { format!("{}×", format::count(r.round())) } else { format!("{r:.1}×") }
}

fn cost(value: f64) -> String {
    if value < 10.0 { format!("{value:.3}") } else { format::count(value) }
}

fn article(word: &str) -> &'static str {
    if word.chars().next().is_some_and(|c| "AEIOUaeiou".contains(c)) { "an" } else { "a" }
}

/// Sentences open with a plan's name; only Culprit's own lowercase names ("plan A", "query 7") get a capital.
fn capitalize(sentence: String) -> String {
    if !["plan ", "query ", "these "].iter().any(|w| sentence.starts_with(w)) {
        return sentence;
    }
    let mut chars = sentence.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => sentence,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Engine, Properties};
    use crate::plan::PlanKind;

    fn node(
        op: &str,
        kind: NodeKind,
        object: Option<&str>,
        index: Option<&str>,
        cost: f64,
        children: Vec<PlanNode>,
    ) -> PlanNode {
        PlanNode {
            id: 0,
            op: op.into(),
            kind,
            detail: None,
            object: object.map(str::to_string),
            index: index.map(str::to_string),
            seek_predicate: None,
            predicate: None,
            est_rows: 10.0,
            est_executions: 1.0,
            act_rows: None,
            act_executions: None,
            rows_read: None,
            table_rows: None,
            subtree_cost: cost + children.iter().map(|c| c.subtree_cost).sum::<f64>(),
            self_cost: 0.0,
            act_time_ms: None,
            warnings: Vec::new(),
            props: Properties::new(),
            children,
        }
    }

    fn plan(engine: Engine, root: PlanNode) -> Plan {
        let mut plan = Plan {
            engine,
            kind: PlanKind::Estimated,
            statement: None,
            total_cost: root.subtree_cost,
            root,
            warnings: Vec::new(),
            index_hints: Vec::new(),
            props: Properties::new(),
            raw: None,
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

    fn seek_plan() -> Plan {
        plan(
            Engine::Mssql,
            node(
                "Nested Loops",
                NodeKind::Join,
                None,
                None,
                0.1,
                vec![
                    node("Index Seek", NodeKind::Seek, Some("shop.orders"), Some("ix_orders_customer"), 0.2, vec![]),
                    node("Key Lookup", NodeKind::Lookup, Some("shop.orders"), Some("pk_orders"), 0.3, vec![]),
                ],
            ),
        )
    }

    #[test]
    fn identical_plans_have_no_changes() {
        let diff = plan_diff(&seek_plan(), &seek_plan());
        assert!(diff.same_shape && diff.changes.is_empty());
        assert_eq!(diff.matches.len(), 3);
        assert_eq!(diff.headlines, ["Plan B runs the same steps as plan A."]);
    }

    #[test]
    fn a_scan_instead_of_a_seek_leads_the_headlines() {
        let scan = plan(
            Engine::Mssql,
            node("Clustered Index Scan", NodeKind::Scan, Some("shop.orders"), Some("pk_orders"), 4.0, vec![]),
        );
        let diff = plan_diff_named(&seek_plan(), "Plan 815", &scan, "Plan 685");
        assert_eq!(diff.headlines[0], "Plan 685 is estimated 6.7× more expensive than Plan 815 (cost 0.600 → 4.000).");
        assert_eq!(
            diff.headlines[1],
            "Plan 685 scans orders where Plan 815 seeks ix_orders_customer and looks up the rows in orders."
        );
        assert_eq!(diff.changes[0].kind, ChangeKind::Access);
        assert_eq!(diff.changes[0].title, "orders: Key Lookup (pk_orders) → Clustered Index Scan (pk_orders)");
        assert!(diff.changes.iter().any(|c| c.kind == ChangeKind::Removed && c.title == "Only in A: Nested Loops and Index Seek on orders"));
    }

    #[test]
    fn a_new_branch_is_one_change() {
        let sorted = plan(
            Engine::Mssql,
            node(
                "Sort",
                NodeKind::Sort,
                None,
                None,
                2.0,
                vec![node("Compute Scalar", NodeKind::Compute, None, None, 0.5, vec![seek_plan().root])],
            ),
        );
        let diff = plan_diff(&seek_plan(), &sorted);
        let added: Vec<_> = diff.changes.iter().filter(|c| c.kind == ChangeKind::Added).collect();
        assert_eq!(added.len(), 1, "{:#?}", diff.changes);
        assert_eq!(added[0].title, "Only in B: Sort and Compute Scalar");
        assert!(
            diff.headlines.iter().any(|h| h.starts_with("Plan B adds a Sort and Compute Scalar (")),
            "{:?}",
            diff.headlines
        );
        assert_eq!(diff.only_b.len(), 2);
    }

    #[test]
    fn engines_are_not_compared_by_cost() {
        let pg = plan(Engine::Postgres, node("Seq Scan", NodeKind::Scan, Some("public.orders"), None, 50.0, vec![]));
        let diff = plan_diff(&seek_plan(), &pg);
        assert!(!diff.comparable && diff.cost_ratio.is_none());
        assert!(diff.headlines[0].starts_with("These plans come from different engines (SQL Server and Postgres)"));
    }

    #[test]
    fn shares_under_a_limit_use_the_operators_cost() {
        let mut limited = plan(
            Engine::Postgres,
            node(
                "Limit",
                NodeKind::Other,
                None,
                None,
                0.0,
                vec![node("Seq Scan", NodeKind::Scan, Some("orders"), None, 1000.0, vec![])],
            ),
        );
        limited.root.subtree_cost = 5.0;
        limited.total_cost = 5.0;
        limited.compute_self_costs();
        assert_eq!(plan_cost_basis(&limited), 1000.0);
    }
}
