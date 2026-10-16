import { ArrowUpRight, Check, ChevronRight, Copy, Info, Layers, Rows3, Table2 } from "lucide-react";
import { useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import type { AdviceQuery } from "../gen/AdviceQuery";
import type { CleanupKind } from "../gen/CleanupKind";
import type { IndexAdvice } from "../gen/IndexAdvice";
import type { IndexCandidate } from "../gen/IndexCandidate";
import type { IndexCleanup } from "../gen/IndexCleanup";
import type { Report } from "../gen/Report";
import type { TableAdvice } from "../gen/TableAdvice";
import { api, errorMessage, type ApiError } from "../lib/api";
import { ago, bytes, count, percent, rowCount } from "../lib/format";
import { useHotkeys } from "../lib/hotkeys";
import { setState } from "../lib/store";
import { Sql } from "./Sql";
import { copyAndTell } from "../lib/clipboard";

type Load = { status: "loading" } | { status: "ready"; advice: IndexAdvice } | { status: "error"; error: ApiError };
type Section = "add" | "cleanup" | "tables";
type Item =
  | { kind: "add"; key: string; candidate: IndexCandidate; rank: number }
  | { kind: "cleanup"; key: string; cleanup: IndexCleanup }
  | { kind: "tables"; key: string; table: TableAdvice };

/** Advice per report id: the advisor is pure over a report, so a re-open is instant. */
const cache = new Map<string, IndexAdvice>();

const SOURCE: Record<IndexCandidate["source"], string> = {
  optimizer: "Asked for by the optimizer",
  "plan-scan": "Seen in a query plan",
  "foreign-key": "Foreign key",
  "engine-built": "Built on every run",
};

const CLEANUP: Record<CleanupKind, { label: string; tone: string; verb: string }> = {
  duplicate: { label: "Duplicate", tone: "medium", verb: "Same keys as" },
  prefix: { label: "Covered", tone: "low", verb: "Covered by" },
  overlap: { label: "Overlaps", tone: "low", verb: "Overlaps" },
  unused: { label: "Unused", tone: "medium", verb: "" },
};

const TABLE_KIND: Record<TableAdvice["kind"], { label: string; icon: ReactNode }> = {
  "scan-heavy": { label: "Read in full", icon: <Rows3 size={14} /> },
  "forwarded-fetches": { label: "Heap with forwarded rows", icon: <Layers size={14} /> },
  "over-indexed": { label: "Many indexes, many writes", icon: <Table2 size={14} /> },
};

function sqlOf(item: Item): string | null {
  if (item.kind === "add") return item.candidate.sql;
  if (item.kind === "cleanup") return item.cleanup.sql;
  return item.table.sql;
}

function findingOf(item: Item): string | null {
  if (item.kind === "add") return item.candidate.finding_id;
  if (item.kind === "cleanup") return item.cleanup.finding_id;
  return item.table.finding_id;
}

function tableOf(item: Item): string {
  if (item.kind === "add") return `${item.candidate.schema}.${item.candidate.table}`;
  if (item.kind === "cleanup") return `${item.cleanup.schema}.${item.cleanup.table}`;
  return `${item.table.schema}.${item.table.table}`;
}

/**
 * The index advisor: one ranked plan for the database. Indexes to add are ordered by the database time of the queries
 * that need them (not by the optimizer's own impact guess), each with those queries, the requests it merges, copy-only
 * DDL, its write cost and how to verify it. Then indexes to clean up and tables whose shape costs reads or writes.
 */
export default function IndexAdvisor({ report, onOpenTable }: { report: Report; onOpenTable: (key: string) => void }) {
  const id = report.snapshot.id;
  const [load, setLoad] = useState<Load>(() => (cache.has(id) ? { status: "ready", advice: cache.get(id)! } : { status: "loading" }));
  const [filter, setFilter] = useState<Section | null>(null);
  const [open, setOpen] = useState<Set<string>>(new Set());
  const [focus, setFocus] = useState(0);
  const listRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (cache.has(id)) return;
    let live = true;
    setLoad({ status: "loading" });
    api()
      .then((client) => client.indexAdvice(report.snapshot.connection))
      .then((advice) => {
        cache.set(id, advice);
        if (!live) return;
        setLoad({ status: "ready", advice });
        // The most valuable item starts open, so the first screen already shows a reason and the DDL.
        const first = advice.candidates[0] ? "add:0" : advice.cleanups[0] ? "cleanup:0" : null;
        if (first) setOpen(new Set([first]));
      })
      .catch((e) => live && setLoad({ status: "error", error: errorMessage(e) }));
    return () => {
      live = false;
    };
  }, [id, report.snapshot.connection]);

  const advice = load.status === "ready" ? load.advice : null;
  const items = useMemo<Item[]>(() => {
    if (!advice) return [];
    const out: Item[] = [];
    if (!filter || filter === "add") advice.candidates.forEach((candidate, i) => out.push({ kind: "add", key: `add:${i}`, candidate, rank: i + 1 }));
    if (!filter || filter === "cleanup") advice.cleanups.forEach((cleanup, i) => out.push({ kind: "cleanup", key: `cleanup:${i}`, cleanup }));
    if (!filter || filter === "tables") advice.tables.forEach((table, i) => out.push({ kind: "tables", key: `tables:${i}`, table }));
    return out;
  }, [advice, filter]);

  const toggle = (key: string) =>
    setOpen((prev) => {
      const next = new Set(prev);
      if (next.has(key)) next.delete(key);
      else next.add(key);
      return next;
    });
  const copy = async (sql: string | null) => {
    if (!sql) return;
    await copyAndTell(sql, "Copied. Culprit never runs it; review it, then run it in your migration tooling.");
  };
  const openFinding = (fid: string | null) => fid && setState({ view: "overview", finding: fid });
  const openQuery = (q: AdviceQuery) => setState({ view: "queries", query: q.id });

  const current = items[Math.min(focus, items.length - 1)];
  useHotkeys({
    j: () => setFocus((f) => Math.min(items.length - 1, f + 1)),
    k: () => setFocus((f) => Math.max(0, f - 1)),
    ArrowDown: () => setFocus((f) => Math.min(items.length - 1, f + 1)),
    ArrowUp: () => setFocus((f) => Math.max(0, f - 1)),
    Enter: () => current && toggle(current.key),
    c: () => current && void copy(sqlOf(current)),
    o: () => current && openFinding(findingOf(current)),
    t: () => current && onOpenTable(tableOf(current)),
  });

  useEffect(() => {
    listRef.current?.querySelector(".ia-item.focus")?.scrollIntoView({ block: "nearest" });
  }, [focus, filter]);

  if (load.status === "loading") {
    return (
      <div className="ia" aria-busy="true">
        <Header advice={null} report={report} />
        <div className="ia-tiles">
          {[0, 1, 2].map((i) => (
            <div key={i} className="ia-tile ia-skel" />
          ))}
        </div>
        {[0, 1, 2].map((i) => (
          <div key={i} className="ia-skel ia-skel-row" />
        ))}
      </div>
    );
  }
  if (load.status === "error") {
    return (
      <div className="ia">
        <Header advice={null} report={report} />
        <div className="ia-note">
          <Info size={14} />
          <div>
            <b>{load.error.message}</b>
            {load.error.hint && <div className="muted">{load.error.hint}</div>}
          </div>
        </div>
      </div>
    );
  }

  const a = load.advice;
  const neededShare = a.candidates.reduce((sum, c) => sum + (c.db_time_share ?? 0), 0);
  const freed = a.cleanups.reduce((sum, c) => sum + (c.size_bytes ?? 0), 0);
  const empty = a.candidates.length + a.cleanups.length + a.tables.length === 0;
  const indexCount = report.snapshot.indexes.length;

  return (
    <div className="ia">
      <Header advice={a} report={report} />
      {empty ? (
        <div className="ia-empty">
          <span className="ia-empty-mark">
            <Check size={18} />
          </span>
          <h3>Nothing to add or clean up</h3>
          <p>
            Culprit checked {count(indexCount)} indexes on {count(report.snapshot.tables.length)} tables: {a.engine === "mssql" ? "the optimizer's missing-index requests, " : ""}plans
            that read whole tables, foreign keys, duplicates, overlaps{report.snapshot.server.capabilities.includes("index-usage") ? " and indexes nothing reads" : ""}.
          </p>
        </div>
      ) : (
        <div className="ia-tiles" role="tablist" aria-label="Show">
          <Tile on={filter === "add"} n={a.candidates.length} label="to add" sub={neededShare > 0 ? `needed by ${percent(Math.min(1, neededShare))} of DB time` : a.candidates.length ? "from plans and foreign keys" : "nothing missing"} onClick={() => (setFilter((f) => (f === "add" ? null : "add")), setFocus(0))} />
          <Tile on={filter === "cleanup"} n={a.cleanups.length} label="to clean up" sub={freed > 0 ? `frees ${bytes(freed)}` : "nothing idle"} onClick={() => (setFilter((f) => (f === "cleanup" ? null : "cleanup")), setFocus(0))} />
          <Tile on={filter === "tables"} n={a.tables.length} label={a.tables.length === 1 ? "table to look at" : "tables to look at"} sub={a.tables.length ? "shape costs reads or writes" : "all in shape"} onClick={() => (setFilter((f) => (f === "tables" ? null : "tables")), setFocus(0))} />
        </div>
      )}

      {a.notes.map((note) => (
        <div key={note} className="ia-note">
          <Info size={14} />
          <span>{note}</span>
        </div>
      ))}

      <div className="ia-list" ref={listRef}>
        {(["add", "cleanup", "tables"] as Section[]).map((section) => {
          const list = items.filter((i) => i.kind === section);
          if (!list.length) return null;
          return (
            <section key={section} className="ia-section">
              <h3>
                {section === "add" ? "Add" : section === "cleanup" ? "Clean up" : "Tables"}
                <span className="ia-count">{list.length}</span>
                {section === "add" && <span className="ia-h-note">ordered by the database time of the queries that need each one</span>}
              </h3>
              {list.map((item) => {
                const index = items.indexOf(item);
                const props = {
                  focused: index === focus,
                  onFocus: () => setFocus(index),
                  expanded: open.has(item.key),
                  onToggle: () => (toggle(item.key), setFocus(index)),
                  onCopy: () => void copy(sqlOf(item)),
                  onFinding: () => openFinding(findingOf(item)),
                  onTable: () => onOpenTable(tableOf(item)),
                  onQuery: openQuery,
                };
                if (item.kind === "add") return <CandidateRow key={item.key} c={item.candidate} rank={item.rank} {...props} />;
                if (item.kind === "cleanup") return <CleanupRow key={item.key} c={item.cleanup} {...props} />;
                return <TableRow key={item.key} t={item.table} {...props} />;
              })}
            </section>
          );
        })}
      </div>
      {!empty && (
        <div className="ia-keys muted">
          <span className="kbd">j</span>
          <span className="kbd">k</span> move · <span className="kbd">↵</span> open · <span className="kbd">c</span> copy SQL · <span className="kbd">o</span> finding · <span className="kbd">t</span> table
        </div>
      )}
    </div>
  );
}

