import { ArrowUpRight, Info, Lightbulb, Radio, RotateCw, X } from "lucide-react";
import { useEffect, useMemo, useRef, useState } from "react";
import type { Report } from "../gen/Report";
import type { WaitCategory } from "../gen/WaitCategory";
import type { WaitSample } from "../gen/WaitSample";
import type { WaitsReport } from "../gen/WaitsReport";
import { api, errorMessage, type ApiError } from "../lib/api";
import { count, duration, percent } from "../lib/format";
import { changeRequest, setState, useStore } from "../lib/store";
import { WAIT_LABEL, foldWaits, startSample, useSample, waitColor } from "../lib/waits";
import { CloudLimits } from "./CloudLimits";
import { ActivityHistoryView } from "./ActivityHistory";
import { HealthTab, SignalTile } from "./Health";
import type { WaitsTab } from "./Waits";
import type { Engine } from "../gen/Engine";
import { can, isMariadb, serverName } from "../lib/engines";

type Loaded = { status: "loading" } | { status: "ready"; report: WaitsReport | null } | { status: "error"; error: ApiError };

const TABS: { key: WaitsTab; label: string }[] = [
  { key: "waits", label: "Where time goes" },
  { key: "now", label: "Right now" },
  { key: "health", label: "Health" },
];

