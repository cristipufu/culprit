import { ArrowRight, ChevronDown, ChevronRight, GitCompareArrows, SquarePen, X } from "lucide-react";
import { useEffect, useMemo, useRef, useState } from "react";
import type { ChangeRequest } from "../gen/ChangeRequest";
import type { Changes } from "../gen/Changes";
import type { ChangeStatus } from "../gen/ChangeStatus";
import type { PeriodStats } from "../gen/PeriodStats";
import type { QueryChange } from "../gen/QueryChange";
import type { Report } from "../gen/Report";
import { api, errorMessage, type ApiError } from "../lib/api";
import { count, duration, percent } from "../lib/format";
import { useHotkeys } from "../lib/hotkeys";
import { querySummary } from "../lib/query-summary";
import { changeRequest, setState } from "../lib/store";
import { rangeLabel, when } from "./TimeChart";

type State = { status: "loading" } | { status: "ready"; changes: Changes } | { status: "error"; error: ApiError };

const GROUPS: { status: ChangeStatus; label: string; hint: string; tone: string }[] = [
  { status: "regressed", label: "Got slower", hint: "Average time per run rose past the threshold, with enough runs in both periods.", tone: "high" },
  { status: "new", label: "New", hint: "Ran in the recent period and not in the baseline, which the source fully covers.", tone: "medium" },
  { status: "improved", label: "Got faster", hint: "Average time per run fell past the threshold.", tone: "good" },
  { status: "stopped", label: "Stopped running", hint: "Ran in the baseline and not since.", tone: "low" },
  { status: "not-comparable", label: "Not comparable", hint: "One period has no data the source can vouch for, so Culprit doesn't guess.", tone: "muted" },
  { status: "low-sample", label: "Too few runs", hint: "Not enough runs in one of the periods for a fair average.", tone: "muted" },
  { status: "unchanged", label: "About the same", hint: "Within the threshold.", tone: "muted" },
];

const PRESETS: { label: string; hours: number }[] = [
  { label: "Last hour", hours: 1 },
  { label: "Last 6 h", hours: 6 },
  { label: "Last 24 h", hours: 24 },
];

function hoursOf(p: { start: string; end: string }): number {
  return (Date.parse(p.end) - Date.parse(p.start)) / 3_600_000;
}

function spanWords(hours: number): string {
  if (hours < 1 / 60) return `${Math.max(1, Math.round(hours * 3600))} s`;
  if (hours < 1.5) return `${Math.round(hours * 60)} min`;
  if (hours < 48) return `${Math.round(hours)} h`;
  return `${Math.round(hours / 24)} days`;
}

/** One run's time with enough precision to compare two: "7.0 ms", not "7 ms". */
function runTime(ms: number): string {
  return ms >= 1 && ms < 10 ? `${ms.toFixed(1)} ms` : duration(ms);
}

function ratioWords(c: QueryChange): string | null {
  if (c.ratio == null) return null;
  const r = c.status === "improved" ? 1 / c.ratio : c.ratio;
  return r >= 10 ? `${Math.round(r)}×` : `${r.toFixed(1).replace(/\.0$/, "")}×`;
}

/** Query Store names its plans; scan-based sources only know the shapes Culprit captured. */
function namedPlans(changes: Changes): boolean {
  return changes.source === "query-store" && changes.engine === "mssql";
}

/** The sentence for a row, in local time; the CLI and agents get the same facts in UTC. */
function sentence(c: QueryChange, changes: Changes): string {
  const ratio = ratioWords(c);
  const s = c.plan_switch;
  const named = namedPlans(changes);
  const switched = s
    ? `${s.at ? ` at ${when(Date.parse(s.at))}` : ""}, when ${named ? `SQL Server switched from plan ${s.from} to plan ${s.to}` : "the plan Culprit captured changed"}`
    : "";
  switch (c.status) {
    case "regressed":
      return `Got ${ratio} slower${switched}${!s && c.same_plan ? `, with the same ${named ? "" : "estimated "}plan` : ""}.`;
    case "improved":
      return `Got ${ratio} faster${switched}.`;
    case "new":
      return `Started running: ${percent(c.recent_share)} of recent database time.`;
    case "stopped":
      return "Stopped running after the baseline.";
    case "not-comparable":
      return c.reason ? `Can't compare: ${c.reason}.` : "Can't compare the periods.";
    case "low-sample":
      return `Too few runs: ${count(c.baseline?.executions ?? 0)} before, ${count(c.recent?.executions ?? 0)} after.`;
    default:
      return "About the same.";
  }
}