function Header({ advice, report }: { advice: IndexAdvice | null; report: Report }) {
  return (
    <header className="ia-head">
      <h2>Index advisor</h2>
      <p className="ia-sub">
        {advice ? `Usage counted ${advice.counters}` : "Reading the latest scan…"} · scanned {ago(report.snapshot.taken_at)} · Culprit only suggests; it never changes indexes
      </p>
    </header>
  );
}

function Tile({ on, n, label, sub, onClick }: { on: boolean; n: number; label: string; sub: string; onClick: () => void }) {
  return (
    <button role="tab" aria-selected={on} className={`ia-tile ${on ? "on" : ""} ${n === 0 ? "zero" : ""}`} onClick={onClick} disabled={n === 0}>
      <span className="ia-tile-n">{n}</span>
      <span className="ia-tile-label">{label}</span>
      <span className="ia-tile-sub">{sub}</span>
    </button>
  );
}

type RowProps = {
  focused: boolean;
  onFocus: () => void;
  expanded: boolean;
  onToggle: () => void;
  onCopy: () => void;
  onFinding: () => void;
  onTable: () => void;
  onQuery: (q: AdviceQuery) => void;
};

function Keys({ keys, include, shared = 0 }: { keys: string[]; include?: string[]; shared?: number }) {
  return (
    <span className="ia-keys-chips">
      {keys.map((k, i) => (
        <span key={`${k}-${i}`} className={`ia-chip ${i < shared ? "shared" : ""}`}>
          {k}
        </span>
      ))}
      {include && include.length > 0 && <span className="ia-chip include" title={`Included: ${include.join(", ")}`}>+ {include.length === 1 ? include[0] : `${include.length} included`}</span>}
    </span>
  );
}

