import type {
  AmbientModelStatus,
  AmbientVoiceSettings,
  ModelStatus,
  WakeBinding,
  WakeWordCheck,
} from "./ambientVoiceApi";

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
      name: bot.name?.trim() || `${bot.pubkey.slice(0, 8)}…`,
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

/** Progress copy for a local model download. */
export function modelStatusLabel(status: ModelStatus): string {
  if (status === "not_downloaded") return "Not downloaded";
  if (status === "ready") return "Ready";
  if ("downloading" in status) {
    return `Downloading… ${status.downloading.progress_percent}%`;
  }
  return `Failed: ${status.error}`;
}
