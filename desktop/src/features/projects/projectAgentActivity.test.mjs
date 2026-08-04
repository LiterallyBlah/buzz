import assert from "node:assert/strict";
import test from "node:test";

import {
  applyProjectActivity,
  EMPTY_PROJECT_ACTIVITY,
  liveProjectActivity,
  parseProjectActivity,
  PROJECT_ACTIVITY_STALE_MS,
} from "./projectAgentActivity.ts";

const AGENT = "a".repeat(64);
const OTHER_AGENT = "b".repeat(64);
const ROOT = "c".repeat(64);
const OTHER_ROOT = "d".repeat(64);
const COORDINATE = `30617:${"e".repeat(64)}:demo`;

function activityEvent(overrides = {}) {
  const {
    agent = AGENT,
    createdAt = 1_000,
    extraTags = [],
    root = ROOT,
    stage = null,
    state = "working",
    turn = "turn-1",
  } = overrides;
  return {
    id: "f".repeat(64),
    pubkey: overrides.pubkey ?? agent,
    created_at: createdAt,
    kind: 20003,
    content: "",
    sig: "0".repeat(128),
    tags: [
      ["a", COORDINATE],
      ["e", root, "", "root"],
      ["agent", agent],
      ["state", state],
      ["turn", turn],
      ...(stage ? [["stage", stage]] : []),
      ...extraTags,
    ],
  };
}

function fold(events, root = ROOT) {
  return events.reduce(
    (state, event) => applyProjectActivity(state, event, root),
    EMPTY_PROJECT_ACTIVITY,
  );
}

test("a working announcement puts the agent on its own root", () => {
  const state = fold([activityEvent({ stage: "reading files" })]);
  const live = liveProjectActivity(state, 1_000_000);
  assert.equal(live.length, 1);
  assert.equal(live[0].agent, AGENT);
  assert.equal(live[0].state, "working");
  assert.equal(live[0].stage, "reading files");
  assert.equal(live[0].turnId, "turn-1");
});

test("a queued announcement puts the agent on the root before any turn", () => {
  // The gap this closes: between the comment reaching the relay and the first
  // `working`, an agent that had picked the comment up and an agent that was
  // never addressed produced the identical wire — nothing.
  const state = fold([
    activityEvent({ state: "queued", turn: `queued:${"1".repeat(64)}` }),
  ]);
  const live = liveProjectActivity(state, 1_000_000);
  assert.equal(live.length, 1);
  assert.equal(live[0].state, "queued");
  assert.equal(
    live[0].stage,
    null,
    "nothing has started, so nothing to caption",
  );
});

test("the turn that starts replaces the queued announcement", () => {
  const state = fold([
    activityEvent({ state: "queued", turn: `queued:${"1".repeat(64)}` }),
    activityEvent({ createdAt: 1_001, stage: "starting", turn: "turn-1" }),
  ]);
  const live = liveProjectActivity(state, 1_001_000);
  assert.equal(live.length, 1, "one agent on one root is one entry");
  assert.equal(live[0].state, "working");
  assert.equal(live[0].turnId, "turn-1");
});

test("a reordered queued frame does not walk a working agent backwards", () => {
  // `created_at` is whole seconds and the queued frame and the turn's first
  // `working` routinely share one, so the relay can deliver them either way
  // round. The queued frame belongs to a different turn id, so the ordinary
  // same-turn guard cannot see the pair at all.
  const state = fold([
    activityEvent({ createdAt: 1_000, stage: "editing files", turn: "turn-1" }),
    activityEvent({
      createdAt: 1_000,
      state: "queued",
      turn: `queued:${"1".repeat(64)}`,
    }),
  ]);
  const live = liveProjectActivity(state, 1_000_000);
  assert.equal(live[0].state, "working");
  assert.equal(live[0].stage, "editing files");
});

test("an idle from the started turn clears a root that began as queued", () => {
  const state = fold([
    activityEvent({ state: "queued", turn: `queued:${"1".repeat(64)}` }),
    activityEvent({ createdAt: 1_001, turn: "turn-1" }),
    activityEvent({ createdAt: 1_002, state: "idle", turn: "turn-1" }),
  ]);
  assert.deepEqual(liveProjectActivity(state, 1_002_000), []);
});

