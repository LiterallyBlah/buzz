import assert from "node:assert/strict";
import test from "node:test";

import {
  ambientModelRows,
  ambientSaveBlock,
  clampSilenceHoldMs,
  mergeAgentOptions,
  modelStatusLabel,
  silenceHoldLabel,
  withPrimaryBinding,
  SILENCE_HOLD_DEFAULT_MS,
  SILENCE_HOLD_MAX_MS,
  SILENCE_HOLD_MIN_MS,
  SILENCE_HOLD_STEP_MS,
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
  assert.equal(option.name, "ffffffff…ffff");
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

test("the slider offers the range the native side accepts, and no more", () => {
  // These bounds are duplicated from `ambient_voice::utterance`, which clamps
  // to the same range on load and refuses a save outside it. A slider that
  // could produce a value the save refuses would put a red banner in front of
  // someone who only dragged a handle.
  assert.equal(SILENCE_HOLD_MIN_MS, 300);
  assert.equal(SILENCE_HOLD_MAX_MS, 10_000);
  assert.equal(SILENCE_HOLD_DEFAULT_MS, 800);
  // A step that divides the range and lands on the default.
  assert.equal(SILENCE_HOLD_DEFAULT_MS % SILENCE_HOLD_STEP_MS, 0);
  assert.equal(
    (SILENCE_HOLD_MAX_MS - SILENCE_HOLD_MIN_MS) % SILENCE_HOLD_STEP_MS,
    0,
  );
});

test("a hold outside the range is clamped rather than sent", () => {
  assert.equal(clampSilenceHoldMs(0), SILENCE_HOLD_MIN_MS);
  assert.equal(clampSilenceHoldMs(-1), SILENCE_HOLD_MIN_MS);
  assert.equal(clampSilenceHoldMs(999_999), SILENCE_HOLD_MAX_MS);
  assert.equal(clampSilenceHoldMs(2_500), 2_500);
  // A settings file with no stored key arrives here as `undefined`; falling
  // through to NaN would render an empty slider and post one back.
  assert.equal(clampSilenceHoldMs(Number.NaN), SILENCE_HOLD_DEFAULT_MS);
  assert.equal(clampSilenceHoldMs(undefined), SILENCE_HOLD_DEFAULT_MS);
});

test("the hold is shown in seconds, in the plainest form it has", () => {
  assert.equal(silenceHoldLabel(SILENCE_HOLD_MIN_MS), "0.3s");
  assert.equal(silenceHoldLabel(SILENCE_HOLD_DEFAULT_MS), "0.8s");
  assert.equal(silenceHoldLabel(2_500), "2.5s");
  assert.equal(silenceHoldLabel(SILENCE_HOLD_MAX_MS), "10s");
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
