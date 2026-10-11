import type { ApprovalRequest } from "../gen/ApprovalRequest";
import type { ConnectionInfo } from "../gen/ConnectionInfo";
import type { DiscoveredDatabase } from "../gen/DiscoveredDatabase";
import type { Drift } from "../gen/Drift";
import type { ImportCandidate } from "../gen/ImportCandidate";
import type { LiveState } from "../gen/LiveState";
import type { Session } from "../gen/Session";
import type { Plan } from "../gen/Plan";
import type { Report } from "../gen/Report";
import type { SavedQuery } from "../gen/SavedQuery";
import type { SaveQuery } from "../gen/SaveQuery";
import type { Api } from "./api";
import { demoRows } from "./demo-rows";

const reports: Record<string, () => Promise<Report>> = import.meta.env.DEV
  ? import.meta.glob<Report>("../../../fixtures/*.report.json", { import: "default" })
  : {};

const discoverSample: () => Promise<DiscoveredDatabase[]> = import.meta.env.DEV
  ? () => import("../../../fixtures/discover.sample.json").then((m) => m.default as DiscoveredDatabase[])
  : async () => [];

const driftSample: () => Promise<Drift | null> = import.meta.env.DEV
  ? () => import("../../../fixtures/drift.sample.json").then((m) => m.default as Drift)
  : async () => null;

/** The sample read from the other side: the same differences with "only in" flipped. */
function flipped(d: Drift): Drift {
  const presence = { "only-a": "only-b", "only-b": "only-a", different: "different", renamed: "renamed" } as const;
  return { ...d, a: d.b, b: d.a, items: d.items.map((i) => ({ ...i, presence: presence[i.presence], a: i.b, b: i.a })) };
}

const importSample: () => Promise<ImportCandidate[]> = import.meta.env.DEV
  ? () => import("../../../fixtures/import.sample.json").then((m) => m.default as ImportCandidate[])
  : async () => [];

/** A small blocking chain so Live has something to show (and end) in the browser demo. */
function demoSessions(engine: string): Session[] {
  const base = { user: "app", host: "web-1", database: "shop", command: null, cpu_ms: null, reads: null, transaction_ms: null };
  return [
    { ...base, id: "57", application: "orders-api", state: "idle in transaction", wait: null, wait_ms: null, blocked_by: null, elapsed_ms: 48_200, transaction_ms: 48_900, sql: "UPDATE shop.orders SET status = 'paid' WHERE id = 81723" },
    { ...base, id: "61", application: "orders-api", state: "active", command: engine === "mssql" ? "UPDATE" : null, wait: engine === "postgres" ? "Lock: transactionid" : "LCK_M_U", wait_ms: 31_400, blocked_by: "57", elapsed_ms: 31_400, sql: "UPDATE shop.orders SET shipped_at = now() WHERE id = 81723" },
    { ...base, id: "64", application: "reports", state: "active", wait: null, wait_ms: null, blocked_by: null, elapsed_ms: 12_900, cpu_ms: 11_800, reads: 1_840_000, sql: "SELECT customer_id, sum(total) FROM shop.orders GROUP BY customer_id ORDER BY 2 DESC" },
  ];
}

function nameOf(path: string): string {
  return path.split("/").pop()!.replace(".report.json", "");
}

async function load(name: string): Promise<Report | null> {
  const entry = Object.entries(reports).find(([path]) => nameOf(path) === name);
  return entry ? entry[1]() : null;
}

