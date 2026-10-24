type Invoke = <T>(command: string, args?: Record<string, unknown>) => Promise<T>;

/** Register searches in user order, independently of how long their async invocations take to start or finish. */
export function discoveryRequests(invoke: Invoke) {
  let registrations = Promise.resolve();
  return async <T>(command: string, args: Record<string, unknown>, signal?: AbortSignal): Promise<T> => {
    const requestId = crypto.getRandomValues(new Uint32Array(4)).join("-");
    const aborted = () => { if (signal?.aborted) throw new DOMException("Search canceled", "AbortError"); };
    const cancel = () => { void invoke("cancel_discovery", { requestId }).catch(() => {}); };
    const registered = registrations.then(async () => {
      aborted();
      await invoke("begin_discovery", { requestId });
    });
    registrations = registered.catch(() => {});
    await registered;
    try {
      // An abort during registration must cancel after registration, even if the earlier cancel arrived too soon.
      aborted();
      signal?.addEventListener("abort", cancel, { once: true });
      return await invoke<T>(command, { ...args, requestId });
    } finally {
      signal?.removeEventListener("abort", cancel);
      cancel();
    }
  };
}
