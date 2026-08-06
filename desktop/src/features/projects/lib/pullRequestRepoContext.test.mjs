import assert from "node:assert/strict";
import test from "node:test";

import {
  projectCheckoutCloneUrl,
  pullRequestRepoOwner,
  pullRequestTargetCloneUrl,
  resolvePullRequestRepoContext,
} from "./pullRequestRepoContext.ts";

const OWNER = "a".repeat(64);
const OTHER_OWNER = "b".repeat(64);
const REPO_ADDRESS = `30617:${OWNER}:buzz`;
const OTHER_REPO_ADDRESS = `30617:${OTHER_OWNER}:other`;
const PROJECT_CLONE = "https://relay.example/git/owner/buzz";
const PR_CLONE = "https://relay.example/git/owner/buzz.git";

function project(overrides = {}) {
  return {
    cloneUrls: [PROJECT_CLONE],
    defaultBranch: "main",
    dtag: "buzz",
    owner: OWNER,
    repoAddress: REPO_ADDRESS,
    ...overrides,
  };
}

function pullRequest(overrides = {}) {
  return {
    cloneUrls: [PR_CLONE],
    repoAddress: REPO_ADDRESS,
    targetBranch: "release",
    ...overrides,
  };
}

test("a selected project supplies the context unchanged", () => {
  // Flows that already have a workspace selection must keep acting on exactly
  // the repo they acted on before — this fix only fills gaps.
  const resolved = resolvePullRequestRepoContext(project(), pullRequest());

  assert.equal(resolved.ok, true);
  assert.deepEqual(resolved.context, {
    owner: OWNER,
    repoAddress: REPO_ADDRESS,
    dtag: "buzz",
    cloneUrls: [PROJECT_CLONE],
    defaultBranch: "main",
    source: "project",
  });
});

test("a selection with no clone URL borrows the pull request's", () => {
  // The reported bug: `fetchProject` derives a relay-hosted clone URL from a
  // one-shot `getCachedRelayOrigin()` read, and that cache is commonly still
  // null on a cold deep-link into a PR — freezing `cloneUrls: []` into the
  // query cache. The repo does have a URL; the PR root carries it.
  const resolved = resolvePullRequestRepoContext(
    project({ cloneUrls: [] }),
    pullRequest(),
  );

  assert.equal(resolved.ok, true);
  assert.deepEqual(resolved.context.cloneUrls, [PR_CLONE]);
  assert.equal(resolved.context.source, "project");
});

test("a pull request for a different repo never lends its clone URL", () => {
  // Borrowing across repositories would redirect a merge to somebody else's
  // remote just because the selection's URL list came up empty.
  const resolved = resolvePullRequestRepoContext(
    project({ cloneUrls: [] }),
    pullRequest({ repoAddress: OTHER_REPO_ADDRESS }),
  );

  assert.equal(resolved.ok, true);
  assert.deepEqual(resolved.context.cloneUrls, []);
});

test("with no selection the pull request's own coordinate resolves the repo", () => {
  // A PR reached directly — from a notification, or a CLI-authored
  // release-train PR — has no selection behind it, and never needed one.
  const resolved = resolvePullRequestRepoContext(null, pullRequest());

  assert.equal(resolved.ok, true);
  assert.deepEqual(resolved.context, {
    owner: OWNER,
    repoAddress: REPO_ADDRESS,
    dtag: "buzz",
    cloneUrls: [PR_CLONE],
    // The PR's target branch is what it merges into, so callers doing
    // `targetBranch ?? context.defaultBranch` land on the right branch.
    defaultBranch: "release",
    source: "pull-request",
  });
});

test("the repo address is kept verbatim for exact-match relay filters", () => {
  // Relay `#a` filters are exact-match, so re-joining the parsed (lowercased)
  // parts could stop matching the very events this address is used to fetch.
  const mixedCase = `30617:${"A".repeat(64)}:Buzz`;
  const resolved = resolvePullRequestRepoContext(
    null,
    pullRequest({ repoAddress: mixedCase }),
  );

  assert.equal(resolved.ok, true);
  assert.equal(resolved.context.repoAddress, mixedCase);
  assert.equal(resolved.context.owner, "a".repeat(64));
});

test("a pull request naming no target branch falls back to main", () => {
  const resolved = resolvePullRequestRepoContext(
    null,
    pullRequest({ targetBranch: null }),
  );

  assert.equal(resolved.ok, true);
  // Matches eventToProject's own default when an announcement omits
  // `default-branch`.
  assert.equal(resolved.context.defaultBranch, "main");
});

test("a pull request that names no repository reports that, not a selection", () => {
  for (const repoAddress of [null, "", "not-a-coordinate", `1618:${OWNER}:x`]) {
    const resolved = resolvePullRequestRepoContext(
      null,
      pullRequest({ repoAddress }),
    );
    assert.equal(resolved.ok, false);
    assert.equal(
      resolved.error,
      "This pull request does not name a repository.",
    );
  }
});

test("a missing clone URL says so instead of blaming the selection", () => {
  // The whole point: "No project selected." sent people hunting for the wrong
  // problem when the project was right there in front of them.
  const resolved = resolvePullRequestRepoContext(
    project({ cloneUrls: [] }),
    pullRequest({ cloneUrls: [] }),
  );
  assert.equal(resolved.ok, true);

  const target = pullRequestTargetCloneUrl(resolved.context);
  assert.equal(target.ok, false);
  assert.match(target.error, /no clone URL/);
  assert.doesNotMatch(target.error, /No project selected/);
});

test("a present clone URL is returned as the merge target", () => {
  const resolved = resolvePullRequestRepoContext(project(), pullRequest());
  assert.deepEqual(pullRequestTargetCloneUrl(resolved.context), {
    ok: true,
    cloneUrl: PROJECT_CLONE,
  });
});

test("a checkout prefers the announced clone URL over the pull request's", () => {
  assert.equal(
    projectCheckoutCloneUrl(project(), pullRequest()),
    PROJECT_CLONE,
  );
});

test("a checkout borrows an open pull request's clone URL for the same repo", () => {
  assert.equal(
    projectCheckoutCloneUrl(project({ cloneUrls: [] }), pullRequest()),
    PR_CLONE,
  );
});

test("a checkout declines a clone URL from a different repo, or none at all", () => {
  assert.equal(
    projectCheckoutCloneUrl(
      project({ cloneUrls: [] }),
      pullRequest({ repoAddress: OTHER_REPO_ADDRESS }),
    ),
    null,
  );
  assert.equal(projectCheckoutCloneUrl(project({ cloneUrls: [] })), null);
  assert.equal(projectCheckoutCloneUrl(project({ cloneUrls: [] }), null), null);
});

test("the repo owner comes from the selection, else the pull request", () => {
  assert.equal(pullRequestRepoOwner(project(), pullRequest()), OWNER);
  assert.equal(pullRequestRepoOwner(null, pullRequest()), OWNER);
  assert.equal(
    pullRequestRepoOwner(
      null,
      pullRequest({ repoAddress: OTHER_REPO_ADDRESS }),
    ),
    OTHER_OWNER,
  );
  assert.equal(
    pullRequestRepoOwner(null, pullRequest({ repoAddress: null })),
    null,
  );
});
