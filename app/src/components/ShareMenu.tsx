import { ChevronDown, Share2, ShieldCheck } from "lucide-react";
import { useEffect, useId, useRef, useState, type ReactNode } from "react";
import { rememberRedact, rememberedRedact } from "../lib/share";

export type ShareAction = {
  id: string;
  icon: ReactNode;
  label: string;
  hint: string;
  /** Why it can't run here, shown instead of running it. */
  unavailable?: string;
  /** Whether redaction applies (the menu's switch is shown when any action uses it). */
  redacts?: boolean;
  run: (redact: boolean) => Promise<unknown> | void;
};

/**
 * "Share" button with a small menu: what to export and how. Redaction of literal values is a visible switch, on by
 * default and remembered. Arrow keys move, Enter runs, Escape closes.
 */
export function ShareMenu({ actions, label = "Share", primary = false }: { actions: ShareAction[]; label?: string; primary?: boolean }) {
  const [open, setOpen] = useState(false);
  const [redact, setRedact] = useState(rememberedRedact);
  const [busy, setBusy] = useState<string | null>(null);
  const root = useRef<HTMLDivElement>(null);
  const items = useRef<(HTMLButtonElement | null)[]>([]);
  const menuId = useId();
  const usesRedaction = actions.some((a) => a.redacts);

  useEffect(() => {
    if (!open) return;
    const onDown = (e: MouseEvent) => {
      if (!root.current?.contains(e.target as Node)) setOpen(false);
    };
    window.addEventListener("mousedown", onDown);
    requestAnimationFrame(() => items.current.find((b) => b && !b.disabled)?.focus());
    return () => window.removeEventListener("mousedown", onDown);
  }, [open]);

  const run = async (action: ShareAction) => {
    if (action.unavailable) return;
    setBusy(action.id);
    try {
      await action.run(redact);
    } finally {
      setBusy(null);
      setOpen(false);
    }
  };

  const onKey = (e: React.KeyboardEvent) => {
    const list = items.current.filter((b): b is HTMLButtonElement => !!b);
    const at = list.indexOf(document.activeElement as HTMLButtonElement);
    if (e.key === "Escape") {
      e.stopPropagation();
      setOpen(false);
    } else if (e.key === "ArrowDown" || e.key === "ArrowUp") {
      e.preventDefault();
      const next = (at + (e.key === "ArrowDown" ? 1 : -1) + list.length) % list.length;
      list[next]?.focus();
    }
  };

  return (
    <div className="share" ref={root} onKeyDown={onKey}>
      <button
        className={`btn ${primary ? "" : "ghost"}`}
        aria-haspopup="menu"
        aria-expanded={open}
        aria-controls={menuId}
        onClick={() => setOpen((o) => !o)}
        title="Copy for a pull request, save an HTML page or the original plan"
        aria-label={label}
      >
        <Share2 size={13} /> <span className="btn-label">{label}</span> <ChevronDown size={12} className="share-caret" />
      </button>
      {open && (
        <div className="share-menu" role="menu" id={menuId}>
          {usesRedaction && (
            <label className="share-redact">
              <ShieldCheck size={14} />
              <span>
                <b>Redact values</b>
                <span className="muted">Strings and numbers in SQL become ? · names stay</span>
              </span>
              <input
                type="checkbox"
                role="switch"
                checked={redact}
                onChange={(e) => {
                  setRedact(e.target.checked);
                  rememberRedact(e.target.checked);
                }}
              />
            </label>
          )}
          {actions.map((a, i) => (
            <button
              key={a.id}
              role="menuitem"
              ref={(el) => {
                items.current[i] = el;
              }}
              className="share-item"
              disabled={!!a.unavailable || busy !== null}
              title={a.unavailable}
              onClick={() => run(a)}
            >
              <span className="share-icon">{busy === a.id ? <span className="spinner" style={{ width: 12, height: 12 }} /> : a.icon}</span>
              <span className="share-text">
                <span className="share-label">{a.label}</span>
                <span className="share-hint">{a.unavailable ?? a.hint}</span>
              </span>
            </button>
          ))}
          <div className="share-foot">Exports never include connection names, hosts or passwords.</div>
        </div>
      )}
    </div>
  );
}
