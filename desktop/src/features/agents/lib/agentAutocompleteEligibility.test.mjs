import assert from "node:assert/strict";
import test from "node:test";

import {
  coalesceAgentAutocompleteCandidates,
  filterAdmittedMentionPubkeys,
  filterCachedAgentSuggestions,
  getAgentMentionAdmission,
  getMentionableAgentPubkeys,
  getSharedChannelIds,
  isAgentDirectoryReady,
  isAgentIdentityInAllowedList,
  isAgentMentionChannelType,
  relayAgentCanRespondInChannel,
  relayAgentIsSharedWithUser,
  resolveAgentEligibilityScope,
  shouldHideAgentFromMentions,
  uniqueAutocompleteLabels,
} from "./agentAutocompleteEligibility.ts";

const CURRENT_PUBKEY = "a".repeat(64);
const OWNER_PUBKEY = "b".repeat(64);
const OTHER_OWNER_PUBKEY = "c".repeat(64);
const PUB_A = "1".repeat(64);
const PUB_B = "2".repeat(64);
const PUB_C = "3".repeat(64);
const PUB_D = "4".repeat(64);

function coalesce(candidates, options = {}) {
  return coalesceAgentAutocompleteCandidates(candidates, {
    currentPubkey: CURRENT_PUBKEY,
    getLabel: (candidate) => candidate.displayName,
    ...options,
  });
}

function makeAgent(overrides = {}) {
  return {
    pubkey: PUB_A,
    displayName: "Pinky",
    isAgent: true,
    isMember: false,
    ...overrides,
  };
}

test("isAgentDirectoryReady: requires successful cached directory evidence", () => {
  assert.equal(isAgentDirectoryReady({ data: [], error: null }), true);
  assert.equal(isAgentDirectoryReady({ data: undefined, error: null }), false);
  assert.equal(
    isAgentDirectoryReady({ data: [], error: new Error("offline") }),
    false,
  );
});

test("getSharedChannelIds: includes only active joined channels", () => {
  assert.deepEqual(
    getSharedChannelIds([
      { id: "joined", isMember: true, archivedAt: null },
      { id: "not-joined", isMember: false, archivedAt: null },
      { id: "archived", isMember: true, archivedAt: "2026-01-01T00:00:00Z" },
    ]),
    new Set(["joined"]),
  );
});

test("relayAgentIsSharedWithUser: accepts shared anyone agents and rejects unshared ones", () => {
  const sharedChannelIds = new Set(["general"]);

  assert.equal(
    relayAgentIsSharedWithUser(
      { respondTo: "anyone", respondToAllowlist: [], channelIds: ["general"] },
      sharedChannelIds,
    ),
    true,
  );
  assert.equal(
    relayAgentIsSharedWithUser(
      {
        ownerPubkey: OTHER_OWNER_PUBKEY,
        respondTo: "owner-only",
        respondToAllowlist: [],
        channelIds: ["general"],
      },
      sharedChannelIds,
      CURRENT_PUBKEY,
    ),
    false,
  );
  assert.equal(
    relayAgentIsSharedWithUser(
      { respondTo: "anyone", respondToAllowlist: [], channelIds: ["other"] },
      sharedChannelIds,
    ),
    false,
  );
});

test("relayAgentIsSharedWithUser: accepts verified same-owner agents across machines", () => {
  assert.equal(
    relayAgentIsSharedWithUser(
      {
        ownerPubkey: CURRENT_PUBKEY.toUpperCase(),
        respondTo: "owner-only",
        respondToAllowlist: [],
        channelIds: ["general"],
      },
      new Set(["general"]),
      CURRENT_PUBKEY,
    ),
    true,
  );
});

test("relayAgentIsSharedWithUser: owner-only fails closed without a verified viewer or owner", () => {
  const agent = {
    ownerPubkey: CURRENT_PUBKEY,
    respondTo: "owner-only",
    respondToAllowlist: [],
    channelIds: ["general"],
  };
  const sharedChannelIds = new Set(["general"]);

  assert.equal(
    relayAgentIsSharedWithUser(agent, sharedChannelIds, null),
    false,
  );
  assert.equal(relayAgentIsSharedWithUser(agent, sharedChannelIds), false);
  assert.equal(
    relayAgentIsSharedWithUser(
      { ...agent, ownerPubkey: null },
      sharedChannelIds,
      CURRENT_PUBKEY,
    ),
    false,
  );
});

