import * as React from "react";

import {
  subscribeAgentObserverStore,
  getAgentObserverSnapshot,
  compareObserverEvents,
} from "@/features/agents/observerRelayStore";
import {
  createLivenessStore,
  type LivenessRecord,
} from "@/shared/lib/livenessStore";
import { normalizePubkey } from "@/shared/lib/pubkey";
import type { ObserverEvent } from "./ui/agentSessionTypes";

/**
 * Slowest liveness cadence we will believe, and the floor under the estimate.
 *
 * The harness emits `turn_liveness` every `BUZZ_ACP_TURN_LIVENESS_SECS`, which
 * deployments set — ours currently runs at 15s. This store used to hardcode the
 * 10s default and derive a 25s removal bound from it, which is wrong by
 * construction at any other setting: at a 15s cadence one dropped ping opens a
 * 30s hole, the bound fires at 25s, and a working badge vanishes in the middle
 * of a turn that is still running. So the cadence is *observed* (see
 * `observeCadence` below) rather than assumed, and only the bounds are
 * constants.
 *
 * The floor is the historical 10s: a burst of frames must not shrink the window
 * below what every previous build tolerated. The ceiling is 60s: past a minute
 * between pings the producer is not heartbeating in any useful sense, and the
 * right response to that is the bounded prune pause below — not an expiry
 * window that keeps growing with the silence it is supposed to detect.
 */
const LIVENESS_FLOOR_MS = 10_000;
const LIVENESS_CEILING_MS = 60_000;
/** Gaps the cadence estimate is taken over — enough to outvote one dropped ping. */
const LIVENESS_CADENCE_SAMPLES = 5;
/** Remove a turn after this many cadences with no activity. Tolerates one fully
 * dropped liveness ping plus slack before pruning a turn whose host died without
 * unwinding (kill -9 / crash) — the only case that reaches this bound, since
 * graceful exits clear via turn_completed and working turns refresh on every
 * stream event. At the 10s floor this is the historical 25s; at a 15s cadence it
 * is 37.5s, which is what makes a single dropped ping survivable there. */
const REMOVE_AFTER_CADENCES = 2.5;
/** Pause pruning for an agent once ALL of its tracked turns have gone this many
 * cadences without activity — the "all at once" signature of that agent's frame
 * stream being down. Below REMOVE_AFTER_CADENCES so the pause engages before the
 * prune would wipe badges. */
const FRAME_GAP_PAUSE_CADENCES = 2;
/** A silent agent is treated as dead after this bounded prune pause. */
const PRUNE_PAUSE_MAX_MS = 3 * 60_000;
/** Maximum concurrent active turns tracked per agent. Purely an unbounded-growth
 * guard, so it sits at the harness's hard upper bound for parallel agent
 * subprocesses (`--agents` / `BUZZ_ACP_AGENTS` accepts `1..=32`) rather than the
 * Desktop default of 24 — any lower value silently evicts a live turn, dropping
 * its working badge. */
const MAX_TURNS_PER_AGENT = 32;
/** Cap on per-agent terminal tombstones (A's resurrection guard). Only the
 * most recently completed turns can be raced by a late liveness frame; older
 * ones are already below the watermark, so a small multiple of the live cap is
 * ample and keeps the map from growing across a long session. */
const MAX_TERMINAL_TOMBSTONES = MAX_TURNS_PER_AGENT * 4;
/** Interval for pruning stale/expired turns. */
const PRUNE_INTERVAL_MS = 5_000;

type ActiveTurn = {
  turnId: string;
  channelId: string;
  /** Normalized pubkey of the agent running it — the store's liveness group. */
  agentKey: string;
  startedAt: number;
};

/** Store key. Turn ids are only unique per agent, so the agent is part of it. */
function turnKey(agentKey: string, turnId: string): string {
  return `${agentKey}|${turnId}`;
}

/** One working channel surfaced to the UI, anchored to the desktop clock. */
export type ActiveTurnSummary = {
  channelId: string;
  anchorAt: number;
};

