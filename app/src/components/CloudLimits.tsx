import { CircleCheck, Cloud, CloudAlert, RefreshCw } from "lucide-react";
import { useMemo, useState } from "react";
import type { CloudSeries } from "../gen/CloudSeries";
import type { CloudSignals } from "../gen/CloudSignals";
import type { ThrottleEpisode } from "../gen/ThrottleEpisode";
import { bytes, count, duration } from "../lib/format";
import { stamp, useCloud } from "../lib/events";
import { TimeChart, VH, type Bucket, type ChartEvent } from "./TimeChart";
import "./timeline.css";

const PERIODS = [
  { hours: 24, label: "24 h" },
  { hours: 24 * 7, label: "7 days" },
  { hours: 24 * 14, label: "14 days" },
];

export function cloudValue(series: Pick<CloudSeries, "unit">, v: number): string {
  switch (series.unit) {
    case "percent":
      return `${v < 10 ? v.toFixed(1) : Math.round(v)}%`;
    case "ms":
      return duration(v);
    case "bytes":
      return bytes(v);
    case "credits":
      return v < 10 ? v.toFixed(1) : String(Math.round(v));
    default:
      return v < 1 ? v.toFixed(2) : v < 10 ? v.toFixed(1) : count(v);
  }
}

const clock = new Intl.DateTimeFormat(undefined, { hour: "2-digit", minute: "2-digit" });

function episodeWhen(e: ThrottleEpisode): string {
  const a = Date.parse(e.start);
  const b = Date.parse(e.end);
  const lasted = duration(b - a);
  return `${stamp(a)}–${e.ongoing ? "now" : clock.format(b)} · ${lasted}`;
}

/**
 * What the cloud measured for this server (CloudWatch or Azure Monitor, read through the person's own CLI sign-in) and
 * when it held the server back. Each metric is drawn against its limit, with throttling shaded, so "slower with the
 * same plan" has an answer the database's own statistics can't give.
 */
