import { ArrowDown, ArrowUp, Search } from "lucide-react";
import { useEffect, useMemo, useRef, useState } from "react";
import type { Plan } from "../gen/Plan";
import type { PlanNode } from "../gen/PlanNode";
import { duration, percent } from "../lib/format";
import { allNodes, costBasis, estRows, insightsByNode, rowError, rows } from "../lib/plan-format";
import { OpIcon } from "./OpIcon";
import { shortPredicate } from "./PlanView";

type SortKey = "cost" | "subtree" | "est" | "act" | "error" | "runs" | "read" | "time" | "issues";

type Row = {
  node: PlanNode;
  depth: number;
  self: number;
  subtree: number;
  error: number;
  issues: number;
  worst: "high" | "medium" | "low" | null;
};

const COLUMNS: { key: SortKey; label: string; title: string; actual?: boolean }[] = [
  { key: "cost", label: "Self", title: "Share of the plan's work done by this operator itself" },
  { key: "subtree", label: "With inputs", title: "Share including everything below it" },
  { key: "est", label: "Est. rows", title: "Rows the optimizer expected, over all executions" },
  { key: "act", label: "Actual rows", title: "Rows that actually came out", actual: true },
  { key: "error", label: "Off by", title: "How far the estimate was from the actual rows", actual: true },
  { key: "runs", label: "Runs", title: "How many times the operator ran" },
  { key: "read", label: "Read", title: "Rows read, when the engine reports it" },
  { key: "time", label: "Time", title: "Elapsed time, from actual plans", actual: true },
  { key: "issues", label: "Issues", title: "Insights and warnings about this operator" },
];

