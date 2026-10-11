import { useState, type MouseEvent } from "react";
import type { Engine } from "../gen/Engine";
import type { OpenTransaction } from "../gen/OpenTransaction";
import type { Pressure } from "../gen/Pressure";
import type { ResourceSample } from "../gen/ResourceSample";
import type { Signal } from "../gen/Signal";
import { duration, percent } from "../lib/format";
import { SIGNAL_SCOPE, signalValue } from "../lib/waits";
import { Replication, StorageLatency } from "./BeyondTime";

const RANK: Record<Signal["status"], number> = { bad: 0, watch: 1, good: 2, info: 3 };

/** A signal: label, value, when it was measured, and what healthy looks like, in words. */
export function SignalTile({ signal }: { signal: Signal }) {
  return (
    <div className={`signal-tile ${signal.status}`}>
      <div className="signal-head">
        <span className="signal-label">{signal.label}</span>
        {signal.status !== "info" && <i className={`sig-dot ${signal.status}`} title={signal.status === "good" ? "Healthy" : signal.status === "watch" ? "Worth a look" : "Needs attention"} />}
      </div>
      <div className="signal-value">{signalValue(signal)}</div>
      <div className="signal-meta">
        <span className="scope">{SIGNAL_SCOPE[signal.scope]}</span>
        {signal.healthy && <span className="healthy">{signal.healthy}</span>}
      </div>
    </div>
  );
}

function OpenTransactionCard({ t }: { t: OpenTransaction }) {
  const long = t.seconds >= 1800;
  return (
    <div className={`open-txn ${long ? "long" : ""}`}>
      <i className={`sig-dot ${long ? "bad" : t.seconds >= 600 ? "watch" : "good"}`} />
      <div>
        <div className="open-txn-title">
          Oldest open transaction: session <b>{t.session}</b>, open {duration(t.seconds * 1000)}
          {t.state ? ` · ${t.state}` : ""}
        </div>
        <div className="open-txn-meta">
          {t.program ? `From ${t.program}. ` : ""}
          {t.held_bytes ? `The version store holds ${Math.round(t.held_bytes / 1024 / 1024)} MB for it. ` : ""}
          {t.held_versions ? `InnoDB keeps ${Math.round(t.held_versions).toLocaleString()} old row versions for it. ` : ""}
          {long ? "Long open transactions keep old row versions alive and hold locks; see the finding for the fix." : "Short-lived: nothing held back."}
        </div>
      </div>
    </div>
  );
}

export function HealthTab({
  pressure,
  resources,
  engineLabel,
  engine,
}: {
  pressure: Pressure | null;
  resources: ResourceSample[];
  engineLabel: string;
  engine?: Engine;
}) {
  const signals = [...(pressure?.signals ?? [])].sort((a, b) => RANK[a.status] - RANK[b.status]);
  if (!signals.length && !resources.length) {
    return (
      <div className="waits-empty">
        <h3>No health signals in this scan</h3>
        <p>Re-scan with a login that can read the server's statistics views to see compiles, memory grants, cache hits and connections.</p>
      </div>
    );
  }
  const uptime = pressure?.uptime_hours;
  return (
    <div className="health-tab">
      {signals.length > 0 && (
        <>
          <div className="health-intro">
            Cheap counters read with the scan. {uptime != null ? `“Since start” covers ${duration(uptime * 3_600_000)} since ${engineLabel} started or its statistics were reset. ` : ""}
            Each tile says what healthy looks like.
          </div>
          <div className="signal-grid">
            {signals.map((s) => (
              <SignalTile key={s.key} signal={s} />
            ))}
          </div>
        </>
      )}
      {pressure?.oldest_transaction && <OpenTransactionCard t={pressure.oldest_transaction} />}
      {engine && <StorageLatency files={pressure?.files ?? []} engine={engine} />}
      <Replication replicas={pressure?.replicas ?? []} />
      {resources.length >= 4 && <ResourceCharts samples={resources} />}
    </div>
  );
}

// ── resource limits (Azure SQL Database) ───────────────────────────────────────────────────────────

const VW = 600;
const VH = 120;
const LIMIT = 95;

type Pick = (s: ResourceSample) => number | null;

const DIMENSIONS: { key: string; label: string; pick: Pick; effect: string }[] = [
  { key: "log", label: "Log writes", pick: (s) => s.log_write_pct, effect: "writes are throttled" },
  { key: "cpu", label: "CPU", pick: (s) => s.cpu_pct, effect: "queries queue for a processor" },
  { key: "io", label: "Data reads", pick: (s) => s.data_io_pct, effect: "reads from disk are throttled" },
  { key: "workers", label: "Worker threads", pick: (s) => s.workers_pct, effect: "new requests fail" },
];

const timeFmt = new Intl.DateTimeFormat(undefined, { timeZone: "UTC", hour: "2-digit", minute: "2-digit", hour12: false });

