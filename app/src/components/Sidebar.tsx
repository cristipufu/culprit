import { Activity, ChevronsUpDown, CircleDot, Gauge, GitCompareArrows, ListOrdered, Monitor, Moon, Plus, RefreshCw, Search, SquareTerminal, Sun, Table2 } from "lucide-react";
import type { ReactNode } from "react";
import { ago } from "../lib/format";
import { EngineBadge } from "./EngineBadge";
import { groupByEnvironment } from "../lib/environments";
import { currentReport, openConnection, scan, setState, useStore, type View } from "../lib/store";
import { useTheme } from "../lib/theme";
import { keys } from "../lib/keys";

function ConnectionList() {
  const connections = useStore((s) => s.connections);
  const active = useStore((s) => s.active);
  if (connections.length === 0) return null;
  return (
    <div className="side-connections">
      <div className="sidebar-section side-section-row">
        Connections
        <button className="side-add" onClick={() => setState({ view: "connections" })} aria-label="Add or manage connections" title="Add or manage connections">
          <Plus size={13} />
        </button>
      </div>
      {groupByEnvironment(connections).flatMap((g) =>
        g.items.map((c) => (
          <button
            key={c.name}
            className={`nav-item conn-item ${c.name === active ? "active" : ""}`}
            title={`${c.display} · ${c.environment}`}
            onClick={() => (c.name === active ? setState({ view: "dashboard" }) : openConnection(c.name))}
          >
            <EngineBadge engine={c.engine} environment={c.environment} product={c.last_report?.product} />
            <span className="conn-name">{c.name}</span>
            {c.last_report && <span className="count">{c.last_report.opportunities}</span>}
          </button>
        )),
      )}
    </div>
  );
}

const NAV: { view: View; label: string; key: string; icon: ReactNode }[] = [
  { view: "dashboard", label: "Dashboard", key: "1", icon: <Gauge size={15} /> },
  { view: "overview", label: "Findings", key: "2", icon: <CircleDot size={15} /> },
  { view: "queries", label: "Queries", key: "3", icon: <ListOrdered size={15} /> },
  { view: "live", label: "Live", key: "4", icon: <Activity size={15} /> },
  { view: "schema", label: "Schema", key: "5", icon: <Table2 size={15} /> },
  { view: "editor", label: "Editor", key: "6", icon: <SquareTerminal size={15} /> },
  { view: "plans", label: "Plans", key: "7", icon: <GitCompareArrows size={15} /> },
];

export function Sidebar() {
  const active = useStore((s) => s.active);
  const view = useStore((s) => s.view);
  const connection = useStore((s) => s.connections.find((c) => c.name === s.active));
  const report = useStore(currentReport);
  const scanning = useStore((s) => (s.report.status === "ready" && s.report.scanning) || s.report.status === "loading");
  const [theme, , setTheme] = useTheme();

  return (
    <aside className="sidebar">
      <div className="sidebar-drag" data-tauri-drag-region />
      <button
        className="conn-switch"
        onClick={() => setState({ view: "connections" })}
        title={report ? `${active} · ${report.snapshot.server.product} · ${report.snapshot.server.database}` : undefined}
      >
        {connection ? (
          <EngineBadge engine={connection.engine} environment={connection.environment} product={report?.snapshot.server.product ?? connection.last_report?.product} size="md" />
        ) : (
          <span className="env-dot" />
        )}
        <span style={{ minWidth: 0 }}>
          <div className="name">{active ?? "No connection"}</div>
          {/* Product and database a line each, so a long database name isn't cut off behind the product. */}
          {report ? (
            <>
              <div className="sub">{report.snapshot.server.product}</div>
              <div className="sub">{report.snapshot.server.database}</div>
            </>
          ) : (
            <div className="sub">{connection ? connection.display : "Choose a database"}</div>
          )}
        </span>
        <ChevronsUpDown size={14} className="chev" />
      </button>
      <button className="side-search" onClick={() => setState({ palette: true })} aria-label={keys("Search or jump to (⌘K)")}>
        <Search size={13} /> Search or jump to… <span className="kbd">{keys("⌘K")}</span>
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
          <button className="scan-status" onClick={() => scan(active)} title={keys("Re-scan (⌘R)")}>
            {scanning ? <span className="spinner" style={{ width: 12, height: 12 }} /> : <RefreshCw size={13} />}
            {scanning ? "Scanning…" : report ? `Scanned ${ago(report.snapshot.taken_at)}` : "Scan"}
          </button>
        )}
        <div className="theme-row">
          Theme
          <span className="theme-seg" role="radiogroup" aria-label="Theme">
            {(
              [
                ["system", <Monitor size={12} key="s" />, "Follow the system"],
                ["light", <Sun size={12} key="l" />, "Light"],
                ["dark", <Moon size={12} key="d" />, "Dark"],
              ] as const
            ).map(([value, icon, label]) => (
              <button key={value} role="radio" aria-checked={theme === value} aria-label={label} title={label} className={theme === value ? "on" : ""} onClick={() => setTheme(value)}>
                {icon}
              </button>
            ))}
          </span>
        </div>
      </div>
    </aside>
  );
}