test("relayAgentIsSharedWithUser: accepts allowlist agents for the current user", () => {
  const sharedChannelIds = new Set(["general"]);

  assert.equal(
    relayAgentIsSharedWithUser(
      {
        respondTo: "allowlist",
        respondToAllowlist: [OTHER_OWNER_PUBKEY, CURRENT_PUBKEY.toUpperCase()],
        channelIds: ["other"],
      },
      sharedChannelIds,
      CURRENT_PUBKEY,
    ),
    true,
  );
  assert.equal(
    relayAgentIsSharedWithUser(
      {
        respondTo: "allowlist",
        respondToAllowlist: [OTHER_OWNER_PUBKEY],
        channelIds: ["general"],
      },
      sharedChannelIds,
      CURRENT_PUBKEY,
    ),
    false,
  );
});

test("relayAgentCanRespondInChannel: requires exact channel membership and viewer access", () => {
  const agent = {
    respondTo: "allowlist",
    respondToAllowlist: [CURRENT_PUBKEY],
    channelIds: ["general"],
  };

  assert.equal(
    relayAgentCanRespondInChannel(agent, "general", CURRENT_PUBKEY),
    true,
  );
  assert.equal(
    relayAgentCanRespondInChannel(agent, "other", CURRENT_PUBKEY),
    false,
  );
  assert.equal(
    relayAgentCanRespondInChannel(agent, "general", OTHER_OWNER_PUBKEY),
    false,
  );
});

test("relayAgentCanRespondInChannel: channel membership stands in for a stale channelIds list", () => {
  // The reported bug: an agent added to a channel after it last published its
  // kind:10100 entry, so the channel is absent from its own `channelIds`.
  const allowlistAgent = {
    respondTo: "allowlist",
    respondToAllowlist: [CURRENT_PUBKEY],
    channelIds: ["some-other-channel"],
  };

  assert.equal(
    relayAgentCanRespondInChannel(
      allowlistAgent,
      "general",
      CURRENT_PUBKEY,
      true,
    ),
    true,
  );
  // Membership widens who we offer, never who the agent answers.
  assert.equal(
    relayAgentCanRespondInChannel(
      allowlistAgent,
      "general",
      OTHER_OWNER_PUBKEY,
      true,
    ),
    false,
  );
  // Non-member that does not list the channel is still ineligible.
  assert.equal(
    relayAgentCanRespondInChannel(
      allowlistAgent,
      "general",
      CURRENT_PUBKEY,
      false,
    ),
    false,
  );

  const anyoneAgent = {
    respondTo: "anyone",
    respondToAllowlist: [],
    channelIds: ["some-other-channel"],
  };
  assert.equal(
    relayAgentCanRespondInChannel(anyoneAgent, "general", CURRENT_PUBKEY, true),
    true,
  );
  assert.equal(
    relayAgentCanRespondInChannel(
      anyoneAgent,
      "general",
      CURRENT_PUBKEY,
      false,
    ),
    false,
  );
});

