import { invoke } from "@tauri-apps/api/core";

/**
 * Typed surface over the `ambient_voice` Tauri commands.
 *
 * Kept in the feature rather than `shared/api/tauri.ts` so the whole preview
 * feature is one directory — it is meant to be liftable into an upstream PR,
 * or deletable, without touching shared files.
 */

/** Mirrors `ambient_voice::status::AmbientStatus` (tagged enum). */
export type AmbientStatus =
  | { state: "off" }
  | { state: "suspended" }
  | { state: "muted" }
  | { state: "starting" }
  | { state: "listening" }
  | { state: "heard" }
  | { state: "capturing" }
  | { state: "transcribing" }
  | { state: "speaking" }
  | { state: "error"; detail: string };

/**
 * Mirrors `ambient_voice::settings::IndicatorPosition` — where the user parked
 * the listening pill, in CSS pixels from the top left of the window.
 */
export type AmbientIndicatorPosition = { x: number; y: number };

/** Mirrors `ambient_voice::WebviewCaptureFlow` — our own last report. */
export type AmbientWebviewCapture = {
  batchesPushed: number;
  captureReady: boolean;
};

/**
 * Mirrors `ambient_voice::speech_health::SpeechRoleHealth` — whether one
 * server-backed speech role is answering.
 *
 * `configured` is false whenever the role runs on this computer, which is the
 * default and the shape of every settings file that has never named a server;
 * nothing about a local role can be "failing". The shape is pinned from the
 * producing side by
 * `the_speech_backend_health_serialises_in_the_shape_the_frontend_parses`.
 */
export type AmbientSpeechRoleHealth = {
  configured: boolean;
  /** Its last attempt failed and it has not answered since. */
  failing: boolean;
  /** Attempts since the last success. */
  consecutiveFailures: number;
  /** The server's own words. `null` when there is nothing to explain. */
  lastError: string | null;
};

/** Mirrors `ambient_voice::speech_health::SpeechBackendHealthReport`. */
export type AmbientSpeechHealth = {
  stt: AmbientSpeechRoleHealth;
  tts: AmbientSpeechRoleHealth;
};

/** Mirrors `ambient_voice::launch::LaunchDiagnostics`. */
export type AmbientLaunchDiagnostics = {
  version: string;
  previousVersion: string | null;
  /**
   * The previous launch ran a different build. Both reports of a deaf wake word
   * were a first launch after an in-app update; the updater itself leaves
   * nothing in the process to detect, so this is the best available signal and
   * it cannot tell an updater relaunch from the user opening the new build.
   */
  firstLaunchAfterUpdate: boolean;
  args: string[];
};

/**
 * Mirrors `ambient_voice::AmbientVoiceStatusReport`.
 *
 * The shape is pinned from the producing side by
 * `the_status_report_serialises_with_the_keys_the_frontend_reads` and
 * `the_audio_diagnostics_serialise_in_the_shape_the_frontend_parses` in
 * `ambient_voice/mod_tests.rs`; those tests and this type change together.
 */
export type AmbientVoiceStatusReport = {
  enabled: boolean;
  muted: boolean;
  suspendedByHuddle: boolean;
  capturing: boolean;
  status: AmbientStatus;
  live: boolean;
  destinationChannelId: string | null;
  agentPubkey: string | null;
  wakeWord: string | null;
  inputDeviceId: string | null;
  /** `null` until the user drags the pill; the default corner applies. */
  indicatorPosition: AmbientIndicatorPosition | null;
  loadError: string | null;
  /**
   * A session is running, unmuted, and nothing has reached its worker for five
   * seconds. `capturing` only ever meant "the worker thread is alive", which is
   * why a deaf session could go on claiming to listen.
   */
  audioStale: boolean;
  /** Batches the native worker has taken off its queue this session. */
  audioBatchesReceived: number;
  /**
   * How long the native worker has been free to receive audio and received
   * none — since the last batch, or since the session started when none ever
   * arrived.
   *
   * Not wall-clock time since the last batch. The worker is one loop, and time
   * it spends inside a transcription is excluded, because during it the worker
   * is not reading its queue at all: measured naively, one utterance through a
   * speech server read as five seconds of silence and the watchdog rebuilt the
   * capture pipeline against a microphone that was working perfectly.
   */
  msSinceLastAudio: number | null;
  /** What this webview last reported pushing. `null` until it reports. */
  webviewCapture: AmbientWebviewCapture | null;
  /** `null` before native boot hydration has run. */
  launch: AmbientLaunchDiagnostics | null;
  /**
   * Whether the speech servers this session was pointed at are answering.
   *
   * Both roles fall back on their own — an utterance to the on-device
   * recogniser, a reply to silence — and that is deliberate. What is not is
   * doing it invisibly, which left the pill saying "Listening for the wake
   * word" while the server behind it had been down for an hour.
   */
  speechBackends: AmbientSpeechHealth;
};

