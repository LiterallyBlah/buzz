import * as React from "react";

import { normalizePubkey } from "@/shared/lib/pubkey";

/**
 * Which agents have *been* on a project root — as opposed to which are on it
 * right now.
 *
 * NIP-PA (kind 20003) is ephemeral by construction: the relay stores nothing,
 * and a frame stops being believable 45 seconds after it was announced. That
 * is exactly right for "is working", and useless for "has worked". An agent
 * enrolled in the background by a peer call — hermes asking Claude to take an
 * issue — announces itself for the length of its turn and then vanishes from
 * the relay's point of view, leaving no comment, no review, no event of any
 * kind attributable to it. Without a local memory that agent is unfindable
 * ten minutes later, which is the whole reason this module exists.
 *
 * The memory is deliberately device-local and deliberately small. It is not a
 * claim about what happened — the relay remains the authority on that — but a
 * record of what *this install observed*, which is the honest framing for
 * something assembled from frames only a listening client ever saw. Two
 * installs watching the same issue will legitimately remember different sets,
 * and neither is wrong.
 *
 * Storage is a single versioned localStorage blob in the idiom the rest of the
 * app already uses (`huddleBackingChannelStorage`, `threadViewModePreference`):
 * read once at module init, rewritten whole on change, every access wrapped in
 * a try/catch because a locked-down webview is a normal state and losing this
 * record is survivable. No new archive infrastructure: the local-archive
 * feature is for relay-replayable event history, and these frames are not
 * replayable by anyone.
 */

const STORAGE_KEY = "buzz-project-seen-agents.v1";

/**
 * How long a remembered sighting stays interesting.
 *
 * Long enough that an agent which worked an issue last week is still listed
 * when someone comes back to read the outcome; short enough that a root nobody
 * has opened in a month stops carrying names that no longer explain anything
 * about it.
 */
export const SEEN_AGENT_TTL_MS = 30 * 24 * 60 * 60 * 1_000;

/**
 * Below this, a repeat sighting is not worth recording.
 *
 * The live hook re-reports its agents on every liveness tick (~2s) for as long
 * as a turn runs. Writing `lastSeenAt` each time would mean a localStorage
 * write and a re-render of every subscriber twice a second for the entire
 * turn, to move a timestamp that is only ever used to order a list. Quantising
 * to a minute makes the repeat case an exact no-op — same object back, no
 * notify, no write — while a *newly* seen agent still lands immediately,
 * because a new pubkey changes the set rather than a timestamp.
 */
export const SEEN_AGENT_REFRESH_MS = 60_000;

/** Roots remembered at once, oldest-activity-first eviction beyond this. */
export const MAX_SEEN_ROOTS = 300;

/**
 * Agents remembered per root.
 *
 * A root with more than a dozen distinct agents on it is not a list anyone
 * reads; it is a wall. Keeping the most recent is the reading that matches
 * what the section is for — "who has been working on this" — since the
 * long-departed are precisely the ones the relay can still explain via their
 * comments, if they left any.
 */
export const MAX_SEEN_AGENTS_PER_ROOT = 12;

/** Agent pubkey (normalised) → local-clock ms of the last sighting. */
export type SeenAgentsForRoot = Readonly<Record<string, number>>;

/** Root event id → the agents seen on it. */
export type ProjectSeenAgentsStore = Readonly<{
  version: 1;
  roots: Readonly<Record<string, SeenAgentsForRoot>>;
}>;

export const EMPTY_SEEN_AGENTS: SeenAgentsForRoot = Object.freeze({});

export const EMPTY_SEEN_AGENTS_STORE: ProjectSeenAgentsStore = Object.freeze({
  version: 1,
  roots: Object.freeze({}),
});

/** The most recent sighting in a root, used for root-level eviction order. */
function latestSighting(agents: SeenAgentsForRoot): number {
  let latest = 0;
  for (const seenAt of Object.values(agents)) {
    if (seenAt > latest) latest = seenAt;
  }
  return latest;
}

