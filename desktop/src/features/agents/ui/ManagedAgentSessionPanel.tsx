import * as React from "react";
import {
  CircleAlert,
  CircleDot,
  Clock3,
  TerminalSquare,
  XCircle,
} from "lucide-react";

import { isManagedAgentActive } from "@/features/agents/lib/managedAgentControlActions";
import type { UserProfileLookup } from "@/features/profile/lib/identity";
import type { ManagedAgent } from "@/shared/api/types";
import { cn } from "@/shared/lib/cn";
import { Badge } from "@/shared/ui/badge";
import { Skeleton } from "@/shared/ui/skeleton";
import { Spinner } from "@/shared/ui/spinner";
import {
  AgentSessionTranscriptList,
  type AgentSessionTranscriptEmptyState,
} from "./AgentSessionTranscriptList";
import { RawEventRail } from "./RawEventRail";
import type {
  ConnectionState,
  ObserverEvent,
  TranscriptItem,
} from "./agentSessionTypes";
import type { AgentSessionTranscriptVariant } from "./agentSessionTranscriptContext";
import {
  deriveLatestSessionId,
  mergeObserverEventWindows,
  resolveDisplayEvents,
  resolveRawRailLayout,
  scopeByChannel,
} from "./agentSessionPanelLayout";
import { shorten } from "./agentSessionUtils";
import {
  useObserverEvents,
  useArchivedChannelEvents,
  useUnattributedAgentFrames,
} from "./useObserverEvents";
import { buildTranscriptState } from "./agentSessionTranscript";

type ManagedAgentSessionPanelProps = {
  agent: Pick<ManagedAgent, "pubkey" | "name" | "status"> & {
    avatarUrl?: string | null;
  };
  autoTail?: boolean;
  channelId?: string | null;
  className?: string;
  emptyDescription?: string;
  emptyState?: AgentSessionTranscriptEmptyState;
  panelPadding?: boolean;
  rawLayout?: "responsive" | "exclusive";
  showHeader?: boolean;
  showRaw?: boolean;
  transcriptContentClassName?: string;
  transcriptVariant?: AgentSessionTranscriptVariant;
  profiles?: UserProfileLookup;
  rawEventsOverride?: ObserverEvent[];
  transcriptOverride?: TranscriptItem[];
};

export function ManagedAgentSessionPanel({
  agent,
  autoTail = false,
  channelId = null,
  className,
  emptyDescription = "Mention this agent in a channel to watch the next turn.",
  emptyState = "idle",
  panelPadding = true,
  rawLayout = "responsive",
  showHeader = true,
  showRaw = true,
  transcriptContentClassName,
  transcriptVariant = "default",
  profiles,
  rawEventsOverride,
  transcriptOverride,
}: ManagedAgentSessionPanelProps) {
  const managedRuntimeActive = isManagedAgentActive(agent);
  // An owned agent need not be launched by this Desktop's managed-agent
  // service. Hermes gateway is externally supervised but publishes the same
  // owner-scoped NIP-AO frames as a local ACP process. Listen unconditionally
  // for the selected agent so its first ephemeral frame can prove that an
  // observer exists; gating the subscription on local process state makes that
  // proof impossible.
  const { connectionState, errorMessage, events } = useObserverEvents(
    Boolean(agent.pubkey),
    agent.pubkey,
  );

  // Channel-scoped live events (capped at MAX_OBSERVER_EVENTS) and uncapped
  // archived events from SQLite paging. Both are raw ObserverEvent[] — we merge
  // them at the raw-event level and derive a single TranscriptState, so stateful
  // aggregates (tool start/update, plan replacement, permission request/response)
  // are never split across two independent state machines.
  const archivedChannelEvents = useArchivedChannelEvents(
    agent.pubkey,
    channelId,
  );

  const scopedLiveEvents = React.useMemo(
    () => scopeByChannel(events, channelId),
    [channelId, events],
  );

  // Combined raw window: live (scoped) + archive merged by (seq, timestamp),
  // sorted ascending. Used as the single source for both the transcript and the
  // raw event rail / header count.
  const combinedEvents = React.useMemo(
    () => mergeObserverEventWindows(scopedLiveEvents, archivedChannelEvents),
    [scopedLiveEvents, archivedChannelEvents],
  );
  const hasObserver = managedRuntimeActive || combinedEvents.length > 0;

  // This agent's frames are reaching us and being refused before decryption
  // because it is not attributed to this account. Without this the pane is
  // indistinguishable from an idle agent, and the absence of events reads as
  // "nothing has happened" rather than "we are throwing your telemetry away".
  const unattributed = useUnattributedAgentFrames(agent.pubkey);

  // Derive transcript once from the combined raw window. When transcriptOverride
  // is set (e.g. E2E snapshot specs), bypass both — the caller supplies the full
  // transcript directly.
  const derivedTranscript = React.useMemo(
    () => buildTranscriptState(combinedEvents).items,
    [combinedEvents],
  );
  const displayTranscript = transcriptOverride ?? derivedTranscript;

  const displayEvents = React.useMemo(
    () => resolveDisplayEvents(combinedEvents, rawEventsOverride),
    [rawEventsOverride, combinedEvents],
  );

  const latestSessionId = React.useMemo(
    () => deriveLatestSessionId(displayEvents),
    [displayEvents],
  );

  return (
    <section
      className={cn(
        "rounded-lg border border-border/70 bg-background/80 shadow-xs",
        panelPadding && "p-4",
        autoTail && "flex flex-col overflow-hidden",
        className,
      )}
    >
      {showHeader ? (
        <SessionHeader
          connectionState={connectionState}
          eventCount={displayEvents.length}
          hasObserver={hasObserver}
          latestSessionId={latestSessionId}
        />
      ) : null}

      <SessionBody
        agentAvatarUrl={agent.avatarUrl ?? null}
        agentName={agent.name}
        agentPubkey={agent.pubkey}
        connectionState={connectionState}
        autoTail={autoTail}
        channelId={channelId}
        emptyDescription={emptyDescription}
        emptyState={emptyState}
        errorMessage={errorMessage}
        events={displayEvents}
        hasObserver={hasObserver}
        hasTranscriptOverride={transcriptOverride != null}
        profiles={profiles}
        rawLayout={rawLayout}
        showRaw={showRaw}
        transcript={displayTranscript}
        transcriptContentClassName={transcriptContentClassName}
        transcriptVariant={transcriptVariant}
        unattributedFrames={unattributed?.droppedFrames ?? 0}
      />
    </section>
  );
}

