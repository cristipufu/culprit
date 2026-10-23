/** One owner per resource. Superseded requests may finish, but cannot publish into a newer request's state. */
export class LatestRequest {
  private current: symbol | null = null;

  start(): () => boolean {
    const token = Symbol();
    this.current = token;
    return () => this.current === token;
  }

  invalidate(): void {
    this.current = null;
  }
}

/** Share initialization and keep successful resources. A failed attempt can be retried by the next caller. */
export function lazyResource<T>(load: () => Promise<T>): () => Promise<T> {
  let current: Promise<T> | null = null;
  return () => {
    if (!current) {
      const pending = Promise.resolve().then(load);
      current = pending;
      void pending.catch(() => { if (current === pending) current = null; });
    }
    return current;
  };
}
