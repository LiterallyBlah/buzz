/**
 * The per-role speech backend rules, and the fixtures they are read against.
 *
 * Every payload below is the **exact** serialisation the native side produces,
 * pinned from the producing side by
 * `an_http_backend_and_its_url_survive_a_save_and_load_verbatim` and
 * `the_check_result_serialises_in_the_shape_the_frontend_parses` in
 * `ambient_voice/settings_tests.rs` and `ambient_voice/speech_http_tests.rs`.
 * This feature has already shipped a frontend written against an invented
 * shape once — every model row rendered "undefined" and nothing failed — so
 * fixtures here are copied from those assertions rather than imagined.
 */

import assert from "node:assert/strict";
import test from "node:test";

import {
  speechBackendLabel,
  speechBackendNotice,
  speechCheckIsProblem,
  speechCheckLabel,
  withSpeechBackend,
  LOCAL_SPEECH_BACKEND,
  SPEECH_ENDPOINT_PLACEHOLDER,
  SPEECH_ROLES,
  SPEECH_ROLE_COPY,
} from "./ambientSpeechBackend.ts";

/** A settings object as `get_ambient_voice_settings` returns it. */
const SETTINGS = {
  version: 1,
  enabled: true,
  muted: false,
  wakeBindings: [
    { wakeWord: "hey hermes", agentPubkey: "a".repeat(64), destination: null },
  ],
  stt: { backend: "local", endpointUrl: null },
  tts: { backend: "local", endpointUrl: null },
  inputDeviceId: null,
  outputDevice: null,
  indicatorPosition: null,
};

test("a role is repointed without disturbing the other one", () => {
  // The card posts the whole settings object back on every change, so a helper
  // that rebuilt it from parts is how the other role — or a wake binding —
  // would quietly go missing.
  const next = withSpeechBackend(SETTINGS, "stt", {
    backend: "http",
    endpointUrl: "http://speech.example:30120",
  });
  assert.deepEqual(next.stt, {
    backend: "http",
    endpointUrl: "http://speech.example:30120",
  });
  assert.deepEqual(next.tts, SETTINGS.tts);
  assert.deepEqual(next.wakeBindings, SETTINGS.wakeBindings);
  assert.equal(next.enabled, true);
  // And the original is untouched: the card holds it in React state.
  assert.deepEqual(SETTINGS.stt, { backend: "local", endpointUrl: null });
});

test("a server chosen with no address says what is actually happening", () => {
  // The native side treats `http` with a blank URL as "not configured yet" and
  // goes on running the role locally (`SpeechBackendSettings::http_base_url`).
  // That is a real gap between what the picker says and what is happening, and
  // the user is the only one who can close it.
  assert.equal(
    speechBackendNotice({ backend: "http", endpointUrl: null }),
    "Add the server's address. Until then this runs on this computer.",
  );
  assert.equal(
    speechBackendNotice({ backend: "http", endpointUrl: "   " }),
    "Add the server's address. Until then this runs on this computer.",
  );
  assert.equal(
    speechBackendNotice({
      backend: "http",
      endpointUrl: "http://speech.example:30120",
    }),
    null,
  );
  // A URL remembered beside a local choice is not a discrepancy: nothing is
  // being sent anywhere.
  assert.equal(
    speechBackendNotice({
      backend: "local",
      endpointUrl: "http://speech.example:30120",
    }),
    null,
  );
});

test("the three check answers stay apart", () => {
  // "Not a URL" is a fault in the field the user is looking at. "Did not
  // answer" is a fault somewhere else entirely, and telling someone to fix
  // their typing when their server is switched off sends them looking in the
  // wrong place.
  assert.equal(speechCheckLabel({ phase: "idle" }), null);
  assert.equal(speechCheckLabel({ phase: "checking" }), "Checking…");

  const ready = {
    phase: "done",
    check: {
      status: "ready",
      detail: null,
      probedUrl: "http://speech.example:30120/v1/health/ready",
    },
  };
  assert.equal(
    speechCheckLabel(ready),
    "Answered at http://speech.example:30120/v1/health/ready",
  );
  assert.equal(speechCheckIsProblem(ready), false);

  const malformed = {
    phase: "done",
    check: {
      status: "malformed",
      detail:
        "The address must start with http:// or https://, not speech.example://",
      probedUrl: null,
    },
  };
  assert.equal(
    speechCheckLabel(malformed),
    "The address must start with http:// or https://, not speech.example://",
  );
  assert.equal(speechCheckIsProblem(malformed), true);

  const unreachable = {
    phase: "done",
    check: {
      status: "unreachable",
      detail: "The server answered HTTP 404 at its health path.",
      probedUrl: "http://speech.example:30120/v1/health/ready",
    },
  };
  assert.equal(
    speechCheckLabel(unreachable),
    "No answer from http://speech.example:30120/v1/health/ready: The server answered HTTP 404 at its health path.",
  );
  assert.equal(speechCheckIsProblem(unreachable), true);

  // The command itself failing is not the same as the server failing, and it
  // must not read as "your address is fine".
  const failed = {
    phase: "failed",
    message: "The server could not be checked.",
  };
  assert.equal(speechCheckLabel(failed), "The server could not be checked.");
  assert.equal(speechCheckIsProblem(failed), true);
});

test("both roles are offered, and the local default is what a fresh file holds", () => {
  assert.deepEqual([...SPEECH_ROLES], ["stt", "tts"]);
  assert.deepEqual(LOCAL_SPEECH_BACKEND, SETTINGS.stt);
  assert.equal(speechBackendLabel(SETTINGS.stt), "This computer");
  assert.equal(
    speechBackendLabel({ backend: "http", endpointUrl: null }),
    "A server",
  );
});

test("nothing in the copy is an address of ours", () => {
  // This feature is meant to be liftable into an upstream PR. A real host baked
  // into the placeholder or the hints would be someone else's network, and the
  // kind of thing that survives a copy-paste into a public repository.
  const copy = [
    SPEECH_ENDPOINT_PLACEHOLDER,
    ...SPEECH_ROLES.flatMap((role) => Object.values(SPEECH_ROLE_COPY[role])),
  ].join(" ");
  assert.match(SPEECH_ENDPOINT_PLACEHOLDER, /^http:\/\/your-server:\d+$/);
  assert.doesNotMatch(copy, /\b\d{1,3}(\.\d{1,3}){3}\b/, "an IP address");
  assert.doesNotMatch(copy, /\bts\.net\b/, "a tailnet host");
  // The paths are named, because "a server" is not enough to know what is
  // being sent where.
  assert.match(SPEECH_ROLE_COPY.stt.hint, /\/v1\/audio\/transcriptions/);
  assert.match(SPEECH_ROLE_COPY.tts.hint, /\/v1\/audio\/speech/);
  // And the one thing that never moves is said in the role that could move it.
  assert.match(
    SPEECH_ROLE_COPY.stt.hint,
    /wake word itself is always heard on this computer/,
  );
});