/** Where database time goes, explained: categories with their queries, a live sample, and the health signals. */
export default function WaitsPanel({
  report,
  initialTab,
  initialCategory,
  onClose,
}: {
  report: Report;
  initialTab: WaitsTab;
  initialCategory?: string;
  onClose: () => void;
}) {
  const connection = report.snapshot.connection;
  const windowMinutes = useStore((s) => s.windowMinutes);
  // Sampling right now needs a server to ask (not SQLite).
  const sampling = can(report.snapshot.server.engine, "wait_sample");
  const tabs = TABS.filter((t) => t.key !== "now" || sampling);
  const [tab, setTab] = useState<WaitsTab>(initialTab === "now" && !sampling ? "waits" : initialTab);
  const [loaded, setLoaded] = useState<Loaded>({ status: "loading" });
  const [selected, setSelected] = useState<string | undefined>(initialCategory);
  const sheet = useRef<HTMLDivElement>(null);
  const engine = serverName(report.snapshot.server.engine, report.snapshot.server.product);

  useEffect(() => {
    let live = true;
    api()
      .then((client) => client.waits(connection, windowMinutes))
      .then((r) => live && setLoaded({ status: "ready", report: r }))
      .catch((e) => live && setLoaded({ status: "error", error: errorMessage(e) }));
    return () => {
      live = false;
    };
  }, [connection, windowMinutes]);

  useEffect(() => {
    sheet.current?.focus();
  }, []);

  const categories = loaded.status === "ready" ? (loaded.report?.categories ?? []) : [];
  const activity = loaded.status === "ready" ? loaded.report?.activity : undefined;
  const title = tab === "health" ? "Database health" : tab === "now" ? "Live database activity" : activity ? "Recorded database activity" : "Where database time goes";
  const navigation = activity?.categories ?? categories;
  const current = categories.find((c) => c.key === selected) ?? categories[0];

  const onKey = (e: React.KeyboardEvent) => {
    // The panel is modal: the views' and app's shortcuts underneath must not react while it's open.
    e.stopPropagation();
    if (e.key === "Escape") return onClose();
    if (e.target instanceof HTMLInputElement) return;
    const index = tabs.findIndex((t) => t.key === tab);
    if (e.key === "ArrowRight" || e.key === "ArrowLeft") {
      e.preventDefault();
      setTab(tabs[(index + (e.key === "ArrowRight" ? 1 : tabs.length - 1)) % tabs.length]!.key);
    }
    const n = Number(e.key);
    if (n >= 1 && n <= tabs.length) setTab(tabs[n - 1]!.key);
    if (tab === "waits" && navigation.length && (e.key === "ArrowDown" || e.key === "ArrowUp" || e.key === "j" || e.key === "k")) {
      e.preventDefault();
      const at = Math.max(0, navigation.findIndex((c) => c.key === selected));
      const next = e.key === "ArrowDown" || e.key === "j" ? Math.min(navigation.length - 1, at + 1) : Math.max(0, at - 1);
      setSelected(navigation[next]!.key);
    }
    if (e.key === "r" && tab === "now") void startSample(connection, 30);
  };

  return (
    <div className="waits-backdrop" onMouseDown={(e) => e.target === e.currentTarget && onClose()}>
      <div className="waits-sheet" role="dialog" aria-modal="true" aria-label={title} tabIndex={-1} ref={sheet} onKeyDown={onKey}>
        <header className="waits-header">
          <div>
            <h2>{title}</h2>
            <div className="waits-sub">
              {connection} · {engine}
              {tab === "waits" && loaded.status === "ready" && loaded.report ? ` · ${activity?.period.source ?? loaded.report.source}` : ""}
            </div>
          </div>
          <button className="icon-btn" onClick={onClose} title="Close (Esc)" aria-label="Close">
            <X size={16} />
          </button>
        </header>
        <nav className="waits-tabs" role="tablist">
          {tabs.map((t, i) => (
            <button key={t.key} role="tab" aria-selected={tab === t.key} className={tab === t.key ? "on" : ""} onClick={() => setTab(t.key)}>
              {t.key === "waits" && activity ? "Recorded activity" : t.label}
              <span className="kbd">{i + 1}</span>
            </button>
          ))}
        </nav>
        <div className="waits-body">
          {tab === "waits" &&
            (loaded.status === "loading" ? (
              <WaitsSkeleton />
            ) : loaded.status === "error" ? (
              <div className="waits-empty">
                <h3>{loaded.error.message}</h3>
                {loaded.error.hint && <p>{loaded.error.hint}</p>}
              </div>
            ) : activity ? (
              <ActivityHistoryView activity={activity} selected={selected} onSelect={setSelected} onClose={onClose} />
            ) : !loaded.report || !current ? (
              <NoHistory engine={report.snapshot.server.engine} product={report.snapshot.server.product} reason={report.snapshot.sources?.find((s) => s.source === "docdb-activity-history")?.reason} onSample={sampling ? () => setTab("now") : undefined} />
            ) : (
              <WaitsView report={loaded.report} current={current} onSelect={setSelected} onClose={onClose} onSample={() => setTab("now")} />
            ))}
          {tab === "now" && <RightNow connection={connection} history={loaded.status === "ready" ? loaded.report : null} engine={report.snapshot.server.engine} />}
          {tab === "health" && (
            <>
              <HealthTab report={report} pressure={report.snapshot.pressure ?? null} resources={report.snapshot.resources} engineLabel={engine} engine={report.snapshot.server.engine} />
              <CloudLimits
                connection={connection}
                windowMinutes={windowMinutes ?? report.snapshot.options?.window_minutes ?? (report.snapshot.options?.window_days ?? 7) * 1440}
                onCompare={(from, to) => {
                  onClose();
                  setState({ changes: changeRequest({ recent_start: new Date(from).toISOString(), recent_end: new Date(to).toISOString() }) });
                }}
              />
            </>
          )}
        </div>
      </div>
    </div>
  );
}

function WaitsSkeleton() {
  return (
    <div className="waits-grid">
      <div className="waits-list">
        {[0, 1, 2, 3].map((i) => (
          <div key={i} className="skeleton-row" />
        ))}
      </div>
      <div className="waits-detail">
        <div className="skeleton-row wide" />
        <div className="skeleton-row" />
      </div>
    </div>
  );
}

