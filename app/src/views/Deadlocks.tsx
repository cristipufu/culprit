import { ArrowUpRight, CircleCheck, ClipboardPaste, Copy, FileWarning } from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { DeadlockGraph } from "../components/DeadlockGraph";
import { Sql } from "../components/Sql";
import type { Deadlock } from "../gen/Deadlock";
import type { DeadlockPattern } from "../gen/DeadlockPattern";
import type { DeadlockProcess } from "../gen/DeadlockProcess";
import type { DeadlockReport } from "../gen/DeadlockReport";
import type { Engine } from "../gen/Engine";
import { api, errorMessage, type ApiError } from "../lib/api";
import { useHotkeys } from "../lib/hotkeys";
import { currentReport, setState, toast, useStore } from "../lib/store";
import { keys } from "../lib/keys";
import { copyAndTell } from "../lib/clipboard";

const PATTERN: Record<DeadlockPattern, string> = {
  "key-lookup": "Key lookup",
  "lock-conversion": "Read, then update",
  "opposite-order": "Opposite order",
  "lock-escalation": "Lock escalation",
  parallelism: "Parallelism",
  cycle: "Lock cycle",
};

const RANGES = [
  { hours: 24, label: "24 hours" },
  { hours: 168, label: "7 days" },
] as const;

type Load = { status: "loading" } | { status: "ready"; report: DeadlockReport } | { status: "error"; error: ApiError };

function when(iso: string | null): { short: string; full: string } {
  if (!iso) return { short: "Time unknown", full: "The engine didn't record when" };
  const d = new Date(iso);
  const today = new Date();
  const sameDay = d.toDateString() === today.toDateString();
  const time = d.toLocaleTimeString([], { hour: "numeric", minute: "2-digit" });
  const short = sameDay ? `Today ${time}` : `${d.toLocaleDateString([], { month: "short", day: "numeric" })}, ${time}`;
  return { short, full: d.toLocaleString() };
}

function sourceLine(engine: Engine): string {
  switch (engine) {
    case "mssql":
      return "Culprit reads SQL Server's built-in system_health session: its event files and the in-memory buffer.";
    case "mysql":
      return "Culprit reads InnoDB's latest deadlock report (SHOW ENGINE INNODB STATUS).";
    case "postgres":
      return "PostgreSQL counts deadlocks but keeps the details in the server log.";
    case "sqlite":
      return "SQLite has no deadlocks to record: a writer that can't get the lock waits busy_timeout, then gets SQLITE_BUSY in the application.";
  }
}

