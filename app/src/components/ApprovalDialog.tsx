import { Clock, RotateCcw, ShieldAlert, ShieldCheck, UserRound } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { useFocusTrap } from "../lib/focus";
import type { ApprovalRequest } from "../gen/ApprovalRequest";
import { api, errorMessage } from "../lib/api";
import { ENVIRONMENT_LABEL } from "../lib/environments";
import { useStore } from "../lib/store";
import { EngineBadge } from "./EngineBadge";
import { Sql } from "./Sql";
import { keys } from "../lib/keys";

const POLL_MS = 1000;

/**
 * Statements coding agents want to run through `culprit mcp`, one at a time. Polling also tells the MCP server someone
 * is here to answer; without the app open, agents are told to ask the person to open it. Deny is the default action.
 */
export default function ApprovalDialog() {
  const [pending, setPending] = useState<ApprovalRequest[]>([]);
  const [answered, setAnswered] = useState<Set<string>>(() => new Set());
  const [error, setError] = useState<string | null>(null);
  const [now, setNow] = useState(() => Date.now());
  const connections = useStore((s) => s.connections);
  const dialog = useRef<HTMLDivElement>(null);

  useEffect(() => {
    let alive = true;
    let timer: ReturnType<typeof setTimeout> | undefined;
    const tick = async () => {
      const client = await api();
      try {
        const list = await client.pendingApprovals();
        if (alive) setPending(list);
      } catch {
        // The queue is best-effort; the agent times out and says so.
      }
      if (alive) timer = setTimeout(tick, POLL_MS);
    };
    tick();
    return () => {
      alive = false;
      clearTimeout(timer);
    };
  }, []);

  const waiting = pending.filter((r) => !answered.has(r.id));
  const request = waiting[0] ?? null;

  useEffect(() => {
    if (!request) return;
    const t = setInterval(() => setNow(Date.now()), 250);
    return () => clearInterval(t);
  }, [request?.id]);

  const decide = async (approved: boolean) => {
    if (!request) return;
    setAnswered((s) => new Set(s).add(request.id));
    try {
      await (await api()).decideApproval(request.id, request.hash, approved);
      setError(null);
    } catch (e) {
      setError(errorMessage(e).message);
    }
  };

  const secondsLeft = request ? Math.max(0, Math.ceil((Date.parse(request.expires_at) - now) / 1000)) : 0;
  const expiredFor = useRef<string | null>(null);
  useEffect(() => {
    if (request && secondsLeft === 0 && expiredFor.current !== request.id) {
      expiredFor.current = request.id;
      decide(false);
    }
  });

  // While a request waits, Tab stays inside the dialog and focus returns where it was afterwards.
  useFocusTrap(dialog, request !== null);

  if (!request) {
    return error ? (
      <div className="toast" role="status">
        {error}
      </div>
    ) : null;
  }

  const production = request.environment === "production";
  const target = connections.find((c) => c.name === request.connection);
  const engine = target?.engine;
  const total = Math.max(1, (Date.parse(request.expires_at) - Date.parse(request.requested_at)) / 1000);
  return (
    <div className="overlay approval-overlay">
      <div
        ref={dialog}
        className="approval"
        role="alertdialog"
        aria-modal="true"
        aria-labelledby="approval-title"
        onKeyDown={(e) => {
          if (e.key === "Escape") {
            e.preventDefault();
            decide(false);
          } else if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) {
            // A plain Enter lands on Deny (focused); approving takes the deliberate chord.
            e.preventDefault();
            decide(true);
          }
        }}
      >
        <div className="approval-timer" aria-hidden="true">
          <span style={{ transform: `scaleX(${secondsLeft / total})` }} />
        </div>
        <div className="approval-head">
          <ShieldAlert size={18} className={production ? "danger" : ""} />
          <div>
            <h2 id="approval-title">{request.client} wants to run a statement</h2>
            <div className="approval-sub">
              {engine && <EngineBadge engine={engine} environment={request.environment} product={target?.last_report?.product} />}
              <b>{request.connection}</b>
              <span className={`env-chip env-${request.environment}`}>{ENVIRONMENT_LABEL[request.environment]}</span>
              <span className="muted">via {request.tool}</span>
            </div>
          </div>
        </div>
        <div className="codeblock approval-sql selectable">
          <Sql text={request.sql} pretty={false} />
        </div>
        <ul className="approval-facts">
          <li>
            <ShieldCheck size={13} /> Checked: one statement that only reads
          </li>
          <li>
            <RotateCcw size={13} /> Runs inside a transaction that is rolled back, never committed
          </li>
          <li>
            <Clock size={13} /> At most 200 rows; cancelled after 30 seconds
          </li>
          <li>
            <UserRound size={13} /> As this connection's login, exactly as shown
          </li>
        </ul>
        {production && <p className="approval-prod">This is a production database: the read still costs it CPU and IO.</p>}
        <div className="approval-actions">
          <span className="muted" aria-live="polite">
            Denied automatically in {secondsLeft} s{waiting.length > 1 ? ` · ${waiting.length - 1} more waiting` : ""}
          </span>
          <span style={{ flex: 1 }} />
          <button className="btn" autoFocus onClick={() => decide(false)}>
            Deny <span className="kbd">Esc</span>
          </button>
          <button className={`btn ${production ? "danger" : "primary"}`} onClick={() => decide(true)}>
            Run once <span className="kbd">{keys("⌘↵")}</span>
          </button>
        </div>
      </div>
    </div>
  );
}
