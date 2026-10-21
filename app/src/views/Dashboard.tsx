import { GitCompareArrows, Radio } from "lucide-react";
import { lazy, Suspense, useEffect, useMemo, useRef, useState, type KeyboardEvent } from "react";
import { chartEvents } from "../components/ChangeTimeline";
import { DashboardSkeleton } from "../components/DashboardSkeleton";
import { MarkButton } from "../components/MarkButton";
import { planFlags, Sparkline, trendNote, WorkloadTrend } from "../components/Trends";
import type { WaitsTab } from "../components/Waits";
import type { ConnectionInfo } from "../gen/ConnectionInfo";
import type { Dashboard as DashboardModel } from "../gen/Dashboard";
import type { QueryStat } from "../gen/QueryStat";
import type { Report } from "../gen/Report";
import type { Tile } from "../gen/Tile";
import type { TileKey } from "../gen/TileKey";
import type { TileStatus } from "../gen/TileStatus";
import { api, errorMessage, type ApiError } from "../lib/api";
import { can, serverName } from "../lib/engines";
import { useEvents } from "../lib/events";
import { isActionable } from "../lib/findings";
import { ago, duration } from "../lib/format";
import { useHotkeys } from "../lib/hotkeys";
import { figures } from "../lib/rates";
import { changeRequest, setState, useStore } from "../lib/store";
import { waitColor } from "../lib/waits";
import { NowTile, useLiveNow } from "./dashboard/NowTile";
import {
  DeadlocksTile,
  Fix,
  Frame,
  HealthTile,
  IndexesTile,
  metricValue,
  openFinding,
  openQuery,
  PlansTile,
  queryLabel,
  severityOf,
  SignalsTile,
  StatisticsTile,
} from "./dashboard/Tiles";
import { compareSince } from "./Overview";

const WaitsPanel = lazy(() => import("../components/WaitsPanel"));

type Loaded = { status: "loading" } | { status: "ready"; scope: string; model: DashboardModel } | { status: "error"; scope: string; error: ApiError };

/**
 * One database at a glance: fourteen tiles, each a plain sentence, a small honest picture and a door to the view that
 * explains it. Composed from the latest scan by the same code as `culprit dashboard`; sessions are read live.
 */
export default function Dashboard({ report, connection }: { report: Report; connection: ConnectionInfo }) {
  const windowMinutes = useStore((s) => s.windowMinutes);
  const scanning = useStore((s) => s.report.status === "ready" && s.report.scanning);
  const [loaded, setLoaded] = useState<Loaded>({ status: "loading" });
  const [waits, setWaits] = useState<{ tab: WaitsTab; category?: string } | null>(null);
  const scope = JSON.stringify([connection.name, windowMinutes, report.snapshot.id]);
  const cold = loaded.status === "ready" && loaded.scope === scope ? loaded.model : null;
  const live = useLiveNow(connection.name, can(connection.engine, "sessions"), cold, scope);
  const gridRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    let current = true;
    api()
      .then((client) => client.dashboard(connection.name, windowMinutes, report.snapshot.id))
      .then((model) => current && setLoaded({ status: "ready", scope, model }))
      .catch((e) => current && setLoaded({ status: "error", scope, error: errorMessage(e) }));
    return () => {
      current = false;
    };
  }, [connection.name, windowMinutes, report.snapshot.id]);

  const model = live.dashboard ?? cold;

  // From nowhere in particular, j or ↓ steps into the grid; inside it, the grid's own keys take over.
  const enter = () => {
    if (document.activeElement && document.activeElement !== document.body) return;
    gridRef.current?.querySelector<HTMLElement>("[data-tile]")?.focus();
  };
  const periods = can(connection.engine, "periods");
  useHotkeys({ ...(periods ? { v: () => setState({ view: "overview", changes: changeRequest() }) } : {}), j: enter, ArrowDown: enter }, !waits);

  if (loaded.status === "error" && loaded.scope === scope) {
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
  if (!model) return <DashboardSkeleton />;

  const { tiles, headline, attention: keys } = model;
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
  const hidden = report.findings.filter((f) => isActionable(f) && dismissed[f.id]).length;
  const open = report.findings.filter((f) => isActionable(f) && !dismissed[f.id]);
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

// ───────── top queries ─────────

/** "2.5 ms per run over 89 hours of Culprit's scans": a statement's average as the report states it (`figures`). */
function perRun(report: Report, q: QueryStat): string {
  const f = figures(report, q);
  return `${duration(f.mean)} per run${f.over ? ` over ${f.over}` : ""}`;
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
                <span className="dash-row-value" title={q ? perRun(report, q) : undefined}>
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
