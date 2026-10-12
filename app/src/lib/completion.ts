import type { Completion } from "@codemirror/autocomplete";
import type { SQLConfig, SQLNamespace } from "@codemirror/lang-sql";
import type { Catalog } from "../gen/Catalog";
import type { Engine } from "../gen/Engine";
import type { Report } from "../gen/Report";
import { rowCount } from "./format";

/** What the latest report knows about a table: row count, keys, foreign keys, the columns indexes lead with. */
function tableFacts(report: Report | null) {
  const key = (schema: string, table: string) => `${schema}.${table}`.toLowerCase();
  const rows = new Map<string, number>();
  const pk = new Map<string, Set<string>>();
  const fk = new Map<string, Map<string, string>>();
  const indexed = new Map<string, Set<string>>();
  if (!report) return { rows, pk, fk, indexed, key };
  for (const t of report.snapshot.tables) if (t.rows != null) rows.set(key(t.schema, t.name), t.rows);
  for (const i of report.snapshot.indexes ?? []) {
    const k = key(i.schema, i.table);
    const lead = i.columns[0]?.toLowerCase();
    if (i.is_primary_key) {
      const keys = pk.get(k) ?? pk.set(k, new Set()).get(k)!;
      i.columns.forEach((c) => keys.add(c.toLowerCase()));
    }
    if (lead) (indexed.get(k) ?? indexed.set(k, new Set()).get(k)!).add(lead);
  }
  for (const f of report.snapshot.foreign_keys ?? []) {
    const k = key(f.schema, f.table);
    const map = fk.get(k) ?? fk.set(k, new Map()).get(k)!;
    f.columns.forEach((c) => map.set(c.toLowerCase(), f.ref_table));
  }
  return { rows, pk, fk, indexed, key };
}

/** Completion for the editor: schema → table → column, with column types, keys and table sizes as detail. */
export function completionConfig(engine: Engine, catalog: Catalog | null, report: Report | null = null): Pick<SQLConfig, "schema" | "defaultSchema"> {
  if (!catalog || catalog.tables.length === 0) return {};
  const facts = tableFacts(report);
  const schemas: Record<string, Record<string, SQLNamespace>> = {};
  for (const table of catalog.tables) {
    const k = facts.key(table.schema, table.name);
    const pk = facts.pk.get(k);
    const fk = facts.fk.get(k);
    const indexed = facts.indexed.get(k);
    const columns: Completion[] = table.columns.map((c, i) => {
      const name = c.name.toLowerCase();
      const tags = [pk?.has(name) ? "PK" : null, fk?.has(name) ? `FK → ${fk.get(name)}` : null, !pk?.has(name) && indexed?.has(name) ? "indexed" : null].filter(Boolean);
      return { label: c.name, type: "property", detail: [c.data_type, ...tags].join(" · "), boost: (pk?.has(name) ? 0.2 : 0) - i / 1000 };
    });
    const rows = facts.rows.get(k);
    const self: Completion = {
      label: table.name,
      type: table.view ? "interface" : "class",
      detail: table.view ? "view" : rows != null ? rowCount(rows) : `${table.columns.length} columns`,
      // Among equal matches the shorter name first: "ord" offers orders before order_items.
      boost: -Math.min(table.name.length, 90) / 100,
    };
    (schemas[table.schema] ??= {})[table.name] = { self, children: columns };
  }
  const schema: Record<string, SQLNamespace> = {};
  for (const [name, tables] of Object.entries(schemas)) {
    schema[name] = { self: { label: name, type: "namespace", detail: "schema" }, children: tables };
  }
  for (const database of catalog.databases) {
    if (!(database in schema)) schema[database] = { self: { label: database, type: "namespace", detail: "database" }, children: {} };
  }
  return { schema, defaultSchema: defaultSchema(engine, Object.keys(schemas)) };
}

/** Unqualified table names resolve against this schema. */
function defaultSchema(engine: Engine, schemas: string[]): string | undefined {
  const preferred = engine === "mssql" ? "dbo" : engine === "postgres" ? "public" : undefined;
  if (preferred && schemas.includes(preferred)) return preferred;
  // MySQL's catalog only holds the current database; elsewhere, a database with one schema uses it.
  return schemas.length === 1 ? schemas[0] : preferred;
}

/** A catalog from the latest report, so completion works before (or without) the live catalog. */
export function catalogFromReport(report: Report | null): Catalog | null {
  if (!report) return null;
  const { tables, columns } = report.snapshot;
  if (!columns?.length) return null;
  return {
    databases: [],
    truncated: false,
    tables: tables.map((t) => ({
      schema: t.schema,
      name: t.name,
      view: false,
      columns: columns
        .filter((c) => c.schema === t.schema && c.table === t.name)
        .sort((a, b) => a.ordinal - b.ordinal)
        .map((c) => ({ name: c.name, data_type: c.data_type })),
    })),
  };
}