function SessionHeader({
  connectionState,
  eventCount,
  hasObserver,
  latestSessionId,
}: {
  connectionState: ConnectionState;
  eventCount: number;
  hasObserver: boolean;
  latestSessionId: string | null | undefined;
}) {
  return (
    <div className="flex flex-col gap-3 sm:flex-row sm:items-start sm:justify-between">
      <div className="min-w-0">
        <div className="flex items-center gap-2">
          <h3 className="text-sm font-semibold tracking-tight">
            Live ACP session
          </h3>
          <ObserverStatusBadge state={connectionState} />
        </div>
        <p className="mt-1 text-sm text-muted-foreground">
          {hasObserver
            ? latestSessionId
              ? `Session ${shorten(latestSessionId)}`
              : "Waiting for the next agent turn."
            : "Restart this local agent to attach the observer feed."}
        </p>
      </div>
      <Badge className="w-fit font-mono" variant="outline">
        {eventCount} event{eventCount === 1 ? "" : "s"}
      </Badge>
    </div>
  );
}

function SessionBody({
  agentAvatarUrl,
  agentName,
  agentPubkey,
  autoTail,
  connectionState,
  channelId,
  emptyDescription,
  emptyState,
  errorMessage,
  events,
  hasObserver,
  hasTranscriptOverride,
  profiles,
  rawLayout,
  showRaw,
  transcript,
  transcriptContentClassName,
  transcriptVariant,
  unattributedFrames,
}: {
  agentAvatarUrl: string | null;
  agentName: string;
  agentPubkey: string;
  autoTail: boolean;
  channelId: string | null;
  connectionState: ConnectionState;
  emptyDescription: string;
  emptyState: AgentSessionTranscriptEmptyState;
  errorMessage: string | null;
  events: ObserverEvent[];
  hasObserver: boolean;
  hasTranscriptOverride: boolean;
  profiles?: UserProfileLookup;
  rawLayout: "responsive" | "exclusive";
  showRaw: boolean;
  transcript: TranscriptItem[];
  transcriptContentClassName?: string;
  transcriptVariant: AgentSessionTranscriptVariant;
  unattributedFrames: number;
}) {
  const rawRail = resolveRawRailLayout(showRaw, rawLayout);

  if (rawRail.mode === "exclusive") {
    return (
      <>
        <UnattributedAgentNotice droppedFrames={unattributedFrames} />
        <RawEventRail events={events} />

        {errorMessage ? (
          <p className="mt-4 inline-flex items-center gap-2 rounded-lg border border-destructive/30 bg-destructive/10 px-3 py-2 text-sm text-destructive">
            <CircleAlert className="h-4 w-4" />
            {errorMessage}
          </p>
        ) : null}
      </>
    );
  }

  return (
    <>
      <UnattributedAgentNotice droppedFrames={unattributedFrames} />

      {!hasObserver &&
      !hasTranscriptOverride &&
      transcript.length === 0 &&
      events.length === 0 ? (
        <EmptyObserverState />
      ) : connectionState === "connecting" &&
        events.length === 0 &&
        !hasTranscriptOverride ? (
        <SessionLoadingSkeleton />
      ) : (
        <div
          className={cn(
            rawRail.mode === "side"
              ? "mt-4 grid gap-4 xl:grid-cols-[minmax(0,1fr)_20rem]"
              : "mt-0",
            autoTail && "min-h-0 flex-1 overflow-hidden",
          )}
        >
          <AgentSessionTranscriptList
            agentAvatarUrl={agentAvatarUrl}
            agentName={agentName}
            agentPubkey={agentPubkey}
            channelId={channelId}
            emptyDescription={emptyDescription}
            emptyState={emptyState}
            items={transcript}
            profiles={profiles}
            contentContainerClassName={transcriptContentClassName}
            scrollScopeKey={`${agentPubkey}:${channelId ?? "all"}`}
            autoTail={autoTail}
            variant={transcriptVariant}
          />
          {rawRail.mode === "side" ? <RawEventRail events={events} /> : null}
        </div>
      )}

      {errorMessage ? (
        <p className="mt-4 inline-flex items-center gap-2 rounded-lg border border-destructive/30 bg-destructive/10 px-3 py-2 text-sm text-destructive">
          <CircleAlert className="h-4 w-4" />
          {errorMessage}
        </p>
      ) : null}
    </>
  );
}

