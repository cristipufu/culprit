import type { CompletionSource } from "@codemirror/autocomplete";
import { MariaSQL, MSSQL, MySQL, PostgreSQL, SQLite, keywordCompletionSource, schemaCompletionSource } from "@codemirror/lang-sql";
import { HighlightStyle, LanguageSupport, syntaxHighlighting } from "@codemirror/language";
import { tags } from "@lezer/highlight";
import { Compartment, EditorState, Prec, StateEffect, StateField, type Extension } from "@codemirror/state";
import { Decoration, keymap, type DecorationSet } from "@codemirror/view";
import { EditorView, basicSetup } from "codemirror";
import { Bookmark, PanelLeftClose, PanelLeftOpen, Play, Plus, RefreshCw, Save, ScanSearch, TriangleAlert, WandSparkles, X, Zap } from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import { ConfirmRun, type PendingRun } from "../components/ConfirmRun";
import { LookToggle, PlanSurface, rememberLook, rememberedLook, type PlanLook } from "../components/PlanSurface";
import { ResultGrid } from "../components/ResultGrid";
import { SavedRail } from "../components/SavedRail";
import type { Catalog } from "../gen/Catalog";
import type { ConnectionInfo } from "../gen/ConnectionInfo";
import type { ExplainMode } from "../gen/ExplainMode";
import type { Finding } from "../gen/Finding";
import type { Plan } from "../gen/Plan";
import type { ResultSet } from "../gen/ResultSet";
import type { SavedQuery } from "../gen/SavedQuery";
import { api, errorMessage, type ApiError } from "../lib/api";
import { catalogFromReport, completionConfig } from "../lib/completion";
import { splitSqlStatements } from "../lib/editor-sql";
import { EditorTabStorage, MAX_EDITOR_TABS, newEditorTab, type EditorTab as Tab, type EditorTabs as Tabs } from "../lib/editor-tabs";
import { duration, oneLine } from "../lib/format";
import { currentReport, getState, loadSaved, setState, toast, useStore } from "../lib/store";
import { can, isMariadb, measuresRuns, sqlSyntax } from "../lib/engines";
import { keys } from "../lib/keys";

type StatementResult = { result: ResultSet; sql: string };

type Output =
  | { kind: "none" }
  | { kind: "busy"; label: string }
  | { kind: "plan"; plan: Plan }
  | { kind: "rows"; results: StatementResult[] }
  | { kind: "error"; error: ApiError };

const SPLIT_KEY = "culprit.editor.split";
const RAIL_KEY = "culprit.editor.rail";
const ROW_LIMIT = 1000;
const PLACEHOLDER = keys("-- Write a query. ⌘↵ runs it (or the selection), ⌘E shows its plan without running it, ⌘S saves it.\n");
const HAS_DDL = /\b(create|alter|drop|truncate|rename|reindex|vacuum|cluster)\b/i;

const setLintEffect = StateEffect.define<Finding[]>();

const DOLLAR_TAG_RE = /^\$(?:[A-Za-z_][A-Za-z0-9_]*)?\$/;

function extractSummaryStatement(summary: string): string | null {
  const prefix = summary.startsWith("The statement `")
    ? "The statement `"
    : summary.startsWith("The migration statement `")
      ? "The migration statement `"
      : null;
  if (prefix !== null) {
    const rest = summary.slice(prefix.length);
    const closers = [
      "` uses `CONCURRENTLY`",
      "` drops `",
      "` truncates ",
      "` runs `",
      "` rebuilds ",
      "` builds an index ",
      "` alters ",
      "` adds ",
      "` references ",
    ];
    let best = -1;
    for (const c of closers) {
      const idx = rest.indexOf(c);
      if (idx !== -1 && (best === -1 || idx < best)) best = idx;
    }
    if (best !== -1) return rest.slice(0, best);
    return rest.split("`")[0] ?? null;
  }
  return summary.split("`")[1] ?? null;
}

