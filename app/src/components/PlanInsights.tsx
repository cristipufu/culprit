import { ChartColumn, ChevronDown, CircleCheck, Copy, Cpu, Crosshair, Info, KeyRound, ListChecks, MemoryStick, Server, SquareCode, type LucideIcon } from "lucide-react";
import { useEffect, useMemo, useRef, useState } from "react";
import type { InsightArea } from "../gen/InsightArea";
import type { Plan } from "../gen/Plan";
import type { PlanInsight } from "../gen/PlanInsight";
import type { Severity } from "../gen/Severity";

import { percent } from "../lib/format";
import { bySeverity } from "../lib/plan-format";

import { Prose } from "./Prose";
import { Sql } from "./Sql";
import { copyAndTell } from "../lib/clipboard";

const AREA: Record<InsightArea, { icon: LucideIcon; label: string }> = {
  index: { icon: KeyRound, label: "Index" },
  query: { icon: SquareCode, label: "Query" },
  statistics: { icon: ChartColumn, label: "Statistics" },
  memory: { icon: MemoryStick, label: "Memory" },
  parallelism: { icon: Cpu, label: "Parallelism" },
  server: { icon: Server, label: "Setting" },
};

const SEVERITY_LABEL: Record<Severity, string> = { high: "High", medium: "Medium", low: "Low" };
const SHOWN = 4;

/**
 * What's wrong with this plan, most important first: one sentence each, the share of the plan's work it touches, and
 * (expanded) why, the fix with SQL to copy, and how to check it worked. Selecting one selects its operator.
 */
export function PlanInsights({ plan, selectedNode, onSelectNode }: { plan: Plan; selectedNode?: number | null; onSelectNode: (id: number) => void }) {
  const items = useMemo(() => [...(plan.insights?.items ?? [])].sort(bySeverity), [plan]);
  const note = plan.insights?.note ?? null;
  const [open, setOpen] = useState<number | null>(null);
  const [all, setAll] = useState(false);
  const list = useRef<HTMLDivElement>(null);
  const counts = useMemo(() => {
    const c: Record<Severity, number> = { high: 0, medium: 0, low: 0 };
    items.forEach((i) => (c[i.severity] += 1));
    return c;
  }, [items]);

  // Selecting an operator in the diagram highlights the insights about it.
  const related = (i: PlanInsight) => selectedNode != null && i.nodes.includes(selectedNode);
  useEffect(() => {
    if (selectedNode == null) return;
    const index = items.findIndex((i) => i.nodes.includes(selectedNode));
    if (index >= SHOWN) setAll(true);
  }, [selectedNode, items]);

  if (!plan.insights) return null;

  if (items.length === 0) {
    return (
      <div className="insights empty-insights">
        <CircleCheck size={16} className="ok" />
        <div>
          <div className="ins-empty-title">No problems found in this plan</div>
          {note && <div className="ins-note">{note}</div>}
        </div>
      </div>
    );
  }

  const shown = all ? items : items.slice(0, SHOWN);
  const focusRow = (index: number) => list.current?.querySelectorAll<HTMLButtonElement>(".ins-row")[index]?.focus();

  return (
    <section className="insights" aria-label="Plan insights">
      <header className="ins-head">
        <ListChecks size={14} />
        <span className="ins-label">Insights</span>
        <span className="ins-counts">
          {(["high", "medium", "low"] as Severity[])
            .filter((s) => counts[s] > 0)
            .map((s) => (
              <span key={s} className={`ins-count ${s}`}>
                <i />
                {counts[s]} {SEVERITY_LABEL[s].toLowerCase()}
              </span>
            ))}
        </span>
        <span className="spacer" />
        {note && (
          <span className="ins-note-icon" title={note}>
            <Info size={13} />
          </span>
        )}
      </header>
      <div className="ins-list" ref={list} role="listbox" aria-label="Insights, most important first">
        {shown.map((i, index) => {
          const area = AREA[i.area];
          const Icon = area.icon;
          const expanded = open === index;
          return (
            <div key={`${i.rule}-${i.nodes[0] ?? index}`} className={`ins ${i.severity} ${expanded ? "open" : ""} ${related(i) ? "related" : ""}`}>
              <button
                className="ins-row"
                role="option"
                aria-selected={related(i)}
                aria-expanded={expanded}
                onClick={() => {
                  setOpen(expanded ? null : index);
                  if (i.nodes[0] != null) onSelectNode(i.nodes[0]);
                }}
                onKeyDown={(e) => {
                  if (e.key === "ArrowDown") {
                    e.preventDefault();
                    focusRow(Math.min(shown.length - 1, index + 1));
                  } else if (e.key === "ArrowUp") {
                    e.preventDefault();
                    focusRow(Math.max(0, index - 1));
                  } else if (e.key === "Escape" && expanded) {
                    e.preventDefault();
                    setOpen(null);
                  }
                }}
              >
                <span className="ins-sev" title={`${SEVERITY_LABEL[i.severity]} severity`} />
                <span className="ins-area" title={area.label}>
                  <Icon size={13} />
                </span>
                <span className="ins-title">{i.title}</span>
                <span className="ins-impact" title="Share of this plan's work at the operators involved">
                  <span className="ins-bar">
                    <span style={{ width: `${Math.max(4, Math.min(100, i.impact * 100))}%` }} />
                  </span>
                  {percent(i.impact)}
                </span>
                <ChevronDown size={14} className="ins-chev" />
              </button>
              {expanded && (
                <div className="ins-body">
                  <p className="ins-detail">
                    <Prose text={i.detail} />
                  </p>
                  <div className="ins-block">
                    <div className="ins-block-label">Fix</div>
                    <p>
                      <Prose text={i.fix} />
                    </p>
                    {i.fix_sql && (
                      <div className="ins-sql">
                        <Sql text={i.fix_sql} />
                        <button className="btn ghost small ins-copy" onClick={() => copyAndTell(i.fix_sql!, "Copied the fix")} title="Copy the SQL; Culprit never runs it">
                          <Copy size={12} /> Copy
                        </button>
                      </div>
                    )}
                  </div>
                  <div className="ins-block">
                    <div className="ins-block-label">Verify</div>
                    <p>{i.verify}</p>
                  </div>
                  {i.nodes[0] != null && (
                    <button className="btn ghost small" onClick={() => onSelectNode(i.nodes[0]!)}>
                      <Crosshair size={12} /> Show the operator
                    </button>
                  )}
                </div>
              )}
            </div>
          );
        })}
      </div>
      {items.length > SHOWN && (
        <button className="ins-more" onClick={() => setAll((v) => !v)}>
          {all ? "Show fewer" : `Show ${items.length - SHOWN} more`}
        </button>
      )}
    </section>
  );
}
