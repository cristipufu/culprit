import { ArrowLeftRight, ClipboardPaste, Copy, Download, FileUp, GitCompareArrows, Trash2, TriangleAlert, X } from "lucide-react";
import { useEffect, useMemo, useRef, useState, type DragEvent, type ReactNode } from "react";
import { PlanDiagram, type Mark } from "../components/PlanDiagram";
import { PlanView } from "../components/PlanView";
import { Sql } from "../components/Sql";
import type { ChangeKind } from "../gen/ChangeKind";
import type { Plan } from "../gen/Plan";
import type { PlanDiff } from "../gen/PlanDiff";
import { api, copyText, errorMessage, type ApiError } from "../lib/api";
import { duration } from "../lib/format";
import { useHotkeys } from "../lib/hotkeys";
import {
  dismissError,
  exportable,
  importFile,
  importText,
  removePlan,
  savePlan,
  selectPlan,
  setMode,
  setSide,
  swapSides,
  usePlans,
  type LoadedPlan,
} from "../lib/plans";
import { toast } from "../lib/store";

const ENGINE: Record<Plan["engine"], string> = { mssql: "SQL Server", postgres: "Postgres", mysql: "MySQL" };
const KIND: Record<Plan["kind"], string> = { estimated: "estimated", generic: "generic", actual: "actual" };
const LOOK_KEY = "culprit.plan.look";

type Look = "diagram" | "tree";

function rememberedLook(): Look {
  try {
    return localStorage.getItem(LOOK_KEY) === "tree" ? "tree" : "diagram";
  } catch {
    return "diagram";
  }
}

function cost(value: number): string {
  if (value < 10) return value.toFixed(3);
  if (value < 100) return value.toFixed(1);
  if (value >= 1e6) return `${(value / 1e6).toFixed(1)}M`;
  return value >= 1e4 ? `${Math.round(value / 1e3)}K` : value.toFixed(0);
}

function nodeCount(plan: Plan): number {
  const walk = (n: Plan["root"]): number => 1 + n.children.reduce((s, c) => s + walk(c), 0);
  return walk(plan.root);
}

