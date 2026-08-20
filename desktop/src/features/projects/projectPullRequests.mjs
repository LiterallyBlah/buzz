import {
  allowedActorsForProjectRoot,
  getAllTags,
  getImetaTags,
  getTag,
  PROJECT_ROOT_STATUS_KINDS,
  projectDeletionRemoves,
  referencesProjectRoot,
} from "./projectIssues.mjs";

const PROJECT_ROOT_STATUS_KIND_SET = new Set(PROJECT_ROOT_STATUS_KINDS);
const KIND_COMMENT = 1;
const KIND_DELETION = 5;
const KIND_PR_UPDATE = 1619;

/**
 * The root facts every trust and review rule below is written in terms of.
 *
 * A parsed `ProjectPullRequest` already carries exactly these three fields, so
 * a model can be passed wherever these facts are expected. That is what lets
 * the live-merge path re-derive review state from a cached pull request with
 * the rules that built it — the alternative, a second implementation reading
 * models instead of events, is how a merged view and a refetched view start
 * disagreeing about who is a reviewer.
 */
function rootFactsFromEvent(event) {
  return {
    author: event.pubkey,
    repoAddress: getTag(event, "a") ?? null,
    recipients: getAllTags(event, "p"),
  };
}

// Updates and status changes rewrite the PR's tip commit, clone URLs, and
// lifecycle state, so they are only honored when signed by the PR author or
// the repo owner — an arbitrary relay user must not be able to re-point an
// open PR at their own commit/clone URL or flip its status.
//
// Revisions address their root with an uppercase `E`; a lowercase-only kind
// 1619 is not a revision of this pull request (NIP-34 reserves `E` for exactly
// this reference, and the relay's root resolver agrees).
function trustedUpdateEvents(rootId, allowedActors, updateEvents) {
  return updateEvents.filter(
    (event) =>
      allowedActors.has(event.pubkey.toLowerCase()) &&
      getTag(event, "E") === rootId,
  );
}

function latestStatusEvent(rootId, allowedActors, statusEvents) {
  return statusEvents
    .filter(
      (event) =>
        allowedActors.has(event.pubkey.toLowerCase()) &&
        referencesProjectRoot(event, rootId),
    )
    .sort((left, right) => right.created_at - left.created_at)[0];
}

function eventsForPullRequest(pullRequestId, events) {
  return events
    .filter((event) => referencesProjectRoot(event, pullRequestId))
    .sort((left, right) => left.created_at - right.created_at);
}

function getCloneUrls(event) {
  return event.tags
    .filter((tag) => tag[0] === "clone")
    .flatMap((tag) => tag.slice(1))
    .filter(Boolean);
}

function pullRequestStatusFrom(labels, statusEvent) {
  if (statusEvent?.kind === 1630) return "Open";
  if (statusEvent?.kind === 1631) return "Merged";
  if (statusEvent?.kind === 1632) return "Closed";
  if (statusEvent?.kind === 1633) return "Draft";
  return labels.some((label) => label.toLowerCase() === "draft")
    ? "Draft"
    : "Open";
}

/** Keep consecutive lifecycle writes ordered even when they happen within the
 * same whole-second Nostr timestamp. */
export function nextProjectPullRequestStatusCreatedAt(pullRequest, now) {
  return Math.max(now, (pullRequest.statusCreatedAt ?? 0) + 1);
}

/** Keep consecutive decisions ordered across whole-second Nostr timestamps. */
export function nextProjectPullRequestReviewCreatedAt(pullRequest, now) {
  const latestDecisionCreatedAt = [
    ...pullRequest.approvals,
    ...pullRequest.changeRequests,
  ].reduce((latest, decision) => Math.max(latest, decision.createdAt), 0);
  return Math.max(now, latestDecisionCreatedAt + 1);
}

/** Trusted presentation kind for a compact review timeline row. */
export function projectPullRequestCommentTimelineKind(comment) {
  if (comment.isTrustedReviewRequest) return "review-request";
  if (
    !comment.isTrustedReviewDecision ||
    !comment.reviewDecisionStatus ||
    !comment.reviewDecision
  ) {
    return null;
  }
  return comment.reviewDecision;
}