/** One channel with active agent work, aggregated across agents. */
export type ActiveChannelTurnSummary = {
  channelId: string;
  anchorAt: number;
  agentCount: number;
  agentPubkeys: string[];
  agentNames?: string[];
};

/**
 * The live turns, on the shared liveness core.
 *
 * The core owns what is generic: the entry map, subscribers, the cadence
 * estimate and the windows derived from it, the group-wide prune pause, and a
 * sweep that only runs while there are turns *and* somebody watching. What
 * stays in this module is what is about *these* frames — the observer
 * watermark, agent-host clock skew, terminal tombstones and the resurrection
 * guard, the per-agent turn cap, and the channel aggregations the UI reads.
 * None of those generalize, and pushing them down would have made the core a
 * second copy of this file with holes in it.
 *
 * Each agent is its own liveness group: one agent's frame stream going quiet
 * says nothing about another's, so cadence and the pause verdict are per agent.
 */
const turnsStore = createLivenessStore<ActiveTurn>({
  groupOf: (turn) => turn.agentKey,
  cadence: {
    kind: "adaptive",
    floorMs: LIVENESS_FLOOR_MS,
    ceilingMs: LIVENESS_CEILING_MS,
    sampleWindow: LIVENESS_CADENCE_SAMPLES,
  },
  expiryMultiplier: REMOVE_AFTER_CADENCES,
  pause: {
    gapMultiplier: FRAME_GAP_PAUSE_CADENCES,
    maxMs: PRUNE_PAUSE_MAX_MS,
  },
  pruneIntervalMs: PRUNE_INTERVAL_MS,
  onInvalidate: (agentKey) => invalidateCache(agentKey),
});

// Per-agent clock offset: the desktop clock minus the agent-host clock, in
// milliseconds. Estimated as the running minimum of
// (Date.now() - Date.parse(event.timestamp)) across that agent's events. The
// minimum converges on true skew minus the smallest network/processing delay
// seen — a monotonically tightening estimate immune to per-event jitter. While
// true skew is constant or shrinking it is conservative: elapsed under-reports
// by the minimum delay and never inflates. The minimum never loosens, so under
// GROWING skew (an NTP step forward, or the host clock drifting further behind
// mid-session) the stored estimate goes stale-too-small and elapsed can over-
// report — bounded by how far the skew grows, sub-second over a session. A
// turn's badge anchor is startedAt + offset: the agent's own start, translated
// into desktop-clock terms. Anchors are derived at read time so a later, tighter
// offset retroactively corrects every live turn — distinct agent starts then
// yield distinct anchors (no lockstep) and a turn started long ago anchors into
// the past (large elapsed) instead of resetting to Date.now().
const clockOffsetByAgent = new Map<string, number>();

// Cached snapshots for useSyncExternalStore reference stability.
// Only regenerated when the underlying turn map for an agent actually changes.
const cachedTurnSummaries = new Map<string, ActiveTurnSummary[]>();
let cachedChannelTurnSummaries: ActiveChannelTurnSummary[] | null = null;

// Composite watermark per agent: the newest observer event processed, by
// (timestamp, seq) ordering. An event is processed only if it is strictly
// newer than this — making full-buffer replays idempotent and post-restart
// streams (seq resets to 1, timestamp keeps climbing) handled for free.
const lastProcessed = new Map<string, ObserverEvent>();

// Per-agent record of when each turn terminally ended (turnId →
// terminal-event timestamp, in agent-host clock ms). endTurn hard-deletes a
// turn with no surviving record, so without this a late liveness frame for an
// already-completed turn would resurrect a dead badge. Resurrection (A) checks
// this: a turn is revived only if the recovered liveness is strictly newer
// than its recorded terminal timestamp.
const terminalAtByAgent = new Map<string, Map<string, number>>();

function invalidateCache(agentKey: string) {
  cachedTurnSummaries.delete(agentKey);
  cachedChannelTurnSummaries = null;
}

function notifyListeners() {
  turnsStore.notify();
}

