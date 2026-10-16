import { ChevronRight, Copy, FileText, GitCompareArrows, Radio, RefreshCw, SquarePen } from "lucide-react";
import { lazy, Suspense, useEffect, useMemo, useRef, useState, type KeyboardEvent, type MouseEvent, type ReactNode } from "react";
import type { ConnectionInfo } from "../gen/ConnectionInfo";
import type { Dashboard as DashboardModel } from "../gen/Dashboard";
import type { LiveState } from "../gen/LiveState";
import type { QueryStat } from "../gen/QueryStat";
import type { Report } from "../gen/Report";
import type { Tile } from "../gen/Tile";
import type { TileItem } from "../gen/TileItem";
import type { TileKey } from "../gen/TileKey";
import type { TileStatus } from "../gen/TileStatus";
import { chartEvents } from "../components/ChangeTimeline";
import { DashboardSkeleton } from "../components/DashboardSkeleton";
import { MarkButton } from "../components/MarkButton";
import { planFlags, Sparkline, trendNote, WorkloadTrend } from "../components/Trends";
import type { WaitsTab } from "../components/Waits";
import { api, errorMessage, type ApiError } from "../lib/api";
import { can, serverName } from "../lib/engines";
import { useEvents } from "../lib/events";
import { attention, nowTile, nowUnavailable, STATUS_LABEL } from "../lib/dashboard";
import { ago, duration, percent, when } from "../lib/format";
import { useHotkeys } from "../lib/hotkeys";
import { querySummary } from "../lib/query-summary";
import { changeRequest, scan, setState, useStore } from "../lib/store";
import { waitColor } from "../lib/waits";
import { openLive } from "./Live";
import { compareSince } from "./Overview";
import { keys } from "../lib/keys";
import { copyAndTell } from "../lib/clipboard";

const WaitsPanel = lazy(() => import("../components/WaitsPanel"));

/** Sessions are read this often while the dashboard is on screen; never while the window is hidden. */
const REFRESH_MS = 5000;
/** Five minutes of reads for the activity line. */
const HISTORY = Math.round((5 * 60 * 1000) / REFRESH_MS);

type Sample = { at: number; active: number; blocked: number };
type Loaded = { status: "loading" } | { status: "ready"; model: DashboardModel } | { status: "error"; error: ApiError };

/** Open a query in Queries, on one of its tabs. */
function openQuery(id: string, tab?: "summary" | "plan" | "history"): void {
  if (tab) {
    try {
      localStorage.setItem("culprit.query.tab", tab);
    } catch {}
  }
  setState({ view: "queries", query: id });
}

function openFinding(id: string): void {
  setState({ view: "overview", finding: id, changes: null });
}

/**
 * One database at a glance: fourteen tiles, each a plain sentence, a small honest picture and a door to the view that
 * explains it. Composed from the latest scan by the same code as `culprit dashboard`; sessions are read live.
 */
