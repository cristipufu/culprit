import { ArrowRight, Cloud, Database, Import, KeyRound, Pencil, Plus, Search } from "lucide-react";
import { useEffect, useState } from "react";
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
import { api, errorMessage, type ApiError } from "../lib/api";
import { ENVIRONMENT_LABEL, groupByEnvironment } from "../lib/environments";
import { ago } from "../lib/format";
import { loadConnections, openConnection, useStore } from "../lib/store";

const ENVIRONMENTS: Environment[] = ["production", "staging", "development", "local"];

const ENGINE_LABEL: Record<Engine, string> = { mssql: "SQL Server", postgres: "Postgres", mysql: "MySQL" };

const AUTH_LABEL: Partial<Record<AuthKind, string>> = { entra: "Entra ID", "aws-iam": "AWS IAM" };

type TestState = { status: "idle" } | { status: "busy" } | { status: "ok"; label: string } | { status: "error"; error: ApiError };

export function Connections() {
  const connections = useStore((s) => s.connections);
  const [panel, setPanel] = useState<"none" | "new" | "import" | CloudKind>("none");
  const active = connections.length === 0 && panel === "none" ? "new" : panel;

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
        {groupByEnvironment(connections).map((group) => (
          <section key={group.environment} style={{ marginBottom: 18 }}>
            <div className="eyebrow" style={{ marginBottom: 8 }}>
              {ENVIRONMENT_LABEL[group.environment]}
            </div>
            {group.items.map((c) => (
              <ConnectionCard key={c.name} connection={c} />
            ))}
          </section>
        ))}
        <div style={{ display: "flex", gap: 8, marginTop: 6 }}>
          <button className={`btn ${active === "new" ? "primary" : ""}`} onClick={() => setPanel(active === "new" ? "none" : "new")}>
            <Plus size={14} /> New connection
          </button>
          <button className={`btn ${active === "azure" ? "primary" : ""}`} onClick={() => setPanel(active === "azure" ? "none" : "azure")}>
            <Cloud size={14} /> Find Azure databases
          </button>
          <button className={`btn ${active === "aws" ? "primary" : ""}`} onClick={() => setPanel(active === "aws" ? "none" : "aws")}>
            <Cloud size={14} /> Find AWS databases
          </button>
          <button className={`btn ${active === "import" ? "primary" : ""}`} onClick={() => setPanel(active === "import" ? "none" : "import")}>
            <Import size={14} /> Import from other tools
          </button>
        </div>
        {active === "new" && <NewConnectionForm onDone={() => setPanel("none")} canCancel={connections.length > 0} />}
        {(active === "azure" || active === "aws") && <DiscoverPanel key={active} cloud={active} onDone={() => setPanel("none")} />}
        {active === "import" && <ImportPanel onDone={() => setPanel("none")} />}
      </div>
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
        <span className={`env-dot env-${c.environment}`} />
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
        <span className="sub">{error ?? `${ENGINE_LABEL[c.engine]} · ${c.display}`}</span>
      </div>
    );
  }

  return (
    <div className="conn-card" role="button" tabIndex={0} onClick={() => openConnection(c.name)} onKeyDown={(e) => e.key === "Enter" && openConnection(c.name)}>
      <span className={`env-dot env-${c.environment}`} />
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
            <b>{c.last_report.opportunities}</b> opportunities
            <div className="muted">{ago(c.last_report.taken_at)}</div>
          </>
        ) : (
          <span className="muted">
            Scan <ArrowRight size={12} />
          </span>
        )}
      </span>
      <span className="sub">
        {ENGINE_LABEL[c.engine]} · {AUTH_LABEL[c.auth] ? `${AUTH_LABEL[c.auth]} · ` : ""}
        {c.display}
        {c.auth === "password" && !c.has_secret && <SetPassword name={c.name} />}
        <AgentAccessPicker connection={c} />
      </span>
    </div>
  );
}

const AGENT_ACCESS: { value: AgentAccess; label: string; title: string }[] = [
  { value: "off", label: "Agents: off", title: "Coding agents using `culprit mcp` don't see this connection" },
  { value: "stats", label: "Agents: stats & plans", title: "Agents read findings, query statistics, schema and estimated plans. Nothing runs your SQL." },
  { value: "query", label: "Agents: approved queries", title: "Agents may also run read-only statements, each one only after you approve it here" },
];

