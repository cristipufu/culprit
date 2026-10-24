import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { PlanCatalog } from "../components/PlanCatalog";
import type { Plan } from "../gen/Plan";
import type { PlanSample } from "../gen/PlanSample";
import { PRODUCTS } from "./engines";
import { planSamples, sameSamplePlan } from "./plan-samples";
import { getPlans, removePlan, SAMPLES, trySample } from "./plans";
import shared from "../../../fixtures/share.sample.json";

function check(ok: boolean, message: string) {
  if (!ok) throw new Error(message);
}

const samples = await planSamples();
const rendered = renderToStaticMarkup(createElement(PlanCatalog, { onSample: () => {} }));
for (const product of PRODUCTS) {
  check(rendered.split(`data-product="${product.id}"`).length === 3, `${product.label}: the Plans view needs a format row and sample action`);
  check(rendered.includes(product.plan_format), `${product.label}: the view must use the core format description`);
}
const expected = PRODUCTS.map((product) => product.id).sort();
check(JSON.stringify(samples.map((sample) => sample.product).sort()) === JSON.stringify(expected), "Every core product needs exactly one bundled sample");
check(JSON.stringify(SAMPLES.map((sample) => sample.id).sort()) === JSON.stringify(expected), "Plans sample controls must follow the core catalog");
for (const product of PRODUCTS) check(!!product.plan_format.trim(), `${product.label} needs a format description`);
for (const sample of samples) {
  const sidecar = shared.compares.find((entry) => entry.product === sample.product)!;
  check(!!sidecar, `${sample.product}: missing browser comparison`);
  const [a, b] = sample.plans;
  check(sameSamplePlan(a, sidecar.a as unknown as Plan) && sameSamplePlan(b, sidecar.b as unknown as Plan), `${sample.product}: browser and desktop samples differ`);
  check(!sameSamplePlan(a, b), `${sample.product}: sample plans must be distinct`);
  const clone = structuredClone(a);
  check(sameSamplePlan(a, clone), `${sample.product}: copied sample must retain its identity`);
  clone.root.op = "A different plan with the same cost";
  check(!sameSamplePlan(a, clone), `${sample.product}: equal costs must not identify a plan`);
  await trySample(sample.product);
  const state = getPlans();
  check(state.mode === "compare" && state.items.length === 2 && !state.busy && !state.error, `${sample.product}: sample did not open a comparison`);
  check(state.items.every((item) => item.origin === `Sample · ${PRODUCTS.find((product) => product.id === sample.product)!.label}`), `${sample.product}: wrong product label`);
  for (const item of state.items) removePlan(item.id);
}
const sqlite = samples.find((sample) => sample.product === "sqlite") as PlanSample;
check(sqlite.plans.every((plan) => plan.facts?.estimated_cost_unavailable), "SQLite plan costs must remain unavailable");
check(sqlite.plans[0].total_cost === sqlite.plans[1].total_cost && !sameSamplePlan(...sqlite.plans), "SQLite's absent costs must not collapse its comparison");
console.log(`Plan samples: ${samples.length} core products, native captures, standalone loading and cost collisions passed`);