test("a queued announcement expires on the same clock as a working one", () => {
  // The runtime re-announces `queued` on the same 15-second cadence for as long
  // as the comment is waiting, so an expired one means the runtime stopped
  // talking — not that the comment stopped waiting.
  const state = fold([
    activityEvent({
      createdAt: 1_000,
      state: "queued",
      turn: `queued:${"1".repeat(64)}`,
    }),
  ]);
  assert.equal(
    liveProjectActivity(state, 1_000 * 1_000 + PROJECT_ACTIVITY_STALE_MS - 1)
      .length,
    1,
  );
  assert.equal(
    liveProjectActivity(state, 1_000 * 1_000 + PROJECT_ACTIVITY_STALE_MS)
      .length,
    0,
  );
});

test("a state this build has never heard of is refused, not shown as working", () => {
  assert.equal(
    parseProjectActivity(activityEvent({ state: "deliberating" }), ROOT),
    null,
  );
});

test("activity for another root never reaches this one", () => {
  // The subscription is per root, but a relay is not an authority on what it
  // sends and a stale subscription can outlive the view that opened it.
  const state = fold([activityEvent({ root: OTHER_ROOT })]);
  assert.deepEqual(liveProjectActivity(state, 1_000_000), []);
});

test("an idle from the shown turn clears it", () => {
  const state = fold([activityEvent(), activityEvent({ state: "idle" })]);
  assert.deepEqual(liveProjectActivity(state, 1_000_000), []);
});

test("a stale idle from a finished turn does not clear the running one", () => {
  const state = fold([
    activityEvent({ turn: "turn-1" }),
    activityEvent({ state: "idle", turn: "turn-1" }),
    activityEvent({ createdAt: 2_000, turn: "turn-2" }),
    // The late terminal frame of the turn that already ended.
    activityEvent({ state: "idle", turn: "turn-1" }),
  ]);
  const live = liveProjectActivity(state, 2_000_000);
  assert.equal(live.length, 1, "the running turn was cleared by an older one");
  assert.equal(live[0].turnId, "turn-2");
});

test("a working announcement expires without any idle at all", () => {
  // A harness killed mid-turn sends no terminal frame. A view that waited for
  // one would show that agent as working forever.
  const state = fold([activityEvent({ createdAt: 1_000 })]);
  const justBefore = 1_000 * 1_000 + PROJECT_ACTIVITY_STALE_MS - 1;
  assert.equal(liveProjectActivity(state, justBefore).length, 1);
  assert.equal(
    liveProjectActivity(state, 1_000 * 1_000 + PROJECT_ACTIVITY_STALE_MS)
      .length,
    0,
  );
});

test("an out-of-order refresh does not replace a newer caption", () => {
  const state = fold([
    activityEvent({ createdAt: 2_000, stage: "editing files" }),
    activityEvent({ createdAt: 1_000, stage: "reading files" }),
  ]);
  assert.equal(liveProjectActivity(state, 2_000_000)[0].stage, "editing files");
});

test("two agents on one root are both shown", () => {
  const state = fold([
    activityEvent({ agent: AGENT }),
    activityEvent({ agent: OTHER_AGENT, turn: "turn-9" }),
  ]);
  assert.deepEqual(
    liveProjectActivity(state, 1_000_000).map((entry) => entry.agent),
    [AGENT, OTHER_AGENT],
  );
});

test("an event whose agent tag disagrees with its signature is refused", () => {
  // The tag exists so a consumer can filter without reading authorship. When
  // the two disagree one of them is a claim rather than a signature.
  const event = activityEvent({ agent: AGENT, pubkey: OTHER_AGENT });
  assert.equal(parseProjectActivity(event, ROOT), null);
  assert.deepEqual(liveProjectActivity(fold([event]), 1_000_000), []);
});

test("an event carrying an h tag is refused", () => {
  // NIP-PA forbids it: an event naming both a channel and a root names two
  // places for one signal, and guessing which wins puts the indicator on the
  // wrong surface.
  const event = activityEvent({
    extraTags: [["h", "11111111-1111-1111-1111-111111111111"]],
  });
  assert.equal(parseProjectActivity(event, ROOT), null);
});

test("an announcement missing its turn id is refused", () => {
  const event = activityEvent();
  event.tags = event.tags.filter((tag) => tag[0] !== "turn");
  assert.equal(parseProjectActivity(event, ROOT), null);
});

test("a foreign kind on the subscription is refused", () => {
  const event = activityEvent();
  event.kind = 20002;
  assert.equal(parseProjectActivity(event, ROOT), null);
});

test("an unchanged fold returns the same object so React can skip a render", () => {
  const state = fold([activityEvent()]);
  const again = applyProjectActivity(
    state,
    activityEvent({ root: OTHER_ROOT }),
    ROOT,
  );
  assert.equal(again, state);
});
