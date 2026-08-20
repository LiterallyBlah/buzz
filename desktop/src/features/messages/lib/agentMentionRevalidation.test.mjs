import assert from "node:assert/strict";
import test from "node:test";

import { getMentionableAgentPubkeys } from "../../agents/lib/agentAutocompleteEligibility.ts";
import { revalidateAgentMentionPubkeys } from "./agentMentionRevalidation.ts";

const CURRENT = "a".repeat(64);
const AGENT = "b".repeat(64);
const HUMAN = "c".repeat(64);
const OTHER_OWNER = "d".repeat(64);
const LOCAL_AGENT = "e".repeat(64);
const FOREIGN_AGENT = "f".repeat(64);

function options(refetchOwnerProfiles) {
  return {
    pubkeys: [HUMAN, AGENT],
    agentPubkeys: new Set([AGENT]),
    currentPubkey: CURRENT,
    eligibilityScope: { type: "channel", channelId: "general" },
    sharedChannelIds: new Set(["general"]),
    ownerOnly: true,
    ownerPolicyError: null,
    refetchManagedAgents: async () => ({ data: [], error: null }),
    fetchRelayAgents: async () => [
      {
        pubkey: AGENT,
        respondTo: "anyone",
        respondToAllowlist: [],
        channelIds: ["general"],
      },
    ],
    refetchOwnerProfiles,
  };
}

test("owner-only revalidation admits an agent only from a fresh same-owner proof", async () => {
  const requested = [];
  const result = await revalidateAgentMentionPubkeys(
    options(async (pubkeys) => {
      requested.push(...pubkeys);
      return {
        profiles: { [AGENT]: { ownerPubkey: CURRENT } },
        missing: [],
      };
    }),
  );

  assert.deepEqual(requested, [AGENT]);
  assert.deepEqual(result, [HUMAN, AGENT]);
});

test("fresh managed evidence survives unrelated relay authorization errors", async () => {
  const result = await revalidateAgentMentionPubkeys({
    ...options(async () => {
      throw new Error("owner profiles unavailable");
    }),
    pubkeys: [HUMAN, LOCAL_AGENT],
    agentPubkeys: new Set([LOCAL_AGENT]),
    refetchManagedAgents: async () => ({
      data: [{ pubkey: LOCAL_AGENT }],
      error: null,
    }),
    fetchRelayAgents: async () => {
      throw new Error("relay directory unavailable");
    },
  });

  assert.deepEqual(result, [HUMAN, LOCAL_AGENT]);
});

test("relay-only agents still fail closed when relay discovery fails", async () => {
  const result = await revalidateAgentMentionPubkeys({
    ...options(async () => ({
      profiles: { [AGENT]: { ownerPubkey: CURRENT } },
      missing: [],
    })),
    fetchRelayAgents: async () => {
      throw new Error("relay directory unavailable");
    },
  });

  assert.deepEqual(result, [HUMAN]);
});

test("mixed evidence preserves only fresh managed agents and humans", async () => {
  const result = await revalidateAgentMentionPubkeys({
    ...options(async () => ({
      profiles: { [AGENT]: { ownerPubkey: CURRENT } },
      missing: [LOCAL_AGENT],
    })),
    pubkeys: [HUMAN, LOCAL_AGENT, AGENT],
    agentPubkeys: new Set([LOCAL_AGENT, AGENT]),
    refetchManagedAgents: async () => ({
      data: [{ pubkey: LOCAL_AGENT }],
      error: null,
    }),
    fetchRelayAgents: async () => {
      throw new Error("relay directory unavailable");
    },
  });

  assert.deepEqual(result, [HUMAN, LOCAL_AGENT]);
});

test("send-time revalidation admits exactly the owner-only agents autocomplete offered", async () => {
  // AGENT is the owner's own owner-only agent, a channel member whose
  // channelIds snapshot predates the channel; FOREIGN_AGENT is someone
  // else's owner-only agent that lists the channel.
  const ownAgent = {
    pubkey: AGENT,
    ownerPubkey: CURRENT,
    respondTo: "owner-only",
    respondToAllowlist: [],
    channelIds: ["some-other-channel"],
  };
  const foreignAgent = {
    pubkey: FOREIGN_AGENT,
    ownerPubkey: OTHER_OWNER,
    respondTo: "owner-only",
    respondToAllowlist: [],
    channelIds: ["general"],
  };
  // The hook passes the composer's scope object through unchanged, so
  // autocomplete and send-time admission consult the same member set.
  const eligibilityScope = {
    type: "channel",
    channelId: "general",
    memberPubkeys: new Set([AGENT]),
  };
  const sharedChannelIds = new Set(["general"]);

  const offered = getMentionableAgentPubkeys({
    currentPubkey: CURRENT,
    eligibilityScope,
    managedAgentPubkeys: [],
    relayAgents: [ownAgent, foreignAgent],
    sharedChannelIds,
  });
  assert.deepEqual(offered, new Set([AGENT]));

  const result = await revalidateAgentMentionPubkeys({
    pubkeys: [HUMAN, AGENT, FOREIGN_AGENT],
    agentPubkeys: new Set([AGENT, FOREIGN_AGENT]),
    currentPubkey: CURRENT,
    eligibilityScope,
    sharedChannelIds,
    ownerOnly: true,
    ownerPolicyError: null,
    refetchManagedAgents: async () => ({ data: [], error: null }),
    fetchRelayAgents: async () => [ownAgent, foreignAgent],
    refetchOwnerProfiles: async () => ({
      profiles: {
        [AGENT]: { ownerPubkey: CURRENT },
        [FOREIGN_AGENT]: { ownerPubkey: OTHER_OWNER },
      },
      missing: [],
    }),
  });

  // Send-time admission reproduces the autocomplete result: the offered
  // agent survives, the never-offered foreign agent is stripped.
  assert.deepEqual(result, [HUMAN, AGENT]);
});

for (const [name, refetchOwnerProfiles] of [
  ["revoked owner proof", async () => ({ profiles: {}, missing: [AGENT] })],
  [
    "changed owner proof",
    async () => ({
      profiles: { [AGENT]: { ownerPubkey: OTHER_OWNER } },
      missing: [],
    }),
  ],
  [
    "owner profile query error",
    async () => {
      throw new Error("relay unavailable");
    },
  ],
]) {
  test(`owner-only revalidation fails closed on ${name}`, async () => {
    assert.deepEqual(
      await revalidateAgentMentionPubkeys(options(refetchOwnerProfiles)),
      [HUMAN],
    );
  });
}