/** Why a scan has no waits, in the engine's own terms. */
const NO_WAIT_HISTORY: Record<Engine, string> = {
  mssql: "The login may not be allowed to read wait statistics, or Query Store isn't capturing waits. A live sample still works.",
  postgres: "PostgreSQL keeps no wait history. Turn on track_io_timing to time disk reads per statement, and sample what sessions wait on right now.",
  yugabyte: "DocDB RPC timings may overlap and do not provide an exclusive historical wait mix. Inspect native storage metrics or sample active YSQL sessions.",
  mysql: "performance_schema isn't timing the waits (its wait instruments and consumers are off), or the login can't read them. A live sample still works.",
  sqlite: "SQLite has no server, so nothing records what it waits on. Waiting for the write lock shows up in the application, as SQLITE_BUSY or slow commits.",
};

function NoHistory({ engine, product, reason, onSample }: { engine: Engine; product: string; reason?: string | null; onSample?: () => void }) {
  return (
    <div className="waits-empty">
      <h3>This scan has no record of what queries waited on</h3>
      <p>{isMariadb(engine, product) ? NO_WAIT_HISTORY.mysql.replace("performance_schema", "MariaDB's performance_schema") : NO_WAIT_HISTORY[engine]}</p>
      {reason && <p className="muted">{reason}</p>}
      {onSample && (
        <button className="btn primary" onClick={onSample}>
          <Radio size={13} /> Sample right now
        </button>
      )}
    </div>
  );
}

function Bar({ items }: { items: { key: string; label: string; share: number }[] }) {
  return (
    <div className="waits-strip" aria-hidden>
      {items.map((c) => (
        <span key={c.key} style={{ flexGrow: Math.max(c.share, 0.004), background: waitColor(c.key) }} title={`${c.label} · ${percent(c.share)}`} />
      ))}
    </div>
  );
}

function WaitsView({
  report,
  current,
  onSelect,
  onClose,
  onSample,
}: {
  report: WaitsReport;
  current: WaitCategory;
  onSelect: (key: string) => void;
  onClose: () => void;
  onSample: () => void;
}) {
  const openQuery = (id: string) => {
    setState({ view: "queries", query: id });
    onClose();
  };
  return (
    <div className="waits-grid">
      <div className="waits-list" role="listbox" aria-label="Wait categories">
        <Bar items={report.categories} />
        {report.categories.map((c) => (
          <button
            key={c.key}
            role="option"
            aria-selected={c.key === current.key}
            className={`waits-row ${c.key === current.key ? "on" : ""}`}
            style={{ "--cat": waitColor(c.key) } as React.CSSProperties}
            onClick={() => onSelect(c.key)}
          >
            <i className="swatch" />
            <span className="waits-row-text">
              <span className="name">{c.label}</span>
              {c.top_wait && <span className="top mono">{c.top_wait}</span>}
            </span>
            <span className="waits-row-num">
              <b>{percent(c.share)}</b>
              <span>{duration(c.wait_ms)}</span>
            </span>
          </button>
        ))}
        <div className="waits-keys">
          <span className="kbd">↑</span>
          <span className="kbd">↓</span> categories · <span className="kbd">Esc</span> close
        </div>
      </div>
      <div className="waits-detail" key={current.key}>
        <div className="detail-head">
          <i className="swatch big" style={{ background: waitColor(current.key) }} />
          <h3>{current.label}</h3>
          <span className="pill">{percent(current.share)} of recorded time</span>
          <span className="muted mono small">{current.top_wait}</span>
        </div>
        <p className="meaning">{current.meaning}</p>
        {current.detail && (
          <div className="insight">
            <Lightbulb size={14} />
            <span>{current.detail}</span>
          </div>
        )}
        <h4 className="section-title">
          Queries behind it <span className="muted">{current.queries.length ? `${current.queries.length} shown` : ""}</span>
        </h4>
        {current.queries.length ? (
          <div className="wq-list">
            {current.queries.map((q) => (
              <button key={q.id} className="wq-row" onClick={() => openQuery(q.id)} title="Open in Queries">
                <span className="wq-share">
                  <span className="wq-track">
                    <span style={{ width: `${Math.max(2, q.share_of_category * 100)}%`, background: waitColor(current.key) }} />
                  </span>
                  <b>{percent(q.share_of_category)}</b>
                </span>
                <span className="wq-text mono">{q.text}</span>
                <span className="wq-meta">
                  {/* Waits of a parallel query's threads add up, so they can exceed its elapsed time: per run is always true. */}
                  {report.engine === "postgres" && current.key === "other" ? "recorded time" : "waits"} {duration(q.executions > 0 ? q.wait_ms / q.executions : q.wait_ms)} per run
                  {q.share_of_query < 0.95 ? ` (${percent(q.share_of_query)} of its time)` : ""} · {count(q.executions)} {q.executions === 1 ? "run" : "runs"}
                  {q.rows_per_run != null ? ` · ${count(q.rows_per_run)} ${q.rows_per_run === 1 ? "row" : "rows"} per run` : ""}
                </span>
                <ArrowUpRight className="wq-go" size={13} />
              </button>
            ))}
          </div>
        ) : (
          <div className="calm-note">
            <Info size={14} />
            <span>
              {current.attribution ?? "No query recorded this wait."}{" "}
              <button className="link" onClick={onSample}>
                Sample right now
              </button>
            </span>
          </div>
        )}
        {report.notes.length > 0 && (
          <div className="waits-notes">
            {report.notes.map((n) => (
              <div key={n} className="calm-note">
                <Info size={14} />
                <span>{n}</span>
              </div>
            ))}
          </div>
        )}
      </div>
    </div>
  );
}