test("relayAgentCanRespondInChannel: owner-only agents answer exactly their verified owner", () => {
  const ownerOnlyAgent = {
    ownerPubkey: OWNER_PUBKEY,
    respondTo: "owner-only",
    respondToAllowlist: [],
    channelIds: ["general"],
  };

  // Matching owner in a channel the agent lists itself.
  assert.equal(
    relayAgentCanRespondInChannel(ownerOnlyAgent, "general", OWNER_PUBKEY),
    true,
  );
  // The owner comparison is normalized on both sides.
  assert.equal(
    relayAgentCanRespondInChannel(
      { ...ownerOnlyAgent, ownerPubkey: OWNER_PUBKEY.toUpperCase() },
      "general",
      OWNER_PUBKEY,
    ),
    true,
  );
  // Matching owner admitted through actual membership when the agent's own
  // channelIds snapshot predates the channel.
  assert.equal(
    relayAgentCanRespondInChannel(
      { ...ownerOnlyAgent, channelIds: ["some-other-channel"] },
      "general",
      OWNER_PUBKEY,
      true,
    ),
    true,
  );
  // Membership widens presence, never `respond_to`: a different viewer stays
  // rejected even when the agent is a member.
  assert.equal(
    relayAgentCanRespondInChannel(
      ownerOnlyAgent,
      "general",
      CURRENT_PUBKEY,
      true,
    ),
    false,
  );
  // An allowlist entry does not promote an owner-only agent either.
  assert.equal(
    relayAgentCanRespondInChannel(
      { ...ownerOnlyAgent, respondToAllowlist: [CURRENT_PUBKEY] },
      "general",
      CURRENT_PUBKEY,
      true,
    ),
    false,
  );
  // Fail closed without a verified viewer or a verified owner.
  assert.equal(
    relayAgentCanRespondInChannel(ownerOnlyAgent, "general", null, true),
    false,
  );
  assert.equal(
    relayAgentCanRespondInChannel(ownerOnlyAgent, "general", undefined, true),
    false,
  );
  assert.equal(
    relayAgentCanRespondInChannel(
      { ...ownerOnlyAgent, ownerPubkey: null },
      "general",
      OWNER_PUBKEY,
      true,
    ),
    false,
  );
  // Presence is still required: the owner cannot summon the agent into a
  // channel it neither lists nor belongs to.
  assert.equal(
    relayAgentCanRespondInChannel(
      { ...ownerOnlyAgent, channelIds: ["some-other-channel"] },
      "general",
      OWNER_PUBKEY,
      false,
    ),
    false,
  );
});

test("getMentionableAgentPubkeys: channel scope admits member agents with a stale channelIds list", () => {
  // PUB_B is the reported bug: a member whose directory entry predates the
  // channel. PUB_C is a member whose allowlist excludes us. PUB_D is neither
  // a member nor a self-declared participant.
  const relayAgents = [
    {
      pubkey: PUB_B,
      respondTo: "allowlist",
      respondToAllowlist: [CURRENT_PUBKEY],
      channelIds: ["some-other-channel"],
    },
    {
      pubkey: PUB_C,
      respondTo: "allowlist",
      respondToAllowlist: [OTHER_OWNER_PUBKEY],
      channelIds: ["some-other-channel"],
    },
    {
      pubkey: PUB_D,
      respondTo: "anyone",
      respondToAllowlist: [],
      channelIds: ["some-other-channel"],
    },
  ];
  const base = {
    currentPubkey: CURRENT_PUBKEY,
    managedAgentPubkeys: [PUB_A],
    relayAgents,
    sharedChannelIds: new Set(["general"]),
  };

  assert.deepEqual(
    getMentionableAgentPubkeys({
      ...base,
      eligibilityScope: {
        type: "channel",
        channelId: "general",
        // Mixed case: the set is normalized before it is consulted.
        memberPubkeys: new Set([PUB_B.toUpperCase(), PUB_C]),
      },
    }),
    new Set([PUB_A, PUB_B]),
  );

  // Without a member set, channel scope behaves exactly as it did before.
  assert.deepEqual(
    getMentionableAgentPubkeys({
      ...base,
      eligibilityScope: { type: "channel", channelId: "general" },
    }),
    new Set([PUB_A]),
  );

  // An "anyone" agent that is a member but does not list the channel.
  assert.deepEqual(
    getMentionableAgentPubkeys({
      ...base,
      eligibilityScope: {
        type: "channel",
        channelId: "general",
        memberPubkeys: new Set([PUB_D]),
      },
    }),
    new Set([PUB_A, PUB_D]),
  );

  // Membership in some other channel does not leak into this one.
  assert.deepEqual(
    getMentionableAgentPubkeys({
      ...base,
      eligibilityScope: {
        type: "channel",
        channelId: "unrelated",
        memberPubkeys: new Set(),
      },
    }),
    new Set([PUB_A]),
  );
});

