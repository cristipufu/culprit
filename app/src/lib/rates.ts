import type { Workload } from "../gen/Workload";
import { windowLabel } from "./format";

/** Below this, a per-day figure would be a forecast from a sliver of data; show what was measured instead. */
export const PER_DAY_MIN_HOURS = 20;

export type Honest = {
  /** True when values are per day; false when they are totals over the measured span. */
  perDay: boolean;
  /** "per day", or "in the last 2 minutes" / "in the 5 hours since statistics were reset". */
  suffix: string;
  /** Column header for counts: "Runs/day" or "Runs". */
  runsHeader: string;
  value: (total: number) => number;
};

/** Per-day rates for windows long enough to mean something, measured totals otherwise. */
export function honest(w: Workload): Honest {
  if (w.window_hours >= PER_DAY_MIN_HOURS) {
    return { perDay: true, suffix: "per day", runsHeader: "Runs/day", value: (v) => (v * 24) / w.window_hours };
  }
  const span = windowLabel(w.window_hours);
  return {
    perDay: false,
    suffix: w.windowed ? `in the ${span}` : `in the ${span.replace(/^last /, "")} since statistics were reset`,
    runsHeader: "Runs",
    value: (v) => v,
  };
}
