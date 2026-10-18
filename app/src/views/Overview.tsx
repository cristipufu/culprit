import { GitCompareArrows, Search } from "lucide-react";
import { lazy, Suspense, useEffect, useMemo, useRef, useState } from "react";
import type { Category } from "../gen/Category";
import type { ChangeRequest } from "../gen/ChangeRequest";
import type { Finding } from "../gen/Finding";
import type { Report } from "../gen/Report";
import { ago, bytes, count, duration, percent } from "../lib/format";
import { windowPhrase, windowUnknown } from "../lib/rates";
import { useHotkeys } from "../lib/hotkeys";
import { changeRequest, setState, useStore } from "../lib/store";
import { planFlags, trendNote, WorkloadTrend } from "../components/Trends";
import { chartEvents } from "../components/ChangeTimeline";
import { MarkButton } from "../components/MarkButton";
import { useEvents } from "../lib/events";
import type { Flag, Range } from "../components/TimeChart";
import { CheckedFine } from "../components/CheckedFine";
import { WorkloadUnlock } from "../components/WorkloadUnlock";
import { HealthLine, WaitsVital, type WaitsTab } from "../components/Waits";
import { FindingDetail } from "./FindingDetail";
import { can } from "../lib/engines";

const CATEGORY_LABEL: Record<Category, string> = {
  indexes: "Indexes",
  queries: "Queries",
  maintenance: "Maintenance",
  configuration: "Configuration",
};

/** One chip per category that has findings, counted from the list itself so a chip's number is what it shows. */
function categoryCounts(findings: Finding[]): { category: Category; count: number }[] {
  const order = Object.keys(CATEGORY_LABEL) as Category[];
  return order.map((category) => ({ category, count: findings.filter((f) => f.category === category).length })).filter((c) => c.count > 0);
}

// The waits and "What changed" panels load on first use, so Overview's first paint stays as light as before.
const WaitsPanel = lazy(() => import("../components/WaitsPanel"));
const WhatChanged = lazy(() => import("../components/WhatChanged"));

/** Compare everything since `at` (a plan change, a deploy) with the same length of time before it. */
export function compareSince(at: number): void {
  setState({ view: "overview", changes: changeRequest({ since: new Date(at).toISOString() }) });
}

/** The recent period a request asks for, to show on the chart. */
function requestedRange(request: ChangeRequest | null, end: number): Range | null {
  if (!request) return null;
  if (request.recent_start && request.recent_end) return { from: Date.parse(request.recent_start), to: Date.parse(request.recent_end) };
  if (request.since) return { from: Date.parse(request.since), to: end };
  if (request.recent_hours) return { from: end - request.recent_hours * 3_600_000, to: end };
  return null;
}

