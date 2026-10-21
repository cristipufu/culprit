import { useEffect, useRef, useState } from "react";
import type { Dashboard as DashboardModel } from "../../gen/Dashboard";
import type { DashboardLive } from "../../gen/DashboardLive";
import type { LiveState } from "../../gen/LiveState";
import type { Tile } from "../../gen/Tile";
import { api, errorMessage, type ApiError } from "../../lib/api";
import { openLive } from "../Live";
import { Frame, metricText, metricValue } from "./Tiles";

/** Sessions are read this often while the dashboard is on screen; never while the window is hidden. */
const REFRESH_MS = 5000;
/** Five minutes of reads for the activity line. */
const HISTORY = Math.round((5 * 60 * 1000) / REFRESH_MS);

export type Sample = { at: number; active: number; blocked: number | null };

export type LiveNow = {
  dashboard: DashboardModel | null;
  state: LiveState | null;
  error: ApiError | null;
  hidden: boolean;
  history: Sample[];
  age: number | null;
};

/** Sessions right now, read every few seconds while the window is visible; never for an engine without sessions. */
export function useLiveNow(connection: string, enabled: boolean, dashboard: DashboardModel | null, scope: string): LiveNow {
  const [read, setRead] = useState<{ scope: string; view: DashboardLive | null; error: ApiError | null; history: Sample[] } | null>(null);
  const current = read?.scope === scope ? read : null;
  const state = current?.view?.live ?? null;
  const error = current?.error ?? current?.view?.error ?? null;
  const [hidden, setHidden] = useState(() => typeof document !== "undefined" && document.hidden);
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
    if (!enabled || !dashboard || (hidden && hasRead.current)) return;
    let cancelled = false;
    let timer: ReturnType<typeof setTimeout>;
    const tick = async () => {
      try {
        const next = await (await api()).dashboardLive(connection, dashboard);
        if (cancelled) return;
        const tile = next.dashboard.tiles.find((t) => t.key === "now");
        const active = tile?.metrics.find((m) => m.key === "active")?.value;
        const blocked = tile?.metrics.find((m) => m.key === "blocked")?.value ?? null;
        setRead((old) => {
          const previous = old?.scope === scope ? old.history : [];
          const history = next.live && active != null ? [...previous.slice(-(HISTORY - 1)), { at: Date.now(), active, blocked }] : previous;
          return { scope, view: next, error: null, history };
        });
        if (next.error?.code === "unsupported") return;
      } catch (e) {
        const error = errorMessage(e);
        if (!cancelled) setRead((old) => ({ scope, view: null, error, history: old?.scope === scope ? old.history : [] }));
        if (error.code === "unsupported") return;
      }
      if (!cancelled && !document.hidden) timer = setTimeout(tick, REFRESH_MS);
    };
    tick();
    return () => {
      cancelled = true;
      clearTimeout(timer);
    };
  }, [connection, hidden, enabled, dashboard, scope]);

  const age = state ? Math.max(0, Math.round((now - Date.parse(state.taken_at)) / 1000)) : null;
  return { dashboard: current?.view?.dashboard ?? null, state, error, hidden, history: current?.history ?? [], age };
}

export function NowTile({ tile, live, sessions }: { tile: Tile; live: LiveNow; sessions: boolean }) {
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
  if (live.error && tile.status !== "unavailable") {
    return (
      <Frame tile={{ ...tile, status: "unavailable", takeaway: "Couldn't refresh sessions.", detail: null }} foot={foot}>
        <p className="dash-detail">{live.error.message}</p>
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
            <div className={`dash-count ${metricValue(tile, "blocked") > 0 && (tile.status === "bad" || tile.status === "watch") ? tile.status : ""}`}>
              <b>{metricText(tile, "blocked") ?? "0"}</b>
              <span>Blocked</span>
            </div>
            <div className={`dash-count ${metricValue(tile, "long-idle-in-transaction") > 0 ? "watch" : ""}`}>
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
  const line = (key: "active" | "blocked") => {
    let connected = false;
    return samples
      .map((s, i) => {
        const value = s[key];
        if (value == null) {
          connected = false;
          return "";
        }
        const command = connected ? "L" : "M";
        connected = true;
        return `${command}${x(i + offset).toFixed(1)},${y(value).toFixed(1)}`;
      })
      .join("");
  };
  const area = `${line("active")}L${x(HISTORY - 1)},${h}L${x(offset).toFixed(1)},${h}Z`;
  const blocked = samples.some((s) => s.blocked != null && s.blocked > 0);
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
