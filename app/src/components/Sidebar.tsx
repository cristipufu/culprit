import { Activity, ChevronsUpDown, Command, Gauge, GitCompareArrows, ListOrdered, Moon, RefreshCw, SquareTerminal, Sun, Table2 } from "lucide-react";
import type { ReactNode } from "react";
import { ago } from "../lib/format";
import { groupByEnvironment } from "../lib/environments";
import { currentReport, openConnection, scan, setState, useStore, type View } from "../lib/store";
import { useTheme } from "../lib/theme";

function ConnectionList() {
  const connections = useStore((s) => s.connections);
  const active = useStore((s) => s.active);
  if (connections.length === 0) return null;
  return (
    <div className="side-connections">
      <div className="sidebar-section">Connections</div>
      {groupByEnvironment(connections).flatMap((g) =>
        g.items.map((c) => (
          <button
            key={c.name}
            className={`nav-item conn-item ${c.name === active ? "active" : ""}`}
            title={`${c.display} · ${c.environment}`}
            onClick={() => (c.name === active ? setState({ view: "overview" }) : openConnection(c.name))}
          >
            <span className={`env-dot env-${c.environment}`} />
            <span className="conn-name">{c.name}</span>
            {c.last_report && <span className="count">{c.last_report.opportunities}</span>}
          </button>
        )),
      )}
    </div>
  );
}

const NAV: { view: View; label: string; key: string; icon: ReactNode }[] = [
  { view: "overview", label: "Overview", key: "1", icon: <Gauge size={15} /> },
  { view: "queries", label: "Queries", key: "2", icon: <ListOrdered size={15} /> },
  { view: "live", label: "Live", key: "3", icon: <Activity size={15} /> },
  { view: "schema", label: "Schema", key: "4", icon: <Table2 size={15} /> },
  { view: "editor", label: "Editor", key: "5", icon: <SquareTerminal size={15} /> },
  { view: "plans", label: "Plans", key: "6", icon: <GitCompareArrows size={15} /> },
];

export function Sidebar() {
  const active = useStore((s) => s.active);
  const view = useStore((s) => s.view);
  const connection = useStore((s) => s.connections.find((c) => c.name === s.active));
  const report = useStore(currentReport);
  const scanning = useStore((s) => (s.report.status === "ready" && s.report.scanning) || s.report.status === "loading");
  const [theme, cycleTheme] = useTheme();

  return (
    <aside className="sidebar">
      <div className="sidebar-drag" data-tauri-drag-region />
      <button className="conn-switch" onClick={() => setState({ view: "connections" })}>
        {connection ? <span className={`env-dot env-${connection.environment}`} /> : <span className="env-dot" />}
        <span style={{ minWidth: 0 }}>
          <div className="name">{active ?? "No connection"}</div>
          <div className="sub">
            {report ? `${report.snapshot.server.product} · ${report.snapshot.server.database}` : connection ? connection.display : "Choose a database"}
          </div>
        </span>
        <ChevronsUpDown size={14} className="chev" />
      </button>
      {(
        <nav className="nav">
          {NAV.filter((n) => active || n.view === "plans").map((n) => (
            <button key={n.view} className={`nav-item ${view === n.view ? "active" : ""}`} onClick={() => setState({ view: n.view })}>
              {n.icon}
              {n.label}
              {n.view === "overview" && report && <span className="count">{report.summary.opportunities}</span>}
              <span className="key">{n.key}</span>
            </button>
          ))}
        </nav>
      )}
      <ConnectionList />
      <div className="sidebar-foot">
        {active && (
          <button className="scan-status" onClick={() => scan(active)} title="Re-scan (⌘R)">
            {scanning ? <span className="spinner" style={{ width: 12, height: 12 }} /> : <RefreshCw size={13} />}
            {scanning ? "Scanning…" : report ? `Scanned ${ago(report.snapshot.taken_at)}` : "Scan"}
          </button>
        )}
        <button className="scan-status" onClick={() => setState({ palette: true })}>
          <Command size={13} /> Command palette <span className="kbd" style={{ marginLeft: "auto" }}>⌘K</span>
        </button>
        <button className="scan-status" onClick={cycleTheme}>
          {theme === "dark" ? <Moon size={13} /> : <Sun size={13} />} Theme: {theme}
        </button>
      </div>
    </aside>
  );
}
