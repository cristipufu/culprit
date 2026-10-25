import { ArrowDown, ArrowUp, Filter, Play, SquarePen, X } from "lucide-react";
import { useEffect, useId, useMemo, useRef, useState } from "react";
import { ResultGrid } from "../components/ResultGrid";
import type { ColumnSort } from "../gen/ColumnSort";
import type { Report } from "../gen/Report";
import type { TablePreview } from "../gen/TablePreview";
import type { TablePreviewOptions } from "../gen/TablePreviewOptions";
import type { TableStat } from "../gen/TableStat";
import { api, errorMessage, type ApiError } from "../lib/api";
import { LatestRequest } from "../lib/requests";
import { nextColumnSort, previewSql } from "../lib/schema-model";
import { getState, setState } from "../lib/store";
import "./schema-data.css";

const ROWS = 1000;
const ROW_LABEL = ROWS.toLocaleString();
type Preview = TablePreview & { options: TablePreviewOptions };

/** A deliberate, bounded server read. Draft filters never change the displayed result until explicitly applied. */
export function SchemaData({ report, table }: { report: Report; table: TableStat }) {
  const { snapshot } = report;
  const [filter, setFilter] = useState("");
  const [preview, setPreview] = useState<Preview | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<ApiError | null>(null);
  const request = useRef(new LatestRequest());
  const running = useRef(false);
  const input = useRef<HTMLInputElement>(null);
  const help = useId();
  useEffect(() => () => request.current.invalidate(), []);
  const order = preview?.options.order ?? null;
  const applied = preview?.options.filter ?? "";
  const dirty = filter.trim() !== applied;
  const sql = previewSql(snapshot.server.engine, table, ROWS, { filter, order });
  const types = useMemo(() => Object.fromEntries(snapshot.columns
    .filter((column) => column.schema === table.schema && column.table === table.name)
    .map((column) => [column.name.toLowerCase(), column.data_type])), [snapshot.columns, table]);

  const run = async (nextOrder: ColumnSort | null = order, nextFilter = filter) => {
    if (running.current) return;
    running.current = true;
    const owns = request.current.start();
    const options: TablePreviewOptions = { schema: table.schema, table: table.name, filter: nextFilter.trim() || null, order: nextOrder, limit: ROWS };
    setBusy(true); setError(null);
    try {
      const result = await (await api()).previewTable(snapshot.connection, options);
      if (owns() && getState().active === snapshot.connection) setPreview({ ...result, options });
    } catch (error) {
      if (owns() && getState().active === snapshot.connection) setError(errorMessage(error));
    } finally {
      running.current = false;
      if (owns()) setBusy(false);
    }
  };

  return <div className="data-tab" aria-busy={busy}>
    <form className="data-filter-bar" onSubmit={(event) => { event.preventDefault(); void run(); }}>
      <div className="data-filter-input"><Filter size={14} aria-hidden="true" /><span>WHERE</span>
        <input ref={input} aria-label="Quick filter" aria-describedby={help} value={filter} maxLength={16384} spellCheck={false} autoComplete="off"
          placeholder="status = 'open' AND amount > 100" onChange={(event) => setFilter(event.target.value)} />
        {filter && <button type="button" className="icon-btn" aria-label="Clear filter" disabled={busy} onClick={() => {
          setFilter(""); input.current?.focus(); if (applied) void run(order, "");
        }}><X size={13} /></button>}
      </div>
      <button className="btn primary" type="submit" disabled={busy}>{busy ? <span className="spinner" /> : <Play size={12} />}
        {busy ? "Reading…" : !preview ? `Preview ${ROW_LABEL} rows` : dirty ? "Apply filter" : "Refresh"}</button>
      <button className="btn ghost" type="button" onClick={() => setState({ view: "editor", editorSql: sql })}><SquarePen size={12} /> Open in editor</button>
    </form>
    <div className="data-preview-meta">
      <span id={help}>{preview ? `Up to ${ROW_LABEL} matching rows · read-only` : "Nothing is fetched until you ask."}</span>
      {order && <button type="button" className="data-sort-chip" disabled={busy} title="Clear sorting" onClick={() => void run(null, applied)}>
        <span>{order.column}</span>{order.direction === "asc" ? <ArrowUp size={12} /> : <ArrowDown size={12} />}<X size={11} />
      </button>}
      {dirty && preview && <span className="data-draft">Filter changes not applied</span>}
    </div>
    {error && <div className="notice error" role="alert"><div><b>Couldn't read rows.</b> {error.message}{error.hint && <div>{error.hint}</div>}</div></div>}
    {!preview && !busy && <p className="muted data-note">Enter a WHERE expression to filter. Click a column header to sort ascending, descending, then clear sorting.</p>}
    {!preview && busy && <div className="empty" style={{ padding: 30 }}><span className="spinner" /> Reading rows…</div>}
    {preview && <ResultGrid result={preview.result} types={types} order={order} sortDisabled={busy}
      summary={`${preview.result.rows.length.toLocaleString()} rows loaded · limit ${ROW_LABEL}${preview.result.truncated ? " · partial result" : ""}`}
      onSort={(column) => void run(nextColumnSort(order, column), applied)} />}
    <details className="data-query"><summary>{preview ? "Preview SQL" : "SQL"}</summary><pre>{preview?.sql ?? sql}</pre></details>
  </div>;
}
