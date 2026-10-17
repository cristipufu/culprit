import { useEffect, useMemo, useRef, useState, type KeyboardEvent, type PointerEvent, type ReactNode } from "react";
import type { EventKind } from "../gen/EventKind";
import { EventIcon, eventTone } from "./EventIcon";
import "./timeline.css";

/**
 * A time chart with context: light gridlines with values, local-time ticks, a peak marker, flags (plan changes, a
 * "since" anchor), a readout that follows the pointer, and an optional brush. Shapes are drawn in a stretched SVG; all
 * text and round marks are HTML so nothing distorts.
 */

export const VW = 1000;
export const VH = 100;

export type Bucket = {
  start: number;
  end: number;
  /** Share of the bucket the source covered (edge buckets). */
  covered?: number;
  /** The data starts (or stops) during this bucket, so its value isn't comparable with the rest. */
  partial?: boolean;
  /** The source can't account for this bucket in full (entries left it between two scans and took their totals with
   * them, or a counter was misread): its value is no measurement, drawn as such and never the chart's peak. Mirrors
   * culprit_core::model::Timeline::unknown. */
  unknown?: boolean;
};
export type Flag = { at: number; label: string; tone?: "plan" | "anchor"; color?: string; onClick?: () => void };
/** Something that changed around the database: a point in time, a window between two scans, or an episode. */
export type ChartEvent = {
  id: string;
  kind: EventKind;
  from: number;
  to: number;
  title: string;
  when: string;
  /** Happened at `to` exactly (the database dated it, or someone marked it); otherwise somewhere in `from`–`to`. */
  exact: boolean;
  onClick?: () => void;
};
export type Range = { from: number; to: number };

export function isPartial(b: Bucket): boolean {
  return b.partial === true || b.unknown === true || (b.covered != null && b.covered < 0.95);
}

/** "Nice" round values for gridlines: 1, 2 or 5 times a power of ten. */
export function niceTicks(max: number, count = 3): number[] {
  if (!(max > 0)) return [];
  const raw = max / count;
  const power = 10 ** Math.floor(Math.log10(raw));
  const step = [1, 2, 5, 10].map((m) => m * power).find((s) => s >= raw) ?? raw;
  const out: number[] = [];
  for (let v = step; v <= max * 1.0001; v += step) out.push(v);
  return out;
}

const MINUTE = 60_000;
const HOUR = 60 * MINUTE;
const STEPS = [5 * MINUTE, 10 * MINUTE, 15 * MINUTE, 30 * MINUTE, HOUR, 2 * HOUR, 3 * HOUR, 6 * HOUR, 12 * HOUR, 24 * HOUR, 2 * 24 * HOUR, 7 * 24 * HOUR];

/** 4–6 time ticks on round local times. */
export function timeTicks(from: number, to: number): number[] {
  const span = to - from;
  const step = STEPS.find((s) => span / s <= 6) ?? STEPS[STEPS.length - 1]!;
  const offset = new Date(from).getTimezoneOffset() * MINUTE;
  let t = Math.ceil((from - offset) / step) * step + offset;
  const out: number[] = [];
  for (; t <= to; t += step) if (t > from + span * 0.02 && t < to - span * 0.02) out.push(t);
  return out;
}

const clock = new Intl.DateTimeFormat(undefined, { hour: "numeric", minute: "2-digit" });
const dayClock = new Intl.DateTimeFormat(undefined, { weekday: "short", hour: "numeric", minute: "2-digit" });
const day = new Intl.DateTimeFormat(undefined, { month: "short", day: "numeric" });

export function tickLabel(t: number, spanMs: number): string {
  if (spanMs <= 36 * HOUR) return clock.format(t);
  return day.format(t);
}

/** "Tue 14:05" in local time. */
export function when(t: number): string {
  return dayClock.format(t);
}

export function rangeLabel(from: number, to: number): string {
  const sameDay = new Date(from).toDateString() === new Date(to).toDateString();
  return sameDay ? `${dayClock.format(from)}–${clock.format(to)}` : `${dayClock.format(from)} – ${dayClock.format(to)}`;
}

