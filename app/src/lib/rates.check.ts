import { figures } from "./rates";
import type { Report } from "../gen/Report";
import type { QueryStat } from "../gen/QueryStat";

const q = { id: "new-upsert", executions: 3917, total_time_ms: 4000, mean_time_ms: 1 } as QueryStat;
const report = {
  snapshot: { workload: { windowed: false, window_hours: 1282 * 24, total_time_ms: 4000000, window_start: "2023-04-17T00:00:00Z" } },
  summary: { over_scans: [{ query: q.id, share: 0.001, mean_ms: 1, counted: 825, runs: "at least 825 runs observed over 14 hours between scans", runs_per_day: null, over: null, totals: null, activity: { runs: 825, time_ms: 700, incomplete: true, over: "14 hours between scans" } }] },
} as unknown as Report;
const f = figures(report, q);
if (f.runs !== 825 || f.perDay || !f.said?.includes("at least 825")) throw new Error("Observed calls were diluted into a lifetime daily rate");
if (f.mean !== 1 || f.share !== 0.001) throw new Error("An incomplete interval supplied an average or workload share");
console.log("rates: observed calls retain their period without inventing a daily rate, share or average");
