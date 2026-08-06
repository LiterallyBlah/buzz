/**
 * Tests for the observability of the `knownAgentPubkeys` ingest gate.
 *
 * The gate itself is defense-in-depth and is not under test here — it must
 * keep refusing frames for agents this identity cannot attribute to itself.
 * What is under test is that the refusal leaves a trace: before this, a frame
 * addressed to us for an unattributed agent was discarded with no console
 * signal, no connection-state change, and no store write, so the Activity pane
 * was indistinguishable from one belonging to an agent that had simply done
 * nothing.
 *
 * These drive the real live-relay handler via `_testHandleRelayObserverEvent`,
 * so the buffering branch, the record, and the clear-on-registration path are
 * all exercised as production runs them.
 */

import assert from "node:assert/strict";
import { beforeEach, describe, it } from "node:test";

import {
  getUnattributedAgentFrames,
  resetAgentObserverStore,
  _testHandleRelayObserverEvent,
  _testRegisterKnownAgents,
} from "@/features/agents/observerRelayStore.ts";

// ── Constants ─────────────────────────────────────────────────────────────────

const TRUSTED_AGENT = "a".repeat(64);
const UNATTRIBUTED_AGENT = "b".repeat(64);
const OWNER_PUBKEY = "c".repeat(64);
const SUB_ID = "test-sub-1";

// ── Helpers ───────────────────────────────────────────────────────────────────

function makeRawEvent({ agent = UNATTRIBUTED_AGENT, createdAt = 1000 } = {}) {
  return {
    id: "e".repeat(64),
    pubkey: agent,
    created_at: createdAt,
    kind: 24200,
    tags: [
      ["p", OWNER_PUBKEY],
      ["agent", agent],
      ["frame", "telemetry"],
    ],
    content: "encrypted",
    sig: "s".repeat(128),
  };
}

// `recordUnattributedAgentFrame` console.warns once per agent. Silence it so a
// deliberate misconfiguration case does not look like a failing test run, while
// still asserting the signal is emitted exactly once.
function captureWarnings(run) {
  const original = console.warn;
  const warnings = [];
  console.warn = (...args) => warnings.push(args.join(" "));
  return Promise.resolve()
    .then(run)
    .finally(() => {
      console.warn = original;
    })
    .then(() => warnings);
}

// ── Tests ─────────────────────────────────────────────────────────────────────

