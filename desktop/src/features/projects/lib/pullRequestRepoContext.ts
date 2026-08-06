/**
 * Resolving the repository a pull request write acts on.
 *
 * Every PR-side write — merging, publishing a status, requesting a review,
 * approving — needs the same three facts: which repo (`a`-tag coordinate), who
 * owns it (the signing/recipient pubkey), and, for git-side work, a clone URL.
 * Those flows used to read all three off a `Project` handed down from a
 * workspace selection, and threw `"No project selected."` when it was missing.
 *
 * That binding is wrong twice over:
 *
 *  1. A PR reached directly has no selection behind it. Desktop notifications
 *     navigate by project id and the inbox matches work items by repo address,
 *     so the object usually arrives — but nothing about merging a PR actually
 *     requires it, because the PR root already names its repository.
 *
 *  2. Even *with* a selection the guard fired, because it tested
 *     `project.cloneUrls[0]` rather than `project`. `eventToProject` derives a
 *     relay-hosted clone URL for announcements that carry no `clone` tag (see
 *     lib/projectCloneUrl.ts), but it does so from a one-shot synchronous
 *     `getCachedRelayOrigin()` read taken inside the project fetch. That cache
 *     "is commonly still null on a component's first render"
 *     (shared/lib/useRelayOrigin.ts) — so landing straight on a PR from a cold
 *     start resolves the project *before* the relay origin, and freezes
 *     `cloneUrls: []` into the react-query cache. The repo has a clone URL; the
 *     selection just missed it.
 *
 * A PR root carries its own `clone` tags (written from the project's clone URLs
 * at creation, and by buzz-sdk for CLI-authored PRs), so it can supply exactly
 * what the stale selection lacks. This module resolves the context from the
 * selection when there is one and from the PR's own tags when there is not, and
 * reports what is genuinely missing when neither can answer.
 */

import type { Repository as Project } from "../hooks";
import type { ProjectPullRequest } from "../projectPullRequests.mjs";
import { parseRepoCoordinate } from "./repoCoordinate";

/** The subset of a `Project` that identifies its repository. */
export type ProjectRepoFacts = Pick<
  Project,
  "cloneUrls" | "defaultBranch" | "dtag" | "owner" | "repoAddress"
>;

/** The subset of a `ProjectPullRequest` that identifies its repository. */
export type PullRequestRepoFacts = Pick<
  ProjectPullRequest,
  "cloneUrls" | "repoAddress" | "targetBranch"
>;

export type PullRequestRepoContext = {
  /** Repo owner pubkey — `targetOwner` for signing, and a status recipient. */
  owner: string;
  /** The `a`-tag coordinate, verbatim as relays index it. */
  repoAddress: string;
  /** The announcement's `d` tag — names the local checkout directory. */
  dtag: string;
  /** Clone URLs to reach the repo, most preferred first. May be empty. */
  cloneUrls: string[];
  /** Branch to merge into when the PR names no explicit target branch. */
  defaultBranch: string;
  /** Where the facts came from — useful in tests and when logging. */
  source: "project" | "pull-request";
};

export type PullRequestRepoContextResult =
  | { ok: true; context: PullRequestRepoContext }
  | { ok: false; error: string };

/** `eventToProject`'s default when an announcement omits `default-branch`. */
const FALLBACK_DEFAULT_BRANCH = "main";

function sameRepo(a: string | null | undefined, b: string | null | undefined) {
  if (!a || !b) return false;
  return a.toLowerCase() === b.toLowerCase();
}

/**
 * Resolves the repository context for a pull-request write.
 *
 * A selection wins when present — flows that already have one keep behaving
 * exactly as before, including which repo they act on. The PR only fills the
 * gaps: its clone URLs stand in when the announcement advertised none, and its
 * coordinate supplies the whole context when there is no selection at all.
 */
export function resolvePullRequestRepoContext(
  project: ProjectRepoFacts | null | undefined,
  pullRequest: PullRequestRepoFacts,
): PullRequestRepoContextResult {
  if (project) {
    // Borrow the PR's clone URLs only when it targets this very repo. A PR
    // whose `a` tag names somewhere else must never redirect a merge to that
    // other remote just because the selection's URL list came up empty.
    const canBorrow = sameRepo(project.repoAddress, pullRequest.repoAddress);
    return {
      ok: true,
      context: {
        owner: project.owner,
        repoAddress: project.repoAddress,
        dtag: project.dtag,
        cloneUrls:
          project.cloneUrls.length > 0
            ? project.cloneUrls
            : canBorrow
              ? pullRequest.cloneUrls
              : [],
        defaultBranch: project.defaultBranch,
        source: "project",
      },
    };
  }

  const repoAddress = pullRequest.repoAddress;
  const coordinate = parseRepoCoordinate(repoAddress);
  if (!coordinate || !repoAddress) {
    return {
      ok: false,
      error: "This pull request does not name a repository.",
    };
  }

  return {
    ok: true,
    context: {
      owner: coordinate.owner,
      // Keep the tag verbatim rather than re-joining the parsed parts: relay
      // `#a` filters are exact-match, so a normalized rebuild could stop
      // matching the very events this address is used to fetch and publish.
      repoAddress,
      dtag: coordinate.identifier,
      cloneUrls: pullRequest.cloneUrls,
      // Without an announcement in hand there is no announced default branch.
      // `targetBranch` is what this PR merges into anyway, so callers doing
      // `pullRequest.targetBranch ?? context.defaultBranch` land on the right
      // branch either way, and only fall through to `main` when the PR named
      // no target at all — matching `eventToProject`'s own default.
      defaultBranch: pullRequest.targetBranch ?? FALLBACK_DEFAULT_BRANCH,
      source: "pull-request",
    },
  };
}

export type CloneUrlResult =
  | { ok: true; cloneUrl: string }
  | { ok: false; error: string };

/**
 * The clone URL a git-side operation should target, or a message naming what
 * is actually absent. "No project selected." was never the truth here: by this
 * point the repository is known, it simply advertises nowhere to reach it.
 */
export function pullRequestTargetCloneUrl(
  context: PullRequestRepoContext,
): CloneUrlResult {
  const cloneUrl = context.cloneUrls[0];
  if (!cloneUrl) {
    return {
      ok: false,
      error:
        "This project has no clone URL. Add a clone URL to the repository announcement, or reconnect to the relay that hosts it.",
    };
  }
  return { ok: true, cloneUrl };
}

/**
 * The clone URL for a project checkout, borrowing an open pull request's when
 * the announcement advertises none. Same stale-relay-origin gap as above, for
 * the checkout-scoped flows (clone/pull/push) that always do have a selection.
 */
export function projectCheckoutCloneUrl(
  project: Pick<Project, "cloneUrls" | "repoAddress">,
  pullRequest?: PullRequestRepoFacts | null,
): string | null {
  const announced = project.cloneUrls[0];
  if (announced) return announced;
  if (pullRequest && sameRepo(project.repoAddress, pullRequest.repoAddress)) {
    return pullRequest.cloneUrls[0] ?? null;
  }
  return null;
}

/**
 * The repo owner for a pull request, from the selection when there is one and
 * from the PR's own coordinate otherwise. Used by read-side checks that only
 * need to know who owns the repo (e.g. review eligibility).
 */
export function pullRequestRepoOwner(
  project: Pick<Project, "owner"> | null | undefined,
  pullRequest: Pick<ProjectPullRequest, "repoAddress">,
): string | null {
  if (project) return project.owner;
  return parseRepoCoordinate(pullRequest.repoAddress)?.owner ?? null;
}
