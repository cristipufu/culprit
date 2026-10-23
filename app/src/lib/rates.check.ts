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

// List rendering must inspect summary entries linearly across a report, including repeated sorting lookups.
let reads = 0;
const queries = Array.from({ length: 500 }, (_, i) => ({ ...q, id: `q${i}` }));
const large = {
  snapshot: report.snapshot,
  summary: {
    runs_said: queries.map((q) => ({ query: q.id, runs: "10 runs" })),
    over_scans: queries.map((q) => ({ get query() { reads++; return q.id; }, share: 0.25, mean_ms: 2 })),
  },
} as unknown as Report;
for (let pass = 0; pass < 3; pass++) {
  for (const query of queries) {
    const value = figures(large, query);
    if (value.share !== 0.25 || value.mean !== 2 || value.said !== "10 runs") throw new Error("Indexed figures changed their evidence");
  }
}
if (reads > queries.length * 4) throw new Error("Summary lookups scale quadratically with the query list");
large.summary.over_scans = [];
large.summary.runs_said = [];
if (figures(large, queries[0]!).share === 0.25 || figures(large, queries[0]!).said != null) throw new Error("Replaced summary arrays kept stale figures");
console.log("rates: repeated list lookups stay linear and replaced summaries invalidate their index");
