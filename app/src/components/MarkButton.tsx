import { Flag } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { api, errorMessage } from "../lib/api";
import { eventsChanged } from "../lib/events";
import { toast } from "../lib/store";
import "./timeline.css";

function localInput(t: number): string {
  const d = new Date(t);
  const pad = (n: number) => String(n).padStart(2, "0");
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}T${pad(d.getHours())}:${pad(d.getMinutes())}`;
}

/**
 * "Mark…": put a deploy, a migration or a config change on the connection's timeline, now or at an earlier time. The
 * charts and What changed line queries up with it; CI and coding agents do the same with `culprit mark` and the
 * `mark` MCP tool.
 */
export function MarkButton({ connection, label = "Mark" }: { connection: string; label?: string }) {
  const [open, setOpen] = useState(false);
  const [text, setText] = useState("");
  const [at, setAt] = useState(() => localInput(Date.now()));
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const input = useRef<HTMLInputElement>(null);
  const box = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!open) return;
    setAt(localInput(Date.now()));
    setError(null);
    input.current?.focus();
    const away = (e: PointerEvent) => !box.current?.contains(e.target as Node) && setOpen(false);
    document.addEventListener("pointerdown", away);
    return () => document.removeEventListener("pointerdown", away);
  }, [open]);

  const save = async () => {
    if (!text.trim() || busy) return;
    const t = Date.parse(at);
    setBusy(true);
    try {
      const client = await api();
      // Within a minute of now means "now"; an earlier time is kept as entered.
      const when = Number.isFinite(t) && Date.now() - t > 60_000 ? new Date(t).toISOString() : null;
      await client.mark(connection, text.trim(), when);
      setText("");
      setOpen(false);
      eventsChanged();
      toast("Marked on the timeline");
    } catch (e) {
      setError(errorMessage(e).message);
    } finally {
      setBusy(false);
    }
  };

  return (
    <span className="mark-anchor" ref={box}>
      <button className="btn small" onClick={() => setOpen((o) => !o)} title="Put a deploy or a change on the timeline" aria-expanded={open}>
        <Flag size={12} /> {label}
      </button>
      {open && (
        <div
          className="mark-pop"
          role="dialog"
          aria-label="Mark the timeline"
          onKeyDown={(e) => {
            e.stopPropagation();
            if (e.key === "Escape") setOpen(false);
            if (e.key === "Enter") void save();
          }}
        >
          <h4>
            <Flag size={13} /> Mark the timeline
          </h4>
          <p>A deploy, a migration, a setting you changed. Charts and What changed line queries up with it.</p>
          <label>
            What happened
            <input ref={input} value={text} maxLength={200} placeholder="deploy v2.3.0" onChange={(e) => setText(e.target.value)} />
          </label>
          <label>
            When
            <input type="datetime-local" value={at} max={localInput(Date.now())} onChange={(e) => setAt(e.target.value)} />
          </label>
          {error && <div className="warn-text" style={{ fontSize: 11.5, marginBottom: 6 }}>{error}</div>}
          <div className="row">
            <button className="btn small ghost" onClick={() => setOpen(false)}>
              Cancel
            </button>
            <button className="btn small primary" onClick={() => void save()} disabled={!text.trim() || busy}>
              {busy ? "Marking…" : "Mark"}
            </button>
          </div>
        </div>
      )}
    </span>
  );
}
