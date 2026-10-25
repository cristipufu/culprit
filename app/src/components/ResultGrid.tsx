import { ChevronLeft, ChevronRight, Copy, PanelRightOpen } from "lucide-react";
import { useEffect, useId, useMemo, useRef, useState } from "react";
import type { ResultSet } from "../gen/ResultSet";
import { copyAndTell } from "../lib/clipboard";
import { cellText, columnType, resultPageSize, rowTsv, valuePreview, type ResultCell } from "../lib/result-values";
import { ResultInspector } from "./ResultInspector";
import { ResultExportMenu } from "./ResultExportMenu";
import "./result-grid.css";

const NUMERIC = /^(tiny|small|medium|big)?int|^int|^integer|^numeric|^decimal|^real|^float|^double|^money|^smallmoney|^bit\b|^serial|^bigserial/i;
type Selection = { result: ResultSet; cell: ResultCell | null; page: number; inspecting: boolean };

/** A bounded result page. Selection and the inspector belong to this result, never to a later query's rows. */
export function ResultGrid({ result, types, summary }: { result: ResultSet; types?: Record<string, string>; summary?: string }) {
  const [selection, setSelection] = useState<Selection>({ result, cell: null, page: 0, inspecting: false });
  let current = selection;
  if (current.result !== result) {
    // Reset at the transition, including when a cached result returns after another query tab.
    current = { result, cell: null, page: 0, inspecting: false };
    setSelection(current);
  }
  const selected = current.cell;
  const grid = useRef<HTMLTableElement>(null);
  const scroll = useRef<HTMLDivElement>(null);
  const id = useId();
  const pageSize = resultPageSize(result.columns.length);
  const start = current.page * pageSize;
  const end = Math.min(start + pageSize, result.rows.length);
  const numeric = useMemo(() => result.columns.map((column, col) => NUMERIC.test(columnType(types, column) ?? "") ||
    (result.rows.some((row) => row[col] != null) && result.rows.every((row) => row[col] == null || typeof row[col] === "number"))), [result, types]);
  const rows = useMemo(() => result.rows.slice(start, end).map((row) => result.columns.map((_, col) => {
    const value = row[col];
    const text = cellText(value);
    return { text: valuePreview(text, 320), title: text.length > 40 ? valuePreview(text, 1024) + (text.length > 1024 ? "… · Double-click to inspect" : "") : undefined, isNull: value == null };
  })), [result, start, end]);
  const select = (cell: ResultCell, inspecting = current.inspecting) => setSelection({ result, cell, page: Math.floor(cell.row / pageSize), inspecting });
  const inspect = () => { if (result.rows.length && result.columns.length) select(selected ?? { row: start, col: 0 }, true); };
  const header = result.columns.join("\t");
  const copyValue = () => selected && copyAndTell(cellText(result.rows[selected.row]?.[selected.col]), "Value copied");
  const changePage = (page: number) => setSelection({ result, page, inspecting: false, cell: selected ? { row: page * pageSize, col: selected.col } : null });
  useEffect(() => {
    const cell = selected && document.getElementById(`${id}-${selected.row}-${selected.col}`);
    const area = scroll.current;
    if (!cell || !area) return;
    const bounds = area.getBoundingClientRect();
    const rect = cell.getBoundingClientRect();
    const top = bounds.top + (grid.current?.tHead?.getBoundingClientRect().height ?? 0);
    const left = bounds.left + (grid.current?.querySelector(".rg-n")?.getBoundingClientRect().width ?? 0);
    if (rect.top < top) area.scrollTop += rect.top - top;
    else if (rect.bottom > bounds.bottom) area.scrollTop += rect.bottom - bounds.bottom;
    if (rect.left < left || rect.width > bounds.right - left) area.scrollLeft += rect.left - left;
    else if (rect.right > bounds.right) area.scrollLeft += rect.right - bounds.right;
  }, [id, selected?.row, selected?.col, result]);

  return <div className="result-grid">
    <div className="rg-bar">
      <span className="muted rg-summary" title={summary}>{summary ?? `${result.rows.length.toLocaleString()} row${result.rows.length === 1 ? "" : "s"}${result.truncated ? " (more exist)" : ""}`}</span>
      <div className="rg-actions">
        <button className="btn ghost" disabled={!result.rows.length || !result.columns.length} onClick={inspect} title="Inspect a value (Enter or double-click)"><PanelRightOpen size={13} /> Inspect</button>
        {selected && <>
          <button className="btn ghost" onClick={() => void copyValue()}><Copy size={12} /> Value</button>
          <button className="btn ghost" onClick={() => void copyAndTell(header + "\n" + rowTsv(result, selected.row), "Row copied as TSV")}><Copy size={12} /> Row</button>
        </>}
        <button className="btn ghost" onClick={() => void copyAndTell([header, ...result.rows.map((_, row) => rowTsv(result, row))].join("\n"), `${result.rows.length.toLocaleString()} rows copied as TSV`)}><Copy size={12} /> All as TSV</button>
        <ResultExportMenu result={result} />
      </div>
    </div>
    <div ref={scroll} className="rg-scroll" onKeyDown={(event) => {
      if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === "c" && selected && !window.getSelection()?.toString()) {
        event.preventDefault(); event.stopPropagation(); void copyValue(); return;
      }
      if (event.key === "Enter") { event.preventDefault(); event.stopPropagation(); inspect(); return; }
      const moves: Record<string, [number, number]> = { ArrowDown: [1, 0], ArrowUp: [-1, 0], ArrowLeft: [0, -1], ArrowRight: [0, 1] };
      const move = moves[event.key];
      if (!move || !result.rows.length || !result.columns.length || event.metaKey || event.ctrlKey || event.altKey) return;
      event.preventDefault(); event.stopPropagation();
      const cell = selected ?? { row: start, col: 0 };
      select({ row: Math.min(result.rows.length - 1, Math.max(0, cell.row + move[0])), col: Math.min(result.columns.length - 1, Math.max(0, cell.col + move[1])) });
    }}>
      <table ref={grid} className="rg" role="grid" tabIndex={0} aria-label="Query results" aria-rowcount={result.rows.length + 1} aria-colcount={result.columns.length + 1} aria-activedescendant={selected ? `${id}-${selected.row}-${selected.col}` : undefined} onFocus={() => {
        if (result.rows.length && result.columns.length) setSelection((previous) => previous.result === result && previous.cell ? previous : { result, page: current.page, inspecting: false, cell: { row: start, col: 0 } });
      }}>
        <thead><tr aria-rowindex={1}><th className="rg-n">#</th>{result.columns.map((column, col) => <th key={col} className={numeric[col] ? "r" : ""} title={columnType(types, column) ? `${column} · ${columnType(types, column)}` : column}><span className="rg-name">{column}</span>{columnType(types, column) && <span className="rg-type">{columnType(types, column)}</span>}</th>)}</tr></thead>
        <tbody>{rows.map((row, index) => {
          const r = start + index;
          return <tr key={r} aria-rowindex={r + 2} className={selected?.row === r ? "on" : ""}><td className="rg-n" role="rowheader">{r + 1}</td>{row.map((value, col) => <td key={col} id={`${id}-${r}-${col}`} role="gridcell" aria-selected={selected?.row === r && selected.col === col} className={`${numeric[col] ? "r" : ""} ${value.isNull ? "null" : ""} ${selected?.row === r && selected.col === col ? "sel" : ""}`} title={value.title} onClick={() => { select({ row: r, col }); grid.current?.focus({ preventScroll: true }); }} onDoubleClick={() => select({ row: r, col }, true)}>{value.text}</td>)}</tr>;
        })}</tbody>
      </table>
      {!result.rows.length && <div className="rg-empty">No rows returned</div>}
    </div>
    <footer className="rg-footer">
      <span className="muted">{result.rows.length ? `${(start + 1).toLocaleString()}–${end.toLocaleString()} of ${result.rows.length.toLocaleString()} rows` : "0 rows"}{result.truncated && " · partial result"}</span>
      <span className="rg-hint">Double-click a cell to inspect</span>
      {result.rows.length > pageSize && <div className="rg-pages"><button className="icon-btn" aria-label="Previous result page" disabled={current.page === 0} onClick={() => changePage(current.page - 1)}><ChevronLeft size={14} /></button><span>{current.page + 1} / {Math.ceil(result.rows.length / pageSize)}</span><button className="icon-btn" aria-label="Next result page" disabled={end === result.rows.length} onClick={() => changePage(current.page + 1)}><ChevronRight size={14} /></button></div>}
    </footer>
    {current.inspecting && selected && <ResultInspector result={result} cell={selected} types={types} onSelect={(cell) => select(cell, true)} onClose={() => setSelection({ ...current, inspecting: false })} />}
  </div>;
}
