import { useSyncExternalStore } from "react";
import type { Plan } from "../gen/Plan";
import type { QueryStat } from "../gen/QueryStat";
import type { DatabaseProduct } from "../gen/DatabaseProduct";
import { api, errorMessage, type ApiError } from "./api";
import { PRODUCTS } from "./engines";
import { planSamples } from "./plan-samples";
import { LatestRequest } from "./requests";
import { setState, toast } from "./store";

/** A plan opened in the Plans view: pasted, read from a file, or sent from a query. Kept for the session only. */
export type LoadedPlan = {
  id: string;
  name: string;
  /** Where it came from, shown under the name: a file name, "Pasted", "mssql-demo · query 7". */
  origin: string;
  plan: Plan;
  /** Measurements from the scan this plan was opened from; a later scan must not change its explanation. */
  query?: QueryStat;
  /** The scan the plan came from, when it came from one: scenarios read that scan's statistics, and exports can fetch
   * the plan's original text (.sqlplan). */
  source?: PlanSource;
};

export type PlanSource = { connection: string; queryId: string; planId: string | null };

export type PlansState = {
  items: LoadedPlan[];
  selected: string | null;
  mode: "single" | "compare";
  a: string | null;
  b: string | null;
  busy: boolean;
  error: ApiError | null;
};

let state: PlansState = { items: [], selected: null, mode: "single", a: null, b: null, busy: false, error: null };
const listeners = new Set<() => void>();
let counter = 0;
const sampleRequest = new LatestRequest();

export function getPlans(): PlansState {
  return state;
}

function update(patch: Partial<PlansState>): void {
  state = { ...state, ...patch };
  listeners.forEach((l) => l());
}

export function usePlans<T>(select: (s: PlansState) => T): T {
  return useSyncExternalStore(
    (cb) => {
      listeners.add(cb);
      return () => listeners.delete(cb);
    },
    () => select(state),
  );
}

/** Add a plan and show it. The same plan object sent twice (e.g. "Open in Plans" again) is not duplicated. */
export function addPlan(plan: Plan, name: string, origin: string, options: { show?: boolean; source?: PlanSource; query?: QueryStat } = {}): string {
  sampleRequest.invalidate();
  const existing = state.items.find((p) => p.plan === plan);
  const id = existing?.id ?? `p${++counter}`;
  if (!existing) update({ items: [...state.items, { id, name, origin, plan, source: options.source, query: options.query && structuredClone(options.query) }] });
  if (options.show ?? true) {
    update({ selected: id, mode: "single", error: null });
    setState({ view: "plans" });
  }
  return id;
}

export function dismissError(): void {
  update({ error: null });
}

export function selectPlan(id: string): void {
  update({ selected: id, mode: state.mode === "compare" ? "single" : state.mode });
}

export function removePlan(id: string): void {
  const items = state.items.filter((p) => p.id !== id);
  const fallback = items[items.length - 1]?.id ?? null;
  update({
    items,
    selected: state.selected === id ? fallback : state.selected,
    a: state.a === id ? null : state.a,
    b: state.b === id ? null : state.b,
    mode: items.length < 2 ? "single" : state.mode,
  });
}

export function setMode(mode: "single" | "compare"): void {
  if (mode === "compare") {
    const ids = state.items.map((p) => p.id);
    const a = state.a && ids.includes(state.a) ? state.a : ids.find((id) => id !== state.selected) ?? ids[0] ?? null;
    const b = state.b && ids.includes(state.b) && state.b !== a ? state.b : ids.find((id) => id !== a) ?? null;
    update({ mode, a, b });
  } else {
    update({ mode });
  }
}

export function setSide(side: "a" | "b", id: string): void {
  update(side === "a" ? { a: id } : { b: id });
}

export function swapSides(): void {
  update({ a: state.b, b: state.a });
}

/** Open two plans side by side (from Queries' plan variants). */
type Side = { plan: Plan; name: string; origin: string; source?: PlanSource; query?: QueryStat };

export function compare(a: Side, b: Side): void {
  const idA = addPlan(a.plan, a.name, a.origin, { show: false, source: a.source, query: a.query });
  const idB = addPlan(b.plan, b.name, b.origin, { show: false, source: b.source, query: b.query });
  update({ mode: "compare", a: idA, b: idB, selected: idB, error: null });
  setState({ view: "plans" });
}

/** Parse pasted or file text into a plan and show it; errors stay in the Plans view. */
export async function importText(text: string, name: string, origin: string): Promise<boolean> {
  sampleRequest.invalidate();
  update({ busy: true, error: null });
  setState({ view: "plans" });
  try {
    const plan = await (await api()).parsePlan(text);
    addPlan(plan, name, origin);
    update({ busy: false });
    return true;
  } catch (e) {
    update({ busy: false, error: errorMessage(e) });
    return false;
  }
}

