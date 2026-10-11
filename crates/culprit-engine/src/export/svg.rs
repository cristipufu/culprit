//! The plan diagram as static SVG, for HTML exports: the same tidy top-down tree as the app (leaves on consecutive
//! slots, parents centred over their children), cards with a heat stripe for their share of the cost, edges as thick
//! as the rows that flow through them, and the path to the most expensive step highlighted. Colors come from CSS
//! variables on the page, so the diagram follows light and dark mode.

use std::collections::{HashMap, HashSet};
use std::fmt::Write;

use culprit_core::{NodeKind, Plan, PlanNode, format};

use super::{rows_out, share, table};

const W: f64 = 200.0;
const H: f64 = 82.0;
const GAP_X: f64 = 20.0;
const GAP_Y: f64 = 52.0;
const PAD: f64 = 22.0;
/// Approximate glyph widths (px) for truncating text to the card.
const SANS: f64 = 6.9;
const MONO: f64 = 6.3;

/// How an operator relates to the other plan in a comparison.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mark {
    /// Part of a region of identical steps: tinted with the region's color on both sides.
    Region(usize),
    /// Matched, but a different operator or index.
    Changed,
    /// Only this plan has it.
    Only,
}

pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c if (c as u32) < 0x20 && c != '\n' && c != '\t' => out.push(' '),
            c => out.push(c),
        }
    }
    out
}

fn fit(text: &str, width: f64, glyph: f64) -> String {
    let max = (width / glyph).floor() as usize;
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max {
        text.to_string()
    } else {
        chars[..max.saturating_sub(1)].iter().collect::<String>() + "…"
    }
}

/// "1 row", "1.2K rows".
fn rows_word(n: f64) -> String {
    format!("{} row{}", format::count(n), if n.round() == 1.0 { "" } else { "s" })
}

pub fn heat_class(share: f64) -> &'static str {
    if share >= 0.3 {
        "h4"
    } else if share >= 0.15 {
        "h3"
    } else if share >= 0.05 {
        "h2"
    } else if share >= 0.01 {
        "h1"
    } else {
        "h0"
    }
}

pub fn kind_class(kind: NodeKind) -> (&'static str, &'static str) {
    match kind {
        NodeKind::Scan => ("k-scan", "Scan"),
        NodeKind::Seek => ("k-seek", "Seek"),
        NodeKind::Lookup => ("k-lookup", "Lookup"),
        NodeKind::Join => ("k-join", "Join"),
        NodeKind::Sort => ("k-sort", "Sort"),
        NodeKind::Aggregate => ("k-agg", "Aggregate"),
        NodeKind::Spool => ("k-dim", "Spool"),
        NodeKind::Parallelism => ("k-dim", "Parallel"),
        NodeKind::Modify => ("k-write", "Write"),
        NodeKind::Compute => ("k-dim", "Compute"),
        NodeKind::Other => ("k-dim", ""),
    }
}

struct Placed<'a> {
    node: &'a PlanNode,
    x: f64,
    depth: usize,
}

fn place<'a>(node: &'a PlanNode, depth: usize, slot: &mut f64, out: &mut Vec<Placed<'a>>) -> f64 {
    let x = if node.children.is_empty() {
        let x = *slot;
        *slot += 1.0;
        x
    } else {
        let xs: Vec<f64> = node.children.iter().map(|c| place(c, depth + 1, slot, out)).collect();
        (xs.iter().copied().fold(f64::INFINITY, f64::min) + xs.iter().copied().fold(f64::NEG_INFINITY, f64::max)) / 2.0
    };
    out.push(Placed { node, x, depth });
    x
}

fn path_to(node: &PlanNode, id: u32, out: &mut Vec<u32>) -> bool {
    out.push(node.id);
    if node.id == id || node.children.iter().any(|c| path_to(c, id, out)) {
        return true;
    }
    out.pop();
    false
}

