import { ArrowRight, ChevronRight, Copy, KeyRound, Link2 } from "lucide-react";
import { Fragment, useId, useMemo, useState, type ReactNode } from "react";
import { TableSignalFacts } from "../components/BeyondTime";
import { SchemaData } from "./SchemaData";
import { Sql } from "../components/Sql";
import type { IndexStat } from "../gen/IndexStat";
import type { Report } from "../gen/Report";
import type { TableStat } from "../gen/TableStat";
import { ago, bytes, count, oneLine, percent, when } from "../lib/format";
import { tableSignalWarning } from "../lib/health";
import { useHotkeys } from "../lib/hotkeys";
import { indexKey, indexUsage, plannedQueryCount } from "../lib/index-usage";
import { canSeek, columnMembership, coveredBy, ddl, referenceOf, relations, tableFindings, tableKey } from "../lib/schema-model";
import { setState, toast } from "../lib/store";

type Tab = "columns" | "indexes" | "relations" | "data" | "ddl";

const TAB_KEY = "culprit.schema.tab";

function rememberedTab(): Tab {
  try {
    const v = localStorage.getItem(TAB_KEY);
    return v === "indexes" || v === "relations" || v === "data" || v === "ddl" ? v : "columns";
  } catch {
    return "columns";
  }
}

async function copy(text: string, what: string) {
  try {
    await navigator.clipboard.writeText(text);
    toast(`Copied ${what}`);
  } catch {
    toast("Couldn't reach the clipboard");
  }
}

/** One table: what it is, how big, what's wrong with it, and tabs for its columns, indexes, relations, rows and DDL. */
export function SchemaTable({ report, table, onOpenTable }: { report: Report; table: TableStat; onOpenTable: (key: string) => void }) {
  const { snapshot } = report;
  const engine = snapshot.server.engine;
  const tabId = useId();
  const [tab, setTab] = useState<Tab>(rememberedTab);
  const indexes = useMemo(() => snapshot.indexes.filter((i) => i.schema === table.schema && i.table === table.name), [snapshot.indexes, table]);
  const columns = useMemo(
    () => snapshot.columns.filter((c) => c.schema === table.schema && c.table === table.name).sort((a, b) => a.ordinal - b.ordinal),
    [snapshot.columns, table],
  );
  const links = useMemo(() => relations(report, table), [report, table]);
  const findings = useMemo(() => tableFindings(report, table), [report, table]);
  const choose = (next: Tab) => {
    setTab(next);
    try {
      localStorage.setItem(TAB_KEY, next);
    } catch {}
  };
  useHotkeys({
    c: () => choose("columns"),
    i: () => choose("indexes"),
    r: () => choose("relations"),
    p: () => choose("data"),
    d: () => choose("ddl"),
  });

  const tabs: { key: Tab; label: string; n?: number; hotkey: string }[] = [
    { key: "columns", label: "Columns", n: columns.length, hotkey: "C" },
    { key: "indexes", label: "Indexes", n: indexes.length, hotkey: "I" },
    {
      key: "relations",
      label: "Relations",
      n: links.outgoing.length + links.incoming.length,
      hotkey: "R",
    },
    { key: "data", label: "Data", hotkey: "P" },
    { key: "ddl", label: "DDL", hotkey: "D" },
  ];

  return (
    <div className="td" onKeyDown={(e) => {
      if (["ArrowUp", "ArrowDown", "ArrowLeft", "ArrowRight"].includes(e.key)) e.stopPropagation();
    }}>
      <Header report={report} table={table} findings={findings} />
      <div
        className="td-tabs"
        role="tablist"
        aria-label="Table details"
        onKeyDown={(e) => {
          if (e.key !== "ArrowRight" && e.key !== "ArrowLeft") return;
          e.preventDefault();
          const at = tabs.findIndex((t) => t.key === tab);
          const next = tabs[(at + (e.key === "ArrowRight" ? 1 : tabs.length - 1)) % tabs.length];
          if (!next) return;
          choose(next.key);
          (e.currentTarget.querySelector(`[data-tab="${next.key}"]`) as HTMLElement | null)?.focus();
        }}
      >
        {tabs.map((t) => (
          <button
            key={t.key}
            id={`${tabId}-${t.key}`}
            data-tab={t.key}
            role="tab"
            aria-selected={tab === t.key}
            aria-controls={`${tabId}-panel`}
            tabIndex={tab === t.key ? 0 : -1}
            className={tab === t.key ? "on" : ""}
            title={`${t.label} (${t.hotkey})`}
            onClick={() => choose(t.key)}
          >
            {t.label}
            {t.n != null && <span className="n">{t.n}</span>}
          </button>
        ))}
      </div>
      <div id={`${tabId}-panel`} className={`td-body ${tab === "data" ? "fill" : tab === "columns" || tab === "indexes" ? "grid-tab" : ""}`} role="tabpanel" aria-labelledby={`${tabId}-${tab}`}>
        {tab === "columns" && <Columns report={report} table={table} indexes={indexes} links={links} onOpenTable={onOpenTable} />}
        {tab === "indexes" && <Indexes report={report} table={table} indexes={indexes} links={links} />}
        {tab === "relations" && <Relations links={links} onOpenTable={onOpenTable} />}
        {tab === "data" && <SchemaData key={JSON.stringify([snapshot.connection, engine, table.schema, table.name])} report={report} table={table} />}
        {tab === "ddl" && <Ddl text={ddl(engine, table, columns, indexes, links.outgoing)} original={table.definition != null} />}
      </div>
    </div>
  );
}