export default function Dashboard({ report, connection }: { report: Report; connection: ConnectionInfo }) {
  const windowMinutes = useStore((s) => s.windowMinutes);
  const scanning = useStore((s) => s.report.status === "ready" && s.report.scanning);
  const [loaded, setLoaded] = useState<Loaded>({ status: "loading" });
  const [waits, setWaits] = useState<{ tab: WaitsTab; category?: string } | null>(null);
  const live = useLiveNow(connection.name, can(connection.engine, "sessions"));
  const gridRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    let current = true;
    api()
      .then((client) => client.dashboard(connection.name, windowMinutes))
      .then((model) => current && setLoaded({ status: "ready", model }))
      .catch((e) => current && setLoaded({ status: "error", error: errorMessage(e) }));
    return () => {
      current = false;
    };
  }, [connection.name, windowMinutes, report.snapshot.id]);

  const tiles = useMemo(() => {
    if (loaded.status !== "ready") return null;
    const now = live.state ? nowTile(live.state) : live.error ? nowUnavailable(live.error.message) : null;
    return loaded.model.tiles.map((t) => (t.key === "now" && now ? now : t));
  }, [loaded, live.state, live.error]);

  // From nowhere in particular, j or ↓ steps into the grid; inside it, the grid's own keys take over.
  const enter = () => {
    if (document.activeElement && document.activeElement !== document.body) return;
    gridRef.current?.querySelector<HTMLElement>("[data-tile]")?.focus();
  };
  const periods = can(connection.engine, "periods");
  useHotkeys({ ...(periods ? { v: () => setState({ view: "overview", changes: changeRequest() }) } : {}), j: enter, ArrowDown: enter }, !waits);

  if (loaded.status === "error") {
    return (
      <div className="empty">
        <h3>Couldn't build the dashboard</h3>
        <p>{loaded.error.message}</p>
        {loaded.error.hint && <p className="muted">{loaded.error.hint}</p>}
        <button className="btn" onClick={() => setState({ view: "overview" })}>
          Open findings
        </button>
      </div>
    );
  }
  if (!tiles || loaded.status !== "ready") return <DashboardSkeleton />;

  const model = loaded.model;
  const { keys, headline } = attention(tiles);
  // A dashboard built by an older Culprit lacks newer tiles: they say so instead of breaking the page.
  const tile = (key: TileKey): Tile =>
    tiles.find((t) => t.key === key) ?? {
      key,
      title: key.replace(/-/g, " ").replace(/^./, (c) => c.toUpperCase()),
      status: "unavailable",
      takeaway: "Scan again to fill this tile.",
      detail: null,
      metrics: [],
      items: [],
      fix_sql: null,
      next: null,
    };
  const noWorkload = !report.snapshot.workload;
  const worst: TileStatus = keys[0] ? tile(keys[0]).status : "good";
  const focusTile = (key: TileKey) => gridRef.current?.querySelector<HTMLElement>(`[data-tile="${key}"]`)?.focus();
  const openWaits = (tab: WaitsTab, category?: string) => setWaits({ tab, category });

  const onGridKey = (e: KeyboardEvent<HTMLDivElement>) => {
    const target = e.target as HTMLElement;
    if (target.closest("input, textarea, select, [contenteditable=true]")) return;
    const all = [...(gridRef.current?.querySelectorAll<HTMLElement>("[data-tile]") ?? [])];
    const from = target.closest<HTMLElement>("[data-tile]");
    const next = nextTile(all, from, e.key);
    if (next) {
      e.preventDefault();
      next.focus();
    }
  };

  return (
    <div className="dash" onKeyDown={onGridKey}>
      <header className="dash-head">
        <div className="dash-headline">
          <span className={`dash-pulse ${worst}`} aria-hidden />
          <h2>{headline}</h2>
        </div>
        <div className="dash-sub">
          {keys.length > 0 && (
            <div className="dash-attn" role="list" aria-label="Needs a look">
              {keys.map((k) => (
                <button key={k} role="listitem" className={`dash-chip ${tile(k).status}`} onClick={() => focusTile(k)} title={tile(k).takeaway}>
                  <i aria-hidden /> {tile(k).title}
                </button>
              ))}
            </div>
          )}
          <span className="dash-meta">
            {model.coverage ? `${model.coverage} · ` : ""}scanned {ago(model.taken_at)}
            {scanning ? " · scanning…" : ""}
          </span>
        </div>
      </header>

      <div className={`dash-grid ${noWorkload ? "no-workload" : ""}`} ref={gridRef}>
        <DbTimeTile tile={tile("db-time")} report={report} />
        <FindingsTile tile={tile("findings")} report={report} />
        <TopQueriesTile tile={tile("top-queries")} report={report} />
        <WaitsTile tile={tile("waits")} onOpen={openWaits} sample={can(connection.engine, "wait_sample")} />
        <NowTile tile={tile("now")} live={live} sessions={can(connection.engine, "sessions")} />
        <HealthTile tile={tile("health")} onOpen={() => openWaits("health")} />
        <DeadlocksTile tile={tile("deadlocks")} report={report} />
        <IndexesTile tile={tile("indexes")} />
        <PlansTile tile={tile("plans")} />
        <SignalsTile tile={tile("storage")} action="Storage" onOpen={() => openWaits("health")} />
        <SignalsTile tile={tile("connections")} action="All signals" onOpen={() => openWaits("health")} />
        <SignalsTile tile={tile("replication")} action="Replication" onOpen={() => openWaits("health")} />
        <SignalsTile
          tile={tile("failures")}
          report={report}
          action="Findings"
          onOpen={() => setState({ view: "overview" })}
          onRow={(item) => (item.query_id ? openQuery(item.query_id) : setState({ view: "overview" }))}
        />
        <StatisticsTile tile={tile("statistics")} report={report} />
      </div>

      {waits && (
        <Suspense fallback={null}>
          <WaitsPanel report={report} initialTab={waits.tab} initialCategory={waits.category} onClose={() => setWaits(null)} />
        </Suspense>
      )}
    </div>
  );
}

/** Arrow keys move to the nearest tile in that direction; j/k and Home/End walk them in reading order. */
function nextTile(all: HTMLElement[], from: HTMLElement | null, key: string): HTMLElement | null {
  if (all.length === 0) return null;
  if (!from) return ["ArrowDown", "ArrowRight", "j", "Home"].includes(key) ? all[0]! : key === "End" ? all[all.length - 1]! : null;
  const index = all.indexOf(from);
  if (key === "j") return all[Math.min(all.length - 1, index + 1)]!;
  if (key === "k") return all[Math.max(0, index - 1)]!;
  if (key === "Home") return all[0]!;
  if (key === "End") return all[all.length - 1]!;
  const dir = { ArrowRight: [1, 0], ArrowLeft: [-1, 0], ArrowDown: [0, 1], ArrowUp: [0, -1] }[key];
  if (!dir) return null;
  const a = from.getBoundingClientRect();
  const ax = a.left + a.width / 2;
  const ay = a.top + a.height / 2;
  let best: HTMLElement | null = null;
  let bestScore = Infinity;
  for (const el of all) {
    if (el === from) continue;
    const b = el.getBoundingClientRect();
    const dx = b.left + b.width / 2 - ax;
    const dy = b.top + b.height / 2 - ay;
    const along = dx * dir[0]! + dy * dir[1]!;
    if (along <= 4) continue;
    // Straight ahead beats diagonal: sideways distance counts double.
    const across = Math.abs(dx * dir[1]! + dy * dir[0]!);
    const score = along + across * 2;
    if (score < bestScore) {
      bestScore = score;
      best = el;
    }
  }
  return best;
}