const LENGTHS = [10, 30, 60];

function RightNow({ connection, history, engine }: { connection: string; history: WaitsReport | null; engine: Engine }) {
  const state = useSample(connection);
  const [seconds, setSeconds] = useState(30);
  const [now, setNow] = useState(Date.now());
  const sessionSamples = engine === "postgres" || engine === "yugabyte";
  useEffect(() => {
    if (state.status !== "running") return;
    const id = setInterval(() => setNow(Date.now()), 250);
    return () => clearInterval(id);
  }, [state.status]);

  if (state.status === "running") {
    const elapsed = (now - state.startedAt) / 1000;
    const left = Math.max(0, Math.ceil(state.seconds - elapsed));
    const done = Math.min(1, elapsed / state.seconds);
    return (
      <div className="now-running" aria-live="polite">
        <div className="now-ring" style={{ "--p": done } as React.CSSProperties}>
          <span>{left}s</span>
        </div>
        <h3>Sampling what {connection} waits on…</h3>
        <p className="muted">
          {sessionSamples
            ? `Looking at every busy session once a second for ${state.seconds} seconds.`
            : `Reading the server's wait counters at the start and end of ${state.seconds} seconds.`}{" "}
          You can close this panel; the sample keeps running and appears here when it's done.
        </p>
      </div>
    );
  }
  const start = () => void startSample(connection, seconds);
  return (
    <div className="now-tab">
      <div className="now-controls">
        <div>
          <h3>{state.status === "done" ? "What it waited on just now" : "What is it waiting on right now?"}</h3>
          <p className="muted">
            {sessionSamples
              ? "Culprit looks at every busy session once a second and counts what each one waits on. Read-only and safe on production."
              : "Culprit reads the server's wait counters twice and shows the difference. Read-only and safe on production."}
          </p>
        </div>
        <div className="now-actions">
          <span className="seg" role="radiogroup" aria-label="Sample length">
            {LENGTHS.map((s) => (
              <button key={s} role="radio" aria-checked={seconds === s} className={seconds === s ? "on" : ""} onClick={() => setSeconds(s)}>
                {s} s
              </button>
            ))}
          </span>
          <button className="btn primary" onClick={start}>
            {state.status === "done" ? <RotateCw size={13} /> : <Radio size={13} />} {state.status === "done" ? "Sample again" : `Sample for ${seconds} s`} <span className="kbd">R</span>
          </button>
        </div>
      </div>
      {state.status === "error" && (
        <div className="calm-note">
          <Info size={14} />
          <span>
            {state.error.message}
            {state.error.hint ? ` ${state.error.hint}` : ""}
          </span>
        </div>
      )}
      {state.status === "done" ? <SampleView sample={state.sample} history={history} /> : history && history.categories.length > 0 && <Usual history={history} />}
    </div>
  );
}