export function CloudLimits({ connection, onCompare }: { connection: string; onCompare?: (from: number, to: number) => void }) {
  const [hours, setHours] = useState(24);
  const [refresh, setRefresh] = useState(0);
  const state = useCloud(connection, hours, refresh);
  if (state.status === "none") return null;
  return (
    <section className="cloud" aria-label="Cloud limits">
      <div className="cloud-head">
        <h3>
          <Cloud size={14} /> Cloud limits
        </h3>
        {state.status === "ready" && (
          <span className="cloud-tier">
            {state.signals.service} · {state.signals.resource}
            {state.signals.tier ? ` · ${state.signals.tier}` : ""}
          </span>
        )}
        <span className="spacer" />
        <span className="seg">
          {PERIODS.map((p) => (
            <button key={p.hours} className={hours === p.hours ? "on" : ""} onClick={() => setHours(p.hours)}>
              {p.label}
            </button>
          ))}
        </span>
        <button className="icon-btn" title="Read the metrics again" aria-label="Read the cloud metrics again" onClick={() => setRefresh((n) => n + 1)}>
          <RefreshCw size={13} className={state.status === "loading" ? "spin" : ""} />
        </button>
      </div>
      {state.status === "loading" && <div className="cloud-note muted">Reading the cloud's metrics…</div>}
      {state.status === "error" && (
        <div className="cloud-note">
          <b>{state.error.message}</b>
          {state.error.hint && <div className="muted">{state.error.hint}</div>}
        </div>
      )}
      {state.status === "ready" && <Signals signals={state.signals} onCompare={onCompare} />}
    </section>
  );
}

function Signals({ signals, onCompare }: { signals: CloudSignals; onCompare?: (from: number, to: number) => void }) {
  const bad = signals.episodes.length > 0;
  const shown = signals.series.filter((s) => s.points.length > 1);
  const fetched = Date.parse(signals.fetched_at);
  return (
    <>
      <div className={`cloud-summary ${bad ? "bad" : ""}`}>
        {bad ? <CloudAlert size={14} color="var(--high)" /> : <CircleCheck size={14} color="var(--good)" />}
        <span>{signals.summary}</span>
      </div>
      {bad && (
        <ul className="cloud-episodes">
          {signals.episodes.map((e) => (
            <li key={`${e.kind}-${e.start}`}>
              <span>
                <b>{e.title}</b> <span className="when">{episodeWhen(e)}</span>
                <div className="muted">{e.detail}</div>
                {onCompare && (
                  <button className="link-button" onClick={() => onCompare(Date.parse(e.start), Date.parse(e.end))}>
                    What changed while it lasted?
                  </button>
                )}
              </span>
            </li>
          ))}
        </ul>
      )}
      <div className="cloud-grid">
        {shown.map((s) => (
          <CloudChart key={s.metric} series={s} signals={signals} />
        ))}
      </div>
      <div className="cloud-foot">
        {signals.notes.join(" ")} {Number.isFinite(fetched) && `Read at ${clock.format(fetched)}.`}
      </div>
    </>
  );
}

function CloudChart({ series, signals }: { series: CloudSeries; signals: CloudSignals }) {
  const step = signals.period_seconds * 1000;
  const buckets: Bucket[] = useMemo(() => series.points.map((p) => ({ start: Date.parse(p.at), end: Date.parse(p.at) + step })), [series, step]);
  const values = series.points.map((p) => p.value);
  const peak = Math.max(...values, 0);
  // Percentages are drawn on 0–100 so headroom reads honestly; elsewhere the limit is drawn when it is in range (a
  // limit far above the data would flatten the line into the floor).
  const percent = series.unit === "percent";
  const limit = series.limit != null && (percent || series.floor || series.limit <= peak * 2) ? series.limit : null;
  const max = percent ? Math.max(100, peak) : Math.max(peak, limit ?? 0, 1e-9);
  const episodes: ChartEvent[] = signals.episodes
    .filter((e) => e.metric === series.metric)
    .map((e) => ({ id: `${e.kind}-${e.start}`, kind: "throttle", from: Date.parse(e.start), to: Date.parse(e.end), title: e.title, when: episodeWhen(e), exact: true }));
  const latest = values[values.length - 1];
  const worst = series.floor ? Math.min(...values) : peak;
  return (
    <div className={`cloud-metric ${episodes.length ? "bad" : ""}`}>
      <div className="trend-head">
        <span className="label">{series.label}</span>
        <span className="muted" title={series.limit_label ?? undefined}>
          {series.floor ? "min" : "max"} {cloudValue(series, worst)}
          {series.limit_label ? ` · ${series.limit_label}` : ""}
        </span>
        {latest != null && <span className="now">{cloudValue(series, latest)}</span>}
      </div>
      <TimeChart
        buckets={buckets}
        max={max}
        format={(v) => cloudValue(series, v)}
        height={56}
        lane={false}
        events={episodes}
        ariaLabel={`${series.label} over the period`}
        readout={(i) => <b>{values[i] != null ? cloudValue(series, values[i]!) : "—"}</b>}
      >
        {({ x, y }) => {
          let line = "";
          series.points.forEach((p, i) => {
            const b = buckets[i]!;
            line += `${i === 0 ? "M" : "L"}${x((b.start + b.end) / 2).toFixed(1)},${y(p.value).toFixed(1)}`;
          });
          const first = buckets[0]!;
          const last = buckets[buckets.length - 1]!;
          return (
            <>
              <path className="cloud-area" d={`${line}L${x((last.start + last.end) / 2)},${VH}L${x((first.start + first.end) / 2)},${VH}Z`} />
              <path className="cloud-line" d={line} />
              {limit != null && <line className={`cloud-limit ${series.limit_label?.startsWith("baseline") ? "baseline" : ""}`} x1={0} x2={1000} y1={y(limit)} y2={y(limit)} />}
            </>
          );
        }}
      </TimeChart>
    </div>
  );
}