/** Mirrors `ambient_voice::settings::WakeBinding`. */
export type WakeBinding = {
  wakeWord: string;
  agentPubkey: string;
  /** `null` means "the DM with this agent" — the only M1 destination. */
  destination: string | null;
};

/** Mirrors `ambient_voice::settings::SpeechBackend`. */
export type SpeechBackend = "local" | "http";

/**
 * Mirrors `ambient_voice::settings::SpeechBackendSettings`.
 *
 * `endpointUrl` is a **base** URL — the native side appends the API's paths to
 * it. It is kept while the role runs locally too, so switching back and forth
 * does not cost the user what they typed.
 */
export type SpeechBackendSettings = {
  backend: SpeechBackend;
  endpointUrl: string | null;
};

/**
 * Mirrors `ambient_voice::speech_http::SpeechEndpointCheck`.
 *
 * Pinned from the producing side by
 * `the_check_result_serialises_in_the_shape_the_frontend_parses` in
 * `ambient_voice/speech_http_tests.rs`; that test, this type and the fixtures
 * in `ambientSpeechBackend.test.mjs` change together.
 */
export type SpeechEndpointCheck = {
  /** `ready` — answered; `unreachable` — did not; `malformed` — not a URL. */
  status: "ready" | "malformed" | "unreachable";
  /** Shown verbatim. `null` only when the server is ready. */
  detail: string | null;
  /** What was actually probed. `null` when nothing could be derived. */
  probedUrl: string | null;
};

/** Mirrors `ambient_voice::settings::AmbientVoiceSettings`. */
export type AmbientVoiceSettings = {
  version: number;
  enabled: boolean;
  muted: boolean;
  wakeBindings: WakeBinding[];
  /** Where what the user says is turned into text. */
  stt: SpeechBackendSettings;
  /** Where the agent's replies are turned into speech. */
  tts: SpeechBackendSettings;
  /**
   * How long a pause must last before it ends what you are saying.
   *
   * Read with a serde default on the native side, so a settings file written
   * before this existed loads `SILENCE_HOLD_DEFAULT_MS` rather than zero.
   */
  silenceHoldMs: number;
  /**
   * Optional phrase that ends a capture the moment it is heard. `null` — and a
   * blank string — mean none is armed. It goes onto the same keyword spotter as
   * the wake word, so it is held to the same validation.
   */
  stopPhrase: string | null;
  inputDeviceId: string | null;
  outputDevice: string | null;
  /**
   * Written only by `setAmbientIndicatorPosition`. The native side keeps the
   * stored value over whatever a whole-object settings write carries, so a
   * settings copy fetched before a drag cannot move the pill back.
   */
  indicatorPosition: AmbientIndicatorPosition | null;
};

export type WakeWordCheck = {
  valid: boolean;
  message: string | null;
  tokens: string[] | null;
  checkedAgainstModel: boolean;
};

/**
 * One model's download state, exactly as the Rust `ModelStatus` enum
 * serialises (externally tagged, snake_case): unit variants are plain
 * strings, data-carrying variants a single-key object. `HuddleBar` parses
 * the same shape, and `ambient_model_status_serialises_the_shape_the_frontend_parses`
 * in `ambient_voice/mod_tests.rs` pins it from the producing side.
 */
export type ModelStatus =
  | "not_downloaded"
  | "ready"
  | { downloading: { progress_percent: number } }
  | { error: string };

export type AmbientModelStatus = {
  kws: ModelStatus;
  stt: ModelStatus;
  tts: ModelStatus;
};

/** Event name the native side emits on every ambient state transition. */
export const AMBIENT_STATE_CHANGED_EVENT = "ambient-voice-state-changed";

export const getAmbientVoiceSettings = () =>
  invoke<AmbientVoiceSettings>("get_ambient_voice_settings");

export const setAmbientVoiceSettings = (settings: AmbientVoiceSettings) =>
  invoke<AmbientVoiceStatusReport>("set_ambient_voice_settings", { settings });

export const setAmbientVoiceEnabled = (enabled: boolean) =>
  invoke<AmbientVoiceStatusReport>("set_ambient_voice_enabled", { enabled });

export const setAmbientVoiceMuted = (muted: boolean) =>
  invoke<AmbientVoiceStatusReport>("set_ambient_voice_muted", { muted });

export const getAmbientVoiceStatus = () =>
  invoke<AmbientVoiceStatusReport>("get_ambient_voice_status");

/** Remember where the pill was dragged to. `null` restores the default corner. */
export const setAmbientIndicatorPosition = (
  position: AmbientIndicatorPosition | null,
) =>
  invoke<AmbientVoiceStatusReport>("set_ambient_indicator_position", {
    position,
  });

export const getAmbientModelStatus = () =>
  invoke<AmbientModelStatus>("get_ambient_model_status");

export const checkAmbientWakeWord = (wakeWord: string) =>
  invoke<WakeWordCheck>("check_ambient_wake_word", { wakeWord });

