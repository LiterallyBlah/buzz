import {
  classifySpeakableAgentText,
  type LiveTtsEvent,
} from "@/features/huddle/lib/ttsLiveMessages";

/**
 * Which agent replies the ambient session reads aloud.
 *
 * Built on the huddle classifier (`classifySpeakableAgentText`) rather than
 * beside it: attachment stripping, `[System]` filtering, self-authorship and
 * the `h`-tag check are the same problem, and forking them would let the two
 * drift. What differs is authorisation. A huddle speaks anything from any
 * agent in the ephemeral channel; an ambient session speaks only the ONE agent
 * the user bound a wake word to. The destination is usually a DM, so the
 * membership set is exactly {user, agent} — but a channel destination (a later
 * milestone) must not turn the wake word into a licence for every bot in that
 * channel to talk through the user's speakers.
 */
export type AmbientReplyEligibility =
  | { text: string; reason: null }
  | {
      text: null;
      reason:
        | "unsupported_kind"
        | "h_tag_mismatch"
        | "author_not_agent"
        | "self_authored"
        | "empty_or_system"
        | "not_the_bound_agent"
        | "no_destination";
    };

export function classifyAmbientReply(
  event: LiveTtsEvent,
  boundAgentPubkey: string | null,
  selfPubkey: string | null,
  destinationChannelId: string | null,
): AmbientReplyEligibility {
  // Fail closed: no binding or no destination means nothing is authorised.
  if (!boundAgentPubkey || !destinationChannelId)
    return { text: null, reason: "no_destination" };
  if (event.pubkey !== boundAgentPubkey)
    return { text: null, reason: "not_the_bound_agent" };

  const eligibility = classifySpeakableAgentText(
    event,
    new Set([boundAgentPubkey]),
    selfPubkey,
    destinationChannelId,
  );
  if (eligibility.text === null) return eligibility;
  return { text: eligibility.text, reason: null };
}

export type AmbientRouteResult =
  | "queued"
  | "disabled"
  | Exclude<AmbientReplyEligibility, { text: string }>["reason"];

/** Classify one live event and hand the speakable text to the speaker queue. */
export function routeAmbientReply(
  event: LiveTtsEvent,
  boundAgentPubkey: string | null,
  selfPubkey: string | null,
  destinationChannelId: string | null,
  routeId: number,
  enqueue: (text: string, routeId: number) => "queued" | "disabled",
): AmbientRouteResult {
  const eligibility = classifyAmbientReply(
    event,
    boundAgentPubkey,
    selfPubkey,
    destinationChannelId,
  );
  if (eligibility.text === null) return eligibility.reason;
  return enqueue(eligibility.text, routeId);
}
