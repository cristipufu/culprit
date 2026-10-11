import { Flame, Maximize2, Minus, Plus, TriangleAlert } from "lucide-react";
import { useEffect, useMemo, useState } from "react";
import type { NodeKind } from "../gen/NodeKind";
import type { Plan } from "../gen/Plan";
import type { PlanNode } from "../gen/PlanNode";
import { count, duration, percent } from "../lib/format";
import { NodeProps, shortPredicate } from "./PlanView";

const CARD_W = 214;
const CARD_H = 92;
const GAP_X = 26;
const GAP_Y = 64;
const PAD = 28;
const CHEAP_SHARE = 0.005;

const KIND: Record<NodeKind, { label: string; color: string }> = {
  scan: { label: "Scan", color: "var(--medium)" },
  seek: { label: "Seek", color: "var(--good)" },
  lookup: { label: "Lookup", color: "#d9782d" },
  join: { label: "Join", color: "var(--focus)" },
  sort: { label: "Sort", color: "#8f3fbf" },
  aggregate: { label: "Aggregate", color: "#1f9aa3" },
  spool: { label: "Spool", color: "var(--text-3)" },
  parallelism: { label: "Parallel", color: "var(--text-3)" },
  modify: { label: "Write", color: "var(--high)" },
  compute: { label: "Compute", color: "var(--text-3)" },
  other: { label: "", color: "var(--text-3)" },
};

/** How a node differs from the other plan in a comparison. */
export type Mark = "changed" | "added" | "removed";

const MARK_LABEL: Record<Mark, string> = { changed: "changed", added: "only here", removed: "only here" };

/** What operator shares are measured against: the plan's cost, or the sum of operator costs when that is larger
 * (under a Postgres Limit the children's costs assume they run to completion). */
export function costBasis(plan: Plan): number {
  const own = allNodes(plan.root).reduce((s, n) => s + n.self_cost, 0);
  return Math.max(plan.total_cost, plan.root.subtree_cost, own) || 1;
}

type Box = { node: PlanNode; x: number; y: number; depth: number; folded: number };
type Fold = { id: string; parent: number; count: number; share: number; x: number; y: number };

function subtreeSize(n: PlanNode): number {
  return 1 + n.children.reduce((s, c) => s + subtreeSize(c), 0);
}

function rowsOut(n: PlanNode): number {
  return n.act_rows ?? n.est_rows * Math.max(1, n.est_executions);
}

function heat(share: number): string {
  if (share >= 0.3) return "var(--heat-4)";
  if (share >= 0.15) return "var(--heat-3)";
  if (share >= 0.05) return "var(--heat-2)";
  if (share >= 0.01) return "var(--heat-1)";
  return "transparent";
}

function pathTo(node: PlanNode, id: number): Set<number> {
  const walk = (n: PlanNode): number[] | null => {
    if (n.id === id) return [n.id];
    for (const c of n.children) {
      const p = walk(c);
      if (p) return [n.id, ...p];
    }
    return null;
  };
  return new Set(walk(node) ?? []);
}

function allNodes(n: PlanNode, out: PlanNode[] = []): PlanNode[] {
  out.push(n);
  n.children.forEach((c) => allNodes(c, out));
  return out;
}

