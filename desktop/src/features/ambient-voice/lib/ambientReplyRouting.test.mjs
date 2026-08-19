import assert from "node:assert/strict";
import test from "node:test";

import {
  classifyAmbientReply,
  routeAmbientReply,
} from "./ambientReplyRouting.ts";

const AGENT = "a".repeat(64);
const OTHER_AGENT = "b".repeat(64);
const SELF = "c".repeat(64);
const DM = "11111111-1111-4111-8111-111111111111";

const reply = (overrides = {}) => ({
  id: "1",
  kind: 9,
  pubkey: AGENT,
  content: "It is sunny.",
  tags: [["h", DM]],
  ...overrides,
});

const classify = (event, agent = AGENT, destination = DM) =>
  classifyAmbientReply(event, agent, SELF, destination);

test("the bound agent's reply in the destination is spoken", () => {
  assert.equal(classify(reply()).text, "It is sunny.");
});

test("only the bound agent is spoken, even inside the destination", () => {
  // A channel destination can hold several bots. Binding a wake word to one
  // agent is not consent for the rest to speak through the user's speakers.
  const result = classify(reply({ pubkey: OTHER_AGENT }));
  assert.equal(result.text, null);
  assert.equal(result.reason, "not_the_bound_agent");
});

test("messages in another channel are ignored", () => {
  const result = classify(
    reply({ tags: [["h", "22222222-2222-4222-8222-222222222222"]] }),
  );
  assert.equal(result.text, null);
  assert.equal(result.reason, "h_tag_mismatch");
});

test("the user's own messages are never read back", () => {
  const result = classifyAmbientReply(reply({ pubkey: SELF }), SELF, SELF, DM);
  assert.equal(result.text, null);
  assert.equal(result.reason, "self_authored");
});

test("without a binding or a destination nothing is spoken", () => {
  assert.equal(
    classifyAmbientReply(reply(), null, SELF, DM).reason,
    "no_destination",
  );
  assert.equal(
    classifyAmbientReply(reply(), AGENT, SELF, null).reason,
    "no_destination",
  );
});

test("non-message kinds and system text are dropped", () => {
  assert.equal(classify(reply({ kind: 48106 })).reason, "unsupported_kind");
  assert.equal(
    classify(reply({ content: "[System] agent joined" })).reason,
    "empty_or_system",
  );
  assert.equal(classify(reply({ content: "   " })).reason, "empty_or_system");
});

test("attachment markup is stripped before speaking", () => {
  const event = reply({
    content: "Here it is.\n[shot](https://media.example/x.png)",
    tags: [
      ["h", DM],
      ["imeta", "url https://media.example/x.png"],
    ],
  });
  assert.equal(classify(event).text, "Here it is.");
});

test("routing hands the speakable text to the queue and reports the outcome", () => {
  const spoken = [];
  const enqueue = (text, routeId) => {
    spoken.push([text, routeId]);
    return "queued";
  };
  assert.equal(
    routeAmbientReply(reply(), AGENT, SELF, DM, 7, enqueue),
    "queued",
  );
  assert.deepEqual(spoken, [["It is sunny.", 7]]);

  // A rejected event never reaches the queue.
  assert.equal(
    routeAmbientReply(
      reply({ pubkey: OTHER_AGENT }),
      AGENT,
      SELF,
      DM,
      8,
      enqueue,
    ),
    "not_the_bound_agent",
  );
  assert.equal(spoken.length, 1);
});

test("a disabled speaker is reported rather than swallowed", () => {
  assert.equal(
    routeAmbientReply(reply(), AGENT, SELF, DM, 9, () => "disabled"),
    "disabled",
  );
});
