import { ArrowRight, Bot, Check, ChevronDown, Cloud, Database, FileText, FolderOpen, Import, KeyRound, LockKeyhole, Pencil, Plus, Search, ShieldCheck } from "lucide-react";
import { useEffect, useId, useRef, useState, type ReactNode } from "react";
import { Logo } from "../components/Logo";
import type { ConnectionInfo } from "../gen/ConnectionInfo";
import type { AgentAccess } from "../gen/AgentAccess";
import type { AuthKind } from "../gen/AuthKind";
import type { Cloud as CloudKind } from "../gen/Cloud";
import type { DiscoveredDatabase } from "../gen/DiscoveredDatabase";
import type { Engine } from "../gen/Engine";
import type { Environment } from "../gen/Environment";
import type { ImportCandidate } from "../gen/ImportCandidate";
import type { ImportSource } from "../gen/ImportSource";
import type { ParsedConnection } from "../gen/ParsedConnection";
import { api, errorMessage, isDesktop, type ApiError } from "../lib/api";
import { ENVIRONMENT_LABEL, groupByEnvironment } from "../lib/environments";
import { ago } from "../lib/format";
import { loadConnections, openConnection, useStore } from "../lib/store";
import { EngineBadge } from "../components/EngineBadge";
import { connectFacts, ENGINES, engineLabel, serverName } from "../lib/engines";

const ENVIRONMENTS: Environment[] = ["production", "staging", "development", "local"];


const AUTH_LABEL: Partial<Record<AuthKind, string>> = { entra: "Entra ID", "aws-iam": "AWS IAM" };

type TestState = { status: "idle" } | { status: "busy" } | { status: "ok"; label: string } | { status: "error"; error: ApiError };

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

function ConnectionCard({ connection: c }: { connection: ConnectionInfo }) {
  const [editing, setEditing] = useState(false);
  const [alias, setAlias] = useState(c.name);
  const [error, setError] = useState<string | null>(null);

  const save = async () => {
    if (alias === c.name) return setEditing(false);
    try {
      await (await api()).renameConnection(c.name, alias);
      await loadConnections();
      setEditing(false);
      setError(null);
    } catch (e) {
      setError(errorMessage(e).message);
    }
  };

  if (editing) {
    return (
      <div className="conn-card">
        <EngineBadge engine={c.engine} environment={c.environment} product={c.last_report?.product} size="md" />
        <input
          className="alias-input"
          autoFocus
          value={alias}
          onChange={(e) => setAlias(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter") save();
            if (e.key === "Escape") setEditing(false);
          }}
          onBlur={save}
        />
        <span className="last muted">Enter to save</span>
        <span className="sub">{error ?? `${serverName(c.engine, c.last_report?.product)} · ${c.display}`}</span>
      </div>
    );
  }

  return (
    <div className="conn-card" role="button" tabIndex={0} onClick={() => openConnection(c.name)} onKeyDown={(e) => e.key === "Enter" && openConnection(c.name)}>
      <EngineBadge engine={c.engine} environment={c.environment} product={c.last_report?.product} size="md" />
      <span className="name">
        {c.name}
        <button
          className="rename"
          title="Rename"
          onClick={(e) => {
            e.stopPropagation();
            setEditing(true);
          }}
        >
          <Pencil size={12} />
        </button>
      </span>
      <span className="last">
        {c.last_report ? (
          <>
            <span className="conn-count">
              <b>{c.last_report.opportunities}</b> {c.last_report.opportunities === 1 ? "opportunity" : "opportunities"}
            </span>
            <span className={`conn-when ${stale(c.last_report.taken_at) ? "stale" : ""}`} title={stale(c.last_report.taken_at) ? "Scanned more than a day ago; open to scan again" : c.last_report.taken_at}>
              {stale(c.last_report.taken_at) ? `Stale · ${ago(c.last_report.taken_at)}` : ago(c.last_report.taken_at)}
            </span>
          </>
        ) : (
          <span className="conn-scan">
            First scan <ArrowRight size={12} />
          </span>
        )}
      </span>
      <span className="sub">
        {serverName(c.engine, c.last_report?.product)} · {AUTH_LABEL[c.auth] ? `${AUTH_LABEL[c.auth]} · ` : ""}
        {c.display}
      </span>
      {c.last_report && (
        <span className="conn-headline">
          {c.last_report.no_workload ? `No query history: ${c.last_report.no_workload}` : (takeaway(c.last_report.headline) ?? NO_TAKEAWAY)}
        </span>
      )}
      <span className="conn-tools">
        {c.auth === "password" && !c.has_secret && <SetPassword name={c.name} />}
        {(c.engine === "sqlite" || c.query_log) && <QueryLogSetting connection={c} />}
        <AgentAccessMenu connection={c} />
      </span>
    </div>
  );
}

