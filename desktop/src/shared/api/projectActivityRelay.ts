import type { RelayEvent } from "@/shared/api/types";
import { KIND_PROJECT_ACTIVITY } from "@/shared/constants/kinds";
import { relayClient } from "./relayClient";

// How far back the subscription looks on connect.
//
// Kind 20003 is ephemeral: the relay fans it out and stores nothing, so this
// window only covers what a reconnect can replay. It is deliberately a little
// wider than NIP-PA's 45-second staleness bound so a frame that arrives right
// at the edge is still judged by the store's expiry rather than silently
// missing — the store drops anything too old anyway.
const PROJECT_ACTIVITY_LOOKBACK_SECS = 60;

/**
 * Subscribe to agent activity on one project root.
 *
 * Scoped by `#e` and nothing else. Filtering by the repository coordinate
 * instead would light up every issue in the repo whenever any one of them is
 * busy, which is worse than showing nothing: it is confidently wrong on every
 * issue but the one that is actually working.
 */
export function subscribeToProjectActivity(
  rootEventId: string,
  onEvent: (event: RelayEvent) => void,
) {
  return relayClient.subscribeLive(
    {
      kinds: [KIND_PROJECT_ACTIVITY],
      "#e": [rootEventId],
      limit: 50,
      since: Math.floor(Date.now() / 1_000) - PROJECT_ACTIVITY_LOOKBACK_SECS,
    },
    onEvent,
  );
}
