import type { Plan } from "../gen/Plan";
import type { PlanNode } from "../gen/PlanNode";
import type { QueryStat } from "../gen/QueryStat";
import { allNodes, sharesNote } from "./plan-format";

export type Shares = {
  basis: "time" | "cost";
  /** Own share of the whole, 0..1, by node id: for laying the plan out (what to fold, what to open), never to show. */
  own: Map<number, number>;
  /** Share including everything underneath, 0..1, by node id (used to fold cheap subtrees). */
  subtree: Map<number, number>;
  /** The own share a person reads, by node id: the measured time's, or the cost's where the plan can state one
   * (`stepShare`); null where it can't (a step it puts no cost on beside it, rows that rest on a default guess). */
  stated: Map<number, number | null>;
  /** The share with everything underneath a person reads, under the same rule. */
  statedSubtree: Map<number, number | null>;
  /** Why some steps show no share, for under the plan; null when each step that has a cost shows one. */
  note: string | null;
  /** The step where the work is, as the core picks it (never an exchange such as Gather Merge), in a plan that does real
   * work and only where its share can be stated: what the flame marks and the views open on. */
  hottest: PlanNode | null;
  /** Own milliseconds by node id, for time-based shares. */
  ms: Map<number, number>;
  total: number;
};

/** Convert Rust's projection into maps used by the layouts. No engine rules or measurement inference run here. */
export function planShares(plan: Plan, query?: QueryStat | null): Shares {
  const view = plan.view;
  const timed = plan.kind === "actual" && view?.kind === "actual" && view.timed_total_ms != null;
  const entries = view?.nodes ?? [];
  const nodes = allNodes(plan.root);
  const own = new Map(entries.map((n) => [n.id, timed ? n.time_share ?? 0 : n.layout_own_share]));
  const subtree = new Map(entries.map((n) => [n.id, timed ? n.subtree_time_share ?? 0 : n.layout_subtree_share]));
  const stated = new Map(entries.map((n) => [n.id, timed ? n.time_share : n.cost_share]));
  const statedSubtree = new Map(entries.map((n) => [n.id, timed ? n.subtree_time_share : n.subtree_cost_share]));
  const hot = timed ? view?.hottest_time : view?.hottest_cost;
  return {
    basis: timed ? "time" : "cost", own, subtree, stated, statedSubtree,
    hottest: nodes.find((n) => n.id === hot) ?? null,
    ms: new Map(entries.flatMap((n) => timed && n.own_ms != null ? [[n.id, n.own_ms] as const] : [])),
    note: timed ? view?.time_note ?? null : sharesNote(plan, query),
    total: (timed ? view?.timed_total_ms : view?.cost_basis) ?? 1,
  };
}

export type ActualFacts = {
  triggers: { name: string; relation: string | null; ms: number; calls: number | null }[];
  elapsedMs: number | null;
  cpuMs: number | null;
  logicalReads: number | null;
  physicalReads: number | null;
  rows: number | null;
  grantedKb: number | null;
  usedKb: number | null;
  spilled: boolean;
  spillPages: number;
  /** Per-operator time was kept (Analyze, live); false for SQL Server's last actual plan, which keeps rows only. */
  timed: boolean;
  slowest: { node: PlanNode; ms: number; share: number } | null;
  worstEstimate: { node: PlanNode; factor: number } | null;
  skew: { node: PlanNode; busiest: number; threads: number } | null;
  lastRunAt: string | null;
  workers: number | null;
};

/** The same actual-run facts the CLI and MCP consume, attached by the current Rust analyzer. */
export function actualFacts(plan: Plan): ActualFacts | null {
  const actual = plan.kind === "actual" && plan.view?.kind === "actual" ? plan.view.actual : null;
  if (!actual) return null;
  const nodes = allNodes(plan.root);
  const node = (id: number | null) => nodes.find((n) => n.id === id) ?? null;
  const slowest = node(actual.slowest_node);
  const worst = node(actual.worst_estimate_node);
  const skew = actual.skew ? node(actual.skew.node) : null;
  const share = plan.view?.nodes.find((n) => n.id === actual.slowest_node)?.time_share;
  return {
    triggers: actual.triggers.map((t) => ({ name: t.name, relation: t.relation, ms: t.time_ms, calls: t.calls })),
    elapsedMs: actual.elapsed_ms, cpuMs: actual.cpu_ms, logicalReads: actual.logical_reads,
    physicalReads: actual.physical_reads, rows: actual.rows,
    grantedKb: actual.memory_granted_kb, usedKb: actual.memory_used_kb,
    spilled: actual.spilled, spillPages: actual.spill_pages ?? 0, timed: actual.timed,
    slowest: slowest && actual.slowest_ms != null && share != null ? { node: slowest, ms: actual.slowest_ms, share } : null,
    worstEstimate: worst && actual.worst_estimate != null ? { node: worst, factor: actual.worst_estimate } : null,
    skew: skew && actual.skew ? { node: skew, busiest: actual.skew.busiest, threads: actual.skew.threads } : null,
    lastRunAt: actual.last_run_at, workers: actual.workers,
  };
}
