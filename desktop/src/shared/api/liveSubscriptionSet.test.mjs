/**
 * Unit tests for the shared live-subscription lifecycle.
 *
 * Everything the set touches is injected — a fake `open` whose promises the
 * test settles by hand, a fake timer host, a fake clock — so the races that
 * make this code hard (an open that resolves after its key was dropped, a
 * second opener starting while the first is in flight, a retry landing after
 * teardown) are deterministic rather than timing-dependent.
 */

import assert from "node:assert/strict";
import test from "node:test";

import {
  createLiveSubscriptionSet,
  DEFAULT_LIVE_SUBSCRIPTION_RETRY,
  liveSubscriptionRetryDelayMs,
} from "./liveSubscriptionSet.ts";

// ── Fakes ────────────────────────────────────────────────────────────────────

/**
 * Fake `subscribeLive`. Every call is recorded and left pending until the test
 * resolves or rejects it, which is what makes mid-open cancellation testable.
 */
function createFakeRelay({ autoResolve = true } = {}) {
  const calls = [];

  const open = (request, onEvent) =>
    new Promise((resolve, reject) => {
      const call = {
        request,
        onEvent,
        disposeCount: 0,
        resolve: () => resolve(call.dispose),
        reject: (error) => reject(error ?? new Error("relay is down")),
      };
      call.dispose = async () => {
        call.disposeCount += 1;
      };
      calls.push(call);
      if (autoResolve) call.resolve();
    });

  return {
    calls,
    open,
    /** Requests in call order, for asserting filter shape. */
    requests: () => calls.map((call) => call.request),
    /** Calls whose dispose was invoked at least once. */
    disposed: () => calls.filter((call) => call.disposeCount > 0),
    last: () => calls[calls.length - 1],
  };
}

/** Fake timer host: nothing fires until the test fires it. */
function createFakeHost() {
  const timers = new Map();
  let nextId = 0;

  return {
    setTimeout: (handler, ms) => {
      const id = ++nextId;
      timers.set(id, { handler, ms });
      return id;
    },
    clearTimeout: (id) => {
      timers.delete(id);
    },
    /** Delays of all armed timers, in arm order. */
    delays: () => [...timers.values()].map((timer) => timer.ms),
    /** Fire every armed timer once. */
    fire: () => {
      const armed = [...timers.values()];
      timers.clear();
      for (const timer of armed) timer.handler();
    },
  };
}

function createReconnects() {
  const listeners = new Set();
  return {
    subscribeToReconnects: (listener) => {
      listeners.add(listener);
      return () => listeners.delete(listener);
    },
    emit: () => {
      for (const listener of [...listeners]) listener();
    },
    size: () => listeners.size,
  };
}

/** A set whose keys map to two filters, in the shape the projects code uses. */
function pairSet(relay, host, overrides = {}) {
  return createLiveSubscriptionSet({
    open: relay.open,
    buildGroup: (key, { nowSeconds }) => [
      { kinds: [1], "#e": [key], limit: 100, since: nowSeconds },
      { kinds: [1619], "#E": [key], limit: 100, since: nowSeconds },
    ],
    onEvent: () => {},
    onError: () => {},
    host,
    now: () => 1_000_000_000_000,
    ...overrides,
  });
}

/** A set whose keys map to a single filter, in the shape the channel code uses. */
function singleFilterSet(relay, host, overrides = {}) {
  return createLiveSubscriptionSet({
    open: relay.open,
    buildGroup: (key, { sinceSeconds }) => [
      { kinds: [9], "#h": [key], limit: 1000, since: sinceSeconds },
    ],
    groupOpenPolicy: "perFilter",
    onEvent: () => {},
    onError: () => {},
    host,
    now: () => 1_000_000_000_000,
    ...overrides,
  });
}

// ── Retry schedule ───────────────────────────────────────────────────────────

