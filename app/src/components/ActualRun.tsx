import { Activity, Check, Copy, History, Play, SquarePen } from "lucide-react";
import { useEffect, useState } from "react";
import type { Engine } from "../gen/Engine";
import type { Plan } from "../gen/Plan";
import type { QueryStat } from "../gen/QueryStat";
import { api, errorMessage, type ApiError } from "../lib/api";
import { canOpenQueryInEditor, prepareQueryForEditor } from "../lib/editor-sql";
import { serverName } from "../lib/engines";
import { currentReport, setState, useStore } from "../lib/store";
import { PlanSurface, type PlanLook } from "./PlanSurface";
import { copyAndTell } from "../lib/clipboard";

export type PlanSource = "plan" | "actual";

const TURN_ON = "ALTER DATABASE SCOPED CONFIGURATION SET LAST_QUERY_PLAN_STATS = ON;";

/** "Estimated" (what the optimizer planned) or "Last run" (what actually happened). */
export function PlanSourceToggle({ value, onChange }: { value: PlanSource; onChange: (v: PlanSource) => void }) {
  return (
    <span className="seg" role="tablist" aria-label="Which plan">
      <button role="tab" aria-selected={value === "plan"} className={value === "plan" ? "on" : ""} onClick={() => onChange("plan")} title="The plan the optimizer chose, with its estimates">
        Estimated
      </button>
      <button role="tab" aria-selected={value === "actual"} className={value === "actual" ? "on" : ""} onClick={() => onChange("actual")} title="Real row counts (and timings, when measured) from a run of this query">
        <History size={12} /> Last run
      </button>
    </span>
  );
}

/** Placeholders a recorded statement keeps instead of values; such text can't run as it is. */
function runnable(text: string): boolean {
  return !/(\$\d+|@P\d+|@p\d+|(^|[\s(,=<>])\?($|[\s),]))/.test(text);
}

type State =
  | { status: "loading" }
  | { status: "ready"; plan: Plan; measured: boolean }
  | { status: "missing"; error: ApiError }
  | { status: "measuring" };

/**
 * The actual run of a query: SQL Server's kept last actual plan when it has one, otherwise a calm explanation of why not
 * and the safe ways to get one (turn the setting on, measure once, or open it in the editor with real values).
 */
export function LastActualRun(props: { connection: string; engine: Engine; query: QueryStat; planId: string | null; reportId?: string; look: PlanLook }) {
  const { connection, engine, query, planId, reportId, look } = props;
  const product = useStore((s) => currentReport(s)?.snapshot.server.product);
  const [state, setRun] = useState<State>({ status: "loading" });
  const [copied, setCopied] = useState(false);

  useEffect(() => {
    let cancelled = false;
    setRun({ status: "loading" });
    api()
      .then((client) => client.lastActualPlan(connection, query.id, planId, reportId))
      .then((plan) => !cancelled && setRun({ status: "ready", plan, measured: false }))
      .catch((e) => !cancelled && setRun({ status: "missing", error: errorMessage(e) }));
    return () => {
      cancelled = true;
    };
  }, [connection, query.id, planId, reportId]);

  const measure = async () => {
    setRun({ status: "measuring" });
    try {
      const plan = await (await api()).explain(connection, query.text, "analyze", true);
      setRun({ status: "ready", plan, measured: true });
    } catch (e) {
      setRun({ status: "missing", error: errorMessage(e) });
    }
  };

  if (state.status === "loading" || state.status === "measuring") {
    return (
      <div className="plan-skeleton" aria-busy="true">
        <div className="sk-line" style={{ width: "38%" }} />
        <div className="sk-cards">
          <span />
          <span />
          <span />
        </div>
        <div className="muted">{state.status === "measuring" ? "Running it once and measuring every step…" : "Reading the last run…"}</div>
      </div>
    );
  }
  if (state.status === "ready") {
    return (
      <>
        {state.measured && (
          <div className="actual-origin">
            <Activity size={12} /> Measured just now: Culprit ran the statement once {engine === "mysql" ? "inside a read-only transaction" : "inside a transaction that was rolled back"}.
          </div>
        )}
        <PlanSurface key={`actual-${query.id}-${planId}`} plan={state.plan} look={look} />
      </>
    );
  }

  const settingOff = engine === "mssql" && state.error.hint?.includes("LAST_QUERY_PLAN_STATS");
  const canRun = runnable(query.text);
  return (
    <div className="actual-missing">
      <div className="am-title">{settingOff ? "SQL Server isn't keeping actual plans for this database yet" : state.error.message}</div>
      <p className="am-body">
        {settingOff ? (
          <>
            With <span className="mono">LAST_QUERY_PLAN_STATS</span> on, SQL Server keeps the real row counts of every cached query's latest run, so you can see which estimate was wrong without running anything.
          </>
        ) : engine === "mssql" ? (
          state.error.hint
        ) : (
          <>
            {serverName(engine, product)} only reports what a query actually did while you measure it. {canRun ? "You can run it once now and see real rows and timings for every step." : "Its recorded text has placeholders instead of values, so open it in the editor, fill in real values and measure it there."}
          </>
        )}
      </p>
      {settingOff && (
        <div className="am-code">
          <code>{TURN_ON}</code>
          <button
            className="btn small"
            onClick={() =>
              copyAndTell(TURN_ON).then((ok) => {
                if (!ok) return;
                setCopied(true);
                setTimeout(() => setCopied(false), 1500);
              })
            }
          >
            {copied ? <Check size={12} /> : <Copy size={12} />} {copied ? "Copied" : "Copy"}
          </button>
        </div>
      )}
      <div className="am-actions">
        {canRun && (
          <button className="btn primary" onClick={measure} title="Runs the statement once; reads only on production, and writes are rolled back elsewhere">
            <Play size={13} /> Measure it once
          </button>
        )}
        {(() => {
          const editorSql = prepareQueryForEditor(query, engine);
          return (
            canOpenQueryInEditor(query.text, editorSql) && (
              <button className="btn" onClick={() => setState({ view: "editor", editorSql })}>
                <SquarePen size={13} /> Open in editor
              </button>
            )
          );
        })()}
      </div>
    </div>
  );
}
