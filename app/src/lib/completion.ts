import type { Completion } from "@codemirror/autocomplete";
import type { SQLConfig, SQLNamespace } from "@codemirror/lang-sql";
import type { Catalog } from "../gen/Catalog";
import type { Engine } from "../gen/Engine";
import type { Report } from "../gen/Report";

/** Completion for the editor: schema → table → column, with column types and table sizes as detail. */
export function completionConfig(engine: Engine, catalog: Catalog | null): Pick<SQLConfig, "schema" | "defaultSchema"> {
  if (!catalog || catalog.tables.length === 0) return {};
  const schemas: Record<string, Record<string, SQLNamespace>> = {};
  for (const table of catalog.tables) {
    const columns: Completion[] = table.columns.map((c, i) => ({ label: c.name, type: "property", detail: c.data_type, boost: -i / 1000 }));
    const self: Completion = { label: table.name, type: table.view ? "interface" : "class", detail: table.view ? "view" : `${table.columns.length} columns` };
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
