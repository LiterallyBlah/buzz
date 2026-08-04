import type { RelaySubscriptionFilter } from "@/shared/api/relayClientShared";
import type { RelayEvent } from "@/shared/api/types";
import { createTrailingDebounce } from "@/shared/lib/trailingDebounce";

/**
 * A keyed set of live relay subscriptions, reconciled against a target key set.
 *
 * Three features grew their own copy of this: the channel timeline
 * (`useLiveChannelUpdates`), the issue/PR detail panel (`useLiveProjectRoot`),
 * and the project notification watcher (`projectNotificationsLive`). They
 * differed in filter shape and in two policies — whether a group's filters are
 * all-or-none, and what a reconnect should re-send — but every one of them had
 * hand-rolled the same four hard parts, each slightly differently:
 *
 * - **Cancellation-safe opens.** `subscribeLive` is async, so between the call
 *   and its resolution the key may have been dropped, the consumer unmounted,
 *   or a second opener started. A handle that resolves into a torn-down set is
 *   a leaked REQ that keeps delivering events to a dead closure, and a second
 *   opener that starts while the first is in flight opens a duplicate REQ that
 *   double-delivers every event. Both are guarded here, once: opens are
 *   tracked per (key, request index) while in flight, and every continuation
 *   re-checks the record it captured before storing its handle.
 * - **Set diffing.** Keys that stay are left strictly alone — re-opening a
 *   subscription that is already live costs a REQ round trip and a backlog
 *   replay, and a set that churns on every render never converges.
 * - **Retry with backoff.** A relay that is down at mount rejects every open;
 *   without a backoff the retry loop hammers it, and without a cap it gives up
 *   in practice.
 * - **Reconnect repair.** See `LiveReconnectStrategy` — this is the policy that
 *   is easiest to get wrong and most expensive when wrong.
 *
 * Nothing here knows about React, the relay client singleton, or any query
 * cache: `open`, `subscribeToReconnects`, the clock and the timer host are all
 * injected, so the whole lifecycle is unit-testable against a fake relay.
 */

/** Closes one relay subscription. Matches `relayClient.subscribeLive`. */
export type LiveSubscriptionDispose = () => Promise<void>;

/**
 * Opens one subscription and resolves with its disposer.
 *
 * Injected rather than imported so the set never reaches for the relay
 * singleton — and so a consumer whose subscriptions are not plain filters
 * (channel mentions are opened through a purpose-built client method) can use
 * the same lifecycle by choosing its own request type.
 */
export type LiveSubscriptionOpen<TRequest> = (
  request: TRequest,
  onEvent: (event: RelayEvent) => void,
) => Promise<LiveSubscriptionDispose>;

/**
 * The time bounds a sync pass hands to `buildGroup`.
 *
 * One clock read per pass, shared by every filter the pass opens: a group whose
 * filters are two halves of one grammar (`#e` comments and `#E` revisions of
 * the same root) must not straddle a second boundary, or the two halves cover
 * different windows and the seam between them is a hole.
 *
 * `sinceSeconds` applies the configured overlap. Overlap is the cure for the
 * gap between "the query that filled the view fetched" and "the subscription
 * opened": an event published in that window is missed by both unless the
 * subscription reaches back over it. It is only safe for consumers whose merge
 * is keyed by event id, which is why it is opt-in per set rather than assumed.
 */
export type LiveFilterWindow = {
  /** Whole seconds since the epoch, read once at the start of the pass. */
  nowSeconds: number;
  /** `nowSeconds` minus the set's configured overlap. */
  sinceSeconds: number;
};

/**
 * How the filters of one key are opened.
 *
 * - `atomic` — all-or-none. Used when the filters are halves of one grammar and
 *   half of it is not usable: a watcher holding only the `#e` filter silently
 *   never sees pull-request revisions, which looks exactly like "nothing is
 *   happening". On any failure the ones that did open are closed again and the
 *   whole group is retried together.
 * - `perFilter` — each filter stands alone; successes are kept and only the
 *   failed indices are re-opened later. Used when a filter that opened is
 *   already doing useful work, and when re-opening it would double-deliver.
 */
export type LiveGroupOpenPolicy = "atomic" | "perFilter";

