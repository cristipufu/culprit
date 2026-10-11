import { useState } from "react";

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

export function useTheme(): [Theme, () => void] {
  const [theme, setTheme] = useState<Theme>(read);
  const cycle = () => {
    const next: Theme = theme === "system" ? "light" : theme === "light" ? "dark" : "system";
    apply(next);
    setTheme(next);
  };
  return [theme, cycle];
}
