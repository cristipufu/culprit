import { Bot, Cloud, Database, Import, LockKeyhole, Plus, Search, ShieldCheck } from "lucide-react";
import { useState, type ReactNode } from "react";
import { Logo } from "../components/Logo";
import type { Cloud as CloudKind } from "../gen/Cloud";
import { serverName } from "../lib/engines";
import { ENVIRONMENT_LABEL, groupByEnvironment } from "../lib/environments";
import { useStore } from "../lib/store";
import { ConnectionCard } from "./connections/ConnectionCard";
import { DiscoverPanel, ImportPanel } from "./connections/DiscoverPanel";
import { NewConnectionForm } from "./connections/NewConnectionForm";

export function Connections() {
  const connections = useStore((s) => s.connections);
  const [panel, setPanel] = useState<Panel>("none");
  const active = panel;
  const firstRun = connections.length === 0;
  const [filter, setFilter] = useState("");
  const q = filter.trim().toLowerCase();
  const shown = q ? connections.filter((c) => `${c.name} ${c.display} ${serverName(c.engine, c.last_report?.product)} ${c.environment}`.toLowerCase().includes(q)) : connections;

  return (
    <div className="connections">
      <div className="connections-inner">
        <div className="brand" data-tauri-drag-region>
          <Logo size={36} />
          <div>
            <h1>Culprit</h1>
            <p>Connect a database and see what's slowing it down.</p>
          </div>
        </div>
        {firstRun && <FirstRun chosen={active} onChoose={(next) => setPanel(next === active ? "none" : next)} />}
        {/* Adding comes first: with many connections, the actions and the form they open stay in view, above the list. */}
        {!firstRun && (
          <div className="conn-actions">
            <button className={`btn ${active === "new" || connections.length === 0 ? "primary" : ""}`} onClick={() => setPanel(active === "new" ? "none" : "new")}>
              <Plus size={14} /> New connection
            </button>
            <button className={`btn ${active === "azure" ? "primary" : ""}`} onClick={() => setPanel(active === "azure" ? "none" : "azure")} title="Find Azure SQL, PostgreSQL and MySQL servers your az login can see">
              <Cloud size={14} /> From Azure
            </button>
            <button className={`btn ${active === "aws" ? "primary" : ""}`} onClick={() => setPanel(active === "aws" ? "none" : "aws")} title="Find RDS and Aurora databases your aws login can see">
              <Cloud size={14} /> From AWS
            </button>
            <button className={`btn ${active === "import" ? "primary" : ""}`} onClick={() => setPanel(active === "import" ? "none" : "import")} title="Import connections saved in Azure Data Studio, VS Code, DBeaver or DataGrip">
              <Import size={14} /> Import
            </button>
          </div>
        )}
        {active !== "none" && (
          <div className="conn-panel">
            {active === "new" && <NewConnectionForm onDone={() => setPanel("none")} canCancel />}
            {(active === "azure" || active === "aws") && <DiscoverPanel key={active} cloud={active} onDone={() => setPanel("none")} />}
            {active === "import" && <ImportPanel onDone={() => setPanel("none")} />}
          </div>
        )}
        {connections.length > 6 && (
          <label className="search conn-filter">
            <Search size={13} />
            <input value={filter} placeholder={`Filter ${connections.length} connections`} onChange={(e) => setFilter(e.target.value)} aria-label="Filter connections" />
          </label>
        )}
        {q && shown.length === 0 && <p className="muted">No connection matches “{filter}”.</p>}
        {groupByEnvironment(shown).map((group) => (
          <section key={group.environment} style={{ marginBottom: 18 }}>
            <div className="eyebrow" style={{ marginBottom: 8 }}>
              {ENVIRONMENT_LABEL[group.environment]}
            </div>
            {group.items.map((c) => (
              <ConnectionCard key={c.name} connection={c} />
            ))}
          </section>
        ))}
      </div>
    </div>
  );
}

type Panel = "none" | "new" | "import" | CloudKind;

const FIRST_RUN: { panel: Exclude<Panel, "none">; icon: ReactNode; title: string; text: string }[] = [
  { panel: "new", icon: <Database size={18} />, title: "Connect a database", text: "Pick SQL Server, PostgreSQL, MySQL or MariaDB and paste its connection string, or pick SQLite and choose the file." },
  { panel: "azure", icon: <Cloud size={18} />, title: "Find in Azure", text: "Lists the SQL, PostgreSQL and MySQL servers your az login can reach." },
  { panel: "aws", icon: <Cloud size={18} />, title: "Find in AWS", text: "Lists the RDS and Aurora databases your aws login can see." },
  { panel: "import", icon: <Import size={18} />, title: "Import from another tool", text: "Azure Data Studio, VS Code, DBeaver or DataGrip connections, without retyping." },
];

/** No connections yet: four ways in, and what Culprit promises before anyone types a password. */
function FirstRun({ chosen, onChoose }: { chosen: Panel; onChoose: (panel: Exclude<Panel, "none">) => void }) {
  return (
    <div className="first-run">
      <div className="first-tiles">
        {FIRST_RUN.map((t) => (
          <button key={t.panel} className={`first-tile ${chosen === t.panel ? "on" : ""}`} aria-pressed={chosen === t.panel} onClick={() => onChoose(t.panel)}>
            <span className="first-icon">{t.icon}</span>
            <b>{t.title}</b>
            <span>{t.text}</span>
          </button>
        ))}
      </div>
      <ul className="first-promises">
        <li>
          <ShieldCheck size={13} /> Reads statistics and catalog views only; never changes your database
        </li>
        <li>
          <LockKeyhole size={13} /> Passwords stay in your OS keychain
        </li>
        <li>
          <Bot size={13} /> Coding agents get the same answers through <code>culprit mcp</code>
        </li>
      </ul>
    </div>
  );
}
