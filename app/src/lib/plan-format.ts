import type { Plan } from "../gen/Plan";
import type { PlanInsight } from "../gen/PlanInsight";
import type { PlanNode } from "../gen/PlanNode";
import type { QueryStat } from "../gen/QueryStat";
import type { Severity } from "../gen/Severity";
import { count, duration } from "./format";

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

/** How a step's cost rests on the planner's default guess for a parameter: "rows" where it or a step below it was planned
 * on the guess, "runs" where a loop runs it once for each row of an input planned on one (the inner side of a nested loop
 * over `= ANY($1)`). Mirrors culprit_core::plan::Plan::guesses. */
export type Guess = "rows" | "runs";

const guessCache = new WeakMap<Plan, Map<number, Guess>>();

/** Every step whose cost rests on a guess, by node id (worked out once per plan). Mirrors
 * culprit_core::plan::Plan::guesses. */
export function guesses(plan: Plan): Map<number, Guess> {
  const cached = guessCache.get(plan);
  if (cached) return cached;
  // Planned on the guess: the step itself or any step below it.
  const planned = new Set<number>();
  const mark = (n: PlanNode): boolean => {
    let any = n.props[GUESSED_SELECTIVITY] != null;
    for (const c of n.children) any = mark(c) || any;
    if (any) planned.add(n.id);
    return any;
  };
  mark(plan.root);
  const out = new Map<number, Guess>();
  const visit = (n: PlanNode, repeated: boolean) => {
    if (planned.has(n.id)) out.set(n.id, "rows");
    else if (repeated) out.set(n.id, "runs");
    const nestedLoop = n.op.toLowerCase().startsWith("nested loop");
    let earlier = false;
    n.children.forEach((c, i) => {
      const relation = c.props["Parent Relationship"];
      const once = relation === "InitPlan" || relation === "CTE";
      const perRow = !once && i > 0 && (nestedLoop || c.est_executions > Math.max(n.est_executions, 1));
      visit(c, repeated || (perRow && earlier));
      earlier = earlier || planned.has(c.id);
    });
  };
  visit(plan.root, false);
  guessCache.set(plan, out);
  return out;
}

/** Whether any step was planned on a default guess for a parameter: then the plan's total, and whether it is cheap, rest
 * on it too. Mirrors culprit_core::plan::Plan::has_guess. */
export function hasGuess(plan: Plan): boolean {
  return allNodes(plan.root).some((n) => n.props[GUESSED_SELECTIVITY] != null);
}

/** Whether the plan states any step's share of its cost: it weighs its steps, does real work (half of a point lookup's
 * cost isn't where its statement's time goes) and costs every step doing work. Mirrors
 * culprit_core::plan::Plan::states_shares. */
export function statesShares(plan: Plan, uncosted: PlanNode[] = uncostedSteps(plan)): boolean {
  return weighed(plan) && doesRealWork(plan) && uncosted.length === 0;
}

/** Steps doing work that carry no cost of their own in a plan that weighs its steps: MySQL and MariaDB put none on a
 * filesort or an internal temporary table (nor on the grouping that uses one). Beside one, a step's share of what the
 * plan costs isn't its share of the statement's work. Only theirs: SQL Server and PostgreSQL cost every step, so a step
 * whose own cost works out at nothing there costs too little to matter. Mirrors
 * culprit_core::plan::Plan::uncosted_steps. */
export function uncostedSteps(plan: Plan): PlanNode[] {
  if (!weighed(plan) || plan.engine !== "mysql") return [];
  const temporary = (n: PlanNode) => [n.op, n.detail ?? ""].some((t) => t.toLowerCase().includes("temporary"));
  return allNodes(plan.root).filter((n) => n.self_cost <= 0 && (n.kind === "sort" || n.kind === "aggregate" || n.kind === "spool" || temporary(n)));
}

/** A step's share of the plan's cost where the plan can state one, at most 1; null where the plan states no shares (no
 * costs, a cheap plan, a step doing work it puts no cost on: next to MySQL's filesort a scan reads as "100%"), for a step
 * with no cost of its own, and for a step whose cost rests on a default guess. The one rule behind every share of the
 * cost a person reads, so the plan viewer never shows one the brief withholds. Mirrors
 * culprit_core::plan::Plan::step_share. */
export function stepShare(plan: Plan, n: PlanNode, uncosted: PlanNode[] = uncostedSteps(plan)): number | null {
  if (!statesShares(plan, uncosted) || n.self_cost <= 0 || guesses(plan).has(n.id)) return null;
  return Math.min(1, Math.max(0, n.self_cost / costBasis(plan)));
}

