import * as React from "react";

import { useKnownAgentPubkeys } from "@/features/agents/useKnownAgentPubkeys";
import { useOpenAgentActivity } from "@/features/agents/useOpenAgentActivity";
import {
  resolveUserLabel,
  type UserProfileLookup,
} from "@/features/profile/lib/identity";
import {
  buildProjectRootAgents,
  type ProjectRootAgent,
  type ProjectRootCommentAuthor,
} from "@/features/projects/projectRootAgents";
import type { ProjectAgentActivity } from "@/features/projects/projectAgentActivity";
import { useProjectSeenAgents } from "@/features/projects/projectSeenAgents";
import { useProjectAgentActivity } from "@/features/projects/useProjectAgentActivity";
import { normalizePubkey } from "@/shared/lib/pubkey";
import { UserAvatar } from "@/shared/ui/UserAvatar";
import { OverviewRailSection } from "./ProjectOverviewPanel";

/**
 * What a working agent is doing, under the name it belongs to.
 *
 * The stage is the only thing the removed inline strip said that this section
 * did not, and it is the half worth keeping: "working" answers a question a
 * reader did not have to ask, "reading files" answers the one they did. It
 * hangs off its own agent's row rather than sitting under the list as a
 * sentence, because with two agents live a single caption is a statement about
 * both that is wrong about one.
 *
 * Rendered only for `working`. A caption beside `queued` would describe a turn
 * that has not started.
 */
function AgentStageCaption({ stage }: { stage: string | null }) {
  if (!stage) return null;
  return (
    <p
      className="truncate pl-7 text-2xs text-muted-foreground"
      data-testid="project-root-agent-stage"
      title={stage}
    >
      {stage}
    </p>
  );
}

/** The badge for an agent that is live right now, or nothing for one that is
 * only remembered. "Remembered" gets no badge on purpose: a grey "was here"
 * chip on every row would turn the common case into visual noise, and the
 * absence of a badge already says it. */
function AgentStateBadge({ state }: { state: ProjectRootAgent["state"] }) {
  if (state === null) return null;
  return (
    <span
      className={
        state === "working"
          ? "shrink-0 rounded-full border border-green-500/40 px-1.5 py-0.5 text-2xs font-medium text-green-500"
          : "shrink-0 rounded-full border border-border/60 px-1.5 py-0.5 text-2xs font-medium text-muted-foreground"
      }
    >
      {state}
    </span>
  );
}

/**
 * "Agents" for one issue or pull request, in the detail rail.
 *
 * Since the inline activity strip was removed this is the *only* live agent
 * surface on a work item, which is the point: two places reporting one fact
 * drift, and the reader was being made to choose which end of a long page to
 * stand at to watch them.
 *
 * Answers a question the rest of the surface cannot: *who has worked on this*.
 * The conversation shows whoever wrote something, and the state badge shows
 * whoever is running right now, but an agent enrolled in the background by a
 * peer call — hermes handing an issue to Claude — appears in neither. It
 * announces NIP-PA activity for the length of its turn, leaves no comment, and
 * then, because kind 20003 is ephemeral, becomes unfindable. This section is
 * where that agent stays visible, by unioning the ephemeral signal with a
 * local record of having seen it (`projectSeenAgents`).
 *
 * Renders nothing when the union is empty, matching the rail's existing
 * convention — `Labels` and `Reviewers` are absent rather than empty, and a
 * permanent "No agents" row would be furniture reporting the normal case.
 *
 * Each name opens that agent's ACP activity through `useOpenAgentActivity`,
 * the same ingress the live indicator and the channel-side working rows use.
 * No channel id is passed: a project root is not a channel, so any id supplied
 * from here would be invented, and the ingress already resolves the agent's
 * own scope and says so plainly when there is nowhere reachable.
 */
export function ProjectRootAgentsSection({
  commentAuthors,
  live: providedLive,
  profiles,
  rootEventId,
}: {
  commentAuthors?: readonly ProjectRootCommentAuthor[];
  /**
   * Activity from a caller that already has it, instead of a second REQ.
   *
   * Offered rather than required, and this is the whole reason the section
   * subscribes at all: the rail outlives any one live surface on several
   * routes — the pull-request "Files changed" and "Checks" tabs render it
   * beside content that has no activity subscription of its own — so a
   * required prop would leave the section frozen exactly where a reader is
   * most likely to be waiting on a build. The issue detail passes its own
   * because it needs the same frames to drive the jump-to-latest pill, and
   * two REQs with an identical `#e` filter on one root is a duplicate rather
   * than a fallback.
   */
  live?: readonly ProjectAgentActivity[];
  profiles?: UserProfileLookup;
  rootEventId: string;
}) {
  // `null` is the hook's own "open nothing", so a caller-supplied feed costs
  // no subscription here rather than opening one and discarding it.
  const ownLive = useProjectAgentActivity(providedLive ? null : rootEventId);
  const live = providedLive ?? ownLive;
  const seen = useProjectSeenAgents(rootEventId);
  const knownAgents = useKnownAgentPubkeys();
  const { openAgentActivity } = useOpenAgentActivity();

  const agents = React.useMemo(
    () =>
      buildProjectRootAgents({
        commentAuthors,
        // The shared baseline, widened by the profile's own `isAgent` flag in
        // the documented additive direction: a commenting agent that is
        // neither locally managed nor relay-registered still has a profile
        // that says what it is, and the alternative is filing it under a
        // heading it does not belong to.
        isKnownAgent: (pubkey) =>
          knownAgents.has(pubkey) ||
          profiles?.[normalizePubkey(pubkey)]?.isAgent === true,
        live,
        seen,
      }),
    [commentAuthors, knownAgents, live, profiles, seen],
  );

  // The stage is read here rather than carried on `ProjectRootAgent`: it is
  // the one field on the live entry that is purely presentational, it has no
  // meaning for a remembered agent, and threading it through the merge would
  // give every rule in `buildProjectRootAgents` a value to decide about.
  const stageFor = React.useCallback(
    (pubkey: string) =>
      live.find(
        (entry) =>
          entry.state === "working" && normalizePubkey(entry.agent) === pubkey,
      )?.stage ?? null,
    [live],
  );

  if (agents.length === 0) return null;

  return (
    <OverviewRailSection title="Agents">
      <ul className="space-y-2" data-testid="project-root-agents">
        {agents.map((agent) => {
          const profile = profiles?.[agent.pubkey];
          const label = resolveUserLabel({ profiles, pubkey: agent.pubkey });
          return (
            <li className="min-w-0 space-y-1" key={agent.pubkey}>
              <div className="flex min-w-0 items-center gap-2">
                <UserAvatar
                  accent
                  avatarUrl={profile?.avatarUrl ?? null}
                  displayName={label}
                  size="xs"
                />
                <button
                  className="min-w-0 flex-1 truncate rounded-sm text-left text-xs font-medium text-foreground hover:underline focus-visible:outline-hidden focus-visible:ring-1 focus-visible:ring-ring"
                  data-testid={`project-root-agent-${agent.pubkey}`}
                  onClick={() => openAgentActivity(agent.pubkey)}
                  title={`View ${label}'s activity`}
                  type="button"
                >
                  {label}
                </button>
                <AgentStateBadge state={agent.state} />
              </div>
              <AgentStageCaption stage={stageFor(agent.pubkey)} />
            </li>
          );
        })}
      </ul>
    </OverviewRailSection>
  );
}