/** Effective review summary shown above the PR review actions. */
export function projectPullRequestReviewSummary(pullRequest) {
  const approvalCount = pullRequest.approvals.length;
  const changeRequestCount = pullRequest.changeRequests.length;
  const isDraft = pullRequest.status === "Draft";
  const state = isDraft
    ? "This review is still a work in progress."
    : changeRequestCount > 0
      ? `${changeRequestCount} reviewer${changeRequestCount === 1 ? "" : "s"} requested changes.`
      : pullRequest.reviewers.length > 0
        ? "Review requested — no approvals yet."
        : "No reviews yet.";

  return {
    approvalCount,
    changeRequestCount,
    detail: isDraft
      ? "Draft reviews cannot be merged."
      : approvalCount === 0 && changeRequestCount === 0
        ? "Approvals from reviewers will show up here."
        : null,
    showState: approvalCount === 0 || changeRequestCount > 0,
    state,
  };
}

/** Effective trusted, current-commit review decision represented by a comment. */
export function projectPullRequestEffectiveReviewDecision(
  pullRequest,
  comment,
) {
  if (pullRequest.approvals.some((decision) => decision.id === comment.id)) {
    return "approved";
  }
  if (
    pullRequest.changeRequests.some((decision) => decision.id === comment.id)
  ) {
    return "changes-requested";
  }
  return null;
}

function eventToPullRequestUpdate(event) {
  return {
    id: event.id,
    content: event.content,
    tags: getImetaTags(event),
    author: event.pubkey,
    createdAt: event.created_at,
    commit: getTag(event, "c") ?? null,
    cloneUrls: getCloneUrls(event),
  };
}

// Review requests and approvals are kind:1 comments labeled with a `t` tag —
// NIP-34 has no dedicated review kinds, and labeled text notes stay readable
// for any client (including `buzz` CLI users) that treats them as comments.
export const PR_REVIEW_REQUEST_LABEL = "review-request";
export const PR_APPROVAL_LABEL = "approval";
export const PR_CHANGES_REQUESTED_LABEL = "changes-requested";
export const PR_INLINE_COMMENT_LABEL = "inline-comment";

/** Validate an inline diff anchor without normalizing attacker-controlled paths. */
export function normalizeProjectPullRequestCommentAnchor(anchor) {
  if (!anchor || typeof anchor.path !== "string") return null;
  const path = anchor.path;
  if (
    path.length === 0 ||
    path.length > 4_096 ||
    path.startsWith("/") ||
    path.includes("\\") ||
    path.includes("\0") ||
    path
      .split("/")
      .some((segment) => !segment || segment === "." || segment === "..")
  ) {
    return null;
  }
  if (anchor.side !== "old" && anchor.side !== "new") return null;
  if (!Number.isSafeInteger(anchor.line) || anchor.line < 1) return null;
  return { line: anchor.line, path, side: anchor.side };
}

function eventToPullRequestComment(event) {
  const labels = getAllTags(event, "t").map((label) => label.toLowerCase());
  const isReviewRequest = labels.includes(PR_REVIEW_REQUEST_LABEL);
  const isApproval = labels.includes(PR_APPROVAL_LABEL);
  const isChangeRequest = labels.includes(PR_CHANGES_REQUESTED_LABEL);
  const lineTag = getTag(event, "line");
  const parsedLine =
    lineTag && /^[1-9]\d*$/.test(lineTag) ? Number(lineTag) : Number.NaN;
  const anchor =
    isReviewRequest || isApproval
      ? null
      : normalizeProjectPullRequestCommentAnchor({
          line: parsedLine,
          path: getTag(event, "file"),
          side: getTag(event, "side"),
        });
  return {
    id: event.id,
    content: event.content,
    tags: getImetaTags(event),
    author: event.pubkey,
    createdAt: event.created_at,
    commit: getTag(event, "c") ?? null,
    anchor,
    isInlineComment:
      Boolean(anchor) || labels.includes(PR_INLINE_COMMENT_LABEL),
    isApproval,
    isChangeRequest,
    isReviewRequest,
    reviewDecision:
      isApproval === isChangeRequest
        ? null
        : isApproval
          ? "approved"
          : "changes-requested",
    // For review requests the `p` tags are the requested reviewers.
    reviewerPubkeys: isReviewRequest
      ? getAllTags(event, "p").map((pubkey) => pubkey.toLowerCase())
      : [],
  };
}

/**
 * Requested reviewers: `p` tags on the PR root plus `p` tags of trusted
 * review-request comments (signed by the PR author or repo owner). The PR
 * author is never their own reviewer.
 */
