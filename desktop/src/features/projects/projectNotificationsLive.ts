import * as React from "react";
import { useQueryClient } from "@tanstack/react-query";

import type { Project } from "@/features/projects/hooks";
import type { ProjectsWorkItemsResult } from "@/features/projects/projectWorkItems";
import {
  decideProjectNotification,
  PROJECT_REPLY_KINDS,
  PROJECT_REVISION_KINDS,
} from "@/features/notifications/lib/projectNotify";
import {
  deriveWatchedProjectRoots,
  type WatchedProjectRoot,
  type WatchedProjectRootsResult,
} from "@/features/projects/projectUnreadRoots";
import {
  createLiveSubscriptionSet,
  DEFAULT_LIVE_SUBSCRIPTION_RETRY,
  type LiveSubscriptionSet,
} from "@/shared/api/liveSubscriptionSet";
import { relayClient } from "@/shared/api/relayClient";
import type { RelayEvent } from "@/shared/api/types";
import { createTrailingDebounce } from "@/shared/lib/trailingDebounce";

/**
 * Live relay listener for activity on project work items the user cares about.
 *
 * Shaped after `useLiveChannelUpdates`: derive an interest set, hold bounded
 * live subscriptions, dedupe, and hand notifiable events to the caller. The
 * differences are all consequences of the projects grammar:
 *
 * - Interest cannot be asked of the relay ("issues I commented on" is not a
 *   filter), so it is reconstructed from the work-items query cache.
 * - Roots are addressed by `e` (comments, statuses) *and* `E` (PR revisions),
 *   which are distinct relay filter keys, hence two subscriptions.
 * - The set is capped; see `WATCHED_PROJECT_ROOT_LIMIT`.
 */

/** Prefix-matches every `["projects","work-items", <project ids>]` cache row. */
const WORK_ITEMS_QUERY_KEY = ["projects", "work-items"] as const;

/**
 * Quiet window before rebuilding subscriptions after the cache changes.
 *
 * Opening the Projects screen writes several work-items rows in quick
 * succession (one per filter the user touches) and each mutation invalidates
 * them again. Rebuilding per write would tear down and re-open two relay REQs
 * each time; a trailing debounce collapses the burst into one rebuild.
 */
const WATCH_SET_REBUILD_DEBOUNCE_MS = 750;

/**
 * Backlog depth requested per subscription.
 *
 * Paired with `since: now` this is effectively "live only" — the limit exists
 * because `RelaySubscriptionFilter` requires one, and it bounds what a
 * reconnect replay can hand back in one burst.
 */
const SUBSCRIPTION_LIMIT = 100;

const EMPTY_WATCHED: WatchedProjectRootsResult = {
  roots: [],
  byRootId: new Map(),
  rootIdsKey: "",
  candidateCount: 0,
  truncatedCount: 0,
};

export type ProjectActivityHandler = (
  event: RelayEvent,
  root: WatchedProjectRoot,
) => void;

export type UseProjectNotificationsLiveOptions = {
  currentPubkey: string | undefined;
  /** False tears every subscription down (huddle windows, projects disabled). */
  enabled: boolean;
  /** Called once per notifiable event, after kind/author/root/dedupe filtering. */
  onProjectActivity: ProjectActivityHandler;
};

/**
 * Reads every populated work-items cache row.
 *
 * `findAll` prefix-matches, which matters: the query key ends with the
 * project-id list the caller passed, so the Projects screen and this feature's
 * own seeding query produce different rows for the same underlying data.
 * Merging beats guessing which row is authoritative.
 */
function readWorkItemsSnapshots(
  queryClient: ReturnType<typeof useQueryClient>,
): Array<ProjectsWorkItemsResult<Project> | undefined> {
  return queryClient
    .getQueryCache()
    .findAll({ queryKey: WORK_ITEMS_QUERY_KEY })
    .map(
      (query) =>
        query.state.data as ProjectsWorkItemsResult<Project> | undefined,
    );
}

function isWorkItemsQueryKey(queryKey: readonly unknown[]): boolean {
  return queryKey[0] === "projects" && queryKey[1] === "work-items";
}

/**
 * The two REQs that cover the whole watch set.
 *
 * The set's single key is the sorted root-id list, because that is the unit
 * the subscriptions are actually shaped by: a relay filter takes a list of
 * tag values, so N watched roots still cost two REQs, and the pair is rebuilt
 * only when the list itself changes.
 *
 * A factory rather than an inline literal because the hook has to be able to
 * build a second one: a disposed set stays closed by design, and StrictMode's
 * simulated unmount/remount would otherwise leave the hook holding a set that
 * can never open again.
 */