/** Tidy top-down layout: leaves take consecutive slots, parents center over their children. Cheap subtrees fold into one pill. */
function layout(root: PlanNode, total: number, expanded: Set<number>): { boxes: Box[]; folds: Fold[]; width: number; height: number } {
  const boxes: Box[] = [];
  const folds: Fold[] = [];
  let slot = 0;
  const place = (n: PlanNode, depth: number): number => {
    const visible = n.children.filter((c) => expanded.has(c.id) || c.subtree_cost / total >= CHEAP_SHARE);
    const hidden = n.children.filter((c) => !visible.includes(c));
    const xs = visible.map((c) => place(c, depth + 1));
    let x: number;
    if (hidden.length > 0) {
      const fx = slot++;
      folds.push({
        id: `fold-${n.id}`,
        parent: n.id,
        count: hidden.reduce((s, c) => s + subtreeSize(c), 0),
        share: hidden.reduce((s, c) => s + c.subtree_cost, 0) / total,
        x: fx,
        y: depth + 1,
      });
      xs.push(fx);
    }
    x = xs.length ? (Math.min(...xs) + Math.max(...xs)) / 2 : slot++;
    boxes.push({ node: n, x, y: depth, depth, folded: hidden.length });
    return x;
  };
  place(root, 0);
  const maxX = Math.max(0, ...boxes.map((b) => b.x), ...folds.map((f) => f.x));
  const maxY = Math.max(0, ...boxes.map((b) => b.y), ...folds.map((f) => f.y));
  return {
    boxes,
    folds,
    width: PAD * 2 + (maxX + 1) * CARD_W + maxX * GAP_X,
    height: PAD * 2 + 22 + (maxY + 1) * CARD_H + maxY * GAP_Y,
  };
}

const px = (x: number) => PAD + x * (CARD_W + GAP_X);
const py = (y: number) => PAD + 22 + y * (CARD_H + GAP_Y);

export function CostStrip({ plan, onPick }: { plan: Plan; onPick: (id: number) => void }) {
  const total = costBasis(plan);
  const top = allNodes(plan.root)
    .filter((n) => n.self_cost > 0)
    .sort((a, b) => b.self_cost - a.self_cost)
    .slice(0, 4);
  const rest = Math.max(0, 1 - top.reduce((s, n) => s + n.self_cost / total, 0));
  return (
    <div className="cost-strip">
      <div className="eyebrow">Where the cost goes</div>
      <div className="cost-bar">
        {top.map((n) => (
          <button key={n.id} style={{ flex: n.self_cost / total, background: heat(n.self_cost / total) === "transparent" ? "var(--sunken)" : heat(n.self_cost / total) }} title={`${n.op} ${percent(n.self_cost / total)}`} onClick={() => onPick(n.id)} />
        ))}
        {rest > 0.005 && <span style={{ flex: rest, background: "var(--sunken)" }} title={`Everything else ${percent(rest)}`} />}
      </div>
      <div className="cost-legend">
        {top.map((n) => (
          <button key={n.id} onClick={() => onPick(n.id)}>
            <b>{percent(n.self_cost / total)}</b> {n.op}
            {n.object && <span className="mono muted"> {n.object.split(".").pop()}</span>}
          </button>
        ))}
      </div>
    </div>
  );
}

