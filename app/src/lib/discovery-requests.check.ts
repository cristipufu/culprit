import { discoveryRequests } from "./discovery-requests";

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

// Cancellation during registration must be delivered after it completes, with no database search dispatched.
{
  const registration = deferred<void>();
  const calls: Call[] = [];
  const run = discoveryRequests(async <T>(command: string, args?: Record<string, unknown>) => {
    calls.push({ command, id: String(args?.requestId) });
    if (command === "begin_discovery" && calls.length === 1) await registration.promise;
    return "next result" as T;
  });
  const controller = new AbortController();
  const old = run("discover_azure", { terms: ["or-"] }, controller.signal).then(() => false, () => true);
  await until(() => calls.length === 1);
  controller.abort();
  registration.resolve();
  check(await old, "A search aborted during registration must reject");
  check(calls.length === 2 && calls[1]?.command === "cancel_discovery" && calls[0]?.id === calls[1]?.id, "Registration must be followed by cancellation, not dispatch");
  check(await run("discover_azure", { terms: ["rcs"] }) === "next result", "An aborted registration must not wedge the next search");
}

// Register in user order even when native IPC acknowledges the first registration late.
{
  const registration = deferred<void>();
  const oldResult = deferred<string>();
  const calls: Call[] = [];
  const run = discoveryRequests(async <T>(command: string, args?: Record<string, unknown>) => {
    calls.push({ command, id: String(args?.requestId) });
    if (command === "begin_discovery" && calls.length === 1) await registration.promise;
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
  const begins = calls.filter((call) => call.command === "begin_discovery");
  check(begins.length === 2 && begins[0]?.id !== begins[1]?.id, "Overlapping searches need distinct native owners");
  oldResult.resolve("stale Azure result");
  await old;
  check(calls.at(-1)?.command === "cancel_discovery" && calls.at(-1)?.id === begins[0]?.id, "A late completion must clean up its own owner only");
}

// An active native invocation receives abort, and its rejection cleans up without poisoning later registrations.
{
  const result = deferred<void>();
  const calls: Call[] = [];
  const run = discoveryRequests(async <T>(command: string, args?: Record<string, unknown>) => {
    calls.push({ command, id: String(args?.requestId) });
    if (command === "discover_azure") await result.promise;
    if (command === "cancel_discovery") result.reject(new Error("canceled"));
    return undefined as T;
  });
  const controller = new AbortController();
  const active = run("discover_azure", {}, controller.signal).then(() => false, () => true);
  await until(() => calls.some((call) => call.command === "discover_azure"));
  controller.abort();
  check(await active, "Native cancellation must retire the in-flight invocation");
  check(calls.some((call) => call.command === "cancel_discovery"), "Abort must reach the native search owner");
  const before = calls.length;
  check(await run("discover_aws", {}) === undefined, "A canceled search must permit a later search");
  const alreadyAborted = new AbortController();
  alreadyAborted.abort();
  const after = calls.length;
  check(await run("discover_aws", {}, alreadyAborted.signal).then(() => false, () => true), "Already-aborted searches must reject");
  check(calls.length === after && after > before, "Already-aborted searches must start no native work");
}

console.log("Discovery requests: ordered registration, overlapping clouds, abort races and retry passed");
