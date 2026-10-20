import { ChevronDown, ChevronRight, Flame, TriangleAlert } from "lucide-react";
import { useEffect, useMemo, useState } from "react";
import type { Plan } from "../gen/Plan";
import type { PlanInsight } from "../gen/PlanInsight";
import type { PlanNode } from "../gen/PlanNode";
import type { QueryStat } from "../gen/QueryStat";
import { planShares } from "../lib/actual";
import { can } from "../lib/engines";
import { count, percent } from "../lib/format";
import { estRows, estRowsForRuns, findingNoShareReason, insightsByNode, misestimate, noShareReason, rows as rowsText } from "../lib/plan-format";
import { NodeDetails } from "./NodeDetails";
import { OpIcon } from "./OpIcon";
import { ActualSummary } from "./ActualSummary";

type Props = {
  plan: Plan;
  query?: QueryStat | null;
  focus?: number | null;
  compact?: boolean;
  /** Select (and reveal) this node when it changes, e.g. an insight picked in the list. */
  selectedId?: number | null;
  onSelect?: (id: number) => void;
};

const WARNING_LABEL: Record<string, string> = {
  "implicit-conversion": "Type conversion",
  spill: "Spills to disk",
  "no-join-predicate": "No join predicate",
  "missing-statistics": "No statistics",
  "memory-grant": "Memory grant",
  misestimate: "Estimate off",
  "residual-predicate": "Filters after reading",
};

function walk(node: PlanNode, out: PlanNode[] = []): PlanNode[] {
  out.push(node);
  node.children.forEach((c) => walk(c, out));
  return out;
}

function pathTo(node: PlanNode, id: number): number[] | null {
  if (node.id === id) return [node.id];
  for (const child of node.children) {
    const path = pathTo(child, id);
    if (path) return [node.id, ...path];
  }
  return null;
}

/** `[db].[dbo].[orders].[created_at]>=[@P1]` reads as `created_at >= @P1`. */
export function shortPredicate(text: string): string {
  return text
    .replace(/(\[[^\]]+\]\.)+(?=\[)/g, "")
    .replace(/\b\w+\.\w+\.(?=\w)/g, "")
    .replace(/[[\]]/g, "")
    .replace(/\s*(>=|<=|<>|=|<|>)\s*/g, " $1 ")
    .replace(/\s+/g, " ")
    .trim();
}

function heatVar(share: number): string {
  if (share >= 0.3) return "var(--heat-4)";
  if (share >= 0.15) return "var(--heat-3)";
  if (share >= 0.05) return "var(--heat-2)";
  if (share >= 0.01) return "var(--heat-1)";
  return "var(--heat-0)";
}

