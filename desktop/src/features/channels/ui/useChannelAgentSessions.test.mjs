import assert from "node:assert/strict";
import { describe, it } from "node:test";

import {
  buildChannelAgentSessionCandidates,
  getChannelAgentSessionAgents,
} from "./useChannelAgentSessions.ts";

const AGENT = "aa".repeat(32);
const OTHER = "bb".repeat(32);

const CHANNEL = { id: "chan-new", name: "new-chan" };

function relayAgent(overrides = {}) {
  return {
    pubkey: AGENT,
    name: "Fable",
    status: "online",
    channels: [],
    channelIds: [],
    ...overrides,
  };
}

function pick(agents) {
  return agents.map((agent) => agent.pubkey);
}

describe("getChannelAgentSessionAgents", () => {
  it("admits a relay agent whose declared list is stale", () => {
    // The regression: an agent's kind:10100 snapshot predates this channel,
    // but the relay says it is a member. Excluding it here strips the bot
    // typing classification, the working signal, and the View activity
    // affordance from every channel created after the agent last published.
    const agents = buildChannelAgentSessionCandidates({
      channelMembers: [{ pubkey: AGENT, role: "bot" }],
      managedAgents: [],
      relayAgents: [
        relayAgent({ channels: ["buzz"], channelIds: ["chan-buzz"] }),
      ],
    });

    assert.deepEqual(
      pick(
        getChannelAgentSessionAgents({
          activeChannel: CHANNEL,
          activeChannelId: CHANNEL.id,
          agents,
          channelMembers: [{ pubkey: AGENT, role: "bot" }],
        }),
      ),
      [AGENT],
    );
  });

  it("admits a relay agent that declares the channel but is not a member", () => {
    const agents = buildChannelAgentSessionCandidates({
      channelMembers: [],
      managedAgents: [],
      relayAgents: [relayAgent({ channelIds: [CHANNEL.id] })],
    });

    assert.deepEqual(
      pick(
        getChannelAgentSessionAgents({
          activeChannel: CHANNEL,
          activeChannelId: CHANNEL.id,
          agents,
          channelMembers: [],
        }),
      ),
      [AGENT],
    );
  });

  it("still excludes a relay agent that neither declares nor belongs", () => {
    const agents = buildChannelAgentSessionCandidates({
      channelMembers: [{ pubkey: OTHER, role: "member" }],
      managedAgents: [],
      relayAgents: [
        relayAgent({ channels: ["buzz"], channelIds: ["chan-buzz"] }),
      ],
    });

    assert.deepEqual(
      pick(
        getChannelAgentSessionAgents({
          activeChannel: CHANNEL,
          activeChannelId: CHANNEL.id,
          agents,
          channelMembers: [{ pubkey: OTHER, role: "member" }],
        }),
      ),
      [],
    );
  });

  it("matches membership case-insensitively", () => {
    const members = [{ pubkey: AGENT.toUpperCase(), role: "bot" }];
    const agents = buildChannelAgentSessionCandidates({
      channelMembers: members,
      managedAgents: [],
      relayAgents: [
        relayAgent({ channels: ["buzz"], channelIds: ["chan-buzz"] }),
      ],
    });

    assert.deepEqual(
      pick(
        getChannelAgentSessionAgents({
          activeChannel: CHANNEL,
          activeChannelId: CHANNEL.id,
          agents,
          channelMembers: members,
        }),
      ),
      [AGENT],
    );
  });

  it("falls back to the declared list when the member list is unavailable", () => {
    // `channelMembers` undefined means "not loaded yet", not "no members".
    const agents = buildChannelAgentSessionCandidates({
      managedAgents: [],
      relayAgents: [relayAgent({ channels: [CHANNEL.name] })],
    });

    assert.deepEqual(
      pick(
        getChannelAgentSessionAgents({
          activeChannel: CHANNEL,
          activeChannelId: CHANNEL.id,
          agents,
        }),
      ),
      [AGENT],
    );
  });

  it("leaves the managed and member-bot branches on membership", () => {
    const members = [
      { pubkey: AGENT, role: "member" },
      { pubkey: OTHER, role: "bot", displayName: "Member Bot" },
    ];
    const agents = buildChannelAgentSessionCandidates({
      channelMembers: members,
      managedAgents: [{ pubkey: AGENT, name: "Managed", status: "deployed" }],
      relayAgents: [],
    });

    const admitted = getChannelAgentSessionAgents({
      activeChannel: CHANNEL,
      activeChannelId: CHANNEL.id,
      agents,
      channelMembers: members,
    });

    assert.deepEqual(pick(admitted), [AGENT, OTHER]);
    assert.deepEqual(
      admitted.map((agent) => agent.agentSource),
      ["managed", "member-bot"],
    );
  });

  it("returns nothing without an active channel", () => {
    const agents = buildChannelAgentSessionCandidates({
      channelMembers: [{ pubkey: AGENT, role: "bot" }],
      managedAgents: [],
      relayAgents: [relayAgent()],
    });

    assert.deepEqual(
      getChannelAgentSessionAgents({
        activeChannel: null,
        activeChannelId: null,
        agents,
        channelMembers: [{ pubkey: AGENT, role: "bot" }],
      }),
      [],
    );
  });
});
