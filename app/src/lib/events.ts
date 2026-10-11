import { useEffect, useState, useSyncExternalStore } from "react";
import type { ChangeEvent } from "../gen/ChangeEvent";
import type { CloudSignals } from "../gen/CloudSignals";
import type { EventKind } from "../gen/EventKind";
import type { EventLog } from "../gen/EventLog";
import type { EventsRequest } from "../gen/EventsRequest";
import { api, errorMessage, type ApiError } from "./api";

/** What each kind of change is called in a legend or a chip. */
export const KIND_LABEL: Record<EventKind, string> = {
  mark: "Mark",
  index: "Index",
  table: "Table",
  code: "Code",
  setting: "Setting",
  version: "Upgrade",
  restart: "Restart",
  "stats-reset": "Stats reset",
  statistics: "Statistics",
  throttle: "Cloud",
};

const clock = new Intl.DateTimeFormat(undefined, { hour: "2-digit", minute: "2-digit" });
const dayClock = new Intl.DateTimeFormat(undefined, { weekday: "short", hour: "2-digit", minute: "2-digit" });
const dateClock = new Intl.DateTimeFormat(undefined, { month: "short", day: "numeric", hour: "2-digit", minute: "2-digit" });

/** "Tue 14:05" within the week, "Oct 3, 14:05" before. */
export function stamp(t: number): string {
  return Date.now() - t < 6 * 86_400_000 ? dayClock.format(t) : dateClock.format(t);
}

/** When an event happened, in local time and as exactly as its source knows. */
export function eventTime(e: ChangeEvent): string {
  const at = Date.parse(e.at);
  if (e.after) return `between the ${stamp(Date.parse(e.after))} and ${clock.format(at)} scans`;
  if (e.until) return `${stamp(at)}–${clock.format(Date.parse(e.until))}`;
  return stamp(at);
}

/** Earliest and latest moment it may have happened (or the episode's span), in ms. */
export function eventSpan(e: ChangeEvent): { from: number; to: number } {
  const at = Date.parse(e.at);
  return { from: e.after ? Date.parse(e.after) : at, to: e.until ? Date.parse(e.until) : at };
}

// Marks change from several places (the chart, the palette, What changed); anything showing events refetches.
let version = 0;
const listeners = new Set<() => void>();
export function eventsChanged(): void {
  version += 1;
  listeners.forEach((l) => l());
}
export function useEventsVersion(): number {
  return useSyncExternalStore(
    (l) => {
      listeners.add(l);
      return () => listeners.delete(l);
    },
    () => version,
  );
}

export function eventsRequest(fields: Partial<EventsRequest> = {}): EventsRequest {
  return { start: null, end: null, hours: null, cloud: null, refresh: false, ...fields };
}

/** Events between `from` and `to` (ms). Saved scans and marks answer at once; cloud metrics follow when they arrive. */
export function useEvents(connection: string | null, from: number, to: number): EventLog | null {
  const [log, setLog] = useState<EventLog | null>(null);
  const v = useEventsVersion();
  // Minutes are precise enough for a chart, and keep one fetch per render burst.
  const start = Number.isFinite(from) ? new Date(Math.floor(from / 60_000) * 60_000).toISOString() : null;
  const end = Number.isFinite(to) ? new Date(Math.ceil(to / 60_000) * 60_000).toISOString() : null;
  useEffect(() => {
    if (!connection || !start || !end || start >= end) return;
    let cancelled = false;
    const load = (cloud: boolean) =>
      api()
        .then((client) => client.events(connection, eventsRequest({ start, end, cloud })))
        .then((next) => !cancelled && setLog(next))
        .catch(() => {});
    // Local history first, then again with the cloud's metrics (seconds on the first call, cached after).
    void load(false).then(() => load(true));
    return () => {
      cancelled = true;
    };
  }, [connection, start, end, v]);
  return log;
}

export type CloudState = { status: "loading" } | { status: "ready"; signals: CloudSignals } | { status: "none" } | { status: "error"; error: ApiError };

/** What the cloud measured over the last `hours`; `none` when the connection isn't a cloud database. */
export function useCloud(connection: string, hours: number, refresh: number): CloudState {
  const [state, setState] = useState<CloudState>({ status: "loading" });
  useEffect(() => {
    let cancelled = false;
    setState((s) => (s.status === "ready" ? s : { status: "loading" }));
    api()
      .then((client) => client.cloud(connection, eventsRequest({ hours, refresh: refresh > 0 })))
      .then((signals) => !cancelled && setState({ status: "ready", signals }))
      .catch((e) => {
        if (cancelled) return;
        const error = errorMessage(e);
        setState(error.code === "unsupported" ? { status: "none" } : { status: "error", error });
      });
    return () => {
      cancelled = true;
    };
  }, [connection, hours, refresh]);
  return state;
}
