import { GitCompareArrows, Search, SquareArrowOutUpRight, SquarePen } from "lucide-react";
import { useEffect, useMemo, useRef, useState } from "react";
import { LookToggle, PlanSurface, rememberLook, rememberedLook, type PlanLook } from "../components/PlanSurface";
import { LastActualRun, PlanSourceToggle, type PlanSource } from "../components/ActualRun";
import { measuresRuns } from "../lib/engines";
import { QueryTrend, Sparkline } from "../components/Trends";
import { RuntimeSpread } from "../components/RuntimeSpread";
import { QueryCostsPanel } from "../components/BeyondTime";
import { Sql } from "../components/Sql";
import type { Plan } from "../gen/Plan";
import type { QueryStat } from "../gen/QueryStat";
import type { Report } from "../gen/Report";
import { api, errorMessage, type ApiError } from "../lib/api";
import { count, duration, oneLine, percent, when } from "../lib/format";
import { useHotkeys } from "../lib/hotkeys";
import { addPlan, compare } from "../lib/plans";
import { querySummary } from "../lib/query-summary";
import { honest, workloadKind } from "../lib/rates";
import { setState, toast, useStore } from "../lib/store";
import { compareSince } from "./Overview";

type SortKey = "share" | "runs" | "avg" | "max" | "rows" | "reads" | "cpu";

/** `optional` columns step aside on narrow panes (container queries) before the table would scroll sideways. */
const COLUMNS: { key: SortKey; label: string; optional?: boolean }[] = [
  { key: "runs", label: "Runs/day" },
  { key: "avg", label: "Avg" },
  { key: "max", label: "Max", optional: true },
  { key: "rows", label: "Rows/run", optional: true },
  { key: "reads", label: "Reads/run", optional: true },
];

