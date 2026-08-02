import assert from "node:assert/strict";
import test from "node:test";

import {
  allowedActorsForProjectIssue,
  allowedActorsForRoot,
  buildGitIssueTags,
  buildGitStatusTags,
  nextProjectIssueStatusCreatedAt,
  eventToProjectIssue,
  getAllTags,
  getTag,
  PROJECT_ISSUE_STATUS,
} from "./projectIssues.mjs";

const OWNER = "a".repeat(64);
const AUTHOR = "b".repeat(64);
const ATTACKER = "c".repeat(64);
const REPO_ADDRESS = `30617:${OWNER}:demo`;

function issueEvent(overrides = {}) {
  return {
    id: "e".repeat(64),
    kind: 1621,
    pubkey: AUTHOR,
    created_at: 100,
    content: "Something is broken",
    tags: [
      ["a", REPO_ADDRESS],
      ["subject", "Something is broken"],
    ],
    ...overrides,
  };
}

function statusEvent({ kind, pubkey, createdAt }) {
  return {
    id: `status-${pubkey.slice(0, 8)}-${createdAt}`,
    kind,
    pubkey,
    created_at: createdAt,
    content: "",
    tags: [
      ["e", "e".repeat(64), "", "root"],
      ["a", REPO_ADDRESS],
    ],
  };
}

test("ignores status events from a different pubkey", () => {
  const attackerClosed = statusEvent({
    kind: 1632,
    pubkey: ATTACKER,
    createdAt: 300,
  });

  const issue = eventToProjectIssue(issueEvent(), [attackerClosed]);

  assert.equal(issue.status, PROJECT_ISSUE_STATUS.BACKLOG);
});

test("honors status events from the issue author and repo owner", () => {
  const authorDone = statusEvent({
    kind: 1631,
    pubkey: AUTHOR,
    createdAt: 300,
  });
  assert.equal(
    eventToProjectIssue(issueEvent(), [authorDone]).status,
    PROJECT_ISSUE_STATUS.DONE,
  );

  const ownerClosed = statusEvent({
    kind: 1632,
    pubkey: OWNER,
    createdAt: 300,
  });
  assert.equal(
    eventToProjectIssue(issueEvent(), [ownerClosed]).status,
    PROJECT_ISSUE_STATUS.CLOSED,
  );
});

test("tag helpers drop malformed value-less tags", () => {
  const event = issueEvent({
    tags: [
      ["a", REPO_ADDRESS],
      ["t"],
      ["t", ""],
      ["t", "bug"],
      ["p"],
      ["subject"],
    ],
  });

  assert.deepEqual(getAllTags(event, "t"), ["bug"]);
  assert.deepEqual(getAllTags(event, "p"), []);
  assert.equal(getTag(event, "subject"), undefined);

  const issue = eventToProjectIssue(event);
  assert.deepEqual(issue.labels, ["bug"]);
  assert.equal(issue.status, PROJECT_ISSUE_STATUS.BACKLOG);
  assert.equal(issue.title, "Something is broken");
});

test("preserves root and comment tags for rich content rendering", () => {
  const root = issueEvent({
    tags: [
      ["a", REPO_ADDRESS],
      ["subject", "Something is broken"],
      ["imeta", "url https://relay.example/media/root.png", "m image/png"],
    ],
  });
  const comment = {
    id: "comment-rich-content",
    kind: 1,
    pubkey: ATTACKER,
    created_at: 200,
    content: "![Screenshot](https://relay.example/media/comment.png)",
    tags: [
      ["e", root.id, "", "root"],
      ["imeta", "url https://relay.example/media/comment.png", "m image/png"],
    ],
  };

  const issue = eventToProjectIssue(root, [], [comment]);

  assert.deepEqual(issue.tags, [root.tags[2]]);
  assert.deepEqual(issue.comments[0].tags, [comment.tags[1]]);
});

test("builds repository-scoped issue creation tags", () => {
  assert.deepEqual(
    buildGitIssueTags({
      repoAddress: REPO_ADDRESS,
      repoOwner: OWNER,
      title: "  Fix the broken workflow  ",
    }),
    [
      ["a", REPO_ADDRESS],
      ["p", OWNER],
      ["subject", "Fix the broken workflow"],
    ],
  );
});

// ── Phase 4: mention picker and issue status controls ────────────────────────

test("selected mentions become p tags on the issue", () => {
  const first = "1".repeat(64);
  const second = "2".repeat(64);
  const tags = buildGitIssueTags({
    repoAddress: REPO_ADDRESS,
    repoOwner: OWNER,
    title: "Needs eyes",
    recipients: [first.toUpperCase(), second],
  });

  assert.deepEqual(tags, [
    ["a", REPO_ADDRESS],
    ["p", OWNER],
    ["subject", "Needs eyes"],
    ["p", first],
    ["p", second],
  ]);
});