// ───────── tile frame ─────────

type FrameProps = {
  tile: Tile;
  /** What clicking the tile (or Enter on it) opens, in a few words. */
  action?: string;
  onOpen?: () => void;
  className?: string;
  children?: ReactNode;
  /** Shown instead of the detail line. */
  foot?: ReactNode;
};

/** Every tile: status and title, the takeaway first, then the picture, then one quiet line. */
function Frame({ tile, action, onOpen, className = "", children, foot }: FrameProps) {
  const open = (e: MouseEvent) => {
    if (!onOpen) return;
    // Rows, chips and charts inside the tile do their own thing.
    if ((e.target as HTMLElement).closest("button, a, [data-own-click]")) return;
    onOpen();
  };
  const titleId = `dash-${tile.key}-title`;
  return (
    <section
      className={`dash-tile t-${tile.key} ${tile.status} ${onOpen ? "openable" : ""} ${className}`}
      data-tile={tile.key}
      tabIndex={0}
      aria-labelledby={titleId}
      aria-describedby={`dash-${tile.key}-takeaway`}
      onClick={open}
      onKeyDown={(e) => {
        if (onOpen && e.target === e.currentTarget && (e.key === "Enter" || e.key === " ")) {
          e.preventDefault();
          onOpen();
        }
      }}
    >
      <header className="dash-tile-head">
        <span className={`dash-dot ${tile.status}`} title={STATUS_LABEL[tile.status]} aria-label={STATUS_LABEL[tile.status]} role="img" />
        <h3 id={titleId}>{tile.title}</h3>
        {onOpen && action && (
          <button className="dash-open" onClick={onOpen} tabIndex={-1} aria-hidden>
            {action} <ChevronRight size={12} />
          </button>
        )}
      </header>
      <p className="dash-takeaway" id={`dash-${tile.key}-takeaway`}>
        {tile.takeaway}
      </p>
      {children}
      {foot !== undefined ? foot : tile.detail ? <p className="dash-detail">{tile.detail}</p> : null}
    </section>
  );
}

function metricText(tile: Tile, key: string): string | null {
  return tile.metrics.find((m) => m.key === key)?.text ?? null;
}

function metricValue(tile: Tile, key: string): number {
  return tile.metrics.find((m) => m.key === key)?.value ?? 0;
}

function Fix({ sql }: { sql: string }) {
  return (
    <div className="dash-fix" data-own-click>
      <code>{sql}</code>
      <button className="btn small" onClick={() => copyAndTell(sql, "Copied")} aria-label="Copy the statement">
        <Copy size={12} /> Copy
      </button>
    </div>
  );
}

// ───────── database time ─────────

function DbTimeTile({ tile, report }: { tile: Tile; report: Report }) {
  const w = report.snapshot.workload;
  const compare = () => setState({ view: "overview", changes: changeRequest() });
  // The same change lane as Findings' chart: marks, schema and settings changes, restarts, cloud throttling.
  const end = w?.timeline ? Date.parse(w.timeline.end) : Date.now();
  const start = w?.timeline?.starts[0] ? Date.parse(w.timeline.starts[0]) : NaN;
  const log = useEvents(w?.timeline ? report.snapshot.connection : null, start, end);
  const events = useMemo(
    () =>
      chartEvents(log?.events ?? [], (r) =>
        setState({
          view: "overview",
          changes: r.to
            ? changeRequest({ recent_start: new Date(r.from).toISOString(), recent_end: new Date(r.to).toISOString() })
            : changeRequest({ since: new Date(r.from).toISOString() }),
        }),
      ),
    [log],
  );
  if (!w || tile.status === "unavailable") return <Frame tile={tile} />;
  // SQLite keeps no history: a query log is one period, with nothing to compare it to.
  const periods = can(report.snapshot.server.engine, "periods");
  const flags = planFlags(w, compareSince);
  const hasTrend = !!w.timeline && w.timeline.starts.length >= 2;
  const stats = tile.metrics.filter((m) => m.key !== "window-hours");
  return (
    <Frame tile={tile} action={periods ? "What changed" : undefined} onOpen={periods ? compare : undefined} foot={tile.items.length > 0 || !hasTrend ? null : undefined}>
      <div className="dash-stats">
        {stats.map((m) => (
          <div key={m.key} className="dash-stat">
            <b>{m.text}</b>
            <span>{m.label}</span>
          </div>
        ))}
      </div>
      {hasTrend && w.timeline ? (
        <div className="dash-chart" data-own-click>
          <WorkloadTrend
            timeline={w.timeline}
            windowStart={w.window_start}
            flags={flags}
            onSelect={(range) =>
              setState({
                view: "overview",
                changes: changeRequest({ recent_start: new Date(range.from).toISOString(), recent_end: new Date(range.to).toISOString() }),
              })
            }
            onCompare={compare}
            events={events}
            actions={<MarkButton connection={report.snapshot.connection} />}
          />
        </div>
      ) : (
        // Where the chart will be: why there's none yet, in the chart's place, so every engine's tile has the same shape.
        <div className="dash-chart-empty">
          <p>
            {tile.detail ??
              (periods ? (trendNote(w) ?? "Not enough history for a trend yet.") : `${serverName(report.snapshot.server.engine)} keeps no history to chart: the log Culprit read is one period.`)}
          </p>
          {periods && (
            <button className="btn small" onClick={compare} title="Compare two periods (V)">
              <GitCompareArrows size={12} /> What changed?
            </button>
          )}
        </div>
      )}
      {tile.items.length > 0 && (
        <ul className="dash-rows compact">
          {tile.items.slice(0, 2).map((item) => (
            <li key={item.id + item.query_id}>
              <button className="dash-row" onClick={() => item.query_id && openQuery(item.query_id, "history")} title="Open the query's plan history">
                <i className="dash-flag" aria-hidden />
                <span className="dash-row-label">
                  {queryLabel(item, report)} <span className="muted">switched plans{item.at ? ` ${ago(item.at)}` : ""}</span>
                </span>
                <span className="dash-row-value">{item.value}</span>
              </button>
            </li>
          ))}
        </ul>
      )}
    </Frame>
  );
}