/**
 * Ask whether a stop phrase can be armed, before it is saved.
 *
 * The wake word's check with the stop phrase's two extra rules: an empty
 * phrase is valid (it is how the feature is switched off), and the phrase must
 * differ from `wakeWord`. Both are the native side's, so what this reports is
 * what the save door enforces — the field used to be unchecked, and a phrase
 * the model could not encode saved cleanly and then took the whole session
 * down at arm time.
 */
export const checkAmbientStopPhrase = (stopPhrase: string, wakeWord: string) =>
  invoke<WakeWordCheck>("check_ambient_stop_phrase", { stopPhrase, wakeWord });

export const checkAmbientHotstart = () =>
  invoke<AmbientVoiceStatusReport>("check_ambient_hotstart");

/**
 * Ask whether a speech server is there.
 *
 * Answers about the address, not about the session: nothing is started,
 * stopped or reconfigured, and a URL that fails a check is still savable — the
 * server may simply be switched off.
 */
export const checkSpeechEndpoint = (url: string) =>
  invoke<SpeechEndpointCheck>("check_speech_endpoint", { url });

export const ambientSpeak = (text: string) =>
  invoke<void>("ambient_speak", { text });

/**
 * Tell the native side the webview lost the microphone.
 *
 * `getUserMedia` lives here, so a device that is refused, busy or unplugged is
 * invisible to the native worker — it would go on reporting that it is
 * listening. `message` is shown to the user verbatim.
 */
export const reportAmbientCaptureError = (message: string) =>
  invoke<AmbientVoiceStatusReport>("report_ambient_capture_error", { message });

/**
 * Tell the native side how much audio this webview believes it has sent.
 *
 * The two halves of the audio path live in different processes and the deafness
 * bug is somewhere between them: batches pushed but never received is an IPC or
 * session-lifetime fault, none pushed at all is the microphone, the worklet or
 * the AudioContext. Only the webview can count the second, so it says so on a
 * slow cadence — and that call is also what makes the native side notice that a
 * running session has gone quiet.
 */
export const reportAmbientAudioFlow = (pushed: number, captureReady: boolean) =>
  invoke<AmbientVoiceStatusReport>("report_ambient_audio_flow", {
    pushed,
    captureReady,
  });

/**
 * What the indicator and the settings section say about a whole report.
 *
 * Not `ambientStatusLabel(report.status)`: a session whose worker is alive
 * reports `listening` whatever the microphone is doing, so a webview that never
 * pushed a frame read as "Listening for the wake word" for the rest of the run.
 * The audio counters are the only thing that can tell those apart, and a pill
 * that lies about the one thing it exists to say is the harm being fixed.
 */
export function ambientReportLabel(
  report: AmbientVoiceStatusReport | null,
): string {
  if (!report) return "Not started";
  if (report.audioStale) return AMBIENT_NO_AUDIO_MESSAGE;
  // Only in place of "listening". Every other status is a specific fact about
  // what is happening right now — transcribing, speaking, an error already on
  // screen — and burying it under a server's health would be the same trade
  // this exists to undo. "Listening for the wake word" is the one that claims
  // all is well, so it is the one a failing server replaces.
  if (report.status.state === "listening") {
    const failing = speechBackendFailureLabel(report.speechBackends);
    if (failing) return failing;
  }
  return ambientStatusLabel(report.status);
}

/** Shown in place of "Listening for the wake word" when nothing arrives. */
export const AMBIENT_NO_AUDIO_MESSAGE = "No audio arriving from the microphone";

/** What each server-backed role is called, in the user's terms. */
export const SPEECH_ROLE_NAMES = {
  stt: "Speech-to-text",
  tts: "Voice",
} as const;

/**
 * One line naming the failing server, or `null` when both are fine.
 *
 * Deliberately short and deliberately not alarming: the feature still works —
 * speech-to-text falls back to this computer, and a reply that cannot be
 * synthesised is still on screen — so this says what is broken, not that
 * everything is. Both roles failing is named as both rather than as the first
 * one found, because "the server is down" and "both servers are down" are
 * different things to go and look at.
 */
export function speechBackendFailureLabel(
  health: AmbientSpeechHealth | undefined,
): string | null {
  if (!health) return null;
  const failing = (["stt", "tts"] as const).filter(
    (role) => health[role].failing,
  );
  if (failing.length === 0) return null;
  if (failing.length === 2) return "Speech servers are not answering";
  return `${SPEECH_ROLE_NAMES[failing[0]]} server is not answering`;
}

/** Human-readable label for the listening indicator. */
export function ambientStatusLabel(status: AmbientStatus): string {
  switch (status.state) {
    case "off":
      return "Off";
    case "suspended":
      return "Paused for a huddle";
    case "muted":
      return "Muted";
    case "starting":
      return "Starting…";
    case "listening":
      return "Listening for the wake word";
    case "heard":
      return "Wake word heard — go ahead";
    case "capturing":
      return "Listening…";
    case "transcribing":
      return "Transcribing…";
    case "speaking":
      return "Speaking";
    case "error":
      return status.detail;
  }
}
