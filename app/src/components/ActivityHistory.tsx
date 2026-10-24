import { useState } from "react";
import type { ActivityCategory } from "../gen/ActivityCategory";
import type { ActivityReport } from "../gen/ActivityReport";
import { count, percent, when } from "../lib/format";
import { setState } from "../lib/store";
import { waitColor } from "../lib/waits";

export function ActivityHistoryView({ activity, selected, onSelect, onClose }: {
  activity: ActivityReport; selected?: string | null; onSelect: (key: string) => void; onClose: () => void;
}) {
  const current = activity.categories.find((c) => c.key === selected) ?? activity.categories[0];
  const p = activity.period;
  return <div aria-label="Recorded active task samples">
    <div className="notice activity-notice">
      <b>{count(p.observations)} retained observations · sample weight {count(p.weighted_observations)}</b>
      <p title={`${p.first_sample ?? ""} — ${p.last_sample ?? ""}`}>{p.first_sample && p.last_sample ? `Observed ${when(p.first_sample).short} to ${when(p.last_sample).short}.` : "No activity samples were retained in this requested period."}
        {p.partial && " This read was partial."}</p>
      <p className="muted" title={`${p.requested_start} — ${p.requested_end}`}>Requested {when(p.requested_start).short} to {when(p.requested_end).short}. {p.source}.</p>
      {p.notes.map((note, i) => <p className="muted" key={i}>{note}</p>)}
    </div>
    {current && <div className="waits-grid">
      <div className="waits-list" role="listbox" aria-label="Activity categories">
        {activity.categories.map((c) => <button key={c.key} role="option" aria-selected={c.key === current.key}
          className={`waits-row ${c.key === current.key ? "on" : ""}`} onClick={() => onSelect(c.key)}
          style={{ "--cat": waitColor(c.key) } as React.CSSProperties}>
          <i className="swatch" /><span className="waits-row-text"><span className="name">{c.label}</span></span>
          <span className="waits-row-num"><b>{percent(c.share)}</b><span>weight {count(c.weighted_observations)}</span></span>
        </button>)}
      </div>
      <ActivityDetail key={current.key} category={current} onClose={onClose} />
    </div>}
  </div>;
}

const PAGE_SIZE = 25;
function ActivityDetail({ category: c, onClose }: { category: ActivityCategory; onClose: () => void }) {
  const [page, setPage] = useState(0);
  const currentPage = Math.min(page, Math.max(0, Math.ceil(c.events.length / PAGE_SIZE) - 1));
  const events = c.events.slice(currentPage * PAGE_SIZE, (currentPage + 1) * PAGE_SIZE);
  return <div className="waits-detail">
    <div className="detail-head"><h3>{c.label}</h3><span className="pill">{percent(c.share)} of retained sample weight</span></div>
    <p className="muted">{count(c.observations)} observations · weight {count(c.weighted_observations)}. Native components can overlap; these are task observations.</p>
    <table className="grid"><thead><tr><th>Node</th><th>Component</th><th>Native event</th><th className="r">Observations</th><th className="r">Weight</th></tr></thead>
      <tbody>{events.map((e, i) => <tr key={i}><td className="mono" title={e.node}>{e.node?.slice(0, 8) ?? "Unknown"}</td><td>{e.component}</td><td className="mono" style={{ whiteSpace: "normal", overflowWrap: "anywhere" }}>{e.wait_event}{e.query_id && <div className="muted">Query {e.query_id}</div>}</td><td className="r">{count(e.observations)}</td><td className="r">{count(e.weighted_observations)}</td></tr>)}</tbody>
    </table>
    {c.events.length > PAGE_SIZE && <div className="actions">
      <button className="btn" disabled={currentPage === 0} onClick={() => setPage(currentPage - 1)}>Previous</button>
      <span className="muted">{currentPage * PAGE_SIZE + 1}–{Math.min((currentPage + 1) * PAGE_SIZE, c.events.length)} of {c.events.length} event groups</span>
      <button className="btn" disabled={(currentPage + 1) * PAGE_SIZE >= c.events.length} onClick={() => setPage(currentPage + 1)}>Next</button>
    </div>}
    <h4 className="section-title">Queries behind these observations</h4>
    <div className="wq-list">{c.queries.map((q) => <button className="wq-row" key={q.id} disabled={!q.text}
      title={q.text ? "Open in Queries" : "This query is outside the collected workload"}
      onClick={() => { setState({ view: "queries", query: q.id }); onClose(); }}>
      <span className="wq-share">{percent(q.share_of_category)}</span><span>Query {q.id}<br />{q.text ?? "Not retained in the statement statistics"}</span>
    </button>)}</div>
    {!c.queries.length && <p className="muted">These observations have no resolved statement identifier.</p>}
  </div>;
}
