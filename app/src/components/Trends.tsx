import { useMemo, type ReactNode } from "react";
import type { QueryStat } from "../gen/QueryStat";
import type { Timeline } from "../gen/Timeline";
import type { Workload } from "../gen/Workload";
import { count, duration } from "../lib/format";
import { isPartial, TimeChart, VH, VW, when, type Bucket, type ChartEvent, type Flag, type Range } from "./TimeChart";

/** Colors that tell plans apart, the same in chips, flags and lanes. */
export const PLAN_COLORS = ["var(--focus)", "#8b5cf6", "#0f9f95", "#d97706", "#db2777", "#64748b"];

export function planColor(index: number): string {
  return PLAN_COLORS[index % PLAN_COLORS.length]!;
}

function bucketsOf(t: Timeline, windowStart?: string | null): Bucket[] {
  const end = Date.parse(t.end);
  const start = windowStart ? Date.parse(windowStart) : NaN;
  return t.starts.map((s, i) => {
    const next = t.starts[i + 1];
    const b: Bucket = { start: Date.parse(s), end: next ? Date.parse(next) : end, unknown: t.unknown?.[i] === true };
    // The window can begin inside the first bucket: that bucket only holds part of its time.
    if (i === 0 && Number.isFinite(start) && start > b.start && start < b.end) b.covered = (b.end - start) / (b.end - b.start);
    return b;
  });
}

/** Per hour when buckets are at least an hour wide, per minute otherwise: a rate never stretches a sliver into an hour. */
function rateUnit(b: Bucket[]): { ms: number; label: string } {
  const widths = b.map((x) => x.end - x.start).sort((a, c) => a - c);
  const median = widths[Math.floor(widths.length / 2)] ?? 0;
  return median >= 3_600_000 ? { ms: 3_600_000, label: "hour" } : { ms: 60_000, label: "minute" };
}

function coveredMs(b: Bucket): number {
  return (b.end - b.start) * (b.covered ?? 1);
}

/** Why there is no trend, in one sentence. */
export function trendNote(workload: Workload): string | null {
  if (workload.timeline && workload.timeline.starts.length >= 2) return null;
  if (workload.windowed) return null;
  return `${workload.source} keeps running totals only, so Culprit draws trends from its own scans. Scan again later to see one.`;
}

/** Plan changes worth a flag on the workload chart: new plans of the queries that matter most. */
export function planFlags(workload: Workload, onSince?: (at: number) => void): Flag[] {
  const total = workload.total_time_ms || 1;
  return workload.queries
    .filter((q) => q.plan_history?.kind !== "cache" && q.plans.length >= 2 && q.total_time_ms / total >= 0.02)
    .flatMap((q) => {
      const first = Math.min(...q.plans.map((p) => (p.first_seen ? Date.parse(p.first_seen) : Infinity)));
      return q.plans
        .filter((p) => p.first_seen && Date.parse(p.first_seen) > first)
        .map((p) => ({ at: Date.parse(p.first_seen!), label: `Plan ${p.id}`, share: q.total_time_ms / total }));
    })
    .sort((a, b) => b.share - a.share)
    .slice(0, 5)
    .map((f) => ({ at: f.at, label: f.label, tone: "plan" as const, onClick: onSince ? () => onSince(f.at) : undefined }));
}

type WorkloadTrendProps = {
  timeline: Timeline;
  windowStart?: string | null;
  flags?: Flag[];
  /** Brushing a range compares it with the period before it. */
  onSelect?: (range: Range) => void;
  selection?: Range | null;
  onCompare?: () => void;
  /** What changed around the database, on a lane under the chart. */
  events?: ChartEvent[];
  /** More buttons in the chart's head (Mark…). */
  actions?: ReactNode;
};