/** A step's share of the plan's cost with everything below it, under the same rule as `stepShare` (a subtree without
 * a cost of its own still has its inputs'). */
export function subtreeShare(plan: Plan, n: PlanNode, uncosted: PlanNode[] = uncostedSteps(plan)): number | null {
  if (!statesShares(plan, uncosted) || guesses(plan).has(n.id)) return null;
  return Math.min(1, Math.max(0, n.subtree_cost / costBasis(plan)));
}

/** The share of the plan's work an insight states: its impact where that is a share (not a rule's own weight), the plan
 * states shares and none of its steps' cost rests on a guess. Mirrors culprit_core::insights::stated_share. */
export function statedInsightShare(plan: Plan, i: PlanInsight): number | null {
  const guessed = guesses(plan);
  if (i.fixed_weight || !statesShares(plan) || i.nodes.some((id) => guessed.has(id))) return null;
  return i.impact;
}

/** "Sort (using filesort)": a step and what it does. */
function stepName(n: PlanNode): string {
  return n.detail ? `${n.op} (${n.detail})` : n.op;
}

function list(items: string[]): string {
  if (items.length <= 1) return items[0] ?? "";
  return `${items.slice(0, -1).join(", ")} and ${items[items.length - 1]}`;
}

const GUESSED_ROWS = "The rows it works on rest on the planner's default guess for a parameter, not on the table's statistics, so no share is given.";
const GUESSED_RUNS = "It runs once for each row of a step whose rows rest on the planner's default guess for a parameter, so how often it runs is that guess, and no share is given.";

/** Why a plan that weighs its steps states no share of its cost for `n`, as a sentence; null where it states one or on a
 * plan without costs. The true reason, with what the statement `query`'s runs measured where it is given: a misestimate
 * isn't "cheap", and nor is a scan of a whole table big enough to matter (`wholeReadMatters`). Mirrors
 * culprit_checks::no_share_reason, so the viewer says what the
 * brief says. */
export function noShareReason(plan: Plan, n: PlanNode, query?: QueryStat | null): string | null {
  const uncosted = uncostedSteps(plan);
  if (!weighed(plan) || stepShare(plan, n, uncosted) != null) return null;
  if (uncosted.length > 0) {
    const one = uncosted.length === 1;
    return `${list(uncosted.map(stepName))} ${one ? "has" : "have"} no cost of ${one ? "its" : "their"} own in the plan, so no share is given.`;
  }
  const guess = guesses(plan).get(n.id);
  if (guess === "rows") return GUESSED_ROWS;
  if (guess === "runs") return GUESSED_RUNS;
  if (!doesRealWork(plan)) return cheapPlanReason(plan, query);
  return `${stepName(n)} has no cost of its own in the plan, so no share is given.`;
}

/** The share of its plan's cost a finding's step is said to have: `stepShare`, which states one only in a plan that
 * does real work, as heavy-query says it. Mirrors culprit_checks::finding_step_share. */
export function findingStepShare(plan: Plan, n: PlanNode): number | null {
  return stepShare(plan, n);
}

/** Why a finding's step states no share (`findingStepShare`), with what its statement's runs measured: `noShareReason`,
 * except that a step the finding is about which reads its table whole on a cheap plan is said as that read, whatever
 * the table's size (the finding shows it matters), unless its runs measured a misestimate. Mirrors
 * culprit_checks::finding_no_share_reason. */
export function findingNoShareReason(plan: Plan, n: PlanNode, query?: QueryStat | null): string | null {
  const why = noShareReason(plan, n, query);
  if (why == null) return null;
  const cheap = uncostedSteps(plan).length === 0 && !hasGuess(plan) && !doesRealWork(plan);
  return cheap && !misestimateReason(plan, query) && readsTableWhole(n) ? wholeReadReason(plan, n) : why;
}

/** "…too small for a share…, but `Seq Scan` reads all 3.9K rows of public.apps on every run." */
function wholeReadReason(plan: Plan, s: PlanNode): string {
  const what = s.object ?? "its table";
  const all = s.table_rows != null && s.table_rows > 0 ? `all ${rowCount(s.table_rows)} of ${what}` : `all of ${what}`;
  return `Its estimated cost (${costText(Math.max(plan.total_cost, plan.root.subtree_cost))}) is too small for a share of it to say where the time goes, but \`${s.op}\` reads ${all} on every run.`;
}

/** The misestimate a statement's runs measured against a cheap plan, as the sentence that says it; null where they
 * measured none (or weren't measured). Mirrors the MoreRows/MoreReads arms of culprit_core::insights::cheap_plan_runs. */
