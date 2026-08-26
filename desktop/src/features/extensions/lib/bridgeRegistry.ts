/**
 * Per-port request bookkeeping: admission, correlation, and teardown.
 *
 * One registry per `MessagePort`. It owns the answer to three questions the
 * dispatcher must not answer twice: may this request be admitted, has this id
 * been used before, and has this request already reached a terminal state.
 *
 * # Every accepted request reaches exactly one terminal state
 *
 * Replied, or settled at teardown. Suppressing a write is **not** the same as
 * settling a request: the earlier spine simply dropped the reply when disposed,
 * which leaves the extension's promise pending for the lifetime of the frame.
 * §9 requires in-flight requests to be torn down, and a caller that never hears
 * back cannot distinguish "closed" from "still working".
 *
 * # Dedup spans the port's whole life, not just the in-flight window
 *
 * An id that has completed must not be reusable. Once effectful methods land,
 * a replayed id is a replayed effect, and a dedup window that only covers
 * concurrent requests would let the replay through as soon as the first
 * completed. So ids are retained for the life of the port.
 *
 * Memory is bounded by a **finite per-port request budget**, not by eviction.
 * An LRU would evict old ids back into validity, which is precisely the
 * property being defended. When the budget is spent the port is done and the
 * frame must renew it.
 */

/** §8 codes this module produces. */
const QUOTA_EXCEEDED = "quota_exceeded";
const RATE_LIMITED = "rate_limited";
const INVALID_PARAMS = "invalid_params";
const INTERNAL = "internal";

/**
 * Most requests one port may ever admit.
 *
 * Bounds the retained id set. Generous for a UI surface — an extension issuing
 * one request a second would take five hours to reach it — and reaching it is
 * not fatal: the frame renews the port.
 */
export const MAX_REQUESTS_PER_PORT = 20_000;

/**
 * Most requests that may be outstanding at once.
 *
 * Each dispatched call can open and configure a SQLite connection host-side.
 * Bounding frame size without bounding concurrency closes one tap and leaves
 * the bath running.
 */
export const MAX_IN_FLIGHT = 32;

export type Admission =
  | { kind: "admitted" }
  | { kind: "refused"; code: string; message: string };

export type Registry = {
  /** Reserve an id, or explain why not. */
  readonly admit: (id: string) => Admission;
  /**
   * Mark a request terminal. Returns true only for the first call, so a late
   * completion after teardown cannot emit a second result.
   */
  readonly settle: (id: string) => boolean;
  /**
   * Stop admitting, and hand back every request still outstanding so the
   * caller can answer them. Each returned id is already marked terminal.
   */
  readonly closeAndDrain: () => string[];
  /** Test/introspection: how many requests are outstanding. */
  readonly inFlight: () => number;
};

export const TEARDOWN_ERROR = {
  code: INTERNAL,
  message: "the extension frame closed before this request completed",
} as const;

export function createRegistry(): Registry {
  /** Ids ever admitted on this port — never evicted while the port lives. */
  const used = new Set<string>();
  /** Ids admitted and not yet terminal. */
  const outstanding = new Set<string>();
  let admitting = true;

  return {
    admit(id: string): Admission {
      if (!admitting) {
        // Closed ports admit nothing. Reached only if a frame arrives between
        // teardown and the listener being removed.
        return { kind: "refused", ...TEARDOWN_ERROR };
      }
      if (used.has(id)) {
        // Covers both an active duplicate and a replay of a completed id.
        return {
          kind: "refused",
          code: INVALID_PARAMS,
          message: "this request id has already been used on this port",
        };
      }
      if (used.size >= MAX_REQUESTS_PER_PORT) {
        return {
          kind: "refused",
          code: QUOTA_EXCEEDED,
          message: "this connection has reached its request budget",
        };
      }
      if (outstanding.size >= MAX_IN_FLIGHT) {
        return {
          kind: "refused",
          code: RATE_LIMITED,
          message: "too many requests are already in flight",
        };
      }
      used.add(id);
      outstanding.add(id);
      return { kind: "admitted" };
    },

    settle(id: string): boolean {
      return outstanding.delete(id);
    },

    closeAndDrain(): string[] {
      // Order matters and is the atomicity this needs: admission stops before
      // anything is drained, so a request cannot be admitted into a set that
      // has already been walked and would never be settled.
      admitting = false;
      const draining = [...outstanding];
      outstanding.clear();
      return draining;
    },

    inFlight(): number {
      return outstanding.size;
    },
  };
}
