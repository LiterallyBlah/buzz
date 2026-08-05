import assert from "node:assert/strict";
import { describe, it } from "node:test";

import {
  buildVerifiedAgentOwnerIndex,
  selectOwnedRelayAgents,
} from "./ownedRelayAgents.ts";

const ME = "aaaa1234aaaa1234aaaa1234aaaa1234aaaa1234aaaa1234aaaa1234aaaa1234";
const OTHER =
  "bbbb4321bbbb4321bbbb4321bbbb4321bbbb4321bbbb4321bbbb4321bbbb4321";
const AGENT_LOCAL =
  "cccc1111cccc1111cccc1111cccc1111cccc1111cccc1111cccc1111cccc1111";
const AGENT_REMOTE =
  "dddd2222dddd2222dddd2222dddd2222dddd2222dddd2222dddd2222dddd2222";
const AGENT_FOREIGN =
  "eeee3333eeee3333eeee3333eeee3333eeee3333eeee3333eeee3333eeee3333";

const ownedBy = (owner) => new Map([[AGENT_REMOTE, owner]]);

describe("buildVerifiedAgentOwnerIndex", () => {
  it("indexes only profiles that carry a verified NIP-OA owner", () => {
    const index = buildVerifiedAgentOwnerIndex({
      [AGENT_REMOTE]: { ownerPubkey: ME },
      [AGENT_FOREIGN]: { ownerPubkey: null },
    });

    assert.deepEqual([...index], [[AGENT_REMOTE, ME]]);
  });

  it("normalises both the agent and the owner pubkey", () => {
    const index = buildVerifiedAgentOwnerIndex({
      [AGENT_REMOTE.toUpperCase()]: { ownerPubkey: ` ${ME.toUpperCase()} ` },
    });

    assert.equal(index.get(AGENT_REMOTE), ME);
  });

  it("is empty when the batch has not resolved", () => {
    assert.equal(buildVerifiedAgentOwnerIndex(undefined).size, 0);
  });
});

describe("selectOwnedRelayAgents", () => {
  it("lists a relay agent whose verified owner is the current identity", () => {
    const remote = { pubkey: AGENT_REMOTE, name: "nadia" };

    const result = selectOwnedRelayAgents([remote], [], ownedBy(ME), ME);

    assert.deepEqual(result, [remote]);
  });

  it("omits a relay agent owned by somebody else", () => {
    const result = selectOwnedRelayAgents(
      [{ pubkey: AGENT_FOREIGN, name: "someone-elses-bot" }],
      [],
      new Map([[AGENT_FOREIGN, OTHER]]),
      ME,
    );

    assert.deepEqual(result, []);
  });

  it("omits a relay agent with no verified owner at all", () => {
    // The unscoped kind:10100 community directory is mostly these. Ownership
    // is the scope, so an entry with no NIP-OA owner is never listed — its
    // kind:10100 name and capabilities are self-asserted and prove nothing.
    const result = selectOwnedRelayAgents(
      [{ pubkey: AGENT_REMOTE, name: "claims-to-be-mine" }],
      [],
      new Map(),
      ME,
    );

    assert.deepEqual(result, []);
  });

  it("omits an agent that is already managed locally, so it appears once", () => {
    const result = selectOwnedRelayAgents(
      [{ pubkey: AGENT_LOCAL, name: "local-copy" }],
      [{ pubkey: AGENT_LOCAL }],
      new Map([[AGENT_LOCAL, ME]]),
      ME,
    );

    assert.deepEqual(result, []);
  });

  it("deduplicates a local agent listed under different casing", () => {
    const result = selectOwnedRelayAgents(
      [{ pubkey: AGENT_LOCAL.toUpperCase(), name: "local-copy" }],
      [{ pubkey: AGENT_LOCAL }],
      new Map([[AGENT_LOCAL, ME]]),
      ME,
    );

    assert.deepEqual(result, []);
  });

  it("emits one card when the relay lists the same agent twice", () => {
    const result = selectOwnedRelayAgents(
      [
        { pubkey: AGENT_REMOTE, name: "nadia" },
        { pubkey: AGENT_REMOTE.toUpperCase(), name: "nadia" },
      ],
      [],
      ownedBy(ME),
      ME,
    );

    assert.deepEqual(result, [{ pubkey: AGENT_REMOTE, name: "nadia" }]);
  });

  it("matches ownership case-insensitively", () => {
    const remote = { pubkey: AGENT_REMOTE.toUpperCase(), name: "nadia" };

    const result = selectOwnedRelayAgents(
      [remote],
      [],
      ownedBy(ME.toUpperCase()),
      ME,
    );

    assert.deepEqual(result, [remote]);
  });

  it("lists nothing before identity resolves", () => {
    // An unresolved identity must never read as "owned by me" — that would
    // flash every owner-declared relay agent onto the tab during startup.
    assert.deepEqual(
      selectOwnedRelayAgents(
        [{ pubkey: AGENT_REMOTE, name: "nadia" }],
        [],
        ownedBy(ME),
        undefined,
      ),
      [],
    );
  });

  it("lists nothing before the relay and profile queries resolve", () => {
    assert.deepEqual(
      selectOwnedRelayAgents(undefined, undefined, new Map(), ME),
      [],
    );
  });
});
