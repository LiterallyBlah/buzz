import assert from "node:assert/strict";
import test from "node:test";

import {
  projectIssueEventInput,
  projectIssueStatusEventInput,
} from "./issueMutations.ts";

const OWNER = "a".repeat(64);
const AUTHOR = "b".repeat(64);
const MENTIONED = "c".repeat(64);
const ISSUE_ID = "d".repeat(64);
const REPO_ADDRESS = `30617:${OWNER}:demo`;

const project = {
  id: "project-1",
  name: "demo",
  owner: OWNER,
  repoAddress: REPO_ADDRESS,
};

const issue = {
  id: ISSUE_ID,
  author: AUTHOR,
  repoAddress: REPO_ADDRESS,
  status: "Backlog",
  statusCreatedAt: null,
};

function tagValues(event, key) {
  return event.tags.filter((tag) => tag[0] === key).map((tag) => tag[1]);
}

// The dialog collects pubkeys; this is the step that decides whether they
// reach the submitted event at all. Asserting only the tag builder would pass
// with `recipients` dropped on the floor right here.
test("mentions selected in the dialog reach the submitted issue event", () => {
  const event = projectIssueEventInput(project, {
    title: "Needs eyes",
    body: " body ",
    recipients: [MENTIONED],
  });

  assert.equal(event.kind, 1621);
  assert.equal(event.content, "body");
  assert.deepEqual(tagValues(event, "p"), [OWNER, MENTIONED]);
  assert.deepEqual(tagValues(event, "a"), [REPO_ADDRESS]);
});

test("creating an issue without touching the picker is unchanged", () => {
  const withoutField = projectIssueEventInput(project, {
    title: "Plain",
    body: "",
  });
  const withEmpty = projectIssueEventInput(project, {
    title: "Plain",
    body: "",
    recipients: [],
  });

  assert.deepEqual(tagValues(withoutField, "p"), [OWNER]);
  assert.deepEqual(withoutField, withEmpty);
});

test("a status change is addressed to its own root and repository", () => {
  const event = projectIssueStatusEventInput({
    issue,
    now: 1_000,
    project,
    status: "closed",
  });

  assert.equal(event.kind, 1632);
  assert.equal(event.content, "");
  assert.deepEqual(event.tags[0], ["e", ISSUE_ID, "", "root"]);
  assert.deepEqual(tagValues(event, "a"), [REPO_ADDRESS]);
  assert.deepEqual(tagValues(event, "p"), [OWNER, AUTHOR]);
});

test("each lifecycle state publishes the kind the reader recognises", () => {
  // Publishing a kind `statusFromEvent` does not read would leave the panel
  // showing the old status after an accepted event.
  const kindFor = (status) =>
    projectIssueStatusEventInput({ issue, now: 1, project, status }).kind;
  assert.equal(kindFor("open"), 1630);
  assert.equal(kindFor("resolved"), 1631);
  assert.equal(kindFor("closed"), 1632);
  assert.equal(kindFor("draft"), 1633);
});

test("a second status change in the same second still lands after the first", () => {
  const afterClose = { ...issue, statusCreatedAt: 1_000 };
  assert.equal(
    projectIssueStatusEventInput({
      issue: afterClose,
      now: 1_000,
      project,
      status: "open",
    }).createdAt,
    1_001,
  );
});

// Route binding, carried over from the earlier phases: a status change on one
// root must not be addressed to another. The issue's own repository wins over
// the project's, so a stale project selection cannot redirect it.
test("the status event follows the issue, not the surrounding project", () => {
  const otherRepo = `30617:${OWNER}:elsewhere`;
  const event = projectIssueStatusEventInput({
    issue: { ...issue, repoAddress: otherRepo },
    now: 1,
    project,
    status: "resolved",
  });
  assert.deepEqual(tagValues(event, "a"), [otherRepo]);
  assert.deepEqual(event.tags[0], ["e", ISSUE_ID, "", "root"]);
});
