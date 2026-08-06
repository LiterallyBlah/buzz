/**
 * Opening an ACP activity pane for an agent nobody has a roster entry for.
 *
 * The case that matters is the hand-provisioned relay member: it announces
 * turns on a project root, so its name is on screen and clickable, but it is
 * on neither the managed-agent roster nor the relay-agent roster this client
 * keeps. Every assertion here is about that miss being a supported outcome
 * rather than an error path — in particular that the synthesized descriptor
 * still opens a live feed, because a descriptor that reported the agent
 * stopped would silently close the pane on precisely the agents this path
 * exists for.
 */

import assert from "node:assert/strict";
import { describe, it } from "node:test";

import {
  resolveAgentSessionPaneAgent,
  resolveAgentSessionStopState,
} from "./agentSessionPaneAgent.ts";

const ALICE = "a".repeat(64);
const BOB = "b".repeat(64);

function candidate(overrides = {}) {
  return {
    pubkey: ALICE,
    name: "Claude",
    status: "running",
    agentSource: "managed",
    canInterruptTurn: true,
    ...overrides,
  };
}

describe("resolveAgentSessionPaneAgent", () => {
  it("returns the roster entry when the agent is known", () => {
    const agent = resolveAgentSessionPaneAgent({
      candidates: [candidate({ pubkey: BOB, name: "Goose" }), candidate()],
      pubkey: ALICE,
    });

    assert.equal(agent.pubkey, ALICE);
    assert.equal(agent.name, "Claude");
    assert.equal(agent.status, "running");
    assert.equal(agent.agentSource, "managed");
    // Interruptibility is the roster's claim to make, not this function's:
    // a managed agent stays interruptible, and whether the *pane* can act on
    // that is resolveAgentSessionStopState's question.
    assert.equal(agent.canInterruptTurn, true);
  });

  it("matches on normalized pubkeys, not raw string equality", () => {
    const agent = resolveAgentSessionPaneAgent({
      candidates: [candidate({ pubkey: ALICE.toUpperCase() })],
      pubkey: ALICE,
    });

    assert.equal(agent.agentSource, "managed");
    assert.equal(agent.name, "Claude");
  });

  it("synthesizes a live relay descriptor for an agent on no roster", () => {
    const agent = resolveAgentSessionPaneAgent({
      candidates: [],
      fallbackName: "Claude",
      pubkey: ALICE,
    });

    assert.equal(agent.pubkey, ALICE);
    assert.equal(agent.name, "Claude");
    // The load-bearing assertion: "deployed" is what isManagedAgentActive
    // reads to decide whether to subscribe to the observer feed.
    assert.equal(agent.status, "deployed");
    assert.equal(agent.agentSource, "relay");
    assert.equal(agent.canInterruptTurn, false);
  });

  it("falls back to a truncated pubkey when no name is available", () => {
    const agent = resolveAgentSessionPaneAgent({
      candidates: [],
      pubkey: ALICE,
    });

    assert.equal(agent.name, `${ALICE.slice(0, 8)}…${ALICE.slice(-4)}`);
  });

  it("treats a blank fallback name as no name", () => {
    const agent = resolveAgentSessionPaneAgent({
      candidates: [],
      fallbackName: "   ",
      pubkey: ALICE,
    });

    assert.equal(agent.name, `${ALICE.slice(0, 8)}…${ALICE.slice(-4)}`);
  });

  it("names a roster entry whose own name is empty", () => {
    const agent = resolveAgentSessionPaneAgent({
      candidates: [candidate({ name: "" })],
      fallbackName: "Claude",
      pubkey: ALICE,
    });

    assert.equal(agent.name, "Claude");
    // Still the roster entry in every other respect — the name fallback must
    // not downgrade a known managed agent to a synthesized relay one.
    assert.equal(agent.agentSource, "managed");
  });
});

describe("resolveAgentSessionStopState", () => {
  it("enables the stop action only with a working, interruptible, channel-scoped agent", () => {
    const state = resolveAgentSessionStopState({
      canInterruptTurn: true,
      hasChannel: true,
      isWorking: true,
    });

    assert.equal(state.enabled, true);
  });

  it("reports idleness first, so a stopped agent is not blamed on provenance", () => {
    const state = resolveAgentSessionStopState({
      canInterruptTurn: false,
      hasChannel: false,
      isWorking: false,
    });

    assert.equal(state.enabled, false);
    assert.match(state.reason, /while the agent is working/);
  });

  it("blames provenance when the agent is working but not locally managed", () => {
    const state = resolveAgentSessionStopState({
      canInterruptTurn: false,
      hasChannel: true,
      isWorking: true,
    });

    assert.equal(state.enabled, false);
    assert.match(state.reason, /locally managed/);
  });

  it("blames the missing channel for an interruptible agent in an agent-scoped pane", () => {
    const state = resolveAgentSessionStopState({
      canInterruptTurn: true,
      hasChannel: false,
      isWorking: true,
    });

    assert.equal(state.enabled, false);
    // The distinction this whole function exists for: a project-root pane must
    // not tell the viewer their locally managed agent is somebody else's.
    assert.match(state.reason, /needs the channel/);
    assert.doesNotMatch(state.reason, /locally managed/);
  });
});