/** Database time across the window as steps, each bucket exactly as wide as the time it covers. */
export function WorkloadTrend({ timeline, windowStart, flags, onSelect, selection, onCompare, events, actions }: WorkloadTrendProps) {
  const b = useMemo(() => bucketsOf(timeline, windowStart), [timeline, windowStart]);
  const unit = useMemo(() => rateUnit(b), [b]);
  const rate = useMemo(() => b.map((x, i) => ((timeline.time_ms[i] ?? 0) / Math.max(coveredMs(x), 1000)) * unit.ms), [b, timeline, unit]);
  // Data that starts mid-bucket: the first bucket far below the rest is a partial one, not a quiet hour.
  const buckets = useMemo(() => {
    if (b.length <= 4 || !b[0] || isPartial(b[0])) return b;
    const rest = [...rate.slice(1)].sort((p, q) => p - q);
    const median = rest[Math.floor(rest.length / 2)] ?? 0;
    return (rate[0] ?? 0) < median * 0.35 ? [{ ...b[0], partial: true }, ...b.slice(1)] : b;
  }, [b, rate]);
  if (buckets.length < 2) return null;
  // A bucket the source can't account for in full is no measurement: never the scale's top or the peak.
  const known = rate.map((r, i) => (buckets[i]?.unknown ? null : r));
  const max = Math.max(...known.map((r) => r ?? 0), 1e-9);
  const peakIndex = known.indexOf(max);
  const shown = (i: number) => Math.min(rate[i] ?? 0, max);
  return (
    <div className="trend">
      <div className="trend-head">
        <span className="label">Database time per {unit.label}</span>
        {(onCompare || actions) && (
          <span className="trend-actions">
            {onCompare && <span className="muted trend-hint">drag to compare</span>}
            {actions}
            {onCompare && (
              <button className="btn small" onClick={onCompare} title="Compare two periods (V)">
                What changed?
              </button>
            )}
          </span>
        )}
      </div>
      <TimeChart
        buckets={buckets}
        max={max}
        format={(v) => duration(v)}
        height={92}
        peak={peakIndex >= 0 ? { index: peakIndex, value: max } : null}
        flags={flags}
        events={events}
        onSelect={onSelect}
        selection={selection}
        ariaLabel={`Database time per ${unit.label}; use arrow keys to move, Shift+arrows to select a period, Enter to compare it`}
        footnote={
          timeline.source === "scans"
            ? `Between ${buckets.length + 1} of Culprit's scans: each step covers the time between two scans.`
            : timeline.source === "slow-log"
              ? "From the slow query log: statements faster than long_query_time aren't in it."
              : undefined
        }
        readout={(i) =>
          buckets[i]?.unknown ? (
            <>
              <b>not known in full</b>
              <span className="muted"> · entries left the source between these scans and took their totals with them</span>
            </>
          ) : (
            <>
              <b>{duration(rate[i] ?? 0)}</b> per {unit.label}
              <span className="muted">
                {" "}
                · {count(timeline.executions[i] ?? 0)} runs{buckets[i] && isPartial(buckets[i]!) ? " · partial" : ""}
              </span>
            </>
          )
        }
      >
        {({ x, y }) => {
          // Partial edge buckets and buckets not known in full are drawn dashed and left out of the area, so they
          // don't read as a cliff or a peak; one not known in full is drawn no higher than the chart's top.
          const dashed = (i: number) => buckets[i]!.unknown === true || (isPartial(buckets[i]!) && (i === 0 || i === buckets.length - 1));
          // Each stretch of solid buckets is its own line and area: nothing is drawn across a bucket not known in full.
          const stretches: { bucket: Bucket; i: number }[][] = [];
          buckets.forEach((bucket, i) => {
            if (dashed(i)) stretches.push([]);
            else (stretches[stretches.length - 1] ?? stretches[stretches.push([]) - 1]!).push({ bucket, i });
          });
          return (
            <>
              {stretches
                .filter((s) => s.length > 0)
                .map((s) => {
                  let line = "";
                  s.forEach(({ bucket, i }, n) => {
                    const v = y(shown(i));
                    line += `${n === 0 ? "M" : "L"}${x(bucket.start)},${v}L${x(bucket.end)},${v}`;
                  });
                  const first = s[0]!.bucket;
                  const last = s[s.length - 1]!.bucket;
                  return (
                    <g key={first.start}>
                      <path className="trend-area" d={`${line}L${x(last.end)},${VH}L${x(first.start)},${VH}Z`} />
                      <path className="trend-line" d={line} />
                    </g>
                  );
                })}
              {buckets.map((bucket, i) =>
                dashed(i) ? <path key={bucket.start} className="trend-line partial" d={`M${x(bucket.start)},${y(shown(i))}L${x(bucket.end)},${y(shown(i))}`} /> : null,
              )}
            </>
          );
        }}
      </TimeChart>
    </div>
  );
}

/** What a statement ran in bucket `i`, `[time ms, runs]`, where the bucket accounts for it: null where it wasn't
 * tracked, in a bucket the source can't account for in full, and where it ran more than the whole database did (a
 * counter misread, not a run). Mirrors culprit_core::model::Timeline::statement_bucket. */
export function statementBucket(t: Timeline, q: QueryStat, i: number): [number, number] | null {
  const time = q.trend?.time_ms[i];
  const runs = q.trend?.executions[i];
  const allTime = t.time_ms[i];
  const allRuns = t.executions[i];
  if (time == null || runs == null || allTime == null || allRuns == null || t.unknown?.[i] === true) return null;
  return time <= allTime + 0.5 && runs <= allRuns + 0.5 ? [time, runs] : null;
}