/** Read a plan file: UTF-8, or UTF-16 with a byte-order mark (SSMS saves .sqlplan that way). */
export async function importFile(file: File): Promise<boolean> {
  if (file.size > 64 * 1024 * 1024) {
    update({ error: { code: "invalid_input", message: `${file.name} is far larger than any query plan`, hint: null } });
    setState({ view: "plans" });
    return false;
  }
  const bytes = new Uint8Array(await file.arrayBuffer());
  const encoding = bytes[0] === 0xff && bytes[1] === 0xfe ? "utf-16le" : bytes[0] === 0xfe && bytes[1] === 0xff ? "utf-16be" : "utf-8";
  const text = new TextDecoder(encoding).decode(bytes);
  return importText(text, file.name.replace(/\.(sqlplan|json|txt|xml)$/i, ""), file.name);
}

/** Cheap shape check so pasting ordinary text outside the Plans view never hijacks the clipboard. */
export function looksLikePlan(text: string): boolean {
  const t = text.trimStart();
  return (
    t.includes("ShowPlanXML") ||
    /"Plan"\s*:\s*\{/.test(t) ||
    /"query_block"\s*:/.test(t) ||
    /\(cost=[\d.]+\.\.[\d.]+ rows=/.test(t) ||
    /^(EXPLAIN: )?-> .+\(cost=/m.test(t) ||
    // SQLite's EXPLAIN QUERY PLAN as the sqlite3 shell prints it.
    /^QUERY PLAN\s*\n[|`]--/.test(t)
  );
}

function editable(target: EventTarget | null): boolean {
  if (!(target instanceof HTMLElement)) return false;
  return target.isContentEditable || target.closest("input, textarea, [contenteditable], .cm-editor") !== null;
}

/** ⌘V anywhere outside a text field: a plan opens in the Plans view. In the Plans view any paste is tried. */
export function installPlanPaste(currentView: () => string): () => void {
  const onPaste = (e: ClipboardEvent) => {
    if (editable(e.target)) return;
    const text = e.clipboardData?.getData("text/plain") ?? "";
    if (!text.trim()) return;
    if (currentView() !== "plans" && !looksLikePlan(text)) return;
    e.preventDefault();
    importText(text, `Pasted plan ${state.items.length + 1}`, "Pasted");
  };
  window.addEventListener("paste", onPaste);
  return () => window.removeEventListener("paste", onPaste);
}

/** The plan's own text when it was imported (so .sqlplan opens in SSMS again), else Culprit's JSON. */
export function exportable(item: LoadedPlan): { name: string; contents: string; native: boolean } {
  const raw = item.plan.raw;
  const base = item.name.replace(/[^\w .-]+/g, "-").slice(0, 70) || "plan";
  if (raw) {
    const trimmed = raw.trimStart();
    const ext = item.plan.engine === "mssql" ? "sqlplan" : trimmed.startsWith("{") || trimmed.startsWith("[") ? "json" : "txt";
    return { name: `${base}.${ext}`, contents: raw, native: true };
  }
  return { name: `${base}.plan.json`, contents: JSON.stringify(item.plan, null, 2), native: false };
}

export async function savePlan(item: LoadedPlan, asJson: boolean): Promise<void> {
  const file = asJson
    ? { name: `${item.name.replace(/[^\w .-]+/g, "-").slice(0, 70) || "plan"}.plan.json`, contents: JSON.stringify(item.plan, null, 2) }
    : exportable(item);
  try {
    const where = await (await api()).savePlanFile(file.name, file.contents);
    toast(where.includes("/") || where.includes("\\") ? `Saved to ${where.split(/[\\/]/).slice(-2).join("/")}` : `Saved ${where}`);
  } catch (e) {
    toast(errorMessage(e).message);
  }
}

/** Every product must have a captured comparison; backend and consumer tests enforce catalog parity. */
export const SAMPLES = PRODUCTS;

/** Compare two sample plans captured on the demo databases (a seek that became a scan), to see what Plans does. */
export async function trySample(product: DatabaseProduct): Promise<void> {
  const current = sampleRequest.start();
  update({ busy: true, error: null });
  try {
    const sample = (await planSamples()).find((sample) => sample.product === product);
    if (!current()) return;
    if (!sample) throw new Error("This product's captured comparison is missing");
    const origin = `Sample · ${PRODUCTS.find((p) => p.id === product)?.label ?? product}`;
    compare({ plan: sample.plans[0], name: sample.names[0], origin }, { plan: sample.plans[1], name: sample.names[1], origin });
    update({ busy: false });
  } catch (error) {
    if (current()) update({ busy: false, error: errorMessage(error) });
  }
}