/** Recent deadlocks, explained: what pattern it is, who was rolled back, and the fix. Paste or drop one to explain it. */
export default function Deadlocks({ connection, engine }: { connection: string; engine: Engine }) {
  const [hours, setHours] = useState<number>(24);
  const [load, setLoad] = useState<Load>({ status: "loading" });
  const [pasted, setPasted] = useState<Deadlock[]>([]);
  const [pasteError, setPasteError] = useState<ApiError | null>(null);
  const [selected, setSelected] = useState<string | null>(null);
  const [dragging, setDragging] = useState(false);
  const report = useStore(currentReport);
  const listRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    let cancelled = false;
    setLoad({ status: "loading" });
    api()
      .then((client) => client.deadlocks(connection, hours))
      .then((r) => !cancelled && setLoad({ status: "ready", report: r }))
      .catch((e) => !cancelled && setLoad({ status: "error", error: errorMessage(e) }));
    return () => {
      cancelled = true;
    };
  }, [connection, hours]);

  const all = useMemo(() => [...pasted, ...(load.status === "ready" ? load.report.deadlocks : [])], [pasted, load]);
  const current = all.find((d) => d.id === selected) ?? all[0] ?? null;

  const explain = useCallback(async (text: string) => {
    if (!text.trim()) return;
    try {
      const found = await (await api()).parseDeadlock(text);
      setPasted((p) => [...found, ...p.filter((x) => !found.some((f) => f.id === x.id))]);
      setSelected(found[0]?.id ?? null);
      setPasteError(null);
      toast(found.length === 1 ? "Deadlock explained" : `${found.length} deadlocks explained`);
    } catch (e) {
      setPasteError(errorMessage(e));
    }
  }, []);

  useEffect(() => {
    const onPaste = (e: ClipboardEvent) => {
      const target = e.target as HTMLElement | null;
      if (target && (target.tagName === "INPUT" || target.tagName === "TEXTAREA" || target.isContentEditable)) return;
      const text = e.clipboardData?.getData("text") ?? "";
      if (text.includes("deadlock") || text.includes("DEADLOCK")) {
        e.preventDefault();
        void explain(text);
      }
    };
    window.addEventListener("paste", onPaste);
    return () => window.removeEventListener("paste", onPaste);
  }, [explain]);

  const move = (delta: number) => {
    if (!all.length) return;
    const i = Math.max(0, all.findIndex((d) => d.id === current?.id));
    const next = all[Math.min(all.length - 1, Math.max(0, i + delta))];
    if (next) {
      setSelected(next.id);
      listRef.current?.querySelector(`[data-id="${next.id}"]`)?.scrollIntoView({ block: "nearest" });
    }
  };
  useHotkeys({ j: () => move(1), k: () => move(-1), ArrowDown: () => move(1), ArrowUp: () => move(-1) });

  const onDrop = async (e: React.DragEvent) => {
    e.preventDefault();
    setDragging(false);
    const file = e.dataTransfer.files[0];
    if (file) await explain(await file.text());
    else await explain(e.dataTransfer.getData("text"));
  };

  const findingTitle = (id: string | null) => (id ? report?.findings.find((f) => f.id === id)?.title ?? null : null);
  const counted = load.status === "ready" ? load.report.counted : null;

  return (
    <div
      className={`dl-view ${dragging ? "dragging" : ""}`}
      onDragOver={(e) => {
        e.preventDefault();
        setDragging(true);
      }}
      onDragLeave={(e) => e.currentTarget === e.target && setDragging(false)}
      onDrop={onDrop}
    >
      <div className="dl-toolbar">
        {/* PostgreSQL only counts deadlocks since its statistics were reset: there's no window to choose. */}
        {engine !== "postgres" && (
          <span className="seg" role="radiogroup" aria-label="How far back">
            {RANGES.map((r) => (
              <button key={r.hours} role="radio" aria-checked={hours === r.hours} className={hours === r.hours ? "on" : ""} onClick={() => setHours(r.hours)}>
                {r.label}
              </button>
            ))}
          </span>
        )}
        <span className="muted dl-source">{sourceLine(engine)}</span>
        <span className="spacer" />
        <span className="muted dl-paste-hint">
          <ClipboardPaste size={12} /> Paste <span className="kbd">{keys("⌘V")}</span> or drop a deadlock to explain it
        </span>
      </div>

      {pasteError && (
        <div className="dl-inline-note" role="alert">
          <span className="sev medium" aria-hidden /> {pasteError.message}
          {pasteError.hint && <span className="muted"> {pasteError.hint}</span>}
          <button className="btn ghost small" onClick={() => setPasteError(null)}>
            Dismiss
          </button>
        </div>
      )}

      {load.status === "loading" && all.length === 0 ? (
        <Skeleton />
      ) : load.status === "error" && all.length === 0 ? (
        <div className="dl-empty">
          <FileWarning size={22} />
          <h3>Couldn't read deadlocks</h3>
          <p>{load.error.message}</p>
          {load.error.hint && <p className="muted">{load.error.hint}</p>}
        </div>
      ) : all.length === 0 ? (
        <div className="dl-empty">
          {counted != null && counted > 0 ? (
            <>
              <div className="dl-counter">
                <span className="dl-counter-value">{counted.toLocaleString()}</span>
                <span className="dl-counter-label">
                  deadlock{counted === 1 ? "" : "s"} since statistics were reset
                  {load.status === "ready" && load.report.counted_since ? ` (${when(load.report.counted_since).short})` : ""}
                </span>
              </div>
              <p>
                PostgreSQL keeps only this counter. The details (who waited for whom, and both statements) go to the server log:
                paste the lines from <code>deadlock detected</code> to <code>STATEMENT</code> below and Culprit draws the cycle and names the fix.
              </p>
              {load.status === "ready" && load.report.note?.includes("log_lock_waits") && (
                <p className="muted">Tip: turn on log_lock_waits to also log lock waits longer than deadlock_timeout.</p>
              )}
            </>
          ) : (
            <>
              <CircleCheck size={22} className="dl-ok" />
              {/* A counter (PostgreSQL) covers the time since statistics were reset, not the range. */}
              <h3>
                {counted != null
                  ? `No deadlocks since statistics were reset${load.status === "ready" && load.report.counted_since ? ` (${when(load.report.counted_since).short})` : ""}`
                  : `No deadlocks in the last ${RANGES.find((r) => r.hours === hours)?.label}`}
              </h3>
              <p className="muted">{load.status === "ready" && load.report.note ? load.report.note : sourceLine(engine)}</p>
            </>
          )}
          <DropZone onText={explain} />
        </div>
      ) : (
        <div className="dl-split">
          <div className="dl-list" ref={listRef} role="listbox" aria-label="Deadlocks" aria-activedescendant={current ? `dl-${current.id}` : undefined}>
            {all.map((d) => {
              const victim = d.processes.find((p) => p.victim);
              const t = when(d.at);
              return (
                <button
                  key={d.id}
                  id={`dl-${d.id}`}
                  data-id={d.id}
                  role="option"
                  aria-selected={d.id === current?.id}
                  className={`dl-item ${d.id === current?.id ? "on" : ""}`}
                  onClick={() => setSelected(d.id)}
                >
                  <span className="dl-item-top">
                    <span className="dl-pattern">{PATTERN[d.pattern]}</span>
                    <span className="muted" title={t.full}>
                      {d.source === "pasted" ? "Pasted" : t.short}
                    </span>
                  </span>
                  <span className="dl-item-objects">{d.objects.length ? d.objects.join(", ") : "Objects not named"}</span>
                  {victim && <span className="dl-item-victim muted">Rolled back: {victim.application ?? victim.login ?? (victim.session ? `session ${victim.session}` : victim.id)}</span>}
                </button>
              );
            })}
            {load.status === "ready" && load.report.note && <p className="dl-list-note muted">{load.report.note}</p>}
          </div>
          {current && (
            <article className="dl-detail" key={current.id}>
              <div className="dl-detail-meta">
                <span className="dl-pattern">{PATTERN[current.pattern]}</span>
                <span className="muted" title={when(current.at).full}>
                  {current.source === "pasted" ? "Pasted" : when(current.at).short} · {current.source}
                </span>
              </div>
              <h2 className="dl-headline">{current.headline}</h2>
              <DeadlockGraph deadlock={current} onPick={(p: DeadlockProcess) => document.getElementById(`dl-proc-${p.id}`)?.scrollIntoView({ block: "nearest", behavior: "smooth" })} />
              <p className="dl-explain">{current.explanation}</p>

              <h3 className="dl-section">How to fix it</h3>
              <ol className="dl-fixes">
                {current.fixes.map((f, i) => {
                  const related = findingTitle(f.finding);
                  return (
                    <li key={i} className="dl-fix">
                      <div className="dl-fix-title">{f.title}</div>
                      <p>{f.detail}</p>
                      {f.sql && (
                        <div className="dl-fix-sql">
                          <Sql text={f.sql} pretty={false} />
                          <button className="btn ghost small" onClick={() => copyAndTell(f.sql!, "Copied")} aria-label="Copy SQL">
                            <Copy size={12} /> Copy
                          </button>
                        </div>
                      )}
                      {related && f.finding && (
                        <button className="link-button dl-related" onClick={() => setState({ view: "overview", finding: f.finding })}>
                          Same fix as the finding “{related}” <ArrowUpRight size={12} />
                        </button>
                      )}
                    </li>
                  );
                })}
              </ol>

              <h3 className="dl-section">Who was involved</h3>
              <div className="dl-procs">
                {current.processes.map((p) => (
                  <div key={p.id} id={`dl-proc-${p.id}`} className={`dl-proc-card ${p.victim ? "victim" : ""}`}>
                    <div className="dl-proc-head">
                      <strong>{p.session ? `Session ${p.session}` : `Process ${p.id}`}</strong>
                      {p.victim && <span className="pill victim">rolled back</span>}
                    </div>
                    <div className="dl-proc-facts muted">
                      {[p.application, p.login && p.host ? `${p.login}@${p.host}` : (p.login ?? p.host), p.isolation, p.wait_mode ? `wanted ${p.wait_mode}` : null, p.procedure ? `in ${p.procedure}` : null]
                        .filter(Boolean)
                        .join(" · ")}
                    </div>
                    {p.statement ? <Sql text={p.statement} className="dl-proc-sql" /> : <p className="muted">The engine didn't record the statement.</p>}
                  </div>
                ))}
              </div>
            </article>
          )}
        </div>
      )}
      {dragging && <div className="dl-drop-overlay">Drop to explain this deadlock</div>}
    </div>
  );
}

