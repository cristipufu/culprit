import type { Catalog } from "../gen/Catalog";
import type { ChangeRequest } from "../gen/ChangeRequest";
import type { Changes } from "../gen/Changes";
import type { CloudSignals } from "../gen/CloudSignals";
import type { CheckInfo } from "../gen/CheckInfo";
import type { AgentAccess } from "../gen/AgentAccess";
import type { ApprovalRequest } from "../gen/ApprovalRequest";
import type { ConnectionInfo } from "../gen/ConnectionInfo";
import type { Deadlock } from "../gen/Deadlock";
import type { Dashboard } from "../gen/Dashboard";
import type { DeadlockReport } from "../gen/DeadlockReport";
import type { DiscoveredDatabase } from "../gen/DiscoveredDatabase";
import type { Drift } from "../gen/Drift";
import type { IndexAdvice } from "../gen/IndexAdvice";
import type { EventLog } from "../gen/EventLog";
import type { EventsRequest } from "../gen/EventsRequest";
import type { ExplainMode } from "../gen/ExplainMode";
import type { ExportFile } from "../gen/ExportFile";
import type { ExportFormat } from "../gen/ExportFormat";
import type { ExportOptions } from "../gen/ExportOptions";
import type { ImportCandidate } from "../gen/ImportCandidate";
import type { ImportSource } from "../gen/ImportSource";
import type { LiveState } from "../gen/LiveState";
import type { Mark } from "../gen/Mark";
import type { NewConnection } from "../gen/NewConnection";
import type { ParsedConnection } from "../gen/ParsedConnection";
import type { Plan } from "../gen/Plan";
import type { QueryProgress } from "../gen/QueryProgress";
import type { PlanDiff } from "../gen/PlanDiff";
import type { Report } from "../gen/Report";
import type { ResultSet } from "../gen/ResultSet";
import type { SavedQuery } from "../gen/SavedQuery";
import type { SaveQuery } from "../gen/SaveQuery";
import type { ServerInfo } from "../gen/ServerInfo";
import type { WaitSample } from "../gen/WaitSample";
import type { WaitsReport } from "../gen/WaitsReport";
import type { Engine } from "../gen/Engine";
import type { StatementCheck } from "../gen/StatementCheck";
import type { Verification } from "../gen/Verification";
import type { SessionAction } from "../gen/SessionAction";
import type { SignInEvent } from "../gen/SignInEvent";

export type ApiError = { code: string; message: string; hint: string | null };

