import type { AmbientVoiceStatusReport } from "./ambientVoiceApi";

/**
 * Whether the webview should be holding a microphone open right now.
 *
 * This is deliberately a pure function with every input named, because it
 * encodes the feature's first acceptance criterion: with the flag off, no
 * `getUserMedia` call is ever made. Anything that could reintroduce a mic
 * acquisition has to come through here, and every gate has a test.
 */
export type AmbientCaptureInputs = {
  /** The `ambientVoice` preview flag for this user. */
  featureEnabled: boolean;
  /** This window owns the audio session (the huddle room window does not). */
  ownsAudioSession: boolean;
  /** A huddle is running; it owns the microphone. */
  huddleActive: boolean;
  /** Latest native status report, or `null` before the first one arrives. */
  report: AmbientVoiceStatusReport | null;
};

export function shouldCaptureAmbientAudio({
  featureEnabled,
  ownsAudioSession,
  huddleActive,
  report,
}: AmbientCaptureInputs): boolean {
  // Fail closed on every unknown: no flag, no window ownership, no report, no
  // microphone. A missing report means the native side has not told us it is
  // running, and guessing "probably fine" here is exactly the mistake that
  // would leave a mic open.
  if (!featureEnabled) return false;
  if (!ownsAudioSession) return false;
  if (huddleActive) return false;
  if (!report) return false;
  if (!report.enabled) return false;
  if (report.suspendedByHuddle) return false;
  // Muted is a user-visible promise that the microphone is shut, not merely
  // that the audio is discarded further down. Release the device.
  if (report.muted) return false;
  // The native worker is the only consumer of these frames. If it is not
  // running there is nothing to push to, and holding the device open would
  // light the OS indicator for no reason.
  return report.capturing;
}

/**
 * Whether the reply watcher should be subscribed, and to which channel.
 *
 * Separate from capture on purpose: replies are still worth speaking while the
 * microphone is muted is *not* true — mute means quiet in both directions — but
 * a suspended-for-huddle session should not be speaking over the huddle either.
 */
export function ambientReplyChannel(
  featureEnabled: boolean,
  report: AmbientVoiceStatusReport | null,
): string | null {
  if (!featureEnabled) return null;
  if (!report?.enabled) return null;
  if (report.muted || report.suspendedByHuddle) return null;
  return report.destinationChannelId;
}

/**
 * Poll interval for `check_ambient_hotstart`, matching the huddle hot-start
 * cadence. The wake-word model downloads on demand, so the first enable
 * usually cannot start a session and something has to notice when it can.
 */
export const AMBIENT_HOTSTART_INTERVAL_MS = 3_000;

/** Sent when a device that was working disappears mid-session. */
export const AMBIENT_CAPTURE_LOST_MESSAGE =
  "The microphone stopped — reconnect it or choose another one in settings";

/**
 * What to tell the native side when the webview cannot hold a microphone.
 *
 * The indicator and the settings section print this verbatim, so these are
 * sentences naming what the user can do rather than the `DOMException` name,
 * which says nothing to anyone. The exception itself still goes to the console:
 * this string is for the person, not for the bug report.
 */
export function ambientCaptureErrorMessage(error: unknown): string {
  switch (error instanceof Error ? error.name : "") {
    case "NotAllowedError":
    case "SecurityError":
      return "Microphone access was refused — allow it for Buzz in your system settings";
    case "NotFoundError":
    case "OverconstrainedError":
      return "The microphone chosen for ambient voice is not available — choose another one in settings";
    case "NotReadableError":
    case "AbortError":
      return "The microphone could not be opened — another application may be using it";
    default:
      return "The microphone could not be opened for ambient voice";
  }
}
