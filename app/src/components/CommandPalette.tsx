import { Activity, Bookmark, CircleDot, Database, Gauge, GitCompareArrows, ListOrdered, Moon, Plus, RefreshCw, SquareTerminal, Sun, Table2, TextSearch } from "lucide-react";
import { useEffect, useMemo, useState, type ReactNode } from "react";
import { useTheme } from "../lib/theme";
import { EngineBadge } from "./EngineBadge";
import { useHotkeys } from "../lib/hotkeys";
import { currentReport, loadSaved, openConnection, scan, setState, useStore, type View } from "../lib/store";

type Item = { id: string; section: string; title: string; hint?: string; icon?: ReactNode; run: () => void };

const VIEWS: { view: View; title: string; icon: ReactNode }[] = [
  { view: "dashboard", title: "Dashboard", icon: <Gauge size={13} /> },
  { view: "overview", title: "Findings", icon: <CircleDot size={13} /> },
  { view: "queries", title: "Queries", icon: <ListOrdered size={13} /> },
  { view: "live", title: "Live sessions", icon: <Activity size={13} /> },
  { view: "schema", title: "Schema", icon: <Table2 size={13} /> },
  { view: "editor", title: "Editor", icon: <SquareTerminal size={13} /> },
  { view: "plans", title: "Plans", icon: <GitCompareArrows size={13} /> },
  { view: "connections", title: "Connections", icon: <Database size={13} /> },
];

const SEVERITY_COLOR: Record<string, string> = { high: "var(--high)", medium: "var(--medium)", low: "var(--low)" };

const icon = (node: ReactNode) => <span className="p-icon">{node}</span>;

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
  const [theme, , setTheme] = useTheme();

  const items = useMemo<Item[]>(() => {
    const close = () => setState({ palette: false });
    const list: Item[] = [];
    if (active) {
      list.push({ id: "rescan", section: "Actions", title: "Scan now", hint: "⌘R", icon: icon(<RefreshCw size={13} />), run: () => (close(), scan(active)) });
      list.push({ id: "new-tab", section: "Actions", title: "New query tab", hint: "⌘T", icon: icon(<Plus size={13} />), run: () => setState({ view: "editor", editorSql: " ", palette: false }) });
    }
    list.push({ id: "paste-plan", section: "Actions", title: "Open or compare a plan", icon: icon(<GitCompareArrows size={13} />), run: () => setState({ view: "plans", palette: false }) });
    const dark = theme === "dark" || (theme === "system" && window.matchMedia?.("(prefers-color-scheme: dark)").matches);
    list.push({
      id: "theme",
      section: "Actions",
      title: dark ? "Switch to light theme" : "Switch to dark theme",
      icon: icon(dark ? <Sun size={13} /> : <Moon size={13} />),
      run: () => (setTheme(dark ? "light" : "dark"), close()),
    });
    VIEWS.filter((v) => active || v.view === "plans" || v.view === "connections").forEach((v) =>
      list.push({ id: `view:${v.view}`, section: "Go to", title: v.title, icon: icon(v.icon), run: () => setState({ view: v.view, palette: false }) }),
    );
    saved?.forEach((q) => {
      const where = q.connection ? ` · ${q.connection}` : "";
      list.push({ id: `s:${q.name}`, section: "Saved queries", title: `Run ${q.name}`, hint: `saved query${where}`, icon: icon(<Bookmark size={13} />), run: () => setState({ view: "editor", editorSaved: { name: q.name, run: true }, palette: false }) });
    });
    report?.findings.forEach((f) =>
      list.push({ id: `f:${f.id}`, section: "Findings", title: f.title, hint: f.impact.label, icon: icon(<CircleDot size={12} color={SEVERITY_COLOR[f.severity]} />), run: () => setState({ view: "overview", finding: f.id, palette: false }) }),
    );
    report?.snapshot.workload?.queries.forEach((q) =>
      list.push({ id: `q:${q.id}`, section: "Queries", title: q.text.replace(/\s+/g, " ").slice(0, 120), hint: `query ${q.id}`, icon: icon(<TextSearch size={13} />), run: () => setState({ view: "queries", query: q.id, palette: false }) }),
    );
    connections.forEach((c) =>
      list.push({ id: `c:${c.name}`, section: "Connections", title: c.name, hint: c.display, icon: <EngineBadge engine={c.engine} environment={c.environment} product={c.last_report?.product} />, run: () => (close(), openConnection(c.name)) }),
    );
    list.push({ id: "new", section: "Actions", title: "Add a connection", icon: icon(<Plus size={13} />), run: () => setState({ view: "connections", palette: false }) });
    return list;
  }, [connections, report, active, saved, theme, setTheme]);

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
                {item.icon}
                <span className="t">{item.title}</span>
                {item.hint && <span className="k">{item.hint}</span>}
              </button>
            </div>
          ))}
          {results.length === 0 && <div className="muted" style={{ padding: 12 }}>No matches</div>}
        </div>
        <div className="palette-foot" aria-hidden="true">
          <span>
            <span className="kbd">↑</span>
            <span className="kbd">↓</span> move
          </span>
          <span>
            <span className="kbd">↵</span> open
          </span>
          <span>
            <span className="kbd">esc</span> close
          </span>
        </div>
      </div>
    </div>
  );
}