export interface Api {
  readonly kind: "desktop" | "demo";
  connections(): Promise<ConnectionInfo[]>;
  parseConnection(input: string): Promise<ParsedConnection>;
  testConnection(input: NewConnection): Promise<ServerInfo>;
  addConnection(input: NewConnection): Promise<ConnectionInfo>;
  removeConnection(name: string): Promise<void>;
  renameConnection(old: string, next: string): Promise<ConnectionInfo>;
  discoverAzure(terms: string[]): Promise<DiscoveredDatabase[]>;
  discoverAws(terms: string[], regions: string[]): Promise<DiscoveredDatabase[]>;
  setPassword(name: string, password: string): Promise<ConnectionInfo>;
  addDiscovered(items: DiscoveredDatabase[]): Promise<ConnectionInfo[]>;
  importCandidates(source: ImportSource | null): Promise<ImportCandidate[]>;
  importConnections(items: ImportCandidate[]): Promise<ConnectionInfo[]>;
  /** Device-code sign-in progress for Microsoft Entra ID; resolves to an unsubscribe function. */
  onSignIn(listener: (event: SignInEvent) => void): Promise<() => void>;
  cancelSignIn(): Promise<void>;
  openSignInPage(): Promise<void>;
  latestReport(name: string, windowDays: number | null): Promise<Report | null>;
  scan(name: string, windowDays: number | null): Promise<Report>;
  brief(name: string, findingId: string, windowDays: number | null): Promise<string>;
  explain(name: string, sql: string, mode: ExplainMode, confirmed: boolean): Promise<Plan>;
  queryPlan(name: string, queryId: string, planId: string | null): Promise<Plan>;
  /** The actual plan of the query's last run as the engine kept it (SQL Server 2019+); never runs the query. */
  lastActualPlan(name: string, queryId: string, planId: string | null): Promise<Plan>;
  /** How far along a session's running statement is; `withPlan` asks for the plan too. */
  queryProgress(name: string, session: string, withPlan: boolean): Promise<QueryProgress>;
  /** What differs between the latest scans of two connections of the same engine. */
  drift(a: string, b: string): Promise<Drift>;
  /** The dashboard for the latest report with this window (no live read; the app polls sessions itself). */
  dashboard(name: string, windowDays: number | null): Promise<Dashboard>;
  /** The index advisor's plan from the latest report: indexes to add, to clean up, and tables to look at. */
  indexAdvice(name: string): Promise<IndexAdvice>;
  parsePlan(text: string): Promise<Plan>;
  /** `connection`: where the plans came from, so scenarios can use its latest scan's statistics. */
  diffPlans(a: Plan, b: Plan, nameA: string, nameB: string, connection?: string | null): Promise<PlanDiff>;
  exportPlan(plan: Plan, title: string, format: ExportFormat, options: ExportOptions): Promise<ExportFile>;
  exportDiff(a: Plan, b: Plan, nameA: string, nameB: string, format: ExportFormat, options: ExportOptions, connection?: string | null): Promise<ExportFile>;
  /** A plan of a query from the latest scan, with its original text when the engine keeps it (.sqlplan). */
  exportQueryPlan(name: string, queryId: string, planId: string | null, format: ExportFormat, options: ExportOptions): Promise<ExportFile>;
  exportFinding(name: string, findingId: string, format: ExportFormat, options: ExportOptions): Promise<ExportFile>;
  /** Re-scan and compare with the latest scan, finding by finding. */
  verifyFindings(name: string, ids: string[]): Promise<Verification>;
  /** Writes into the Downloads folder and returns the path; the browser demo downloads instead. */
  savePlanFile(name: string, contents: string): Promise<string>;
  live(name: string): Promise<LiveState>;
  /** Where the latest scan's database time went, by wait category with the queries behind each. */
  waits(name: string, windowDays: number | null): Promise<WaitsReport | null>;
  /** What the server waits on right now, sampled for `seconds`. */
  waitSample(name: string, seconds: number): Promise<WaitSample>;
  interruptSession(name: string, session: string, action: SessionAction): Promise<void>;
  run(name: string, sql: string, limit: number, confirmed: boolean): Promise<ResultSet>;
  /** What changed between two periods: Query Store intervals, or Culprit's own scans. */
  changes(name: string, request: ChangeRequest): Promise<Changes>;
  /** What changed around a connection: marks, schema, settings, restarts and cloud throttling. */
  events(name: string, request: EventsRequest): Promise<EventLog>;
  /** What the cloud measured for the server (CloudWatch, Azure Monitor); `unsupported` for other hosts. */
  cloud(name: string, request: EventsRequest): Promise<CloudSignals>;
  /** Put a moment (a deploy, a migration) on the connection's timeline. */
  mark(name: string, text: string, at: string | null): Promise<Mark>;
  unmark(name: string, id: string): Promise<Mark>;
  /** Recent deadlocks on a connection, explained. */
  deadlocks(name: string, sinceHours: number): Promise<DeadlockReport>;
  /** Deadlocks in pasted text: SQL Server .xdl XML, InnoDB status, PostgreSQL log lines. */
  parseDeadlock(text: string): Promise<Deadlock[]>;
  checkStatement(engine: Engine, sql: string): Promise<StatementCheck>;
  checks(): Promise<CheckInfo[]>;
  catalog(name: string): Promise<Catalog>;
  savedQueries(): Promise<SavedQuery[]>;
  saveQuery(query: SaveQuery): Promise<SavedQuery>;
  deleteSavedQuery(name: string): Promise<void>;
  savedQueryRan(name: string): Promise<void>;
  setAgentAccess(name: string, access: AgentAccess): Promise<ConnectionInfo>;
  pendingApprovals(): Promise<ApprovalRequest[]>;
  decideApproval(id: string, hash: string, approved: boolean): Promise<void>;
}

export function isApiError(value: unknown): value is ApiError {
  return typeof value === "object" && value !== null && "message" in value && "code" in value;
}

export function errorMessage(value: unknown): ApiError {
  if (isApiError(value)) return value;
  if (value instanceof Error) return { code: "internal", message: value.message, hint: null };
  return { code: "internal", message: String(value), hint: null };
}

function isDesktop(): boolean {
  return typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;
}