/** A headline with only a count (no workload, or no finding above 5% of database time) still gets a line, so every
 * scanned card has the same shape. */
const NO_TAKEAWAY = "No finding is tied to a large share of database time.";

/** The report headline without its count ("14 opportunities. The top ones touch…"), which the card already shows. */
function takeaway(headline: string | undefined): string | null {
  const rest = headline?.replace(/^\s*\d[\d,]*\s+opportunit(?:y|ies)\.?\s*/i, "").trim();
  return rest ? rest : null;
}

/** A report older than a day may describe a database that has moved on. */
function stale(iso: string): boolean {
  const t = Date.parse(iso);
  return !Number.isNaN(t) && Date.now() - t > 86_400_000;
}

const AGENT_ACCESS: { value: AgentAccess; label: string; title: string }[] = [
  { value: "off", label: "Off", title: "Coding agents using culprit mcp don't see this connection." },
  { value: "stats", label: "Stats & plans", title: "Agents read findings, query statistics, schema and estimated plans. Nothing runs your SQL." },
  { value: "query", label: "Approved queries", title: "Agents may also run read-only statements, each one only after you approve it here." },
];

/** What coding agents may do with this connection through `culprit mcp`: a compact chip that opens a small menu. */
function AgentAccessMenu({ connection: c }: { connection: ConnectionInfo }) {
  const [open, setOpen] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const root = useRef<HTMLSpanElement>(null);
  const items = useRef<(HTMLButtonElement | null)[]>([]);
  const menuId = useId();
  const current = AGENT_ACCESS.find((a) => a.value === c.agent_access) ?? AGENT_ACCESS[0]!;

  useEffect(() => {
    if (!open) return;
    const onDown = (e: MouseEvent) => {
      if (!root.current?.contains(e.target as Node)) setOpen(false);
    };
    window.addEventListener("mousedown", onDown);
    requestAnimationFrame(() => items.current[AGENT_ACCESS.indexOf(current)]?.focus());
    return () => window.removeEventListener("mousedown", onDown);
  }, [open, current]);

  const choose = async (value: AgentAccess) => {
    setOpen(false);
    if (value === c.agent_access) return;
    try {
      await (await api()).setAgentAccess(c.name, value);
      await loadConnections();
      setError(null);
    } catch (err) {
      setError(errorMessage(err).message);
    }
  };

  const onKey = (e: React.KeyboardEvent) => {
    e.stopPropagation();
    if (!open) return;
    const at = items.current.indexOf(document.activeElement as HTMLButtonElement);
    if (e.key === "Escape") setOpen(false);
    else if (e.key === "ArrowDown" || e.key === "ArrowUp") {
      e.preventDefault();
      items.current[(at + (e.key === "ArrowDown" ? 1 : -1) + AGENT_ACCESS.length) % AGENT_ACCESS.length]?.focus();
    }
  };

  return (
    <span className="agent-access" ref={root} onClick={(e) => e.stopPropagation()} onKeyDown={onKey}>
      <button className={`agent-chip access-${c.agent_access}`} aria-haspopup="menu" aria-expanded={open} aria-controls={menuId} title={current.title} onClick={() => setOpen((o) => !o)}>
        <Bot size={12} /> Agents: {current.label}
        <ChevronDown size={11} />
      </button>
      {open && (
        <span className="agent-menu" role="menu" id={menuId}>
          <span className="agent-menu-head">What coding agents may do here</span>
          {AGENT_ACCESS.map((a, i) => (
            <button
              key={a.value}
              role="menuitemradio"
              aria-checked={a.value === c.agent_access}
              ref={(el) => {
                items.current[i] = el;
              }}
              className="agent-option"
              onClick={() => choose(a.value)}
            >
              <span className="agent-check">{a.value === c.agent_access && <Check size={13} />}</span>
              <span>
                <b>{a.label}</b>
                <span className="muted">{a.title}</span>
              </span>
            </button>
          ))}
        </span>
      )}
      {error && <span className="muted"> {error}</span>}
    </span>
  );
}

/** The log the application's framework writes (Rails, Django, EF Core, Phoenix), which scans read the statements from
 *  when the database keeps none (SQLite). Nothing changes in the application: Culprit reads the file it already writes. */