function Header({ report, table, findings }: { report: Report; table: TableStat; findings: ReturnType<typeof tableFindings> }) {
  const [all, setAll] = useState(false);
  const { snapshot } = report;
  const data = table.data_bytes ?? 0;
  const index = table.index_bytes ?? 0;
  const total = Math.max(1, data + index);
  // How the rows' own data splits between the rows and the large values kept apart from them, where the engine says.
  const large = table.lob_bytes == null ? null : table.lob_bytes + (table.overflow_bytes ?? 0);
  const own = large == null || table.in_row_bytes == null ? null : table.in_row_bytes + large;
  const averageRow = own != null && own > 0 && table.rows != null && table.rows >= 1 ? own / table.rows : null;
  // SQL Server keeps statistics per object, not per table: show the stalest one Culprit read for this table.
  const stats = snapshot.statistics.filter((s) => s.schema === table.schema && s.table === table.name && s.last_updated);
  const stalest = stats.reduce<string | null>((oldest, s) => (oldest == null || (s.last_updated ?? "") < oldest ? s.last_updated : oldest), null);
  const analyzed = snapshot.server.engine === "mssql" ? stalest : table.last_analyze;
  const knowsStats = snapshot.server.engine === "postgres" || stats.length > 0 || table.last_analyze != null;
  const writes = [table.inserts, table.updates, table.deletes].every((v) => v == null)
    ? null
    : (table.inserts ?? 0) + (table.updates ?? 0) + (table.deletes ?? 0);
  const health: { label: string; why: string }[] = [];
  if (table.is_heap)
    health.push({
      label: "No clustered index",
      why: "Rows live in an unordered heap: scans read forwarded records and space isn't reclaimed well",
    });
  if (!table.has_primary_key && table.kind !== "virtual")
    health.push({
      label: "No primary key",
      why: "Nothing identifies a row uniquely; replication, ORMs and deduplication all suffer",
    });
  if (table.dead_rows != null && table.rows != null && table.dead_rows > Math.max(10_000, table.rows * 0.2))
    health.push({
      label: `${count(table.dead_rows)} dead rows`,
      why: "Deleted or updated row versions waiting for vacuum: scans still read them",
    });
  const rank = { high: 3, medium: 2, low: 1 } as const;
  const sorted = [...findings].sort((a, b) => rank[b.severity] - rank[a.severity]);
  const shown = all ? sorted : sorted.slice(0, 1);

  return (
    <header className="td-head">
      <div className="td-title">
        <h2 title={tableKey(table)}>
          <span className="td-schema">{table.schema}.</span>
          {table.name}
        </h2>
        {table.kind === "virtual" && <span className="tag">Virtual table</span>}
        {table.kind === "hypertable" && <span className="tag">Hypertable</span>}
        <button className="btn ghost icon" title="Copy the qualified name" onClick={() => copy(tableKey(table), "the table name")}>
          <Copy size={13} />
        </button>
        {health.map((h) => (
          <span key={h.label} className="tag medium" title={h.why}>
            {h.label}
          </span>
        ))}
      </div>
      <div className="td-facts">
        <Fact label="Rows" value={count(table.rows)} />
        {table.storage?.hypertable && <Fact label="Chunks" value={count(table.storage.hypertable.chunks)} detail={table.storage.hypertable.compressed_chunks == null ? undefined : `${count(table.storage.hypertable.compressed_chunks)} in columnstore`} />}
        <Fact
          label="Size"
          value={bytes(table.total_bytes ?? (data + index || null))}
          detail={
            data + index > 0 ? (
              <span className="td-split" title={`${bytes(data)} data · ${bytes(index)} indexes`}>
                <span className="td-split-bar">
                  <span className="d" style={{ width: `${(data / total) * 100}%` }} />
                  <span className="i" style={{ width: `${(index / total) * 100}%` }} />
                </span>
                <span>
                  {bytes(data)} data · {bytes(index)} indexes
                </span>
              </span>
            ) : null
          }
        />
        {averageRow != null && own != null && large != null && (
          <Fact
            label="Average row"
            value={bytes(averageRow)}
            warn={tableSignalWarning(report, table, ["large-rows"])}
            detail={<span title="Large values kept apart from the rows: LOB pages on SQL Server, TOAST on PostgreSQL">{percent(large / own)} in large values</span>}
          />
        )}
        {knowsStats && (
          <Fact
            label="Statistics"
            value={analyzed ? ago(analyzed) : "no update recorded"}
            warn={tableSignalWarning(report, table, ["stale-stats"])}
            detail={
              table.modifications_since_analyze
                ? `${count(table.modifications_since_analyze)} changes since`
                : stats.length > 1
                  ? `oldest of ${stats.length}`
                  : null
            }
          />
        )}
        {table.seq_scans != null && (
          <Fact label="Full scans" value={count(table.seq_scans)} detail={table.index_scans != null ? `${count(table.index_scans)} index scans` : null} />
        )}
        {writes != null && writes > 0 && (
          <Fact label="Writes" value={count(writes)} detail={`${count(table.inserts)} ins · ${count(table.updates)} upd · ${count(table.deletes)} del`} />
        )}
        <TableSignalFacts report={report} table={table} />
      </div>
      <HypertableChunks key={tableKey(table)} table={table} />
      {findings.length > 0 && (
        <div className="td-findings">
          {shown.map((f, n) => (
            <div key={f.id} className="td-finding-row">
              <button className="td-finding" onClick={() => setState({ view: "overview", finding: f.id })} title="Open in Findings">
                <span className={`sev ${f.severity}`} />
                <span className="t">{f.title}</span>
                <ChevronRight size={13} className="muted" />
              </button>
              {n === 0 && findings.length > 1 && (
                <button className="td-more" onClick={() => setAll((a) => !a)} aria-expanded={all}>
                  {all ? "Fewer" : `+${findings.length - 1} more`}
                </button>
              )}
            </div>
          ))}
        </div>
      )}
    </header>
  );
}

