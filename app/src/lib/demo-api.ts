import type { ApprovalRequest } from "../gen/ApprovalRequest";
import type { ChangeEvent } from "../gen/ChangeEvent";
import type { Changes } from "../gen/Changes";
import type { ConnectionInfo } from "../gen/ConnectionInfo";
import type { Dashboard } from "../gen/Dashboard";
import type { DashboardLive } from "../gen/DashboardLive";
import type { DeadlockReport } from "../gen/DeadlockReport";
import type { DiscoveredDatabase } from "../gen/DiscoveredDatabase";
import type { Drift } from "../gen/Drift";
import type { IndexAdvice } from "../gen/IndexAdvice";
import type { ImportCandidate } from "../gen/ImportCandidate";
import type { LiveState } from "../gen/LiveState";
import type { Mark } from "../gen/Mark";
import type { Plan } from "../gen/Plan";
import type { PlanVariant } from "../gen/PlanVariant";
import type { QueryStat } from "../gen/QueryStat";
import type { PlanInsights } from "../gen/PlanInsights";
import type { PlanDiff } from "../gen/PlanDiff";
import type { QueryProgress } from "../gen/QueryProgress";
import type { Report } from "../gen/Report";
import type { CheckInfo } from "../gen/CheckInfo";
import type { SavedQuery } from "../gen/SavedQuery";
import type { SaveQuery } from "../gen/SaveQuery";
import type { Pressure } from "../gen/Pressure";
import type { ResourceSample } from "../gen/ResourceSample";
import type { WaitSample } from "../gen/WaitSample";
import type { WaitsReport } from "../gen/WaitsReport";
import type { Api, UpdateProgress } from "./api";
import { demoCloud } from "./demo-cloud";
import { demoRows } from "./demo-rows";
import { signalsSample, withSignals } from "./demo-signals";
import { can, refusal, sessionActions, waitingState } from "./engines";

/** The capabilities query history comes from (mirrors `Snapshot::no_workload_reason`). */
/** A scan's window in minutes, also for scans from before windows could be hours. */
const windowMinutesOf = (options: { window_days: number; window_minutes?: number } | null | undefined): number | null =>
  options ? options.window_minutes ?? options.window_days * 1440 : null;


const reports: Record<string, () => Promise<Report>> = import.meta.env.DEV
  ? import.meta.glob<Report>("../../../fixtures/*.report.json", { import: "default" })
  : {};

const discoverSample: () => Promise<DiscoveredDatabase[]> = import.meta.env.DEV
  ? () => import("../../../fixtures/discover.sample.json").then((m) => m.default as DiscoveredDatabase[])
  : async () => [];

/** The advisor's output for each demo connection, made with `culprit indexes fixtures/<name>.report.json --json`. */
const adviceSample: () => Promise<Record<string, IndexAdvice>> = import.meta.env.DEV
  ? () => import("../../../fixtures/index-advice.sample.json").then((m) => m.default as unknown as Record<string, IndexAdvice>)
  : async () => ({});

/**
 * The dashboard for each demo connection, made with `culprit dashboard <report> --no-live --json` from the fixture
 * reports merged with the sidecars below (pressure, waits, deadlocks), exactly as this demo serves them.
 */
const dashboardSample: () => Promise<Record<string, Dashboard>> = import.meta.env.DEV
  ? () => import("../../../fixtures/dashboard.sample.json").then((m) => m.default as unknown as Record<string, Dashboard>)
  : async () => ({});

/** Sessions and blocking chains captured from the demo databases with a planted chain, per engine. */
const liveSample: () => Promise<Record<string, LiveState> | null> = import.meta.env.DEV
  ? () => import("../../../fixtures/live.sample.json").then((m) => m.default as unknown as Record<string, LiveState>)
  : async () => null;

/** Rust-produced dashboard updates for every subset of the demo's planted sessions. */
const dashboardLiveSample: () => Promise<Record<string, Record<string, DashboardLive>>> = import.meta.env.DEV
  ? () => import("../../../fixtures/dashboard-live.sample.json").then((m) => m.default as unknown as Record<string, Record<string, DashboardLive>>)
  : async () => ({});