function normalizeSqlWithOffsets(docText: string): { norm: string; offsets: number[] } {
  let norm = "";
  const offsets: number[] = [];
  let inSingle = false;
  let inDouble = false;
  let inBacktick = false;
  let inBracket = false;
  let dollarTag: string | null = null;
  let lastSpace = true;

  for (let i = 0; i < docText.length; i++) {
    const ch = docText[i]!;
    const next = docText[i + 1];
    if (dollarTag !== null) {
      if (/\s/.test(ch)) {
        if (!lastSpace) {
          norm += " ";
          offsets.push(i);
          lastSpace = true;
        }
      } else {
        norm += ch.toLowerCase();
        offsets.push(i);
        lastSpace = false;
      }
      if (ch === "$" && docText.startsWith(dollarTag, i)) {
        for (let k = 1; k < dollarTag.length; k++) {
          norm += docText[i + k]!.toLowerCase();
          offsets.push(i + k);
        }
        i += dollarTag.length - 1;
        dollarTag = null;
      }
      continue;
    }
    if (inSingle && ch === "\\" && i + 1 < docText.length) {
      norm += ch.toLowerCase() + next!.toLowerCase();
      offsets.push(i, i + 1);
      lastSpace = false;
      i += 1;
      continue;
    }
    if (!inSingle && !inDouble && !inBacktick && !inBracket) {
      if (ch === "$") {
        const m = DOLLAR_TAG_RE.exec(docText.slice(i));
        if (m) {
          dollarTag = m[0];
          for (let k = 0; k < dollarTag.length; k++) {
            norm += docText[i + k]!.toLowerCase();
            offsets.push(i + k);
          }
          lastSpace = false;
          i += dollarTag.length - 1;
          continue;
        }
      }
      if (ch === "-" && next === "-") {
        const nl = docText.indexOf("\n", i + 2);
        if (nl === -1) break;
        i = nl;
        if (!lastSpace) {
          norm += " ";
          offsets.push(i);
          lastSpace = true;
        }
        continue;
      }
      if (ch === "/" && next === "*") {
        const end = docText.indexOf("*/", i + 2);
        if (end === -1) break;
        i = end + 1;
        if (!lastSpace) {
          norm += " ";
          offsets.push(i);
          lastSpace = true;
        }
        continue;
      }
    }
    if (ch === "'" && !inDouble && !inBacktick && !inBracket) inSingle = !inSingle;
    else if (ch === '"' && !inSingle && !inBacktick && !inBracket) inDouble = !inDouble;
    else if (ch === "`" && !inSingle && !inDouble && !inBracket) inBacktick = !inBacktick;
    else if (ch === "[" && !inSingle && !inDouble && !inBacktick && !inBracket) inBracket = true;
    else if (ch === "]" && inBracket) inBracket = false;

    if (/\s/.test(ch)) {
      if (!lastSpace) {
        norm += " ";
        offsets.push(i);
        lastSpace = true;
      }
    } else {
      norm += ch.toLowerCase();
      offsets.push(i);
      lastSpace = false;
    }
  }
  return { norm, offsets };
}

function statementStartOffset(docText: string, targetOffset: number): number {
  let inSingle = false;
  let inDouble = false;
  let inBacktick = false;
  let inBracket = false;
  let dollarTag: string | null = null;
  let stmtStart: number | null = null;
  for (let i = 0; i < docText.length && i <= targetOffset; i++) {
    const ch = docText[i]!;
    const next = docText[i + 1];
    if (dollarTag !== null) {
      if (ch === "$" && docText.startsWith(dollarTag, i)) {
        i += dollarTag.length - 1;
        dollarTag = null;
      }
      continue;
    }
    if (inSingle && ch === "\\" && i + 1 < docText.length) {
      i += 1;
      continue;
    }
    if (!inSingle && !inDouble && !inBacktick && !inBracket) {
      if (ch === "$") {
        const m = DOLLAR_TAG_RE.exec(docText.slice(i));
        if (m) {
          if (stmtStart === null) stmtStart = i;
          dollarTag = m[0];
          i += dollarTag.length - 1;
          continue;
        }
      }
      if (ch === "-" && next === "-") {
        const nl = docText.indexOf("\n", i + 2);
        if (nl === -1) break;
        i = nl;
        continue;
      }
      if (ch === "/" && next === "*") {
        const end = docText.indexOf("*/", i + 2);
        if (end === -1) break;
        i = end + 1;
        continue;
      }
      if (ch === ";") {
        stmtStart = null;
        continue;
      }
    }
    if (ch === "'" && !inDouble && !inBacktick && !inBracket) inSingle = !inSingle;
    else if (ch === '"' && !inSingle && !inBacktick && !inBracket) inDouble = !inDouble;
    else if (ch === "`" && !inSingle && !inDouble && !inBracket) inBacktick = !inBacktick;
    else if (ch === "[" && !inSingle && !inDouble && !inBacktick && !inBracket) inBracket = true;
    else if (ch === "]" && inBracket) inBracket = false;
    if (!/\s/.test(ch) && stmtStart === null) {
      stmtStart = i;
    }
  }
  return stmtStart ?? targetOffset;
}

function lineSpanFromOffset(docText: string, startOffset: number): [number, number] | null {
  const lineEndRaw = docText.indexOf("\n", startOffset);
  const lineEnd = lineEndRaw === -1 ? docText.length : lineEndRaw;
  const lineSlice = docText.slice(startOffset, lineEnd);
  const trimmedEnd = startOffset + lineSlice.trimEnd().length;
  return trimmedEnd > startOffset ? [startOffset, trimmedEnd] : null;
}