/** Saved queries in the browser demo live in this browser only. */
const demoSaved = (() => {
  const KEY = "culprit.demo.saved";
  const load = (): SavedQuery[] => {
    try {
      return JSON.parse(localStorage.getItem(KEY) ?? "[]");
    } catch {
      return [];
    }
  };
  const store = (list: SavedQuery[]) => {
    try {
      localStorage.setItem(KEY, JSON.stringify(list));
    } catch {}
  };
  const same = (a: string, b: string) => a.trim().toLowerCase() === b.trim().toLowerCase();
  return {
    list: () => load().sort((a, b) => (b.last_run_at ?? b.updated_at).localeCompare(a.last_run_at ?? a.updated_at)),
    put: (q: SaveQuery): SavedQuery => {
      const list = load();
      const name = q.name.trim();
      const target = q.previous_name ?? name;
      if (!name || !q.sql.trim()) throw { code: "invalid_input", message: "A saved query needs a name and SQL", hint: null };
      if (list.some((x) => same(x.name, name) && !same(x.name, target))) throw { code: "invalid_input", message: `A saved query named '${name}' already exists`, hint: null };
      const now = new Date().toISOString().slice(0, 19) + "Z";
      const existing = list.find((x) => same(x.name, target));
      const saved: SavedQuery = existing
        ? { ...existing, name, sql: q.sql, connection: q.connection, description: q.description, updated_at: now }
        : { name, sql: q.sql, connection: q.connection, description: q.description, created_at: now, updated_at: now, last_run_at: null };
      store([saved, ...list.filter((x) => !same(x.name, target))]);
      return saved;
    },
    remove: (name: string) => store(load().filter((x) => !same(x.name, name))),
    touch: (name: string) => store(load().map((x) => (same(x.name, name) ? { ...x, last_run_at: new Date().toISOString().slice(0, 19) + "Z" } : x))),
  };
})();

function unsupported(): Promise<never> {
  return Promise.reject({ code: "unsupported", message: "This is the browser demo; open the desktop app to connect to a database.", hint: null });
}

