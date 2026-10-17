import type { Plan } from "../gen/Plan";
import type { PlanNode } from "../gen/PlanNode";
import { doesRealWork, hottestStep, misestimate, sharesNote, stepShare, subtreeShare, uncostedSteps } from "./plan-format";

function nodesOf(n: PlanNode, out: PlanNode[] = []): PlanNode[] {
  out.push(n);
  n.children.forEach((c) => nodesOf(c, out));
  return out;
}

function batchMode(node: PlanNode): boolean {
  return ["ActualExecutionMode", "EstimatedExecutionMode"].some((k) => node.props[k]?.toLowerCase() === "batch");
}

/**
 * Each operator's own time. Row-mode operators (and every PostgreSQL/MySQL operator) report time including their inputs,
 * SQL Server batch-mode operators report their own, and untimed operators (Compute Scalar) count as the sum of their
 * inputs. Mirrors culprit_core::actual::own_times.
 */
export function ownTimes(root: PlanNode, time: (n: PlanNode) => number | null | undefined): Map<number, number> {
  const out = new Map<number, number>();
  const visit = (node: PlanNode): number => {
    const inputs = node.children.reduce((s, c) => s + visit(c), 0);
    const t = time(node);
    if (t == null) {
      out.set(node.id, 0);
      return inputs;
    }
    if (batchMode(node)) {
      out.set(node.id, t);
      return t + inputs;
    }
    out.set(node.id, Math.max(0, t - inputs));
    return Math.max(t, inputs);
  };
  visit(root);
  return out;
}

/** How operator shares are measured: actual time when the plan carries per-operator timing, estimated cost otherwise. */
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

/** The largest of `shares` over steps that aren't exchanges. */
function largest(nodes: PlanNode[], shares: Map<number, number | null>): PlanNode | null {
  let best: PlanNode | null = null;
  for (const n of nodes) {
    const s = shares.get(n.id);
    if (n.kind !== "parallelism" && s != null && (!best || s >= (shares.get(best.id) ?? 0))) best = n;
  }
  return best;
}

/** Timed operators must be at least half the plan, or the picture would be mostly guesswork. */
export function planShares(plan: Plan): Shares {
  const nodes = nodesOf(plan.root);
  const timed = nodes.filter((n) => n.act_time_ms != null).length;
  if (plan.kind === "actual" && timed > 0 && timed * 2 >= nodes.length) {
    const ms = ownTimes(plan.root, (n) => n.act_time_ms);
    const total = [...ms.values()].reduce((s, v) => s + v, 0);
    if (total > 0) {
      const own = new Map([...ms].map(([id, v]) => [id, v / total]));
      const subtree = new Map<number, number>();
      const sum = (n: PlanNode): number => {
        const v = (own.get(n.id) ?? 0) + n.children.reduce((s, c) => s + sum(c), 0);
        subtree.set(n.id, v);
        return v;
      };
      sum(plan.root);
      // Measured time: every share is stated.
      const stated = new Map<number, number | null>(own);
      const hottest = doesRealWork(plan) ? largest(nodes, stated) : null;
      return { basis: "time", own, subtree, stated, statedSubtree: new Map(subtree), note: null, hottest, ms, total };
    }
  }
  const total = Math.max(plan.total_cost, plan.root.subtree_cost, nodes.reduce((s, n) => s + n.self_cost, 0)) || 1;
  const uncosted = uncostedSteps(plan);
  const stated = new Map(nodes.map((n) => [n.id, stepShare(plan, n, uncosted)]));
  // The core's hottest step, and only where its share can be stated: beside an uncosted filesort, or on a guess, no step
  // is called the most expensive.
  const hot = doesRealWork(plan) ? hottestStep(plan) : null;
  return {
    basis: "cost",
    own: new Map(nodes.map((n) => [n.id, n.self_cost / total])),
    subtree: new Map(nodes.map((n) => [n.id, n.subtree_cost / total])),
    stated,
    statedSubtree: new Map(nodes.map((n) => [n.id, subtreeShare(plan, n, uncosted)])),
    note: sharesNote(plan),
    hottest: hot && stated.get(hot.id) != null ? hot : null,
    ms: new Map(),
    total,
  };
}