/** The scan's mix of waiting, shown before a sample so "right now" has something to be compared with. */
function Usual({ history }: { history: WaitsReport }) {
  return (
    <div className="usual">
      <div className="usual-head">
        <span className="usual-title">Usual mix</span>
        <span className="muted">{history.source}</span>
      </div>
      <Bar items={history.categories} />
      <div className="usual-legend">
        {history.categories.slice(0, 6).map((c) => (
          <span key={c.key}>
            <i style={{ background: waitColor(c.key) }} />
            {c.label} <b>{percent(c.share)}</b>
          </span>
        ))}
      </div>
      <p className="muted usual-why">
        Sample while users report slowness, during a deploy, or after a fix: the result shows which kinds of waiting grew or shrank against this mix.
      </p>
    </div>
  );
}

function SampleView({ sample, history }: { sample: WaitSample; history: WaitsReport | null }) {
  const folded = useMemo(() => foldWaits(sample.categories), [sample]);
  const past = new Map((history?.categories ?? []).map((c) => [c.key, c.share]));
  const taken = new Date(sample.taken_at);
  return (
    <div className="sample-view">
      <div className="sample-meta">
        {Math.round(sample.seconds)} s ending {Number.isNaN(taken.getTime()) ? "" : taken.toLocaleTimeString([], { hour: "numeric", minute: "2-digit", second: "2-digit" })}
        {sample.active_sessions != null ? ` · ${count(sample.active_sessions)} busy sessions on average` : ""}
      </div>
      {history && history.categories.length > 0 && folded.length > 0 && (
        <div className="compare-bars">
          <span className="compare-label">Now</span>
          <Bar items={folded} />
          <span className="compare-label muted">Usual</span>
          <Bar items={history.categories} />
        </div>
      )}
      {folded.length === 0 ? (
        <div className="calm-note">
          <Info size={14} />
          <span>Nothing waited during the sample: the server was idle, or nothing ran long enough to register.</span>
        </div>
      ) : (
        <>
          {!(history && history.categories.length > 0) && <Bar items={folded} />}
          <div className="sample-rows">
            {folded.map((c) => {
              const before = past.get(c.key);
              const delta = before != null ? (c.share - before) * 100 : null;
              return (
                <div key={c.key} className="sample-row" style={{ "--cat": waitColor(c.key) } as React.CSSProperties}>
                  <i className="swatch" />
                  <span className="name">{WAIT_LABEL[c.key] ?? c.label}</span>
                  <span className="top mono">{c.top}</span>
                  <span className="num">
                    <b>{percent(c.share)}</b> {duration(c.ms)}
                  </span>
                  <span className={`delta ${delta == null ? "new" : Math.abs(delta) < 5 ? "flat" : delta > 0 ? "up" : "down"}`}>
                    {delta == null ? "not in history" : Math.abs(delta) < 5 ? "as usual" : `${delta > 0 ? "+" : "−"}${Math.round(Math.abs(delta))} pts vs history`}
                  </span>
                </div>
              );
            })}
          </div>
        </>
      )}
      {sample.signals.length > 0 && (
        <div className="signal-grid compact">
          {sample.signals.map((s) => (
            <SignalTile key={s.key} signal={s} />
          ))}
        </div>
      )}
      <div className="sample-method muted">
        <Info size={12} /> {sample.method}. Shares above are of waiting during the sample; “vs history” compares them with the scan.
      </div>
      {sample.notes.map((n) => (
        <div key={n} className="calm-note">
          <Info size={14} />
          <span>{n}</span>
        </div>
      ))}
    </div>
  );
}
