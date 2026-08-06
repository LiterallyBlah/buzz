/**
 * The two rules a bottom-pinned issue thread runs on.
 *
 * Both are exercised as arithmetic rather than through a rendered thread,
 * because that is what they are: the hook around them owns a ref and a scroll
 * listener, and neither of those is where a wrong at-bottom threshold or a
 * pill that counts deletions as arrivals would come from.
 */

import assert from "node:assert/strict";
import test from "node:test";

import {
  hasTurnSettled,
  isThreadAtBottom,
  nextUnreadBelowFold,
  THREAD_AT_BOTTOM_THRESHOLD_PX,
  workingAgentsKey,
} from "./projectThreadPin.ts";

test("at-bottom matches the chat timeline's threshold, on both sides of it", () => {
  assert.equal(THREAD_AT_BOTTOM_THRESHOLD_PX, 32);

  const floor = { clientHeight: 500, scrollHeight: 1000, scrollTop: 500 };
  assert.equal(isThreadAtBottom(floor), true);

  // Exactly at the threshold still counts; one pixel further up does not.
  assert.equal(isThreadAtBottom({ ...floor, scrollTop: 468 }), true);
  assert.equal(isThreadAtBottom({ ...floor, scrollTop: 467 }), false);
});

test("a thread shorter than its container is at the bottom", () => {
  assert.equal(
    isThreadAtBottom({ clientHeight: 500, scrollHeight: 300, scrollTop: 0 }),
    true,
  );
});

test("arrivals only count while the reader is away from the bottom", () => {
  assert.equal(
    nextUnreadBelowFold({
      atBottom: false,
      commentCount: 12,
      previousCommentCount: 10,
      unread: 1,
    }),
    3,
  );

  // Back at the bottom, the arrivals are on screen — the pill has nothing to
  // offer and must not survive as a count of comments the reader is reading.
  assert.equal(
    nextUnreadBelowFold({
      atBottom: true,
      commentCount: 12,
      previousCommentCount: 10,
      unread: 1,
    }),
    0,
  );
});

test("a comment deleted under a scrolled-up reader is not an arrival", () => {
  assert.equal(
    nextUnreadBelowFold({
      atBottom: false,
      commentCount: 9,
      previousCommentCount: 10,
      unread: 3,
    }),
    2,
  );
});

test("the count never promises more comments than the thread holds", () => {
  assert.equal(
    nextUnreadBelowFold({
      atBottom: false,
      commentCount: 2,
      previousCommentCount: 2,
      unread: 5,
    }),
    2,
  );
  assert.equal(
    nextUnreadBelowFold({
      atBottom: false,
      commentCount: 0,
      previousCommentCount: 4,
      unread: 4,
    }),
    0,
  );
});

test("only working agents are in the key, and their order cannot change it", () => {
  const entries = [
    { agent: "bbbb", state: "working" },
    { agent: "aaaa", state: "queued" },
    { agent: "cccc", state: "working" },
  ];
  assert.equal(workingAgentsKey(entries), "bbbb,cccc");
  assert.equal(workingAgentsKey([...entries].reverse()), "bbbb,cccc");
  assert.equal(workingAgentsKey([]), "");
});

test("an agent leaving the working set is a turn that settled", () => {
  assert.equal(hasTurnSettled("aaaa", ""), true);
  assert.equal(hasTurnSettled("aaaa,bbbb", "bbbb"), true);
});

test("an agent starting work is not something to catch up on", () => {
  assert.equal(hasTurnSettled("", "aaaa"), false);
  assert.equal(hasTurnSettled("aaaa", "aaaa,bbbb"), false);
  assert.equal(hasTurnSettled("aaaa", "aaaa"), false);
});

test("one agent finishing as another starts still settles a turn", () => {
  // Not a no-op just because the count is unchanged: the reader is waiting on
  // a specific agent's turn, and a same-size swap ended it.
  assert.equal(hasTurnSettled("aaaa", "bbbb"), true);
});

test("an unchanged thread leaves the count alone", () => {
  assert.equal(
    nextUnreadBelowFold({
      atBottom: false,
      commentCount: 10,
      previousCommentCount: 10,
      unread: 4,
    }),
    4,
  );
});