/** Average duration per run, bucket by bucket; gaps where the query didn't run or the source can't tell. */
function meanPoints(q: QueryStat, t: Timeline, b: Bucket[]): (number | null)[] {
  if (!q.trend) return [];
  return b.map((_, i) => {
    const known = statementBucket(t, q, i);
    return known && known[1] > 0 ? known[0] / known[1] : null;
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
  const b = useMemo(() => (timeline ? bucketsOf(timeline) : []), [timeline]);
  const points = useMemo(() => (timeline ? meanPoints(query, timeline, b) : []), [query, timeline, b]);
  const known = points.filter((p): p is number => p != null);
  // Two points draw as a stray tick, not a trend: show a trend from three readings.
  if (known.length < 3) return <span className="spark-empty">—</span>;
  const from = b[0]!.start;
  const to = b[b.length - 1]!.end;
  const max = Math.max(...known);
  const x = (t: number) => ((t - from) / (to - from)) * VW;
  const y = (v: number) => VH - 8 - (v / max) * (VH - 16);
  return (
    <svg className={`spark ${alert ? "alert" : ""}`} viewBox={`0 0 ${VW} ${VH}`} preserveAspectRatio="none" aria-label="Average duration over time">
      <path d={linePath(points, b, x, y)} />
    </svg>
  );
}

/** The query's time per run and how often it ran, on one time axis, with a flag where each plan first ran. */
export function QueryTrend({ query, timeline, onSince }: { query: QueryStat; timeline: Timeline; onSince?: (at: number) => void }) {
  const b = useMemo(() => bucketsOf(timeline), [timeline]);
  const points = useMemo(() => meanPoints(query, timeline, b), [query, timeline, b]);
  const known = points.filter((p): p is number => p != null);
  if (b.length < 2 || known.length < 2) return null;
  // Runs only where the bucket accounts for them: never one the source can't account for, so never a false peak.
  const runs = b.map((_, i) => statementBucket(timeline, query, i)?.[1] ?? null);
  const maxMean = Math.max(...known);
  const maxRuns = Math.max(...runs.map((r) => r ?? 0), 1e-9);
  const ordered = (query.plan_history?.kind === "cache" ? [] : [...query.plans]).sort((a, c) => (a.first_seen ?? "").localeCompare(c.first_seen ?? ""));
  const firstPlan = ordered[0]?.first_seen ? Date.parse(ordered[0].first_seen) : -Infinity;
  const flags: Flag[] = ordered
    .map((p) => ({ p, color: planColor(query.plans.indexOf(p)) }))
    .filter(({ p }) => p.first_seen && Date.parse(p.first_seen) > firstPlan)
    .map(({ p, color }) => ({
      at: Date.parse(p.first_seen!),
      label: `Plan ${p.id}`,
      tone: "plan" as const,
      color,
      onClick: onSince ? () => onSince(Date.parse(p.first_seen!)) : undefined,
    }));
  const peak = points.indexOf(maxMean);
  return (
    <section>
      <h3>Over time</h3>
      {query.plan_history?.kind !== "cache" && query.plans.length > 1 && (
        <div className="plan-keys">
          {query.plans.map((p, i) => (
            <span key={p.id} className="plan-key">
              <i style={{ background: planColor(i) }} />
              Plan {p.id}
              {p.first_seen && <span className="muted"> · from {when(Date.parse(p.first_seen))}</span>}
            </span>
          ))}
        </div>
      )}
      <div className="trend stacked">
        <div className="trend-head">
          <span className="label">Time per run</span>
        </div>
        <TimeChart
          buckets={b}
          max={maxMean}
          format={(v) => duration(v)}
          height={88}
          peak={peak >= 0 ? { index: peak, value: maxMean } : null}
          flags={flags}
          showTimeAxis={false}
          ariaLabel="Average time per run over the window"
          readout={(i) => (
            <>
              <b>{points[i] != null ? duration(points[i]!) : "—"}</b> per run
              <span className="muted"> · {runs[i] != null ? `${count(runs[i]!)} runs` : b[i]?.unknown ? "not known in full" : "not tracked"}</span>
            </>
          )}
        >
          {({ x, y }) => <path className="trend-line" d={linePath(points, b, x, y)} />}
        </TimeChart>
        <div className="trend-head">
          <span className="label">Runs</span>
        </div>
        <TimeChart
          buckets={b}
          max={maxRuns}
          format={(v) => count(v)}
          height={40}
          ariaLabel="Runs per bucket over the window"
          footnote={
            timeline.source === "scans"
              ? "From Culprit's scans: each bar covers the time between two scans."
              : timeline.source === "slow-log"
                ? "From the slow query log: each run as it was logged."
                : "From Query Store."
          }
          readout={(i) => (
            <>
              <b>{runs[i] != null ? count(runs[i]!) : "—"}</b> runs
            </>
          )}
        >
          {({ x, y }) =>
            b.map((bucket, i) => {
              const r = runs[i];
              if (r == null) return null;
              const x0 = x(bucket.start);
              const w = Math.max(x(bucket.end) - x0 - 2, 1);
              return <rect key={i} className="trend-bar" x={x0 + 1} y={y(r)} width={w} height={VH - y(r)} />;
            })
          }
        </TimeChart>
      </div>
    </section>
  );
}
