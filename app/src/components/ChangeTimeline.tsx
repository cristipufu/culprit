import { Clock, X } from "lucide-react";
import { useMemo, useState } from "react";
import type { ChangeEvent } from "../gen/ChangeEvent";
import type { Changes } from "../gen/Changes";
import { api, errorMessage } from "../lib/api";
import { eventSpan, eventTime, eventsChanged, KIND_LABEL } from "../lib/events";
import { toast } from "../lib/store";
import { EventIcon, eventTone } from "./EventIcon";
import type { ChartEvent } from "./TimeChart";
import "./timeline.css";

/** Events as chart markers; `onCompare` opens What changed from the moment each one happened. */
export function chartEvents(events: ChangeEvent[], onCompare?: (range: { from: number; to?: number }) => void): ChartEvent[] {
  return events
    .filter((e) => !e.minor)
    .map((e) => {
      const { from, to } = eventSpan(e);
      return {
        id: e.id,
        kind: e.kind,
        from,
        to,
        title: e.title,
        when: eventTime(e),
        exact: !e.after,
        // An episode compares itself with the time before it; a change compares everything since it happened (for
        // changes noticed between scans, since the earlier scan, so the change is inside the recent period).
        onClick: onCompare ? () => onCompare(e.until ? { from, to } : { from }) : undefined,
      };
    });
}

/** Where a source of the time came from, in a few words. */
function sourceWords(e: ChangeEvent): string {
  switch (e.source) {
    case "mark":
      return e.by ? `marked by ${e.by === "app" ? "you" : e.by}` : "marked";
    case "database":
      return "dated by the database";
    case "scans":
      return "noticed between scans";
    default:
      return e.object ? `from the cloud · ${e.object}` : "from the cloud";
  }
}

/**
 * "Around then": everything that changed across the two periods, oldest first, each with how exactly its time is
 * known and how many of the queries below it may explain. Marks can be removed here.
 */
export function AroundThen({ changes, onFocusQuery }: { changes: Changes; onFocusQuery: (id: string) => void }) {
  const [all, setAll] = useState(false);
  const linked = useMemo(() => {
    const map = new Map<string, string[]>();
    for (const q of changes.queries) for (const l of q.context) map.set(l.event, [...(map.get(l.event) ?? []), q.query_id]);
    return map;
  }, [changes]);
  const events = changes.events;
  if (!events.length) return null;
  const limit = 6;
  const shown = all ? events : events.slice(0, limit);
  const remove = async (e: ChangeEvent) => {
    const id = e.id.split(":")[1];
    if (!id) return;
    try {
      await (await api()).unmark(changes.connection, id);
      eventsChanged();
      toast("Mark removed");
    } catch (err) {
      toast(errorMessage(err).message);
    }
  };
  return (
    <section className="around" aria-label="What changed around these periods">
      <h3>
        <Clock size={13} /> Around then <span className="muted">{events.length}</span>
      </h3>
      <ol className="around-list">
        {shown.map((e) => {
          const queries = linked.get(e.id) ?? [];
          return (
            <li key={e.id} className={`around-item ${queries.length || e.kind === "mark" || e.kind === "throttle" ? "" : "dim"}`}>
              <span className={`tc-ev-dot ${eventTone(e.kind)}`} title={KIND_LABEL[e.kind]}>
                <EventIcon kind={e.kind} size={11} />
              </span>
              <span className="around-text">
                <b>{e.title}</b>
                <span className="around-meta">
                  {eventTime(e)} · {sourceWords(e)}
                </span>
                {e.detail && <span className="around-detail">{e.detail}</span>}
              </span>
              <span className="around-side">
                {queries.length > 0 && (
                  <button className="around-links" onClick={() => onFocusQuery(queries[0]!)} title="Show the query it may explain">
                    may explain {queries.length} {queries.length === 1 ? "query" : "queries"}
                  </button>
                )}
                {e.kind === "mark" && e.source === "mark" && (
                  <button className="icon-btn" title="Remove this mark" aria-label={`Remove the mark ${e.title}`} onClick={() => void remove(e)}>
                    <X size={12} />
                  </button>
                )}
              </span>
            </li>
          );
        })}
      </ol>
      {events.length > limit && (
        <button className="link-button around-toggle" onClick={() => setAll((v) => !v)}>
          {all ? "Show fewer" : `Show all ${events.length}`}
        </button>
      )}
    </section>
  );
}

const day = new Intl.DateTimeFormat(undefined, { weekday: "short", hour: "numeric", minute: "2-digit" });

/** The two periods on one line, with a marker wherever something changed. */
export function PeriodStrip({ changes, onFocusEvent }: { changes: Changes; onFocusEvent?: (id: string) => void }) {
  const b0 = Date.parse(changes.baseline.start);
  const r0 = Date.parse(changes.recent.start);
  const r1 = Date.parse(changes.recent.end);
  const b1 = Date.parse(changes.baseline.end);
  const from = Math.min(b0, r0);
  const to = Math.max(r1, b1);
  const pct = (t: number) => `${((Math.min(to, Math.max(from, t)) - from) / (to - from || 1)) * 100}%`;
  const marks = changes.events.filter((e) => !e.minor || changes.queries.some((q) => q.context.some((l) => l.event === e.id)));
  return (
    <div className="pstrip" aria-hidden={marks.length === 0}>
      <div className="pstrip-track" />
      <div className="pstrip-seg baseline" style={{ left: pct(b0), width: `calc(${pct(b1)} - ${pct(b0)})` }} />
      <div className="pstrip-seg recent" style={{ left: pct(r0), width: `calc(${pct(r1)} - ${pct(r0)})` }} />
      {marks.map((e) => {
        const { from: a, to: b } = eventSpan(e);
        const tone = eventTone(e.kind);
        return (
          <button
            key={e.id}
            type="button"
            className={`tc-ev ${tone}`}
            style={{ left: pct(e.until ? a : b) }}
            title={`${e.title} · ${eventTime(e)}`}
            aria-label={`${e.title}, ${eventTime(e)}`}
            onClick={() => onFocusEvent?.(e.id)}
          >
            <EventIcon kind={e.kind} size={10} />
          </button>
        );
      })}
      <div className="pstrip-labels">
        <span style={{ left: 0 }}>{day.format(from)}</span>
        <span style={{ left: pct(r0), transform: "translateX(-50%)" }}>{day.format(r0)}</span>
        <span style={{ right: 0 }}>{day.format(to)}</span>
      </div>
    </div>
  );
}
