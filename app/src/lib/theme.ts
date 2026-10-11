import { useSyncExternalStore } from "react";

export type Theme = "system" | "light" | "dark";

const KEY = "culprit.theme";

function read(): Theme {
  try {
    const v = localStorage.getItem(KEY);
    return v === "light" || v === "dark" ? v : "system";
  } catch {
    return "system";
  }
}

function apply(theme: Theme): void {
  if (theme === "system") delete document.documentElement.dataset.theme;
  else document.documentElement.dataset.theme = theme;
  try {
    if (theme === "system") localStorage.removeItem(KEY);
    else localStorage.setItem(KEY, theme);
  } catch {}
}

// One theme for the whole app: every control that shows or changes it reads the same value.
let current: Theme = read();
const listeners = new Set<() => void>();

function subscribe(listener: () => void): () => void {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

export function setTheme(next: Theme): void {
  if (next === current) return;
  current = next;
  apply(next);
  listeners.forEach((l) => l());
}

export function useTheme(): [Theme, () => void, (theme: Theme) => void] {
  const theme = useSyncExternalStore(subscribe, () => current, () => current);
  const cycle = () => setTheme(theme === "system" ? "light" : theme === "light" ? "dark" : "system");
  return [theme, cycle, setTheme];
}
