import type { Unit } from "../gen/Unit";

// Every number here reads as culprit_core::format writes it in findings and briefs, so a value reads the same on
// every surface. `bun run typecheck` compares them on the cases Rust writes to gen/format-cases.json (format.check.ts).

/** `x.toFixed(digits)` rounded as Rust's `format!("{:.N}")` rounds: an exact tie goes to the even digit (2.5 → "2",
 * 0.125 → "0.12", 12.5% → "12%"), where toFixed rounds it up. Exact ties are the values that are an odd number of
 * 1/2^(digits+1); every other value rounds the same in both. */
function fixed(x: number, digits: number): string {
  const text = x.toFixed(digits);
  const halves = Math.abs(x) * 2 ** (digits + 1);
  if (!Number.isInteger(halves) || halves % 2 !== 1) return text;
  const scale = 10 ** digits;
  const up = Math.round(Math.abs(Number(text)) * scale);
  if (up % 2 === 0) return text;
  return `${x < 0 ? "-" : ""}${((up - 1) / scale).toFixed(digits)}`;
}

/** Rust's `f64::round`: half away from zero (`Math.round` rounds -2.5 to -2). */
function roundHalfAway(x: number): number {
  return Math.sign(x) * Math.round(Math.abs(x));
}

export function duration(ms: number | null | undefined): string {
  if (ms == null) return "—";
  const v = Math.max(0, ms);
  if (v === 0) return "0 ms";
  // Under a hundredth of a millisecond, "0.00 ms" says nothing: microseconds do.
  if (v < 0.01) return `${fixed(Math.max(1, v * 1000), 0)} µs`;
  if (v < 1) return `${fixed(v, 2)} ms`;
  // A tenth under 10 ms: rounding 2.59 ms to "3 ms" is off by 16%. A whole number reads without its ".0".
  if (v < 9.95) {
    const tenths = fixed(v, 1);
    return `${tenths.endsWith(".0") ? tenths.slice(0, -2) : tenths} ms`;
  }
  if (v < 1000) return `${fixed(v, 0)} ms`;
  const s = v / 1000;
  if (s < 60) return `${fixed(s, 1)} s`;
  const m = s / 60;
  if (m < 60) return `${fixed(m, 0)} min`;
  const h = m / 60;
  if (h < 48) {
    const whole = Math.floor(h);
    const minutes = roundHalfAway((h - whole) * 60);
    return minutes >= 1 ? `${whole} h ${minutes} min` : `${whole} h`;
  }
  return `${fixed(h / 24, 1)} days`;
}

/** Days counted, never more than were: a tenth under 10 (9.51 days are "9.5", not "10"), whole days from 10 on.
 * Mirrors culprit_core::format::days_counted. */
export function daysCounted(value: number): string {
  // A hair under a whole number from adding hours is that number.
  const d = value + 1e-9;
  if (d < 10) {
    const tenths = fixed(Math.floor(d * 10) / 10, 1);
    return tenths.endsWith(".0") ? tenths.slice(0, -2) : tenths;
  }
  return `${Math.floor(d)}`;
}

/** A span of days as a metric says it: "9.5 days", "1 day", and under a day hours and minutes ("9 h 36 min"), never
 * "0 days". Mirrors culprit_core::format::days. */
export function days(value: number): string {
  if (value < 1) return duration(Math.max(0, value) * 86_400_000);
  const n = daysCounted(value);
  return n === "1" ? "1 day" : `${n} days`;
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
  if (unit === 0 || v >= 100) return `${fixed(v, 0)} ${units[unit]}`;
  return `${fixed(v, 1)} ${units[unit]}`;
}

export function count(value: number | null | undefined): string {
  if (value == null) return "—";
  // An empty sum is -0; it reads as nothing, not as a negative.
  const n = value === 0 ? 0 : value;
  const v = Math.abs(n);
  if (v >= 1e9) return `${fixed(n / 1e9, 1)}B`;
  if (v >= 1e6) return `${fixed(n / 1e6, 1)}M`;
  if (v >= 1e4) return `${fixed(n / 1e3, 0)}K`;
  if (v >= 1e3) return `${fixed(n / 1e3, 1)}K`;
  if (v >= 100 || Number.isInteger(n)) return fixed(n, 0);
  return fixed(n, 1);
}

/** "1 row", "12 rows", "1.2K rows"; "— rows" when unknown. */
export function rowCount(value: number | null | undefined): string {
  const n = count(value);
  return `${n} ${n === "1" ? "row" : "rows"}`;
}

export function percent(ratio: number | null | undefined): string {
  if (ratio == null) return "—";
  const p = ratio * 100;
  if (p > 0 && p < 1) return "<1%";
  return `${fixed(p, 0)}%`;
}

/** A measured percentage on a 0–100 scale. Mirrors culprit_core::format::percent_value. */
export function percentValue(value: number | null | undefined): string {
  if (value == null || !Number.isFinite(value)) return "—";
  if (value === 0) return "0%";
  if (Math.abs(value) < 0.01) return value > 0 ? "<0.01%" : ">-0.01%";
  return `${fixed(value, 2).replace(/0+$/, "").replace(/\.$/, "")}%`;
}

/** A total wait to the second under an hour: "1 min 29 s", where `duration` would round 88,960 ms down to "1 min" next to
 * a title that says otherwise, and 152 s up to "3 min". Mirrors culprit_checks::signals::total_wait. */
export function totalWait(ms: number): string {
  const seconds = Math.round(ms / 1000);
  if (seconds < 60 || seconds >= 3600) return duration(ms);
  const minutes = Math.floor(seconds / 60);
  const rest = seconds % 60;
  return rest === 0 ? `${minutes} min` : `${minutes} min ${rest} s`;
}

/** Metrics that total a wait, said to the second as their findings' titles say them. Mirrors
 * culprit_checks::TOTAL_WAIT_METRICS. */
const TOTAL_WAIT_METRICS = new Set(["Time waiting", "Time waiting since start"]);

/** A finding's metric as its Evidence says it. Mirrors culprit_checks::metric_text. */
export function findingMetric(m: { label: string; value: number; unit: Unit; text?: string | null }): string {
  return m.unit === "ms" && TOTAL_WAIT_METRICS.has(m.label) ? totalWait(m.value) : metric(m.value, m.unit, m.text);
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
      return days(value);
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
