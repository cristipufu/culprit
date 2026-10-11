import { Lock, Pause, Play } from "lucide-react";
import { lazy, Suspense, useEffect, useRef, useState } from "react";
import { BlockingChains } from "../components/BlockingChains";
import type { LiveState } from "../gen/LiveState";
import type { Session } from "../gen/Session";
import type { SessionAction } from "../gen/SessionAction";
import { api, errorMessage, type ApiError } from "../lib/api";
import { count, duration, oneLine } from "../lib/format";
import { useHotkeys } from "../lib/hotkeys";
import { currentReport, setState, toast, useStore } from "../lib/store";

const Deadlocks = lazy(() => import("./Deadlocks"));

const REFRESH_MS = 2500;
/** Five minutes of samples for the activity sparkline. */
const HISTORY = Math.round((5 * 60 * 1000) / REFRESH_MS);
const TAB_KEY = "culprit.live.tab";

type Tab = "sessions" | "deadlocks";
type Sample = { at: number; active: number; blocked: number };

function rememberedTab(): Tab {
  try {
    return localStorage.getItem(TAB_KEY) === "deadlocks" ? "deadlocks" : "sessions";
  } catch {
    return "sessions";
  }
}

/** Open Live on a tab, e.g. from the dashboard's Right now and Deadlocks tiles. */
export function openLive(tab: Tab): void {
  try {
    localStorage.setItem(TAB_KEY, tab);
  } catch {}
  setState({ view: "live" });
}

// Per-operator progress of a running statement; loaded when a session is opened.
const QueryProgressPanel = lazy(() => import("../components/QueryProgress").then((m) => ({ default: m.QueryProgressPanel })));

