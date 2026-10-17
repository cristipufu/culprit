import {
  ArrowLeftRight,
  ArrowRightLeft,
  CircleCheck,
  ClipboardCopy,
  EqualNot,
  FileCode,
  FileDiff,
  Flag,
  Gauge,
  Info,
  KeyRound,
  Link2,
  MemoryStick,
  Ruler,
  Split,
  Unlink2,
  Variable,
} from "lucide-react";
import { useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import { PlanDiagram, type Mark } from "../components/PlanDiagram";
import { PlanView } from "../components/PlanView";
import { ShareMenu } from "../components/ShareMenu";
import type { ChangeKind } from "../gen/ChangeKind";
import type { Plan } from "../gen/Plan";
import type { PlanDiff } from "../gen/PlanDiff";
import type { PlanNode } from "../gen/PlanNode";
import type { ScenarioKind } from "../gen/ScenarioKind";
import { api, errorMessage, type ApiError } from "../lib/api";
import { count, duration, percent } from "../lib/format";
import { stepShare } from "../lib/plan-format";
import { useHotkeys } from "../lib/hotkeys";
import { SAMPLES, setSide, swapSides, trySample, usePlans, type LoadedPlan } from "../lib/plans";
import { deliver, exportOptions } from "../lib/share";
import { can, planEngineName } from "../lib/engines";

const KIND: Record<Plan["kind"], string> = { estimated: "estimated", generic: "generic", actual: "actual" };
const LOOK_KEY = "culprit.plan.look";
const SYNC_KEY = "culprit.compare.sync";

type Look = "diagram" | "tree";
type Tab = "changes" | "why" | "props";

const CHANGE: Record<ChangeKind, { label: string; tone: string }> = {
  access: { label: "Access", tone: "warn" },
  operator: { label: "Operator", tone: "warn" },
  added: { label: "Added", tone: "good" },
  removed: { label: "Removed", tone: "bad" },
  cost: { label: "Cost", tone: "info" },
  rows: { label: "Rows", tone: "info" },
  warning: { label: "Warning", tone: "warn" },
};

const SCENARIO: Record<ScenarioKind, { label: string; icon: ReactNode }> = {
  statement: { label: "Statement", icon: <FileDiff size={15} /> },
  "plan-kind": { label: "Plan kind", icon: <Flag size={15} /> },
  estimator: { label: "Estimator", icon: <Gauge size={15} /> },
  parameters: { label: "Parameters", icon: <Variable size={15} /> },
  estimates: { label: "Estimates", icon: <Ruler size={15} /> },
  index: { label: "Indexes", icon: <KeyRound size={15} /> },
  conversion: { label: "Types", icon: <ArrowRightLeft size={15} /> },
  parallelism: { label: "Parallelism", icon: <Split size={15} /> },
  memory: { label: "Memory", icon: <MemoryStick size={15} /> },
};

function remembered<T extends string>(key: string, fallback: T, allowed: readonly T[]): T {
  try {
    const v = localStorage.getItem(key) as T | null;
    return v && allowed.includes(v) ? v : fallback;
  } catch {
    return fallback;
  }
}

function remember(key: string, value: string): void {
  try {
    localStorage.setItem(key, value);
  } catch {}
}

function cost(value: number): string {
  if (value < 10) return value.toFixed(3);
  if (value < 100) return value.toFixed(1);
  if (value >= 1e6) return `${(value / 1e6).toFixed(1)}M`;
  return value >= 1e4 ? `${Math.round(value / 1e3)}K` : value.toFixed(0);
}

function nodesOf(plan: Plan): PlanNode[] {
  const out: PlanNode[] = [];
  const walk = (n: PlanNode) => {
    out.push(n);
    n.children.forEach(walk);
  };
  walk(plan.root);
  return out;
}

/** "3.4× slower" from actual times when both plans ran, else "3.4× costlier" from estimated cost. */
function verdict(diff: PlanDiff): { tone: "worse" | "better" | "same"; text: string } {
  const timed = diff.time_a_ms != null && diff.time_b_ms != null && diff.time_a_ms > 0 && diff.time_b_ms > 0;
  const r = timed ? diff.time_b_ms! / diff.time_a_ms! : diff.cost_ratio;
  if (r == null) return { tone: "same", text: diff.comparable ? "no costs to compare" : "different engines" };
  const x = (v: number) => (v >= 1000 ? `${(v / 1000).toFixed(1).replace(/\.0$/, "")}K×` : v >= 10 ? `${Math.round(v)}×` : `${v.toFixed(1)}×`);
  if (r >= 1.25) return { tone: "worse", text: `${x(r)} ${timed ? "slower" : "costlier"}` };
  if (r <= 0.8) return { tone: "better", text: `${x(1 / r)} ${timed ? "faster" : "cheaper"}` };
  return { tone: "same", text: "about the same" };
}

function contextOf(a: LoadedPlan, b: LoadedPlan): string | null {
  return a.source?.connection ?? b.source?.connection ?? null;
}

export function PlanCompare() {
  const items = usePlans((s) => s.items);
  const aId = usePlans((s) => s.a);
  const bId = usePlans((s) => s.b);
  const a = items.find((p) => p.id === aId) ?? null;
  const b = items.find((p) => p.id === bId) ?? null;
  const [diff, setDiff] = useState<{ status: "idle" | "loading" } | { status: "ready"; diff: PlanDiff } | { status: "error"; error: ApiError }>({ status: "idle" });
  const [pickA, setPickA] = useState<number | null>(null);
  const [pickB, setPickB] = useState<number | null>(null);
  const [active, setActive] = useState<number | null>(null);
  const [tab, setTab] = useState<Tab>("changes");
  const [look, setLookState] = useState<Look>(() => remembered(LOOK_KEY, "diagram", ["diagram", "tree"] as const));
  const [sync, setSyncState] = useState(() => remembered(SYNC_KEY, "on", ["on", "off"] as const) === "on");
  const [zoom, setZoom] = useState(0.8);
  const sideA = useRef<HTMLDivElement>(null);
  const sideB = useRef<HTMLDivElement>(null);

  const names = useMemo((): [string, string] => (!a || !b || a.name === b.name ? ["plan A", "plan B"] : [a.name, b.name]), [a, b]);

  useEffect(() => {
    setActive(null);
    setPickA(null);
    setPickB(null);
    if (!a || !b) return setDiff({ status: "idle" });
    let cancelled = false;
    setDiff({ status: "loading" });
    api()
      .then((client) => client.diffPlans(a.plan, b.plan, names[0], names[1], contextOf(a, b)))
      .then((d) => !cancelled && setDiff({ status: "ready", diff: d }))
      .catch((e) => !cancelled && setDiff({ status: "error", error: errorMessage(e) }));
    return () => {
      cancelled = true;
    };
  }, [a, b, names]);

  const ready = diff.status === "ready" ? diff.diff : null;
  const view = useMemo(() => {
    const marksA = new Map<number, Mark>();
    const marksB = new Map<number, Mark>();
    const tintsA = new Map<number, number>();
    const tintsB = new Map<number, number>();
    const aToB = new Map<number, number>();
    const bToA = new Map<number, number>();
    if (ready) {
      ready.matches.forEach((m) => {
        aToB.set(m.a, m.b);
        bToA.set(m.b, m.a);
      });
      ready.regions.forEach((r, i) => {
        r.a.forEach((id) => tintsA.set(id, i % 6));
        r.b.forEach((id) => tintsB.set(id, i % 6));
      });
      ready.changes.forEach((c) => {
        if (c.kind === "access" || c.kind === "operator") {
          if (c.a != null) marksA.set(c.a, "changed");
          if (c.b != null) marksB.set(c.b, "changed");
        }
      });
      ready.only_a.forEach((id) => marksA.set(id, "removed"));
      ready.only_b.forEach((id) => marksB.set(id, "added"));
    }
    return { marksA, marksB, tintsA, tintsB, aToB, bToA };
  }, [ready]);

  // The picked operators, scrolled to the middle of their diagrams (SSMS centers the other plan on its match).
  useEffect(() => {
    const reduce = window.matchMedia?.("(prefers-reduced-motion: reduce)").matches;
    const show = (side: HTMLDivElement | null, id: number | null) => {
      if (id == null || !side) return;
      requestAnimationFrame(() =>
        side.querySelector(`[data-node="${id}"]`)?.scrollIntoView({ block: "nearest", inline: "center", behavior: reduce ? "auto" : "smooth" }),
      );
    };
    show(sideA.current, pickA);
    show(sideB.current, pickB);
  }, [pickA, pickB]);

  // Keep the two diagrams' scroll positions in step while syncing.
  useEffect(() => {
    if (!sync || look !== "diagram") return;
    const boxes = [sideA.current, sideB.current].map((s) => s?.querySelector<HTMLElement>(".diagram-scroll") ?? null);
    if (!boxes[0] || !boxes[1]) return;
    let mirroring = false;
    const handlers = boxes.map((from, i) => () => {
      const to = boxes[1 - i];
      if (!from || !to || mirroring) return;
      mirroring = true;
      const fx = from.scrollWidth - from.clientWidth;
      const fy = from.scrollHeight - from.clientHeight;
      to.scrollLeft = fx > 0 ? (from.scrollLeft / fx) * (to.scrollWidth - to.clientWidth) : 0;
      to.scrollTop = fy > 0 ? (from.scrollTop / fy) * (to.scrollHeight - to.clientHeight) : 0;
      requestAnimationFrame(() => (mirroring = false));
    });
    boxes.forEach((box, i) => box?.addEventListener("scroll", handlers[i]!, { passive: true }));
    return () => boxes.forEach((box, i) => box?.removeEventListener("scroll", handlers[i]!));
  }, [sync, look, ready, a, b]);

  const pickChange = (i: number) => {
    if (!ready) return;
    const c = ready.changes[i];
    if (!c) return;
    setActive(i);
    setPickA(c.a ?? (c.b != null ? view.bToA.get(c.b) ?? null : null));
    setPickB(c.b ?? (c.a != null ? view.aToB.get(c.a) ?? null : null));
  };

  useHotkeys({
    j: () => ready && pickChange(Math.min(ready.changes.length - 1, (active ?? -1) + 1)),
    k: () => ready && pickChange(Math.max(0, (active ?? 1) - 1)),
    ArrowDown: () => ready && tab === "changes" && pickChange(Math.min(ready.changes.length - 1, (active ?? -1) + 1)),
    ArrowUp: () => ready && tab === "changes" && pickChange(Math.max(0, (active ?? 1) - 1)),
    c: () => setTab("changes"),
    w: () => setTab("why"),
    p: () => setTab("props"),
    x: swapSides,
  });

  if (!a || !b) return <div className="empty">Pick two plans to compare.</div>;

  const setLook = (next: Look) => {
    setLookState(next);
    remember(LOOK_KEY, next);
  };
  const setSync = (on: boolean) => {
    setSyncState(on);
    remember(SYNC_KEY, on ? "on" : "off");
  };
  const connection = contextOf(a, b);
  const nodeA = pickA != null ? nodesOf(a.plan).find((n) => n.id === pickA) ?? null : null;
  const nodeB = pickB != null ? nodesOf(b.plan).find((n) => n.id === pickB) ?? null : null;
  const v = ready ? verdict(ready) : null;

  return (
    <div className="plans-compare cmp">
      <div className="compare-pickers">
        <SidePicker side="A" value={a.id} onChange={(id) => setSide("a", id)} />
        <button className="btn icon ghost" title="Swap A and B (X)" aria-label="Swap A and B" onClick={swapSides}>
          <ArrowLeftRight size={14} />
        </button>
        <SidePicker side="B" value={b.id} onChange={(id) => setSide("b", id)} />
        <span className="spacer" />
        <span className="seg" role="group" aria-label="Plan view">
          <button className={look === "diagram" ? "on" : ""} onClick={() => setLook("diagram")}>
            Diagram
          </button>
          <button className={look === "tree" ? "on" : ""} onClick={() => setLook("tree")}>
            Tree
          </button>
        </span>
        {look === "diagram" && (
          <button className={`btn ghost cmp-sync ${sync ? "on" : ""}`} aria-pressed={sync} title={sync ? "Zoom and scroll move both plans together" : "Zoom and scroll each plan on its own"} onClick={() => setSync(!sync)}>
            {sync ? <Link2 size={13} /> : <Unlink2 size={13} />} Sync
          </button>
        )}
        <ShareMenu
          actions={[
            {
              id: "md",
              icon: <ClipboardCopy size={15} />,
              label: "Copy for a pull request",
              hint: "Markdown: the verdict, what changed and why",
              redacts: true,
              run: (redact) => deliver(api().then((c) => c.exportDiff(a.plan, b.plan, names[0], names[1], "markdown", exportOptions(redact), connection)), "copy", redact),
            },
            {
              id: "html",
              icon: <FileCode size={15} />,
              label: "Save as an HTML page",
              hint: "One file with both diagrams; opens offline, prints cleanly",
              redacts: true,
              run: (redact) => deliver(api().then((c) => c.exportDiff(a.plan, b.plan, names[0], names[1], "html", exportOptions(redact), connection)), "save", redact),
            },
          ]}
        />
      </div>

      {diff.status === "loading" && (
        <div className="cmp-verdict">
          <div className="skeleton" style={{ height: 22, width: "62%" }} />
          <div className="skeleton" style={{ height: 15, width: "38%", marginTop: 10 }} />
          <div className="skeleton" style={{ height: 44, width: "100%", marginTop: 14 }} />
        </div>
      )}
      {diff.status === "error" &&
        (diff.error.code === "unsupported" ? (
          <div className="cmp-info">
            <Info size={16} />
            <div>
              <b>{diff.error.message}</b>
              <div className="muted">{diff.error.hint}</div>
              <div className="cmp-samples">
                Try a sample:{" "}
                {SAMPLES.map((e) => (
                  <button key={e.key} className="link" onClick={() => trySample(e.key)}>
                    {e.label}
                  </button>
                ))}
              </div>
            </div>
          </div>
        ) : (
          <div className="notice error">
            <b>{diff.error.message}</b>
            {diff.error.hint && <div>{diff.error.hint}</div>}
          </div>
        ))}

      {ready && v && (
        <section className="cmp-verdict" aria-live="polite">
          <p className="cmp-lead">
            <span className={`cmp-pill ${v.tone}`}>{v.text}</span>
            {ready.headlines[0]}
          </p>
          {ready.headlines.length > 1 && (
            <ul className="cmp-more">
              {ready.headlines.slice(1).map((h, i) => (
                <li key={i}>{h}</li>
              ))}
            </ul>
          )}
          <div className="cmp-figures">
            <Figure label={`cost · ${names[0]}`} value={cost(ready.cost_a)} />
            <Figure label={`cost · ${names[1]}`} value={cost(ready.cost_b)} tone={v.tone} />
            {ready.time_a_ms != null && ready.time_b_ms != null && (
              <>
                <Figure label={`time · ${names[0]}`} value={duration(ready.time_a_ms)} />
                <Figure label={`time · ${names[1]}`} value={duration(ready.time_b_ms)} tone={v.tone} />
              </>
            )}
            <Figure label="operators" value={`${nodesOf(a.plan).length} → ${nodesOf(b.plan).length}`} />
            <Figure label="identical regions" value={String(ready.regions.length)} />
          </div>
        </section>
      )}

      {ready && (
        <section className="cmp-panel">
          <div className="cmp-tabs" role="tablist" aria-label="Comparison details">
            <TabButton id="changes" tab={tab} onPick={setTab} label="What changed" count={ready.changes.length} hotkey="C" />
            <TabButton id="why" tab={tab} onPick={setTab} label="Why" count={ready.scenarios.length} hotkey="W" />
            <TabButton id="props" tab={tab} onPick={setTab} label="Properties" hotkey="P" />
            <span className="spacer" />
            {tab === "changes" && ready.changes.length > 0 && <span className="muted cmp-keys">J / K to step through</span>}
          </div>
          <div className="cmp-body" role="tabpanel">
            {tab === "changes" &&
              (ready.changes.length === 0 ? (
                <div className="cmp-empty">
                  <CircleCheck size={18} />
                  <div>
                    <b>{ready.same_shape ? "Same steps in both plans." : "Only minor steps differ."}</b>
                    <div className="muted">Any difference in speed comes from the data or the server, not from the plan.</div>
                  </div>
                </div>
              ) : (
                <ul className="cmp-changes">
                  {ready.changes.map((c, i) => (
                    <li key={i}>
                      <button className={`cmp-change ${active === i ? "on" : ""}`} onClick={() => pickChange(i)}>
                        <span className={`cmp-tag ${CHANGE[c.kind].tone}`}>{CHANGE[c.kind].label}</span>
                        <span className="cmp-change-text">
                          <span className="cmp-title">{c.title}</span>
                          {c.detail && <span className="cmp-detail">{c.detail}</span>}
                        </span>
                      </button>
                    </li>
                  ))}
                </ul>
              ))}
            {tab === "why" &&
              (ready.scenarios.length === 0 ? (
                <div className="cmp-empty">
                  <CircleCheck size={18} />
                  <div>
                    <b>Nothing in the conditions differs.</b>
                    <div className="muted">
                      Same estimator, parameter values, memory and parallelism, as far as the plans say. The difference is in the steps under “What changed”.
                    </div>
                  </div>
                </div>
              ) : (
                <ul className="cmp-scenarios">
                  {ready.scenarios.map((s, i) => (
                    <li key={i} className="cmp-scenario">
                      <span className="cmp-scenario-icon" aria-hidden>
                        {SCENARIO[s.kind].icon}
                      </span>
                      <div>
                        <div className="cmp-scenario-kind">{SCENARIO[s.kind].label}</div>
                        <div className="cmp-title">{s.title}</div>
                        {s.detail && <div className="cmp-detail wrap">{s.detail}</div>}
                        {(s.a != null || s.b != null) && (
                          <button
                            className="link"
                            onClick={() => {
                              setPickA(s.a ?? (s.b != null ? view.bToA.get(s.b) ?? null : null));
                              setPickB(s.b ?? (s.a != null ? view.aToB.get(s.a) ?? null : null));
                              setTab("props");
                            }}
                          >
                            Show the step
                          </button>
                        )}
                      </div>
                    </li>
                  ))}
                </ul>
              ))}
            {tab === "props" && (
              <PropsCompare a={nodeA} b={nodeB} planA={a.plan} planB={b.plan} names={names} />
            )}
          </div>
        </section>
      )}

      <div className="cmp-legend" aria-hidden>
        <span className="cmp-key">
          <i className="tint" /> Same color: the same steps in both plans
        </span>
        <span className="cmp-key">
          <i className="changed" /> Changed operator or index
        </span>
        <span className="cmp-key">
          <i className="only" /> Only in this plan
        </span>
      </div>
      <div className="compare-sides">
        <Side label="A" item={a} innerRef={sideA}>
          {look === "diagram" ? (
            <PlanDiagram
              key={`a-${a.id}-${b.id}-${ready ? 1 : 0}`}
              plan={a.plan}
              marks={view.marksA}
              tints={view.tintsA}
              selectedId={pickA}
              initialZoom={0.8}
              zoom={sync ? zoom : undefined}
              onZoomChange={setZoom}
              showProps={false}
              onSelect={(id) => {
                setActive(null);
                setPickA(id);
                setPickB(view.aToB.get(id) ?? null);
                setTab("props");
              }}
            />
          ) : (
            <PlanView key={`a-${a.id}`} plan={a.plan} focus={pickA} />
          )}
        </Side>
        <Side label="B" item={b} innerRef={sideB}>
          {look === "diagram" ? (
            <PlanDiagram
              key={`b-${a.id}-${b.id}-${ready ? 1 : 0}`}
              plan={b.plan}
              marks={view.marksB}
              tints={view.tintsB}
              selectedId={pickB}
              initialZoom={0.8}
              zoom={sync ? zoom : undefined}
              onZoomChange={setZoom}
              showProps={false}
              onSelect={(id) => {
                setActive(null);
                setPickB(id);
                setPickA(view.bToA.get(id) ?? null);
                setTab("props");
              }}
            />
          ) : (
            <PlanView key={`b-${b.id}`} plan={b.plan} focus={pickB} />
          )}
        </Side>
      </div>
    </div>
  );
}

function TabButton({ id, tab, onPick, label, count: n, hotkey }: { id: Tab; tab: Tab; onPick: (t: Tab) => void; label: string; count?: number; hotkey: string }) {
  return (
    <button role="tab" aria-selected={tab === id} className={`cmp-tab ${tab === id ? "on" : ""}`} onClick={() => onPick(id)} title={`${label} (${hotkey})`}>
      {label}
      {n != null && <span className="cmp-count">{n}</span>}
    </button>
  );
}

function Figure({ label, value, tone }: { label: string; value: string; tone?: string }) {
  return (
    <div className={`cmp-figure ${tone === "worse" || tone === "better" ? tone : ""}`}>
      <div className="value">{value}</div>
      <div className="label" title={label}>
        {label}
      </div>
    </div>
  );
}

function SidePicker({ side, value, onChange }: { side: "A" | "B"; value: string; onChange: (id: string) => void }) {
  const items = usePlans((s) => s.items);
  return (
    <label className="side-picker">
      <span className="side-badge">{side}</span>
      <select value={value} onChange={(e) => onChange(e.target.value)} aria-label={`Plan ${side}`}>
        {items.map((p) => (
          <option key={p.id} value={p.id}>
            {p.origin === p.name ? p.name : `${p.name} — ${p.origin}`}
          </option>
        ))}
      </select>
    </label>
  );
}

function Side({ label, item, innerRef, children }: { label: string; item: LoadedPlan; innerRef: React.RefObject<HTMLDivElement | null>; children: ReactNode }) {
  return (
    <div className="compare-side" ref={innerRef}>
      <div className="compare-side-head">
        <span className="side-badge">{label}</span>
        <b>{item.name}</b>
        <span className="muted">
          {planEngineName(item.plan)} · {KIND[item.plan.kind]}
        </span>
      </div>
      {children}
    </div>
  );
}

/** Drops the database part of SQL Server's four-part names so the same column compares equal across environments. */
function norm(value: string): string {
  return value.replace(/\[[^\]]+\]\.(?=\[[^\]]+\]\.\[[^\]]+\]\.\[[^\]]+\])/g, "").replace(/\s+/g, " ").trim();
}

