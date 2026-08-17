import * as React from "react";

import { createOrderedSpeaker } from "@/features/huddle/lib/ttsLiveMessages";
import { buildHuddleTtsLiveFilter } from "@/shared/api/relayChannelFilters";
import { relayClient } from "@/shared/api/relayClient";
import { ambientSpeak } from "./ambientVoiceApi";
import { routeAmbientReply } from "./ambientReplyRouting";

/** Replay window, matching the huddle watcher's startup recovery. */
const REPLAY_WINDOW_SECONDS = 5;
const MAX_SEEN_EVENTS = 5000;

let nextRouteId = 1;

/**
 * Speak the bound agent's replies in the ambient destination.
 *
 * The huddle's `useTtsSubscription` is the template, minus the parts that do
 * not apply: there is no membership fetch (the authorised speaker is the bound
 * agent, known from the settings, so there is nothing to be fail-closed
 * *about*), and no huddle TTS-enabled state (the ambient session's own enable
 * and mute are the switch). What is kept is what makes it work: a bounded
 * replay so the first reply is not lost to a subscription race, event-id dedup
 * against reconnect fan-out, and a serialized speaker so sentences are spoken
 * in arrival order rather than in IPC-completion order.
 */
export function useAmbientReplySubscription(
  destinationChannelId: string | null,
  boundAgentPubkey: string | null,
  selfPubkeyRef: React.RefObject<string | null>,
) {
  React.useEffect(() => {
    if (!destinationChannelId || !boundAgentPubkey) return;

    let disposed = false;
    let cleanup: (() => void) | null = null;

    const speakInOrder = createOrderedSpeaker(
      async (text) => {
        if (disposed) return;
        await ambientSpeak(text);
      },
      (error) => {
        console.warn("[ambient] speaking a reply failed:", error);
      },
      true,
    );

    const seenEventIds = new Set<string>();
    const seenOrder: string[] = [];
    const replaySince = Math.floor(Date.now() / 1000) - REPLAY_WINDOW_SECONDS;

    relayClient
      .subscribeLive(
        buildHuddleTtsLiveFilter(destinationChannelId, replaySince),
        (event) => {
          if (disposed) return;
          if (seenEventIds.has(event.id)) return;
          seenEventIds.add(event.id);
          seenOrder.push(event.id);
          if (seenOrder.length > MAX_SEEN_EVENTS) {
            const oldest = seenOrder.shift();
            if (oldest !== undefined) seenEventIds.delete(oldest);
          }

          const routeId = nextRouteId;
          nextRouteId += 1;
          const result = routeAmbientReply(
            event,
            boundAgentPubkey,
            selfPubkeyRef.current,
            destinationChannelId,
            routeId,
            (text, queuedRouteId) =>
              speakInOrder.enqueue(text, queuedRouteId, event.pubkey),
          );
          if (result !== "queued") {
            console.debug(
              `[ambient] reply stage=eligibility status=rejected reason=${result} route_id=${routeId}`,
            );
          }
        },
      )
      .then((dispose) => {
        if (disposed) {
          void dispose();
          return;
        }
        cleanup = () => void dispose();
      })
      .catch((error) => {
        console.error("[ambient] reply subscription failed:", error);
      });

    return () => {
      disposed = true;
      // Stops anything still queued from reaching the speaker after the
      // destination changed or the feature was switched off.
      speakInOrder.setEnabled(false);
      cleanup?.();
    };
  }, [destinationChannelId, boundAgentPubkey, selfPubkeyRef]);
}
