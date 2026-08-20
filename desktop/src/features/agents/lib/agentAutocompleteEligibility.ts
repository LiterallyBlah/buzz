import type { Channel, ChannelType, RelayAgent } from "@/shared/api/types";
import { normalizePubkey } from "@/shared/lib/pubkey";

export function getSharedChannelIds(channels: readonly Channel[] | undefined) {
  return new Set(
    (channels ?? [])
      .filter((channel) => channel.isMember && channel.archivedAt === null)
      .map((channel) => channel.id),
  );
}

// The `respond_to` half of eligibility: will this agent answer *us*, ignoring
// which channel we are in. Sole producer of the whole respond_to rule — all
// three modes — so the channel and community paths cannot drift apart on who
// an agent talks to.
function relayAgentRespondsToUser(
  agent: Pick<RelayAgent, "ownerPubkey" | "respondTo" | "respondToAllowlist">,
  currentPubkey?: string | null,
) {
  if (agent.respondTo === "owner-only") {
    // Only a verified same-owner pair is admitted: an unverified viewer or an
    // unattributed agent fails closed.
    const normalizedCurrentPubkey = currentPubkey
      ? normalizePubkey(currentPubkey)
      : null;
    return (
      normalizedCurrentPubkey !== null &&
      !!agent.ownerPubkey &&
      normalizePubkey(agent.ownerPubkey) === normalizedCurrentPubkey
    );
  }

  if (agent.respondTo === "allowlist") {
    const normalizedCurrentPubkey = currentPubkey
      ? normalizePubkey(currentPubkey)
      : null;
    return (
      normalizedCurrentPubkey !== null &&
      agent.respondToAllowlist
        .map((pubkey) => normalizePubkey(pubkey))
        .includes(normalizedCurrentPubkey)
    );
  }

  return agent.respondTo === "anyone";
}

export function relayAgentIsSharedWithUser(
  agent: Pick<
    RelayAgent,
    "channelIds" | "ownerPubkey" | "respondTo" | "respondToAllowlist"
  >,
  sharedChannelIds: ReadonlySet<string>,
  currentPubkey?: string | null,
) {
  // The respond_to rule is produced once, in `relayAgentRespondsToUser`, so
  // the community and channel paths cannot drift on who an agent talks to. An
  // owner-only agent is shared exactly with its verified owner, an allowlist
  // agent with its listed pubkeys — neither needs a shared channel. Only an
  // "anyone" agent is scoped to the channels it actually shares with us.
  if (agent.respondTo === "owner-only" || agent.respondTo === "allowlist") {
    return relayAgentRespondsToUser(agent, currentPubkey);
  }

  return (
    agent.respondTo === "anyone" &&
    agent.channelIds.some((channelId) => sharedChannelIds.has(channelId))
  );
}

export function relayAgentCanRespondInChannel(
  agent: Pick<
    RelayAgent,
    "channelIds" | "ownerPubkey" | "respondTo" | "respondToAllowlist"
  >,
  channelId: string,
  currentPubkey?: string | null,
  // Real channel membership, from the channel's member list.
  isChannelMember = false,
) {
  // The channel half. An agent's `channelIds` comes from its own kind:10100
  // directory entry, which is a snapshot taken when the agent last published:
  // nothing republishes it when the agent is later added to a channel. So an
  // agent that *is* a member of this channel can be absent from its own list.
  // Actual membership therefore stands in for the self-declared list.
  const isPresentInChannel =
    isChannelMember || agent.channelIds.includes(channelId);

  // The `respond_to` half is unchanged: membership widens who Desktop will
  // offer, never who an agent will answer. An agent whose allowlist excludes
  // the current user stays hidden even when it is a member, and an owner-only
  // agent stays hidden from everyone but its verified owner.
  return isPresentInChannel && relayAgentRespondsToUser(agent, currentPubkey);
}