export function PlanView({ plan, query, focus, compact, selectedId, onSelect }: Props) {
  const nodes = useMemo(() => walk(plan.root), [plan]);
  const flagged = useMemo(() => insightsByNode(plan), [plan]);
  // Measured plans rank operators by their own time; estimated plans by cost (under a PostgreSQL Limit, children's costs
  // assume they run to completion and add up to more than the root's, which planShares accounts for).
  const shares = useMemo(() => planShares(plan, query), [plan, query]);
  const reason = (n: PlanNode) => focus === n.id ? findingNoShareReason(plan, n, query) : noShareReason(plan, n, query);
  const focused = nodes.find((n) => n.id === focus);
  const note = shares.basis === "cost" && focused ? reason(focused) ?? shares.note : shares.note;
  // The share a person reads: none where the plan can't state one (beside a step it puts no cost on, on a default guess).
  const shareOf = (n: PlanNode) => shares.stated.get(n.id) ?? null;
  // The core's hottest step (never an exchange), only in a plan that does real work and where its share can be stated.
  const hottest = shares.hottest;
  const target = focus ?? hottest?.id ?? plan.root.id;
  const hotPath = useMemo(() => new Set(pathTo(plan.root, target) ?? []), [plan.root, target]);

  const [expanded, setExpanded] = useState<Set<number>>(() => {
    const open = new Set<number>(hotPath);
    nodes.forEach((n) => {
      if ((shares.subtree.get(n.id) ?? 0) >= 0.05) open.add(n.id);
    });
    return open;
  });
  const [selected, setSelected] = useState<number | null>(selectedId ?? target);
  const selectedNode = nodes.find((n) => n.id === selected) ?? null;

  useEffect(() => {
    if (selectedId == null) return;
    setSelected(selectedId);
    const path = pathTo(plan.root, selectedId) ?? [];
    setExpanded((prev) => new Set([...prev, ...path]));
  }, [selectedId, plan.root]);
  const hasActuals = nodes.some((n) => n.act_rows != null);
  // SQLite's plans have no row or cost estimates: no shares or estimated rows to show.
  const weighed = can(plan.engine, "plan_estimates") || hasActuals;

  const toggle = (id: number) =>
    setExpanded((prev) => {
      const next = new Set(prev);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });

  const rows: { node: PlanNode; depth: number }[] = [];
  const visit = (node: PlanNode, depth: number) => {
    rows.push({ node, depth });
    if (expanded.has(node.id)) node.children.forEach((c) => visit(c, depth + 1));
  };
  visit(plan.root, 0);

  return (
    <div className="plan">
      <div className="plan-head">
        <span className="tag">{plan.kind === "actual" ? "Actual plan" : plan.kind === "generic" ? "Generic plan" : "Estimated plan"}</span>
        <span>
          {nodes.length} operator{nodes.length === 1 ? "" : "s"}
          {weighed && ` · cost ${(plan.total_cost || plan.root.subtree_cost).toFixed((plan.total_cost || plan.root.subtree_cost) < 10 ? 3 : 1)}`}
        </span>
        <span className="spacer" />
        {plan.warnings.length > 0 && (
          <span className="warn-chip">
            <TriangleAlert size={11} />
            {plan.warnings.map((w) => WARNING_LABEL[w] ?? w).join(", ")}
          </span>
        )}
      </div>
      {plan.kind === "actual" && <ActualSummary plan={plan} onPick={setSelected} />}
      <div className="plan-cols">
        <span>Operator</span>
        <span>{hasActuals ? "Rows est → actual" : weighed ? "Est. rows" : ""}</span>
        <span>{shares.basis === "time" ? "Own time" : weighed ? "Self cost" : ""}</span>
      </div>
      <div style={compact ? { maxHeight: 340, overflowY: "auto" } : undefined}>
        {rows.map(({ node, depth }) => {
          const share = shareOf(node);
          // Shown as off only where the core flagged it (10× on 100+ rows, not an operator stopped early).
          const mis = node.warnings.includes("misestimate") ? misestimate(plan, node) : null;
          return (
            <button
              key={node.id}
              className={`pnode ${selected === node.id ? "sel" : ""} ${hotPath.has(node.id) ? "hot" : ""}`}
              onClick={() => {
                setSelected(node.id);
                onSelect?.(node.id);
              }}
              onDoubleClick={() => toggle(node.id)}
            >
              <span className="label">
                <span className="indent" style={{ width: Math.min(depth, 12) * 12 }} />
                <span
                  className="twisty"
                  onClick={(e) => {
                    e.stopPropagation();
                    toggle(node.id);
                  }}
                >
                  {node.children.length > 0 ? expanded.has(node.id) ? <ChevronDown size={14} /> : <ChevronRight size={14} /> : null}
                </span>
                <span className="lines">
                  <span className="line1">
                    <span className="op-icon">
                      <OpIcon node={node} size={13} />
                    </span>
                    {node.id === hottest?.id && (share ?? 0) >= 0.05 && <Flame size={13} color="var(--heat-4)" className="flame" />}
                    <span className="opname">{node.op}</span>
                    {flagged.get(node.id)?.[0] && (
                      <span className={`tree-flag ${flagged.get(node.id)![0]!.severity}`} title={flagged.get(node.id)!.map((i) => i.title).join("\n")}>
                        {flagged.get(node.id)!.length}
                      </span>
                    )}
                    {node.object && (
                      <span className="obj">
                        {node.object}
                        {node.index ? ` · ${node.index}` : ""}
                      </span>
                    )}
                  </span>
                  {(node.warnings.length > 0 || node.predicate || node.seek_predicate) && (
                    <span className="line2">
                      {node.warnings.map((w) => (
                        <span key={w} className="warn-chip">
                          {WARNING_LABEL[w] ?? w}
                        </span>
                      ))}
                      {(node.seek_predicate || node.predicate) && <span className="pred">{node.seek_predicate ? `seek ${shortPredicate(node.seek_predicate)}` : `filter ${shortPredicate(node.predicate ?? "")}`}</span>}
                    </span>
                  )}
                </span>
              </span>
              <span className="rows">
                {node.act_rows != null ? (
                  <>
                    {rowsText(estRowsForRuns(plan, node))} <span className="arrow">→</span> {rowsText(node.act_rows)}
                    {mis != null && <span className="mis">×{count(Math.abs(mis))}{mis < 0 ? " over" : " under"}</span>}
                  </>
                ) : (
                  weighed && rowsText(estRows(plan, node))
                )}
              </span>
              <span className="cost">
                {weighed &&
                  (share != null ? (
                    <>
                      <span className="num">{percent(share)}</span>
                      <span className="heat">
                        <span style={{ width: `${Math.max(2, Math.min(100, share * 100))}%`, background: heatVar(share) }} />
                      </span>
                    </>
                  ) : (
                    <span className="num muted" title={reason(node) ?? undefined}>
                      —
                    </span>
                  ))}
              </span>
            </button>
          );
        })}
      </div>
      {weighed && note && <div className="muted" style={{ fontSize: 12, padding: "6px 10px" }}>— {note}</div>}
      {selectedNode && (
        <NodeDetails
          plan={plan}
          estimates={weighed}
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

/** The operator detail panel; kept under this name for existing callers. */
export function NodeProps({ node, share, insights }: { node: PlanNode; share: number | null; insights?: PlanInsight[] }) {
  return <NodeDetails node={node} share={share} insights={insights} />;
}