// ───────── findings ─────────

function FindingsTile({ tile, report }: { tile: Tile; report: Report }) {
  // Findings you dismissed stay out of the tile, as they do in the list; the takeaway still describes the whole scan.
  const dismissed = useStore((s) => s.dismissed);
  const hidden = report.findings.filter((f) => !f.inactive && dismissed[f.id]).length;
  const open = report.findings.filter((f) => !f.inactive && !dismissed[f.id]);
  const counts = (["high", "medium", "low"] as const).map((s) => ({
    s,
    n: hidden > 0 ? open.filter((f) => f.severity === s).length : metricValue(tile, s),
  }));
  const total = counts.reduce((a, c) => a + c.n, 0);
  const rows: { id: string; label: string; value: string | null; status: TileStatus | null }[] =
    hidden > 0
      ? open.slice(0, 3).map((f) => ({
          id: f.id,
          label: f.title,
          value: f.impact.label,
          status: f.severity === "high" ? "bad" : f.severity === "medium" ? "watch" : "info",
        }))
      : tile.items.map((i) => ({ id: i.id, label: i.label, value: i.value, status: i.status }));
  return (
    <Frame tile={tile} action="All findings" onOpen={() => setState({ view: "overview", changes: null })}>
      {total > 0 && (
        <div className="dash-sevbar" role="img" aria-label={counts.map((c) => `${c.n} ${c.s}`).join(", ")}>
          <div className="dash-sevbar-track">
            {counts.map((c) => (c.n > 0 ? <span key={c.s} className={c.s} style={{ flexGrow: c.n }} /> : null))}
          </div>
          <div className="dash-sevbar-legend">
            {counts.map((c) => (
              <span key={c.s} className={c.n === 0 ? "zero" : ""}>
                <i className={`sev ${c.s}`} /> {c.n} {c.s}
              </span>
            ))}
          </div>
        </div>
      )}
      {rows.length > 0 && (
        <ul className="dash-rows">
          {rows.map((item) => (
            <li key={item.id}>
              <button className="dash-row two-line" onClick={() => openFinding(item.id)}>
                <span className={`sev ${severityOf(item.status)}`} aria-hidden />
                <span className="dash-row-stack">
                  <span className="dash-row-label clamp">{item.label}</span>
                  <span className="dash-row-meta">{item.value}</span>
                </span>
              </button>
            </li>
          ))}
        </ul>
      )}
      {hidden > 0 && <p className="dash-detail">{hidden === 1 ? "1 dismissed finding is" : `${hidden} dismissed findings are`} left out here.</p>}
    </Frame>
  );
}

function severityOf(status: TileStatus | null): "high" | "medium" | "low" {
  return status === "bad" ? "high" : status === "watch" ? "medium" : "low";
}

// ───────── top queries ─────────

function queryLabel(item: TileItem, report: Report): ReactNode {
  const q = item.query_id ? report.snapshot.workload?.queries.find((x) => x.id === item.query_id) : undefined;
  if (!q) return item.label;
  const s = querySummary(q.text, q.object_name);
  return (
    <>
      <span className="dash-verb">{s.verb}</span> {s.target ?? ""}
      {s.predicate && <span className="dash-pred"> · {s.predicate}</span>}
    </>
  );
}