type Row = { label: string; a: string | null; b: string | null };

function rowsFor(node: PlanNode | null, plan: Plan): Record<string, string | null> {
  if (!node) return {};
  const est = node.est_rows * Math.max(1, node.est_executions);
  // SQLite's plans have no estimates or costs: nothing to show rather than zeros.
  const weighed = can(plan.engine, "plan_estimates") || plan.kind === "actual";
  return {
    Operator: node.op,
    Table: node.object,
    Index: node.index,
    "Seek on": node.seek_predicate ? norm(node.seek_predicate) : null,
    Filter: node.predicate ? norm(node.predicate) : null,
    "Estimated rows": weighed ? count(est) : null,
    "Estimated executions": weighed ? count(node.est_executions) : null,
    "Actual rows": node.act_rows != null ? count(node.act_rows) : null,
    "Rows read": node.rows_read != null ? count(node.rows_read) : null,
    "Rows in table": node.table_rows != null ? count(node.table_rows) : null,
    "Actual time": node.act_time_ms != null ? duration(node.act_time_ms) : null,
    // Only where the plan can state one (none beside a step it puts no cost on, nor on a default guess): "—".
    "Share of the plan's cost": weighed ? percent(stepShare(plan, node)) : null,
    "Subtree cost": weighed ? cost(node.subtree_cost) : null,
    Warnings: node.warnings.length ? node.warnings.map((w) => w.replace(/-/g, " ")).join(", ") : null,
  };
}

