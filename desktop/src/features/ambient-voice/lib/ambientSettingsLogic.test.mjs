import assert from "node:assert/strict";
import test from "node:test";

import {
  ambientModelRows,
  ambientSaveBlock,
  ambientSpeechHealthLines,
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
import {
  ambientReportLabel,
  speechBackendFailureLabel,
} from "./ambientVoiceApi.ts";
import {
  ambientReport,
  failingSpeechServerReport,
} from "./ambientVoiceTestDom.mjs";

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

/** A saveable form, which each case below breaks in exactly one way. */
function form(overrides = {}) {
  return {
    wakeWord: "hey hermes",
    wakeWordCheck: valid,
    stopPhrase: "",
    stopPhraseCheck: null,
    agentPubkey: AGENT,
    loadError: null,
    ...overrides,
  };
}

test("an unchecked wake word is never saveable", () => {
  // Fail closed: no answer from the validator is not a pass. Persisting an
  // un-encodable phrase would hand the C library input that kills the process.
  const block = ambientSaveBlock(form({ wakeWordCheck: null }));
  assert.equal(block.reason, "wake_word");
});

test("an invalid wake word blocks saving and shows the validator's own words", () => {
  const block = ambientSaveBlock(
    form({
      wakeWord: "the",
      wakeWordCheck: {
        valid: false,
        message: "A one-word wake phrase needs at least 8 letters",
        tokens: null,
        checkedAgainstModel: false,
      },
    }),
  );
  assert.equal(block.reason, "wake_word");
  assert.match(block.message, /at least 8 letters/);
});

test("an empty wake word and a missing agent are each reported", () => {
  assert.equal(ambientSaveBlock(form({ wakeWord: "  " })).reason, "wake_word");
  assert.equal(ambientSaveBlock(form({ agentPubkey: null })).reason, "agent");
});

test("a valid form with an agent is saveable", () => {
  assert.equal(ambientSaveBlock(form()), null);
});

test("a settings file that failed to load blocks every write", () => {
  const block = ambientSaveBlock(form({ loadError: "not valid JSON" }));
  assert.equal(block.reason, "load_error");
  assert.match(block.message, /not valid JSON/);
});

test("no stop phrase is a complete form, not an unfinished one", () => {
  // Empty is how the second keyword is switched off, and it is what every
  // install has until someone types one. It must never be waited on, or the
  // default configuration would be permanently unsaveable.
  assert.equal(ambientSaveBlock(form({ stopPhrase: "" })), null);
  assert.equal(ambientSaveBlock(form({ stopPhrase: "   " })), null);
});

test("an unchecked stop phrase is never saveable", () => {
  // The same fail-closed rule as the wake word, because it is armed on the
  // same spotter: a phrase saved without an answer is a phrase that can stop
  // the session from starting at all.
  const block = ambientSaveBlock(form({ stopPhrase: "buzz stop" }));
  assert.equal(block.reason, "stop_phrase");
  assert.match(block.message, /Checking/);
});

test("a stop phrase the model cannot encode blocks saving", () => {
  // The message is the native validator's, so what the user reads is the
  // reason the save door would give — not a second wording of it.
  const block = ambientSaveBlock(
    form({
      stopPhrase: "buzz stop.",
      stopPhraseCheck: {
        valid: false,
        message:
          "Stop phrase: The wake-word model only understands unaccented English letters. It cannot hear: .",
        tokens: null,
        checkedAgainstModel: true,
      },
    }),
  );
  assert.equal(block.reason, "stop_phrase");
  assert.match(block.message, /cannot hear/);
});

test("a checked stop phrase saves alongside everything else", () => {
  assert.equal(
    ambientSaveBlock(
      form({ stopPhrase: "that's all", stopPhraseCheck: valid }),
    ),
    null,
  );
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

// ── A speech server that has stopped answering ───────────────────────────────

test("a role that runs on this computer is never reported as a failing server", () => {
  // The default, and the shape of every settings file that has never named a
  // server. An alarm about a server that does not exist would be the same
  // class of harm as the silence this replaces.
  assert.equal(speechBackendFailureLabel(ambientReport().speechBackends), null);
  assert.deepEqual(ambientSpeechHealthLines(ambientReport()), []);
  assert.equal(
    ambientReportLabel(ambientReport()),
    "Listening for the wake word",
  );
});

test("a failing speech server replaces the pill's claim to be listening", () => {
  // The shipped fault: speech-to-text on a server that has been refused for
  // an hour, the utterance quietly falling back to this computer, and the
  // pill still saying "Listening for the wake word".
  const report = failingSpeechServerReport();
  assert.equal(
    ambientReportLabel(report),
    "Speech-to-text server is not answering",
  );
});

test("both servers failing is named as both", () => {
  const report = failingSpeechServerReport({
    speechBackends: {
      stt: {
        configured: true,
        failing: true,
        consecutiveFailures: 1,
        lastError: "connection refused",
      },
      tts: {
        configured: true,
        failing: true,
        consecutiveFailures: 1,
        lastError: "connection refused",
      },
    },
  });
  assert.equal(ambientReportLabel(report), "Speech servers are not answering");
});

test("a more specific status keeps the pill, a failing server does not bury it", () => {
  // "Transcribing…" and an error already on screen are facts about right now;
  // replacing either with the server's health would be the same trade this
  // exists to undo. Only the state that claims all is well is replaced.
  for (const status of [
    { state: "transcribing" },
    { state: "speaking" },
    { state: "error", detail: "the wake-word model is incomplete" },
    { state: "muted" },
  ]) {
    const report = failingSpeechServerReport({ status });
    assert.notEqual(
      ambientReportLabel(report),
      "Speech-to-text server is not answering",
      `${status.state} was buried under the server health`,
    );
  }
  // And a deaf microphone still outranks it: it is the more urgent of the two,
  // and the one the user can act on.
  const deaf = failingSpeechServerReport({ audioStale: true });
  assert.equal(
    ambientReportLabel(deaf),
    "No audio arriving from the microphone",
  );
});

test("the settings section says which server, how long, and what it said", () => {
  // The pill has room for the headline only. This is where someone who read it
  // finds out which server and why.
  assert.deepEqual(ambientSpeechHealthLines(failingSpeechServerReport()), [
    "Speech-to-text server is not answering (3 attempts): speech server did not answer: connection refused",
  ]);
});

test("a server that came back stops being complained about", () => {
  // This answers "is it failing now". A line that stayed after recovery would
  // become furniture the user learns to ignore.
  const recovered = failingSpeechServerReport({
    speechBackends: {
      stt: {
        configured: true,
        failing: false,
        consecutiveFailures: 0,
        lastError: null,
      },
      tts: {
        configured: false,
        failing: false,
        consecutiveFailures: 0,
        lastError: null,
      },
    },
  });
  assert.deepEqual(ambientSpeechHealthLines(recovered), []);
  assert.equal(ambientReportLabel(recovered), "Listening for the wake word");
});
