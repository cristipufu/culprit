import type { Plan } from "../gen/Plan";
import type { PlanInsight } from "../gen/PlanInsight";
import type { PlanNode } from "../gen/PlanNode";
import type { PlanOperatorView } from "../gen/PlanOperatorView";
import type { PlanView } from "../gen/PlanView";
import type { QueryStat } from "../gen/QueryStat";
import type { Severity } from "../gen/Severity";
import type { Guess } from "../gen/Guess";
export type { Guess } from "../gen/Guess";
import { count } from "./format";

/** Tree walking and presentation only: analysis decisions arrive in Rust's PlanView. */
export function allNodes(n: PlanNode, out: PlanNode[] = []): PlanNode[] {
  out.push(n);
  n.children.forEach((c) => allNodes(c, out));
  return out;
}

const nodesCache = new WeakMap<Plan, { source: PlanView; nodes: Map<number, PlanOperatorView> }>();
export function operatorView(plan: Plan | null | undefined, node: PlanNode): PlanOperatorView | null {
  if (!plan?.view) return null;
  let cached = nodesCache.get(plan);
  if (!cached || cached.source !== plan.view) {
    cached = { source: plan.view, nodes: new Map(plan.view.nodes.map((n) => [n.id, n])) };
    nodesCache.set(plan, cached);
  }
  return cached.nodes.get(node.id) ?? null;
}

export const costBasis = (plan: Plan): number => plan.view?.cost_basis ?? 1;
export const weighed = (plan: Plan): boolean => plan.view?.weighed ?? false;
export const statesShares = (plan: Plan): boolean => plan.view?.states_shares ?? false;
export const guesses = (plan: Plan): Map<number, Guess> => new Map((plan.view?.nodes ?? []).flatMap((n) => n.guess ? [[n.id, n.guess] as const] : []));
export const hasGuess = (plan: Plan): boolean => (plan.view?.nodes ?? []).some((n) => n.guess != null);
export const uncostedSteps = (plan: Plan): PlanNode[] => allNodes(plan.root).filter((n) => operatorView(plan, n)?.uncosted);
export const stepShare = (plan: Plan, node: PlanNode): number | null => operatorView(plan, node)?.cost_share ?? null;
export const subtreeShare = (plan: Plan, node: PlanNode): number | null => operatorView(plan, node)?.subtree_cost_share ?? null;

export function statedInsightShare(plan: Plan, insight: PlanInsight): number | null {
  return plan.view?.insights.find((i) => i.rule === insight.rule && i.nodes.length === insight.nodes.length && i.nodes.every((id, at) => id === insight.nodes[at]))?.share ?? null;
}

const MISSING_VIEW = "Reopen this plan to compute its measurements and explanations with the current analyzer.";
const OTHER_MEASUREMENTS = "This plan view was prepared for different statement or plan measurements. Reopen the selected plan to refresh its explanation.";
const matchesQuery = (plan: Plan, query?: QueryStat | null): boolean =>
  !query || !plan.view?.query || (plan.view.query.query_id === query.id && (plan.view.query.plan_id == null || query.plan_id == null || plan.view.query.plan_id === query.plan_id));

export function noShareReason(plan: Plan, node: PlanNode, query?: QueryStat | null): string | null {
  if (!plan.view) return MISSING_VIEW;
  return matchesQuery(plan, query) ? operatorView(plan, node)?.no_share_reason ?? null : OTHER_MEASUREMENTS;
}
export const findingStepShare = stepShare;
export function findingNoShareReason(plan: Plan, node: PlanNode, query?: QueryStat | null): string | null {
  if (!plan.view) return MISSING_VIEW;
  return matchesQuery(plan, query) ? operatorView(plan, node)?.finding_no_share_reason ?? null : OTHER_MEASUREMENTS;
}
export function sharesNote(plan: Plan, query?: QueryStat | null): string | null {
  if (!plan.view) return MISSING_VIEW;
  return matchesQuery(plan, query) ? plan.view.cost_note : OTHER_MEASUREMENTS;
}
export const hottestStep = (plan: Plan): PlanNode | null => allNodes(plan.root).find((n) => n.id === plan.view?.hottest_cost) ?? null;
export const doesRealWork = (plan: Plan): boolean => plan.view?.real_work ?? false;
export const aboutThePlan = (plan: Plan, check: string): boolean => plan.view?.focus_checks.includes(check) ?? false;