function SessionLoadingSkeleton() {
  return (
    <div className="mx-auto w-full max-w-3xl space-y-6 py-4">
      <div className="flex justify-end">
        <div className="max-w-[70%] space-y-2">
          <Skeleton className="h-4 w-48 rounded-lg" />
        </div>
      </div>
      <div className="space-y-2">
        <div className="flex items-center gap-2">
          <Skeleton className="h-5 w-5 rounded-full" />
          <Skeleton className="h-3 w-16 rounded-full" />
        </div>
        <div className="space-y-2">
          <Skeleton className="h-4 w-full rounded-lg" />
          <Skeleton className="h-4 w-[86%] rounded-lg" />
          <Skeleton className="h-4 w-[58%] rounded-lg" />
        </div>
      </div>
      <div className="space-y-2">
        <Skeleton className="h-4 w-44 rounded-lg" />
        <Skeleton className="h-4 w-[68%] rounded-lg" />
      </div>
    </div>
  );
}

function ObserverStatusBadge({ state }: { state: ConnectionState }) {
  const display =
    state === "open"
      ? { label: "Live", Icon: CircleDot, variant: "default" as const }
      : state === "connecting"
        ? { label: "Connecting", variant: "secondary" as const }
        : state === "error"
          ? {
              label: "Unavailable",
              Icon: XCircle,
              variant: "destructive" as const,
            }
          : state === "closed"
            ? { label: "Closed", Icon: Clock3, variant: "secondary" as const }
            : { label: "Idle", Icon: Clock3, variant: "secondary" as const };
  const StatusIcon = display.Icon;

  return (
    <Badge className="gap-1.5" variant={display.variant}>
      {StatusIcon ? (
        <StatusIcon aria-hidden className="h-4 w-4" />
      ) : (
        <Spinner aria-hidden className="h-4 w-4 border-2" />
      )}
      {display.label}
    </Badge>
  );
}

/**
 * The observer feed for this agent is empty *because we are discarding it*.
 *
 * Silence is a rendered state, never an empty void — and this particular
 * silence is not idleness. The frames reached this app addressed to our own
 * identity and were refused before decryption because the agent is not in the
 * trusted set, which for an externally-supervised agent means its kind:0
 * carries no owner attestation naming this account.
 *
 * Rendered above the transcript rather than in place of it, and in every raw
 * rail layout, because the states this has to displace are not all empty
 * states: an agent the relay reports as `deployed` makes `hasObserver` true,
 * so the transcript's own "mention this agent to see its work" copy is what
 * the user is otherwise left reading.
 */
function UnattributedAgentNotice({ droppedFrames }: { droppedFrames: number }) {
  if (droppedFrames <= 0) {
    return null;
  }

  return (
    <p className="mt-4 flex items-start gap-2 rounded-lg border border-destructive/30 bg-destructive/10 px-3 py-2 text-sm text-destructive">
      <CircleAlert aria-hidden className="mt-0.5 h-4 w-4 shrink-0" />
      <span>
        This agent is publishing activity, but {droppedFrames} frame
        {droppedFrames === 1 ? "" : "s"} {droppedFrames === 1 ? "was" : "were"}{" "}
        discarded: it is not attributed to your account. Re-publish the
        agent&rsquo;s profile with its owner attestation to restore this feed.
      </span>
    </p>
  );
}

function EmptyObserverState() {
  return (
    <div className="mt-4 flex min-h-48 flex-col items-center justify-center px-6 py-8 text-center">
      <TerminalSquare className="mx-auto h-4 w-4 text-muted-foreground" />
      <p className="mt-3 text-sm font-medium">Observer not attached</p>
      <p className="mt-1 text-sm text-muted-foreground">
        The live feed is available for local agents started after this update.
      </p>
    </div>
  );
}
