import type { QueryStat } from "../gen/QueryStat";
import type { Report } from "../gen/Report";
import type { Workload } from "../gen/Workload";
import type { WorkloadKind } from "../gen/WorkloadKind";
import { windowLabel } from "./format";

/** What a workload was read from: the provider's kind, or, in a scan saved before providers said so, its source's name
 * (as `Workload::kind` reads it in culprit-core). */
export function workloadKind(w: Workload): WorkloadKind | null {
  if (w.kind) return w.kind;
  const byName: Record<string, WorkloadKind> = {
    "Query Store": "query-store",
    "Plan cache": "plan-cache",
    pg_stat_statements: "statement-stats",
    performance_schema: "performance-schema",
    "slow query log": "slow-log",
  };
  return byName[w.source] ?? (w.log ? "query-log" : null);
}

/** Below this, a per-day figure would be a forecast from a sliver of data; show what was measured instead. */
export const PER_DAY_MIN_HOURS = 20;

export type Honest = {
  /** True when values are per day; false when they are totals over the measured span. */
  perDay: boolean;
  /** "per day", or "over the last 2 minutes" / "over the 5 hours since statistics were reset". */
  suffix: string;
  /** Column header for counts: "Runs/day" or "Runs". */
  runsHeader: string;
  value: (total: number) => number;
};

/** Whether nobody can say how long the workload covers: counters with no reset time, or a log without timestamps. */
export function windowUnknown(w: Workload): boolean {
  return (!w.windowed || w.log != null) && w.window_start == null && w.window_hours <= 0;
}

/** "the last 7 days", "the 35 minutes since statistics were reset", "the 50 minutes the Rails log covers": the core's phrase. */
export function windowPhrase(w: Workload): string {
  const span = windowLabel(w.window_hours).replace(/^last /, "");
  if (w.log != null) return windowUnknown(w) ? `the period the ${w.log} log covers` : `the ${span} the ${w.log} log covers`;
  if (w.windowed) return `the last ${span}`;
  return windowUnknown(w) ? "the time since statistics were last reset" : `the ${span} since statistics were reset`;
}

/** How the runs of statements a rate a day would misstate are said, by query id: those that came in bursts ("750 runs
 * on Oct 11, 15:51–15:55 UTC") and those that ran fewer times than the window has days (the core's `Summary::runs_said`,
 * from `Workload::cadence`). A report saved before it said so has none. */
export function runsSaid(summary: { runs_said?: { query: string; runs: string }[] }): Map<string, string> {
  return new Map((summary.runs_said ?? []).map((r) => [r.query, r.runs]));
}

/** Per-day rates for windows long enough to mean something, measured totals otherwise. */
export function honest(w: Workload): Honest {
  if (w.window_hours >= PER_DAY_MIN_HOURS) {
    return { perDay: true, suffix: "per day", runsHeader: "Runs/day", value: (v) => (v * 24) / w.window_hours };
  }
  return {
    perDay: false,
    suffix: `over ${windowPhrase(w)}`,
    runsHeader: "Runs",
    value: (v) => v,
  };
}

/** A statement's share of database time, runs and average run as every surface states them. */
export type Figures = {
  share: number;
  /** Runs a day where `perDay`, otherwise the runs counted (see `Honest`, `said`). */
  runs: number;
  perDay: boolean;
  /** How its runs are said where a rate a day would misstate them (`Summary::runs_said`): "750 runs on Oct 11,
   * 15:51–15:55 UTC". `runs` is then that count, never a rate. */
  said: string | null;
  mean: number;
  /** "89 hours of Culprit's scans" where the figures are what those scans measured; null: the workload's window. */
  over: string | null;
  /** The totals since the source began counting, where they differ twofold from the scans': "about 214 runs a day over
   * the 1295 days since statistics were reset". */
  totals: string | null;
};

/** What `report` states of statement `q` (culprit-core's `Workload::cost_of`): on a source of running totals, what
 * Culprit's own scans measured where they cover a day or more (`summary.over_scans`), its totals over the window
 * otherwise; runs a rate a day would misstate (a burst, a few runs) as their count and how they came
 * (`summary.runs_said`). Both worked out by the core, never again here. */
export function figures(report: Report, q: QueryStat): Figures {
  const w = report.snapshot.workload!;
  const rates = honest(w);
  const scans = (report.summary.over_scans ?? []).find((c) => c.query === q.id);
  const said = runsSaid(report.summary).get(q.id) ?? null;
  const share = scans?.share ?? (w.total_time_ms > 0 ? q.total_time_ms / w.total_time_ms : 0);
  const mean = scans?.mean_ms ?? q.mean_time_ms;
  const over = scans?.over ?? null;
  const totals = scans?.totals ?? null;
  // A burst, or fewer runs than days: the count, never a rate.
  if (said != null) return { share, runs: scans?.counted ?? q.executions, perDay: false, said, mean, over, totals };
  if (scans?.runs_per_day != null) return { share, runs: scans.runs_per_day, perDay: true, said, mean, over, totals };
  return { share, runs: rates.value(q.executions), perDay: rates.perDay, said, mean, over, totals };
}
