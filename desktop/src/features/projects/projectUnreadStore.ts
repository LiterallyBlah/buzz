import * as React from "react";

import type {
  WatchedProjectRoot,
  WatchedProjectWorkItemKind,
} from "@/features/projects/projectUnreadRoots";

/**
 * In-memory unread state for project work items.
 *
 * Deliberately NOT persisted. Channel unread state is durable because it is
 * relay-backed read markers (NIP-RS) that other clients also write; projects
 * have no such marker kind yet, so persisting here would invent a private,
 * device-local read model that silently disagrees with every other Buzz
 * install. Session-scoped ambient feedback ("something happened while you had
 * the app open") is the honest subset, and it is what makes the badge safe to
 * clear aggressively — nothing durable is being thrown away.
 *
 * Exposed as a module-level external store rather than React state so the
 * sidebar badge can subscribe directly, without threading a prop through
 * `AppShell` → `AppSidebar` → `AppSidebarPrimaryMenu`. Mirrors the
 * `useFeatureEnabled` / `useSyncExternalStore` pattern already used in
 * `shared/features`.
 */

export type ProjectUnreadEntry = {
  rootId: string;
  projectId: string;
  projectName: string;
  workItemKind: WatchedProjectWorkItemKind;
  title: string;
  /** Number of unseen events on this root since it was last cleared. */
  count: number;
  lastEventId: string;
  lastAuthor: string;
  lastAt: number;
};

export type ProjectUnreadSnapshot = {
  /** Unread ROOTS, not events — what the sidebar badge renders. */
  unreadRootCount: number;
  /** Total unseen events across all unread roots. */
  unreadEventCount: number;
  entriesByRootId: ReadonlyMap<string, ProjectUnreadEntry>;
  rootIdsByProjectId: ReadonlyMap<string, ReadonlySet<string>>;
};

/**
 * Cap on tracked roots.
 *
 * Matches `WATCHED_PROJECT_ROOT_LIMIT`: the listener can never mark more
 * distinct roots than it watches, so this is a belt-and-braces bound that
 * keeps a pathological relay (or a future caller) from growing the map without
 * limit. Eviction drops the least recently active root.
 */
export const PROJECT_UNREAD_ENTRY_LIMIT = 200;

const EMPTY_SNAPSHOT: ProjectUnreadSnapshot = {
  unreadRootCount: 0,
  unreadEventCount: 0,
  entriesByRootId: new Map(),
  rootIdsByProjectId: new Map(),
};

const entries = new Map<string, ProjectUnreadEntry>();
const listeners = new Set<() => void>();

// useSyncExternalStore demands a referentially stable snapshot between
// changes; rebuilding the derived maps on every getSnapshot() call would
// re-render forever. Rebuild only when the map actually mutates.
let snapshot: ProjectUnreadSnapshot = EMPTY_SNAPSHOT;

function rebuildSnapshot() {
  if (entries.size === 0) {
    snapshot = EMPTY_SNAPSHOT;
  } else {
    const rootIdsByProjectId = new Map<string, Set<string>>();
    let unreadEventCount = 0;

    for (const entry of entries.values()) {
      unreadEventCount += entry.count;
      const projectRootIds = rootIdsByProjectId.get(entry.projectId);
      if (projectRootIds) {
        projectRootIds.add(entry.rootId);
      } else {
        rootIdsByProjectId.set(entry.projectId, new Set([entry.rootId]));
      }
    }

    snapshot = {
      unreadRootCount: entries.size,
      unreadEventCount,
      entriesByRootId: new Map(entries),
      rootIdsByProjectId,
    };
  }

  for (const listener of listeners) {
    listener();
  }
}

function evictOldestIfNeeded() {
  if (entries.size <= PROJECT_UNREAD_ENTRY_LIMIT) {
    return;
  }

  let oldestRootId: string | null = null;
  let oldestAt = Number.POSITIVE_INFINITY;
  for (const entry of entries.values()) {
    if (entry.lastAt < oldestAt) {
      oldestAt = entry.lastAt;
      oldestRootId = entry.rootId;
    }
  }

  if (oldestRootId !== null) {
    entries.delete(oldestRootId);
  }
}

export function getProjectUnreadSnapshot(): ProjectUnreadSnapshot {
  return snapshot;
}

export function subscribeToProjectUnread(listener: () => void): () => void {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}

/** Records one unseen event on a watched root. Idempotent per event id. */
export function markProjectRootUnread(input: {
  root: WatchedProjectRoot;
  eventId: string;
  author: string;
  createdAt: number;
}): void {
  const existing = entries.get(input.root.rootId);

  // The listener already dedupes by event id, but a duplicate here would
  // inflate the count in a way the user cannot clear, so guard again.
  if (existing?.lastEventId === input.eventId) {
    return;
  }

  entries.set(input.root.rootId, {
    rootId: input.root.rootId,
    projectId: input.root.projectId,
    projectName: input.root.projectName,
    workItemKind: input.root.workItemKind,
    title: input.root.title,
    count: (existing?.count ?? 0) + 1,
    lastEventId: input.eventId,
    lastAuthor: input.author,
    lastAt: input.createdAt,
  });
  evictOldestIfNeeded();
  rebuildSnapshot();
}

export function clearProjectUnreadRoot(rootId: string): void {
  if (entries.delete(rootId)) {
    rebuildSnapshot();
  }
}

export function clearProjectUnreadForProject(projectId: string): void {
  let changed = false;
  for (const entry of [...entries.values()]) {
    if (entry.projectId === projectId) {
      entries.delete(entry.rootId);
      changed = true;
    }
  }

  if (changed) {
    rebuildSnapshot();
  }
}

export function clearAllProjectUnread(): void {
  if (entries.size === 0) {
    return;
  }

  entries.clear();
  rebuildSnapshot();
}

/**
 * Drops unread entries whose root is no longer watched.
 *
 * Without this, a root that leaves the watch set (cap eviction, or the item
 * being deleted upstream) would keep a badge lit that nothing on screen can
 * explain or clear.
 */
export function retainProjectUnreadRoots(
  watchedRootIds: ReadonlySet<string>,
): void {
  let changed = false;
  for (const rootId of [...entries.keys()]) {
    if (!watchedRootIds.has(rootId)) {
      entries.delete(rootId);
      changed = true;
    }
  }

  if (changed) {
    rebuildSnapshot();
  }
}

/** Test/teardown hook — resets module state without notifying listeners. */
export function resetProjectUnreadStoreForTests(): void {
  entries.clear();
  listeners.clear();
  snapshot = EMPTY_SNAPSHOT;
}

export function useProjectUnreadSnapshot(): ProjectUnreadSnapshot {
  return React.useSyncExternalStore(
    subscribeToProjectUnread,
    getProjectUnreadSnapshot,
    getProjectUnreadSnapshot,
  );
}

/** Unread work-item count for the sidebar's Projects entry. */
export function useProjectUnreadCount(): number {
  return useProjectUnreadSnapshot().unreadRootCount;
}

/** Unread roots within one project, for per-project surfaces. */
export function useProjectUnreadRootIds(
  projectId: string | null | undefined,
): ReadonlySet<string> {
  const currentSnapshot = useProjectUnreadSnapshot();
  return React.useMemo(() => {
    if (!projectId) return new Set<string>();
    return currentSnapshot.rootIdsByProjectId.get(projectId) ?? new Set();
  }, [currentSnapshot, projectId]);
}