function ResourceCharts({ samples }: { samples: ResourceSample[] }) {
  const times = samples.map((s) => Date.parse(s.at));
  return (
    <section className="resource-charts">
      <div className="resource-head">
        <h4>Against the tier's limits</h4>
        <span className="muted">
          {timeFmt.format(times[0]!)}–{timeFmt.format(times[times.length - 1]!)} UTC · 15-second samples · the dashed line is where Azure starts to throttle
        </span>
      </div>
      <div className="resource-grid">
        {DIMENSIONS.map((d) => (
          <ResourceChart key={d.key} label={d.label} effect={d.effect} times={times} values={samples.map(d.pick)} />
        ))}
      </div>
    </section>
  );
}

function ResourceChart({ label, effect, times, values }: { label: string; effect: string; times: number[]; values: (number | null)[] }) {
  const [hover, setHover] = useState<number | null>(null);
  const known = values.filter((v): v is number => v != null);
  if (known.length < 4) return null;
  const from = times[0]!;
  const to = times[times.length - 1]!;
  const x = (t: number) => ((t - from) / Math.max(1, to - from)) * VW;
  const y = (v: number) => VH - (Math.min(100, Math.max(0, v)) / 100) * VH;
  const points = values.map((v, i) => (v == null ? null : [x(times[i]!), y(v)] as const)).filter((p): p is readonly [number, number] => p != null);
  const line = points.map(([px, py], i) => `${i ? "L" : "M"}${px.toFixed(1)},${py.toFixed(1)}`).join("");
  const area = `${line}L${points[points.length - 1]![0]},${VH}L${points[0]![0]},${VH}Z`;
  const atLimit = known.filter((v) => v >= LIMIT).length / known.length;
  const peakIndex = values.reduce<number>((best, v, i) => (v != null && v > (values[best] ?? -1) ? i : best), 0);
  const peak = values[peakIndex] ?? 0;
  const ticks = [0, 0.25, 0.5, 0.75, 1].map((f) => from + f * (to - from));
  const status = atLimit >= 0.3 ? "bad" : atLimit >= 0.1 ? "watch" : "good";
  const onMove = (e: MouseEvent<HTMLDivElement>) => {
    const box = e.currentTarget.getBoundingClientRect();
    const at = from + ((e.clientX - box.left) / box.width) * (to - from);
    let best = 0;
    times.forEach((t, i) => {
      if (Math.abs(t - at) < Math.abs(times[best]! - at)) best = i;
    });
    setHover(best);
  };
  const hv = hover != null ? values[hover] : null;
  return (
    <div className="resource-chart">
      <div className="resource-chart-head">
        <span className="resource-label">
          <i className={`sig-dot ${status}`} /> {label}
        </span>
        <span className="resource-sentence">
          {atLimit >= 0.01 ? (
            <>
              At the limit <b>{percent(atLimit)}</b> of the time: {effect}
            </>
          ) : (
            <>Never at the limit · peak {Math.round(peak)}%</>
          )}
        </span>
      </div>
      <div className="resource-plot" onMouseMove={onMove} onMouseLeave={() => setHover(null)}>
        <div className="resource-yaxis" aria-hidden>
          <span style={{ top: "0%" }}>100%</span>
          <span style={{ top: "50%" }}>50%</span>
          <span style={{ top: "100%" }}>0</span>
        </div>
        <div className="resource-canvas">
          <svg viewBox={`0 0 ${VW} ${VH}`} preserveAspectRatio="none" role="img" aria-label={`${label}: at the limit ${percent(atLimit)} of the time, peak ${Math.round(peak)}%`}>
            <line className="grid" x1="0" x2={VW} y1={y(50)} y2={y(50)} />
            <line className="grid" x1="0" x2={VW} y1={y(100)} y2={y(100)} />
            <line className="limit" x1="0" x2={VW} y1={y(LIMIT)} y2={y(LIMIT)} />
            <path className="area" d={area} />
            <path className="line" d={line} vectorEffect="non-scaling-stroke" />
            {hover != null && <line className="cursor" x1={x(times[hover]!)} x2={x(times[hover]!)} y1="0" y2={VH} vectorEffect="non-scaling-stroke" />}
          </svg>
          <span className="peak" style={{ left: `${(x(times[peakIndex]!) / VW) * 100}%`, top: `${(y(peak) / VH) * 100}%` }} title={`Peak ${Math.round(peak)}% at ${timeFmt.format(times[peakIndex]!)} UTC`} />
          <span className="limit-label">limit</span>
          {hover != null && hv != null && (
            <span className="readout" style={{ left: `${(x(times[hover]!) / VW) * 100}%` }}>
              <b>{Math.round(hv)}%</b> {timeFmt.format(times[hover]!)}
            </span>
          )}
        </div>
      </div>
      <div className="resource-xaxis" aria-hidden>
        {ticks.map((t, i) => (
          <span key={i} style={{ left: `${i * 25}%` }}>
            {timeFmt.format(t)}
          </span>
        ))}
      </div>
    </div>
  );
}