function reviewersForPullRequest(facts, comments) {
  const allowedActors = allowedActorsForProjectRoot(facts);
  const reviewers = new Set(
    facts.recipients.map((pubkey) => pubkey.toLowerCase()),
  );
  for (const comment of comments) {
    if (
      comment.isReviewRequest &&
      allowedActors.has(comment.author.toLowerCase())
    ) {
      for (const pubkey of comment.reviewerPubkeys) {
        reviewers.add(pubkey);
      }
    }
  }
  reviewers.delete(facts.author.toLowerCase());
  return [...reviewers];
}

function reviewDecisionCommit(comment, initialCommit) {
  return comment.commit ?? initialCommit;
}

function trustedReviewActors(facts, reviewers) {
  const author = facts.author.toLowerCase();
  const trustedActors = new Set(reviewers);
  for (const actor of allowedActorsForProjectRoot(facts)) {
    if (actor !== author) trustedActors.add(actor);
  }
  return trustedActors;
}

/** Latest trusted, current-commit review decision per author. */
function reviewDecisionsForPullRequest(
  comments,
  trustedActors,
  initialCommit,
  currentCommit,
) {
  const byAuthor = new Map();
  for (const comment of comments) {
    if (!comment.reviewDecision || !currentCommit) continue;
    const key = comment.author.toLowerCase();
    if (!trustedActors.has(key)) continue;
    const commit = reviewDecisionCommit(comment, initialCommit);
    if (commit !== currentCommit) continue;
    const existing = byAuthor.get(key);
    if (
      !existing ||
      comment.createdAt > existing.createdAt ||
      (comment.createdAt === existing.createdAt && comment.id > existing.id)
    ) {
      byAuthor.set(key, { ...comment, commit });
    }
  }
  const decisions = [...byAuthor.values()]
    .map(({ id, author: reviewer, createdAt, commit, reviewDecision }) => ({
      id,
      author: reviewer,
      createdAt,
      commit,
      reviewDecision,
    }))
    .sort(
      (left, right) =>
        left.createdAt - right.createdAt || left.id.localeCompare(right.id),
    );
  return {
    approvals: decisions.filter(
      (decision) => decision.reviewDecision === "approved",
    ),
    changeRequests: decisions.filter(
      (decision) => decision.reviewDecision === "changes-requested",
    ),
  };
}

/**
 * Everything a pull request's review surface is a function of: its comments,
 * who is trusted, and which commit they speak about.
 *
 * Split out of `eventToProjectPullRequest` because the live path re-runs it
 * with the model's own comments after a merge. Re-decorating already-decorated
 * comments is intentional and idempotent: a decorated comment is a parsed
 * comment plus derived fields, and the spread below overwrites exactly those.
 */
function projectPullRequestReviewState(
  facts,
  parsedComments,
  initialCommit,
  latestCommit,
) {
  const reviewers = reviewersForPullRequest(facts, parsedComments);
  const trustedActors = trustedReviewActors(facts, reviewers);
  const trustedReviewRequestActors = allowedActorsForProjectRoot(facts);
  const comments = parsedComments.map((comment) => ({
    ...comment,
    inlineCommentStatus: comment.anchor
      ? latestCommit &&
        reviewDecisionCommit(comment, initialCommit) === latestCommit
        ? "current"
        : "outdated"
      : null,
    isTrustedReviewDecision:
      Boolean(comment.reviewDecision) &&
      trustedActors.has(comment.author.toLowerCase()),
    reviewDecisionStatus:
      comment.reviewDecision && trustedActors.has(comment.author.toLowerCase())
        ? latestCommit &&
          reviewDecisionCommit(comment, initialCommit) === latestCommit
          ? "current"
          : "historical"
        : null,
    isTrustedReviewRequest:
      comment.isReviewRequest &&
      trustedReviewRequestActors.has(comment.author.toLowerCase()),
  }));

  return {
    ...reviewDecisionsForPullRequest(
      comments,
      trustedActors,
      initialCommit,
      latestCommit,
    ),
    comments,
    reviewers,
  };
}

