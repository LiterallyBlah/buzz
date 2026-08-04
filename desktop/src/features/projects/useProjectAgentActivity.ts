import * as React from "react";

import {
  applyProjectActivity,
  EMPTY_PROJECT_ACTIVITY,
  liveProjectActivity,
  type ProjectActivityState,
  type ProjectAgentActivity,
} from "@/features/projects/projectAgentActivity";
import { subscribeToProjectActivity } from "@/shared/api/projectActivityRelay";
import { useLivenessSweep } from "@/shared/lib/useLivenessSweep";

/**
 * How often the hook re-evaluates staleness.
 *
 * The store's expiry is a function of the clock, so without a tick an
 * indicator whose last announcement aged out stays on screen until some
 * unrelated event forces a render. Two seconds is well under NIP-PA's 45-second
 * window and cheap enough to run only while a root is on screen.
 */
const ACTIVITY_TICK_MS = 2_000;

/**
 * The agents currently working on one project root (NIP-PA).
 *
 * Subscribes per root and unsubscribes when the view moves, so activity from
 * one issue cannot arrive while another is displayed. The subscription is not
 * shared across roots on purpose: a single app-wide subscription would need a
 * routing layer keyed by root, and the thing that goes wrong there — one root's
 * events rendering under another — is exactly what this phase exists to
 * prevent.
 */
export function useProjectAgentActivity(
  rootEventId: string | null | undefined,
): ProjectAgentActivity[] {
  const [state, setState] = React.useState<ProjectActivityState>(
    EMPTY_PROJECT_ACTIVITY,
  );
  const [now, setNow] = React.useState(() => Date.now());

  React.useEffect(() => {
    setState(EMPTY_PROJECT_ACTIVITY);
    if (!rootEventId) return;

    let cancelled = false;
    let unsubscribe: (() => void) | undefined;

    void (async () => {
      try {
        const handle = await subscribeToProjectActivity(
          rootEventId,
          (event) => {
            if (cancelled) return;
            setState((current) =>
              applyProjectActivity(current, event, rootEventId),
            );
          },
        );
        if (cancelled) {
          handle?.();
          return;
        }
        unsubscribe = handle;
      } catch {
        // A relay that will not open this subscription leaves the indicator
        // absent, which is the honest reading: nothing is known about who is
        // working. Surfacing an error here would put a failure banner on an
        // issue over a purely decorative signal.
      }
    })();

    return () => {
      cancelled = true;
      unsubscribe?.();
    };
  }, [rootEventId]);

  useLivenessSweep(Object.keys(state).length > 0, ACTIVITY_TICK_MS, () =>
    setNow(Date.now()),
  );

  return React.useMemo(() => liveProjectActivity(state, now), [state, now]);
}
