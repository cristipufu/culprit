import { Search } from "lucide-react";
import { useMemo, useState } from "react";
import type { Report } from "../gen/Report";
import type { TableStat } from "../gen/TableStat";
import { ago, bytes, count } from "../lib/format";
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
    <div className="split">
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
      <section>
        <h3>Indexes</h3>
        <table className="grid">
          <thead>
            <tr>
              <th>Index</th>
              <th>Keys</th>
              <th className="r">Reads</th>
              <th className="r">Writes</th>
              <th className="r">Size</th>
            </tr>
          </thead>
          <tbody>
            {indexes.map((i) => (
              <tr key={i.name} className="row">
                <td className="mono">
                  {i.name}
                  {i.is_primary_key && <span className="tag" style={{ marginLeft: 6 }}>PK</span>}
                  {i.is_unique && !i.is_primary_key && <span className="tag" style={{ marginLeft: 6 }}>unique</span>}
                  {flagged(i.name) && <span className="tag medium" style={{ marginLeft: 6 }}>{flagged(i.name)!.check === "unused-index" ? "unused" : "redundant"}</span>}
                </td>
                <td className="mono" title={i.include.length ? `INCLUDE ${i.include.join(", ")}` : ""}>
                  {i.columns.join(", ")}
                  {i.include.length > 0 && <span className="muted"> +{i.include.length}</span>}
                </td>
                <td className="r">{count(i.reads)}</td>
                <td className="r">{count(i.writes)}</td>
                <td className="r">{bytes(i.size_bytes)}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </section>
    </div>
  );
}
