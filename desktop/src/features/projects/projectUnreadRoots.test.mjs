import assert from "node:assert/strict";
import test from "node:test";

import {
  deriveWatchedProjectRoots,
  WATCHED_PROJECT_ROOT_LIMIT,
} from "./projectUnreadRoots.ts";

const ME = "a".repeat(64);
const AGENT = "b".repeat(64);
const STRANGER = "c".repeat(64);

const PROJECT = { id: "project-1", name: "buzz-desktop" };
const OTHER_PROJECT = { id: "project-2", name: "buzz-relay" };

function issue(overrides = {}) {
  return {
    id: "issue-1",
    title: "Login is broken",
    author: ME,
    updatedAt: 100,
    comments: [],
    ...overrides,
  };
}

function pullRequest(overrides = {}) {
  return {
    id: "pr-1",
    title: "Fix login",
    author: AGENT,
    updatedAt: 200,
    comments: [],
    ...overrides,
  };
}

function snapshot({ issues = [], pullRequests = [], project = PROJECT } = {}) {
  return {
    issues: {
      items: issues.map((item) => ({ project, issue: item })),
      failedSections: [],
    },
    pullRequests: {
      items: pullRequests.map((item) => ({ project, pullRequest: item })),
      failedSections: [],
    },
  };
}

test("watches issues the user authored", () => {
  const result = deriveWatchedProjectRoots(
    [snapshot({ issues: [issue()] })],
    ME,
  );

  assert.deepEqual(
    result.roots.map((root) => root.rootId),
    ["issue-1"],
  );
  assert.equal(result.roots[0].authored, true);
  assert.equal(result.roots[0].workItemKind, "issue");
  assert.equal(result.roots[0].projectId, "project-1");
  assert.equal(result.roots[0].projectName, "buzz-desktop");
});

test("watches items the user only commented on", () => {
  const result = deriveWatchedProjectRoots(
    [
      snapshot({
        pullRequests: [
          pullRequest({ comments: [{ author: STRANGER }, { author: ME }] }),
        ],
      }),
    ],
    ME,
  );

  assert.deepEqual(
    result.roots.map((root) => root.rootId),
    ["pr-1"],
  );
  assert.equal(result.roots[0].authored, false);
  assert.equal(result.roots[0].workItemKind, "pull-request");
});

test("ignores items the user has no involvement with", () => {
  const result = deriveWatchedProjectRoots(
    [
      snapshot({
        issues: [issue({ author: STRANGER, comments: [{ author: AGENT }] })],
        pullRequests: [pullRequest()],
      }),
    ],
    ME,
  );

  assert.deepEqual(result.roots, []);
  assert.equal(result.rootIdsKey, "");
});

test("involvement matching is case-insensitive on pubkeys", () => {
  const result = deriveWatchedProjectRoots(
    [snapshot({ issues: [issue({ author: ME.toUpperCase() })] })],
    ME.toLowerCase(),
  );

  assert.equal(result.roots.length, 1);
});

test("no identity means no watch set (never watch everything)", () => {
  const result = deriveWatchedProjectRoots(
    [snapshot({ issues: [issue()] })],
    undefined,
  );

  assert.deepEqual(result.roots, []);
  assert.equal(result.candidateCount, 0);
});

test("merges duplicate roots across cache rows, keeping the freshest", () => {
  const stale = snapshot({ issues: [issue({ updatedAt: 100 })] });
  const fresh = snapshot({
    issues: [issue({ updatedAt: 400, title: "Login is broken (updated)" })],
  });

  const result = deriveWatchedProjectRoots([stale, fresh], ME);

  assert.equal(result.roots.length, 1);
  assert.equal(result.roots[0].updatedAt, 400);
  assert.equal(result.roots[0].title, "Login is broken (updated)");
});

test("a staler row cannot downgrade authored to false", () => {
  const authoredRow = snapshot({ issues: [issue({ updatedAt: 100 })] });
  const participatedRow = snapshot({
    issues: [
      issue({ updatedAt: 400, author: STRANGER, comments: [{ author: ME }] }),
    ],
  });

  const result = deriveWatchedProjectRoots([authoredRow, participatedRow], ME);
  assert.equal(result.roots.length, 1);
  assert.equal(result.roots[0].authored, true);
});

test("undefined cache rows are skipped", () => {
  const result = deriveWatchedProjectRoots(
    [undefined, snapshot({ issues: [issue()] })],
    ME,
  );

  assert.equal(result.roots.length, 1);
});

test("keeps the most recently active roots and reports the truncation", () => {
  const issues = Array.from({ length: 5 }, (_, index) =>
    issue({ id: `issue-${index}`, updatedAt: index }),
  );

  const result = deriveWatchedProjectRoots([snapshot({ issues })], ME, 2);

  assert.deepEqual(
    result.roots.map((root) => root.rootId),
    ["issue-4", "issue-3"],
  );
  assert.equal(result.candidateCount, 5);
  assert.equal(result.truncatedCount, 3);
});

test("no truncation reports zero dropped roots", () => {
  const result = deriveWatchedProjectRoots(
    [snapshot({ issues: [issue()] })],
    ME,
    WATCHED_PROJECT_ROOT_LIMIT,
  );

  assert.equal(result.truncatedCount, 0);
  assert.equal(result.candidateCount, 1);
});

test("the cap breaks timestamp ties deterministically", () => {
  const issues = [
    issue({ id: "issue-b", updatedAt: 10 }),
    issue({ id: "issue-a", updatedAt: 10 }),
  ];

  const first = deriveWatchedProjectRoots([snapshot({ issues })], ME, 1);
  const reversed = deriveWatchedProjectRoots(
    [snapshot({ issues: [...issues].reverse() })],
    ME,
    1,
  );

  assert.deepEqual(
    first.roots.map((root) => root.rootId),
    ["issue-a"],
  );
  assert.deepEqual(first.roots, reversed.roots);
});

test("rootIdsKey is order-independent so it can gate resubscription", () => {
  const forward = deriveWatchedProjectRoots(
    [
      snapshot({
        issues: [issue({ id: "issue-z" }), issue({ id: "issue-a" })],
      }),
    ],
    ME,
  );
  const backward = deriveWatchedProjectRoots(
    [
      snapshot({
        issues: [issue({ id: "issue-a" }), issue({ id: "issue-z" })],
      }),
    ],
    ME,
  );

  assert.equal(forward.rootIdsKey, "issue-a,issue-z");
  assert.equal(forward.rootIdsKey, backward.rootIdsKey);
});

test("byRootId indexes exactly the capped roots", () => {
  const issues = Array.from({ length: 3 }, (_, index) =>
    issue({ id: `issue-${index}`, updatedAt: index }),
  );

  const result = deriveWatchedProjectRoots([snapshot({ issues })], ME, 2);

  assert.equal(result.byRootId.size, 2);
  assert.equal(result.byRootId.has("issue-0"), false);
  assert.equal(result.byRootId.get("issue-2").title, "Login is broken");
});

test("roots from different projects keep their own project identity", () => {
  const result = deriveWatchedProjectRoots(
    [
      snapshot({ issues: [issue({ id: "issue-1", updatedAt: 2 })] }),
      snapshot({
        project: OTHER_PROJECT,
        issues: [issue({ id: "issue-2", updatedAt: 1 })],
      }),
    ],
    ME,
  );

  assert.deepEqual(
    result.roots.map((root) => [root.rootId, root.projectId]),
    [
      ["issue-1", "project-1"],
      ["issue-2", "project-2"],
    ],
  );
});