export function Live({ connection }: { connection: string }) {
  const [state, setLive] = useState<LiveState | null>(null);
  const [error, setError] = useState<ApiError | null>(null);
  const [paused, setPaused] = useState(false);
  const [showIdle, setShowIdle] = useState(false);
  const [refresh, setRefresh] = useState(0);
  const [tab, setTab] = useState<Tab>(rememberedTab);
  const [hidden, setHidden] = useState(() => typeof document !== "undefined" && document.hidden);
  const [history, setHistory] = useState<Sample[]>([]);
  const [located, setLocated] = useState<string | null>(null);
  const [now, setNow] = useState(() => Date.now());
  const engine = useStore((s) => s.connections.find((c) => c.name === connection)?.engine ?? "postgres");
  const recentDeadlocks = useStore((s) => currentReport(s)?.snapshot.deadlocks?.deadlocks.length ?? 0);
  const tableRef = useRef<HTMLDivElement>(null);
  const stopped = paused || hidden || tab !== "sessions";
  const stoppedRef = useRef(stopped);
  stoppedRef.current = stopped;
  const [watching, setWatching] = useState<string | null>(null);

  useHotkeys({ " ": () => setPaused((p) => !p) });

  const chooseTab = (next: Tab) => {
    setTab(next);
    try {
      localStorage.setItem(TAB_KEY, next);
    } catch {}
  };

  // Polling stops while the window is hidden; the freshness label says so.
  useEffect(() => {
    const onVisibility = () => setHidden(document.hidden);
    document.addEventListener("visibilitychange", onVisibility);
    return () => document.removeEventListener("visibilitychange", onVisibility);
  }, []);

  useEffect(() => {
    const timer = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(timer);
  }, []);

  useEffect(() => {
    if (!state) return;
    const active = state.sessions.filter((s) => s.state !== "idle" && s.state !== "sleeping").length;
    const blocked = state.sessions.filter((s) => s.blocked_by).length;
    setHistory((h) => [...h.slice(-(HISTORY - 1)), { at: Date.now(), active, blocked }]);
  }, [state]);

  useEffect(() => {
    // The first read always happens; after that, polling stops while paused, hidden, or on Deadlocks.
    if (stopped && state) return;
    let cancelled = false;
    let timer: ReturnType<typeof setTimeout>;
    const tick = async () => {
      try {
        const next = await (await api()).live(connection);
        if (cancelled) return;
        setLive(next);
        setError(null);
      } catch (e) {
        if (!cancelled) setError(errorMessage(e));
      }
      if (!cancelled && !stoppedRef.current) timer = setTimeout(tick, REFRESH_MS);
    };
    tick();
    return () => {
      cancelled = true;
      clearTimeout(timer);
    };
    // `state` only gates the first read; re-running on every new state would restart the timer each tick.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [connection, stopped, refresh]);

  const locate = (id: string) => {
    setLocated(id);
    tableRef.current?.querySelector(`[data-session="${CSS.escape(id)}"]`)?.scrollIntoView({ block: "nearest", behavior: "smooth" });
    setTimeout(() => setLocated((l) => (l === id ? null : l)), 1800);
  };

  const interrupt = async (session: Session, action: SessionAction) => {
    await (await api()).interruptSession(connection, session.id, action);
    toast(action === "cancel" ? `Cancelled the statement in session ${session.id}` : `Ended session ${session.id}`);
    setRefresh((n) => n + 1);
  };
  const actions = state?.session_actions ?? [];

  const sessions = state?.sessions ?? [];
  const blocked = sessions.filter((s) => s.blocked_by);
  const heads = [...new Set(blocked.map((s) => s.blocked_by!))].filter((id) => !blocked.some((b) => b.id === id));
  const visible = sessions.filter((s) => showIdle || s.state !== "idle" || s.blocked_by || heads.includes(s.id));
  const chains = state?.blocking?.chains ?? [];
  const activeNow = sessions.filter((s) => s.state !== "idle" && s.state !== "sleeping").length;
  const age = state ? Math.max(0, Math.round((now - Date.parse(state.taken_at)) / 1000)) : null;
  const freshness = paused ? "Paused" : hidden ? "Paused while hidden" : tab === "deadlocks" ? "Paused on Deadlocks" : age == null ? "Connecting…" : age <= 1 ? "Updated just now" : `Updated ${age} s ago`;

  return (
    <div className="view live-view" style={{ overflow: "hidden" }}>
      <div className="live-top">
        <span className={`live-status ${paused || hidden || tab === "deadlocks" ? "paused" : ""}`} title={state ? `Last read ${new Date(state.taken_at).toLocaleTimeString()}` : undefined}>
          <span className="live-dot" aria-hidden /> Live · <span className="live-age">{freshness}</span>
        </span>
        {tab === "sessions" && <Sparkline samples={history} />}
        <span className="seg live-tabs" role="tablist" aria-label="Live">
          <button role="tab" aria-selected={tab === "sessions"} className={tab === "sessions" ? "on" : ""} onClick={() => chooseTab("sessions")}>
            Sessions
            {blocked.length > 0 && <span className="tab-count warn">{blocked.length} waiting</span>}
          </button>
          <button role="tab" aria-selected={tab === "deadlocks"} className={tab === "deadlocks" ? "on" : ""} onClick={() => chooseTab("deadlocks")}>
            Deadlocks
            {recentDeadlocks > 0 && <span className="tab-count">{recentDeadlocks}</span>}
          </button>
        </span>
        <span className="spacer" />
        {tab === "sessions" && (
          <>
            <label className="muted live-idle">
              <input type="checkbox" checked={showIdle} onChange={(e) => setShowIdle(e.target.checked)} /> Show idle
            </label>
            <button className="btn" onClick={() => setPaused((p) => !p)} aria-pressed={paused}>
              {paused ? <Play size={13} /> : <Pause size={13} />} {paused ? "Resume" : "Pause"} <span className="kbd">Space</span>
            </button>
          </>
        )}
      </div>
      {tab === "deadlocks" ? (
        <Suspense fallback={<div className="empty"><div className="spinner" /></div>}>
          <Deadlocks connection={connection} engine={engine} />
        </Suspense>
      ) : (
        <>
      {error ? (
        <div className="live-calm error" role="alert">
          <span className="sev high" aria-hidden /> {error.message}
          {error.hint && <span className="muted"> {error.hint}</span>}
        </div>
      ) : chains.length > 0 && state?.blocking ? (
        <BlockingChains
          blocking={state.blocking}
          sessions={sessions}
          actions={actions}
          actionsNote={state.session_actions_note}
          onInterrupt={interrupt}
          onLocate={locate}
        />
      ) : blocked.length > 0 ? (
        <div className="live-calm">
          <span className="sev medium" aria-hidden /> {blocked.length} {blocked.length === 1 ? "session waits" : "sessions wait"} on {heads.length === 1 ? `session ${heads[0]}` : `${heads.length} sessions`}.
        </div>
      ) : (
        <div className="live-calm good">
          <span className="pulse" aria-hidden /> Nothing is blocked{state ? ` · ${activeNow} active ${activeNow === 1 ? "session" : "sessions"}` : ""}
          {state?.session_actions_note && (
            <span className="muted live-note" title="Cancelling statements and ending sessions is only offered on non-production connections.">
              <Lock size={12} /> {state.session_actions_note}
            </span>
          )}
        </div>
      )}
      <div className="table-wrap" ref={tableRef} style={watching ? { flex: "0 1 auto", maxHeight: "34%" } : undefined}>
        <table className="grid">
          <thead>
            <tr>
              <th>Session</th>
              <th>State</th>
              <th className="r">Running for</th>
              <th>Waiting on</th>
              <th>Blocked by</th>
              <th className="r">CPU</th>
              <th className="r">Reads</th>
              <th>Statement</th>
              <th>Application</th>
              {actions.length > 0 && <th className="r" />}
            </tr>
          </thead>
          <tbody>
            {visible.map((s) => (
              <SessionRow key={s.id} session={s} isHead={heads.includes(s.id)} located={located === s.id} actions={actions} onInterrupt={interrupt} open={watching === s.id} onOpen={() => setWatching((w) => (w === s.id ? null : s.id))} />
            ))}
          </tbody>
        </table>
        {state && visible.length === 0 && (
          <div className="empty" style={{ height: 240 }}>
            <h3>Quiet</h3>
            <p>No active sessions right now.</p>
          </div>
        )}
      </div>
      {watching && (
        <Suspense fallback={null}>
          <QueryProgressPanel connection={connection} session={watching} onClose={() => setWatching(null)} />
        </Suspense>
          )}
        </>
      )}
    </div>
  );
}

/** Active sessions over the last five minutes, with blocked sessions drawn on top. */
function Sparkline({ samples }: { samples: Sample[] }) {
  if (samples.length < 3) return <span className="live-spark-placeholder" aria-hidden />;
  const w = 96;
  const h = 22;
  const peak = Math.max(1, ...samples.map((s) => s.active));
  // Headroom above the busiest moment, so a steady load reads as a calm line rather than a filled block.
  const top = Math.max(2, peak) * 1.4;
  // Spread what has been seen so far across the full width; the label says how long that is.
  const x = (i: number) => (i / (samples.length - 1)) * w;
  const y = (v: number) => h - 2 - (v / top) * (h - 4);
  const line = (key: "active" | "blocked") => samples.map((s, i) => `${i ? "L" : "M"} ${x(i).toFixed(1)} ${y(s[key]).toFixed(1)}`).join(" ");
  const anyBlocked = samples.some((s) => s.blocked > 0);
  const area = `${line("active")} L ${x(samples.length - 1).toFixed(1)} ${h} L ${x(0).toFixed(1)} ${h} Z`;
  const blockedPeak = Math.max(...samples.map((s) => s.blocked));
  return (
    <svg className="live-spark" width={w} height={h} viewBox={`0 0 ${w} ${h}`} role="img" aria-label={`Active sessions over the last ${Math.round((samples.length * REFRESH_MS) / 60000) || 1} minutes: peak ${peak}${anyBlocked ? `; up to ${blockedPeak} blocked` : ""}`}>
      <title>{`Active sessions, last 5 minutes · peak ${peak}${anyBlocked ? ` · up to ${blockedPeak} blocked` : ""}`}</title>
      <path d={area} className="spark-area" />
      <path d={line("active")} className="spark-line" />
      {anyBlocked && <path d={line("blocked")} className="spark-blocked" />}
    </svg>
  );
}

const ACTION_COPY: Record<SessionAction, { button: string; confirm: string; explain: string }> = {
  cancel: { button: "Cancel", confirm: "Cancel statement", explain: "Stops the running statement; the session stays connected." },
  terminate: { button: "End", confirm: "End session", explain: "Closes the session; its open transaction rolls back." },
};

type RowAction = { status: "idle" } | { status: "confirm"; action: SessionAction } | { status: "busy" } | { status: "error"; message: string };

function SessionRow({
  session: s,
  isHead,
  located = false,
  actions,
  onInterrupt,
  open,
  onOpen,
}: {
  session: Session;
  isHead: boolean;
  located?: boolean;
  actions: SessionAction[];
  onInterrupt: (session: Session, action: SessionAction) => Promise<void>;
  /** Its progress panel is showing. */
  open: boolean;
  /** Show (or hide) how far along its running statement is. */
  onOpen: () => void;
}) {
  const [row, setRow] = useState<RowAction>({ status: "idle" });
  const offered = actions.filter((a) => a !== "cancel" || s.state === "active");
  const run = async (action: SessionAction) => {
    setRow({ status: "busy" });
    try {
      await onInterrupt(s, action);
      setRow({ status: "idle" });
    } catch (e) {
      setRow({ status: "error", message: errorMessage(e).message });
    }
  };
  return (
    <tr
      data-session={s.id}
      className={`row ${row.status === "confirm" ? "confirming" : ""} ${s.transaction_ms != null && (s.state === "idle in transaction" || s.state === "sleeping") ? "idle-tx" : ""} ${located ? "located" : ""} ${open ? "selected" : ""} ${s.state !== "idle" ? "openable" : ""}`}
      tabIndex={s.state !== "idle" ? 0 : undefined}
      aria-expanded={s.state !== "idle" ? open : undefined}
      title={s.state !== "idle" ? "Show how far along its statement is (Enter)" : undefined}
      onClick={(e) => s.state !== "idle" && !(e.target as HTMLElement).closest("button") && onOpen()}
      onKeyDown={(e) => s.state !== "idle" && e.key === "Enter" && e.target === e.currentTarget && onOpen()}
    >
      <td className="mono">
        {s.id}
        {isHead && <span className="tag high" style={{ marginLeft: 6 }}>blocking</span>}
      </td>
      <td>{s.state}</td>
      <td className="r">{duration(s.elapsed_ms)}</td>
      <td>{s.wait ? `${s.wait}${s.wait_ms ? ` · ${duration(s.wait_ms)}` : ""}` : "—"}</td>
      <td className="mono">{s.blocked_by ?? ""}</td>
      <td className="r">{s.cpu_ms != null ? duration(s.cpu_ms) : "—"}</td>
      <td className="r">{s.reads != null ? count(s.reads) : "—"}</td>
      <td className="sqlcell" title={s.sql ?? ""}>
        {s.sql ? oneLine(s.sql) : ""}
      </td>
      <td className="muted">{s.application ?? ""}</td>
      {actions.length > 0 && (
        <td className="r session-actions">
          {row.status === "idle" &&
            offered.map((a) => (
              <button key={a} className="btn ghost small" title={ACTION_COPY[a].explain} onClick={() => setRow({ status: "confirm", action: a })}>
                {ACTION_COPY[a].button}
              </button>
            ))}
          {row.status === "confirm" && (
            <span className="inline-confirm" onKeyDown={(e) => e.key === "Escape" && setRow({ status: "idle" })}>
              <button className="btn danger small" autoFocus title={ACTION_COPY[row.action].explain} onClick={() => run(row.action)}>
                {ACTION_COPY[row.action].confirm} {s.id}
              </button>
              <button className="btn ghost small" onClick={() => setRow({ status: "idle" })}>
                Keep
              </button>
            </span>
          )}
          {row.status === "busy" && <span className="spinner" />}
          {row.status === "error" && (
            <span className="inline-confirm">
              <span className="error-text" title={row.message}>
                {row.message}
              </span>
              <button className="btn ghost small" onClick={() => setRow({ status: "idle" })}>
                OK
              </button>
            </span>
          )}
        </td>
      )}
    </tr>
  );
}