function QueryLogSetting({ connection: c }: { connection: ConnectionInfo }) {
  const [open, setOpen] = useState(false);
  const [path, setPath] = useState(c.query_log ?? "");
  const [error, setError] = useState<string | null>(null);
  const file = c.query_log?.split(/[\\/]/).pop();
  const save = async (value: string | null) => {
    try {
      await (await api()).setQueryLog(c.name, value);
      setOpen(false);
      setError(null);
      await loadConnections();
    } catch (e) {
      setError(errorMessage(e).message);
    }
  };
  // The desktop app picks the file in the system's own dialog; the browser demo can only take a typed path.
  const choose = async () => {
    try {
      const chosen = await (await api()).chooseQueryLog(c.name);
      setError(null);
      if (chosen) {
        setOpen(false);
        await loadConnections();
      }
    } catch (e) {
      setError(errorMessage(e).message);
    }
  };
  if (isDesktop() && !open && !c.query_log) {
    return (
      <span style={{ display: "inline-flex", gap: 6, marginLeft: 8, alignItems: "center" }} onClick={(e) => e.stopPropagation()}>
        <button className="btn ghost" style={{ height: 22 }} title="Read the statements and their times from the log your framework writes (Rails, Django, EF Core, Phoenix)" onClick={choose}>
          <FileText size={12} /> Add query log
        </button>
        {error && <span className="muted">{error}</span>}
      </span>
    );
  }
  if (isDesktop() && open) {
    return (
      <span style={{ display: "inline-flex", gap: 6, marginLeft: 8, alignItems: "center" }} onClick={(e) => e.stopPropagation()}>
        <span className="muted mono" title={c.query_log ?? ""}>
          {file}
        </span>
        <button className="btn" onClick={choose}>
          Choose another…
        </button>
        <button className="btn ghost" onClick={() => save(null)}>
          Remove
        </button>
        <button className="btn ghost" onClick={() => setOpen(false)}>
          Done
        </button>
        {error && <span className="muted">{error}</span>}
      </span>
    );
  }
  if (!open) {
    return (
      <button
        className="btn ghost"
        style={{ marginLeft: 8, height: 22 }}
        title={c.query_log ? `Scans read the statements from ${c.query_log}` : "Read the statements and their times from the log your framework writes (Rails, Django, EF Core, Phoenix)"}
        onClick={(e) => {
          e.stopPropagation();
          setPath(c.query_log ?? "");
          setOpen(true);
        }}
      >
        <FileText size={12} /> {file ? `Query log: ${file}` : "Add query log"}
      </button>
    );
  }
  return (
    <span style={{ display: "inline-flex", gap: 6, marginLeft: 8, alignItems: "center" }} onClick={(e) => e.stopPropagation()}>
      <input
        className="alias-input"
        autoFocus
        value={path}
        placeholder="/path/to/log/production.log"
        onChange={(e) => setPath(e.target.value)}
        onKeyDown={(e) => {
          e.stopPropagation();
          if (e.key === "Enter" && path.trim()) save(path.trim());
          if (e.key === "Escape") setOpen(false);
        }}
      />
      <button className="btn" disabled={!path.trim()} onClick={() => save(path.trim())}>
        Save
      </button>
      {c.query_log && (
        <button className="btn ghost" onClick={() => save(null)}>
          Remove
        </button>
      )}
      {error && <span className="muted">{error}</span>}
    </span>
  );
}

/** Discovered password-auth databases arrive without a secret; this stores one in the OS keychain. */
function SetPassword({ name }: { name: string }) {
  const [open, setOpen] = useState(false);
  const [password, setPassword] = useState("");
  const [error, setError] = useState<string | null>(null);
  const save = async () => {
    try {
      await (await api()).setPassword(name, password);
      setPassword("");
      setOpen(false);
      await loadConnections();
    } catch (e) {
      setError(errorMessage(e).message);
    }
  };
  if (!open) {
    return (
      <button
        className="btn ghost"
        style={{ marginLeft: 8, height: 22 }}
        onClick={(e) => {
          e.stopPropagation();
          setOpen(true);
        }}
      >
        <KeyRound size={12} /> Set password
      </button>
    );
  }
  return (
    <span style={{ display: "inline-flex", gap: 6, marginLeft: 8, alignItems: "center" }} onClick={(e) => e.stopPropagation()}>
      <input
        className="alias-input"
        type="password"
        autoFocus
        value={password}
        placeholder="Stored in the OS keychain"
        onChange={(e) => setPassword(e.target.value)}
        onKeyDown={(e) => {
          e.stopPropagation();
          if (e.key === "Enter") save();
          if (e.key === "Escape") setOpen(false);
        }}
      />
      <button className="btn" disabled={!password} onClick={save}>
        Save
      </button>
      {error && <span className="muted">{error}</span>}
    </span>
  );
}

const TERMS_KEY: Record<CloudKind, string> = { azure: "culprit.discover.terms", aws: "culprit.discover.aws.terms" };
const REGIONS_KEY = "culprit.discover.aws.regions";
/** Tick results for the person only when a search has narrowed the list down; never pre-tick a whole subscription. */
const PRESELECT_LIMIT = 10;

