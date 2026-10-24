import { useState } from "react";
import type { MemoryLimit } from "../gen/MemoryLimit";
import type { NodeResources } from "../gen/NodeResources";
import { bytes, percent, when } from "../lib/format";

const PAGE_SIZE = 12;
const memory = (n: number | null) => n === null ? "Not provided" : bytes(n);
const cpu = (n: number | null) => n === null ? "Not provided" : percent(n);
const limit = (n: MemoryLimit | null) => n === null ? "Not provided" : n.kind === "unlimited" ? "Unlimited" : bytes(n.bytes);

export function NodeResourcesView({ resources: r }: { resources: NodeResources }) {
  const [page, setPage] = useState(0);
  const nodes = r.nodes.slice(0, 1000).sort((a, b) => Number(a.available) - Number(b.available));
  const currentPage = Math.min(page, Math.max(0, Math.ceil(nodes.length / PAGE_SIZE) - 1));
  const visible = nodes.slice(currentPage * PAGE_SIZE, (currentPage + 1) * PAGE_SIZE);
  return <section className="operational-card" aria-label="Shared node resource measurements">
    <h4>Shared node resources · {nodes.length} retained {nodes.length === 1 ? "node" : "nodes"}</h4>
    <p className="muted">{r.source}. {r.nodes_expected === null ? "Node inventory could not be established." : `${r.nodes_expected} ${r.nodes_expected === 1 ? "node" : "nodes"} expected.`} {r.partial && "This read was partial."}</p>
    <p className="muted" title={`${r.observed_from} — ${r.observed_until}`}>Read {when(r.observed_from).short} to {when(r.observed_until).short}.</p>
    {r.notes.map((note, i) => <p className="muted" key={i}>{note}</p>)}
    {!nodes.length ? <p>No node measurements were retained.</p> : <div style={{ overflowX: "auto" }}><table className="grid"><thead><tr>
      <th>Node</th><th>Host CPU user / system</th><th>Host memory available / total</th><th>Tracked process memory</th><th>Soft limit</th><th>Hard limit</th>
    </tr></thead><tbody>{visible.map((n) => <tr key={n.node}>
      <td className="mono" title={n.node}>{n.node.slice(0, 8)}</td>
      {!n.available ? <td colSpan={5}>Resource measurements unavailable for this node</td> : <>
        <td>{cpu(n.cpu_user_fraction)} / {cpu(n.cpu_system_fraction)}</td>
        <td>{memory(n.host_memory_available_bytes)} / {memory(n.host_memory_total_bytes)}</td>
        <td title={n.process_memory?.name}>{memory(n.process_memory?.tracked_bytes ?? null)}</td>
        <td>{limit(n.process_memory?.soft_limit ?? null)}</td><td>{limit(n.process_memory?.hard_limit ?? null)}</td>
      </>}
    </tr>)}</tbody></table></div>}
    {nodes.length > PAGE_SIZE && <div className="actions">
      <button className="btn" disabled={currentPage === 0} onClick={() => setPage(currentPage - 1)}>Previous nodes</button>
      <span className="muted">{currentPage * PAGE_SIZE + 1}–{Math.min((currentPage + 1) * PAGE_SIZE, nodes.length)} of {nodes.length}</span>
      <button className="btn" disabled={(currentPage + 1) * PAGE_SIZE >= nodes.length} onClick={() => setPage(currentPage + 1)}>Next nodes</button>
    </div>}
    {r.nodes.length > nodes.length && <p className="muted">Showing the first {nodes.length} of {r.nodes.length} nodes in this saved input.</p>}
  </section>;
}
