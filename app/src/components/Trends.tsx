import { useMemo, useState, type MouseEvent } from "react";
import type { QueryStat } from "../gen/QueryStat";
import type { Timeline } from "../gen/Timeline";
import type { Workload } from "../gen/Workload";
import { count, duration } from "../lib/format";

/** Charts are drawn in a fixed coordinate space and stretched; text lives in HTML so it never distorts. */
const VW = 1000;
const VH = 100;

type Bucket = { start: number; end: number };

function buckets(t: Timeline): Bucket[] {
  const end = Date.parse(t.end);
  return t.starts.map((s, i) => {
    const next = t.starts[i + 1];
    return { start: Date.parse(s), end: next ? Date.parse(next) : end };
  });
}

function span(b: Bucket[]): [number, number] {
  return [b[0]?.start ?? 0, b[b.length - 1]?.end ?? 1];
}

const utc = (options: Intl.DateTimeFormatOptions) => new Intl.DateTimeFormat(undefined, { timeZone: "UTC", ...options });
const timeFmt = utc({ hour: "2-digit", minute: "2-digit", hour12: false });
const dayFmt = utc({ month: "short", day: "numeric" });

/** Axis label: a clock time for spans up to a day and a half, a date beyond that. */
function tick(ms: number, spanHours: number): string {
  return spanHours <= 36 ? timeFmt.format(ms) : dayFmt.format(ms);
}

function range(b: Bucket | undefined, spanHours: number): string {
  if (!b) return "";
  const day = spanHours > 20 ? `${dayFmt.format(b.start)} ` : "";
  return `${day}${timeFmt.format(b.start)}–${timeFmt.format(b.end)} UTC`;
}

/** Index of the bucket under the pointer. */
function useHover(b: Bucket[]) {
  const [hover, setHover] = useState<number | null>(null);
  const [from, to] = span(b);
  const onMove = (e: MouseEvent<HTMLElement>) => {
    const box = e.currentTarget.getBoundingClientRect();
    const at = from + ((e.clientX - box.left) / box.width) * (to - from);
    const i = b.findIndex((x) => at >= x.start && at < x.end);
    setHover(i >= 0 ? i : null);
  };
  return { hover, onMove, onLeave: () => setHover(null) };
}

/** Why there is no trend, in one sentence. */
export function trendNote(workload: Workload): string | null {
  if (workload.timeline && workload.timeline.starts.length >= 2) return null;
  if (workload.windowed) return null;
  return `${workload.source} keeps running totals only, so Culprit draws trends from its own scans. Scan again later to see one.`;
}

/** Database time per hour across the window, as steps: each bucket is drawn exactly as wide as the time it covers. */
export function WorkloadTrend({ timeline }: { timeline: Timeline }) {
  const b = useMemo(() => buckets(timeline), [timeline]);
  const { hover, onMove, onLeave } = useHover(b);
  if (b.length < 2) return null;
  const [from, to] = span(b);
  const hours = (to - from) / 3_600_000;
  const rate = b.map((x, i) => (timeline.time_ms[i] ?? 0) / Math.max((x.end - x.start) / 3_600_000, 1 / 60));
  const max = Math.max(...rate, 1e-9);
  const x = (t: number) => ((t - from) / (to - from)) * VW;
  const y = (v: number) => VH - (v / max) * (VH - 6);
  let line = "";
  b.forEach((bucket, i) => {
    const v = y(rate[i] ?? 0);
    line += `${i === 0 ? "M" : "L"}${x(bucket.start)},${v}L${x(bucket.end)},${v}`;
  });
  const area = `${line}L${x(to)},${VH}L${x(from)},${VH}Z`;
  const peak = rate.indexOf(Math.max(...rate));
  const shown = hover ?? peak;
  return (
    <div className="trend">
      <div className="trend-head">
        <span className="label">Database time per hour</span>
        <span className="readout">
          {hover == null && <span className="muted">peak </span>}
          <b>{duration(rate[shown])}</b>
          <span className="muted">
            {" "}
            · {count(timeline.executions[shown])} runs · {range(b[shown], hours)}
          </span>
        </span>
      </div>
      <div className="trend-plot" style={{ height: 52 }} onMouseMove={onMove} onMouseLeave={onLeave}>
        <svg viewBox={`0 0 ${VW} ${VH}`} preserveAspectRatio="none" aria-label="Database time per hour">
          <path className="trend-area" d={area} />
          <path className="trend-line" d={line} />
        </svg>
        <Cursor bucket={hover != null ? b[hover] : undefined} x={x} />
      </div>
      <div className="trend-axis">
        <span>{tick(from, hours)}</span>
        <span>{timeline.source === "scans" ? `between ${b.length + 1} scans` : ""}</span>
        <span>{tick(to, hours)}</span>
      </div>
    </div>
  );
}

function Cursor({ bucket, x }: { bucket: Bucket | undefined; x: (t: number) => number }) {
  if (!bucket) return null;
  const left = (x(bucket.start) / VW) * 100;
  const width = ((x(bucket.end) - x(bucket.start)) / VW) * 100;
  return <div className="trend-cursor" style={{ left: `${left}%`, width: `max(1px, ${width}%)` }} />;
}

