import { ChevronDown, ChevronRight, Lock } from "lucide-react";
import { useLayoutEffect, useRef, useState } from "react";
import type { BlockNode } from "../gen/BlockNode";
import type { Blocking } from "../gen/Blocking";
import type { BlockingChain } from "../gen/BlockingChain";
import type { Session } from "../gen/Session";
import type { SessionAction } from "../gen/SessionAction";
import { errorMessage } from "../lib/api";
import { duration, oneLine } from "../lib/format";

type Props = {
  blocking: Blocking;
  sessions: Session[];
  actions: SessionAction[];
  actionsNote: string | null;
  onInterrupt: (session: Session, action: SessionAction) => Promise<void>;
  onLocate: (session: string) => void;
};

const HEAD_LABEL: Record<BlockingChain["head_kind"], string> = {
  "idle-in-transaction": "Idle in an open transaction",
  running: "Running while holding locks",
  idle: "Idle, still holding locks",
  unknown: "Not visible to this login",
};

/** Every chain, biggest first: a calm card with the head blocker, the story, and the tree of who waits behind whom. */
export function BlockingChains({ blocking, sessions, actions, actionsNote, onInterrupt, onLocate }: Props) {
  return (
    <div className="block-chains" aria-label={`${blocking.waiting} sessions waiting`}>
      {blocking.chains.map((chain, i) => (
        <ChainCard key={chain.head} chain={chain} sessions={sessions} actions={actions} actionsNote={actionsNote} onInterrupt={onInterrupt} onLocate={onLocate} defaultOpen={i < 2} />
      ))}
    </div>
  );
}

function ChainCard({ chain, sessions, actions, actionsNote, onInterrupt, onLocate, defaultOpen }: Omit<Props, "blocking"> & { chain: BlockingChain; defaultOpen: boolean }) {
  const [open, setOpen] = useState(defaultOpen);
  const [adviceOpen, setAdviceOpen] = useState(false);
  const [clamped, setClamped] = useState(false);
  const adviceRef = useRef<HTMLParagraphElement>(null);
  useLayoutEffect(() => {
    const el = adviceRef.current;
    if (el && !adviceOpen) setClamped(el.scrollHeight > el.clientHeight + 1);
  }, [chain.advice, adviceOpen]);
  const head = sessions.find((s) => s.id === chain.head) ?? null;
  const severity = chain.waiters >= 5 || (chain.longest_wait_ms ?? 0) > 60_000 ? "high" : "medium";
  const treeRef = useRef<HTMLDivElement>(null);

  const moveFocus = (e: React.KeyboardEvent) => {
    if (e.key !== "ArrowDown" && e.key !== "ArrowUp") return;
    const rows = [...(treeRef.current?.querySelectorAll<HTMLElement>("[role=treeitem]") ?? [])];
    const index = rows.indexOf(document.activeElement as HTMLElement);
    const next = rows[Math.min(rows.length - 1, Math.max(0, index + (e.key === "ArrowDown" ? 1 : -1)))];
    if (next) {
      e.preventDefault();
      next.focus();
    }
  };

  return (
    <section className={`block-card ${chain.head_kind}`} aria-label={chain.headline}>
      <header className="block-head">
        <span className={`sev ${severity}`} aria-hidden />
        <h3>{chain.headline}</h3>
      </header>
      <div className="block-facts">
        <span className="fact">{HEAD_LABEL[chain.head_kind]}</span>
        {chain.transaction_ms != null && (
          <span className="fact">
            <span className="fact-label">Transaction open</span> {duration(chain.transaction_ms)}
          </span>
        )}
        <span className="fact">
          <span className="fact-label">Waiting</span> {chain.waiters} {chain.waiters === 1 ? "session" : "sessions"}
        </span>
        {chain.longest_wait_ms != null && (
          <span className="fact">
            <span className="fact-label">Longest wait</span> {duration(chain.longest_wait_ms)}
          </span>
        )}
        {chain.wait_type && <code className="fact-code" title="The wait type the engine reports">{chain.wait_type}</code>}
      </div>
      <p className={`block-advice ${adviceOpen ? "open" : ""}`} ref={adviceRef}>
        {chain.advice}{" "}
      </p>
      {!adviceOpen && clamped && (
        <button className="link-button block-more" onClick={() => setAdviceOpen(true)} aria-label="Show the full explanation">
          More
        </button>
      )}
      <div className="block-tree-head">
        <button className="link-button" aria-expanded={open} onClick={() => setOpen((o) => !o)}>
          {open ? <ChevronDown size={13} /> : <ChevronRight size={13} />} {open ? "Hide" : "Show"} who waits behind whom
        </button>
        <span className="spacer" />
        {head && actions.length > 0 && <HeadActions head={head} actions={actions} onInterrupt={onInterrupt} />}
        {actions.length === 0 && actionsNote && (
          <span className="muted block-note" title={actionsNote}>
            <Lock size={11} /> Production: end it with your own tooling
          </span>
        )}
      </div>
      {open && (
        <div className="block-tree" role="tree" ref={treeRef} onKeyDown={moveFocus}>
          <TreeRow node={chain.tree} depth={0} sessions={sessions} onLocate={onLocate} isHead />
        </div>
      )}
    </section>
  );
}