/**
 * What a relay reconnect should re-send.
 *
 * - `repairFailedOnly` — re-open only what is not currently open. This is the
 *   correct default against this relay session: it replays the REQs it
 *   accepted itself (see `relayReconnectReplay`), so a blanket resubscribe
 *   would open a second REQ for every filter that was already healthy and
 *   deliver every subsequent event twice. What the session cannot replay is a
 *   REQ it never accepted — the ones that rejected because the relay was down
 *   when the set was built — and those are exactly the ones missing here.
 * - `resubscribeAll` — drop everything and re-open. Only correct against a
 *   transport that does *not* replay; kept explicit so the choice is visible
 *   rather than implied by whichever hook was copied last.
 * - `custom` — the set does nothing; `onReconnect` owns the response.
 *
 * `onReconnect` fires before the strategy runs in all three cases, for the
 * side effects that are not subscription management (a refetch to close the
 * gap for events published while the socket was down, which no replay covers).
 */
export type LiveReconnectStrategy =
  | "repairFailedOnly"
  | "resubscribeAll"
  | "custom";

export type LiveReconnectOptions = {
  strategy: LiveReconnectStrategy;
  /** Injected `relayClient.subscribeToReconnects`; returns an unsubscribe. */
  subscribeToReconnects: (listener: () => void) => () => void;
  /** Runs on every reconnect, before the strategy. */
  onReconnect?: () => void;
};

/** Exponential backoff bounds for failed opens. */
export type LiveSubscriptionRetryPolicy = {
  baseMs: number;
  maxMs: number;
};

/**
 * The schedule every copy of this had converged on: 1s, 2s, 4s … 30s.
 *
 * Jitterless on purpose. These sets are per-window, not per-user, so there is
 * no thundering herd to spread; a predictable schedule is easier to reason
 * about in a log and is what the existing consumers already shipped.
 */
export const DEFAULT_LIVE_SUBSCRIPTION_RETRY: LiveSubscriptionRetryPolicy = {
  baseMs: 1_000,
  maxMs: 30_000,
};

/**
 * Clamp on the exponent, not on the delay.
 *
 * `2 ** attempt` reaches Infinity around attempt 1024, and `Math.min` with a
 * cap hides that only as long as the arithmetic stays finite. Clamping the
 * exponent keeps the multiplication in range no matter how long a relay stays
 * down; the delay it produces is identical, because the cap dominates long
 * before the clamp is reached.
 */
const RETRY_EXPONENT_CEILING = 30;

/** Delay before retry number `attempt` (0-based). */
export function liveSubscriptionRetryDelayMs(
  attempt: number,
  policy: LiveSubscriptionRetryPolicy,
): number {
  const exponent = Math.min(Math.max(attempt, 0), RETRY_EXPONENT_CEILING);
  return Math.min(policy.baseMs * 2 ** exponent, policy.maxMs);
}

/** Default cap for the optional event-id dedupe guard. */
const DEFAULT_DEDUPE_LIMIT = 5_000;

export type LiveSubscriptionTimerHost = {
  setTimeout: (handler: () => void, ms: number) => number;
  clearTimeout: (id: number) => void;
};