export type AgentEligibilityScope =
  | { type: "community" }
  | {
      type: "channel";
      channelId: string;
      /** Pubkeys of the channel's current members, in any case/encoding. */
      memberPubkeys?: ReadonlySet<string>;
    }
  | { type: "managed-only" };

export const COMMUNITY_AGENT_ELIGIBILITY_SCOPE = {
  type: "community",
} as const satisfies AgentEligibilityScope;

export function resolveAgentEligibilityScope({
  channelId,
  channelType,
  explicitScope,
}: {
  channelId?: string | null;
  channelType?: ChannelType | null;
  explicitScope?: AgentEligibilityScope;
}): AgentEligibilityScope {
  if (explicitScope) return explicitScope;
  return channelId && isAgentMentionChannelType(channelType)
    ? { type: "channel", channelId }
    : { type: "managed-only" };
}

export function getMentionableAgentPubkeys({
  currentPubkey,
  eligibilityScope,
  managedAgentPubkeys,
  relayAgents,
  sharedChannelIds,
}: {
  currentPubkey?: string | null;
  eligibilityScope: AgentEligibilityScope;
  managedAgentPubkeys: Iterable<string>;
  relayAgents: readonly RelayAgent[] | undefined;
  sharedChannelIds: ReadonlySet<string>;
}) {
  const pubkeys = new Set(
    [...managedAgentPubkeys].map((pubkey) => normalizePubkey(pubkey)),
  );

  const channelMemberPubkeys =
    eligibilityScope.type === "channel" && eligibilityScope.memberPubkeys
      ? new Set(
          [...eligibilityScope.memberPubkeys].map((pubkey) =>
            normalizePubkey(pubkey),
          ),
        )
      : null;

  for (const agent of relayAgents ?? []) {
    const isAllowed =
      eligibilityScope.type === "managed-only"
        ? false
        : eligibilityScope.type === "community"
          ? relayAgentIsSharedWithUser(agent, sharedChannelIds, currentPubkey)
          : relayAgentCanRespondInChannel(
              agent,
              eligibilityScope.channelId,
              currentPubkey,
              channelMemberPubkeys?.has(normalizePubkey(agent.pubkey)) === true,
            );
    if (isAllowed) {
      pubkeys.add(normalizePubkey(agent.pubkey));
    }
  }

  return pubkeys;
}

export function isAgentIdentityInAllowedList(
  candidate: { isAgent?: boolean; pubkey: string },
  allowedAgentPubkeys: ReadonlySet<string>,
) {
  if (candidate.isAgent !== true) {
    return true;
  }
  const normalized = normalizePubkey(candidate.pubkey);
  // Managed is not the only way an agent identity is real. A relay-resident
  // agent (provisioned outside this desktop, attributed via a NIP-OA owner
  // tag) is on no managed list anywhere, and requiring one would hide every
  // such agent the moment its attribution becomes verifiable — while an
  // unattributed directory ghost should stay hidden. So: managed, or
  // explicitly invocable for this user (`getMentionableAgentPubkeys`).
  return allowedAgentPubkeys.has(normalized);
}

export type AgentMentionAdmission = "allow" | "deny" | "unknown";

export function getAgentMentionAdmission({
  isAgent,
  isManagedAgent,
  pubkey,
  ownerPubkey,
  currentPubkey,
  mentionableAgentPubkeys,
  directoryReady,
  ownerOnly,
}: {
  isAgent: boolean;
  isManagedAgent: boolean;
  pubkey: string;
  ownerPubkey?: string | null;
  currentPubkey?: string | null;
  mentionableAgentPubkeys: ReadonlySet<string>;
  directoryReady: boolean;
  ownerOnly: boolean | undefined;
}): AgentMentionAdmission {
  if (!isAgent) return "allow";
  if (!directoryReady || ownerOnly === undefined) return "unknown";

  const normalized = normalizePubkey(pubkey);
  if (!mentionableAgentPubkeys.has(normalized)) return "deny";
  if (!ownerOnly || isManagedAgent) return "allow";
  if (!ownerPubkey || !currentPubkey) return "unknown";

  return normalizePubkey(ownerPubkey) === normalizePubkey(currentPubkey)
    ? "allow"
    : "deny";
}

