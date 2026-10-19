import { isActionable } from "./findings";
import type { FileIo } from "../gen/FileIo";
import type { Finding } from "../gen/Finding";
import type { QueryStat } from "../gen/QueryStat";
import type { Replica } from "../gen/Replica";
import type { Report } from "../gen/Report";
import type { SignalStatus } from "../gen/SignalStatus";
import type { TableStat } from "../gen/TableStat";

/** Measurements are facts. Warning colors come from the report's assessed findings, not a second set of UI ratios. */
export function findingStatus(finding: Finding | undefined): SignalStatus {
  if (!finding || !isActionable(finding) || finding.severity === "low") return "info";
  return finding.severity === "high" ? "bad" : "watch";
}

export function worstFinding(report: Report, matches: (f: Finding) => boolean): Finding | undefined {
  const rank = { high: 2, medium: 1, low: 0 };
  return report.findings.filter((f) => isActionable(f) && matches(f)).sort((a, b) => rank[b.severity] - rank[a.severity])[0];
}

export function querySignalStatus(report: Report, query: QueryStat, checks: string[]): SignalStatus {
  return findingStatus(worstFinding(report, (f) => checks.includes(f.check) && f.queries.some((q) => q.id === query.id)));
}

export function tableSignalWarning(report: Report, table: TableStat, checks: string[]): boolean {
  const name = `${table.schema}.${table.name}`.toLowerCase();
  const status = findingStatus(worstFinding(report, (f) => checks.includes(f.check) &&
    (f.subject.name.toLowerCase() === name || f.subject.name.toLowerCase().startsWith(`${name}.`))));
  return status === "watch" || status === "bad";
}

export function storageStatus(report: Report, file: FileIo): SignalStatus {
  return findingStatus(worstFinding(report, (f) => f.check === "slow-storage" && f.id.startsWith(`slow-storage:${file.database ?? ""}:`) && f.metrics.some((m) =>
    (m.label === "File" || m.label === "Files") &&
    (m.text === file.name || m.text?.split(", ").includes(file.name) ||
      (file.kind === "log" && m.text === "redo log")))));
}

export function replicaStatus(report: Report, replica: Replica): SignalStatus {
  return findingStatus(worstFinding(report, (f) => f.check === "replica-lag" && f.subject.name === replica.name));
}
