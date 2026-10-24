import { ChevronRight } from "lucide-react";
import { useMemo, type KeyboardEvent } from "react";
import type { Report } from "../gen/Report";
import type { Signal } from "../gen/Signal";
import { percent } from "../lib/format";
import { foldActivity, foldWaits, signalValue, waitColor } from "../lib/waits";
import { can, serverName } from "../lib/engines";

/** Which part of the waits panel to open. */
export type WaitsTab = "waits" | "now" | "health";
export type OpenWaits = (tab: WaitsTab, category?: string) => void;

/** Short names for the legend under the bar, where space is tight. */
const SHORT: Record<string, string> = {
  cpu: "CPU",
  io: "Disk",
  rows: "Rows",
  lock: "Locks",
  latch: "Contention",
  log: "Log",
  network: "Network",
  parallelism: "Parallel",
  memory: "Memory",
  compilation: "Compiles",
  other: "Other",
};


function activate(e: KeyboardEvent, run: () => void) {
  if (e.key === "Enter" || e.key === " ") {
    e.preventDefault();
    run();
  }
}

/**
 * The Overview's "Where time goes" card: the biggest category, a bar you can click by segment, and a legend. It keeps its
 * place on every engine; when the scan recorded no waits it offers a live sample instead.
 */
export function WaitsVital({ report, onOpen }: { report: Report; onOpen: OpenWaits }) {
  const activity = report.snapshot.operations.activity_history;
  const sampled = useMemo(() => activity ? foldActivity(activity) : [], [activity]);
  if (activity) {
    const categories = sampled;
    const top = categories[0];
    return <div className="vital waits-vital" role="button" tabIndex={0} onClick={() => onOpen("waits", top?.key)} onKeyDown={(e) => activate(e, () => onOpen("waits", top?.key))} title="Retained active-task sample weights, independent of statement time">
      <div className="label">Recorded activity</div><div className="value waits-value">{top?.label ?? "No retained samples"}</div>
      <div className="waits-bar" aria-hidden>{categories.map((c) => <span key={c.key} style={{ flexGrow: c.weight, background: waitColor(c.key) }} />)}</div>
      <div className="hint">{top ? `${percent(top.share)} of retained sample weight` : "Sampling may miss short activity"}{activity.period.partial && " · partial read"}</div>
    </div>;
  }
  const waits = foldWaits(report.snapshot.waits);
  if (waits.length === 0 && !can(report.snapshot.server.engine, "wait_sample")) {
    return (
      <div className="vital waits-vital" title={`${serverName(report.snapshot.server.engine)} has no server, so nothing records what it waits on`}>
        <div className="label">Where time goes</div>
        <div className="value muted">Not recorded</div>
        <div className="hint">No server to ask</div>
      </div>
    );
  }
  if (waits.length === 0) {
    return (
      <div className="vital waits-vital" role="button" tabIndex={0} onClick={() => onOpen("now")} onKeyDown={(e) => activate(e, () => onOpen("now"))} title="Sample what the server waits on right now">
        <div className="label">Where time goes</div>
        <div className="value muted">Not tracked</div>
        <div className="hint waits-cta">
          Sample it now <ChevronRight size={11} />
        </div>
      </div>
    );
  }
  const top = waits[0]!;
  return (
    <div className="vital waits-vital" role="button" tabIndex={0} onClick={() => onOpen("waits", top.key)} onKeyDown={(e) => activate(e, () => onOpen("waits", top.key))} title="See where the time goes, and the queries behind each part">
      <div className="label">Where time goes</div>
      <div className="value waits-value" title={`${top.label}: ${percent(top.share)} of recorded time`}>
        <span className="full">{top.label}</span>
        <span className="short">{SHORT[top.key] ?? top.label}</span>
      </div>
      <div className="waits-bar" aria-hidden>
        {waits.map((w) => (
          <span
            key={w.key}
            title={`${w.label} · ${percent(w.share)}`}
            style={{ flexGrow: w.ms, background: waitColor(w.key) }}
            onClick={(e) => {
              e.stopPropagation();
              onOpen("waits", w.key);
            }}
          />
        ))}
      </div>
      <div className="waits-legend">
        {waits.slice(0, 3).map((w) => (
          <span key={w.key}>
            <i style={{ background: waitColor(w.key) }} />
            {SHORT[w.key] ?? w.label} {percent(w.share)}
          </span>
        ))}
      </div>
    </div>
  );
}

const RANK: Record<Signal["status"], number> = { bad: 0, watch: 1, info: 2, good: 3 };

/** One calm line under the vitals: the signals that need a look, or a note that all is well. */
export function HealthLine({ report, onOpen }: { report: Report; onOpen: OpenWaits }) {
  const signals = report.snapshot.pressure?.signals ?? [];
  if (signals.length === 0) return null;
  const notable = signals.filter((s) => s.status === "bad" || s.status === "watch").sort((a, b) => RANK[a.status] - RANK[b.status]);
  const graded = signals.filter((s) => s.status !== "info").length;
  return (
    <div className="health-line">
      <span className="health-line-label">Health</span>
      {notable.length === 0 ? (
        <span className="health-pill">
          <i className={`sig-dot ${graded > 0 ? "good" : "info"}`} /> {graded > 0 ? `${graded} measured signals look healthy` : "Counters recorded; no health verdict"}
        </span>
      ) : (
        notable.slice(0, 1).map((s) => (
          <button key={s.key} className="health-pill" onClick={() => onOpen("health")} title={`${s.label}: ${signalValue(s)}${s.healthy ? ` (${s.healthy})` : ""}`}>
            <i className={`sig-dot ${s.status}`} />
            <span className="health-pill-label">{s.label}</span> <b>{signalValue(s)}</b>
          </button>
        ))
      )}
      <span className="spacer" />
      <button className="health-more" onClick={() => onOpen("health")}>
        {notable.length > 1 ? `${notable.length - 1} more · ` : ""}All signals <ChevronRight size={12} />
      </button>
    </div>
  );
}
