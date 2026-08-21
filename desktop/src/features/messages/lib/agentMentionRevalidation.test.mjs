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

function options() {
  return {
    pubkeys: [HUMAN, AGENT],
    agentPubkeys: new Set([AGENT]),
    currentPubkey: CURRENT,
    eligibilityScope: { type: "channel", channelId: "general" },
    sharedChannelIds: new Set(["general"]),
    refetchManagedAgents: async () => ({ data: [], error: null }),
    fetchRelayAgents: async () => [
      {
        pubkey: AGENT,
        respondTo: "anyone",
        respondToAllowlist: [],
        channelIds: ["general"],
      },
    ],
  };
}

test("relay policy revalidation admits an authorized external agent", async () => {
  assert.deepEqual(await revalidateAgentMentionPubkeys(options()), [
    HUMAN,
    AGENT,
  ]);
});

test("fresh managed evidence survives unrelated relay authorization errors", async () => {
  const result = await revalidateAgentMentionPubkeys({
    ...options(),
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
    ...options(),
    fetchRelayAgents: async () => {
      throw new Error("relay directory unavailable");
    },
  });

  assert.deepEqual(result, [HUMAN]);
});

test("mixed evidence preserves only fresh managed agents and humans", async () => {
  const result = await revalidateAgentMentionPubkeys({
    ...options(),
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
    refetchManagedAgents: async () => ({ data: [], error: null }),
    fetchRelayAgents: async () => [ownAgent, foreignAgent],
  });

  assert.deepEqual(result, [HUMAN, AGENT]);
});