test("getMentionableAgentPubkeys: channel scope admits owner-only agents for their verified owner", () => {
  const ownerOnly = (pubkey, overrides = {}) => ({
    pubkey,
    ownerPubkey: CURRENT_PUBKEY,
    respondTo: "owner-only",
    respondToAllowlist: [],
    channelIds: ["general"],
    ...overrides,
  });
  const base = {
    currentPubkey: CURRENT_PUBKEY,
    managedAgentPubkeys: [],
    sharedChannelIds: new Set(["general"]),
  };

  // PUB_A lists the channel; PUB_B is a member whose channelIds snapshot
  // predates the channel; PUB_C belongs to a different owner; PUB_D has no
  // verified owner at all.
  assert.deepEqual(
    getMentionableAgentPubkeys({
      ...base,
      relayAgents: [
        ownerOnly(PUB_A),
        ownerOnly(PUB_B, { channelIds: ["some-other-channel"] }),
        ownerOnly(PUB_C, { ownerPubkey: OTHER_OWNER_PUBKEY }),
        ownerOnly(PUB_D, { ownerPubkey: null }),
      ],
      eligibilityScope: {
        type: "channel",
        channelId: "general",
        memberPubkeys: new Set([PUB_B]),
      },
    }),
    new Set([PUB_A, PUB_B]),
  );

  // Without a verified current user every owner-only agent fails closed.
  assert.deepEqual(
    getMentionableAgentPubkeys({
      ...base,
      currentPubkey: null,
      relayAgents: [ownerOnly(PUB_A)],
      eligibilityScope: { type: "channel", channelId: "general" },
    }),
    new Set(),
  );
});

test("getMentionableAgentPubkeys: member pubkeys never widen community or managed-only scope", () => {
  const relayAgents = [
    {
      pubkey: PUB_B,
      respondTo: "anyone",
      respondToAllowlist: [],
      channelIds: ["some-other-channel"],
    },
  ];
  const base = {
    currentPubkey: CURRENT_PUBKEY,
    managedAgentPubkeys: [PUB_A],
    relayAgents,
    sharedChannelIds: new Set(["general"]),
  };

  assert.deepEqual(
    getMentionableAgentPubkeys({
      ...base,
      eligibilityScope: { type: "community" },
    }),
    new Set([PUB_A]),
  );
  assert.deepEqual(
    getMentionableAgentPubkeys({
      ...base,
      eligibilityScope: { type: "managed-only" },
    }),
    new Set([PUB_A]),
  );
});

test("getMentionableAgentPubkeys: keeps managed agents and shared relay agents", () => {
  const result = getMentionableAgentPubkeys({
    eligibilityScope: { type: "community" },
    managedAgentPubkeys: [PUB_A],
    currentPubkey: CURRENT_PUBKEY,
    relayAgents: [
      {
        pubkey: PUB_B,
        respondTo: "anyone",
        respondToAllowlist: [],
        channelIds: ["general"],
      },
      {
        pubkey: PUB_C,
        respondTo: "allowlist",
        respondToAllowlist: [CURRENT_PUBKEY],
        channelIds: ["other"],
      },
      {
        pubkey: PUB_D,
        respondTo: "anyone",
        respondToAllowlist: [],
        channelIds: ["other"],
      },
    ],
    sharedChannelIds: new Set(["general"]),
  });

  assert.deepEqual(result, new Set([PUB_A, PUB_B, PUB_C]));
});

test("getMentionableAgentPubkeys: scopes channel composers and fails closed without context", () => {
  const relayAgents = [
    {
      pubkey: PUB_B,
      respondTo: "allowlist",
      respondToAllowlist: [CURRENT_PUBKEY],
      channelIds: ["general"],
    },
  ];
  const base = {
    currentPubkey: CURRENT_PUBKEY,
    managedAgentPubkeys: [PUB_A],
    relayAgents,
    sharedChannelIds: new Set(["general"]),
  };

  assert.deepEqual(
    getMentionableAgentPubkeys({
      ...base,
      eligibilityScope: { type: "channel", channelId: "general" },
    }),
    new Set([PUB_A, PUB_B]),
  );
  assert.deepEqual(
    getMentionableAgentPubkeys({
      ...base,
      eligibilityScope: { type: "channel", channelId: "other" },
    }),
    new Set([PUB_A]),
  );
  assert.deepEqual(
    getMentionableAgentPubkeys({
      ...base,
      eligibilityScope: { type: "managed-only" },
    }),
    new Set([PUB_A]),
  );
});

