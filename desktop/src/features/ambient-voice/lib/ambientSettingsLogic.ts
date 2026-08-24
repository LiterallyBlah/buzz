import {
  SPEECH_ROLE_NAMES,
  type AmbientModelStatus,
  type AmbientVoiceSettings,
  type AmbientVoiceStatusReport,
  type ModelStatus,
  type WakeBinding,
  type WakeWordCheck,
} from "./ambientVoiceApi";
import { truncatePubkey } from "@/shared/lib/pubkey";

/** Agent as offered by the picker, from either source. */
export type AmbientAgentOption = {
  pubkey: string;
  name: string;
  /** Where the entry came from — managed agents, or a channel `bot` member. */
  source: "managed" | "channel";
  /** Managed-agent process status, when known. */
  status?: string;
};

export type ManagedAgentSummary = {
  pubkey: string;
  name: string;
  status: string;
};

export type ChannelBotMember = {
  pubkey: string;
  name?: string | null;
};

/**
 * Merge the two authoritative agent sources into one picker list.
 *
 * Managed agents are the ones this desktop can start; channel `bot` members
 * cover agents that run elsewhere (the Hermes gateway is one). A pubkey in
 * both is offered once, as the managed entry, because that entry can also
 * report whether the process is running.
 */
export function mergeAgentOptions(
  managed: ManagedAgentSummary[],
  channelBots: ChannelBotMember[],
): AmbientAgentOption[] {
  const options: AmbientAgentOption[] = managed.map((agent) => ({
    pubkey: agent.pubkey,
    name: agent.name,
    source: "managed",
    status: agent.status,
  }));
  const seen = new Set(options.map((option) => option.pubkey));
  for (const bot of channelBots) {
    if (seen.has(bot.pubkey)) continue;
    seen.add(bot.pubkey);
    options.push({
      pubkey: bot.pubkey,
      name: bot.name?.trim() || truncatePubkey(bot.pubkey),
      source: "channel",
    });
  }
  return options.sort((a, b) =>
    a.name.localeCompare(b.name, undefined, { sensitivity: "base" }),
  );
}

/** Why the Save button is disabled, or `null` when it is not. */
export type AmbientSaveBlock =
  | { reason: "wake_word"; message: string }
  | { reason: "agent"; message: string }
  | { reason: "load_error"; message: string }
  | null;

/**
 * Whether the current form can be saved, and if not, what to tell the user.
 *
 * A wake word that the model cannot encode must never be persisted: the
 * settings file is read at boot and handed to a C library that terminates the
 * process on input it cannot tokenise. The check is the same one the native
 * side runs, so a phrase accepted here cannot kill the app later.
 */
export function ambientSaveBlock(
  wakeWord: string,
  agentPubkey: string | null,
  check: WakeWordCheck | null,
  loadError: string | null,
): AmbientSaveBlock {
  if (loadError) {
    return {
      reason: "load_error",
      message: `Settings cannot be saved until the existing file is fixed: ${loadError}`,
    };
  }
  if (wakeWord.trim().length === 0) {
    return { reason: "wake_word", message: "Choose a wake word." };
  }
  // Fail closed while a check is in flight: no answer is not a pass.
  if (!check) {
    return { reason: "wake_word", message: "Checking the wake word…" };
  }
  if (!check.valid) {
    return {
      reason: "wake_word",
      message: check.message ?? "That wake word cannot be used.",
    };
  }
  if (!agentPubkey) {
    return { reason: "agent", message: "Choose an agent to talk to." };
  }
  return null;
}

/** Build the settings payload from the form, preserving unrelated fields. */
export function withPrimaryBinding(
  settings: AmbientVoiceSettings,
  binding: WakeBinding,
): AmbientVoiceSettings {
  // Replace the first binding and keep any extras a later milestone stored, so
  // editing the M1 row never silently deletes M2 configuration.
  const rest = settings.wakeBindings.slice(1);
  return { ...settings, wakeBindings: [binding, ...rest] };
}

// ── The pause that ends what you are saying ──────────────────────────────────
//
// The bounds mirror `MIN_SILENCE_HOLD_MS` / `MAX_SILENCE_HOLD_MS` /
// `DEFAULT_SILENCE_HOLD_MS` in `ambient_voice::utterance`, which clamps to the
// same range on load. Duplicated rather than fetched because the slider has to
// render before any native call answers, and pinned from the producing side by
// `a_hold_no_slider_could_produce_is_clamped_on_load_and_refused_on_save`.

export const SILENCE_HOLD_MIN_MS = 300;
export const SILENCE_HOLD_MAX_MS = 10_000;
export const SILENCE_HOLD_DEFAULT_MS = 800;

/** 100 ms steps: fine enough to tune by ear, coarse enough to land on a value. */
export const SILENCE_HOLD_STEP_MS = 100;

