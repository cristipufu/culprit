import { CalendarClock, Check, ChevronDown } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { windowLabel } from "../lib/format";
import { useHotkeys } from "../lib/hotkeys";
import { setWindow, useStore, WINDOW_OPTIONS, type WindowMinutes } from "../lib/store";

const LABEL: Record<WindowMinutes, string> = {
  60: "Last hour",
  360: "Last 6 hours",
  1440: "Last 24 hours",
  10080: "Last 7 days",
  43200: "Last 30 days",
};
const SHORT: Record<WindowMinutes, string> = { 60: "1h", 360: "6h", 1440: "24h", 10080: "7d", 43200: "30d" };

/** Workload window for history-keeping sources (Query Store, the slow query log). `coveredHours` is what the source
 *  actually had; `source` names it. */
export function WindowPicker({ coveredHours, source }: { coveredHours: number | null; source: string | null }) {
  const from = source ?? "Query Store";
  const minutes = useStore((s) => s.windowMinutes);
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

  const choose = (d: WindowMinutes) => {
    setOpen(false);
    setWindow(d);
  };
  const cycle = () => {
    const i = WINDOW_OPTIONS.indexOf(minutes);
    choose(WINDOW_OPTIONS[(i + 1) % WINDOW_OPTIONS.length]!);
  };
  useHotkeys({ w: cycle, Escape: () => setOpen(false) });

  const short = coveredHours != null && coveredHours < (minutes / 60) * 0.9;
  const held = coveredHours != null ? windowLabel(coveredHours).replace("last ", "") : "";
  // A window longer than the history shows nothing a shorter one doesn't, once the shortest such window covers it all.
  const same = (d: WindowMinutes) => coveredHours != null && WINDOW_OPTIONS.some((o) => o < d && coveredHours < (o / 60) * 0.9);

  return (
    <div className="menu-anchor" ref={anchor}>
      <button className="picker" aria-haspopup="listbox" aria-expanded={open} title="Workload window (W)" onClick={() => setOpen((o) => !o)}>
        <CalendarClock size={13} />
        {LABEL[minutes]}
        <ChevronDown size={13} className="muted" />
      </button>
      {short && (
        <span className="picker-note" title={`${from} has kept history for ${held}; a longer window shows the same data until it has more.`}>
          {from} keeps {held}
        </span>
      )}
      {open && (
        <div className="menu" role="listbox" aria-label="Workload window">
          {WINDOW_OPTIONS.map((d) => (
            <button key={d} role="option" aria-selected={d === minutes} className={`menu-item ${d === minutes ? "on" : ""} ${same(d) ? "same" : ""}`} onClick={() => choose(d)}>
              <span>
                {LABEL[d]}
                {same(d) && <span className="menu-sub">same data: {held} kept</span>}
              </span>
              <span className="menu-key">{d === minutes ? <Check size={13} /> : SHORT[d]}</span>
            </button>
          ))}
          <div className="menu-hint">From {from} history. Short windows show what is slow now; long ones catch weekly and monthly jobs. Press W to cycle.</div>
        </div>
      )}
    </div>
  );
}