export type LiveSubscriptionSetOptions<TRequest = RelaySubscriptionFilter> = {
  /**
   * The filters for one key, rebuilt on every pass that has something to open
   * so `since` is always current — a retry five minutes later must not
   * subscribe from the window the first attempt would have used.
   */
  buildGroup: (
    key: string,
    filterWindow: LiveFilterWindow,
  ) => readonly TRequest[];
  open: LiveSubscriptionOpen<TRequest>;
  /** Receives every event with the key whose subscription delivered it. */
  onEvent: (event: RelayEvent, key: string) => void;
  /** Defaults to `atomic` — the safer of the two when filters are paired. */
  groupOpenPolicy?: LiveGroupOpenPolicy;
  /** Omit to never retry on a timer (repair on reconnect only). */
  retry?: LiveSubscriptionRetryPolicy;
  /** Omit to ignore reconnects entirely. */
  reconnect?: LiveReconnectOptions;
  /** Seconds `sinceSeconds` reaches back before `nowSeconds`. Default 0. */
  sinceOverlapSecs?: number;
  /**
   * Drop events whose id was already delivered through this set.
   *
   * Off by default: a consumer whose dedupe decision is fused with its notify
   * decision must keep owning it, or ids get recorded for events that were
   * never surfaced and the guard starts suppressing the wrong things.
   */
  dedupeById?: boolean;
  /** Bound on the dedupe guard. Oldest ids are evicted first. */
  dedupeLimit?: number;
  /**
   * Quiet window before a `setKeys` call is acted on. Default 0 (immediate).
   *
   * For consumers whose key set is derived from a cache that is written in
   * bursts: rebuilding per write tears down and re-opens REQs that the next
   * write would have asked for again.
   */
  rebuildDebounceMs?: number;
  /**
   * Runs at the start of every sync pass — including retries — after removals
   * and before any open, with the pass's target keys and time window.
   *
   * Exists for state that must be stamped with the moment the subscription
   * window opens (suppressing replayed backlog by `created_at`), which is only
   * knowable here: it has to move on every re-open, not just the first.
   */
  onBeforeOpen?: (
    keys: readonly string[],
    filterWindow: LiveFilterWindow,
  ) => void;
  /** Called once per failed open. Defaults to a console.error. */
  onError?: (error: unknown, key: string) => void;
  host?: LiveSubscriptionTimerHost;
  /** Epoch milliseconds. Injected for tests. */
  now?: () => number;
};

export type LiveSubscriptionSet = {
  /**
   * Reconcile against a new target key set: keys that left are disposed, keys
   * that stayed are untouched, keys that arrived are opened. Cancels a pending
   * retry and resets the backoff — an explicit reconcile supersedes it.
   *
   * Call it from an effect keyed on a primitive id string, not on every
   * render: each call runs a pass.
   */
  setKeys: (keys: Iterable<string>) => void;
  /** Keys with at least one open subscription. */
  getOpenKeys: () => string[];
  /** Resolves when no sync pass is in flight. */
  whenIdle: () => Promise<void>;
  /**
   * Stop delivering, close everything, and stay closed. Handles from opens
   * still in flight are disposed as they resolve.
   */
  dispose: () => Promise<void>;
};

type GroupRecord = {
  /** Open handles by request index; a missing index is not open. */
  disposers: Map<number, LiveSubscriptionDispose>;
  /** Request indices with an `open()` in flight — the duplicate-REQ guard. */
  opening: Set<number>;
  /** Request count from the most recent build; -1 before the first build. */
  size: number;
  /**
   * Set when the key leaves the target set (or the whole set is disposed).
   *
   * The record, not the key, is the cancellation token: a key that is removed
   * and re-added gets a fresh record, so a handle from the old one still
   * resolves into a retired record and disposes itself instead of landing in
   * the new group.
   */
  retired: boolean;
};

function disposeQuietly(dispose: LiveSubscriptionDispose): Promise<void> {
  return dispose().catch(() => {});
}