/** Verb, table and key predicate on one line ("SELECT orders · customer_id = @P1"); the statement on hover. */
function QueryCell({ q }: { q: QueryStat }) {
  const s = querySummary(q.text, q.object_name);
  return (
    <span className="qsum" title={oneLine(q.text)}>
      <b>{s.verb}</b>
      {s.target && <span className="qsum-table">{s.target}</span>}
      {s.predicate ? <span className="qsum-pred">{s.predicate}</span> : !s.target && <span className="qsum-pred">{oneLine(q.text)}</span>}
    </span>
  );
}

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
  const regressed = useMemo(() => new Set(report.findings.filter((f) => f.check === "regression").flatMap((f) => f.queries.map((r) => r.id))), [report.findings]);

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
        <h3>No query history</h3>
        <p>{report.summary.no_workload ?? "This scan has no query history."}</p>
      </div>
    );
  }

  // Per-day only over windows long enough to mean it; measured totals otherwise.
  const rates = honest(workload);

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
                <th className="col-query">Query</th>
                <th className="col-trend" title="Average duration per run over the window">
                  Trend
                </th>
                {COLUMNS.map((c) => (
                  <th
                    key={c.key}
                    className={`r sortable ${sort === c.key ? "sorted" : ""} ${c.optional ? "col-optional" : ""}`}
                    onClick={() => setSort(c.key)}
                    title={c.key === "runs" && !rates.perDay ? `Runs ${rates.suffix}` : undefined}
                  >
                    {c.key === "runs" ? rates.runsHeader : c.label}
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
                    <td className="sqlcell col-query">
                      <QueryCell q={q} />
                    </td>
                    <td className="trendcell col-trend">
                      <Sparkline query={q} timeline={workload.timeline} alert={regressed.has(q.id)} />
                    </td>
                    <td className="r">{count(rates.value(q.executions))}</td>
                    <td className="r">{duration(q.mean_time_ms)}</td>
                    <td className="r col-optional">{duration(q.max_time_ms)}</td>
                    <td className="r col-optional">{q.rows != null && q.executions > 0 ? count(q.rows / q.executions) : "—"}</td>
                    <td className="r col-optional">{q.logical_reads != null && q.executions > 0 ? count(q.logical_reads / q.executions) : "—"}</td>
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

type DetailTab = "summary" | "plan" | "history";
const TAB_KEY = "culprit.query.tab";

function rememberedTab(): DetailTab {
  try {
    const v = localStorage.getItem(TAB_KEY);
    return v === "plan" || v === "history" ? v : "summary";
  } catch {
    return "summary";
  }
}

function QueryDetail({ report, query }: { report: Report; query: QueryStat }) {
  const w = report.snapshot.workload!;
  const share = w.total_time_ms > 0 ? query.total_time_ms / w.total_time_ms : 0;
  const rates = honest(w);
  const related = report.findings.filter((f) => f.queries.some((r) => r.id === query.id));
  const focus = related.flatMap((f) => f.queries.filter((r) => r.id === query.id).map((r) => r.plan_node)).find((n) => n != null) ?? null;
  const [tab, setTab] = useState<DetailTab>(rememberedTab);
  const [planId, setPlanId] = useState<string | null>(query.plans[0]?.id ?? null);
  const insights = query.plan?.insights?.items ?? [];
  const worst = insights.find((i) => i.severity === "high") ? "high" : insights.find((i) => i.severity === "medium") ? "medium" : insights.length ? "low" : null;

  const choose = (next: DetailTab) => {
    setTab(next);
    try {
      localStorage.setItem(TAB_KEY, next);
    } catch {}
  };
  useHotkeys({ p: () => choose(tab === "plan" ? "summary" : "plan") });

  const tabs: { tab: DetailTab; label: string; extra?: React.ReactNode; hint?: string }[] = [
    { tab: "summary", label: "Summary" },
    {
      tab: "plan",
      label: "Plan",
      hint: "P",
      extra: insights.length > 0 ? <span className={`tab-count ${worst}`}>{insights.length}</span> : null,
    },
    { tab: "history", label: "History", extra: query.plans.length > 1 ? <span className="tab-count">{query.plans.length}</span> : null },
  ];

  return (
    <div className="detail query-detail">
      <div className="qd-head">
        <div className="crumbs">
          <span className="tag mono">query {query.id}</span>
          {query.object_name && <span className="tag mono">{query.object_name}</span>}
          {query.plans.length > 1 && <span className="tag medium">{query.plans.length} plans</span>}
          <span style={{ flex: 1 }} />
          <button className="btn" onClick={() => setState({ view: "editor", editorSql: query.text })}>
            <SquarePen size={13} /> Open in editor
          </button>
        </div>
        <div className="qd-tabs" role="tablist" aria-label="Query detail">
          {tabs.map((t) => (
            <button key={t.tab} role="tab" aria-selected={tab === t.tab} className={`qd-tab ${tab === t.tab ? "on" : ""}`} onClick={() => choose(t.tab)}>
              {t.label}
              {t.extra}
              {t.hint && <span className="kbd">{t.hint}</span>}
            </button>
          ))}
        </div>
      </div>
      <div className="qd-body">
        {tab === "summary" && (
          <>
            <section style={{ marginTop: 4 }}>
              <div className="metrics">
                <Metric value={percent(share)} label="of DB time" />
                <Metric value={count(rates.value(query.executions))} label={`runs ${rates.suffix}`} />
                <Metric value={duration(query.mean_time_ms)} label="average" />
                <Metric value={duration(query.max_time_ms)} label="slowest" />
                {query.rows != null && <Metric value={count(query.rows / Math.max(1, query.executions))} label="rows per run" />}
                {query.logical_reads != null && <Metric value={count(query.logical_reads / Math.max(1, query.executions))} label={report.snapshot.server.engine === "mysql" ? "rows examined per run" : "pages read per run"} />}
              </div>
            </section>
            {insights.length > 0 && (
              <button className={`plan-teaser ${worst}`} onClick={() => choose("plan")}>
                <span className="pt-label">In the plan</span>
                <span className="pt-title">{insights[0]!.title}</span>
                {insights.length > 1 && <span className="muted">+{insights.length - 1} more</span>}
                <span className="pt-go">
                  Open the plan <span className="kbd">P</span>
                </span>
              </button>
            )}
            {w.timeline && query.trend && <QueryTrend query={query} timeline={w.timeline} onSince={compareSince} />}
            <RuntimeSpread query={query} engine={report.snapshot.server.engine} windowed={w.windowed} product={report.snapshot.server.product} kind={workloadKind(w)} />
            <QueryCostsPanel report={report} query={query} />
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
          </>
        )}
        {tab === "plan" && <QueryPlans key={query.id} report={report} query={query} focus={focus} planId={planId} setPlanId={setPlanId} />}
        {tab === "history" && <PlanHistory query={query} planId={planId} onOpen={(id) => (setPlanId(id), choose("plan"))} />}
      </div>
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

/** The plans the engine kept for this query, best first by average time, and when each was used. */
function PlanHistory({ query, planId, onOpen }: { query: QueryStat; planId: string | null; onOpen: (id: string) => void }) {
  if (query.plans.length === 0) {
    return <div className="empty" style={{ padding: 30 }}>This engine doesn't keep plan history for a query; the Plan tab shows its current plan.</div>;
  }
  const best = Math.min(...query.plans.map((v) => v.mean_time_ms));
  return (
    <section>
      <h3>
        Plans this query used <span className="muted">{query.plans.length}</span>
      </h3>
      <table className="grid history-table">
        <thead>
          <tr>
            <th>Plan</th>
            <th className="r">Average</th>
            <th className="r">Runs</th>
            <th>First seen</th>
            <th>Last seen</th>
            <th />
          </tr>
        </thead>
        <tbody>
          {query.plans.map((v, i) => (
            <tr key={v.id} className={`row ${planId === v.id ? "selected" : ""}`} onClick={() => onOpen(v.id)}>
              <td>
                <b>Plan {v.id}</b>
                {i === 0 && <span className="tag" style={{ marginLeft: 6 }}>most time</span>}
                {v.is_forced && <span className="tag medium" style={{ marginLeft: 6 }}>forced</span>}
              </td>
              <td className={`r ${query.plans.length > 1 && v.mean_time_ms > best * 2 ? "slow" : ""}`}>{duration(v.mean_time_ms)}</td>
              <td className="r">{count(v.executions)}</td>
              <td className="muted" title={v.first_seen ? seenAt(v.first_seen).full : undefined}>
                {v.first_seen ? seenAt(v.first_seen).short : "—"}
              </td>
              <td className="muted" title={v.last_seen ? seenAt(v.last_seen).full : undefined}>
                {v.last_seen ? seenAt(v.last_seen).short : "—"}
              </td>
              <td className="r">
                <span className="link">Open plan →</span>
              </td>
            </tr>
          ))}
        </tbody>
      </table>
    </section>
  );
}

type PlanState = { status: "idle" } | { status: "loading" } | { status: "ready"; plan: Plan } | { status: "error"; error: ApiError };

/** Every plan the engine kept for the query, its numbers, and the selected one drawn as a diagram or a tree. */
function QueryPlans(props: { report: Report; query: QueryStat; focus: number | null; planId: string | null; setPlanId: (id: string | null) => void }) {
  const { report, query, focus, planId, setPlanId } = props;
  const variants = query.plans;
  const dominant = variants[0]?.id ?? null;
  const [look, setLook] = useState<PlanLook>(rememberedLook);
  const [source, setSource] = useState<PlanSource>("plan");
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
    rememberLook(next);
  };
  const best = variants.length > 1 ? Math.min(...variants.map((v) => v.mean_time_ms)) : null;
  const [comparing, setComparing] = useState(false);
  const origin = `${connection} · query ${query.id}`;
  const nameOf = (id: string | null) => (id ? `Plan ${id}` : `Query ${query.id}`);

  /** The dominant plan next to the selected one (or the runner-up when the dominant one is selected), faster one first. */
  const compareVariants = async () => {
    const other = planId && planId !== dominant ? planId : variants[1]?.id;
    if (!dominant || !other) return;
    setComparing(true);
    try {
      const client = await api();
      const load = (id: string) =>
        id === dominant && query.plan ? Promise.resolve(query.plan) : state.status === "ready" && id === planId ? Promise.resolve(state.plan) : client.queryPlan(connection, query.id, id);
      const [first, second] = await Promise.all([load(dominant), load(other)]);
      // The faster plan is the baseline, so the comparison reads "the slow plan does X where the fast one does Y".
      const speed = (id: string) => variants.find((v) => v.id === id)?.mean_time_ms ?? 0;
      const a = { plan: first, name: nameOf(dominant), origin, source: { connection, queryId: query.id, planId: dominant } };
      const b = { plan: second, name: nameOf(other), origin, source: { connection, queryId: query.id, planId: other } };
      if (speed(dominant) <= speed(other)) compare(a, b);
      else compare(b, a);
    } catch (e) {
      toast(errorMessage(e).message);
    } finally {
      setComparing(false);
    }
  };

  return (
    <section>
      <h3 className="plan-bar">
        {variants.length > 1 ? (
          <>
            Plans <span className="muted">{variants.length}</span>
          </>
        ) : (
          <span className="muted plan-source">{query.plan?.kind === "actual" ? "Actual plan" : report.snapshot.server.engine === "mssql" ? (report.snapshot.workload?.source ?? "Plan") : "Estimated plan"}</span>
        )}
        <span className="actions">
          {variants.length > 1 && (
            <button className="btn ghost" disabled={comparing} title="Compare the plan that took the most time with the selected one" onClick={compareVariants}>
              {comparing ? <span className="spinner" style={{ width: 11, height: 11 }} /> : <GitCompareArrows size={13} />} Compare
            </button>
          )}
          {state.status === "ready" && (
            <button className="btn ghost" title="Open this plan in Plans to save it or compare it with another" onClick={() => addPlan(state.plan, nameOf(planId), origin, { source: { connection, queryId: query.id, planId } })}>
              <SquareArrowOutUpRight size={13} /> Open in Plans
            </button>
          )}
          {measuresRuns(report.snapshot.server.engine) && <PlanSourceToggle value={source} onChange={setSource} />}
          <LookToggle look={look} onChange={chooseLook} />
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
                {v.last_seen && (
                  <span className="muted" title={seenAt(v.last_seen).full}>
                    {" "}
                    · last {seenAt(v.last_seen).short}
                  </span>
                )}
              </span>
            </button>
          ))}
        </div>
      )}
      {source === "actual" && <LastActualRun connection={connection} engine={report.snapshot.server.engine} query={query} planId={planId} look={look} />}
      {source === "plan" && state.status === "loading" && (
        <div className="empty" style={{ padding: 30 }}>
          <div className="spinner" />
          Loading the plan…
        </div>
      )}
      {source === "plan" && state.status === "error" && (
        <div className="notice">
          <div>
            <b>No plan for this query.</b> {state.error.message}
            {state.error.hint && <div className="muted">{state.error.hint}</div>}
          </div>
        </div>
      )}
      {source === "plan" && state.status === "ready" && <PlanSurface key={`${query.id}-${planId}`} plan={state.plan} focus={planId === dominant ? focus : null} look={look} />}
    </section>
  );
}

/** A first or last seen time as people say it ("today 3:55 PM"); sources that write no zone mean UTC. */
function seenAt(text: string): { short: string; full: string } {
  const iso = /[zZ]$|[+-]\d\d:?\d\d$/.test(text) ? text : `${text.replace(" ", "T")}Z`;
  return when(iso);
}
