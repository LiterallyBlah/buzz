/**
 * What the Ambient voice settings section says about the local models, and
 * whether it still agrees with the native runtime once that runtime moves.
 *
 * Dogfood hit a session that ran, showed a live indicator, and made no sound.
 * The cause was a text-to-speech model that had not finished downloading —
 * `start_ambient_tts` treats that as non-fatal on purpose, so nothing failed
 * loudly — and the section only ever showed the wake-word download, so there
 * was nowhere in the app that said so.
 *
 * This renders the real section against a native side reporting one model of
 * each kind in a different state, and requires all three to be on screen.
 */

import assert from "node:assert/strict";
import test from "node:test";

import { AMBIENT_STATE_CHANGED_EVENT } from "../lib/ambientVoiceApi.ts";
import {
  ambientReport,
  deafAmbientReport,
  withAmbientDom,
} from "../lib/ambientVoiceTestDom.mjs";

const SETTINGS = {
  version: 1,
  enabled: true,
  muted: false,
  wakeBindings: [
    { wakeWord: "hey hermes", agentPubkey: "a".repeat(64), destination: null },
  ],
  stt: { backend: "local", endpointUrl: null },
  tts: { backend: "local", endpointUrl: null },
  inputDeviceId: null,
  outputDevice: null,
  indicatorPosition: null,
};

async function mountSettings(models, body, { report = ambientReport() } = {}) {
  await withAmbientDom(
    {
      invoke: (command) => {
        switch (command) {
          case "get_ambient_voice_settings":
            return SETTINGS;
          case "get_ambient_voice_status":
            return report;
          case "get_ambient_model_status":
            return models;
          case "list_managed_agents":
          case "list_relay_agents":
          case "list_audio_output_devices":
            return [];
          case "check_ambient_wake_word":
            return {
              valid: true,
              message: null,
              tokens: null,
              checkedAgainstModel: false,
            };
          default:
            throw new Error(`unexpected command: ${command}`);
        }
      },
    },
    async ({ dom, emit }) => {
      const { OVERRIDES_KEY } = await import("@/shared/features/store.ts");
      dom.window.localStorage.setItem(
        OVERRIDES_KEY,
        JSON.stringify({ ambientVoice: true }),
      );

      const React = await import("react");
      const testing = await import("@testing-library/react");
      const { AmbientVoiceSettingsCard } = await import(
        "./AmbientVoiceSettingsCard.tsx"
      );

      const view = testing.render(
        React.createElement(AmbientVoiceSettingsCard),
      );
      const flush = async () => {
        await testing.act(async () => {
          await new Promise((resolve) => setTimeout(resolve, 0));
        });
      };
      await flush();
      try {
        await body({
          view,
          announce: async (next) => {
            await testing.act(async () => {
              emit(AMBIENT_STATE_CHANGED_EVENT, next);
            });
            await flush();
          },
        });
      } finally {
        testing.cleanup();
      }
    },
  );
}

test("every local model's readiness is listed, not just the wake word", async () => {
  // These fixtures are the exact serialisation of the Rust `ModelStatus` enum
  // (externally tagged, snake_case), pinned from the producing side by
  // `ambient_model_status_serialises_the_shape_the_frontend_parses`. The
  // first version of this test invented an internally-tagged shape instead,
  // and every row shipped rendering "undefined".
  await mountSettings(
    {
      kws: "ready",
      stt: { downloading: { progress_percent: 42 } },
      tts: { error: "checksum mismatch" },
    },
    async ({ view }) => {
      assert.equal(
        view.getByTestId("ambient-model-kws").textContent,
        "Wake word: Ready",
      );
      // The two that used to be invisible. Without them a half-downloaded
      // speech model reads as "the app just does not hear me".
      assert.equal(
        view.getByTestId("ambient-model-stt").textContent,
        "Speech to text: Downloading… 42%",
      );
      assert.equal(
        view.getByTestId("ambient-model-tts").textContent,
        "Voice: Failed: checksum mismatch",
      );
    },
  );
});

test("a model that was never downloaded says so rather than staying blank", async () => {
  await mountSettings(
    {
      kws: "ready",
      stt: "ready",
      tts: "not_downloaded",
    },
    async ({ view }) => {
      assert.equal(
        view.getByTestId("ambient-model-tts").textContent,
        "Voice: Not downloaded",
      );
    },
  );
});

const READY_MODELS = {
  kws: "ready",
  stt: "ready",
  tts: "ready",
};

test("mute and status follow the runtime, not the snapshot taken at mount", async () => {
  // Mute is the native runtime's to own — the listening pill and the
  // Experiments toggle both move it without this section being told. Reading
  // it from one status report fetched at mount meant the settings page and the
  // pill could disagree indefinitely: the microphone was shut and this switch
  // still said it was open.
  await mountSettings(READY_MODELS, async ({ announce, view }) => {
    assert.equal(
      view.getByTestId("ambient-mute").getAttribute("aria-checked"),
      "false",
    );
    assert.equal(
      view.getByTestId("ambient-status").textContent,
      "Listening for the wake word",
    );

    await announce(ambientReport({ muted: true, status: { state: "muted" } }));

    assert.equal(
      view.getByTestId("ambient-mute").getAttribute("aria-checked"),
      "true",
    );
    assert.equal(view.getByTestId("ambient-status").textContent, "Muted");
  });
});

test("a session that hears nothing is called deaf here too, with the counts", async () => {
  // The settings section is where someone looks when the app is not hearing
  // them, so it must not be the last place still saying "Listening for the wake
  // word". The two counts below it are what the next occurrence needs: nothing
  // pushed by this window and nothing received says the break is on this side
  // of the IPC, and the launch line says whether this is the first start after
  // an update — which both reports were.
  await mountSettings(READY_MODELS, async ({ announce, view }) => {
    await announce(
      deafAmbientReport({
        launch: {
          version: "0.5.8-unified.11",
          previousVersion: "0.5.8-unified.10",
          firstLaunchAfterUpdate: true,
          args: [],
        },
      }),
    );

    assert.equal(
      view.getByTestId("ambient-status").textContent,
      "No audio arriving from the microphone",
    );
    assert.equal(
      view.getByTestId("ambient-audio-flow").textContent,
      "Audio: none received for 12s (0 received, 0 sent by this window)",
    );
    assert.equal(
      view.getByTestId("ambient-launch").textContent,
      "Launch: 0.5.8-unified.11, first start after 0.5.8-unified.10",
    );
  });
});

test("a healthy session reports what is flowing rather than an alarm", async () => {
  await mountSettings(READY_MODELS, async ({ announce, view }) => {
    await announce(ambientReport());

    assert.equal(
      view.getByTestId("ambient-status").textContent,
      "Listening for the wake word",
    );
    assert.equal(
      view.getByTestId("ambient-audio-flow").textContent,
      "Audio: 640 received, 642 sent by this window",
    );
  });
});

test("a session that fails after mount is shown here, not left as listening", async () => {
  // The same staleness in its worst form: the native side gave up, the pill
  // says so, and this section would go on describing a live session.
  await mountSettings(READY_MODELS, async ({ announce, view }) => {
    await announce(
      ambientReport({
        capturing: false,
        live: false,
        status: { state: "error", detail: "The microphone was disconnected" },
      }),
    );

    assert.equal(
      view.getByTestId("ambient-status").textContent,
      "The microphone was disconnected",
    );
  });
});