/**
 * Refine this agent's clock-offset estimate from one observer event. Samples
 * Date.now() - Date.parse(timestamp) and keeps the running minimum. When the
 * minimum tightens, every live anchor for the agent shifts, so the cache is
 * invalidated. Events with an unparseable timestamp contribute no sample.
 * Returns true when the offset changed.
 */
function sampleClockOffset(agentKey: string, timestamp: string): boolean {
  const sample = Date.now() - Date.parse(timestamp);
  if (Number.isNaN(sample)) return false;
  const prior = clockOffsetByAgent.get(agentKey);
  if (prior !== undefined && sample >= prior) return false;
  clockOffsetByAgent.set(agentKey, sample);
  invalidateCache(agentKey);
  return true;
}

function parseTimestamp(timestamp: string): number | null {
  const parsed = Date.parse(timestamp);
  return Number.isFinite(parsed) ? parsed : null;
}

function startTurn(
  agentPubkey: string,
  channelId: string,
  turnId: string,
  timestamp: string,
) {
  const key = normalizePubkey(agentPubkey);
  const entryKey = turnKey(key, turnId);

  // Cap at MAX_TURNS_PER_AGENT — evict oldest if exceeded
  if (
    turnsStore.groupSize(key) >= MAX_TURNS_PER_AGENT &&
    !turnsStore.has(entryKey)
  ) {
    let oldest: LivenessRecord<ActiveTurn> | null = null;
    for (const record of turnsStore.recordsInGroup(key)) {
      if (!oldest || record.value.startedAt < oldest.value.startedAt) {
        oldest = record;
      }
    }
    if (oldest) {
      turnsStore.drop(oldest.key);
    }
  }

  const startedAt = parseTimestamp(timestamp) ?? Date.now();
  // No `frameAtMs`: the agent host is on its own clock (see the skew estimate
  // above), so the removal window must be measured from when WE saw the frame.
  turnsStore.upsert(entryKey, { turnId, channelId, agentKey: key, startedAt });
}

function recordActivity(agentPubkey: string, turnId: string | null): boolean {
  if (!turnId) return false;
  return turnsStore.refresh(turnKey(normalizePubkey(agentPubkey), turnId));
}

/**
 * A — resurrect a badge that was pruned out from under a still-running turn.
 * A recovered liveness/acp frame for a turn no longer in the live map recreates
 * it, UNLESS C's tombstone shows the turn already terminally ended at or after
 * this frame's time (a stale frame must not revive a completed turn). The frame
 * may carry its original `startedAt` envelope field; when valid and not later
 * than the frame, preserve the elapsed timer by anchoring to that timestamp.
 * Old, malformed, or impossible future starts fall back to the recovery
 * timestamp. Returns true on revive.
 */
function resurrectTurn(agentPubkey: string, event: ObserverEvent): boolean {
  if (!event.turnId || !event.channelId) return false;
  const key = normalizePubkey(agentPubkey);
  const terminalAt = terminalAtByAgent.get(key)?.get(event.turnId);
  const frameAt = parseTimestamp(event.timestamp);
  // Only revive when this frame is strictly newer than the recorded terminal.
  if (terminalAt !== undefined && (frameAt === null || frameAt <= terminalAt)) {
    return false;
  }
  const startedAt =
    typeof event.startedAt === "string" &&
    parseTimestamp(event.startedAt) !== null
      ? event.startedAt
      : event.timestamp;
  const startedAtMs = parseTimestamp(startedAt);
  const safeStartedAt =
    frameAt !== null && startedAtMs !== null && startedAtMs <= frameAt
      ? startedAt
      : event.timestamp;
  startTurn(agentPubkey, event.channelId, event.turnId, safeStartedAt);
  return true;
}

