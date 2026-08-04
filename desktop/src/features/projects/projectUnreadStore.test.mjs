import assert from "node:assert/strict";
import test from "node:test";

import {
  clearAllProjectUnread,
  clearProjectUnreadForProject,
  clearProjectUnreadRoot,
  getProjectUnreadSnapshot,
  markProjectRootUnread,
  resetProjectUnreadStoreForTests,
  retainProjectUnreadRoots,
  subscribeToProjectUnread,
} from "./projectUnreadStore.ts";

function root(overrides = {}) {
  return {
    rootId: "issue-1",
    projectId: "project-1",
    projectName: "buzz-desktop",
    workItemKind: "issue",
    title: "Login is broken",
    updatedAt: 100,
    authored: true,
    ...overrides,
  };
}

function mark(overrides = {}) {
  const { rootOverrides = {}, ...rest } = overrides;
  markProjectRootUnread({
    root: root(rootOverrides),
    eventId: "event-1",
    author: "b".repeat(64),
    createdAt: 1_700_000_000,
    ...rest,
  });
}

test.beforeEach(() => {
  resetProjectUnreadStoreForTests();
});

test("marking a root makes it unread and notifies subscribers", () => {
  let notifications = 0;
  subscribeToProjectUnread(() => {
    notifications += 1;
  });

  mark();

  const snapshot = getProjectUnreadSnapshot();
  assert.equal(snapshot.unreadRootCount, 1);
  assert.equal(snapshot.unreadEventCount, 1);
  assert.equal(notifications, 1);
  assert.equal(
    snapshot.entriesByRootId.get("issue-1").projectName,
    "buzz-desktop",
  );
});

test("further events on the same root raise the event count, not the root count", () => {
  mark({ eventId: "event-1" });
  mark({ eventId: "event-2" });

  const snapshot = getProjectUnreadSnapshot();
  assert.equal(snapshot.unreadRootCount, 1);
  assert.equal(snapshot.unreadEventCount, 2);
  assert.equal(snapshot.entriesByRootId.get("issue-1").lastEventId, "event-2");
});

test("the same event id delivered twice does not inflate the count", () => {
  mark({ eventId: "event-1" });
  mark({ eventId: "event-1" });

  assert.equal(getProjectUnreadSnapshot().unreadEventCount, 1);
});

test("the snapshot is referentially stable between changes", () => {
  mark();
  const first = getProjectUnreadSnapshot();
  assert.equal(getProjectUnreadSnapshot(), first);

  mark({ eventId: "event-2" });
  assert.notEqual(getProjectUnreadSnapshot(), first);
});

test("roots are indexed by project so a project can be cleared alone", () => {
  mark({ rootOverrides: { rootId: "issue-1", projectId: "project-1" } });
  mark({
    eventId: "event-2",
    rootOverrides: {
      rootId: "pr-1",
      projectId: "project-2",
      workItemKind: "pull-request",
    },
  });

  assert.equal(getProjectUnreadSnapshot().unreadRootCount, 2);

  clearProjectUnreadForProject("project-1");
  const snapshot = getProjectUnreadSnapshot();
  assert.deepEqual([...snapshot.entriesByRootId.keys()], ["pr-1"]);
  assert.equal(snapshot.rootIdsByProjectId.has("project-1"), false);
});

test("clearing a single root and clearing everything both work", () => {
  mark({ rootOverrides: { rootId: "issue-1" } });
  mark({ eventId: "event-2", rootOverrides: { rootId: "issue-2" } });

  clearProjectUnreadRoot("issue-1");
  assert.equal(getProjectUnreadSnapshot().unreadRootCount, 1);

  clearAllProjectUnread();
  assert.equal(getProjectUnreadSnapshot().unreadRootCount, 0);
});

test("clearing something already clear does not notify subscribers", () => {
  let notifications = 0;
  subscribeToProjectUnread(() => {
    notifications += 1;
  });

  clearAllProjectUnread();
  clearProjectUnreadRoot("nope");
  clearProjectUnreadForProject("nope");

  assert.equal(notifications, 0);
});

test("retain drops entries whose root is no longer watched", () => {
  mark({ rootOverrides: { rootId: "issue-1" } });
  mark({ eventId: "event-2", rootOverrides: { rootId: "issue-2" } });

  retainProjectUnreadRoots(new Set(["issue-2"]));

  assert.deepEqual(
    [...getProjectUnreadSnapshot().entriesByRootId.keys()],
    ["issue-2"],
  );
});

test("the entry map is bounded, evicting the least recently active root", () => {
  for (let index = 0; index < 205; index += 1) {
    mark({
      eventId: `event-${index}`,
      createdAt: 1_700_000_000 + index,
      rootOverrides: { rootId: `issue-${index}` },
    });
  }

  const snapshot = getProjectUnreadSnapshot();
  assert.equal(snapshot.unreadRootCount, 200);
  assert.equal(snapshot.entriesByRootId.has("issue-0"), false);
  assert.equal(snapshot.entriesByRootId.has("issue-204"), true);
});
