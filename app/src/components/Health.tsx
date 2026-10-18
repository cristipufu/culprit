import { useState, type MouseEvent } from "react";
import type { Engine } from "../gen/Engine";
import type { OpenTransaction } from "../gen/OpenTransaction";
import type { Pressure } from "../gen/Pressure";
import type { Report } from "../gen/Report";
import type { SignalStatus } from "../gen/SignalStatus";
import type { ResourceSample } from "../gen/ResourceSample";
import type { Signal } from "../gen/Signal";
import { duration, percent, percentValue } from "../lib/format";
import { findingStatus, worstFinding } from "../lib/health";
import { SIGNAL_SCOPE, signalValue } from "../lib/waits";
import { Replication, StorageLatency } from "./BeyondTime";

const RANK: Record<Signal["status"], number> = { bad: 0, watch: 1, good: 2, info: 3 };

/** A signal: label, value, when it was measured, and what healthy looks like, in words. */
export function SignalTile({ signal }: { signal: Signal }) {
  return (
    <div className={`signal-tile ${signal.status}`}>
      <div className="signal-head">
        <span className="signal-label" title={signal.label}>{signal.label}</span>
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

function OpenTransactionCard({ t, status }: { t: OpenTransaction; status: SignalStatus }) {
  // A replication slot has no start time: how far it holds the horizon back is what tells.
  const slot = t.holder === "replication-slot";
  const behind = t.xid_age ?? 0;
  const long = status === "bad" || status === "watch";
  return (
    <div className={`open-txn ${long ? "long" : ""}`}>
      <i className={`sig-dot ${status}`} />
      <div>
        <div className="open-txn-title">
          {slot ? (
            <>
              Replication slot <b>{t.session}</b> holds cleanup back by {Math.round(behind).toLocaleString()} transactions
            </>
          ) : (
            <>
              {t.holder === "prepared-transaction" ? "Oldest prepared transaction" : "Oldest open transaction"}:{" "}
              {t.holder === "prepared-transaction" ? "" : "session "}
              <b>{t.session}</b>, open {duration(t.seconds * 1000)}
            </>
          )}
          {t.state ? ` · ${t.state}` : ""}
        </div>
        <div className="open-txn-meta">
          {t.program ? `From ${t.program}. ` : ""}
          {t.held_bytes ? `The database's version store held ${Math.round(t.held_bytes / 1024 / 1024)} MB. ` : ""}
          {t.held_versions ? `InnoDB keeps ${Math.round(t.held_versions).toLocaleString()} old row versions for it. ` : ""}
          {long ? "See the held-cleanup finding for the measured impact and next step." : "Recorded when the scan ran."}
        </div>
      </div>
    </div>
  );
}

export function HealthTab({
  pressure,
  resources,
  report,
  engineLabel,
  engine,
}: {
  pressure: Pressure | null;
  resources: ResourceSample[];
  report: Report;
  engineLabel: string;
  engine?: Engine;
}) {
  const signals = [...(pressure?.signals ?? [])].sort((a, b) => RANK[a.status] - RANK[b.status]);
  if (!signals.length && !resources.length) {
    return (
      <div className="waits-empty">
        <h3>No health signals in this scan</h3>
        <p>
          {engine === "sqlite"
            ? "SQLite has no server, so there are no compiles, memory grants, cache hits or connections to read: the file's own settings are under Schema."
            : "Re-scan with a login that can read the server's statistics views to see compiles, memory grants, cache hits and connections."}
        </p>
      </div>
    );
  }
  const uptime = pressure?.uptime_hours;
  return (
    <div className="health-tab">
      {signals.length > 0 && (
        <>
          <div className="health-intro">
            Measurements use the selected period where their source records history. Current queues are measured when the scan runs; cumulative totals keep their own period. {uptime != null ? `“Since start” covers ${duration(uptime * 3_600_000)} since ${engineLabel} started or its statistics were reset. ` : ""}
            Each tile states its coverage; only supported health warnings are colored.
          </div>
          <div className="signal-grid">
            {signals.filter((s) => s.scope !== "since-startup" && s.scope !== "query-history").map((s) => <SignalTile key={s.key} signal={s} />)}
          </div>
          {signals.some((s) => s.scope === "since-startup" || s.scope === "query-history") && (
            <details className="historical-signals">
              <summary>Historical counters · their totals cannot be isolated to the selected period</summary>
              <div className="signal-grid">
                {signals.filter((s) => s.scope === "since-startup" || s.scope === "query-history").map((s) => <SignalTile key={s.key} signal={s} />)}
              </div>
            </details>
          )}
        </>
      )}
      {(report.snapshot.server.platform === "azure-sql-database" || report.snapshot.server.cloud === "Azure SQL Database") && !signals.some((s) => s.key === "cpu-use") && (
        <p className="muted">CPU utilization was not measured for the selected period in this scan.</p>
      )}
      {pressure?.oldest_transaction && <OpenTransactionCard t={pressure.oldest_transaction} status={findingStatus(worstFinding(report, (f) => f.check === "held-cleanup"))} />}
      {engine && <StorageLatency files={pressure?.files ?? []} engine={engine} report={report} />}
      <Replication replicas={pressure?.replicas ?? []} report={report} />
      {resources.length >= 4 && <ResourceCharts samples={resources} signals={signals} />}
    </div>
  );
}

// ── resource limits (Azure SQL Database) ───────────────────────────────────────────────────────────

const VW = 600;
const VH = 120;
const LIMIT = 95;

type Pick = (s: ResourceSample) => number | null;

const DIMENSIONS: { key: string; label: string; pick: Pick }[] = [
  { key: "log-write-use", label: "Log writes", pick: (s) => s.log_write_pct },
  { key: "cpu-use", label: "CPU", pick: (s) => s.cpu_pct },
  { key: "data-io", label: "Data reads", pick: (s) => s.data_io_pct },
  { key: "workers-use", label: "Worker threads", pick: (s) => s.workers_pct },
];

const timeFmt = new Intl.DateTimeFormat(undefined, { timeZone: "UTC", hour: "2-digit", minute: "2-digit", hour12: false });

function ResourceCharts({ samples, signals }: { samples: ResourceSample[]; signals: Signal[] }) {
  const times = samples.map((s) => Date.parse(s.at));
  return (
    <section className="resource-charts">
      <div className="resource-head">
        <h4>Against the tier's limits</h4>
        <span className="muted">
          {timeFmt.format(times[0]!)}–{timeFmt.format(times[times.length - 1]!)} UTC · 15-second samples · available resource history · the dashed line marks 95% of this database's limit
        </span>
      </div>
      <div className="resource-grid">
        {DIMENSIONS.map((d) => (
          <ResourceChart key={d.key} label={d.label} status={signals.find((s) => s.key === d.key)?.status ?? "info"} times={times} values={samples.map(d.pick)} />
        ))}
      </div>
    </section>
  );
}

function ResourceChart({ label, status, times, values }: { label: string; status: SignalStatus; times: number[]; values: (number | null)[] }) {
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
              At or above 95% in <b>{percent(atLimit)}</b> of recorded samples
            </>
          ) : (
            <>Recorded peak {percentValue(peak)}</>
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
          <svg viewBox={`0 0 ${VW} ${VH}`} preserveAspectRatio="none" role="img" aria-label={`${label}: at or above 95% in ${percent(atLimit)} of recorded samples, peak ${percentValue(peak)}`}>
            <line className="grid" x1="0" x2={VW} y1={y(50)} y2={y(50)} />
            <line className="grid" x1="0" x2={VW} y1={y(100)} y2={y(100)} />
            <line className="limit" x1="0" x2={VW} y1={y(LIMIT)} y2={y(LIMIT)} />
            <path className="area" d={area} />
            <path className="line" d={line} vectorEffect="non-scaling-stroke" />
            {hover != null && <line className="cursor" x1={x(times[hover]!)} x2={x(times[hover]!)} y1="0" y2={VH} vectorEffect="non-scaling-stroke" />}
          </svg>
          <span className="peak" style={{ left: `${(x(times[peakIndex]!) / VW) * 100}%`, top: `${(y(peak) / VH) * 100}%` }} title={`Peak ${percentValue(peak)} at ${timeFmt.format(times[peakIndex]!)} UTC`} />
          <span className="limit-label">95%</span>
          {hover != null && hv != null && (
            <span className="readout" style={{ left: `${(x(times[hover]!) / VW) * 100}%` }}>
              <b>{percentValue(hv)}</b> {timeFmt.format(times[hover]!)}
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
