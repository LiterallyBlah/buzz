import assert from "node:assert/strict";
import { afterEach, describe, it, mock } from "node:test";

import { createLivenessMap, createLivenessStore } from "./livenessStore.ts";

describe("createLivenessMap", () => {
  const map = createLivenessMap({ ttlMs: 8_000 });

  it("keeps an entry until its deadline and drops it at the bound", () => {
    const state = map.upsert(
      map.empty,
      "a",
      { note: "typing" },
      { nowMs: 1000 },
    );
    assert.equal(map.live(state, 8_999).length, 1);
    assert.equal(
      map.live(state, 9_000).length,
      0,
      "at the deadline the entry is already gone",
    );
  });

  it("lets a frame's own clock shorten the window but never extend it", () => {
    // A frame that says it was true 6s ago has 2s of its TTL left, not 8 — the
    // window is evidence-bound.
    const stale = map.upsert(
      map.empty,
      "a",
      { note: "typing" },
      { nowMs: 10_000, frameAtMs: 4_000 },
    );
    assert.equal(map.live(stale, 11_999).length, 1);
    assert.equal(map.live(stale, 12_000).length, 0);

    // A frame stamped in the future is a clock disagreement, not extra life.
    const future = map.upsert(
      map.empty,
      "a",
      { note: "typing" },
      { nowMs: 10_000, frameAtMs: 90_000 },
    );
    assert.equal(map.live(future, 18_000).length, 0);
  });

  it("returns the same state when a prune finds nothing past due", () => {
    const state = map.upsert(map.empty, "a", { note: "x" }, { nowMs: 0 });
    assert.equal(map.prune(state, 1_000), state);
    assert.notEqual(map.prune(state, 9_000), state);
    assert.equal(map.size(map.prune(state, 9_000)), 0);
  });

  it("returns the same state when the supersede rule refuses a frame", () => {
    const ranked = createLivenessMap({
      ttlMs: 8_000,
      supersede: (existing, incoming) => incoming.rank <= existing.rank,
    });
    const first = ranked.upsert(ranked.empty, "a", { rank: 2 }, { nowMs: 0 });
    const refused = ranked.upsert(first, "a", { rank: 1 }, { nowMs: 1_000 });
    assert.equal(refused, first, "a refused frame must not re-render anything");
    assert.equal(ranked.get(refused, "a").rank, 2);

    const accepted = ranked.upsert(first, "a", { rank: 3 }, { nowMs: 1_000 });
    assert.notEqual(accepted, first);
    assert.equal(ranked.get(accepted, "a").rank, 3);
  });

  it("refuses to shorten a live entry when a late frame is accepted", () => {
    // Nothing in the value ordering says the *window* may walk backwards: a
    // frame delivered late still proves the scope was alive at delivery.
    const state = map.upsert(map.empty, "a", { note: "x" }, { nowMs: 10_000 });
    const late = map.upsert(state, "a", { note: "y" }, { nowMs: 4_000 });
    assert.equal(map.live(late, 17_999).length, 1);
  });

  it("treats a repeat frame as a refresh: deadline moves, identity does not", () => {
    const repeated = createLivenessMap({
      ttlMs: 8_000,
      sameValue: () => true,
    });
    const first = repeated.upsert(repeated.empty, "a", { n: 1 }, { nowMs: 0 });
    const again = repeated.upsert(first, "a", { n: 1 }, { nowMs: 4_000 });
    assert.equal(again, first, "a heartbeat is not a change");
    assert.equal(
      repeated.live(first, 11_000).length,
      1,
      "but it did push the deadline out",
    );
  });

  it("reports whether a refresh landed, without touching state identity", () => {
    const state = map.upsert(map.empty, "a", { note: "x" }, { nowMs: 0 });
    assert.equal(map.refresh(state, "missing", { nowMs: 1_000 }), false);
    assert.equal(map.refresh(state, "a", { nowMs: 4_000 }), true);
    assert.equal(map.live(state, 11_000).length, 1);
  });

  it("clears only the entry a terminal frame is actually about", () => {
    const state = map.upsert(map.empty, "a", { turn: "t2" }, { nowMs: 0 });
    const stale = map.drop(state, "a", (entry) => entry.turn === "t1");
    assert.equal(
      stale,
      state,
      "a terminal for a replaced turn changes nothing",
    );
    assert.equal(map.size(map.drop(state, "a", (e) => e.turn === "t2")), 0);
  });

  it("takes at most `limit` matches and reports which went", () => {
    let state = map.upsert(map.empty, "a", { channel: "c1" }, { nowMs: 0 });
    state = map.upsert(state, "b", { channel: "c1" }, { nowMs: 0 });
    state = map.upsert(state, "c", { channel: "c2" }, { nowMs: 0 });
    const result = map.take(state, (r) => r.value.channel === "c1", 1);
    assert.equal(result.taken.length, 1);
    assert.equal(map.size(result.state), 2);

    const none = map.take(state, (r) => r.value.channel === "c9");
    assert.equal(none.state, state, "no match must not copy the state");
  });

  it("orders snapshots by the configured comparator, first-seen surviving refreshes", () => {
    const ordered = createLivenessMap({
      ttlMs: 8_000,
      sameValue: () => true,
      compare: (a, b) => a.firstSeenAt - b.firstSeenAt,
    });
    let state = ordered.upsert(ordered.empty, "early", { n: 1 }, { nowMs: 0 });
    state = ordered.upsert(state, "late", { n: 2 }, { nowMs: 1_000 });
    // The earlier typist refreshes; it must not jump to the end of the list.
    state = ordered.upsert(state, "early", { n: 1 }, { nowMs: 2_000 });
    assert.deepEqual(
      ordered.list(state).map((entry) => entry.n),
      [1, 2],
    );
  });
});

