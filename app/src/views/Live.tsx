import { Pause, Play } from "lucide-react";
import { useEffect, useState } from "react";
import type { LiveState } from "../gen/LiveState";
import type { Session } from "../gen/Session";
import { api, errorMessage, type ApiError } from "../lib/api";
import { count, duration, oneLine } from "../lib/format";
import { useHotkeys } from "../lib/hotkeys";

const REFRESH_MS = 2500;

export function Live({ connection }: { connection: string }) {
  const [state, setLive] = useState<LiveState | null>(null);
  const [error, setError] = useState<ApiError | null>(null);
  const [paused, setPaused] = useState(false);
  const [showIdle, setShowIdle] = useState(false);

  useHotkeys({ " ": () => setPaused((p) => !p) });

  useEffect(() => {
    if (paused) return;
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
      if (!cancelled) timer = setTimeout(tick, REFRESH_MS);
    };
    tick();
    return () => {
      cancelled = true;
      clearTimeout(timer);
    };
  }, [connection, paused]);

  const sessions = state?.sessions ?? [];
  const blocked = sessions.filter((s) => s.blocked_by);
  const heads = [...new Set(blocked.map((s) => s.blocked_by!))].filter((id) => !blocked.some((b) => b.id === id));
  const visible = sessions.filter((s) => showIdle || s.state !== "idle" || s.blocked_by || heads.includes(s.id));
  const longestBlock = Math.max(0, ...blocked.map((s) => s.wait_ms ?? 0));

  return (
    <div className="view" style={{ overflow: "hidden" }}>
      {error ? (
        <div className="banner">{error.message}</div>
      ) : blocked.length > 0 ? (
        <div className="banner">
          {blocked.length} {blocked.length === 1 ? "session is" : "sessions are"} blocked by {heads.length === 1 ? `session ${heads[0]}` : `${heads.length} sessions`}
          {longestBlock > 0 ? ` for up to ${duration(longestBlock)}` : ""}.
        </div>
      ) : (
        <div className="banner calm">
          <span className="pulse" /> Nothing is blocked{state ? ` · ${sessions.filter((s) => s.state !== "idle").length} active sessions` : ""}
        </div>
      )}
      <div className="toolbar" style={{ marginTop: 10 }}>
        <button className="btn" onClick={() => setPaused((p) => !p)}>
          {paused ? <Play size={13} /> : <Pause size={13} />} {paused ? "Resume" : "Pause"} <span className="kbd">Space</span>
        </button>
        <label className="muted" style={{ display: "flex", alignItems: "center", gap: 6 }}>
          <input type="checkbox" checked={showIdle} onChange={(e) => setShowIdle(e.target.checked)} /> Show idle sessions
        </label>
        <span className="spacer" />
        <span className="muted">{state ? `Updated ${new Date(state.taken_at).toLocaleTimeString()}` : "Loading…"}</span>
      </div>
      <div className="table-wrap">
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
            </tr>
          </thead>
          <tbody>
            {visible.map((s) => (
              <SessionRow key={s.id} session={s} isHead={heads.includes(s.id)} />
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
    </div>
  );
}

function SessionRow({ session: s, isHead }: { session: Session; isHead: boolean }) {
  return (
    <tr className="row">
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
    </tr>
  );
}
