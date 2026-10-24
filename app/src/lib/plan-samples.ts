import type { Plan } from "../gen/Plan";
import type { PlanSample } from "../gen/PlanSample";
import { lazyResource } from "./requests";

/** Captured samples are loaded only when requested, in both desktop builds and the browser demo. */
export const planSamples = lazyResource(async (): Promise<PlanSample[]> =>
  (await import("../../../fixtures/plan-samples.json")).default as unknown as PlanSample[]);

/** Costs are not identities: SQLite has no cost, and distinct plans can have the same estimate. */
export function sameSamplePlan(a: Plan, b: Plan): boolean {
  return a === b || (a.engine === b.engine && JSON.stringify(a) === JSON.stringify(b));
}
