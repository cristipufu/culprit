import { GitBranch, ListTree, Network, TableProperties } from "lucide-react";
import { useMemo, useState } from "react";
import type { Plan } from "../gen/Plan";
import { allNodes, cost, costBasis, insightsByNode } from "../lib/plan-format";
import { NodeDetails } from "./NodeDetails";
import { PlanDiagram } from "./PlanDiagram";
import { PlanInsights } from "./PlanInsights";
import { PlanOperations } from "./PlanOperations";
import { PlanView } from "./PlanView";

export type PlanLook = "diagram" | "tree" | "operations";

const LOOK_KEY = "culprit.plan.look";

export function rememberedLook(): PlanLook {
  try {
    const v = localStorage.getItem(LOOK_KEY);
    return v === "tree" || v === "operations" ? v : "diagram";
  } catch {
    return "diagram";
  }
}

export function rememberLook(look: PlanLook): void {
  try {
    localStorage.setItem(LOOK_KEY, look);
  } catch {}
}

const LOOKS: { look: PlanLook; label: string; icon: typeof Network; hint: string }[] = [
  { look: "diagram", label: "Diagram", icon: Network, hint: "The plan as a flow of operators" },
  { look: "tree", label: "Tree", icon: ListTree, hint: "Operators as an indented tree" },
  { look: "operations", label: "Operations", icon: TableProperties, hint: "Every operator in a sortable table" },
];

/** Diagram · Tree · Operations. */
export function LookToggle({ look, onChange }: { look: PlanLook; onChange: (look: PlanLook) => void }) {
  return (
    <span className="seg look-toggle" role="tablist" aria-label="Plan view">
      {LOOKS.map((l) => {
        const Icon = l.icon;
        return (
          <button key={l.look} role="tab" aria-selected={look === l.look} className={look === l.look ? "on" : ""} title={l.hint} onClick={() => onChange(l.look)}>
            <Icon size={12} />
            {l.label}
          </button>
        );
      })}
    </span>
  );
}

/**
 * A plan with everything Culprit knows about it: the insights first (what's wrong, how to fix it), then the plan in the
 * chosen view. Picking an insight selects its operator in whichever view is showing, and back.
 */
export function PlanSurface({ plan, focus, look, badge }: { plan: Plan; focus?: number | null; look: PlanLook; badge?: string }) {
  const [selected, setSelected] = useState<number | null>(null);
  return (
    <div className="plan-surface">
      <PlanInsights plan={plan} selectedNode={selected} onSelectNode={setSelected} />
      {look === "diagram" && <PlanDiagram plan={plan} focus={focus} selectedId={selected} onSelect={setSelected} badge={badge} />}
      {look === "tree" && <PlanView plan={plan} focus={focus} selectedId={selected} onSelect={setSelected} />}
      {look === "operations" && <OperationsPanel plan={plan} selected={selected ?? focus ?? null} onSelect={setSelected} />}
    </div>
  );
}

function OperationsPanel({ plan, selected, onSelect }: { plan: Plan; selected: number | null; onSelect: (id: number) => void }) {
  const total = costBasis(plan);
  const nodes = useMemo(() => allNodes(plan.root), [plan]);
  const flagged = useMemo(() => insightsByNode(plan), [plan]);
  const hottest = useMemo(() => nodes.reduce((a, b) => (b.self_cost > a.self_cost ? b : a), plan.root), [nodes, plan.root]);
  const current = nodes.find((n) => n.id === (selected ?? hottest.id)) ?? null;
  return (
    <div className="plan">
      <div className="plan-head">
        <span className="tag">{plan.kind === "actual" ? "Actual plan" : plan.kind === "generic" ? "Generic plan" : "Estimated plan"}</span>
        <span>
          {nodes.length} operator{nodes.length === 1 ? "" : "s"} · cost {cost(plan.total_cost || plan.root.subtree_cost)}
        </span>
        <span className="spacer" />
        <GitBranch size={12} className="muted" />
        <span className="muted">click a row to see the operator</span>
      </div>
      <PlanOperations plan={plan} selectedId={current?.id ?? null} onSelect={onSelect} />
      {current && <NodeDetails node={current} share={current.self_cost / total} subtreeShare={current.subtree_cost / total} insights={flagged.get(current.id)} />}
    </div>
  );
}
