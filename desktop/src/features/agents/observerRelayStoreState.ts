import type { RelayEvent, ControlResultFrame } from "@/shared/api/types";
import type { AgentManagementRequest } from "./agentManagement";
import type { ProjectChannelRequest } from "@/features/projects/projectChannelRequest";
import type {
  ConnectionState,
  ObserverEvent,
  TranscriptItem,
} from "./ui/agentSessionTypes";
import type { TranscriptState } from "./ui/agentSessionTranscript";
import { normalizePubkey } from "@/shared/lib/pubkey";

export const MAX_OBSERVER_EVENTS = 3000;
// Length the per-agent journal is evicted down to when it overflows
// MAX_OBSERVER_EVENTS. Eviction rebuilds the transcript from the retained
// window (see appendAgentEvents), so trimming back to exactly the cap re-arms
// eviction on the very next append — every steady-state append then replays the
// whole history. Leaving 10% headroom amortizes one rebuild across the ~300
// appends that refill it, while keeping the window within the cap. Expressed as
// a fraction (not a fixed count) so the same math stays correct if the cap is
// ever made per-agent, where a fixed headroom could exceed a smaller cap.
export const OBSERVER_EVENTS_LOW_WATER = Math.floor(MAX_OBSERVER_EVENTS * 0.9);
export const MAX_PENDING_UNKNOWN_AGENT_FRAMES = 100;

export type ObserverSnapshot = {
  connectionState: ConnectionState;
  errorMessage: string | null;
  events: ObserverEvent[];
};

export const IDLE_SNAPSHOT: ObserverSnapshot = {
  connectionState: "idle",
  errorMessage: null,
  events: [],
};

export const EMPTY_EVENTS: ObserverEvent[] = [];
export const EMPTY_TRANSCRIPT: TranscriptItem[] = [];

export type AgentObserverStoreUpdate = {
  agentPubkey: string;
  events: readonly ObserverEvent[];
};

export type AgentObserverStoreListener = (
  update?: AgentObserverStoreUpdate,
) => void;

export const listeners = new Set<AgentObserverStoreListener>();
export const eventsByAgent = new Map<string, ObserverEvent[]>();
export const transcriptByAgent = new Map<string, TranscriptState>();
export const snapshotByAgent = new Map<string, ObserverSnapshot>();

// Per-agent eviction floor: the ordering key of the newest event that eviction
// has ever discarded for this agent. Once the journal is trimmed to the
// low-water mark, the dedup set (built only from the retained array) no longer
// remembers the discarded frames, so a delayed/replayed relay frame at or below
// that boundary would be re-admitted into the headroom — and a later refill to
// the cap would then trim away 300 legitimate retained events with no new
// activity. The floor rejects any arrival at or before it (equal included: the
// floor event itself was evicted), so already-evicted history can never
// re-enter. Cleared with the observer store; only advances forward.
export const evictionFloorByAgent = new Map<
  string,
  { timestamp: string; seq: number }
>();

// Channel-scoped archive event journal — holds paged history loaded from the local
// SQLite archive without the MAX_OBSERVER_EVENTS live-relay cap. Keyed by
// `${normalizedAgentPubkey}:${channelId}`. The live relay path writes to
// `eventsByAgent` (per-agent, capped) and this map is NEVER written by live
// events — separation is strict so loading deep history can never evict live frames
// or vice versa. UI consumers merge the raw events from both sources, then derive
// TranscriptState once over the combined window.
export const archiveEventsByChannel = new Map<string, ObserverEvent[]>();

// Per-agent, per-channel latest-live-session-id.
// Key: `${normalizePubkey(agentPubkey)}:${channelId}`.
// Set when a live relay observer event with a sessionId arrives.
// Cleared in resetAgentObserverStore.
//
// "Latest-live" means: the sessionId that most recently appeared via the
// live relay path (handleRelayObserverEvent). It is NOT derived from
// connectionState or an ever-live Set — an ever-live Set would incorrectly
// mark session A as "current" after session B has started (Thufir Pass 3).
//
// Stored as `{ sessionId, timestamp, seq }` so that late-arriving live frames
// from an older session never regress the latest-live id. We only advance when
// the parsed event sorts strictly AFTER the stored one, using the same
// two-key ordering as `compareObserverEvents`: timestamp first, then seq on a
// tie — so a higher-seq frame at equal timestamp still advances the entry.
export type LatestLiveEntry = {
  sessionId: string;
  timestamp: string;
  seq: number;
};
export const latestLiveSessionByAgentChannel = new Map<
  string,
  LatestLiveEntry
>();

export function liveSessionKey(
  agentPubkey: string,
  channelId: string | null,
): string {
  return `${normalizePubkey(agentPubkey)}:${channelId ?? ""}`;
}

/** Read the latest-live-session-id for a (agent, channel) pair. */
export function getLatestLiveSessionId(
  agentPubkey: string | null | undefined,
  channelId: string | null | undefined,
): string | null {
  if (!agentPubkey) return null;
  return (
    latestLiveSessionByAgentChannel.get(
      liveSessionKey(agentPubkey, channelId ?? null),
    )?.sessionId ?? null
  );
}

// Per-agent listeners for `control_result` frames. The ModelPicker subscribes
// here to learn the async outcome of a `switch_model` frame (the send is
// fire-and-forget; the harness replies out-of-band over the observer relay).
export const controlResultListeners = new Map<
  string,
  Set<(frame: ControlResultFrame) => void>
>();

export const agentManagementListeners = new Set<
  (agentPubkey: string, request: AgentManagementRequest) => void
>();
export const projectChannelRequestListeners = new Set<
  (agentPubkey: string, request: ProjectChannelRequest) => void
>();

// Normalized pubkeys of agents we are actively managing. Only events whose
// "agent" tag matches an entry here will be decrypted (defense-in-depth).
//
// This set is the *union* of every active subscriber's contribution. Multiple
// callers of `useManagedAgentObserverBridge` (e.g. the channel screen and the
// profile panel) can be mounted at once, each tracking a different agent list.
// We key each subscriber's contribution in `knownAgentsBySubscription` and
// recompute the union, so co-mounted callers no longer clobber each other.
export const knownAgentPubkeys = new Set<string>();
export const knownAgentsBySubscription = new Map<string, Set<string>>();
export const pendingUnknownAgentFrames: RelayEvent[] = [];
