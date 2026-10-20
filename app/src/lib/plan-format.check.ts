import samples from "../../../fixtures/plan-view.sample.json";
import type { Plan } from "../gen/Plan";
import type { QueryStat } from "../gen/QueryStat";
import { findingNoShareReason, noShareReason, sharesNote } from "./plan-format";
import { addPlan, getPlans, removePlan } from "./plans";

function check(ok: boolean, message: string): void { if (!ok) throw new Error(message); }
function sample(name: string): { plan: Plan; query: QueryStat } {
  const value = samples.find((c) => c.name === name)!;
  return structuredClone(value) as unknown as { plan: Plan; query: QueryStat };
}
const read = sample("wide-read");
check(sharesNote(read.plan, read.query)!.includes("72 pages a run"), "The renderer lost the captured read evidence");
const slow = sample("slow-variant");
const fast = sample("fast-variant");
check(sharesNote(slow.plan, slow.query)!.includes("322 pages a run"), "The selected variant's evidence was lost");
check(!sharesNote(fast.plan, fast.query)!.includes("322 pages"), "Another variant's measurements leaked in");
check(sharesNote(slow.plan, fast.query)!.includes("different statement or plan measurements"), "Mismatched variant context must not reuse an explanation");
const outlier = sample("outlier");
check(sharesNote(outlier.plan, outlier.query)!.includes("4 runs took 2.4 s in all"), "The renderer lost the outlier-aware wording");
const small = sample("small-scan");
check(noShareReason(small.plan, small.plan.root, small.query)!.includes("as a whole is cheap"), "The general size bar still holds");
check(findingNoShareReason(small.plan, small.plan.root, small.query)!.includes("all 3.9K rows"), "A finding's explicit whole-table evidence must stay visible");

const old = sample("retained-old");
const opened = addPlan(old.plan, "Captured plan", "Demo query", { show: false, query: old.query });
const saved = getPlans().items.find((p) => p.id === opened)!;
const original = sharesNote(saved.plan, saved.query);
old.query.logical_reads = 75_000;
old.query.mean_time_ms = 50;
check(sharesNote(saved.plan, saved.query) === original, "A later scan changed retained evidence");
const latest = sample("retained-new");
const reopened = addPlan(latest.plan, "Captured plan", "Demo query", { show: false, query: latest.query });
check(reopened !== opened && sharesNote(latest.plan, latest.query)!.includes("750 pages a run"), "A newer plan lost its own evidence");
check(addPlan(old.plan, "Captured plan", "Demo query", { show: false, query: old.query }) === opened, "The same plan object was duplicated");
removePlan(opened); removePlan(reopened);
console.log("Plan renderer preserves Rust's explanations, variants and retained measurements");
