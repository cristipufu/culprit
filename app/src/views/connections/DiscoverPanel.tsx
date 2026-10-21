import { Search } from "lucide-react";
import { useEffect, useState } from "react";
import { EngineBadge } from "../../components/EngineBadge";
import type { Cloud as CloudKind } from "../../gen/Cloud";
import type { DiscoveredDatabase } from "../../gen/DiscoveredDatabase";
import type { ImportCandidate } from "../../gen/ImportCandidate";
import type { ImportSource } from "../../gen/ImportSource";
import { api, errorMessage, type ApiError } from "../../lib/api";
import { engineLabel } from "../../lib/engines";
import { loadConnections } from "../../lib/store";

const TERMS_KEY: Record<CloudKind, string> = { azure: "culprit.discover.terms", aws: "culprit.discover.aws.terms" };
const REGIONS_KEY = "culprit.discover.aws.regions";
/** Tick results for the person only when a search has narrowed the list down; never pre-tick a whole subscription. */
const PRESELECT_LIMIT = 10;

export function remembered(key: string): string {
  try {
    return localStorage.getItem(key) ?? "";
  } catch {
    return "";
  }
}

export function remember(key: string, value: string): void {
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

export function DiscoverPanel({ cloud, onDone }: { cloud: CloudKind; onDone: () => void }) {
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
export function ImportPanel({ onDone }: { onDone: () => void }) {
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