/** Deadlocks captured from the demo databases (and a key-lookup graph), per engine. */
const deadlockSample: () => Promise<Record<string, DeadlockReport> | null> = import.meta.env.DEV
  ? () => import("../../../fixtures/deadlocks.sample.json").then((m) => m.default as unknown as Record<string, DeadlockReport>)
  : async () => null;

/** "What changed" as the desktop app computed it on the demo databases (Query Store, and scans for PostgreSQL/MySQL). */
const changesSample: () => Promise<Record<string, Changes> | null> = import.meta.env.DEV
  ? () => import("../../../fixtures/changes.sample.json").then((m) => m.default as unknown as Record<string, Changes>)
  : async () => null;

/** Marks and schema/settings changes around the demo reports' timelines (synthetic). */
const eventsSample: () => Promise<Record<string, ChangeEvent[]> | null> = import.meta.env.DEV
  ? () => import("../../../fixtures/events.sample.json").then((m) => m.default as unknown as Record<string, ChangeEvent[]>)
  : async () => null;

/** Runtime spread captured from the demo databases: fixture reports predate it. `extra` adds a planted
 * parameter-sensitive query (SQL Server). */
type SpreadPatch = Pick<QueryStat, "spread" | "max_time_ms" | "stddev_time_ms"> & {
  plans?: Record<string, Pick<PlanVariant, "min_time_ms" | "max_time_ms" | "stddev_time_ms" | "compiled" | "variant_query">>;
};
type SpreadSample = Record<string, { queries: Record<string, SpreadPatch>; extra: QueryStat[] }>;
const spreadSample: () => Promise<SpreadSample | null> = import.meta.env.DEV
  ? () => import("../../../fixtures/spread.sample.json").then((m) => m.default as unknown as SpreadSample)
  : async () => null;

function withSpread(report: Report, sample: SpreadSample[string] | undefined): Report {
  const workload = report.snapshot.workload;
  if (!sample || !workload) return report;
  const patched = workload.queries.map((q) => {
    const patch = sample.queries[q.id];
    if (!patch) return q;
    const plans = q.plans.map((p) => ({ ...p, ...(patch.plans?.[p.id] ?? {}) }));
    return { ...q, spread: patch.spread, max_time_ms: patch.max_time_ms ?? q.max_time_ms, stddev_time_ms: patch.stddev_time_ms ?? q.stddev_time_ms, plans };
  });
  const extra = sample.extra.filter((x) => !patched.some((q) => q.id === x.id));
  const queries = [...patched, ...extra];
  const added = extra.reduce((sum, q) => sum + q.total_time_ms, 0);
  return {
    ...report,
    snapshot: {
      ...report.snapshot,
      workload: {
        ...workload,
        queries,
        query_count: workload.query_count + extra.length,
        total_time_ms: workload.total_time_ms + added,
        total_executions: workload.total_executions + extra.reduce((sum, q) => sum + q.executions, 0),
      },
    },
  };
}

/** Comparisons and exports the desktop app's Rust code made from plans captured on the demo databases. */
export type ShareSample = {
  compares: { engine: Plan["engine"] | "mariadb"; names: [string, string]; a: Plan; b: Plan; diff: PlanDiff; reverse: PlanDiff; markdown: string; html: string; plan_markdown: [string, string] }[];
  findings: Record<string, { markdown: string; html?: string }>;
};

export const shareSample: () => Promise<ShareSample | null> = import.meta.env.DEV
  ? () => import("../../../fixtures/share.sample.json").then((m) => m.default as unknown as ShareSample)
  : async () => null;

/** Demo limitations are information, not errors: the UI shows them calmly. */
function desktopOnly(message: string): Promise<never> {
  return Promise.reject({ code: "unsupported", message, hint: "Open the desktop app or use the CLI for your own plans and scans." });
}

const driftSample: () => Promise<Drift | null> = import.meta.env.DEV
  ? () => import("../../../fixtures/drift.sample.json").then((m) => m.default as Drift)
  : async () => null;

/** The sample read from the other side: the same differences with "only in" flipped. */
function flipped(d: Drift): Drift {
  const presence = { "only-a": "only-b", "only-b": "only-a", different: "different", renamed: "renamed" } as const;
  return { ...d, a: d.b, b: d.a, items: d.items.map((i) => ({ ...i, presence: presence[i.presence], a: i.b, b: i.a })) };
}

