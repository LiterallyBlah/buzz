/**
 * The paced self-repair for a session that is running and hearing nothing.
 *
 * The fault it answers: after an update relaunch the native session runs, the
 * pill says "Listening for the wake word", and no audio ever reaches the
 * wake-word engine. A settings off/on fixes it every time, and what that does
 * to this webview is rebuild the microphone, the AudioContext and the worklet.
 *
 * What is pinned here is the pacing, because the failure mode of a retry loop
 * is already known in this feature: an unpaced retry against a device that
 * cannot be opened rebuilt the native session — two ONNX model loads — on every
 * three-second poll (`CAPTURE_ERROR_BACKOFF`, commit 227dcee2). A watchdog on
 * this side must not reintroduce that shape from the other end.
 */

import assert from "node:assert/strict";
import test from "node:test";

import {
  ambientRebuildAttempted,
  ambientRebuildBackoffMs,
  shouldRebuildAmbientCapture,
  AMBIENT_CAPTURE_REBUILD_LIMIT,
  AMBIENT_REBUILD_START,
} from "./ambientAudioWatchdog.ts";
import { ambientReport, deafAmbientReport } from "./ambientVoiceTestDom.mjs";

const NOW = 1_700_000_000_000;

const inputs = (overrides = {}) => ({
  report: deafAmbientReport(),
  captureReady: true,
  state: AMBIENT_REBUILD_START,
  nowMs: NOW,
  ...overrides,
});

test("a session that is being fed nothing is rebuilt at once", async () => {
  // The native side has already withheld the stale flag for five seconds, so by
  // the time it is set the deafness is confirmed. Waiting longer only keeps the
  // user in front of a pill that is telling them the truth and doing nothing.
  assert.equal(shouldRebuildAmbientCapture(inputs()), true);
});

test("a healthy session is never rebuilt", async () => {
  // The failure mode that matters: a watchdog that fires on a working session
  // would drop the microphone under a user mid-sentence.
  assert.equal(
    shouldRebuildAmbientCapture(inputs({ report: ambientReport() })),
    false,
  );
});

test("nothing is rebuilt without a session to rebuild for", async () => {
  assert.equal(shouldRebuildAmbientCapture(inputs({ report: null })), false);
  assert.equal(
    shouldRebuildAmbientCapture(
      inputs({ report: deafAmbientReport({ capturing: false }) }),
    ),
    false,
  );
});

test("a capture pipeline that was never built is left alone", async () => {
  // `captureReady` false means `getUserMedia` or the worklet setup is still in
  // flight — a permission prompt is the ordinary case. A second acquisition
  // there fights the prompt instead of fixing anything, and the failure path
  // for a setup that genuinely fails already exists (it reports a capture
  // error, which stops the session).
  assert.equal(
    shouldRebuildAmbientCapture(inputs({ captureReady: false })),
    false,
  );
});

test("a second rebuild waits, and the third waits longer", async () => {
  // One rebuild that did not help is not evidence the next one will, so the
  // gap grows. Without this the five-second flow tick would rebuild the
  // microphone twelve times a minute for as long as the fault lasted.
  const afterFirst = ambientRebuildAttempted(AMBIENT_REBUILD_START, NOW);
  assert.deepEqual(afterFirst, { attempts: 1, lastAttemptAtMs: NOW });

  assert.equal(
    shouldRebuildAmbientCapture(
      inputs({ state: afterFirst, nowMs: NOW + 5_000 }),
    ),
    false,
  );
  assert.equal(
    shouldRebuildAmbientCapture(
      inputs({
        state: afterFirst,
        nowMs: NOW + ambientRebuildBackoffMs(1),
      }),
    ),
    true,
  );

  const afterSecond = ambientRebuildAttempted(
    afterFirst,
    NOW + ambientRebuildBackoffMs(1),
  );
  assert.equal(
    shouldRebuildAmbientCapture(
      inputs({
        state: afterSecond,
        nowMs: afterSecond.lastAttemptAtMs + ambientRebuildBackoffMs(1),
      }),
    ),
    false,
    "the third rebuild reused the second's shorter wait",
  );
  assert.equal(
    shouldRebuildAmbientCapture(
      inputs({
        state: afterSecond,
        nowMs: afterSecond.lastAttemptAtMs + ambientRebuildBackoffMs(2),
      }),
    ),
    true,
  );
});

test("the attempts are capped for the session, however long it stays deaf", async () => {
  // The cap is what makes this bounded rather than a loop: a microphone that is
  // genuinely gone costs three re-acquisitions, not one every tick until the
  // app is closed. A new session, or a device chosen in settings, starts the
  // count again — that reset lives in the provider.
  let state = AMBIENT_REBUILD_START;
  let nowMs = NOW;
  let rebuilds = 0;
  for (let tick = 0; tick < 100; tick += 1) {
    if (shouldRebuildAmbientCapture(inputs({ state, nowMs }))) {
      state = ambientRebuildAttempted(state, nowMs);
      rebuilds += 1;
    }
    nowMs += 5_000;
  }
  assert.equal(rebuilds, AMBIENT_CAPTURE_REBUILD_LIMIT);
  assert.equal(state.attempts, AMBIENT_CAPTURE_REBUILD_LIMIT);
});

test("audio arriving again is what clears the count", async () => {
  // The provider resets on a report that is no longer stale; the state this
  // produces has to be the same one a fresh session starts from, or the
  // allowance would silently shrink over a long run.
  const spent = {
    attempts: AMBIENT_CAPTURE_REBUILD_LIMIT,
    lastAttemptAtMs: NOW,
  };
  assert.equal(shouldRebuildAmbientCapture(inputs({ state: spent })), false);
  assert.equal(
    shouldRebuildAmbientCapture(
      inputs({ state: AMBIENT_REBUILD_START, nowMs: NOW + 60_000 }),
    ),
    true,
  );
});