export function shouldHideAgentFromMentions({
  isAgent,
  isManagedAgent = false,
  pubkey,
  ownerPubkey,
  currentPubkey,
  mentionableAgentPubkeys,
  directoryReady = true,
  ownerOnly,
}: {
  isAgent: boolean;
  isManagedAgent?: boolean;
  pubkey: string;
  ownerPubkey?: string | null;
  currentPubkey?: string | null;
  mentionableAgentPubkeys: ReadonlySet<string>;
  directoryReady?: boolean;
  ownerOnly: boolean | undefined;
}) {
  return (
    getAgentMentionAdmission({
      isAgent,
      isManagedAgent,
      pubkey,
      ownerPubkey,
      currentPubkey,
      mentionableAgentPubkeys,
      directoryReady,
      ownerOnly,
    }) !== "allow"
  );
}

export function getAgentIdentityPubkeys({
  managedAgentPubkeys,
  relayAgents,
  members,
  profileIsAgent,
}: {
  managedAgentPubkeys: ReadonlySet<string>;
  relayAgents: readonly { pubkey: string }[];
  members: readonly {
    pubkey: string;
    isAgent?: boolean;
    role?: string | null;
  }[];
  profileIsAgent: (pubkey: string) => boolean;
}) {
  return new Set([
    ...managedAgentPubkeys,
    ...relayAgents.map(({ pubkey }) => normalizePubkey(pubkey)),
    ...members
      .filter(
        (member) =>
          member.isAgent === true ||
          member.role === "bot" ||
          profileIsAgent(normalizePubkey(member.pubkey)),
      )
      .map(({ pubkey }) => normalizePubkey(pubkey)),
  ]);
}

export function getAdmittedAgentPubkeys(
  candidates: readonly { pubkey?: string; isAgent?: boolean }[],
) {
  return new Set(
    candidates.flatMap((candidate) =>
      candidate.isAgent && candidate.pubkey
        ? [normalizePubkey(candidate.pubkey)]
        : [],
    ),
  );
}

export function rememberSelectedAgentPubkeys(
  target: Set<string>,
  selected: readonly { pubkey?: string; isAgent?: boolean }[],
  selectionIsAgent: boolean,
) {
  for (const candidate of selected) {
    if (candidate.pubkey && (selectionIsAgent || candidate.isAgent === true)) {
      target.add(normalizePubkey(candidate.pubkey));
    }
  }
}

export function filterAdmittedMentionPubkeys(
  pubkeys: readonly string[],
  agentIdentityPubkeys: ReadonlySet<string>,
  admittedAgentPubkeys: ReadonlySet<string>,
) {
  return pubkeys.filter((pubkey) => {
    const normalized = normalizePubkey(pubkey);
    return (
      !agentIdentityPubkeys.has(normalized) ||
      admittedAgentPubkeys.has(normalized)
    );
  });
}

export function isAgentMentionChannelType(type?: string | null) {
  return type === "stream" || type === "forum";
}

export function uniqueAutocompleteLabels(
  candidates: readonly AgentAutocompleteCandidate[],
) {
  const unique = new Map<string, string>();
  for (const candidate of candidates) {
    for (const label of [
      candidate.displayName,
      candidate.personaName,
      candidate.secondaryLabel,
    ]) {
      const trimmed = label?.trim();
      if (trimmed && !unique.has(trimmed.toLowerCase())) {
        unique.set(trimmed.toLowerCase(), trimmed);
      }
    }
  }
  return [...unique.values()];
}

export function filterCachedAgentSuggestions<
  T extends {
    isAgent?: boolean;
    pubkey?: string;
  },