export function eventToProjectPullRequest(
  pullRequest,
  updateEvents = [],
  commentEvents = [],
  statusEvents = [],
) {
  const facts = rootFactsFromEvent(pullRequest);
  const allowedActors = allowedActorsForProjectRoot(facts);
  const trustedUpdates = trustedUpdateEvents(
    pullRequest.id,
    allowedActors,
    updateEvents,
  );
  const latestUpdate = [...trustedUpdates].sort(
    (left, right) => right.created_at - left.created_at,
  )[0];
  const latestStatus = latestStatusEvent(
    pullRequest.id,
    allowedActors,
    statusEvents,
  );
  const updates = eventsForPullRequest(pullRequest.id, trustedUpdates).map(
    eventToPullRequestUpdate,
  );
  const parsedComments = eventsForPullRequest(
    pullRequest.id,
    commentEvents,
  ).map(eventToPullRequestComment);
  const latestCommit = getTag(latestUpdate ?? pullRequest, "c") ?? null;
  const initialCommit = getTag(pullRequest, "c") ?? null;
  const { approvals, changeRequests, comments, reviewers } =
    projectPullRequestReviewState(
      facts,
      parsedComments,
      initialCommit,
      latestCommit,
    );
  const title =
    getTag(pullRequest, "subject") ||
    pullRequest.content.split("\n")[0] ||
    "Untitled review";

  return {
    id: pullRequest.id,
    title,
    content: pullRequest.content,
    tags: getImetaTags(pullRequest),
    author: pullRequest.pubkey,
    createdAt: pullRequest.created_at,
    repoAddress: getTag(pullRequest, "a") ?? null,
    channelId: getTag(pullRequest, "h") ?? null,
    originAgentName: getTag(pullRequest, "buzz-origin-agent") ?? null,
    labels: getAllTags(pullRequest, "t"),
    recipients: getAllTags(pullRequest, "p"),
    reviewers,
    approvals,
    changeRequests,
    status: pullRequestStatusFrom(getAllTags(pullRequest, "t"), latestStatus),
    statusEventId: latestStatus?.id ?? null,
    statusCreatedAt: latestStatus?.created_at ?? null,
    branchName: getTag(pullRequest, "branch-name") ?? null,
    targetBranch: getTag(pullRequest, "target-branch") ?? null,
    initialCommit,
    commit: latestCommit,
    cloneUrls: getCloneUrls(latestUpdate ?? pullRequest),
    updateCount: updates.length,
    updatedAt:
      [
        ...updates,
        ...comments,
        ...(latestStatus
          ? [
              {
                createdAt: latestStatus.created_at,
              },
            ]
          : []),
      ].sort((left, right) => right.createdAt - left.createdAt)[0]?.createdAt ??
      latestUpdate?.created_at ??
      pullRequest.created_at,
    updates,
    comments,
  };
}

export function projectPullRequestEventsToPullRequests(
  pullRequestEvents,
  updateEvents = [],
  commentEvents = [],
  statusEvents = [],
) {
  return [...pullRequestEvents]
    .map((pullRequest) =>
      eventToProjectPullRequest(
        pullRequest,
        updateEvents,
        commentEvents,
        statusEvents,
      ),
    )
    .sort((left, right) => right.updatedAt - left.updatedAt);
}

function pullRequestWithComment(pullRequest, event) {
  if (!referencesProjectRoot(event, pullRequest.id)) return pullRequest;
  // Dedupe by event id. The live filter overlaps what the last fetch already
  // returned and a reconnect replays that overlap, so the identical reference
  // returned here is what keeps a replay from duplicating a comment row.
  if (pullRequest.comments.some((comment) => comment.id === event.id)) {
    return pullRequest;
  }

  const parsedComments = [
    ...pullRequest.comments,
    eventToPullRequestComment(event),
  ].sort((left, right) => left.createdAt - right.createdAt);

  return {
    ...pullRequest,
    ...projectPullRequestReviewState(
      pullRequest,
      parsedComments,
      pullRequest.initialCommit,
      pullRequest.commit,
    ),
    updatedAt: Math.max(pullRequest.updatedAt, event.created_at),
  };
}

/**
 * The pull request's `updatedAt`, recomputed the way
 * `eventToProjectPullRequest` derives it, so a list merged after a deletion
 * and a list refetched afterwards agree on order.
 */
function pullRequestUpdatedAt(pullRequest, comments) {
  return (
    [
      ...pullRequest.updates.map((update) => update.createdAt),
      ...comments.map((comment) => comment.createdAt),
      ...(pullRequest.statusCreatedAt === null
        ? []
        : [pullRequest.statusCreatedAt]),
    ].sort((left, right) => right - left)[0] ?? pullRequest.createdAt
  );
}

function pullRequestWithoutComment(pullRequest, event) {
  const parsedComments = pullRequest.comments.filter(
    (comment) => !projectDeletionRemoves(event, comment),
  );
  if (parsedComments.length === pullRequest.comments.length) {
    return pullRequest;
  }

  return {
    ...pullRequest,
    // A deleted approval or change request is no longer a review decision, and
    // a deleted review request no longer names a reviewer, so the whole review
    // surface is re-derived from what is left rather than patched in place.
    ...projectPullRequestReviewState(
      pullRequest,
      parsedComments,
      pullRequest.initialCommit,
      pullRequest.commit,
    ),
    updatedAt: pullRequestUpdatedAt(pullRequest, parsedComments),
  };
}

