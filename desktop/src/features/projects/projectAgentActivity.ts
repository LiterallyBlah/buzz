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

/** How long an announcement stays believable (NIP-PA §Refresh). */
export const PROJECT_ACTIVITY_STALE_MS = 45_000;

/**
 * A state that puts an agent on the root, as opposed to taking it off.
 *
 * `queued` is the runtime saying it has accepted a comment for a turn that has
 * not started — the interval between "the comment is on the relay" and the
 * first `working`, which on a busy pool is minutes. It is a separate state
 * rather than a `working` with a "queued" stage because it is a different
 * claim: nothing is being read, written or run yet, and a caption is
 * presentation while this is fact.
 */
export type ProjectActivityLiveState = "working" | "queued";

export type ProjectAgentActivity = {
  agent: string;
  turnId: string;
  /**
   * What this agent last said about this root.
   *
   * Never `idle`: that state removes an entry rather than being one, so a
   * stored `idle` would be an agent recorded as present in order to say it is
   * absent.
   */
  state: ProjectActivityLiveState;
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
 * One announcement, once it has survived every refusal.
 *
 * `idle` carries no entry on purpose. It is the only state that says something
 * about a turn *ending*, so the one thing a consumer may do with it is match
 * its `turn` against what is on screen — modelling it as an entry would invite
 * storing it.
 */
export type ProjectActivityFrame =
  | { agent: string; state: "idle"; turnId: string }
  | {
      agent: string;
      state: ProjectActivityLiveState;
      entry: ProjectAgentActivity;
    };

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
 * - **An unknown `state`.** Refused rather than treated as present: a future
 *   state this build has never heard of is not safely renderable as "working",
 *   and showing nothing is the honest reading of a word we cannot interpret.
 */
export function parseProjectActivity(
  event: RelayEvent,
  rootEventId: string,
): ProjectActivityFrame | null {
  if (event.kind !== KIND_PROJECT_ACTIVITY) return null;
  if (tag(event, "e") !== rootEventId) return null;
  if (tag(event, "h") !== null) return null;

  const agent = tag(event, "agent");
  if (!agent || agent.toLowerCase() !== event.pubkey.toLowerCase()) return null;

  const state = tag(event, "state");
  if (state !== "working" && state !== "queued" && state !== "idle")
    return null;

  const turnId = tag(event, "turn");
  if (!turnId) return null;

  if (state === "idle") {
    return { agent: agent.toLowerCase(), state, turnId };
  }
  return {
    agent: agent.toLowerCase(),
    state,
    entry: {
      agent: agent.toLowerCase(),
      turnId,
      state,
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
    if (!existing || existing.turnId !== parsed.turnId) return current;
    const next = { ...current };
    delete next[parsed.agent];
    return next;
  }

  // An out-of-order announcement from an older frame must not overwrite a newer
  // one — relays do not promise ordering, and the older frame's stage would
  // replace the current caption.
  if (
    existing &&
    existing.turnId === parsed.entry.turnId &&
    existing.announcedAt >= parsed.entry.announcedAt
  ) {
    return current;
  }

  // `queued` never displaces `working`, and this is not the same rule as the
  // one above: the two frames belong to *different* turns — a queued frame is
  // named after the comment that caused it, never after a turn — so the turn-id
  // check cannot see them as a pair. The runtime already refuses to announce
  // one over the other, but `created_at` is whole seconds and the same second
  // routinely carries both, so a reordered pair would arrive as
  // "working — editing files" then "queued" and walk the indicator backwards
  // for a full refresh cycle.
  if (
    parsed.entry.state === "queued" &&
    existing?.state === "working" &&
    existing.announcedAt >= parsed.entry.announcedAt
  ) {
    return current;
  }
  return { ...current, [parsed.agent]: parsed.entry };
}

/**
 * The agents still believably on this root, given the clock.
 *
 * Expiry is the real terminator, not `idle`. A harness killed mid-turn sends no
 * terminal frame at all, and a view that waited for one would show that agent
 * as working forever.
 *
 * `queued` is held to the same window, because the runtime re-announces it on
 * the same 15-second cadence for as long as the comment is still waiting. An
 * expired `queued` therefore means the runtime stopped talking, which is the
 * one reading under which the agent is genuinely no longer on this root.
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