/** Before → after on a shared log scale, so rows compare at a glance. */
function Dumbbell({ before, after, scale, tone }: { before?: PeriodStats | null; after?: PeriodStats | null; scale: [number, number]; tone: string }) {
  const [lo, hi] = scale;
  const pos = (ms: number) => {
    const v = Math.log10(Math.max(ms, lo));
    return Math.min(100, Math.max(0, ((v - Math.log10(lo)) / (Math.log10(hi) - Math.log10(lo) || 1)) * 100));
  };
  const a = before && before.executions > 0 ? pos(before.mean_ms) : null;
  const b = after && after.executions > 0 ? pos(after.mean_ms) : null;
  return (
    <div className={`dumbbell ${tone}`} aria-hidden="true">
      <div className="db-track" />
      {a != null && b != null && <div className="db-bar" style={{ left: `${Math.min(a, b)}%`, width: `${Math.abs(b - a)}%` }} />}
      {a != null && <i className="db-before" style={{ left: `${a}%` }} />}
      {b != null && <i className="db-after" style={{ left: `${b}%` }} />}
    </div>
  );
}

function toIsoLocal(value: string): string | null {
  const t = Date.parse(value);
  return Number.isFinite(t) ? new Date(t).toISOString() : null;
}

function toInputLocal(iso: string | null | undefined): string {
  if (!iso) return "";
  const d = new Date(iso);
  const pad = (n: number) => String(n).padStart(2, "0");
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}T${pad(d.getHours())}:${pad(d.getMinutes())}`;
}

export default function WhatChanged({ report, request, onClose }: { report: Report; request: ChangeRequest; onClose: () => void }) {
  const [state, setLocal] = useState<State>({ status: "loading" });
  const [open, setOpen] = useState<string | null>(null);
  const [cursor, setCursor] = useState(0);
  const [showUnchanged, setShowUnchanged] = useState(false);
  const list = useRef<HTMLDivElement>(null);
  const connection = report.snapshot.connection;
  const key = JSON.stringify(request);

  useEffect(() => {
    let cancelled = false;
    setLocal({ status: "loading" });
    setOpen(null);
    setCursor(0);
    api()
      .then((client) => client.changes(connection, request))
      .then((changes) => !cancelled && setLocal({ status: "ready", changes }))
      .catch((e) => !cancelled && setLocal({ status: "error", error: errorMessage(e) }));
    return () => {
      cancelled = true;
    };
    // `key` stands for the request's contents.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [connection, key]);

  const changes = state.status === "ready" ? state.changes : null;
  const rows = useMemo(() => (changes ? changes.queries.filter((q) => q.status !== "unchanged" || showUnchanged) : []), [changes, showUnchanged]);
  const scale = useMemo<[number, number]>(() => {
    const values = rows.flatMap((q) => [q.baseline?.mean_ms, q.recent?.mean_ms]).filter((v): v is number => v != null && v > 0);
    if (!values.length) return [0.01, 1000];
    return [Math.max(Math.min(...values) / 1.6, 0.005), Math.max(...values) * 1.6];
  }, [rows]);
  const known = useMemo(() => new Set(report.snapshot.workload?.queries.map((q) => q.id) ?? []), [report]);

  useHotkeys({
    j: () => setCursor((c) => Math.min(rows.length - 1, c + 1)),
    k: () => setCursor((c) => Math.max(0, c - 1)),
    ArrowDown: () => setCursor((c) => Math.min(rows.length - 1, c + 1)),
    ArrowUp: () => setCursor((c) => Math.max(0, c - 1)),
    Enter: () => rows[cursor] && setOpen((o) => (o === rows[cursor]!.query_id ? null : rows[cursor]!.query_id)),
    Escape: onClose,
  });
  useEffect(() => {
    list.current?.querySelector(".wc-row.cursor")?.scrollIntoView({ block: "nearest" });
  }, [cursor]);

  const setRequest = (next: Partial<ChangeRequest>) =>
    setState({ changes: changeRequest({ ...next, min_executions: request.min_executions, threshold: request.threshold }) });

  return (
    <div className="wc">
      <div className="wc-head">
        <div className="wc-title">
          <GitCompareArrows size={15} />
          <h2>What changed</h2>
          <span className="spacer" />
          <button className="icon-btn" title="Close (Esc)" onClick={onClose}>
            <X size={15} />
          </button>
        </div>
        <div className="wc-periods">
          {changes ? (
            <>
              <span className="wc-period recent">
                <b>Recent</b> {rangeLabel(Date.parse(changes.recent.start), Date.parse(changes.recent.end))} <span className="muted">· {spanWords(hoursOf(changes.recent))}</span>
              </span>
              <span className="muted">vs</span>
              <span className="wc-period">
                <b>Baseline</b> {rangeLabel(Date.parse(changes.baseline.start), Date.parse(changes.baseline.end))} <span className="muted">· {spanWords(hoursOf(changes.baseline))}</span>
              </span>
            </>
          ) : (
            <span className="muted">Comparing two periods…</span>
          )}
        </div>
        <div className="wc-controls">
          <span className="seg">
            {PRESETS.map((p) => (
              <button key={p.label} className={request.recent_hours === p.hours && !request.since ? "on" : ""} onClick={() => setRequest({ recent_hours: p.hours })}>
                {p.label}
              </button>
            ))}
          </span>
          <label className="wc-since">
            Since
            <input
              type="datetime-local"
              value={toInputLocal(request.since ?? null)}
              onChange={(e) => {
                const iso = toIsoLocal(e.target.value);
                if (iso) setRequest({ since: iso });
              }}
            />
          </label>
        </div>
      </div>

      {state.status === "loading" && (
        <div className="wc-body">
          {[0, 1, 2, 3].map((i) => (
            <div key={i} className="wc-skeleton" style={{ animationDelay: `${i * 80}ms` }} />
          ))}
        </div>
      )}
      {state.status === "error" && (
        <div className="wc-body">
          <div className="notice">
            <div>
              <b>{state.error.message}</b>
              {state.error.hint && <div className="muted">{state.error.hint}</div>}
            </div>
          </div>
        </div>
      )}
      {changes && (
        <div className="wc-body" ref={list}>
          <Summary changes={changes} />
          {GROUPS.map((g) => {
            const group = rows.filter((q) => q.status === g.status);
            if (!group.length) return null;
            return (
              <section key={g.status} className="wc-group">
                <h3 title={g.hint}>
                  <span className={`wc-dot ${g.tone}`} />
                  {g.label} <span className="muted">{group.length}</span>
                </h3>
                {group.map((q) => {
                  const index = rows.indexOf(q);
                  const expanded = open === q.query_id;
                  const summary = querySummary(q.text, q.object_name);
                  const switchTitle =
                    q.plan_switch?.what ?? (q.plan_switch?.from_label && q.plan_switch?.to_label ? `${q.plan_switch.from_label} → ${q.plan_switch.to_label}` : undefined);
                  return (
                    <div key={q.query_id} className={`wc-row ${index === cursor ? "cursor" : ""} ${expanded ? "open" : ""}`}>
                      <button
                        className="wc-main"
                        onClick={() => {
                          setCursor(index);
                          setOpen(expanded ? null : q.query_id);
                        }}
                        aria-expanded={expanded}
                      >
                        <span className="wc-caret">{expanded ? <ChevronDown size={13} /> : <ChevronRight size={13} />}</span>
                        <span className="wc-what">
                          <span className="qsum">
                            <b>{summary.verb}</b>
                            {summary.target && <span className="qsum-table">{summary.target}</span>}
                            {summary.predicate && <span className="qsum-pred">{summary.predicate}</span>}
                          </span>
                          <span className="wc-sentence">{sentence(q, changes)}</span>
                        </span>
                        <span className="wc-numbers">
                          <Dumbbell before={q.baseline} after={q.recent} scale={scale} tone={g.tone} />
                          <span className="wc-times">
                            {q.baseline ? runTime(q.baseline.mean_ms) : "—"}
                            <ArrowRight size={11} />
                            {q.recent ? runTime(q.recent.mean_ms) : "—"}
                          </span>
                        </span>
                        <span className="wc-side">
                          {ratioWords(q) && <span className={`wc-ratio ${g.tone}`}>{ratioWords(q)}</span>}
                          {q.plan_switch && (
                            <span className="wc-plan" title={switchTitle}>
                              {namedPlans(changes) ? `plan ${q.plan_switch.from} → ${q.plan_switch.to}` : "plan changed"}
                            </span>
                          )}
                        </span>
                      </button>
                      {expanded && <Detail q={q} changes={changes} inReport={known.has(q.query_id)} />}
                    </div>
                  );
                })}
              </section>
            );
          })}
          {changes.unchanged > 0 && (
            <button className="btn ghost wc-more" onClick={() => setShowUnchanged((v) => !v)}>
              {showUnchanged ? "Hide" : "Show"} {changes.unchanged} {changes.unchanged === 1 ? "query" : "queries"} that ran about the same
            </button>
          )}
          {rows.length === 0 && changes.unchanged === 0 && (
            <div className="empty" style={{ padding: 28 }}>
              <h3>Nothing to compare</h3>
              <p>No query ran in either period with enough runs.</p>
            </div>
          )}
          <footer className="wc-foot">
            <span>
              Averages are weighted by runs; a query needs {count(changes.min_executions)} runs in each period, and changes count from {changes.threshold}×.
            </span>
            {changes.notes
              // The period chips already say which scans the periods follow, in local time.
              .filter((n) => !n.startsWith("Periods follow Culprit's scans"))
              .map((n) => (
                <span key={n}>{n}</span>
              ))}
            <span className="muted">j/k to move · Enter to open · Esc to close</span>
          </footer>
        </div>
      )}
    </div>
  );
}

function Summary({ changes }: { changes: Changes }) {
  const rate = (s: PeriodStats, p: { start: string; end: string }) => s.total_ms / Math.max(hoursOf(p), 1 / 3600);
  const short = hoursOf(changes.baseline) < 1 || hoursOf(changes.recent) < 1;
  const before = rate(changes.baseline_total, changes.baseline);
  const after = rate(changes.recent_total, changes.recent);
  const ratio = before > 0 ? after / before : null;
  const tone = ratio == null ? "" : ratio >= 1.15 ? "high" : ratio <= 1 / 1.15 ? "good" : "";
  const counts = (status: ChangeStatus) => changes.queries.filter((q) => q.status === status).length;
  return (
    <div className="wc-summary">
      <div className="wc-stat">
        <span className="label">{short ? "Database time" : "Database time per hour"}</span>
        <span className="value">
          {short ? duration(changes.baseline_total.total_ms) : duration(before)}
          <ArrowRight size={13} />
          <b>{short ? duration(changes.recent_total.total_ms) : duration(after)}</b>
          {ratio != null && Number.isFinite(ratio) && <span className={`wc-ratio ${tone}`}>{ratio >= 10 ? Math.round(ratio) : ratio.toFixed(1)}× rate</span>}
        </span>
        {short && (
          <span className="hint">
            over {spanWords(hoursOf(changes.baseline))} before and {spanWords(hoursOf(changes.recent))} since
          </span>
        )}
      </div>
      <div className="wc-stat">
        <span className="label">Runs</span>
        <span className="value">
          {count(changes.baseline_total.executions)}
          <ArrowRight size={13} />
          <b>{count(changes.recent_total.executions)}</b>
        </span>
      </div>
      <div className="wc-stat">
        <span className="label">Queries</span>
        <span className="value wc-counts">
          <span className="high">{counts("regressed")} slower</span>
          <span className="medium">{counts("new")} new</span>
          <span className="good">{counts("improved")} faster</span>
        </span>
      </div>
      <p className="wc-source">
        {changes.source === "query-store" ? "From Query Store's intervals" : "From Culprit's own scans"}
        {changes.baseline_coverage < 0.99 && <span className="medium"> · the source covers {percent(changes.baseline_coverage)} of the baseline</span>}
      </p>
    </div>
  );
}

function Detail({ q, changes, inReport }: { q: QueryChange; changes: Changes; inReport: boolean }) {
  const row = (label: string, pick: (s: PeriodStats) => string) => (
    <tr>
      <th>{label}</th>
      <td>{q.baseline ? pick(q.baseline) : "—"}</td>
      <td>{q.recent ? pick(q.recent) : "—"}</td>
    </tr>
  );
  return (
    <div className="wc-detail">
      <table className="wc-table">
        <thead>
          <tr>
            <th />
            <th>Baseline</th>
            <th>Recent</th>
          </tr>
        </thead>
        <tbody>
          {row("Runs", (s) => count(s.executions))}
          {row("Average", (s) => runTime(s.mean_ms))}
          {(q.baseline?.min_ms != null || q.recent?.min_ms != null) && row("Fastest", (s) => (s.min_ms != null ? runTime(s.min_ms) : "—"))}
          {(q.baseline?.max_ms != null || q.recent?.max_ms != null) && row("Slowest", (s) => (s.max_ms != null ? runTime(s.max_ms) : "—"))}
          {row("Database time", (s) => duration(s.total_ms))}
        </tbody>
      </table>
      {q.plan_switch && (
        <div className="wc-switch">
          <b>{namedPlans(changes) ? `Plan ${q.plan_switch.from} → plan ${q.plan_switch.to}` : "The captured plan changed"}</b>
          {q.plan_switch.what && <span>{q.plan_switch.what}</span>}
          {!q.plan_switch.what && (q.plan_switch.from_label || q.plan_switch.to_label) && (
            <span>
              {q.plan_switch.from_label ?? "?"} → {q.plan_switch.to_label ?? "?"}
            </span>
          )}
        </div>
      )}
      <pre className="wc-sql">{q.text}</pre>
      <div className="wc-actions">
        <span className="muted">query {q.query_id}</span>
        <span className="spacer" />
        {inReport && (
          <button className="btn small" onClick={() => setState({ view: "queries", query: q.query_id })}>
            Open in Queries
          </button>
        )}
        <button className="btn small" onClick={() => setState({ view: "editor", editorSql: q.text })}>
          <SquarePen size={12} /> Open in editor
        </button>
      </div>
    </div>
  );
}