function recordTerminal(agentKey: string, turnId: string, terminalAt: number) {
  if (!Number.isFinite(terminalAt)) return;
  let terminals = terminalAtByAgent.get(agentKey);
  if (!terminals) {
    terminals = new Map();
    terminalAtByAgent.set(agentKey, terminals);
  }
  terminals.set(turnId, terminalAt);
  // Bound the tombstone map: only recently-completed turns can be the target of
  // a racing late liveness frame (older ones are already below the watermark).
  // Evict the oldest terminal once past the cap so the map can't grow unbounded
  // across a long session. Insertion order tracks completion order closely
  // enough; the first key is the oldest survivor.
  if (terminals.size > MAX_TERMINAL_TOMBSTONES) {
    const oldest = terminals.keys().next().value;
    if (oldest !== undefined) terminals.delete(oldest);
  }
}

function endTurn(
  agentPubkey: string,
  turnId: string | null,
  channelId: string | null,
  terminalAt: number,
) {
  const key = normalizePubkey(agentPubkey);
  // Tombstone the terminal time so a late liveness frame can't resurrect a
  // completed turn (A's guard). With an explicit turnId this is recorded even
  // when the turn was already pruned and the agent's live map is gone — the
  // completion is authoritative and must outlive the active record.
  if (turnId) {
    recordTerminal(key, turnId, terminalAt);
    turnsStore.drop(turnKey(key, turnId));
    return;
  }

  if (channelId) {
    // Fallback: remove by channelId if turnId not available. Tombstone the
    // resolved turn so a later stale liveness for it can't resurrect a badge.
    const [removed] = turnsStore.take(
      (record) => record.group === key && record.value.channelId === channelId,
      1,
    );
    if (removed) {
      recordTerminal(key, removed.value.turnId, terminalAt);
    }
  }
}

// INVARIANT: events must be sorted by (timestamp, seq) ascending.
// syncAgentTurnsFromEvents receives sorted arrays from observerRelayStore.
// Calling with unsorted events will cause silent data loss.
function processEvent(agentPubkey: string, event: ObserverEvent) {
  const key = normalizePubkey(agentPubkey);

  // Gate every event kind on the watermark uniformly: process only events
  // strictly newer than the last one seen for this agent. With sorted buffers
  // (the documented invariant), this makes full-buffer replays a complete
  // no-op. Evictions must be gated too — replaying a stale turn_error/
  // agent_panic (emitted with a null turnId) would otherwise fall back to
  // deleting the first turn in the channel, killing the live turn. Resurrection
  // (the turn_liveness/acp case below) is gated here too: it runs only for a
  // frame that passes the watermark, so replayed stale frames cannot revive a
  // pruned turn, and the per-turn terminal tombstone blocks reviving a turn
  // that already completed.
  const last = lastProcessed.get(key);
  if (last && compareObserverEvents(event, last) <= 0) {
    return;
  }
  lastProcessed.set(key, event);

  // Refine the clock offset from every fresh event. A tighter offset shifts
  // every live anchor for this agent, so a change must reach the UI even when
  // the event itself surfaces no new turn.
  const offsetChanged = sampleClockOffset(key, event.timestamp);

  switch (event.kind) {
    case "turn_started":
      if (event.channelId) {
        startTurn(
          agentPubkey,
          event.channelId,
          event.turnId ?? `seq-${event.seq}`,
          event.timestamp,
        );
        notifyListeners();
        return;
      }
      break;
    case "turn_completed":
    case "turn_error":
    case "agent_panic":
      endTurn(
        agentPubkey,
        event.turnId ?? null,
        event.channelId ?? null,
        Date.parse(event.timestamp),
      );
      notifyListeners();
      return;
    case "acp_read":
    case "acp_write":
    // turn_liveness keeps a quiet-but-alive turn from being pruned; same
    // refresh-only path as stream activity — no surfaced summary change on its
    // own, so it only notifies when the offset above actually moved. If the
    // turn was pruned out from under a still-running host (a transient drop
    // raced the pause, or the lone-crash residual self-healed), resurrect it.
    case "turn_liveness": {
      if (event.kind === "turn_liveness") {
        // The heartbeat is the only frame kind emitted on a fixed schedule, so
        // it is the only one whose spacing measures the harness's configured
        // cadence. ACP frames are stream traffic — bursty by nature, and
        // sampling them would read a quiet stretch of a turn as a slow
        // heartbeat. The agent host's own timestamps are used because the gap
        // being measured is the producer's interval, not our delivery jitter.
        turnsStore.observeCadence(key, parseTimestamp(event.timestamp));
      }
      const refreshed = recordActivity(agentPubkey, event.turnId ?? null);
      if (!refreshed && resurrectTurn(agentPubkey, event)) {
        notifyListeners();
        return;
      }
      break;
    }
  }

  if (offsetChanged) {
    notifyListeners();
  }
}

