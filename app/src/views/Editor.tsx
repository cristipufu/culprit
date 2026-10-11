import type { CompletionSource } from "@codemirror/autocomplete";
import { MSSQL, MySQL, PostgreSQL, keywordCompletionSource, schemaCompletionSource } from "@codemirror/lang-sql";
import { LanguageSupport } from "@codemirror/language";
import { Compartment, EditorState, type Extension } from "@codemirror/state";
import { keymap } from "@codemirror/view";
import { EditorView, basicSetup } from "codemirror";
import { Bookmark, PanelLeftClose, PanelLeftOpen, Play, Plus, RefreshCw, Save, ScanSearch, X, Zap } from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { ConfirmRun, type PendingRun } from "../components/ConfirmRun";
import { PlanDiagram } from "../components/PlanDiagram";
import { PlanView } from "../components/PlanView";
import { ResultGrid } from "../components/ResultGrid";
import { SavedRail } from "../components/SavedRail";
import type { Catalog } from "../gen/Catalog";
import type { ConnectionInfo } from "../gen/ConnectionInfo";
import type { ExplainMode } from "../gen/ExplainMode";
import type { Plan } from "../gen/Plan";
import type { ResultSet } from "../gen/ResultSet";
import type { SavedQuery } from "../gen/SavedQuery";
import { api, errorMessage, type ApiError } from "../lib/api";
import { catalogFromReport, completionConfig } from "../lib/completion";
import { duration } from "../lib/format";
import { currentReport, getState, loadSaved, setState, toast, useStore } from "../lib/store";

type Output =
  | { kind: "none" }
  | { kind: "busy"; label: string }
  | { kind: "plan"; plan: Plan }
  | { kind: "rows"; result: ResultSet; sql: string }
  | { kind: "error"; error: ApiError };

type Tab = { id: string; title: string; sql: string; saved: string | null };
type Tabs = { tabs: Tab[]; active: string };

const TABS_KEY = "culprit.editor.tabs";
const SPLIT_KEY = "culprit.editor.split";
const RAIL_KEY = "culprit.editor.rail";
const LOOK_KEY = "culprit.plan.look";
const ROW_LIMIT = 1000;
const PLACEHOLDER = "-- Write a query. ⌘↵ runs it (or the selection), ⌘E shows its plan without running it, ⌘S saves it.\n";

/** Catalogs fetched this session, per connection. */
const catalogs = new Map<string, Catalog>();

function newId(): string {
  return Math.random().toString(36).slice(2, 10);
}

