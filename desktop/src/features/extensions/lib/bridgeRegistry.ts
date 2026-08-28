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
 * property being defended.
 *
 * # Exhausting the budget ends the port, it does not throttle it
 *
 * An earlier revision returned `quota_exceeded` forever and claimed the frame
 * would "renew the port". No such path existed: §2 admits exactly one `ready`
 * per frame, so a successor port cannot be negotiated on a live frame, and the
 * handshake latches after the first one. That comment described code that was
 * never written.
 *
 * Rather than relax a §2 rule inside a hardening increment, exhaustion is now
 * **terminal**: the exhausting request is refused `quota_exceeded`, no further
 * request is admitted, and the owner tears the port down — settling anything
 * outstanding. Recovery is re-opening the extension frame, which mints a fresh
 * lease and a fresh port through the ordinary path. That is a deliberate
 * lifecycle contract, not silent permanent exhaustion.
 */

/** §8 codes this module produces. */
const QUOTA_EXCEEDED = "quota_exceeded";
const RATE_LIMITED = "rate_limited";
const INVALID_PARAMS = "invalid_params";
const INTERNAL = "internal";

/**
 * Most requests one port may ever admit.
 *
 * Bounds the retained id set, at roughly 80 bytes an id. Generous for a UI
 * surface — an extension issuing one request a second would take five and a
 * half hours to reach it — and reaching it ends the port rather than wedging
 * it, so the failure is observable instead of an endless refusal.
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
  | {
      kind: "refused";
      code: string;
      message: string;
      /** The port cannot serve again; the owner must tear it down. */
      terminal?: boolean;
    };

/**
 * Most live subscriptions one port may hold at once.
 *
 * Separate from the request budget because a stream is many frames per `sub`
 * rather than one settle per `id`. Bounding concurrency here is what stops an
 * extension opening subscriptions until the host runs out of branches.
 */
export const MAX_SUBS_PER_PORT = 64;

export type SubAdmission =
  | { kind: "opened"; sub: string }
  | { kind: "refused"; code: string; message: string };

export type Registry = {
  /** Reserve an id, or explain why not. */
  readonly admit: (id: string) => Admission;
  /**
   * Open a subscription slot and mint its **host-generated opaque** id.
   *
   * The extension never proposes a `sub`: an id it chose could collide with a
   * live one, or be guessed to probe another port. Ids are unique for the
   * owning port's life and **never reused after close**, so a late frame or a
   * stale `unsubscribe` for a dead sub can never bind to a new one.
   *
   * Consumes no request-id budget — streams must not burn
   * `MAX_REQUESTS_PER_PORT`.
   */
  readonly openSub: () => SubAdmission;
  /** Is this `sub` currently live on **this** port? */
  readonly isSubLive: (sub: string) => boolean;
  /**
   * Ensure a `sub` is not live. Returns whether it was.
   *
   * The boolean is for the host's own bookkeeping — releasing quota, emitting
   * a `closed` frame — and **must not** reach the extension: the reply to
   * `unsubscribe` is identical either way, so a caller cannot use it to learn
   * whether an id exists on another port.
   */
  readonly closeSub: (sub: string) => boolean;
  /** Live subs, for teardown. */
  readonly liveSubs: () => string[];
  /**
   * Stop admitting subs and hand back every live one, already marked closed,
   * so the caller can emit `closed` frames and release their quota.
   */
  readonly closeAndDrainSubs: () => string[];
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
  /** Has the port spent its budget and become unusable? */
  readonly isExhausted: () => boolean;
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
  /**
   * Every `sub` this port has ever minted — never evicted while the port
   * lives, so an id is not reissued after its subscription closes.
   */
  const usedSubs = new Set<string>();
  /** Subs currently live on this port. */
  const liveSubSet = new Set<string>();
  let admitting = true;
  let exhausted = false;

  const spent = (): Admission => ({
    kind: "refused",
    code: QUOTA_EXCEEDED,
    message: "this connection has reached its request budget",
    terminal: true,
  });

  return {
    admit(id: string): Admission {
      // Reported before the teardown arm so the reason stays accurate for any
      // request that races the teardown this very refusal triggers.
      if (exhausted) {
        return spent();
      }
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
        exhausted = true;
        return spent();
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

    openSub(): SubAdmission {
      if (!admitting) {
        return { kind: "refused", ...TEARDOWN_ERROR };
      }
      if (liveSubSet.size >= MAX_SUBS_PER_PORT) {
        return {
          kind: "refused",
          code: QUOTA_EXCEEDED,
          message: "too many live subscriptions on this connection",
        };
      }
      // Opaque and host-generated. `usedSubs` makes reuse impossible even
      // across a close, which is what stops a late frame for a dead sub
      // binding to a live one.
      //
      // A collision is refused rather than retried. Retrying reads as the
      // obvious fix and is wrong twice over: it is an unbounded loop inside a
      // module whose contract is that everything is bounded, and against a
      // degenerate or stubbed generator it never terminates. Refusing is
      // fail-closed, and it is reachable by a test, which a "cannot happen"
      // branch otherwise is not.
      const sub = crypto.randomUUID();
      if (usedSubs.has(sub)) {
        return {
          kind: "refused",
          code: INTERNAL,
          message: "could not mint a unique subscription id",
        };
      }
      usedSubs.add(sub);
      liveSubSet.add(sub);
      return { kind: "opened", sub };
    },

    isSubLive(sub: string): boolean {
      return liveSubSet.has(sub);
    },

    closeSub(sub: string): boolean {
      return liveSubSet.delete(sub);
    },

    liveSubs(): string[] {
      return [...liveSubSet];
    },

    closeAndDrainSubs(): string[] {
      // Same ordering as closeAndDrain: stop admitting before draining, so a
      // sub cannot be opened into a set that has already been walked and
      // would never be closed or have its quota released.
      admitting = false;
      const draining = [...liveSubSet];
      liveSubSet.clear();
      return draining;
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

    isExhausted(): boolean {
      return exhausted;
    },
  };
}