/** Waits, pressure and samples captured from the demo databases (see fixtures/waits.sample.json). */
type WaitsSidecar = {
  reports: Record<string, WaitsReport | null>;
  pressure: Record<string, Pressure | null>;
  samples: Record<string, WaitSample>;
  resources: Record<string, ResourceSample[]>;
};

const waitsSample: () => Promise<WaitsSidecar | null> = import.meta.env.DEV
  ? () => import("../../../fixtures/waits.sample.json").then((m) => m.default as unknown as WaitsSidecar)
  : async () => null;

type ActualSample = { last_actual: Record<string, Record<string, Plan>>; progress: Record<string, QueryProgress[]> };

/** Last actual plans captured from the demo SQL Server, and progress frames for the running "reports" session. */
const actualSample: () => Promise<ActualSample | null> = import.meta.env.DEV
  ? () => import("../../../fixtures/actual.sample.json").then((m) => m.default as unknown as ActualSample)
  : async () => null;

const importSample: () => Promise<ImportCandidate[]> = import.meta.env.DEV
  ? () => import("../../../fixtures/import.sample.json").then((m) => m.default as ImportCandidate[])
  : async () => [];

function nameOf(path: string): string {
  return path.split("/").pop()!.replace(".report.json", "");
}

/** The desktop app computes plan insights live; the demo reads the same output from a sidecar file. */
const insightsSample: () => Promise<Record<string, Record<string, PlanInsights>>> = import.meta.env.DEV
  ? () => import("../../../fixtures/insights.sample.json").then((m) => m.default as unknown as Record<string, Record<string, PlanInsights>>)
  : async () => ({});