test("retry delay doubles from the base and stops at the cap", () => {
  const policy = DEFAULT_LIVE_SUBSCRIPTION_RETRY;
  const schedule = [0, 1, 2, 3, 4, 5, 6, 7].map((attempt) =>
    liveSubscriptionRetryDelayMs(attempt, policy),
  );

  assert.deepEqual(
    schedule,
    [1_000, 2_000, 4_000, 8_000, 16_000, 30_000, 30_000, 30_000],
    "the schedule the three folded consumers already shipped",
  );
});

test("retry delay stays finite for an absurd attempt count", () => {
  // 2 ** 5000 is Infinity; the exponent clamp is what keeps the cap meaningful.
  assert.equal(
    liveSubscriptionRetryDelayMs(5_000, DEFAULT_LIVE_SUBSCRIPTION_RETRY),
    30_000,
  );
});

// ── Atomic group opens ───────────────────────────────────────────────────────

test("atomic group opens every filter with one shared since", async () => {
  const relay = createFakeRelay();
  const host = createFakeHost();
  const set = pairSet(relay, host);

  set.setKeys(["root-1"]);
  await set.whenIdle();

  assert.equal(relay.calls.length, 2);
  const [first, second] = relay.requests();
  assert.deepEqual(first["#e"], ["root-1"]);
  assert.deepEqual(second["#E"], ["root-1"]);
  assert.equal(
    first.since,
    second.since,
    "both halves of one grammar must cover the same window",
  );
  assert.deepEqual(set.getOpenKeys(), ["root-1"]);
});

test("atomic group discards the half that opened when the other fails", async () => {
  const relay = createFakeRelay({ autoResolve: false });
  const host = createFakeHost();
  const errors = [];
  const set = pairSet(relay, host, {
    retry: DEFAULT_LIVE_SUBSCRIPTION_RETRY,
    onError: (error, key) => errors.push([String(error), key]),
  });

  set.setKeys(["root-1"]);
  relay.calls[0].resolve();
  relay.calls[1].reject(new Error("REQ rejected"));
  await set.whenIdle();

  assert.equal(relay.calls[0].disposeCount, 1, "the opened half is closed");
  assert.deepEqual(set.getOpenKeys(), [], "the group is left closed");
  assert.deepEqual(errors, [["Error: REQ rejected", "root-1"]]);
  assert.deepEqual(host.delays(), [1_000], "the pair is retried together");
});

test("atomic retry reopens the pair and stops once it succeeds", async () => {
  const relay = createFakeRelay({ autoResolve: false });
  const host = createFakeHost();
  const set = pairSet(relay, host, { retry: DEFAULT_LIVE_SUBSCRIPTION_RETRY });

  set.setKeys(["root-1"]);
  relay.calls[0].reject();
  relay.calls[1].reject();
  await set.whenIdle();
  assert.deepEqual(host.delays(), [1_000]);

  host.fire();
  relay.calls[2].reject();
  relay.calls[3].reject();
  await set.whenIdle();
  assert.deepEqual(
    host.delays(),
    [2_000],
    "backoff advances on repeat failure",
  );

  host.fire();
  relay.calls[4].resolve();
  relay.calls[5].resolve();
  await set.whenIdle();
  assert.deepEqual(host.delays(), [], "success stops the retry loop");
  assert.deepEqual(set.getOpenKeys(), ["root-1"]);

  // A later failure starts from the base delay again, not from where it left.
  set.setKeys([]);
  set.setKeys(["root-2"]);
  relay.calls[6].reject();
  relay.calls[7].reject();
  await set.whenIdle();
  assert.deepEqual(host.delays(), [1_000]);
});

test("atomic group is not reopened while its first open is still in flight", async () => {
  const relay = createFakeRelay({ autoResolve: false });
  const host = createFakeHost();
  const set = pairSet(relay, host);

  set.setKeys(["root-1"]);
  assert.equal(relay.calls.length, 2);

  // A second reconcile for the same key while the pair is opening must not
  // open a duplicate REQ — `disposers` is still empty during that window.
  set.setKeys(["root-1"]);
  assert.equal(relay.calls.length, 2, "no duplicate REQ for an opening group");

  relay.calls[0].resolve();
  relay.calls[1].resolve();
  await set.whenIdle();
  assert.equal(relay.calls.length, 2);
});

