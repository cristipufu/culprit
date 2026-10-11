import { Clock, Cpu, Database, Gauge, HardDrive, Scale, TriangleAlert, Users } from "lucide-react";
import type { ReactNode } from "react";
import type { Plan } from "../gen/Plan";
import { actualFacts } from "../lib/actual";
import { count, duration, percent, when } from "../lib/format";

function kb(value: number): string {
  if (value >= 1024 * 1024) return `${(value / 1024 / 1024).toFixed(1)} GB`;
  if (value >= 1024) return `${(value / 1024).toFixed(value >= 10 * 1024 ? 0 : 1)} MB`;
  return `${Math.round(value)} KB`;
}

function where(node: { op: string; object: string | null }): string {
  const table = node.object?.split(".").pop();
  return table ? `${node.op} on ${table}` : node.op;
}

function Stat({ icon, value, label, title }: { icon: ReactNode; value: string; label: string; title?: string }) {
  return (
    <span className="as-stat" title={title}>
      {icon}
      <b>{value}</b>
      <span>{label}</span>
    </span>
  );
}

/**
 * What a measured run cost, in words: how long it took, what it read, how much memory it was given and used, and the
 * operators that explain the time (slowest step, worst estimate, a worker doing most of the work, a spill).
 * Each callout selects its operator in the diagram.
 */
export function ActualSummary({ plan, onPick }: { plan: Plan; onPick?: (id: number) => void }) {
  const f = actualFacts(plan);
  if (!f) return null;
  const memory = f.grantedKb != null && f.grantedKb > 0 ? { granted: f.grantedKb, used: f.usedKb ?? 0 } : null;
  const idle = memory ? 1 - memory.used / memory.granted : 0;
  const callouts: { key: string; tone: "hot" | "warn" | "info"; icon: ReactNode; text: ReactNode; node?: number; title: string }[] = [];
  if (f.slowest && f.slowest.share >= 0.2) {
    callouts.push({
      key: "slowest",
      tone: "hot",
      icon: <Gauge size={13} />,
      text: (
        <>
          Most time: <b>{where(f.slowest.node)}</b> · {duration(f.slowest.ms)} ({percent(f.slowest.share)})
        </>
      ),
      node: f.slowest.node.id,
      title: "The step that spent the most time on its own work, not counting its inputs",
    });
  }
  if (f.worstEstimate) {
    const n = f.worstEstimate.node;
    const expected = n.est_rows * Math.max(1, n.est_executions);
    callouts.push({
      key: "estimate",
      tone: "warn",
      icon: <Scale size={13} />,
      text: (
        <>
          Expected {count(expected)} rows, got <b>{count(n.act_rows ?? 0)}</b> at <b>{where(n)}</b> · ×{count(Math.abs(f.worstEstimate.factor))} off
        </>
      ),
      node: f.worstEstimate.node.id,
      title: "The optimizer chose this plan for a row count that turned out wrong; that is often why a plan is slow",
    });
  }
  if (f.skew) {
    callouts.push({
      key: "skew",
      tone: "warn",
      icon: <Users size={13} />,
      text: (
        <>
          One of {f.skew.threads} workers did {percent(f.skew.busiest)} of <b>{where(f.skew.node)}</b>
        </>
      ),
      node: f.skew.node.id,
      title: "Parallel work was uneven, so the other workers mostly waited",
    });
  }
  if (f.spilled) {
    callouts.push({
      key: "spill",
      tone: "warn",
      icon: <HardDrive size={13} />,
      text: f.spillPages > 0 ? <>Ran out of memory and wrote {count(f.spillPages)} pages to disk</> : <>Ran out of memory and spilled to disk</>,
      title: "A sort or hash didn't fit in its memory grant and used tempdb / temp files instead",
    });
  }
  return (
    <div className="actual-summary">
      <div className="as-head">
        <span className="as-kicker">{f.lastRunAt ? "Last run" : "Measured run"}</span>
        {f.elapsedMs != null && (
          <span className="as-elapsed" title="Elapsed time of the statement">
            <Clock size={14} />
            {duration(f.elapsedMs)}
          </span>
        )}
        <span className="as-stats">
          {f.cpuMs != null && <Stat icon={<Cpu size={12} />} value={duration(f.cpuMs)} label="CPU" title={f.workers ? `Across ${f.workers} worker threads` : undefined} />}
          {f.logicalReads != null && (
            <Stat
              icon={<Database size={12} />}
              value={count(f.logicalReads)}
              label={f.physicalReads ? `reads · ${count(f.physicalReads)} from disk` : "reads"}
              title="Pages (SQL Server) or buffers (Postgres) read, from memory or disk"
            />
          )}
          {f.rows != null && <Stat icon={null} value={count(f.rows)} label={f.rows === 1 ? "row" : "rows"} />}
          {f.workers != null && f.workers > 1 && <Stat icon={<Users size={12} />} value={String(f.workers)} label="workers" />}
        </span>
        {f.lastRunAt && (
          <span className="as-when" title={when(f.lastRunAt.endsWith("Z") ? f.lastRunAt : `${f.lastRunAt}Z`).full}>
            {when(f.lastRunAt.endsWith("Z") ? f.lastRunAt : `${f.lastRunAt}Z`).short}
          </span>
        )}
      </div>
      {memory && (
        <div className="as-memory" title="Memory SQL Server reserved for sorts and hashes, and how much the run actually used">
          <span className="as-label">Memory</span>
          <span className="as-bar" aria-hidden>
            <span style={{ width: `${Math.min(100, (memory.used / memory.granted) * 100)}%` }} />
          </span>
          <span>
            used <b>{kb(memory.used)}</b> of {kb(memory.granted)} granted
            {idle >= 0.75 && memory.granted >= 10 * 1024 && <span className="muted"> · {percent(idle)} sat idle, other queries may have waited for it</span>}
          </span>
        </div>
      )}
      {!f.timed && (
        <div className="as-note">
          <TriangleAlert size={12} /> Actual row counts only: {f.lastRunAt ? "SQL Server keeps rows, not time, for past runs" : "this run reported no per-step time"}. Shares are of the estimated cost.
        </div>
      )}
      {callouts.length > 0 && (
        <div className="as-callouts">
          {callouts.map((c) =>
            c.node != null && onPick ? (
              <button key={c.key} className={`as-callout ${c.tone}`} title={c.title} onClick={() => onPick(c.node!)}>
                {c.icon}
                <span>{c.text}</span>
              </button>
            ) : (
              <span key={c.key} className={`as-callout ${c.tone}`} title={c.title}>
                {c.icon}
                <span>{c.text}</span>
              </span>
            ),
          )}
        </div>
      )}
    </div>
  );
}