/** Literal counts can still be inspected in an older, unprojected plan; inferred comparisons require Rust's view. */
export const estRows = (plan: Plan | null | undefined, node: PlanNode): number | null => {
  const view = operatorView(plan, node);
  return view ? view.estimated_rows : node.facts?.estimated_rows_unavailable ? null : node.est_rows * Math.max(1, node.est_executions);
};
export const rowsOut = (plan: Plan | null | undefined, node: PlanNode): number | null => {
  const view = operatorView(plan, node);
  return view ? view.rows_out : node.act_rows ?? estRows(plan, node);
};
export const estRowsForRuns = (plan: Plan | null | undefined, node: PlanNode): number | null => {
  const view = operatorView(plan, node);
  return view ? view.estimated_rows_for_runs : node.act_executions == null ? estRows(plan, node) : null;
};
export const operatorRuns = (plan: Plan | null | undefined, node: PlanNode): number | null => {
  const view = operatorView(plan, node);
  return view ? view.runs : node.facts?.estimated_rows_unavailable ? node.act_executions : node.act_executions == null ? Math.max(1, node.est_executions) : null;
};
export const rowsRead = (plan: Plan, node: PlanNode): number | null => operatorView(plan, node)?.rows_read ?? node.rows_read;
export const misestimate = (plan: Plan | null | undefined, node: PlanNode): number | null => operatorView(plan, node)?.misestimate ?? null;

/** Grouped integers ("1,000,000"); small fractions keep one decimal. Never exponent notation. */
export function exact(value: number | null): string {
  if (value == null) return "—";
  if (!Number.isFinite(value)) return "—";
  if (Math.abs(value) >= 100 || Number.isInteger(value)) return Math.round(value).toLocaleString("en-US");
  return value.toLocaleString("en-US", { maximumFractionDigits: 2 });
}

/** Optimizer cost to three significant figures; it's a unit-less estimate, not time. */
export function cost(value: number): string {
  if (value === 0) return "0";
  if (Math.abs(value) >= 1000) return count(value);
  return Number(value.toPrecision(3)).toLocaleString("en-US");
}

/** Rows as people say them: 20K, 1.2M, 14, 4.7. */
export function rows(value: number | null): string {
  if (value == null) return "—";
  return value >= 10 ? count(Math.round(value)) : count(Math.round(value * 10) / 10);
}

/** A raw engine property made readable: numbers grouped, booleans as words, long lists trimmed by the UI. */
export function propValue(value: string): string {
  const trimmed = value.trim();
  if (/^-?\d+(\.\d+)?(e[+-]?\d+)?$/i.test(trimmed)) {
    const n = Number(trimmed);
    if (Number.isFinite(n)) return Math.abs(n) >= 1e4 || Number.isInteger(n) ? exact(n) : Number(n.toPrecision(4)).toLocaleString("en-US");
  }
  if (trimmed === "1" || trimmed.toLowerCase() === "true") return "yes";
  if (trimmed.toLowerCase() === "false") return "no";
  return value;
}

const RANK: Record<Severity, number> = { high: 3, medium: 2, low: 1 };

export function bySeverity(a: PlanInsight, b: PlanInsight): number {
  return RANK[b.severity] - RANK[a.severity] || b.impact - a.impact;
}

/** Insights keyed by every operator they mention, most severe first. */
export function insightsByNode(plan: Plan): Map<number, PlanInsight[]> {
  const map = new Map<number, PlanInsight[]>();
  (plan.insights?.items ?? []).forEach((i) =>
    i.nodes.forEach((id) => {
      const list = map.get(id) ?? [];
      list.push(i);
      list.sort(bySeverity);
      map.set(id, list);
    }),
  );
  return map;
}

/** Operators that only pass rows through (MySQL's query block, a 0% Compute Scalar, a Gather with one child) and can be
 * drawn as a label on the edge instead of a card. */
export function isWrapper(n: PlanNode, total: number): boolean {
  if (n.children.length !== 1 || n.warnings.length > 0) return false;
  const free = n.self_cost / total < 0.001;
  const op = n.op.toLowerCase();
  return free && (op === "query block" || op === "compute scalar" || op === "query" || op === "result" || op === "stream results");
}