function remembered(key: string): string {
  try {
    return localStorage.getItem(key) ?? "";
  } catch {
    return "";
  }
}

function remember(key: string, value: string): void {
  try {
    localStorage.setItem(key, value);
  } catch {}
}

const words = (text: string) => text.split(/[\s,]+/).filter(Boolean);

const CLOUD_COPY: Record<CloudKind, { heading: string; login: string; busy: string; note: string }> = {
  azure: {
    heading: "Azure SQL, PostgreSQL and MySQL databases your",
    login: "az login",
    busy: "Asking Azure…",
    note: "Saved with Microsoft Entra ID through your Azure CLI login where the server allows it; the others ask for their password once (kept in the OS keychain).",
  },
  aws: {
    heading: "Amazon RDS and Aurora databases your",
    login: "aws login",
    busy: "Asking AWS…",
    note: "IAM-enabled databases sign in through your AWS CLI login; the others ask for their password once (kept in the OS keychain).",
  },
};

type DiscoverState = { status: "idle" } | { status: "busy" } | { status: "done"; items: DiscoveredDatabase[] } | { status: "error"; error: ApiError };

function DiscoverPanel({ cloud, onDone }: { cloud: CloudKind; onDone: () => void }) {
  const copy = CLOUD_COPY[cloud];
  const [terms, setTerms] = useState(() => remembered(TERMS_KEY[cloud]));
  const [regions, setRegions] = useState(() => (cloud === "aws" ? remembered(REGIONS_KEY) : ""));
  const [state, setDiscover] = useState<DiscoverState>({ status: "idle" });
  const [picked, setPicked] = useState<Record<string, boolean>>({});
  const [aliases, setAliases] = useState<Record<string, string>>({});
  const [adding, setAdding] = useState(false);

  const keyOf = (d: DiscoveredDatabase) => `${d.cloud}/${d.host}/${d.database}`;

  const search = async () => {
    remember(TERMS_KEY[cloud], terms);
    if (cloud === "aws") remember(REGIONS_KEY, regions);
    setDiscover({ status: "busy" });
    try {
      const client = await api();
      const items = cloud === "aws" ? await client.discoverAws(words(terms), words(regions)) : await client.discoverAzure(words(terms));
      setDiscover({ status: "done", items });
      const candidates = items.filter((d) => !d.saved_as && d.environment !== "production");
      const narrowedDown = terms.trim() !== "" && candidates.length <= PRESELECT_LIMIT;
      setPicked(narrowedDown ? Object.fromEntries(candidates.map((d) => [keyOf(d), true])) : {});
      setAliases(Object.fromEntries(items.map((d) => [keyOf(d), d.alias])));
    } catch (e) {
      setDiscover({ status: "error", error: errorMessage(e) });
    }
  };

  useEffect(() => {
    search();
  }, []);

  const selected = state.status === "done" ? state.items.filter((d) => picked[keyOf(d)] && !d.saved_as) : [];

  const add = async () => {
    setAdding(true);
    try {
      await (await api()).addDiscovered(selected.map((d) => ({ ...d, alias: aliases[keyOf(d)] ?? d.alias })));
      await loadConnections();
      onDone();
    } catch (e) {
      setDiscover({ status: "error", error: errorMessage(e) });
    } finally {
      setAdding(false);
    }
  };

  return (
    <div style={{ marginTop: 18 }}>
      <div className="eyebrow" style={{ marginBottom: 8 }}>
        {copy.heading} <span className="mono">{copy.login}</span> can see
      </div>
      <label className="smart-input">
        <Search size={16} color="var(--text-3)" />
        <input value={terms} placeholder="Words in the database or server name, e.g. shop stg" onChange={(e) => setTerms(e.target.value)} onKeyDown={(e) => e.key === "Enter" && search()} />
        {cloud === "aws" && (
          <input
            value={regions}
            style={{ flex: "0 0 190px", borderLeft: "1px solid var(--border)", paddingLeft: 10 }}
            placeholder="All enabled regions"
            title="Regions to search, e.g. us-east-1 eu-central-1"
            onChange={(e) => setRegions(e.target.value)}
            onKeyDown={(e) => e.key === "Enter" && search()}
          />
        )}
        <button className="btn" onClick={search}>
          Search
        </button>
      </label>
      {state.status === "busy" && (
        <div className="empty" style={{ padding: 24 }}>
          <div className="spinner" />
          {copy.busy}
        </div>
      )}
      {state.status === "error" && (
        <div className="notice error" style={{ marginTop: 12 }}>
          <div>
            <b>{state.error.message}</b>
            {state.error.hint && <div>{state.error.hint}</div>}
          </div>
        </div>
      )}
      {state.status === "done" && (
        <div className="discover-list">
          {state.items.length === 0 && <div className="muted" style={{ padding: 12 }}>No databases match.</div>}
          {state.items.map((d) => {
            const key = keyOf(d);
            return (
              <label key={key} className={`discover-row ${d.saved_as ? "saved" : ""}`}>
                <input type="checkbox" disabled={!!d.saved_as} checked={!!d.saved_as || !!picked[key]} onChange={(e) => setPicked((p) => ({ ...p, [key]: e.target.checked }))} />
                <EngineBadge engine={d.engine} environment={d.environment} />
                {d.saved_as ? (
                  <span className="alias">{d.saved_as}</span>
                ) : (
                  <input className="alias-input" value={aliases[key] ?? d.alias} onChange={(e) => setAliases((a) => ({ ...a, [key]: e.target.value }))} />
                )}
                <span className="where muted" title={d.host}>
                  {d.cloud === "aws"
                    ? `${d.server} · ${engineLabel(d.engine)} · ${d.location} · ${d.auth === "aws-iam" ? "IAM" : "password"}`
                    : `${d.server}${d.database ? ` / ${d.database}` : ""} · ${engineLabel(d.engine)} · ${d.location}${d.auth === "password" ? " · password" : ""}`}
                </span>
                <span className="muted">{d.saved_as ? "saved" : d.environment}</span>
              </label>
            );
          })}
          {state.items.length > 0 && (
            <div style={{ display: "flex", gap: 8, alignItems: "center", padding: "10px 4px 2px" }}>
              <button className="btn primary" disabled={selected.length === 0 || adding} onClick={add}>
                Add {selected.length} {selected.length === 1 ? "connection" : "connections"}
              </button>
              <span className="muted">{copy.note}</span>
            </div>
          )}
        </div>
      )}
    </div>
  );
}