/**
 * Read a persisted blob, discarding anything that is not exactly the shape we
 * wrote.
 *
 * Field-by-field rather than a cast: this value has been sitting on disk
 * across upgrades, and a half-believed record would put a malformed key into
 * a pubkey position — which downstream becomes a name lookup, a click target,
 * and eventually a relay query for an identity that does not exist.
 */
export function parseSeenAgentsStore(value: unknown): ProjectSeenAgentsStore {
  if (!value || typeof value !== "object") return EMPTY_SEEN_AGENTS_STORE;
  const candidate = value as { version?: unknown; roots?: unknown };
  if (candidate.version !== 1) return EMPTY_SEEN_AGENTS_STORE;
  if (!candidate.roots || typeof candidate.roots !== "object") {
    return EMPTY_SEEN_AGENTS_STORE;
  }

  const roots: Record<string, SeenAgentsForRoot> = {};
  for (const [rootId, rawAgents] of Object.entries(
    candidate.roots as Record<string, unknown>,
  )) {
    if (!rootId || !rawAgents || typeof rawAgents !== "object") continue;
    const agents: Record<string, number> = {};
    for (const [pubkey, seenAt] of Object.entries(
      rawAgents as Record<string, unknown>,
    )) {
      if (typeof seenAt !== "number" || !Number.isFinite(seenAt)) continue;
      if (seenAt <= 0) continue;
      const normalized = normalizePubkey(pubkey);
      if (!normalized) continue;
      // Two spellings of one pubkey can only ever have collapsed on read, so
      // keep the later sighting rather than whichever came last in the object.
      agents[normalized] = Math.max(agents[normalized] ?? 0, seenAt);
    }
    if (Object.keys(agents).length > 0) roots[rootId] = agents;
  }

  return { version: 1, roots };
}

/**
 * Drop what has aged out and enforce the caps.
 *
 * Returns the same object when nothing needed removing, so a prune on every
 * write cannot by itself churn the snapshot identity and re-render the world.
 */
export function pruneSeenAgentsStore(
  store: ProjectSeenAgentsStore,
  nowMs: number,
): ProjectSeenAgentsStore {
  const horizon = nowMs - SEEN_AGENT_TTL_MS;
  let changed = false;
  const roots: Record<string, SeenAgentsForRoot> = {};

  for (const [rootId, agents] of Object.entries(store.roots)) {
    const entries = Object.entries(agents)
      .filter(([, seenAt]) => seenAt >= horizon)
      // Most recent first, so the cap keeps the agents still worth naming.
      // Pubkey breaks ties: two sightings inside the same quantised minute are
      // routine, and an arbitrary survivor would make the list flicker.
      .sort((a, b) => b[1] - a[1] || a[0].localeCompare(b[0]))
      .slice(0, MAX_SEEN_AGENTS_PER_ROOT);

    // Rebuild only the roots that actually lost something. Handing back a
    // fresh object for every root would make one root's expiry re-render every
    // component watching any other root — the exact churn the identity-stable
    // snapshot contract exists to prevent.
    if (entries.length === Object.keys(agents).length) {
      roots[rootId] = agents;
      continue;
    }
    changed = true;
    if (entries.length === 0) continue;
    roots[rootId] = Object.fromEntries(entries);
  }

  const rootIds = Object.keys(roots);
  if (rootIds.length > MAX_SEEN_ROOTS) {
    changed = true;
    const keep = rootIds
      .sort(
        (a, b) =>
          latestSighting(roots[b]) - latestSighting(roots[a]) ||
          a.localeCompare(b),
      )
      .slice(0, MAX_SEEN_ROOTS);
    const capped: Record<string, SeenAgentsForRoot> = {};
    for (const rootId of keep) capped[rootId] = roots[rootId];
    return { version: 1, roots: capped };
  }

  return changed ? { version: 1, roots } : store;
}

/**
 * Fold a sighting into the store, returning the same object when the sighting
 * told us nothing new.
 *
 * "Nothing new" is the common case by a wide margin — see
 * `SEEN_AGENT_REFRESH_MS` — and identity equality is what lets the caller run
 * this on every tick without thinking about it.
 *
 * Untouched roots keep their existing sub-object, so a consumer selecting one
 * root out of the snapshot gets a referentially stable value even when a
 * different root changed.
 */
