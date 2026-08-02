import assert from "node:assert/strict";
import test, { mock } from "node:test";

import { subscribeToProjectActivity } from "./projectActivityRelay.ts";
import { relayClient } from "./relayClient.ts";

// The whole point of the subscription is that it is scoped to one root. A
// filter on the repository coordinate instead would light up every issue in the
// repo whenever any one of them is busy — confidently wrong on every issue but
// the one that is actually working, which is worse than showing nothing.
test("subscribeToProjectActivity scopes to the root and not the repository", () => {
  const calls = [];
  const restore = mock.method(relayClient, "subscribeLive", (filter) => {
    calls.push(filter);
    return Promise.resolve(() => {});
  });

  try {
    void subscribeToProjectActivity("c".repeat(64), () => {});
  } finally {
    restore.mock.restore();
  }

  assert.equal(calls.length, 1);
  const filter = calls[0];
  assert.deepEqual(filter.kinds, [20003]);
  assert.deepEqual(filter["#e"], ["c".repeat(64)]);
  assert.equal(
    filter["#a"],
    undefined,
    "scoping by repository shows every issue as busy",
  );
  assert.equal(filter["#h"], undefined, "an issue is not a channel");
  // Ephemeral events are not stored, so the lookback only covers reconnect
  // replay. A zero limit would make a reconnect mid-turn silently lose the
  // indicator until the next refresh tick.
  assert.ok(filter.limit > 0, "reconnect replay needs a non-zero limit");
  assert.ok(typeof filter.since === "number");
});
