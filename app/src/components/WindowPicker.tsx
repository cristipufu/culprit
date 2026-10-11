import { CalendarClock, Check, ChevronDown } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { windowLabel } from "../lib/format";
import { useHotkeys } from "../lib/hotkeys";
import { setWindow, useStore, WINDOW_OPTIONS, type WindowDays } from "../lib/store";

const LABEL: Record<WindowDays, string> = { 1: "Last 24 hours", 7: "Last 7 days", 30: "Last 30 days" };
const SHORT: Record<WindowDays, string> = { 1: "24h", 7: "7d", 30: "30d" };

/** Workload window for history-keeping sources (Query Store). `coveredHours` is what the source actually had. */
export function WindowPicker({ coveredHours }: { coveredHours: number | null }) {
  const days = useStore((s) => s.windowDays);
  const [open, setOpen] = useState(false);
  const anchor = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!open) return;
    const close = (e: MouseEvent) => {
      if (!anchor.current?.contains(e.target as Node)) setOpen(false);
    };
    window.addEventListener("mousedown", close);
    return () => window.removeEventListener("mousedown", close);
  }, [open]);

  const choose = (d: WindowDays) => {
    setOpen(false);
    setWindow(d);
  };
  const cycle = () => {
    const i = WINDOW_OPTIONS.indexOf(days);
    choose(WINDOW_OPTIONS[(i + 1) % WINDOW_OPTIONS.length]!);
  };
  useHotkeys({ w: cycle, Escape: () => setOpen(false) });

  const short = coveredHours != null && coveredHours < days * 24 * 0.9;
  const held = coveredHours != null ? windowLabel(coveredHours).replace("last ", "") : "";
  // A window longer than the history shows nothing a shorter one doesn't, once the shortest such window covers it all.
  const same = (d: WindowDays) => coveredHours != null && WINDOW_OPTIONS.some((o) => o < d && coveredHours < o * 24 * 0.9);

  return (
    <div className="menu-anchor" ref={anchor}>
      <button className="picker" aria-haspopup="listbox" aria-expanded={open} title="Workload window (W)" onClick={() => setOpen((o) => !o)}>
        <CalendarClock size={13} />
        {LABEL[days]}
        <ChevronDown size={13} className="muted" />
      </button>
      {short && (
        <span className="picker-note" title={`Query Store has kept history for ${held}; a longer window shows the same data until it has more.`}>
          Query Store keeps {held}
        </span>
      )}
      {open && (
        <div className="menu" role="listbox" aria-label="Workload window">
          {WINDOW_OPTIONS.map((d) => (
            <button key={d} role="option" aria-selected={d === days} className={`menu-item ${d === days ? "on" : ""} ${same(d) ? "same" : ""}`} onClick={() => choose(d)}>
              <span>
                {LABEL[d]}
                {same(d) && <span className="menu-sub">same data: {held} kept</span>}
              </span>
              <span className="menu-key">{d === days ? <Check size={13} /> : SHORT[d]}</span>
            </button>
          ))}
          <div className="menu-hint">From Query Store history. Short windows show what is slow now; long ones catch weekly and monthly jobs. Press W to cycle.</div>
        </div>
      )}
    </div>
  );
}
