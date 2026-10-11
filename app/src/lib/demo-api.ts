import type { ConnectionInfo } from "../gen/ConnectionInfo";
import type { DiscoveredDatabase } from "../gen/DiscoveredDatabase";
import type { Report } from "../gen/Report";
import type { Api } from "./api";

const reports: Record<string, () => Promise<Report>> = import.meta.env.DEV
  ? import.meta.glob<Report>("../../../fixtures/*.report.json", { import: "default" })
  : {};

const discoverSample: () => Promise<DiscoveredDatabase[]> = import.meta.env.DEV
  ? () => import("../../../fixtures/discover.sample.json").then((m) => m.default as DiscoveredDatabase[])
  : async () => [];

function nameOf(path: string): string {
  return path.split("/").pop()!.replace(".report.json", "");
}

async function load(name: string): Promise<Report | null> {
  const entry = Object.entries(reports).find(([path]) => nameOf(path) === name);
  return entry ? entry[1]() : null;
}

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
      last_report: {
        id: r.snapshot.id,
        taken_at: r.snapshot.taken_at,
        opportunities: r.summary.opportunities,
        headline: r.summary.headline,
        window_days: r.snapshot.options?.window_days ?? null,
      },
    }));
  const aliases = new Map<string, string>();
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
    live: async () => ({ taken_at: new Date().toISOString(), sessions: [], waits: [] }),
    run: unsupported,
  };
}