export function Plans() {
  const items = usePlans((s) => s.items);
  const mode = usePlans((s) => s.mode);
  const selected = usePlans((s) => s.selected);
  const busy = usePlans((s) => s.busy);
  const error = usePlans((s) => s.error);
  const fileInput = useRef<HTMLInputElement>(null);
  const [dragging, setDragging] = useState(false);
  const current = items.find((p) => p.id === selected) ?? items[items.length - 1] ?? null;

  const openFile = () => fileInput.current?.click();
  const pasteFromClipboard = async () => {
    try {
      const text = await navigator.clipboard.readText();
      if (text.trim()) await importText(text, `Pasted plan ${items.length + 1}`, "Pasted");
      else toast("The clipboard is empty");
    } catch {
      toast("Press ⌘V to paste the plan");
    }
  };
  useHotkeys({ "mod+o": openFile });

  const onDrop = async (e: DragEvent) => {
    e.preventDefault();
    setDragging(false);
    const files = [...e.dataTransfer.files];
    if (files.length) {
      for (const file of files.slice(0, 8)) await importFile(file);
      return;
    }
    const text = e.dataTransfer.getData("text/plain");
    if (text.trim()) await importText(text, `Dropped plan ${items.length + 1}`, "Dropped text");
  };

  return (
    <div
      className={`plans ${dragging ? "dragging" : ""}`}
      onDragOver={(e) => {
        e.preventDefault();
        if (!dragging) setDragging(true);
      }}
      onDragLeave={(e) => {
        if (e.currentTarget === e.target || !e.currentTarget.contains(e.relatedTarget as Node)) setDragging(false);
      }}
      onDrop={onDrop}
    >
      <input
        ref={fileInput}
        type="file"
        accept=".sqlplan,.json,.txt,.xml"
        multiple
        hidden
        onChange={async (e) => {
          const files = [...(e.target.files ?? [])];
          e.target.value = "";
          for (const file of files) await importFile(file);
        }}
      />
      {items.length === 0 ? (
        <Welcome busy={busy} error={error} onOpen={openFile} onPaste={pasteFromClipboard} />
      ) : (
        <>
          <aside className="plans-list">
            <div className="plans-actions">
              <button className="btn" onClick={openFile} title="Open a plan file (⌘O)">
                <FileUp size={13} /> Open
              </button>
              <button className="btn" onClick={pasteFromClipboard} title="Paste a plan (⌘V)">
                <ClipboardPaste size={13} /> Paste
              </button>
            </div>
            <div className="plans-mode seg">
              <button className={mode === "single" ? "on" : ""} onClick={() => setMode("single")}>
                One plan
              </button>
              <button className={mode === "compare" ? "on" : ""} disabled={items.length < 2} title={items.length < 2 ? "Open a second plan to compare" : "Compare two plans"} onClick={() => setMode("compare")}>
                <GitCompareArrows size={12} /> Compare
              </button>
            </div>
            {mode === "compare" && <div className="plans-tip">A is the baseline. Click a plan to compare it as B.</div>}
            {error && <ErrorNote error={error} onDismiss={dismissError} />}
            {busy && (
              <div className="plans-busy">
                <span className="spinner" style={{ width: 12, height: 12 }} /> Reading the plan…
              </div>
            )}
            <div className="plans-items">
              {items.map((item) => (
                <PlanRow key={item.id} item={item} active={mode === "single" && item.id === current?.id} />
              ))}
            </div>
            <div className="plans-hint">Drop .sqlplan, EXPLAIN JSON or text anywhere here</div>
          </aside>
          <section className="plans-main">{mode === "compare" ? <Compare /> : current && <Single item={current} />}</section>
        </>
      )}
      {dragging && <div className="drop-veil">Drop to open the plan</div>}
    </div>
  );
}

function Welcome({ busy, error, onOpen, onPaste }: { busy: boolean; error: ApiError | null; onOpen: () => void; onPaste: () => void }) {
  return (
    <div className="plans-welcome">
      <div className="welcome-card">
        <GitCompareArrows size={26} className="welcome-icon" />
        <h2>Paste, drop or open a query plan</h2>
        <p className="muted">Culprit draws it, shows where the cost goes, and compares any two plans step by step.</p>
        <div className="welcome-actions">
          <button className="btn primary" onClick={onOpen}>
            <FileUp size={14} /> Open a file…
          </button>
          <button className="btn" onClick={onPaste}>
            <ClipboardPaste size={14} /> Paste <span className="kbd">⌘V</span>
          </button>
        </div>
        {busy && (
          <div className="plans-busy">
            <span className="spinner" style={{ width: 12, height: 12 }} /> Reading the plan…
          </div>
        )}
        {error && <ErrorNote error={error} onDismiss={dismissError} />}
        <div className="formats">
          <Format engine="SQL Server" what=".sqlplan or showplan XML (SSMS, Query Store)" />
          <Format engine="Postgres" what="EXPLAIN or EXPLAIN ANALYZE, as text or FORMAT JSON" />
          <Format engine="MySQL" what="EXPLAIN FORMAT=JSON, EXPLAIN ANALYZE or FORMAT=TREE" />
        </div>
        <p className="muted small">From Queries, any plan opens here, and two plans of the same query open side by side.</p>
      </div>
    </div>
  );
}

function Format({ engine, what }: { engine: string; what: string }) {
  return (
    <div className="format-row">
      <b>{engine}</b>
      <span className="muted">{what}</span>
    </div>
  );
}

