import type { Engine } from "../gen/Engine";
import type { QueryStat } from "../gen/QueryStat";
import type { WorkloadKind } from "../gen/WorkloadKind";
import { count, duration, percent } from "../lib/format";
import { planColor } from "./Trends";
import { serverName } from "../lib/engines";

/**
 * How a query's run times spread, lane by lane: a plan per lane where the engine keeps plans (SQL Server), the whole
 * statement otherwise. Whisker = fastest to slowest run, band = one standard deviation around the average, dot = the
 * average (or the median where a histogram exists). One log scale for every lane, so plans compare at a glance.
 */

type Lane = {
  key: string;
  label: string;
  color: string;
  runs: number;
  mean: number;
  min?: number | null;
  max?: number | null;
  stddev?: number | null;
  p50?: number | null;
  p95?: number | null;
  p99?: number | null;
  histogram?: [number, number, number][];
  compiled?: string | null;
};

/** Showplan's `@c = NULL, @t = (2)` as the values that were sniffed: `@t = 2`. */
function tidy(compiled?: string | null): string | null {
  if (!compiled) return null;
  const parts = compiled
    .split(/,\s*(?=@)/)
    .map((p) => p.trim())
    .filter((p) => !/=\s*NULL$/i.test(p))
    .map((p) => p.replace(/=\s*\((.*)\)$/, "= $1"));
  return parts.length ? parts.join(", ") : null;
}

function lanes(q: QueryStat): Lane[] {
  const spread = q.spread;
  if (q.plans.length > 0 && q.plan_history?.kind !== "cache") {
    return q.plans
      .filter((p) => p.executions > 0)
      .map((p, i) => ({
        key: `${p.variant_query ?? ""}-${p.id}`,
        label: p.variant_query ? `Variant · plan ${p.id}` : `Plan ${p.id}`,
        color: planColor(q.plans.indexOf(p)),
        runs: p.executions,
        mean: p.mean_time_ms,
        min: p.min_time_ms,
        max: p.max_time_ms,
        stddev: p.stddev_time_ms,
        compiled: tidy(p.compiled) ?? (i === 0 ? tidy(q.plan?.props?.Parameters) : null),
      }));
  }
  return [
    {
      key: q.id,
      label: "All runs",
      color: planColor(0),
      runs: q.executions,
      mean: q.mean_time_ms,
      min: spread?.min_ms,
      max: q.max_time_ms,
      stddev: q.stddev_time_ms,
      p50: underMax(spread?.p50_ms, q.max_time_ms),
      p95: underMax(spread?.p95_ms, q.max_time_ms),
      p99: underMax(spread?.p99_ms, q.max_time_ms),
      histogram: spread?.histogram as [number, number, number][] | undefined,
    },
  ];
}

/** A percentile never above the slowest run (MySQL reports bucket upper edges, which can overshoot it). */
function underMax(p: number | null | undefined, max: number | null | undefined): number | null | undefined {
  return p != null && max != null && max > 0 ? Math.min(p, max) : p;
}

/** 1, 2 and 5 times each power of ten, for log-axis ticks. */
const STEPS = [0.001, 0.01, 0.1, 1, 10, 100, 1_000, 10_000, 100_000].flatMap((d) => [d, d * 2, d * 5]);
const DECADES = [0.001, 0.01, 0.1, 1, 10, 100, 1_000, 10_000, 100_000];

/** An axis label: round steps read "1 s" and "10 s", not "1.0 s". */
function tick(ms: number): string {
  return duration(ms).replace(/\.0 (s)$/, " $1");
}

function logTicks(lo: number, hi: number): number[] {
  const all = STEPS.filter((t) => t >= lo && t <= hi);
  const decades = all.filter((t) => DECADES.includes(t));
  return decades.length >= 3 ? decades : all.length > 6 ? all.filter((_, i) => i % 2 === 0) : all;
}

