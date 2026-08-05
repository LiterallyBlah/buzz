import type { ProjectAgentActivity } from "@/features/projects/projectAgentActivity";
import type { SeenAgentsForRoot } from "@/features/projects/projectSeenAgents";
import { normalizePubkey } from "@/shared/lib/pubkey";

/**
 * "Which agents have been active on this root", assembled from three sources
 * that each know something the other two do not.
 *
 * - **Live NIP-PA activity** knows who is working *right now*, and nothing at
 *   all about five minutes ago.
 * - **The local seen-agent memory** knows who this install has watched work
 *   here, including the background case that leaves no other trace: an agent
 *   enrolled by a peer call, which announces a turn and never writes a comment.
 * - **Comment authors** know who wrote something here, including work done
 *   before this install was ever watching — or on another machine entirely.
 *
 * The union is the point. Any one source alone produces a list that is wrong
 * in a way a reader would notice: live-only forgets everything, memory-only
 * misses history it never saw, comments-only misses exactly the background
 * agents this feature was asked to surface.
 */

/** Live state to badge with, or `null` for an agent that is only remembered. */
export type ProjectRootAgentState = "working" | "queued" | null;

export type ProjectRootAgent = {
  /** Normalised pubkey. */
  pubkey: string;
  state: ProjectRootAgentState;
  /**
   * Best local-clock evidence, in ms, of when this agent was last active here.
   *
   * Ordering only. It mixes a locally-observed sighting with a relay-supplied
   * `created_at`, so it is not a fact about the world precise enough to
   * display — which is why nothing renders it.
   */
  lastActiveAt: number;
};

/** Enough of a comment to attribute it. Structurally typed so tests and both
 * comment shapes (issue, pull request) fit without a conversion. */
export type ProjectRootCommentAuthor = {
  author: string;
  /** Seconds since the epoch, as it arrives on the event. */
  createdAt: number;
};

/** Live first, and within live, running before waiting. */
const STATE_RANK: Record<"working" | "queued", number> = {
  working: 0,
  queued: 1,
};

function rankOf(state: ProjectRootAgentState): number {
  return state === null ? 2 : STATE_RANK[state];
}

/**
 * The agents to list for one root, ordered for reading.
 *
 * `live` and `seen` are trusted as agents without consulting the known-agent
 * set, and that is deliberate rather than lax. A NIP-PA frame only becomes an
 * entry after `parseProjectActivity` has checked that its `agent` tag matches
 * the event's own `pubkey` — the announcement is signed by the identity it
 * names. Gating those on the known-agent baseline would filter out precisely
 * the case this section exists for: an agent that a peer enrolled in the
 * background is frequently not in this viewer's managed or relay-registered
 * lists, and the correct answer for "was it working here" is still yes.
 *
 * `commentAuthors` get the opposite treatment, because a comment carries no
 * claim of being an agent at all. Most of them are people, and listing a
 * person under "Agents" is a straightforward falsehood — so those must clear
 * `isKnownAgent` (the shared `useKnownAgentPubkeys` baseline, optionally
 * widened at the call site by a profile's `isAgent` flag).
 *
 * The root's own author is not a source here. They already have a dedicated
 * "Author" section immediately above, and repeating them under a second
 * heading says nothing new while suggesting they did something extra.
 */
export function buildProjectRootAgents({
  commentAuthors = [],
  isKnownAgent,
  live = [],
  seen = {},
}: {
  commentAuthors?: readonly ProjectRootCommentAuthor[];
  isKnownAgent: (pubkey: string) => boolean;
  live?: readonly ProjectAgentActivity[];
  seen?: SeenAgentsForRoot;
}): ProjectRootAgent[] {
  const byPubkey = new Map<string, ProjectRootAgent>();

  const merge = (
    pubkey: string,
    state: ProjectRootAgentState,
    lastActiveAt: number,
  ) => {
    const normalized = normalizePubkey(pubkey);
    if (!normalized) return;
    const existing = byPubkey.get(normalized);
    if (!existing) {
      byPubkey.set(normalized, { pubkey: normalized, state, lastActiveAt });
      return;
    }
    // A live state always wins over no state, and `working` over `queued`: an
    // agent that is running and also has a stale queued frame is running.
    if (rankOf(state) < rankOf(existing.state)) existing.state = state;
    if (lastActiveAt > existing.lastActiveAt) {
      existing.lastActiveAt = lastActiveAt;
    }
  };

  for (const [pubkey, seenAt] of Object.entries(seen)) {
    merge(pubkey, null, seenAt);
  }

  for (const entry of live) {
    // `announcedAt` is the event's `created_at`, in seconds.
    merge(entry.agent, entry.state, entry.announcedAt * 1_000);
  }

  for (const comment of commentAuthors) {
    const normalized = normalizePubkey(comment.author);
    // Already-listed authors skip the gate on purpose: a pubkey that announced
    // NIP-PA activity here is an agent by the stronger evidence, and dropping
    // its comment timestamp because the viewer's agent list has not caught up
    // would order it as though it had never spoken.
    if (!normalized) continue;
    if (!byPubkey.has(normalized) && !isKnownAgent(normalized)) continue;
    merge(normalized, null, comment.createdAt * 1_000);
  }

  return [...byPubkey.values()].sort(
    (a, b) =>
      rankOf(a.state) - rankOf(b.state) ||
      b.lastActiveAt - a.lastActiveAt ||
      // Total order, so a re-render with identical inputs cannot reshuffle the
      // list — two agents seen in the same quantised minute are common.
      a.pubkey.localeCompare(b.pubkey),
  );
}