/** Demo backed by fixtures, for UI work and screenshots outside Tauri. Connection edits live in memory only. */
export async function demoApi(): Promise<Api> {
  const all = await Promise.all(Object.keys(reports).map((p) => load(nameOf(p))));
  let connections: ConnectionInfo[] = all
    .filter((r): r is Report => r !== null)
    .map((r) => ({
      name: r.snapshot.connection,
      engine: r.snapshot.server.engine,
      environment: r.snapshot.environment,
      host: r.snapshot.server.engine === "mssql" ? "localhost,1433" : "localhost:5432",
      port: null,
      database: r.snapshot.server.database,
      user: null,
      auth: "password",
      has_secret: true,
      display: r.snapshot.server.database,
      agent_access: "stats",
      last_report: {
        id: r.snapshot.id,
        taken_at: r.snapshot.taken_at,
        opportunities: r.summary.opportunities,
        headline: r.summary.headline,
        window_days: r.snapshot.options?.window_days ?? null,
      },
    }));
  let demoApproval: ApprovalRequest | null =
    import.meta.env.DEV && typeof location !== "undefined" && location.search.includes("demo-approval")
      ? {
          id: "0".repeat(32),
          connection: connections[0]?.name ?? "mssql-demo",
          environment: "staging",
          tool: "run_query",
          sql: "SELECT segment, COUNT(*) AS customers\nFROM shop.customers\nWHERE email LIKE '%@example.com'\nGROUP BY segment",
          hash: "",
          client: "claude-code",
          requested_at: new Date().toISOString(),
          expires_at: new Date(Date.now() + 60_000).toISOString(),
        }
      : null;
  const aliases = new Map<string, string>();
  const endedSessions = new Map<string, Set<string>>();
  const reportName = (name: string) => aliases.get(name) ?? name;
  const byConnection = (name: string) => all.find((r) => r?.snapshot.connection === reportName(name)) ?? null;
  const sampleFor = async (cloud: "azure" | "aws", terms: string[]) => {
    await new Promise((r) => setTimeout(r, 400));
    const sample = await discoverSample();
    return sample
      .filter((d) => (d.cloud ?? "azure") === cloud)
      .filter((d) => terms.every((t) => `${d.database} ${d.server}`.toLowerCase().includes(t.toLowerCase())))
      .map((d) => ({ ...d, saved_as: connections.find((c) => c.name === d.saved_as)?.name ?? null }));
  };
  return {
    kind: "demo",
    connections: async () => connections,
    parseConnection: unsupported,
    testConnection: unsupported,
    addConnection: unsupported,
    removeConnection: unsupported,
    renameConnection: async (old, next) => {
      if (!/^[A-Za-z0-9._-]{1,64}$/.test(next)) return Promise.reject({ code: "invalid_input", message: `'${next}' is not a valid connection name`, hint: null });
      if (connections.some((c) => c.name === next)) return Promise.reject({ code: "invalid_input", message: `A connection named '${next}' already exists`, hint: null });
      const current = connections.find((c) => c.name === old);
      if (!current) return Promise.reject({ code: "not_found", message: `No connection named '${old}'`, hint: null });
      aliases.set(next, reportName(old));
      const renamed = { ...current, name: next };
      connections = connections.map((c) => (c.name === old ? renamed : c));
      return renamed;
    },
    discoverAzure: async (terms) => sampleFor("azure", terms),
    discoverAws: async (terms, regions) =>
      (await sampleFor("aws", terms)).filter((d) => regions.length === 0 || regions.includes(d.region ?? "")),
    setPassword: async (name, password) => {
      const current = connections.find((c) => c.name === name);
      if (!current) return Promise.reject({ code: "not_found", message: `No connection named '${name}'`, hint: null });
      if (!password) return Promise.reject({ code: "invalid_input", message: "The password is empty", hint: null });
      const updated = { ...current, has_secret: true };
      connections = connections.map((c) => (c.name === name ? updated : c));
      return updated;
    },
    importCandidates: async (source) => {
      await new Promise((r) => setTimeout(r, 250));
      const sample = await importSample();
      return sample
        .filter((c) => source === null || c.source === source)
        .map((c) => ({ ...c, saved_as: connections.find((x) => x.host === c.host && x.database === c.database)?.name ?? null }));
    },
    importConnections: async (items) => {
      const added: ConnectionInfo[] = items.map((c) => ({
        name: c.alias,
        engine: c.engine,
        environment: c.environment,
        host: c.host,
        port: c.port,
        database: c.database,
        user: c.user,
        auth: c.auth,
        has_secret: false,
        display: `${c.user ? `${c.user}@` : ""}${c.host}${c.port ? `:${c.port}` : ""}${c.database ? `/${c.database}` : ""}`,
        last_report: null,
        agent_access: "stats",
      }));
      connections = [...connections, ...added];
      return added;
    },
    onSignIn: async (listener) => {
      // `?demo-sign-in` previews the Microsoft Entra ID device-code dialog.
      if (!location.search.includes("demo-sign-in")) return () => {};
      const timer = setTimeout(
        () =>
          listener({
            state: "code",
            prompt: {
              user_code: "C7XQ-4PLM",
              verification_uri: "https://microsoft.com/devicelogin",
              message: "To sign in, use a web browser to open the page https://microsoft.com/devicelogin and enter the code C7XQ-4PLM to authenticate.",
              expires_in_secs: 900,
            },
          }),
        400,
      );
      return () => clearTimeout(timer);
    },
    cancelSignIn: async () => {},
    openSignInPage: async () => {},
    addDiscovered: async (items) => {
      const added: ConnectionInfo[] = items.map((d) => {
        const port = d.port ?? 1433;
        const auth = d.cloud === "aws" ? d.auth : "entra";
        return {
          name: d.alias,
          engine: d.engine,
          environment: d.environment,
          host: d.host,
          port,
          database: d.database || null,
          user: d.user,
          auth,
          has_secret: false,
          display: `${d.user ? `${d.user}@` : ""}${d.host}:${port}${d.database ? `/${d.database}` : ""}`,
          agent_access: "stats",
          last_report: null,
        };
      });
      connections = [...connections, ...added];
      return added;
    },
    latestReport: async (name) => byConnection(name),
    scan: async (name, windowDays) => {
      await new Promise((r) => setTimeout(r, 900));
      const report = byConnection(name);
      if (!report) return unsupported();
      const options = { window_days: windowDays ?? 7, top_queries: 60, plans: 25 };
      return { ...report, snapshot: { ...report.snapshot, taken_at: new Date().toISOString(), options } };
    },
    brief: async (_, id) => `# ${id}\n\nThe agent brief is generated by the desktop app and the CLI (\`culprit show\`).`,
    explain: unsupported,
    queryPlan: async (name, queryId) => {
      const plan = byConnection(name)?.snapshot.workload?.queries.find((q) => q.id === queryId)?.plan;
      return plan ?? unsupported();
    },
    drift: async (a, b) => {
      const sample = await driftSample();
      if (sample && sample.a.connection === reportName(a) && sample.b.connection === reportName(b)) return sample;
      if (sample && sample.a.connection === reportName(b) && sample.b.connection === reportName(a)) return flipped(sample);
      return unsupported();
    },
    parsePlan: async (text) => {
      try {
        const value = JSON.parse(text);
        if (value && typeof value === "object" && "root" in value && "engine" in value) return value as Plan;
      } catch {}
      return Promise.reject({
        code: "unsupported",
        message: "The browser demo only reads plans Culprit saved as JSON.",
        hint: "Open the desktop app to read .sqlplan files and EXPLAIN output.",
      });
    },
    diffPlans: () =>
      Promise.reject({ code: "unsupported", message: "Comparing plans runs in the desktop app and the CLI (`culprit plan diff`).", hint: null }),
    savePlanFile: async (name, contents) => {
      const url = URL.createObjectURL(new Blob([contents], { type: "text/plain" }));
      const link = Object.assign(document.createElement("a"), { href: url, download: name });
      link.click();
      setTimeout(() => URL.revokeObjectURL(url), 1000);
      return name;
    },
    live: async (name) => {
      const c = connections.find((x) => x.name === name);
      const production = c?.environment === "production";
      const ended = endedSessions.get(name) ?? new Set<string>();
      const state: LiveState = {
        taken_at: new Date().toISOString(),
        sessions: demoSessions(c?.engine ?? "postgres")
          .filter((s) => !ended.has(s.id))
          .map((s) => (s.blocked_by && ended.has(s.blocked_by) ? { ...s, blocked_by: null, wait: null, wait_ms: null } : s)),
        waits: [],
        session_actions: production ? [] : c?.engine === "mssql" ? ["terminate"] : ["cancel", "terminate"],
        session_actions_note: production ? "Production connection: Culprit doesn't cancel or end sessions here." : null,
      };
      return state;
    },
    interruptSession: async (name, session) => {
      await new Promise((r) => setTimeout(r, 300));
      endedSessions.set(name, new Set([...(endedSessions.get(name) ?? []), session]));
    },
    run: async (name, sql) => demoRows(byConnection(name), sql) ?? unsupported(),
    // The demo can't parse SQL like the desktop app; a leading SELECT/WITH is the only read it recognizes.
    checkStatement: async (_engine, sql) => {
      const verb = (sql.trim().match(/^[(\s]*([A-Za-z]+)/)?.[1] ?? "").toUpperCase();
      const reads_only = ["SELECT", "WITH", "SHOW", "VALUES", "TABLE"].includes(verb) && !/\b(insert|update|delete|merge|into)\b/i.test(sql);
      const every = (verb === "UPDATE" || verb === "DELETE") && !/\bwhere\b/i.test(sql);
      return { reads_only, verb, warning: every ? "There is no WHERE clause, so this changes every row in the table." : null, problem: null };
    },
    catalog: async (name) => {
      const snapshot = byConnection(name)?.snapshot;
      if (!snapshot) return { databases: [], tables: [], truncated: false };
      const tables = snapshot.tables.map((t) => ({
        schema: t.schema,
        name: t.name,
        view: false,
        columns: (snapshot.columns ?? []).filter((c) => c.schema === t.schema && c.table === t.name).sort((a, b) => a.ordinal - b.ordinal).map((c) => ({ name: c.name, data_type: c.data_type })),
      }));
      return { databases: [], tables, truncated: false };
    },
    savedQueries: async () => demoSaved.list(),
    saveQuery: async (query) => demoSaved.put(query),
    deleteSavedQuery: async (name) => demoSaved.remove(name),
    savedQueryRan: async (name) => demoSaved.touch(name),
    setAgentAccess: async (name, access) => {
      const current = connections.find((c) => c.name === name);
      if (!current) return Promise.reject({ code: "not_found", message: `No connection named '${name}'`, hint: null });
      const updated = { ...current, agent_access: access };
      connections = connections.map((c) => (c.name === name ? updated : c));
      return updated;
    },
    // `?demo-approval` shows a sample request so the approval dialog can be designed without an agent.
    pendingApprovals: async () => (demoApproval ? [demoApproval] : []),
    decideApproval: async () => {
      demoApproval = null;
    },
  };
}