function HypertableChunks({ table }: { table: TableStat }) {
  const [shown, setShown] = useState(20);
  const hypertable = table.storage?.hypertable;
  if (!hypertable) return null;
  const chunks = hypertable.chunk_details;
  return (
    <details className="td-chunks">
      <summary>Managed chunks{hypertable.dimensions.length > 0 ? ` · ${hypertable.dimensions.join(", ")}` : ""}</summary>
      {chunks == null ? <p className="muted">Chunk details could not be read. Hypertable totals still include its managed storage.</p> : (
        <>
          <div className="td-chunk-list">
            <table>
              <thead><tr><th>Chunk</th><th>Rows</th><th>Size</th><th>Storage</th></tr></thead>
              <tbody>{chunks.slice(0, shown).map((chunk) => <tr key={`${chunk.schema}.${chunk.name}`}>
                <td title={`${chunk.schema}.${chunk.name}`}>{chunk.name}</td>
                <td>{count(chunk.rows)}</td><td>{bytes(chunk.total_bytes)}</td>
                <td>{chunk.compressed ? "Columnstore" : "Rowstore"}</td>
              </tr>)}</tbody>
            </table>
          </div>
          {shown < chunks.length && <button className="btn small" onClick={() => setShown((n) => n + 20)}>Show 20 more ({chunks.length - shown} remaining)</button>}
          <p className="muted">Sizes include compressed storage. Chunks are managed by TimescaleDB; index advice targets the hypertable.</p>
        </>
      )}
    </details>
  );
}

