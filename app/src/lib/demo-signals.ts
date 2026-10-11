import type { ColumnHistogram } from "../gen/ColumnHistogram";
import type { ConstraintIssue } from "../gen/ConstraintIssue";
import type { FileIo } from "../gen/FileIo";
import type { Finding } from "../gen/Finding";
import type { IndexContention } from "../gen/IndexContention";
import type { IndexStat } from "../gen/IndexStat";
import type { QueryCosts } from "../gen/QueryCosts";
import type { QueryStat } from "../gen/QueryStat";
import type { Replica } from "../gen/Replica";
import type { Report } from "../gen/Report";
import type { Signal } from "../gen/Signal";
import type { TableStat } from "../gen/TableStat";
import type { TableStorage } from "../gen/TableStorage";

/** Signals beyond run time captured from the demo databases (fixtures/signals.sample.json); fixture reports predate them. */
type SignalsPatch = {
  costs: Record<string, QueryCosts>;
  extra: QueryStat[];
  tables: TableStat[];
  indexes: IndexStat[];
  storage: Record<string, TableStorage>;
  contention: Record<string, IndexContention>;
  constraints: ConstraintIssue[];
  histograms: ColumnHistogram[] | null;
  files: FileIo[];
  replicas: Replica[];
  signals: Signal[];
  findings: Finding[];
};
type SignalsSample = Record<string, SignalsPatch>;

export const signalsSample: () => Promise<SignalsSample | null> = import.meta.env.DEV
  ? () => import("../../../fixtures/signals.sample.json").then((m) => m.default as unknown as SignalsSample)
  : async () => null;

/** Merge the captured signals into a fixture report: costs by query id, the queries and objects new findings point at. */
export function withSignals(report: Report, patch: SignalsPatch | undefined): Report {
  if (!patch) return report;
  const s = report.snapshot;
  const workload = s.workload;
  const known = new Set(workload?.queries.map((q) => q.id) ?? []);
  const extra = patch.extra.filter((q) => !known.has(q.id));
  const queries = [...(workload?.queries ?? []).map((q) => ({ ...q, costs: patch.costs[q.id] ?? q.costs ?? null })), ...extra];
  const key = (schema: string, name: string) => `${schema}.${name}`.toLowerCase();
  const tables = [...s.tables, ...patch.tables.filter((t) => !s.tables.some((x) => key(x.schema, x.name) === key(t.schema, t.name)))].map((t) => ({
    ...t,
    storage: patch.storage[key(t.schema, t.name)] ?? t.storage ?? null,
  }));
  const indexes = [...s.indexes, ...patch.indexes.filter((i) => !s.indexes.some((x) => key(x.schema, `${x.table}.${x.name}`) === key(i.schema, `${i.table}.${i.name}`)))].map(
    (i) => ({ ...i, contention: patch.contention[key(i.schema, `${i.table}.${i.name}`)] ?? i.contention ?? null }),
  );
  const pressure = s.pressure
    ? {
        ...s.pressure,
        signals: [...s.pressure.signals.filter((x) => !patch.signals.some((y) => y.key === x.key)), ...patch.signals],
        files: patch.files,
        replicas: patch.replicas,
      }
    : null;
  // Findings are not merged here: the fixture reports are re-analyzed with this data baked in
  // (scratchpad gen/regen-reports.ts), so their findings, summary and shares all come from one run of the checks.
  return {
    ...report,
    snapshot: {
      ...s,
      workload: workload
        ? {
            ...workload,
            queries,
            query_count: workload.query_count + extra.length,
            total_time_ms: workload.total_time_ms + extra.reduce((sum, q) => sum + q.total_time_ms, 0),
            total_executions: workload.total_executions + extra.reduce((sum, q) => sum + q.executions, 0),
          }
        : workload,
      tables,
      indexes,
      pressure,
      constraints: patch.constraints,
      histograms: patch.histograms,
    },
  };
}
