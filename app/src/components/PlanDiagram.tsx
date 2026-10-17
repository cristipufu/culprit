import { Flame, Maximize2, Minus, Plus, Scan, TriangleAlert } from "lucide-react";
import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState, type ReactNode } from "react";
import type { Plan } from "../gen/Plan";
import type { PlanInsight } from "../gen/PlanInsight";
import type { PlanNode } from "../gen/PlanNode";
import type { QueryStat } from "../gen/QueryStat";
import { planShares } from "../lib/actual";
import { duration, percent } from "../lib/format";
import { can, serverName } from "../lib/engines";
import { allNodes, costBasis as basis, cost, estRowsForRuns, findingNoShareReason, insightsByNode, isWrapper, misestimate, noShareReason, rows, rowsOut, uncostedSteps } from "../lib/plan-format";
import { ActualSummary } from "./ActualSummary";
import { NodeDetails } from "./NodeDetails";
import { OpIcon } from "./OpIcon";
import { shortPredicate } from "./PlanView";

const CARD_W = 196;

/**
 * An operator name short enough for a card, keeping the word that tells operators apart: "Clustered Index Update" →
 * "Clustered Update" (it is never the "Index" that differs). The card's tooltip and label keep the full name.
 */
export function compactOp(op: string): string {
  return op
    .replace(/^(Clustered|Nonclustered|Columnstore|Clustered Columnstore) Index (?=\S)/i, "$1 ")
    .replace(/^(Unique|Subquery) index lookup$/i, "$1 lookup")
    .replace(/^Table-valued function$/i, "Table function");
}
const CARD_H = 74;
const GAP_X = 28;
const GAP_Y = 62;
const PAD = 26;
const CHEAP_SHARE = 0.005;
const MIN_FIT = 0.45;

/** How a node differs from the other plan in a comparison. */
export type Mark = "changed" | "added" | "removed";

const MARK_LABEL: Record<Mark, string> = { changed: "changed", added: "only here", removed: "only here" };

export const costBasis = basis;

/** A card on the canvas: an operator, with the pass-through operators above it folded into its edge label. */
type Box = { node: PlanNode; x: number; y: number; via: string[] };
type Fold = { id: string; parent: number; count: number; share: number | null; x: number; y: number };