/** What coding agents may do with this connection through `culprit mcp`. */
function AgentAccessPicker({ connection: c }: { connection: ConnectionInfo }) {
  const [error, setError] = useState<string | null>(null);
  const title = AGENT_ACCESS.find((a) => a.value === c.agent_access)?.title;
  return (
    <>
      <select
        className={`agent-select access-${c.agent_access}`}
        value={c.agent_access}
        title={title}
        aria-label="What coding agents may do with this connection"
        onClick={(e) => e.stopPropagation()}
        onKeyDown={(e) => e.stopPropagation()}
        onChange={async (e) => {
          try {
            await (await api()).setAgentAccess(c.name, e.target.value as AgentAccess);
            await loadConnections();
            setError(null);
          } catch (err) {
            setError(errorMessage(err).message);
          }
        }}
      >
        {AGENT_ACCESS.map((a) => (
          <option key={a.value} value={a.value} title={a.title}>
            {a.label}
          </option>
        ))}
      </select>
      {error && <span className="muted"> {error}</span>}
    </>
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
                <span className={`env-dot env-${d.environment}`} />
                {d.saved_as ? (
                  <span className="alias">{d.saved_as}</span>
                ) : (
                  <input className="alias-input" value={aliases[key] ?? d.alias} onChange={(e) => setAliases((a) => ({ ...a, [key]: e.target.value }))} />
                )}
                <span className="where muted" title={d.host}>
                  {d.cloud === "aws"
                    ? `${d.server} · ${ENGINE_LABEL[d.engine]} · ${d.location} · ${d.auth === "aws-iam" ? "IAM" : "password"}`
                    : `${d.server}${d.database ? ` / ${d.database}` : ""} · ${ENGINE_LABEL[d.engine]} · ${d.location}${d.auth === "password" ? " · password" : ""}`}
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
                  <span className={`env-dot env-${c.environment}`} />
                  {c.saved_as ? (
                    <span className="alias">{c.saved_as}</span>
                  ) : (
                    <input className="alias-input" value={aliases[key] ?? c.alias} onChange={(e) => setAliases((a) => ({ ...a, [key]: e.target.value }))} />
                  )}
                  <span className="where muted" title={`${c.origin} in ${SOURCE_LABEL[c.source]} · ${c.file}`}>
                    {ENGINE_LABEL[c.engine]} · {target(c)}
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

function NewConnectionForm({ onDone, canCancel }: { onDone: () => void; canCancel: boolean }) {
  const [input, setInput] = useState("");
  const [parsed, setParsed] = useState<ParsedConnection | null>(null);
  const [parseError, setParseError] = useState<string | null>(null);
  const [name, setName] = useState("");
  const [nameTouched, setNameTouched] = useState(false);
  const [environment, setEnvironment] = useState<Environment>("development");
  const [password, setPassword] = useState("");
  const [test, setTest] = useState<TestState>({ status: "idle" });

  useEffect(() => {
    if (!input.trim()) {
      setParsed(null);
      setParseError(null);
      return;
    }
    const handle = setTimeout(async () => {
      try {
        const p = await (await api()).parseConnection(input);
        setParsed(p);
        setParseError(null);
        if (!nameTouched) setName(p.suggested_name);
        if (/prod/i.test(input)) setEnvironment("production");
        else if (/alpha|staging|stg/i.test(input)) setEnvironment("staging");
        else if (/localhost|127\.0\.0\.1/i.test(input)) setEnvironment("local");
      } catch (e) {
        setParsed(null);
        setParseError(errorMessage(e).message);
      }
    }, 150);
    return () => clearTimeout(handle);
  }, [input, nameTouched]);

  const request = () => ({
    name,
    input,
    environment,
    engine: parsed?.engine ?? null,
    database: null,
    user: null,
    password: password || null,
    password_env: null,
    auth: null,
  });

  const needsPassword = parsed && parsed.auth === "password" && !parsed.has_password;

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
      await loadConnections();
      onDone();
      openConnection(saved.name);
    } catch (e) {
      setTest({ status: "error", error: errorMessage(e) });
    }
  };

  return (
    <div style={{ marginTop: 18 }}>
      <div className="eyebrow" style={{ marginBottom: 8 }}>
        New connection
      </div>
      <label className="smart-input">
        <Database size={16} color="var(--text-3)" />
        <input
          autoFocus
          value={input}
          spellCheck={false}
          placeholder="postgres://…  ·  Server=…;Database=…  ·  host:port/db"
          onChange={(e) => setInput(e.target.value)}
        />
      </label>
      {parseError && <div className="muted" style={{ marginTop: 6 }}>{parseError}</div>}
      {parsed && (
        <div className="form-grid">
          <label>Detected</label>
          <div>
            <b>{parsed.engine ? ENGINE_LABEL[parsed.engine] : "Unknown engine"}</b>
            <span className="muted"> · {parsed.display}{parsed.auth === "entra" ? " · Microsoft Entra ID (Azure CLI)" : ""}</span>
          </div>
          <label>Name</label>
          <input
            value={name}
            onChange={(e) => {
              setName(e.target.value);
              setNameTouched(true);
            }}
          />
          <label>Environment</label>
          <div className="seg">
            {ENVIRONMENTS.map((env) => (
              <button key={env} className={environment === env ? "on" : ""} onClick={() => setEnvironment(env)}>
                <span className={`env-dot env-${env}`} style={{ display: "inline-block", marginRight: 6, width: 6, height: 6, boxShadow: "none" }} />
                {env}
              </button>
            ))}
          </div>
          {needsPassword && (
            <>
              <label>Password</label>
              <input type="password" value={password} onChange={(e) => setPassword(e.target.value)} placeholder="Stored in the OS keychain" />
            </>
          )}
          <span />
          <div style={{ display: "flex", gap: 8, alignItems: "center", flexWrap: "wrap" }}>
            <button className="btn primary" disabled={!name || test.status === "busy"} onClick={onSave}>
              Connect and scan
            </button>
            <button className="btn" disabled={test.status === "busy"} onClick={onTest}>
              Test
            </button>
            {canCancel && (
              <button className="btn ghost" onClick={onDone}>
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
    </div>
  );
}