function coverage(engine: Engine, hasHistogram: boolean, product?: string, kind?: WorkloadKind | null): string {
  if (kind === "slow-log")
    return hasHistogram
      ? "The slow query log keeps every logged run, so the bars show how many runs took how long (only runs slower than long_query_time)."
      : "The slow query log keeps every logged run slower than long_query_time; the lane shows fastest, average and slowest.";
  const name = serverName(engine, product);
  switch (engine) {
    case "mssql":
      return kind === "plan-cache"
        ? "The plan cache keeps the fastest and slowest run of each cached plan since it was cached, not their spread: Query Store would add it."
        : "Query Store keeps the fastest and slowest run and the spread of every plan, and each plan's compiled parameter values.";
    case "postgres":
      return "PostgreSQL keeps one fastest, average, slowest and spread per statement: enough to see a long tail, not which values caused it.";
    case "yugabyte":
      return "YSQL keeps aggregate run-time statistics in the recorded node or cluster scope. Their spread does not identify which parameter values, tablets, RPCs or retries caused slow runs.";
    case "mysql":
      return hasHistogram
        ? `${name} keeps a run-time histogram per statement, so the bars show how many runs took how long.`
        : name === "MariaDB"
          ? "MariaDB keeps no run-time histogram per statement; the lane shows fastest, average and slowest. The slow query log table adds every run."
          : "This MySQL keeps no run-time histogram (8.0.19+ does); the lane shows fastest, average and slowest.";
    case "sqlite":
      return "SQLite keeps no run times; these come from the query log Culprit read, one entry per logged run.";
  }
}

function sentence(q: QueryStat, ls: Lane[]): string | null {
  const busy = ls.filter((l) => l.runs >= Math.max(10, q.executions * 0.01));
  if (busy.length >= 2) {
    const fast = busy.reduce((a, b) => (b.mean < a.mean ? b : a));
    const slow = busy.reduce((a, b) => (b.mean > a.mean ? b : a));
    if (slow.mean >= fast.mean * 3) {
      return `${fast.label} handles ${percent(fast.runs / q.executions)} of calls in ${duration(fast.mean)}; ${slow.label.toLowerCase()} takes ${duration(slow.mean)} for ${percent(slow.runs / q.executions)} of them.`;
    }
  }
  const lane = ls[0];
  if (!lane) return null;
  // "1 in 100" needs a hundred runs to mean anything; fewer get the median alone.
  if (lane.p50 != null && lane.p99 != null && lane.runs >= 100) return `Half the runs finish within ${duration(lane.p50)}; 1 in 100 takes over ${duration(lane.p99)}.`;
  if (lane.p50 != null && lane.max != null && lane.runs >= 2) return `Half the runs finish within ${duration(lane.p50)}; the slowest took ${duration(lane.max)}.`;
  if (lane.min != null && lane.max != null && lane.max > lane.mean * 3) {
    return `Runs range from ${duration(lane.min)} to ${duration(lane.max)}, averaging ${duration(lane.mean)}${lane.stddev != null ? ` (±${duration(lane.stddev)})` : ""}.`;
  }
  return null;
}

