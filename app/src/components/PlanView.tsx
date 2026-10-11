import { ChevronDown, ChevronRight, Flame, TriangleAlert } from "lucide-react";
import { useMemo, useState } from "react";
import type { Plan } from "../gen/Plan";
import type { PlanNode } from "../gen/PlanNode";
import { count, duration, percent } from "../lib/format";

type Props = { plan: Plan; focus?: number | null; compact?: boolean };

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

function misestimate(node: PlanNode): number | null {
  if (node.act_rows == null) return null;
  const est = Math.max(1, node.est_rows * Math.max(1, node.est_executions));
  const act = Math.max(1, node.act_rows);
  return act >= est ? act / est : -(est / act);
}

export function PlanView({ plan, focus, compact }: Props) {
  const nodes = useMemo(() => walk(plan.root), [plan]);
  const hottest = useMemo(() => nodes.reduce((a, b) => (b.self_cost > a.self_cost ? b : a), plan.root), [nodes, plan.root]);
  const target = focus ?? hottest.id;
  const hotPath = useMemo(() => new Set(pathTo(plan.root, target) ?? []), [plan.root, target]);
  const total = plan.total_cost || plan.root.subtree_cost || 1;

  const [expanded, setExpanded] = useState<Set<number>>(() => {
    const open = new Set<number>(hotPath);
    nodes.forEach((n) => {
      if (n.subtree_cost / total >= 0.05) open.add(n.id);
    });
    return open;
  });
  const [selected, setSelected] = useState<number | null>(target);
  const selectedNode = nodes.find((n) => n.id === selected) ?? null;
  const hasActuals = nodes.some((n) => n.act_rows != null);

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
          {nodes.length} operators · cost {total.toFixed(total < 10 ? 3 : 1)}
        </span>
        <span className="spacer" />
        {plan.warnings.length > 0 && (
          <span className="warn-chip">
            <TriangleAlert size={11} />
            {plan.warnings.map((w) => WARNING_LABEL[w] ?? w).join(", ")}
          </span>
        )}
      </div>
      <div className="plan-cols">
        <span>Operator</span>
        <span>{hasActuals ? "Rows est → actual" : "Est. rows"}</span>
        <span>Self cost</span>
      </div>
      <div style={compact ? { maxHeight: 340, overflowY: "auto" } : undefined}>
        {rows.map(({ node, depth }) => {
          const share = node.self_cost / total;
          const mis = misestimate(node);
          return (
            <button
              key={node.id}
              className={`pnode ${selected === node.id ? "sel" : ""} ${hotPath.has(node.id) ? "hot" : ""}`}
              onClick={() => setSelected(node.id)}
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
                    {node.id === hottest.id && share >= 0.05 && <Flame size={13} color="var(--heat-4)" className="flame" />}
                    <span className="opname">{node.op}</span>
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
                    {count(node.est_rows * Math.max(1, node.est_executions))} <span className="arrow">→</span> {count(node.act_rows)}
                    {mis != null && Math.abs(mis) >= 10 && <span className="mis">×{count(Math.abs(mis))}{mis < 0 ? " over" : " under"}</span>}
                  </>
                ) : (
                  count(node.est_rows * Math.max(1, node.est_executions))
                )}
              </span>
              <span className="cost">
                <span className="num">{percent(share)}</span>
                <span className="heat">
                  <span style={{ width: `${Math.max(2, Math.min(100, share * 100))}%`, background: heatVar(share) }} />
                </span>
              </span>
            </button>
          );
        })}
      </div>
      {selectedNode && <NodeProps node={selectedNode} share={selectedNode.self_cost / total} />}
    </div>
  );
}

export function NodeProps({ node, share }: { node: PlanNode; share: number }) {
  const entries: [string, string][] = [];
  if (node.detail) entries.push(["Detail", node.detail]);
  if (node.object) entries.push(["Object", node.object]);
  if (node.index) entries.push(["Index", node.index]);
  if (node.seek_predicate) entries.push(["Seek / index condition", shortPredicate(node.seek_predicate)]);
  if (node.predicate) entries.push(["Filter", shortPredicate(node.predicate)]);
  entries.push(["Estimated rows", count(node.est_rows)]);
  if (node.est_executions > 1) entries.push(["Estimated executions", count(node.est_executions)]);
  if (node.act_rows != null) entries.push(["Actual rows", count(node.act_rows)]);
  if (node.act_executions != null) entries.push(["Actual executions", count(node.act_executions)]);
  if (node.rows_read != null) entries.push(["Rows read", count(node.rows_read)]);
  if (node.table_rows != null) entries.push(["Rows in table", count(node.table_rows)]);
  if (node.act_time_ms != null) entries.push(["Actual time", duration(node.act_time_ms)]);
  entries.push(["Self cost", `${percent(share)} (${node.self_cost.toFixed(4)})`]);
  const noise = (v: string) => v === "0" || v === "false" || v === "False" || v === "";
  Object.entries(node.props).forEach(([k, v]) => {
    if (v != null && !noise(v) && !entries.some(([e]) => e === k)) entries.push([k, v]);
  });
  return (
    <div className="node-props">
      <div className="eyebrow">{node.op}</div>
      <dl>
        {entries.map(([k, v]) => (
          <div key={k} style={{ display: "contents" }}>
            <dt>{k}</dt>
            <dd>{v}</dd>
          </div>
        ))}
      </dl>
    </div>
  );
}