function titleFor(sql: string, index: number): string {
  const words = sql
    .replace(/--.*$/gm, "")
    .replace(/\/\*[\s\S]*?\*\//g, "")
    .trim()
    .split(/\s+/)
    .filter(Boolean)
    .slice(0, 4)
    .join(" ");
  return words ? (words.length > 28 ? `${words.slice(0, 27)}…` : words) : `Query ${index}`;
}

function loadTabs(): Tabs {
  try {
    const parsed = JSON.parse(localStorage.getItem(TABS_KEY) ?? "null") as Tabs | null;
    if (parsed?.tabs?.length && parsed.tabs.some((t) => t.id === parsed.active)) return parsed;
  } catch {}
  const id = newId();
  return { tabs: [{ id, title: "Query 1", sql: PLACEHOLDER, saved: null }], active: id };
}

function storeTabs(tabs: Tabs): void {
  try {
    localStorage.setItem(TABS_KEY, JSON.stringify(tabs));
  } catch {}
}

function remembered<T extends string>(key: string, fallback: T, allowed: readonly T[]): T {
  try {
    const v = localStorage.getItem(key) as T | null;
    return v && allowed.includes(v) ? v : fallback;
  } catch {
    return fallback;
  }
}

function remember(key: string, value: string): void {
  try {
    localStorage.setItem(key, value);
  } catch {}
}

export default function Editor({ connection }: { connection: ConnectionInfo }) {
  const host = useRef<HTMLDivElement>(null);
  const view = useRef<EditorView | null>(null);
  const states = useRef(new Map<string, EditorState>());
  const language = useRef(new Compartment());
  const [tabs, setTabs] = useState<Tabs>(loadTabs);
  const tabsRef = useRef(tabs);
  tabsRef.current = tabs;
  const [outputs, setOutputs] = useState<Record<string, Output>>({});
  const [pending, setPending] = useState<PendingRun | null>(null);
  const [naming, setNaming] = useState<string | null>(null);
  const [rail, setRail] = useState(() => remembered(RAIL_KEY, "open", ["open", "closed"] as const));
  const [look, setLook] = useState(() => remembered(LOOK_KEY, "diagram", ["diagram", "tree"] as const));
  const [split, setSplit] = useState(() => {
    try {
      return Math.min(0.8, Math.max(0.2, Number(localStorage.getItem(SPLIT_KEY)) || 0.45));
    } catch {
      return 0.45;
    }
  });
  const [catalog, setCatalog] = useState<Catalog | null>(() => catalogs.get(connection.name) ?? null);
  const [catalogBusy, setCatalogBusy] = useState(false);
  const report = useStore(currentReport);
  const saved = useStore((s) => s.saved);
  const openSaved = useStore((s) => s.editorSaved);
  const incoming = useStore((s) => s.editorSql);
  const production = connection.environment === "production";
  // loadTabs and closeTab never leave the list empty.
  const activeTab = tabs.tabs.find((t) => t.id === tabs.active) ?? tabs.tabs[0]!;
  const output = outputs[activeTab.id] ?? { kind: "none" };

  const fallbackCatalog = useMemo(() => (report?.snapshot.connection === connection.name ? catalogFromReport(report) : null), [report, connection.name]);
  const effectiveCatalog = catalog ?? fallbackCatalog;

  const languageExtension = useCallback((): Extension => {
    const dialect = connection.engine === "mssql" ? MSSQL : connection.engine === "mysql" ? MySQL : PostgreSQL;
    const keywords = keywordCompletionSource(dialect, true);
    // After `alias.` only that table's columns make sense; keywords there are noise.
    const keywordsOutsideMembers: CompletionSource = (context) => (/[\w"`\]]\.\w*$/.test(context.state.sliceDoc(Math.max(0, context.pos - 120), context.pos)) ? null : keywords(context));
    return new LanguageSupport(dialect.language, [
      dialect.language.data.of({ autocomplete: schemaCompletionSource({ dialect, ...completionConfig(connection.engine, effectiveCatalog) }) }),
      dialect.language.data.of({ autocomplete: keywordsOutsideMembers }),
    ]);
  }, [connection.engine, effectiveCatalog]);

  // ── tab bookkeeping ────────────────────────────────────────────────────────────────────────────
  const updateTabs = useCallback((next: (t: Tabs) => Tabs) => {
    setTabs((current) => {
      const value = next(current);
      storeTabs(value);
      return value;
    });
  }, []);

  const currentText = () => view.current?.state.doc.toString() ?? "";

  /** The text to act on: the selection when there is one, otherwise the whole tab. */
  const target = () => {
    const state = view.current?.state;
    if (!state) return "";
    const selected = state.selection.ranges.map((r) => state.sliceDoc(r.from, r.to)).join("\n").trim();
    return selected || state.doc.toString().trim();
  };

  const setOutput = (id: string, value: Output) => setOutputs((o) => ({ ...o, [id]: value }));

  const stateFor = useCallback(
    (tab: Tab, actions: Extension) =>
      EditorState.create({
        doc: tab.sql,
        extensions: [
          basicSetup,
          language.current.of(languageExtension()),
          actions,
          EditorView.updateListener.of((update) => {
            if (!update.docChanged) return;
            const text = update.state.doc.toString();
            const id = tabsRef.current.active;
            updateTabs((t) => ({ ...t, tabs: t.tabs.map((x) => (x.id === id ? { ...x, sql: text } : x)) }));
          }),
        ],
      }),
    [languageExtension, updateTabs],
  );

  // ── actions ────────────────────────────────────────────────────────────────────────────────────
  const run = async (sql: string, confirmed = false) => {
    if (!sql || sql === PLACEHOLDER.trim()) return;
    const tab = tabsRef.current.active;
    const client = await api();
    if (production && !confirmed) {
      const check = await client.checkStatement(connection.engine, sql);
      if (!check.reads_only) return setPending({ kind: "run", sql, check });
    }
    setOutput(tab, { kind: "busy", label: "Running…" });
    try {
      const result = await client.run(connection.name, sql, ROW_LIMIT, confirmed);
      setOutput(tab, { kind: "rows", result, sql });
      const name = tabsRef.current.tabs.find((t) => t.id === tab)?.saved;
      if (name) void client.savedQueryRan(name).then(() => loadSaved());
    } catch (e) {
      const error = errorMessage(e);
      if (error.code === "confirmation_required") {
        const check = await client.checkStatement(connection.engine, sql);
        setOutput(tab, { kind: "none" });
        return setPending({ kind: "run", sql, check });
      }
      setOutput(tab, { kind: "error", error });
    }
  };

  const explain = async (mode: ExplainMode) => {
    const sql = target();
    if (!sql || sql === PLACEHOLDER.trim()) return;
    const tab = tabsRef.current.active;
    setOutput(tab, { kind: "busy", label: mode === "analyze" ? "Running and measuring…" : "Planning…" });
    try {
      const plan = await (await api()).explain(connection.name, sql, mode, true);
      setOutput(tab, { kind: "plan", plan });
    } catch (e) {
      setOutput(tab, { kind: "error", error: errorMessage(e) });
    }
  };

  const save = async (name?: string) => {
    const tab = tabsRef.current.tabs.find((t) => t.id === tabsRef.current.active);
    if (!tab) return;
    const text = currentText().trim();
    if (!text || text === PLACEHOLDER.trim()) return toast("Write a query before saving it");
    const existing = getState().saved?.find((q) => q.name === tab.saved);
    if (!tab.saved && !name) return setNaming("");
    try {
      const stored = await (await api()).saveQuery({
        name: name ?? tab.saved!,
        sql: text,
        connection: existing?.connection ?? connection.name,
        description: existing?.description ?? null,
        previous_name: tab.saved,
      });
      updateTabs((t) => ({ ...t, tabs: t.tabs.map((x) => (x.id === tab.id ? { ...x, saved: stored.name, title: stored.name } : x)) }));
      setNaming(null);
      await loadSaved();
      toast(`Saved “${stored.name}”`);
    } catch (e) {
      toast(errorMessage(e).message);
    }
  };

  const activate = useCallback(
    (id: string) => {
      const editor = view.current;
      const current = tabsRef.current.active;
      if (editor) states.current.set(current, editor.state);
      updateTabs((t) => ({ ...t, active: id }));
      tabsRef.current = { ...tabsRef.current, active: id };
      const tab = tabsRef.current.tabs.find((t) => t.id === id);
      if (editor && tab) {
        editor.setState(states.current.get(id) ?? stateFor(tab, actionKeys.current));
        editor.focus();
      }
    },
    [stateFor, updateTabs],
  );

  const newTab = useCallback(
    (sql = "", title?: string, savedName: string | null = null) => {
      const id = newId();
      const tab: Tab = { id, title: title ?? titleFor(sql, tabsRef.current.tabs.length + 1), sql, saved: savedName };
      updateTabs((t) => ({ ...t, tabs: [...t.tabs, tab] }));
      tabsRef.current = { ...tabsRef.current, tabs: [...tabsRef.current.tabs, tab] };
      activate(id);
      return id;
    },
    [activate, updateTabs],
  );

  const closeTab = useCallback(
    (id: string) => {
      const list = tabsRef.current.tabs;
      if (list.length === 1) {
        states.current.delete(id);
        const fresh = { id: newId(), title: "Query 1", sql: "", saved: null };
        updateTabs(() => ({ tabs: [fresh], active: fresh.id }));
        tabsRef.current = { tabs: [fresh], active: fresh.id };
        view.current?.setState(stateFor(fresh, actionKeys.current));
        return;
      }
      const index = list.findIndex((t) => t.id === id);
      const rest = list.filter((t) => t.id !== id);
      states.current.delete(id);
      setOutputs((o) => {
        const { [id]: _, ...others } = o;
        return others;
      });
      updateTabs((t) => ({ ...t, tabs: rest }));
      tabsRef.current = { ...tabsRef.current, tabs: rest };
      if (tabsRef.current.active === id) activate(rest[Math.max(0, index - 1)]!.id);
    },
    [activate, stateFor, updateTabs],
  );

  const cycle = (delta: number) => {
    const list = tabsRef.current.tabs;
    const index = list.findIndex((t) => t.id === tabsRef.current.active);
    activate(list[(index + delta + list.length) % list.length]!.id);
  };

  const openQuery = useCallback(
    (query: SavedQuery, thenRun: boolean) => {
      const open = tabsRef.current.tabs.find((t) => t.saved === query.name);
      if (open) activate(open.id);
      else newTab(query.sql, query.name, query.name);
      if (thenRun) void run(query.sql);
    },
    // run reads the latest connection through closures; re-binding on connection change is enough.
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [activate, newTab, connection.name],
  );

  const actions = useRef({ run, explain, save, newTab, closeTab, cycle, target });
  actions.current = { run, explain, save, newTab, closeTab, cycle, target };
  const actionKeys = useRef<Extension>(
    keymap.of([
      { key: "Mod-Enter", preventDefault: true, run: () => (void actions.current.run(actions.current.target()), true) },
      { key: "Mod-e", preventDefault: true, run: () => (void actions.current.explain("estimated"), true) },
      { key: "Mod-Shift-e", preventDefault: true, run: () => (void actions.current.explain("analyze"), true) },
      { key: "Mod-s", preventDefault: true, run: () => (void actions.current.save(), true) },
      { key: "Mod-t", preventDefault: true, run: () => (actions.current.newTab(), true) },
      { key: "Mod-Shift-w", preventDefault: true, run: () => (actions.current.closeTab(tabsRef.current.active), true) },
      { key: "Mod-Shift-]", preventDefault: true, run: () => (actions.current.cycle(1), true) },
      { key: "Mod-Shift-[", preventDefault: true, run: () => (actions.current.cycle(-1), true) },
    ]),
  );

  // ── lifecycle ──────────────────────────────────────────────────────────────────────────────────
  useEffect(() => {
    if (!host.current) return;
    const editor = new EditorView({ parent: host.current, state: stateFor(activeTab, actionKeys.current) });
    view.current = editor;
    editor.focus();
    return () => {
      states.current.clear();
      editor.destroy();
      view.current = null;
    };
    // The view is created once per engine; tab switches swap states.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [connection.engine]);

  // New table/column names: reconfigure the visible tab and drop cached states so other tabs pick them up on switch.
  useEffect(() => {
    const editor = view.current;
    if (!editor) return;
    editor.dispatch({ effects: language.current.reconfigure(languageExtension()) });
    for (const id of [...states.current.keys()]) if (id !== tabsRef.current.active) states.current.delete(id);
  }, [languageExtension]);

  const refreshCatalog = useCallback(async () => {
    setCatalogBusy(true);
    try {
      const fresh = await (await api()).catalog(connection.name);
      catalogs.set(connection.name, fresh);
      setCatalog(fresh);
    } catch {
      // Completion falls back to the latest report's tables; nothing to tell the person here.
    } finally {
      setCatalogBusy(false);
    }
  }, [connection.name]);

  useEffect(() => {
    setCatalog(catalogs.get(connection.name) ?? null);
    if (!catalogs.has(connection.name)) void refreshCatalog();
  }, [connection.name, refreshCatalog]);

  useEffect(() => {
    if (saved === null) void loadSaved();
  }, [saved]);

  // "Open in editor" from findings and queries: a new tab with that SQL.
  useEffect(() => {
    if (!incoming || !view.current) return;
    newTab(incoming);
    setState({ editorSql: null });
  }, [incoming, newTab]);

  // A saved query picked in the command palette.
  useEffect(() => {
    if (!openSaved || saved === null) return;
    const query = saved.find((q) => q.name === openSaved.name);
    setState({ editorSaved: null });
    if (query) openQuery(query, openSaved.run);
  }, [openSaved, saved, openQuery]);

  // ── splitter ───────────────────────────────────────────────────────────────────────────────────
  const body = useRef<HTMLDivElement>(null);
  const drag = (event: React.PointerEvent) => {
    const box = body.current?.getBoundingClientRect();
    if (!box) return;
    event.preventDefault();
    const move = (e: PointerEvent) => setSplit(Math.min(0.8, Math.max(0.2, (e.clientY - box.top) / box.height)));
    const up = () => {
      window.removeEventListener("pointermove", move);
      window.removeEventListener("pointerup", up);
      setSplit((s) => (remember(SPLIT_KEY, String(s)), s));
    };
    window.addEventListener("pointermove", move);
    window.addEventListener("pointerup", up);
  };

  const savedQuery = saved?.find((q) => q.name === activeTab.saved);
  const dirty = (tab: Tab) => {
    const stored = saved?.find((q) => q.name === tab.saved);
    return stored ? stored.sql.trim() !== tab.sql.trim() : false;
  };

  return (
    <div className="view editor-view">
      <div className="editor-tabs" role="tablist">
        <button className="icon-btn" title={rail === "open" ? "Hide saved queries" : "Show saved queries"} onClick={() => setRail((r) => { const next = r === "open" ? "closed" : "open"; remember(RAIL_KEY, next); return next; })}>
          {rail === "open" ? <PanelLeftClose size={14} /> : <PanelLeftOpen size={14} />}
        </button>
        <div className="tab-strip">
          {tabs.tabs.map((tab) => (
            <div
              key={tab.id}
              role="tab"
              aria-selected={tab.id === activeTab.id}
              className={`editor-tab ${tab.id === activeTab.id ? "on" : ""}`}
              onClick={() => activate(tab.id)}
              onAuxClick={(e) => e.button === 1 && closeTab(tab.id)}
              title={tab.saved ? `Saved query “${tab.saved}”` : tab.title}
            >
              {tab.saved && <Bookmark size={11} className="tab-saved" />}
              <span className="tab-title">{tab.title}</span>
              {dirty(tab) && <span className="tab-dirty" title="Changed since it was saved" />}
              <button className="tab-close" title="Close tab (⌘⇧W)" onClick={(e) => (e.stopPropagation(), closeTab(tab.id))}>
                <X size={11} />
              </button>
            </div>
          ))}
          <button className="icon-btn" title="New tab (⌘T)" onClick={() => newTab()}>
            <Plus size={14} />
          </button>
        </div>
      </div>
      <div className="editor-main">
        {rail === "open" && <SavedRail current={connection} saved={saved} active={activeTab.saved} onOpen={openQuery} onRenamed={(from, to) => updateTabs((t) => ({ ...t, tabs: t.tabs.map((x) => (x.saved === from ? { ...x, saved: to, title: to } : x)) }))} onDeleted={(name) => updateTabs((t) => ({ ...t, tabs: t.tabs.map((x) => (x.saved === name ? { ...x, saved: null } : x)) }))} />}
        <div className="editor-work">
          <div className="toolbar">
            <button className="btn primary" onClick={() => run(target())} title="Run the selection, or the whole tab">
              <Play size={13} /> Run <span className="kbd">⌘↵</span>
            </button>
            <button className="btn" onClick={() => explain("estimated")} title="Show the plan without running the statement">
              <ScanSearch size={14} /> Explain <span className="kbd">⌘E</span>
            </button>
            <button className="btn" onClick={() => explain("analyze")} title="Run the statement and measure each step">
              <Zap size={13} /> Analyze <span className="kbd">⌘⇧E</span>
            </button>
            {naming === null ? (
              <button className="btn" onClick={() => save()} title={activeTab.saved ? `Update “${activeTab.saved}”` : "Save as a named query"}>
                <Save size={13} /> {activeTab.saved ? "Save" : "Save as…"} <span className="kbd">⌘S</span>
              </button>
            ) : (
              <form
                className="name-query"
                onSubmit={(e) => {
                  e.preventDefault();
                  if (naming.trim()) void save(naming.trim());
                }}
              >
                <input autoFocus value={naming} placeholder="Name it, e.g. Open orders by customer" onChange={(e) => setNaming(e.target.value)} onKeyDown={(e) => e.key === "Escape" && (setNaming(null), view.current?.focus())} />
                <button className="btn primary" type="submit" disabled={!naming.trim()}>
                  Save
                </button>
              </form>
            )}
            <span className="spacer" />
            <span className={`env-note ${production ? "production" : ""}`}>
              {production ? "Production: reads run as is; anything that changes data asks first" : "Reads run read-only; changes commit"}
            </span>
            <button className="icon-btn" title={catalogBusy ? "Loading table and column names…" : `Reload table and column names${effectiveCatalog ? ` (${effectiveCatalog.tables.length} tables)` : ""}`} onClick={refreshCatalog} disabled={catalogBusy}>
              <RefreshCw size={13} className={catalogBusy ? "spin" : ""} />
            </button>
          </div>
          {savedQuery?.connection && savedQuery.connection !== connection.name && (
            <div className="editor-note">
              Saved for <b>{savedQuery.connection}</b>; running on <b>{connection.name}</b>.
            </div>
          )}
          <div className="editor-body" ref={body}>
            <div className="editor-host" ref={host} style={{ flex: `0 0 ${split * 100}%` }} />
            <div className="editor-splitter" onPointerDown={drag} role="separator" aria-orientation="horizontal" />
            <div className="editor-output">
              <OutputPane
                output={output}
                look={look}
                onLook={(next) => {
                  setLook(next);
                  remember(LOOK_KEY, next);
                }}
              />
            </div>
          </div>
        </div>
      </div>
      {pending && (
        <ConfirmRun
          pending={pending}
          connection={connection}
          onCancel={() => {
            setPending(null);
            view.current?.focus();
          }}
          onConfirm={() => {
            const sql = pending.sql;
            setPending(null);
            void run(sql, true);
          }}
        />
      )}
    </div>
  );
}

function OutputPane({ output, look, onLook }: { output: Output; look: "diagram" | "tree"; onLook: (look: "diagram" | "tree") => void }) {
  switch (output.kind) {
    case "none":
      return <div className="empty">Results and plans appear here.</div>;
    case "busy":
      return (
        <div className="empty">
          <div className="spinner" />
          {output.label}
        </div>
      );
    case "error":
      return (
        <div className="notice error" style={{ margin: 12 }}>
          <div>
            <b>{output.error.message}</b>
            {output.error.hint && <div style={{ marginTop: 4 }}>{output.error.hint}</div>}
          </div>
        </div>
      );
    case "plan":
      return (
        <div style={{ padding: 12 }}>
          <div className="output-head">
            <span className="seg">
              <button className={look === "diagram" ? "on" : ""} onClick={() => onLook("diagram")}>
                Diagram
              </button>
              <button className={look === "tree" ? "on" : ""} onClick={() => onLook("tree")}>
                Tree
              </button>
            </span>
          </div>
          {look === "diagram" ? <PlanDiagram plan={output.plan} focus={null} /> : <PlanView plan={output.plan} />}
        </div>
      );
    case "rows": {
      const r = output.result;
      const summary = r.columns.length === 0 ? r.messages.join(" ") : `${r.rows.length.toLocaleString()} row${r.rows.length === 1 ? "" : "s"}${r.truncated ? ` (first ${ROW_LIMIT.toLocaleString()})` : ""}`;
      if (r.columns.length === 0) {
        return (
          <div className="result-message">
            <b>{r.messages.join(" ") || "Done."}</b> <span className="muted">· {duration(r.elapsed_ms)}</span>
          </div>
        );
      }
      return <ResultGrid result={r} summary={`${summary} · ${duration(r.elapsed_ms)}`} />;
    }
  }
}
