import { ArrowRight, Bot, Check, ChevronDown, FileText, KeyRound, Pencil } from "lucide-react";
import { useEffect, useId, useRef, useState } from "react";
import { EngineBadge } from "../../components/EngineBadge";
import type { AgentAccess } from "../../gen/AgentAccess";
import type { AuthKind } from "../../gen/AuthKind";
import type { ConnectionInfo } from "../../gen/ConnectionInfo";
import { api, errorMessage, isDesktop } from "../../lib/api";
import { serverName } from "../../lib/engines";
import { ago } from "../../lib/format";
import { loadConnections, openConnection } from "../../lib/store";

const AUTH_LABEL: Partial<Record<AuthKind, string>> = { entra: "Entra ID", "aws-iam": "AWS IAM" };

export function ConnectionCard({ connection: c }: { connection: ConnectionInfo }) {
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
