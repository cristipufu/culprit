import { sql as sqlLanguage, MSSQL, PostgreSQL } from "@codemirror/lang-sql";
import { keymap } from "@codemirror/view";
import { EditorView, basicSetup } from "codemirror";
import { Play, ScanSearch, Zap } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { PlanView } from "../components/PlanView";
import type { ConnectionInfo } from "../gen/ConnectionInfo";
import type { ExplainMode } from "../gen/ExplainMode";
import type { Plan } from "../gen/Plan";
import type { ResultSet } from "../gen/ResultSet";
import { api, errorMessage, type ApiError } from "../lib/api";
import { duration } from "../lib/format";
import { setState, useStore } from "../lib/store";

type Output = { kind: "none" } | { kind: "busy"; label: string } | { kind: "plan"; plan: Plan } | { kind: "rows"; result: ResultSet } | { kind: "error"; error: ApiError };
type Pending = { action: "run" | "analyze" } | null;

const PLACEHOLDER = "-- Write a query, then Explain (⌘E) to see its plan without running it.\n";

export default function Editor({ connection }: { connection: ConnectionInfo }) {
  const host = useRef<HTMLDivElement>(null);
  const view = useRef<EditorView | null>(null);
  const initial = useStore((s) => s.editorSql);
  const [output, setOutput] = useState<Output>({ kind: "none" });
  const [pending, setPending] = useState<Pending>(null);
  const production = connection.environment === "production";

  const text = () => view.current?.state.doc.toString().trim() ?? "";

  const explain = async (mode: ExplainMode, confirmed = false) => {
    const sql = text();
    if (!sql) return;
    setOutput({ kind: "busy", label: mode === "analyze" ? "Running and measuring…" : "Planning…" });
    try {
      const plan = await (await api()).explain(connection.name, sql, mode, confirmed);
      setOutput({ kind: "plan", plan });
    } catch (e) {
      setOutput({ kind: "error", error: errorMessage(e) });
    }
  };

  const run = async (confirmed = false) => {
    const sql = text();
    if (!sql) return;
    setOutput({ kind: "busy", label: "Running…" });
    try {
      const result = await (await api()).run(connection.name, sql, 500, confirmed);
      setOutput({ kind: "rows", result });
    } catch (e) {
      setOutput({ kind: "error", error: errorMessage(e) });
    }
  };

  const guarded = (action: "run" | "analyze") => {
    if (production) return setPending({ action });
    if (action === "run") return run();
    return explain("analyze");
  };

  const actions = useRef({ explain, guarded });
  actions.current = { explain, guarded };

  useEffect(() => {
    if (!host.current) return;
    const editor = new EditorView({
      parent: host.current,
      doc: initial ?? PLACEHOLDER,
      extensions: [
        basicSetup,
        sqlLanguage({ dialect: connection.engine === "mssql" ? MSSQL : PostgreSQL }),
        keymap.of([
          { key: "Mod-e", preventDefault: true, run: () => (actions.current.explain("estimated"), true) },
          { key: "Mod-Shift-e", preventDefault: true, run: () => (actions.current.guarded("analyze"), true) },
          { key: "Mod-Enter", preventDefault: true, run: () => (actions.current.guarded("run"), true) },
        ]),
      ],
    });
    view.current = editor;
    editor.focus();
    setState({ editorSql: null });
    return () => editor.destroy();
  }, [connection.engine]);

  useEffect(() => {
    if (!initial || !view.current) return;
    view.current.dispatch({ changes: { from: 0, to: view.current.state.doc.length, insert: initial } });
    setState({ editorSql: null });
  }, [initial]);

  return (
    <div className="view">
      <div className="toolbar">
        <button className="btn primary" onClick={() => explain("estimated")}>
          <ScanSearch size={14} /> Explain <span className="kbd">⌘E</span>
        </button>
        <button className="btn" onClick={() => guarded("analyze")}>
          <Zap size={13} /> Explain analyze <span className="kbd">⌘⇧E</span>
        </button>
        <button className="btn" onClick={() => guarded("run")}>
          <Play size={13} /> Run <span className="kbd">⌘↵</span>
        </button>
        <span className="spacer" />
        <span className="muted">{production ? "Production: Explain never executes; Run and Analyze ask first." : connection.engine === "postgres" ? "Runs in a read-only transaction" : "Explain never executes the statement"}</span>
      </div>
      {pending && (
        <div className="banner" style={{ margin: "10px 12px 0", justifyContent: "space-between" }}>
          <span>This executes the statement on the production database {connection.name}.</span>
          <span style={{ display: "flex", gap: 8 }}>
            <button className="btn" onClick={() => setPending(null)}>
              Cancel
            </button>
            <button
              className="btn primary"
              onClick={() => {
                const action = pending.action;
                setPending(null);
                if (action === "run") run(true);
                else explain("analyze", true);
              }}
            >
              Execute on production
            </button>
          </span>
        </div>
      )}
      <div className="editor-host" ref={host} />
      <div style={{ flex: 1, minHeight: 0, overflow: "auto", padding: 12 }}>
        <OutputPane output={output} />
      </div>
    </div>
  );
}

function OutputPane({ output }: { output: Output }) {
  switch (output.kind) {
    case "none":
      return <div className="empty">Plans and results appear here.</div>;
    case "busy":
      return (
        <div className="empty">
          <div className="spinner" />
          {output.label}
        </div>
      );
    case "error":
      return (
        <div className="notice error">
          <div>
            <b>{output.error.message}</b>
            {output.error.hint && <div style={{ marginTop: 4 }}>{output.error.hint}</div>}
          </div>
        </div>
      );
    case "plan":
      return <PlanView plan={output.plan} />;
    case "rows":
      return (
        <>
          <div className="muted" style={{ marginBottom: 8 }}>
            {output.result.rows.length} rows{output.result.truncated ? " (first 500)" : ""} · {duration(output.result.elapsed_ms)}
          </div>
          <table className="grid">
            <thead>
              <tr>
                {output.result.columns.map((c, i) => (
                  <th key={i}>{c}</th>
                ))}
              </tr>
            </thead>
            <tbody>
              {output.result.rows.map((row, i) => (
                <tr key={i} className="row">
                  {row.map((v, j) => (
                    <td key={j} className="mono selectable">
                      {v === null ? <span className="muted">NULL</span> : typeof v === "object" ? JSON.stringify(v) : String(v)}
                    </td>
                  ))}
                </tr>
              ))}
            </tbody>
          </table>
        </>
      );
  }
}