async function load(name: string): Promise<Report | null> {
  const entry = Object.entries(reports).find(([path]) => nameOf(path) === name);
  if (!entry) return null;
  const report = await entry[1]();
  const insights = (await insightsSample())[report.snapshot.connection] ?? {};
  report.snapshot.workload?.queries.forEach((q) => {
    if (q.plan && !q.plan.insights && insights[q.id]) q.plan.insights = insights[q.id];
  });
  return report;
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
  const loaded = await Promise.all(Object.keys(reports).map((p) => load(nameOf(p))));
  // Fixture reports predate pressure signals; the waits sidecar fills them in.
  const waits = await waitsSample();
  const spread = await spreadSample();
  const recordedDeadlocks = await deadlockSample();
  const signals = await signalsSample();
  const all = loaded.map((loadedReport) => {
    const r = loadedReport && spread ? withSpread(loadedReport, spread[loadedReport.snapshot.connection]) : loadedReport;
    if (!r || !waits) return r;
    const name = r.snapshot.connection;
    const resources = r.snapshot.resources.length ? r.snapshot.resources : (waits.resources[name] ?? []);
    // Older fixtures carry no waits for engines that gained them later; the card and the panel must agree.
    const report = waits.reports[name];
    const recorded = r.snapshot.waits.length
      ? r.snapshot.waits
      : (report?.categories ?? []).map((c) => ({ category: c.key, wait_type: c.top_wait, wait_ms: c.wait_ms, tasks: null }));
    // Every scan records a day of deadlocks; the fixtures predate that, so the captured sample stands in.
    const deadlocks = r.snapshot.deadlocks ?? recordedDeadlocks?.[r.snapshot.server.engine] ?? null;
    return { ...r, snapshot: { ...r.snapshot, waits: recorded, pressure: r.snapshot.pressure ?? waits.pressure[name] ?? null, resources, deadlocks } };
  }).map((r) => (r && signals ? withSignals(r, signals[r.snapshot.connection]) : r));
  // `?first-run` in the dev demo starts with no saved connections, to see the first-run screen.
  const firstRun = import.meta.env.DEV && typeof location !== "undefined" && location.search.includes("first-run");
  let connections: ConnectionInfo[] = all
    .filter((r): r is Report => r !== null && !firstRun)
    .map((r) => ({
      name: r.snapshot.connection,
      engine: r.snapshot.server.engine,
      environment: r.snapshot.environment,
      host: { mssql: "localhost,1433", postgres: "localhost:5432", mysql: "localhost:3306", sqlite: "" }[r.snapshot.server.engine],
      port: null,
      database: r.snapshot.server.database,
      user: null,
      auth: can(r.snapshot.server.engine, "sign_in") ? "password" : "none",
      has_secret: can(r.snapshot.server.engine, "sign_in"),
      display: r.snapshot.server.database,
      agent_access: "stats",
      // A report whose statements came from a log: the desktop would have it saved on the connection.
      query_log: r.snapshot.workload?.log != null ? "log/development.log" : null,
      last_report: {
        id: r.snapshot.id,
        taken_at: r.snapshot.taken_at,
        opportunities: r.summary.opportunities,
        headline: r.summary.headline,
        window_days: r.snapshot.options && r.snapshot.options.window_minutes == null ? r.snapshot.options.window_days : null,
        window_minutes: windowMinutesOf(r.snapshot.options),
        product: r.snapshot.server.product,
        no_workload: r.snapshot.workload ? null : (r.summary.no_workload ?? "This scan has no query history."),
      },
    }));
  const demoUpdate = import.meta.env.DEV && typeof location !== "undefined" && location.search.includes("demo-update");
  const updateListeners = new Set<(progress: UpdateProgress) => void>();
  let demoApproval: ApprovalRequest | null =
    import.meta.env.DEV && typeof location !== "undefined" && location.search.includes("demo-approval")
      ? {
          id: "0".repeat(32),
          connection: connections[0]?.name ?? "mssql-demo",
          environment: connections[0]?.environment ?? "staging",
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
  const demoMarks = new Map<string, Mark[]>();
  const reportName = (name: string) => aliases.get(name) ?? name;
  const byConnection = (name: string) => all.find((r) => r?.snapshot.connection === reportName(name)) ?? null;
  const liveView = async (name: string): Promise<DashboardLive> => {
    const connection = connections.find((x) => x.name === name);
    if (connection && !can(connection.engine, "sessions")) return Promise.reject(refusal(connection.engine, "sessions"));
    const captured = (await liveSample())?.[connection?.engine ?? "postgres"];
    const ended = endedSessions.get(name) ?? new Set<string>();
    const key = JSON.stringify((captured?.sessions ?? []).filter((s) => !ended.has(s.id)).map((s) => s.id).sort());
    const view = (await dashboardLiveSample())[reportName(name)]?.[key];
    if (!view?.live) return unsupported();
    const production = connection?.environment === "production";
    return {
      ...view,
      live: {
        ...view.live,
        taken_at: new Date().toISOString(),
        session_actions: production || !connection ? [] : sessionActions(connection.engine),
        session_actions_note: production ? "Production connection: Culprit doesn't cancel or end sessions here." : null,
      },
    };
  };
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
        query_log: null,
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
        query_log: null,
          last_report: null,
        };
      });
      connections = [...connections, ...added];
      return added;
    },
    latestReport: async (name) => byConnection(name),
    waits: async (name) => (await waitsSample())?.reports[reportName(name)] ?? null,
    // The demo replays a sample captured from the demo database, at a tenth of the real time.
    waitSample: async (name, seconds) => {
      const engine = connections.find((c) => c.name === name)?.engine;
      if (engine && !can(engine, "wait_sample")) return Promise.reject(refusal(engine, "wait_sample"));
      const sample = (await waitsSample())?.samples[reportName(name)];
      await new Promise((r) => setTimeout(r, seconds * 100));
      return sample ? { ...sample, taken_at: new Date().toISOString() } : unsupported();
    },
    scan: async (name, windowMinutes) => {
      await new Promise((r) => setTimeout(r, 900));
      const report = byConnection(name);
      if (!report) return unsupported();
      const minutes = windowMinutes ?? 10080;
      const exactDays = minutes % 1440 === 0;
      const options = {
        window_days: Math.ceil(minutes / 1440),
        ...(exactDays ? {} : { window_minutes: minutes }),
        top_queries: 60,
        plans: 25,
      };
      return { ...report, snapshot: { ...report.snapshot, taken_at: new Date().toISOString(), options } };
    },
    brief: async (_, id) => `# ${id}\n\nThe agent brief is generated by the desktop app and the CLI (\`culprit show\`).`,
    explain: unsupported,
    lastActualPlan: async (name, queryId) => {
      // Answered as the engine does: its own refusal where it keeps no actual plans (from the engine facts).
      const engine = connections.find((c) => c.name === name)?.engine;
      if (engine && !can(engine, "last_actual_plan")) return Promise.reject(refusal(engine, "last_actual_plan"));
      const plan = (await actualSample())?.last_actual[reportName(name)]?.[queryId];
      return (
        plan ??
        Promise.reject({ code: "not_found", message: "This query's plan isn't in the plan cache right now", hint: "SQL Server keeps the last actual plan only while the plan is cached. It comes back the next time the query runs." })
      );
    },
    queryProgress: async (name, session, withPlan) => {
      const engine = connections.find((c) => c.name === name)?.engine ?? "mssql";
      if (!can(engine, "sessions")) return Promise.reject(refusal(engine, "sessions"));
      const frames = (await actualSample())?.progress[engine] ?? [];
      const now = new Date().toISOString();
      const live = (await liveSample())?.[engine]?.sessions.find((s) => s.id === session);
      // The captured progress frames replay for the session running a statement without waiting on another.
      if (live?.state !== "idle" && !live?.blocked_by && frames.length) {
        const frame = frames[Math.floor(Date.now() / 1500) % frames.length]!;
        // Live per-step counters come every time on an engine that sends them; elsewhere the plan only when asked.
        return { ...frame, session, taken_at: now, plan: withPlan || can(engine, "operator_progress") ? frame.plan : null };
      }
      const idle: QueryProgress = {
        session, engine, taken_at: now, running: false, state: live?.state ?? null, wait: null, wait_ms: null, elapsed_ms: null, cpu_ms: null,
        logical_reads: null, rows_examined: null, statement: live?.sql ?? null, plan: null, nodes: [], percent: null, percent_basis: null,
        hot_node: null, phase: null, phase_percent: null, note: null,
      };
      if (live?.state === "active") {
        return { ...idle, running: true, state: waitingState(engine), wait: live.wait, wait_ms: live.wait_ms, elapsed_ms: live.elapsed_ms, note: live.blocked_by ? `It is waiting for session ${live.blocked_by} to release a lock, so no step has started yet.` : null };
      }
      return idle;
    },
    queryPlan: async (name, queryId) => {
      const plan = byConnection(name)?.snapshot.workload?.queries.find((q) => q.id === queryId)?.plan;
      return plan ?? unsupported();
    },
    dashboard: async (name) => {
      const sample = (await dashboardSample())[reportName(name)];
      if (sample) return { ...sample, connection: name };
      return Promise.reject({ code: "not_found", message: `No saved report for '${name}'`, hint: "Scan it first." });
    },
    dashboardLive: async (name, dashboard) => {
      const view = await liveView(name);
      const now = view.dashboard.tiles.find((t) => t.key === "now")!;
      return {
        ...view,
        dashboard: { ...dashboard, headline: view.dashboard.headline, attention: view.dashboard.attention, tiles: dashboard.tiles.map((t) => t.key === "now" ? now : t) },
      };
    },
    indexAdvice: async (name) => {
      const sample = (await adviceSample())[reportName(name)];
      if (sample) return { ...sample, connection: name };
      return Promise.reject({ code: "not_found", message: `No saved report for '${name}'`, hint: "Scan it first." });
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
    diffPlans: async (a, b) => {
      const sample = await shareSample();
      for (const c of sample?.compares ?? []) {
        if (c.a.total_cost === a.total_cost && c.b.total_cost === b.total_cost) return c.diff;
        if (c.a.total_cost === b.total_cost && c.b.total_cost === a.total_cost) return c.reverse;
      }
      return desktopOnly("The demo compares its sample plans; comparing other plans runs in the desktop app.");
    },
    exportPlan: async (plan, title, format) => {
      const sample = await shareSample();
      for (const c of sample?.compares ?? []) {
        const contents = c.a.total_cost === plan.total_cost ? c.plan_markdown[0] : c.b.total_cost === plan.total_cost ? c.plan_markdown[1] : null;
        if (contents && format === "markdown") return { name: `${title.toLowerCase().replace(/[^a-z0-9]+/g, "-")}.md`, contents, format };
      }
      return desktopOnly("The demo exports its sample plans as Markdown; HTML pages and .sqlplan files come from the desktop app.");
    },
    exportDiff: async (a, b, nameA, nameB, format) => {
      const sample = await shareSample();
      const c = sample?.compares.find((x) => x.a.total_cost === a.total_cost && x.b.total_cost === b.total_cost);
      if (!c) return desktopOnly("The demo exports its sample comparisons; others come from the desktop app.");
      const stem = `compare-${nameA}-${nameB}`.toLowerCase().replace(/[^a-z0-9]+/g, "-");
      return format === "html" ? { name: `${stem}.html`, contents: c.html, format } : { name: `${stem}.md`, contents: c.markdown, format: "markdown" };
    },
    exportQueryPlan: () => desktopOnly("Exporting a query's stored plan reads it from the database."),
    exportFinding: async (name, findingId, format) => {
      const sample = await shareSample();
      const entry = sample?.findings[`${reportName(name)}|${findingId}`];
      const stem = findingId.toLowerCase().replace(/[^a-z0-9]+/g, "-").slice(0, 60);
      if (entry && format === "html" && entry.html) return { name: `${stem}.html`, contents: entry.html, format };
      if (entry && format !== "html") return { name: `${stem}.md`, contents: entry.markdown, format: "markdown" };
      return desktopOnly("The demo has HTML pages for the top findings only.");
    },
    verifyFindings: () => desktopOnly("Verifying re-scans the database to see whether the finding is gone."),
    savePlanFile: async (name, contents) => {
      const url = URL.createObjectURL(new Blob([contents], { type: "text/plain" }));
      const link = Object.assign(document.createElement("a"), { href: url, download: name });
      link.click();
      setTimeout(() => URL.revokeObjectURL(url), 1000);
      return name;
    },
    screenshot: () => desktopOnly("Screenshots of the window are taken by the desktop app."),
    live: async (name) => (await liveView(name)).live!,
    interruptSession: async (name, session) => {
      await new Promise((r) => setTimeout(r, 300));
      endedSessions.set(name, new Set([...(endedSessions.get(name) ?? []), session]));
    },
    changes: async (name) => {
      await new Promise((r) => setTimeout(r, 350));
      const sample = (await changesSample())?.[reportName(name)];
      return sample ?? unsupported();
    },
    events: async (name, request) => {
      const end = request.end ? Date.parse(request.end) : Date.now();
      const start = request.start ? Date.parse(request.start) : end - (request.hours ?? 168) * 3_600_000;
      if (request.cloud) await new Promise((r) => setTimeout(r, 700));
      const fixture = (await eventsSample())?.[reportName(name)] ?? [];
      const marks: ChangeEvent[] = (demoMarks.get(name) ?? []).map((m) => ({
        id: `mark:${m.id}:${m.at}`,
        kind: "mark",
        source: "mark",
        action: null,
        at: m.at,
        after: null,
        until: null,
        title: m.text,
        detail: null,
        table: null,
        object: null,
        minor: false,
        by: m.by,
      }));
      const cloud = request.cloud ? demoCloud(reportName(name), start, end) : null;
      const throttles: ChangeEvent[] = (cloud?.episodes ?? []).map((e) => ({
        id: `throttle:${e.kind}:${e.start}`,
        kind: "throttle",
        source: "cloud",
        action: null,
        at: e.start,
        after: null,
        until: e.end,
        title: e.title,
        detail: e.detail,
        table: null,
        object: cloud?.resource ?? null,
        minor: false,
        by: null,
      }));
      const events = [...fixture, ...marks, ...throttles]
        .filter((e) => Date.parse(e.until ?? e.at) >= start && Date.parse(e.after ?? e.at) <= end)
        .sort((a, b) => Date.parse(a.after ?? a.at) - Date.parse(b.after ?? b.at));
      return { connection: name, start: new Date(start).toISOString(), end: new Date(end).toISOString(), events, notes: [] };
    },
    cloud: async (name, request) => {
      await new Promise((r) => setTimeout(r, 500));
      const end = request.end ? Date.parse(request.end) : Date.now();
      const start = request.start ? Date.parse(request.start) : end - (request.hours ?? 24) * 3_600_000;
      return demoCloud(reportName(name), start, end) ?? Promise.reject({ code: "unsupported", message: "Not a cloud database", hint: null });
    },
    mark: async (name, text, at) => {
      const mark: Mark = { id: `m${Date.now()}`, at: at ?? new Date().toISOString().replace(/\.\d+Z$/, "Z"), text: text.trim(), by: "app", created_at: new Date().toISOString() };
      demoMarks.set(name, [...(demoMarks.get(name) ?? []), mark]);
      return mark;
    },
    unmark: async (name, id) => {
      const list = demoMarks.get(name) ?? [];
      const mark = list.find((m) => m.id === id);
      if (!mark) return Promise.reject({ code: "not_found", message: `No mark '${id}'`, hint: null });
      demoMarks.set(name, list.filter((m) => m.id !== id));
      return mark;
    },
    deadlocks: async (name, sinceHours) => {
      const c = connections.find((x) => x.name === name);
      if (c && !can(c.engine, "deadlocks")) return Promise.reject(refusal(c.engine, "deadlocks"));
      const sample = (await deadlockSample())?.[c?.engine ?? "postgres"];
      return sample ? { ...sample, since_hours: sinceHours } : { deadlocks: [], source: "demo", since_hours: sinceHours, counted: null, counted_since: null, note: null };
    },
    parseDeadlock: () =>
      Promise.reject({
        code: "unsupported",
        message: "Explaining a pasted deadlock runs in the desktop app and the CLI (`culprit deadlocks --file`).",
        hint: null,
      }),
    run: async (name, sql) => demoRows(byConnection(name), sql) ?? unsupported(),
    // The demo can't parse SQL like the desktop app; a leading SELECT/WITH is the only read it recognizes.
    checkStatement: async (_engine, sql) => {
      const verb = (sql.trim().match(/^[(\s]*([A-Za-z]+)/)?.[1] ?? "").toUpperCase();
      const reads_only = ["SELECT", "WITH", "SHOW", "VALUES", "TABLE"].includes(verb) && !/\b(insert|update|delete|merge|into)\b/i.test(sql);
      const every = (verb === "UPDATE" || verb === "DELETE") && !/\bwhere\b/i.test(sql);
      return { reads_only, verb, warning: every ? "There is no WHERE clause, so this changes every row in the table." : null, problem: null };
    },
    lintSql: async () => [],
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
    checks: async () => (import.meta.env.DEV ? (await import("../../../fixtures/checks.sample.json")).default as CheckInfo[] : []),
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
    setQueryLog: async (name, path) => {
      const current = connections.find((c) => c.name === name);
      if (!current) return Promise.reject({ code: "not_found", message: `No connection named '${name}'`, hint: null });
      const updated = { ...current, query_log: path };
      connections = connections.map((c) => (c.name === name ? updated : c));
      return updated;
    },
    chooseQueryLog: () => unsupported(),
    chooseDatabaseFile: () => unsupported(),
    // `?demo-approval` shows a sample request so the approval dialog can be designed without an agent.
    pendingApprovals: async () => (demoApproval ? [demoApproval] : []),
    decideApproval: async () => {
      demoApproval = null;
    },
    // `?demo-update` offers a sample update so the update flow can be designed without a release.
    checkUpdate: async () => (demoUpdate ? { version: "0.2.1", current: "0.2.0", notes: "### Updates itself\n\n- A sample release." } : null),
    installUpdate: async () => {
      const total = 18_000_000;
      for (let downloaded = 0; downloaded <= total; downloaded += total / 20) {
        updateListeners.forEach((listener) => listener({ downloaded, total }));
        await new Promise((resolve) => setTimeout(resolve, 120));
      }
    },
    onUpdateProgress: async (listener) => {
      updateListeners.add(listener);
      return () => updateListeners.delete(listener);
    },
    restartApp: async () => window.location.reload(),
  };
}
