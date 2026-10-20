import { ChevronRight, Search, TriangleAlert } from "lucide-react";
import { useMemo, useState } from "react";
import type { PlanInsight } from "../gen/PlanInsight";
import type { Plan } from "../gen/Plan";
import type { PlanNode } from "../gen/PlanNode";
import { duration, percent } from "../lib/format";
import { cost, estRowsForRuns, exact, misestimate, operatorRuns, propValue } from "../lib/plan-format";
import { OpIcon } from "./OpIcon";
import { shortPredicate } from "./PlanView";

const WARNING_LABEL: Record<string, string> = {
  "implicit-conversion": "Type conversion in a comparison",
  spill: "Spilled to disk",
  "no-join-predicate": "Join without a condition",
  "missing-statistics": "Columns without statistics",
  "memory-grant": "Memory grant warning",
  misestimate: "Actual rows far from the estimate",
  "residual-predicate": "Filters after reading most rows",
};

/** Properties shown in the sections above, so the full list doesn't repeat them. */
const SHOWN = new Set([
  "PhysicalOp",
  "LogicalOp",
  "EstimateRows",
  "EstimatedRowsRead",
  "TableCardinality",
  "EstimatedTotalSubtreeCost",
  "EstimateCPU",
  "EstimateIO",
  "ActualRows",
  "ActualExecutions",
  "ActualRowsRead",
  "EstimateRebinds",
  "EstimateRewinds",
  "OutputList",
  "Rows Removed by Filter",
  "Parent Relationship",
]);

/** How far the actual rows were from the estimate; marked as bad only where the core flagged the operator. */
function ratio(plan: Plan | undefined, n: PlanNode) {
  const factor = misestimate(plan, n);
  if (factor == null || Math.abs(factor) < 2) return null;
  return <span className={`nd-ratio ${n.warnings.includes("misestimate") ? "bad" : ""}`}>{`×${exact(Math.round(Math.abs(factor)))} ${factor > 0 ? "more than expected" : "fewer than expected"}`}</span>;
}

