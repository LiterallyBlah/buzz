import { useQueryClient } from "@tanstack/react-query";
import * as React from "react";

import { relayClient } from "@/shared/api/relayClient";
import type { RelayEvent } from "@/shared/api/types";
import { createTrailingDebounce } from "@/shared/lib/trailingDebounce";
import {
  applyProjectRootEvent,
  projectRootEventRole,
  projectRootLiveFilters,
} from "./projectRootLiveUpdates";

/**
 * Quiet window before the refetch that follows a lifecycle change.
 *
 * Status changes and revisions arrive in bursts (a merge publishes a status and
 * a revision within the same second), and each one would otherwise cost a
 * four-filter fan-out per open panel.
 */
const PROJECT_ROOT_REFRESH_DEBOUNCE_MS = 500;

/**
 * Keep one issue or pull request live while its detail view is on screen.
 *
 * Projects were fetch-only: a reply from an agent existed on the relay and in
 * nobody's window until the panel was remounted, because nothing in the
 * feature ever asked the relay a second question. This holds the same kind of
 * persistent REQ the channel timeline holds, and merges what arrives straight
 * into the query caches — the event carries the entire change, so paying a
 * relay round trip to re-learn it would be slower and no more correct.
 *
 * Scoped to one root, and unsubscribed when the view moves to another. A
 * repository-wide subscription would be one REQ instead of two, but the thing
 * that goes wrong there — another issue's comments appearing under this one
 * — is the exact failure this is meant to prevent.
 *
 * Lifecycle events (statuses, revisions) additionally schedule a debounced
 * refetch: they move the cross-project activity summaries, which are an
 * aggregate no single event can be folded into. Comments never need it.
 */
export function useLiveProjectRoot(
  projectId: string | null | undefined,
  rootId: string | null | undefined,
) {
  const queryClient = useQueryClient();

  React.useEffect(() => {
    if (!projectId || !rootId) return;

    let disposed = false;
    const disposers = new Map<number, () => Promise<void>>();
    const refresh = createTrailingDebounce(() => {
      for (const queryKey of [
        ["project", projectId, "issues"],
        ["project", projectId, "pull-requests"],
        ["projects", "work-items"],
        ["projects", "activity-summaries"],
      ]) {
        void queryClient.invalidateQueries({ queryKey });
      }
    }, PROJECT_ROOT_REFRESH_DEBOUNCE_MS);

    const handleEvent = (event: RelayEvent) => {
      if (disposed) return;

      // Second gate, after the relay's own filtering: a relay that
      // over-delivers must not put another root's comment in this panel.
      const role = projectRootEventRole(event, rootId);
      if (!role) return;

      applyProjectRootEvent(queryClient, { event, projectId, rootId });
      if (role !== "comment") refresh.trigger();
    };

    // Subscribe by index so a filter whose REQ failed (relay down at mount) is
    // the only one retried on reconnect. The session replays subscriptions it
    // accepted, so resubscribing those would open a second REQ for the same
    // filter and deliver every event twice.
    const subscribeMissingFilters = async () => {
      const filters = projectRootLiveFilters(rootId);
      await Promise.all(
        filters.map(async (filter, index) => {
          if (disposed || disposers.has(index)) return;
          try {
            const dispose = await relayClient.subscribeLive(
              filter,
              handleEvent,
            );
            if (disposed) {
              void dispose();
              return;
            }
            disposers.set(index, dispose);
          } catch (error) {
            console.error(
              "Failed to subscribe to live project root updates",
              rootId,
              error,
            );
          }
        }),
      );
    };

    void subscribeMissingFilters();

    const unsubscribeFromReconnects = relayClient.subscribeToReconnects(() => {
      // Events published while the socket was down do not replay in full, so
      // close the gap once with a refetch rather than leaving the panel to
      // heal on its next mount.
      refresh.trigger();
      void subscribeMissingFilters();
    });

    return () => {
      disposed = true;
      refresh.cancel();
      unsubscribeFromReconnects();
      for (const dispose of disposers.values()) {
        void dispose().catch(() => {});
      }
      disposers.clear();
    };
  }, [projectId, queryClient, rootId]);
}
