import { useSyncExternalStore } from "react";
import { api, errorMessage, type UpdateInfo } from "./api";
import { toast } from "./store";

// Whether the app checks once a day by itself (on unless turned off), and when a check last found nothing new.
const AUTO_KEY = "culprit.updates.auto";
const LAST_KEY = "culprit.updates.lastCheck";
const DAY_MS = 24 * 60 * 60 * 1000;
// Well after the first paint, so a check never competes with start-up.
const FIRST_CHECK_MS = 8_000;
const TICK_MS = 60 * 60 * 1000;

export type UpdateState =
  | { status: "idle" }
  | { status: "checking"; manual: boolean }
  | { status: "available"; info: UpdateInfo }
  | { status: "downloading"; info: UpdateInfo; progress: number | null }
  | { status: "ready"; info: UpdateInfo }
  | { status: "failed"; info: UpdateInfo; error: string };

let state: UpdateState = { status: "idle" };
const listeners = new Set<() => void>();

function set(next: UpdateState): void {
  state = next;
  listeners.forEach((listener) => listener());
}

function subscribe(listener: () => void): () => void {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

/** The update the app knows about, and how far its install has got. */
export function useUpdate(): UpdateState {
  return useSyncExternalStore(subscribe, () => state);
}

function read(key: string): string | null {
  try {
    return localStorage.getItem(key);
  } catch {
    return null;
  }
}

function write(key: string, value: string | null): void {
  try {
    if (value === null) localStorage.removeItem(key);
    else localStorage.setItem(key, value);
  } catch {}
}

export function autoCheckEnabled(): boolean {
  return read(AUTO_KEY) !== "off";
}

export function useAutoCheck(): boolean {
  return useSyncExternalStore(subscribe, autoCheckEnabled);
}

export function setAutoCheck(on: boolean): void {
  write(AUTO_KEY, on ? null : "off");
  listeners.forEach((listener) => listener());
}

/**
 * Asks whether a newer release is out. A manual check says what it found; the daily one stays quiet unless there is
 * an update. Only a check that found nothing counts towards the day: one that found an update, or couldn't reach the
 * releases, is asked again, so an update offered yesterday is still offered after a restart.
 */
export async function checkForUpdates(manual: boolean): Promise<void> {
  if (state.status === "downloading" || state.status === "ready") {
    if (manual) toast(state.status === "ready" ? `Restart to finish updating to ${state.info.version}` : "The update is downloading");
    return;
  }
  if (state.status === "checking") return;
  const before = state;
  set({ status: "checking", manual });
  try {
    const info = await (await api()).checkUpdate();
    if (info) {
      set({ status: "available", info });
      if (manual) toast(`Culprit ${info.version} is available`);
    } else {
      write(LAST_KEY, String(Date.now()));
      set({ status: "idle" });
      if (manual) toast("Culprit is up to date");
    }
  } catch (e) {
    set(before);
    if (manual) toast(errorMessage(e).message);
  }
}

/** Downloads, verifies and installs the update; the app keeps running until the person restarts it. */
export async function installUpdate(): Promise<void> {
  if (state.status !== "available" && state.status !== "failed") return;
  const info = state.info;
  set({ status: "downloading", info, progress: null });
  const client = await api();
  const stop = await client.onUpdateProgress((p) => {
    if (state.status === "downloading") set({ ...state, progress: p.total ? Math.min(1, p.downloaded / p.total) : null });
  });
  try {
    await client.installUpdate();
    set({ status: "ready", info });
  } catch (e) {
    set({ status: "failed", info, error: errorMessage(e).message });
  } finally {
    stop();
  }
}

export async function restartToUpdate(): Promise<void> {
  await (await api()).restartApp();
}

/** The daily check: first a few seconds after start, then on the hour, whenever a day has passed since the last. */
export function startUpdateChecks(): () => void {
  const due = () => autoCheckEnabled() && Date.now() - Number(read(LAST_KEY) ?? 0) >= DAY_MS;
  const run = () => {
    if (due()) void checkForUpdates(false);
  };
  const first = setTimeout(run, FIRST_CHECK_MS);
  const tick = setInterval(run, TICK_MS);
  return () => {
    clearTimeout(first);
    clearInterval(tick);
  };
}
