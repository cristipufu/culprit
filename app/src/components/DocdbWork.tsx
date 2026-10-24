import type { DocdbMetrics } from "../gen/DocdbMetrics";
import { count, duration } from "../lib/format";

/** Native storage entries and RPCs retain their units; they are never PostgreSQL pages or CPU utilization. */
export function DocdbWork({ metrics, basis, runs }: { metrics?: DocdbMetrics | null; basis: "execution" | "loop" | "workload"; runs?: number }) {
  if (!metrics) return null;
  const divisor = basis === "workload" && runs != null && runs > 0 ? runs : 1;
  const label = basis === "workload" ? (runs != null && runs > 0 ? "average per statement run" : "retained totals") : basis === "loop" ? "average per operator loop" : "this measured execution";
  const entries: [keyof DocdbMetrics, string, "count" | "time"][] = [
    ["read_requests", "Read RPCs", "count"], ["write_requests", "Write RPCs", "count"],
    ["read_operations", "Read operations", "count"], ["write_operations", "Write operations", "count"],
    ["rows_scanned", "Storage entries scanned", "count"], ["rows_returned", "Storage rows returned", "count"],
    ["read_ms", "Storage read time", "time"], ["write_ms", "Storage write time", "time"],
    ["wait_ms", "DocDB wait time", "time"], ["catalog_ms", "Catalog time", "time"],
    ["retries", "Retries", "count"], ["conflict_retries", "Conflict retries", "count"],
    ["read_restarts", "Read restarts", "count"], ["obsolete_rows_scanned", "Obsolete entries scanned", "count"],
    ["seeks", "LSM seeks", "count"], ["nexts", "LSM nexts", "count"], ["prevs", "LSM previous steps", "count"],
  ];
  const rows = entries.flatMap(([key, title, unit]) => {
    const value = metrics[key];
    return value == null || !Number.isFinite(value) || value < 0 ? [] : [{ key, title, value: unit === "time" ? duration(value / divisor) : count(value / divisor) }];
  });
  if (rows.length === 0) return null;
  return (
    <section className="beyond" aria-label="DocDB work">
      <h3>DocDB work</h3>
      <p className="beyond-foot">{label}. Storage entries can include index and table reads of the same SQL row. Timings can overlap and are not exclusive shares of run time.</p>
      <div className="beyond-rows" role="list">
        {rows.map((row) => <div className="beyond-row" key={row.key} role="listitem"><i className="sig-dot info" aria-hidden /><span className="beyond-label">{row.title}</span><span className="beyond-value">{row.value}</span></div>)}
      </div>
    </section>
  );
}