/// The diagram for `plan`. `marks` tints operators in a comparison; `focus` outlines one operator (a finding's step).
/// `id` keeps marker ids unique when a page holds several diagrams.
pub fn diagram(plan: &Plan, marks: &HashMap<u32, Mark>, focus: Option<u32>, label: &str, id: &str) -> String {
    let mut placed = Vec::new();
    let mut slot = 0.0;
    place(&plan.root, 0, &mut slot, &mut placed);
    let max_x = placed.iter().map(|p| p.x).fold(0.0, f64::max);
    let max_depth = placed.iter().map(|p| p.depth).max().unwrap_or(0) as f64;
    let width = PAD * 2.0 + (max_x + 1.0) * W + max_x * GAP_X;
    let height = PAD * 2.0 + 18.0 + (max_depth + 1.0) * H + max_depth * GAP_Y;
    let px = |x: f64| PAD + x * (W + GAP_X);
    let py = |d: usize| PAD + 18.0 + d as f64 * (H + GAP_Y);
    let by_id: HashMap<u32, &Placed> = placed.iter().map(|p| (p.node.id, p)).collect();
    let hottest = plan.hottest();
    let mut hot_path = Vec::new();
    path_to(&plan.root, hottest.id, &mut hot_path);
    let hot: HashSet<u32> = if share(plan, hottest) >= 0.05 { hot_path.into_iter().collect() } else { HashSet::new() };
    let max_rows = placed.iter().map(|p| rows_out(p.node)).fold(1.0, f64::max);
    let stroke = |rows: f64| 1.5 + 5.0 * ((rows + 1.0).log10() / (max_rows + 1.0).log10()).clamp(0.0, 1.0);

    let mut svg = String::new();
    let _ = write!(
        svg,
        r#"<svg class="diagram" viewBox="0 0 {width:.0} {height:.0}" width="{width:.0}" height="{height:.0}" role="img" aria-label="{}">"#,
        escape(label)
    );
    // Arrowheads point into the parent: rows flow up from the operator that produces them.
    let _ = write!(
        svg,
        r#"<defs><marker id="arrow-{id}" viewBox="0 0 10 10" refX="8" refY="5" markerUnits="userSpaceOnUse" markerWidth="9" markerHeight="9" orient="auto-start-reverse"><path d="M 0 1 L 9 5 L 0 9 z" class="arrowhead"/></marker></defs>"#
    );
    let root = by_id[&plan.root.id];
    let _ = write!(svg, r#"<text class="result" x="{:.1}" y="{:.1}">RESULT</text>"#, px(root.x) + W / 2.0, PAD + 6.0);
    // Edges first, under the cards.
    for p in &placed {
        for c in &p.node.children {
            let Some(child) = by_id.get(&c.id) else { continue };
            let (x1, y1) = (px(child.x) + W / 2.0, py(child.depth));
            let (x2, y2) = (px(p.x) + W / 2.0, py(p.depth) + H);
            let mid = (y1 + y2) / 2.0;
            let rows = rows_out(c);
            let class = if hot.contains(&c.id) && hot.contains(&p.node.id) { "edge hot" } else { "edge" };
            let _ = write!(
                svg,
                r#"<path class="{class}" d="M {x1:.1} {y1:.1} C {x1:.1} {mid:.1}, {x2:.1} {mid:.1}, {x2:.1} {:.1}" stroke-width="{:.2}" marker-end="url(#arrow-{id})"/>"#,
                y2 + 3.0,
                stroke(rows)
            );
            // Beside the line, on a background halo, so the label never sits on the edge.
            let _ = write!(
                svg,
                r#"<text class="edge-label" x="{:.1}" y="{:.1}">{}</text>"#,
                x1 + stroke(rows) / 2.0 + 7.0,
                y1 - 9.0,
                escape(&rows_word(rows))
            );
        }
    }
    for p in &placed {
        let n = p.node;
        let (x, y) = (px(p.x), py(p.depth));
        let s = share(plan, n);
        let mark = marks.get(&n.id).copied();
        let mut classes = vec!["card".to_string()];
        match mark {
            Some(Mark::Region(r)) => classes.push(format!("tint t{}", r % 6)),
            Some(Mark::Changed) => classes.push("changed".into()),
            Some(Mark::Only) => classes.push("only".into()),
            None => {}
        }
        if hot.contains(&n.id) {
            classes.push("hot".into());
        }
        if focus == Some(n.id) {
            classes.push("focus".into());
        }
        let (kind_class, kind_label) = kind_class(n.kind);
        let tip = n
            .predicate
            .as_deref()
            .map(|p| format!("filter {p}"))
            .or_else(|| n.seek_predicate.as_deref().map(|p| format!("seek {p}")))
            .unwrap_or_else(|| n.op.clone());
        let _ = write!(svg, r#"<g class="{}"><title>{}</title>"#, classes.join(" "), escape(&tip));
        let _ = write!(svg, r#"<rect class="box" x="{x:.1}" y="{y:.1}" width="{W}" height="{H}" rx="9"/>"#);
        // The operator's share of the cost as a bar along the card's bottom edge.
        let _ = write!(
            svg,
            r#"<rect class="track" x="{:.1}" y="{:.1}" width="{:.1}" height="3" rx="1.5"/><rect class="bar {}" x="{:.1}" y="{:.1}" width="{:.1}" height="3" rx="1.5"/>"#,
            x + 12.0,
            y + H - 8.0,
            W - 24.0,
            heat_class(s),
            x + 12.0,
            y + H - 8.0,
            ((W - 24.0) * s.clamp(0.0, 1.0)).max(if s > 0.0 { 2.0 } else { 0.0 })
        );
        let kind = if kind_label.is_empty() { n.op.to_uppercase() } else { kind_label.to_uppercase() };
        let _ = write!(
            svg,
            r#"<text class="kind {kind_class}" x="{:.1}" y="{:.1}">{}</text>"#,
            x + 12.0,
            y + 18.0,
            escape(&fit(&kind, W * 0.5, 6.4))
        );
        let badge = match mark {
            Some(Mark::Changed) => "changed · ",
            Some(Mark::Only) => "only here · ",
            _ => "",
        };
        let flame = if n.id == hottest.id && s >= 0.05 { "▲ " } else { "" };
        let _ = write!(
            svg,
            r#"<text class="pct" x="{:.1}" y="{:.1}" text-anchor="end">{}{flame}{}</text>"#,
            x + W - 12.0,
            y + 18.0,
            badge,
            format::percent(s)
        );
        let _ = write!(
            svg,
            r#"<text class="op" x="{:.1}" y="{:.1}">{}</text>"#,
            x + 12.0,
            y + 37.0,
            escape(&fit(&n.op, W - 24.0, SANS * 1.05))
        );
        let object = match (table(n), n.index.as_deref()) {
            (Some(t), Some(i)) => format!("{t} · {i}"),
            (Some(t), None) => t,
            (None, _) => n.detail.clone().unwrap_or_default(),
        };
        let _ = write!(
            svg,
            r#"<text class="obj" x="{:.1}" y="{:.1}">{}</text>"#,
            x + 12.0,
            y + 53.0,
            escape(&fit(&object, W - 24.0, MONO))
        );
        let est = n.est_rows_total();
        let rows = match n.act_rows {
            Some(act) => {
                let off = act.max(1.0) / est.max(1.0);
                let warn = if !(0.1..10.0).contains(&off) {
                    format!("  ×{} off", format::count(if off >= 1.0 { off } else { 1.0 / off }))
                } else {
                    String::new()
                };
                format!("{} → {}{warn}", format::count(est), rows_word(act))
            }
            None => format!("{} est.", rows_word(est)),
        };
        let time = n.act_time_ms.map(|t| format!(" · {}", format::duration(t))).unwrap_or_default();
        let warn = if n.warnings.is_empty() { "" } else { "  ⚠" };
        let _ = write!(
            svg,
            r#"<text class="rows" x="{:.1}" y="{:.1}">{}</text>"#,
            x + 12.0,
            y + 67.0,
            escape(&fit(&format!("{rows}{time}{warn}"), W - 24.0, SANS * 0.9))
        );
        svg.push_str("</g>");
    }
    svg.push_str("</svg>");
    svg
}
