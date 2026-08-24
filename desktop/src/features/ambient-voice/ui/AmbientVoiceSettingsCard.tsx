import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import * as React from "react";
import { ChevronDown, Mic, MicOff } from "lucide-react";

import {
  SettingsOptionGroup,
  SettingsOptionRow,
} from "@/features/settings/ui/SettingsOptionGroup";
import { SettingsSectionHeader } from "@/features/settings/ui/SettingsSectionHeader";
import { listManagedAgents, listRelayAgents } from "@/shared/api/tauri";
import { Button } from "@/shared/ui/button";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuRadioGroup,
  DropdownMenuRadioItem,
  DropdownMenuTrigger,
} from "@/shared/ui/dropdown-menu";
import { Input } from "@/shared/ui/input";
import { Switch } from "@/shared/ui/switch";
import {
  AMBIENT_STATE_CHANGED_EVENT,
  ambientReportLabel,
  checkAmbientStopPhrase,
  checkAmbientWakeWord,
  getAmbientModelStatus,
  getAmbientVoiceSettings,
  getAmbientVoiceStatus,
  setAmbientVoiceMuted,
  setAmbientVoiceSettings,
  type AmbientModelStatus,
  type AmbientVoiceSettings,
  type AmbientVoiceStatusReport,
  type WakeWordCheck,
} from "../lib/ambientVoiceApi";
import {
  ambientAudioFlowLine,
  ambientLaunchLine,
  ambientModelRows,
  ambientSaveBlock,
  ambientSpeechHealthLines,
  clampSilenceHoldMs,
  mergeAgentOptions,
  modelStatusLabel,
  silenceHoldLabel,
  withPrimaryBinding,
  SILENCE_HOLD_DEFAULT_MS,
  SILENCE_HOLD_MAX_MS,
  SILENCE_HOLD_MIN_MS,
  SILENCE_HOLD_STEP_MS,
  type AmbientAgentOption,
} from "../lib/ambientSettingsLogic";
import {
  withSpeechBackend,
  LOCAL_SPEECH_BACKEND,
  SPEECH_ROLES,
} from "../lib/ambientSpeechBackend";
import { useAmbientAudioDevices } from "../lib/useAmbientAudioDevices";
import { AmbientSpeechBackendRow } from "./AmbientSpeechBackendRow";

const WAKE_WORD_CHECK_DEBOUNCE_MS = 250;

/** The stop phrase is checked on the same cadence, and for the same reason. */
const STOP_PHRASE_CHECK_DEBOUNCE_MS = 250;

/**
 * Settings section for the `ambientVoice` preview feature.
 *
 * Only reachable when the flag is on — the section descriptor carries
 * `featureGate: "ambientVoice"` and `SettingsView` filters on it.
 */
