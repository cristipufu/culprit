import { useSyncExternalStore } from "react";
import type { ChangeRequest } from "../gen/ChangeRequest";
import type { ConnectionInfo } from "../gen/ConnectionInfo";
import type { Report } from "../gen/Report";
import type { SavedQuery } from "../gen/SavedQuery";
import { api, errorMessage, type ApiError } from "./api";

/** `dashboard` is where a connection opens; `overview` is the ranked findings list (titled "Findings"). */
export type View = "dashboard" | "overview" | "queries" | "live" | "schema" | "editor" | "plans" | "connections";

export type ReportState =
  | { status: "none" }
  | { status: "loading" }
  | { status: "ready"; report: Report; scanning: boolean; scanError: ApiError | null }
  | { status: "error"; error: ApiError };

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

let toastTimer: ReturnType<typeof setTimeout> | undefined;

/** Load (or reload) the saved queries into the store. */
export async function loadSaved(): Promise<SavedQuery[]> {
  try {
    const saved = await (await api()).savedQueries();
    setState({ saved });
    return saved;
  } catch {
    setState({ saved: [] });
    return [];
  }
}

export function toast(message: string): void {
  clearTimeout(toastTimer);
  setState({ toast: message });
  toastTimer = setTimeout(() => setState({ toast: null }), 1800);
}

export async function loadConnections(): Promise<ConnectionInfo[]> {
  const connections = await (await api()).connections();
  setState({ connections });
  return connections;
}

/** Open a connection: show its last report for the remembered window instantly, then refresh it in place. */
export async function openConnection(name: string, options: { rescan?: boolean } = {}): Promise<void> {
  const rescan = options.rescan ?? true;
  const windowMinutes = rememberedWindow(name);
  setState({ active: name, view: "dashboard", finding: null, query: null, changes: null, report: { status: "loading" }, windowMinutes });
  rememberLast(name);
  await showCachedThenScan(name, windowMinutes, rescan);
}

/** Switch the workload window (Query Store): cached report for that window if there is one, then a fresh scan. */
export async function setWindow(minutes: WindowMinutes): Promise<void> {
  const name = getState().active;
  if (!name || minutes === getState().windowMinutes) return;
  try {
    localStorage.setItem(windowKey(name), String(minutes));
  } catch {}
  setState({ windowMinutes: minutes, report: { status: "loading" } });
  await showCachedThenScan(name, minutes, true);
}

async function showCachedThenScan(name: string, windowMinutes: WindowMinutes, rescan: boolean): Promise<void> {
  const client = await api();
  try {
    const cached = await client.latestReport(name, windowMinutes);
    if (getState().active !== name || getState().windowMinutes !== windowMinutes) return;
    if (cached) {
      const keep = getState().finding;
      const finding = keep && cached.findings.some((f) => f.id === keep) ? keep : cached.findings[0]?.id ?? null;
      setState({ report: { status: "ready", report: cached, scanning: rescan, scanError: null }, finding });
    }
    if (!cached || rescan) await scan(name);
  } catch (e) {
    if (getState().active === name) setState({ report: { status: "error", error: errorMessage(e) } });
  }
}

export async function scan(name: string): Promise<void> {
  const windowMinutes = getState().windowMinutes;
  const current = getState().report;
  if (current.status === "ready") setState({ report: { ...current, scanning: true, scanError: null } });
  else setState({ report: { status: "loading" } });
  try {
    const report = await (await api()).scan(name, windowMinutes);
    if (getState().active !== name || getState().windowMinutes !== windowMinutes) return;
    const keep = getState().finding;
    const stillThere = keep && report.findings.some((f) => f.id === keep);
    setState({
      report: { status: "ready", report, scanning: false, scanError: null },
      finding: stillThere ? keep : report.findings[0]?.id ?? null,
    });
    loadConnections();
  } catch (e) {
    if (getState().active !== name || getState().windowMinutes !== windowMinutes) return;
    const prev = getState().report;
    if (prev.status === "ready") setState({ report: { ...prev, scanning: false, scanError: errorMessage(e) } });
    else setState({ report: { status: "error", error: errorMessage(e) } });
  }
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
