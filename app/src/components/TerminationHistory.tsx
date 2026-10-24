import { useState } from "react";
import type { TerminationHistory } from "../gen/TerminationHistory";
import type { TerminationReason } from "../gen/TerminationReason";
import type { Report } from "../gen/Report";
import { when } from "../lib/format";
import { setState } from "../lib/store";

const REASONS: Record<TerminationReason, string> = {
  "temp-file-limit": "Temporary-file limit exceeded",
  sigkill: "Backend terminated by SIGKILL",
  sigsegv: "Backend terminated by SIGSEGV",
  other: "Other native termination",
};
const PAGE_SIZE = 25;

export function TerminationHistoryView({ history, report }: { history: TerminationHistory; report: Report }) {
  const [page, setPage] = useState(0);
  const records = history.events.slice(0, 1000);
  const currentPage = Math.min(page, Math.max(0, Math.ceil(records.length / PAGE_SIZE) - 1));
  const events = records.slice(currentPage * PAGE_SIZE, (currentPage + 1) * PAGE_SIZE);
  const queries = new Set(report.snapshot.workload?.queries.map((q) => q.id));
  return <section className="operational-card" aria-label="Recorded unexpected terminations">
    <h4>Recorded unexpected terminations · {records.length} retained records</h4>
    <p className="muted">{history.source}. {history.visibility}. {history.partial && "This read was partial."}</p>
    <p className="muted" title={`${history.requested_start} — ${history.requested_end}`}>Requested {when(history.requested_start).short} to {when(history.requested_end).short}.</p>
    {history.notes.map((note, i) => <p className="muted" key={i}>{note}</p>)}
    {history.events.length > records.length && <p className="muted">Showing the first {records.length} of {history.events.length} records in this saved input.</p>}
    {!records.length ? <p>No records were retained in this read. This does not establish that no queries failed.</p> : <>
      <div style={{ overflowX: "auto" }}><table className="grid"><thead><tr><th>Terminated</th><th>Reason</th><th>Node</th><th>Query</th></tr></thead>
        <tbody>{events.map((event, i) => <tr key={i}>
          <td title={event.ended_at}>{when(event.ended_at).short}</td><td>{REASONS[event.reason]}</td>
          <td className="mono" title={event.node ?? undefined}>{event.node?.slice(0, 8) ?? "Unknown"}</td>
          <td>{event.query_id ? queries.has(event.query_id) ? <button className="link mono" onClick={() => setState({ view: "queries", query: event.query_id! })}>{event.query_id}</button> : <span className="mono" title="This query is outside the retained statement statistics">{event.query_id}</span> : "Unresolved"}</td>
        </tr>)}</tbody></table></div>
      {records.length > PAGE_SIZE && <div className="actions">
        <button className="btn" disabled={currentPage === 0} onClick={() => setPage(currentPage - 1)}>Previous records</button>
        <span className="muted">{currentPage * PAGE_SIZE + 1}–{Math.min((currentPage + 1) * PAGE_SIZE, records.length)} of {records.length}</span>
        <button className="btn" disabled={(currentPage + 1) * PAGE_SIZE >= records.length} onClick={() => setPage(currentPage + 1)}>Next records</button>
      </div>}
    </>}
  </section>;
}
