import assert from "node:assert/strict";
import test from "node:test";

import {
  allowedActorsForProjectRoot,
  allowedActorsForRoot,
  buildGitIssueTags,
  buildGitStatusTags,
  nextProjectIssueStatusCreatedAt,
  eventToProjectIssue,
  mergeProjectIssueEvent,
  mergeProjectIssuesEvent,
  projectIssueEventsToIssues,
  getAllTags,
  getTag,
  ISSUE_ASSIGNMENT_LABEL,
  ISSUE_UNASSIGNMENT_LABEL,
  nextProjectIssueCommentCreatedAt,
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

function assignmentComment(
  pubkey,
  assignees,
  id,
  label = ISSUE_ASSIGNMENT_LABEL,
  createdAt = 200,
  prior,
) {
  return {
    id,
    kind: 1,
    pubkey,
    created_at: createdAt,
    content:
      label === ISSUE_ASSIGNMENT_LABEL
        ? "Assigned this issue"
        : "Unassigned this issue",
    tags: [
      ["e", "e".repeat(64), "", "root"],
      ["a", REPO_ADDRESS],
      ...assignees.map((value) => ["p", value]),
      ["t", label],
      ...(prior ? [["prior", prior]] : []),
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
  assert.equal(issue.category, "issue");
  assert.equal(issue.status, PROJECT_ISSUE_STATUS.BACKLOG);
  assert.equal(issue.title, "Something is broken");
});

test("derives task categories from labels while defaulting legacy tasks to issue", () => {
  const changeRequest = eventToProjectIssue(
    issueEvent({
      tags: [
        ["a", REPO_ADDRESS],
        ["subject", "Update the release workflow"],
        ["t", "change-request"],
        ["t", "release"],
      ],
    }),
  );

  assert.equal(changeRequest.category, "change-request");
  assert.deepEqual(changeRequest.labels, ["change-request", "release"]);
  assert.equal(eventToProjectIssue(issueEvent()).category, "issue");
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

test("parses public and private-safe issue provenance", () => {
  const channelId = "9a1657ac-f7aa-5db0-b632-d8bbeb6dfb50";
  const publicIssue = eventToProjectIssue(
    issueEvent({
      tags: [
        ["a", REPO_ADDRESS],
        ["h", channelId],
      ],
    }),
  );
  const privateIssue = eventToProjectIssue(
    issueEvent({
      tags: [
        ["a", REPO_ADDRESS],
        ["buzz-origin-agent", "Builder"],
      ],
    }),
  );

  assert.equal(publicIssue.channelId, channelId);
  assert.equal(publicIssue.originAgentName, null);
  assert.equal(privateIssue.channelId, null);
  assert.equal(privateIssue.originAgentName, "Builder");
});

test("assignees follow trusted assignment operations in deterministic order", () => {
  const assignee = "d".repeat(64);
  const otherAssignee = "f".repeat(64);
  const volunteer = "5".repeat(64);

  const issue = eventToProjectIssue(
    issueEvent(),
    [],
    [
      // Author assigns (self-assignment included) — trusted.
      assignmentComment(AUTHOR, [assignee.toUpperCase(), AUTHOR], "assign-1"),
      // Repo owner assigns — trusted; duplicate assignee dedupes.
      assignmentComment(OWNER, [assignee, otherAssignee], "assign-2"),
      // Any member self-assigning (sole p tag is the signer) — trusted.
      assignmentComment(volunteer, [volunteer], "assign-3"),
      // Untrusted signer assigning someone else — ignored.
      assignmentComment(ATTACKER, ["a".repeat(64)], "assign-4"),
      // Untrusted signer sneaking themselves in alongside others — ignored.
      assignmentComment(ATTACKER, [ATTACKER, "b".repeat(64)], "assign-5"),
      // A volunteer may remove only themselves.
      assignmentComment(
        volunteer,
        [volunteer],
        "unassign-1",
        ISSUE_UNASSIGNMENT_LABEL,
        201,
      ),
      // An untrusted signer cannot remove somebody else.
      assignmentComment(
        ATTACKER,
        [otherAssignee],
        "unassign-2",
        ISSUE_UNASSIGNMENT_LABEL,
        202,
      ),
      // Repo owner may remove any assignee.
      assignmentComment(
        OWNER,
        [otherAssignee],
        "unassign-3",
        ISSUE_UNASSIGNMENT_LABEL,
        203,
      ),
      // Same-second operations use event id as a stable tie-breaker:
      // assign sorts before unassign here, leaving the assignee removed.
      assignmentComment(OWNER, [otherAssignee], "a-assign", undefined, 204),
      assignmentComment(
        OWNER,
        [otherAssignee],
        "z-unassign",
        ISSUE_UNASSIGNMENT_LABEL,
        204,
      ),
      // Trusted plain comment without the label adds nothing.
      {
        id: "plain-comment",
        kind: 1,
        pubkey: AUTHOR,
        created_at: 201,
        content: "Just a comment",
        tags: [
          ["e", "e".repeat(64), "", "root"],
          ["p", ATTACKER],
        ],
      },
    ],
  );

  assert.deepEqual(issue.assignees.sort(), [AUTHOR, assignee].sort());
});

test("owner unassignment overrides a future-dated self-assignment", () => {
  const volunteer = "5".repeat(64);
  const issue = eventToProjectIssue(
    issueEvent(),
    [],
    [
      assignmentComment(
        volunteer,
        [volunteer],
        "future-self-assign",
        undefined,
        1_000,
      ),
      assignmentComment(
        OWNER,
        [volunteer],
        "owner-unassign",
        ISSUE_UNASSIGNMENT_LABEL,
        200,
      ),
    ],
  );

  assert.deepEqual(issue.assignees, []);
});

test("owner assignment overrides a future-dated self-unassignment", () => {
  const volunteer = "5".repeat(64);
  const issue = eventToProjectIssue(
    issueEvent(),
    [],
    [
      assignmentComment(
        volunteer,
        [volunteer],
        "future-self-unassign",
        ISSUE_UNASSIGNMENT_LABEL,
        1_000,
      ),
      assignmentComment(OWNER, [volunteer], "owner-assign", undefined, 200),
    ],
  );

  assert.deepEqual(issue.assignees, [volunteer]);
});

test("causal self-unassignment can follow an owner assignment", () => {
  const volunteer = "5".repeat(64);
  const ownerAssignmentId = "1".repeat(64);
  const selfUnassignmentId = "2".repeat(64);
  const issue = eventToProjectIssue(
    issueEvent(),
    [],
    [
      assignmentComment(OWNER, [volunteer], ownerAssignmentId),
      assignmentComment(
        volunteer,
        [volunteer],
        selfUnassignmentId,
        ISSUE_UNASSIGNMENT_LABEL,
        300,
        ownerAssignmentId,
      ),
    ],
  );

  assert.deepEqual(issue.assignees, []);
  assert.equal(issue.assigneeOperationHeads[volunteer], selfUnassignmentId);
});

test("causal self-assignment can follow an owner unassignment", () => {
  const volunteer = "5".repeat(64);
  const ownerUnassignmentId = "3".repeat(64);
  const selfAssignmentId = "4".repeat(64);
  const issue = eventToProjectIssue(
    issueEvent(),
    [],
    [
      assignmentComment(
        OWNER,
        [volunteer],
        ownerUnassignmentId,
        ISSUE_UNASSIGNMENT_LABEL,
      ),
      assignmentComment(
        volunteer,
        [volunteer],
        selfAssignmentId,
        ISSUE_ASSIGNMENT_LABEL,
        300,
        ownerUnassignmentId,
      ),
    ],
  );

  assert.deepEqual(issue.assignees, [volunteer]);
  assert.equal(issue.assigneeOperationHeads[volunteer], selfAssignmentId);
});

test("ignores a causal self-operation with a stale prior", () => {
  const volunteer = "5".repeat(64);
  const initialAssignmentId = "6".repeat(64);
  const ownerUnassignmentId = "7".repeat(64);
  const staleSelfAssignmentId = "8".repeat(64);
  const issue = eventToProjectIssue(
    issueEvent(),
    [],
    [
      assignmentComment(OWNER, [volunteer], initialAssignmentId),
      assignmentComment(
        OWNER,
        [volunteer],
        ownerUnassignmentId,
        ISSUE_UNASSIGNMENT_LABEL,
        250,
      ),
      assignmentComment(
        volunteer,
        [volunteer],
        staleSelfAssignmentId,
        ISSUE_ASSIGNMENT_LABEL,
        300,
        initialAssignmentId,
      ),
    ],
  );

  assert.deepEqual(issue.assignees, []);
  assert.equal(issue.assigneeOperationHeads[volunteer], ownerUnassignmentId);
});

test("issue recipients remain notification routing, not assignments", () => {
  const recipient = "d".repeat(64);
  const otherRecipient = "f".repeat(64);
  const issue = eventToProjectIssue(
    issueEvent({
      tags: [
        ["a", REPO_ADDRESS],
        ["subject", "Something is broken"],
        // Routing tag every issue carries — not an assignment.
        ["p", OWNER],
        ["p", recipient.toUpperCase()],
        ["p", otherRecipient],
      ],
    }),
  );

  assert.deepEqual(issue.assignees, []);
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
    [...allowedActorsForProjectRoot(issue)].sort(),
    [...allowedActorsForRoot(root)].sort(),
  );
  assert.ok(allowedActorsForProjectRoot(issue).has(AUTHOR.toLowerCase()));
  assert.ok(allowedActorsForProjectRoot(issue).has(OWNER.toLowerCase()));
  assert.ok(!allowedActorsForProjectRoot(issue).has(ATTACKER.toLowerCase()));
});

const ISSUE_ID = "e".repeat(64);

function commentEvent({
  id,
  createdAt,
  pubkey = OWNER,
  rootId = ISSUE_ID,
  rootTagName = "e",
  content = "Working on it.",
}) {
  return {
    id,
    kind: 1,
    pubkey,
    created_at: createdAt,
    content,
    tags: [
      [rootTagName, rootId, "", "root"],
      ["a", REPO_ADDRESS],
    ],
  };
}

test("a live comment lands exactly where a refetch would put it", () => {
  const first = commentEvent({ id: "comment-1", createdAt: 200 });
  const second = commentEvent({ id: "comment-2", createdAt: 300 });

  const merged = mergeProjectIssueEvent(
    eventToProjectIssue(issueEvent(), [], [first]),
    second,
  );

  assert.deepEqual(
    merged,
    eventToProjectIssue(issueEvent(), [], [first, second]),
  );
  assert.deepEqual(
    merged.comments.map((comment) => comment.id),
    ["comment-1", "comment-2"],
  );
  assert.equal(merged.updatedAt, 300);
});

test("a replayed comment is the same issue, not a second row", () => {
  const comment = commentEvent({ id: "comment-1", createdAt: 200 });
  const issue = eventToProjectIssue(issueEvent(), [], [comment]);

  assert.equal(mergeProjectIssueEvent(issue, comment), issue);
});

test("a comment on another root leaves this issue untouched", () => {
  const issue = eventToProjectIssue(issueEvent(), [], []);
  const otherRoot = commentEvent({
    id: "comment-1",
    createdAt: 200,
    rootId: "f".repeat(64),
  });

  assert.equal(mergeProjectIssueEvent(issue, otherRoot), issue);
});

test("a live status change follows the same trust and precedence rules", () => {
  const issue = eventToProjectIssue(issueEvent(), [
    statusEvent({ kind: 1631, pubkey: AUTHOR, createdAt: 300 }),
  ]);

  assert.equal(
    mergeProjectIssueEvent(
      issue,
      statusEvent({ kind: 1632, pubkey: ATTACKER, createdAt: 400 }),
    ),
    issue,
    "an untrusted signer cannot close the issue",
  );
  assert.equal(
    mergeProjectIssueEvent(
      issue,
      statusEvent({ kind: 1633, pubkey: OWNER, createdAt: 300 }),
    ),
    issue,
    "an equally-old status does not roll the panel back",
  );

  const closed = mergeProjectIssueEvent(
    issue,
    statusEvent({ kind: 1632, pubkey: OWNER, createdAt: 400 }),
  );
  assert.equal(closed.status, PROJECT_ISSUE_STATUS.CLOSED);
  assert.equal(closed.statusCreatedAt, 400);
  assert.equal(closed.updatedAt, 400);
});

test("an issue status must address its root in lowercase", () => {
  const issue = eventToProjectIssue(issueEvent(), []);
  const uppercaseRoot = {
    ...statusEvent({ kind: 1632, pubkey: OWNER, createdAt: 400 }),
    tags: [
      ["E", ISSUE_ID, "", "root"],
      ["a", REPO_ADDRESS],
    ],
  };

  assert.equal(mergeProjectIssueEvent(issue, uppercaseRoot), issue);
});

test("merging a list re-sorts it the way a refetch would order it", () => {
  const otherRoot = issueEvent({
    id: "d".repeat(64),
    created_at: 150,
    content: "Another issue",
  });
  const comment = commentEvent({ id: "comment-1", createdAt: 400 });
  const issues = projectIssueEventsToIssues([issueEvent(), otherRoot], [], []);

  assert.deepEqual(
    issues.map((issue) => issue.id),
    [otherRoot.id, ISSUE_ID],
  );
  assert.deepEqual(
    mergeProjectIssuesEvent(issues, comment),
    projectIssueEventsToIssues([issueEvent(), otherRoot], [], [comment]),
  );
  assert.equal(
    mergeProjectIssuesEvent(
      issues,
      commentEvent({
        id: "comment-2",
        createdAt: 400,
        rootId: "f".repeat(64),
      }),
    ),
    issues,
    "an event for no cached issue leaves the list identical",
  );
});

function deletionEvent({ pubkey, rootId = ISSUE_ID, targetId }) {
  return {
    id: `deletion-${targetId}`,
    kind: 5,
    pubkey,
    created_at: 900,
    content: "Delete comment",
    sig: "",
    tags: [
      ["e", targetId],
      ["E", rootId],
    ],
  };
}

test("deleting a comment leaves the issue exactly as a refetch would", () => {
  const kept = commentEvent({ id: "comment-1", createdAt: 200 });
  const removed = commentEvent({ id: "comment-2", createdAt: 400 });
  const issue = eventToProjectIssue(issueEvent(), [], [kept, removed]);
  assert.equal(issue.updatedAt, 400);

  const merged = mergeProjectIssueEvent(
    issue,
    deletionEvent({ pubkey: OWNER, targetId: "comment-2" }),
  );

  // Including `updatedAt`: a merged list that outranked a refetched one would
  // silently re-sort the issue under the reader on the next fetch.
  assert.deepEqual(merged, eventToProjectIssue(issueEvent(), [], [kept]));
  assert.equal(merged.updatedAt, 200);
});

test("removing the last comment falls back to the issue's own timestamps", () => {
  const comment = commentEvent({ id: "comment-1", createdAt: 400 });
  const merged = mergeProjectIssueEvent(
    eventToProjectIssue(issueEvent(), [], [comment]),
    deletionEvent({ pubkey: OWNER, targetId: "comment-1" }),
  );

  assert.deepEqual(merged.comments, []);
  assert.equal(merged.updatedAt, 100);
});

test("a tombstone only counts from the signer of what it names", () => {
  const comment = commentEvent({ id: "comment-1", createdAt: 200 });
  const issue = eventToProjectIssue(issueEvent(), [], [comment]);

  assert.equal(
    mergeProjectIssueEvent(
      issue,
      deletionEvent({ pubkey: ATTACKER, targetId: "comment-1" }),
    ),
    issue,
  );
  assert.equal(
    mergeProjectIssueEvent(
      issue,
      deletionEvent({ pubkey: OWNER, targetId: "comment-missing" }),
    ),
    issue,
    "a tombstone for a comment this issue does not hold changes nothing",
  );
});

test("deleting an issue drops it from the list, not its neighbour", () => {
  const otherRoot = issueEvent({
    id: "d".repeat(64),
    created_at: 500,
    content: "Another issue",
  });
  const issues = projectIssueEventsToIssues([issueEvent(), otherRoot]);

  assert.deepEqual(
    mergeProjectIssuesEvent(
      issues,
      deletionEvent({ pubkey: AUTHOR, targetId: ISSUE_ID }),
    ).map((issue) => issue.id),
    [otherRoot.id],
  );
  assert.equal(
    mergeProjectIssuesEvent(
      issues,
      deletionEvent({ pubkey: ATTACKER, targetId: ISSUE_ID }),
    ),
    issues,
    "an untrusted tombstone leaves the list identical",
  );
});

test("orders consecutive issue comments across whole-second timestamps", () => {
  const issue = eventToProjectIssue(
    issueEvent(),
    [],
    [
      {
        id: "comment-1",
        kind: 1,
        pubkey: AUTHOR,
        created_at: 200,
        content: "First",
        tags: [["e", "e".repeat(64), "", "root"]],
      },
      {
        id: "comment-2",
        kind: 1,
        pubkey: AUTHOR,
        created_at: 201,
        content: "Second",
        tags: [["e", "e".repeat(64), "", "root"]],
      },
      {
        id: "attacker-comment",
        kind: 1,
        pubkey: ATTACKER,
        created_at: 10_000,
        content: "Future",
        tags: [["e", "e".repeat(64), "", "root"]],
      },
    ],
  );

  assert.equal(nextProjectIssueCommentCreatedAt(issue, 200, AUTHOR), 202);
  assert.equal(nextProjectIssueCommentCreatedAt(issue, 300, AUTHOR), 300);
});
