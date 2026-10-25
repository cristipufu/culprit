import { ArrowRight, Bot, Check, ChevronDown, Clock3, FileText, KeyRound, Pencil } from "lucide-react";
import { useEffect, useId, useRef, useState } from "react";
import { SshSetting } from "./SshSetting";
import { EngineBadge } from "../../components/EngineBadge";
import type { AgentAccess } from "../../gen/AgentAccess";
import type { AuthKind } from "../../gen/AuthKind";
import type { ConnectionInfo } from "../../gen/ConnectionInfo";
import { api, errorMessage, isDesktop } from "../../lib/api";
import { connectFacts, serverName } from "../../lib/engines";
import { ago } from "../../lib/format";
import { loadConnections, openConnection, renameConnection } from "../../lib/store";

const AUTH_LABEL: Partial<Record<AuthKind, string>> = { entra: "Entra ID", "aws-iam": "AWS IAM" };

export function ConnectionCard({ connection: c }: { connection: ConnectionInfo }) {
  const [editing, setEditing] = useState(false);
  const [alias, setAlias] = useState(c.name);
  const [error, setError] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  const submitted = useRef(false);

  const nameId = useId();
  const product = serverName(c.engine, c.last_report?.product);
  const file = connectFacts(c.engine).file;
  const database = c.database ?? (file ? c.host.split(/[\\/]/).pop() : null);
  const host = c.host.includes(":") && !c.host.startsWith("[") ? `[${c.host}]` : c.host;
  const endpoint = file ? c.display : `${host}${c.port ? `:${c.port}` : ""}`;
  const headline = c.last_report
    ? c.last_report.no_workload ? `No query history: ${c.last_report.no_workload}` : (takeaway(c.last_report.headline) ?? NO_TAKEAWAY)
    : null;

  const save = async () => {
    if (submitted.current) return;
    submitted.current = true;
    if (alias === c.name) return setEditing(false);
    setSaving(true);
    try {
      await renameConnection(c.name, alias);
      setEditing(false);
      setError(null);
    } catch (e) {
      submitted.current = false;
      setError(errorMessage(e).message);
    } finally {
      setSaving(false);
    }
  };

  if (editing) {
    return (
      <article className="conn-card conn-card-editing">
        <EngineBadge engine={c.engine} environment={c.environment} product={c.last_report?.product} size="md" />
        <div className="conn-edit-name">
          <label htmlFor={nameId}>Connection name</label>
          <input id={nameId} className="alias-input" autoFocus value={alias} readOnly={saving}
            aria-label={`Rename ${c.name}`} aria-busy={saving}
            onChange={(event) => setAlias(event.target.value)}
            onKeyDown={(event) => {
              if (event.key === "Enter") { event.preventDefault(); void save(); }
              if (event.key === "Escape" && !submitted.current) {
                event.preventDefault(); submitted.current = true; setEditing(false);
              }
            }} onBlur={save} />
          <span className="muted">{saving ? "Saving…" : "Enter to save · Escape to cancel"}</span>
          {error && <span className="error" role="alert">{error}</span>}
        </div>
      </article>
    );
  }

  return (
    <article className="conn-card">
      <button type="button" className="conn-open" onClick={() => openConnection(c.name)}>
        <EngineBadge engine={c.engine} environment={c.environment} product={c.last_report?.product} size="md" />
        <span className="conn-identity">
          <strong>{c.name}</strong>
          <span className="conn-meta">{product}{database && <> · {database}</>}</span>
        </span>
        <span className="conn-count">
          {c.last_report ? <><b>{c.last_report.opportunities}</b><span>{c.last_report.opportunities === 1 ? "opportunity" : "opportunities"}</span></>
            : <span className="conn-scan">First scan <ArrowRight size={12} /></span>}
        </span>
        <span className="conn-endpoint" title={c.display}>
          <span>{file ? "File" : "Host"}</span><span>{endpoint}</span>
          {AUTH_LABEL[c.auth] && <span className="conn-auth">{AUTH_LABEL[c.auth]}</span>}
        </span>
        {headline && <span className="conn-summary" title={headline}>{headline}</span>}
      </button>
      <div className="conn-card-footer">
        <span className={`conn-when ${c.last_report && stale(c.last_report.taken_at) ? "stale" : ""}`}
          title={c.last_report ? `${c.last_report.taken_at}${stale(c.last_report.taken_at) ? " · Open to refresh this saved report" : ""}` : undefined}>
          <Clock3 size={12} aria-hidden="true" />
          {c.last_report ? `Scanned ${ago(c.last_report.taken_at)}` : "No saved report"}
        </span>
        <span className="conn-tools">
          {c.auth === "password" && !c.has_secret && <SetPassword name={c.name} />}
          {(file || c.query_log) && <QueryLogSetting connection={c} />}
          {!file && <SshSetting connection={c} />}
          <AgentAccessMenu connection={c} />
          <button type="button" className="btn ghost conn-rename" title={`Rename ${c.name}`} aria-label={`Rename ${c.name}`}
            onClick={() => { submitted.current = false; setAlias(c.name); setError(null); setEditing(true); }}>
            <Pencil size={12} />
          </button>
        </span>
      </div>
    </article>
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
  if (isDesktop() && open) {
    return (
      <span className="conn-settings-editor" onClick={(e) => e.stopPropagation()}>
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
        className="btn ghost conn-log"
        aria-label={c.query_log ? `Query log for ${c.name}: ${file}` : `Add query log for ${c.name}`}
        title={c.query_log ? `Scans read the statements from ${c.query_log}` : "Read the statements and their times from the log your framework writes (Rails, Django, EF Core, Phoenix)"}
        onClick={(e) => {
          e.stopPropagation();
          if (isDesktop() && !c.query_log) void choose();
          else { setPath(c.query_log ?? ""); setOpen(true); }
        }}
      >
        <FileText size={12} />
      </button>
    );
  }
  return (
    <span className="conn-settings-editor" onClick={(e) => e.stopPropagation()}>
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
        className="btn ghost conn-key" title="Set password" aria-label={`Set password for ${name}`}
        onClick={(e) => {
          e.stopPropagation();
          setOpen(true);
        }}
      >
        <KeyRound size={12} />
      </button>
    );
  }
  return (
    <span className="conn-settings-editor" onClick={(e) => e.stopPropagation()}>
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
