import assert from "node:assert/strict";
import test from "node:test";

import {
  ambientModelRows,
  ambientSaveBlock,
  mergeAgentOptions,
  modelStatusLabel,
  withPrimaryBinding,
} from "./ambientSettingsLogic.ts";

const AGENT = "a".repeat(64);
const valid = {
  valid: true,
  message: null,
  tokens: null,
  checkedAgainstModel: true,
};

test("both agent sources are offered, deduplicated by pubkey", () => {
  const options = mergeAgentOptions(
    [{ pubkey: AGENT, name: "Hermes", status: "running" }],
    [
      { pubkey: AGENT, name: "hermes-gateway" },
      { pubkey: "b".repeat(64), name: "Archivist" },
    ],
  );
  assert.deepEqual(
    options.map((o) => [o.name, o.source]),
    [
      ["Archivist", "channel"],
      ["Hermes", "managed"],
    ],
  );
  // The managed entry wins the duplicate, because it carries process status.
  assert.equal(options.find((o) => o.pubkey === AGENT).status, "running");
});

test("a channel bot with no name falls back to a readable pubkey stub", () => {
  const [option] = mergeAgentOptions(
    [],
    [{ pubkey: "f".repeat(64), name: "  " }],
  );
  assert.equal(option.name, "ffffffff…");
});

test("an unchecked wake word is never saveable", () => {
  // Fail closed: no answer from the validator is not a pass. Persisting an
  // un-encodable phrase would hand the C library input that kills the process.
  const block = ambientSaveBlock("hey hermes", AGENT, null, null);
  assert.equal(block.reason, "wake_word");
});

test("an invalid wake word blocks saving and shows the validator's own words", () => {
  const block = ambientSaveBlock(
    "the",
    AGENT,
    {
      valid: false,
      message: "A one-word wake phrase needs at least 8 letters",
      tokens: null,
      checkedAgainstModel: false,
    },
    null,
  );
  assert.equal(block.reason, "wake_word");
  assert.match(block.message, /at least 8 letters/);
});

test("an empty wake word and a missing agent are each reported", () => {
  assert.equal(ambientSaveBlock("  ", AGENT, valid, null).reason, "wake_word");
  assert.equal(
    ambientSaveBlock("hey hermes", null, valid, null).reason,
    "agent",
  );
});

test("a valid form with an agent is saveable", () => {
  assert.equal(ambientSaveBlock("hey hermes", AGENT, valid, null), null);
});

test("a settings file that failed to load blocks every write", () => {
  const block = ambientSaveBlock("hey hermes", AGENT, valid, "not valid JSON");
  assert.equal(block.reason, "load_error");
  assert.match(block.message, /not valid JSON/);
});

test("editing the first binding preserves later ones", () => {
  const settings = {
    version: 1,
    enabled: true,
    muted: false,
    wakeBindings: [
      { wakeWord: "hey hermes", agentPubkey: AGENT, destination: null },
      {
        wakeWord: "hey archivist",
        agentPubkey: "b".repeat(64),
        destination: null,
      },
    ],
    stt: { backend: "local", endpointUrl: null },
    tts: { backend: "local", endpointUrl: null },
    inputDeviceId: null,
    outputDevice: null,
  };
  const next = withPrimaryBinding(settings, {
    wakeWord: "good morning buzz",
    agentPubkey: AGENT,
    destination: null,
  });
  assert.equal(next.wakeBindings.length, 2);
  assert.equal(next.wakeBindings[0].wakeWord, "good morning buzz");
  assert.equal(next.wakeBindings[1].wakeWord, "hey archivist");
  // Unrelated fields are carried through untouched.
  assert.equal(next.enabled, true);
});

test("model status reads as progress, not as a state name", () => {
  // Fixtures are the Rust `ModelStatus` enum's real serialisation (externally
  // tagged, snake_case), pinned from the producing side by
  // `ambient_model_status_serialises_the_shape_the_frontend_parses`. This
  // test's first version invented a `{status: "…"}` shape, and every model
  // row shipped rendering "undefined".
  assert.equal(modelStatusLabel("not_downloaded"), "Not downloaded");
  assert.equal(
    modelStatusLabel({ downloading: { progress_percent: 42 } }),
    "Downloading… 42%",
  );
  assert.equal(modelStatusLabel("ready"), "Ready");
  assert.equal(modelStatusLabel({ error: "network" }), "Failed: network");
});

test("all three local models are listed, in the order the session needs them", () => {
  // Listing only the wake word was the M1 gap: a missing speech-to-text model
  // makes the session deaf and a missing voice makes it mute, and neither
  // failure surfaces anywhere else in the app.
  const models = {
    kws: "ready",
    stt: { downloading: { progress_percent: 7 } },
    tts: { error: "checksum mismatch" },
  };
  assert.deepEqual(
    ambientModelRows(models).map((row) => [
      row.key,
      row.label,
      modelStatusLabel(row.status),
    ]),
    [
      ["kws", "Wake word", "Ready"],
      ["stt", "Speech to text", "Downloading… 7%"],
      ["tts", "Voice", "Failed: checksum mismatch"],
    ],
  );
});

test("nothing is listed before the model manager has answered", () => {
  // A blank list is honest; inventing "Not downloaded" for three models the
  // app has not asked about yet would be a fabricated alarm on every launch.
  assert.deepEqual(ambientModelRows(null), []);
});
