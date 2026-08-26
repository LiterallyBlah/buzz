import assert from "node:assert/strict";
import test from "node:test";

import {
  createRegistry,
  MAX_IN_FLIGHT,
  MAX_REQUESTS_PER_PORT,
} from "./bridgeRegistry.ts";

const id = (n) => `3f2504e0-4f89-41d3-9a0c-${String(n).padStart(12, "0")}`;

test("a fresh id is admitted", () => {
  const registry = createRegistry();
  assert.equal(registry.admit(id(1)).kind, "admitted");
  assert.equal(registry.inFlight(), 1);
});

test("an active duplicate id is refused", () => {
  const registry = createRegistry();
  registry.admit(id(1));
  const second = registry.admit(id(1));
  assert.equal(second.kind, "refused");
  assert.equal(second.code, "invalid_params");
});

test("a completed id cannot be reused for the life of the port", () => {
  // The property an active-window-only dedup would miss. Once effectful
  // methods land, a replayed id is a replayed effect, and a window that only
  // covers concurrent requests lets the replay through the moment the first
  // one finishes.
  const registry = createRegistry();
  registry.admit(id(1));
  assert.equal(registry.settle(id(1)), true);
  assert.equal(registry.inFlight(), 0, "the request is no longer outstanding");

  const replay = registry.admit(id(1));
  assert.equal(replay.kind, "refused", "a completed id must not be reusable");
  assert.equal(replay.code, "invalid_params");
});

test("settle is terminal exactly once", () => {
  // What stops a late completion emitting a second result for one id.
  const registry = createRegistry();
  registry.admit(id(1));
  assert.equal(registry.settle(id(1)), true);
  assert.equal(registry.settle(id(1)), false);
  assert.equal(registry.settle(id(2)), false, "never admitted, never terminal");
});

test("the in-flight ceiling refuses excess with rate_limited", () => {
  const registry = createRegistry();
  for (let n = 0; n < MAX_IN_FLIGHT; n += 1) {
    assert.equal(registry.admit(id(n)).kind, "admitted");
  }
  const excess = registry.admit(id(MAX_IN_FLIGHT));
  assert.equal(excess.kind, "refused");
  assert.equal(excess.code, "rate_limited");

  // Transient: completing one frees a slot.
  registry.settle(id(0));
  assert.equal(registry.admit(id(MAX_IN_FLIGHT)).kind, "admitted");
});

test("the per-port budget refuses with quota_exceeded and never evicts", () => {
  // Bounded memory without an LRU. An LRU would evict old ids back into
  // validity, which is exactly the replay this defends against — so the port
  // is finite instead, and the frame renews it.
  const registry = createRegistry();
  for (let n = 0; n < MAX_REQUESTS_PER_PORT; n += 1) {
    assert.equal(registry.admit(id(n)).kind, "admitted", `request ${n}`);
    registry.settle(id(n));
  }
  const spent = registry.admit(id(MAX_REQUESTS_PER_PORT));
  assert.equal(spent.kind, "refused");
  assert.equal(spent.code, "quota_exceeded");

  // The very first id must still be refused as used, not forgotten.
  const earliest = registry.admit(id(0));
  assert.equal(earliest.kind, "refused");
  assert.equal(
    earliest.code,
    "invalid_params",
    "the oldest id must still read as used, not evicted back into validity",
  );
});

test("closing stops admission and hands back everything outstanding", () => {
  const registry = createRegistry();
  registry.admit(id(1));
  registry.admit(id(2));
  registry.admit(id(3));
  registry.settle(id(2));

  const drained = registry.closeAndDrain();
  assert.deepEqual(drained.sort(), [id(1), id(3)].sort());
  assert.equal(registry.inFlight(), 0);

  // Already terminal: the caller answers them, and a late completion finds
  // nothing left to settle.
  assert.equal(registry.settle(id(1)), false);

  const afterClose = registry.admit(id(4));
  assert.equal(afterClose.kind, "refused");
  assert.equal(afterClose.code, "internal");
});

test("draining twice yields nothing the second time", () => {
  const registry = createRegistry();
  registry.admit(id(1));
  assert.equal(registry.closeAndDrain().length, 1);
  assert.equal(registry.closeAndDrain().length, 0);
});