export function subscribeActiveAgentTurns(listener: () => void) {
  return turnsStore.subscribe(listener);
}

/**
 * Returns the channels where the given agent has active turns, sorted by
 * channelId, each anchored to the earliest `anchorAt` for that channel.
 * The array reference is cached and stable until the turn map mutates — a
 * requirement for `useSyncExternalStore`.
 */
export function getActiveTurnsForAgent(
  agentPubkey: string | null | undefined,
): ActiveTurnSummary[] {
  if (!agentPubkey) return EMPTY_TURNS;
  const key = normalizePubkey(agentPubkey);
  const agentTurns = turnsStore.listGroup(key);
  if (agentTurns.length === 0) return EMPTY_TURNS;

  const cached = cachedTurnSummaries.get(key);
  if (cached) return cached;

  const offset = clockOffsetByAgent.get(key) ?? 0;

  // Collapse multiple turns in one channel to the earliest start — the badge
  // should count from when the channel's oldest live turn began. Anchors are
  // derived here (startedAt + offset) so the latest skew estimate applies.
  const earliestByChannel = new Map<string, number>();
  for (const turn of agentTurns) {
    const prior = earliestByChannel.get(turn.channelId);
    if (prior === undefined || turn.startedAt < prior) {
      earliestByChannel.set(turn.channelId, turn.startedAt);
    }
  }

  const result = [...earliestByChannel.entries()]
    .map(([channelId, startedAt]) => ({
      channelId,
      anchorAt: startedAt + offset,
    }))
    .sort((a, b) => a.channelId.localeCompare(b.channelId));
  cachedTurnSummaries.set(key, result);
  return result;
}

const EMPTY_TURNS: ActiveTurnSummary[] = [];
const EMPTY_CHANNEL_TURNS: ActiveChannelTurnSummary[] = [];

/**
 * Returns active working channels across all tracked agents, sorted by
 * channelId and anchored to the earliest live turn in each channel.
 */
export function getActiveTurnsByChannel(): ActiveChannelTurnSummary[] {
  if (cachedChannelTurnSummaries) return cachedChannelTurnSummaries;
  if (turnsStore.size() === 0) return EMPTY_CHANNEL_TURNS;

  const summaries = new Map<
    string,
    { anchorAt: number; agentPubkeys: Set<string> }
  >();

  for (const turn of turnsStore.list()) {
    const offset = clockOffsetByAgent.get(turn.agentKey) ?? 0;
    const anchorAt = turn.startedAt + offset;
    const summary = summaries.get(turn.channelId);
    if (!summary) {
      summaries.set(turn.channelId, {
        anchorAt,
        agentPubkeys: new Set([turn.agentKey]),
      });
      continue;
    }

    summary.agentPubkeys.add(turn.agentKey);
    if (anchorAt < summary.anchorAt) {
      summary.anchorAt = anchorAt;
    }
  }

  const result = [...summaries.entries()]
    .map(([channelId, summary]) => ({
      channelId,
      anchorAt: summary.anchorAt,
      agentCount: summary.agentPubkeys.size,
      agentPubkeys: [...summary.agentPubkeys].sort(),
    }))
    .sort((a, b) => a.channelId.localeCompare(b.channelId));
  cachedChannelTurnSummaries = result;
  return result;
}

/**
 * Synchronize the active-turns store with the latest observer events for a
 * given agent.
 */
