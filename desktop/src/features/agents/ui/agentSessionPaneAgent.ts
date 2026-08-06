import type { ChannelAgentSessionAgent } from "@/features/channels/ui/useChannelAgentSessions";
import { normalizePubkey, truncatePubkey } from "@/shared/lib/pubkey";

/**
 * The agent descriptor an ACP activity pane needs, resolved from a bare pubkey.
 *
 * On a channel screen the pane is handed an agent that the channel already
 * knows about: `buildChannelAgentSessionCandidates` merges the managed-agent
 * roster, the relay-agent roster and the channel's bot members, and
 * `getChannelAgentSessionAgents` narrows that to the ones scoped to the open
 * channel. Every field the pane reads — name, status, interruptibility —
 * arrives already populated because a channel is a membership list and the
 * agent is on it.
 *
 * Off a channel there is no such list. A project root names its agents by
 * pubkey and nothing else: the announcement that reaches an issue view is a
 * kind 20003 turn event, whose only identity claim is a 32-byte key. Agents
 * hand-provisioned as relay members are absent from both rosters entirely, so
 * a lookup is not merely slower there — it legitimately returns nothing, and a
 * pane that refused to open on a miss would refuse for exactly the deployments
 * this path exists to serve.
 *
 * So the resolution is: use the roster entry when there is one, and otherwise
 * describe the agent from what the caller can actually see. The synthesized
 * descriptor is a statement about the viewer's knowledge, not a guess about
 * the agent — see the field notes below for what each default is really
 * asserting.
 */
export function resolveAgentSessionPaneAgent({
  candidates,
  fallbackName,
  pubkey,
}: {
  /**
   * Known agents, in `buildChannelAgentSessionCandidates` shape. Passing the
   * builder's output rather than raw rosters keeps one merge rule in the
   * codebase: whatever precedence managed entries take over relay entries on
   * a channel screen, they take here too.
   */
  candidates: readonly ChannelAgentSessionAgent[];
  /**
   * Display name from the caller's profile lookup — the same lookup that
   * rendered the name the viewer just clicked. Used only when the roster has
   * nothing better, so the pane header cannot disagree with the affordance
   * that opened it.
   */
  fallbackName?: string | null;
  pubkey: string;
}): ChannelAgentSessionAgent {
  const key = normalizePubkey(pubkey);
  const known = candidates.find(
    (candidate) => normalizePubkey(candidate.pubkey) === key,
  );

  if (known) {
    // A roster entry can still carry an empty name (a relay profile published
    // without one), and falling through to the pubkey in that case is better
    // than a header with a blank where the agent's name goes.
    return {
      ...known,
      name: known.name.trim() || fallbackName?.trim() || truncatePubkey(pubkey),
    };
  }

  return {
    pubkey,
    name: fallbackName?.trim() || truncatePubkey(pubkey),
    // "deployed", not "stopped". Status drives `isManagedAgentActive`, which
    // is what decides whether the pane opens a live observer subscription at
    // all — so calling an unknown agent stopped would close the feed on the
    // one agent whose feed the viewer just asked for. We have no process
    // information about an agent that is on no roster, and the honest default
    // for "unknown liveness" is the one that still listens: a subscription
    // that receives nothing costs a socket, whereas the absent subscription
    // costs the whole feature. This is also the value the roster path lands on
    // anyway — `relayStatusToManagedStatus` maps every non-offline relay
    // status to "deployed".
    status: "deployed",
    // "relay" is the accurate provenance for an agent reached this way: it is
    // published to the relay and observed there, just not enrolled in a roster
    // this client maintains. Claiming "managed" would additionally claim a
    // local process this client controls, which is the one thing we know is
    // false.
    agentSource: "relay",
    // Turn cancellation goes through the managed-agent control plane, which
    // only exists for agents this client started. An agent we cannot even find
    // on a roster is not one we can interrupt.
    canInterruptTurn: false,
  };
}

/**
 * Whether the pane's "Stop current turn" action can actually do anything, and
 * why not when it cannot.
 *
 * Three independent conditions have to hold, and the reason they are resolved
 * together is that the menu item is disabled for all three but a reader
 * deserves to know which one they are looking at. "Available while the agent
 * is working" beside a working agent is not a smaller inaccuracy than a button
 * that silently does nothing — it is the same failure wearing an explanation.
 *
 * The channel requirement is the one that is easy to miss.
 * `cancelManagedAgentTurn` addresses a turn by (agent, channel); a pane opened
 * from a project root has no channel to name, because an issue is not a
 * channel and any channel id supplied from there would be invented. So the
 * action is genuinely unavailable in agent-scoped panes rather than merely
 * unwired, and the copy says which channel-shaped thing is missing instead of
 * blaming the agent's provenance.
 */
export function resolveAgentSessionStopState({
  canInterruptTurn,
  hasChannel,
  isWorking,
}: {
  canInterruptTurn: boolean;
  hasChannel: boolean;
  isWorking: boolean;
}): { enabled: boolean; reason: string } {
  if (!isWorking) {
    return {
      enabled: false,
      reason: "Available while the agent is working.",
    };
  }
  if (!canInterruptTurn) {
    return {
      enabled: false,
      reason:
        "Only locally managed agents can be interrupted from this community.",
    };
  }
  if (!hasChannel) {
    return {
      enabled: false,
      reason:
        "Stopping a turn needs the channel it is running in. Open this agent from that channel to stop it.",
    };
  }
  return {
    enabled: true,
    reason:
      "Interrupt the current ACP turn without stopping the agent process.",
  };
}