export function PlanDiagram(props: {
  plan: Plan;
  focus?: number | null;
  /** Comparison marks by node id; marked nodes are never folded away. */
  marks?: Map<number, Mark>;
  /** Select (and reveal) this node when it changes, e.g. a change picked in the comparison. */
  selectedId?: number | null;
  onSelect?: (id: number) => void;
  initialZoom?: number;
  /** Shown before the operator count, e.g. "A" in a comparison. */
  badge?: string;
}) {
  const { plan, focus, marks, selectedId, onSelect, initialZoom, badge } = props;
  const total = costBasis(plan);
  const planCost = plan.total_cost || plan.root.subtree_cost;
  const nodes = useMemo(() => allNodes(plan.root), [plan]);
  const hottest = useMemo(() => nodes.reduce((a, b) => (b.self_cost > a.self_cost ? b : a), plan.root), [nodes, plan.root]);
  const target = selectedId ?? focus ?? hottest.id;
  const [expanded, setExpanded] = useState<Set<number>>(() => {
    const open = pathTo(plan.root, target);
    marks?.forEach((_, id) => pathTo(plan.root, id).forEach((n) => open.add(n)));
    return open;
  });
  const [selected, setSelected] = useState<number>(target);
  const [zoom, setZoom] = useState(initialZoom ?? 1);
  const hot = useMemo(() => pathTo(plan.root, target), [plan.root, target]);
  const { boxes, folds, width, height } = useMemo(() => layout(plan.root, total, expanded), [plan.root, total, expanded]);
  const byId = new Map(boxes.map((b) => [b.node.id, b]));
  const maxRows = Math.max(1, ...nodes.map(rowsOut));
  const stroke = (rows: number) => 1.5 + 5 * (Math.log10(rows + 1) / Math.log10(maxRows + 1));
  const selectedNode = nodes.find((n) => n.id === selected) ?? null;

  const pick = (id: number) => {
    setSelected(id);
    setExpanded((prev) => new Set([...prev, ...pathTo(plan.root, id)]));
  };

  useEffect(() => {
    if (selectedId != null && nodes.some((n) => n.id === selectedId)) pick(selectedId);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [selectedId]);

  const edges: { from: Box | Fold; to: Box; rows: number; hot: boolean; key: string }[] = [];
  boxes.forEach((b) => {
    b.node.children.forEach((c) => {
      const child = byId.get(c.id);
      if (child) edges.push({ from: child, to: b, rows: rowsOut(c), hot: hot.has(c.id) && hot.has(b.node.id), key: `${b.node.id}-${c.id}` });
    });
  });
  folds.forEach((f) => {
    const parent = byId.get(f.parent);
    if (parent) edges.push({ from: f, to: parent, rows: 0, hot: false, key: f.id });
  });

  return (
    <div className="plan">
      <div className="plan-head">
        {badge && <span className="side-badge">{badge}</span>}
        <span className="tag">{plan.kind === "actual" ? "Actual plan" : plan.kind === "generic" ? "Generic plan" : "Estimated plan"}</span>
        <span>
          {nodes.length} operator{nodes.length === 1 ? "" : "s"} · cost {planCost.toFixed(planCost < 10 ? 3 : 1)}
        </span>
        <span className="spacer" />
        {plan.warnings.length > 0 && (
          <span className="warn-chip">
            <TriangleAlert size={11} /> {plan.warnings.length} plan warning{plan.warnings.length > 1 ? "s" : ""}
          </span>
        )}
        <span className="zoom">
          <button className="btn icon ghost" title="Zoom out" onClick={() => setZoom((z) => Math.max(0.4, +(z - 0.15).toFixed(2)))}>
            <Minus size={13} />
          </button>
          <button className="btn icon ghost" title="Actual size" onClick={() => setZoom(1)}>
            <Maximize2 size={12} />
          </button>
          <button className="btn icon ghost" title="Zoom in" onClick={() => setZoom((z) => Math.min(1.6, +(z + 0.15).toFixed(2)))}>
            <Plus size={13} />
          </button>
        </span>
      </div>
      <CostStrip
        plan={plan}
        onPick={(id) => {
          pick(id);
          onSelect?.(id);
        }}
      />
      <div className="diagram-scroll">
        <div className="diagram" style={{ width: width * zoom, height: height * zoom }}>
          <div style={{ width, height, transform: `scale(${zoom})`, transformOrigin: "0 0", position: "relative" }}>
            <svg width={width} height={height} className="diagram-edges">
              {edges.map((e) => {
                const x1 = px(e.from.x) + CARD_W / 2;
                const y1 = py(e.from.y);
                const x2 = px(e.to.x) + CARD_W / 2;
                const y2 = py(e.to.y) + CARD_H;
                const mid = (y1 + y2) / 2;
                return (
                  <g key={e.key}>
                    <path
                      d={`M ${x1} ${y1} C ${x1} ${mid}, ${x2} ${mid}, ${x2} ${y2}`}
                      fill="none"
                      stroke={e.hot ? "var(--heat-4)" : "var(--border-strong)"}
                      strokeOpacity={e.hot ? 0.75 : 1}
                      strokeWidth={e.rows ? stroke(e.rows) : 1.5}
                      strokeDasharray={e.rows ? undefined : "4 4"}
                    />
                    {e.rows > 0 && (
                      <text x={x1 + 8} y={y1 - 8} className="edge-label">
                        {count(e.rows)} rows
                      </text>
                    )}
                  </g>
                );
              })}
            </svg>
            <div className="diagram-result" style={{ left: px(byId.get(plan.root.id)?.x ?? 0), top: PAD - 4, width: CARD_W }}>
              result
            </div>
            {boxes.map((b) => (
              <PlanCard
                key={b.node.id}
                node={b.node}
                share={b.node.self_cost / total}
                x={px(b.x)}
                y={py(b.y)}
                hot={hot.has(b.node.id)}
                hottest={b.node.id === hottest.id && b.node.self_cost / total >= 0.05}
                selected={b.node.id === selected}
                mark={marks?.get(b.node.id)}
                onSelect={() => {
                  setSelected(b.node.id);
                  onSelect?.(b.node.id);
                }}
              />
            ))}
            {folds.map((f) => (
              <button
                key={f.id}
                className="fold-pill"
                style={{ left: px(f.x) + CARD_W / 2 - 80, top: py(f.y) + CARD_H / 2 - 15 }}
                title="Show the cheap operators under this node"
                onClick={() => {
                  const parent = nodes.find((n) => n.id === f.parent);
                  if (parent) setExpanded((prev) => new Set([...prev, ...parent.children.map((c) => c.id)]));
                }}
              >
                +{f.count} cheap operator{f.count > 1 ? "s" : ""} · {percent(f.share)}
              </button>
            ))}
          </div>
        </div>
      </div>
      {selectedNode && <NodeProps node={selectedNode} share={selectedNode.self_cost / total} />}
    </div>
  );
}

function PlanCard(props: {
  node: PlanNode;
  share: number;
  x: number;
  y: number;
  hot: boolean;
  hottest: boolean;
  selected: boolean;
  mark?: Mark;
  onSelect: () => void;
}) {
  const { node, share, x, y, hot, hottest, selected, mark, onSelect } = props;
  const kind = KIND[node.kind];
  const est = node.est_rows * Math.max(1, node.est_executions);
  const act = node.act_rows;
  const off = act != null ? Math.max(act, 1) / Math.max(est, 1) : null;
  const misestimate = off != null && (off >= 10 || off <= 0.1) ? off : null;
  const table = node.object?.split(".").pop();
  return (
    <button
      className={`plan-card ${selected ? "sel" : ""} ${hot ? "hot" : ""} ${mark ? `mark-${mark}` : ""}`}
      style={{ left: x, top: y, width: CARD_W, height: CARD_H, ["--heat" as string]: heat(share) }}
      onClick={onSelect}
      title={node.predicate ? `filter ${shortPredicate(node.predicate)}` : node.seek_predicate ? `seek ${shortPredicate(node.seek_predicate)}` : node.op}
    >
      <span className="pc-top">
        <span className="pc-kind" style={{ color: kind.color }}>
          {kind.label || node.op}
        </span>
        <span className="pc-share">
          {mark && <span className={`pc-mark ${mark}`}>{MARK_LABEL[mark]}</span>}
          {hottest && <Flame size={12} color="var(--heat-4)" />}
          {percent(share)}
        </span>
      </span>
      <span className="pc-op">{node.op}</span>
      <span className="pc-obj mono">{table ? `${table}${node.index ? ` · ${node.index}` : ""}` : node.detail ?? ""}</span>
      <span className="pc-rows">
        {act != null ? (
          <>
            {count(est)} → <b>{count(act)}</b> rows
            {misestimate && <span className="mis">×{count(misestimate >= 1 ? misestimate : 1 / misestimate)}</span>}
          </>
        ) : (
          <>{count(est)} rows est.</>
        )}
        {node.act_time_ms != null && <span className="muted"> · {duration(node.act_time_ms)}</span>}
        {node.warnings.length > 0 && <TriangleAlert size={11} color="var(--medium)" style={{ marginLeft: 4 }} />}
      </span>
    </button>
  );
}