export function Overview({ report }: { report: Report }) {
  const selected = useStore((s) => s.finding);
  const dismissed = useStore((s) => s.dismissed);
  const changes = useStore((s) => s.changes);
  const [category, setCategory] = useState<Category | "all">("all");
  const [filter, setFilter] = useState("");
  const [showDismissed, setShowDismissed] = useState(false);
  const searchRef = useRef<HTMLInputElement>(null);
  const listRef = useRef<HTMLDivElement>(null);
  const [waits, setWaits] = useState<{ tab: WaitsTab; category?: string } | null>(null);
  const openWaits = (tab: WaitsTab, category?: string) => setWaits({ tab, category });

  const visible = useMemo(() => {
    const q = filter.trim().toLowerCase();
    return report.findings.filter((f) => {
      if (category !== "all" && f.category !== category) return false;
      if (!showDismissed && dismissed[f.id]) return false;
      if (!q) return true;
      return `${f.title} ${f.subject.name} ${f.id}`.toLowerCase().includes(q);
    });
  }, [report.findings, category, filter, dismissed, showDismissed]);

  const dismissedCount = report.findings.filter((f) => dismissed[f.id]).length;
  // Chips count what their filter would list: dismissed findings only while they're shown.
  const listed = showDismissed ? report.findings : report.findings.filter((f) => !dismissed[f.id]);
  const current = report.findings.find((f) => f.id === selected) ?? visible[0] ?? null;
  // A source that keeps running totals (pg_stat_statements) is dated by Culprit's scans: the first one bounds when a
  // statement that hasn't run since last ran.
  const firstScan = report.snapshot.workload && !report.snapshot.workload.windowed ? (report.snapshot.workload.timeline?.starts[0] ?? null) : null;

  const move = (delta: number) => {
    if (visible.length === 0) return;
    const index = Math.max(0, visible.findIndex((f) => f.id === current?.id));
    const next = visible[Math.min(visible.length - 1, Math.max(0, index + delta))];
    if (next) setState({ finding: next.id });
  };

  useHotkeys({
    j: () => move(1),
    k: () => move(-1),
    ArrowDown: () => move(1),
    ArrowUp: () => move(-1),
    "/": () => searchRef.current?.focus(),
    // `c` copies the selected finding's fix; `v` reads "recent vs baseline".
    ...(can(report.snapshot.server.engine, "periods") ? { v: () => setState({ changes: changeRequest() }) } : {}),
  }, !waits && !changes);

  useEffect(() => {
    listRef.current?.querySelector(".frow.selected")?.scrollIntoView({ block: "nearest" });
  }, [current?.id]);

  return (
    <div className="overview">
      <div className="ov-list">
        <Header report={report} onOpenWaits={openWaits} changes={changes} />
        <div className="filters">
          <button className={`chip ${category === "all" ? "on" : ""}`} onClick={() => setCategory("all")}>
            All <span className="n">{listed.length}</span>
          </button>
          {categoryCounts(listed).map((c) => (
            <button key={c.category} className={`chip ${category === c.category ? "on" : ""}`} onClick={() => setCategory(c.category)}>
              {CATEGORY_LABEL[c.category]} <span className="n">{c.count}</span>
            </button>
          ))}
          <label className="search" style={{ minWidth: 120, maxWidth: 180 }}>
            <Search size={13} />
            <input
              ref={searchRef}
              value={filter}
              placeholder="Filter"
              onChange={(e) => setFilter(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Escape") {
                  setFilter("");
                  e.currentTarget.blur();
                }
              }}
            />
          </label>
        </div>
        <div className="list" ref={listRef}>
          <WorkloadUnlock report={report} />
          {visible.map((f, i) => (
            <div key={f.id}>
              {f.inactive && !visible[i - 1]?.inactive && (
                <div className="group-label eyebrow" title="These queries stopped running before the scan's data ends: a deploy may have changed or fixed them">
                  Stopped running
                </div>
              )}
              <FindingRow finding={f} selected={f.id === current?.id} dismissed={!!dismissed[f.id]} firstScan={firstScan} />
            </div>
          ))}
          {visible.length === 0 && (
            <div className="empty" style={{ padding: 32 }}>
              <h3>{report.findings.length === 0 ? "Nothing stands out" : "No findings match"}</h3>
              <p>{report.findings.length === 0 ? "The checks found nothing worth your time in this window." : "Clear the filter to see everything."}</p>
            </div>
          )}
          {dismissedCount > 0 && (
            <button className="btn ghost" style={{ margin: "8px 4px" }} onClick={() => setShowDismissed((v) => !v)}>
              {showDismissed ? "Hide" : "Show"} {dismissedCount} dismissed
            </button>
          )}
          {report.locked.length > 0 && <Locked report={report} />}
          <CheckedFine report={report} />
        </div>
      </div>
      {waits && (
        <Suspense fallback={null}>
          <WaitsPanel report={report} initialTab={waits.tab} initialCategory={waits.category} onClose={() => setWaits(null)} />
        </Suspense>
      )}
      {changes ? (
        <Suspense fallback={<div className="wc" />}>
          <WhatChanged report={report} request={changes} onClose={() => setState({ changes: null })} />
        </Suspense>
      ) : current ? (
        <FindingDetail key={current.id} report={report} finding={current} />
      ) : (
        <div className="empty">
          <h3>Select a finding</h3>
        </div>
      )}
    </div>
  );
}

function Header({
  report,
  onOpenWaits,
  changes,
}: {
  report: Report;
  onOpenWaits: (tab: WaitsTab, category?: string) => void;
  changes: ChangeRequest | null;
}) {
  const { snapshot, summary } = report;
  const w = snapshot.workload;
  const compare = () => setState({ changes: changeRequest() });
  const end = w?.timeline ? Date.parse(w.timeline.end) : Date.now();
  const start = w?.timeline?.starts[0] ? Date.parse(w.timeline.starts[0]) : NaN;
  // Marks, schema and settings changes, restarts and cloud throttling on a lane under the chart.
  const log = useEvents(w?.timeline ? snapshot.connection : null, start, end);
  const events = useMemo(
    () =>
      chartEvents(log?.events ?? [], (r) =>
        setState({
          changes: r.to
            ? changeRequest({ recent_start: new Date(r.from).toISOString(), recent_end: new Date(r.to).toISOString() })
            : changeRequest({ since: new Date(r.from).toISOString() }),
        }),
      ),
    [log],
  );
  const flags: Flag[] = w ? planFlags(w, compareSince) : [];
  if (changes?.since) flags.push({ at: Date.parse(changes.since), label: "Since", tone: "anchor" });
  // Under 20 hours of history a "per day" figure would be an extrapolation: show what was measured, and over how long.
  const short = w != null && w.window_hours < 20;
  const span = !w
    ? ""
    : w.log != null
      ? `${w.log} log${windowUnknown(w) ? "" : ` · ${compactSpan(w.window_hours)}`}`
      : w.windowed
        ? `last ${compactSpan(w.window_hours)}`
        : `${compactSpan(w.window_hours)} since reset`;
  return (
    <div className="ov-head">
      <h2>
        {summary.opportunities === 0 ? "No opportunities" : `${summary.opportunities} ${summary.opportunities === 1 ? "opportunity" : "opportunities"}`}
      </h2>
      <p>{headlineTail(report)}</p>
      <div className="vitals">
        <div className="vital">
          <div className="label">{short ? "Database time" : "Database time per day"}</div>
          <div className="value">{w ? duration(short ? w.total_time_ms : perDay(w.total_time_ms, w.window_hours)) : "—"}</div>
          <div className="hint" title={w ? `${count(w.query_count)} queries · ${count(w.total_executions)} runs · ${span}` : undefined}>
            {w ? (short ? span : `${span} · ${count(w.query_count)} queries · ${count(perDay(w.total_executions, w.window_hours))} runs/day`) : "No workload data"}
          </div>
        </div>
        <WaitsVital report={report} onOpen={onOpenWaits} />
        <div className="vital">
          <div className="label">Size</div>
          <div className="value">{bytes(snapshot.server.database_size_bytes)}</div>
          <div className="hint">
            {snapshot.tables.length} tables · {snapshot.indexes.length} indexes
          </div>
        </div>
      </div>
      <HealthLine report={report} onOpen={onOpenWaits} />
      {w?.timeline && w.timeline.starts.length >= 2 ? (
        <WorkloadTrend
          timeline={w.timeline}
          windowStart={w.window_start}
          flags={flags}
          onSelect={(range) =>
            setState({ changes: changeRequest({ recent_start: new Date(range.from).toISOString(), recent_end: new Date(range.to).toISOString() }) })
          }
          selection={requestedRange(changes, end)}
          onCompare={compare}
          events={events}
          actions={<MarkButton connection={snapshot.connection} />}
        />
      ) : w && trendNote(w) && can(snapshot.server.engine, "periods") ? (
        <p className="trend-note">
          {trendNote(w)}{" "}
          <button className="link-button" onClick={compare} title="Compare two periods (V)">
            <GitCompareArrows size={12} /> What changed?
          </button>
        </p>
      ) : null}
    </div>
  );
}

