export const PROJECT_ISSUE_STATUS = {
  TRIAGE: "Triage",
  BACKLOG: "Backlog",
  IN_PROGRESS: "In Progress",
  IN_REVIEW: "In Review",
  DONE: "Done",
  CLOSED: "Closed",
};

function isNonEmptyString(value) {
  return typeof value === "string" && value.length > 0;
}

export function getTag(event, name) {
  const value = event.tags.find((tag) => tag[0] === name)?.[1];
  return isNonEmptyString(value) ? value : undefined;
}

export function getAllTags(event, name) {
  return event.tags
    .filter((tag) => tag[0] === name && isNonEmptyString(tag[1]))
    .map((tag) => tag[1]);
}

export function getImetaTags(event) {
  return event.tags.filter((tag) => tag[0] === "imeta");
}

function repoOwnerFromAddress(repoAddress) {
  const owner = (repoAddress ?? "").split(":")[1] ?? "";
  return /^[a-fA-F0-9]{64}$/.test(owner) ? owner.toLowerCase() : null;
}

/**
 * Pubkeys allowed to change a root event's lifecycle (status, updates):
 * the root author and the owner of the repo the root event targets.
 * Anyone else's status/update events are ignored (NIP-34 scopes these
 * to the root author or a maintainer).
 */
export function allowedActorsForRoot(rootEvent) {
  return lifecycleActors(rootEvent.pubkey, getTag(rootEvent, "a"));
}

/**
 * The same rule, for a root that has already been parsed into a model.
 *
 * Issues and pull requests share it: both models carry the author and repo
 * coordinate the rule is written in terms of, and the live-merge path can only
 * stay consistent with the fetch path if it asks the same question of the
 * parsed root that the fetch asks of the event.
 */
export function allowedActorsForProjectRoot(root) {
  return lifecycleActors(root.author, root.repoAddress);
}

/**
 * The one place the rule lives.
 *
 * Two adapters, one predicate: a UI that decided who may change a status by a
 * second implementation would eventually offer a control whose events the
 * reader discards — a button that publishes and changes nothing.
 */
function lifecycleActors(authorPubkey, repoAddress) {
  const allowed = new Set([String(authorPubkey ?? "").toLowerCase()]);
  const owner = repoOwnerFromAddress(repoAddress);
  if (owner) allowed.add(owner);
  return allowed;
}

/**
 * NIP-34 lifecycle kinds, inlined rather than imported so this module stays a
 * dependency-free transform (it is loaded by tests and by the work-item
 * aggregator alike).
 */
export const PROJECT_ROOT_STATUS_KINDS = [1630, 1631, 1632, 1633];
const PROJECT_ROOT_STATUS_KIND_SET = new Set(PROJECT_ROOT_STATUS_KINDS);
const KIND_COMMENT = 1;

/**
 * Does an event address this root?
 *
 * Comments and statuses name their root with a lowercase `e`; only pull-request
 * revisions (kind 1619) use the uppercase `E`. Readers accept either case for
 * comments because third-party clients have shipped both, which is why
 * `allowUppercase` exists rather than a single hardcoded tag name — a status
 * write path that accepted `E` would let a revision-shaped event flip a
 * lifecycle.
 */
export function referencesProjectRoot(event, rootId, allowUppercase = true) {
  return event.tags.some(
    (tag) =>
      (tag[0] === "e" || (allowUppercase && tag[0] === "E")) &&
      tag[1] === rootId,
  );
}

function latestStatusForIssue(issue, statusEvents) {
  const allowedActors = allowedActorsForRoot(issue);
  return statusEvents
    .filter(
      (event) =>
        allowedActors.has(event.pubkey.toLowerCase()) &&
        referencesProjectRoot(event, issue.id, false),
    )
    .sort((left, right) => right.created_at - left.created_at)[0];
}

