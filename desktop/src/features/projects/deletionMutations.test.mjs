import assert from "node:assert/strict";
import test from "node:test";

import { QueryClient } from "@tanstack/react-query";

import {
  canDeleteProjectEvent,
  deleteProjectEvent,
  projectDeletionEventInput,
} from "./deletionMutations.ts";
import { eventToProjectIssue } from "./projectIssues.mjs";
import { eventToProjectPullRequest } from "./projectPullRequests.mjs";

const OWNER = "a".repeat(64);
const AUTHOR = "b".repeat(64);
const OTHER = "c".repeat(64);
const REPO_ADDRESS = `30617:${OWNER}:demo`;
const ISSUE_ID = "e".repeat(64);
const PR_ID = "f".repeat(64);
const COMMENT_ID = "1".repeat(64);
const PROJECT_ID = `${OWNER}:demo`;

function tagValues(event, key) {
  return event.tags.filter((tag) => tag[0] === key).map((tag) => tag[1]);
}

test("a deletion names exactly one target and one route", () => {
  const event = projectDeletionEventInput({
    rootId: ISSUE_ID,
    subject: "comment",
    targetId: COMMENT_ID,
  });

  assert.equal(event.kind, 5);
  assert.equal(event.content, "Delete comment");
  // The relay rejects a kind:5 naming anything but exactly one `e`-or-`a`
  // target, so a second `e` — or any `a` — would fail every delete at ingest.
  assert.deepEqual(tagValues(event, "e"), [COMMENT_ID]);
  assert.deepEqual(tagValues(event, "a"), []);
  // The uppercase `E` is the route, not a target: it is how a comment's
  // tombstone reaches the only subscription an open panel holds.
  assert.deepEqual(tagValues(event, "E"), [ISSUE_ID]);
});

test("deleting a root routes the tombstone to itself", () => {
  const event = projectDeletionEventInput({
    rootId: PR_ID,
    subject: "pull request",
    targetId: PR_ID,
  });

  assert.equal(event.content, "Delete pull request");
  assert.deepEqual(tagValues(event, "e"), [PR_ID]);
  assert.deepEqual(tagValues(event, "E"), [PR_ID]);
});

test("an id that is not an event id never reaches the signer", () => {
  assert.throws(
    () =>
      projectDeletionEventInput({
        rootId: ISSUE_ID,
        subject: "comment",
        targetId: "not-an-event-id",
      }),
    /64-character event id/,
  );
  assert.throws(
    () =>
      projectDeletionEventInput({
        rootId: "",
        subject: "issue",
        targetId: ISSUE_ID,
      }),
    /64-character event id/,
  );
});

test("only the author of an event is offered its delete control", () => {
  assert.equal(canDeleteProjectEvent(AUTHOR, AUTHOR.toUpperCase()), true);
  assert.equal(canDeleteProjectEvent(AUTHOR.toUpperCase(), AUTHOR), true);
  assert.equal(canDeleteProjectEvent(AUTHOR, OTHER), false);
  // The repository owner is not the author of somebody else's issue, and this
  // slice does not infer moderation from ownership.
  assert.equal(canDeleteProjectEvent(AUTHOR, OWNER), false);
  assert.equal(canDeleteProjectEvent(AUTHOR, undefined), false);
  assert.equal(canDeleteProjectEvent(AUTHOR, null), false);
});

function issueEvent() {
  return {
    id: ISSUE_ID,
    kind: 1621,
    pubkey: AUTHOR,
    created_at: 100,
    content: "Something is broken",
    tags: [
      ["a", REPO_ADDRESS],
      ["subject", "Something is broken"],
    ],
  };
}

function commentEvent() {
  return {
    id: COMMENT_ID,
    kind: 1,
    pubkey: AUTHOR,
    created_at: 400,
    content: "A second thought",
    tags: [
      ["e", ISSUE_ID, "", "root"],
      ["a", REPO_ADDRESS],
    ],
  };
}