test("resolveAgentEligibilityScope: project work surfaces explicitly use community eligibility", () => {
  assert.deepEqual(
    resolveAgentEligibilityScope({
      explicitScope: { type: "community" },
    }),
    { type: "community" },
  );
  assert.deepEqual(
    resolveAgentEligibilityScope({
      channelId: "buzz",
      channelType: "forum",
      explicitScope: { type: "community" },
    }),
    { type: "community" },
  );
});

test("resolveAgentEligibilityScope: ordinary channels retain exact channel eligibility", () => {
  assert.deepEqual(
    resolveAgentEligibilityScope({
      channelId: "general",
      channelType: "stream",
    }),
    { type: "channel", channelId: "general" },
  );
});

test("resolveAgentEligibilityScope: absent or unsupported context stays managed-only", () => {
  assert.deepEqual(resolveAgentEligibilityScope({}), { type: "managed-only" });
  assert.deepEqual(
    resolveAgentEligibilityScope({ channelId: "direct", channelType: "dm" }),
    { type: "managed-only" },
  );
});

test("autocomplete helper extraction preserves safe filtering and labels", () => {
  assert.equal(isAgentMentionChannelType("stream"), true);
  assert.equal(isAgentMentionChannelType("forum"), true);
  assert.equal(isAgentMentionChannelType("dm"), false);
  assert.equal(isAgentMentionChannelType(null), false);

  assert.deepEqual(
    uniqueAutocompleteLabels([
      { displayName: " Alice ", personaName: "alice" },
      { displayName: null, secondaryLabel: "Bob" },
      { displayName: "BOB" },
    ]),
    ["Alice", "Bob"],
  );

  const person = { pubkey: PUB_A, isAgent: false };
  const admittedAgent = { pubkey: PUB_B.toUpperCase(), isAgent: true };
  const removedAgent = { pubkey: PUB_C, isAgent: true };
  const persona = { isAgent: true };
  assert.deepEqual(
    filterCachedAgentSuggestions(
      [person, admittedAgent, removedAgent, persona],
      [{ pubkey: PUB_B, isAgent: true }],
    ),
    [person, admittedAgent, persona],
  );
});

test("isAgentIdentityInAllowedList: keeps people and only explicitly allowed agent identities", () => {
  const allowedAgentPubkeys = new Set([PUB_A]);

  assert.equal(
    isAgentIdentityInAllowedList(
      { isAgent: false, pubkey: PUB_B },
      allowedAgentPubkeys,
    ),
    true,
  );
  assert.equal(
    isAgentIdentityInAllowedList(
      { isAgent: true, pubkey: PUB_A.toUpperCase() },
      allowedAgentPubkeys,
    ),
    true,
  );
  assert.equal(
    isAgentIdentityInAllowedList(
      { isAgent: true, pubkey: PUB_B },
      allowedAgentPubkeys,
    ),
    false,
  );
});

test("isAgentIdentityInAllowedList: an invocable relay-resident agent passes without a managed entry", () => {
  // PUB_A is managed, PUB_B is relay-resident and invocable for this user;
  // getMentionableAgentPubkeys unions both into one allowed set.
  const allowedAgentPubkeys = new Set([PUB_A, PUB_B]);

  // Attributed + invocable, managed nowhere: the deployment this exists for.
  assert.equal(
    isAgentIdentityInAllowedList(
      { isAgent: true, pubkey: PUB_B },
      allowedAgentPubkeys,
    ),
    true,
  );
  // Case-insensitive like the managed check.
  assert.equal(
    isAgentIdentityInAllowedList(
      { isAgent: true, pubkey: PUB_B.toUpperCase() },
      allowedAgentPubkeys,
    ),
    true,
  );
  // A directory ghost with no invocability signal stays hidden.
  assert.equal(
    isAgentIdentityInAllowedList(
      { isAgent: true, pubkey: PUB_C },
      allowedAgentPubkeys,
    ),
    false,
  );
  // People are never gated, with or without the set.
  assert.equal(
    isAgentIdentityInAllowedList(
      { isAgent: false, pubkey: PUB_C },
      allowedAgentPubkeys,
    ),
    true,
  );
});