// ── Per-filter group opens ───────────────────────────────────────────────────

test("perFilter keeps the filter that opened and reopens only the failure", async () => {
  const relay = createFakeRelay({ autoResolve: false });
  const host = createFakeHost();
  const reconnects = createReconnects();
  const set = pairSet(relay, host, {
    groupOpenPolicy: "perFilter",
    reconnect: {
      strategy: "repairFailedOnly",
      subscribeToReconnects: reconnects.subscribeToReconnects,
    },
  });

  set.setKeys(["root-1"]);
  relay.calls[0].resolve();
  relay.calls[1].reject();
  await set.whenIdle();

  assert.equal(relay.calls[0].disposeCount, 0, "the healthy filter stays open");
  assert.deepEqual(set.getOpenKeys(), ["root-1"]);

  reconnects.emit();
  relay.calls[2].resolve();
  await set.whenIdle();

  assert.equal(relay.calls.length, 3, "only the failed filter is re-sent");
  assert.deepEqual(
    relay.calls[2].request["#E"],
    ["root-1"],
    "and it is the one that failed, not the one the session already replays",
  );
});

test("perFilter set that is fully open does not rebuild filters on re-sync", async () => {
  const relay = createFakeRelay();
  const host = createFakeHost();
  let builds = 0;
  const set = singleFilterSet(relay, host, {
    buildGroup: (key, { sinceSeconds }) => {
      builds += 1;
      return [{ kinds: [9], "#h": [key], limit: 1000, since: sinceSeconds }];
    },
  });

  set.setKeys(["a"]);
  await set.whenIdle();
  assert.equal(builds, 1);

  set.setKeys(["a"]);
  await set.whenIdle();
  assert.equal(builds, 1, "an already-open key costs nothing to re-sync");
  assert.equal(relay.calls.length, 1);
});

// ── Key-set diffing ──────────────────────────────────────────────────────────

test("diff disposes removed keys, leaves kept keys alone, opens added keys", async () => {
  const relay = createFakeRelay();
  const host = createFakeHost();
  const set = singleFilterSet(relay, host);

  set.setKeys(["a", "b"]);
  await set.whenIdle();
  assert.deepEqual(
    relay.requests().map((request) => request["#h"][0]),
    ["a", "b"],
  );

  set.setKeys(["b", "c"]);
  await set.whenIdle();

  assert.equal(relay.calls[0].disposeCount, 1, "a is disposed");
  assert.equal(relay.calls[1].disposeCount, 0, "b is untouched");
  assert.equal(relay.calls.length, 3, "only c is opened");
  assert.deepEqual(relay.calls[2].request["#h"], ["c"]);
  assert.deepEqual(set.getOpenKeys(), ["b", "c"]);
});

test("duplicate keys collapse to one subscription", async () => {
  const relay = createFakeRelay();
  const host = createFakeHost();
  const set = singleFilterSet(relay, host);

  set.setKeys(["a", "a", "a"]);
  await set.whenIdle();

  assert.equal(relay.calls.length, 1);
});

test("onBeforeOpen runs on every pass, after removals and before opens", async () => {
  const relay = createFakeRelay({ autoResolve: false });
  const host = createFakeHost();
  const passes = [];
  const set = singleFilterSet(relay, host, {
    retry: DEFAULT_LIVE_SUBSCRIPTION_RETRY,
    onBeforeOpen: (keys, { nowSeconds }) => {
      passes.push({ keys: [...keys], nowSeconds, opens: relay.calls.length });
    },
  });

  set.setKeys(["a"]);
  relay.calls[0].reject();
  await set.whenIdle();
  host.fire();
  relay.calls[1].resolve();
  await set.whenIdle();

  assert.deepEqual(
    passes.map((pass) => pass.keys),
    [["a"], ["a"]],
    "the retry pass reports the target set too",
  );
  assert.deepEqual(
    passes.map((pass) => pass.opens),
    [0, 1],
    "it runs before the pass opens anything",
  );
  assert.equal(passes[0].nowSeconds, 1_000_000_000);
});