function pullRequestEvent() {
  return {
    id: PR_ID,
    kind: 1618,
    pubkey: AUTHOR,
    created_at: 100,
    content: "Add feature",
    tags: [
      ["a", REPO_ADDRESS],
      ["subject", "Add feature"],
      ["c", "1111111111111111111111111111111111111111"],
    ],
  };
}

function seededQueryClient() {
  const queryClient = new QueryClient();
  const issue = eventToProjectIssue(issueEvent(), [], [commentEvent()]);
  const pullRequest = eventToProjectPullRequest(pullRequestEvent());
  const project = { id: PROJECT_ID, repoAddress: REPO_ADDRESS };

  queryClient.setQueryData(["project", PROJECT_ID, "issues"], [issue]);
  queryClient.setQueryData(
    ["project", PROJECT_ID, "pull-requests"],
    [pullRequest],
  );
  queryClient.setQueryData(["projects", "work-items", [PROJECT_ID]], {
    issues: { items: [{ project, issue }], failedSections: [] },
    pullRequests: { items: [{ project, pullRequest }], failedSections: [] },
  });
  return queryClient;
}

function cachedIssues(queryClient) {
  return queryClient.getQueryData(["project", PROJECT_ID, "issues"]);
}

function cachedWorkItems(queryClient) {
  return queryClient.getQueryData(["projects", "work-items", [PROJECT_ID]]);
}

function tombstone(input) {
  return {
    ...projectDeletionEventInput(input),
    id: "9".repeat(64),
    pubkey: AUTHOR,
    created_at: 500,
    sig: "",
  };
}

test("a rejected deletion leaves every cache exactly as it was", async () => {
  const queryClient = seededQueryClient();
  const issuesBefore = cachedIssues(queryClient);
  const workItemsBefore = cachedWorkItems(queryClient);

  await assert.rejects(
    deleteProjectEvent({
      input: {
        author: AUTHOR,
        rootId: ISSUE_ID,
        subject: "issue",
        targetId: ISSUE_ID,
      },
      projectId: PROJECT_ID,
      // The relay refusing is the case that matters: nothing may be removed
      // from a cache before the relay has accepted the tombstone.
      publish: async () => {
        throw new Error("Failed to delete issue.");
      },
      queryClient,
    }),
    /Failed to delete issue\./,
  );

  assert.equal(cachedIssues(queryClient), issuesBefore);
  assert.equal(cachedWorkItems(queryClient), workItemsBefore);
  assert.equal(issuesBefore.length, 1);
});

test("deleting an issue removes it from the project list and the inbox", async () => {
  const queryClient = seededQueryClient();

  await deleteProjectEvent({
    input: {
      author: AUTHOR,
      rootId: ISSUE_ID,
      subject: "issue",
      targetId: ISSUE_ID,
    },
    projectId: PROJECT_ID,
    publish: async (input) => tombstone(input),
    queryClient,
  });

  assert.deepEqual(cachedIssues(queryClient), []);
  assert.deepEqual(cachedWorkItems(queryClient).issues.items, []);
  assert.equal(
    cachedWorkItems(queryClient).pullRequests.items.length,
    1,
    "deleting an issue leaves the pull requests alone",
  );
});

test("deleting a comment removes only that comment", async () => {
  const queryClient = seededQueryClient();
  assert.equal(cachedIssues(queryClient)[0].comments.length, 1);

  await deleteProjectEvent({
    input: {
      author: AUTHOR,
      rootId: ISSUE_ID,
      subject: "comment",
      targetId: COMMENT_ID,
    },
    projectId: PROJECT_ID,
    publish: async (input) => tombstone(input),
    queryClient,
  });

  const [issue] = cachedIssues(queryClient);
  assert.equal(issue.id, ISSUE_ID, "the issue itself survives its comment");
  assert.deepEqual(issue.comments, []);
  assert.equal(
    issue.updatedAt,
    issue.createdAt,
    "the comment that set updatedAt is gone, so it falls back to creation",
  );
  assert.deepEqual(
    cachedWorkItems(queryClient).issues.items[0].issue.comments,
    [],
  );
});
