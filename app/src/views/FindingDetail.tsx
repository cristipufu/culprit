import { ArrowUpRight, Bot, Check, Copy, EyeOff, Flame, History, SquarePen } from "lucide-react";
import { useLayoutEffect, useRef, useState } from "react";
import { SignalEvidence } from "../components/BeyondTime";
import { FindingShareMenu, VerifyButton, VerifyResult, useVerify } from "../components/FindingShare";
import { PlanView, shortPredicate } from "../components/PlanView";
import { Prose } from "../components/Prose";
import { Sql } from "../components/Sql";
import type { Finding } from "../gen/Finding";
import type { PlanNode } from "../gen/PlanNode";
import type { QueryStat } from "../gen/QueryStat";
import type { Report } from "../gen/Report";
import { api } from "../lib/api";
import { ago, count, duration, metric, percent, rowCount } from "../lib/format";
import { useHotkeys } from "../lib/hotkeys";
import { honest } from "../lib/rates";
import { setState, toast, toggleDismissed, useStore } from "../lib/store";
import { copyAndTell } from "../lib/clipboard";

const CONFIDENCE_LABEL = { high: "High confidence", medium: "Medium confidence", low: "Low confidence" } as const;
const SEVERITY_LABEL = { high: "High impact", medium: "Medium impact", low: "Low impact" } as const;

