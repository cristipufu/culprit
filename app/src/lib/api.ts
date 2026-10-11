import type { ConnectionInfo } from "../gen/ConnectionInfo";
import type { DiscoveredDatabase } from "../gen/DiscoveredDatabase";
import type { ExplainMode } from "../gen/ExplainMode";
import type { LiveState } from "../gen/LiveState";
import type { NewConnection } from "../gen/NewConnection";
import type { ParsedConnection } from "../gen/ParsedConnection";
import type { Plan } from "../gen/Plan";
import type { Report } from "../gen/Report";
import type { ResultSet } from "../gen/ResultSet";
import type { ServerInfo } from "../gen/ServerInfo";

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
  addDiscovered(items: DiscoveredDatabase[]): Promise<ConnectionInfo[]>;
  latestReport(name: string, windowDays: number | null): Promise<Report | null>;
  scan(name: string, windowDays: number | null): Promise<Report>;
  brief(name: string, findingId: string, windowDays: number | null): Promise<string>;
  explain(name: string, sql: string, mode: ExplainMode, confirmed: boolean): Promise<Plan>;
  live(name: string): Promise<LiveState>;
  run(name: string, sql: string, limit: number, confirmed: boolean): Promise<ResultSet>;
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
    addDiscovered: (items) => invoke("add_discovered", { items }),
    latestReport: (name, windowDays) => invoke("latest_report", { name, windowDays }),
    scan: (name, windowDays) => invoke("scan", { name, windowDays }),
    brief: (name, findingId, windowDays) => invoke("brief", { name, findingId, windowDays }),
    explain: (name, sql, mode, confirmed) => invoke("explain", { name, sql, mode, confirmed }),
    live: (name) => invoke("live", { name }),
    run: (name, sql, limit, confirmed) => invoke("run_sql", { name, sql, limit, confirmed }),
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