test("creating an issue with no mentions is byte-for-byte what it was", () => {
  // The regression that matters most: the picker is optional, and an issue
  // created without touching it must produce the previous event exactly.
  const base = {
    repoAddress: REPO_ADDRESS,
    repoOwner: OWNER,
    title: "Fix the broken workflow",
  };
  assert.deepEqual(buildGitIssueTags(base), [
    ["a", REPO_ADDRESS],
    ["p", OWNER],
    ["subject", "Fix the broken workflow"],
  ]);
  assert.deepEqual(buildGitIssueTags({ ...base, recipients: [] }), [
    ["a", REPO_ADDRESS],
    ["p", OWNER],
    ["subject", "Fix the broken workflow"],
  ]);
});

test("mentioning the repo owner does not tag them twice", () => {
  // A duplicate `p` notifies once and reads as two participants everywhere the
  // tag list is rendered.
  const tags = buildGitIssueTags({
    repoAddress: REPO_ADDRESS,
    repoOwner: OWNER,
    title: "Hello",
    recipients: [OWNER.toUpperCase(), OWNER],
  });
  assert.equal(tags.filter((tag) => tag[0] === "p").length, 1);
});

test("a malformed mention is refused rather than published", () => {
  assert.throws(() =>
    buildGitIssueTags({
      repoAddress: REPO_ADDRESS,
      repoOwner: OWNER,
      title: "Hello",
      recipients: ["not-a-pubkey"],
    }),
  );
});

test("issue status tags bind the exact root, repo, owner and author", () => {
  const issueId = "a".repeat(64);
  const author = "b".repeat(64);
  assert.deepEqual(
    buildGitStatusTags({
      issueId,
      repoAddress: REPO_ADDRESS,
      repoOwner: OWNER,
      issueAuthor: author,
    }),
    [
      ["e", issueId, "", "root"],
      ["a", REPO_ADDRESS],
      ["p", OWNER],
      ["p", author],
    ],
  );
});

test("an issue authored by the repo owner is not p-tagged twice", () => {
  const issueId = "a".repeat(64);
  const tags = buildGitStatusTags({
    issueId,
    repoAddress: REPO_ADDRESS,
    repoOwner: OWNER,
    issueAuthor: OWNER.toUpperCase(),
  });
  assert.equal(tags.filter((tag) => tag[0] === "p").length, 1);
});

test("a status change from the author or repo owner moves the issue", () => {
  // The reader's own rule, exercised end to end: a published 1631 from a
  // trusted actor must be what the panel then shows.
  const root = {
    id: "a".repeat(64),
    kind: 1621,
    pubkey: AUTHOR,
    created_at: 100,
    content: "body",
    tags: [
      ["a", REPO_ADDRESS],
      ["subject", "Broken"],
    ],
  };
  const resolved = {
    id: "c".repeat(64),
    kind: 1631,
    pubkey: AUTHOR,
    created_at: 200,
    content: "",
    tags: [["e", root.id, "", "root"]],
  };

  assert.equal(eventToProjectIssue(root, [], []).status, "Backlog");
  const done = eventToProjectIssue(root, [resolved], []);
  assert.equal(done.status, "Done");
  assert.equal(done.statusCreatedAt, 200);

  // And the same event from anybody else changes nothing, which is why the
  // control is only offered to the two trusted pubkeys.
  const impostor = { ...resolved, id: "d".repeat(64), pubkey: ATTACKER };
  assert.equal(eventToProjectIssue(root, [impostor], []).status, "Backlog");
});

test("two status changes in one second stay ordered", () => {
  // `latestStatusForIssue` sorts by created_at, so without the bump the second
  // change resolves by tie-break rather than by what the person last did.
  const issue = { statusCreatedAt: 500 };
  assert.equal(nextProjectIssueStatusCreatedAt(issue, 500), 501);
  assert.equal(nextProjectIssueStatusCreatedAt(issue, 900), 900);
  assert.equal(
    nextProjectIssueStatusCreatedAt({ statusCreatedAt: null }, 42),
    42,
  );
});

test("the lifecycle actors rule has one implementation", () => {
  const root = {
    id: "a".repeat(64),
    kind: 1621,
    pubkey: AUTHOR,
    created_at: 1,
    content: "",
    tags: [["a", REPO_ADDRESS]],
  };
  const issue = eventToProjectIssue(root, [], []);
  assert.deepEqual(
    [...allowedActorsForProjectIssue(issue)].sort(),
    [...allowedActorsForRoot(root)].sort(),
  );
  assert.ok(allowedActorsForProjectIssue(issue).has(AUTHOR.toLowerCase()));
  assert.ok(allowedActorsForProjectIssue(issue).has(OWNER.toLowerCase()));
  assert.ok(!allowedActorsForProjectIssue(issue).has(ATTACKER.toLowerCase()));
});