function DropZone({ onText }: { onText: (text: string) => Promise<void> }) {
  const [text, setText] = useState("");
  return (
    <div className="dl-dropzone">
      <ClipboardPaste size={16} />
      <div>
        <strong>Have a deadlock from somewhere else?</strong>
        <p className="muted">Paste a SQL Server deadlock graph (.xdl), the LATEST DETECTED DEADLOCK section from MySQL or MariaDB, or PostgreSQL log lines from “deadlock detected”. Culprit names the pattern and the fix.</p>
        <textarea
          aria-label="Paste a deadlock"
          placeholder={keys("Paste here, or press ⌘V anywhere on this page")}
          value={text}
          onChange={(e) => setText(e.target.value)}
          rows={3}
          spellCheck={false}
        />
        <button className="btn small" disabled={!text.trim()} onClick={() => onText(text).then(() => setText(""))}>
          Explain
        </button>
      </div>
    </div>
  );
}

function Skeleton() {
  return (
    <div className="dl-split" aria-busy="true" aria-label="Loading deadlocks">
      <div className="dl-list">
        {[0, 1, 2].map((i) => (
          <div key={i} className="dl-item skeleton">
            <span className="sk sk-short" />
            <span className="sk" />
          </div>
        ))}
      </div>
      <div className="dl-detail">
        <span className="sk sk-title" />
        <span className="sk sk-block" />
      </div>
    </div>
  );
}
