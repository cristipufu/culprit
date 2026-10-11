import { Search } from "lucide-react";
import { Fragment, useMemo, useState } from "react";
import type { Finding } from "../gen/Finding";
import type { IndexStat } from "../gen/IndexStat";
import type { Report } from "../gen/Report";
import type { TableStat } from "../gen/TableStat";
import { ago, bytes, count, oneLine } from "../lib/format";
import { indexKey, indexUsage, plannedQueryCount } from "../lib/index-usage";
import { setState } from "../lib/store";

export function Schema({ report }: { report: Report }) {
  const { snapshot } = report;
  const [filter, setFilter] = useState("");
  const [selected, setSelected] = useState<string | null>(null);
  const tables = useMemo(() => {
    const q = filter.trim().toLowerCase();
    return snapshot.tables
      .filter((t) => !q || `${t.schema}.${t.name}`.toLowerCase().includes(q))
      .sort((a, b) => (b.total_bytes ?? 0) - (a.total_bytes ?? 0));
  }, [snapshot.tables, filter]);
  const maxBytes = Math.max(1, ...tables.map((t) => t.total_bytes ?? 0));
  const current = tables.find((t) => `${t.schema}.${t.name}` === selected) ?? tables[0] ?? null;
  const findingsBySubject = (name: string) => report.findings.filter((f) => f.subject.name.toLowerCase().startsWith(name.toLowerCase()));

  return (
    <div className="split schema-split">
      <div className="pane">
        <div className="toolbar">
          <label className="search">
            <Search size={13} />
            <input value={filter} placeholder="Filter tables" onChange={(e) => setFilter(e.target.value)} />
          </label>
          <span className="spacer" />
          <span className="muted">
            {snapshot.tables.length} tables · {bytes(snapshot.server.database_size_bytes)}
          </span>
        </div>
        <div className="table-wrap">
          <table className="grid">
            <thead>
              <tr>
                <th>Table</th>
                <th>Size</th>
                <th className="r">Rows</th>
                <th className="r">Indexes</th>
                <th>Health</th>
              </tr>
            </thead>
            <tbody>
              {tables.map((t) => {
                const name = `${t.schema}.${t.name}`;
                const issues = findingsBySubject(name);
                return (
                  <tr key={name} className={`row ${current === t ? "selected" : ""}`} onClick={() => setSelected(name)}>
                    <td className="mono">{name}</td>
                    <td>
                      <span className="sharecell">
                        <span className="bar">
                          <span style={{ width: `${Math.max(2, ((t.total_bytes ?? 0) / maxBytes) * 100)}%` }} />
                        </span>
                        <span className="num">{bytes(t.total_bytes)}</span>
                      </span>
                    </td>
                    <td className="r">{count(t.rows)}</td>
                    <td className="r">{snapshot.indexes.filter((i) => i.schema === t.schema && i.table === t.name).length}</td>
                    <td>
                      {badges(t).map((b) => (
                        <span key={b} className="tag medium" style={{ marginRight: 4 }}>
                          {b}
                        </span>
                      ))}
                      {issues.length > 0 && <span className="tag high">{issues.length} finding{issues.length > 1 ? "s" : ""}</span>}
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        </div>
      </div>
      <div className="pane">{current && <TableDetail report={report} table={current} />}</div>
    </div>
  );
}

function badges(t: TableStat): string[] {
  const out: string[] = [];
  if (t.is_heap) out.push("no clustered index");
  if (!t.has_primary_key) out.push("no primary key");
  if (t.dead_rows != null && t.rows != null && t.dead_rows > Math.max(10_000, t.rows * 0.2)) out.push("dead rows");
  return out;
}

function TableDetail({ report, table }: { report: Report; table: TableStat }) {
  const name = `${table.schema}.${table.name}`;
  const indexes = report.snapshot.indexes.filter((i) => i.schema === table.schema && i.table === table.name);
  const related = report.findings.filter((f) => f.subject.name.toLowerCase().startsWith(name.toLowerCase()));
  const flagged = (index: string) => report.findings.find((f) => (f.check === "unused-index" || f.check === "duplicate-index") && f.subject.name.toLowerCase().startsWith(name.toLowerCase()) && f.summary.includes(index));
  return (
    <div className="detail" style={{ padding: "18px 20px 30px" }}>
      <div className="eyebrow">Table</div>
      <h2 className="mono" style={{ fontSize: 17 }}>
        {name}
      </h2>
      <div className="metrics">
        <div className="metric">
          <div className="value">{count(table.rows)}</div>
          <div className="label">rows</div>
        </div>
        <div className="metric">
          <div className="value">{bytes(table.data_bytes)}</div>
          <div className="label">data</div>
        </div>
        <div className="metric">
          <div className="value">{bytes(table.index_bytes)}</div>
          <div className="label">indexes</div>
        </div>
        {table.seq_scans != null && (
          <div className="metric">
            <div className="value">{count(table.seq_scans)}</div>
            <div className="label">full scans</div>
          </div>
        )}
        {table.last_analyze !== undefined && (
          <div className="metric">
            <div className="value">{ago(table.last_analyze)}</div>
            <div className="label">statistics updated</div>
          </div>
        )}
      </div>
      {related.length > 0 && (
        <section>
          <h3>Findings</h3>
          {related.map((f) => (
            <button key={f.id} className="frow" onClick={() => setState({ view: "overview", finding: f.id })}>
              <span className={`sev ${f.severity}`} />
              <span className="title">{f.title}</span>
              <span />
            </button>
          ))}
        </section>
      )}
      <ColumnsSection report={report} table={table} indexes={indexes} />
      <IndexesSection report={report} indexes={indexes} flagged={flagged} />
    </div>
  );
}

function ColumnsSection({ report, table, indexes }: { report: Report; table: TableStat; indexes: IndexStat[] }) {
  const columns = report.snapshot.columns
    .filter((c) => c.schema === table.schema && c.table === table.name)
    .sort((a, b) => a.ordinal - b.ordinal);
  if (columns.length === 0) {
    return (
      <section>
        <h3>Columns</h3>
        <div className="notice">This scan has no column metadata. Re-scan to collect it.</div>
      </section>
    );
  }
  const membership = (column: string) =>
    indexes.flatMap((i) => {
      const keys = i.columns.map((k) => k.replace(/ DESC$/, "").toLowerCase());
      const position = keys.indexOf(column.toLowerCase());
      if (position >= 0) return [{ index: i, label: i.is_primary_key ? "PK" : position === 0 ? i.name : `${i.name} · key ${position + 1}`, leading: position === 0 }];
      if (i.include.some((c) => c.toLowerCase() === column.toLowerCase())) return [{ index: i, label: `${i.name} · included`, leading: false }];
      return [];
    });
  return (
    <section>
      <h3>
        Columns <span className="muted">{columns.length}</span>
      </h3>
      <table className="grid">
        <thead>
          <tr>
            <th>Column</th>
            <th>Type</th>
            <th>Null</th>
            <th>Default</th>
            <th>Indexed by</th>
          </tr>
        </thead>
        <tbody>
          {columns.map((c) => (
            <tr key={c.name} className="row">
              <td className="mono">
                {c.name}
                {c.generated && <span className="tag" style={{ marginLeft: 6 }}>generated</span>}
              </td>
              <td className="mono muted">{c.data_type}</td>
              <td>{c.nullable ? "yes" : <span className="muted">no</span>}</td>
              <td className="mono muted" title={c.default ?? ""} style={{ maxWidth: 160, overflow: "hidden", textOverflow: "ellipsis" }}>
                {c.default ?? ""}
              </td>
              <td>
                {membership(c.name).map((m) => (
                  <span key={m.index.name} className={`tag ${m.leading ? "good" : ""}`} style={{ marginRight: 4 }} title={m.leading ? "Leading key: searches on this column can seek" : ""}>
                    {m.label}
                  </span>
                ))}
              </td>
            </tr>
          ))}
        </tbody>
      </table>
    </section>
  );
}

function IndexesSection({
  report,
  indexes,
  flagged,
}: {
  report: Report;
  indexes: IndexStat[];
  flagged: (index: string) => Finding | undefined;
}) {
  const usage = useMemo(() => indexUsage(report), [report]);
  const [open, setOpen] = useState<string | null>(null);
  const planned = plannedQueryCount(report);
  const since = report.snapshot.server.started_at;
  return (
    <section>
      <h3>
        Indexes <span className="muted">{indexes.length}</span>
      </h3>
      <table className="grid">
        <thead>
          <tr>
            <th>Index</th>
            <th>Keys</th>
            <th>Used by</th>
            <th className="r">Reads</th>
            <th className="r">Writes</th>
            <th className="r">Size</th>
          </tr>
        </thead>
        <tbody>
          {indexes.map((i) => {
            const uses = usage.get(indexKey(i.schema, i.table, i.name)) ?? [];
            const issue = flagged(i.name);
            const neverRead = i.reads === 0 && !i.is_primary_key;
            const expanded = open === i.name && uses.length > 0;
            return (
              <Fragment key={i.name}>
                <tr className={`row ${expanded ? "selected" : ""}`} onClick={() => setOpen(expanded ? null : i.name)} style={{ cursor: uses.length ? "pointer" : "default" }}>
                  <td className="mono">
                    {i.name}
                    {i.is_primary_key && <span className="tag" style={{ marginLeft: 6 }}>PK</span>}
                    {i.is_unique && !i.is_primary_key && <span className="tag" style={{ marginLeft: 6 }}>unique</span>}
                    {issue && issue.check === "duplicate-index" && <span className="tag medium" style={{ marginLeft: 6 }}>redundant</span>}
                  </td>
                  <td className="mono" title={i.include.length ? `INCLUDE ${i.include.join(", ")}` : ""}>
                    {i.columns.join(", ")}
                    {i.include.length > 0 && <span className="muted"> +{i.include.length}</span>}
                  </td>
                  <td className="wrap">
                    {uses.length > 0 ? (
                      <span>
                        <b>{uses.length}</b> {uses.length === 1 ? "query" : "queries"}
                        <span className="muted"> · {[...new Set(uses.flatMap((u) => u.ops))].slice(0, 2).join(", ").toLowerCase()}</span>
                      </span>
                    ) : neverRead ? (
                      <span className="tag high" title={since ? `No reads since ${since.slice(0, 16).replace("T", " ")}` : "No reads since the last restart"}>
                        unused
                      </span>
                    ) : (
                      <span className="muted" title="Read since the last restart, but by none of the queries whose plans Culprit captured">
                        not in captured plans
                      </span>
                    )}
                  </td>
                  <td className="r">{count(i.reads)}</td>
                  <td className="r">{count(i.writes)}</td>
                  <td className="r">{bytes(i.size_bytes)}</td>
                </tr>
                {expanded && (
                  <tr>
                    <td colSpan={6} style={{ padding: "4px 10px 10px", height: "auto" }}>
                      {uses.map((u) => (
                        <button key={u.query.id} className="frow" onClick={() => setState({ view: "queries", query: u.query.id })}>
                          <span className="sev low" />
                          <span className="title mono" style={{ fontSize: 11.5, fontWeight: 500, whiteSpace: "nowrap", overflow: "hidden", textOverflow: "ellipsis" }}>
                            {oneLine(u.query.text)}
                          </span>
                          <span className="muted" style={{ fontSize: 12, whiteSpace: "nowrap" }}>{u.ops.join(", ")}</span>
                        </button>
                      ))}
                    </td>
                  </tr>
                )}
              </Fragment>
            );
          })}
        </tbody>
      </table>
      <p className="muted" style={{ fontSize: 12, marginTop: 8 }}>
        "Used by" comes from the plans of the {planned} heaviest queries; read and write counts cover all activity
        {since ? ` since ${new Date(since).toLocaleDateString()}` : " since the last restart"}.
      </p>
    </section>
  );
}