export function syncAgentTurnsFromEvents(
  agentPubkey: string,
  events: ObserverEvent[],
) {
  for (const event of events) {
    processEvent(agentPubkey, event);
  }
}

/**
 * Hook: returns the channels where the given agent is currently working, each
 * with the desktop-clock `anchorAt` to anchor a live elapsed counter.
 * Re-renders when the set of channels changes — not when the clock ticks.
 */
export function useActiveAgentTurns(
  agentPubkey: string | null | undefined,
): ActiveTurnSummary[] {
  const getSnapshot = React.useCallback(
    () => getActiveTurnsForAgent(agentPubkey),
    [agentPubkey],
  );

  return React.useSyncExternalStore(subscribeActiveAgentTurns, getSnapshot);
}

/**
 * Hook: returns channels with active agent work across all tracked agents.
 * Re-renders when the channel set changes — not when the clock ticks.
 */
export function useActiveAgentTurnsByChannel(): ActiveChannelTurnSummary[] {
  return React.useSyncExternalStore(
    subscribeActiveAgentTurns,
    getActiveTurnsByChannel,
  );
}

/**
 * Sync every running/deployed agent's observer events into the active-turns
 * store. Extracted from the bridge hook so a regression can drive the exact
 * observer→derived-liveness path without a React renderer.
 */
export function syncActiveAgentTurnsFromObserver(
  agents: readonly { pubkey: string; status: string }[],
) {
  for (const agent of agents) {
    if (agent.status !== "running" && agent.status !== "deployed") continue;
    const snapshot = getAgentObserverSnapshot(agent.pubkey, true);
    syncAgentTurnsFromEvents(agent.pubkey, snapshot.events);
  }
}

/**
 * Bridge hook: processes observer events into the active-turns store.
 * Should be called by a parent component that has access to the observer events.
 */
export function useActiveAgentTurnsBridge(
  agents: readonly { pubkey: string; status: string }[],
) {
  React.useEffect(() => {
    function syncAll() {
      syncActiveAgentTurnsFromObserver(agents);
    }

    syncAll();
    return subscribeAgentObserverStore(syncAll);
  }, [agents]);
}

/**
 * Immediately clear all active turns for a specific agent — called when
 * Desktop itself stops or restarts the agent, so the turn store doesn't
 * have to wait for the 3-minute prune-pause backstop.
 *
 * Preserves `lastProcessed` (the watermark) so a full-buffer replay after
 * the clear is still a no-op — without the watermark a replayed
 * `turn_started` would immediately resurrect the badge.  Preserves
 * `clockOffsetByAgent` — the offset remains valid and harmless.
 *
 * Tombstones every cleared turn (C) so an in-flight `turn_liveness` frame
 * already on the wire at kill time cannot resurrect the badge via
 * `resurrectTurn`.  A restarted agent's genuinely new turns carry new
 * turnIds / newer timestamps, so the tombstones don't block them.
 */
export function clearActiveTurnsForAgent(agentPubkey: string): void {
  const key = normalizePubkey(agentPubkey);
  const agentTurns = turnsStore.listGroup(key);
  if (agentTurns.length === 0) return;

  const agentClockNow = Date.now() - (clockOffsetByAgent.get(key) ?? 0);
  for (const turn of agentTurns) {
    recordTerminal(key, turn.turnId, agentClockNow);
  }

  turnsStore.clearGroup(key);
  notifyListeners();
}

/**
 * Clears all live turn state (active turns, offsets, watermarks, tombstones).
 * Intentionally preserves `savedByCommunity` — community-switch snapshots
 * must survive the reset that runs between save and restore.
 */
export function resetActiveAgentTurnsStore() {
  turnsStore.clear();
  lastProcessed.clear();
  clockOffsetByAgent.clear();
  cachedTurnSummaries.clear();
  cachedChannelTurnSummaries = null;
  terminalAtByAgent.clear();
  notifyListeners();
}

// ---------------------------------------------------------------------------
// Community-switch save / restore
// ---------------------------------------------------------------------------