type Props = {
  buckets: Bucket[];
  /** Highest value on the y axis (before headroom). */
  max: number;
  format: (v: number) => string;
  height: number;
  /** The plot itself, drawn in VW×VH with `x` and `y`. */
  children: (scale: { x: (t: number) => number; y: (v: number) => number }) => ReactNode;
  /** What the readout says for bucket `i`. */
  readout: (i: number) => ReactNode;
  peak?: { index: number; value: number } | null;
  flags?: Flag[];
  /** Brushing: drag (or Shift + arrows) to select a range of buckets. */
  onSelect?: (range: Range) => void;
  selection?: Range | null;
  ariaLabel: string;
  footnote?: ReactNode;
  showTimeAxis?: boolean;
  /** Changes on a lane under the plot; cloud throttling also shades the plot, marks draw a line through it. */
  events?: ChartEvent[];
  /** Show the lane under the plot (small charts only shade their episodes). */
  lane?: boolean;
};

export function TimeChart({ buckets, max, format, height, children, readout, peak, flags = [], onSelect, selection, ariaLabel, footnote, showTimeAxis = true, events = [], lane = true }: Props) {
  const plot = useRef<HTMLDivElement>(null);
  const [hover, setHover] = useState<number | null>(null);
  // The drag lives in a ref so fast pointer sequences never read a stale render; state only redraws it.
  const dragRef = useRef<{ a: number; b: number } | null>(null);
  const [drag, setDragView] = useState<{ a: number; b: number } | null>(null);
  const setDrag = (next: { a: number; b: number } | null) => {
    dragRef.current = next;
    setDragView(next);
  };
  const from = buckets[0]?.start ?? 0;
  const to = buckets[buckets.length - 1]?.end ?? 1;
  const top = useMemo(() => {
    const ticks = niceTicks(max);
    return Math.max(max * 1.08, ticks[ticks.length - 1] ?? 0, 1e-9);
  }, [max]);
  const x = (t: number) => ((t - from) / (to - from)) * VW;
  const y = (v: number) => VH - (v / top) * VH;
  const pct = (t: number) => `${(x(t) / VW) * 100}%`;

  const indexAt = (clientX: number) => {
    const box = plot.current?.getBoundingClientRect();
    if (!box) return null;
    const at = from + ((clientX - box.left) / box.width) * (to - from);
    const i = buckets.findIndex((b) => at >= b.start && at < b.end);
    return i >= 0 ? i : at >= to ? buckets.length - 1 : 0;
  };

  const onPointerDown = (e: PointerEvent<HTMLDivElement>) => {
    if (!onSelect || e.button !== 0) return;
    const i = indexAt(e.clientX);
    if (i == null) return;
    e.currentTarget.setPointerCapture(e.pointerId);
    setDrag({ a: i, b: i });
  };
  const onPointerMove = (e: PointerEvent<HTMLDivElement>) => {
    const i = indexAt(e.clientX);
    setHover(i);
    const current = dragRef.current;
    if (current && i != null && i !== current.b) setDrag({ ...current, b: i });
  };
  const finish = (e?: PointerEvent<HTMLDivElement>) => {
    const current = dragRef.current;
    if (!current || !onSelect) return;
    const end = e ? indexAt(e.clientX) : null;
    const last = end ?? current.b;
    const [a, b] = [Math.min(current.a, last), Math.max(current.a, last)];
    setDrag(null);
    // A click (no drag) on a short chart still selects that bucket; on a long one it does nothing.
    if (b > a || buckets.length <= 4) onSelect({ from: buckets[a]!.start, to: buckets[b]!.end });
  };

  // Keyboard: arrows move the cursor, Shift extends a selection from where it started, Enter compares it.
  const [anchor, setAnchor] = useState<number | null>(null);
  const onKeyDown = (e: KeyboardEvent<HTMLDivElement>) => {
    const last = buckets.length - 1;
    const current = hover ?? last;
    if (e.key === "ArrowLeft" || e.key === "ArrowRight") {
      e.preventDefault();
      e.stopPropagation();
      const next = Math.min(last, Math.max(0, current + (e.key === "ArrowLeft" ? -1 : 1)));
      if (e.shiftKey && onSelect) {
        const start = anchor ?? current;
        setAnchor(start);
        setDrag({ a: start, b: next });
      } else {
        setAnchor(null);
        setDrag(null);
      }
      setHover(next);
    } else if (e.key === "Enter" && dragRef.current) {
      e.preventDefault();
      e.stopPropagation();
      finish();
      setAnchor(null);
    } else if (e.key === "Escape") {
      setDrag(null);
      setAnchor(null);
      setHover(null);
    }
  };
  useEffect(() => setHover(null), [buckets]);

  const ticks = niceTicks(max);
  // Time ticks step aside for flags, whose labels sit on the same line.
  const times = timeTicks(from, to).filter((t) => !flags.some((f) => Math.abs(f.at - t) < (to - from) * 0.07));
  const shownSelection: Range | null = drag
    ? { from: buckets[Math.min(drag.a, drag.b)]!.start, to: buckets[Math.max(drag.a, drag.b)]!.end }
    : (selection ?? null);
  const hoverBucket = hover != null ? buckets[hover] : undefined;
  const readoutLeft = hoverBucket ? (x((hoverBucket.start + hoverBucket.end) / 2) / VW) * 100 : 0;
  const peakBucket = peak ? buckets[peak.index] : undefined;
  const peakAt = peakBucket ? (peakBucket.start + peakBucket.end) / 2 : 0;
  const shown = events.filter((e) => e.to >= from && e.from <= to);
  const clamp = (t: number) => Math.min(to, Math.max(from, t));

  return (
    <div className={`tc ${lane && shown.length ? "has-lane" : ""}`}>
      <div
        ref={plot}
        className={`tc-plot ${onSelect ? "brushable" : ""}`}
        style={{ height }}
        tabIndex={0}
        role="img"
        aria-label={ariaLabel}
        onPointerDown={onPointerDown}
        onPointerMove={onPointerMove}
        onPointerUp={(e) => finish(e)}
        onPointerCancel={() => setDrag(null)}
        onPointerLeave={() => !dragRef.current && setHover(null)}
        onKeyDown={onKeyDown}
        onBlur={() => !dragRef.current && setHover(null)}
      >
        {ticks.map((v) => (
          <div key={v} className="tc-grid" style={{ top: `${(y(v) / VH) * 100}%` }}>
            <span>{format(v)}</span>
          </div>
        ))}
        {buckets.map((b, i) =>
          isPartial(b) ? (
            <div
              key={`p${i}`}
              className="tc-partial"
              style={{ left: pct(b.start), width: `calc(${pct(b.end)} - ${pct(b.start)})` }}
              title={
                b.unknown
                  ? "Not known in full: the source can't account for everything that ran in this period (entries left it between two scans and took their totals with them)"
                  : b.covered != null
                    ? `Partial: the data covers ${Math.round(b.covered * 100)}% of this period`
                    : "Partial: the data starts during this period"
              }
            />
          ) : null,
        )}
        <svg viewBox={`0 0 ${VW} ${VH}`} preserveAspectRatio="none" aria-hidden="true">
          {children({ x, y })}
        </svg>
        {shown
          .filter((e) => e.kind === "throttle")
          .map((e) => (
            <div key={`band-${e.id}`} className="tc-band" style={{ left: pct(clamp(e.from)), width: `max(2px, calc(${pct(clamp(e.to))} - ${pct(clamp(e.from))}))` }} />
          ))}
        {shown
          .filter((e) => e.kind === "mark")
          .map((e) => (
            <div key={`line-${e.id}`} className="tc-markline" style={{ left: pct(clamp(e.to)) }} />
          ))}
        {shownSelection && <div className="tc-selection" style={{ left: pct(shownSelection.from), width: `calc(${pct(shownSelection.to)} - ${pct(shownSelection.from)})` }} />}
        {flags.map((f, i) =>
          f.at > from && f.at < to ? (
            <button
              key={`${f.label}-${i}`}
              type="button"
              className={`tc-flag ${f.tone ?? "plan"}`}
              style={{ left: pct(f.at), ...(f.color ? { ["--flag" as string]: f.color } : {}) }}
              onPointerDown={(e) => e.stopPropagation()}
              onClick={f.onClick}
              title={f.onClick ? `${f.label}: compare before and after` : f.label}
            >
              <span>{f.label}</span>
            </button>
          ) : null,
        )}
        {peak && peakBucket && hover == null && !shownSelection && (
          <div
            // Near the top, the label goes under the point so it never covers the chart's header actions.
            className={`tc-peak ${x(peakAt) > VW * 0.7 ? "left" : ""} ${y(peak.value) / VH < 0.3 ? "below" : ""}`}
            style={{ left: pct(peakAt), top: `${(y(peak.value) / VH) * 100}%` }}
          >
            <i />
            <span>peak {format(peak.value)}</span>
          </div>
        )}
        {hoverBucket && (
          <>
            <div className="tc-cursor" style={{ left: pct(hoverBucket.start), width: `max(1px, calc(${pct(hoverBucket.end)} - ${pct(hoverBucket.start)}))` }} />
            <div className={`tc-readout ${readoutLeft > 62 ? "left" : ""}`} style={{ left: `${readoutLeft}%` }}>
              <div className="tc-when">{rangeLabel(hoverBucket.start, hoverBucket.end)}</div>
              {readout(hover!)}
            </div>
          </>
        )}
      </div>
      {lane && shown.length > 0 && <EventLane events={shown} from={from} to={to} />}
      {showTimeAxis && (
        <div className="tc-axis">
          {times.map((t) => (
            <span key={t} style={{ left: pct(t) }}>
              {tickLabel(t, to - from)}
            </span>
          ))}
        </div>
      )}
      {footnote && <div className="tc-foot">{footnote}</div>}
    </div>
  );
}