const SOURCE_LABEL: Record<ImportSource, string> = { ads: "Azure Data Studio", vscode: "VS Code", dbeaver: "DBeaver", datagrip: "DataGrip" };
const SOURCES: ImportSource[] = ["ads", "vscode", "dbeaver", "datagrip"];
/** Personal tool lists are short; past this, nothing is pre-ticked. Production connections are never pre-ticked. */
const IMPORT_PRESELECT_LIMIT = 25;

type ImportState = { status: "busy" } | { status: "done"; items: ImportCandidate[] } | { status: "error"; error: ApiError };

function target(c: ImportCandidate): string {
  return `${c.host}${c.port ? `:${c.port}` : ""}${c.database ? `/${c.database}` : ""}`;
}

/** Connections saved in Azure Data Studio, VS Code, DBeaver and DataGrip on this computer. Metadata only; never passwords. */
function ImportPanel({ onDone }: { onDone: () => void }) {
  const [state, setImport] = useState<ImportState>({ status: "busy" });
  const [source, setSource] = useState<ImportSource | null>(null);
  const [picked, setPicked] = useState<Record<string, boolean>>({});
  const [aliases, setAliases] = useState<Record<string, string>>({});
  const [adding, setAdding] = useState(false);

  const keyOf = (c: ImportCandidate) => `${c.source}/${c.engine}/${target(c)}/${c.user ?? ""}`;

  useEffect(() => {
    (async () => {
      try {
        const items = await (await api()).importCandidates(null);
        setImport({ status: "done", items });
        const fresh = items.filter((c) => !c.saved_as && c.environment !== "production");
        setPicked(fresh.length <= IMPORT_PRESELECT_LIMIT ? Object.fromEntries(fresh.map((c) => [keyOf(c), true])) : {});
        setAliases(Object.fromEntries(items.map((c) => [keyOf(c), c.alias])));
      } catch (e) {
        setImport({ status: "error", error: errorMessage(e) });
      }
    })();
  }, []);

  const items = state.status === "done" ? state.items : [];
  const shown = items.filter((c) => source === null || c.source === source);
  const selected = items.filter((c) => picked[keyOf(c)] && !c.saved_as);
  const counts = Object.fromEntries(SOURCES.map((s) => [s, items.filter((c) => c.source === s).length]));

  const add = async () => {
    setAdding(true);
    try {
      await (await api()).importConnections(selected.map((c) => ({ ...c, alias: aliases[keyOf(c)] ?? c.alias })));
      await loadConnections();
      onDone();
    } catch (e) {
      setImport({ status: "error", error: errorMessage(e) });
    } finally {
      setAdding(false);
    }
  };

  return (
    <div style={{ marginTop: 18 }}>
      <div className="eyebrow" style={{ marginBottom: 8 }}>
        Connections saved in other tools on this computer
      </div>
      {state.status === "done" && items.length > 0 && (
        <div className="import-sources">
          <div className="seg">
            <button className={source === null ? "on" : ""} onClick={() => setSource(null)}>
              All {items.length}
            </button>
            {SOURCES.filter((s) => (counts[s] ?? 0) > 0).map((s) => (
              <button key={s} className={source === s ? "on" : ""} onClick={() => setSource(s)}>
                {SOURCE_LABEL[s]} {counts[s] ?? 0}
              </button>
            ))}
          </div>
        </div>
      )}
      {state.status === "busy" && (
        <div className="empty" style={{ padding: 24 }}>
          <div className="spinner" />
          Looking for Azure Data Studio, VS Code, DBeaver and DataGrip connections…
        </div>
      )}
      {state.status === "error" && (
        <div className="notice error" style={{ marginTop: 12 }}>
          <div>
            <b>{state.error.message}</b>
            {state.error.hint && <div>{state.error.hint}</div>}
          </div>
        </div>
      )}
      {state.status === "done" && items.length === 0 && (
        <div className="notice" style={{ marginTop: 12 }}>
          <div>
            No saved connections found in Azure Data Studio, VS Code (mssql), DBeaver or DataGrip on this computer. Point the CLI at a settings file
            with <span className="mono">culprit conn import --file …</span>
          </div>
        </div>
      )}
      {shown.length > 0 && (
        <div className="discover-list">
          {shown.map((c) => {
            const key = keyOf(c);
            return (
              <div key={key}>
                <label className={`discover-row ${c.saved_as ? "saved" : ""}`}>
                  <input type="checkbox" disabled={!!c.saved_as} checked={!!c.saved_as || !!picked[key]} onChange={(e) => setPicked((p) => ({ ...p, [key]: e.target.checked }))} />
                  <EngineBadge engine={c.engine} environment={c.environment} />
                  {c.saved_as ? (
                    <span className="alias">{c.saved_as}</span>
                  ) : (
                    <input className="alias-input" value={aliases[key] ?? c.alias} onChange={(e) => setAliases((a) => ({ ...a, [key]: e.target.value }))} />
                  )}
                  <span className="where muted" title={`${c.origin} in ${SOURCE_LABEL[c.source]} · ${c.file}`}>
                    {engineLabel(c.engine)} · {target(c)}
                    {c.auth === "entra" ? " · Entra ID" : c.user ? ` · ${c.user}` : ""}
                  </span>
                  <span className="muted">{c.saved_as ? "saved" : c.environment}</span>
                </label>
                {c.note && !c.saved_as && <div className="import-note">{c.note}</div>}
              </div>
            );
          })}
          <div style={{ display: "flex", gap: 8, alignItems: "center", padding: "10px 4px 2px" }}>
            <button className="btn primary" disabled={selected.length === 0 || adding} onClick={add}>
              Import {selected.length} {selected.length === 1 ? "connection" : "connections"}
            </button>
            <span className="muted">Names, hosts, databases and user names only. Passwords stay in the other tool; Culprit asks once and keeps them in the OS keychain.</span>
          </div>
        </div>
      )}
    </div>
  );
}

