import type { ReactNode } from "react";
import type { Engine } from "../gen/Engine";
import type { FileIo } from "../gen/FileIo";
import type { Finding } from "../gen/Finding";
import type { QueryStat } from "../gen/QueryStat";
import type { Replica } from "../gen/Replica";
import type { Report } from "../gen/Report";
import type { TableStat } from "../gen/TableStat";
import { serverName } from "../lib/engines";
import { bytes, count, duration, percent, rowCount, when } from "../lib/format";
import { waitKey } from "../lib/waits";
import { SignalTile } from "./Health";

/**
 * Signals beyond run time: what a query costs besides its duration (failed runs, planning, JIT, lock waits, log, result
 * size), storage latency and replication for the server, and the hot spots of a table. Each block leads with one
 * sentence that says what matters, and says plainly what the engine doesn't measure.
 */

type Status = "good" | "watch" | "bad" | "info";
type Row = { key: string; label: string; value: string; note: string; status: Status; takeaway?: string };

const RANK: Record<Status, number> = { bad: 0, watch: 1, good: 2, info: 3 };

function engineName(report: Report): string {
  return serverName(report.snapshot.server.engine, report.snapshot.server.product);
}

function verb(text: string): string {
  return (text.trimStart().replace(/^\(+/, "").match(/^[a-z]+/i)?.[0] ?? "").toLowerCase();
}

const grade = (value: number, watch: number, bad: number): Status => (value >= bad ? "bad" : value >= watch ? "watch" : "good");

function lockMs(q: QueryStat): number {
  return q.waits.filter((w) => waitKey(w.category) === "lock").reduce((sum, w) => sum + w.wait_ms, 0);
}

function costRows(report: Report, q: QueryStat): Row[] {
  const engine = report.snapshot.server.engine;
  const c = q.costs ?? null;
  const runs = Math.max(1, q.executions);
  const rows: Row[] = [];

  if (c && (c.failed != null || c.aborted != null)) {
    const aborted = c.aborted ?? 0;
    const failed = c.failed ?? 0;
    const rate = (aborted + failed) / runs;
    const timeout = c.aborted_mean_ms != null && [15, 30, 60, 120].some((s) => Math.abs(c.aborted_mean_ms! / 1000 - s) <= s * 0.1);
    const parts: string[] = [];
    if (aborted > 0) parts.push(`${count(aborted)} cancelled${c.aborted_mean_ms != null ? ` after ${duration(c.aborted_mean_ms)} on average` : ""}${timeout ? ", a client timeout" : ""}`);
    if (failed > 0) parts.push(`${count(failed)} errors${c.top_error ? `, recently ${c.top_error}` : ""}`);
    if (c.warnings) parts.push(`${count(c.warnings)} warnings`);
    const status = aborted + failed >= 5 ? grade(rate, 0.01, 0.1) : aborted + failed > 0 ? "info" : "good";
    rows.push({
      key: "unfinished",
      label: "Didn't finish",
      value: aborted + failed === 0 ? "none" : `${percent(rate)} of runs`,
      note: parts.length ? parts.join(" · ") : "Every run finished",
      status,
      takeaway: `${rate >= 0.999 ? "No run finishes" : `${percent(rate)} of runs don't finish`}${timeout ? ": they hit a client timeout" : failed > 0 && c.top_error ? `: ${c.top_error}` : ""}`,
    });
  }

  if (c?.plan_ms != null) {
    const share = c.plan_ms / (c.plan_ms + q.total_time_ms);
    const plans = Math.max(1, c.plans ?? 1);
    rows.push({
      key: "planning",
      label: engine === "postgres" ? "Planning" : "Compiling",
      value: `${percent(share)} of its time`,
      note: `${duration(c.plan_ms / plans)} per plan · ${plans >= runs * 0.9 ? "a new plan every run" : `each plan reused about ${count(runs / plans)} times`}`,
      status: q.executions >= 50 ? grade(share, 0.25, 0.5) : "info",
      takeaway: `Planning takes ${percent(share)} of its time`,
    });
  } else if (engine === "postgres") {
    const tracking = report.snapshot.settings.find((s) => s.name === "pg_stat_statements.track_planning");
    if (tracking && tracking.value !== "on")
      rows.push({ key: "planning", label: "Planning", value: "not measured", note: "pg_stat_statements.track_planning is off", status: "info" });
  }

  if (c?.jit_ms != null && q.total_time_ms > 0) {
    const share = c.jit_ms / q.total_time_ms;
    rows.push({
      key: "jit",
      label: "JIT",
      value: `${percent(share)} of run time`,
      note: `${duration(c.jit_ms / runs)} per run${c.jit_functions ? ` · ${count(c.jit_functions)} functions compiled` : ""}`,
      status: grade(share, 0.25, 0.5),
      takeaway: share >= 0.5 ? "JIT compiling takes longer than running it" : `JIT takes ${percent(share)} of its run time`,
    });
  }

  const lock = lockMs(q);
  if (lock > 0 && engine !== "postgres") {
    const share = Math.min(1, lock / Math.max(1, q.total_time_ms));
    rows.push({
      key: "lock",
      label: "Lock waits",
      value: `${percent(share)} of its time`,
      note: `${duration(lock / runs)} per run, waiting for rows other sessions hold`,
      status: lock >= 10_000 ? grade(share, 0.25, 0.5) : "info",
      takeaway: `It waits on locks for ${percent(share)} of its time`,
    });
  }

  const writes = ["insert", "update", "delete", "merge", "copy", "replace"].includes(verb(q.text));
  if (c?.log_bytes != null && (writes || c.log_bytes / runs >= 65_536)) {
    const log = engine === "postgres" ? "WAL" : "log";
    const perRow = q.rows != null && q.rows >= 1 ? c.log_bytes / q.rows : null;
    rows.push({
      key: "log",
      label: engine === "postgres" ? "WAL written" : "Log written",
      value: `${bytes(c.log_bytes / runs)} per run`,
      note: [c.log_share != null ? `${percent(c.log_share)} of all ${log}` : null, perRow != null ? `${bytes(perRow)} per row` : null].filter(Boolean).join(" · ") || "",
      // The log-volume check's thresholds: a large share of a lot of log, or many KB per row changed.
      status:
        ((c.log_share ?? 0) >= 0.3 && c.log_bytes >= 1024 ** 3) || (perRow != null && perRow >= 4096 && (q.rows ?? 0) >= 1000 && c.log_bytes >= 100 * 1024 ** 2)
          ? "watch"
          : "info",
      takeaway: perRow != null && perRow >= 4096 ? `It writes ${bytes(perRow)} of ${log} per row it changes` : `It writes ${percent(c.log_share ?? 0)} of the ${log}`,
    });
  }

  // Small results are the norm; the row earns its place from a thousand rows a run.
  if (q.rows != null && q.rows / runs >= 1_000 && ["select", "with", "values", "table"].includes(verb(q.text))) {
    const perRun = q.rows / runs;
    const width = c?.row_bytes ?? null;
    rows.push({
      key: "result",
      label: "Result",
      value: `${rowCount(perRun)}${width ? ` ≈ ${bytes(perRun * width)}` : ""}`,
      note: `per run${width ? ` · about ${bytes(width)} a row, from the plan` : ""}`,
      status: q.executions >= 20 ? grade(perRun, 10_000, 100_000) : "info",
      takeaway: `It returns ${rowCount(perRun)}${width ? ` (≈ ${bytes(perRun * width)})` : ""} per run`,
    });
  }
  return rows;
}

function notKept(report: Report, q: QueryStat): string | null {
  const name = engineName(report);
  switch (report.snapshot.server.engine) {
    case "postgres":
      return `${name} keeps no error count or lock time per statement; Live shows blocking as it happens.`;
    case "mysql":
      return `${name} keeps no planning time or redo bytes per statement.`;
    default:
      return (q.costs?.failed ?? 0) > 0 ? "Query Store counts errors but doesn't keep them; the finding shows how to capture them." : null;
  }
}

/** The query detail's "beyond run time" block: one sentence, then one row per signal the engine keeps. */
export function QueryCostsPanel({ report, query }: { report: Report; query: QueryStat }) {
  const rows = costRows(report, query);
  if (rows.length === 0) return null;
  const worst = [...rows].sort((a, b) => RANK[a.status] - RANK[b.status])[0]!;
  const lead = worst.status === "bad" || worst.status === "watch" ? `${worst.takeaway}.` : "Nothing here needs attention.";
  const footer = notKept(report, query);
  return (
    <section className="beyond">
      <h3>Beyond run time</h3>
      <p className={`beyond-lead ${worst.status}`}>{lead}</p>
      <div className="beyond-rows" role="list">
        {rows.map((r) => (
          <div key={r.key} className="beyond-row" role="listitem">
            <i className={`sig-dot ${r.status}`} aria-hidden />
            <span className="beyond-label">{r.label}</span>
            <span className="beyond-value">{r.value}</span>
            <span className="beyond-note">{r.note}</span>
          </div>
        ))}
      </div>
      {footer && <p className="beyond-foot">{footer}</p>}
    </section>
  );
}

// ── storage and replication ─────────────────────────────────────────────────────────────────────

const KIND_LABEL: Record<FileIo["kind"], string> = { data: "data", log: "log", temp: "temp", other: "other" };

/** Storage latency in µs below a tenth of a millisecond, where cache hits and fast SSDs live. */
function latency(ms: number | null): string {
  if (ms == null) return "—";
  return ms < 0.1 ? `${Math.round(ms * 1000)} µs` : duration(ms);
}

function avg(ms: number | null, n: number): number | null {
  return ms != null && n > 0 ? ms / n : null;
}

/** Status of a file from what its workload waits on: reads of data and temp files, writes of the log. */
function fileStatus(f: FileIo): Status {
  if (f.kind === "log") {
    const w = avg(f.write_ms, f.writes);
    return w == null || f.writes < 1000 ? "info" : grade(w, 10, 30);
  }
  const r = avg(f.read_ms, f.reads);
  return r == null || f.reads < 1000 || f.kind === "other" ? "info" : grade(r, 20, 50);
}

function fileName(f: FileIo): string {
  return f.database && !f.name.startsWith(f.database) ? `${f.database} · ${f.name}` : f.name;
}

function storageLead(files: FileIo[], engine: Engine): { text: string; status: Status } {
  const timed = files.filter((f) => f.read_ms != null || f.write_ms != null);
  if (timed.length === 0) {
    return engine === "postgres"
      ? { text: "PostgreSQL isn't timing reads and writes: turn on track_io_timing to see storage latency.", status: "info" }
      : { text: "The server counts reads and writes but doesn't time them.", status: "info" };
  }
  const slow = [...files].map((f) => ({ f, s: fileStatus(f) })).sort((a, b) => RANK[a.s] - RANK[b.s])[0];
  if (slow && (slow.s === "bad" || slow.s === "watch")) {
    const f = slow.f;
    return f.kind === "log"
      ? { text: `Writes to ${fileName(f)} average ${latency(avg(f.write_ms, f.writes))}: every commit waits for them.`, status: slow.s }
      : { text: `Reads from ${fileName(f)} average ${latency(avg(f.read_ms, f.reads))}: storage is slow.`, status: slow.s };
  }
  const reads = files.filter((f) => f.kind !== "log" && f.read_ms != null).reduce((a, f) => ({ ms: a.ms + (f.read_ms ?? 0), n: a.n + f.reads }), { ms: 0, n: 0 });
  const logs = files.filter((f) => f.kind === "log" && f.write_ms != null).reduce((a, f) => ({ ms: a.ms + (f.write_ms ?? 0), n: a.n + f.writes }), { ms: 0, n: 0 });
  const sentence = [reads.n > 0 ? `reads average ${latency(reads.ms / reads.n)}` : null, logs.n > 0 ? `log writes ${latency(logs.ms / logs.n)}` : null]
    .filter(Boolean)
    .join(" and ");
  return { text: sentence ? `${sentence[0]!.toUpperCase()}${sentence.slice(1)}: storage keeps up.` : "Storage keeps up.", status: "good" };
}

function Latency({ ms, n, label }: { ms: number | null; n: number; label: string }) {
  if (n <= 0) return <span className="io-cell muted">—</span>;
  const a = avg(ms, n);
  return (
    <span className="io-cell" title={`${count(n)} ${label}${ms != null ? `, ${duration(ms)} in all` : ", not timed"}`}>
      <b>{a == null ? "untimed" : latency(a)}</b>
      <span className="muted"> × {count(n)}</span>
    </span>
  );
}

/** Average read and write time per file since startup, slowest first, under one sentence. */
export function StorageLatency({ files, engine, compact }: { files: FileIo[]; engine: Engine; compact?: boolean }) {
  if (files.length === 0) return null;
  const lead = storageLead(files, engine);
  return (
    <section className="io-panel">
      {!compact && (
        <div className="io-head">
          <h4>Storage</h4>
          <span className="muted">
            average per read and write since start · {engine === "postgres" ? "whole server, including reads the OS cache served" : "by file"}
          </span>
        </div>
      )}
      <p className={`beyond-lead ${lead.status}`}>{lead.text}</p>
      <div className="io-table" role="table" aria-label="Storage latency per file">
        <div className="io-row io-th" role="row">
          <span role="columnheader">File</span>
          <span role="columnheader">Reads</span>
          <span role="columnheader">Writes</span>
        </div>
        {files.slice(0, compact ? 8 : 12).map((f) => (
          <div key={`${f.database ?? ""}:${f.name}`} className="io-row" role="row">
            <span className="io-name" role="cell">
              <i className={`sig-dot ${fileStatus(f)}`} aria-hidden />
              <span className="io-file" title={fileName(f)}>
                {fileName(f)}
              </span>
              <span className={`io-kind ${f.kind}`}>{KIND_LABEL[f.kind]}</span>
            </span>
            <Latency ms={f.read_ms} n={f.reads} label="reads" />
            <Latency ms={f.write_ms} n={f.writes} label={f.kind === "log" && engine === "postgres" ? "flushes" : "writes"} />
          </div>
        ))}
      </div>
    </section>
  );
}

function replicaStatus(r: Replica): Status {
  if (r.role === "slot") return r.state === "inactive" && (r.lag_bytes ?? 0) >= 1024 ** 3 ? "bad" : r.state === "inactive" ? "watch" : "good";
  if (r.state && /stopped|not receiving|NOT SYNCHRONIZING/i.test(r.state)) return "bad";
  const s = r.lag_seconds;
  if (s != null) return grade(s, 30, 600);
  return (r.lag_bytes ?? 0) >= 1024 ** 3 ? "watch" : "good";
}

const ROLE: Record<Replica["role"], string> = { replica: "replica", "this-server": "this server", slot: "slot" };

/** Replicas, this server's own lag when it is one, and replication slots. */
export function Replication({ replicas }: { replicas: Replica[] }) {
  if (replicas.length === 0) return null;
  const worst = [...replicas].sort((a, b) => RANK[replicaStatus(a)] - RANK[replicaStatus(b)])[0]!;
  const ws = replicaStatus(worst);
  const lead =
    ws === "good"
      ? "Replicas are keeping up."
      : worst.role === "slot"
        ? `The inactive slot ${worst.name} keeps ${bytes(worst.lag_bytes)} of WAL.`
        : worst.lag_seconds != null && ws !== "bad"
          ? `${worst.role === "this-server" ? "This server" : worst.name} is ${duration(worst.lag_seconds * 1000)} behind.`
          : `${worst.role === "this-server" ? "Replication to this server" : worst.name}: ${worst.state ?? "behind"}.`;
  return (
    <section className="io-panel">
      <div className="io-head">
        <h4>Replication</h4>
      </div>
      <p className={`beyond-lead ${ws}`}>{lead}</p>
      <div className="io-table" role="table" aria-label="Replicas">
        {replicas.map((r) => (
          <div key={`${r.role}:${r.name}`} className="io-row repl" role="row">
            <span className="io-name" role="cell">
              <i className={`sig-dot ${replicaStatus(r)}`} aria-hidden />
              <span className="io-file">{r.name}</span>
              <span className="io-kind">{ROLE[r.role]}</span>
            </span>
            <span className="io-cell" role="cell">
              {[r.state, r.sync].filter(Boolean).join(" · ") || "—"}
            </span>
            <span className="io-cell" role="cell">
              {[
                r.lag_seconds != null ? `${duration(r.lag_seconds * 1000)} behind` : null,
                r.lag_bytes != null ? `${bytes(r.lag_bytes)} ${r.role === "slot" ? "kept" : "to apply"}` : null,
                r.unsent_bytes ? `${bytes(r.unsent_bytes)} unsent` : null,
                r.error,
              ]
                .filter(Boolean)
                .join(" · ") || "—"}
            </span>
          </div>
        ))}
      </div>
    </section>
  );
}

// ── table hot spots ─────────────────────────────────────────────────────────────────────────────

function Fact({ label, value, detail, warn, title }: { label: string; value: string; detail?: ReactNode; warn?: boolean; title?: string }) {
  return (
    <div className={`fact ${warn ? "warn" : ""}`} title={title}>
      <div className="l">{label}</div>
      <div className="v">{value}</div>
      {detail && <div className="d">{detail}</div>}
    </div>
  );
}

/** Facts for a table's header: how updates land, empty space, lock and latch waits, untrusted constraints, histograms. */
export function TableSignalFacts({ report, table }: { report: Report; table: TableStat }) {
  const { snapshot } = report;
  const facts: ReactNode[] = [];
  const s = table.storage ?? null;
  if (s?.hot_updates != null && (table.updates ?? 0) >= 100) {
    const ratio = s.hot_updates / Math.max(1, table.updates ?? 1);
    facts.push(
      <Fact
        key="hot"
        label="HOT updates"
        value={percent(ratio)}
        warn={ratio < 0.5 && (table.updates ?? 0) >= 10_000}
        detail={`fillfactor ${s.fillfactor ?? 100}${s.new_page_updates != null ? ` · ${count(s.new_page_updates)} moved to a new page` : ""}`}
        title="Updates that stayed on their page and wrote no index entries"
      />,
    );
  }
  if (s?.bloat_bytes != null && s.bloat_bytes > 0) {
    const size = table.data_bytes ?? table.total_bytes ?? 0;
    const share = size > 0 ? s.bloat_bytes / size : 0;
    facts.push(
      <Fact
        key="bloat"
        label="Empty space"
        value={`≈ ${bytes(s.bloat_bytes)}`}
        warn={share >= 0.4 && s.bloat_bytes >= 100 * 1024 ** 2}
        detail={`${percent(share)} of the table, estimated`}
        title="Estimated from row counts and column widths in pg_stats"
      />,
    );
  }
  const contended = snapshot.indexes.filter((i) => i.schema === table.schema && i.table === table.name && i.contention);
  if (contended.length) {
    const lockMs = contended.reduce((a, i) => a + i.contention!.row_lock_wait_ms + i.contention!.page_lock_wait_ms, 0);
    const waits = contended.reduce((a, i) => a + i.contention!.row_lock_waits + i.contention!.page_lock_waits, 0);
    const worst = [...contended].sort((a, b) => b.contention!.row_lock_wait_ms - a.contention!.row_lock_wait_ms)[0]!;
    if (waits > 0)
      facts.push(
        <Fact
          key="locks"
          label="Lock waits"
          value={duration(lockMs)}
          warn={lockMs >= 10_000}
          detail={`${count(waits)} waits · most on ${worst.name}`}
          title="Row and page lock waits since the indexes were loaded"
        />,
      );
    const latch = contended.reduce((a, i) => a + i.contention!.page_latch_wait_ms, 0);
    const sequential = contended.find((i) => i.contention!.sequential_key && i.contention!.page_latch_waits > 0);
    if (latch >= 1_000)
      facts.push(
        <Fact
          key="latch"
          label="Page latch waits"
          value={duration(latch)}
          warn={latch >= 10_000 && !!sequential}
          detail={sequential ? `last-page inserts on ${sequential.name}` : "hot pages"}
        />,
      );
  }
  const untrusted = (snapshot.constraints ?? []).filter((c) => c.schema === table.schema && c.table === table.name);
  if (untrusted.length)
    facts.push(
      <Fact
        key="constraints"
        label={snapshot.server.engine === "postgres" ? "Not validated" : "Untrusted"}
        value={`${untrusted.length} constraint${untrusted.length === 1 ? "" : "s"}`}
        warn
        detail={untrusted.map((c) => c.name).join(", ")}
      />,
    );
  if (snapshot.histograms != null) {
    const columns = snapshot.histograms.filter((h) => h.table.toLowerCase() === table.name.toLowerCase());
    // MariaDB keeps statistics for a column with or without a histogram; one without says so by having no kind.
    facts.push(
      <Fact
        key="histograms"
        label={columns.some((h) => h.kind == null) ? "Column statistics" : "Histograms"}
        value={columns.length ? `${columns.length} column${columns.length === 1 ? "" : "s"}` : "none"}
        detail={columns.length ? columns.map((h) => h.column).join(", ") : "the optimizer guesses for unindexed filters"}
      />,
    );
  }
  return <>{facts}</>;
}

// ── finding evidence ────────────────────────────────────────────────────────────────────────────

const CHURN = ["new-connections", "session-length", "sessions-abandoned", "pooled-reuse", "thread-cache-misses", "aborted-connects", "aborted-clients"];

/** The data behind server and object findings, which have no query to show. */
/** The metric the lock-escalations check counts new escalations under, when an earlier scan is there to compare with. */
const ESCALATIONS_SINCE = "Escalations since the previous scan";

/** SQL Server counts escalations from when it started, with no time for each: what the counts cover, and how many came since the scan before. */
function escalationsSince(startedAt: string | null, previousScan: string | undefined, finding: Finding): string {
  const counted = startedAt
    ? `Counted since SQL Server started (${when(startedAt).short}), with no time for each.`
    : "Counted since each index was loaded into memory, with no time for each.";
  const since = finding.metrics.find((m) => m.label === ESCALATIONS_SINCE)?.value;
  if (since === undefined || !previousScan) return `${counted} The next scan says whether any happen after this one.`;
  const at = when(previousScan).short;
  return since > 0 ? `${counted} ${count(since)} since the previous scan (${at}).` : `${counted} None since the previous scan (${at}).`;
}

export function SignalEvidence({ report, finding }: { report: Report; finding: Finding }) {
  const { snapshot } = report;
  const p = snapshot.pressure ?? null;
  switch (finding.check) {
    case "slow-storage":
      return p?.files?.length ? (
        <section>
          <h3>Evidence</h3>
          <StorageLatency files={p.files} engine={snapshot.server.engine} compact />
        </section>
      ) : null;
    case "replica-lag":
      return p?.replicas?.length ? (
        <section>
          <h3>Evidence</h3>
          <Replication replicas={p.replicas} />
        </section>
      ) : null;
    case "connection-churn": {
      const signals = (p?.signals ?? []).filter((s) => CHURN.includes(s.key));
      return signals.length ? (
        <section>
          <h3>Evidence</h3>
          <div className="signal-grid compact">
            {signals.map((s) => (
              <SignalTile key={s.key} signal={s} />
            ))}
          </div>
        </section>
      ) : null;
    }
    case "lock-escalations": {
      const indexes = snapshot.indexes.filter((i) => {
        const c = i.contention;
        return `${i.schema}.${i.table}` === finding.subject.name && c && c.lock_escalations > 0;
      });
      if (!indexes.length) return null;
      const table = indexes[0]!.table;
      return (
        <section>
          <h3>Evidence</h3>
          <div className="io-table" role="table" aria-label={`Lock escalations on ${table}`}>
            <div className="io-row io-th" role="row">
              <span role="columnheader">On {table}</span>
              <span role="columnheader">Table locks</span>
              <span role="columnheader">Lock waits</span>
            </div>
            {indexes.map((i) => {
              const c = i.contention!;
              const waits = c.row_lock_waits + c.page_lock_waits;
              return (
                <div key={i.name} className="io-row" role="row">
                  <span className="io-name" role="cell">
                    {i.name}
                  </span>
                  <span className="io-cell" role="cell">
                    {c.lock_escalations > 0 ? <b>{count(c.lock_escalations)}</b> : "none"}
                  </span>
                  <span className="io-cell" role="cell">
                    {waits > 0 ? `${count(waits)}, ${duration(c.row_lock_wait_ms + c.page_lock_wait_ms)}` : "none"}
                  </span>
                </div>
              );
            })}
          </div>
          <p className="beyond-foot">
            Each table lock replaced one statement's row locks on that index, and every other writer waited for it.{" "}
            {escalationsSince(snapshot.server.started_at, snapshot.escalations_before?.taken_at, finding)}
          </p>
        </section>
      );
    }
    case "hot-rows":
    case "last-page-insert": {
      const index = snapshot.indexes.find((i) => `${i.schema}.${i.table}.${i.name}` === finding.subject.name);
      const c = index?.contention;
      if (!index || !c) return null;
      const rows: [string, number, number][] = [
        ["Row locks", c.row_lock_waits, c.row_lock_wait_ms],
        ["Page locks", c.page_lock_waits, c.page_lock_wait_ms],
        ["Page latches", c.page_latch_waits, c.page_latch_wait_ms],
      ];
      return (
        <section>
          <h3>Evidence</h3>
          <div className="io-table" role="table" aria-label={`Waits on ${index.name}`}>
            <div className="io-row four io-th" role="row">
              <span role="columnheader">On {index.name}</span>
              <span role="columnheader">Waits</span>
              <span role="columnheader">In all</span>
              <span role="columnheader">Average</span>
            </div>
            {rows.map(([label, n, ms]) => (
              <div key={label} className={`io-row four ${n > 0 ? "" : "quiet"}`} role="row">
                <span className="io-name" role="cell">
                  {label}
                </span>
                <span className="io-cell" role="cell">
                  {n > 0 ? count(n) : "none"}
                </span>
                <span className="io-cell" role="cell">
                  {n > 0 ? duration(ms) : "—"}
                </span>
                <span className="io-cell" role="cell">
                  {n > 0 ? <b>{duration(ms / n)}</b> : "—"}
                </span>
              </div>
            ))}
          </div>
          <p className="beyond-foot">
            {c.lock_escalations > 0 ? `${count(c.lock_escalations)} escalations to a table lock. ` : ""}
            {c.sequential_key ? `${index.columns[0] ?? "The key"} only grows, so inserts land on the last page. ` : ""}
            Counted since the index was loaded into memory.
          </p>
        </section>
      );
    }
    default:
      return null;
  }
}
