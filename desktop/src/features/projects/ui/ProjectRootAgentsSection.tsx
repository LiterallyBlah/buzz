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
import { useProjectSeenAgents } from "@/features/projects/projectSeenAgents";
import { useProjectAgentActivity } from "@/features/projects/useProjectAgentActivity";
import { normalizePubkey } from "@/shared/lib/pubkey";
import { UserAvatar } from "@/shared/ui/UserAvatar";
import { OverviewRailSection } from "./ProjectOverviewPanel";

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
 * Answers a question the rest of the surface cannot: *who has worked on this*.
 * The conversation shows whoever wrote something, and the live indicator shows
 * whoever is typing right now, but an agent enrolled in the background by a
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
  profiles,
  rootEventId,
}: {
  commentAuthors?: readonly ProjectRootCommentAuthor[];
  profiles?: UserProfileLookup;
  rootEventId: string;
}) {
  // Its own subscription rather than a prop threaded down from the detail
  // view. The rail outlives the live indicator on several routes — the
  // pull-request "Files changed" and "Checks" tabs render the rail beside
  // content that never mounts the indicator — so a shared subscription owned
  // by the indicator would leave this section frozen exactly where a reader is
  // most likely to be waiting on a build. The extra REQ carries the same `#e`
  // filter on a root already on screen.
  const live = useProjectAgentActivity(rootEventId);
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

  if (agents.length === 0) return null;

  return (
    <OverviewRailSection title="Agents">
      <ul className="space-y-2" data-testid="project-root-agents">
        {agents.map((agent) => {
          const profile = profiles?.[agent.pubkey];
          const label = resolveUserLabel({ profiles, pubkey: agent.pubkey });
          return (
            <li className="flex min-w-0 items-center gap-2" key={agent.pubkey}>
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
            </li>
          );
        })}
      </ul>
    </OverviewRailSection>
  );
}