export function AmbientVoiceSettingsCard() {
  const [settings, setSettings] = React.useState<AmbientVoiceSettings | null>(
    null,
  );
  const [report, setReport] = React.useState<AmbientVoiceStatusReport | null>(
    null,
  );
  const [models, setModels] = React.useState<AmbientModelStatus | null>(null);
  const [agents, setAgents] = React.useState<AmbientAgentOption[]>([]);
  const [wakeWord, setWakeWord] = React.useState("");
  const [stopPhrase, setStopPhrase] = React.useState("");
  // The slider's own position while it is being dragged. Persisting on every
  // pointer move would post a settings write — and therefore a session restart,
  // at two ONNX model loads — for each pixel, so the committed value is the one
  // the user let go of.
  const [silenceHoldMs, setSilenceHoldMs] = React.useState(
    SILENCE_HOLD_DEFAULT_MS,
  );
  const [agentPubkey, setAgentPubkey] = React.useState<string | null>(null);
  const [check, setCheck] = React.useState<WakeWordCheck | null>(null);
  const [stopPhraseCheck, setStopPhraseCheck] =
    React.useState<WakeWordCheck | null>(null);
  const [error, setError] = React.useState<string | null>(null);
  const [saving, setSaving] = React.useState(false);
  const { inputDevices, outputDevices } = useAmbientAudioDevices();

  // ── Load ─────────────────────────────────────────────────────────────────
  React.useEffect(() => {
    let disposed = false;
    void (async () => {
      try {
        const loaded = await getAmbientVoiceSettings();
        if (disposed) return;
        setSettings(loaded);
        const binding = loaded.wakeBindings[0];
        setWakeWord(binding?.wakeWord ?? "");
        setStopPhrase(loaded.stopPhrase ?? "");
        setSilenceHoldMs(clampSilenceHoldMs(loaded.silenceHoldMs));
        setAgentPubkey(binding?.agentPubkey ?? null);
      } catch (loadError) {
        if (!disposed) {
          setError(
            loadError instanceof Error
              ? loadError.message
              : "Ambient voice settings could not be loaded.",
          );
        }
      }
    })();
    return () => {
      disposed = true;
    };
  }, []);

  // ── Live native state ────────────────────────────────────────────────────
  //
  // Mute and enablement belong to the native runtime — the listening pill
  // mutes, the Experiments toggle enables — and a report fetched once at mount
  // goes stale the moment either moves, leaving this section showing a shut
  // microphone as open. The indicator already follows the same event, so this
  // is the same subscription rather than a second source of truth.
  //
  // Listener first, snapshot second: a transition that happens while the IPC
  // is in flight must not be lost behind a stale answer.
  React.useEffect(() => {
    let disposed = false;
    let unlisten: UnlistenFn | null = null;
    void listen<AmbientVoiceStatusReport>(
      AMBIENT_STATE_CHANGED_EVENT,
      (event) => {
        if (!disposed) setReport(event.payload);
      },
    )
      .then((dispose) => {
        if (disposed) {
          dispose();
          return;
        }
        unlisten = dispose;
        void getAmbientVoiceStatus()
          .then((snapshot) => {
            // Only seed; a live event that already arrived is newer.
            if (!disposed) setReport((current) => current ?? snapshot);
          })
          .catch(() => {
            /* the section stays on "Not started" until an event arrives */
          });
      })
      .catch(() => {
        /* no listener: the rows below fall back to their unknown state */
      });
    return () => {
      disposed = true;
      unlisten?.();
    };
  }, []);

  React.useEffect(() => {
    let disposed = false;
    const refresh = () => {
      void getAmbientModelStatus()
        .then((next) => {
          if (!disposed) setModels(next);
        })
        .catch(() => {
          /* the model manager is unavailable; the row simply stays blank */
        });
    };
    refresh();
    const id = window.setInterval(refresh, 2_000);
    return () => {
      disposed = true;
      window.clearInterval(id);
    };
  }, []);

  React.useEffect(() => {
    let disposed = false;
    void Promise.all([listManagedAgents(), listRelayAgents()])
      .then(([managed, relay]) => {
        if (disposed) return;
        setAgents(
          mergeAgentOptions(
            managed.map((agent) => ({
              pubkey: agent.pubkey,
              name: agent.name,
              status: agent.status,
            })),
            relay.map((agent) => ({ pubkey: agent.pubkey, name: agent.name })),
          ),
        );
      })
      .catch(() => {
        if (!disposed) setError("Agents could not be listed.");
      });
    return () => {
      disposed = true;
    };
  }, []);

  // ── Wake-word validation, debounced ──────────────────────────────────────
  //
  // Every keystroke re-checks, because a phrase the model cannot encode must
  // never reach the engine and the user should learn that while typing rather
  // than when the session refuses to start.
  React.useEffect(() => {
    setCheck(null);
    if (wakeWord.trim().length === 0) return;
    let disposed = false;
    const id = window.setTimeout(() => {
      void checkAmbientWakeWord(wakeWord)
        .then((next) => {
          if (!disposed) setCheck(next);
        })
        .catch(() => {
          if (!disposed) {
            setCheck({
              valid: false,
              message: "The wake word could not be checked.",
              tokens: null,
              checkedAgainstModel: false,
            });
          }
        });
    }, WAKE_WORD_CHECK_DEBOUNCE_MS);
    return () => {
      disposed = true;
      window.clearTimeout(id);
    };
  }, [wakeWord]);

  // ── Stop-phrase validation, debounced ────────────────────────────────────
  //
  // The same gate as the wake word, because it is armed on the same spotter: a
  // phrase the model cannot encode used to save cleanly and then take the whole
  // session down when it next started. It depends on the wake word too — the
  // two must differ — so a wake-word edit re-runs it.
  React.useEffect(() => {
    setStopPhraseCheck(null);
    if (stopPhrase.trim().length === 0) return;
    let disposed = false;
    const id = window.setTimeout(() => {
      void checkAmbientStopPhrase(stopPhrase, wakeWord)
        .then((next) => {
          if (!disposed) setStopPhraseCheck(next);
        })
        .catch(() => {
          if (!disposed) {
            setStopPhraseCheck({
              valid: false,
              message: "The stop phrase could not be checked.",
              tokens: null,
              checkedAgainstModel: false,
            });
          }
        });
    }, STOP_PHRASE_CHECK_DEBOUNCE_MS);
    return () => {
      disposed = true;
      window.clearTimeout(id);
    };
  }, [stopPhrase, wakeWord]);

  const block = ambientSaveBlock({
    wakeWord,
    wakeWordCheck: check,
    stopPhrase,
    stopPhraseCheck,
    agentPubkey,
    loadError: report?.loadError ?? null,
  });

  const persist = React.useCallback(async (next: AmbientVoiceSettings) => {
    setSaving(true);
    setError(null);
    try {
      const status = await setAmbientVoiceSettings(next);
      setSettings(next);
      setReport(status);
    } catch (saveError) {
      setError(
        saveError instanceof Error
          ? saveError.message
          : "Ambient voice settings could not be saved.",
      );
    } finally {
      setSaving(false);
    }
  }, []);

  const saveBinding = React.useCallback(() => {
    if (!settings || block || !agentPubkey) return;
    void persist(
      withPrimaryBinding(settings, {
        wakeWord: wakeWord.trim(),
        agentPubkey,
        destination: null,
      }),
    );
  }, [settings, block, agentPubkey, wakeWord, persist]);

  /** Write the slider's committed position, if it moved. */
  const saveSilenceHold = React.useCallback(() => {
    const next = clampSilenceHoldMs(silenceHoldMs);
    if (!settings || settings.silenceHoldMs === next) return;
    void persist({ ...settings, silenceHoldMs: next });
  }, [persist, settings, silenceHoldMs]);

  const saveStopPhrase = React.useCallback(async () => {
    // Blank means "no stop phrase", which the native side reads from `null`
    // as readily as from an empty string — but `null` is what an untouched
    // install has, so writing it back keeps the file identical either way.
    const trimmed = stopPhrase.trim();
    const next = trimmed.length === 0 ? null : trimmed;
    if (!settings || (settings.stopPhrase ?? null) === next) return;
    // Clearing the field is always allowed: emptying it is how the second
    // keyword is switched off, and a phrase already on disk must stay
    // removable even after it stopped being valid — a wake word edited to
    // match it, say.
    if (next === null) {
      void persist({ ...settings, stopPhrase: null });
      return;
    }
    // A phrase is checked before it is written. Asked here and not only in the
    // debounced effect because leaving the field is the commit, and someone
    // who types and tabs away inside the debounce window would otherwise have
    // a perfectly good phrase silently dropped.
    let verdict = stopPhraseCheck;
    if (!verdict) {
      verdict = await checkAmbientStopPhrase(stopPhrase, wakeWord).catch(
        (): WakeWordCheck => ({
          valid: false,
          message: "The stop phrase could not be checked.",
          tokens: null,
          checkedAgainstModel: false,
        }),
      );
      setStopPhraseCheck(verdict);
    }
    if (!verdict.valid) return;
    void persist({ ...settings, stopPhrase: next });
  }, [persist, settings, stopPhrase, stopPhraseCheck, wakeWord]);

  const selectedAgent = agents.find((agent) => agent.pubkey === agentPubkey);
  const audioFlowLine = ambientAudioFlowLine(report);
  const launchLine = ambientLaunchLine(report);
  const speechHealthLines = ambientSpeechHealthLines(report);

  return (
    <section className="min-w-0" data-testid="settings-ambient-voice">
      <SettingsSectionHeader
        title="Ambient voice"
        description={
          <>
            Say a wake word and talk to one agent, hands-free. The wake word is
            always heard on this computer, and nothing is sent anywhere until it
            fires; speech recognition and the voice run here too unless you
            point them at a server below. While this is switched on the
            microphone stays open, so your operating system will show its
            microphone indicator the whole time.
          </>
        }
      />

      {error ? (
        <p className="mb-3 text-sm text-destructive" role="alert">
          {error}
        </p>
      ) : null}

      <SettingsOptionGroup>
        <SettingsOptionRow>
          <div className="min-w-0">
            <p className="text-sm font-medium">Wake word</p>
            <p className="text-sm font-normal text-muted-foreground">
              Two words work best. Short, common words fire on unrelated speech.
            </p>
          </div>
          <Input
            aria-label="Wake word"
            className="max-w-56"
            data-testid="ambient-wake-word"
            onBlur={saveBinding}
            onChange={(event) => setWakeWord(event.target.value)}
            placeholder="hey hermes"
            value={wakeWord}
          />
        </SettingsOptionRow>

        {check && !check.valid && check.message ? (
          <p className="px-4 pb-3 text-sm text-destructive" role="alert">
            {check.message}
          </p>
        ) : null}

        <SettingsOptionRow>
          <div className="min-w-0">
            <p className="text-sm font-medium">Agent</p>
            <p className="text-sm font-normal text-muted-foreground">
              What you say arrives in this agent's DM; its replies are read
              aloud.
            </p>
          </div>
          <DropdownMenu modal={false}>
            <DropdownMenuTrigger asChild>
              <Button
                className="h-7 min-w-40 justify-between gap-1.5"
                data-testid="ambient-agent-trigger"
                size="sm"
                type="button"
                variant="ghost"
              >
                <span className="truncate">
                  {selectedAgent?.name ?? "Choose an agent"}
                </span>
                <ChevronDown className="h-4 w-4 text-muted-foreground" />
              </Button>
            </DropdownMenuTrigger>
            <DropdownMenuContent align="end" className="min-w-56">
              <DropdownMenuRadioGroup
                onValueChange={(next) => {
                  setAgentPubkey(next);
                  if (settings && wakeWord.trim() && check?.valid) {
                    void persist(
                      withPrimaryBinding(settings, {
                        wakeWord: wakeWord.trim(),
                        agentPubkey: next,
                        destination: null,
                      }),
                    );
                  }
                }}
                value={agentPubkey ?? ""}
              >
                {agents.map((agent) => (
                  <DropdownMenuRadioItem
                    data-testid={`ambient-agent-${agent.pubkey}`}
                    key={agent.pubkey}
                    value={agent.pubkey}
                  >
                    <span className="flex min-w-0 flex-col">
                      <span className="font-medium">{agent.name}</span>
                      <span className="text-2xs text-muted-foreground">
                        {agent.source === "managed"
                          ? `On this computer · ${agent.status ?? "unknown"}`
                          : "On the relay"}
                      </span>
                    </span>
                  </DropdownMenuRadioItem>
                ))}
              </DropdownMenuRadioGroup>
            </DropdownMenuContent>
          </DropdownMenu>
        </SettingsOptionRow>

        <SettingsOptionRow>
          <div className="min-w-0">
            <p className="text-sm font-medium">
              Pause before it stops listening
            </p>
            <p className="text-sm font-normal text-muted-foreground">
              How long a silence has to last before Buzz decides you have
              finished. Longer lets you think mid-sentence without being cut
              off.
            </p>
          </div>
          <div className="flex w-48 shrink-0 items-center gap-2">
            <input
              aria-label="Pause before it stops listening"
              aria-valuetext={silenceHoldLabel(silenceHoldMs)}
              className="h-1.5 w-full cursor-pointer appearance-none rounded-full bg-muted accent-foreground"
              data-testid="ambient-silence-hold"
              disabled={saving}
              max={SILENCE_HOLD_MAX_MS}
              min={SILENCE_HOLD_MIN_MS}
              // Dragging reports every step; only letting go, tabbing away, or
              // finishing an arrow-key nudge writes the setting.
              onBlur={saveSilenceHold}
              onChange={(event) => setSilenceHoldMs(Number(event.target.value))}
              onKeyUp={saveSilenceHold}
              onPointerUp={saveSilenceHold}
              step={SILENCE_HOLD_STEP_MS}
              type="range"
              value={silenceHoldMs}
            />
            <span
              className="w-10 text-right text-xs text-muted-foreground"
              data-testid="ambient-silence-hold-value"
            >
              {silenceHoldLabel(silenceHoldMs)}
            </span>
          </div>
        </SettingsOptionRow>

        <SettingsOptionRow>
          <div className="min-w-0">
            <p className="text-sm font-medium">Stop phrase</p>
            <p className="text-sm font-normal text-muted-foreground">
              Say this to send what you have said so far without waiting for the
              pause. The phrase itself is not sent. Leave it empty for none.
            </p>
          </div>
          <Input
            aria-label="Stop phrase"
            className="max-w-56"
            data-testid="ambient-stop-phrase"
            onBlur={() => void saveStopPhrase()}
            onChange={(event) => setStopPhrase(event.target.value)}
            placeholder="that's all"
            value={stopPhrase}
          />
        </SettingsOptionRow>

        {stopPhraseCheck &&
        !stopPhraseCheck.valid &&
        stopPhraseCheck.message ? (
          <p
            className="px-4 pb-3 text-sm text-destructive"
            data-testid="ambient-stop-phrase-error"
            role="alert"
          >
            {stopPhraseCheck.message}
          </p>
        ) : null}

        <DevicePickerRow
          devices={inputDevices.map((device) => ({
            value: device.deviceId,
            label: device.label || "System default",
          }))}
          description="Persisted, so it survives a restart."
          label="Microphone"
          onSelect={(value) => {
            if (!settings) return;
            void persist({ ...settings, inputDeviceId: value || null });
          }}
          testId="ambient-input-device"
          value={settings?.inputDeviceId ?? ""}
        />

        <DevicePickerRow
          devices={outputDevices.map((device) => ({
            value: device.name,
            label: device.name,
          }))}
          description="Where replies are spoken."
          label="Speaker"
          onSelect={(value) => {
            if (!settings) return;
            void persist({ ...settings, outputDevice: value || null });
          }}
          testId="ambient-output-device"
          value={settings?.outputDevice ?? ""}
        />

        {SPEECH_ROLES.map((role) => (
          <AmbientSpeechBackendRow
            key={role}
            onChange={(next) => {
              if (!settings) return;
              void persist(withSpeechBackend(settings, role, next));
            }}
            role={role}
            value={settings?.[role] ?? LOCAL_SPEECH_BACKEND}
          />
        ))}

        <SettingsOptionRow>
          <div className="min-w-0">
            <p className="text-sm font-medium">Mute</p>
            <p className="text-sm font-normal text-muted-foreground">
              Closes the microphone and stops replies being spoken, without
              losing your settings.
            </p>
          </div>
          <div className="flex items-center gap-2">
            {report?.muted ? (
              <MicOff className="h-4 w-4 text-muted-foreground" />
            ) : (
              <Mic className="h-4 w-4 text-muted-foreground" />
            )}
            <Switch
              aria-label="Mute ambient voice"
              checked={report?.muted ?? false}
              data-testid="ambient-mute"
              disabled={saving}
              onCheckedChange={(value) => {
                void setAmbientVoiceMuted(value)
                  .then(setReport)
                  .catch(() => {
                    setError("Mute could not be applied.");
                  });
              }}
            />
          </div>
        </SettingsOptionRow>

        <SettingsOptionRow>
          <div className="min-w-0">
            <p className="text-sm font-medium">Status</p>
            <p
              className="text-sm font-normal text-muted-foreground"
              data-testid="ambient-status"
            >
              {ambientReportLabel(report)}
            </p>
            {/* The evidence behind that line. A session can be alive and deaf,
                and these two counts are what say where it broke — worth being
                on screen where someone reporting it can read them out. */}
            {audioFlowLine ? (
              <p
                className="text-2xs text-muted-foreground"
                data-testid="ambient-audio-flow"
              >
                {audioFlowLine}
              </p>
            ) : null}
            {/* A configured server that is failing. The feature keeps
                working — speech to text falls back to this computer, and a
                reply that cannot be spoken is still on screen — which is
                exactly why it had to be said out loud somewhere. */}
            {speechHealthLines.map((line) => (
              <p
                className="text-2xs text-destructive"
                data-testid="ambient-speech-health"
                key={line}
                role="alert"
              >
                {line}
              </p>
            ))}
            {launchLine ? (
              <p
                className="text-2xs text-muted-foreground"
                data-testid="ambient-launch"
              >
                {launchLine}
              </p>
            ) : null}
          </div>
        </SettingsOptionRow>

        <SettingsOptionRow>
          <div className="min-w-0">
            <p className="text-sm font-medium">Voice models</p>
            <p className="text-sm font-normal text-muted-foreground">
              The wake word always runs on this computer. The other two are used
              whenever their role above is set to this computer — and speech to
              text also stands in for a server that fails.
            </p>
          </div>
          <div
            className="flex shrink-0 flex-col items-end gap-0.5"
            data-testid="ambient-models"
          >
            {ambientModelRows(models).map((model) => (
              <p
                className="text-2xs text-muted-foreground"
                data-testid={`ambient-model-${model.key}`}
                key={model.key}
              >
                {`${model.label}: ${modelStatusLabel(model.status)}`}
              </p>
            ))}
          </div>
        </SettingsOptionRow>
      </SettingsOptionGroup>

      {block && block.reason !== "load_error" ? (
        <p className="mt-3 text-sm text-muted-foreground">{block.message}</p>
      ) : null}
      {block?.reason === "load_error" ? (
        <p className="mt-3 text-sm text-destructive" role="alert">
          {block.message}
        </p>
      ) : null}
    </section>
  );
}