test("sinceOverlapSecs reaches back over the fetch/subscribe seam", async () => {
  const relay = createFakeRelay();
  const host = createFakeHost();
  const set = singleFilterSet(relay, host, { sinceOverlapSecs: 30 });

  set.setKeys(["a"]);
  await set.whenIdle();

  assert.equal(relay.calls[0].request.since, 1_000_000_000 - 30);
});

// ── Reconnect strategies ─────────────────────────────────────────────────────

test("repairFailedOnly re-sends nothing when every subscription is open", async () => {
  const relay = createFakeRelay();
  const host = createFakeHost();
  const reconnects = createReconnects();
  const set = pairSet(relay, host, {
    reconnect: {
      strategy: "repairFailedOnly",
      subscribeToReconnects: reconnects.subscribeToReconnects,
    },
  });

  set.setKeys(["root-1"]);
  await set.whenIdle();
  assert.equal(relay.calls.length, 2);

  reconnects.emit();
  await set.whenIdle();

  assert.equal(
    relay.calls.length,
    2,
    "the session replays accepted REQs itself; re-sending would double-deliver",
  );
});

test("repairFailedOnly reopens a group that never opened, skipping the backoff", async () => {
  const relay = createFakeRelay({ autoResolve: false });
  const host = createFakeHost();
  const reconnects = createReconnects();
  const set = pairSet(relay, host, {
    retry: DEFAULT_LIVE_SUBSCRIPTION_RETRY,
    reconnect: {
      strategy: "repairFailedOnly",
      subscribeToReconnects: reconnects.subscribeToReconnects,
    },
  });

  set.setKeys(["root-1"]);
  relay.calls[0].reject();
  relay.calls[1].reject();
  await set.whenIdle();
  assert.deepEqual(host.delays(), [1_000]);

  reconnects.emit();
  relay.calls[2].resolve();
  relay.calls[3].resolve();
  await set.whenIdle();

  assert.equal(relay.calls.length, 4);
  assert.deepEqual(
    host.delays(),
    [],
    "the superseded retry timer is dropped, not left to re-run the pass",
  );
  assert.deepEqual(set.getOpenKeys(), ["root-1"]);
});

test("resubscribeAll closes everything and opens it again", async () => {
  const relay = createFakeRelay();
  const host = createFakeHost();
  const reconnects = createReconnects();
  const set = pairSet(relay, host, {
    reconnect: {
      strategy: "resubscribeAll",
      subscribeToReconnects: reconnects.subscribeToReconnects,
    },
  });

  set.setKeys(["root-1"]);
  await set.whenIdle();

  reconnects.emit();
  await set.whenIdle();

  assert.equal(relay.calls[0].disposeCount, 1);
  assert.equal(relay.calls[1].disposeCount, 1);
  assert.equal(relay.calls.length, 4, "the pair is re-opened");
});

test("custom strategy leaves subscriptions alone and only runs the hook", async () => {
  const relay = createFakeRelay();
  const host = createFakeHost();
  const reconnects = createReconnects();
  const seen = [];
  const set = pairSet(relay, host, {
    reconnect: {
      strategy: "custom",
      subscribeToReconnects: reconnects.subscribeToReconnects,
      onReconnect: () => seen.push("reconnect"),
    },
  });

  set.setKeys(["root-1"]);
  await set.whenIdle();
  reconnects.emit();
  await set.whenIdle();

  assert.deepEqual(seen, ["reconnect"]);
  assert.equal(relay.calls.length, 2, "the set itself does nothing");
});

