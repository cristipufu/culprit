import type { Report } from "../gen/Report";
import type { OperationalCounterKind } from "../gen/OperationalCounterKind";
import { bytes, count, percent } from "../lib/format";
import { TerminationHistoryView } from "./TerminationHistory";
import { NodeResourcesView } from "./NodeResources";

export function hasOperationalHealth(report: Report): boolean {
  const o = report.snapshot.operations;
  return !!o && (!!o.spaces.length || !!o.counters.length || !!o.sequences.length || !!o.multixacts.length || !!o.sqlite || !!o.cluster || !!o.termination_history || !!o.node_resources || !!report.snapshot.sources?.some((s) => ["docdb-terminations", "docdb-node-resources"].includes(s.source) && s.state === "unavailable"));
}

const COUNTERS: Record<OperationalCounterKind, string> = {
  "archive-failures": "WAL archive failures",
  "subscription-apply-errors": "Subscription apply errors",
  "subscription-sync-errors": "Subscription synchronization errors",
  "standby-conflicts": "Standby recovery cancellations",
  "redo-log-waits": "Redo buffer waits",
  "checksum-failures": "Page checksum or I/O corruption",
  "connection-failures": "Failed connection handshakes",
};

/** Display the source's measurements and period. Findings determine whether they need attention. */
export function OperationalHealth({ report }: { report: Report }) {
  if (!hasOperationalHealth(report)) return null;
  const o = report.snapshot.operations;
  const issues = report.findings.filter((f) => ["storage-capacity", "sequence-capacity", "multixact-age", "operational-errors", "cluster-health", "sqlite-wal"].includes(f.check));
  const flagged = new Set(issues.map((f) => f.subject.name));
  const sequences = [...o.sequences].sort((a, b) => Number(flagged.has(b.name)) - Number(flagged.has(a.name))).slice(0, 100);
  const multixacts = [...o.multixacts].sort((a, b) => b.age - a.age).slice(0, 100);
  return <section className="operational-health">
    <h3>Capacity and operational evidence</h3>
    <p className="muted">Allocation and node state were measured when this scan ran. Counters retain their own reset dates; shared measurements cover the whole server. These measurements alone do not establish that requests are slow.</p>
    {o.node_resources ? <NodeResourcesView key={o.node_resources.observed_until} resources={o.node_resources} /> : report.snapshot.sources?.filter((s) => s.source === "docdb-node-resources" && s.state === "unavailable").map((s) => <div className="operational-card" key={s.source}><h4>Shared node resources unavailable</h4><p>{s.reason}</p></div>)}
    {o.termination_history ? <TerminationHistoryView key={o.termination_history.requested_end} history={o.termination_history} report={report} /> : report.snapshot.sources?.filter((s) => s.source === "docdb-terminations" && s.state === "unavailable").map((s) => <div className="operational-card" key={s.source}><h4>Recorded terminations unavailable</h4><p>{s.reason}</p></div>)}
    {o.spaces.length > 0 && <div className="operational-grid">{o.spaces.map((s) => <div className="operational-card" key={`${s.scope}:${s.name}`}>
      <h4>{s.name}</h4>
      <p><b>{bytes(s.used_bytes)}</b> used of {bytes(s.total_bytes)} currently allocated</p>
      <p className="muted">{s.scope === "server" ? "Whole server · " : "Current database · "}Growth {s.growth_enabled === null ? "not measured" : s.growth_enabled ? "configured" : "unavailable at the recorded limit"}{s.limit_bytes !== null ? ` · configured limit ${bytes(s.limit_bytes)}` : ""}. Filesystem free space was not measured.</p>
      {s.parts.length > 0 && <dl>{s.parts.map((p) => <div key={p.name}><dt>{p.name}</dt><dd>{bytes(p.bytes)}</dd></div>)}</dl>}
      {s.reuse_wait && <p>Log reuse reason: <code>{s.reuse_wait}</code></p>}
    </div>)}</div>}
    {o.sqlite && <div className="operational-card">
      <h4>SQLite WAL file</h4><p><b>{bytes(o.sqlite.wal_bytes)}</b> allocated on disk at scan time</p>
      <p className="muted">WAL allocation can be reused. File size does not measure uncheckpointed frames, a blocked writer or checkpoint progress.</p>
    </div>}
    {o.cluster && <div className="operational-card">
      <h4>Galera node state</h4>
      <dl>{([["Ready", o.cluster.ready], ["Connected", o.cluster.connected], ["Primary component", o.cluster.primary_component]] as const).map(([name, value]) => <div key={name}><dt>{name}</dt><dd>{value === null ? "Not measured" : value ? "Yes" : "No"}</dd></div>)}</dl>
      {o.cluster.local_state && <p>{o.cluster.local_state}</p>}
      {o.cluster.flow_control_paused !== null && <p className="muted">Flow control paused {percent(o.cluster.flow_control_paused)} of the time since the last FLUSH STATUS; that reset date was not recorded. This cluster measurement has its own period, rather than measuring this database's query time.</p>}
    </div>}
    {o.counters.length > 0 && <details><summary>Historical operational counters · {o.counters.length}</summary><div className="operational-grid">{o.counters.map((c) => <div className="operational-card" key={`${c.kind}:${c.subject}`}>
      <h4>{COUNTERS[c.kind]}</h4><p>{c.subject} · <b>{count(c.count)}</b> recorded</p>
      <p className="muted">{c.scope === "server" ? "Whole server" : "Current database"} · {c.since ? `since ${c.since}` : "reset time unknown"}. A retained total does not establish an ongoing problem.</p>
      {c.before && <p>{count(c.count - c.before.count)} newly counted between {c.before.at} and {report.snapshot.taken_at}. This comparison retains those dates even when they extend beyond the selected workload period.</p>}
      {c.last_event_at && <p className="muted">Latest recorded event: {c.last_event_at}</p>}
    </div>)}</div></details>}
    {o.sequences.length > 0 && <details><summary>Identity and sequence bounds · {o.sequences.length}</summary>{o.sequences.length > 100 && <p className="muted">Showing 100, with findings first. The full scan JSON contains every recorded bound.</p>}<div className="operational-grid">{sequences.map((s) => <div className="operational-card" key={s.name}>
      <h4>{s.name}</h4><dl><div><dt>Recorded allocation</dt><dd>{s.current}</dd></div><div><dt>Increment</dt><dd>{s.increment}</dd></div><div><dt>Bounds</dt><dd>{s.minimum} to {s.maximum}</dd></div></dl>
      <p className="muted">Cycling {s.cycles ? "enabled" : "disabled"}. Cache reservation, skipped values and allocation rules prevent a forecast of remaining application requests.</p>
    </div>)}</div></details>}
    {o.multixacts.length > 0 && <details><summary>MultiXact ages · {o.multixacts.length}</summary>{o.multixacts.length > 100 && <p className="muted">Showing the 100 oldest subjects. The full scan JSON contains every recorded age.</p>}<div className="operational-grid">{multixacts.map((m) => <div className="operational-card" key={m.subject}>
      <h4>{m.subject}</h4><p>{count(m.age)} MultiXact IDs old</p><p className="muted">Independent of ordinary transaction-ID age. {m.freeze_max_age !== null ? `Configured freeze maximum: ${count(m.freeze_max_age)}.` : "Configured freeze maximum was not recorded."} Age is not elapsed time.</p>
    </div>)}</div></details>}
    {issues.length > 0 && <details><summary>Related findings · {issues.length}</summary><ul>{issues.map((f) => <li key={f.id}>{f.title} <span className="muted">· {f.actionability.replaceAll("-", " ")}</span></li>)}</ul></details>}
  </section>;
}
