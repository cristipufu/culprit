import type { Report } from "../gen/Report";
import type { ResultSet } from "../gen/ResultSet";

const unquote = (s: string) => s.replace(/^[[`"]|[\]`"]$/g, "").replace(/\]\]|``|""/g, (m) => m[0] ?? "");
const WORDS = ["new", "paid", "shipped", "delivered", "cancelled", "refunded"];

/** Synthetic rows for the browser demo's "Preview rows" (the desktop app reads real ones): shaped by the fixture's column types. */
export function demoRows(report: Report | null, sql: string): ResultSet | null {
  const m = /FROM\s+([[`"]?[^\s.]+[\]`"]?)\.([[`"]?[^\s]+?[\]`"]?)(?:\s+LIMIT\s+(\d+))?\s*$/i.exec(sql.trim());
  if (!report || !m) return null;
  const [schema, table] = [unquote(m[1] ?? ""), unquote(m[2] ?? "")];
  const limit = Number(m[3] ?? /TOP\s*\((\d+)\)/i.exec(sql)?.[1] ?? 100);
  const columns = report.snapshot.columns.filter((c) => c.schema === schema && c.table === table).sort((a, b) => a.ordinal - b.ordinal);
  if (!columns.length) return null;
  const total = report.snapshot.tables.find((t) => t.schema === schema && t.name === table)?.rows ?? limit;
  const n = Math.min(limit, Math.max(0, total));
  const base = Date.parse("2026-10-01T09:00:00Z");
  const value = (name: string, type: string, nullable: boolean, i: number): string | number | boolean | null => {
    const t = type.toLowerCase();
    if (nullable && (i * 7 + name.length) % 9 === 0) return null;
    if (/^(big|small|tiny)?int|^integer|serial/.test(t)) return name === "id" ? i + 1 : ((i * 7919 + name.length * 31) % 5000) + 1;
    if (/^(numeric|decimal|money|real|float|double)/.test(t)) return Number((((i * 37.31 + name.length) % 480) + 4.99).toFixed(2));
    if (/^bit|^bool/.test(t)) return i % 3 !== 0;
    if (/date|time/.test(t)) return new Date(base - i * 3_600_000 * 5).toISOString().replace(".000Z", "Z");
    if (/json/.test(t)) return JSON.stringify({ source: "demo", n: i });
    if (name.includes("email")) return `customer${i + 1}@example.com`;
    if (name.includes("status") || name.includes("kind") || name.includes("channel")) return WORDS[(i + name.length) % WORDS.length] ?? "new";
    return `${name}-${(i + 1).toString().padStart(4, "0")}`;
  };
  return {
    columns: columns.map((c) => c.name),
    rows: Array.from({ length: n }, (_, i) => columns.map((c) => value(c.name.toLowerCase(), c.data_type, c.nullable, i))),
    truncated: total > n,
    elapsed_ms: 12,
    messages: [],
  };
}