function TopQueriesTile({ tile, report }: { tile: Tile; report: Report }) {
  const w = report.snapshot.workload;
  if (!w || tile.status === "unavailable") return <Frame tile={tile} />;
  const byId = new Map<string, QueryStat>(w.queries.map((q) => [q.id, q]));
  const top = tile.items[0]?.query_id;
  return (
    <Frame tile={tile} action="All queries" onOpen={() => (top ? openQuery(top) : setState({ view: "queries" }))}>
      <ul className="dash-rows">
        {tile.items.map((item) => {
          const q = item.query_id ? byId.get(item.query_id) : undefined;
          return (
            <li key={item.id}>
              <button className="dash-row query" onClick={() => item.query_id && openQuery(item.query_id)} title={q?.text.slice(0, 400)}>
                <span className="dash-row-label">{queryLabel(item, report)}</span>
                {q && w.timeline && <Sparkline query={q} timeline={w.timeline} />}
                <span className="dash-share" aria-hidden>
                  <span style={{ width: `${Math.max(3, (item.share ?? 0) * 100)}%` }} />
                </span>
                <span className="dash-row-value" title={q ? `${duration(q.mean_time_ms)} per run` : undefined}>
                  {item.value}
                </span>
              </button>
            </li>
          );
        })}
      </ul>
    </Frame>
  );
}

// ───────── waits ─────────

function WaitsTile({ tile, onOpen, sample }: { tile: Tile; onOpen: (tab: WaitsTab, category?: string) => void; sample: boolean }) {
  if (tile.status === "unavailable" && !sample) {
    return <Frame tile={tile}>{tile.fix_sql && <Fix sql={tile.fix_sql} />}</Frame>;
  }
  if (tile.status === "unavailable") {
    return (
      <Frame tile={tile} action="Sample now" onOpen={() => onOpen("now")}>
        <div className="dash-actions">
          <button className="btn small" onClick={() => onOpen("now")}>
            <Radio size={12} /> Sample what it waits on now
          </button>
        </div>
        {tile.fix_sql && <Fix sql={tile.fix_sql} />}
      </Frame>
    );
  }
  const items = tile.items;
  const top = items.find((i) => i.id !== "other") ?? items[0];
  return (
    <Frame tile={tile} action="Where time goes" onOpen={() => onOpen("waits", top?.id)}>
      <div className="dash-stack" role="img" aria-label={items.map((i) => `${i.label} ${i.value}`).join(", ")}>
        {items.map((i) => (
          <span
            key={i.id}
            style={{ flexGrow: Math.max(i.share ?? 0, 0.004), background: waitColor(i.id) }}
            title={`${i.label} · ${i.value}`}
            data-own-click
            onClick={() => onOpen("waits", i.id)}
          />
        ))}
      </div>
      <ul className="dash-legend">
        {items.slice(0, 6).map((i) => (
          <li key={i.id}>
            <button onClick={() => onOpen("waits", i.id)}>
              <i style={{ background: waitColor(i.id) }} aria-hidden />
              <span className="dash-legend-label">{i.label}</span>
              <span className="dash-row-value">{i.value}</span>
            </button>
          </li>
        ))}
      </ul>
    </Frame>
  );
}

// ───────── right now ─────────

type LiveNow = { state: LiveState | null; error: ApiError | null; hidden: boolean; history: Sample[]; age: number | null };

/** Sessions every few seconds while the dashboard is on screen; nothing while the window is hidden. */
/** Sessions right now, read every few seconds while the window is visible; never for an engine without sessions. */
function useLiveNow(connection: string, enabled: boolean): LiveNow {
  const [state, setLive] = useState<LiveState | null>(null);
  const [error, setError] = useState<ApiError | null>(null);
  const [hidden, setHidden] = useState(() => typeof document !== "undefined" && document.hidden);
  const [history, setHistory] = useState<Sample[]>([]);
  const [now, setNow] = useState(() => Date.now());

  useEffect(() => {
    const onVisibility = () => setHidden(document.hidden);
    document.addEventListener("visibilitychange", onVisibility);
    return () => document.removeEventListener("visibilitychange", onVisibility);
  }, []);

  useEffect(() => {
    if (hidden) return;
    const timer = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(timer);
  }, [hidden]);

  const hasRead = useRef(false);
  hasRead.current = state !== null || error !== null;
  useEffect(() => {
    // The first read always happens, so the tile has something to say; after that, nothing while hidden.
    if (!enabled || (hidden && hasRead.current)) return;
    let cancelled = false;
    let timer: ReturnType<typeof setTimeout>;
    const tick = async () => {
      try {
        const next = await (await api()).live(connection);
        if (cancelled) return;
        setLive(next);
        setError(null);
        const active = next.sessions.filter((s) => s.state !== "idle" && s.state !== "sleeping" && !s.state.startsWith("idle in transaction")).length;
        const blocked = next.sessions.filter((s) => s.blocked_by).length;
        setHistory((h) => [...h.slice(-(HISTORY - 1)), { at: Date.now(), active, blocked }]);
      } catch (e) {
        const error = errorMessage(e);
        if (!cancelled) setError(error);
        if (error.code === "unsupported") return;
      }
      if (!cancelled && !document.hidden) timer = setTimeout(tick, REFRESH_MS);
    };
    tick();
    return () => {
      cancelled = true;
      clearTimeout(timer);
    };
  }, [connection, hidden, enabled]);

  const age = state ? Math.max(0, Math.round((now - Date.parse(state.taken_at)) / 1000)) : null;
  return { state, error, hidden, history, age };
}