/**
 * Hold a slider value to what the native side will accept.
 *
 * A save outside the range is refused there, and the refusal reaches the user
 * as a red banner over a setting they moved with a mouse — so it is clamped
 * here instead. A value that is not a number at all (an empty or half-typed
 * field) falls back to the default rather than to `NaN`.
 */
export function clampSilenceHoldMs(ms: number): number {
  if (!Number.isFinite(ms)) return SILENCE_HOLD_DEFAULT_MS;
  return Math.min(
    SILENCE_HOLD_MAX_MS,
    Math.max(SILENCE_HOLD_MIN_MS, Math.round(ms)),
  );
}

/** What the row shows beside the slider: "0.3s", "0.8s", "10s". */
export function silenceHoldLabel(ms: number): string {
  const seconds = clampSilenceHoldMs(ms) / 1000;
  return `${seconds.toFixed(1).replace(/\.0$/, "")}s`;
}

/** One local model, as listed in the settings section. */
export type AmbientModelRow = {
  /** Field on `AmbientModelStatus`; also the row's React key. */
  key: keyof AmbientModelStatus;
  /** What the model does, in the user's terms rather than the engine's. */
  label: string;
  status: ModelStatus;
};

/**
 * Every model an ambient session needs, in the order it is needed.
 *
 * The section used to list the wake-word download alone, which is the one
 * model whose absence is *visible* — no wake word, no session, and the status
 * line says so. The other two fail silently: without speech-to-text nothing is
 * ever transcribed, and without the voice the agent's replies simply are not
 * spoken (`start_ambient_tts` treats a missing model as non-fatal on purpose).
 * A user hitting either of those sees a working indicator and no audio, with
 * nothing anywhere to explain it.
 */
export function ambientModelRows(
  models: AmbientModelStatus | null,
): AmbientModelRow[] {
  if (!models) return [];
  return [
    { key: "kws", label: "Wake word", status: models.kws },
    { key: "stt", label: "Speech to text", status: models.stt },
    { key: "tts", label: "Voice", status: models.tts },
  ];
}

/**
 * One line describing the audio actually moving through the session.
 *
 * The settings section is where someone looks when the app is not hearing them,
 * and until now everything it could say was about configuration. These are the
 * two counts that decide where a deaf session broke: what this webview pushed,
 * and what the native worker received. `null` when there is no session to
 * describe — an empty row would be its own small lie.
 */
export function ambientAudioFlowLine(
  report: AmbientVoiceStatusReport | null,
): string | null {
  if (!report?.capturing) return null;
  const pushed = report.webviewCapture
    ? `${report.webviewCapture.batchesPushed} sent by this window`
    : "nothing reported by this window yet";
  const received = `${report.audioBatchesReceived} received`;
  if (!report.audioStale) return `Audio: ${received}, ${pushed}`;
  const quietFor =
    report.msSinceLastAudio === null
      ? ""
      : ` for ${Math.round(report.msSinceLastAudio / 1000)}s`;
  const pipeline =
    report.webviewCapture?.captureReady === false
      ? "; this window has no microphone open"
      : "";
  return `Audio: none received${quietFor} (${received}, ${pushed})${pipeline}`;
}

/**
 * One line per speech server that is failing, with what it said.
 *
 * The pill has room for the headline only; this is where someone who has read
 * it comes to find out which server and why. Empty when both roles are fine or
 * run on this computer — a permanently present "servers: OK" row would be
 * furniture, and this section already lists the addresses.
 */
export function ambientSpeechHealthLines(
  report: AmbientVoiceStatusReport | null,
): string[] {
  const health = report?.speechBackends;
  if (!health) return [];
  return (["stt", "tts"] as const)
    .filter((role) => health[role].failing)
    .map((role) => {
      const detail = health[role].lastError;
      const attempts = health[role].consecutiveFailures;
      const tried =
        attempts > 1 ? ` (${attempts} attempts)` : attempts === 1 ? "" : "";
      return `${SPEECH_ROLE_NAMES[role]} server is not answering${tried}${
        detail ? `: ${detail}` : ""
      }`;
    });
}

/**
 * One line naming the build, and whether the previous launch ran another one.
 *
 * Both reports of a deaf wake word were the first start after an in-app update.
 * Nothing in the process says "the updater started me" (see
 * `ambient_voice::launch`), so this says what is actually known and nothing
 * more — the version, and whether it changed since the last launch.
 */
export function ambientLaunchLine(
  report: AmbientVoiceStatusReport | null,
): string | null {
  const launch = report?.launch;
  if (!launch) return null;
  if (!launch.firstLaunchAfterUpdate) return `Launch: ${launch.version}`;
  return `Launch: ${launch.version}, first start after ${launch.previousVersion ?? "another build"}`;
}

/** Progress copy for a local model download. */
export function modelStatusLabel(status: ModelStatus): string {
  if (status === "not_downloaded") return "Not downloaded";
  if (status === "ready") return "Ready";
  if ("downloading" in status) {
    return `Downloading… ${status.downloading.progress_percent}%`;
  }
  return `Failed: ${status.error}`;
}