/** "2 min", "5 h", "3 days": short enough for a card's hint line. */
function compactSpan(hours: number): string {
  if (hours >= 47) return `${Math.round(hours / 24)} days`;
  if (hours >= 1.5) return `${Math.round(hours)} h`;
  return `${Math.max(1, Math.round(hours * 60))} min`;
}

function perDay(value: number, hours: number): number {
  return hours > 0 ? (value * 24) / hours : value;
}

function headlineTail(report: Report): string {
  const w = report.snapshot.workload;
  const share = report.summary.top_share;
  const span = w ? windowPhrase(w) : "";
  if (report.summary.opportunities === 0) return w ? `Nothing stands out over ${span}.` : "Nothing stands out.";
  if (w && share != null && share >= 0.05) return `The top ones touch ${percent(share)} of database time over ${span}.`;
  if (w) return `Based on ${w.source}, ${span}.`;
  return "Workload statistics are unavailable, so findings come from schema and index statistics.";
}

/** When an inactive finding's statements last ran. Unchanged since Culprit's first scan, that scan is only the latest
 * they can have run: they may have stopped long before, so it isn't "last ran". */
function lastRan(lastSeen: string, firstScan: string | null): string {
  const first = firstScan != null ? Date.parse(firstScan) : NaN;
  if (Math.abs(first - Date.parse(lastSeen)) < 1000) return `Not run since Culprit's first scan, ${ago(firstScan)}`;
  return `Last ran ${ago(lastSeen)}`;
}

function FindingRow({ finding, selected, dismissed, firstScan }: { finding: Finding; selected: boolean; dismissed: boolean; firstScan: string | null }) {
  const share = finding.impact.db_time_share;
  const fill = share != null ? Math.sqrt(share) : finding.impact.score / 100;
  return (
    <button
      className={`frow ${selected ? "selected" : ""} ${dismissed || finding.inactive ? "dismissed" : ""}`}
      onClick={() => setState({ finding: finding.id, changes: null })}
    >
      <span className={`sev ${finding.severity}`} />
      <span className="title">{finding.title}</span>
      <span className="impact">
        <span className="v">{share != null ? percent(share) : ""}</span>
        <span className="bar">
          <span style={{ width: `${Math.max(4, Math.min(100, fill * 100))}%`, background: `var(--${finding.severity})` }} />
        </span>
      </span>
      <span className="meta">
        {finding.inactive && finding.last_seen ? `${lastRan(finding.last_seen, firstScan)} · ` : ""}
        {CATEGORY_LABEL[finding.category]} · {finding.impact.label}
        {finding.subject.kind !== "query" ? ` · ${finding.subject.name}` : ""}
      </span>
    </button>
  );
}

function Locked({ report }: { report: Report }) {
  return (
    <div className="notice" style={{ margin: "12px 4px 4px" }}>
      <div>
        <b>
          {report.locked.length} {report.locked.length === 1 ? "check is" : "checks are"} locked.
        </b>{" "}
        {report.locked.map((l) => l.check.title).join(", ")} need{report.locked.length === 1 ? "s" : ""}{" "}
        {[...new Set(report.locked.flatMap((l) => l.missing))].join(", ")}.{" "}
        {report.snapshot.server.missing[0]?.fix && <span className="muted">{report.snapshot.server.missing[0].reason}</span>}
      </div>
    </div>
  );
}