function DevicePickerRow({
  devices,
  description,
  label,
  onSelect,
  testId,
  value,
}: {
  devices: { value: string; label: string }[];
  description: string;
  label: string;
  onSelect: (value: string) => void;
  testId: string;
  value: string;
}) {
  const selected = devices.find((device) => device.value === value);
  return (
    <SettingsOptionRow>
      <div className="min-w-0">
        <p className="text-sm font-medium">{label}</p>
        <p className="text-sm font-normal text-muted-foreground">
          {description}
        </p>
      </div>
      <DropdownMenu modal={false}>
        <DropdownMenuTrigger asChild>
          <Button
            className="h-7 min-w-40 justify-between gap-1.5"
            data-testid={`${testId}-trigger`}
            size="sm"
            type="button"
            variant="ghost"
          >
            <span className="truncate">
              {selected?.label ?? "System default"}
            </span>
            <ChevronDown className="h-4 w-4 text-muted-foreground" />
          </Button>
        </DropdownMenuTrigger>
        <DropdownMenuContent align="end" className="min-w-56">
          <DropdownMenuRadioGroup onValueChange={onSelect} value={value}>
            <DropdownMenuRadioItem value="">
              System default
            </DropdownMenuRadioItem>
            {devices.map((device) => (
              <DropdownMenuRadioItem
                data-testid={`${testId}-${device.value}`}
                key={device.value}
                value={device.value}
              >
                {device.label}
              </DropdownMenuRadioItem>
            ))}
          </DropdownMenuRadioGroup>
        </DropdownMenuContent>
      </DropdownMenu>
    </SettingsOptionRow>
  );
}
