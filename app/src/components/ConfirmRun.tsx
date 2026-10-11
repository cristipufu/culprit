import { TriangleAlert } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import type { ConnectionInfo } from "../gen/ConnectionInfo";
import type { StatementCheck } from "../gen/StatementCheck";

export type PendingRun = { kind: "run"; sql: string; check: StatementCheck };

/**
 * Asked before a statement that changes data runs on production. Cancel has focus; a statement that touches every row
 * or removes an object also needs the connection's name typed, so a reflexive Enter can't run it.
 */
export function ConfirmRun({ pending, connection, onCancel, onConfirm }: { pending: PendingRun; connection: ConnectionInfo; onCancel: () => void; onConfirm: () => void }) {
  const { check, sql } = pending;
  const risky = Boolean(check.warning || check.problem);
  const [typed, setTyped] = useState("");
  const cancel = useRef<HTMLButtonElement>(null);
  const input = useRef<HTMLInputElement>(null);
  const ready = !risky || typed.trim() === connection.name;

  useEffect(() => {
    (risky ? input.current : cancel.current)?.focus();
    const onKey = (e: KeyboardEvent) => e.key === "Escape" && onCancel();
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [risky, onCancel]);

  const verb = check.verb || "statement";
  const lines = sql.split("\n");
  const preview = lines.length > 14 ? [...lines.slice(0, 14), `… ${lines.length - 14} more lines`].join("\n") : sql;

  return (
    <div className="modal-backdrop" onMouseDown={(e) => e.target === e.currentTarget && onCancel()}>
      <div className="modal confirm-run" role="alertdialog" aria-modal="true" aria-labelledby="confirm-run-title">
        <div className="confirm-head">
          <TriangleAlert size={18} />
          <div>
            <h2 id="confirm-run-title">Change data on production?</h2>
            <div className="muted">
              This {verb} runs on <span className="env-pill production">{connection.name}</span> and commits.
            </div>
          </div>
        </div>
        {(check.warning || check.problem) && <div className="confirm-warning">{check.problem ?? check.warning}</div>}
        <pre className="confirm-sql">{preview}</pre>
        {risky && (
          <label className="confirm-type">
            Type <b>{connection.name}</b> to run it
            <input ref={input} value={typed} onChange={(e) => setTyped(e.target.value)} onKeyDown={(e) => e.key === "Enter" && ready && onConfirm()} spellCheck={false} autoComplete="off" />
          </label>
        )}
        <div className="confirm-actions">
          <button ref={cancel} className="btn" onClick={onCancel}>
            Cancel
          </button>
          <button className="btn danger" disabled={!ready} onClick={onConfirm}>
            Run on {connection.name}
          </button>
        </div>
      </div>
    </div>
  );
}
