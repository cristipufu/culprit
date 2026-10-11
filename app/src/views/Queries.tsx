import { Search, SquarePen } from "lucide-react";
import { useEffect, useMemo, useRef, useState } from "react";
import { PlanView } from "../components/PlanView";
import { Sql } from "../components/Sql";
import type { QueryStat } from "../gen/QueryStat";
import type { Report } from "../gen/Report";
import { count, duration, oneLine, percent } from "../lib/format";
import { useHotkeys } from "../lib/hotkeys";
import { setState, useStore } from "../lib/store";

type SortKey = "share" | "runs" | "avg" | "max" | "rows" | "reads" | "cpu";

const COLUMNS: { key: SortKey; label: string }[] = [
  { key: "runs", label: "Runs/day" },
  { key: "avg", label: "Avg" },
  { key: "max", label: "Max" },
  { key: "rows", label: "Rows/run" },
  { key: "reads", label: "Reads/run" },
];

function value(q: QueryStat, key: SortKey): number {
  switch (key) {
    case "share":
      return q.total_time_ms;
    case "runs":
      return q.executions;
    case "avg":
      return q.mean_time_ms;
    case "max":
      return q.max_time_ms ?? 0;
    case "rows":
      return q.executions > 0 ? (q.rows ?? 0) / q.executions : 0;
    case "reads":
      return q.executions > 0 ? (q.logical_reads ?? 0) / q.executions : 0;
    case "cpu":
      return q.cpu_time_ms ?? 0;
  }
}

