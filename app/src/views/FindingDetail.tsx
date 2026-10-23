import { actionLabel, isActionable } from "../lib/findings";
import { ArrowUpRight, Bot, Check, Copy, EyeOff, Flame, History, SquarePen } from "lucide-react";
import { useLayoutEffect, useRef, useState } from "react";
import { SignalEvidence } from "../components/BeyondTime";
import { FindingShareMenu, VerifyButton, VerifyResult, useVerify } from "../components/FindingShare";
import { PlanView, shortPredicate } from "../components/PlanView";
import { Prose } from "../components/Prose";
import { Sql } from "../components/Sql";
import type { Finding } from "../gen/Finding";
import type { Plan } from "../gen/Plan";
import type { PlanNode } from "../gen/PlanNode";
import type { QueryStat } from "../gen/QueryStat";
import type { Report } from "../gen/Report";
import { api } from "../lib/api";
import { ago, count, duration, findingMetric, percent, rowCount } from "../lib/format";
import { useHotkeys } from "../lib/hotkeys";
import { aboutThePlan, doesRealWork, estRows, findingNoShareReason, findingStepShare, hottestStep, rowsOut, rowsRead, stepShare } from "../lib/plan-format";
import { figures, honest } from "../lib/rates";
import { setState, toast, toggleDismissed, useStore, windowMinutesOf } from "../lib/store";
import { copyAndTell } from "../lib/clipboard";
import { canOpenFixInEditor } from "../lib/editor-sql";
import { can } from "../lib/engines";

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
    const brief = await (await api()).brief(report.snapshot.connection, finding.id, windowMinutesOf(report.snapshot.options));
    await copyAndTell(brief, "Brief copied. Paste it into your coding agent");
  };
  const canOpenFix = canOpenFixInEditor(finding.fix?.sql, report.snapshot.server.engine);
  const openInEditor = () => {
    const sql = finding.fix?.sql;
    if (!sql || !canOpenFix) return;
    setState({ view: "editor", editorSql: sql });
  };

  useHotkeys({
    c: copyFix,
    a: copyBrief,
    ...(canOpenFix ? { e: openInEditor } : {}),
    d: () => toggleDismissed(finding.id),
  });

  return (
    <div className="detail">
      <div className="detail-inner">
        <div className="crumbs">
          <span className={`tag ${isActionable(finding) ? finding.severity : ""}`}>{actionLabel(finding)}</span>
          {isActionable(finding) && <span className={`tag ${finding.severity}`}>{SEVERITY_LABEL[finding.severity]}</span>}
          <span className="tag">{CONFIDENCE_LABEL[finding.confidence]}</span>
          <CopyId id={finding.id} />
        </div>
        {finding.inactive && (
          <div className="notice" style={{ marginTop: 12 }}>
            <History size={15} style={{ flexShrink: 0, marginTop: 1 }} />
            <div>
              <StoppedRunning report={report} lastSeen={finding.last_seen} /> Check the current version of this code path before acting.
            </div>
          </div>
        )}
        <h2 className="selectable">{finding.title}</h2>
        <p className="summary selectable">
          <Prose text={finding.summary} />
        </p>

        {finding.metrics.length > 0 && (
          <section>
            <div className="metrics finding-metrics">
              {finding.metrics.slice(0, 6).map((m) => (
                <div className={`metric ${m.unit === "text" ? "metric-text" : ""}`} key={m.label}>
                  {m.unit === "text" && <div className="label">{m.label}</div>}
                  <div className={`value ${m.unit === "text" ? "selectable" : ""}`}>{findingMetric(m)}</div>
                  {m.unit !== "text" && <div className="label">{m.label}</div>}
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
                <HotNode
                  plan={firstWithPlan.query.plan}
                  query={firstWithPlan.query}
                  focus={firstWithPlan.ref.plan_node}
                  aboutThePlan={aboutThePlan(firstWithPlan.query.plan, finding.check)}
                  weighed={can(firstWithPlan.query.plan.engine, "plan_estimates") || firstWithPlan.query.plan.kind === "actual"}
                />
                <button className="btn ghost" style={{ marginTop: 8 }} onClick={() => setShowPlan((v) => !v)}>
                  {showPlan ? "Hide plan" : "Show full plan"}
                </button>
                {showPlan && (
                  <div style={{ marginTop: 8 }}>
                    <PlanView plan={firstWithPlan.query.plan} query={firstWithPlan.query} focus={firstWithPlan.ref.plan_node} compact />
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
              <div className="codeblock fix-sql">
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
          {canOpenFix && (
            <button className="btn" onClick={openInEditor}>
              <SquarePen size={13} /> Open in editor <span className="kbd">E</span>
            </button>
          )}
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
  // Share, runs and average as the report states them: over Culprit's scans where they measured it.
  const stated = w ? figures(report, query) : null;
  const share = w && w.total_time_ms > 0 ? stated!.share : null;
  // Per day only over a long enough window; a few minutes of data stays a measured count, and a burst (or fewer runs
  // than days) its count with when it ran.
  const rates = w ? honest(w) : null;
  const ran = stated?.said ?? null;
  return (
    <div
      className="qcard"
      role="button"
      tabIndex={0}
      onClick={() => setState({ view: "queries", query: query.id })}
      onKeyDown={(e) => e.key === "Enter" && setState({ view: "queries", query: query.id })}
    >
      <SqlPreview text={query.text} />
      <div className="qmeta" title={stated?.over ? `Over ${stated.over}${stated.totals ? ` (${stated.totals})` : ""}` : undefined}>
        {share != null && (
          <span>
            <b>{percent(share)}</b> of DB time
          </span>
        )}
        <span>
          <b>{count(stated ? stated.runs : query.executions)}</b>{" "}
          {ran != null ? ran.replace(/^\S+ /, "") : (stated?.perDay ?? rates?.perDay) ? "runs/day" : "runs"}
        </span>
        <span>
          avg <b>{duration(stated?.mean ?? query.mean_time_ms)}</b>
        </span>
        {stated?.over && <span className="muted">over {stated.over}</span>}
        {query.object_name && <span className="mono">{query.object_name}</span>}
        <span className="open muted">
          {node != null ? "Open plan" : "Open query"} <ArrowUpRight size={12} />
        </span>
      </div>
    </div>
  );
}

/** When an inactive finding's statements last ran, the way the findings list says it (Overview's `lastRan`). Unchanged
 * since Culprit's first scan of a source that keeps no last run, that scan is only the latest they can have run: they may
 * have stopped long before it, and the deploy that changed them may be an earlier one (culprit_checks'
 * `last_ran_by_first_scan`). */
function StoppedRunning({ report, lastSeen }: { report: Report; lastSeen: string | null }) {
  const workload = report.snapshot.workload;
  const firstScan = workload && !workload.windowed ? (workload.timeline?.starts[0] ?? null) : null;
  const first = firstScan != null ? Date.parse(firstScan) : NaN;
  if (lastSeen != null && Math.abs(first - Date.parse(lastSeen)) < 1000) {
    return (
      <>
        <b>Not run since Culprit's first scan, {ago(firstScan)}.</b> These queries may have stopped before it: a deploy before that scan may have changed them.
      </>
    );
  }
  return (
    <>
      <b>{lastSeen ? `Last ran ${ago(lastSeen)}.` : "Stopped running."}</b> These queries no longer run: a deploy may have changed them.
    </>
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

/** The step to look at, as the brief picks it (culprit_checks::finding_step): the one the finding names, else, for a
 *  finding about where its plan does its work, the costliest step (never an exchange) in a plan that does real work and
 *  can state its share. A finding about something else (a result's size, how often a statement runs) points at no
 *  step. A plan without costs or row estimates (SQLite's) names only the step the finding points at, with what it
 *  searches by, and claims no share or estimate. */
function HotNode({ plan, query, focus, weighed, aboutThePlan }: { plan: Plan; query: QueryStat; focus: number | null; weighed: boolean; aboutThePlan: boolean }) {
  const named = focus != null ? find(plan.root, focus) : null;
  if (!weighed) {
    if (!named) return null;
    const by = named.seek_predicate ?? named.predicate;
    return (
      <div className="hotnode">
        <Flame size={16} color="var(--heat-4)" style={{ marginTop: 1, flexShrink: 0 }} />
        <div className="selectable">
          <div>
            <span className="op">{named.op}</span>
            {named.object && <span className="mono"> {named.object}</span>}
            {named.index && <span className="muted"> · {named.index}</span>}
          </div>
          {by && <div className="muted" style={{ marginTop: 2 }}>by {shortPredicate(by)}</div>}
        </div>
      </div>
    );
  }
  const hot = aboutThePlan && doesRealWork(plan) ? hottestStep(plan) : null;
  const node = named ?? (hot && stepShare(plan, hot) != null ? hot : null);
  if (!node) return null;
  // A share only where the plan can state one, in a plan that does real work; otherwise why not.
  const share = findingStepShare(plan, node);
  const why = share == null ? findingNoShareReason(plan, node, query) : null;
  const read = rowsRead(plan, node);
  // Counted only by an actual plan whose engine reported it; otherwise the optimizer's estimate or the table's size.
  const counted = plan.kind === "actual" && node.rows_read != null;
  const returned = node.act_rows != null ? count(node.act_rows) : `~${count(Math.max(1, Math.round(rowsOut(plan, node))))}`;
  return (
    <div className="hotnode">
      <Flame size={16} color="var(--heat-4)" style={{ marginTop: 1, flexShrink: 0 }} />
      <div className="selectable">
        <div>
          <span className="op">{node.op}</span>
          {node.object && <span className="mono"> {node.object}</span>}
          {share != null && <span className="muted"> · {percent(share)} of the plan's cost</span>}
        </div>
        <div className="muted" style={{ marginTop: 2 }}>
          {read != null ? `${counted ? "Reads" : "Expected to read"} ${rowCount(read)} to return ${returned}` : node.act_rows != null ? `Returns ${rowCount(node.act_rows)}` : `Estimates ${rowCount(estRows(plan, node))}`}
          {node.predicate ? ` · filter ${shortPredicate(node.predicate)}` : ""}
        </div>
        {why && (
          <div className="muted" style={{ marginTop: 2 }}>
            {why}
          </div>
        )}
      </div>
    </div>
  );
}