export function createLiveSubscriptionSet<TRequest = RelaySubscriptionFilter>(
  options: LiveSubscriptionSetOptions<TRequest>,
): LiveSubscriptionSet {
  const {
    buildGroup,
    open,
    onEvent,
    groupOpenPolicy = "atomic",
    retry,
    reconnect,
    sinceOverlapSecs = 0,
    dedupeById = false,
    dedupeLimit = DEFAULT_DEDUPE_LIMIT,
    rebuildDebounceMs = 0,
    onBeforeOpen,
    onError,
    host = window,
    now = Date.now,
  } = options;

  const groups = new Map<string, GroupRecord>();
  const inFlightPasses = new Set<Promise<void>>();
  const seenEventIds = new Set<string>();
  let targetKeys: string[] = [];
  let disposed = false;
  let retryAttempt = 0;
  let retryTimeout: number | undefined;

  const reportError = (error: unknown, key: string) => {
    if (onError) {
      onError(error, key);
      return;
    }
    console.error("Failed to open live subscription", key, error);
  };

  const cancelRetry = () => {
    if (retryTimeout !== undefined) {
      host.clearTimeout(retryTimeout);
      retryTimeout = undefined;
    }
  };

  const retireGroup = (group: GroupRecord) => {
    group.retired = true;
    const handles = [...group.disposers.values()];
    group.disposers.clear();
    for (const dispose of handles) {
      void disposeQuietly(dispose);
    }
  };

  /**
   * Deliver to the consumer, unless the set is torn down.
   *
   * Post-dispose suppression is the set's job because dispose is async: the
   * CLOSE is in flight for as long as the relay takes to acknowledge it, and
   * anything that arrives in that window would otherwise reach a callback
   * whose surrounding state (query client, refs) is already gone.
   */
  const deliver = (key: string) => (event: RelayEvent) => {
    if (disposed) return;
    if (dedupeById) {
      if (seenEventIds.has(event.id)) return;
      seenEventIds.add(event.id);
      if (seenEventIds.size > dedupeLimit) {
        const oldest = seenEventIds.values().next().value;
        if (oldest !== undefined) seenEventIds.delete(oldest);
      }
    }
    onEvent(event, key);
  };

  /** True when the handle must be thrown away instead of recorded. */
  const isStale = (key: string, group: GroupRecord) =>
    disposed || group.retired || groups.get(key) !== group;

  const openAtomic = async (
    key: string,
    group: GroupRecord,
    requests: readonly TRequest[],
  ): Promise<boolean> => {
    for (let index = 0; index < requests.length; index += 1) {
      group.opening.add(index);
    }

    const results = await Promise.allSettled(
      requests.map((request) => open(request, deliver(key))),
    );
    group.opening.clear();

    // Handles keep their request index so a group whose records survive is
    // indexed the same way `perFilter` indexes it.
    const opened: Array<[number, LiveSubscriptionDispose]> = [];
    let failed = false;
    for (const [index, result] of results.entries()) {
      if (result.status === "fulfilled") {
        opened.push([index, result.value]);
      } else {
        failed = true;
        reportError(result.reason, key);
      }
    }

    if (isStale(key, group)) {
      for (const [, dispose] of opened) void disposeQuietly(dispose);
      // Not a failure: nobody is waiting on this key any more, so retrying it
      // would re-open a subscription the consumer just asked to be rid of.
      return true;
    }

    if (failed) {
      // Partial success is not usable — drop what opened and retry the group
      // as a unit, so the two halves always share one `since`.
      for (const [, dispose] of opened) void disposeQuietly(dispose);
      return false;
    }

    for (const [index, dispose] of opened) group.disposers.set(index, dispose);
    return true;
  };

  const openPerFilter = async (
    key: string,
    group: GroupRecord,
    requests: readonly TRequest[],
  ): Promise<boolean> => {
    let failed = false;

    await Promise.allSettled(
      requests.map(async (request, index) => {
        if (group.disposers.has(index) || group.opening.has(index)) return;
        group.opening.add(index);
        try {
          const dispose = await open(request, deliver(key));
          if (isStale(key, group)) {
            void disposeQuietly(dispose);
            return;
          }
          group.disposers.set(index, dispose);
        } catch (error) {
          failed = true;
          reportError(error, key);
        } finally {
          group.opening.delete(index);
        }
      }),
    );

    return !failed;
  };

  const openGroup = async (
    key: string,
    filterWindow: LiveFilterWindow,
  ): Promise<boolean> => {
    let group = groups.get(key);
    if (group === undefined) {
      group = {
        disposers: new Map(),
        opening: new Set(),
        size: -1,
        retired: false,
      };
      groups.set(key, group);
    }

    if (groupOpenPolicy === "atomic") {
      // Whole, or on its way to whole. An in-flight open is as good as an open
      // one here: it is the pass that started it that owns the outcome,
      // including scheduling the retry if it fails.
      if (group.disposers.size > 0 || group.opening.size > 0) return true;
    } else if (
      group.size >= 0 &&
      group.disposers.size + group.opening.size >= group.size
    ) {
      // Nothing missing. Skipping the build matters: consumers re-sync on
      // every effect run, and rebuilding filters for a set that is already
      // fully open is pure allocation.
      return true;
    }

    const requests = buildGroup(key, filterWindow);
    group.size = requests.length;
    if (requests.length === 0) return true;

    return groupOpenPolicy === "atomic"
      ? openAtomic(key, group, requests)
      : openPerFilter(key, group, requests);
  };

  /** One reconciliation. Resolves false when anything failed to open. */
  const runSyncPass = async (): Promise<boolean> => {
    if (disposed) return true;

    // Snapshot the target once: `setKeys` can land while this pass is awaiting
    // an open, and a pass that diffed against one set and opened against
    // another is how keys get lost.
    const keys = targetKeys;
    const wanted = new Set(keys);

    for (const [key, group] of [...groups]) {
      if (!wanted.has(key)) {
        groups.delete(key);
        retireGroup(group);
      }
    }

    const nowSeconds = Math.floor(now() / 1_000);
    const filterWindow: LiveFilterWindow = {
      nowSeconds,
      sinceSeconds: nowSeconds - sinceOverlapSecs,
    };
    onBeforeOpen?.(keys, filterWindow);

    let ok = true;
    await Promise.allSettled(
      keys.map(async (key) => {
        try {
          if (!(await openGroup(key, filterWindow))) ok = false;
        } catch (error) {
          // A throwing buildGroup would otherwise wedge the whole set with an
          // unhandled rejection; treat it as a failed open and let backoff run.
          ok = false;
          reportError(error, key);
        }
      }),
    );

    return ok;
  };

  const sync = (): Promise<void> => {
    const pass = (async () => {
      let ok = false;
      try {
        ok = await runSyncPass();
      } catch (error) {
        // Nothing above should reject — the per-key opens are already
        // isolated — but a pass that escapes as an unhandled rejection would
        // take the retry loop down with it, so treat it as a failed pass.
        reportError(error, "");
      }
      if (disposed) return;
      if (ok) {
        retryAttempt = 0;
        return;
      }
      if (!retry) return;
      const delayMs = liveSubscriptionRetryDelayMs(retryAttempt, retry);
      retryAttempt += 1;
      cancelRetry();
      retryTimeout = host.setTimeout(() => {
        retryTimeout = undefined;
        void sync();
      }, delayMs);
    })();

    inFlightPasses.add(pass);
    void pass.finally(() => {
      inFlightPasses.delete(pass);
    });
    return pass;
  };

  const rebuild =
    rebuildDebounceMs > 0
      ? createTrailingDebounce(
          () => {
            void sync();
          },
          rebuildDebounceMs,
          host,
        )
      : null;

  const handleReconnect = () => {
    if (disposed || !reconnect) return;
    reconnect.onReconnect?.();
    if (reconnect.strategy === "custom") return;

    if (reconnect.strategy === "resubscribeAll") {
      for (const group of groups.values()) {
        retireGroup(group);
      }
      groups.clear();
    }

    // A reconnect is the cheapest evidence the transport is healthy again, so
    // it short-circuits the backoff instead of waiting out a 30s window. The
    // pending timer is dropped: this pass supersedes it, and letting it fire
    // later would only re-run a reconciliation that already happened.
    cancelRetry();
    retryAttempt = 0;
    void sync();
  };

  const unsubscribeReconnects = reconnect
    ? reconnect.subscribeToReconnects(handleReconnect)
    : null;

  const whenIdle = async () => {
    while (inFlightPasses.size > 0) {
      await Promise.allSettled([...inFlightPasses]);
    }
  };

  return {
    setKeys: (keys) => {
      if (disposed) return;
      targetKeys = [...new Set(keys)];
      cancelRetry();
      retryAttempt = 0;
      if (rebuild) {
        rebuild.trigger();
        return;
      }
      void sync();
    },
    getOpenKeys: () =>
      [...groups]
        .filter(([, group]) => group.disposers.size > 0)
        .map(([key]) => key),
    whenIdle,
    dispose: async () => {
      if (disposed) return;
      disposed = true;
      targetKeys = [];
      cancelRetry();
      rebuild?.cancel();
      unsubscribeReconnects?.();

      const handles: LiveSubscriptionDispose[] = [];
      for (const group of groups.values()) {
        group.retired = true;
        handles.push(...group.disposers.values());
        group.disposers.clear();
      }
      groups.clear();

      // Wait on the in-flight passes too: their opens resolve into a disposed
      // set and close themselves, and a caller that awaits dispose() is
      // entitled to expect no REQ outlives it.
      await Promise.allSettled([
        ...handles.map((dispose) => disposeQuietly(dispose)),
        whenIdle(),
      ]);
    },
  };
}
