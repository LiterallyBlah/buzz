import assert from "node:assert/strict";
import test from "node:test";

import { buildProjectRootAgents } from "./projectRootAgents.ts";

const AGENT_A = "a".repeat(64);
const AGENT_B = "b".repeat(64);
const HUMAN = "c".repeat(64);

const never = () => false;
const always = () => true;

/** A live NIP-PA entry as `liveProjectActivity` hands it over. */
function liveEntry(agent, state, announcedAtSecs) {
  return {
    agent,
    turnId: `turn-${agent}`,
    state,
    stage: null,
    announcedAt: announcedAtSecs,
  };
}

test("remembers a background agent that never commented", () => {
  // The whole point: an agent enrolled by a peer call announced a turn here,
  // said nothing, and its NIP-PA frames have long since expired.
  const agents = buildProjectRootAgents({
    commentAuthors: [{ author: HUMAN, createdAt: 1_700 }],
    isKnownAgent: never,
    live: [],
    seen: { [AGENT_A]: 5_000 },
  });

  assert.deepEqual(agents, [
    { pubkey: AGENT_A, state: null, lastActiveAt: 5_000 },
  ]);
});

test("unions live activity, memory, and agent comment authors", () => {
  const agents = buildProjectRootAgents({
    commentAuthors: [
      { author: AGENT_B, createdAt: 9 },
      { author: HUMAN, createdAt: 10 },
    ],
    isKnownAgent: (pubkey) => pubkey === AGENT_B,
    live: [liveEntry(AGENT_A, "working", 100)],
    seen: { [AGENT_A]: 1_000 },
  });

  assert.deepEqual(
    agents.map((agent) => agent.pubkey),
    [AGENT_A, AGENT_B],
  );
});

test("excludes comment authors that are not known agents", () => {
  const agents = buildProjectRootAgents({
    commentAuthors: [{ author: HUMAN, createdAt: 10 }],
    isKnownAgent: never,
  });

  assert.deepEqual(agents, []);
});

test("keeps a commenting agent whose agent-ness is only known live", () => {
  // The known-agent baseline can lag a background enrollment. A pubkey that
  // signed a NIP-PA frame here is an agent by stronger evidence than the list,
  // so its comment must still count toward ordering rather than be dropped.
  const agents = buildProjectRootAgents({
    commentAuthors: [{ author: AGENT_A, createdAt: 900 }],
    isKnownAgent: never,
    live: [liveEntry(AGENT_A, "queued", 100)],
  });

  assert.deepEqual(agents, [
    { pubkey: AGENT_A, state: "queued", lastActiveAt: 900_000 },
  ]);
});

test("dedupes one agent seen through every source", () => {
  const agents = buildProjectRootAgents({
    commentAuthors: [{ author: AGENT_A.toUpperCase(), createdAt: 1 }],
    isKnownAgent: always,
    live: [liveEntry(AGENT_A, "working", 2)],
    seen: { [AGENT_A]: 3_000 },
  });

  assert.equal(agents.length, 1);
  assert.equal(agents[0].pubkey, AGENT_A);
  assert.equal(agents[0].state, "working");
  // Best evidence across sources wins: the memory's 3000ms beats both events.
  assert.equal(agents[0].lastActiveAt, 3_000);
});

test("orders working before queued before remembered", () => {
  const AGENT_C = "d".repeat(64);
  const agents = buildProjectRootAgents({
    isKnownAgent: never,
    live: [liveEntry(AGENT_C, "queued", 500), liveEntry(AGENT_B, "working", 1)],
    // The remembered agent has the most recent evidence by far and still sorts
    // last: live state outranks recency.
    seen: { [AGENT_A]: 9_000_000 },
  });

  assert.deepEqual(
    agents.map((agent) => [agent.pubkey, agent.state]),
    [
      [AGENT_B, "working"],
      [AGENT_C, "queued"],
      [AGENT_A, null],
    ],
  );
});

test("orders equal-state agents by recency, then pubkey", () => {
  const AGENT_C = "d".repeat(64);
  const agents = buildProjectRootAgents({
    isKnownAgent: never,
    seen: { [AGENT_B]: 100, [AGENT_A]: 100, [AGENT_C]: 200 },
  });

  assert.deepEqual(
    agents.map((agent) => agent.pubkey),
    [AGENT_C, AGENT_A, AGENT_B],
  );
});

test("working displaces a queued frame for the same agent", () => {
  const agents = buildProjectRootAgents({
    isKnownAgent: never,
    live: [liveEntry(AGENT_A, "queued", 10), liveEntry(AGENT_A, "working", 5)],
  });

  assert.deepEqual(agents, [
    { pubkey: AGENT_A, state: "working", lastActiveAt: 10_000 },
  ]);
});

test("returns nothing when every source is empty", () => {
  assert.deepEqual(buildProjectRootAgents({ isKnownAgent: always }), []);
});