/** Events close enough to share one marker: within 2% of the chart's width. */
function cluster(events: ChartEvent[], from: number, to: number): { at: number; start: number; events: ChartEvent[] }[] {
  const span = to - from || 1;
  const sorted = [...events].sort((a, b) => a.to - b.to);
  const out: { at: number; start: number; events: ChartEvent[] }[] = [];
  for (const e of sorted) {
    const at = Math.min(to, Math.max(from, e.kind === "throttle" ? e.from : e.to));
    const start = Math.min(to, Math.max(from, e.from));
    const last = out[out.length - 1];
    if (last && (at - last.at) / span < 0.02) {
      last.events.push(e);
      last.start = Math.min(last.start, start);
    } else out.push({ at, start, events: [e] });
  }
  return out;
}

/** The change lane under a chart: an icon where each change happened, a bracket where only a window between scans
 * is known, and a tooltip listing what changed. Click compares before and after. */
function EventLane({ events, from, to }: { events: ChartEvent[]; from: number; to: number }) {
  const [open, setOpen] = useState<number | null>(null);
  const groups = useMemo(() => cluster(events, from, to), [events, from, to]);
  const pct = (t: number) => ((t - from) / (to - from || 1)) * 100;
  return (
    <div className="tc-lane" onPointerLeave={() => setOpen(null)}>
      {groups.map((g, i) => {
        const first = g.events[0]!;
        const tone = g.events.some((e) => e.kind === "throttle") ? "throttle" : g.events.some((e) => e.kind === "mark") ? "mark" : eventTone(first.kind);
        const spanned = g.events.every((e) => !e.exact) && pct(g.at) - pct(g.start) > 1.5;
        const label = g.events.length === 1 ? `${first.title}, ${first.when}` : `${g.events.length} changes: ${g.events.map((e) => e.title).join("; ")}`;
        return (
          <div key={`${g.at}-${i}`} className={`tc-ev-group ${tone}`}>
            {spanned && <div className="tc-ev-span" style={{ left: `${pct(g.start)}%`, width: `${pct(g.at) - pct(g.start)}%` }} />}
            <button
              type="button"
              className={`tc-ev ${tone}`}
              style={{ left: `${pct(g.at)}%` }}
              aria-label={label}
              onPointerEnter={() => setOpen(i)}
              onFocus={() => setOpen(i)}
              onBlur={() => setOpen(null)}
              onClick={first.onClick}
            >
              <EventIcon kind={tone === "throttle" ? "throttle" : tone === "mark" ? "mark" : first.kind} size={10} />
              {g.events.length > 1 && <b>{g.events.length}</b>}
            </button>
            {open === i && (
              <div className={`tc-ev-tip ${pct(g.at) > 62 ? "left" : ""}`} style={{ left: `${pct(g.at)}%` }} role="tooltip">
                {g.events.slice(0, 5).map((e) => (
                  <div key={e.id} className="tc-ev-item">
                    <span className={`tc-ev-dot ${eventTone(e.kind)}`}>
                      <EventIcon kind={e.kind} size={11} />
                    </span>
                    <span>
                      <b>{e.title}</b>
                      <span className="muted">{e.when}</span>
                    </span>
                  </div>
                ))}
                {g.events.length > 5 && <div className="muted tc-ev-more">+{g.events.length - 5} more</div>}
                {first.onClick && <div className="tc-ev-hint">Click to compare before and after</div>}
              </div>
            )}
          </div>
        );
      })}
    </div>
  );
}
