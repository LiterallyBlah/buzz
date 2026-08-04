import assert from "node:assert/strict";
import test from "node:test";

import { QueryClient } from "@tanstack/react-query";

import { eventToProjectIssue } from "./projectIssues.mjs";
import { eventToProjectPullRequest } from "./projectPullRequests.mjs";
import {
  applyProjectRootEvent,
  projectRootEventRole,
  projectRootLiveFilters,
} from "./projectRootLiveUpdates.ts";

const OWNER = "a".repeat(64);
const AUTHOR = "b".repeat(64);
const REPO_ADDRESS = `30617:${OWNER}:demo`;
const ISSUE_ID = "e".repeat(64);
const PR_ID = "f".repeat(64);
const OTHER_ROOT_ID = "1".repeat(64);
const PROJECT_ID = `${OWNER}:demo`;

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

function relayEvent({ kind, rootId, rootTagName = "e", id = "event-1" }) {
  return {
    id,
    kind,
    pubkey: OWNER,
    created_at: 400,
    content: "Agent reply",
    sig: "",
    tags: [
      [rootTagName, rootId, "", "root"],
      ["a", REPO_ADDRESS],
    ],
  };
}

test("comments and statuses address their root in lowercase", () => {
  assert.equal(
    projectRootEventRole(relayEvent({ kind: 1, rootId: ISSUE_ID }), ISSUE_ID),
    "comment",
  );
  assert.equal(
    projectRootEventRole(
      relayEvent({ kind: 1632, rootId: ISSUE_ID }),
      ISSUE_ID,
    ),
    "status",
  );
  for (const kind of [1630, 1631, 1632, 1633]) {
    assert.equal(
      projectRootEventRole(relayEvent({ kind, rootId: PR_ID }), PR_ID),
      "status",
      String(kind),
    );
  }
});

test("a pull-request revision is routed by its uppercase root tag", () => {
  assert.equal(
    projectRootEventRole(
      relayEvent({ kind: 1619, rootId: PR_ID, rootTagName: "E" }),
      PR_ID,
    ),
    "revision",
  );
  assert.equal(
    projectRootEventRole(relayEvent({ kind: 1619, rootId: PR_ID }), PR_ID),
    null,
    "a lowercase-tagged 1619 does not revise this pull request",
  );
});

test("nothing from another root or an unrelated kind is routed here", () => {
  assert.equal(
    projectRootEventRole(
      relayEvent({ kind: 1, rootId: OTHER_ROOT_ID }),
      ISSUE_ID,
    ),
    null,
  );
  assert.equal(
    projectRootEventRole(
      relayEvent({ kind: 1619, rootId: OTHER_ROOT_ID, rootTagName: "E" }),
      PR_ID,
    ),
    null,
  );
  assert.equal(
    projectRootEventRole(relayEvent({ kind: 7, rootId: ISSUE_ID }), ISSUE_ID),
    null,
    "a reaction is not part of this root's rendered state",
  );
});

test("the two live filters split the root's traffic by tag case", () => {
  const [conversation, revisions] = projectRootLiveFilters(ISSUE_ID, 1_000);

  assert.deepEqual(conversation.kinds, [1, 1630, 1631, 1632, 1633]);
  assert.deepEqual(conversation["#e"], [ISSUE_ID]);
  assert.equal(conversation["#E"], undefined);
  assert.deepEqual(revisions.kinds, [1619]);
  assert.deepEqual(revisions["#E"], [ISSUE_ID]);
  assert.equal(revisions["#e"], undefined);
  // Both look back past "now" so an event published between the fetch that
  // filled the panel and this subscription is not lost between them.
  assert.ok(conversation.since < 1_000);
  assert.equal(conversation.since, revisions.since);
});

function seededQueryClient() {
  const queryClient = new QueryClient();
  const issue = eventToProjectIssue(issueEvent());
  const pullRequest = eventToProjectPullRequest(pullRequestEvent());
  const project = { id: PROJECT_ID, repoAddress: REPO_ADDRESS };

  queryClient.setQueryData(["project", PROJECT_ID, "issues"], [issue]);
  queryClient.setQueryData(
    ["project", PROJECT_ID, "pull-requests"],
    [pullRequest],
  );
  queryClient.setQueryData(["projects", "work-items", [PROJECT_ID]], {
    issues: { items: [{ project, issue }], failedSections: [] },
    pullRequests: {
      items: [{ project, pullRequest }],
      failedSections: [],
    },
  });
  return queryClient;
}

