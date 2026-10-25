import { ArrowDown, ArrowUp, Braces, Copy, WrapText, X } from "lucide-react";
import { useEffect, useId, useMemo, useRef, useState } from "react";
import { createPortal } from "react-dom";
import type { ResultSet } from "../gen/ResultSet";
import { copyAndTell } from "../lib/clipboard";
import { useFocusTrap } from "../lib/focus";
import { cellText, columnType, formattedJson, rowTsv, VALUE_PREVIEW_CHARS, valuePreview, type ResultCell } from "../lib/result-values";

export function ResultInspector({ result, cell, types, onSelect, onClose }: {
  result: ResultSet; cell: ResultCell; types?: Record<string, string>;
  onSelect: (cell: ResultCell) => void; onClose: () => void;
}) {
  const [tab, setTab] = useState<"value" | "row">("value");
  const [pretty, setPretty] = useState(true);
  const [wrap, setWrap] = useState(true);
  const root = useRef<HTMLDivElement>(null);
  const close = useRef<HTMLButtonElement>(null);
  const title = useId();
  const value = result.rows[cell.row]?.[cell.col];
  const raw = useMemo(() => cellText(value), [value]);
  const formatted = useMemo(() => formattedJson(raw), [raw]);
  const displayed = pretty && formatted !== null ? formatted : raw;
  const column = result.columns[cell.col] ?? "Value";
  const kind = value == null ? "NULL" : columnType(types, column) ?? (formatted !== null || typeof value === "object" ? "JSON" : typeof value === "string" ? "Text" : typeof value === "number" ? "Number" : "Boolean");
  useFocusTrap(root);
  useEffect(() => { window.getSelection()?.removeAllRanges(); close.current?.focus(); }, []);
  const copy = () => tab === "row"
    ? copyAndTell(result.columns.join("\t") + "\n" + rowTsv(result, cell.row), "Row copied as TSV")
    : copyAndTell(displayed, formatted !== null && pretty ? "Formatted JSON copied" : "Value copied");

  return createPortal(
    <div className="rg-inspector-backdrop" onMouseDown={(event) => { if (event.target === event.currentTarget) onClose(); }}>
      <div ref={root} className="rg-inspector" role="dialog" aria-modal="true" aria-labelledby={title} onKeyDown={(event) => {
        event.stopPropagation();
        if (event.key === "Escape") { event.preventDefault(); onClose(); }
        if ((event.metaKey || event.ctrlKey) && event.key === "Enter") event.preventDefault();
        if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === "c" && !window.getSelection()?.toString()) {
          event.preventDefault(); void copy();
        }
      }}>
        <header className="rg-inspector-head">
          <div className="rg-inspector-heading">
            <span className="eyebrow">Result inspector</span>
            <h2 id={title} title={column}>{tab === "value" ? column : `Row ${(cell.row + 1).toLocaleString()}`}</h2>
            <div className="rg-inspector-meta"><span className="rg-value-kind">{tab === "value" ? kind : `${result.columns.length} columns`}</span><span>Row {(cell.row + 1).toLocaleString()} of {result.rows.length.toLocaleString()}</span></div>
          </div>
          <button ref={close} className="icon-btn" aria-label="Close result inspector" title="Close (Esc)" onClick={onClose}><X size={17} /></button>
        </header>
        <div className="rg-inspector-toolbar">
          <div className="rg-switch" role="group" aria-label="Inspect">
            <button className={tab === "value" ? "on" : ""} aria-pressed={tab === "value"} onClick={() => setTab("value")}>Value</button>
            <button className={tab === "row" ? "on" : ""} aria-pressed={tab === "row"} onClick={() => setTab("row")}>Row</button>
          </div>
          <span className="spacer" />
          <button className="btn ghost" onClick={() => void copy()}><Copy size={13} /> {tab === "row" ? "Copy row" : "Copy value"}</button>
        </div>
        {tab === "value" ? <>
          <div className="rg-inspector-column">
            <label htmlFor={title + "-column"}>Column</label>
            <select id={title + "-column"} value={cell.col} onChange={(event) => onSelect({ row: cell.row, col: Number(event.target.value) })}>
              {result.columns.map((name, col) => <option key={col} value={col}>{col + 1}. {name}</option>)}
            </select>
          </div>
          <div className="rg-value-toolbar">
            {formatted !== null ? <div className="rg-switch" role="group" aria-label="Value format">
              <button className={pretty ? "on" : ""} aria-pressed={pretty} onClick={() => setPretty(true)}><Braces size={12} /> Formatted</button>
              <button className={!pretty ? "on" : ""} aria-pressed={!pretty} onClick={() => setPretty(false)}>Raw</button>
            </div> : <span className="muted">{value == null ? "SQL NULL" : "Value as returned"}</span>}
            <button className={`btn ghost rg-wrap ${wrap ? "on" : ""}`} aria-label="Wrap value text" aria-pressed={wrap} onClick={() => setWrap(!wrap)}><WrapText size={14} /> Wrap</button>
          </div>
          <ValuePreview key={`${cell.row}:${cell.col}:${pretty}`} text={displayed} wrap={wrap} isNull={value == null} />
        </> : <div className="rg-row-fields">
          {result.columns.map((name, col) => <button key={col} className={`rg-row-field ${col === cell.col ? "on" : ""}`} onClick={() => { onSelect({ row: cell.row, col }); setTab("value"); }}>
            <span className="rg-field-name"><span>{name}</span><small>{columnType(types, name) ?? `Column ${col + 1}`}</small></span>
            <span className={`rg-field-preview ${result.rows[cell.row]?.[col] == null ? "null" : ""}`}>{valuePreview(cellText(result.rows[cell.row]?.[col]), 240)}</span>
          </button>)}
        </div>}
        <footer className="rg-inspector-foot">
          <span className="muted">{tab === "value" ? `${raw.length.toLocaleString()} characters` : "Choose a field to inspect its value"}</span>
          <div className="rg-row-navigation">
            <button className="icon-btn" aria-label="Previous result row" title="Previous row" disabled={cell.row === 0} onClick={() => onSelect({ ...cell, row: cell.row - 1 })}><ArrowUp size={15} /></button>
            <button className="icon-btn" aria-label="Next result row" title="Next row" disabled={cell.row === result.rows.length - 1} onClick={() => onSelect({ ...cell, row: cell.row + 1 })}><ArrowDown size={15} /></button>
          </div>
        </footer>
      </div>
    </div>, document.body,
  );
}

function ValuePreview({ text, wrap, isNull }: { text: string; wrap: boolean; isNull: boolean }) {
  const [limit, setLimit] = useState(VALUE_PREVIEW_CHARS);
  const preview = valuePreview(text, limit);
  return <div className="rg-value-preview">
    <div className="rg-value-scroll" tabIndex={0} aria-label="Cell value"><pre className={`${wrap ? "wrap" : ""} ${isNull ? "null" : ""}`}>{preview}</pre></div>
    {preview.length < text.length && <div className="rg-preview-more"><span>Showing {preview.length.toLocaleString()} of {text.length.toLocaleString()} characters. Copy includes the full value.</span><button className="btn ghost" onClick={() => setLimit(Math.min(text.length, limit * 2))}>Show more</button></div>}
  </div>;
}