describe("unattributed observer frames", () => {
  beforeEach(() => {
    resetAgentObserverStore();
  });

  it("records a frame dropped for an agent outside a populated trusted set", async () => {
    _testRegisterKnownAgents(SUB_ID, [TRUSTED_AGENT]);

    const warnings = await captureWarnings(() =>
      _testHandleRelayObserverEvent(
        makeRawEvent({ agent: UNATTRIBUTED_AGENT, createdAt: 1700 }),
      ),
    );

    const record = getUnattributedAgentFrames(UNATTRIBUTED_AGENT);
    assert.deepEqual(record, {
      agentPubkey: UNATTRIBUTED_AGENT,
      droppedFrames: 1,
      lastFrameAt: 1700,
    });
    assert.equal(warnings.length, 1, "the drop must be diagnosable");
    assert.match(warnings[0], new RegExp(UNATTRIBUTED_AGENT));
  });

  it("counts repeat drops but warns only once per agent", async () => {
    _testRegisterKnownAgents(SUB_ID, [TRUSTED_AGENT]);

    const warnings = await captureWarnings(async () => {
      await _testHandleRelayObserverEvent(makeRawEvent({ createdAt: 1700 }));
      await _testHandleRelayObserverEvent(makeRawEvent({ createdAt: 1800 }));
      await _testHandleRelayObserverEvent(makeRawEvent({ createdAt: 1900 }));
    });

    assert.deepEqual(getUnattributedAgentFrames(UNATTRIBUTED_AGENT), {
      agentPubkey: UNATTRIBUTED_AGENT,
      droppedFrames: 3,
      lastFrameAt: 1900,
    });
    assert.equal(warnings.length, 1, "a busy agent must not spam the console");
  });

  it("does not report a startup race: an empty trusted set buffers instead", async () => {
    // No registration at all — this is the pre-registration window.
    const warnings = await captureWarnings(() =>
      _testHandleRelayObserverEvent(makeRawEvent()),
    );

    assert.equal(getUnattributedAgentFrames(UNATTRIBUTED_AGENT), null);
    assert.equal(warnings.length, 0);

    // A subscriber that registers an empty agent list is still the startup
    // window — `knownAgentPubkeys` has not filled yet.
    _testRegisterKnownAgents(SUB_ID, []);
    const moreWarnings = await captureWarnings(() =>
      _testHandleRelayObserverEvent(makeRawEvent()),
    );

    assert.equal(getUnattributedAgentFrames(UNATTRIBUTED_AGENT), null);
    assert.equal(moreWarnings.length, 0);
  });

  it("clears the record once the agent becomes trusted", async () => {
    _testRegisterKnownAgents(SUB_ID, [TRUSTED_AGENT]);
    await captureWarnings(() => _testHandleRelayObserverEvent(makeRawEvent()));
    assert.ok(getUnattributedAgentFrames(UNATTRIBUTED_AGENT));

    // Its profile finally loaded and it joined the ingestion list.
    _testRegisterKnownAgents(SUB_ID, [TRUSTED_AGENT, UNATTRIBUTED_AGENT]);

    assert.equal(
      getUnattributedAgentFrames(UNATTRIBUTED_AGENT),
      null,
      "an agent that is now trusted must not still read as unattributed",
    );
  });

  it("leaves a trusted agent's frames unrecorded", async () => {
    _testRegisterKnownAgents(SUB_ID, [TRUSTED_AGENT]);

    // A trusted agent's frame proceeds past the gate. Decryption is not
    // available in this harness, so the handler surfaces a connection error
    // rather than a drop record — what matters here is that the gate did not
    // treat it as unattributed.
    await captureWarnings(() =>
      _testHandleRelayObserverEvent(makeRawEvent({ agent: TRUSTED_AGENT })),
    );

    assert.equal(getUnattributedAgentFrames(TRUSTED_AGENT), null);
  });

  it("returns null for an agent with no dropped frames and for no agent", () => {
    assert.equal(getUnattributedAgentFrames(UNATTRIBUTED_AGENT), null);
    assert.equal(getUnattributedAgentFrames(null), null);
    assert.equal(getUnattributedAgentFrames(undefined), null);
  });

  it("normalizes pubkey case on both write and read", async () => {
    _testRegisterKnownAgents(SUB_ID, [TRUSTED_AGENT]);
    const upper = "D".repeat(64);

    await captureWarnings(() =>
      _testHandleRelayObserverEvent(
        makeRawEvent({ agent: upper, createdAt: 2000 }),
      ),
    );

    assert.deepEqual(getUnattributedAgentFrames(upper), {
      agentPubkey: "d".repeat(64),
      droppedFrames: 1,
      lastFrameAt: 2000,
    });
    assert.deepEqual(
      getUnattributedAgentFrames("d".repeat(64)),
      getUnattributedAgentFrames(upper),
    );
  });

  it("is cleared by the community-switch store reset", async () => {
    _testRegisterKnownAgents(SUB_ID, [TRUSTED_AGENT]);
    await captureWarnings(() => _testHandleRelayObserverEvent(makeRawEvent()));
    assert.ok(getUnattributedAgentFrames(UNATTRIBUTED_AGENT));

    resetAgentObserverStore();

    assert.equal(
      getUnattributedAgentFrames(UNATTRIBUTED_AGENT),
      null,
      "one community's misattribution must not leak into the next",
    );
  });
});
