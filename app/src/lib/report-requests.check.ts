import type { Report } from "../gen/Report";
import { ReportRequests, type ReportState } from "./report-requests";
import { LatestRequest, lazyResource } from "./requests";

function equal(actual: unknown, expected: unknown, why: string): void {
  if (JSON.stringify(actual) !== JSON.stringify(expected)) throw new Error(`${why}: ${JSON.stringify(actual)}`);
}
function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (error: unknown) => void;
  const promise = new Promise<T>((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}
const tick = () => new Promise<void>((resolve) => setTimeout(resolve, 0));
const report = (id: string) => ({ snapshot: { id, connection: "a" }, findings: [
  { id: "info", actionability: "information" }, { id: "issue", actionability: "needs-attention" },
] }) as unknown as Report;

type Client = { latestReport: (name: string, minutes: number | null) => Promise<Report | null>;
  scan: (name: string, minutes: number | null) => Promise<Report> };
function harness(client: () => Promise<Client>, collected: () => Promise<unknown> = async () => {}) {
  let state: { active: string | null; windowMinutes: number; report: ReportState; finding: string | null } = {
    active: "a", windowMinutes: 60, report: { status: "loading" }, finding: null,
  };
  const requests = new ReportRequests({ client, read: () => state,
    publish: (update) => { state = { ...state, ...update }; }, collected });
  return { requests, read: () => state,
    target(name: string, minutes = 60) {
      requests.invalidate();
      state = { ...state, active: name, windowMinutes: minutes, report: { status: "loading" }, finding: null };
    },
    seed(value: Report) { state.report = { status: "ready", report: value, scanning: false, scanError: null }; },
    select(id: string) { state.finding = id; },
    visible() {
      const value = state.report;
      return value.status === "ready" ? [value.report.snapshot.id, value.scanning, value.scanError?.message ?? null]
        : [value.status, value.status === "error" ? value.error.message : null];
    },
  };
}

// Re-entering the same connection must not revive its previous request, including stale failures.
for (const failOld of [false, true]) {
  const old = deferred<Report | null>(), other = deferred<Report | null>(), current = deferred<Report | null>();
  const queue = [old, other, current];
  const h = harness(async () => ({ latestReport: () => queue.shift()!.promise,
    scan: async () => { throw new Error("unexpected scan"); } }));
  const a = h.requests.open("a", 60, false); await tick();
  h.target("b"); const b = h.requests.open("b", 60, false); await tick();
  h.target("a"); const again = h.requests.open("a", 60, false); await tick();
  current.resolve(report("current")); await again;
  if (failOld) old.reject(new Error("old failure")); else old.resolve(report("old"));
  other.resolve(report("other")); await Promise.all([a, b]);
  equal(h.visible(), ["current", false, null], "A → B → A revived an old request");
}

// Window identity applies to errors as well as successful responses.
{
  const old = deferred<Report | null>();
  const h = harness(async () => ({ latestReport: async (_name, minutes) => minutes === 60 ? old.promise : report("6h"),
    scan: async () => report("unused") }));
  const pending = h.requests.open("a", 60, false); await tick();
  h.target("a", 360); await h.requests.open("a", 360, false);
  old.reject(new Error("old window failed")); await pending;
  equal(h.visible(), ["6h", false, null], "A stale window error replaced the selected report");
}

// Two scans of the same target finish independently; only the latest request owns the result and spinner.
for (const failOld of [false, true]) {
  const old = deferred<Report>(), current = deferred<Report>();
  const queue = [old, current];
  const h = harness(async () => ({ latestReport: async () => null, scan: () => queue.shift()!.promise }));
  h.seed(report("cached"));
  const first = h.requests.scan("a", 60); await tick();
  const second = h.requests.scan("a", 60); await tick();
  if (failOld) old.reject(new Error("old scan failed")); else old.resolve(report("old scan"));
  await first;
  equal(h.visible(), ["cached", true, null], "An old scan changed a newer scan's state");
  current.resolve(report("fresh")); await second;
  equal(h.visible(), ["fresh", false, null], "The current scan did not finish");
}

// A late cache cannot replace a manual refresh or launch another automatic scan.
{
  const cache = deferred<Report | null>(); let scans = 0;
  const h = harness(async () => ({ latestReport: () => cache.promise,
    scan: async () => { scans++; return report("manual"); } }));
  const opening = h.requests.open("a", 60, true); await tick();
  await h.requests.scan("a", 60);
  cache.resolve(report("cache")); await opening;
  equal(h.visible(), ["manual", false, null], "A late cache replaced a completed scan");
  equal(scans, 1, "A superseded cache launched another scan");
}

// A failed refresh retains usable cached data and the current selection; API bootstrap failures are handled too.
{
  const scan = deferred<Report>();
  const h = harness(async () => ({ latestReport: async () => report("cache"), scan: () => scan.promise }));
  const opening = h.requests.open("a", 60, true); await tick();
  equal(h.visible(), ["cache", true, null], "Cache was not shown during refresh");
  equal(h.read().finding, "issue", "An informational finding became the default selection");
  scan.reject(new Error("offline")); await opening;
  equal(h.visible(), ["cache", false, "offline"], "A refresh failure discarded usable cached data");
  const broken = harness(async () => { throw new Error("API bootstrap failed"); });
  await broken.requests.open("a", 60, false);
  equal(broken.visible(), ["error", "API bootstrap failed"], "API bootstrap rejection escaped the lifecycle");
}

// Ancillary refresh failures do not turn successful scans into failures, and stale views cannot initiate work.
{
  let scans = 0, refreshed = 0;
  const cache = deferred<Report | null>();
  const h = harness(async () => ({ latestReport: () => cache.promise,
    scan: async () => { scans++; return report("fresh"); } }), async () => { refreshed++; throw new Error("sidebar failed"); });
  const pending = h.requests.open("a", 60, false); await tick();
  await h.requests.scan("b", 60);
  cache.resolve(report("cache")); await pending;
  equal(h.visible(), ["cache", false, null], "An inactive view superseded the current request");
  equal(scans, 0, "An inactive view launched a scan");
  await h.requests.scan("a", 60); await tick();
  equal(h.visible(), ["fresh", false, null], "A sidebar failure invalidated a successful scan");
  equal(refreshed, 1, "Accepted scans did not refresh connection metadata");
}

{
  const owner = new LatestRequest(); const first = owner.start(), second = owner.start();
  equal([first(), second()], [false, true], "List request ownership is ambiguous");
  owner.invalidate(); equal(second(), false, "Invalidation revived an old owner");
}
console.log("Report requests: navigation, windows, overlapping scans, cache races and failures retain the current owner");

// API initialization is shared while pending, retryable after rejection, and cached after success.
{
  const initial = deferred<number>(); let attempts = 0;
  const resource = lazyResource(() => ++attempts === 1 ? initial.promise : Promise.resolve(42));
  const first = resource(), same = resource();
  if (first !== same) throw new Error("Concurrent API initialization was duplicated");
  initial.reject(new Error("temporary failure"));
  await Promise.allSettled([first, same]);
  equal(await resource(), 42, "API initialization could not recover after failure");
  equal(await resource(), 42, "Successful API initialization was not cached");
  equal(attempts, 2, "Resource initialization ran again after success");
}

// Navigation triggered by a state subscriber must retire the cache-to-scan continuation too.
{
  let scans = 0;
  let state: { active: string; windowMinutes: number; report: ReportState; finding: string | null } = {
    active: "a", windowMinutes: 60, report: { status: "loading" }, finding: null,
  };
  const requests = new ReportRequests({ read: () => state,
    client: async () => ({ latestReport: async () => report("cache"), scan: async () => { scans++; return report("fresh"); } }),
    publish: (update) => { state = { ...state, ...update }; requests.invalidate(); state.active = "b"; },
    collected: async () => {},
  });
  await requests.open("a", 60, true);
  equal(scans, 0, "A retired cache continuation started another scan");
}
console.log("API initialization retries after failure; retired cache continuations launch no scan");
