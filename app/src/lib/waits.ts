import { useSyncExternalStore } from "react";
import type { Signal } from "../gen/Signal";
import type { WaitSample } from "../gen/WaitSample";
import type { WaitStat } from "../gen/WaitStat";
import { api, errorMessage, type ApiError } from "./api";
import { bytes, count, duration, percentValue } from "./format";

/** The shared wait categories, in the same order and words as the Rust side (culprit_core::waits). */
export const WAIT_LABEL: Record<string, string> = {
  cpu: "CPU",
  io: "Disk reads",
  rows: "Row access",
  lock: "Locks",
  latch: "In-memory contention",
  log: "Log writes",
  network: "Sending results",
  parallelism: "Parallel queries",
  memory: "Memory",
  compilation: "Compiling plans",
  other: "Everything else",
};

/** Fold an engine's category name into a shared key (mirrors `category_key` in Rust). */
export function waitKey(category: string): string {
  switch (category.toLowerCase()) {
    case "cpu":
    case "worker thread":
    case "running":
      return "cpu";
    case "io":
    case "buffer io":
    case "other disk io":
    case "disk":
      return "io";
    case "rows":
    case "table io":
      return "rows";
    case "lock":
      return "lock";
    case "latch":
    case "buffer latch":
    case "lwlock":
    case "bufferpin":
      return "latch";
    case "log":
    case "tran log io":
    case "log rate governor":
    case "wal":
      return "log";
    case "network":
    case "network io":
    case "client":
      return "network";
    case "parallelism":
      return "parallelism";
    case "memory":
      return "memory";
    case "compilation":
      return "compilation";
    default:
      return "other";
  }
}

/** Each category's color, as a CSS variable so light and dark themes both work. */
export function waitColor(key: string): string {
  return `var(--wait-${key in WAIT_LABEL ? key : "other"})`;
}

export type FoldedWait = { key: string; label: string; ms: number; share: number; top: string | null };

/** Raw waits folded into the shared categories, biggest first, with shares of the total. */
export function foldWaits(waits: WaitStat[]): FoldedWait[] {
  const by = new Map<string, { ms: number; top: string | null; topMs: number }>();
  for (const w of waits) {
    const key = waitKey(w.category);
    const entry = by.get(key) ?? { ms: 0, top: null, topMs: 0 };
    entry.ms += w.wait_ms;
    if (w.wait_type && w.wait_ms >= entry.topMs) {
      entry.top = w.wait_type;
      entry.topMs = w.wait_ms;
    }
    by.set(key, entry);
  }
  const total = [...by.values()].reduce((s, x) => s + x.ms, 0);
  return [...by.entries()]
    .filter(([, x]) => x.ms > 0)
    .map(([key, x]) => ({ key, label: WAIT_LABEL[key] ?? key, ms: x.ms, share: total > 0 ? x.ms / total : 0, top: x.top }))
    .sort((a, b) => b.ms - a.ms);
}

/** A signal's value in words: "62%", "0.1/s", "1.2 GB", "14 s". */
export function signalValue(s: Signal): string {
  switch (s.unit) {
    case "percent":
      return percentValue(s.value);
    case "per-second":
      // Under one every ten seconds, a per-minute rate says more than "0.0/s".
      return s.value > 0 && s.value < 0.1 ? `${count(s.value * 60)}/min` : `${count(s.value)}/s`;
    case "per-hour":
      return `${count(s.value)}/h`;
    case "count":
      return count(s.value);
    case "bytes":
      return bytes(s.value);
    case "milliseconds":
      return duration(s.value);
    case "seconds":
      return duration(s.value * 1000);
  }
}

export const SIGNAL_SCOPE: Record<Signal["scope"], string> = { now: "when scanned", "since-startup": "since start", sampled: "sampled", "last-hour": "available recent history", "query-history": "retained query history", window: "selected period" };

// ── the "right now" sample keeps running while the panel is closed ────────────────────────────────

export type SampleState =
  | { status: "idle" }
  | { status: "running"; seconds: number; startedAt: number }
  | { status: "done"; sample: WaitSample }
  | { status: "error"; error: ApiError };

const samples = new Map<string, SampleState>();
const listeners = new Set<() => void>();

function emit() {
  listeners.forEach((l) => l());
}

function subscribe(listener: () => void) {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

const IDLE: SampleState = { status: "idle" };

export function useSample(connection: string): SampleState {
  return useSyncExternalStore(subscribe, () => samples.get(connection) ?? IDLE);
}

export async function startSample(connection: string, seconds: number): Promise<void> {
  if (samples.get(connection)?.status === "running") return;
  samples.set(connection, { status: "running", seconds, startedAt: Date.now() });
  emit();
  try {
    const sample = await (await api()).waitSample(connection, seconds);
    samples.set(connection, { status: "done", sample });
  } catch (e) {
    samples.set(connection, { status: "error", error: errorMessage(e) });
  }
  emit();
}