function NowTile({ tile, live, sessions }: { tile: Tile; live: LiveNow; sessions: boolean }) {
  // An engine without sessions (SQLite): the scan's own word on it, with no live read to wait for.
  if (!sessions) return <Frame tile={tile} />;
  const freshness = live.hidden
    ? "Paused while hidden"
    : live.error && !live.state
      ? "Couldn't read"
      : live.age == null
        ? "Reading…"
        : live.age <= 1
          ? "Updated just now"
          : `Updated ${live.age} s ago`;
  const blocking = tile.items.filter((i) => i.status === "bad");
  const foot = (
    <p className={`dash-live ${live.hidden || (live.error && !live.state) ? "paused" : ""}`} title={live.state ? `Read at ${new Date(live.state.taken_at).toLocaleTimeString()}` : undefined}>
      <span className="live-dot" aria-hidden /> Live · {freshness}
    </p>
  );
  if (!live.state && !live.error) {
    return (
      <Frame tile={{ ...tile, takeaway: "Reading sessions…", status: "info" }} foot={foot}>
        <div className="dash-counts">
          {["Running", "Blocked", "Idle in tx"].map((l) => (
            <div key={l} className="dash-count">
              <b className="skeleton" style={{ width: 18, height: 18, display: "block" }} />
              <span>{l}</span>
            </div>
          ))}
        </div>
      </Frame>
    );
  }
  return (
    <Frame tile={tile} action={blocking.length ? "Blocking" : "Sessions"} onOpen={() => openLive("sessions")} foot={foot}>
      {tile.status !== "unavailable" && (
        <>
          <div className="dash-counts">
            <div className="dash-count">
              <b>{metricText(tile, "active") ?? "0"}</b>
              <span>Running</span>
            </div>
            <div className={`dash-count ${metricValue(tile, "blocked") > 0 ? "bad" : ""}`}>
              <b>{metricText(tile, "blocked") ?? "0"}</b>
              <span>Blocked</span>
            </div>
            <div className={`dash-count ${metricValue(tile, "idle-in-transaction") > 0 ? "watch" : ""}`}>
              <b>{metricText(tile, "idle-in-transaction") ?? "0"}</b>
              <span>Idle in tx</span>
            </div>
          </div>
          <Activity samples={live.history} />
        </>
      )}
    </Frame>
  );
}

/** Running sessions over the last few minutes, blocked ones on top. */
function Activity({ samples }: { samples: Sample[] }) {
  const span = Math.round((HISTORY * REFRESH_MS) / 60000);
  if (samples.length < 2) {
    return (
      <>
        <div className="dash-activity empty" aria-hidden />
        <div className="dash-activity-cap">Sampling every {REFRESH_MS / 1000} s</div>
      </>
    );
  }
  const w = 200;
  const h = 34;
  const peak = Math.max(2, ...samples.map((s) => s.active)) * 1.25;
  const x = (i: number) => (i / (HISTORY - 1)) * w;
  // The newest read sits at the right edge; history grows leftward as it fills.
  const offset = HISTORY - samples.length;
  const y = (v: number) => h - 2 - (v / peak) * (h - 4);
  const line = (key: "active" | "blocked") => samples.map((s, i) => `${i ? "L" : "M"}${x(i + offset).toFixed(1)},${y(s[key]).toFixed(1)}`).join("");
  const area = `${line("active")}L${x(HISTORY - 1)},${h}L${x(offset).toFixed(1)},${h}Z`;
  const blocked = samples.some((s) => s.blocked > 0);
  const minutes = Math.max(1, Math.round((samples.length * REFRESH_MS) / 60000));
  const filling = offset > 0;
  return (
    <>
      <svg className="dash-activity" viewBox={`0 0 ${w} ${h}`} preserveAspectRatio="none" role="img" aria-label={`Running sessions over the last ${minutes} min${blocked ? ", some blocked" : ""}`}>
        {/* A baseline across the whole window, dashed where no sample has arrived yet, so a young chart reads as filling. */}
        {filling && <line className="pending" x1={0} x2={x(offset)} y1={h - 0.5} y2={h - 0.5} />}
        <line className="base" x1={x(offset)} x2={w} y1={h - 0.5} y2={h - 0.5} />
        <path className="area" d={area} />
        <path className="line" d={line("active")} />
        {blocked && <path className="blocked" d={line("blocked")} />}
      </svg>
      <div className="dash-activity-cap">
        <span>{filling ? `${minutes} of ${span} min so far` : `Last ${span} min`}</span>
        <span>now</span>
      </div>
    </>
  );
}

// ───────── health ─────────