function issueStatusFrom(labels, statusEvent) {
  if (statusEvent?.kind === 1631) return PROJECT_ISSUE_STATUS.DONE;
  if (statusEvent?.kind === 1632) return PROJECT_ISSUE_STATUS.CLOSED;
  // NIP-34 calls 1633 "Draft"; we surface it as Triage for issues. The
  // label-based fallbacks below are client-side heuristics, not protocol.
  if (statusEvent?.kind === 1633) return PROJECT_ISSUE_STATUS.TRIAGE;

  const normalized = labels.map((label) => label.toLowerCase());
  if (normalized.includes("in-review") || normalized.includes("review")) {
    return PROJECT_ISSUE_STATUS.IN_REVIEW;
  }
  if (normalized.includes("in-progress") || normalized.includes("active")) {
    return PROJECT_ISSUE_STATUS.IN_PROGRESS;
  }
  if (normalized.includes("triage")) return PROJECT_ISSUE_STATUS.TRIAGE;
  return PROJECT_ISSUE_STATUS.BACKLOG;
}

function eventToIssueComment(event) {
  return {
    id: event.id,
    content: event.content,
    tags: getImetaTags(event),
    author: event.pubkey,
    createdAt: event.created_at,
  };
}

function commentsForIssue(issueId, commentEvents) {
  return commentEvents
    .filter((event) => referencesProjectRoot(event, issueId))
    .sort((left, right) => left.created_at - right.created_at)
    .map(eventToIssueComment);
}

export function eventToProjectIssue(
  issue,
  statusEvents = [],
  commentEvents = [],
) {
  const latestStatus = latestStatusForIssue(issue, statusEvents);
  const comments = commentsForIssue(issue.id, commentEvents);
  const title =
    getTag(issue, "subject") ||
    issue.content.split("\n")[0] ||
    "Untitled issue";

  return {
    id: issue.id,
    title,
    content: issue.content,
    tags: getImetaTags(issue),
    author: issue.pubkey,
    createdAt: issue.created_at,
    repoAddress: getTag(issue, "a") ?? null,
    labels: getAllTags(issue, "t"),
    recipients: getAllTags(issue, "p"),
    status: issueStatusFrom(getAllTags(issue, "t"), latestStatus),
    statusEventId: latestStatus?.id ?? null,
    statusCreatedAt: latestStatus?.created_at ?? null,
    updatedAt:
      [
        ...comments,
        ...(latestStatus ? [{ createdAt: latestStatus.created_at }] : []),
      ].sort((left, right) => right.createdAt - left.createdAt)[0]?.createdAt ??
      issue.created_at,
    comments,
  };
}

export function projectIssueEventsToIssues(
  issueEvents,
  statusEvents = [],
  commentEvents = [],
) {
  return [...issueEvents]
    .map((issue) => eventToProjectIssue(issue, statusEvents, commentEvents))
    .sort((left, right) => right.updatedAt - left.updatedAt);
}

function issueWithComment(issue, event) {
  if (!referencesProjectRoot(event, issue.id)) return issue;
  // Dedupe by event id: the live subscription overlaps its `since` window with
  // what the last fetch already returned, and a reconnect replays the overlap
  // again. Returning the identical reference is what makes that free — the
  // query cache treats an unchanged object as "no update", so a replayed
  // comment costs neither a duplicate row nor a render.
  if (issue.comments.some((comment) => comment.id === event.id)) return issue;

  return {
    ...issue,
    comments: [...issue.comments, eventToIssueComment(event)].sort(
      (left, right) => left.createdAt - right.createdAt,
    ),
    updatedAt: Math.max(issue.updatedAt, event.created_at),
  };
}

function issueWithStatus(issue, event) {
  if (!referencesProjectRoot(event, issue.id, false)) return issue;
  if (!allowedActorsForProjectRoot(issue).has(event.pubkey.toLowerCase())) {
    return issue;
  }
  // `latestStatusForIssue` keeps the newest status. A live event therefore only
  // wins when it is strictly newer: an equal-or-older replay must not roll the
  // panel back to a status the reader would discard on the next refetch.
  if (
    issue.statusCreatedAt !== null &&
    event.created_at <= issue.statusCreatedAt
  ) {
    return issue;
  }

  return {
    ...issue,
    status: issueStatusFrom(issue.labels, event),
    statusEventId: event.id,
    statusCreatedAt: event.created_at,
    updatedAt: Math.max(issue.updatedAt, event.created_at),
  };
}

