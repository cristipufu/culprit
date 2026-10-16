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