function subtreeSize(n: PlanNode): number {
  return 1 + n.children.reduce((s, c) => s + subtreeSize(c), 0);
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

/** Skip pass-through wrappers: the operator to draw and the names of the ones folded into its edge. */
function drawn(n: PlanNode, total: number, keep: Set<number>): { node: PlanNode; via: string[] } {
  const via: string[] = [];
  let cur = n;
  while (isWrapper(cur, total) && !keep.has(cur.id) && cur.children[0]) {
    via.push(cur.op);
    cur = cur.children[0];
  }
  return { node: cur, via };
}

/** Tidy top-down layout: leaves take consecutive slots, parents center over their children. Cheap subtrees fold into one
 * pill (only where the plan weighs its steps: without estimates nothing is cheap); wrappers that only pass rows on become
 * edge labels. */
function layout(root: PlanNode, total: number, expanded: Set<number>, keep: Set<number>, weighed: boolean, subtreeShare: (n: PlanNode) => number | null) {
  const boxes: Box[] = [];
  const folds: Fold[] = [];
  const parentOf = new Map<number, number>();
  let slot = 0;
  const place = (n: PlanNode, via: string[], depth: number): number => {
    const kids = n.children.map((c) => drawn(c, total, keep));
    const visible = kids.filter((k) => !weighed || expanded.has(k.node.id) || keep.has(k.node.id) || k.node.subtree_cost / total >= CHEAP_SHARE);
    const hidden = kids.filter((k) => !visible.includes(k));
    const xs = visible.map((k) => {
      parentOf.set(k.node.id, n.id);
      return place(k.node, k.via, depth + 1);
    });
    if (hidden.length > 0) {
      const fx = slot++;
      // Their share only where the plan can state each one's.
      const shares = hidden.map((k) => subtreeShare(k.node));
      folds.push({
        id: `fold-${n.id}`,
        parent: n.id,
        count: hidden.reduce((s, k) => s + subtreeSize(k.node) + k.via.length, 0),
        share: shares.every((v) => v != null) ? shares.reduce((s: number, v) => s + (v ?? 0), 0) : null,
        x: fx,
        y: depth + 1,
      });
      xs.push(fx);
    }
    const x = xs.length ? (Math.min(...xs) + Math.max(...xs)) / 2 : slot++;
    boxes.push({ node: n, x, y: depth, via });
    return x;
  };
  const top = drawn(root, total, keep);
  place(top.node, top.via, 0);
  const maxX = Math.max(0, ...boxes.map((b) => b.x), ...folds.map((f) => f.x));
  const maxY = Math.max(0, ...boxes.map((b) => b.y), ...folds.map((f) => f.y));
  return {
    boxes,
    folds,
    parentOf,
    rootVia: top.via,
    width: PAD * 2 + (maxX + 1) * CARD_W + maxX * GAP_X,
    height: PAD * 2 + 24 + (maxY + 1) * CARD_H + maxY * GAP_Y,
  };
}

const px = (x: number) => PAD + x * (CARD_W + GAP_X);
const py = (y: number) => PAD + 24 + y * (CARD_H + GAP_Y);

export function CostStrip({ plan, query, focus, onPick }: { plan: Plan; query?: QueryStat | null; focus?: number | null; onPick: (id: number) => void }) {
  // Actual plans with per-operator timing show where the time went; everything else, the optimizer's cost estimate.
  const shares = useMemo(() => planShares(plan, query), [plan, query]);
  const focused = allNodes(plan.root).find((n) => n.id === focus);
  const note = shares.basis === "cost" && focused ? findingNoShareReason(plan, focused, query) ?? shares.note : shares.note;
  if (shares.basis !== "time" && !can(plan.engine, "plan_estimates")) {
    return (
      <div className="cost-strip">
        <div className="eyebrow">{serverName(plan.engine)}'s plan has the access path only: no row or cost estimates to weigh its steps by.</div>
      </div>
    );
  }
  // Only shares the plan can state: none beside a step it puts no cost on (MySQL's filesort), nor on a default guess.
  const own = (n: PlanNode) => shares.stated.get(n.id) ?? 0;
  const top = allNodes(plan.root)
    .filter((n) => own(n) > 0)
    .sort((a, b) => own(b) - own(a))
    .slice(0, 4);
  if (top.length === 0 && note) {
    return (
      <div className="cost-strip">
        <div className="eyebrow">Where the cost goes: {note}</div>
      </div>
    );
  }
  const rest = Math.max(0, 1 - top.reduce((s, n) => s + own(n), 0));
  return (
    <div className="cost-strip">
      <div className="eyebrow">{shares.basis === "time" ? `Where the ${duration(shares.total)} went` : "Where the cost goes"}</div>
      <div className="cost-bar">
        {top.map((n) => (
          <button
            key={n.id}
            // Outlined only for the step where the work is (never an exchange), in a plan that does real work.
            className={n.id === shares.hottest?.id ? "hottest" : ""}
            style={{ flex: own(n), background: heat(own(n)) === "transparent" ? "var(--sunken)" : heat(own(n)) }}
            title={`${n.op} ${percent(own(n))}`}
            onClick={() => onPick(n.id)}
          />
        ))}
        {rest > 0.005 && <span style={{ flex: rest, background: "var(--sunken)" }} title={`Everything else ${percent(rest)}`} />}
      </div>
      <div className="cost-legend">
        {top.map((n) => (
          <button key={n.id} onClick={() => onPick(n.id)}>
            <OpIcon node={n} size={12} />
            <b>{percent(own(n))}</b> {n.op}
            {n.object && <span className="mono muted"> {n.object.split(".").pop()}</span>}
            {/* An exchange's cost is the charge for passing rows between threads: never the step where the work is. */}
            {n.kind === "parallelism" && <span className="muted"> · passing rows between workers</span>}
          </button>
        ))}
      </div>
      {shares.note && <div className="muted" style={{ fontSize: 12, marginTop: 4 }}>{shares.note}</div>}
    </div>
  );
}

export function PlanDiagram(props: {
  plan: Plan;
  query?: QueryStat | null;
  focus?: number | null;
  /** Comparison marks by node id; marked nodes are never folded away. */
  marks?: Map<number, Mark>;
  /** Select (and reveal) this node when it changes, e.g. an insight or a change picked elsewhere. */
  selectedId?: number | null;
  onSelect?: (id: number) => void;
  initialZoom?: number;
  /** Shown before the operator count, e.g. "A" in a comparison. */
  badge?: string;
  /** Hide the header and cost strip when a surrounding panel shows them. */
  bare?: boolean;
  /** Comparison regions by node id: cards in the same region get the same tint on both sides (`data-tint`). */
  tints?: Map<number, number>;
  /** Controlled zoom, e.g. kept in step across two diagrams. */
  zoom?: number;
  onZoomChange?: (zoom: number) => void;
  /** Show the selected operator's properties under the diagram (a comparison shows them side by side instead). */
  showProps?: boolean;
  /** Replaces the plan-kind tag, e.g. "Running now" for a statement in flight. */
  kindLabel?: string;
  /** Drawn over each visible operator in the same coordinates as its card, e.g. live progress. */
  overlay?: (node: PlanNode, box: { x: number; y: number; w: number; h: number }) => ReactNode;
}) {
  const { plan, query, focus, marks, selectedId, onSelect, initialZoom, badge, bare, tints, onZoomChange, overlay, kindLabel } = props;
  const reason = (n: PlanNode) => focus === n.id ? findingNoShareReason(plan, n, query) : noShareReason(plan, n, query);
  const total = basis(plan);
  // Measured plans rank operators by their own time; estimated plans by cost.
  const shares = useMemo(() => planShares(plan, query), [plan, query]);
  // The share a person reads: none where the plan can't state one (beside a step it puts no cost on, on a default guess).
  const shareOf = (n: PlanNode) => shares.stated.get(n.id) ?? null;
  const planCost = plan.total_cost || plan.root.subtree_cost;
  const estimates = can(plan.engine, "plan_estimates") || shares.basis === "time";
  const nodes = useMemo(() => allNodes(plan.root), [plan]);
  const flagged = useMemo(() => insightsByNode(plan), [plan]);
  // The core's hottest step (never an exchange such as Gather Merge), only in a plan that does real work and where its
  // share can be stated: a point lookup's costliest step isn't where its time goes.
  const hottest = shares.hottest;
  const target = selectedId ?? focus ?? hottest?.id ?? plan.root.id;
  // Operators that must stay visible: marked in a comparison, named by an insight, or doing work the plan puts no cost on
  // (a filesort with no cost of its own isn't cheap).
  const keep = useMemo(() => new Set<number>([...(marks?.keys() ?? []), ...flagged.keys(), ...uncostedSteps(plan).map((n) => n.id)]), [marks, flagged, plan]);
  const [expanded, setExpanded] = useState<Set<number>>(() => {
    const open = pathTo(plan.root, target);
    keep.forEach((id) => pathTo(plan.root, id).forEach((n) => open.add(n)));
    return open;
  });
  const [selected, setSelected] = useState<number>(target);
  const [ownZoom, setOwnZoom] = useState(initialZoom ?? 1);
  const zoom = props.zoom ?? ownZoom;
  const setZoom = (next: number | ((z: number) => number)) => {
    const value = typeof next === "function" ? next(zoom) : next;
    setOwnZoom(value);
    onZoomChange?.(value);
  };
  const [fitted, setFitted] = useState(initialZoom != null || props.zoom != null);
  const scroller = useRef<HTMLDivElement>(null);
  const hot = useMemo(() => pathTo(plan.root, target), [plan.root, target]);
  const { boxes, folds, parentOf, rootVia, width, height } = useMemo(
    () => layout(plan.root, total, expanded, keep, estimates, (n) => shares.statedSubtree.get(n.id) ?? null),
    [plan.root, total, expanded, keep, estimates, shares],
  );
  const byId = new Map(boxes.map((b) => [b.node.id, b]));
  const maxRows = Math.max(1, ...nodes.map(rowsOut));
  const stroke = (r: number) => 1.5 + 4.5 * (Math.log10(r + 1) / Math.log10(maxRows + 1));
  const selectedNode = nodes.find((n) => n.id === selected) ?? null;

  /** Fit the whole plan in the visible width (never above 100%), so small plans aren't lost in a corner. */
  const fit = useCallback(() => {
    const box = scroller.current?.getBoundingClientRect();
    if (!box || box.width === 0) return;
    setZoom(Math.max(MIN_FIT, Math.min(1, (box.width - 8) / width)));
  }, [width]);

  useLayoutEffect(() => {
    if (fitted) return;
    fit();
    setFitted(true);
  }, [fit, fitted]);

  const pick = (id: number) => {
    // A wrapper folded into an edge selects the operator it wraps.
    const node = nodes.find((n) => n.id === id);
    const visibleId = byId.has(id) || !node ? id : drawn(node, total, keep).node.id;
    setSelected(visibleId);
    setExpanded((prev) => new Set([...prev, ...pathTo(plan.root, visibleId)]));
  };

  useEffect(() => {
    if (selectedId != null && nodes.some((n) => n.id === selectedId)) pick(selectedId);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [selectedId]);

  // Keep the selected card in view, scrolling the canvas only (never the page around it).
  useEffect(() => {
    const canvas = scroller.current;
    const el = canvas?.querySelector<HTMLElement>(`[data-node="${selected}"]`);
    if (!canvas || !el) return;
    const c = canvas.getBoundingClientRect();
    const e = el.getBoundingClientRect();
    if (e.top < c.top || e.bottom > c.bottom) canvas.scrollTop += e.top - c.top - (c.height - e.height) / 2;
    if (e.left < c.left || e.right > c.right) canvas.scrollLeft += e.left - c.left - (c.width - e.width) / 2;
  }, [selected, zoom]);

  const edges: { from: Box | Fold; to: Box; rows: number; hot: boolean; key: string; via: string[] }[] = [];
  boxes.forEach((b) => {
    const parent = parentOf.get(b.node.id);
    const to = parent != null ? byId.get(parent) : undefined;
    if (to) edges.push({ from: b, to, rows: rowsOut(b.node), hot: hot.has(b.node.id) && hot.has(to.node.id), key: `${to.node.id}-${b.node.id}`, via: b.via });
  });
  folds.forEach((f) => {
    const parent = byId.get(f.parent);
    if (parent) edges.push({ from: f, to: parent, rows: 0, hot: false, key: f.id, via: [] });
  });

  const select = (id: number) => {
    setSelected(id);
    onSelect?.(id);
  };

  return (
    <div className={`plan ${bare ? "bare" : ""}`}>
      {!bare && (
        <div className="plan-head">
          {badge && <span className="side-badge">{badge}</span>}
          <span className="tag">{kindLabel ?? (plan.kind === "actual" ? "Actual plan" : plan.kind === "generic" ? "Generic plan" : "Estimated plan")}</span>
          <span>
            {nodes.length} operator{nodes.length === 1 ? "" : "s"}
            {estimates && ` · cost ${cost(planCost)}`}
          </span>
          <span className="spacer" />
          {plan.warnings.length > 0 && (
            <span className="warn-chip">
              <TriangleAlert size={11} /> {plan.warnings.length} plan warning{plan.warnings.length > 1 ? "s" : ""}
            </span>
          )}
          <ZoomControls zoom={zoom} setZoom={setZoom} fit={fit} />
        </div>
      )}
      {!bare && plan.kind === "actual" && <ActualSummary plan={plan} onPick={select} />}
      {!bare && <CostStrip plan={plan} query={query} focus={focus} onPick={select} />}
      <div className="diagram-wrap">
        {bare && (
          <div className="diagram-float">
            <ZoomControls zoom={zoom} setZoom={setZoom} fit={fit} />
          </div>
        )}
        <div className="diagram-scroll" ref={scroller}>
          <div className="diagram" style={{ width: width * zoom, height: height * zoom }}>
            <div style={{ width, height, transform: `scale(${zoom})`, transformOrigin: "0 0", position: "relative" }}>
              <svg width={width} height={height} className="diagram-edges" aria-hidden="true">
                <defs>
                  <marker id="pd-arrow" viewBox="0 0 8 8" refX="7" refY="4" markerWidth="8" markerHeight="8" markerUnits="userSpaceOnUse" orient="auto-start-reverse">
                    <path d="M0,0 L8,4 L0,8 z" className="pd-arrow" />
                  </marker>
                  <marker id="pd-arrow-hot" viewBox="0 0 8 8" refX="7" refY="4" markerWidth="8" markerHeight="8" markerUnits="userSpaceOnUse" orient="auto-start-reverse">
                    <path d="M0,0 L8,4 L0,8 z" className="pd-arrow hot" />
                  </marker>
                </defs>
                {edges.map((e) => {
                  const x1 = px(e.from.x) + CARD_W / 2;
                  const y1 = py(e.from.y);
                  const x2 = px(e.to.x) + CARD_W / 2;
                  const y2 = py(e.to.y) + CARD_H + 3;
                  const mid = (y1 + y2) / 2;
                  const label = e.rows > 0 ? `${rows(e.rows)} row${rows(e.rows) === "1" ? "" : "s"}` : "";
                  const via = e.via.length ? `via ${e.via.join(", ")}` : "";
                  const text = [label, via].filter(Boolean).join(" · ");
                  const w = text.length * 5.9 + 12;
                  // Beside the curve's midpoint, on the side away from the parent, so it never sits on the line.
                  const lx = (x1 + x2) / 2 + (x1 <= x2 ? -w - 8 : 8);
                  return (
                    <g key={e.key}>
                      <path
                        d={`M ${x1} ${y1} C ${x1} ${mid}, ${x2} ${mid}, ${x2} ${y2}`}
                        fill="none"
                        className={`pd-edge ${e.hot ? "hot" : ""}`}
                        strokeWidth={e.rows ? stroke(e.rows) : 1.5}
                        strokeDasharray={e.rows ? undefined : "4 4"}
                        markerEnd={`url(#${e.hot ? "pd-arrow-hot" : "pd-arrow"})`}
                      />
                      {text && (
                        <g transform={`translate(${lx} ${mid - 9})`} className="edge-pill">
                          <rect width={w} height={18} rx={9} />
                          <text x={w / 2} y={12.5} textAnchor="middle">
                            {text}
                          </text>
                        </g>
                      )}
                    </g>
                  );
                })}
              </svg>
              <div className="diagram-result" style={{ left: px(boxes.find((b) => !parentOf.has(b.node.id))?.x ?? 0), top: PAD - 4, width: CARD_W }}>
                result{rootVia.length ? ` · via ${rootVia.join(", ")}` : ""}
              </div>
              {boxes.map((b) => (
                <PlanCard
                  key={b.node.id}
                  estimates={estimates}
                  node={b.node}
                  share={shareOf(b.node)}
                  ownMs={shares.ms.get(b.node.id)}
                  x={px(b.x)}
                  y={py(b.y)}
                  hot={hot.has(b.node.id)}
                  hottest={b.node.id === hottest?.id && (shareOf(b.node) ?? 0) >= 0.05}
                  noShare={shares.basis === "cost" ? reason(b.node) : null}
                  selected={b.node.id === selected}
                  mark={marks?.get(b.node.id)}
                  tint={tints?.get(b.node.id)}
                  insights={flagged.get(b.node.id)}
                  onSelect={() => select(b.node.id)}
                />
              ))}
              {overlay && boxes.map((b) => <span key={`overlay-${b.node.id}`}>{overlay(b.node, { x: px(b.x), y: py(b.y), w: CARD_W, h: CARD_H })}</span>)}
              {folds.map((f) => (
                <button
                  key={f.id}
                  className="fold-pill"
                  style={{ left: px(f.x) + CARD_W / 2 - 78, top: py(f.y) + CARD_H / 2 - 14 }}
                  title="Show the cheap operators under this node"
                  onClick={() => {
                    const parent = nodes.find((n) => n.id === f.parent);
                    if (parent) setExpanded((prev) => new Set([...prev, ...parent.children.map((c) => drawn(c, total, keep).node.id)]));
                  }}
                >
                  +{f.count} cheap operator{f.count > 1 ? "s" : ""}
                  {f.share != null && ` · ${percent(f.share)}`}
                </button>
              ))}
            </div>
          </div>
        </div>
      </div>
      {selectedNode && props.showProps !== false && (
        <NodeDetails
          estimates={estimates}
          actual={plan.kind === "actual"}
          node={selectedNode}
          share={shareOf(selectedNode)}
          subtreeShare={shares.statedSubtree.get(selectedNode.id) ?? null}
          noShare={shares.basis === "cost" ? reason(selectedNode) : null}
          insights={flagged.get(selectedNode.id)}
        />
      )}
    </div>
  );
}

function ZoomControls({ zoom, setZoom, fit }: { zoom: number; setZoom: (f: (z: number) => number) => void; fit: () => void }) {
  return (
    <span className="zoom">
      <button className="btn icon ghost" title="Zoom out" onClick={() => setZoom((z) => Math.max(0.3, +(z - 0.15).toFixed(2)))}>
        <Minus size={13} />
      </button>
      <span className="zoom-level">{Math.round(zoom * 100)}%</span>
      <button className="btn icon ghost" title="Zoom in" onClick={() => setZoom((z) => Math.min(1.6, +(z + 0.15).toFixed(2)))}>
        <Plus size={13} />
      </button>
      <button className="btn icon ghost" title="Fit the whole plan" onClick={fit}>
        <Scan size={13} />
      </button>
      <button className="btn icon ghost" title="Actual size" onClick={() => setZoom(() => 1)}>
        <Maximize2 size={12} />
      </button>
    </span>
  );
}

function PlanCard(props: {
  node: PlanNode;
  /** Its own share; null where the plan can't state one. */
  share: number | null;
  /** Why it has none. */
  noShare?: string | null;
  x: number;
  y: number;
  hot: boolean;
  hottest: boolean;
  selected: boolean;
  mark?: Mark;
  insights?: PlanInsight[];
  tint?: number;
  /** The operator's own time in a measured run (its inputs' time taken out). */
  ownMs?: number;
  /** The plan weighs its steps (costs or measured time): show each one's share and its rows. */
  estimates: boolean;
  onSelect: () => void;
}) {
  const { node, x, y, hot, hottest, selected, mark, insights, tint, ownMs, estimates, onSelect, noShare } = props;
  const share = props.share ?? 0;
  // Shown as off only where the core flagged it (10× on 100+ rows, not an operator stopped early).
  const mis = node.warnings.includes("misestimate") ? misestimate(node) : null;
  const table = node.object?.split(".").pop();
  const worst = insights?.[0];
  const tip = [
    node.op,
    node.predicate ? `filter ${shortPredicate(node.predicate)}` : node.seek_predicate ? `seek ${shortPredicate(node.seek_predicate)}` : "",
    estimates && props.share == null ? noShare ?? "" : "",
    ...(insights ?? []).map((i) => `• ${i.title}`),
  ]
    .filter(Boolean)
    .join("\n");
  return (
    <button
      data-node={node.id}
      data-tint={tint}
      className={`plan-card ${selected ? "sel" : ""} ${hot ? "hot" : ""} ${mark ? `mark-${mark}` : ""} ${worst ? `flag-${worst.severity}` : ""}`}
      style={{ left: x, top: y, width: CARD_W, height: CARD_H, ["--heat" as string]: heat(share), ["--share" as string]: `${Math.max(share > 0 ? 3 : 0, Math.min(100, share * 100))}%` }}
      onClick={onSelect}
      title={tip}
      aria-label={`${node.op}${table ? ` on ${table}` : ""}${estimates && props.share != null ? `, ${percent(share)} of the work` : ""}${worst ? `, ${insights!.length} issue${insights!.length > 1 ? "s" : ""}` : ""}`}
    >
      <span className="pc-top">
        <span className="pc-icon">
          <OpIcon node={node} size={14} />
        </span>
        <span className={`pc-op ${compactOp(node.op).length > 13 ? "long" : ""}`} title={node.op}>
          {compactOp(node.op)}
        </span>
        {estimates && (
          <span className="pc-share">
            {hottest && <Flame size={11} color="var(--heat-4)" />}
            {props.share != null ? percent(share) : "—"}
          </span>
        )}
      </span>
      <span className="pc-obj mono">{table ? `${table}${node.index ? ` · ${node.index}` : ""}` : node.detail ?? " "}</span>
      <span className="pc-rows">
        {node.act_rows != null ? (
          <>
            {rows(estRowsForRuns(node))} → <b>{rows(node.act_rows)}</b>
            {mis != null && <span className="mis">×{rows(Math.abs(mis))}</span>}
          </>
        ) : estimates ? (
          <>{rows(node.est_rows * Math.max(1, node.est_executions))} est.</>
        ) : (
          node.seek_predicate && (
            <span className="muted mono pc-seek" title={node.seek_predicate}>
              {node.seek_predicate}
            </span>
          )
        )}
        {(ownMs ?? node.act_time_ms) != null && <span className="muted"> · {duration(ownMs ?? node.act_time_ms!)}</span>}
        {node.warnings.length > 0 && !worst && <TriangleAlert size={11} color="var(--medium-text)" style={{ marginLeft: 3 }} />}
        {/* The compare mark sits here, not beside the name: the name is what tells two steps apart. */}
        {mark && <span className={`pc-mark ${mark}`}>{MARK_LABEL[mark]}</span>}
      </span>
      {worst && (
        <span className={`pc-flag ${worst.severity}`} title={insights!.map((i) => i.title).join("\n")}>
          {insights!.length}
        </span>
      )}
    </button>
  );
}