test("onReconnect runs before the repair pass", async () => {
  const relay = createFakeRelay({ autoResolve: false });
  const host = createFakeHost();
  const reconnects = createReconnects();
  const order = [];
  const set = pairSet(relay, host, {
    reconnect: {
      strategy: "repairFailedOnly",
      subscribeToReconnects: reconnects.subscribeToReconnects,
      onReconnect: () => order.push(`hook@${relay.calls.length}`),
    },
  });

  set.setKeys(["root-1"]);
  relay.calls[0].reject();
  relay.calls[1].reject();
  await set.whenIdle();

  reconnects.emit();
  assert.deepEqual(order, ["hook@2"], "the refetch is scheduled first");
  relay.calls[2].resolve();
  relay.calls[3].resolve();
  await set.whenIdle();
});

test("dispose unsubscribes the reconnect listener", async () => {
  const relay = createFakeRelay();
  const host = createFakeHost();
  const reconnects = createReconnects();
  const set = pairSet(relay, host, {
    reconnect: {
      strategy: "repairFailedOnly",
      subscribeToReconnects: reconnects.subscribeToReconnects,
    },
  });

  assert.equal(reconnects.size(), 1);
  set.setKeys(["root-1"]);
  await set.dispose();
  assert.equal(reconnects.size(), 0);

  reconnects.emit();
  await set.whenIdle();
  assert.equal(relay.calls.length, 2, "no pass runs after teardown");
});

// ── Cancellation ─────────────────────────────────────────────────────────────

test("a handle that resolves after its key was dropped is disposed, not stored", async () => {
  const relay = createFakeRelay({ autoResolve: false });
  const host = createFakeHost();
  const set = singleFilterSet(relay, host);

  set.setKeys(["a"]);
  assert.equal(relay.calls.length, 1);

  set.setKeys([]);
  relay.calls[0].resolve();
  await set.whenIdle();

  assert.equal(relay.calls[0].disposeCount, 1, "the late handle closes itself");
  assert.deepEqual(set.getOpenKeys(), []);
});

test("a key removed and re-added mid-open does not adopt the stale handle", async () => {
  const relay = createFakeRelay({ autoResolve: false });
  const host = createFakeHost();
  const set = singleFilterSet(relay, host);

  set.setKeys(["a"]);
  set.setKeys([]);
  set.setKeys(["a"]);
  assert.equal(relay.calls.length, 2, "the re-add opens a fresh subscription");

  relay.calls[0].resolve();
  relay.calls[1].resolve();
  await set.whenIdle();

  assert.equal(relay.calls[0].disposeCount, 1, "the retired handle is closed");
  assert.equal(relay.calls[1].disposeCount, 0, "the fresh one is kept");
  assert.deepEqual(set.getOpenKeys(), ["a"]);
});

test("dispose closes handles that were still opening", async () => {
  const relay = createFakeRelay({ autoResolve: false });
  const host = createFakeHost();
  const set = pairSet(relay, host);

  set.setKeys(["root-1"]);
  const disposal = set.dispose();
  relay.calls[0].resolve();
  relay.calls[1].resolve();
  await disposal;

  assert.equal(relay.disposed().length, 2);
  assert.deepEqual(set.getOpenKeys(), []);
});

test("dispose closes open handles and refuses later reconciles", async () => {
  const relay = createFakeRelay();
  const host = createFakeHost();
  const set = singleFilterSet(relay, host);

  set.setKeys(["a", "b"]);
  await set.whenIdle();
  await set.dispose();

  assert.equal(relay.disposed().length, 2);

  set.setKeys(["c"]);
  await set.whenIdle();
  assert.equal(relay.calls.length, 2, "a disposed set stays closed");
});

test("a pending retry timer is cleared by dispose", async () => {
  const relay = createFakeRelay({ autoResolve: false });
  const host = createFakeHost();
  const set = singleFilterSet(relay, host, {
    retry: DEFAULT_LIVE_SUBSCRIPTION_RETRY,
  });

  set.setKeys(["a"]);
  relay.calls[0].reject();
  await set.whenIdle();
  assert.deepEqual(host.delays(), [1_000]);

  await set.dispose();
  assert.deepEqual(host.delays(), []);
  host.fire();
  await set.whenIdle();
  assert.equal(relay.calls.length, 1);
});