function locateFindingSpan(docText: string, f: Finding): [number, number] | null {
  const { norm, offsets } = normalizeSqlWithOffsets(docText);
  const quoted = extractSummaryStatement(f.summary);
  const quotedFull = quoted
    ? quoted
        .trim()
        .replace(/;+$/, "")
        .trim()
        .replace(/\s+/g, " ")
        .toLowerCase()
    : "";
  if (quotedFull.length > 0) {
    const pos = norm.indexOf(quotedFull);
    if (pos !== -1 && offsets[pos] !== undefined) {
      const span = lineSpanFromOffset(docText, offsets[pos]!);
      if (span) return span;
    }
  }
  const prefix = quoted ? quoted.trim().split(/\s+/).slice(0, 4).join(" ").toLowerCase() : "";
  if (prefix.length > 0) {
    const pos = norm.indexOf(prefix);
    if (pos !== -1 && offsets[pos] !== undefined) {
      const span = lineSpanFromOffset(docText, statementStartOffset(docText, offsets[pos]!));
      if (span) return span;
    }
  }
  const leaf = (f.subject.name.split(".").pop() ?? f.subject.name).toLowerCase();
  if (leaf.length > 0) {
    const pos = norm.indexOf(leaf);
    if (pos !== -1 && offsets[pos] !== undefined) {
      const span = lineSpanFromOffset(docText, statementStartOffset(docText, offsets[pos]!));
      if (span) return span;
    }
  }
  return null;
}

function statementEndOffset(docText: string, from: number, fallbackTo: number): number {
  let inSingle = false;
  let inDouble = false;
  let inBacktick = false;
  let inBracket = false;
  let dollarTag: string | null = null;
  for (let i = from; i < docText.length; i++) {
    const ch = docText[i]!;
    const next = docText[i + 1];
    if (dollarTag !== null) {
      if (ch === "$" && docText.startsWith(dollarTag, i)) {
        i += dollarTag.length - 1;
        dollarTag = null;
      }
      continue;
    }
    if (inSingle && ch === "\\" && i + 1 < docText.length) {
      i += 1;
      continue;
    }
    if (!inSingle && !inDouble && !inBacktick && !inBracket) {
      if (ch === "$") {
        const m = DOLLAR_TAG_RE.exec(docText.slice(i));
        if (m) {
          dollarTag = m[0];
          i += dollarTag.length - 1;
          continue;
        }
      }
      if (ch === "-" && next === "-") {
        const nl = docText.indexOf("\n", i + 2);
        if (nl === -1) break;
        i = nl;
        continue;
      }
      if (ch === "/" && next === "*") {
        const end = docText.indexOf("*/", i + 2);
        if (end === -1) break;
        i = end + 1;
        continue;
      }
      if (ch === ";") return i + 1;
    }
    if (ch === "'" && !inDouble && !inBacktick && !inBracket) inSingle = !inSingle;
    else if (ch === '"' && !inSingle && !inBacktick && !inBracket) inDouble = !inDouble;
    else if (ch === "`" && !inSingle && !inDouble && !inBracket) inBacktick = !inBacktick;
    else if (ch === "[" && !inSingle && !inDouble && !inBacktick && !inBracket) inBracket = true;
    else if (ch === "]" && inBracket) inBracket = false;
  }
  return fallbackTo;
}

const lintField = StateField.define<DecorationSet>({
  create: () => Decoration.none,
  update(decorations, tr) {
    decorations = decorations.map(tr.changes);
    for (const effect of tr.effects) {
      if (effect.is(setLintEffect)) {
        const docText = tr.state.doc.toString();
        const marks = effect.value
          .map((f) => {
            const span = locateFindingSpan(docText, f);
            if (!span) return null;
            const cls = f.severity === "high" ? "cm-lint-error" : "cm-lint-warn";
            return Decoration.mark({ class: cls, attributes: { title: f.title } }).range(span[0], span[1]);
          })
          .filter((r): r is NonNullable<typeof r> => r !== null)
          .sort((a, b) => a.from - b.from);
        decorations = Decoration.set(marks, true);
      }
    }
    return decorations;
  },
  provide: (field) => EditorView.decorations.from(field),
});

/** SQL colors from the same tokens as every other code block, so they follow light and dark mode. */
const sqlColors = syntaxHighlighting(
  HighlightStyle.define([
    { tag: [tags.keyword, tags.operatorKeyword, tags.modifier], color: "var(--code-keyword)" },
    { tag: [tags.string, tags.special(tags.string)], color: "var(--code-string)" },
    { tag: [tags.number, tags.bool, tags.null], color: "var(--code-number)" },
    { tag: [tags.comment, tags.lineComment, tags.blockComment], color: "var(--code-comment)", fontStyle: "italic" },
    { tag: [tags.special(tags.name), tags.variableName], color: "var(--code-param)" },
    { tag: [tags.typeName, tags.standard(tags.name)], color: "var(--code-keyword)" },
  ]),
);