function misestimateReason(plan: Plan, query?: QueryStat | null): string | null {
  const runs = query && query.executions > 0 ? query.executions : null;
  const writes = query ? /^[\s(]*(insert|update|delete|merge|replace)/i.test(withoutParameterList(query.text)) : false;
  const handled = query && runs ? (writes ? (query.rows_affected ?? query.rows) : query.rows) : null;
  const reads = query?.logical_reads != null && runs ? query.logical_reads / runs : null;
  const rows = handled != null && runs ? handled / runs : null;
  if (reads == null || rows == null) return null;
  const root = plan.root;
  const fed = root.kind === "modify" ? (root.children[0]?.est_rows ?? 0) : 0;
  const expected = Math.max(root.est_rows, fed, 1);
  if (rows >= expected * 10 && rows >= 100) {
    return `Its costs rest on an estimate of ${rowCount(expected)} a run, and its runs ${writes ? "change" : "return"} far more (${rowCount(rows)} a run), so no share of them is put on one step.`;
  }
  if (reads > LOOKUP_PAGES_PER_ROW * Math.max(rows, 1)) {
    const unit = plan.engine === "mysql" ? "rows examined" : "pages";
    return `Its costs rest on an estimate of ${rowCount(expected)} a run, and its runs read far more than that needs (${count(Math.round(reads))} ${unit} a run), so no share of them is put on one step.`;
  }
  return null;
}

/** Why steps of a plan that weighs them show no share of its cost, for under its steps; null when each costed step shows
 * one. Mirrors culprit_checks::plan_shares_note (without the statement: what the plan itself shows). */
export function sharesNote(plan: Plan, query?: QueryStat | null): string | null {
  if (!weighed(plan)) return null;
  if (uncostedSteps(plan).length > 0) return noShareReason(plan, plan.root, query);
  if (!doesRealWork(plan)) return cheapPlanReason(plan, query);
  const found = [...guesses(plan).values()];
  if (found.length === 0) return null;
  return found.includes("runs")
    ? "No share is given for a step whose rows rest on the planner's default guess for a parameter, nor for the steps above it or run once for each of its rows."
    : "No share is given for a step whose rows rest on the planner's default guess for a parameter, nor for the steps above it.";
}

/** Pages a run may read for each row it handles and still be finding rows through indexes. Mirrors
 * culprit_core::insights::LOOKUP_PAGES_PER_ROW. */
const LOOKUP_PAGES_PER_ROW = 32;

/** Whether a step reads its whole table on each run: a scan of a table, unless it may never be read (a partition a run
 * prunes) or the plan's own estimate of its rows read is under half the table's (a scan under a TOP that stops early).
 * Mirrors culprit_core::plan::PlanNode::reads_table_whole. */
function readsTableWhole(n: PlanNode): boolean {
  if (n.kind !== "scan" || !n.object || n.props["Partition Pruning"] != null) return false;
  if (n.rows_read != null && n.table_rows != null && n.table_rows > 0) return n.rows_read >= 0.5 * n.table_rows * runs(n);
  return true;
}

/** Tables of this many rows or more: reading one in full costs something. Mirrors culprit_core::plan::SCAN_ROWS. */
const SCAN_ROWS = 10_000;

/** A name part without its quotes or brackets (`[sys]`, `"pg_catalog"`). Mirrors culprit_core::insights'
 * clean_identifier. */
function cleanIdentifier(raw: string): string {
  const t = raw.trim();
  const pairs: [string, string][] = [["[", "]"], ["`", "`"], ['"', '"']];
  for (const [open, close] of pairs) {
    if (t.length >= 2 && t.startsWith(open) && t.endsWith(close)) return t.slice(1, -1).split(close + close).join(close);
  }
  return t.replace(/^[[\]`"]+|[[\]`"]+$/g, "");
}

const SYSTEM_SCHEMAS = new Set(["sys", "information_schema", "pg_catalog", "pg_toast", "performance_schema", "mysql"]);

/** Whether a plan's object is the engine's own, its catalog and statistics. Mirrors
 * culprit_core::insights::is_system_object. */
function isSystemObject(object: string): boolean {
  const parts = object.split(".").map((p) => cleanIdentifier(p).toLowerCase());
  const schema = parts.length >= 2 ? parts[parts.length - 2] : null;
  return (schema != null && SYSTEM_SCHEMAS.has(schema)) || parts[0] === "mssqlsystemresource" || (parts[parts.length - 1] ?? "").startsWith("sqlite_");
}

/** Whether a SQL Server plan names a schema only for the engine's own objects (its internal functions carry none).
 * Mirrors culprit_core::insights' reads_only_the_catalog. */
function readsOnlyTheCatalog(plan: Plan): boolean {
  if (plan.engine !== "mssql") return false;
  const qualified = allNodes(plan.root).flatMap((n) => (n.object && n.object.includes(".") ? [n.object] : []));
  return qualified.length > 0 && qualified.every(isSystemObject);
}

/** Whether a step reads a whole table in a way that can weigh on a run: it reads it whole, the table has SCAN_ROWS rows
 * or more or the step reads that many over its runs, and it isn't the engine's own. All 2 rows of pg_database are no
 * reason a plan's runs cost what they do. Mirrors culprit_core::insights::whole_read_matters. */
function wholeReadMatters(plan: Plan, n: PlanNode): boolean {
  if (!readsTableWhole(n)) return false;
  const object = n.object ?? "";
  if (isSystemObject(object) || (!object.includes(".") && readsOnlyTheCatalog(plan))) return false;
  const read = rowsRead(n);
  return (n.table_rows != null && n.table_rows >= SCAN_ROWS) || (read != null && read >= SCAN_ROWS);
}

/** A statement without SQL Server's parameter declarations in front (`(@P1 int)UPDATE …`). Mirrors
 * culprit_core::sql::without_parameter_list. */
function withoutParameterList(text: string): string {
  const t = text.trimStart();
  if (!t.startsWith("(") || !t.slice(1).trimStart().startsWith("@")) return t;
  let depth = 1;
  for (let i = 1; i < t.length; i++) {
    if (t[i] === "(") depth++;
    else if (t[i] === ")" && --depth === 0) return t.slice(i + 1);
  }
  return t;
}

/** "1 row", "3.9K rows". */
function rowCount(n: number): string {
  const c = count(n);
  return `${c} row${c === "1" ? "" : "s"}`;
}

/** A plan's estimated cost as the engine writes it: whole units from 10, else three significant digits. Mirrors
 * culprit_checks' cost_text. */
function costText(cost: number): string {
  if (cost >= 10) return cost.toFixed(0);
  const digits = Math.min(6, Math.max(0, 2 - Math.floor(Math.log10(Math.max(cost, 1e-9)))));
  return cost.toFixed(digits);
}

/** Why a plan that does no real work puts a share of its cost on no step, as its statement's runs measured it. Mirrors
 * culprit_checks' cheap_plan_reason over culprit_core::insights::cheap_plan_runs. */
function cheapPlanReason(plan: Plan, query?: QueryStat | null): string {
  if (hasGuess(plan)) return "The plan's costs rest on the planner's default guess for a parameter, so no share of them is put on one step.";
  const cheap = "The plan as a whole is cheap, so no share of its cost is put on one step.";
  let scan: PlanNode | null = null;
  // A whole read of a small table or of the catalog is no reason: the runs are read as if it weren't there.
  // `>=` keeps the last of equal costs, as the core's max_by does.
  for (const n of allNodes(plan.root)) if (wholeReadMatters(plan, n) && (!scan || n.self_cost >= scan.self_cost)) scan = n;
  const runs = query && query.executions > 0 ? query.executions : null;
  const writes = query ? /^[\s(]*(insert|update|delete|merge|replace)/i.test(withoutParameterList(query.text)) : false;
  const handled = query && runs ? (writes ? (query.rows_affected ?? query.rows) : query.rows) : null;
  const reads = query?.logical_reads != null && runs ? query.logical_reads / runs : null;
  const rows = handled != null && runs ? handled / runs : null;
  if (!query || reads == null || rows == null) return scan ? wholeReadReason(plan, scan) : cheap;
  const misestimate = misestimateReason(plan, query);
  if (misestimate) return misestimate;
  if (scan) return wholeReadReason(plan, scan);
  if (query.mean_time_ms >= 10) {
    // What a run or two took, where an average would hide it (culprit_core::model::QueryStat::few_runs).
    const longest = query.max_time_ms != null ? Math.min(Math.max(query.max_time_ms, 0), query.total_time_ms) : null;
    const few =
      query.executions === 1
        ? `its one run took ${duration(query.total_time_ms)}`
        : query.executions === 2 && longest != null && query.total_time_ms > 0 && longest >= (query.total_time_ms - longest) * 2
          ? `its runs took ${duration(longest)} and ${duration(query.total_time_ms - longest)}`
          : `its runs take ${duration(query.mean_time_ms)} on average`;
    return `The plan is estimated to be cheap, yet ${few}: the time isn't in what it costs, so no share of its cost is put on one step.`;
  }
  return cheap;
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
