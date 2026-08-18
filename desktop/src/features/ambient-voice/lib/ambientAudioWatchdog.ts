import type { AmbientVoiceStatusReport } from "./ambientVoiceApi";

/**
 * When a live session stops being fed, rebuild the capture pipeline — paced.
 *
 * The shipped fault: after an update relaunch the session runs, the pill says
 * "Listening for the wake word", and zero audio ever reaches the wake-word
 * engine. A settings off/on always fixes it, and what that toggle does to this
 * webview is rebuild the microphone, the AudioContext and the worklet. So does
 * this, without the user having to find the switch — but only against a
 * pipeline this webview believes it already built, and only a few times.
 *
 * ## Why the pacing is not optional
 *
 * The native side deliberately keeps its own retry cadence
 * (`CAPTURE_ERROR_BACKOFF`, thirty seconds): a reported capture failure stops
 * the session, and an unpaced retry rebuilt it — two ONNX model loads — on
 * every three-second hot-start poll for as long as a device stayed broken.
 * Nothing here may reintroduce that shape from the other side, so:
 *
 * * a rebuild here is **webview-only**. It never stops or starts a native
 *   session, never reports a capture failure, and so never touches the
 *   timestamp that paces the native retry.
 * * a rebuild that fails to open the device takes the *existing* path — the
 *   capture effect's `catch` reports the failure, the native side stops the
 *   session, and `capturing` goes false, which ends this watchdog's business
 *   until a session exists again.
 * * attempts are capped per session and backed off between attempts, so the
 *   worst case is three microphone re-acquisitions a minute apart rather than
 *   one per poll.
 */
export type AmbientRebuildState = {
  /** Rebuilds already made against the current session. */
  attempts: number;
  /** `Date.now()` of the last rebuild, or `null` if none has been made. */
  lastAttemptAtMs: number | null;
};

/** Nothing tried yet. Also what a success or a settings change resets to. */
export const AMBIENT_REBUILD_START: AmbientRebuildState = {
  attempts: 0,
  lastAttemptAtMs: null,
};

/**
 * Rebuilds allowed per session.
 *
 * Three is enough for the failure this exists for — the off/on toggle fixes it
 * on the first try, every time it has been reported — and few enough that a
 * genuinely dead microphone costs three re-acquisitions rather than a loop. A
 * new session (or a settings change) starts the count again, so this is not a
 * permanent give-up.
 */
export const AMBIENT_CAPTURE_REBUILD_LIMIT = 3;

/**
 * How long to wait before the nth rebuild, indexed by attempts already made.
 *
 * The first is immediate because the native side has already withheld the stale
 * flag for five seconds — by the time it is set, "no audio" is confirmed, and
 * making the user wait longer for a fix they would otherwise perform by hand is
 * not caution, it is delay. What follows is backed off: if the first rebuild
 * did not help, the second is unlikely to help sooner.
 */
export const AMBIENT_REBUILD_BACKOFF_MS = [0, 15_000, 45_000];

export function ambientRebuildBackoffMs(attempts: number): number {
  const last = AMBIENT_REBUILD_BACKOFF_MS.length - 1;
  return AMBIENT_REBUILD_BACKOFF_MS[Math.min(Math.max(attempts, 0), last)] ?? 0;
}

export type AmbientRebuildInputs = {
  /** Latest native report, or `null` before the first one arrives. */
  report: AmbientVoiceStatusReport | null;
  /**
   * Whether this webview holds a built capture pipeline. False while
   * `getUserMedia` or the worklet setup is still in flight — including while a
   * permission prompt is open, which is exactly when a second acquisition would
   * do harm rather than good.
   */
  captureReady: boolean;
  state: AmbientRebuildState;
  nowMs: number;
};

/** Whether to rebuild the capture pipeline right now. */
export function shouldRebuildAmbientCapture({
  report,
  captureReady,
  state,
  nowMs,
}: AmbientRebuildInputs): boolean {
  // Fail closed on every unknown, as `shouldCaptureAmbientAudio` does: no
  // report, no session, no evidence of deafness, nothing to fix.
  if (!report) return false;
  if (!report.capturing) return false;
  if (!report.audioStale) return false;
  // A pipeline that was never built is not a pipeline to rebuild: the setup is
  // either still in flight or it already failed and was reported, and both have
  // owners of their own.
  if (!captureReady) return false;
  if (state.attempts >= AMBIENT_CAPTURE_REBUILD_LIMIT) return false;
  if (state.lastAttemptAtMs === null) return true;
  return (
    nowMs - state.lastAttemptAtMs >= ambientRebuildBackoffMs(state.attempts)
  );
}

/** Record that a rebuild was just started. */
export function ambientRebuildAttempted(
  state: AmbientRebuildState,
  nowMs: number,
): AmbientRebuildState {
  return { attempts: state.attempts + 1, lastAttemptAtMs: nowMs };
}