function pullRequestWithUpdate(pullRequest, event) {
  if (getTag(event, "E") !== pullRequest.id) return pullRequest;
  if (
    !allowedActorsForProjectRoot(pullRequest).has(event.pubkey.toLowerCase())
  ) {
    return pullRequest;
  }
  if (pullRequest.updates.some((update) => update.id === event.id)) {
    return pullRequest;
  }

  const updates = [
    ...pullRequest.updates,
    eventToPullRequestUpdate(event),
  ].sort((left, right) => left.createdAt - right.createdAt);
  // The newest revision owns the tip commit and clone URLs, exactly as
  // `eventToProjectPullRequest` reads them off the latest update event. A
  // late-arriving older revision therefore changes the timeline without
  // re-pointing the branch.
  const latestUpdate = updates.reduce(
    (latest, update) =>
      !latest || update.createdAt > latest.createdAt ? update : latest,
    null,
  );
  const latestCommit = latestUpdate?.commit ?? null;

  return {
    ...pullRequest,
    // Review decisions and inline comments are scoped to the commit they were
    // written against, so a new tip re-dates every one of them.
    ...projectPullRequestReviewState(
      pullRequest,
      pullRequest.comments,
      pullRequest.initialCommit,
      latestCommit,
    ),
    cloneUrls: latestUpdate?.cloneUrls ?? pullRequest.cloneUrls,
    commit: latestCommit,
    updateCount: updates.length,
    updatedAt: Math.max(pullRequest.updatedAt, event.created_at),
    updates,
  };
}

function pullRequestWithStatus(pullRequest, event) {
  if (!referencesProjectRoot(event, pullRequest.id)) return pullRequest;
  if (
    !allowedActorsForProjectRoot(pullRequest).has(event.pubkey.toLowerCase())
  ) {
    return pullRequest;
  }
  // Only a strictly newer status wins, matching `latestStatusEvent`: a replayed
  // or out-of-order lifecycle event must not reopen a merged pull request.
  if (
    pullRequest.statusCreatedAt !== null &&
    event.created_at <= pullRequest.statusCreatedAt
  ) {
    return pullRequest;
  }

  return {
    ...pullRequest,
    status: pullRequestStatusFrom(pullRequest.labels, event),
    statusEventId: event.id,
    statusCreatedAt: event.created_at,
    updatedAt: Math.max(pullRequest.updatedAt, event.created_at),
  };
}

/**
 * Fold one live event into an already-parsed pull request.
 *
 * The query cache holds parsed pull requests, not the events they were built
 * from, so the live path cannot re-run `eventToProjectPullRequest`. It applies
 * the same rules incrementally instead, and returns the input untouched when
 * the event says nothing about this pull request — an unrelated root, an
 * untrusted signer, or a duplicate all take that path.
 */
export function mergeProjectPullRequestEvent(pullRequest, event) {
  if (event.kind === KIND_PR_UPDATE) {
    return pullRequestWithUpdate(pullRequest, event);
  }
  if (event.kind === KIND_COMMENT) {
    return pullRequestWithComment(pullRequest, event);
  }
  if (event.kind === KIND_DELETION) {
    return pullRequestWithoutComment(pullRequest, event);
  }
  if (PROJECT_ROOT_STATUS_KIND_SET.has(event.kind)) {
    return pullRequestWithStatus(pullRequest, event);
  }
  return pullRequest;
}

/**
 * The list form. Re-sorts on the key
 * `projectPullRequestEventsToPullRequests` sorts on so a merged list and a
 * refetched list agree on order.
 *
 * Deleting the pull request itself is filtered here rather than merged:
 * `mergeProjectPullRequestEvent` can return a different pull request, never
 * no pull request.
 */
export function mergeProjectPullRequestsEvent(pullRequests, event) {
  const kept = pullRequests.filter(
    (pullRequest) => !projectDeletionRemoves(event, pullRequest),
  );
  let changed = kept.length !== pullRequests.length;
  const merged = kept.map((pullRequest) => {
    const next = mergeProjectPullRequestEvent(pullRequest, event);
    if (next !== pullRequest) changed = true;
    return next;
  });

  return changed
    ? merged.sort((left, right) => right.updatedAt - left.updatedAt)
    : pullRequests;
}
