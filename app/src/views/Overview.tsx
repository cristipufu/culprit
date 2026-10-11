import { Search } from "lucide-react";
import { useEffect, useMemo, useRef, useState } from "react";
import type { Category } from "../gen/Category";
import type { Finding } from "../gen/Finding";
import type { Report } from "../gen/Report";
import type { WaitStat } from "../gen/WaitStat";
import { ago, bytes, count, duration, percent, windowLabel } from "../lib/format";
import { useHotkeys } from "../lib/hotkeys";
import { setState, useStore } from "../lib/store";
import { trendNote, WorkloadTrend } from "../components/Trends";
import { FindingDetail } from "./FindingDetail";

const CATEGORY_LABEL: Record<Category, string> = {
  indexes: "Indexes",
  queries: "Queries",
  maintenance: "Maintenance",
  configuration: "Configuration",
};

const WAIT_COLORS = ["#e0484e", "#e8930c", "#3b6fe8", "#2f9e66", "#8f3fbf", "#8d8a84"];

export function Overview({ report }: { report: Report }) {
  const selected = useStore((s) => s.finding);
  const dismissed = useStore((s) => s.dismissed);
  const [category, setCategory] = useState<Category | "all">("all");
  const [filter, setFilter] = useState("");
  const [showDismissed, setShowDismissed] = useState(false);
  const searchRef = useRef<HTMLInputElement>(null);
  const listRef = useRef<HTMLDivElement>(null);

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
  const current = report.findings.find((f) => f.id === selected) ?? visible[0] ?? null;

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
  });

  useEffect(() => {
    listRef.current?.querySelector(".frow.selected")?.scrollIntoView({ block: "nearest" });
  }, [current?.id]);

  return (
    <div className="overview">
      <div className="ov-list">
        <Header report={report} />
        <div className="filters">
          <button className={`chip ${category === "all" ? "on" : ""}`} onClick={() => setCategory("all")}>
            All <span className="n">{report.findings.length}</span>
          </button>
          {report.summary.by_category.map((c) => (
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
          {visible.map((f, i) => (
            <div key={f.id}>
              {f.inactive && !visible[i - 1]?.inactive && (
                <div className="group-label eyebrow" title="These queries stopped running before the scan's data ends, most likely changed by a deploy">
                  Stopped running
                </div>
              )}
              <FindingRow finding={f} selected={f.id === current?.id} dismissed={!!dismissed[f.id]} />
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
        </div>
      </div>
      {current ? (
        <FindingDetail key={current.id} report={report} finding={current} />
      ) : (
        <div className="empty">
          <h3>Select a finding</h3>
        </div>
      )}
    </div>
  );
}

function Header({ report }: { report: Report }) {
  const { snapshot, summary } = report;
  const w = snapshot.workload;
  const waits = topWaits(snapshot.waits);
  const totalWait = waits.reduce((s, x) => s + x.wait_ms, 0);
  const peakCpu = snapshot.resources.reduce<number | null>((m, r) => (r.cpu_pct != null ? Math.max(m ?? 0, r.cpu_pct) : m), null);
  return (
    <div className="ov-head">
      <h2>
        {summary.opportunities === 0 ? "No opportunities" : `${summary.opportunities} ${summary.opportunities === 1 ? "opportunity" : "opportunities"}`}
      </h2>
      <p>{headlineTail(report)}</p>
      <div className="vitals">
        <div className="vital">
          <div className="label">Database time per day</div>
          <div className="value">{w ? duration(perDay(w.total_time_ms, w.window_hours)) : "—"}</div>
          <div className="hint">{w ? `${count(w.query_count)} queries · ${count(perDay(w.total_executions, w.window_hours))} runs/day` : "No workload data"}</div>
        </div>
        <div className="vital">
          <div className="label">{totalWait > 0 ? "Mostly waiting on" : peakCpu != null ? "CPU, last hour" : "Data window"}</div>
          {totalWait > 0 ? (
            <>
              <div className="value">
                {waits[0]?.category} {percent((waits[0]?.wait_ms ?? 0) / totalWait)}
              </div>
              <div className="waitbar">
                {waits.map((x, i) => (
                  <span key={x.category} title={`${x.category} ${percent(x.wait_ms / totalWait)}`} style={{ flex: x.wait_ms, background: WAIT_COLORS[i % WAIT_COLORS.length] }} />
                ))}
              </div>
            </>
          ) : peakCpu != null ? (
            <>
              <div className="value">peak {Math.round(peakCpu)}%</div>
              <div className="hint">from resource stats</div>
            </>
          ) : (
            <>
              <div className="value">{w ? windowLabel(w.window_hours).replace("last ", "") : "—"}</div>
              <div className="hint">{w?.source ?? "statistics only"}</div>
            </>
          )}
        </div>
        <div className="vital">
          <div className="label">Size</div>
          <div className="value">{bytes(snapshot.server.database_size_bytes)}</div>
          <div className="hint">
            {snapshot.tables.length} tables · {snapshot.indexes.length} indexes
          </div>
        </div>
      </div>
      {w?.timeline && w.timeline.starts.length >= 2 ? <WorkloadTrend timeline={w.timeline} /> : w && trendNote(w) ? <p className="trend-note">{trendNote(w)}</p> : null}
    </div>
  );
}

function perDay(value: number, hours: number): number {
  return hours > 0 ? (value * 24) / hours : value;
}

function headlineTail(report: Report): string {
  const w = report.snapshot.workload;
  const share = report.summary.top_share;
  const span = w ? (w.windowed ? `the ${windowLabel(w.window_hours)}` : `the ${windowLabel(w.window_hours).replace("last ", "")} since statistics were reset`) : "";
  if (report.summary.opportunities === 0) return w ? `Nothing stands out over ${span}.` : "Nothing stands out.";
  if (w && share != null && share >= 0.05) return `The top ones touch ${percent(share)} of database time over ${span}.`;
  if (w) return `Based on ${w.source}, ${span}.`;
  return "Workload statistics are unavailable, so findings come from schema and index statistics.";
}

function topWaits(waits: WaitStat[]): WaitStat[] {
  const byCategory = new Map<string, number>();
  waits.forEach((w) => byCategory.set(w.category, (byCategory.get(w.category) ?? 0) + w.wait_ms));
  return [...byCategory.entries()]
    .map(([category, wait_ms]) => ({ category, wait_ms, wait_type: null, tasks: null }))
    .filter((w) => w.wait_ms > 0)
    .sort((a, b) => b.wait_ms - a.wait_ms)
    .slice(0, 6);
}

function FindingRow({ finding, selected, dismissed }: { finding: Finding; selected: boolean; dismissed: boolean }) {
  const share = finding.impact.db_time_share;
  const fill = share != null ? Math.sqrt(share) : finding.impact.score / 100;
  return (
    <button className={`frow ${selected ? "selected" : ""} ${dismissed || finding.inactive ? "dismissed" : ""}`} onClick={() => setState({ finding: finding.id })}>
      <span className={`sev ${finding.severity}`} />
      <span className="title">{finding.title}</span>
      <span className="impact">
        <span className="v">{share != null ? percent(share) : ""}</span>
        <span className="bar">
          <span style={{ width: `${Math.max(4, Math.min(100, fill * 100))}%`, background: `var(--${finding.severity})` }} />
        </span>
      </span>
      <span className="meta">
        {finding.inactive && finding.last_seen ? `Last ran ${ago(finding.last_seen)} · ` : ""}
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
