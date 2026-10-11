import type { Unit } from "../gen/Unit";

export function duration(ms: number | null | undefined): string {
  if (ms == null) return "—";
  const v = Math.max(0, ms);
  if (v < 1) return `${v.toFixed(2)} ms`;
  if (v < 1000) return `${Math.round(v)} ms`;
  const s = v / 1000;
  if (s < 60) return `${s.toFixed(1)} s`;
  const m = s / 60;
  if (m < 60) return `${Math.round(m)} min`;
  const h = m / 60;
  if (h < 48) {
    const whole = Math.floor(h);
    const minutes = Math.round((h - whole) * 60);
    return minutes >= 1 ? `${whole} h ${minutes} min` : `${whole} h`;
  }
  return `${(h / 24).toFixed(1)} days`;
}

export function bytes(value: number | null | undefined): string {
  if (value == null) return "—";
  const units = ["B", "KB", "MB", "GB", "TB"];
  let v = Math.max(0, value);
  let unit = 0;
  while (v >= 1024 && unit < units.length - 1) {
    v /= 1024;
    unit++;
  }
  if (unit === 0 || v >= 100) return `${Math.round(v)} ${units[unit]}`;
  return `${v.toFixed(1)} ${units[unit]}`;
}

export function count(value: number | null | undefined): string {
  if (value == null) return "—";
  const v = Math.abs(value);
  if (v >= 1e9) return `${(value / 1e9).toFixed(1)}B`;
  if (v >= 1e6) return `${(value / 1e6).toFixed(1)}M`;
  if (v >= 1e4) return `${Math.round(value / 1e3)}K`;
  if (v >= 1e3) return `${(value / 1e3).toFixed(1)}K`;
  if (v >= 100 || Number.isInteger(value)) return `${Math.round(value)}`;
  return value.toFixed(1);
}

export function percent(ratio: number | null | undefined): string {
  if (ratio == null) return "—";
  const p = ratio * 100;
  if (p > 0 && p < 1) return "<1%";
  return `${Math.round(p)}%`;
}

export function metric(value: number, unit: Unit, text?: string | null): string {
  switch (unit) {
    case "ms":
      return duration(value);
    case "bytes":
      return bytes(value);
    case "ratio":
      return percent(value);
    case "days":
      return value < 1 ? `${Math.round(value * 24)} h` : `${Math.round(value)} days`;
    case "text":
      return text ?? "";
    default:
      return count(value);
  }
}

export function ago(iso: string | null | undefined, now = Date.now()): string {
  if (!iso) return "never";
  const t = Date.parse(iso);
  if (Number.isNaN(t)) return iso;
  const s = Math.max(0, (now - t) / 1000);
  if (s < 45) return "just now";
  if (s < 3600) return `${Math.round(s / 60)} min ago`;
  if (s < 86400) return `${Math.round(s / 3600)} h ago`;
  return `${Math.round(s / 86400)} days ago`;
}

/** A moment as people say it: "today 12:13", "yesterday 09:02", "Oct 7, 12:13", "Oct 7, 2025". `full` is for tooltips. */
export function when(iso: string | null | undefined, now = new Date()): { short: string; full: string } {
  if (!iso) return { short: "", full: "" };
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return { short: iso, full: iso };
  const time = d.toLocaleTimeString([], { hour: "numeric", minute: "2-digit" });
  const day = (x: Date) => new Date(x.getFullYear(), x.getMonth(), x.getDate()).getTime();
  const days = Math.round((day(now) - day(d)) / 86_400_000);
  const short =
    days === 0
      ? `today ${time}`
      : days === 1
        ? `yesterday ${time}`
        : d.getFullYear() === now.getFullYear()
          ? `${d.toLocaleDateString([], { month: "short", day: "numeric" })}, ${time}`
          : d.toLocaleDateString([], { month: "short", day: "numeric", year: "numeric" });
  return { short, full: d.toLocaleString([], { dateStyle: "full", timeStyle: "short" }) };
}

export function windowLabel(hours: number): string {
  const unit = (n: number, word: string) => `last ${n} ${word}${n === 1 ? "" : "s"}`;
  if (hours >= 47) return unit(Math.round(hours / 24), "day");
  if (hours >= 1.5) return unit(Math.round(hours), "hour");
  return unit(Math.max(1, Math.round(hours * 60)), "minute");
}

export function oneLine(sql: string): string {
  return sql.replace(/\s+/g, " ").trim();
}
