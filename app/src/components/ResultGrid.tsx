import { Copy } from "lucide-react";
import { useState } from "react";
import type { ResultSet } from "../gen/ResultSet";
import { toast } from "../lib/store";

type Cell = { row: number; col: number };

export function cellText(value: unknown): string {
  if (value === null || value === undefined) return "NULL";
  if (typeof value === "object") return JSON.stringify(value);
  return String(value);
}

const NUMERIC = /^(tiny|small|medium|big)?int|^int|^integer|^numeric|^decimal|^real|^float|^double|^money|^smallmoney|^bit\b|^serial|^bigserial/i;

async function copy(text: string, what: string) {
  try {
    await navigator.clipboard.writeText(text);
    toast(`Copied ${what}`);
  } catch {
    toast("Couldn't reach the clipboard");
  }
}

/**
 * Rows from a query, rendered as text only: sticky header with each column's type, NULLs set apart, numbers aligned,
 * long values clipped with the full value on hover. Click a cell to select it; ⌘C copies it, the buttons copy its row.
 */
export function ResultGrid({ result, types, summary }: { result: ResultSet; types?: Record<string, string>; summary?: string }) {
  const [selected, setSelected] = useState<Cell | null>(null);
  const numeric = result.columns.map((c) => NUMERIC.test(types?.[c.toLowerCase()] ?? ""));
  const valueAt = (cell: Cell) => result.rows[cell.row]?.[cell.col];
  const rowText = (row: number) => (result.rows[row] ?? []).map(cellText).join("\t");
  const header = result.columns.join("\t");

  return (
    <div className="result-grid">
      <div className="rg-bar">
        <span className="muted">
          {summary ?? `${result.rows.length} row${result.rows.length === 1 ? "" : "s"}${result.truncated ? " (more exist)" : ""}`}
        </span>
        <span className="spacer" />
        {selected && (
          <>
            <button className="btn ghost" onClick={() => copy(cellText(valueAt(selected)), "the value")}>
              <Copy size={12} /> Value
            </button>
            <button className="btn ghost" onClick={() => copy(`${header}\n${rowText(selected.row)}`, "the row")}>
              <Copy size={12} /> Row
            </button>
          </>
        )}
        <button className="btn ghost" onClick={() => copy([header, ...result.rows.map((_, i) => rowText(i))].join("\n"), result.rows.length === 1 ? "1 row" : `${result.rows.length} rows`)}>
          <Copy size={12} /> All as TSV
        </button>
      </div>
      <div
        className="rg-scroll"
        tabIndex={0}
        onKeyDown={(e) => {
          if (!selected) return;
          if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === "c") {
            e.preventDefault();
            copy(cellText(valueAt(selected)), "the value");
            return;
          }
          const moves: Record<string, [number, number]> = {
            ArrowDown: [1, 0],
            ArrowUp: [-1, 0],
            ArrowLeft: [0, -1],
            ArrowRight: [0, 1],
          };
          const move = moves[e.key];
          if (!move) return;
          e.preventDefault();
          const row = Math.min(result.rows.length - 1, Math.max(0, selected.row + move[0]));
          const col = Math.min(result.columns.length - 1, Math.max(0, selected.col + move[1]));
          setSelected({ row, col });
        }}
      >
        <table className="rg">
          <thead>
            <tr>
              <th className="rg-n">#</th>
              {result.columns.map((c, i) => (
                <th key={i} className={numeric[i] ? "r" : ""} title={types?.[c.toLowerCase()] ? `${c} · ${types[c.toLowerCase()]}` : c}>
                  <span className="rg-name">{c}</span>
                  {types?.[c.toLowerCase()] && <span className="rg-type">{types[c.toLowerCase()]}</span>}
                </th>
              ))}
            </tr>
          </thead>
          <tbody>
            {result.rows.map((row, r) => (
              <tr key={r} className={selected?.row === r ? "on" : ""}>
                <td className="rg-n">{r + 1}</td>
                {row.map((value, c) => {
                  const text = cellText(value);
                  const isNull = value === null || value === undefined;
                  const on = selected?.row === r && selected.col === c;
                  return (
                    <td
                      key={c}
                      className={`${numeric[c] ? "r" : ""} ${isNull ? "null" : ""} ${on ? "sel" : ""}`}
                      title={text.length > 40 ? text : undefined}
                      onClick={() => setSelected({ row: r, col: c })}
                    >
                      {text}
                    </td>
                  );
                })}
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </div>
  );
}