async function desktopApi(): Promise<Api> {
  const { invoke } = await import("@tauri-apps/api/core");
  return {
    kind: "desktop",
    connections: () => invoke("connections"),
    parseConnection: (input) => invoke("parse_connection", { input }),
    testConnection: (input) => invoke("test_connection", { input }),
    addConnection: (input) => invoke("add_connection", { input }),
    removeConnection: (name) => invoke("remove_connection", { name }),
    renameConnection: (old, next) => invoke("rename_connection", { old, new: next }),
    discoverAzure: (terms) => invoke("discover_azure", { terms }),
    discoverAws: (terms, regions) => invoke("discover_aws", { terms, regions }),
    setPassword: (name, password) => invoke("set_password", { name, password }),
    addDiscovered: (items) => invoke("add_discovered", { items }),
    importCandidates: (source) => invoke("import_candidates", { source }),
    importConnections: (items) => invoke("import_connections", { items }),
    onSignIn: async (listener) => {
      const { listen } = await import("@tauri-apps/api/event");
      return listen<SignInEvent>("entra-sign-in", (e) => listener(e.payload));
    },
    cancelSignIn: () => invoke("cancel_sign_in"),
    openSignInPage: () => invoke("open_sign_in_page"),
    latestReport: (name, windowDays) => invoke("latest_report", { name, windowDays }),
    scan: (name, windowDays) => invoke("scan", { name, windowDays }),
    brief: (name, findingId, windowDays) => invoke("brief", { name, findingId, windowDays }),
    explain: (name, sql, mode, confirmed) => invoke("explain", { name, sql, mode, confirmed }),
    queryPlan: (name, queryId, planId) => invoke("query_plan", { name, queryId, planId }),
    lastActualPlan: (name, queryId, planId) => invoke("last_actual_plan", { name, queryId, planId }),
    queryProgress: (name, session, withPlan) => invoke("query_progress", { name, session, withPlan }),
    drift: (a, b) => invoke("drift", { a, b }),
    indexAdvice: (name) => invoke("index_advice", { name }),
    dashboard: (name, windowDays) => invoke("dashboard", { name, windowDays }),
    parsePlan: (text) => invoke("parse_plan", { text }),
    diffPlans: (a, b, nameA, nameB, connection) =>
      invoke("diff_plans", { a: { ...a, raw: null }, b: { ...b, raw: null }, nameA, nameB, connection: connection ?? null }),
    exportPlan: (plan, title, format, options) => invoke("export_plan", { plan: format === "native" ? plan : { ...plan, raw: null }, title, format, options }),
    exportDiff: (a, b, nameA, nameB, format, options, connection) =>
      invoke("export_diff", { a: { ...a, raw: null }, b: { ...b, raw: null }, nameA, nameB, format, options, connection: connection ?? null }),
    exportQueryPlan: (name, queryId, planId, format, options) => invoke("export_query_plan", { name, queryId, planId, format, options }),
    exportFinding: (name, findingId, format, options) => invoke("export_finding", { name, findingId, format, options }),
    verifyFindings: (name, ids) => invoke("verify_findings", { name, ids }),
    savePlanFile: (name, contents) => invoke("save_plan_file", { name, contents }),
    live: (name) => invoke("live", { name }),
    waits: (name, windowDays) => invoke("waits", { name, windowDays }),
    waitSample: (name, seconds) => invoke("wait_sample", { name, seconds }),
    interruptSession: (name, session, action) => invoke("interrupt_session", { name, session, action }),
    run: (name, sql, limit, confirmed) => invoke("run_sql", { name, sql, limit, confirmed }),
    changes: (name, request) => invoke("changes", { name, request }),
    events: (name, request) => invoke("events", { name, request }),
    cloud: (name, request) => invoke("cloud", { name, request }),
    mark: (name, text, at) => invoke("mark", { name, text, at }),
    unmark: (name, id) => invoke("unmark", { name, id }),
    deadlocks: (name, sinceHours) => invoke("deadlocks", { name, sinceHours }),
    parseDeadlock: (text) => invoke("parse_deadlock", { text }),
    checkStatement: (engine, sql) => invoke("check_statement", { engine, sql }),
    checks: () => invoke("checks"),
    catalog: (name) => invoke("catalog", { name }),
    savedQueries: () => invoke("saved_queries"),
    saveQuery: (query) => invoke("save_query", { query }),
    deleteSavedQuery: (name) => invoke("delete_saved_query", { name }),
    savedQueryRan: (name) => invoke("saved_query_ran", { name }),
    setAgentAccess: (name, access) => invoke("set_agent_access", { name, access }),
    pendingApprovals: () => invoke("pending_approvals"),
    decideApproval: (id, hash, approved) => invoke("decide_approval", { id, hash, approved }),
  };
}

let instance: Promise<Api> | null = null;

export function api(): Promise<Api> {
  if (!instance) instance = isDesktop() ? desktopApi() : import("./demo-api").then((m) => m.demoApi());
  return instance;
}

export async function copyText(text: string): Promise<void> {
  try {
    await navigator.clipboard.writeText(text);
  } catch (e) {
    // The async clipboard refuses when the document isn't focused; the older copy command works within the click.
    const area = document.createElement("textarea");
    area.value = text;
    area.setAttribute("readonly", "");
    area.style.cssText = "position:fixed;top:-1000px;opacity:0";
    document.body.appendChild(area);
    area.select();
    const copied = document.execCommand("copy");
    area.remove();
    if (!copied) throw e;
  }
}