describe("createLivenessStore", () => {
  /** A store with the agent-turns shape, minus everything agent-specific. */
  function makeStore(overrides = {}) {
    return createLivenessStore({
      groupOf: (value) => value.group,
      cadence: { kind: "adaptive", floorMs: 10_000, ceilingMs: 60_000 },
      expiryMultiplier: 2.5,
      pruneIntervalMs: 5_000,
      ...overrides,
    });
  }

  describe("snapshot stability", () => {
    it("hands back the same array until the entry set changes", () => {
      const store = makeStore();
      store.upsert("a|1", { group: "a", id: "1" }, { nowMs: 0 });

      const first = store.list();
      assert.equal(store.list(), first, "unchanged state must not re-allocate");
      assert.equal(store.listGroup("a"), store.listGroup("a"));

      store.refresh("a|1", { nowMs: 5_000 });
      assert.equal(
        store.list(),
        first,
        "a heartbeat changes nothing a subscriber can see",
      );

      store.upsert("a|2", { group: "a", id: "2" }, { nowMs: 5_000 });
      assert.notEqual(store.list(), first, "a new entry is a new snapshot");
    });

    it("gives an empty group a stable empty array", () => {
      const store = makeStore();
      assert.equal(store.listGroup("nobody"), store.listGroup("nobody"));
      assert.equal(store.listGroup("nobody").length, 0);
    });

    it("invalidates only the group that changed", () => {
      const invalidated = [];
      const store = makeStore({
        onInvalidate: (group) => invalidated.push(group),
      });
      store.upsert("a|1", { group: "a", id: "1" }, { nowMs: 0 });
      store.upsert("b|1", { group: "b", id: "1" }, { nowMs: 0 });
      const groupA = store.listGroup("a");
      invalidated.length = 0;

      store.upsert("b|2", { group: "b", id: "2" }, { nowMs: 0 });
      assert.deepEqual(invalidated, ["b"]);
      assert.equal(store.listGroup("a"), groupA);
    });
  });

  describe("observed cadence", () => {
    it("starts at the floor and widens once the producer proves it is slower", () => {
      // The deployed bug: a harness on BUZZ_ACP_TURN_LIVENESS_SECS=15 with one
      // dropped ping leaves a 30s hole. Against the assumed-10s window (25s)
      // that wipes a live badge; against the observed 15s window (37.5s) it
      // does not.
      const store = makeStore();
      assert.equal(store.cadenceMs("a"), 10_000, "no evidence yet: the floor");

      store.upsert("a|1", { group: "a", id: "1" }, { nowMs: 0 });
      for (const at of [0, 15_000, 30_000, 45_000]) {
        store.observeCadence("a", at);
        store.refresh("a|1", { nowMs: at });
      }
      assert.equal(store.cadenceMs("a"), 15_000);
      assert.equal(store.expiryMs("a"), 37_500);

      // One dropped ping: 30s of silence after the last frame at 45s.
      assert.equal(
        store.sweep(75_000),
        false,
        "one dropped ping is survivable",
      );
      assert.equal(store.size(), 1);
      assert.equal(store.sweep(82_500), true, "two dropped pings are not");
    });

    it("narrows again when the producer speeds back up", () => {
      const store = makeStore();
      for (const at of [0, 15_000, 30_000, 45_000]) {
        store.observeCadence("a", at);
      }
      assert.equal(store.cadenceMs("a"), 15_000);

      // The sample window is 5, so three 10s gaps outvote the two 15s ones.
      for (const at of [55_000, 65_000, 75_000]) {
        store.observeCadence("a", at);
      }
      assert.equal(store.cadenceMs("a"), 10_000);
      assert.equal(store.expiryMs("a"), 25_000);
    });

    it("outvotes a single dropped ping rather than inflating on it", () => {
      // A mean or a max would take the 30s outlier as the new cadence and keep
      // a genuinely dead turn on screen for multiples of the real interval.
      const store = makeStore();
      for (const at of [0, 15_000, 30_000, 60_000, 75_000, 90_000]) {
        store.observeCadence("a", at);
      }
      assert.equal(store.cadenceMs("a"), 15_000);
    });

    it("clamps to the floor and the ceiling", () => {
      const fast = makeStore();
      for (const at of [0, 500, 1_000, 1_500]) fast.observeCadence("a", at);
      assert.equal(fast.cadenceMs("a"), 10_000, "a burst cannot shrink it");

      const slow = makeStore();
      for (let at = 0; at <= 600_000; at += 120_000) {
        slow.observeCadence("a", at);
      }
      assert.equal(slow.cadenceMs("a"), 60_000, "silence cannot inflate it");
    });

    it("ignores frames that share a timestamp or arrive out of order", () => {
      const store = makeStore();
      store.observeCadence("a", 0);
      store.observeCadence("a", 0);
      store.observeCadence("a", 15_000);
      store.observeCadence("a", 5_000);
      store.observeCadence("a", 30_000);
      assert.equal(store.cadenceMs("a"), 15_000);
    });

    it("keeps one group's cadence out of another's", () => {
      const store = makeStore();
      for (const at of [0, 20_000, 40_000, 60_000]) {
        store.observeCadence("slow", at);
      }
      assert.equal(store.cadenceMs("slow"), 20_000);
      assert.equal(store.cadenceMs("brisk"), 10_000);
    });

    it("takes the interval as a constant when the cadence is fixed", () => {
      const store = createLivenessStore({
        cadence: { kind: "fixed", intervalMs: 45_000 },
        expiryMultiplier: 1,
        pruneIntervalMs: 2_000,
      });
      store.observeCadence("", 0);
      store.observeCadence("", 300_000);
      assert.equal(store.cadenceMs(""), 45_000, "a contract is not evidence");
    });
  });

  describe("prune pause", () => {
    const paused = () =>
      makeStore({ pause: { gapMultiplier: 2, maxMs: 60_000 } });

    it("still prunes a dead entry while a sibling in the group is fresh", () => {
      const store = paused();
      store.upsert("a|dead", { group: "a", id: "dead" }, { nowMs: 0 });
      store.upsert("a|live", { group: "a", id: "live" }, { nowMs: 0 });
      store.refresh("a|live", { nowMs: 30_000 });

      assert.equal(store.sweep(30_000), true);
      assert.deepEqual(
        store.list().map((entry) => entry.id),
        ["live"],
      );
    });

    it("pauses when the whole group goes quiet at once", () => {
      const store = paused();
      store.upsert("a|1", { group: "a", id: "1" }, { nowMs: 0 });
      store.upsert("a|2", { group: "a", id: "2" }, { nowMs: 0 });

      assert.equal(
        store.sweep(30_000),
        false,
        "the stream is down, not the work",
      );
      assert.equal(store.size(), 2);
    });

    it("gives up on the pause at its cap", () => {
      const store = paused();
      store.upsert("a|1", { group: "a", id: "1" }, { nowMs: 0 });
      assert.equal(store.sweep(59_999), false);
      assert.equal(store.sweep(60_000), true, "silence this long is death");
    });

    it("does not let one group's silence pause another", () => {
      const store = paused();
      store.upsert("quiet|1", { group: "quiet", id: "1" }, { nowMs: 0 });
      store.upsert("busy|1", { group: "busy", id: "1" }, { nowMs: 0 });
      store.refresh("busy|1", { nowMs: 30_000 });

      assert.equal(store.sweep(30_000), false, "the quiet group is paused");
      assert.equal(store.size(), 2);
      assert.equal(store.sweep(60_000), true);
      assert.deepEqual(
        store.list().map((entry) => entry.group),
        ["busy"],
      );
    });

    it("widens the pause window with the observed cadence too", () => {
      const store = paused();
      store.upsert("a|1", { group: "a", id: "1" }, { nowMs: 0 });
      for (const at of [0, 15_000, 30_000, 45_000]) {
        store.observeCadence("a", at);
      }
      // Silent for 25s: past the 20s gap the 10s floor would have used, but
      // short of the 30s the observed cadence justifies — so this is still a
      // plain expiry question, and 25s is inside the 37.5s window.
      assert.equal(store.sweep(25_000), false);
      assert.equal(store.size(), 1);
    });
  });

  describe("sweep scheduling", () => {
    afterEach(() => {
      mock.timers.reset();
    });

    it("holds no timer without both entries and a listener", () => {
      mock.timers.enable({ apis: ["setInterval", "Date"], now: 0 });
      const store = makeStore();

      store.upsert("a|1", { group: "a", id: "1" });
      assert.equal(store.isSweeping(), false, "nobody is watching");

      const unsubscribe = store.subscribe(() => {});
      assert.equal(store.isSweeping(), true);

      unsubscribe();
      assert.equal(store.isSweeping(), false);
    });

    it("starts on the first entry and stops when the last one expires", () => {
      mock.timers.enable({ apis: ["setInterval", "Date"], now: 0 });
      const store = makeStore();
      let notified = 0;
      store.subscribe(() => {
        notified += 1;
      });
      assert.equal(store.isSweeping(), false, "nothing to prune yet");

      store.upsert("a|1", { group: "a", id: "1" });
      assert.equal(store.isSweeping(), true);

      mock.timers.tick(20_000);
      assert.equal(store.size(), 1, "inside the window");
      assert.equal(notified, 0);

      mock.timers.tick(10_000);
      assert.equal(store.size(), 0, "past the window");
      assert.equal(notified, 1, "the sweep is the one mutator that broadcasts");
      assert.equal(store.isSweeping(), false, "no work left, no timer");
    });

    it("stops while suspended and shifts deadlines and cadence on resume", () => {
      mock.timers.enable({ apis: ["setInterval", "Date"], now: 15_000 });
      const store = makeStore();
      const unsubscribe = store.subscribe(() => {});
      store.upsert("a|1", { group: "a", id: "1" }, { nowMs: 0 });
      store.observeCadence("a", 0);
      store.observeCadence("a", 15_000);
      assert.equal(store.cadenceMs("a"), 15_000);
      assert.equal(store.isSweeping(), true);

      store.setSuspended(true, 15_000);
      assert.equal(store.isSuspended(), true);
      assert.equal(store.isSweeping(), false, "hidden stores hold no timer");
      assert.equal(store.sweep(315_000), false, "hidden time cannot prune");

      store.setSuspended(false, 315_000);
      assert.equal(store.isSuspended(), false);
      assert.equal(store.isSweeping(), true);
      assert.equal(store.sweep(337_499), false, "the deadline moved by 5m");
      assert.equal(store.sweep(337_500), true);

      // The last producer-frame clock moved by the same hidden interval, so a
      // normal 15s heartbeat after resume remains a 15s cadence sample.
      store.observeCadence("a", 330_000);
      assert.equal(store.cadenceMs("a"), 15_000);
      unsubscribe();
    });
  });

  describe("clearing", () => {
    it("drops a group's entries and forgets its cadence", () => {
      const store = makeStore();
      store.upsert("a|1", { group: "a", id: "1" }, { nowMs: 0 });
      store.upsert("b|1", { group: "b", id: "1" }, { nowMs: 0 });
      for (const at of [0, 15_000, 30_000, 45_000]) {
        store.observeCadence("a", at);
      }

      assert.equal(store.clearGroup("a"), true);
      assert.equal(store.groupSize("a"), 0);
      assert.equal(store.groupSize("b"), 1);
      assert.equal(
        store.cadenceMs("a"),
        10_000,
        "a restarted producer's old cadence is not evidence about the new one",
      );
      assert.equal(
        store.clearGroup("a"),
        false,
        "clearing nothing is no change",
      );

      store.clear();
      assert.equal(store.size(), 0);
    });
  });
});
