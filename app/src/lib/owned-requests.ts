type Invoke = <T>(command: string, args?: Record<string, unknown>) => Promise<T>;

/** Register in user order, independently of how long async invocations take to start or finish. */
export function ownedRequests(invoke: Invoke, begin: string, retire: string) {
  let registrations = Promise.resolve();
  return async <T>(command: string, args: Record<string, unknown>, signal?: AbortSignal): Promise<T> => {
    const requestId = crypto.getRandomValues(new Uint32Array(4)).join("-");
    const aborted = () => { if (signal?.aborted) throw new DOMException("Request canceled", "AbortError"); };
    const cancel = () => { void invoke(retire, { requestId }).catch(() => {}); };
    let registrationStarted = false;
    const registered = registrations.then(async () => {
      aborted();
      registrationStarted = true;
      await invoke(begin, { requestId });
    });
    registrations = registered.catch(() => {});
    try {
      await registered;
      // An abort during registration must cancel after registration, even if the earlier cancel arrived too soon.
      aborted();
      signal?.addEventListener("abort", cancel, { once: true });
      const result = await invoke<T>(command, { ...args, requestId });
      aborted();
      return result;
    } finally {
      signal?.removeEventListener("abort", cancel);
      if (registrationStarted) cancel();
    }
  };
}
