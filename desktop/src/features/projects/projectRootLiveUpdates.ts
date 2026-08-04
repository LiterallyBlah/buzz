import type { QueryClient } from "@tanstack/react-query";

import type { RelaySubscriptionFilter } from "@/shared/api/relayClientShared";
import type { RelayEvent } from "@/shared/api/types";
import {
  KIND_GIT_PR_UPDATE,
  KIND_GIT_STATUS_CLOSED,
  KIND_GIT_STATUS_DRAFT,
  KIND_GIT_STATUS_MERGED,
  KIND_GIT_STATUS_OPEN,
  KIND_TEXT_NOTE,
} from "@/shared/constants/kinds";
import type { ProjectIssue } from "./projectIssues.mjs";
import {
  mergeProjectIssueEvent,
  mergeProjectIssuesEvent,
  referencesProjectRoot,
} from "./projectIssues.mjs";
import type { ProjectPullRequest } from "./projectPullRequests.mjs";
import {
  mergeProjectPullRequestEvent,
  mergeProjectPullRequestsEvent,
} from "./projectPullRequests.mjs";
import type { ProjectsWorkItemsResult } from "./projectWorkItems";

const PROJECT_ROOT_STATUS_KINDS = [
  KIND_GIT_STATUS_OPEN,
  KIND_GIT_STATUS_MERGED,
  KIND_GIT_STATUS_CLOSED,
  KIND_GIT_STATUS_DRAFT,
];

const PROJECT_ROOT_STATUS_KIND_SET = new Set(PROJECT_ROOT_STATUS_KINDS);

/**
 * How far back a freshly-opened live subscription looks.
 *
 * A detail view subscribes some time after the query that filled it fetched,
 * and the two boundaries are not the same instant. Without an overlap an event
 * published in that gap is missed by both — the fetch was too early, the
 * subscription too late — and stays invisible until something else refetches.
 * The overlap costs nothing because every merge is keyed by event id, so the
 * events the fetch already returned merge to the identical object.
 */
const PROJECT_ROOT_LIVE_OVERLAP_SECS = 30;

/** What a live event does to a root, or null when it is not about this root. */
export type ProjectRootEventRole = "comment" | "revision" | "status";

/**
 * Which of this root's surfaces an event belongs to.
 *
 * The tag case is the whole decision. NIP-34 has comments, statuses, and peer
 * calls address their root with a lowercase `e`, while a pull-request revision
 * (kind 1619) addresses it with an uppercase `E` — the relay's own root
 * resolver draws the same line. A reader that accepts only `e` silently drops
 * every revision; one that accepts `E` for statuses would let a revision-shaped
 * event flip a lifecycle. Returning null for everything else is what keeps one
 * root's events from rendering under another when a relay over-delivers.
 */
export function projectRootEventRole(
  event: RelayEvent,
  rootId: string,
): ProjectRootEventRole | null {
  if (event.kind === KIND_GIT_PR_UPDATE) {
    return event.tags.some((tag) => tag[0] === "E" && tag[1] === rootId)
      ? "revision"
      : null;
  }
  if (event.kind === KIND_TEXT_NOTE) {
    return referencesProjectRoot(event, rootId) ? "comment" : null;
  }
  if (PROJECT_ROOT_STATUS_KIND_SET.has(event.kind)) {
    return referencesProjectRoot(event, rootId) ? "status" : null;
  }
  return null;
}

/**
 * The live filters for one issue or pull request.
 *
 * Two of them, because a relay filter cannot express "either tag case" and
 * `subscribeLive` takes a single filter. Splitting by tag case rather than
 * merging into one loose filter also keeps each REQ narrow: everything the
 * relay sends is something this root can actually use.
 */
export function projectRootLiveFilters(
  rootId: string,
  nowSeconds: number = Math.floor(Date.now() / 1_000),
): RelaySubscriptionFilter[] {
  const since = nowSeconds - PROJECT_ROOT_LIVE_OVERLAP_SECS;

  return [
    {
      kinds: [KIND_TEXT_NOTE, ...PROJECT_ROOT_STATUS_KINDS],
      "#e": [rootId],
      limit: 200,
      since,
    },
    {
      kinds: [KIND_GIT_PR_UPDATE],
      "#E": [rootId],
      limit: 50,
      since,
    },
  ];
}

type WorkItems = ProjectsWorkItemsResult<{ repoAddress: string }>;

function mergeWorkItems(current: WorkItems, event: RelayEvent): WorkItems {
  let changed = false;
  const issues = current.issues.items.map((item) => {
    const issue = mergeProjectIssueEvent(item.issue, event);
    if (issue === item.issue) return item;
    changed = true;
    return { ...item, issue };
  });
  const pullRequests = current.pullRequests.items.map((item) => {
    const pullRequest = mergeProjectPullRequestEvent(item.pullRequest, event);
    if (pullRequest === item.pullRequest) return item;
    changed = true;
    return { ...item, pullRequest };
  });

  if (!changed) return current;

  return {
    issues: {
      ...current.issues,
      items: issues.sort(
        (left, right) => right.issue.updatedAt - left.issue.updatedAt,
      ),
    },
    pullRequests: {
      ...current.pullRequests,
      items: pullRequests.sort(
        (left, right) =>
          right.pullRequest.updatedAt - left.pullRequest.updatedAt,
      ),
    },
  };
}

/**
 * Fold one live event into every cache that can be showing this root, and
 * report whether anything changed.
 *
 * Three caches hold the same work item: the project's issue list, its
 * pull-request list, and the cross-project work-items query behind the Home
 * inbox. A detail view can be rendered from any of them, so all three are
 * updated rather than the one the caller happens to know about — and each
 * merge is a no-op on the caches that do not hold this root, so writing to all
 * three is cheaper than deciding which one to write to.
 *
 * Merging beats invalidating here because the event already contains the whole
 * change: a comment is a comment, and a refetch would spend a relay round trip
 * to learn the same thing, on a query whose fan-out is four filters wide.
 */
export function applyProjectRootEvent(
  queryClient: QueryClient,
  {
    event,
    projectId,
    rootId,
  }: { event: RelayEvent; projectId: string; rootId: string },
): boolean {
  if (!projectRootEventRole(event, rootId)) return false;

  let changed = false;

  queryClient.setQueryData<ProjectIssue[]>(
    ["project", projectId, "issues"],
    (current) => {
      if (!current) return current;
      const next = mergeProjectIssuesEvent(current, event);
      if (next !== current) changed = true;
      return next;
    },
  );

  queryClient.setQueryData<ProjectPullRequest[]>(
    ["project", projectId, "pull-requests"],
    (current) => {
      if (!current) return current;
      const next = mergeProjectPullRequestsEvent(current, event);
      if (next !== current) changed = true;
      return next;
    },
  );

  // The work-items key carries the project ids it was built for, so it is
  // matched by prefix: one open inbox can hold several of them.
  queryClient.setQueriesData<WorkItems>(
    { queryKey: ["projects", "work-items"] },
    (current) => {
      if (!current) return current;
      const next = mergeWorkItems(current, event);
      if (next !== current) changed = true;
      return next;
    },
  );

  return changed;
}
