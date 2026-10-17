import type { Finding } from "../gen/Finding";
import type { FileIo } from "../gen/FileIo";
import type { Replica } from "../gen/Replica";
import type { Report } from "../gen/Report";
import type { QueryStat } from "../gen/QueryStat";
import { findingStatus, querySignalStatus, replicaStatus, storageStatus } from "./health";

const check = (condition: boolean, message: string) => { if (!condition) throw new Error(message); };
const finding = (extra: Partial<Finding>): Finding => ({
  id: "example", check: "example", severity: "low", inactive: false, queries: [], metrics: [],
  subject: { kind: "database", name: "example" }, ...extra,
} as Finding);
const report = (findings: Finding[]): Report => ({ findings } as Report);

// A tiny query can spend nearly all its time planning or in JIT without costing a meaningful amount of time.
const tiny = { id: "tiny", executions: 1000, total_time_ms: 1, costs: { jit_ms: 0.9, plan_ms: 10 } } as QueryStat;
check(querySignalStatus(report([]), tiny, ["jit-overhead"]) === "info", "A large ratio alone cannot warn");
check(querySignalStatus(report([finding({ check: "planning-time", severity: "high", inactive: true, queries: [{ id: "tiny", plan_node: null, note: null }] })]), tiny, ["planning-time"]) === "info", "Stopped findings cannot alarm current health");
check(querySignalStatus(report([finding({ check: "jit-overhead", severity: "medium", queries: [{ id: "tiny", plan_node: null, note: null }] })]), tiny, ["jit-overhead"]) === "watch", "A supported warning must remain visible");

// Bulk reads are a Low context finding; a file with the same name in another database is separate evidence.
const file = { name: "data", database: "shop", kind: "data", reads: 10000, read_ms: 1_000_000 } as FileIo;
const storage = finding({ id: "slow-storage:shop:data", check: "slow-storage", metrics: [{ label: "File", text: "data", value: 0, unit: "count" }] });
check(storageStatus(report([storage]), file) === "info", "Bulk or historical storage context cannot become a red disk warning");
check(storageStatus(report([{ ...storage, id: "slow-storage:other:data", severity: "high" }]), file) === "info", "Another database's matching filename cannot warn here");
check(storageStatus(report([{ ...storage, severity: "medium" }]), file) === "watch", "Assessed slow storage still warns");

// An inactive slot may be between syncs or bounded by a WAL cap; inactivity and byte count alone do not grade it.
const slot = { name: "batch_consumer", role: "slot", state: "inactive", lag_bytes: 2 * 1024 ** 3 } as const;
check(replicaStatus(report([]), slot as Replica) === "info", "An inactive slot alone cannot become an incident");
check(replicaStatus(report([finding({ check: "replica-lag", subject: { kind: "database", name: slot.name } })]), slot as Replica) === "info", "A bounded slot's Low finding stays informational");
check(findingStatus(finding({ severity: "high" })) === "bad", "Confirmed incidents retain their warning");
console.log("Health warning regressions passed");