export function recordSeenAgentsIn(
  store: ProjectSeenAgentsStore,
  rootId: string,
  agentPubkeys: readonly string[],
  nowMs: number,
): ProjectSeenAgentsStore {
  if (!rootId || agentPubkeys.length === 0) return store;

  const existing = store.roots[rootId] ?? EMPTY_SEEN_AGENTS;
  let next: Record<string, number> | null = null;

  for (const pubkey of agentPubkeys) {
    const normalized = normalizePubkey(pubkey);
    if (!normalized) continue;
    const previous = existing[normalized];
    if (previous !== undefined && nowMs - previous < SEEN_AGENT_REFRESH_MS) {
      continue;
    }
    next ??= { ...existing };
    next[normalized] = nowMs;
  }

  if (!next) return store;
  return pruneSeenAgentsStore(
    { version: 1, roots: { ...store.roots, [rootId]: next } },
    nowMs,
  );
}

function readStoredSeenAgents(): ProjectSeenAgentsStore {
  try {
    const raw = globalThis.localStorage?.getItem(STORAGE_KEY);
    if (!raw) return EMPTY_SEEN_AGENTS_STORE;
    return pruneSeenAgentsStore(
      parseSeenAgentsStore(JSON.parse(raw)),
      Date.now(),
    );
  } catch {
    // Unreadable or unparseable storage means we start with no memory, which
    // degrades this section to "live activity and comment authors only" — the
    // behaviour it would have had without this module at all.
    return EMPTY_SEEN_AGENTS_STORE;
  }
}

const listeners = new Set<() => void>();

let store: ProjectSeenAgentsStore = readStoredSeenAgents();

function persist(): void {
  try {
    globalThis.localStorage?.setItem(STORAGE_KEY, JSON.stringify(store));
  } catch {
    // Best effort. The in-memory record still serves this session, and this is
    // a display convenience — a quota toast here would be noise about nothing
    // the reader asked for.
  }
}

/** The whole memory, for tests and non-React callers. */
export function getProjectSeenAgents(): ProjectSeenAgentsStore {
  return store;
}

/**
 * Remember that these agents were observed working on this root.
 *
 * Safe to call on every render pass of a live subscription: repeat sightings
 * inside `SEEN_AGENT_REFRESH_MS` neither write nor notify.
 */
export function recordProjectSeenAgents(
  rootId: string,
  agentPubkeys: readonly string[],
  nowMs: number = Date.now(),
): void {
  const next = recordSeenAgentsIn(store, rootId, agentPubkeys, nowMs);
  if (next === store) return;
  store = next;
  persist();
  for (const listener of listeners) listener();
}

function subscribe(listener: () => void): () => void {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}

function getSnapshot(): ProjectSeenAgentsStore {
  return store;
}

/**
 * The agents remembered on one root.
 *
 * `useSyncExternalStore` wants a snapshot that is referentially stable between
 * real changes, so it subscribes to the whole store and the per-root selection
 * happens after. That is not a compromise: `recordSeenAgentsIn` preserves the
 * sub-object of every root it did not touch, so a component watching issue A
 * gets the identical value back when issue B's agents change, and its memo
 * chain stops there.
 */
export function useProjectSeenAgents(
  rootId: string | null | undefined,
): SeenAgentsForRoot {
  const snapshot = React.useSyncExternalStore(
    subscribe,
    getSnapshot,
    // No server rendering in this app, but the third argument is not optional
    // in spirit: a hook that throws under a non-DOM renderer is a hook that
    // cannot be unit-tested.
    () => EMPTY_SEEN_AGENTS_STORE,
  );
  return (rootId ? snapshot.roots[rootId] : undefined) ?? EMPTY_SEEN_AGENTS;
}

/** Drop the memory entirely. Tests only. */
export function resetProjectSeenAgentsForTests(): void {
  store = EMPTY_SEEN_AGENTS_STORE;
  try {
    globalThis.localStorage?.removeItem(STORAGE_KEY);
  } catch {
    // Nothing to undo if storage was never writable.
  }
  for (const listener of listeners) listener();
}