type TurnsStoreSnapshot = {
  turns: ActiveTurn[];
  offsets: Map<string, number>;
  watermarks: Map<string, ObserverEvent>;
  terminals: Map<string, Map<string, number>>;
};

/** Per-community snapshots. Keyed by community ID. */
const savedByCommunity = new Map<string, TurnsStoreSnapshot>();

/**
 * Snapshot the current active-turns state under `communityId` so it can be
 * restored when the user switches back.  If both the turns map and the
 * tombstone map are empty there is nothing worth restoring — discard any
 * previously-saved snapshot instead.
 *
 * Deep-clones everything it keeps so subsequent mutations on the live state do
 * not corrupt the snapshot.
 */
export function saveActiveAgentTurnsForCommunity(communityId: string): void {
  if (turnsStore.size() === 0 && terminalAtByAgent.size === 0) {
    savedByCommunity.delete(communityId);
    return;
  }

  // Clone the turn structs (plain values, no nested references). Liveness
  // metadata is deliberately not carried across: a restore is a fresh sighting
  // of these turns, and the store re-stamps them on the way back in.
  const turns = turnsStore.list().map((turn) => ({ ...turn }));

  // Shallow-clone scalar maps (primitives as values).
  const offsets = new Map(clockOffsetByAgent);
  const watermarks = new Map(lastProcessed);

  // Deep-clone terminalAtByAgent: outer map + inner per-agent maps.
  const terminals = new Map<string, Map<string, number>>();
  for (const [agentKey, tombstones] of terminalAtByAgent) {
    terminals.set(agentKey, new Map(tombstones));
  }

  savedByCommunity.set(communityId, { turns, offsets, watermarks, terminals });
}

/**
 * Restore a previously saved active-turns snapshot for `communityId` into the
 * module maps.  No-op when no snapshot exists.
 *
 * Clears all four module maps before writing so the function is
 * self-contained — it replaces rather than merging, regardless of whether the
 * caller pre-cleared.  At the primary call site (`useCommunityInit`) the maps
 * are already empty after `resetCommunityState()`, but this guard makes the
 * contract explicit.
 *
 * Re-inserting through the store re-stamps each turn as seen *now*, so the
 * prune sweep doesn't immediately kill turns that were saved longer ago than
 * the removal window.  New observer events arriving after restore refresh them
 * normally via `recordActivity`.  The observed cadence is not restored — the
 * store falls back to the floor until the agent's next two liveness frames
 * re-establish it, which is the conservative direction (a shorter window, i.e.
 * exactly the pre-adaptive behavior) for the ~30s that takes.
 *
 * Consumes the snapshot (deletes it from `savedByCommunity`) — a given
 * community's snapshot is only usable once per round-trip.
 */
export function restoreActiveAgentTurnsForCommunity(communityId: string): void {
  const snap = savedByCommunity.get(communityId);
  if (!snap) return;
  savedByCommunity.delete(communityId);

  // Clear before writing so this is a replace, not a merge.
  turnsStore.clear();
  clockOffsetByAgent.clear();
  lastProcessed.clear();
  terminalAtByAgent.clear();

  for (const turn of snap.turns) {
    turnsStore.upsert(turnKey(turn.agentKey, turn.turnId), { ...turn });
  }

  for (const [agentKey, offset] of snap.offsets) {
    clockOffsetByAgent.set(agentKey, offset);
  }

  for (const [agentKey, event] of snap.watermarks) {
    lastProcessed.set(agentKey, event);
  }

  for (const [agentKey, tombstones] of snap.terminals) {
    terminalAtByAgent.set(agentKey, new Map(tombstones));
  }

  cachedTurnSummaries.clear();
  cachedChannelTurnSummaries = null;
  notifyListeners();
}

/**
 * Discard the saved turn-state snapshot for a community that has been
 * permanently deleted so the entry doesn't sit in memory indefinitely.
 * Call this alongside the other relay-specific GC in `removeCommunity`.
 */
export function clearSavedCommunitySnapshot(communityId: string): void {
  savedByCommunity.delete(communityId);
}