/** Catalogs fetched this session, per connection. */
const catalogs = new Map<string, Catalog>();

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
  const [tabStorage] = useState(() => new EditorTabStorage(connection, PLACEHOLDER));
  const [tabs, setTabs] = useState<Tabs>(tabStorage.initial);
  const [persistenceError, setPersistenceError] = useState(tabStorage.error);
  const mounted = useRef(true);
  useEffect(() => {
    mounted.current = true;
    setPersistenceError(tabStorage.migrate());
    return () => { mounted.current = false; };
  }, [tabStorage]);
  const tabsRef = useRef(tabs);
  tabsRef.current = tabs;
  const [outputs, setOutputs] = useState<Record<string, Output>>({});
  const [pending, setPending] = useState<PendingRun | null>(null);
  const [liveLint, setLiveLint] = useState<Finding[]>([]);
  const [naming, setNaming] = useState<string | null>(null);
  const [rail, setRail] = useState(() => remembered(RAIL_KEY, "open", ["open", "closed"] as const));
  const [look, setLook] = useState<PlanLook>(rememberedLook);
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
  // Loading and closing tabs always leave one tab.
  const activeTab = tabs.tabs.find((t) => t.id === tabs.active) ?? tabs.tabs[0]!;
  const output = outputs[activeTab.id] ?? { kind: "none" };

  const fallbackCatalog = useMemo(() => (report?.snapshot.connection === connection.name ? catalogFromReport(report) : null), [report, connection.name]);
  const effectiveCatalog = catalog ?? fallbackCatalog;

  const languageExtension = useCallback((): Extension => {
    const mariadb = isMariadb(connection.engine, connection.last_report?.product);
    const dialect = { mssql: MSSQL, postgres: PostgreSQL, mysql: mariadb ? MariaSQL : MySQL, sqlite: SQLite }[sqlSyntax(connection.engine)];
    const keywords = keywordCompletionSource(dialect, true);
    // After `alias.` only that table's columns make sense; keywords there are noise.
    const keywordsOutsideMembers: CompletionSource = (context) => (/[\w"`\]]\.\w*$/.test(context.state.sliceDoc(Math.max(0, context.pos - 120), context.pos)) ? null : keywords(context));
    return new LanguageSupport(dialect.language, [
      dialect.language.data.of({ autocomplete: schemaCompletionSource({ dialect, ...completionConfig(connection.engine, effectiveCatalog, report?.snapshot.connection === connection.name ? report : null) }) }),
      dialect.language.data.of({ autocomplete: keywordsOutsideMembers }),
    ]);
  }, [connection.engine, connection.last_report?.product, effectiveCatalog, report, connection.name]);

  // ── tab bookkeeping ────────────────────────────────────────────────────────────────────────────
  const updateTabs = useCallback((next: (t: Tabs) => Tabs) => {
    if (!mounted.current || getState().active !== connection.name) return;
    const value = next(tabsRef.current);
    tabsRef.current = value;
    setTabs(value);
    setPersistenceError(tabStorage.write(value));
  }, [tabStorage, connection.name]);

  const currentText = () => view.current?.state.doc.toString() ?? "";

  /** The text to act on: the selection when there is one, otherwise the whole tab. */
  const target = () => {
    const state = view.current?.state;
    if (!state) return "";
    const selected = state.selection.ranges.map((r) => state.sliceDoc(r.from, r.to)).join("\n").trim();
    return selected || state.doc.toString().trim();
  };

  const setOutput = (id: string, value: Output) => {
    if (!mounted.current || !tabsRef.current.tabs.some((t) => t.id === id)) return;
    setOutputs((o) => ({ ...o, [id]: value }));
  };

  const stateFor = useCallback(
    (tab: Tab, actions: Extension) =>
      EditorState.create({
        doc: tab.sql,
        extensions: [
          basicSetup,
          Prec.high(sqlColors),
          lintField,
          language.current.of(languageExtension()),
          // Above CodeMirror's defaults, which bind Mod-Enter to "insert blank line".
          Prec.highest(actions),
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

  const lintContextRef = useRef<string>("");

  // Instantaneous live migration linter: skipped altogether unless DDL keywords are present.
  useEffect(() => {
    const sql = activeTab.sql;
    const contextKey = `${connection.name}:${activeTab.id}`;
    if (lintContextRef.current !== contextKey) {
      lintContextRef.current = contextKey;
      setLiveLint((prev) => (prev.length === 0 ? prev : []));
      view.current?.dispatch({ effects: setLintEffect.of([]) });
    }
    if (!sql || sql === PLACEHOLDER.trim() || !HAS_DDL.test(sql)) {
      setLiveLint((prev) => (prev.length === 0 ? prev : []));
      view.current?.dispatch({ effects: setLintEffect.of([]) });
      return;
    }
    let cancelled = false;
    const timer = window.setTimeout(() => {
      void api()
        .then((client) => client.lintSql(connection.name, sql))
        .then((findings) => {
          if (cancelled) return;
          setLiveLint(findings);
          view.current?.dispatch({ effects: setLintEffect.of(findings) });
        })
        .catch(() => {
          if (cancelled) return;
          setLiveLint((prev) => (prev.length === 0 ? prev : []));
          view.current?.dispatch({ effects: setLintEffect.of([]) });
        });
    }, 200);
    return () => {
      cancelled = true;
      window.clearTimeout(timer);
    };
  }, [activeTab.id, activeTab.sql, connection.name]);

  const applyLintFix = (f: Finding) => {
    const fixSql = f.fix?.sql?.trim();
    const editor = view.current;
    if (!fixSql || !editor) return;
    const docText = editor.state.doc.toString();
    const span = locateFindingSpan(docText, f);
    if (f.id.startsWith("sql-lint:unindexed-fk:")) {
      const insertAt = span ? span[0] : 0;
      editor.dispatch({ changes: { from: insertAt, to: insertAt, insert: `${fixSql}\n` } });
      toast("Added supporting CREATE INDEX");
      return;
    }
    if (span) {
      const to = statementEndOffset(docText, span[0], span[1]);
      editor.dispatch({ changes: { from: span[0], to, insert: fixSql } });
      toast("Applied safe SQL");
    }
  };

  const applyAllLintFixes = () => {
    const editor = view.current;
    if (!editor) return;
    const docText = editor.state.doc.toString();
    const byStart = new Map<number, { from: number; to: number; prefixes: string[]; replacement: string | null }>();
    let applied = 0;
    for (const f of liveLint) {
      const fixSql = f.fix?.sql?.trim();
      if (!fixSql) continue;
      const span = locateFindingSpan(docText, f);
      if (f.id.startsWith("sql-lint:unindexed-fk:")) {
        const from = span ? span[0] : 0;
        const to = span ? statementEndOffset(docText, span[0], span[1]) : 0;
        const entry = byStart.get(from) ?? { from, to, prefixes: [], replacement: null };
        if (!entry.prefixes.includes(fixSql)) {
          entry.prefixes.push(fixSql);
          applied += 1;
        }
        byStart.set(from, entry);
      } else if (span) {
        const from = span[0];
        const to = statementEndOffset(docText, from, span[1]);
        const entry = byStart.get(from) ?? { from, to, prefixes: [], replacement: null };
        entry.to = Math.max(entry.to, to);
        if (entry.replacement === null) {
          entry.replacement = fixSql;
          applied += 1;
        }
        byStart.set(from, entry);
      }
    }
    if (applied === 0) return;
    const changes = [...byStart.values()]
      .sort((a, b) => a.from - b.from)
      .map((entry) => {
        const prefix = entry.prefixes.length > 0 ? `${entry.prefixes.join("\n")}\n` : "";
        const body = entry.replacement ?? docText.slice(entry.from, entry.to);
        return { from: entry.from, to: entry.to, insert: `${prefix}${body}` };
      });
    editor.dispatch({ changes });
    toast(`Applied ${applied} safe DDL fixes`);
  };

  // ── actions ────────────────────────────────────────────────────────────────────────────────────
  const run = async (sql: string, confirmed = false) => {
    if (!sql || sql === PLACEHOLDER.trim()) return;
    const batch = splitSqlStatements(sql);
    if (batch.length === 0) {
      return toast("No runnable SQL statement — uncomment a statement or fill in placeholders first");
    }
    const tab = tabsRef.current.active;
    const client = await api();
    if (production && !confirmed) {
      const checks = await Promise.all(batch.map((stmt) => client.checkStatement(connection.engine, stmt)));
      const nonReads = checks.filter((c) => !c.reads_only && !c.problem);
      if (nonReads.length > 0) {
        const warning = nonReads.find((c) => c.warning)?.warning ?? null;
        const check = { ...nonReads[0]!, warning };
        const lint = await client.lintSql(connection.name, sql).catch(() => []);
        return setPending({ kind: "run", sql, check, lint });
      }
    }
    setOutput(tab, { kind: "busy", label: batch.length > 1 ? `Running 1 of ${batch.length}…` : "Running…" });
    let currentIdx = 0;
    try {
      const results: StatementResult[] = [];
      for (let i = 0; i < batch.length; i++) {
        currentIdx = i;
        const stmt = batch[i]!;
        if (batch.length > 1 && i > 0) {
          setOutput(tab, { kind: "busy", label: `Running ${i + 1} of ${batch.length}…` });
        }
        const result = await client.run(connection.name, stmt, ROW_LIMIT, confirmed);
        results.push({ result, sql: stmt });
      }
      setOutput(tab, { kind: "rows", results });
      const name = tabsRef.current.tabs.find((t) => t.id === tab)?.saved;
      if (name) void client.savedQueryRan(name).then(() => loadSaved());
    } catch (e) {
      const error = errorMessage(e);
      if (error.code === "confirmation_required") {
        const checks = await Promise.all(batch.map((stmt) => client.checkStatement(connection.engine, stmt)));
        const nonReads = checks.filter((c) => !c.reads_only && !c.problem);
        const warning = nonReads.find((c) => c.warning)?.warning ?? null;
        const baseCheck = nonReads[0] ?? checks[0]!;
        const check = { ...baseCheck, warning: warning ?? baseCheck.warning };
        const lint = await client.lintSql(connection.name, sql).catch(() => []);
        setOutput(tab, { kind: "none" });
        return setPending({ kind: "run", sql, check, lint });
      }
      if (batch.length > 1) {
        error.message = `Statement ${currentIdx + 1} of ${batch.length}: ${error.message}`;
      }
      setOutput(tab, { kind: "error", error });
    }
  };

  const explain = async (mode: ExplainMode) => {
    const raw = target();
    if (!raw || raw === PLACEHOLDER.trim() || (mode === "analyze" && !measuresRuns(connection.engine))) return;
    const statements = splitSqlStatements(raw);
    if (statements.length === 0) {
      return toast("No runnable SQL statement — uncomment a statement or fill in placeholders first");
    }
    const sql = statements[statements.length - 1]!;
    const tab = tabsRef.current.active;
    setOutput(tab, { kind: "busy", label: mode === "analyze" ? "Running and measuring…" : "Planning…" });
    try {
      // No consent travels with a plan request: Culprit only measures provable reads on production, and the engine
      // enforces that whatever the UI sends.
      const plan = await (await api()).explain(connection.name, sql, mode, false);
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
      if (!tabsRef.current.tabs.some((t) => t.id === id)) return;
      if (editor && tabsRef.current.tabs.some((t) => t.id === current)) states.current.set(current, editor.state);
      updateTabs((t) => ({ ...t, active: id }));
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
      if (tabsRef.current.tabs.length >= MAX_EDITOR_TABS) {
        toast(`Close a tab before opening another (${MAX_EDITOR_TABS} tabs open)`);
        return null;
      }
      const tab = newEditorTab(sql, title ?? titleFor(sql, tabsRef.current.tabs.length + 1), savedName);
      const { id } = tab;
      updateTabs((t) => ({ ...t, tabs: [...t.tabs, tab] }));
      activate(id);
      return id;
    },
    [activate, updateTabs],
  );

  const closeTab = useCallback(
    (id: string) => {
      const list = tabsRef.current.tabs;
      if (!list.some((t) => t.id === id)) return;
      if (list.length === 1) {
        states.current.delete(id);
        setOutputs({});
        const fresh = newEditorTab();
        updateTabs(() => ({ tabs: [fresh], active: fresh.id }));
        view.current?.setState(stateFor(fresh, actionKeys.current));
        return;
      }
      const wasActive = tabsRef.current.active === id;
      const index = list.findIndex((t) => t.id === id);
      const rest = list.filter((t) => t.id !== id);
      states.current.delete(id);
      setOutputs((o) => {
        const { [id]: _, ...others } = o;
        return others;
      });
      const next = tabsRef.current.active === id ? rest[Math.max(0, index - 1)]!.id : tabsRef.current.active;
      updateTabs(() => ({ tabs: rest, active: next }));
      if (view.current && wasActive) {
        const tab = rest.find((t) => t.id === next)!;
        view.current.setState(states.current.get(next) ?? stateFor(tab, actionKeys.current));
        view.current.focus();
      }
    },
    [stateFor, updateTabs],
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
      else if (newTab(query.sql, query.name, query.name) === null) return;
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
    const current = tabsRef.current;
    const tab = current.tabs.find((tab) => tab.id === current.active) ?? current.tabs[0]!;
    const editor = new EditorView({ parent: host.current, state: stateFor(tab, actionKeys.current) });
    view.current = editor;
    editor.focus();
    return () => {
      states.current.clear();
      editor.destroy();
      view.current = null;
    };
    // The connection owns this view; tab switches swap states.
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

  // "Open in editor" from findings and queries: a new tab with that SQL, or the tab that already holds it.
  useEffect(() => {
    if (!incoming || getState().editorSql !== incoming || !view.current) return;
    const wanted = incoming.trim();
    const textOf = (tab: Tab) =>
      tab.id === tabsRef.current.active && view.current ? view.current.state.doc.toString() : (states.current.get(tab.id)?.doc.toString() ?? tab.sql);
    const open = wanted ? tabsRef.current.tabs.find((t) => textOf(t).trim() === wanted) : undefined;
    if (open) activate(open.id);
    else if (newTab(incoming) === null) return;
    setState({ editorSql: null });
  }, [incoming, newTab, activate, tabs.tabs.length]);

  // A saved query picked in the command palette.
  useEffect(() => {
    if (!openSaved || getState().editorSaved !== openSaved || saved === null) return;
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
      <div className={`editor-tabs ${production ? "is-production" : ""}`} role="tablist">
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
              <button className="tab-close" title={keys("Close tab (⌘⇧W)")} onClick={(e) => (e.stopPropagation(), closeTab(tab.id))}>
                <X size={11} />
              </button>
            </div>
          ))}
          <button className="icon-btn" title={tabs.tabs.length >= MAX_EDITOR_TABS ? "Close a tab before opening another" : keys("New tab (⌘T)")} disabled={tabs.tabs.length >= MAX_EDITOR_TABS} onClick={() => newTab()}>
            <Plus size={14} />
          </button>
        </div>
      </div>
      {persistenceError && <div className="editor-storage-error" role="alert">
        <TriangleAlert size={15} /><span>{persistenceError}</span>
        <button className="btn" onClick={() => save()}>Save query</button>
      </div>}
      <div className="editor-main">
        {rail === "open" && <SavedRail current={connection} saved={saved} active={activeTab.saved} onOpen={openQuery} onRenamed={(from, to) => updateTabs((t) => ({ ...t, tabs: t.tabs.map((x) => (x.saved === from ? { ...x, saved: to, title: to } : x)) }))} onDeleted={(name) => updateTabs((t) => ({ ...t, tabs: t.tabs.map((x) => (x.saved === name ? { ...x, saved: null } : x)) }))} />}
        <div className="editor-work">
          <div className="toolbar">
            <button className="btn primary" onClick={() => run(target())} title="Run the selection, or the whole tab">
              <Play size={13} /> Run <span className="kbd">{keys("⌘↵")}</span>
            </button>
            <button className="btn" onClick={() => explain("estimated")} title="Show the plan without running the statement">
              <ScanSearch size={14} /> Explain <span className="kbd">{keys("⌘E")}</span>
            </button>
            {measuresRuns(connection.engine) && (
              <button className="btn" onClick={() => explain("analyze")} title="Run the statement and measure each step">
                <Zap size={13} /> Analyze <span className="kbd">{keys("⌘⇧E")}</span>
              </button>
            )}
            {naming === null ? (
              <button className="btn" onClick={() => save()} title={activeTab.saved ? `Update “${activeTab.saved}”` : "Save as a named query"}>
                <Save size={13} /> {activeTab.saved ? "Save" : "Save as…"} <span className="kbd">{keys("⌘S")}</span>
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
            {!can(connection.engine, "writes") ? (
              <span className="env-note" title="Culprit opens the file read-only: statements that read run; anything that would change it is refused">
                Read-only file
              </span>
            ) : production ? (
              <span className="env-chip env-production" title="Reads run as they are; anything that changes data asks first, and statements that touch every row need the connection's name typed">
                Production · changes ask first
              </span>
            ) : (
              <span className="env-note" title="Statements Culprit can prove only read run in a read-only transaction; anything else commits">
                Reads roll back · changes commit
              </span>
            )}
            <button className="icon-btn" title={catalogBusy ? "Loading table and column names…" : `Reload table and column names${effectiveCatalog ? ` (${effectiveCatalog.tables.length} tables)` : ""}`} onClick={refreshCatalog} disabled={catalogBusy}>
              <RefreshCw size={13} className={catalogBusy ? "spin" : ""} />
            </button>
          </div>
          {savedQuery?.connection && savedQuery.connection !== connection.name && (
            <div className="editor-note">
              Saved for <b>{savedQuery.connection}</b>; running on <b>{connection.name}</b>.
            </div>
          )}
          {liveLint.length > 0 && (
            <div className="editor-lint" role="status" aria-live="polite">
              {liveLint.filter((f) => Boolean(f.fix?.sql)).length >= 2 && (
                <div className="editor-lint-row">
                  <WandSparkles size={13} className="editor-lint-icon" />
                  <div className="editor-lint-copy">
                    <strong>{liveLint.filter((f) => Boolean(f.fix?.sql)).length} auto-fixable DDL warnings</strong>
                    <span>Rewrite all fixable statements in place with non-blocking DDL</span>
                  </div>
                  <button
                    type="button"
                    className="btn ghost sm"
                    onClick={applyAllLintFixes}
                    title="Apply all safe DDL fixes"
                  >
                    <WandSparkles size={12} /> Apply all fixes
                  </button>
                </div>
              )}
              {liveLint.map((f, idx) => (
                <div key={`${f.id}:${idx}`} className="editor-lint-row">
                  <TriangleAlert size={13} className="editor-lint-icon" />
                  <div className="editor-lint-copy">
                    <strong>{f.title}</strong>
                    <span>{f.fix?.summary ?? f.summary}</span>
                  </div>
                  {f.fix?.sql && (
                    <button
                      type="button"
                      className="btn ghost sm"
                      onClick={() => applyLintFix(f)}
                      title="Replace statement with safer DDL"
                    >
                      <WandSparkles size={12} /> Quick fix
                    </button>
                  )}
                </div>
              ))}
            </div>
          )}
          <div className="editor-body" ref={body}>
            <div className="editor-host" ref={host} style={{ flex: `0 0 ${split * 100}%` }} />
            <div className="editor-splitter" onPointerDown={drag} role="separator" aria-orientation="horizontal" />
            <div className="editor-output">
              <OutputPane
                empty={<EmptyOutput saved={(saved ?? []).filter((q) => q.connection === connection.name).slice(0, 4)} onOpen={(q) => openQuery(q, false)} />}
                output={output}
                look={look}
                onLook={(next) => {
                  setLook(next);
                  rememberLook(next);
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

/** Before anything ran: the shortcuts, and this connection's saved queries one click away. */
function EmptyOutput({ saved, onOpen }: { saved: SavedQuery[]; onOpen: (q: SavedQuery) => void }) {
  const shortcuts: [string, string][] = [
    [keys("⌘↵"), "Run the selection, or the whole tab"],
    [keys("⌘E"), "Plan without running"],
    [keys("⌘⇧E"), "Run and measure each step"],
    [keys("⌘S"), "Save as a named query"],
  ];
  return (
    <div className="editor-empty">
      <ul className="editor-keys">
        {shortcuts.map(([key, label]) => (
          <li key={key}>
            <span className="kbd">{key}</span> {label}
          </li>
        ))}
      </ul>
      {saved.length > 0 && (
        <div className="editor-recent">
          <div className="eyebrow">Saved for this connection</div>
          {saved.map((q) => (
            <button key={q.name} className="editor-recent-item" onClick={() => onOpen(q)}>
              <Bookmark size={12} />
              <span>{q.name}</span>
            </button>
          ))}
        </div>
      )}
    </div>
  );
}

function OutputPane({ output, look, onLook, empty }: { output: Output; look: PlanLook; onLook: (look: PlanLook) => void; empty: ReactNode }) {
  switch (output.kind) {
    case "none":
      return <>{empty}</>;
    case "busy":
      return (
        <div className="empty">
          <div className="spinner" />
          {output.label}
        </div>
      );
    case "error":
      // "Not available here" (the browser demo, an engine that can't) is information, not a failure.
      return (
        <div className={`notice ${output.error.code === "unsupported" ? "info" : "error"}`} style={{ margin: 12 }}>
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
            <LookToggle look={look} onChange={onLook} />
          </div>
          <PlanSurface plan={output.plan} look={look} />
        </div>
      );
    case "rows": {
      const { results } = output;
      if (results.length === 1) {
        const r = results[0]!.result;
        const summary =
          r.columns.length === 0
            ? r.messages.join(" ")
            : `${r.rows.length.toLocaleString()} row${r.rows.length === 1 ? "" : "s"}${r.truncated ? ` (first ${r.rows.length.toLocaleString()}; more exist)` : ""}`;
        if (r.columns.length === 0) {
          return (
            <div className="result-message">
              <b>{r.messages.join(" ") || "Done."}</b> <span className="muted">· {duration(r.elapsed_ms)}</span>
            </div>
          );
        }
        return <ResultGrid result={r} summary={`${summary} · ${duration(r.elapsed_ms)}`} />;
      }

      const rowResults = results.filter((item) => item.result.columns.length > 0);
      const commandResults = results.filter((item) => item.result.columns.length === 0);
      const commandElapsed = commandResults.reduce((sum, item) => sum + item.result.elapsed_ms, 0);
      const commandMessages = commandResults.flatMap((item) => item.result.messages).filter(Boolean);

      if (rowResults.length === 0) {
        return (
          <div className="result-message">
            <b>
              {commandMessages.join(" · ") ||
                `${commandResults.length} statement${commandResults.length === 1 ? "" : "s"} completed.`}
            </b>{" "}
            <span className="muted">· {duration(commandElapsed)}</span>
          </div>
        );
      }

      if (rowResults.length === 1 && commandResults.length > 0) {
        const r = rowResults[0]!.result;
        const rowSummary = `${r.rows.length.toLocaleString()} row${r.rows.length === 1 ? "" : "s"}${r.truncated ? ` (first ${r.rows.length.toLocaleString()}; more exist)` : ""} · ${duration(r.elapsed_ms)}`;
        const cmdPrefix = `${commandResults.length} statement${commandResults.length === 1 ? "" : "s"} completed (${duration(commandElapsed)})`;
        return <ResultGrid result={r} summary={`${cmdPrefix} · ${rowSummary}`} />;
      }

      return (
        <div className="multi-results" style={{ display: "flex", flexDirection: "column", gap: 12, height: "100%", overflowY: "auto", paddingBottom: 12 }}>
          {commandResults.length > 0 && (
            <div className="result-message" style={{ paddingBottom: 0 }}>
              <b>
                {commandMessages.join(" · ") ||
                  `${commandResults.length} statement${commandResults.length === 1 ? "" : "s"} completed`}
              </b>{" "}
              <span className="muted">· {duration(commandElapsed)}</span>
            </div>
          )}
          {rowResults.map((item, idx) => {
            const r = item.result;
            const rawSnippet = oneLine(item.sql.replace(/--.*$/gm, "").trim());
            const snippet = rawSnippet.length > 72 ? `${rawSnippet.slice(0, 71)}…` : rawSnippet;
            const summary = `Result ${idx + 1} · ${r.rows.length.toLocaleString()} row${r.rows.length === 1 ? "" : "s"}${r.truncated ? ` (first ${r.rows.length.toLocaleString()}; more exist)` : ""} · ${duration(r.elapsed_ms)}${snippet ? ` · ${snippet}` : ""}`;
            return (
              <div key={idx} className="multi-result-pane" style={{ minHeight: 220, flex: "1 0 auto", display: "flex", flexDirection: "column", borderTop: idx > 0 || commandResults.length > 0 ? "1px solid var(--border)" : undefined }}>
                <ResultGrid result={r} summary={summary} />
              </div>
            );
          })}
        </div>
      );
    }
  }
}