test("events stop being delivered once the set is disposed", async () => {
  const relay = createFakeRelay();
  const host = createFakeHost();
  const delivered = [];
  const set = singleFilterSet(relay, host, {
    onEvent: (event, key) => delivered.push([event.id, key]),
  });

  set.setKeys(["a"]);
  await set.whenIdle();
  relay.calls[0].onEvent({ id: "e1" });
  await set.dispose();
  // The CLOSE is in flight for as long as the relay takes to acknowledge it.
  relay.calls[0].onEvent({ id: "e2" });

  assert.deepEqual(delivered, [["e1", "a"]]);
});

// ── Dedupe ───────────────────────────────────────────────────────────────────

test("dedupeById drops an event delivered through both filters of a group", async () => {
  const relay = createFakeRelay();
  const host = createFakeHost();
  const delivered = [];
  const set = pairSet(relay, host, {
    dedupeById: true,
    onEvent: (event) => delivered.push(event.id),
  });

  set.setKeys(["root-1"]);
  await set.whenIdle();

  relay.calls[0].onEvent({ id: "e1" });
  relay.calls[1].onEvent({ id: "e1" });
  relay.calls[1].onEvent({ id: "e2" });

  assert.deepEqual(delivered, ["e1", "e2"]);
});

test("the dedupe guard is bounded and evicts the oldest id", async () => {
  const relay = createFakeRelay();
  const host = createFakeHost();
  const delivered = [];
  const set = singleFilterSet(relay, host, {
    dedupeById: true,
    dedupeLimit: 2,
    onEvent: (event) => delivered.push(event.id),
  });

  set.setKeys(["a"]);
  await set.whenIdle();

  const deliver = relay.calls[0].onEvent;
  deliver({ id: "e1" });
  deliver({ id: "e2" });
  deliver({ id: "e3" });
  // e1 has been evicted by now, so a replay of it is delivered again.
  deliver({ id: "e1" });

  assert.deepEqual(delivered, ["e1", "e2", "e3", "e1"]);
});

// ── Debounced rebuild ────────────────────────────────────────────────────────

test("rebuild debounce coalesces a burst of key changes into one pass", async () => {
  const relay = createFakeRelay();
  const host = createFakeHost();
  const set = singleFilterSet(relay, host, { rebuildDebounceMs: 750 });

  set.setKeys(["a"]);
  set.setKeys(["a", "b"]);
  set.setKeys(["c"]);
  assert.equal(relay.calls.length, 0, "nothing opens during the quiet window");
  assert.deepEqual(host.delays(), [750]);

  host.fire();
  await set.whenIdle();

  assert.equal(relay.calls.length, 1, "only the last key set is opened");
  assert.deepEqual(relay.calls[0].request["#h"], ["c"]);
});

test("a reconnect repair skips the rebuild debounce", async () => {
  const relay = createFakeRelay({ autoResolve: false });
  const host = createFakeHost();
  const reconnects = createReconnects();
  const set = singleFilterSet(relay, host, {
    rebuildDebounceMs: 750,
    reconnect: {
      strategy: "repairFailedOnly",
      subscribeToReconnects: reconnects.subscribeToReconnects,
    },
  });

  set.setKeys(["a"]);
  host.fire();
  relay.calls[0].reject();
  await set.whenIdle();

  reconnects.emit();
  assert.equal(relay.calls.length, 2, "repair is immediate, not debounced");
  relay.calls[1].resolve();
  await set.whenIdle();
  assert.deepEqual(set.getOpenKeys(), ["a"]);
});

test("dispose cancels a pending debounced rebuild", async () => {
  const relay = createFakeRelay();
  const host = createFakeHost();
  const set = singleFilterSet(relay, host, { rebuildDebounceMs: 750 });

  set.setKeys(["a"]);
  await set.dispose();
  host.fire();
  await set.whenIdle();

  assert.equal(relay.calls.length, 0);
});
