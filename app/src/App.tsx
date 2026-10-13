import { lazy, Suspense, useEffect } from "react";
import { CommandPalette } from "./components/CommandPalette";
import { SignInDialog } from "./components/SignInDialog";
import { Sidebar } from "./components/Sidebar";
import { WindowPicker } from "./components/WindowPicker";
import { when } from "./lib/format";
import { useHotkeys } from "./lib/hotkeys";
import { installPlanPaste } from "./lib/plans";
import { getState, lastConnection, loadConnections, openConnection, scan, setState, useStore, type View } from "./lib/store";
import { Connections } from "./views/Connections";
import { Live } from "./views/Live";
import { Overview } from "./views/Overview";
import { Queries } from "./views/Queries";
import { Schema } from "./views/Schema";
import { PasswordNeeded } from "./components/PasswordNeeded";
import { DashboardSkeleton } from "./components/DashboardSkeleton";
import { startUpdateChecks } from "./lib/updates";
import type { Capability } from "./gen/Capability";

const Editor = lazy(() => import("./views/Editor"));
// Where a connection opens; its skeleton (and stylesheet) are in the main bundle so the grid never jumps.
const Dashboard = lazy(() => import("./views/Dashboard"));
// Statements coding agents want to run; loaded after first paint, polls only in the desktop app.
const ApprovalDialog = lazy(() => import("./components/ApprovalDialog"));
const Plans = lazy(() => import("./views/Plans").then((m) => ({ default: m.Plans })));

const TITLES: Record<View, string> = {
  dashboard: "Dashboard",
  overview: "Findings",
  queries: "Queries",
  live: "Live",
  schema: "Schema",
  editor: "Editor",
  plans: "Plans",
  connections: "Connections",
};

/** The sources a scan's query history comes from; when one is missing, its reason says why there is no workload. */
const WORKLOAD_SOURCES: Capability[] = ["query-store", "plan-cache", "pg-stat-statements", "performance-schema", "slow-log"];

const VIEW_KEYS: View[] = ["dashboard", "overview", "queries", "live", "schema", "editor", "plans"];

export function App() {
  const view = useStore((s) => s.view);
  const active = useStore((s) => s.active);
  const environment = useStore((s) => s.connections.find((c) => c.name === s.active)?.environment);
  const palette = useStore((s) => s.palette);
  const toast = useStore((s) => s.toast);

  useEffect(() => installPlanPaste(() => getState().view), []);
  useEffect(() => startUpdateChecks(), []);

  useEffect(() => {
    loadConnections().then((connections) => {
      const last = lastConnection();
      if (last && connections.some((c) => c.name === last)) {
        // The dashboard is where it opens: fetch its code alongside the cached report, not after it.
        void import("./views/Dashboard");
        openConnection(last);
      }
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
          if (getState().active || v === "plans") setState({ view: v });
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
      <main className={`main ${view !== "connections" && environment === "production" ? "is-production" : ""}`}>
        {view !== "connections" && <TopBar />}
        <div className="view">
          {view === "plans" ? (
            <Suspense fallback={<div className="empty"><div className="spinner" /></div>}>
              <Plans />
            </Suspense>
          ) : view === "connections" || !active ? (
            <Connections />
          ) : (
            <Workspace view={view} name={active} />
          )}
        </div>
      </main>
      {palette && <CommandPalette />}
      <SignInDialog />
      {toast && <div className="toast">{toast}</div>}
      <Suspense fallback={null}>
        <ApprovalDialog />
      </Suspense>
    </div>
  );
}

function TopBar() {
  const view = useStore((s) => s.view);
  const environment = useStore((s) => s.connections.find((c) => c.name === s.active)?.environment);
  const report = useStore((s) => (s.report.status === "ready" ? s.report : null));
  const engine = useStore((s) => s.connections.find((c) => c.name === s.active)?.engine);
  const w = report?.report.snapshot.workload;
  // Before the report loads, SQL Server keeps the picker's place; a scan without query history has nothing to window.
  const windowed = w ? w.windowed : engine === "mssql" && !report;
  const noHistory = report && !w ? report.report.snapshot.server.missing.find((m) => WORKLOAD_SOURCES.includes(m.capability)) : undefined;
  const showsWorkload = view === "dashboard" || view === "overview" || view === "queries" || view === "schema";
  if (view === "plans") {
    return (
      <header className="topbar" data-tauri-drag-region>
        <h1>{TITLES[view]}</h1>
        <span className="context">Paste, drop or open plans · compare any two</span>
      </header>
    );
  }
  return (
    <header className="topbar" data-tauri-drag-region>
      <h1>{TITLES[view]}</h1>
      {showsWorkload && windowed && <WindowPicker coveredHours={w?.window_hours ?? null} />}
      {showsWorkload && report && !w && (
        <span className="context" title={noHistory?.reason ?? "This scan found no statement statistics."}>
          No query history in this scan
        </span>
      )}
      {showsWorkload && !windowed && w && (
        <span className="context" title={`Cumulative statistics since they were last reset${w.window_start ? ` (${when(w.window_start).full})` : ""}; this source keeps no time windows.`}>
          {w.source} · since {w.window_start ? when(w.window_start).short : "the last reset"}
        </span>
      )}
      {environment && (
        <span
          className={`env-chip env-${environment}`}
          title={environment === "production" ? "Production: reads run as they are; anything that changes data asks first" : `${environment} connection`}
        >
          {environment}
        </span>
      )}
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
  if (state.status === "loading" || state.status === "none") return view === "dashboard" ? <DashboardSkeleton /> : <Loading />;
  if (state.status === "error") {
    if (connection?.auth === "password" && (state.error.code === "auth_failed" || !connection.has_secret)) {
      return <PasswordNeeded connection={connection} message={state.error.message} />;
    }
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
  if (view === "dashboard" && connection) {
    return (
      <Suspense fallback={<DashboardSkeleton />}>
        <Dashboard key={name} report={report} connection={connection} />
      </Suspense>
    );
  }
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
