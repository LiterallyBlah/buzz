import type { Project } from "@/features/projects/hooks";
import type { ProjectsWorkItemsResult } from "@/features/projects/projectWorkItems";

/**
 * Derives "which project roots do I care about?" from the work-items cache.
 *
 * The relay has no per-user subscription for "issues I am involved in", so
 * interest is reconstructed client-side from data the Projects screen already
 * loads: `fetchProjectsWorkItems` returns every issue and pull request for the
 * user's projects, each with its author and its comment authors. A root counts
 * as watched when the user authored it or commented on it — the projects
 * analogue of the `authoredRootIds` / `participatedRootIds` sets that
 * `useUnreadChannels` feeds into `shouldNotifyForEvent`.
 *
 * Pure and React-free so the cap behaviour is unit-testable.
 */

/**
 * Maximum number of roots the live listener will watch at once.
 *
 * Every watched root becomes an entry in a relay filter's `#e` / `#E` array.
 * Relays cap filter sizes and the whole array is re-sent on every reconnect
 * replay, so an unbounded set turns a prolific contributor's account into a
 * subscription that the relay silently truncates or rejects.
 *
 * 200 is chosen to sit an order of magnitude below the relay's practical
 * filter limits while covering far more than a person actively follows.
 * Truncation is never silent: {@link deriveWatchedProjectRoots} reports how
 * many roots were dropped and the caller logs it (see
 * `projectNotificationsLive.ts`), because a badge that quietly stops working
 * past N items is worse than one that tells you it is capped.
 */
export const WATCHED_PROJECT_ROOT_LIMIT = 200;

export type WatchedProjectWorkItemKind = "issue" | "pull-request";

export type WatchedProjectRoot = {
  /** Root event id — kind 1621 (issue) or 1618 (pull request). */
  rootId: string;
  projectId: string;
  projectName: string;
  workItemKind: WatchedProjectWorkItemKind;
  title: string;
  /** Latest activity timestamp; drives the "keep the newest N" cap. */
  updatedAt: number;
  /** True when the user authored the root itself (vs. only commenting). */
  authored: boolean;
};

export type WatchedProjectRootsResult = {
  /** Newest-first, already capped to {@link WATCHED_PROJECT_ROOT_LIMIT}. */
  roots: WatchedProjectRoot[];
  byRootId: Map<string, WatchedProjectRoot>;
  /** Sorted root ids — a stable key for "did the watch set change?". */
  rootIdsKey: string;
  /** How many roots matched before the cap was applied. */
  candidateCount: number;
  /** How many matching roots the cap dropped. Never silently discarded. */
  truncatedCount: number;
};

type WorkItemLike = {
  id: string;
  title: string;
  author: string;
  updatedAt: number;
  comments: ReadonlyArray<{ author: string }>;
};

function normalize(pubkey: string | null | undefined): string {
  return pubkey?.trim().toLowerCase() ?? "";
}

/**
 * Whether the user shows up on this work item at all.
 *
 * Participation is comment authorship only. Being `p`-tagged as a recipient or
 * a requested reviewer is deliberately NOT participation: those tags are set
 * by whoever opened the item, so treating them as interest would let anyone
 * subscribe you to their issue's notifications.
 */
function involvementForWorkItem(
  item: WorkItemLike,
  currentPubkey: string,
): { involved: boolean; authored: boolean } {
  const authored = normalize(item.author) === currentPubkey;
  if (authored) {
    return { involved: true, authored: true };
  }

  const participated = (item.comments ?? []).some(
    (comment) => normalize(comment.author) === currentPubkey,
  );
  return { involved: participated, authored: false };
}

function collectFrom(
  entries: ReadonlyArray<{ project: Project; item: WorkItemLike }>,
  workItemKind: WatchedProjectWorkItemKind,
  currentPubkey: string,
  collected: Map<string, WatchedProjectRoot>,
) {
  for (const { project, item } of entries) {
    if (!item?.id) {
      continue;
    }

    const { involved, authored } = involvementForWorkItem(item, currentPubkey);
    if (!involved) {
      continue;
    }

    // The same root can appear in several cache entries (the Projects screen
    // keys the query by the filtered project-id list, so "all projects" and
    // "issues only" are separate cache rows). Keep the freshest copy, and
    // never let a staler row downgrade `authored`.
    const existing = collected.get(item.id);
    if (existing && existing.updatedAt >= (item.updatedAt ?? 0)) {
      existing.authored = existing.authored || authored;
      continue;
    }

    collected.set(item.id, {
      rootId: item.id,
      projectId: project.id,
      projectName: project.name,
      workItemKind,
      title: item.title ?? "",
      updatedAt: item.updatedAt ?? 0,
      authored: authored || existing?.authored || false,
    });
  }
}

/**
 * Build the watched-root set from one or more work-items cache snapshots.
 *
 * Accepts an array because the work-items query key includes the project-id
 * list, so a session can hold several populated cache rows at once; merging
 * them is cheaper and less surprising than picking one and hoping it is the
 * complete one.
 */
export function deriveWatchedProjectRoots(
  snapshots: ReadonlyArray<ProjectsWorkItemsResult<Project> | undefined>,
  currentPubkey: string | undefined,
  limit = WATCHED_PROJECT_ROOT_LIMIT,
): WatchedProjectRootsResult {
  const normalizedPubkey = normalize(currentPubkey);
  const collected = new Map<string, WatchedProjectRoot>();

  // Without an identity there is no "mine" to compute, and watching every
  // root in the relay would be both wrong and unbounded.
  if (normalizedPubkey.length > 0) {
    for (const snapshot of snapshots) {
      if (!snapshot) continue;

      collectFrom(
        (snapshot.issues?.items ?? []).map((entry) => ({
          project: entry.project,
          item: entry.issue,
        })),
        "issue",
        normalizedPubkey,
        collected,
      );
      collectFrom(
        (snapshot.pullRequests?.items ?? []).map((entry) => ({
          project: entry.project,
          item: entry.pullRequest,
        })),
        "pull-request",
        normalizedPubkey,
        collected,
      );
    }
  }

  const candidateCount = collected.size;
  const roots = [...collected.values()]
    .sort((left, right) => {
      // Newest activity first, then by id so the cap is deterministic when
      // timestamps tie (bulk-imported items commonly share a timestamp).
      if (right.updatedAt !== left.updatedAt) {
        return right.updatedAt - left.updatedAt;
      }
      return left.rootId.localeCompare(right.rootId);
    })
    .slice(0, Math.max(0, limit));

  return {
    roots,
    byRootId: new Map(roots.map((root) => [root.rootId, root])),
    rootIdsKey: roots
      .map((root) => root.rootId)
      .sort()
      .join(","),
    candidateCount,
    truncatedCount: candidateCount - roots.length,
  };
}