>(
  suggestions: readonly T[],
  currentCandidates: readonly AgentAutocompleteCandidate[],
) {
  const admittedAgentPubkeys = new Set(
    currentCandidates.flatMap((candidate) =>
      candidate.isAgent && candidate.pubkey
        ? [normalizePubkey(candidate.pubkey)]
        : [],
    ),
  );
  return suggestions.filter(
    (suggestion) =>
      !suggestion.isAgent ||
      !suggestion.pubkey ||
      admittedAgentPubkeys.has(normalizePubkey(suggestion.pubkey)),
  );
}

type AgentAutocompleteCandidate = {
  pubkey?: string;
  displayName?: string | null;
  personaName?: string | null;
  secondaryLabel?: string | null;
  ownerPubkey?: string | null;
  isAgent?: boolean;
  isManagedAgent?: boolean;
  isMember?: boolean;
  personaId?: string | null;
};

function agentIdentityKey<T extends AgentAutocompleteCandidate>(candidate: T) {
  if (candidate.isAgent !== true || !candidate.pubkey) {
    return null;
  }

  // Pubkeys—not persona metadata or a display name—are agent identities.
  // A persona may be installed more than once, and an owner may intentionally
  // create multiple same-named agents. Collapsing either case makes one agent
  // impossible to choose from autocomplete.
  return `pubkey:${normalizePubkey(candidate.pubkey)}`;
}

function agentCandidateRank<T extends AgentAutocompleteCandidate>(
  candidate: T,
  preferredPubkeys: ReadonlySet<string>,
) {
  const pubkey = candidate.pubkey ? normalizePubkey(candidate.pubkey) : null;

  return [
    candidate.isMember === true ? 0 : 1,
    pubkey && preferredPubkeys.has(pubkey) ? 0 : 1,
    candidate.isManagedAgent === true ? 0 : 1,
    candidate.personaId ? 0 : 1,
  ];
}

function isPreferredAgentCandidate<T extends AgentAutocompleteCandidate>(
  next: T,
  current: T,
  preferredPubkeys: ReadonlySet<string>,
) {
  const nextRank = agentCandidateRank(next, preferredPubkeys);
  const currentRank = agentCandidateRank(current, preferredPubkeys);

  for (let index = 0; index < nextRank.length; index++) {
    if (nextRank[index] !== currentRank[index]) {
      return nextRank[index] < currentRank[index];
    }
  }

  return false;
}

export function coalesceAutocompleteCandidatesByKey<T>(
  candidates: readonly T[],
  getKey: (candidate: T) => string | null,
) {
  const output: T[] = [];
  const indexesByKey = new Map<string, number>();

  for (const candidate of candidates) {
    const key = getKey(candidate);
    if (!key) {
      output.push(candidate);
      continue;
    }

    if (!indexesByKey.has(key)) {
      indexesByKey.set(key, output.length);
      output.push(candidate);
    }
  }

  return output;
}

export function coalesceAgentAutocompleteCandidates<
  T extends AgentAutocompleteCandidate,
>(
  candidates: readonly T[],
  {
    currentPubkey: _currentPubkey,
    getLabel: _getLabel,
    preferredPubkeys = new Set(),
  }: {
    currentPubkey?: string | null;
    getLabel: (candidate: T) => string | null | undefined;
    preferredPubkeys?: ReadonlySet<string>;
  },
) {
  const output: T[] = [];
  const indexesByKey = new Map<string, number>();

  for (const candidate of candidates) {
    const key = agentIdentityKey(candidate);
    if (!key) {
      output.push(candidate);
      continue;
    }

    const currentIndex = indexesByKey.get(key);
    if (currentIndex === undefined) {
      indexesByKey.set(key, output.length);
      output.push(candidate);
      continue;
    }

    if (
      isPreferredAgentCandidate(
        candidate,
        output[currentIndex],
        preferredPubkeys,
      )
    ) {
      output[currentIndex] = candidate;
    }
  }

  return output;
}