export type ActualFacts = {
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

const num = (v: string | undefined) => (v != null && v !== "" && Number.isFinite(Number(v)) ? Number(v) : null);

/** The headline numbers of an actual plan; mirrors culprit_core::actual_summary plus skew. */
export function actualFacts(plan: Plan): ActualFacts | null {
  if (plan.kind !== "actual") return null;
  const nodes = nodesOf(plan.root);
  const shares = planShares(plan);
  const sum = (pick: (n: PlanNode) => number | null | undefined) => {
    const values = nodes.map(pick).filter((v): v is number => v != null);
    return values.length ? values.reduce((s, v) => s + v, 0) : null;
  };
  let slowest: ActualFacts["slowest"] = null;
  if (shares.basis === "time") {
    for (const n of nodes) {
      const ms = shares.ms.get(n.id) ?? 0;
      if (!slowest || ms > slowest.ms) slowest = { node: n, ms, share: shares.own.get(n.id) ?? 0 };
    }
  }
  // A bad estimate repeats in every operator above the one it starts at; name where it starts (the deepest operator
  // within a factor of two of the worst), since that's what a fix (statistics, an index, a hint) has to reach. Only
  // operators the core flagged count: its warning holds the 10× and 100-row bars and leaves out an operator stopped early.
  const factorOf = (n: PlanNode) => (n.warnings.includes("misestimate") ? misestimate(n) : null);
  const largest = Math.max(0, ...nodes.map((n) => Math.abs(factorOf(n) ?? 0)));
  let worst: ActualFacts["worstEstimate"] = null;
  if (largest > 0) {
    const postOrder = (n: PlanNode): PlanNode | null => {
      for (const c of n.children) {
        const found = postOrder(c);
        if (found) return found;
      }
      const f = factorOf(n);
      return f != null && Math.abs(f) >= largest / 2 ? n : null;
    };
    const origin = postOrder(plan.root);
    if (origin) worst = { node: origin, factor: factorOf(origin)! };
  }
  let skew: ActualFacts["skew"] = null;
  for (const n of nodes) {
    const rows = n.runtime?.thread_rows ?? [];
    const total = rows.reduce((s, v) => s + v, 0);
    if (rows.length < 2 || total < 10_000) continue;
    const busiest = Math.max(...rows) / total;
    if (busiest >= 0.5 && busiest > 1.8 / rows.length && (!skew || busiest > skew.busiest)) skew = { node: n, busiest, threads: rows.length };
  }
  const spillPages = sum((n) => n.runtime?.spill_pages) ?? 0;
  // Plans that kept row counts only (SQL Server's last actual plan) report zero time: that means unknown, not instant.
  const known = (v: number | null | undefined) => (v == null || (v === 0 && shares.basis !== "time") ? null : v);
  return {
    elapsedMs: known(num(plan.props.ElapsedMs) ?? plan.root.act_time_ms),
    cpuMs: known(num(plan.props.CpuMs) ?? sum((n) => n.runtime?.cpu_ms)),
    logicalReads: sum((n) => n.runtime?.logical_reads),
    physicalReads: sum((n) => n.runtime?.physical_reads),
    rows: plan.root.act_rows,
    grantedKb: num(plan.props.MemoryGrantedKB),
    usedKb: num(plan.props.MemoryMaxUsedKB),
    spilled: spillPages > 0 || nodes.some((n) => n.warnings.includes("spill")) || plan.warnings.includes("spill"),
    spillPages,
    timed: shares.basis === "time",
    slowest: slowest && slowest.ms > 0 ? slowest : null,
    worstEstimate: worst,
    skew,
    lastRunAt: plan.props.LastRunAt ?? null,
    workers: num(plan.props.UsedThreads),
  };
}