/**
 * Fold one live event into an already-parsed issue.
 *
 * The live path cannot re-run `eventToProjectIssue`: the query cache holds
 * parsed issues, not the events they were built from. So this applies the same
 * reader rules incrementally — same root matching, same trust check, same
 * status precedence — and returns the input untouched when the event says
 * nothing about this issue. An unrelated root, an unrelated kind, or a
 * duplicate all take that path, so callers can hand every live event to every
 * cached issue without checking first.
 */
export function mergeProjectIssueEvent(issue, event) {
  if (event.kind === KIND_COMMENT) return issueWithComment(issue, event);
  if (PROJECT_ROOT_STATUS_KIND_SET.has(event.kind)) {
    return issueWithStatus(issue, event);
  }
  return issue;
}

/**
 * The list form. Re-sorts on the same key `projectIssueEventsToIssues` sorts
 * on so a merged list and a refetched list agree on order — otherwise a live
 * comment would move an issue in the list only after the next fetch.
 */
export function mergeProjectIssuesEvent(issues, event) {
  let changed = false;
  const merged = issues.map((issue) => {
    const next = mergeProjectIssueEvent(issue, event);
    if (next !== issue) changed = true;
    return next;
  });

  return changed
    ? merged.sort((left, right) => right.updatedAt - left.updatedAt)
    : issues;
}

export function buildGitIssueTags({
  repoAddress,
  repoOwner,
  title,
  labels = [],
  recipients = [],
}) {
  if (!repoAddress.startsWith("30617:")) {
    throw new Error("Issue repo address must reference a kind:30617 repo.");
  }
  if (!/^[a-fA-F0-9]{64}$/.test(repoOwner)) {
    throw new Error("Repo owner must be 64 hex characters.");
  }
  const subject = title.trim();
  if (!subject) {
    throw new Error("Issue title is required.");
  }
  if (subject.length > 256) {
    throw new Error("Issue title must be 256 characters or fewer.");
  }

  const tags = [
    ["a", repoAddress],
    ["p", repoOwner.toLowerCase()],
    ["subject", subject],
  ];

  for (const label of labels) {
    const trimmed = label.trim();
    if (trimmed) tags.push(["t", trimmed]);
  }

  // Mentioned participants. The repo owner is already tagged above, so a
  // selection that includes them adds nothing: a duplicate `p` would notify
  // once and read as two participants everywhere the tag list is rendered.
  const tagged = new Set([repoOwner.toLowerCase()]);
  for (const recipient of recipients) {
    const pubkey = String(recipient ?? "").toLowerCase();
    if (!/^[a-f0-9]{64}$/.test(pubkey)) {
      throw new Error("Mentioned pubkeys must be 64 hex characters.");
    }
    if (tagged.has(pubkey)) continue;
    tagged.add(pubkey);
    tags.push(["p", pubkey]);
  }

  return tags;
}

export function buildGitStatusTags({
  issueId,
  repoAddress,
  repoOwner,
  issueAuthor,
}) {
  if (!/^[a-fA-F0-9]{64}$/.test(issueId)) {
    throw new Error("Issue ID must be 64 hex characters.");
  }
  const tags = [["e", issueId, "", "root"]];
  if (repoAddress) tags.push(["a", repoAddress]);
  // Owner and author both, deduped. `allowedActorsForRoot` trusts exactly
  // these two to change a root's lifecycle, so they are also the two people a
  // status change is about — and an author who is not notified learns their
  // issue was closed only by looking.
  const tagged = new Set();
  for (const pubkey of [repoOwner, issueAuthor]) {
    const normalized = String(pubkey ?? "").toLowerCase();
    if (!/^[a-f0-9]{64}$/.test(normalized) || tagged.has(normalized)) continue;
    tagged.add(normalized);
    tags.push(["p", normalized]);
  }
  return tags;
}

/**
 * Keep consecutive status changes ordered across whole-second Nostr timestamps.
 *
 * `latestStatusForIssue` picks by `created_at` descending, so two changes in
 * the same second resolve by sort tie-break rather than by what the person
 * actually did last. The same guard exists for pull requests
 * (`nextProjectPullRequestStatusCreatedAt`) for the same reason.
 */
export function nextProjectIssueStatusCreatedAt(issue, now) {
  return Math.max(now, (issue.statusCreatedAt ?? 0) + 1);
}