function createWatchedRootSubscriptions(
  onEvent: (event: RelayEvent) => void,
): LiveSubscriptionSet {
  return createLiveSubscriptionSet({
    buildGroup: (rootIdsKey, { nowSeconds }) => {
      const rootIds = rootIdsKey.split(",");
      return [
        // `since: now` keeps the relay from replaying the entire history of
        // every watched root on mount — the badge is for what happens *while*
        // you are running, and a backlog dump would light up every item you
        // ever touched.
        {
          kinds: [...PROJECT_REPLY_KINDS],
          "#e": rootIds,
          limit: SUBSCRIPTION_LIMIT,
          since: nowSeconds,
        },
        // Pull-request revisions address their root with an uppercase `E`,
        // which is a different relay filter key — it cannot be folded into the
        // filter above.
        {
          kinds: [...PROJECT_REVISION_KINDS],
          "#E": rootIds,
          limit: SUBSCRIPTION_LIMIT,
          since: nowSeconds,
        },
      ];
    },
    open: (filter, handler) => relayClient.subscribeLive(filter, handler),
    // Partial success is not usable: half the grammar would be silently
    // missing, which reads as "nothing is happening" rather than as an error.
    // The pair opens, fails, and retries as a unit — and shares one `since`.
    groupOpenPolicy: "atomic",
    onEvent: (event) => onEvent(event),
    retry: DEFAULT_LIVE_SUBSCRIPTION_RETRY,
    onError: (error) => {
      console.error(
        "Failed to subscribe to project work-item activity; retrying",
        error,
      );
    },
    // The relay session replays established live subscriptions itself on
    // reconnect (see `relayReconnectReplay`), so reconnect is only a repair
    // path here: it re-sends the pair only when it never opened (relay down
    // when the set was built), short-circuiting the backoff on the way.
    reconnect: {
      strategy: "repairFailedOnly",
      subscribeToReconnects: (listener) =>
        relayClient.subscribeToReconnects(listener),
    },
  });
}

export function useProjectNotificationsLive({
  currentPubkey,
  enabled,
  onProjectActivity,
}: UseProjectNotificationsLiveOptions): WatchedProjectRootsResult {
  const queryClient = useQueryClient();
  const normalizedPubkey = currentPubkey?.trim().toLowerCase() ?? "";
  const [watched, setWatched] =
    React.useState<WatchedProjectRootsResult>(EMPTY_WATCHED);
  // One shared guard for both filters: an event carrying `e` and `E` tags for
  // the same root matches both subscriptions, and reconnect replay overlaps
  // every live filter by five seconds.
  const seenEventIdsRef = React.useRef(new Set<string>());
  const reportedTruncationRef = React.useRef(-1);

  // Identity changes invalidate the guard: another account's delivery history
  // says nothing about what this one has already seen.
  React.useEffect(() => {
    void normalizedPubkey;
    seenEventIdsRef.current.clear();
  }, [normalizedPubkey]);

  // ---------------------------------------------------------------------
  // Watch set: derived from the query cache, not from a useQuery(select).
  //
  // A cache subscription is used because the rows this feature depends on are
  // owned by the Projects screen: their exact query keys vary with the screen's
  // filter, and this hook must not dictate (or duplicate) that fetch. Cache
  // subscription observes whatever rows exist, whoever wrote them.
  // ---------------------------------------------------------------------
  const recomputeWatchSet = React.useEffectEvent(() => {
    const next = deriveWatchedProjectRoots(
      readWorkItemsSnapshots(queryClient),
      normalizedPubkey,
    );

    if (
      next.truncatedCount > 0 &&
      next.truncatedCount !== reportedTruncationRef.current
    ) {
      reportedTruncationRef.current = next.truncatedCount;
      console.warn(
        `Project notifications are watching the ${next.roots.length} most recently active work items; ${next.truncatedCount} older ones are not being watched.`,
      );
    }

    setWatched((current) =>
      current.rootIdsKey === next.rootIdsKey ? current : next,
    );
  });

  React.useEffect(() => {
    if (!enabled || normalizedPubkey.length === 0) {
      setWatched(EMPTY_WATCHED);
      return;
    }

    const rebuild = createTrailingDebounce(
      () => recomputeWatchSet(),
      WATCH_SET_REBUILD_DEBOUNCE_MS,
    );

    // Seed from whatever is already cached — the Projects screen may have run
    // long before this hook mounted.
    recomputeWatchSet();

    const unsubscribe = queryClient.getQueryCache().subscribe((cacheEvent) => {
      if (isWorkItemsQueryKey(cacheEvent.query.queryKey)) {
        rebuild.trigger();
      }
    });

    return () => {
      rebuild.cancel();
      unsubscribe();
    };
  }, [enabled, normalizedPubkey, queryClient]);

  // ---------------------------------------------------------------------
  // Live subscriptions.
  // ---------------------------------------------------------------------
  const watchedRootIds = React.useMemo(
    () => new Set(watched.byRootId.keys()),
    [watched],
  );

  const handleProjectEvent = React.useEffectEvent((event: RelayEvent) => {
    const decision = decideProjectNotification(event, {
      currentPubkey: normalizedPubkey,
      watchedRootIds,
      seenEventIds: seenEventIdsRef.current,
    });

    if (!decision.notify) {
      return;
    }

    const root = watched.byRootId.get(decision.rootId);
    if (!root) {
      return;
    }

    onProjectActivity(event, root);
  });

  const liveSetRef = React.useRef<LiveSubscriptionSet | null>(null);
  const rootIdsKey = watched.rootIdsKey;

  React.useEffect(() => {
    if (!enabled || rootIdsKey.length === 0) {
      // Tear the pair down rather than leaving it watching a set nobody reads.
      liveSetRef.current?.setKeys([]);
      return;
    }

    if (liveSetRef.current === null) {
      liveSetRef.current = createWatchedRootSubscriptions(handleProjectEvent);
    }

    liveSetRef.current.setKeys([rootIdsKey]);
  }, [enabled, rootIdsKey]);

  React.useEffect(() => {
    return () => {
      // Null the ref: a disposed set stays closed, so a remount (StrictMode
      // simulates one) has to build a fresh one rather than reuse this.
      void liveSetRef.current?.dispose();
      liveSetRef.current = null;
    };
  }, []);

  return watched;
}