function Body({ expanded, children }: { expanded: boolean; children: ReactNode }) {
  return (
    <div className={`ia-body ${expanded ? "open" : ""}`}>
      <div className="ia-body-inner">{expanded && children}</div>
    </div>
  );
}

function SqlBlock({ sql, onCopy }: { sql: string | null; onCopy: () => void }) {
  if (!sql) return null;
  return (
    <div className="ia-sql">
      <div className="ia-sql-bar">
        <span className="muted">Copy and run it yourself</span>
        <button className="btn ghost" onClick={onCopy}>
          <Copy size={12} /> Copy <span className="kbd">c</span>
        </button>
      </div>
      <Sql text={sql} pretty={false} className="ia-sql-code" />
    </div>
  );
}

function Queries({ queries, onQuery, title }: { queries: AdviceQuery[]; onQuery: (q: AdviceQuery) => void; title: string }) {
  if (!queries.length) return null;
  const max = Math.max(0.0001, ...queries.map((q) => q.share ?? 0));
  return (
    <div className="ia-block">
      <div className="ia-label">{title}</div>
      {queries.map((q) => (
        <button key={q.id} className="ia-query" onClick={() => onQuery(q)} title="Open in Queries">
          <span className="ia-qbar" aria-hidden>
            <span style={{ width: `${Math.max(4, ((q.share ?? 0) / max) * 100)}%` }} />
          </span>
          <span className="ia-qshare">{q.share != null ? percent(q.share) : "—"}</span>
          <span className="ia-qtext">{q.text}</span>
          {q.runs_per_day != null && <span className="ia-qruns">{count(q.runs_per_day)}/day</span>}
          <ArrowUpRight size={12} className="ia-go" />
        </button>
      ))}
    </div>
  );
}

