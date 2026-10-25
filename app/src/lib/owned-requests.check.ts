import { discoveryRequests as cloudRequests } from "./discovery-requests";
import { ownedRequests } from "./owned-requests";

function check(ok: boolean, message: string) { if (!ok) throw new Error(message); }
function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (reason: unknown) => void;
  const promise = new Promise<T>((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}
async function until(ready: () => boolean) {
  for (let i = 0; i < 100; i++) { if (ready()) return; await Promise.resolve(); }
  throw new Error("Expected discovery transition did not happen");
}
type Call = { command: string; id: string };

for (const resource of ["discovery", "preview"]) {
const begin = `begin_${resource}`, cancel = `cancel_${resource}`;
const discoveryRequests = resource === "discovery" ? cloudRequests
  : (invoke: Parameters<typeof ownedRequests>[0]) => ownedRequests(invoke, begin, cancel);

// Cancellation during registration must be delivered after it completes, with no database search dispatched.
{
  const registration = deferred<void>();
  const calls: Call[] = [];
  const run = discoveryRequests(async <T>(command: string, args?: Record<string, unknown>) => {
    calls.push({ command, id: String(args?.requestId) });
    if (command === begin && calls.length === 1) await registration.promise;
    return "next result" as T;
  });
  const controller = new AbortController();
  const old = run("discover_azure", { terms: ["or-"] }, controller.signal).then(() => false, () => true);
  await until(() => calls.length === 1);
  controller.abort();
  registration.resolve();
  check(await old, "A search aborted during registration must reject");
  check(calls.length === 2 && calls[1]?.command === cancel && calls[0]?.id === calls[1]?.id, "Registration must be followed by cancellation, not dispatch");
  check(await run("discover_azure", { terms: ["rcs"] }) === "next result", "An aborted registration must not wedge the next search");
}

// Register in user order even when native IPC acknowledges the first registration late.
{
  const registration = deferred<void>();
  const oldResult = deferred<string>();
  const calls: Call[] = [];
  const run = discoveryRequests(async <T>(command: string, args?: Record<string, unknown>) => {
    calls.push({ command, id: String(args?.requestId) });
    if (command === begin && calls.length === 1) await registration.promise;
    if (command === "discover_azure") return await oldResult.promise as T;
    return "new AWS result" as T;
  });
  const old = run<string>("discover_azure", { terms: ["or-"] });
  await until(() => calls.length === 1);
  const next = run<string>("discover_aws", { terms: ["rcs"], regions: [] });
  await Promise.resolve();
  check(calls.length === 1, "A newer registration must wait for the prior registration acknowledgment");
  registration.resolve();
  check(await next === "new AWS result", "New searches must not wait for older cloud results");
  const begins = calls.filter((call) => call.command === begin);
  check(begins.length === 2 && begins[0]?.id !== begins[1]?.id, "Overlapping searches need distinct native owners");
  oldResult.resolve("stale Azure result");
  await old;
  check(calls.at(-1)?.command === cancel && calls.at(-1)?.id === begins[0]?.id, "A late completion must clean up its own owner only");
}

// An active native invocation receives abort, and its rejection cleans up without poisoning later registrations.
{
  const result = deferred<void>();
  const calls: Call[] = [];
  const run = discoveryRequests(async <T>(command: string, args?: Record<string, unknown>) => {
    calls.push({ command, id: String(args?.requestId) });
    if (command === "discover_azure") await result.promise;
    if (command === cancel) result.reject(new Error("canceled"));
    return undefined as T;
  });
  const controller = new AbortController();
  const active = run("discover_azure", {}, controller.signal).then(() => false, () => true);
  await until(() => calls.some((call) => call.command === "discover_azure"));
  controller.abort();
  check(await active, "Native cancellation must retire the in-flight invocation");
  check(calls.some((call) => call.command === cancel), "Abort must reach the native search owner");
  const before = calls.length;
  check(await run("discover_aws", {}) === undefined, "A canceled search must permit a later search");
  const alreadyAborted = new AbortController();
  alreadyAborted.abort();
  const after = calls.length;
  check(await run("discover_aws", {}, alreadyAborted.signal).then(() => false, () => true), "Already-aborted searches must reject");
  check(calls.length === after && after > before, "Already-aborted searches must start no native work");
}

// A failed acknowledgment can follow a successful native registration; retire that owner too.
{
  const calls: string[] = [];
  const run = discoveryRequests(async <T>(command: string) => {
    calls.push(command);
    if (command === begin && calls.length === 1) throw new Error("Lost acknowledgment");
    return 42 as T;
  });
  check(await run("preview_table", {}).then(() => false, () => true), "Registration failure must reject");
  check(calls.length === 2 && calls[1] === cancel, "A failed acknowledgment must retire its possible native owner");
  check(await run("preview_table", {}) === 42, "Registration failure must not wedge later requests");
}

// A backend result already in transit cannot turn an aborted invocation into success.
{
  const result = deferred<number>();
  let dispatched = false;
  const run = discoveryRequests(async <T>(command: string) => {
    if (command === "preview_table") { dispatched = true; return await result.promise as T; }
    return undefined as T;
  });
  const controller = new AbortController();
  const pending = run("preview_table", { name: "shop" }, controller.signal).then(() => false, (error: unknown) => error instanceof DOMException && error.name === "AbortError");
  await until(() => dispatched);
  controller.abort(); result.resolve(42);
  check(await pending, "A late success must not revive an aborted request");
}

}
console.log("Owned desktop requests: discovery and previews, ordered registration, overlapping work, abort races and retry passed");
