import type { ColumnInfo } from "../gen/ColumnInfo";
import type { Engine } from "../gen/Engine";
import type { Finding } from "../gen/Finding";
import type { ForeignKey } from "../gen/ForeignKey";
import type { IndexStat } from "../gen/IndexStat";
import type { Report } from "../gen/Report";
import type { TableStat } from "../gen/TableStat";

export const tableKey = (t: { schema: string; name: string }) => `${t.schema}.${t.name}`;

const bare = (column: string) =>
  column
    .replace(/\s+(ASC|DESC)$/i, "")
    .replace(/[[\]"`]/g, "")
    .toLowerCase();

/** Findings about a table: its own, its indexes', and table-level ones named without the schema (`customers.email`). */
export function tableFindings(report: Report, table: TableStat): Finding[] {
  const qualified = tableKey(table).toLowerCase();
  const short = table.name.toLowerCase();
  return report.findings.filter((f) => {
    if (f.subject.kind !== "table" && f.subject.kind !== "index") return false;
    const name = f.subject.name.toLowerCase();
    return name === qualified || name.startsWith(`${qualified}.`) || name === short || name.startsWith(`${short}.`);
  });
}

export type Match = { score: number; hits: number[] };

/** Fuzzy subsequence match of `query` in `text`; contiguous runs, word starts and name prefixes score higher. */
export function fuzzy(text: string, query: string): Match | null {
  const q = query.trim().toLowerCase();
  if (!q) return { score: 0, hits: [] };
  const t = text.toLowerCase();
  const direct = t.indexOf(q);
  if (direct >= 0) {
    const atWord = direct === 0 || /[._\s-]/.test(t[direct - 1] ?? "");
    return {
      score: 1000 - direct + (atWord ? 200 : 0) + q.length * 10,
      hits: [...Array(q.length).keys()].map((i) => direct + i),
    };
  }
  const hits: number[] = [];
  let score = 0;
  let from = 0;
  for (const ch of q) {
    const at = t.indexOf(ch, from);
    if (at < 0) return null;
    const previous = hits.at(-1);
    score += previous === at - 1 ? 8 : 1;
    if (at === 0 || /[._\s-]/.test(t[at - 1] ?? "")) score += 5;
    hits.push(at);
    from = at + 1;
  }
  return { score, hits };
}

export type Membership = {
  index: IndexStat;
  position: number | null;
  included: boolean;
};

/** The indexes a column takes part in: as a key (with its position) or as an included column. */
export function columnMembership(indexes: IndexStat[], column: string): Membership[] {
  const name = column.toLowerCase();
  return indexes.flatMap((index): Membership[] => {
    const position = index.columns.map(bare).indexOf(name);
    if (position >= 0) return [{ index, position, included: false }];
    if (index.include.map(bare).includes(name)) return [{ index, position: null, included: true }];
    return [];
  });
}

/** An index whose keys are a leading prefix of another's on the same table (and isn't unique): the wider one can serve it. */
export function coveredBy(index: IndexStat, siblings: IndexStat[]): IndexStat | null {
  if (index.is_primary_key || index.is_unique || index.filter) return null;
  const keys = index.columns.map(bare);
  return (
    siblings.find((other) => {
      if (other === index || other.filter || other.columns.length < keys.length) return false;
      const theirs = other.columns.map(bare);
      return keys.every((k, i) => theirs[i] === k) && (other.columns.length > keys.length || other.name < index.name);
    }) ?? null
  );
}

export type Relation = { fk: ForeignKey; indexed: boolean };

/** Foreign keys from this table (with whether an index leads with their columns) and to it. */
export function relations(report: Report, table: TableStat): { outgoing: Relation[]; incoming: ForeignKey[] } {
  const { foreign_keys, indexes } = report.snapshot;
  const same = (schema: string, name: string) => schema.toLowerCase() === table.schema.toLowerCase() && name.toLowerCase() === table.name.toLowerCase();
  const own = indexes.filter((i) => same(i.schema, i.table));
  const outgoing = foreign_keys
    .filter((fk) => same(fk.schema, fk.table))
    .map((fk) => {
      const cols = fk.columns.map(bare);
      const indexed = own.some((i) => {
        const keys = i.columns.map(bare);
        return cols.every((c) => keys.slice(0, cols.length).includes(c));
      });
      return { fk, indexed };
    });
  const incoming = foreign_keys.filter((fk) => same(fk.ref_schema, fk.ref_table));
  return { outgoing, incoming };
}

/** Column name → the table it references through a single-column foreign key. */
export function referenceOf(outgoing: Relation[], column: string): ForeignKey | null {
  const name = column.toLowerCase();
  return outgoing.find((r) => r.fk.columns.length === 1 && bare(r.fk.columns[0] ?? "") === name)?.fk ?? null;
}

export function quoteIdent(engine: Engine, name: string): string {
  switch (engine) {
    case "mssql":
      return `[${name.replaceAll("]", "]]")}]`;
    case "mysql":
      return `\`${name.replaceAll("`", "``")}\``;
    default:
      return `"${name.replaceAll('"', '""')}"`;
  }
}

/** A read-only statement for the first `limit` rows of a table, with every name quoted for the engine. */
export function previewSql(engine: Engine, table: TableStat, limit: number): string {
  const name = `${quoteIdent(engine, table.schema)}.${quoteIdent(engine, table.name)}`;
  return engine === "mssql" ? `SELECT TOP (${limit}) * FROM ${name}` : `SELECT * FROM ${name} LIMIT ${limit}`;
}

/** Approximate DDL rebuilt from catalog metadata (columns, primary key, indexes, foreign keys). */
export function ddl(engine: Engine, table: TableStat, columns: ColumnInfo[], indexes: IndexStat[], outgoing: Relation[]): string {
  const original = table.definition ? `${table.definition.trimEnd().replace(/;$/, "")};` : null;
  if (table.kind === "virtual") return original ?? "-- The virtual table's original definition was not collected. Read it from the database before recreating it.";
  const q = (n: string) => quoteIdent(engine, n);
  const name = `${q(table.schema)}.${q(table.name)}`;
  const lines = columns.map((c) => {
    const parts = [`  ${q(c.name)} ${c.data_type}`];
    if (c.generated) parts.push(engine === "mssql" ? "IDENTITY" : engine === "mysql" ? "AUTO_INCREMENT" : "GENERATED");
    parts.push(c.nullable ? "NULL" : "NOT NULL");
    if (c.default && !c.generated) parts.push(`DEFAULT ${c.default}`);
    return parts.join(" ");
  });
  const pk = indexes.find((i) => i.is_primary_key);
  if (pk) lines.push(`  CONSTRAINT ${q(pk.name)} PRIMARY KEY (${pk.columns.map((c) => q(c.replace(/\s+(ASC|DESC)$/i, ""))).join(", ")})`);
  outgoing.forEach(({ fk }) =>
    lines.push(
      `  CONSTRAINT ${q(fk.name)} FOREIGN KEY (${fk.columns.map(q).join(", ")}) REFERENCES ${q(fk.ref_schema)}.${q(fk.ref_table)} (${fk.ref_columns.map(q).join(", ")})`,
    ),
  );
  const out = [original ?? `CREATE TABLE ${name} (\n${lines.join(",\n")}\n);`];
  indexes
    .filter((i) => !i.is_primary_key && (engine !== "sqlite" || i.definition != null))
    .forEach((i) => {
      if (i.definition && /^CREATE /i.test(i.definition)) {
        out.push(`${i.definition.replace(/;?\s*$/, "")};`);
        return;
      }
      const keys = i.columns.map((c) => {
        const desc = /\s+DESC$/i.test(c);
        return `${q(c.replace(/\s+(ASC|DESC)$/i, ""))}${desc ? " DESC" : ""}`;
      });
      const unique = i.is_unique ? "UNIQUE " : "";
      const kind = engine === "mssql" ? (i.is_clustered ? "CLUSTERED " : "NONCLUSTERED ") : "";
      const include = i.include.length ? ` INCLUDE (${i.include.map(q).join(", ")})` : "";
      const where = i.filter ? ` WHERE ${i.filter}` : "";
      out.push(`CREATE ${unique}${kind}INDEX ${q(i.name)} ON ${name} (${keys.join(", ")})${include}${where};`);
    });
  return out.join("\n\n");
}
