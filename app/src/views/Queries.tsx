import { Search, SquarePen } from "lucide-react";
import { useEffect, useMemo, useRef, useState } from "react";
import { PlanDiagram } from "../components/PlanDiagram";
import { PlanView } from "../components/PlanView";
import { Sql } from "../components/Sql";
import type { Plan } from "../gen/Plan";
import type { QueryStat } from "../gen/QueryStat";
import type { Report } from "../gen/Report";
import { api, errorMessage, type ApiError } from "../lib/api";
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
      <QueryPlans key={query.id} report={report} query={query} focus={focus} />
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

type PlanState = { status: "idle" } | { status: "loading" } | { status: "ready"; plan: Plan } | { status: "error"; error: ApiError };
type PlanLook = "diagram" | "tree";
const LOOK_KEY = "culprit.plan.look";

function rememberedLook(): PlanLook {
  try {
    return localStorage.getItem(LOOK_KEY) === "tree" ? "tree" : "diagram";
  } catch {
    return "diagram";
  }
}

/** Every plan the engine kept for the query, its numbers, and the selected one drawn as a diagram or a tree. */
function QueryPlans({ report, query, focus }: { report: Report; query: QueryStat; focus: number | null }) {
  const variants = query.plans;
  const dominant = variants[0]?.id ?? null;
  const [planId, setPlanId] = useState<string | null>(dominant);
  const [look, setLook] = useState<PlanLook>(rememberedLook);
  const [state, setPlan] = useState<PlanState>(() => (query.plan ? { status: "ready", plan: query.plan } : { status: "idle" }));
  const connection = report.snapshot.connection;

  useEffect(() => {
    const captured = query.plan && (planId === dominant || planId === null);
    if (captured) {
      setPlan({ status: "ready", plan: query.plan! });
      return;
    }
    let cancelled = false;
    setPlan({ status: "loading" });
    api()
      .then((client) => client.queryPlan(connection, query.id, planId))
      .then((plan) => !cancelled && setPlan({ status: "ready", plan }))
      .catch((e) => !cancelled && setPlan({ status: "error", error: errorMessage(e) }));
    return () => {
      cancelled = true;
    };
  }, [planId, query, dominant, connection]);

  const chooseLook = (next: PlanLook) => {
    setLook(next);
    try {
      localStorage.setItem(LOOK_KEY, next);
    } catch {}
  };
  const best = variants.length > 1 ? Math.min(...variants.map((v) => v.mean_time_ms)) : null;

  return (
    <section>
      <h3>
        {variants.length > 1 ? (
          <>
            Plans <span className="muted">{variants.length}</span>
          </>
        ) : (
          "Plan"
        )}
        <span className="actions">
          <span className="seg">
            <button className={look === "diagram" ? "on" : ""} onClick={() => chooseLook("diagram")}>
              Diagram
            </button>
            <button className={look === "tree" ? "on" : ""} onClick={() => chooseLook("tree")}>
              Tree
            </button>
          </span>
        </span>
      </h3>
      {variants.length > 1 && (
        <div className="plan-variants">
          {variants.map((v, i) => (
            <button key={v.id} className={`variant ${planId === v.id ? "on" : ""}`} onClick={() => setPlanId(v.id)}>
              <span className="v-name">
                Plan {v.id}
                {i === 0 && <span className="tag" style={{ marginLeft: 6 }}>most time</span>}
                {v.is_forced && <span className="tag medium" style={{ marginLeft: 6 }}>forced</span>}
              </span>
              <span className="v-stats">
                <b className={best != null && v.mean_time_ms > best * 2 ? "slow" : ""}>{duration(v.mean_time_ms)}</b> avg · {count(v.executions)} runs
                {v.last_seen && <span className="muted"> · last {v.last_seen.slice(0, 16).replace("T", " ")}</span>}
              </span>
            </button>
          ))}
        </div>
      )}
      {state.status === "loading" && (
        <div className="empty" style={{ padding: 30 }}>
          <div className="spinner" />
          Loading the plan…
        </div>
      )}
      {state.status === "error" && (
        <div className="notice">
          <div>
            <b>No plan for this query.</b> {state.error.message}
            {state.error.hint && <div className="muted">{state.error.hint}</div>}
          </div>
        </div>
      )}
      {state.status === "ready" &&
        (look === "diagram" ? (
          <PlanDiagram key={`${query.id}-${planId}`} plan={state.plan} focus={planId === dominant ? focus : null} />
        ) : (
          <PlanView key={`${query.id}-${planId}`} plan={state.plan} focus={planId === dominant ? focus : null} />
        ))}
    </section>
  );
}