function cachedIssue(queryClient) {
  return queryClient.getQueryData(["project", PROJECT_ID, "issues"])[0];
}

function cachedWorkItemIssue(queryClient) {
  return queryClient.getQueryData(["projects", "work-items", [PROJECT_ID]])
    .issues.items[0].issue;
}

test("a comment reaches every cache that can be showing the issue", () => {
  const queryClient = seededQueryClient();
  const comment = relayEvent({ kind: 1, rootId: ISSUE_ID });

  assert.equal(
    applyProjectRootEvent(queryClient, {
      event: comment,
      projectId: PROJECT_ID,
      rootId: ISSUE_ID,
    }),
    true,
  );
  assert.deepEqual(
    cachedIssue(queryClient).comments.map((item) => item.content),
    ["Agent reply"],
  );
  assert.deepEqual(
    cachedWorkItemIssue(queryClient).comments.map((item) => item.content),
    ["Agent reply"],
  );
  assert.deepEqual(
    queryClient
      .getQueryData(["project", PROJECT_ID, "pull-requests"])[0]
      .comments.map((item) => item.id),
    [],
    "an issue's comment does not leak onto a pull request",
  );
});

test("re-applying the same event changes nothing and reports nothing", () => {
  const queryClient = seededQueryClient();
  const comment = relayEvent({ kind: 1, rootId: ISSUE_ID });
  applyProjectRootEvent(queryClient, {
    event: comment,
    projectId: PROJECT_ID,
    rootId: ISSUE_ID,
  });
  const merged = cachedIssue(queryClient);

  assert.equal(
    applyProjectRootEvent(queryClient, {
      event: comment,
      projectId: PROJECT_ID,
      rootId: ISSUE_ID,
    }),
    false,
  );
  assert.equal(cachedIssue(queryClient), merged);
});

test("an event for another root never touches this project's caches", () => {
  const queryClient = seededQueryClient();
  const issue = cachedIssue(queryClient);

  assert.equal(
    applyProjectRootEvent(queryClient, {
      event: relayEvent({ kind: 1, rootId: OTHER_ROOT_ID }),
      projectId: PROJECT_ID,
      rootId: ISSUE_ID,
    }),
    false,
    "the subscribed root is what decides, not the event's own claim",
  );
  assert.equal(cachedIssue(queryClient), issue);
});

test("a revision updates the pull request the panel is showing", () => {
  const queryClient = seededQueryClient();
  const revision = {
    ...relayEvent({ kind: 1619, rootId: PR_ID, rootTagName: "E" }),
    pubkey: AUTHOR,
    tags: [
      ["E", PR_ID],
      ["a", REPO_ADDRESS],
      ["c", "2222222222222222222222222222222222222222"],
    ],
  };

  assert.equal(
    applyProjectRootEvent(queryClient, {
      event: revision,
      projectId: PROJECT_ID,
      rootId: PR_ID,
    }),
    true,
  );
  assert.equal(
    queryClient.getQueryData(["project", PROJECT_ID, "pull-requests"])[0]
      .commit,
    "2222222222222222222222222222222222222222",
  );
});

test("another project's caches are left alone", () => {
  const queryClient = seededQueryClient();
  const issues = queryClient.getQueryData(["project", PROJECT_ID, "issues"]);

  applyProjectRootEvent(queryClient, {
    event: relayEvent({ kind: 1, rootId: ISSUE_ID }),
    projectId: "other-project",
    rootId: ISSUE_ID,
  });

  assert.equal(
    queryClient.getQueryData(["project", PROJECT_ID, "issues"]),
    issues,
  );
  assert.deepEqual(
    cachedWorkItemIssue(queryClient).comments.map((item) => item.content),
    ["Agent reply"],
    "the cross-project inbox is keyed by root, not by which project is open",
  );
});