export function FindingDetail({ report, finding }: { report: Report; finding: Finding }) {
  const dismissed = useStore((s) => !!s.dismissed[finding.id]);
  const workload = report.snapshot.workload;
  const queries = finding.queries
    .map((ref) => ({ ref, query: workload?.queries.find((q) => q.id === ref.id) }))
    .filter((x): x is { ref: (typeof finding.queries)[number]; query: QueryStat } => !!x.query);
  const [showPlan, setShowPlan] = useState(false);
  const verify = useVerify(report, finding);
  const firstWithPlan = queries.find((q) => q.query.plan);

  const copyFix = async () => {
    if (!finding.fix?.sql) return toast("This finding has no script to copy");
    await copyAndTell(finding.fix.sql, "Fix copied");
  };
  const copyBrief = async () => {
    const brief = await (await api()).brief(report.snapshot.connection, finding.id, report.snapshot.options?.window_days ?? null);
    await copyAndTell(brief, "Brief copied. Paste it into your coding agent");
  };
  const openInEditor = () => {
    const sql = finding.fix?.sql ?? queries[0]?.query.text;
    if (!sql) return toast("Nothing to open");
    setState({ view: "editor", editorSql: sql });
  };

  useHotkeys({
    c: copyFix,
    a: copyBrief,
    e: openInEditor,
    d: () => toggleDismissed(finding.id),
  });

  return (
    <div className="detail">
      <div className="detail-inner">
        <div className="crumbs">
          <span className={`tag ${finding.severity}`}>{SEVERITY_LABEL[finding.severity]}</span>
          <span className="tag">{CONFIDENCE_LABEL[finding.confidence]}</span>
          <CopyId id={finding.id} />
        </div>
        {finding.inactive && (
          <div className="notice" style={{ marginTop: 12 }}>
            <History size={15} style={{ flexShrink: 0, marginTop: 1 }} />
            <div>
              <b>Stopped running {finding.last_seen ? ago(finding.last_seen) : ""}.</b> These queries no longer run, most likely changed by a deploy. Check
              the current version of this code path before acting.
            </div>
          </div>
        )}
        <h2 className="selectable">{finding.title}</h2>
        <p className="summary selectable">
          <Prose text={finding.summary} />
        </p>

        {finding.metrics.length > 0 && (
          <section>
            <div className="metrics">
              {finding.metrics.slice(0, 6).map((m) => (
                <div className="metric" key={m.label}>
                  <div className="value">{metric(m.value, m.unit, m.text)}</div>
                  <div className="label">{m.label}</div>
                </div>
              ))}
            </div>
          </section>
        )}

        <SignalEvidence report={report} finding={finding} />

        {queries.length > 0 && (
          <section>
            <h3>
              Evidence <span className="muted">{queries.length === 1 ? "1 query" : `${queries.length} queries`}</span>
            </h3>
            {queries.map(({ ref, query }) => (
              <QueryCard key={query.id} report={report} query={query} node={ref.plan_node} />
            ))}
            {firstWithPlan?.query.plan && (
              <>
                <HotNode plan={firstWithPlan.query.plan.root} focus={firstWithPlan.ref.plan_node} total={firstWithPlan.query.plan.total_cost} />
                <button className="btn ghost" style={{ marginTop: 8 }} onClick={() => setShowPlan((v) => !v)}>
                  {showPlan ? "Hide plan" : "Show full plan"}
                </button>
                {showPlan && (
                  <div style={{ marginTop: 8 }}>
                    <PlanView plan={firstWithPlan.query.plan} focus={firstWithPlan.ref.plan_node} compact />
                  </div>
                )}
              </>
            )}
          </section>
        )}

        {finding.fix && (
          <section>
            <h3>
              Fix
              {finding.fix.sql && (
                <span className="actions">
                  <button className="btn" onClick={copyFix}>
                    <Copy size={13} /> Copy <span className="kbd">C</span>
                  </button>
                </span>
              )}
            </h3>
            <p className="fix-summary selectable">
              <Prose text={finding.fix.summary} />
            </p>
            {finding.fix.sql && (
              <div className="codeblock">
                <Sql text={finding.fix.sql} pretty={false} />
              </div>
            )}
            {finding.fix.cost && <div className="fix-cost">Cost: {finding.fix.cost}</div>}
          </section>
        )}

        {finding.verify.length > 0 && (
          <section>
            <h3>How to verify</h3>
            <ol className="steps selectable">
              {finding.verify.map((v) => (
                <li key={v}>
                  <Prose text={v} />
                </li>
              ))}
            </ol>
          </section>
        )}

        {finding.caveats.length > 0 && (
          <section>
            <h3>Before you act</h3>
            <ul className="caveats selectable">
              {finding.caveats.map((c) => (
                <li key={c} style={{ whiteSpace: "pre-wrap" }}>
                  <Prose text={c} />
                </li>
              ))}
            </ul>
          </section>
        )}

        <div className="actionbar">
          <VerifyResult result={verify.result} report={report} onClose={verify.clear} />
          <button className="btn primary" onClick={copyBrief}>
            <Bot size={14} /> Copy for agent <span className="kbd">A</span>
          </button>
          <button className="btn" onClick={openInEditor}>
            <SquarePen size={13} /> Open in editor <span className="kbd">E</span>
          </button>
          <VerifyButton running={verify.result?.status === "running"} onRun={verify.run} />
          <FindingShareMenu report={report} finding={finding} />
          <span className="spacer" />
          <button className="btn ghost dismiss" onClick={() => toggleDismissed(finding.id)} aria-label={dismissed ? "Restore" : "Dismiss"} title={`${dismissed ? "Restore" : "Dismiss"} (D)`}>
            <EyeOff size={13} /> <span className="btn-label">{dismissed ? "Restore" : "Dismiss"}</span> <span className="kbd">D</span>
          </button>
        </div>
      </div>
    </div>
  );
}

function CopyId({ id }: { id: string }) {
  const [copied, setCopied] = useState(false);
  return (
    <button
      className="tag mono"
      title="Copy finding id (for culprit show)"
      onClick={async () => {
        if (!(await copyAndTell(id))) return;
        setCopied(true);
        setTimeout(() => setCopied(false), 1200);
      }}
    >
      {copied ? <Check size={11} /> : null}
      {id.length > 60 ? `${id.slice(0, 60)}…` : id}
    </button>
  );
}

