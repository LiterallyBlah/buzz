import { useQueryClient } from "@tanstack/react-query";
import * as React from "react";

import { createLiveSubscriptionSet } from "@/shared/api/liveSubscriptionSet";
import { relayClient } from "@/shared/api/relayClient";
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

    // `perFilter` + `repairFailedOnly` is the load-bearing pair here: the two
    // filters stand alone (a comment feed that opened is already useful
    // without the revision feed), and a reconnect must re-send only the one
    // whose REQ failed — the session replays what it accepted, so re-sending
    // an accepted filter opens a second REQ for it and delivers every event
    // twice. No timer retry: a failed open waits for the reconnect that says
    // the relay is reachable again, which is the only news worth acting on
    // for a panel that is only alive while it is on screen.
    const liveRoot = createLiveSubscriptionSet({
      buildGroup: (key, { nowSeconds }) =>
        projectRootLiveFilters(key, nowSeconds),
      open: (filter, onEvent) => relayClient.subscribeLive(filter, onEvent),
      groupOpenPolicy: "perFilter",
      onEvent: (event) => {
        // Second gate, after the relay's own filtering: a relay that
        // over-delivers must not put another root's comment in this panel.
        const role = projectRootEventRole(event, rootId);
        if (!role) return;

        applyProjectRootEvent(queryClient, { event, projectId, rootId });
        if (role !== "comment") refresh.trigger();
      },
      onError: (error) => {
        console.error(
          "Failed to subscribe to live project root updates",
          rootId,
          error,
        );
      },
      reconnect: {
        strategy: "repairFailedOnly",
        subscribeToReconnects: (listener) =>
          relayClient.subscribeToReconnects(listener),
        // Events published while the socket was down do not replay in full,
        // so close the gap once with a refetch rather than leaving the panel
        // to heal on its next mount.
        onReconnect: () => refresh.trigger(),
      },
    });

    liveRoot.setKeys([rootId]);

    return () => {
      refresh.cancel();
      void liveRoot.dispose();
    };
  }, [projectId, queryClient, rootId]);
}