test("shouldHideAgentFromMentions: never hides non-agents", () => {
  assert.equal(
    shouldHideAgentFromMentions({
      isAgent: false,
      isMember: false,
      pubkey: PUB_A,
      mentionableAgentPubkeys: new Set(),
      directoryAgentPubkeys: new Set([PUB_A]),
    }),
    false,
  );
});

test("shouldHideAgentFromMentions: shows invocable agents even when non-member", () => {
  assert.equal(
    shouldHideAgentFromMentions({
      isAgent: true,
      isMember: false,
      pubkey: PUB_A,
      mentionableAgentPubkeys: new Set([PUB_A]),
      directoryAgentPubkeys: new Set([PUB_A]),
    }),
    false,
  );
});

test("shouldHideAgentFromMentions: hides non-member non-invocable agents", () => {
  assert.equal(
    shouldHideAgentFromMentions({
      isAgent: true,
      isMember: false,
      pubkey: PUB_A,
      mentionableAgentPubkeys: new Set(),
      directoryAgentPubkeys: new Set(),
    }),
    true,
  );
});

test("shouldHideAgentFromMentions: hides member agents with an explicit not-invocable directory entry (Fizz)", () => {
  assert.equal(
    shouldHideAgentFromMentions({
      isAgent: true,
      isMember: true,
      pubkey: PUB_A,
      mentionableAgentPubkeys: new Set(),
      directoryAgentPubkeys: new Set([PUB_A]),
    }),
    true,
  );
});

test("shouldHideAgentFromMentions: hides member agents without an affirmative directory grant", () => {
  assert.equal(
    shouldHideAgentFromMentions({
      isAgent: true,
      isMember: true,
      pubkey: PUB_A,
      mentionableAgentPubkeys: new Set(),
      directoryAgentPubkeys: new Set(),
    }),
    true,
  );
});

test("shouldHideAgentFromMentions: hides unknown member agents while directories load", () => {
  assert.equal(
    shouldHideAgentFromMentions({
      isAgent: true,
      isMember: true,
      pubkey: PUB_A,
      mentionableAgentPubkeys: new Set(),
      directoryAgentPubkeys: new Set(),
      directoryReady: false,
    }),
    true,
  );
});

test("shouldHideAgentFromMentions: hides mentionable member agents while directories load", () => {
  assert.equal(
    shouldHideAgentFromMentions({
      isAgent: true,
      isMember: true,
      pubkey: PUB_A,
      mentionableAgentPubkeys: new Set([PUB_A]),
      directoryAgentPubkeys: new Set(),
      directoryReady: false,
    }),
    true,
  );
});

test("shouldHideAgentFromMentions: shows non-agent members while directories load", () => {
  assert.equal(
    shouldHideAgentFromMentions({
      isAgent: false,
      isMember: true,
      pubkey: PUB_A,
      mentionableAgentPubkeys: new Set(),
      directoryAgentPubkeys: new Set([PUB_A]),
      directoryReady: false,
    }),
    false,
  );
});

test("shouldHideAgentFromMentions: hides unknown member agents after empty directories settle", () => {
  assert.equal(
    shouldHideAgentFromMentions({
      isAgent: true,
      isMember: true,
      pubkey: PUB_A,
      mentionableAgentPubkeys: new Set(),
      directoryAgentPubkeys: new Set(),
      directoryReady: true,
    }),
    true,
  );
});

test("shouldHideAgentFromMentions: shows authorized agents without managed-owner policy", () => {
  assert.equal(
    shouldHideAgentFromMentions({
      isAgent: true,
      pubkey: PUB_A,
      mentionableAgentPubkeys: new Set([PUB_A]),
      directoryReady: true,
    }),
    false,
  );
});

test("shouldHideAgentFromMentions: normalizes the pubkey before lookup", () => {
  const mixedCase = "Ab".repeat(32);
  const normalized = mixedCase.toLowerCase();

  assert.equal(
    shouldHideAgentFromMentions({
      isAgent: true,
      isMember: true,
      pubkey: mixedCase,
      mentionableAgentPubkeys: new Set(),
      directoryAgentPubkeys: new Set([normalized]),
    }),
    true,
  );
});

