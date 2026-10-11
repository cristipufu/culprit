import type { PlanNode } from "../gen/PlanNode";
import type { QueryStat } from "../gen/QueryStat";
import type { Report } from "../gen/Report";

export type IndexUse = { query: QueryStat; ops: string[] };

const clean = (s: string) => s.replace(/[[\]"`]/g, "").toLowerCase();

export function indexKey(schema: string, table: string, index: string): string {
  return `${clean(schema)}.${clean(table)}.${clean(index)}`;
}

function walk(node: PlanNode, visit: (n: PlanNode) => void): void {
  visit(node);
  node.children.forEach((c) => walk(c, visit));
}

/** Which captured query plans touch each index, keyed by `schema.table.index`, heaviest queries first. */
export function indexUsage(report: Report): Map<string, IndexUse[]> {
  const out = new Map<string, IndexUse[]>();
  const queries = [...(report.snapshot.workload?.queries ?? [])].sort((a, b) => b.total_time_ms - a.total_time_ms);
  for (const query of queries) {
    if (!query.plan) continue;
    const seen = new Map<string, Set<string>>();
    walk(query.plan.root, (n) => {
      if (!n.index || !n.object) return;
      const parts = clean(n.object).split(".");
      const table = parts.at(-1) ?? "";
      const schema = parts.at(-2) ?? "";
      const key = indexKey(schema, table, n.index);
      if (!seen.has(key)) seen.set(key, new Set());
      seen.get(key)!.add(n.op);
    });
    seen.forEach((ops, key) => {
      if (!out.has(key)) out.set(key, []);
      out.get(key)!.push({ query, ops: [...ops] });
    });
  }
  return out;
}

export function plannedQueryCount(report: Report): number {
  return (report.snapshot.workload?.queries ?? []).filter((q) => q.plan).length;
}
