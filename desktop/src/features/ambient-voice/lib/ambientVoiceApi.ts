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

/** Mirrors `ambient_voice::AmbientVoiceStatusReport`. */
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
  loadError: string | null;
};

/** Mirrors `ambient_voice::settings::WakeBinding`. */
export type WakeBinding = {
  wakeWord: string;
  agentPubkey: string;
  /** `null` means "the DM with this agent" — the only M1 destination. */
  destination: string | null;
};

/** Mirrors `ambient_voice::settings::AmbientVoiceSettings`. */
export type AmbientVoiceSettings = {
  version: number;
  enabled: boolean;
  muted: boolean;
  wakeBindings: WakeBinding[];
  stt: { backend: "local"; endpointUrl: string | null };
  tts: { backend: "local"; endpointUrl: string | null };
  inputDeviceId: string | null;
  outputDevice: string | null;
};

export type WakeWordCheck = {
  valid: boolean;
  message: string | null;
  tokens: string[] | null;
  checkedAgainstModel: boolean;
};

export type ModelStatus =
  | { status: "not_downloaded" }
  | { status: "downloading"; progress_percent: number }
  | { status: "ready" }
  | { status: "failed"; error: string };

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

export const getAmbientModelStatus = () =>
  invoke<AmbientModelStatus>("get_ambient_model_status");

export const checkAmbientWakeWord = (wakeWord: string) =>
  invoke<WakeWordCheck>("check_ambient_wake_word", { wakeWord });

export const checkAmbientHotstart = () =>
  invoke<AmbientVoiceStatusReport>("check_ambient_hotstart");

export const ambientSpeak = (text: string) =>
  invoke<void>("ambient_speak", { text });

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