function TreeRow({ node, depth, sessions, onLocate, isHead = false }: { node: BlockNode; depth: number; sessions: Session[]; onLocate: (id: string) => void; isHead?: boolean }) {
  const s = sessions.find((x) => x.id === node.session);
  const who = s ? [s.user && s.host ? `${s.user}@${s.host}` : (s.user ?? s.host), s.application].filter(Boolean).join(" · ") : "Not visible to this login";
  const waited = s?.wait_ms ?? s?.elapsed_ms ?? null;
  return (
    <>
      <div
        className={`block-row ${isHead ? "head" : ""}`}
        role="treeitem"
        aria-level={depth + 1}
        aria-expanded={node.children.length ? true : undefined}
        tabIndex={0}
        style={{ ["--depth" as string]: depth }}
        onClick={() => onLocate(node.session)}
        onKeyDown={(e) => e.key === "Enter" && onLocate(node.session)}
        title="Show this session in the table"
      >
        <span className="block-indent" aria-hidden />
        <span className="block-session">{node.session}</span>
        <span className="block-state">
          {isHead ? (
            <span className="pill head">{s?.state === "sleeping" ? "idle in transaction" : (s?.state ?? "holding locks")}</span>
          ) : (
            <span className="pill waiting">waits {waited != null ? duration(waited) : ""}</span>
          )}
        </span>
        <span className="block-what">
          <span className="block-who">{who}</span>
          {node.waits_for && <span className="block-lock">wants {node.waits_for}</span>}
          {s?.sql && <span className="block-sql mono">{oneLine(s.sql)}</span>}
        </span>
      </div>
      {node.children.map((c) => (
        <TreeRow key={c.session} node={c} depth={depth + 1} sessions={sessions} onLocate={onLocate} />
      ))}
    </>
  );
}

const COPY: Record<SessionAction, { button: string; confirm: string; explain: string }> = {
  cancel: { button: "Cancel its statement", confirm: "Cancel statement", explain: "Stops the running statement; the session stays connected." },
  terminate: { button: "End session", confirm: "End session", explain: "Closes the session; its open transaction rolls back and the waiters continue." },
};

function HeadActions({ head, actions, onInterrupt }: { head: Session; actions: SessionAction[]; onInterrupt: (session: Session, action: SessionAction) => Promise<void> }) {
  const [state, setState] = useState<{ status: "idle" } | { status: "confirm"; action: SessionAction } | { status: "busy" } | { status: "error"; message: string }>({ status: "idle" });
  const offered = actions.filter((a) => a !== "cancel" || head.state === "active");
  const run = async (action: SessionAction) => {
    setState({ status: "busy" });
    try {
      await onInterrupt(head, action);
      setState({ status: "idle" });
    } catch (e) {
      setState({ status: "error", message: errorMessage(e).message });
    }
  };
  if (state.status === "busy") return <span className="spinner" aria-label="Working" />;
  if (state.status === "error")
    return (
      <span className="inline-confirm">
        <span className="error-text" title={state.message}>
          {state.message}
        </span>
        <button className="btn ghost small" onClick={() => setState({ status: "idle" })}>
          OK
        </button>
      </span>
    );
  if (state.status === "confirm")
    return (
      <span className="inline-confirm" onKeyDown={(e) => e.key === "Escape" && setState({ status: "idle" })}>
        <span className="muted">{COPY[state.action].explain}</span>
        <button className="btn danger small" autoFocus onClick={() => run(state.action)}>
          {COPY[state.action].confirm} {head.id}
        </button>
        <button className="btn ghost small" onClick={() => setState({ status: "idle" })}>
          Keep
        </button>
      </span>
    );
  return (
    <span className="block-actions">
      {offered.map((a) => (
        <button key={a} className="btn small" title={COPY[a].explain} onClick={() => setState({ status: "confirm", action: a })}>
          {COPY[a].button}
        </button>
      ))}
    </span>
  );
}