/** One operator, explained: what it does, its rows and cost, what's wrong with it, then every raw property on demand. */
export function NodeDetails({
  plan,
  node,
  share,
  subtreeShare,
  noShare,
  insights = [],
  estimates = true,
  actual = false,
}: {
  plan?: Plan;
  node: PlanNode;
  /** Its own share of the work; null where the plan can't state one (see `stepShare`). */
  share: number | null;
  subtreeShare?: number | null;
  /** Why there is no share, when there isn't one. */
  noShare?: string | null;
  insights?: PlanInsight[];
  /** The plan weighs its steps (costs or measured time); SQLite's has the access path only. */
  estimates?: boolean;
  /** An actual plan: the rows it read were counted, not estimated. */
  actual?: boolean;
}) {
  const [filter, setFilter] = useState("");
  const props = useMemo(() => {
    const q = filter.trim().toLowerCase();
    return Object.entries(node.props)
      .filter(([k, v]) => v != null && v !== "" && !SHOWN.has(k))
      .filter(([k, v]) => !q || k.toLowerCase().includes(q) || v.toLowerCase().includes(q))
      .sort(([a], [b]) => a.localeCompare(b));
  }, [node, filter]);
  const cpu = Number(node.props.EstimateCPU ?? NaN);
  const io = Number(node.props.EstimateIO ?? NaN);
  const split = Number.isFinite(cpu) && Number.isFinite(io) && cpu + io > 0 ? cpu / (cpu + io) : null;
  const removed = node.props["Rows Removed by Filter"];
  const executions = operatorRuns(plan, node) ?? 0;

  return (
    <div className="nd">
      <div className="nd-head">
        <span className="nd-icon">
          <OpIcon node={node} size={16} />
        </span>
        <div className="nd-title">
          <div className="nd-op">
            {node.op}
            {node.detail && <span className="nd-detail">{node.detail}</span>}
          </div>
          {(node.object || node.index) && (
            <div className="nd-obj mono">
              {node.object}
              {node.index && <span className="muted"> · {node.index}</span>}
            </div>
          )}
        </div>
        {estimates &&
          (share != null ? (
            <span className="nd-share" title="Share of the plan's work done by this operator itself">
              {percent(share)}
              <span className="muted"> of the work</span>
            </span>
          ) : (
            <span className="nd-share muted" title={noShare ?? undefined}>
              no share of the work
            </span>
          ))}
      </div>

      {insights.length > 0 && (
        <ul className="nd-insights">
          {insights.map((i) => (
            <li key={i.rule} className={i.severity}>
              <i />
              {i.title}
            </li>
          ))}
        </ul>
      )}

      <div className="nd-grid">
        {(node.seek_predicate || node.predicate) && (
          <section className="nd-card wide">
            <h4>What it does</h4>
            {node.seek_predicate && (
              <div className="nd-cond">
                <span className="nd-k">{node.kind === "seek" || node.kind === "lookup" ? "Seeks on" : "Index condition"}</span>
                <code>{shortPredicate(node.seek_predicate)}</code>
              </div>
            )}
            {node.predicate && (
              <div className="nd-cond">
                <span className="nd-k">Then filters</span>
                <code>{shortPredicate(node.predicate)}</code>
              </div>
            )}
          </section>
        )}
        {estimates && (
        <section className="nd-card">
          <h4>Rows</h4>
          <dl>
            <dt>Estimated</dt>
            <dd title={node.act_executions != null ? "Rows per run as estimated, times the runs that happened" : undefined}>{exact(estRowsForRuns(plan, node))}</dd>
            {node.act_rows != null && (
              <>
                <dt>Actual</dt>
                <dd>
                  {exact(node.act_rows)} {ratio(plan, node)}
                </dd>
              </>
            )}
            {executions > 1 && (
              <>
                <dt>{node.act_executions != null ? "Ran" : "Expected runs"}</dt>
                <dd>{exact(executions)} times</dd>
              </>
            )}
            {node.rows_read != null && (
              <>
                <dt>{actual ? "Read" : "Expected to read"}</dt>
                <dd>{exact(node.rows_read)}</dd>
              </>
            )}
            {removed && (
              <>
                <dt>Thrown away by the filter</dt>
                <dd>{propValue(removed)}</dd>
              </>
            )}
            {node.table_rows != null && (
              <>
                <dt>In the table</dt>
                <dd>{exact(node.table_rows)}</dd>
              </>
            )}
          </dl>
        </section>
        )}
        {estimates && (
        <section className="nd-card">
          <h4>Cost</h4>
          <dl>
            <dt>This operator</dt>
            <dd>
              {share != null ? percent(share) : "no share"} <span className="muted">· {cost(node.self_cost)} units</span>
            </dd>
            {subtreeShare != null && (
              <>
                <dt>With its inputs</dt>
                <dd>{percent(subtreeShare)}</dd>
              </>
            )}
            {node.act_time_ms != null && (
              <>
                <dt>Actual time</dt>
                <dd>{duration(node.act_time_ms)}</dd>
              </>
            )}
          </dl>
          {share == null && noShare && <p className="muted" style={{ margin: "6px 0 0", fontSize: 12 }}>{noShare}</p>}
          {split != null && (
            <div className="nd-split" title={`CPU ${percent(split)} · I/O ${percent(1 - split)} of the estimate`}>
              <div className="nd-split-bar">
                <span className="cpu" style={{ width: `${split * 100}%` }} />
                <span className="io" style={{ width: `${(1 - split) * 100}%` }} />
              </div>
              <div className="nd-split-legend">
                <span>
                  <i className="cpu" />
                  CPU {percent(split)}
                </span>
                <span>
                  <i className="io" />
                  I/O {percent(1 - split)}
                </span>
              </div>
            </div>
          )}
        </section>
        )}
        {node.warnings.length > 0 && (
          <section className="nd-card nd-warn">
            <h4>
              <TriangleAlert size={12} /> Warnings
            </h4>
            <ul>
              {node.warnings.map((w) => (
                <li key={w}>{WARNING_LABEL[w] ?? w}</li>
              ))}
            </ul>
          </section>
        )}
      </div>

      {Object.keys(node.props).length > 0 && (
        <details className="nd-all">
          <summary>
            <ChevronRight size={13} className="chev" /> All properties <span className="muted">{props.length}</span>
          </summary>
          <label className="search nd-search">
            <Search size={12} />
            <input value={filter} placeholder="Find a property" onChange={(e) => setFilter(e.target.value)} />
          </label>
          <dl className="nd-props">
            {props.map(([k, v]) => (
              <div key={k} className="nd-prop">
                <dt>{k}</dt>
                <dd title={v.length > 80 ? v : undefined}>{propValue(v)}</dd>
              </div>
            ))}
          </dl>
        </details>
      )}
    </div>
  );
}