function Fact({ label, value, detail, warn }: { label: string; value: string; detail?: ReactNode; warn?: boolean }) {
  return (
    <div className={`fact ${warn ? "warn" : ""}`}>
      <div className="l">{label}</div>
      <div className="v">{value}</div>
      {detail && <div className="d">{detail}</div>}
    </div>
  );
}

type Links = ReturnType<typeof relations>;

function Columns({
  report,
  table,
  indexes,
  links,
  onOpenTable,
}: {
  report: Report;
  table: TableStat;
  indexes: IndexStat[];
  links: Links;
  onOpenTable: (key: string) => void;
}) {
  const columns = report.snapshot.columns.filter((c) => c.schema === table.schema && c.table === table.name).sort((a, b) => a.ordinal - b.ordinal);
  if (columns.length === 0) {
    const note = report.snapshot.notes.find((n) => n.collector === "catalog" && n.message.startsWith(`Columns of ${tableKey(table)} are unavailable:`));
    return <div className="notice">{note?.message ?? "This scan has no column metadata. Re-scan to collect it."}</div>;
  }
  return (
    <div className="td-grid-wrap" role="region" aria-label="Columns" tabIndex={0}>
      <table className="grid td-grid columns-grid">
        <colgroup>
          <col className="num-col" />
          <col className="column-name-col" />
          <col className="column-type-col" />
          <col className="nullable-col" />
          <col />
        </colgroup>
        <thead>
          <tr>
            <th className="num-col">#</th>
            <th>Column</th>
            <th>Type</th>
            <th>Nullable</th>
            <th>In indexes</th>
          </tr>
        </thead>
        <tbody>
          {columns.map((c) => {
            const member = columnMembership(indexes, c.name);
            const pk = member.some((m) => m.index.is_primary_key && m.position != null);
            const ref = referenceOf(links.outgoing, c.name);
            const chips = member.filter((m) => !m.index.is_primary_key);
            return (
              <tr key={c.name} className="row">
                <td className="num-col muted">{c.ordinal}</td>
                <td className="col-name">
                  <span className="cn">
                    {pk && <KeyRound size={12} className="pk-icon" aria-label="Primary key" />}
                    <span className="mono" title={c.name}>
                      {c.name}
                    </span>
                    {c.generated && <span className="tag">auto</span>}
                  </span>
                  {ref && (
                    <button className="fk-link" title={`${ref.ref_table}.${ref.ref_columns[0]} · Foreign key ${ref.name}`} onClick={() => onOpenTable(`${ref.ref_schema}.${ref.ref_table}`)}>
                      <Link2 size={11} />
                      <span>{ref.ref_table}.{ref.ref_columns[0]}</span>
                    </button>
                  )}
                </td>
                <td className="type-col" title={c.default ? `${c.data_type} DEFAULT ${c.default}` : c.data_type}>
                  <span className="mono muted">{c.data_type}</span>
                  {c.default && <span className="mono default">= {c.default}</span>}
                </td>
                <td className="muted">{c.nullable ? "Yes" : "No"}</td>
                <td className="chips-col">
                  <div className="column-indexes">
                    {chips.slice(0, 3).map((m) => (
                      <span
                        key={m.index.name}
                        className={`chip-ix ${m.position === 0 ? "lead" : ""}`}
                        title={
                          m.included
                            ? `${m.index.name}: included column (stored, not searchable)`
                            : m.position === 0
                              ? `${m.index.name}: leading key, so searches on ${c.name} can seek`
                              : `${m.index.name}: key ${m.position! + 1} of ${m.index.columns.length}`
                        }
                      >
                        <span className="nm">{m.index.name}</span>
                        <span className="pos">
                          {m.included ? "incl" : m.position === 0 ? "1st" : `${m.position! + 1}${m.position === 1 ? "nd" : m.position === 2 ? "rd" : "th"}`}
                        </span>
                      </span>
                    ))}
                    {chips.length > 3 && (
                      <span
                        className="chip-ix more"
                        title={chips
                          .slice(3)
                          .map((m) => m.index.name)
                          .join(", ")}
                      >
                        +{chips.length - 3}
                      </span>
                    )}
                    {chips.length === 0 && <span className="muted">—</span>}
                  </div>
                </td>
              </tr>
            );
          })}
        </tbody>
      </table>
    </div>
  );
}