export function Queries({ report }: { report: Report }) {
  const workload = report.snapshot.workload;
  const selected = useStore((s) => s.query);
  const [sort, setSort] = useState<SortKey>("share");
  const [filter, setFilter] = useState("");
  const searchRef = useRef<HTMLInputElement>(null);
  const tableRef = useRef<HTMLDivElement>(null);

  const rows = useMemo(() => {
    if (!workload) return [];
    const q = filter.trim().toLowerCase();
    return workload.queries
      .filter((x) => !q || x.text.toLowerCase().includes(q) || x.id.includes(q) || (x.object_name ?? "").toLowerCase().includes(q))
      .sort((a, b) => value(b, sort) - value(a, sort));
  }, [workload, sort, filter]);

  const current = rows.find((q) => q.id === selected) ?? rows[0] ?? null;

  const move = (delta: number) => {
    const index = Math.max(0, rows.findIndex((q) => q.id === current?.id));
    const next = rows[Math.min(rows.length - 1, Math.max(0, index + delta))];
    if (next) setState({ query: next.id });
  };
  useHotkeys({ j: () => move(1), k: () => move(-1), ArrowDown: () => move(1), ArrowUp: () => move(-1), "/": () => searchRef.current?.focus() });

  useEffect(() => {
    tableRef.current?.querySelector("tr.selected")?.scrollIntoView({ block: "nearest" });
  }, [current?.id]);

  if (!workload) {
    return (
      <div className="empty">
        <h3>No workload statistics</h3>
        <p>{report.snapshot.server.missing.find((m) => ["query-store", "plan-cache", "pg-stat-statements"].includes(m.capability))?.reason ?? "This login can't read query statistics."}</p>
      </div>
    );
  }

  const perDay = (v: number) => (workload.window_hours > 0 ? (v * 24) / workload.window_hours : v);

  return (
    <div className="split">
      <div className="pane">
        <div className="toolbar">
          <label className="search">
            <Search size={13} />
            <input ref={searchRef} value={filter} placeholder="Filter by text, table or id" onChange={(e) => setFilter(e.target.value)} />
          </label>
          <span className="spacer" />
          <span className="muted">
            {rows.length} of {count(workload.query_count)} queries · {workload.source}
          </span>
        </div>
        <div className="table-wrap" ref={tableRef}>
          <table className="grid">
            <thead>
              <tr>
                <th className={`sortable ${sort === "share" ? "sorted" : ""}`} onClick={() => setSort("share")}>
                  DB time
                </th>
                <th>Query</th>
                {COLUMNS.map((c) => (
                  <th key={c.key} className={`r sortable ${sort === c.key ? "sorted" : ""}`} onClick={() => setSort(c.key)}>
                    {c.label}
                  </th>
                ))}
              </tr>
            </thead>
            <tbody>
              {rows.map((q) => {
                const share = workload.total_time_ms > 0 ? q.total_time_ms / workload.total_time_ms : 0;
                return (
                  <tr key={q.id} className={`row ${q.id === current?.id ? "selected" : ""}`} onClick={() => setState({ query: q.id })}>
                    <td>
                      <span className="sharecell">
                        <span className="bar">
                          <span style={{ width: `${Math.max(2, Math.sqrt(share) * 100)}%`, background: share >= 0.15 ? "var(--high)" : share >= 0.03 ? "var(--medium)" : "var(--low)" }} />
                        </span>
                        <span className="num">{percent(share)}</span>
                      </span>
                    </td>
                    <td className="sqlcell">{q.object_name ? `${q.object_name}: ` : ""}{oneLine(q.text)}</td>
                    <td className="r">{count(perDay(q.executions))}</td>
                    <td className="r">{duration(q.mean_time_ms)}</td>
                    <td className="r">{duration(q.max_time_ms)}</td>
                    <td className="r">{q.rows != null && q.executions > 0 ? count(q.rows / q.executions) : "—"}</td>
                    <td className="r">{q.logical_reads != null && q.executions > 0 ? count(q.logical_reads / q.executions) : "—"}</td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        </div>
      </div>
      <div className="pane">{current ? <QueryDetail report={report} query={current} /> : <div className="empty">No queries match</div>}</div>
    </div>
  );
}

function QueryDetail({ report, query }: { report: Report; query: QueryStat }) {
  const w = report.snapshot.workload!;
  const share = w.total_time_ms > 0 ? query.total_time_ms / w.total_time_ms : 0;
  const perDay = (v: number) => (w.window_hours > 0 ? (v * 24) / w.window_hours : v);
  const related = report.findings.filter((f) => f.queries.some((r) => r.id === query.id));
  const focus = related.flatMap((f) => f.queries.filter((r) => r.id === query.id).map((r) => r.plan_node)).find((n) => n != null) ?? null;
  return (
    <div className="detail" style={{ padding: "18px 20px 30px" }}>
      <div className="crumbs">
        <span className="tag mono">query {query.id}</span>
        {query.object_name && <span className="tag mono">{query.object_name}</span>}
        {query.plans.length > 1 && <span className="tag medium">{query.plans.length} plans</span>}
        <span style={{ flex: 1 }} />
        <button className="btn" onClick={() => setState({ view: "editor", editorSql: query.text })}>
          <SquarePen size={13} /> Open in editor
        </button>
      </div>
      <section style={{ marginTop: 14 }}>
        <div className="metrics">
          <Metric value={percent(share)} label="of DB time" />
          <Metric value={count(perDay(query.executions))} label="runs per day" />
          <Metric value={duration(query.mean_time_ms)} label="average" />
          <Metric value={duration(query.max_time_ms)} label="slowest" />
          {query.rows != null && <Metric value={count(query.rows / Math.max(1, query.executions))} label="rows per run" />}
          {query.logical_reads != null && <Metric value={count(query.logical_reads / Math.max(1, query.executions))} label="pages read per run" />}
        </div>
      </section>
      {related.length > 0 && (
        <section>
          <h3>Findings</h3>
          {related.map((f) => (
            <button key={f.id} className="frow" onClick={() => setState({ view: "overview", finding: f.id })}>
              <span className={`sev ${f.severity}`} />
              <span className="title">{f.title}</span>
              <span />
            </button>
          ))}
        </section>
      )}
      <section>
        <h3>Statement</h3>
        <div className="codeblock">
          <Sql text={query.text} />
        </div>
      </section>
      {query.plans.length > 1 && (
        <section>
          <h3>Plan history</h3>
          <table className="grid">
            <thead>
              <tr>
                <th>Plan</th>
                <th className="r">Runs</th>
                <th className="r">Avg</th>
                <th>Last seen</th>
              </tr>
            </thead>
            <tbody>
              {query.plans.map((p) => (
                <tr key={p.id} className="row">
                  <td className="mono">
                    {p.id}
                    {p.is_forced ? " (forced)" : ""}
                  </td>
                  <td className="r">{count(p.executions)}</td>
                  <td className="r">{duration(p.mean_time_ms)}</td>
                  <td className="muted">{p.last_seen?.slice(0, 16).replace("T", " ") ?? "—"}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </section>
      )}
      <section>
        <h3>Plan</h3>
        {query.plan ? <PlanView key={query.id} plan={query.plan} focus={focus} /> : <div className="notice">No plan was captured for this query. Open it in the editor and press Explain.</div>}
      </section>
    </div>
  );
}

function Metric({ value, label }: { value: string; label: string }) {
  return (
    <div className="metric">
      <div className="value">{value}</div>
      <div className="label">{label}</div>
    </div>
  );
}
