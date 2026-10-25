import { useSyncExternalStore } from "react";
import type { ChangeRequest } from "../gen/ChangeRequest";
import type { ConnectionInfo } from "../gen/ConnectionInfo";
import type { Report } from "../gen/Report";
import type { SavedQuery } from "../gen/SavedQuery";
import { api } from "./api";
import { renameEditorTabs } from "./editor-tabs";
import { ReportRequests, type ReportState, withConnectionName } from "./report-requests";
import { LatestRequest } from "./requests";
export type { ReportState } from "./report-requests";

/** `dashboard` is where a connection opens; `overview` is the ranked findings list (titled "Findings"). */
export type View = "dashboard" | "overview" | "queries" | "live" | "schema" | "editor" | "plans" | "connections";

export type State = {
  connections: ConnectionInfo[];
  active: string | null;
  view: View;
  report: ReportState;
  finding: string | null;
  query: string | null;
  /** "What changed" is open, comparing these periods. */
  changes: ChangeRequest | null;
  palette: boolean;
  toast: string | null;
  editorSql: string | null;
  /** A saved query to open in the editor (and run, from the palette). */
  editorSaved: { name: string; run: boolean } | null;
  /** Saved queries, loaded on first use. */
  saved: SavedQuery[] | null;
  /** Open Schema on the index advisor (from the dashboard). */
  advisor: boolean;
  /** Open Schema on this table (`schema.table`, from the palette). */
  schemaTable: string | null;
  dismissed: Record<string, true>;
  windowMinutes: WindowMinutes;
};

/** A "What changed" request with only the given fields set; the rest fall back to the defaults. */
export function changeRequest(fields: Partial<ChangeRequest> = {}): ChangeRequest {
  return {
    since: null,
    recent_start: null,
    recent_end: null,
    baseline_start: null,
    baseline_end: null,
    recent_hours: null,
    min_executions: null,
    threshold: null,
    cloud: null,
    ...fields,
  };
}

/** The workload windows the picker offers, in minutes: an hour, 6 hours, 24 hours, 7 days and 30 days. */
export const WINDOW_OPTIONS = [60, 360, 1440, 10080, 43200] as const;
export type WindowMinutes = (typeof WINDOW_OPTIONS)[number];
const DEFAULT_WINDOW: WindowMinutes = 10080;
const MINUTES_PER_DAY = 1440;

/** A scan's window in minutes, also for scans from before windows could be hours. */
export function windowMinutesOf(options: { window_days: number; window_minutes?: number } | null | undefined): number | null {
  return options ? options.window_minutes ?? options.window_days * MINUTES_PER_DAY : null;
}

function windowKey(name: string): string {
  return `culprit.window.${name}`;
}

function rememberedWindow(name: string): WindowMinutes {
  try {
    const v = Number(localStorage.getItem(windowKey(name)));
    const offered = (m: number): m is WindowMinutes => (WINDOW_OPTIONS as readonly number[]).includes(m);
    if (offered(v)) return v;
    // Remembered in days before the picker offered hours.
    const fromDays = v * MINUTES_PER_DAY;
    return offered(fromDays) ? fromDays : DEFAULT_WINDOW;
  } catch {
    return DEFAULT_WINDOW;
  }
}

const DISMISSED_KEY = "culprit.dismissed";

function loadDismissed(): Record<string, true> {
  try {
    return JSON.parse(localStorage.getItem(DISMISSED_KEY) ?? "{}");
  } catch {
    return {};
  }
}

let state: State = {
  connections: [],
  active: null,
  view: "connections",
  report: { status: "none" },
  finding: null,
  query: null,
  changes: null,
  palette: false,
  toast: null,
  editorSql: null,
  editorSaved: null,
  saved: null,
  advisor: false,
  schemaTable: null,
  dismissed: loadDismissed(),
  windowMinutes: DEFAULT_WINDOW,
};

const listeners = new Set<() => void>();

export function getState(): State {
  return state;
}

export function setState(patch: Partial<State> | ((s: State) => Partial<State>)): void {
  const next = typeof patch === "function" ? patch(state) : patch;
  if ((next.active !== undefined && next.active !== state.active)
    || (next.windowMinutes !== undefined && next.windowMinutes !== state.windowMinutes)) reports.invalidate();
  state = { ...state, ...next };
  listeners.forEach((l) => l());
}

export function useStore<T>(select: (s: State) => T): T {
  return useSyncExternalStore(
    (cb) => {
      listeners.add(cb);
      return () => listeners.delete(cb);
    },
    () => select(state),
  );
}