function HealthTile({ tile, onOpen }: { tile: Tile; onOpen: () => void }) {
  if (tile.status === "unavailable") return <Frame tile={tile} />;
  // The takeaway names the worst signal; the rows show the next ones, then the healthy ones with their values.
  const notable = tile.items.filter((i) => i.status === "bad" || i.status === "watch");
  const rows = [...notable.slice(1), ...tile.items.filter((i) => i.status !== "bad" && i.status !== "watch")].slice(0, 3);
  return (
    <Frame tile={tile} action="All signals" onOpen={onOpen}>
      <div className="dash-signals" role="img" aria-label={`${tile.items.length} signals`}>
        {tile.items.map((i) => (
          <span key={i.id} className={`dash-signal ${i.status ?? "info"}`} title={`${i.label}: ${i.value ?? ""}`} />
        ))}
      </div>
      {rows.length > 0 && (
        <ul className="dash-rows compact">
          {rows.map((i) => (
            <li key={i.id}>
              <button className="dash-row" onClick={onOpen}>
                <i className={`sig-dot ${i.status}`} aria-hidden />
                <span className="dash-row-label">{i.label}</span>
                <span className="dash-row-value">{i.value}</span>
              </button>
            </li>
          ))}
        </ul>
      )}
    </Frame>
  );
}

// ───────── storage, connections, replication, failing queries ─────────

/** The four signal tiles share one shape: up to two numbers, then the rows behind them, worst first. */
/** Rows are signals, or queries when `report` is given: those read like Top queries ("UPDATE orders · id = ?"), so two
 * statements on one table stay apart. */
function SignalsTile({ tile, action, onOpen, onRow, report }: { tile: Tile; action: string; onOpen: () => void; onRow?: (item: TileItem) => void; report?: Report }) {
  if (tile.status === "unavailable") return <Frame tile={tile} />;
  return (
    <Frame tile={tile} action={action} onOpen={onOpen}>
      {tile.metrics.length > 0 && (
        <div className="dash-stats compact">
          {tile.metrics.slice(0, 2).map((m) => (
            <div key={m.key} className="dash-stat">
              <b>{m.text}</b>
              <span>{m.label}</span>
            </div>
          ))}
        </div>
      )}
      {tile.items.length > 0 && (
        <ul className="dash-rows compact">
          {tile.items.map((i) => (
            <li key={i.id}>
              <button className="dash-row" onClick={() => (onRow ? onRow(i) : onOpen())}>
                <i className={`sig-dot ${i.status ?? "info"}`} aria-hidden />
                <span className="dash-row-label">{report ? queryLabel(i, report) : i.label}</span>
                <span className="dash-row-value">{i.value}</span>
              </button>
            </li>
          ))}
        </ul>
      )}
    </Frame>
  );
}

// ───────── deadlocks ─────────