function ErrorNote({ error, onDismiss }: { error: ApiError; onDismiss?: () => void }) {
  return (
    <div className="notice error plans-error">
      <TriangleAlert size={14} style={{ flex: "none", marginTop: 2 }} />
      <div style={{ flex: 1 }}>
        <b>{error.message}</b>
        {error.hint && <div className="muted">{error.hint}</div>}
      </div>
      {onDismiss && (
        <button className="btn icon ghost" title="Dismiss" onClick={onDismiss} style={{ width: 20, height: 20 }}>
          <X size={12} />
        </button>
      )}
    </div>
  );
}

function PlanRow({ item, active }: { item: LoadedPlan; active: boolean }) {
  const mode = usePlans((s) => s.mode);
  const a = usePlans((s) => s.a);
  const b = usePlans((s) => s.b);
  const side = item.id === a ? "A" : item.id === b ? "B" : null;
  return (
    <div className={`plan-row ${active ? "on" : ""} ${mode === "compare" && side ? "in-compare" : ""}`}>
      <button
        className="plan-row-main"
        title={mode === "compare" ? (side === "A" ? "Swap A and B" : "Compare this plan as B") : undefined}
        onClick={() => (mode !== "compare" ? selectPlan(item.id) : side === "A" ? swapSides() : setSide("b", item.id))}
      >
        <span className="plan-row-name">
          {mode === "compare" && side && <span className="side-badge">{side}</span>}
          {item.name}
        </span>
        <span className="plan-row-meta">
          {ENGINE[item.plan.engine]} · {KIND[item.plan.kind]} · cost {cost(item.plan.total_cost)}
        </span>
        {item.origin !== item.name && <span className="plan-row-origin">{item.origin}</span>}
      </button>
      <button className="btn icon ghost plan-row-remove" title="Close this plan" onClick={() => removePlan(item.id)}>
        <X size={13} />
      </button>
    </div>
  );
}

function LookToggle({ look, onChange }: { look: Look; onChange: (look: Look) => void }) {
  return (
    <span className="seg">
      <button className={look === "diagram" ? "on" : ""} onClick={() => onChange("diagram")}>
        Diagram
      </button>
      <button className={look === "tree" ? "on" : ""} onClick={() => onChange("tree")}>
        Tree
      </button>
    </span>
  );
}

function useLook(): [Look, (look: Look) => void] {
  const [look, setLook] = useState<Look>(rememberedLook);
  return [
    look,
    (next) => {
      setLook(next);
      try {
        localStorage.setItem(LOOK_KEY, next);
      } catch {}
    },
  ];
}

function Single({ item }: { item: LoadedPlan }) {
  const [look, setLook] = useLook();
  const [showSql, setShowSql] = useState(false);
  const file = exportable(item);
  const plan = item.plan;
  return (
    <div className="plans-single">
      <div className="plans-head">
        <div style={{ minWidth: 0 }}>
          <h2 title={item.name}>{item.name}</h2>
          <div className="muted plans-sub">
            {ENGINE[plan.engine]} · {KIND[plan.kind]} plan · {nodeCount(plan) === 1 ? "1 operator" : `${nodeCount(plan)} operators`}
            {item.origin !== item.name && ` · ${item.origin}`}
          </div>
        </div>
        <span className="spacer" />
        <LookToggle look={look} onChange={setLook} />
        {file.native && (
          <button className="btn" title={`Save ${file.name} to Downloads`} onClick={() => savePlan(item, false)}>
            <Download size={13} /> Save .{file.name.split(".").pop()}
          </button>
        )}
        <button className="btn ghost" title="Save Culprit's plan JSON to Downloads" onClick={() => savePlan(item, true)}>
          <Download size={13} /> JSON
        </button>
        <button
          className="btn icon ghost"
          title="Copy the plan text"
          onClick={() => copyText(file.contents).then(() => toast("Plan copied"), () => toast("Couldn't copy"))}
        >
          <Copy size={13} />
        </button>
        <button className="btn icon ghost" title="Close this plan" onClick={() => removePlan(item.id)}>
          <Trash2 size={13} />
        </button>
      </div>
      {plan.statement && (
        <div className={`plans-sql ${showSql ? "open" : ""}`} onClick={() => setShowSql((v) => !v)} title={showSql ? "Collapse" : "Show the whole statement"}>
          <Sql text={plan.statement} />
        </div>
      )}
      {look === "diagram" ? <PlanDiagram key={item.id} plan={plan} /> : <PlanView key={item.id} plan={plan} />}
    </div>
  );
}

