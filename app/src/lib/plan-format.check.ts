// The same evidence cases as the core's cheap-plan tests: a plan's own measured runs, large values, outliers and a
// finding on a small whole-table read. Run with typecheck so changes to the viewer keep these explanations honest.
import fixture from "../../../fixtures/mssql-demo.json";
import type { QueryStat } from "../gen/QueryStat";
import { findingNoShareReason, noShareReason, sharesNote } from "./plan-format";

function check(ok: boolean, message: string): void {
  if (!ok) throw new Error(message);
}

const query = structuredClone(fixture.workload.queries[0]) as unknown as QueryStat;
const plan = query.plan!;
plan.root.children = [];
plan.root.kind = "seek";
plan.root.object = null;
plan.root.props = {};
plan.root.est_rows = 1;
plan.root.self_cost = plan.root.subtree_cost = plan.total_cost = 0.001;
query.plans = [];
query.executions = query.rows = 100;
query.logical_reads = 7200;
query.mean_time_ms = 1;
query.total_time_ms = 100;
let note = sharesNote(plan, query)!;
check(note.includes("72 pages a run") && !note.includes("costs rest on an estimate of 1 row"), note);

const variant = (id: string, mean: number, reads: number): QueryStat["plans"][number] => ({
  id, executions: 10, mean_time_ms: mean, total_time_ms: mean * 10, logical_reads: reads * 10,
  first_seen: null, last_seen: null, is_forced: false, min_time_ms: null, max_time_ms: null,
  stddev_time_ms: null, compiled: null, variant_query: null, cpu_time_ms: null,
});
query.plans = [variant("fast", 1, 2), variant("slow", 20, 322)];
query.plan_id = "slow";
note = sharesNote(plan, query)!;
check(note.includes("322 pages a run"), note);
query.plan_id = "fast";
note = sharesNote(plan, query)!;
check(!note.includes("322 pages") && !note.includes("20 ms"), note);
query.plans[1]!.logical_reads = 20;
query.plan_id = "slow";
note = sharesNote(plan, query)!;
check(note.includes("averaged 20 ms"), note);

query.plans = [];
query.plan_id = null;
query.executions = query.rows = query.logical_reads = 4;
query.total_time_ms = 2404;
query.mean_time_ms = 601;
query.max_time_ms = 2400;
note = sharesNote(plan, query)!;
check(note.includes("4 runs took 2.4 s in all, with the slowest taking 2.4 s") && !note.includes("601 ms"), note);

query.mean_time_ms = 1;
plan.root.kind = "scan";
plan.root.object = "shop.widgets";
plan.root.table_rows = plan.root.rows_read = 3900;
check(noShareReason(plan, plan.root, query)!.includes("as a whole is cheap"), "the general size bar still holds");
note = findingNoShareReason(plan, plan.root, query)!;
check(note.includes("all 3.9K rows of shop.widgets"), note);
console.log("Plan explanations keep their measurement basis and finding context");
