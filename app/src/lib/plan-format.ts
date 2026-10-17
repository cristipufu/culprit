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
 * (under a PostgreSQL Limit the children's costs assume they run to completion). */
export function costBasis(plan: Plan): number {
  const own = allNodes(plan.root).reduce((s, n) => s + n.self_cost, 0);
  return Math.max(plan.total_cost, plan.root.subtree_cost, own) || 1;
}

/** Whether the plan weighs its steps with costs: what "share of the cost" and estimated rows mean on it. Mirrors
 * culprit_core::plan::Plan::weighed. */
export function weighed(plan: Plan): boolean {
  return plan.total_cost > 0 || allNodes(plan.root).some((n) => n.self_cost > 0 || n.subtree_cost > 0);
}

/** Plan prop a collector sets when the rows a scan keeps are the planner's default guess (a parameter compared by range
 * or pattern in a generic plan). Mirrors culprit_core::plan::GUESSED_SELECTIVITY. */
const GUESSED_SELECTIVITY = "Selectivity";

/** Whether a step's rows rest on the planner's default guess for a parameter: its own, or any of its inputs'. */
export function restsOnGuess(n: PlanNode): boolean {
  return n.props[GUESSED_SELECTIVITY] != null || n.children.some(restsOnGuess);
}

/** Steps doing work that carry no cost of their own in a plan that weighs its steps: MySQL and MariaDB put none on a
 * filesort or an internal temporary table (nor on the grouping that uses one). Beside one, a step's share of what the
 * plan costs isn't its share of the statement's work. Mirrors culprit_core::plan::Plan::uncosted_steps. */
export function uncostedSteps(plan: Plan): PlanNode[] {
  if (!weighed(plan)) return [];
  const temporary = (n: PlanNode) => [n.op, n.detail ?? ""].some((t) => t.toLowerCase().includes("temporary"));
  return allNodes(plan.root).filter((n) => n.self_cost <= 0 && (n.kind === "sort" || n.kind === "aggregate" || n.kind === "spool" || temporary(n)));
}

/** A step's share of the plan's cost where the plan can state one, at most 1; null on a plan without costs, for a step
 * with no cost of its own, beside a step doing work the plan puts no cost on (next to MySQL's filesort a scan reads as
 * "100%"), and for a step whose rows rest on a default guess. The one rule behind every share of the cost a person reads,
 * so the plan viewer never shows one the brief withholds. Mirrors culprit_core::plan::Plan::step_share. */
export function stepShare(plan: Plan, n: PlanNode, uncosted: PlanNode[] = uncostedSteps(plan)): number | null {
  if (!weighed(plan) || n.self_cost <= 0 || uncosted.length > 0 || restsOnGuess(n)) return null;
  return Math.min(1, Math.max(0, n.self_cost / costBasis(plan)));
}

/** A step's share of the plan's cost with everything below it, under the same rule as `stepShare` (a subtree without
 * a cost of its own still has its inputs'). */
export function subtreeShare(plan: Plan, n: PlanNode, uncosted: PlanNode[] = uncostedSteps(plan)): number | null {
  if (!weighed(plan) || uncosted.length > 0 || restsOnGuess(n)) return null;
  return Math.min(1, Math.max(0, n.subtree_cost / costBasis(plan)));
}

/** "Sort (using filesort)": a step and what it does. */
function stepName(n: PlanNode): string {
  return n.detail ? `${n.op} (${n.detail})` : n.op;
}

function list(items: string[]): string {
  if (items.length <= 1) return items[0] ?? "";
  return `${items.slice(0, -1).join(", ")} and ${items[items.length - 1]}`;
}

/** Why a plan that weighs its steps states no share of its cost for `n`, as a sentence; null where it states one or on a
 * plan without costs. Mirrors culprit_checks::no_share_reason, so the viewer says what the brief says. */
export function noShareReason(plan: Plan, n: PlanNode): string | null {
  const uncosted = uncostedSteps(plan);
  if (!weighed(plan) || stepShare(plan, n, uncosted) != null) return null;
  if (uncosted.length > 0) {
    const one = uncosted.length === 1;
    return `${list(uncosted.map(stepName))} ${one ? "has" : "have"} no cost of ${one ? "its" : "their"} own in the plan, so no share is given.`;
  }
  if (restsOnGuess(n)) return "The rows it works on rest on the planner's default guess for a parameter, not on the table's statistics, so no share is given.";
  return `${stepName(n)} has no cost of its own in the plan, so no share is given.`;
}

/** The share of its plan's cost a finding's step is said to have: `stepShare`, only in a plan that does real work, as
 * heavy-query says it (a cheap plan's costliest step isn't where its statement's time goes). Mirrors
 * culprit_checks::finding_step_share. */
export function findingStepShare(plan: Plan, n: PlanNode): number | null {
  return doesRealWork(plan) ? stepShare(plan, n) : null;
}

/** Why a finding's step states no share (`findingStepShare`), as a sentence; null where it states one or on a plan
 * without costs. Mirrors culprit_checks::no_share_reason; the plan viewer, which shows every step's share of a cheap
 * plan too, uses `noShareReason`. */
export function findingNoShareReason(plan: Plan, n: PlanNode): string | null {
  if (weighed(plan) && stepShare(plan, n) != null && !doesRealWork(plan)) {
    return "The plan as a whole is cheap, so no share of its cost is put on one step.";
  }
  return noShareReason(plan, n);
}

/** Why steps of a plan that weighs them show no share of its cost, for under its steps; null when each costed step shows
 * one. Mirrors culprit_engine::export::shares_note. */
export function sharesNote(plan: Plan): string | null {
  if (!weighed(plan)) return null;
  if (uncostedSteps(plan).length > 0) return noShareReason(plan, plan.root);
  return allNodes(plan.root).some((n) => n.props[GUESSED_SELECTIVITY] != null)
    ? "No share is given for a step whose rows rest on the planner's default guess for a parameter, nor for the steps above it."
    : null;
}

/** The step with the largest cost of its own: where the plan spends most of its effort. Null on a plan without costs. An
 * exchange (PostgreSQL's Gather and Gather Merge, SQL Server's Parallelism) is never it: its cost is mostly the charge for
 * passing each row between threads, not work on the data. Mirrors culprit_core::plan::Plan::hottest. */
export function hottestStep(plan: Plan): PlanNode | null {
  if (!weighed(plan)) return null;
  let best: PlanNode | null = null;
  // `>=` keeps the last of equal costs, as the core's max_by does.
  for (const n of allNodes(plan.root)) if (n.kind !== "parallelism" && (!best || n.self_cost >= best.self_cost)) best = n;
  return best;
}

/** Checks whose findings are about where a statement's plan does its work, so the plan's working step is worth showing
 * when the finding names none. Mirrors culprit_checks' ABOUT_THE_PLAN (`finding_step`): a finding about a result's size,
 * how often a statement runs or what it waits for points at no step, and a plan insight that names none is about the
 * whole plan (it runs on one CPU, its compile). */
export const ABOUT_THE_PLAN = new Set([
  "heavy-query",
  "regression",
  "plan-regression",
  "parameter-sensitive",
  "plan-warnings",
  "temp-spill",
  "memory-grant",
  "missing-index",
  "filtered-scan",
  "scan-heavy",
  "missing-histogram",
  "tuning-advice",
]);

/** Rows an operator hands up across all its executions. */
export function rowsOut(n: PlanNode): number {
  return n.act_rows ?? n.est_rows * Math.max(1, n.est_executions);
}

export function estRows(n: PlanNode): number {
  return n.est_rows * Math.max(1, n.est_executions);
}

/** How many times the operator ran, counted the way its estimate counts executions: measured when the plan has actual
 * counts (0 when it never ran; a parallel operator that ran once on each thread counts once), else as estimated.
 * Mirrors culprit_core::plan::PlanNode::runs. */
function runs(n: PlanNode): number {
  if (n.act_executions == null) return Math.max(1, n.est_executions);
  if (n.act_executions <= 0) return 0;
  const threads = Math.max(1, Number(n.props.ParallelThreads) || 1);
  return Math.max(1, n.act_executions - (threads - 1));
}

/** The estimate for the executions that happened: the number to set beside `act_rows`, so the inner side of a nested
 * loop estimated right per seek isn't blamed for an outer side that returned more rows. Without actual counts, the same
 * as `estRows`. Mirrors culprit_core::plan::PlanNode::est_rows_for_runs. */
export function estRowsForRuns(n: PlanNode): number {
  return n.act_executions != null ? n.est_rows * runs(n) : estRows(n);
}

/** Rows an operator read over its runs: what the engine reports, else, for a scan that reads its table in full, the
 * table's rows for every run. Never the table's size for a seek or a lookup, nor for a scan of a partition a run may
 * prune. Mirrors culprit_core::plan::PlanNode::rows_read_total. */
export function rowsRead(n: PlanNode): number | null {
  if (n.rows_read != null) return n.rows_read;
  return n.kind === "scan" && n.props["Partition Pruning"] == null && n.table_rows != null ? n.table_rows * runs(n) : null;
}

/** How far off the estimate was for the executions that happened, as a factor ≥ 1, signed: positive means more rows than
 * estimated. Null without actual rows, when the operator never ran, and where the actual count measures nothing
 * (PostgreSQL's BitmapAnd/BitmapOr always report 0; a BRIN bitmap scan reports page ranges' worth of rows). Whether it
 * is a problem is the core's call: show it as one only on an operator with the "misestimate" warning.
 * Mirrors culprit_core::plan::PlanNode::misestimate. */
export function misestimate(n: PlanNode): number | null {
  if (n.act_rows == null) return null;
  const brin = n.op === "Bitmap Index Scan" && n.props["Index Kind"]?.toLowerCase() === "brin";
  if ((n.act_executions != null && n.act_executions <= 0) || n.op === "BitmapAnd" || n.op === "BitmapOr" || brin) return null;
  const est = Math.max(1, estRowsForRuns(n));
  const act = Math.max(1, n.act_rows);
  return act >= est ? act / est : -(est / act);
}

/** Whether the plan does real work: a measured run of at least 10 ms, or, unmeasured, an estimated cost of at least what
 * reading about 10K rows costs in the engine's own units. A point lookup's plan doesn't, and none of its steps is where
 * the statement's time goes, so nothing in it is marked as the hottest. Mirrors culprit_core::insights::does_real_work. */
export function doesRealWork(plan: Plan): boolean {
  const num = (v: string | undefined) => (v != null && v.trim() !== "" && Number.isFinite(Number(v)) ? Number(v) : null);
  const ms = num(plan.props.ElapsedMs) ?? num(plan.props["Execution Time"]) ?? (plan.kind === "actual" ? plan.root.act_time_ms : null);
  if (ms != null) return ms >= 10;
  return Math.max(plan.total_cost, plan.root.subtree_cost) >= cheapBelow(plan);
}

/** The estimated cost of reading about 10K rows, in each engine's units (culprit_core::insights::cheap_below). */
function cheapBelow(plan: Plan): number {
  const mariadb = plan.props.flavor === "MariaDB" || !!plan.props.server_version?.includes("MariaDB");
  switch (plan.engine) {
    // SQLite's plans carry no cost: never real work by an estimate.
    case "sqlite":
      return Infinity;
    case "mssql":
      return 0.1;
    case "postgres":
      return 250;
    // MariaDB before 11 reports no costs and Culprit counts rows examined; MariaDB 11's costs are close to milliseconds.
    case "mysql":
      return mariadb ? (plan.props.cost_basis != null ? 10_000 : 1) : 1_000;
  }
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
