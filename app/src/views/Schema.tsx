import { GitCompareArrows, Lightbulb, PanelLeftClose, PanelLeftOpen, Search, X } from "lucide-react";
import { lazy, Suspense, useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import type { Report } from "../gen/Report";
import type { TableStat } from "../gen/TableStat";
import { bytes, count, rowCount } from "../lib/format";
import { useHotkeys } from "../lib/hotkeys";
import { fuzzy, tableFindings, tableKey } from "../lib/schema-model";
import { getState, setState, useStore } from "../lib/store";
import { DriftPanel } from "../components/DriftPanel";
import { SchemaTable } from "./SchemaTable";

const IndexAdvisor = lazy(() => import("../components/IndexAdvisor"));

/** Findings the advisor turns into "add" and "clean up" items, for the list entry's counts. */
const ADD_CHECKS = ["missing-index", "filtered-scan", "unindexed-fk"];
const CLEANUP_CHECKS = ["duplicate-index", "unused-index", "overlapping-index"];

type SortKey = "size" | "rows" | "name" | "findings";

const SORTS: { key: SortKey; label: string }[] = [
  { key: "size", label: "Size" },
  { key: "rows", label: "Rows" },
  { key: "findings", label: "Findings" },
  { key: "name", label: "Name" },
];

const SORT_KEY = "culprit.schema.sort";
const WIDTH_KEY = "culprit.schema.listWidth";
const COLLAPSED_KEY = "culprit.schema.listCollapsed";
const MIN_W = 220;
const MAX_W = 460;
const DEFAULT_W = 290;

function remembered<T>(key: string, parse: (v: string) => T | null, fallback: T): T {
  try {
    const v = localStorage.getItem(key);
    return (v == null ? null : parse(v)) ?? fallback;
  } catch {
    return fallback;
  }
}

function remember(key: string, value: string) {
  try {
    localStorage.setItem(key, value);
  } catch {}
}

type Facts = {
  findings: number;
  titles: string[];
  worst: "high" | "medium" | "low" | null;
};

type Entry = {
  table: TableStat;
  key: string;
  hits: number[];
  score: number;
  facts: Facts;
};

const SEVERITY_RANK = { high: 3, medium: 2, low: 1 } as const;

export function Schema({ report }: { report: Report }) {
  const { snapshot } = report;
  const [query, setQuery] = useState("");
  const [sort, setSort] = useState<SortKey>(() => remembered(SORT_KEY, (v) => (SORTS.some((s) => s.key === v) ? (v as SortKey) : null), "size"));
  const [selected, setSelected] = useState<string | null>(null);
  const [comparing, setComparing] = useState(false);
  // The dashboard's Indexes tile opens straight on the advisor.
  const [advising, setAdvising] = useState(() => getState().advisor);
  useEffect(() => {
    if (getState().advisor) setState({ advisor: false });
  }, []);
  // A table picked elsewhere (the command palette) opens here, whatever was selected before.
  const wanted = useStore((s) => s.schemaTable);
  useEffect(() => {
    if (!wanted) return;
    setSelected(wanted);
    setQuery("");
    setAdvising(false);
    setState({ schemaTable: null });
  }, [wanted]);
  const toAdd = report.findings.filter((f) => ADD_CHECKS.includes(f.check)).length;
  const toClean = report.findings.filter((f) => CLEANUP_CHECKS.includes(f.check)).length;
  const [width, setWidth] = useState(() => remembered(WIDTH_KEY, (v) => Math.min(MAX_W, Math.max(MIN_W, Number(v) || DEFAULT_W)), DEFAULT_W));
  const [collapsed, setCollapsed] = useState(() => remembered(COLLAPSED_KEY, (v) => v === "1", false));
  const searchRef = useRef<HTMLInputElement>(null);
  const listRef = useRef<HTMLDivElement>(null);

  const facts = useMemo(() => {
    const out = new Map<string, Facts>();
    for (const table of snapshot.tables) {
      const findings = tableFindings(report, table);
      const worst = findings.reduce<Facts["worst"]>((w, f) => (w == null || SEVERITY_RANK[f.severity] > SEVERITY_RANK[w] ? f.severity : w), null);
      out.set(tableKey(table), {
        findings: findings.length,
        titles: findings.map((f) => f.title),
        worst,
      });
    }
    return out;
  }, [report, snapshot.tables]);

  const entries = useMemo(() => {
    const out: Entry[] = [];
    for (const table of snapshot.tables) {
      const key = tableKey(table);
      // Prefer matches inside the table name (what the row shows); fall back to `schema.table`.
      const own = fuzzy(table.name, query);
      const wide = own ? null : fuzzy(key, query);
      if (!own && !wide) continue;
      const offset = table.schema.length + 1;
      out.push({
        table,
        key,
        hits: own ? own.hits : wide!.hits.filter((h) => h >= offset).map((h) => h - offset),
        score: own ? own.score + 500 : wide!.score,
        facts: facts.get(key)!,
      });
    }
    const by: Record<SortKey, (a: Entry, b: Entry) => number> = {
      size: (a, b) => (b.table.total_bytes ?? 0) - (a.table.total_bytes ?? 0),
      rows: (a, b) => (b.table.rows ?? 0) - (a.table.rows ?? 0),
      findings: (a, b) =>
        b.facts.findings - a.facts.findings ||
        (SEVERITY_RANK[b.facts.worst ?? "low"] ?? 0) - (SEVERITY_RANK[a.facts.worst ?? "low"] ?? 0) ||
        (b.table.total_bytes ?? 0) - (a.table.total_bytes ?? 0),
      name: (a, b) => a.table.name.localeCompare(b.table.name),
    };
    return out.sort((a, b) => (query.trim() ? b.score - a.score : 0) || by[sort](a, b));
  }, [snapshot.tables, query, sort, facts]);

  const groups = useMemo(() => {
    const map = new Map<string, Entry[]>();
    entries.forEach((e) => map.set(e.table.schema, [...(map.get(e.table.schema) ?? []), e]));
    return [...map.entries()].sort((a, b) => b[1].length - a[1].length || a[0].localeCompare(b[0]));
  }, [entries]);
  // Flattened in display order, for keyboard movement.
  const ordered = useMemo(() => groups.flatMap(([, list]) => list), [groups]);
  // A search that hides the selected table doesn't blank the detail: it keeps showing what you were looking at.
  const kept = useMemo(() => {
    const table = snapshot.tables.find((t) => tableKey(t) === selected);
    return table ? { table, key: tableKey(table), hits: [], score: 0, facts: facts.get(tableKey(table))! } : null;
  }, [snapshot.tables, selected, facts]);
  const current = ordered.find((e) => e.key === selected) ?? (query.trim() ? kept : null) ?? ordered[0] ?? kept;
  const maxBytes = Math.max(1, ...snapshot.tables.map((t) => t.total_bytes ?? 0));
  const schemas = new Set(snapshot.tables.map((t) => t.schema));

  const move = (delta: number) => {
    if (!ordered.length) return;
    const at = Math.max(
      0,
      ordered.findIndex((e) => e.key === current?.key),
    );
    const next = ordered[Math.min(ordered.length - 1, Math.max(0, at + delta))];
    if (next) (setSelected(next.key), setAdvising(false));
  };
  const toggleList = () =>
    setCollapsed((c) => {
      remember(COLLAPSED_KEY, c ? "0" : "1");
      return !c;
    });
  // While the advisor is open, j/k and the arrows move through its items instead of the tables.
  useHotkeys({
    j: () => !advising && move(1),
    k: () => !advising && move(-1),
    ArrowDown: () => !advising && move(1),
    ArrowUp: () => !advising && move(-1),
    a: () => setAdvising(true),
    Escape: () => advising && setAdvising(false),
    "/": () => {
      if (collapsed) toggleList();
      requestAnimationFrame(() => searchRef.current?.focus());
    },
    "[": toggleList,
  });

  useEffect(() => {
    listRef.current?.querySelector(".tl-row.on")?.scrollIntoView({ block: "nearest" });
  }, [current?.key]);

  const startResize = (e: React.PointerEvent) => {
    e.preventDefault();
    const startX = e.clientX;
    const startW = width;
    let last = startW;
    const onMove = (ev: PointerEvent) => {
      last = Math.min(MAX_W, Math.max(MIN_W, startW + ev.clientX - startX));
      setWidth(last);
    };
    const onUp = () => {
      window.removeEventListener("pointermove", onMove);
      window.removeEventListener("pointerup", onUp);
      document.body.classList.remove("resizing");
      remember(WIDTH_KEY, String(last));
    };
    document.body.classList.add("resizing");
    window.addEventListener("pointermove", onMove);
    window.addEventListener("pointerup", onUp);
  };

  const metric = (t: TableStat, f: Facts) => {
    switch (sort) {
      case "rows":
        return count(t.rows);
      case "findings":
        return f.findings ? `${f.findings}` : "—";
      default:
        return bytes(t.total_bytes);
    }
  };

  if (snapshot.tables.length === 0) {
    return (
      <div className="empty">
        <h3>No tables</h3>
        <p>This login can't see any tables in the database, or the database is empty.</p>
      </div>
    );
  }

  if (comparing) return <DriftPanel report={report} onClose={() => setComparing(false)} />;

  return (
    <div className={`schema ${collapsed ? "collapsed" : ""}`} style={{ "--list-w": `${width}px` } as React.CSSProperties}>
      {collapsed ? (
        <div className="schema-rail">
          <button className="btn ghost icon" title="Show tables ( [ )" onClick={toggleList}>
            <PanelLeftOpen size={15} />
          </button>
        </div>
      ) : (
        <aside className="schema-list" aria-label="Tables">
          <div className="tl-tools">
            <label className="search">
              <Search size={13} />
              <input
                ref={searchRef}
                value={query}
                placeholder="Find a table"
                spellCheck={false}
                onChange={(e) => setQuery(e.target.value)}
                onKeyDown={(e) => {
                  if (e.key === "ArrowDown" || e.key === "ArrowUp") {
                    e.preventDefault();
                    move(e.key === "ArrowDown" ? 1 : -1);
                  } else if (e.key === "Enter") {
                    // Enter opens the best match when the current table isn't in the results.
                    const top = ordered[0];
                    if (top && !ordered.some((x) => x.key === current?.key)) setSelected(top.key);
                    e.currentTarget.blur();
                  } else if (e.key === "Escape") {
                    setQuery("");
                    e.currentTarget.blur();
                  }
                }}
              />
              {query && (
                <button className="clear" title="Clear" onClick={() => setQuery("")}>
                  <X size={12} />
                </button>
              )}
              {!query && <span className="kbd">/</span>}
            </label>
            <button className="btn ghost icon" title="Hide tables ( [ )" onClick={toggleList}>
              <PanelLeftClose size={15} />
            </button>
          </div>
          <div className="tl-sort">
            <span className="muted">
              {query.trim() ? `${entries.length} of ${snapshot.tables.length}` : `${snapshot.tables.length} tables`} ·{" "}
              {bytes(snapshot.server.database_size_bytes)}
            </span>
            <span className="spacer" />
            <label className="mini-select" title="Sort tables">
              <select
                value={sort}
                onChange={(e) => {
                  setSort(e.target.value as SortKey);
                  remember(SORT_KEY, e.target.value);
                }}
              >
                {SORTS.map((s) => (
                  <option key={s.key} value={s.key}>
                    {s.label}
                  </option>
                ))}
              </select>
            </label>
            <button className="icon-btn" title="Compare with another connection: indexes, columns and settings that differ" onClick={() => setComparing(true)}>
              <GitCompareArrows size={14} />
            </button>
          </div>
          <div className="tl-scroll" ref={listRef} role="listbox" aria-activedescendant={advising ? "tl-advisor" : current ? `tl-${current.key}` : undefined}>
            {!query.trim() && (
              <button
                id="tl-advisor"
                role="option"
                aria-selected={advising}
                className={`tl-row tl-advisor ${advising ? "on" : ""}`}
                title="Index advisor: what to add, what to clean up ( a )"
                onClick={() => setAdvising(true)}
              >
                <Lightbulb size={13} className="tl-advisor-icon" />
                <span className="tl-name">Index advisor</span>
                <span className="tl-badges">
                  {toAdd > 0 && <span className="tl-count medium" title={`${toAdd} to add`}>{toAdd}</span>}
                </span>
                <span className="tl-metric">{toClean > 0 ? `${toClean} to clean` : toAdd > 0 ? "" : "all good"}</span>
              </button>
            )}
            {groups.map(([schema, list]) => (
              <div key={schema} className="tl-group">
                {(schemas.size > 1 || query.trim() === "") && (
                  <div className="tl-group-head">
                    <span>{schema}</span>
                    <span className="muted">{list.length}</span>
                  </div>
                )}
                {list.map((e) => (
                  <button
                    key={e.key}
                    id={`tl-${e.key}`}
                    role="option"
                    aria-selected={!advising && e.key === current?.key}
                    className={`tl-row ${!advising && e.key === current?.key ? "on" : ""}`}
                    title={`${e.key} · ${rowCount(e.table.rows)} · ${bytes(e.table.total_bytes)}`}
                    onClick={() => (setSelected(e.key), setAdvising(false))}
                  >
                    <span className="tl-name">{highlight(e.table.name, e.hits)}</span>
                    <span className="tl-badges">
                      {e.facts.findings > 0 && (
                        <span className={`tl-count ${e.facts.worst}`} title={e.facts.titles.join("\n")}>
                          {e.facts.findings}
                        </span>
                      )}
                      {!e.table.has_primary_key && (
                        <span className="tl-flag" title="No primary key">
                          no pk
                        </span>
                      )}
                    </span>
                    <span className="tl-metric">{metric(e.table, e.facts)}</span>
                    <span
                      className="tl-size"
                      style={{
                        width: `${Math.max(1.5, ((e.table.total_bytes ?? 0) / maxBytes) * 100)}%`,
                      }}
                    />
                  </button>
                ))}
              </div>
            ))}
            {entries.length === 0 && <div className="tl-empty">No table matches “{query}”.</div>}
          </div>
        </aside>
      )}
      {!collapsed && (
        <div
          className="schema-resizer"
          onPointerDown={startResize}
          onDoubleClick={() => (setWidth(DEFAULT_W), remember(WIDTH_KEY, String(DEFAULT_W)))}
          title="Drag to resize"
        />
      )}
      <section className="schema-main">
        {advising ? (
          <Suspense fallback={<div className="ia" aria-busy="true" />}>
            <IndexAdvisor report={report} onOpenTable={(key) => (setQuery(""), setSelected(key), setAdvising(false))} />
          </Suspense>
        ) : current ? (
          <SchemaTable key={current.key} report={report} table={current.table} onOpenTable={(key) => (setQuery(""), setSelected(key))} />
        ) : (
          <div className="empty">No table matches “{query}”.</div>
        )}
      </section>
    </div>
  );
}

function highlight(text: string, hits: number[]): ReactNode {
  if (!hits.length) return text;
  const set = new Set(hits);
  const out: ReactNode[] = [];
  let run = "";
  let marked = false;
  for (let i = 0; i < text.length; i++) {
    const on = set.has(i);
    if (on !== marked && run) {
      out.push(marked ? <mark key={i}>{run}</mark> : run);
      run = "";
    }
    marked = on;
    run += text[i];
  }
  if (run) out.push(marked ? <mark key="end">{run}</mark> : run);
  return out;
}