const GROUPS: { title: string; keys: string[] }[] = [
  { title: "What it does", keys: ["Operator", "Table", "Index", "Seek on", "Filter"] },
  { title: "Rows", keys: ["Estimated rows", "Estimated executions", "Actual rows", "Rows read", "Rows in table"] },
  { title: "Cost", keys: ["Share of the plan's cost", "Subtree cost", "Actual time"] },
  { title: "Warnings", keys: ["Warnings"] },
];

/** The selected step on both sides, property by property, with ≠ where they differ. */
function PropsCompare({ a, b, planA, planB, names }: { a: PlanNode | null; b: PlanNode | null; planA: Plan; planB: Plan; names: [string, string] }) {
  const [showSame, setShowSame] = useState(false);
  if (!a && !b) {
    return (
      <div className="cmp-empty">
        <EqualNot size={18} />
        <div>
          <b>Select a step in either plan.</b>
          <div className="muted">Its properties appear next to the matching step of the other plan, with ≠ where they differ.</div>
        </div>
      </div>
    );
  }
  const ra = rowsFor(a, planA);
  const rb = rowsFor(b, planB);
  const label = (n: PlanNode | null) => (n ? `${n.op}${n.object ? ` · ${n.object.split(".").pop()}` : ""}` : "—");
  const groups = GROUPS.map((g) => ({
    title: g.title,
    rows: g.keys
      .map((k): Row => ({ label: k, a: ra[k] ?? null, b: rb[k] ?? null }))
      .filter((r) => r.a != null || r.b != null),
  })).filter((g) => g.rows.length > 0);
  const keys = [...new Set([...Object.keys(a?.props ?? {}), ...Object.keys(b?.props ?? {})])].sort();
  const other: Row[] = keys.map((k) => ({ label: k, a: a?.props[k] != null ? norm(a.props[k]!) : null, b: b?.props[k] != null ? norm(b.props[k]!) : null }));
  const differs = (r: Row) => (r.a ?? "") !== (r.b ?? "");
  const changed = other.filter(differs);
  const same = other.filter((r) => !differs(r));

  const line = (r: Row) => (
    <div key={r.label} className={`cmp-prop ${differs(r) ? "diff" : ""}`}>
      <span className="cmp-prop-label" title={r.label}>
        {r.label}
      </span>
      <span className={`cmp-prop-value ${r.a == null ? "none" : ""}`}>{r.a ?? "—"}</span>
      <span className="cmp-prop-sign" aria-label={differs(r) ? "differs" : "same"}>
        {differs(r) ? "≠" : ""}
      </span>
      <span className={`cmp-prop-value ${r.b == null ? "none" : ""}`}>{r.b ?? "—"}</span>
    </div>
  );

  return (
    <div className="cmp-props">
      <div className="cmp-prop head">
        <span />
        <span>
          <span className="side-badge">A</span> {label(a)}
          {!a && <span className="muted"> · only in {names[1]}</span>}
        </span>
        <span />
        <span>
          <span className="side-badge">B</span> {label(b)}
          {!b && <span className="muted"> · only in {names[0]}</span>}
        </span>
      </div>
      {groups.map((g) => (
        <div key={g.title} className="cmp-prop-group">
          <div className="cmp-prop-title">{g.title}</div>
          {g.rows.map(line)}
        </div>
      ))}
      {other.length > 0 && (
        <div className="cmp-prop-group">
          <div className="cmp-prop-title">
            Engine properties <span className="muted">· {changed.length} differ</span>
          </div>
          {changed.map(line)}
          {same.length > 0 && (
            <button className="link cmp-prop-more" onClick={() => setShowSame((v) => !v)}>
              {showSame ? "Hide" : "Show"} {same.length} identical
            </button>
          )}
          {showSame && same.map(line)}
        </div>
      )}
    </div>
  );
}
