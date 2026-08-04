import {
  KIND_GIT_PR_UPDATE,
  KIND_GIT_STATUS_CLOSED,
  KIND_GIT_STATUS_DRAFT,
  KIND_GIT_STATUS_MERGED,
  KIND_GIT_STATUS_OPEN,
  KIND_TEXT_NOTE,
} from "@/shared/constants/kinds";
import type { RelayEvent } from "@/shared/api/types";

/**
 * Notification decision for project work items (issues / pull requests).
 *
 * Channels answer "should this notify?" with {@link shouldNotifyForEvent},
 * which reasons over NIP-10 thread references inside a channel. Project roots
 * are a different grammar — a kind 1621 issue or 1618 pull request is its own
 * root, referenced by `e`/`E` rather than by an `h` channel tag — so the
 * channel decision function cannot be reused verbatim. This module is the
 * projects-shaped twin: same shape (pure, set-driven, event-id deduped), same
 * author-exclusion rule, different tag grammar.
 *
 * Kept deliberately free of React and of relay/query imports so it stays
 * unit-testable from a plain `.test.mjs` and so the live listener can be
 * reasoned about (and re-tested) independently of the decision itself.
 */

/**
 * Kinds that hang off a work-item root via a *lowercase* `e` tag.
 *
 * - kind 1 comments: `createProjectIssueComment` writes `["e", <rootId>, "", "root"]`.
 * - kinds 1630-1633 statuses: `buildGitStatusTags` writes the same shape.
 *
 * Note that `projectIssues.mjs` / `projectPullRequests.mjs` accept BOTH `e`
 * and `E` when attaching comments to a root, so {@link projectRootIdForEvent}
 * accepts both too rather than mirroring only what Buzz itself publishes —
 * a comment authored by another client must still light up the badge.
 */
export const PROJECT_REPLY_KINDS: readonly number[] = [
  KIND_TEXT_NOTE,
  KIND_GIT_STATUS_OPEN,
  KIND_GIT_STATUS_MERGED,
  KIND_GIT_STATUS_CLOSED,
  KIND_GIT_STATUS_DRAFT,
];

/**
 * Pull-request revision kinds, which reference their root via an *uppercase*
 * `E` tag (see `trustedUpdatesForPullRequest`, which matches on `getTag(event,
 * "E")`). Uppercase `E` needs its own relay filter key, which is why the live
 * listener holds two subscriptions instead of one.
 *
 * NB: the constant is `KIND_GIT_PR_UPDATE = 1619`, not 1618 — 1618 is
 * `KIND_GIT_PULL_REQUEST`, the PR root itself.
 */
export const PROJECT_REVISION_KINDS: readonly number[] = [KIND_GIT_PR_UPDATE];

/** Every kind that can make a watched project root unread. */
export const PROJECT_NOTIFY_KINDS: ReadonlySet<number> = new Set([
  ...PROJECT_REPLY_KINDS,
  ...PROJECT_REVISION_KINDS,
]);

/**
 * Upper bound on the delivered-event guard.
 *
 * Reconnect replay overlaps each live filter by five seconds and the two
 * project filters (`#e` and `#E`) can both match the same event, so the guard
 * must survive a flapping relay. Sized like the channel guard's
 * `SEEN_NOTIFICATION_EVENT_LIMIT` but smaller: project traffic is orders of
 * magnitude thinner than channel traffic.
 */
export const SEEN_PROJECT_EVENT_LIMIT = 1_000;

/**
 * Insertion-ordered bounded set. Returns true the first time an id is seen.
 *
 * Mirrors `trackSeenEvent` in `features/channels/useLiveChannelUpdates` — it
 * is duplicated rather than imported so this module stays free of React and
 * `@tanstack/react-query`, which that module pulls in at import time.
 */
export function trackSeenProjectEvent(
  seenEventIds: Set<string>,
  eventId: string,
  limit = SEEN_PROJECT_EVENT_LIMIT,
): boolean {
  if (seenEventIds.has(eventId)) {
    return false;
  }

  seenEventIds.add(eventId);
  if (seenEventIds.size > limit) {
    const oldestEventId = seenEventIds.values().next().value;
    if (oldestEventId) {
      seenEventIds.delete(oldestEventId);
    }
  }

  return true;
}

/**
 * The watched root this event belongs to, or null when it references none.
 *
 * Matching against the watched set (rather than trusting the first `e` tag)
 * is what makes this safe for threaded replies: a reply to a comment carries
 * an `e` tag for the parent comment *and* one for the root, and only the root
 * is ever in the watched set.
 */
export function projectRootIdForEvent(
  event: Pick<RelayEvent, "tags">,
  watchedRootIds: ReadonlySet<string>,
): string | null {
  for (const tag of event.tags) {
    if (tag[0] !== "e" && tag[0] !== "E") {
      continue;
    }

    const rootId = tag[1];
    if (rootId && watchedRootIds.has(rootId)) {
      return rootId;
    }
  }

  return null;
}

export type ProjectNotifySkipReason =
  /** Not a comment, status, or revision — e.g. a reaction or a repo announcement. */
  | "kind-not-notifiable"
  /** Your own comment must never make your own issue unread. */
  | "self-authored"
  /** References no root the user authored or participated in. */
  | "root-not-watched"
  /** Already delivered — relay replay, or both `#e` and `#E` filters matched. */
  | "already-delivered";

export type ProjectNotifyDecision =
  | { notify: true; rootId: string }
  | { notify: false; reason: ProjectNotifySkipReason };

export type ProjectNotifyOptions = {
  /** Lowercased pubkey of the signed-in user. Empty disables author exclusion. */
  currentPubkey: string;
  /** Root event ids the user authored or participated in. */
  watchedRootIds: ReadonlySet<string>;
  /**
   * Delivered-event guard, mutated in place. Optional so tests (and any
   * caller that dedupes elsewhere) can exercise the pure decision alone.
   */
  seenEventIds?: Set<string>;
};

/**
 * Decide whether an incoming project event should surface to the user.
 *
 * Order matters: the cheap structural rejections (kind, author, root) run
 * before the dedupe guard, so the guard only ever accumulates ids for events
 * that would otherwise have been delivered. That is the same ordering
 * `useLiveChannelUpdates` uses, and it keeps the bounded set from being
 * evicted by traffic that was never going to notify.
 */
export function decideProjectNotification(
  event: Pick<RelayEvent, "id" | "kind" | "pubkey" | "tags">,
  options: ProjectNotifyOptions,
): ProjectNotifyDecision {
  const { currentPubkey, watchedRootIds, seenEventIds } = options;

  if (!PROJECT_NOTIFY_KINDS.has(event.kind)) {
    return { notify: false, reason: "kind-not-notifiable" };
  }

  if (
    currentPubkey.length > 0 &&
    event.pubkey.toLowerCase() === currentPubkey.toLowerCase()
  ) {
    return { notify: false, reason: "self-authored" };
  }

  const rootId = projectRootIdForEvent(event, watchedRootIds);
  if (rootId === null) {
    return { notify: false, reason: "root-not-watched" };
  }

  if (seenEventIds && !trackSeenProjectEvent(seenEventIds, event.id)) {
    return { notify: false, reason: "already-delivered" };
  }

  return { notify: true, rootId };
}
