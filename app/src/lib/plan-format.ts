import type { Plan } from "../gen/Plan";
import type { PlanInsight } from "../gen/PlanInsight";
import type { PlanNode } from "../gen/PlanNode";
import type { Severity } from "../gen/Severity";
import { count } from "./format";

/** Every operator in tree order. */
export function allNodes(n: PlanNode, out: PlanNode[] = []): PlanNode[] {
  out.push(n);
  n.children.forEach((c) => allNodes(c, out));
  return out;
}

/** What operator shares are measured against: the plan's cost, or the sum of operator costs when that is larger
 * (under a Postgres Limit the children's costs assume they run to completion). */
export function costBasis(plan: Plan): number {
  const own = allNodes(plan.root).reduce((s, n) => s + n.self_cost, 0);
  return Math.max(plan.total_cost, plan.root.subtree_cost, own) || 1;
}

/** Rows an operator hands up across all its executions. */
export function rowsOut(n: PlanNode): number {
  return n.act_rows ?? n.est_rows * Math.max(1, n.est_executions);
}

export function estRows(n: PlanNode): number {
  return n.est_rows * Math.max(1, n.est_executions);
}

/** Actual vs estimated rows as a factor ≥ 1 with a direction, when the plan has actuals. */
export function rowError(n: PlanNode): { factor: number; more: boolean } | null {
  if (n.act_rows == null) return null;
  const est = Math.max(1, estRows(n));
  const act = Math.max(1, n.act_rows);
  return act >= est ? { factor: act / est, more: true } : { factor: est / act, more: false };
}

/** Grouped integers ("1,000,000"); small fractions keep one decimal. Never exponent notation. */
export function exact(value: number): string {
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
export function rows(value: number): string {
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
