import { useEffect, useMemo, useState } from "react";
import { useHotkeys } from "../lib/hotkeys";
import { currentReport, loadSaved, openConnection, scan, setState, useStore, type View } from "../lib/store";

type Item = { id: string; section: string; title: string; hint?: string; run: () => void };

const VIEWS: { view: View; title: string }[] = [
  { view: "overview", title: "Overview" },
  { view: "queries", title: "Queries" },
  { view: "live", title: "Live sessions" },
  { view: "schema", title: "Schema" },
  { view: "editor", title: "Editor" },
  { view: "connections", title: "Connections" },
];

function score(text: string, query: string): number {
  if (!query) return 1;
  const t = text.toLowerCase();
  const q = query.toLowerCase();
  const direct = t.indexOf(q);
  if (direct >= 0) return 100 - direct;
  let ti = 0;
  let hits = 0;
  for (const ch of q) {
    const found = t.indexOf(ch, ti);
    if (found < 0) return 0;
    hits += found === ti ? 2 : 1;
    ti = found + 1;
  }
  return hits;
}

export function CommandPalette() {
  const connections = useStore((s) => s.connections);
  const report = useStore(currentReport);
  const active = useStore((s) => s.active);
  const saved = useStore((s) => s.saved);
  const [query, setQuery] = useState("");

  useEffect(() => {
    if (saved === null) void loadSaved();
  }, [saved]);
  const [index, setIndex] = useState(0);

  const items = useMemo<Item[]>(() => {
    const close = () => setState({ palette: false });
    const list: Item[] = [];
    if (active) {
      list.push({ id: "rescan", section: "Actions", title: "Re-scan", hint: "⌘R", run: () => (close(), scan(active)) });
      VIEWS.forEach((v) => list.push({ id: `view:${v.view}`, section: "Go to", title: v.title, run: () => setState({ view: v.view, palette: false }) }));
    }
    saved?.forEach((q) => {
      const where = q.connection ? ` · ${q.connection}` : "";
      list.push({ id: `s:${q.name}`, section: "Saved queries", title: q.name, hint: `run${where}`, run: () => setState({ view: "editor", editorSaved: { name: q.name, run: true }, palette: false }) });
    });
    report?.findings.forEach((f) =>
      list.push({ id: `f:${f.id}`, section: "Findings", title: f.title, hint: f.impact.label, run: () => setState({ view: "overview", finding: f.id, palette: false }) }),
    );
    report?.snapshot.workload?.queries.forEach((q) =>
      list.push({ id: `q:${q.id}`, section: "Queries", title: q.text.replace(/\s+/g, " ").slice(0, 120), hint: `query ${q.id}`, run: () => setState({ view: "queries", query: q.id, palette: false }) }),
    );
    connections.forEach((c) => list.push({ id: `c:${c.name}`, section: "Connections", title: c.name, hint: c.display, run: () => (close(), openConnection(c.name)) }));
    list.push({ id: "new", section: "Actions", title: "New connection", run: () => setState({ view: "connections", palette: false }) });
    return list;
  }, [connections, report, active, saved]);

  const results = useMemo(() => {
    const scored = items.map((item) => ({ item, s: score(`${item.title} ${item.hint ?? ""}`, query) })).filter((x) => x.s > 0);
    if (!query) return scored.slice(0, 60).map((x) => x.item);
    scored.sort((a, b) => b.s - a.s);
    const sections = [...new Set(scored.map((x) => x.item.section))];
    return sections.flatMap((section) => scored.filter((x) => x.item.section === section).slice(0, 8)).map((x) => x.item);
  }, [items, query]);

  const choose = (i: number) => results[i]?.run();

  useHotkeys({ Escape: () => setState({ palette: false }) });

  return (
    <div className="overlay" onMouseDown={() => setState({ palette: false })}>
      <div className="palette" onMouseDown={(e) => e.stopPropagation()}>
        <input
          autoFocus
          value={query}
          placeholder="Search findings, queries, tables, actions…"
          onChange={(e) => {
            setQuery(e.target.value);
            setIndex(0);
          }}
          onKeyDown={(e) => {
            if (e.key === "ArrowDown") {
              e.preventDefault();
              setIndex((i) => Math.min(results.length - 1, i + 1));
            } else if (e.key === "ArrowUp") {
              e.preventDefault();
              setIndex((i) => Math.max(0, i - 1));
            } else if (e.key === "Enter") {
              e.preventDefault();
              choose(index);
            }
          }}
        />
        <div className="results">
          {results.map((item, i) => (
            <div key={item.id}>
              {(i === 0 || results[i - 1]?.section !== item.section) && <div className="section eyebrow">{item.section}</div>}
              <button className={`item ${i === index ? "on" : ""}`} onMouseEnter={() => setIndex(i)} onClick={() => choose(i)}>
                <span className="t">{item.title}</span>
                {item.hint && <span className="k">{item.hint}</span>}
              </button>
            </div>
          ))}
          {results.length === 0 && <div className="muted" style={{ padding: 12 }}>No matches</div>}
        </div>
      </div>
    </div>
  );
}