/** The statement in at most five lines; a longer one fades out over its last line and offers the rest in place. */
function SqlPreview({ text }: { text: string }) {
  const box = useRef<HTMLDivElement>(null);
  const [clipped, setClipped] = useState(false);
  const [open, setOpen] = useState(false);
  // Highlighting and fonts settle after the first layout, so the check runs whenever the statement's size changes.
  useLayoutEffect(() => {
    const el = box.current;
    if (!el || open) return;
    const check = () => setClipped(el.scrollHeight > el.clientHeight + 1);
    check();
    const observer = new ResizeObserver(check);
    if (el.firstElementChild) observer.observe(el.firstElementChild);
    return () => observer.disconnect();
  }, [text, open]);
  return (
    <>
      <div ref={box} className={`sql ${open ? "open" : clipped ? "clipped" : ""}`}>
        <Sql text={text} />
      </div>
      {(clipped || open) && (
        <button
          className="sql-more"
          aria-expanded={open}
          onClick={(e) => {
            e.stopPropagation();
            setOpen((o) => !o);
          }}
          onKeyDown={(e) => e.stopPropagation()}
        >
          {open ? "Show less" : "Show all"}
        </button>
      )}
    </>
  );
}

function QueryCard({ report, query, node }: { report: Report; query: QueryStat; node: number | null }) {
  const w = report.snapshot.workload;
  const share = w && w.total_time_ms > 0 ? query.total_time_ms / w.total_time_ms : null;
  // Per day only over a long enough window; a few minutes of data stays a measured count.
  const rates = w ? honest(w) : null;
  return (
    <div
      className="qcard"
      role="button"
      tabIndex={0}
      onClick={() => setState({ view: "queries", query: query.id })}
      onKeyDown={(e) => e.key === "Enter" && setState({ view: "queries", query: query.id })}
    >
      <SqlPreview text={query.text} />
      <div className="qmeta">
        {share != null && (
          <span>
            <b>{percent(share)}</b> of DB time
          </span>
        )}
        <span>
          <b>{count(rates ? rates.value(query.executions) : query.executions)}</b> {rates?.perDay ? "runs/day" : "runs"}
        </span>
        <span>
          avg <b>{duration(query.mean_time_ms)}</b>
        </span>
        {query.object_name && <span className="mono">{query.object_name}</span>}
        <span className="open muted">
          {node != null ? "Open plan" : "Open query"} <ArrowUpRight size={12} />
        </span>
      </div>
    </div>
  );
}

function find(node: PlanNode, id: number): PlanNode | null {
  if (node.id === id) return node;
  for (const c of node.children) {
    const hit = find(c, id);
    if (hit) return hit;
  }
  return null;
}

function hottest(node: PlanNode): PlanNode {
  return node.children.map(hottest).reduce((a, b) => (b.self_cost > a.self_cost ? b : a), node);
}

function HotNode({ plan, focus, total }: { plan: PlanNode; focus: number | null; total: number }) {
  const node = (focus != null ? find(plan, focus) : null) ?? hottest(plan);
  const share = total > 0 ? node.self_cost / total : 0;
  const read = node.rows_read ?? node.table_rows;
  return (
    <div className="hotnode">
      <Flame size={16} color="var(--heat-4)" style={{ marginTop: 1, flexShrink: 0 }} />
      <div className="selectable">
        <div>
          <span className="op">{node.op}</span>
          {node.object && <span className="mono"> {node.object}</span>}
          <span className="muted"> · {percent(share)} of the plan's cost</span>
        </div>
        <div className="muted" style={{ marginTop: 2 }}>
          {read != null ? `Reads ${rowCount(read)}` : `Estimates ${rowCount(node.est_rows)}`}
          {read != null && ` to return ~${count(Math.max(1, Math.round(node.est_rows)))}`}
          {node.predicate ? ` · filter ${shortPredicate(node.predicate)}` : ""}
        </div>
      </div>
    </div>
  );
}