/** Every operator in one sortable table: where the work goes, where estimates broke, which ones have problems. */
export function PlanOperations({ plan, selectedId, onSelect }: { plan: Plan; selectedId: number | null; onSelect: (id: number) => void }) {
  const [sort, setSort] = useState<{ key: SortKey; desc: boolean }>({ key: "cost", desc: true });
  const [query, setQuery] = useState("");
  const [issuesOnly, setIssuesOnly] = useState(false);
  const body = useRef<HTMLTableSectionElement>(null);
  const total = costBasis(plan);
  const actual = allNodes(plan.root).some((n) => n.act_rows != null);
  const timed = allNodes(plan.root).some((n) => n.act_time_ms != null);
  const flagged = useMemo(() => insightsByNode(plan), [plan]);

  const all = useMemo(() => {
    const out: Row[] = [];
    const visit = (n: PlanNode, depth: number) => {
      const list = flagged.get(n.id) ?? [];
      const err = rowError(n);
      out.push({
        node: n,
        depth,
        self: n.self_cost / total,
        subtree: n.subtree_cost / total,
        error: err ? err.factor : 0,
        issues: list.length + n.warnings.length,
        worst: list[0]?.severity ?? (n.warnings.length ? "low" : null),
      });
      n.children.forEach((c) => visit(c, depth + 1));
    };
    visit(plan.root, 0);
    return out;
  }, [plan, total, flagged]);

  const visible = useMemo(() => {
    const q = query.trim().toLowerCase();
    const value = (r: Row): number => {
      switch (sort.key) {
        case "cost":
          return r.self;
        case "subtree":
          return r.subtree;
        case "est":
          return estRows(r.node);
        case "act":
          return r.node.act_rows ?? -1;
        case "error":
          return r.error;
        case "runs":
          return r.node.act_executions ?? r.node.est_executions;
        case "read":
          return r.node.rows_read ?? -1;
        case "time":
          return r.node.act_time_ms ?? -1;
        case "issues":
          return r.issues * 10 + (r.worst === "high" ? 3 : r.worst === "medium" ? 2 : r.worst ? 1 : 0);
      }
    };
    return all
      .filter((r) => !issuesOnly || r.issues > 0)
      .filter((r) => !q || `${r.node.op} ${r.node.object ?? ""} ${r.node.index ?? ""} ${r.node.predicate ?? ""}`.toLowerCase().includes(q))
      .sort((a, b) => (sort.desc ? value(b) - value(a) : value(a) - value(b)));
  }, [all, sort, query, issuesOnly]);

  useEffect(() => {
    body.current?.querySelector("tr.sel")?.scrollIntoView({ block: "nearest" });
  }, [selectedId]);

  // Columns that would be empty for every operator of this plan are left out, so the useful ones fit.
  const hasRead = all.some((r) => r.node.rows_read != null);
  const hasRuns = all.some((r) => (r.node.act_executions ?? r.node.est_executions) > 1);
  const hasIssues = all.some((r) => r.issues > 0);
  const columns = COLUMNS.filter(
    (c) =>
      (!c.actual || actual) &&
      (c.key !== "time" || timed) &&
      (c.key !== "read" || hasRead) &&
      (c.key !== "runs" || hasRuns) &&
      (c.key !== "issues" || hasIssues),
  );
  const move = (delta: number) => {
    const index = visible.findIndex((r) => r.node.id === selectedId);
    const next = visible[Math.min(visible.length - 1, Math.max(0, index + delta))];
    if (next) onSelect(next.node.id);
  };

  return (
    <div className="ops">
      <div className="ops-tools">
        <label className="search ops-search">
          <Search size={12} />
          <input value={query} placeholder="Filter operators, tables, indexes" onChange={(e) => setQuery(e.target.value)} />
        </label>
        <label className="ops-toggle">
          <input type="checkbox" checked={issuesOnly} onChange={(e) => setIssuesOnly(e.target.checked)} />
          With issues only
        </label>
        <span className="spacer" />
        <span className="muted">
          {visible.length} of {all.length} operators
        </span>
      </div>
      <div
        className="ops-scroll"
        tabIndex={0}
        onKeyDown={(e) => {
          if (e.key === "ArrowDown" || e.key === "j") {
            e.preventDefault();
            move(1);
          } else if (e.key === "ArrowUp" || e.key === "k") {
            e.preventDefault();
            move(-1);
          }
        }}
      >
        <table className="ops-table">
          <thead>
            <tr>
              <th className="ops-op">Operator</th>
              {columns.map((c) => (
                <th
                  key={c.key}
                  className={`r sortable col-${c.key}`}
                  title={c.title}
                  aria-sort={sort.key === c.key ? (sort.desc ? "descending" : "ascending") : "none"}
                  onClick={() => setSort((s) => (s.key === c.key ? { key: c.key, desc: !s.desc } : { key: c.key, desc: true }))}
                >
                  {c.label}
                  {sort.key === c.key && (sort.desc ? <ArrowDown size={11} /> : <ArrowUp size={11} />)}
                </th>
              ))}
            </tr>
          </thead>
          <tbody ref={body}>
            {visible.map((r) => {
              const n = r.node;
              const err = rowError(n);
              const runs = n.act_executions ?? n.est_executions;
              return (
                <tr key={n.id} className={`${n.id === selectedId ? "sel" : ""}`} onClick={() => onSelect(n.id)}>
                  <td className="ops-op">
                    <span className="ops-name">
                      <span className="ops-icon">
                        <OpIcon node={n} size={13} />
                      </span>
                      <span className="ops-text">
                        <b>{n.op}</b>
                        {(n.object || n.index) && (
                          <span className="mono muted">
                            {" "}
                            {n.object?.split(".").pop()}
                            {n.index ? ` · ${n.index}` : ""}
                          </span>
                        )}
                        {(n.seek_predicate || n.predicate) && <span className="ops-pred mono">{shortPredicate(n.seek_predicate ?? n.predicate ?? "")}</span>}
                      </span>
                    </span>
                  </td>
                  {columns.map((c) => (
                    <td key={c.key} className={`r num col-${c.key}`}>
                      {c.key === "cost" && (
                        <span className="ops-cost">
                          <span className="ops-cost-bar">
                            <span style={{ width: `${Math.min(100, r.self * 100)}%` }} />
                          </span>
                          {percent(r.self)}
                        </span>
                      )}
                      {c.key === "subtree" && percent(r.subtree)}
                      {c.key === "est" && rows(estRows(n))}
                      {c.key === "act" && (n.act_rows != null ? rows(n.act_rows) : "—")}
                      {c.key === "error" && (err && err.factor >= 2 ? <span className={`ops-err ${err.factor >= 10 ? "bad" : ""}`}>{`×${rows(err.factor)} ${err.more ? "more" : "fewer"}`}</span> : <span className="muted">—</span>)}
                      {c.key === "runs" && (runs > 1 ? rows(runs) : <span className="muted">1</span>)}
                      {c.key === "read" && (n.rows_read != null ? rows(n.rows_read) : <span className="muted">—</span>)}
                      {c.key === "time" && (n.act_time_ms != null ? duration(n.act_time_ms) : <span className="muted">—</span>)}
                      {c.key === "issues" && (r.issues > 0 ? <span className={`ops-issues ${r.worst ?? ""}`}>{r.issues}</span> : <span className="muted">—</span>)}
                    </td>
                  ))}
                </tr>
              );
            })}
          </tbody>
        </table>
        {visible.length === 0 && <div className="empty" style={{ padding: 24 }}>No operators match.</div>}
      </div>
    </div>
  );
}