/** The engine picked last time, picked again on the next new connection. */
const ENGINE_KEY = "culprit.new.engine";

/** A guess from the text itself: a name that says production or staging, a local host, a file on this computer. */
function guessEnvironment(text: string, engine: Engine): Environment {
  if (/prod/i.test(text)) return "production";
  if (/alpha|staging|stg/i.test(text)) return "staging";
  if (/localhost|127\.0\.0\.1/i.test(text) || connectFacts(engine).file) return "local";
  return "development";
}

/**
 * The engine first, then what it needs: a connection string for a server, the file for SQLite. The pick settles what
 * the text leaves open (a bare host, an ADO.NET string); a text that names another engine (a postgres:// URL, a .db
 * path) switches the pick and says so. Before anything is picked, a pasted connection string picks its own engine.
 */
function NewConnectionForm({ onDone, canCancel }: { onDone: () => void; canCancel: boolean }) {
  const [engine, setEngine] = useState<Engine | null>(() => {
    const last = remembered(ENGINE_KEY) as Engine;
    return ENGINES.includes(last) ? last : null;
  });
  const [switched, setSwitched] = useState(false);
  const [input, setInput] = useState("");
  const [parsed, setParsed] = useState<ParsedConnection | null>(null);
  const [parseError, setParseError] = useState<string | null>(null);
  const [name, setName] = useState("");
  const [nameTouched, setNameTouched] = useState(false);
  const [environment, setEnvironment] = useState<Environment>("development");
  const [environmentTouched, setEnvironmentTouched] = useState(false);
  const [user, setUser] = useState("");
  const [password, setPassword] = useState("");
  const [test, setTest] = useState<TestState>({ status: "idle" });
  const field = useRef<HTMLInputElement>(null);
  const ids = useId();
  const facts = engine ? connectFacts(engine) : null;

  useEffect(() => {
    if (!input.trim()) {
      setParsed(null);
      setParseError(null);
      return;
    }
    let live = true;
    const handle = setTimeout(async () => {
      try {
        const p = await (await api()).parseConnection(input, engine);
        if (!live) return;
        // The text names another engine than the one picked (or none was): it's that engine's, read again as it.
        if (p.engine && p.engine !== engine) {
          setSwitched(engine !== null);
          setEngine(p.engine);
          return;
        }
        setParsed(p);
        setParseError(null);
        if (!nameTouched) setName(p.suggested_name);
        if (!environmentTouched && p.engine) setEnvironment(guessEnvironment(input, p.engine));
      } catch (e) {
        if (!live) return;
        setParsed(null);
        setParseError(errorMessage(e).message);
      }
    }, 150);
    return () => {
      live = false;
      clearTimeout(handle);
    };
  }, [input, engine, nameTouched, environmentTouched]);

  // Picking an engine puts the cursor where its connection string (or file) goes.
  useEffect(() => {
    if (engine) field.current?.focus();
  }, [engine]);

  const pick = (next: Engine) => {
    if (next === engine) return;
    // A path and a connection string are different things: switching between a file and a server starts over.
    if (engine && connectFacts(engine).file !== connectFacts(next).file) setInput("");
    setEngine(next);
    setSwitched(false);
    setTest({ status: "idle" });
  };

  const onPickerKey = (e: React.KeyboardEvent) => {
    const step = e.key === "ArrowRight" || e.key === "ArrowDown" ? 1 : e.key === "ArrowLeft" || e.key === "ArrowUp" ? -1 : 0;
    if (!step) return;
    e.preventDefault();
    const at = engine ? ENGINES.indexOf(engine) : step > 0 ? -1 : 0;
    const next = ENGINES[(at + step + ENGINES.length) % ENGINES.length];
    if (!next) return;
    pick(next);
    (e.currentTarget.querySelector(`[data-engine="${next}"]`) as HTMLElement | null)?.focus();
  };

  // Before an engine is picked, a pasted connection string picks its own.
  const onPaste = (e: React.ClipboardEvent) => {
    if (engine) return;
    const text = e.clipboardData.getData("text").trim();
    if (!text) return;
    e.preventDefault();
    setInput(text);
  };

  const onChoose = async () => {
    try {
      const chosen = await (await api()).chooseDatabaseFile();
      if (chosen) setInput(chosen);
    } catch (err) {
      setParseError(errorMessage(err).message);
    }
  };

  // What was read is the picked engine's: the details and the buttons follow.
  const ready = !!parsed && !!engine && parsed.engine === engine;
  const needsUser = ready && parsed.auth === "password" && !parsed.user;
  const needsPassword = ready && parsed.auth === "password" && !parsed.has_password;

  const request = () => ({
    name,
    input,
    environment,
    engine,
    database: null,
    user: needsUser && user ? user : null,
    password: needsPassword && password ? password : null,
    password_env: null,
    auth: null,
  });

  const onTest = async () => {
    setTest({ status: "busy" });
    try {
      const server = await (await api()).testConnection(request());
      setTest({ status: "ok", label: `${server.product} · ${server.database}` });
    } catch (e) {
      setTest({ status: "error", error: errorMessage(e) });
    }
  };

  const onSave = async () => {
    setTest({ status: "busy" });
    try {
      const client = await api();
      await client.testConnection(request());
      const saved = await client.addConnection(request());
      if (engine) remember(ENGINE_KEY, engine);
      await loadConnections();
      onDone();
      openConnection(saved.name);
    } catch (e) {
      setTest({ status: "error", error: errorMessage(e) });
    }
  };

  return (
    <form
      className="new-conn"
      onPaste={onPaste}
      onSubmit={(e) => {
        e.preventDefault();
        if (ready && name && test.status !== "busy") onSave();
      }}
      onKeyDown={(e) => {
        if (e.key === "Escape" && canCancel) onDone();
      }}
    >
      <div className="eyebrow">New connection</div>
      <div className="engine-picker" role="radiogroup" aria-label="Database engine" onKeyDown={onPickerKey}>
        {ENGINES.map((e) => (
          <button
            key={e}
            type="button"
            role="radio"
            data-engine={e}
            aria-checked={engine === e}
            tabIndex={(engine ?? ENGINES[0]) === e ? 0 : -1}
            autoFocus={!engine && e === ENGINES[0]}
            className={`engine-choice ${engine === e ? "on" : ""}`}
            onClick={() => pick(e)}
          >
            <EngineBadge engine={e} size="md" />
            <span>{connectFacts(e).choice}</span>
          </button>
        ))}
      </div>

      {!engine && (
        <p className="new-conn-help">
          {input.trim() && !parseError
            ? `Which engine is ${input.trim().length > 60 ? `${input.trim().slice(0, 60)}…` : input.trim()}? Pick it above.`
            : parseError ?? "Pick the engine, or paste a connection string and Culprit picks it."}
        </p>
      )}

      {engine && facts && (
        <div className="new-conn-field">
          <label htmlFor={`${ids}-input`}>{facts.file ? "Database file" : "Connection string"}</label>
          <div className="smart-input">
            <Database size={16} color="var(--text-3)" aria-hidden />
            <input
              ref={field}
              id={`${ids}-input`}
              value={input}
              spellCheck={false}
              autoComplete="off"
              autoCapitalize="off"
              placeholder={facts.example}
              aria-describedby={`${ids}-help`}
              onChange={(e) => {
                setInput(e.target.value);
                setSwitched(false);
              }}
            />
            {/* A file is chosen in the system's own dialog rather than typed. */}
            {facts.file && (
              <button type="button" className={`btn ${input.trim() ? "" : "primary"}`} onClick={onChoose}>
                <FolderOpen size={13} /> Choose file…
              </button>
            )}
          </div>
          <p id={`${ids}-help`} className={`new-conn-help ${parseError ? "error" : ""}`}>
            {parseError ? (
              parseError
            ) : ready && !facts.file ? (
              <>
                <Check size={12} className="read" aria-hidden />
                {switched && <b>Switched to {facts.choice}, which the text names. </b>}
                <span className="mono">{parsed.display}</span>
                {parsed.auth === "entra" && " · Microsoft Entra ID through your az login, no password kept"}
              </>
            ) : (
              <>
                {switched && <b>Switched to {facts.choice}, which the text names. </b>}
                {facts.accepts}
              </>
            )}
          </p>
        </div>
      )}

      {ready && (
        <div className="form-grid">
          <label htmlFor={`${ids}-name`}>Name</label>
          <input
            id={`${ids}-name`}
            value={name}
            spellCheck={false}
            onChange={(e) => {
              setName(e.target.value);
              setNameTouched(true);
            }}
          />
          <label>Environment</label>
          <div className="seg" role="radiogroup" aria-label="Environment">
            {ENVIRONMENTS.map((env) => (
              <button
                key={env}
                type="button"
                role="radio"
                aria-checked={environment === env}
                className={environment === env ? "on" : ""}
                onClick={() => {
                  setEnvironment(env);
                  setEnvironmentTouched(true);
                }}
              >
                <span className={`env-dot env-${env}`} style={{ display: "inline-block", marginRight: 6, width: 6, height: 6, boxShadow: "none" }} />
                {env}
              </button>
            ))}
          </div>
          {needsUser && (
            <>
              <label htmlFor={`${ids}-user`}>User</label>
              <input id={`${ids}-user`} value={user} spellCheck={false} autoComplete="off" onChange={(e) => setUser(e.target.value)} placeholder="Not in the connection string" />
            </>
          )}
          {needsPassword && (
            <>
              <label htmlFor={`${ids}-password`}>Password</label>
              <input id={`${ids}-password`} type="password" value={password} onChange={(e) => setPassword(e.target.value)} placeholder="Kept in the OS keychain" />
            </>
          )}
          <span />
          <div className="new-conn-actions">
            <button type="submit" className="btn primary" disabled={!name || test.status === "busy"}>
              Connect and scan
            </button>
            <button type="button" className="btn" disabled={test.status === "busy"} onClick={onTest}>
              Test
            </button>
            {canCancel && (
              <button type="button" className="btn ghost" onClick={onDone}>
                Cancel
              </button>
            )}
            {test.status === "busy" && <span className="spinner" />}
            {test.status === "ok" && <span className="tag good">✓ {test.label}</span>}
          </div>
          {test.status === "error" && (
            <>
              <span />
              <div className="notice error">
                <div>
                  <b>{test.error.message}</b>
                  {test.error.hint && <div>{test.error.hint}</div>}
                </div>
              </div>
            </>
          )}
          {environment === "production" && (
            <>
              <span />
              <div className="notice">Production connections are read-only in Culprit: scans read statistics views, plans are estimated, and anything that executes a statement asks first.</div>
            </>
          )}
        </div>
      )}

    </form>
  );
}
