import * as React from "react";
import { useLocation } from "@tanstack/react-router";

import { huddleWindowChannelId } from "@/features/huddle/lib/huddleWindow";
import { readStoredNotificationSettings } from "@/features/notifications/hooks";
import { deliverProjectDesktopNotification } from "@/features/notifications/lib/projectNotificationDelivery";
import { useProjectNotificationsLive } from "@/features/projects/projectNotificationsLive";
import type { WatchedProjectRoot } from "@/features/projects/projectUnreadRoots";
import {
  clearAllProjectUnread,
  clearProjectUnreadForProject,
  markProjectRootUnread,
  retainProjectUnreadRoots,
} from "@/features/projects/projectUnreadStore";
import { useIdentityQuery } from "@/shared/api/hooks";
import type { RelayEvent } from "@/shared/api/types";
import { useFeatureEnabled } from "@/shared/features";
import { useDeferredStartup } from "@/shared/hooks/useDeferredStartup";

/**
 * App-level wiring for project (issue / pull request) ambient feedback.
 *
 * Headless, and mounted from the root route rather than from `AppShell` for
 * two reasons: it needs the router (for the "which project is open" clear and
 * suppress rules) and it must not add surface to `AppShell`, which is already
 * at the repo's 1000-line file ceiling.
 *
 * Everything downstream of this component is opt-in: the whole tree is gated
 * on the `projects` preview feature, so users who have not enabled Projects
 * pay nothing — no extra relay fan-out, no subscriptions.
 */

/** `/projects/<id>` → the id; `/projects` → null; anything else → undefined. */
export function projectRouteTarget(
  pathname: string,
): { projectId: string | null } | null {
  if (pathname === "/projects" || pathname === "/projects/") {
    return { projectId: null };
  }

  if (!pathname.startsWith("/projects/")) {
    return null;
  }

  const rawProjectId = pathname.slice("/projects/".length).split("/")[0];
  if (!rawProjectId) {
    return { projectId: null };
  }

  return { projectId: decodeURIComponent(rawProjectId) };
}

/**
 * Work-items cache seed, lazy so the projects module graph stays out of the
 * startup chunk for users who never enable the preview feature. See
 * `features/projects/projectNotificationsSeed` for why it exists at all.
 */
const ProjectWorkItemsSeed = React.lazy(async () => {
  const module = await import("@/features/projects/projectNotificationsSeed");
  return { default: module.ProjectWorkItemsSeed };
});

export function ProjectNotificationsBridge() {
  const projectsEnabled = useFeatureEnabled("projects");
  const identityQuery = useIdentityQuery();
  const startupReady = useDeferredStartup();
  const location = useLocation();
  const pathname = location.pathname;
  const currentPubkey = identityQuery.data?.pubkey;

  // A huddle companion window renders the same React tree. It must never open
  // its own project subscriptions or race the main window's notifications.
  const isHuddleWindow = React.useMemo(
    () => huddleWindowChannelId() !== null,
    [],
  );
  const enabled =
    projectsEnabled && !isHuddleWindow && Boolean(currentPubkey?.trim());

  const routeTarget = React.useMemo(
    () => projectRouteTarget(pathname),
    [pathname],
  );

  /**
   * Suppression, at the coarsest granularity this component can actually
   * observe.
   *
   * Which work item is open on screen lives inside the Projects panels, which
   * this component does not own, so "is the Projects screen visible and does
   * the window have focus" is the honest bound. When it holds, an alert would
   * be telling the user about something they are already looking at.
   */
  const isViewingProjects = routeTarget !== null;

  const handleProjectActivity = React.useEffectEvent(
    (event: RelayEvent, root: WatchedProjectRoot) => {
      const isSuppressed =
        isViewingProjects &&
        typeof document !== "undefined" &&
        document.hasFocus() &&
        // On a project detail route only that project is on screen; the list
        // route shows all of them.
        (routeTarget?.projectId === null ||
          routeTarget?.projectId === root.projectId);

      if (isSuppressed) {
        return;
      }

      markProjectRootUnread({
        root,
        eventId: event.id,
        author: event.pubkey,
        createdAt: event.created_at,
      });

      // Settings are read here, at delivery time, so a toggle in Settings
      // takes effect on the very next event.
      void deliverProjectDesktopNotification({
        event,
        root,
        settings: readStoredNotificationSettings(
          currentPubkey?.trim().toLowerCase() ?? "",
        ),
      });
    },
  );

  const watched = useProjectNotificationsLive({
    currentPubkey,
    enabled,
    onProjectActivity: handleProjectActivity,
  });

  // A root that leaves the watch set (cap eviction, deleted item) can no
  // longer be opened or explained, so it must not keep the badge lit.
  //
  // An EMPTY watch set is not treated as "drop everything": that is also what
  // an unpopulated or briefly-failed cache read looks like, and losing the
  // badge because a refetch blipped would be worse than a stale entry that the
  // next non-empty derivation prunes anyway.
  const watchedRootIdsKey = watched.rootIdsKey;
  React.useEffect(() => {
    if (watchedRootIdsKey.length === 0) {
      return;
    }

    retainProjectUnreadRoots(new Set(watchedRootIdsKey.split(",")));
  }, [watchedRootIdsKey]);

  // Clearing is route-driven: reaching Projects clears everything, reaching a
  // single project clears that project. Per-work-item clearing would need a
  // hook inside the issue/PR panels — recorded as follow-up.
  //
  // Re-runs on window focus as well as on navigation: events that arrive while
  // the app is in the background are marked unread (suppression requires
  // focus), and coming back to a Projects screen that is already open must not
  // leave a badge lit for items sitting in front of the user.
  React.useEffect(() => {
    const clearForRoute = () => {
      const target = projectRouteTarget(pathname);
      if (target === null) {
        return;
      }

      if (target.projectId === null) {
        clearAllProjectUnread();
      } else {
        clearProjectUnreadForProject(target.projectId);
      }
    };

    clearForRoute();
    window.addEventListener("focus", clearForRoute);
    return () => {
      window.removeEventListener("focus", clearForRoute);
    };
  }, [pathname]);

  // The seed is deferred past startup: it is a four-query relay fan-out and
  // nothing about it is urgent enough to compete with first paint.
  if (!enabled || !startupReady) {
    return null;
  }

  return (
    <React.Suspense fallback={null}>
      <ProjectWorkItemsSeed />
    </React.Suspense>
  );
}