function Indexes({ report, table, indexes, links }: { report: Report; table: TableStat; indexes: IndexStat[]; links: Links }) {
  const engine = report.snapshot.server.engine;
  const usageId = useId();
  // The foreign key an index serves: its leading keys are the key's columns.
  const backs = (i: IndexStat) =>
    links.outgoing.find(({ fk }) => {
      const keys = i.columns.map((c) => c.replace(/\s+(ASC|DESC)$/i, "").toLowerCase());
      return !i.is_disabled && !i.filter && canSeek(i, fk.columns) && fk.columns.every((c, n) => keys[n] === c.toLowerCase());
    })?.fk ?? null;
  const usage = useMemo(() => indexUsage(report), [report]);
  const [open, setOpen] = useState<string | null>(null);
  const planned = plannedQueryCount(report);
  const since = report.snapshot.server.started_at;
  const hasUsage = indexes.some((index) => index.reads != null || index.writes != null);
  const sinceText = engine === "yugabyte" ? "across the recorded cluster nodes, with independent reset dates" : since ? `since ${when(since).short}` : "since the last restart";
  // An index that may be less than a week old hasn't had time to be read: the checks don't call it unused yet.
  const takenAt = Date.parse(report.snapshot.taken_at);
  const tooNew = (i: IndexStat) =>
    i.created_at_latest != null && takenAt - Date.parse(i.created_at_latest) < 7 * 24 * 3600 * 1000;
  const biggest = Math.max(1, ...indexes.map((i) => i.size_bytes ?? 0));
  const name = tableKey(table).toLowerCase();
  const finding = (index: string, check: string) =>
    report.findings.find((f) => f.check === check && f.subject.kind === "index" && f.subject.name.toLowerCase() === `${name}.${index.toLowerCase()}`);
  if (indexes.length === 0)
    return (
      <div className="notice">
        <div>
          {table.kind === "virtual" ? (
            <><b>Indexes are managed by the virtual table's module.</b> Ordinary CREATE INDEX does not apply to this table.</>
          ) : (
            <><b>No index definitions in this scan.</b> Explain the queries that read this table to check their access paths.</>
          )}
        </div>
      </div>
    );
  return (
    <>
      <div className="td-grid-wrap" role="region" aria-label="Indexes" tabIndex={0}>
        <table className="grid td-grid ix-grid">
          <colgroup>
            <col className="index-name-col" />
            <col />
            <col className="index-used-col" />
            <col className="index-activity-col" />
            <col className="index-size-col" />
          </colgroup>
          <thead>
            <tr>
              <th>Index</th>
              <th>Keys</th>
              <th>Used by</th>
              <th title={hasUsage ? `Reads and writes ${sinceText}` : "Index usage counters are unavailable in this scan"}>Reads · writes</th>
              <th className="r">Size</th>
            </tr>
          </thead>
          <tbody>
            {indexes.map((i, position) => {
              const usesId = `${usageId}-${position}`;
              const uses = usage.get(indexKey(i.schema, i.table, i.name)) ?? [];
              const wider = coveredBy(i, indexes);
              const redundant = finding(i.name, "duplicate-index");
              const unusedFinding = finding(i.name, "unused-index");
              const unprovenNativeAbsence = engine === "yugabyte" && i.reads === 0 && !i.is_primary_key && !unusedFinding;
              const neverRead = i.reads === 0 && !i.is_primary_key && !unprovenNativeAbsence;
              const expanded = open === i.name && uses.length > 0;
              const reads = i.reads ?? 0;
              const writes = i.writes ?? 0;
              const rw = Math.max(1, reads + writes);
              const notes: {
                tone: "high" | "medium" | "low";
                text: string;
                finding?: string;
              }[] = [];
              if (i.is_disabled)
                notes.push({
                  tone: "medium",
                  text: i.state === "invisible" ? "Invisible to the optimizer; still maintained by writes" : i.state === "ignored" ? "Ignored by the optimizer; still maintained by writes" : i.state === "invalid" ? `Invalid: unavailable to the optimizer; ${i.maintained == null ? "maintenance state was not recorded" : i.maintained ? "still maintained by writes" : "not ready for write maintenance"}` : "Disabled: unavailable to the optimizer and not maintained",
                });
              const serves = backs(i);
              if (unprovenNativeAbsence)
                notes.push({ tone: "low", text: `No reads recorded; complete cluster usage with at least a week of known reset history is needed before calling this index unused.${serves ? ` It also supports foreign key ${serves.name}.` : ""}` });
              if (neverRead && serves)
                notes.push({
                  tone: "low",
                  text:
                    engine === "mysql"
                      ? `Never read ${sinceText}, but it backs foreign key ${serves.name}: InnoDB requires it`
                      : `Never read ${sinceText}, but it backs foreign key ${serves.name}: without it, deleting a ${serves.ref_table} row scans ${table.name}`,
                });
              else if (neverRead && !i.is_unique && tooNew(i))
                notes.push({
                  tone: "low",
                  text: `Not read yet, but it may be new (it existed by ${when(i.created_at_latest).short}): too soon to call it unused`,
                });
              else if (neverRead && !i.is_unique)
                notes.push({
                  tone: "high",
                  text: `Never read ${sinceText}, yet every write to ${table.name} maintains it`,
                  finding: unusedFinding?.id,
                });
              else if (neverRead)
                notes.push({
                  tone: "low",
                  text: `Never read ${sinceText}; it still enforces uniqueness`,
                });
              if (wider)
                notes.push({
                  tone: "medium",
                  text: `Covered by ${wider.name}: it leads with the same keys`,
                  finding: redundant?.id,
                });
              return (
                <Fragment key={i.name}>
                  <tr className={`row ${expanded ? "selected" : ""} ${notes.length > 0 ? "ix-has-notes" : ""}`}>
                    <td className="ix-name">
                      <div className="cn">
                        <span className="mono" title={i.name}>
                          {i.name}
                        </span>
                        {i.is_primary_key && <span className="tag">PK</span>}
                        {i.is_unique && !i.is_primary_key && <span className="tag">unique</span>}
                        {i.is_clustered && !i.is_primary_key && <span className="tag">clustered</span>}
                      </div>
                    </td>
                    <td className="ix-keys mono">
                      <span className="ix-key-list" title={i.columns.join(", ")}>{i.columns.join(", ")}</span>
                      {i.docdb && <div className="muted">{i.docdb.hash_columns.length ? `HASH (${i.docdb.hash_columns.join(", ")})` : "range sharded"}{i.docdb.tablets != null ? ` · ${i.docdb.tablets} tablet${i.docdb.tablets === 1 ? "" : "s"}` : ""}</div>}
                      {i.include.length > 0 && (
                        <div className="muted" title={`INCLUDE (${i.include.join(", ")})`}>
                          + {i.include.join(", ")}
                        </div>
                      )}
                      {i.filter && (
                        <div className="muted" title={i.filter}>
                          where {i.filter}
                        </div>
                      )}
                    </td>
                    <td className="ix-used">
                      {uses.length > 0 ? (
                        <button className="used-btn" onClick={() => setOpen(expanded ? null : i.name)} aria-expanded={expanded} aria-controls={expanded ? usesId : undefined} title={`${uses.length} captured ${uses.length === 1 ? "query uses" : "queries use"} ${i.name}`}>
                          <ChevronRight size={12} className={expanded ? "rot" : ""} />
                          <b>{count(uses.length)}</b> {uses.length === 1 ? "query" : "queries"}
                        </button>
                      ) : (
                        <span
                          className="muted"
                          title={
                            planned === 0
                              ? "Culprit has no query plans for this database yet, so it can't tell which queries use each index. Its read count is on the right."
                              : `None of the ${planned} plans Culprit captured (the heaviest queries) use it; lighter queries may still read it, as the read count shows.`
                          }
                        >
                          {planned === 0 ? "no plans yet" : (i.reads ?? 0) > 0 ? "lighter queries" : "no query"}
                        </span>
                      )}
                    </td>
                    <td className="ix-rw">
                      {i.reads == null && i.writes == null ? (
                        <span className="muted">—</span>
                      ) : (
                        <span className="rw" title={`${i.reads == null ? "unmeasured" : count(reads)} reads · ${i.writes == null ? "unmeasured" : count(writes)} writes ${sinceText}`}>
                          <span className="rw-bar">
                            <span className="r" style={{ width: `${(reads / rw) * 100}%` }} />
                            <span className="w" style={{ width: `${(writes / rw) * 100}%` }} />
                          </span>
                          <span className="rw-n">
                            {count(i.reads)} · {count(i.writes)}
                          </span>
                        </span>
                      )}
                    </td>
                    <td className="r ix-size">
                      <span className="size-cell" title={engine === "yugabyte" && i.is_primary_key ? "Primary-key storage is included in the base table; no independent index size is charged" : undefined}>
                        <span className="bar">
                          <span
                            style={{
                              width: `${i.size_bytes == null ? 0 : Math.max(2, (i.size_bytes / biggest) * 100)}%`,
                            }}
                          />
                        </span>
                        {bytes(i.size_bytes)}
                      </span>
                    </td>
                  </tr>
                  {notes.length > 0 && (
                    <tr className="ix-notes">
                      <td colSpan={5}>
                        <div className="ix-notes-inner">
                          {notes.map((n) => (
                            <div key={n.text} className={`ix-note ${n.tone}`} title={n.text}>
                              {n.finding ? (
                                <button onClick={() => setState({ view: "overview", finding: n.finding! })} title="Open the finding">
                                  {n.text} <ArrowRight size={11} />
                                </button>
                              ) : n.text}
                            </div>
                          ))}
                        </div>
                      </td>
                    </tr>
                  )}
                  {expanded && (
                    <tr className="ix-uses" id={usesId}>
                      <td colSpan={5}>
                        <div className="ix-uses-inner">
                          <div className="ix-uses-head">
                            <span>Queries using this index</span>
                            <span className="muted">{uses.length} captured</span>
                          </div>
                          {uses.map((u) => (
                            <button
                              key={u.query.id}
                              className="use-row"
                              onClick={() => setState({ view: "queries", query: u.query.id })}
                              title={`${oneLine(u.query.text)}\nOpen in Queries`}
                            >
                              <span className="mono q">{oneLine(u.query.text)}</span>
                              <span className="muted ops">{u.ops.join(", ")}</span>
                              <ArrowRight size={14} className="muted use-arrow" />
                            </button>
                          ))}
                        </div>
                      </td>
                    </tr>
                  )}
                </Fragment>
              );
            })}
          </tbody>
        </table>
      </div>
      <p className="td-foot">
        “Used by” comes from the plans of the {planned} heaviest queries;{" "}
        {hasUsage
          ? `reads and writes count all activity ${sinceText}.`
          : "Index read and write counters are unavailable in this scan."}
      </p>
    </>
  );
}