function Verify({ steps }: { steps: string[] }) {
  if (!steps.length) return null;
  return (
    <div className="ia-block">
      <div className="ia-label">How to verify</div>
      <ol className="ia-verify">
        {steps.map((s) => (
          <li key={s}>{s}</li>
        ))}
      </ol>
    </div>
  );
}

function Links({ onFinding, hasFinding, onTable }: { onFinding: () => void; hasFinding: boolean; onTable: () => void }) {
  return (
    <div className="ia-links">
      {hasFinding && (
        <button className="btn ghost" onClick={onFinding}>
          Open finding <span className="kbd">o</span>
        </button>
      )}
      <button className="btn ghost" onClick={onTable}>
        Open table <span className="kbd">t</span>
      </button>
    </div>
  );
}

function CandidateRow({ c, rank, focused, onFocus, expanded, onToggle, onCopy, onFinding, onTable, onQuery }: RowProps & { c: IndexCandidate; rank: number }) {
  const facts: string[] = [];
  if (c.lookups != null) facts.push(`${count(c.lookups)} lookups`);
  if (c.estimated_improvement != null) facts.push(`≈${percent(c.estimated_improvement)} cheaper`);
  if (c.table_rows != null) facts.push(rowCount(c.table_rows));
  if (c.writes_per_day != null) facts.push(`${count(c.writes_per_day)} writes/day`);
  return (
    <div className={`ia-item ${focused ? "focus" : ""} ${expanded ? "expanded" : ""}`}>
      <button className="ia-row" onClick={onToggle} onFocus={onFocus} aria-expanded={expanded}>
        <span className="ia-rank">{rank}</span>
        <span className="ia-main">
          <span className="ia-title">
            <span className="ia-table">{c.table}</span>
            <Keys keys={c.keys} include={c.include} />
          </span>
          <span className="ia-meta">
            <span className={`ia-source ${c.source}`}>{SOURCE[c.source]}</span>
            {facts.length > 0 && <span> · {facts.join(" · ")}</span>}
            {c.widens && <span> · widens {c.widens}</span>}
          </span>
        </span>
        <span className="ia-impact" title="Share of database time of the queries that need this index">
          {c.db_time_share != null ? (
            <>
              <span className="ia-meter" aria-hidden>
                <span style={{ width: `${Math.max(3, Math.sqrt(c.db_time_share) * 100)}%` }} />
              </span>
              <span className="ia-impact-n">{percent(c.db_time_share)}</span>
              <span className="ia-impact-l">of DB time</span>
            </>
          ) : (
            <span className="ia-impact-l">{c.queries.length ? `${c.queries.length} quer${c.queries.length === 1 ? "y" : "ies"}` : "no captured queries"}</span>
          )}
        </span>
        <ChevronRight size={14} className="ia-chev" />
      </button>
      <Body expanded={expanded}>
        <p className="ia-reason">{c.reason}</p>
        <Queries queries={c.queries} onQuery={onQuery} title={c.queries.length === 1 ? "The query that needs it" : "The queries that need it"} />
        {c.requests.length > 1 && (
          <div className="ia-block">
            <div className="ia-label">Merges {c.requests.length} optimizer requests</div>
            <div className="ia-requests">
              {c.requests.map((r, i) => (
                <div key={i} className="ia-request">
                  <Keys keys={[...r.equality, ...r.inequality]} include={r.include} />
                  <span className="muted">
                    {count(r.lookups)} lookups · ≈{percent(r.improvement)} cheaper
                  </span>
                </div>
              ))}
            </div>
          </div>
        )}
        <SqlBlock sql={c.sql} onCopy={onCopy} />
        {!c.sql && c.widens && <p className="ia-reason muted">{c.widens} already starts with these columns; check the queries' predicates for type mismatches or functions on the column.</p>}
        {c.write_cost && <p className="ia-cost">{c.write_cost}</p>}
        <Verify steps={c.verify} />
        <Links onFinding={onFinding} hasFinding={!!c.finding_id} onTable={onTable} />
      </Body>
    </div>
  );
}

function cleanupMeta(c: IndexCleanup): string {
  const reads = c.reads != null ? ` · read ${count(c.reads)}×` : "";
  switch (c.kind) {
    case "unused":
      return ["never read", c.writes ? `${count(c.writes)} writes` : null, c.other ? `also covered by ${c.other}` : null].filter(Boolean).join(" · ");
    case "duplicate":
      return `Same keys as ${c.other}${reads}`;
    case "prefix":
      return `Covered by ${c.other}${reads}`;
    case "overlap":
      return `Overlaps ${c.other}${reads}`;
  }
}

function CleanupRow({ c, focused, onFocus, expanded, onToggle, onCopy, onFinding, onTable }: RowProps & { c: IndexCleanup }) {
  const kind = CLEANUP[c.kind];
  const shared = c.other_keys.length ? c.keys.findIndex((k, i) => c.other_keys[i]?.toLowerCase() !== k.toLowerCase()) : 0;
  const prefix = shared === -1 ? c.keys.length : shared;
  return (
    <div className={`ia-item ${focused ? "focus" : ""} ${expanded ? "expanded" : ""}`}>
      <button className="ia-row" onClick={onToggle} onFocus={onFocus} aria-expanded={expanded}>
        <span className={`ia-pill ${kind.tone}`}>{kind.label}</span>
        <span className="ia-main">
          <span className="ia-title">
            <span className="ia-index">{c.index}</span>
            <span className="ia-on">on {c.table}</span>
          </span>
          <span className="ia-meta">{cleanupMeta(c)}</span>
        </span>
        <span className="ia-impact">
          <span className="ia-impact-n">{bytes(c.size_bytes)}</span>
        </span>
        <ChevronRight size={14} className="ia-chev" />
      </button>
      <Body expanded={expanded}>
        <p className="ia-reason">{c.reason}</p>
        {c.other && (
          <div className="ia-compare">
            <span className="ia-compare-name">{c.index}</span>
            <Keys keys={c.keys} shared={prefix} />
            <span className="ia-compare-name">{c.other}</span>
            <Keys keys={c.other_keys} shared={prefix} />
          </div>
        )}
        <SqlBlock sql={c.sql} onCopy={onCopy} />
        {c.caveats.map((cv) => (
          <p key={cv} className="ia-cost">
            {cv}
          </p>
        ))}
        <Links onFinding={onFinding} hasFinding={!!c.finding_id} onTable={onTable} />
      </Body>
    </div>
  );
}

function TableRow({ t, focused, onFocus, expanded, onToggle, onCopy, onFinding, onTable, onQuery }: RowProps & { t: TableAdvice }) {
  const kind = TABLE_KIND[t.kind];
  const facts: string[] = [];
  if (t.full_scans_per_day != null) facts.push(`${count(t.full_scans_per_day)} full reads/day`);
  if (t.rows_scanned_per_day != null) facts.push(`${count(t.rows_scanned_per_day)} rows/day`);
  if (t.forwarded_per_day != null) facts.push(`${count(t.forwarded_per_day)} forwarded reads/day`);
  if (t.indexes != null && t.kind === "over-indexed") facts.push(`${count(t.indexes)} indexes`);
  if (t.writes_per_day != null) facts.push(`${count(t.writes_per_day)} writes/day`);
  return (
    <div className={`ia-item ${focused ? "focus" : ""} ${expanded ? "expanded" : ""}`}>
      <button className="ia-row" onClick={onToggle} onFocus={onFocus} aria-expanded={expanded}>
        <span className="ia-kind-icon">{kind.icon}</span>
        <span className="ia-main">
          <span className="ia-title">
            <span className="ia-table">{t.table}</span>
            <span className="ia-on">{kind.label}</span>
          </span>
          <span className="ia-meta">{facts.join(" · ")}</span>
        </span>
        <span className="ia-impact">{t.rows != null && <span className="ia-impact-l">{rowCount(t.rows)}</span>}</span>
        <ChevronRight size={14} className="ia-chev" />
      </button>
      <Body expanded={expanded}>
        <p className="ia-reason">{t.reason}</p>
        <Queries queries={t.queries} onQuery={onQuery} title="Statements that read it" />
        <SqlBlock sql={t.sql} onCopy={onCopy} />
        <Links onFinding={onFinding} hasFinding={!!t.finding_id} onTable={onTable} />
      </Body>
    </div>
  );
}