test("getAgentMentionAdmission: authorized relay agents are independent of owner", () => {
  const common = {
    isAgent: true,
    pubkey: PUB_A,
    mentionableAgentPubkeys: new Set([PUB_A]),
    directoryReady: true,
  };

  assert.equal(getAgentMentionAdmission(common), "allow");
  assert.equal(
    getAgentMentionAdmission({
      ...common,
      mentionableAgentPubkeys: new Set(),
    }),
    "deny",
  );
});

test("getAgentMentionAdmission: unresolved directory state stays unknown", () => {
  assert.equal(
    getAgentMentionAdmission({
      isAgent: true,
      pubkey: PUB_A,
      mentionableAgentPubkeys: new Set([PUB_A]),
      directoryReady: false,
    }),
    "unknown",
  );
});

test("filterAdmittedMentionPubkeys: rechecks agent admission without dropping people", () => {
  assert.deepEqual(
    filterAdmittedMentionPubkeys(
      [PUB_A, PUB_B, PUB_C],
      new Set([PUB_A, PUB_B]),
      new Set([PUB_B]),
    ),
    [PUB_B, PUB_C],
  );
});

test("coalesceAgentAutocompleteCandidates: keeps agents with the same persona id distinct", () => {
  const first = makeAgent({ pubkey: PUB_A, personaId: "pinky" });
  const second = makeAgent({
    pubkey: PUB_B,
    personaId: "pinky",
    isMember: true,
  });

  assert.deepEqual(coalesce([first, second]), [first, second]);
});

test("coalesceAgentAutocompleteCandidates: keeps agents with the same owner and name distinct", () => {
  const first = makeAgent({ pubkey: PUB_A, ownerPubkey: OWNER_PUBKEY });
  const second = makeAgent({
    pubkey: PUB_B,
    ownerPubkey: OWNER_PUBKEY,
    isMember: true,
  });

  assert.deepEqual(coalesce([first, second]), [first, second]);
});

test("coalesceAgentAutocompleteCandidates: keeps same-name agents with different owners distinct", () => {
  const first = makeAgent({ pubkey: PUB_A, ownerPubkey: OWNER_PUBKEY });
  const second = makeAgent({
    pubkey: PUB_B,
    ownerPubkey: OTHER_OWNER_PUBKEY,
  });

  assert.deepEqual(coalesce([first, second]), [first, second]);
});

test("coalesceAgentAutocompleteCandidates: keeps owner-less same-name agents distinct", () => {
  const first = makeAgent({ pubkey: PUB_A });
  const second = makeAgent({ pubkey: PUB_B });

  assert.deepEqual(coalesce([first, second]), [first, second]);
});

test("coalesceAgentAutocompleteCandidates: keeps owner-less managed same-name agents distinct", () => {
  const first = makeAgent({ pubkey: PUB_A, isManagedAgent: true });
  const second = makeAgent({ pubkey: PUB_B, isManagedAgent: true });

  assert.deepEqual(coalesce([first, second]), [first, second]);
});

test("coalesceAgentAutocompleteCandidates: keeps current-owner same-name agents distinct", () => {
  const first = makeAgent({ pubkey: PUB_A, ownerPubkey: CURRENT_PUBKEY });
  const second = makeAgent({
    pubkey: PUB_B,
    ownerPubkey: CURRENT_PUBKEY,
    isManagedAgent: true,
  });

  assert.deepEqual(coalesce([first, second]), [first, second]);
});

test("coalesceAgentAutocompleteCandidates: coalesces repeated source rows for the same pubkey", () => {
  const first = makeAgent({ pubkey: PUB_A });
  const second = makeAgent({
    pubkey: PUB_A.toUpperCase(),
    isMember: true,
  });

  assert.deepEqual(coalesce([first, second]), [second]);
});

test("coalesceAgentAutocompleteCandidates: leaves non-agents alone", () => {
  const first = makeAgent({ pubkey: PUB_A, isAgent: false });
  const second = makeAgent({ pubkey: PUB_B, isAgent: false });

  assert.deepEqual(coalesce([first, second]), [first, second]);
});
