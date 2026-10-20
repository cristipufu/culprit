import { actualFacts } from "./actual";
import type { Plan } from "../gen/Plan";
import { nowTile } from "./dashboard";
import type { LiveState } from "../gen/LiveState";

const check = (condition: boolean, message: string) => { if (!condition) throw new Error(message); };
const plan = {
  engine: "postgres", kind: "actual", props: { MemoryGrantedKB: "1024", Triggers: JSON.stringify([
    { "Trigger Name": "audit", Relation: "items", Time: 120.5, Calls: 10 },
    { "Trigger Name": "invalid", Time: -1 },
  ]) }, total_cost: 1, warnings: [], root: {
    id: 1, op: "Update", kind: "modify", props: {}, warnings: [], children: [],
    est_rows: 1, est_executions: 1, self_cost: 1, subtree_cost: 1, act_rows: 1, act_time_ms: 10, act_executions: 1,
  },
} as unknown as Plan;
const facts = actualFacts(plan)!;
check(facts.usedKb === null, "Missing memory usage must remain unknown");
check(facts.grantedKb === 1024, "The known grant remains visible");
check(facts.triggers.length === 1 && facts.triggers[0]?.ms === 120.5, "Trigger timing remains separate and rejects invalid data");
check(facts.elapsedMs === 10, "Trigger timing must not be added to operator elapsed time");
const tile = nowTile({ taken_at: "2026-10-20T12:00:00Z", notes: ["Lock instrumentation unavailable"], sessions: [], blocking: null } as unknown as LiveState);
check(tile.status === "info" && tile.takeaway.includes("incomplete"), "An incomplete lock read cannot appear healthy");
check(tile.metrics.find((m) => m.key === "blocked")?.value === null, "An incomplete lock read cannot claim zero blocked sessions");
console.log("Actual-plan and incomplete-lock evidence regressions passed");