/** Average duration per run, bucket by bucket; gaps where the query didn't run or the source can't tell. */
function meanPoints(q: QueryStat, b: Bucket[]): (number | null)[] {
  const t = q.trend;
  if (!t) return [];
  return b.map((_, i) => {
    const time = t.time_ms[i];
    const runs = t.executions[i];
    return time != null && runs != null && runs > 0 ? time / runs : null;
  });
}

function linePath(points: (number | null)[], b: Bucket[], x: (t: number) => number, y: (v: number) => number): string {
  let d = "";
  let open = false;
  points.forEach((v, i) => {
    const bucket = b[i];
    if (v == null || !bucket) {
      open = false;
      return;
    }
    const mid = (bucket.start + bucket.end) / 2;
    d += `${open ? "L" : "M"}${x(mid).toFixed(1)},${y(v).toFixed(1)}`;
    open = true;
  });
  return d;
}

/** A tiny line of the query's average duration over the window, for the Queries table. */
export function Sparkline({ query, timeline, alert }: { query: QueryStat; timeline: Timeline | null | undefined; alert?: boolean }) {
  const b = useMemo(() => (timeline ? buckets(timeline) : []), [timeline]);
  const points = useMemo(() => meanPoints(query, b), [query, b]);
  const known = points.filter((p): p is number => p != null);
  if (known.length < 2) return <span className="spark-empty">—</span>;
  const [from, to] = span(b);
  const max = Math.max(...known);
  const x = (t: number) => ((t - from) / (to - from)) * VW;
  const y = (v: number) => VH - 8 - (v / max) * (VH - 16);
  return (
    <svg className={`spark ${alert ? "alert" : ""}`} viewBox={`0 0 ${VW} ${VH}`} preserveAspectRatio="none" aria-label="Average duration over time">
      <path d={linePath(points, b, x, y)} />
    </svg>
  );
}

/** The query's average duration per run (line) and how often it ran (bars), with markers where a new plan appeared. */
export function QueryTrend({ query, timeline }: { query: QueryStat; timeline: Timeline }) {
  const b = useMemo(() => buckets(timeline), [timeline]);
  const points = useMemo(() => meanPoints(query, b), [query, b]);
  const { hover, onMove, onLeave } = useHover(b);
  const known = points.filter((p): p is number => p != null);
  if (b.length < 2 || known.length < 2) return null;
  const [from, to] = span(b);
  const hours = (to - from) / 3_600_000;
  const runs = b.map((_, i) => query.trend?.executions[i] ?? null);
  const runRate = b.map((x, i) => (runs[i] ?? 0) / Math.max((x.end - x.start) / 3_600_000, 1 / 60));
  const maxMean = Math.max(...known);
  const maxRate = Math.max(...runRate, 1e-9);
  const x = (t: number) => ((t - from) / (to - from)) * VW;
  const y = (v: number) => VH - 4 - (v / maxMean) * (VH - 14);
  const firstPlan = Math.min(...query.plans.map((p) => (p.first_seen ? Date.parse(p.first_seen) : Infinity)));
  const markers = query.plans
    .filter((p) => p.first_seen && Date.parse(p.first_seen) > from && Date.parse(p.first_seen) < to && Date.parse(p.first_seen) > firstPlan)
    .map((p) => ({ id: p.id, at: Date.parse(p.first_seen!) }));
  const shown = hover;
  return (
    <section>
      <h3>Over time</h3>
    <div className="trend">
      <div className="trend-head">
        <span className="legend">
          <i className="key-line" /> average per run <i className="key-bar" /> runs
        </span>
        <span className="readout">
          {shown != null ? (
            <>
              <b>{points[shown] != null ? duration(points[shown]) : "—"}</b>
              <span className="muted">
                {" "}
                · {runs[shown] != null ? `${count(runs[shown])} runs` : "not tracked"} · {range(b[shown], hours)}
              </span>
            </>
          ) : (
            <span className="muted">
              {duration(Math.min(...known))} – {duration(maxMean)} per run
            </span>
          )}
        </span>
      </div>
      <div className="trend-plot" style={{ height: 120 }} onMouseMove={onMove} onMouseLeave={onLeave}>
        <svg viewBox={`0 0 ${VW} ${VH}`} preserveAspectRatio="none" aria-label="Average duration and runs over time">
          {b.map((bucket, i) => {
            const h = ((runRate[i] ?? 0) / maxRate) * (VH * 0.42);
            const x0 = x(bucket.start);
            const w = Math.max(x(bucket.end) - x0 - 2, 1);
            return runs[i] == null ? null : <rect key={i} className="trend-bar" x={x0 + 1} y={VH - h} width={w} height={h} />;
          })}
          <path className="trend-line" d={linePath(points, b, x, y)} />
        </svg>
        {markers.map((m) => (
          <div key={m.id} className="trend-marker" style={{ left: `${(x(m.at) / VW) * 100}%` }}>
            <span>Plan {m.id}</span>
          </div>
        ))}
        <Cursor bucket={hover != null ? b[hover] : undefined} x={x} />
      </div>
      <div className="trend-axis">
        <span>{tick(from, hours)}</span>
        <span>{timeline.source === "scans" ? "between Culprit's scans" : "Query Store"}</span>
        <span>{tick(to, hours)}</span>
      </div>
    </div>
    </section>
  );
}