function DeadlocksTile({ tile, report }: { tile: Tile; report: Report }) {
  const d = report.snapshot.deadlocks;
  if (tile.status === "unavailable" || !d) return <Frame tile={tile} />;
  const open = () => openLive("deadlocks");
  if (d.counted != null && d.deadlocks.length === 0) {
    return (
      <Frame tile={tile} action="Deadlocks" onOpen={open} foot={<p className="dash-detail">Paste the server log's lines into Live → Deadlocks to have one explained.</p>}>
        <div className="dash-big">
          <b>{d.counted}</b>
          <span title={d.counted_since ? when(d.counted_since).full : undefined}>counted{d.counted_since ? ` since ${when(d.counted_since).short}` : ""}</span>
        </div>
      </Frame>
    );
  }
  const end = Date.parse(report.snapshot.taken_at);
  const start = end - d.since_hours * 3_600_000;
  return (
    <Frame tile={tile} action="Deadlocks" onOpen={open}>
      <div className="dash-ticks" role="img" aria-label={`${d.deadlocks.length} deadlocks over the last ${d.since_hours} hours`}>
        {d.deadlocks.map((dl) => {
          const at = dl.at ? Date.parse(dl.at) : NaN;
          if (!Number.isFinite(at)) return null;
          const left = Math.min(100, Math.max(0, ((at - start) / (end - start)) * 100));
          return <span key={dl.id} style={{ left: `${left}%` }} title={`${dl.headline}${dl.at ? ` · ${new Date(dl.at).toLocaleString()}` : ""}`} />;
        })}
        <em>{d.since_hours >= 23.5 ? "24 h ago" : `${Math.round(d.since_hours)} h ago`}</em>
        <em className="end">now</em>
      </div>
      {tile.items.length > 0 && (
        <ul className="dash-rows compact">
          {tile.items.slice(0, 2).map((i) => (
            <li key={i.id}>
              <button className="dash-row" onClick={() => (i.finding_id ? openFinding(i.finding_id) : open())}>
                <span className="dash-row-label">{i.label}</span>
                <span className="dash-row-value">{i.value}</span>
              </button>
            </li>
          ))}
        </ul>
      )}
    </Frame>
  );
}

// ───────── indexes ─────────

function IndexesTile({ tile }: { tile: Tile }) {
  const open = () => setState({ view: "schema", advisor: true });
  const adds = metricValue(tile, "add");
  const cleanups = metricValue(tile, "cleanup");
  const candidate = tile.items.find((i) => i.kind === "index-add");
  return (
    <Frame tile={tile} action="Index advisor" onOpen={open}>
      {(adds > 0 || cleanups > 0) && (
        <div className="dash-pair">
          <div className={`dash-big ${adds > 0 ? "" : "zero"}`}>
            <b>{adds}</b>
            <span>to add</span>
          </div>
          <div className={`dash-big ${cleanups > 0 ? "" : "zero"}`}>
            <b>{cleanups}</b>
            <span>to clean up{cleanups > 0 && metricValue(tile, "reclaim") > 0 ? ` · ${metricText(tile, "reclaim")}` : ""}</span>
          </div>
        </div>
      )}
      {candidate && (
        <ul className="dash-rows compact">
          <li>
            <button className="dash-row" onClick={() => (candidate.finding_id ? openFinding(candidate.finding_id) : open())}>
              <span className="dash-plus" aria-hidden>+</span>
              <span className="dash-row-label mono">{candidate.label}</span>
              <span className="dash-row-value">{candidate.value}</span>
            </button>
          </li>
        </ul>
      )}
    </Frame>
  );
}

// ───────── plans ─────────

function PlansTile({ tile }: { tile: Tile }) {
  if (tile.status === "unavailable") return <Frame tile={tile} />;
  const captured = metricValue(tile, "captured");
  const flagged = metricValue(tile, "flagged");
  const top = tile.items[0]?.query_id;
  return (
    <Frame tile={tile} action="Plans" onOpen={() => (top ? openQuery(top, "plan") : setState({ view: "queries" }))}>
      <div className="dash-meter" role="img" aria-label={`${flagged} of ${captured} plans have something to fix`}>
        {Array.from({ length: Math.min(captured, 40) }, (_, i) => (
          <span key={i} className={i < Math.round((flagged / Math.max(captured, 1)) * Math.min(captured, 40)) ? "on" : ""} />
        ))}
      </div>
      {tile.items.length > 0 && (
        <ul className="dash-rows compact">
          {tile.items.slice(0, 2).map((i) => (
            <li key={i.id}>
              <button className="dash-row" onClick={() => i.query_id && openQuery(i.query_id, "plan")} title={`Open the costliest plan with this (${percent(i.share)} of database time)`}>
                <span className={`sev ${severityOf(i.status)}`} aria-hidden />
                <span className="dash-row-label">{i.label}</span>
                <span className="dash-row-value">{i.value}</span>
              </button>
            </li>
          ))}
        </ul>
      )}
    </Frame>
  );
}

// ───────── query statistics ─────────

function StatisticsTile({ tile, report }: { tile: Tile; report: Report }) {
  // Statements from a log the application writes (SQLite): the step is a setting on the connection, not SQL.
  if (!report.snapshot.workload && report.snapshot.server.missing.some((m) => m.capability === "query-log")) {
    return (
      <Frame tile={tile} className="unlocking">
        <div className="dash-actions">
          <span className="muted">Culprit reads the log file only; nothing changes in the application.</span>
          <button className="btn small" onClick={() => setState({ view: "connections" })}>
            <FileText size={12} /> Add query log
          </button>
        </div>
      </Frame>
    );
  }
  if (!report.snapshot.workload) {
    const sql = tile.fix_sql;
    return (
      <Frame tile={tile} className="unlocking">
        {sql && (
          <div className="dash-fix" data-own-click>
            <code>{sql}</code>
            <button className="btn small" onClick={() => copyAndTell(sql, "Copied")} aria-label="Copy the statement">
              <Copy size={12} /> Copy
            </button>
            <button className="btn small" onClick={() => setState({ view: "editor", editorSql: sql })} title="Opens in the editor; production asks before it runs">
              <SquarePen size={12} /> Open in editor
            </button>
          </div>
        )}
        <div className="dash-actions">
          <span className="muted">Culprit only reads statistics; it never changes the database for you.</span>
          <button className="btn small" onClick={() => scan(report.snapshot.connection)} title={keys("Scan again once it's on (⌘R)")}>
            <RefreshCw size={12} /> Done, re-scan
          </button>
        </div>
      </Frame>
    );
  }
  const finding = tile.items.find((i) => i.finding_id);
  const chips = tile.metrics.filter((m) => m.key !== "locked" || (m.value ?? 0) > 0);
  return (
    <Frame tile={tile} action={finding ? "Open finding" : "Queries"} onOpen={() => (finding?.finding_id ? openFinding(finding.finding_id) : setState({ view: "queries" }))}>
      <div className="dash-chips">
        {chips.map((m) =>
          m.key === "window-hours" ? (
            <span key={m.key}>
              covers <b>{m.text}</b>
            </span>
          ) : (
            <span key={m.key} className={m.key === "locked" ? "warn" : ""}>
              <b>{m.text}</b> {m.label.toLowerCase()}
            </span>
          ),
        )}
      </div>
      {tile.fix_sql && tile.status !== "good" && <Fix sql={tile.fix_sql} />}
    </Frame>
  );
}