const reports = new ReportRequests({ client: api, read: getState, publish: setState, collected: loadConnections });
const connectionRequests = new LatestRequest();
const savedRequests = new LatestRequest();

let toastTimer: ReturnType<typeof setTimeout> | undefined;

/** Load (or reload) the saved queries into the store. */
export async function loadSaved(): Promise<SavedQuery[]> {
  const owns = savedRequests.start();
  try {
    const saved = await (await api()).savedQueries();
    if (owns()) setState({ saved });
    return saved;
  } catch {
    if (owns()) setState({ saved: getState().saved ?? [] });
    return getState().saved ?? [];
  }
}

export function toast(message: string): void {
  clearTimeout(toastTimer);
  setState({ toast: message });
  toastTimer = setTimeout(() => setState({ toast: null }), 1800);
}

export async function loadConnections(): Promise<ConnectionInfo[]> {
  const owns = connectionRequests.start();
  const connections = await (await api()).connections();
  if (owns()) setState({ connections });
  return connections;
}

/** Carry the current UI session and its preferences to the saved alias without changing the selected view. */
export async function renameConnection(old: string, next: string): Promise<ConnectionInfo> {
  const saved = await (await api()).renameConnection(old, next);
  connectionRequests.invalidate();
  const current = getState();
  const active = current.active === old;
  const previous = current.connections.find((c) => c.name === old) ?? { ...saved, name: old };
  const tabError = renameEditorTabs(previous, saved);
  if (tabError) toast(tabError);
  try {
    const remembered = active ? String(current.windowMinutes) : localStorage.getItem(windowKey(old));
    if (remembered !== null) localStorage.setItem(windowKey(saved.name), remembered);
    if (old !== saved.name) localStorage.removeItem(windowKey(old));
  } catch {}
  if (active || lastConnection() === old) rememberLast(saved.name);
  const resume = active && (current.report.status === "loading" || (current.report.status === "ready" && current.report.scanning));
  const connections = current.connections.some((c) => c.name === old)
    ? current.connections.map((c) => c.name === old ? saved : c)
    : [...current.connections.filter((c) => c.name !== saved.name), saved];
  setState({ connections, ...(active ? {
    active: saved.name,
    report: current.report.status === "ready"
      ? { ...current.report, report: withConnectionName(current.report.report, saved.name) }
      : current.report,
  } : {}) });
  if (resume) void reports.open(saved.name, current.windowMinutes, true);
  // A failed metadata refresh must not turn a successful rename into a retry against the retired alias.
  void loadConnections().catch(() => {});
  return saved;
}

/** Open a connection: show its last report for the remembered window instantly, then refresh it in place. */
export async function openConnection(name: string, options: { rescan?: boolean } = {}): Promise<void> {
  const rescan = options.rescan ?? true;
  const windowMinutes = rememberedWindow(name);
  setState({ active: name, view: "dashboard", finding: null, query: null, changes: null, report: { status: "loading" }, windowMinutes });
  rememberLast(name);
  await reports.open(name, windowMinutes, rescan);
}

/** Switch the workload window (Query Store): cached report for that window if there is one, then a fresh scan. */
export async function setWindow(minutes: WindowMinutes): Promise<void> {
  const name = getState().active;
  if (!name || minutes === getState().windowMinutes) return;
  try {
    localStorage.setItem(windowKey(name), String(minutes));
  } catch {}
  setState({ windowMinutes: minutes, report: { status: "loading" } });
  await reports.open(name, minutes, true);
}

/** Refresh only the current report session; a stale view cannot start work for a different connection. */
export async function scan(name: string): Promise<void> {
  await reports.scan(name, getState().windowMinutes);
}

export function toggleDismissed(id: string): void {
  setState((s) => {
    const dismissed = { ...s.dismissed };
    if (dismissed[id]) delete dismissed[id];
    else dismissed[id] = true;
    try {
      localStorage.setItem(DISMISSED_KEY, JSON.stringify(dismissed));
    } catch {}
    return { dismissed };
  });
}

const LAST_KEY = "culprit.last";

function rememberLast(name: string): void {
  try {
    localStorage.setItem(LAST_KEY, name);
  } catch {}
}

export function lastConnection(): string | null {
  try {
    return localStorage.getItem(LAST_KEY);
  } catch {
    return null;
  }
}

export function currentReport(s: State): Report | null {
  return s.report.status === "ready" ? s.report.report : null;
}
