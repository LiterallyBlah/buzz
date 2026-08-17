import type {
  AmbientVoiceSettings,
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

/** Progress copy for the on-demand wake-word model download. */
export function modelStatusLabel(
  status:
    | { status: "not_downloaded" }
    | { status: "downloading"; progress_percent: number }
    | { status: "ready" }
    | { status: "failed"; error: string },
): string {
  switch (status.status) {
    case "not_downloaded":
      return "Not downloaded";
    case "downloading":
      return `Downloading… ${status.progress_percent}%`;
    case "ready":
      return "Ready";
    case "failed":
      return `Failed: ${status.error}`;
  }
}
