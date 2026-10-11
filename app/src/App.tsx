import { lazy, Suspense, useEffect } from "react";
import { CommandPalette } from "./components/CommandPalette";
import { Sidebar } from "./components/Sidebar";
import { WindowPicker } from "./components/WindowPicker";
import { useHotkeys } from "./lib/hotkeys";
import { getState, lastConnection, loadConnections, openConnection, scan, setState, useStore, type View } from "./lib/store";
import { Connections } from "./views/Connections";
import { Live } from "./views/Live";
import { Overview } from "./views/Overview";
import { Queries } from "./views/Queries";
import { Schema } from "./views/Schema";

const Editor = lazy(() => import("./views/Editor"));

const TITLES: Record<View, string> = {
  overview: "Overview",
  queries: "Queries",
  live: "Live",
  schema: "Schema",
  editor: "Editor",
  connections: "Connections",
};

const VIEW_KEYS: View[] = ["overview", "queries", "live", "schema", "editor"];

export function App() {
  const view = useStore((s) => s.view);
  const active = useStore((s) => s.active);
  const palette = useStore((s) => s.palette);
  const toast = useStore((s) => s.toast);

  useEffect(() => {
    loadConnections().then((connections) => {
      const last = lastConnection();
      if (last && connections.some((c) => c.name === last)) openConnection(last);
    });
  }, []);

  useHotkeys({
    "mod+k": () => setState((s) => ({ palette: !s.palette })),
    "mod+r": () => {
      const name = getState().active;
      if (name) scan(name);
    },
    ...Object.fromEntries(
      VIEW_KEYS.map((v, i) => [
        String(i + 1),
        () => {
          if (getState().active) setState({ view: v });
        },
      ]),
    ),
    Escape: () => {
      if (getState().palette) setState({ palette: false });
    },
  });

  return (
    <div className="shell">
      <Sidebar />
      <main className="main">
        {view !== "connections" && <TopBar />}
        <div className="view">{view === "connections" || !active ? <Connections /> : <Workspace view={view} name={active} />}</div>
      </main>
      {palette && <CommandPalette />}
      {toast && <div className="toast">{toast}</div>}
    </div>
  );
}

function TopBar() {
  const view = useStore((s) => s.view);
  const report = useStore((s) => (s.report.status === "ready" ? s.report : null));
  const engine = useStore((s) => s.connections.find((c) => c.name === s.active)?.engine);
  const w = report?.report.snapshot.workload;
  const windowed = w ? w.windowed : engine === "mssql";
  const showsWorkload = view === "overview" || view === "queries" || view === "schema";
  return (
    <header className="topbar" data-tauri-drag-region>
      <h1>{TITLES[view]}</h1>
      {showsWorkload && windowed && <WindowPicker coveredHours={w?.window_hours ?? null} />}
      {showsWorkload && !windowed && w && (
        <span className="context" title="Cumulative statistics since they were last reset; this source keeps no time windows.">
          {w.source} · since {w.window_start ? new Date(w.window_start).toLocaleDateString() : "the last reset"}
        </span>
      )}
      {report?.report.snapshot.environment === "production" && <span className="context">read-only</span>}
      <span className="spacer" />
      {report?.scanError && (
        <span className="tag high" title={report.scanError.hint ?? ""}>
          Last scan failed: {report.scanError.message}
        </span>
      )}
    </header>
  );
}

function Workspace({ view, name }: { view: View; name: string }) {
  const state = useStore((s) => s.report);
  const connection = useStore((s) => s.connections.find((c) => c.name === name));
  if (view === "live") return <Live connection={name} />;
  if (view === "editor" && connection) {
    return (
      <Suspense fallback={<div className="empty"><div className="spinner" /></div>}>
        <Editor connection={connection} />
      </Suspense>
    );
  }
  if (state.status === "loading" || state.status === "none") return <Loading />;
  if (state.status === "error") {
    return (
      <div className="empty">
        <h3>Couldn't scan {name}</h3>
        <p>{state.error.message}</p>
        {state.error.hint && <p className="muted">{state.error.hint}</p>}
        <button className="btn" onClick={() => scan(name)}>
          Try again
        </button>
      </div>
    );
  }
  const report = state.report;
  if (view === "queries") return <Queries report={report} />;
  if (view === "schema") return <Schema report={report} />;
  return <Overview report={report} />;
}

function Loading() {
  return (
    <div className="overview">
      <div className="ov-list" style={{ padding: 18, gap: 12, display: "flex", flexDirection: "column" }}>
        <div className="skeleton" style={{ height: 26, width: "45%" }} />
        <div className="skeleton" style={{ height: 16, width: "80%" }} />
        <div className="skeleton" style={{ height: 58 }} />
        {[0, 1, 2, 3, 4].map((i) => (
          <div key={i} className="skeleton" style={{ height: 52, opacity: 1 - i * 0.15 }} />
        ))}
      </div>
      <div className="empty">
        <div className="spinner" />
        Reading statistics views…
      </div>
    </div>
  );
}
