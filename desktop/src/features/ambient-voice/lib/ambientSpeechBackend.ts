import type {
  AmbientVoiceSettings,
  SpeechBackendSettings,
  SpeechEndpointCheck,
} from "./ambientVoiceApi";

/**
 * The per-role speech backend choice, as the settings section presents it.
 *
 * Copy and rules live here rather than in the component so both can be tested
 * without a DOM, and so the one thing the user has to understand — what leaves
 * this computer, and when — is written in one place.
 */

/** The two roles a user can move to a server, in the order they are shown. */
export const SPEECH_ROLES = ["stt", "tts"] as const;
export type SpeechRole = (typeof SPEECH_ROLES)[number];

/**
 * Shown in the empty URL field.
 *
 * A placeholder, not an address: this feature is meant to be upstreamable, and
 * a real host baked into the app would be someone else's network.
 */
export const SPEECH_ENDPOINT_PLACEHOLDER = "http://your-server:30120";

/** What a role that has not been pointed anywhere looks like. */
export const LOCAL_SPEECH_BACKEND: SpeechBackendSettings = {
  backend: "local",
  endpointUrl: null,
};

export type SpeechRoleCopy = {
  label: string;
  description: string;
  /** What is actually sent, named with the path it is sent to. */
  hint: string;
};

export const SPEECH_ROLE_COPY: Record<SpeechRole, SpeechRoleCopy> = {
  stt: {
    label: "Speech to text",
    description: "Where what you say is turned into a message.",
    hint: "Each thing you say after the wake word is sent to /v1/audio/transcriptions on this server. The wake word itself is always heard on this computer, and nothing is sent until it fires.",
  },
  tts: {
    label: "Text to speech",
    description: "Where the agent's replies are turned into a voice.",
    hint: "Each reply is sent to /v1/audio/speech on this server and the audio it returns is played here.",
  },
};

/** The backend picker's options. */
export const SPEECH_BACKEND_OPTIONS = [
  { value: "local", label: "This computer" },
  { value: "http", label: "A server" },
] as const;

export function speechBackendLabel(backend: SpeechBackendSettings): string {
  return backend.backend === "http" ? "A server" : "This computer";
}

/**
 * Replace one role's backend, leaving everything else in the settings alone.
 *
 * The settings card posts the whole object back on every change, so a helper
 * that rebuilt it from parts would be how the other role — or a wake binding —
 * quietly went missing.
 */
export function withSpeechBackend(
  settings: AmbientVoiceSettings,
  role: SpeechRole,
  backend: SpeechBackendSettings,
): AmbientVoiceSettings {
  return { ...settings, [role]: backend };
}

/**
 * What this role is actually doing right now, when that differs from what the
 * picker says. `null` when the picker already tells the whole story.
 *
 * The native side treats `http` with a blank URL as "not configured yet" and
 * goes on running the role locally, because the field is written as the user
 * types and a session that refused to start on a half-typed URL would be worse
 * than one that waits. That is a real difference between what is selected and
 * what is happening, so it is said out loud.
 */
export function speechBackendNotice(
  backend: SpeechBackendSettings,
): string | null {
  if (backend.backend !== "http") return null;
  if ((backend.endpointUrl ?? "").trim().length > 0) return null;
  return "Add the server's address. Until then this runs on this computer.";
}

/** What a "Check" is saying, from the click to the answer. */
export type SpeechCheckState =
  | { phase: "idle" }
  | { phase: "checking" }
  | { phase: "done"; check: SpeechEndpointCheck }
  | { phase: "failed"; message: string };

export const SPEECH_CHECK_IDLE: SpeechCheckState = { phase: "idle" };

/**
 * One line describing the last check, or `null` before there was one.
 *
 * The three answers are kept apart on purpose. "Not a URL" is a fault in the
 * field the user is looking at; "did not answer" is a fault somewhere else
 * entirely, and telling someone to fix their typing when their server is off
 * would send them looking in the wrong place.
 */
export function speechCheckLabel(state: SpeechCheckState): string | null {
  switch (state.phase) {
    case "idle":
      return null;
    case "checking":
      return "Checking…";
    case "failed":
      return state.message;
    case "done":
      switch (state.check.status) {
        case "ready":
          return `Answered at ${state.check.probedUrl ?? "the health path"}`;
        case "malformed":
          return state.check.detail ?? "That address cannot be used.";
        case "unreachable":
          return `No answer from ${state.check.probedUrl ?? "the server"}${
            state.check.detail ? `: ${state.check.detail}` : ""
          }`;
      }
  }
}

/** Whether the last check should read as a problem rather than an answer. */
export function speechCheckIsProblem(state: SpeechCheckState): boolean {
  if (state.phase === "failed") return true;
  return state.phase === "done" && state.check.status !== "ready";
}
