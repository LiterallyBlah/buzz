/**
 * What the Ambient voice settings section says about the local models.
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

import { ambientReport, withAmbientDom } from "../lib/ambientVoiceTestDom.mjs";

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

async function mountSettings(models, body) {
  await withAmbientDom(
    {
      invoke: (command) => {
        switch (command) {
          case "get_ambient_voice_settings":
            return SETTINGS;
          case "get_ambient_voice_status":
            return ambientReport();
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
    async ({ dom }) => {
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
      await testing.act(async () => {
        await new Promise((resolve) => setTimeout(resolve, 0));
      });
      try {
        await body({ view });
      } finally {
        testing.cleanup();
      }
    },
  );
}

test("every local model's readiness is listed, not just the wake word", async () => {
  await mountSettings(
    {
      kws: { status: "ready" },
      stt: { status: "downloading", progress_percent: 42 },
      tts: { status: "failed", error: "checksum mismatch" },
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
      kws: { status: "ready" },
      stt: { status: "ready" },
      tts: { status: "not_downloaded" },
    },
    async ({ view }) => {
      assert.equal(
        view.getByTestId("ambient-model-tts").textContent,
        "Voice: Not downloaded",
      );
    },
  );
});
