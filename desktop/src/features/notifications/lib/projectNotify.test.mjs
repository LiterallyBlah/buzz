import assert from "node:assert/strict";
import test from "node:test";

import {
  decideProjectNotification,
  PROJECT_NOTIFY_KINDS,
  projectRootIdForEvent,
  trackSeenProjectEvent,
} from "./projectNotify.ts";

const ME = "a".repeat(64);
const AGENT = "b".repeat(64);
const ROOT_ID = `root${"0".repeat(60)}`;
const OTHER_ROOT_ID = `other${"0".repeat(59)}`;
const COMMENT_ID = `comment${"0".repeat(57)}`;

const KIND_TEXT_NOTE = 1;
const KIND_GIT_PULL_REQUEST = 1618;
const KIND_GIT_PR_UPDATE = 1619;
const KIND_GIT_STATUS_CLOSED = 1632;
const KIND_REACTION = 7;

const WATCHED = new Set([ROOT_ID]);

function makeEvent(overrides = {}) {
  return {
    id: `event${"0".repeat(59)}`,
    pubkey: AGENT,
    created_at: 1_700_000_000,
    kind: KIND_TEXT_NOTE,
    tags: [["e", ROOT_ID, "", "root"]],
    content: "Pushed a fix",
    sig: "s".repeat(128),
    ...overrides,
  };
}

const options = (overrides = {}) => ({
  currentPubkey: ME,
  watchedRootIds: WATCHED,
  ...overrides,
});

test("notifies for an external comment on a watched root", () => {
  assert.deepEqual(decideProjectNotification(makeEvent(), options()), {
    notify: true,
    rootId: ROOT_ID,
  });
});

test("notifies for a status change on a watched root", () => {
  const event = makeEvent({ kind: KIND_GIT_STATUS_CLOSED, content: "" });
  assert.equal(decideProjectNotification(event, options()).notify, true);
});

test("notifies for a pull-request revision tagged with uppercase E", () => {
  const event = makeEvent({
    kind: KIND_GIT_PR_UPDATE,
    tags: [["E", ROOT_ID]],
  });
  assert.deepEqual(decideProjectNotification(event, options()), {
    notify: true,
    rootId: ROOT_ID,
  });
});

test("skips kinds that are not comments, statuses, or revisions", () => {
  const reaction = makeEvent({ kind: KIND_REACTION });
  assert.deepEqual(decideProjectNotification(reaction, options()), {
    notify: false,
    reason: "kind-not-notifiable",
  });

  // The PR root itself is not activity ON a root.
  const prRoot = makeEvent({ kind: KIND_GIT_PULL_REQUEST });
  assert.equal(
    decideProjectNotification(prRoot, options()).reason,
    "kind-not-notifiable",
  );
});

test("never notifies for the user's own events", () => {
  const mine = makeEvent({ pubkey: ME });
  assert.deepEqual(decideProjectNotification(mine, options()), {
    notify: false,
    reason: "self-authored",
  });
});

test("author exclusion is case-insensitive", () => {
  const mine = makeEvent({ pubkey: ME.toUpperCase() });
  assert.equal(
    decideProjectNotification(mine, options()).reason,
    "self-authored",
  );
});

test("skips events on roots outside the watch set", () => {
  const event = makeEvent({ tags: [["e", OTHER_ROOT_ID, "", "root"]] });
  assert.deepEqual(decideProjectNotification(event, options()), {
    notify: false,
    reason: "root-not-watched",
  });
});

test("skips events with no e/E tag at all", () => {
  const event = makeEvent({ tags: [["a", "30617:pubkey:repo"]] });
  assert.equal(
    decideProjectNotification(event, options()).reason,
    "root-not-watched",
  );
});

test("a reply to a comment resolves to the watched root, not the parent", () => {
  const event = makeEvent({
    tags: [
      ["e", COMMENT_ID, "", "reply"],
      ["e", ROOT_ID, "", "root"],
    ],
  });
  assert.deepEqual(decideProjectNotification(event, options()), {
    notify: true,
    rootId: ROOT_ID,
  });
});

test("an already-delivered event is skipped on redelivery", () => {
  const seenEventIds = new Set();
  const event = makeEvent();

  assert.equal(
    decideProjectNotification(event, options({ seenEventIds })).notify,
    true,
  );
  assert.deepEqual(
    decideProjectNotification(event, options({ seenEventIds })),
    {
      notify: false,
      reason: "already-delivered",
    },
  );
});

test("the same event arriving via both #e and #E filters delivers once", () => {
  const seenEventIds = new Set();
  const event = makeEvent({
    kind: KIND_GIT_PR_UPDATE,
    tags: [
      ["E", ROOT_ID],
      ["e", ROOT_ID],
    ],
  });

  assert.equal(
    decideProjectNotification(event, options({ seenEventIds })).notify,
    true,
  );
  assert.equal(
    decideProjectNotification(event, options({ seenEventIds })).notify,
    false,
  );
});

test("the dedupe guard only records events that would have been delivered", () => {
  const seenEventIds = new Set();

  decideProjectNotification(
    makeEvent({ kind: KIND_REACTION }),
    options({ seenEventIds }),
  );
  decideProjectNotification(
    makeEvent({ pubkey: ME }),
    options({ seenEventIds }),
  );
  decideProjectNotification(
    makeEvent({ tags: [["e", OTHER_ROOT_ID]] }),
    options({ seenEventIds }),
  );

  assert.equal(seenEventIds.size, 0);
});

test("an empty pubkey disables author exclusion rather than dropping everything", () => {
  const mine = makeEvent({ pubkey: ME });
  assert.equal(
    decideProjectNotification(mine, options({ currentPubkey: "" })).notify,
    true,
  );
});

test("projectRootIdForEvent ignores non-e tags and unwatched ids", () => {
  assert.equal(
    projectRootIdForEvent({ tags: [["p", ROOT_ID]] }, WATCHED),
    null,
  );
  assert.equal(
    projectRootIdForEvent({ tags: [["e", OTHER_ROOT_ID]] }, WATCHED),
    null,
  );
  assert.equal(
    projectRootIdForEvent({ tags: [["E", ROOT_ID]] }, WATCHED),
    ROOT_ID,
  );
});

test("PROJECT_NOTIFY_KINDS covers comments, all four statuses, and revisions", () => {
  assert.deepEqual(
    [...PROJECT_NOTIFY_KINDS].sort((a, b) => a - b),
    [1, 1619, 1630, 1631, 1632, 1633],
  );
});

test("trackSeenProjectEvent evicts the oldest id past the limit", () => {
  const seen = new Set();

  assert.equal(trackSeenProjectEvent(seen, "first", 2), true);
  assert.equal(trackSeenProjectEvent(seen, "first", 2), false);
  trackSeenProjectEvent(seen, "second", 2);
  trackSeenProjectEvent(seen, "third", 2);

  assert.equal(seen.size, 2);
  assert.equal(seen.has("first"), false);
  assert.deepEqual([...seen], ["second", "third"]);
});