export function RuntimeSpread({
  query,
  engine,
  windowed,
  product,
  kind,
}: {
  query: QueryStat;
  engine: Engine;
  windowed: boolean;
  product?: string;
  kind?: WorkloadKind | null;
}) {
  const ls = lanes(query);
  const values = ls
    .flatMap((l) => [l.min, l.max, l.mean, l.p50, l.p99, ...(l.histogram ?? []).flatMap((h) => [h[0], h[1]])])
    .filter((v): v is number => v != null && v > 0);
  // Nothing beyond an average: no spread to draw.
  if (!ls.some((l) => l.max != null && l.max > l.mean) || values.length < 2) return null;
  const lo = Math.max(Math.min(...values) / 1.5, 0.001);
  const hi = Math.max(...values) * 1.5;
  const pos = (ms: number) => ((Math.log10(Math.max(ms, lo)) - Math.log10(lo)) / (Math.log10(hi) - Math.log10(lo))) * 100;
  const ticks = logTicks(lo, hi);
  const hasHistogram = ls.some((l) => (l.histogram?.length ?? 0) > 0);
  const words = sentence(query, ls);
  return (
    <section className="rs">
      <h3>
        Runtime spread
        <span className="muted rs-sub">{windowed ? "over the window" : "since statistics were reset"}</span>
      </h3>
      {words && <p className="rs-sentence">{words}</p>}
      <div className="rs-lanes">
        {ls.map((l) => {
          const histMax = Math.max(...(l.histogram ?? []).map((h) => h[2]), 1);
          const sdLo = l.stddev != null ? Math.max(l.mean - l.stddev, l.min ?? lo, lo) : null;
          const sdHi = l.stddev != null ? Math.min(l.mean + l.stddev, l.max ?? hi) : null;
          // Percentiles of a handful of runs are just the slowest run under another name: mark them only with enough runs.
          const p95 = l.runs >= 20 ? l.p95 : null;
          const p99 = l.runs >= 100 ? l.p99 : null;
          const tail = p95 != null && p99 != null && Math.abs(pos(p99) - pos(p95)) < 7;
          return (
            <div key={l.key} className="rs-lane">
              <div className="rs-label">
                <span className="rs-name">
                  <i style={{ background: l.color }} />
                  {l.label}
                </span>
                <span className="muted">
                  {count(l.runs)} runs{query.executions > 0 && ls.length > 1 ? ` · ${percent(l.runs / query.executions)}` : ""}
                </span>
                {l.compiled && (
                  <span className="rs-compiled" title="The parameter values this plan was compiled for">
                    compiled for {l.compiled}
                  </span>
                )}
              </div>
              <div className="rs-track" style={{ ["--lane" as string]: l.color }}>
                {(l.histogram ?? []).map((h, i) => (
                  <span
                    key={i}
                    className="rs-hist"
                    style={{ left: `${pos(h[0] || lo)}%`, width: `max(2px, calc(${pos(h[1])}% - ${pos(h[0] || lo)}%))`, height: `${Math.max(8, (h[2] / histMax) * 100)}%` }}
                    title={`${count(h[2])} runs between ${duration(h[0])} and ${duration(h[1])}`}
                  />
                ))}
                {l.min != null && l.max != null && <span className="rs-whisker" style={{ left: `${pos(l.min)}%`, width: `${pos(l.max) - pos(l.min)}%` }} />}
                {sdLo != null && sdHi != null && sdHi > sdLo && <span className="rs-band" style={{ left: `${pos(sdLo)}%`, width: `${pos(sdHi) - pos(sdLo)}%` }} />}
                {l.min != null && <span className="rs-cap" style={{ left: `${pos(l.min)}%` }} title={`fastest ${duration(l.min)}`} />}
                {l.max != null && <span className="rs-cap" style={{ left: `${pos(l.max)}%` }} title={`slowest ${duration(l.max)}`} />}
                {tail ? (
                  <span className="rs-q" style={{ left: `${pos(p99!)}%` }} title={`95% of runs within ${duration(p95!)}, 99% within ${duration(p99!)}`}>
                    p95·99
                  </span>
                ) : (
                  <>
                    {p95 != null && (
                      <span className="rs-q" style={{ left: `${pos(p95)}%` }} title={`95% of runs within ${duration(p95)}`}>
                        p95
                      </span>
                    )}
                    {p99 != null && (
                      <span className="rs-q" style={{ left: `${pos(p99)}%` }} title={`99% of runs within ${duration(p99)}`}>
                        p99
                      </span>
                    )}
                  </>
                )}
                <span className="rs-dot" style={{ left: `${pos(l.p50 ?? l.mean)}%` }} title={`${l.p50 != null ? "median" : "average"} ${duration(l.p50 ?? l.mean)}`} />
              </div>
              <div className="rs-values">
                <span>{l.min != null ? duration(l.min) : "—"}</span>
                <b>{duration(l.p50 ?? l.mean)}</b>
                <span>{l.max != null ? duration(l.max) : "—"}</span>
              </div>
            </div>
          );
        })}
        <div className="rs-axis">
          <span />
          <div className="rs-ticks">
            {ticks.map((t) => (
              <span key={t} style={{ left: `${pos(t)}%`, transform: `translateX(${pos(t) > 88 ? -100 : pos(t) < 8 ? 0 : -50}%)` }}>
                {tick(t)}
              </span>
            ))}
          </div>
          <span className="muted rs-legend">fastest · {hasHistogram ? "median" : "average"} · slowest</span>
        </div>
      </div>
      <p className="rs-note">{coverage(engine, hasHistogram, product, kind)}</p>
    </section>
  );
}
