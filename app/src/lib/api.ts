import type { Catalog } from "../gen/Catalog";
import type { AgentAccess } from "../gen/AgentAccess";
import type { ApprovalRequest } from "../gen/ApprovalRequest";
import type { ConnectionInfo } from "../gen/ConnectionInfo";
import type { DiscoveredDatabase } from "../gen/DiscoveredDatabase";
import type { Drift } from "../gen/Drift";
import type { ExplainMode } from "../gen/ExplainMode";
import type { ImportCandidate } from "../gen/ImportCandidate";
import type { ImportSource } from "../gen/ImportSource";
import type { LiveState } from "../gen/LiveState";
import type { NewConnection } from "../gen/NewConnection";
import type { ParsedConnection } from "../gen/ParsedConnection";
import type { Plan } from "../gen/Plan";
import type { PlanDiff } from "../gen/PlanDiff";
import type { Report } from "../gen/Report";
import type { ResultSet } from "../gen/ResultSet";
import type { SavedQuery } from "../gen/SavedQuery";
import type { SaveQuery } from "../gen/SaveQuery";
import type { ServerInfo } from "../gen/ServerInfo";
import type { Engine } from "../gen/Engine";
import type { StatementCheck } from "../gen/StatementCheck";
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
  /** What differs between the latest scans of two connections of the same engine. */
  drift(a: string, b: string): Promise<Drift>;
  parsePlan(text: string): Promise<Plan>;
  diffPlans(a: Plan, b: Plan, nameA: string, nameB: string): Promise<PlanDiff>;
  /** Writes into the Downloads folder and returns the path; the browser demo downloads instead. */
  savePlanFile(name: string, contents: string): Promise<string>;
  live(name: string): Promise<LiveState>;
  interruptSession(name: string, session: string, action: SessionAction): Promise<void>;
  run(name: string, sql: string, limit: number, confirmed: boolean): Promise<ResultSet>;
  checkStatement(engine: Engine, sql: string): Promise<StatementCheck>;
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
    drift: (a, b) => invoke("drift", { a, b }),
    parsePlan: (text) => invoke("parse_plan", { text }),
    diffPlans: (a, b, nameA, nameB) => invoke("diff_plans", { a: { ...a, raw: null }, b: { ...b, raw: null }, nameA, nameB }),
    savePlanFile: (name, contents) => invoke("save_plan_file", { name, contents }),
    live: (name) => invoke("live", { name }),
    interruptSession: (name, session, action) => invoke("interrupt_session", { name, session, action }),
    run: (name, sql, limit, confirmed) => invoke("run_sql", { name, sql, limit, confirmed }),
    checkStatement: (engine, sql) => invoke("check_statement", { engine, sql }),
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
  await navigator.clipboard.writeText(text);
}
