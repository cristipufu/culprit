import { GitBranch, ListTree, Network, TableProperties } from "lucide-react";
import { useMemo, useState } from "react";
import type { Plan } from "../gen/Plan";
import type { QueryStat } from "../gen/QueryStat";
import { planShares } from "../lib/actual";
import { can } from "../lib/engines";
import { allNodes, cost, findingNoShareReason, insightsByNode, noShareReason } from "../lib/plan-format";
import { NodeDetails } from "./NodeDetails";
import { DocdbWork } from "./DocdbWork";
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
export function PlanSurface({ plan, query, focus, look, badge }: { plan: Plan; query?: QueryStat | null; focus?: number | null; look: PlanLook; badge?: string }) {
  const [selected, setSelected] = useState<number | null>(null);
  return (
    <div className="plan-surface">
      {!plan.insights?.note && plan.kind !== "actual" && (plan.props["Generic Plan"] || plan.kind === "generic") && <p className="notice">This plan was obtained without the application's parameter values.{plan.engine === "sqlite" ? " It uses Culprit's SQLite library, which may differ from the application's version and build." : " Connection settings and runtime partition pruning may differ from the application's run."} It does not identify the plan used by historical executions.</p>}
      <PlanInsights plan={plan} selectedNode={selected} onSelectNode={setSelected} />
      <DocdbWork metrics={plan.facts?.docdb} basis="execution" />
      {look === "diagram" && <PlanDiagram plan={plan} query={query} focus={focus} selectedId={selected} onSelect={setSelected} badge={badge} />}
      {look === "tree" && <PlanView plan={plan} query={query} focus={focus} selectedId={selected} onSelect={setSelected} />}
      {look === "operations" && <OperationsPanel plan={plan} query={query} focus={focus} selected={selected ?? focus ?? null} onSelect={setSelected} />}
    </div>
  );
}

function OperationsPanel({ plan, query, focus, selected, onSelect }: { plan: Plan; query?: QueryStat | null; focus?: number | null; selected: number | null; onSelect: (id: number) => void }) {
  // The same shares as the diagram and the tree: own time in a timed run, the estimated cost otherwise.
  const shares = useMemo(() => planShares(plan, query), [plan, query]);
  const nodes = useMemo(() => allNodes(plan.root), [plan]);
  const flagged = useMemo(() => insightsByNode(plan), [plan]);
  // The share a person reads: none where the plan can't state one.
  const shareOf = (id: number) => shares.stated.get(id) ?? null;
  // Open on the step where the work is (the core's pick, never an exchange), in a plan that does real work and where its
  // share can be stated; otherwise on the plan's root.
  const start = shares.hottest ?? plan.root;
  const current = nodes.find((n) => n.id === (selected ?? start.id)) ?? null;
  return (
    <div className="plan">
      <div className="plan-head">
        <span className="tag">{plan.kind === "actual" ? "Actual plan" : plan.kind === "generic" ? "Generic plan" : "Estimated plan"}</span>
        <span>
          {nodes.length} operator{nodes.length === 1 ? "" : "s"}
          {can(plan.engine, "plan_estimates") && ` · cost ${cost(plan.total_cost || plan.root.subtree_cost)}`}
        </span>
        <span className="spacer" />
        <GitBranch size={12} className="muted" />
        <span className="muted">click a row to see the operator</span>
      </div>
      <PlanOperations plan={plan} query={query} focus={focus} selectedId={current?.id ?? null} onSelect={onSelect} />
      {current && (
        <NodeDetails
          plan={plan}
          estimates={can(plan.engine, "plan_estimates")}
          actual={plan.kind === "actual"}
          node={current}
          share={shareOf(current.id)}
          subtreeShare={shares.statedSubtree.get(current.id) ?? null}
          noShare={shares.basis === "cost" ? (current.id === focus ? findingNoShareReason(plan, current, query) : noShareReason(plan, current, query)) : null}
          insights={flagged.get(current.id)}
        />
      )}
    </div>
  );
}