function Relations({ links, onOpenTable }: { links: Links; onOpenTable: (key: string) => void }) {
  if (links.outgoing.length === 0 && links.incoming.length === 0) return <div className="notice">No foreign keys point from or to this table.</div>;
  return (
    <div className="rel">
      <section>
        <h4>
          References <span className="muted">{links.outgoing.length}</span>
        </h4>
        {links.outgoing.length === 0 && <p className="muted">This table doesn't reference others.</p>}
        {links.outgoing.map(({ fk, indexed }) => (
          <div key={fk.name} className="rel-row">
            <div className="rel-path">
              <span className="mono rel-columns" title={fk.columns.join(", ")}>{fk.columns.join(", ")}</span>
              <ArrowRight size={14} className="muted" />
              <div className="rel-endpoint">
                <button className="rel-target" title={`${fk.ref_schema}.${fk.ref_table}`} onClick={() => onOpenTable(`${fk.ref_schema}.${fk.ref_table}`)}>
                  {fk.ref_schema}.{fk.ref_table}
                </button>
                <span className="mono muted rel-columns" title={fk.ref_columns.join(", ")}>({fk.ref_columns.join(", ")})</span>
              </div>
            </div>
            <div className="rel-meta">
              <span className="muted rel-name" title={fk.name}>{fk.name}</span>
              {indexed == null ? (
                <span className="tag" title="Native sharding metadata was unavailable; lookup support cannot be established">
                  index support unknown
                </span>
              ) : indexed ? (
                <span className="tag good" title="An index leads with these columns, so joins and parent deletes can seek">
                  indexed
                </span>
              ) : (
                <span className="tag medium" title={`Deleting or updating a ${fk.ref_table} row scans this table to check ${fk.columns.join(", ")}`}>
                  not indexed
                </span>
              )}
            </div>
          </div>
        ))}
      </section>
      <section>
        <h4>
          Referenced by <span className="muted">{links.incoming.length}</span>
        </h4>
        {links.incoming.length === 0 && <p className="muted">No other table references this one.</p>}
        {links.incoming.map((fk) => (
          <div key={`${fk.schema}.${fk.table}.${fk.name}`} className="rel-row">
            <div className="rel-path">
              <div className="rel-endpoint">
                <button className="rel-target" title={`${fk.schema}.${fk.table}`} onClick={() => onOpenTable(`${fk.schema}.${fk.table}`)}>
                  {fk.schema}.{fk.table}
                </button>
                <span className="mono muted rel-columns" title={fk.columns.join(", ")}>({fk.columns.join(", ")})</span>
              </div>
              <ArrowRight size={14} className="muted" />
              <span className="mono rel-columns" title={fk.ref_columns.join(", ")}>{fk.ref_columns.join(", ")}</span>
            </div>
            <div className="rel-meta">
              <span className="muted rel-name" title={fk.name}>{fk.name}</span>
            </div>
          </div>
        ))}
      </section>
    </div>
  );
}

function Ddl({ text, original }: { text: string; original: boolean }) {
  return (
    <div className="ddl">
      <div className="data-bar">
        <span className="muted">{original ? "Table definition captured from the database." : "Rebuilt from catalog metadata."} Check it against the database before running it anywhere.</span>
        <span className="spacer" />
        <button className="btn" onClick={() => copy(text, "the DDL")}>
          <Copy size={12} /> Copy
        </button>
      </div>
      <div className="codeblock">
        <Sql text={text} pretty={false} />
      </div>
    </div>
  );
}