const CHANGE_ICON: Record<ChangeKind, string> = {
  access: "~",
  operator: "~",
  added: "+",
  removed: "−",
  cost: "$",
  rows: "#",
  warning: "!",
};

function Compare() {
  const items = usePlans((s) => s.items);
  const aId = usePlans((s) => s.a);
  const bId = usePlans((s) => s.b);
  const a = items.find((p) => p.id === aId) ?? null;
  const b = items.find((p) => p.id === bId) ?? null;
  const [diff, setDiff] = useState<{ status: "idle" | "loading" } | { status: "ready"; diff: PlanDiff } | { status: "error"; error: ApiError }>({ status: "idle" });
  const [pickA, setPickA] = useState<number | null>(null);
  const [pickB, setPickB] = useState<number | null>(null);
  const [active, setActive] = useState<number | null>(null);
  const [look, setLook] = useLook();

  const names = useMemo((): [string, string] => {
    if (!a || !b || a.name === b.name) return ["plan A", "plan B"];
    return [a.name, b.name];
  }, [a, b]);

  useEffect(() => {
    setActive(null);
    setPickA(null);
    setPickB(null);
    if (!a || !b) return setDiff({ status: "idle" });
    let cancelled = false;
    setDiff({ status: "loading" });
    api()
      .then((client) => client.diffPlans(a.plan, b.plan, names[0], names[1]))
      .then((d) => !cancelled && setDiff({ status: "ready", diff: d }))
      .catch((e) => !cancelled && setDiff({ status: "error", error: errorMessage(e) }));
    return () => {
      cancelled = true;
    };
  }, [a, b, names]);

  const ready = diff.status === "ready" ? diff.diff : null;
  const { marksA, marksB, aToB, bToA } = useMemo(() => {
    const marksA = new Map<number, Mark>();
    const marksB = new Map<number, Mark>();
    const aToB = new Map<number, number>();
    const bToA = new Map<number, number>();
    if (ready) {
      ready.matches.forEach((m) => {
        aToB.set(m.a, m.b);
        bToA.set(m.b, m.a);
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
    return { marksA, marksB, aToB, bToA };
  }, [ready]);

  if (!a || !b) return <div className="empty">Pick two plans to compare.</div>;

  return (
    <div className="plans-compare">
      <div className="compare-pickers">
        <SidePicker side="A" value={a.id} onChange={(id) => setSide("a", id)} />
        <button className="btn icon ghost" title="Swap A and B" onClick={swapSides}>
          <ArrowLeftRight size={14} />
        </button>
        <SidePicker side="B" value={b.id} onChange={(id) => setSide("b", id)} />
        <span className="spacer" />
        <LookToggle look={look} onChange={setLook} />
      </div>
      {diff.status === "loading" && (
        <div className="diff-summary">
          <div className="skeleton" style={{ height: 22, width: "60%" }} />
          <div className="skeleton" style={{ height: 16, width: "40%", marginTop: 8 }} />
        </div>
      )}
      {diff.status === "error" && <ErrorNote error={diff.error} />}
      {ready && (
        <div className="diff-summary">
          <div className="diff-headlines">
            {ready.headlines.map((h, i) => (
              <p key={i} className={i === 0 ? "lead" : ""}>
                {h}
              </p>
            ))}
          </div>
          <div className="diff-figures">
            <Figure label={`cost · ${names[0]}`} value={cost(ready.cost_a)} />
            <Ratio ratio={ready.cost_ratio} />
            <Figure label={`cost · ${names[1]}`} value={cost(ready.cost_b)} />
            {ready.time_a_ms != null && ready.time_b_ms != null && (
              <>
                <span className="diff-sep" />
                <Figure label="actual time A" value={duration(ready.time_a_ms)} />
                <Figure label="actual time B" value={duration(ready.time_b_ms)} />
              </>
            )}
          </div>
          {ready.changes.length > 0 && (
            <div className="diff-changes">
              {ready.changes.map((c, i) => (
                <button
                  key={i}
                  className={`diff-change ${c.kind} ${active === i ? "on" : ""}`}
                  onClick={() => {
                    setActive(i);
                    setPickA(c.a ?? (c.b != null ? bToA.get(c.b) ?? null : null));
                    setPickB(c.b ?? (c.a != null ? aToB.get(c.a) ?? null : null));
                  }}
                >
                  <span className="dc-icon">{CHANGE_ICON[c.kind]}</span>
                  <span className="dc-title">{c.title}</span>
                  {c.detail && <span className="dc-detail">{c.detail}</span>}
                </button>
              ))}
            </div>
          )}
        </div>
      )}
      <div className="compare-sides">
        <Side label="A" item={a} look={look}>
          {look === "diagram" ? (
            <PlanDiagram
              key={`a-${a.id}-${b.id}-${ready ? 1 : 0}`}
              plan={a.plan}
              marks={marksA}
              selectedId={pickA}
              initialZoom={0.8}
              onSelect={(id) => {
                setActive(null);
                setPickB(aToB.get(id) ?? null);
              }}
            />
          ) : (
            <PlanView key={`a-${a.id}`} plan={a.plan} focus={pickA} />
          )}
        </Side>
        <Side label="B" item={b} look={look}>
          {look === "diagram" ? (
            <PlanDiagram
              key={`b-${a.id}-${b.id}-${ready ? 1 : 0}`}
              plan={b.plan}
              marks={marksB}
              selectedId={pickB}
              initialZoom={0.8}
              onSelect={(id) => {
                setActive(null);
                setPickA(bToA.get(id) ?? null);
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

function SidePicker({ side, value, onChange }: { side: "A" | "B"; value: string; onChange: (id: string) => void }) {
  const items = usePlans((s) => s.items);
  return (
    <label className="side-picker">
      <span className="side-badge">{side}</span>
      <select value={value} onChange={(e) => onChange(e.target.value)}>
        {items.map((p) => (
          <option key={p.id} value={p.id}>
            {p.origin === p.name ? p.name : `${p.name} — ${p.origin}`}
          </option>
        ))}
      </select>
    </label>
  );
}

function Side({ label, item, look, children }: { label: string; item: LoadedPlan; look: Look; children: ReactNode }) {
  return (
    <div className={`compare-side ${look}`}>
      <div className="compare-side-head">
        <span className="side-badge">{label}</span>
        <b>{item.name}</b>
        <span className="muted">
          {ENGINE[item.plan.engine]} · {KIND[item.plan.kind]}
        </span>
      </div>
      {children}
    </div>
  );
}

function Figure({ label, value }: { label: string; value: string }) {
  return (
    <div className="diff-figure">
      <div className="value">{value}</div>
      <div className="label">{label}</div>
    </div>
  );
}

function Ratio({ ratio }: { ratio: number | null }) {
  if (ratio == null) return <span className="diff-ratio">→</span>;
  const worse = ratio > 1;
  const factor = worse ? ratio : 1 / ratio;
  if (factor < 1.05) return <span className="diff-ratio same">≈ same</span>;
  const text = factor >= 10 ? `${Math.round(factor).toLocaleString()}×` : `${factor.toFixed(1)}×`;
  return <span className={`diff-ratio ${worse ? "worse" : "better"}`}>{worse ? `${text} costlier` : `${text} cheaper`}</span>;
}
