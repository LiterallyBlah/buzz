import type { RelayEvent } from "@/shared/api/types";
import { KIND_PROJECT_ACTIVITY } from "@/shared/constants/kinds";

/**
 * NIP-PA: which agents are working on a project root, right now.
 *
 * Pure reducer. Every rule that decides whether an indicator is shown lives
 * here rather than in the component, so the interesting cases — a stale `idle`
 * from a finished turn, a harness that died without sending one at all, an
 * event that belongs to a different issue — are testable without rendering
 * anything.
 */

/** How long a `working` announcement stays believable (NIP-PA §Refresh). */
export const PROJECT_ACTIVITY_STALE_MS = 45_000;

export type ProjectAgentActivity = {
  agent: string;
  turnId: string;
  stage: string | null;
  /** Seconds since the epoch, from the event. */
  announcedAt: number;
};

/** Agent pubkey → its latest live announcement on one root. */
export type ProjectActivityState = Record<string, ProjectAgentActivity>;

export const EMPTY_PROJECT_ACTIVITY: ProjectActivityState = {};

function tag(event: RelayEvent, key: string): string | null {
  for (const entry of event.tags) {
    if (entry[0] === key && typeof entry[1] === "string") return entry[1];
  }
  return null;
}

/**
 * Read an event as an activity announcement for `rootEventId`, or reject it.
 *
 * Refusals, in the order they matter:
 *
 * - **Wrong root.** The subscription is per root, but a relay is not an
 *   authority on what it sends and a stale subscription can outlive a view.
 * - **An `h` tag.** NIP-PA forbids it. An event naming both a channel and a
 *   root names two places for one signal, and guessing which wins is how the
 *   indicator ends up on the wrong surface.
 * - **`agent` disagreeing with `pubkey`.** The tag exists so a consumer can
 *   filter without reading authorship; if the two disagree, one of them is a
 *   claim rather than a signature, and there is no reading under which
 *   believing it is correct.
 */
export function parseProjectActivity(
  event: RelayEvent,
  rootEventId: string,
): {
  agent: string;
  state: "working" | "idle";
  entry: ProjectAgentActivity;
} | null {
  if (event.kind !== KIND_PROJECT_ACTIVITY) return null;
  if (tag(event, "e") !== rootEventId) return null;
  if (tag(event, "h") !== null) return null;

  const agent = tag(event, "agent");
  if (!agent || agent.toLowerCase() !== event.pubkey.toLowerCase()) return null;

  const state = tag(event, "state");
  if (state !== "working" && state !== "idle") return null;

  const turnId = tag(event, "turn");
  if (!turnId) return null;

  return {
    agent: agent.toLowerCase(),
    state,
    entry: {
      agent: agent.toLowerCase(),
      turnId,
      stage: tag(event, "stage"),
      announcedAt: event.created_at,
    },
  };
}

/**
 * Fold one event into the activity state, returning the same object when
 * nothing changed so React can skip the re-render.
 */
export function applyProjectActivity(
  current: ProjectActivityState,
  event: RelayEvent,
  rootEventId: string,
): ProjectActivityState {
  const parsed = parseProjectActivity(event, rootEventId);
  if (!parsed) return current;

  const existing = current[parsed.agent];

  if (parsed.state === "idle") {
    // Only the turn being shown may be cleared by its own `idle`. A late
    // terminal frame from the previous turn would otherwise blank the
    // indicator for work that is still running.
    if (!existing || existing.turnId !== parsed.entry.turnId) return current;
    const next = { ...current };
    delete next[parsed.agent];
    return next;
  }

  // An out-of-order `working` from an older announcement must not overwrite a
  // newer one — relays do not promise ordering, and the older frame's stage
  // would replace the current caption.
  if (
    existing &&
    existing.turnId === parsed.entry.turnId &&
    existing.announcedAt >= parsed.entry.announcedAt
  ) {
    return current;
  }
  return { ...current, [parsed.agent]: parsed.entry };
}

/**
 * The agents still believably working, given the clock.
 *
 * Expiry is the real terminator, not `idle`. A harness killed mid-turn sends no
 * terminal frame at all, and a view that waited for one would show that agent
 * as working forever.
 */
export function liveProjectActivity(
  state: ProjectActivityState,
  nowMs: number,
): ProjectAgentActivity[] {
  return Object.values(state)
    .filter(
      (entry) => nowMs - entry.announcedAt * 1_000 < PROJECT_ACTIVITY_STALE_MS,
    )
    .sort((a, b) => a.agent.localeCompare(b.agent));
}
