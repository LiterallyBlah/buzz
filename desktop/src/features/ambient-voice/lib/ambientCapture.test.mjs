import assert from "node:assert/strict";
import test from "node:test";
import { JSDOM } from "jsdom";

import {
  ambientReplyChannel,
  shouldCaptureAmbientAudio,
} from "./ambientCapture.ts";

const runningReport = {
  enabled: true,
  muted: false,
  suspendedByHuddle: false,
  capturing: true,
  status: { state: "listening" },
  live: true,
  destinationChannelId: "11111111-1111-4111-8111-111111111111",
  agentPubkey: "a".repeat(64),
  wakeWord: "hey hermes",
  inputDeviceId: null,
  loadError: null,
};

const inputs = (overrides = {}) => ({
  featureEnabled: true,
  ownsAudioSession: true,
  huddleActive: false,
  report: runningReport,
  ...overrides,
});

test("the flag being off is enough to prevent any microphone acquisition", () => {
  // Acceptance criterion 1. Even a native side reporting itself as capturing
  // cannot make the webview open a device while the flag is off.
  assert.equal(
    shouldCaptureAmbientAudio(inputs({ featureEnabled: false })),
    false,
  );
});

test("the mounted provider never calls getUserMedia while the flag is off", async () => {
  const dom = new JSDOM("<!doctype html><html><body></body></html>", {
    url: "http://localhost/",
  });
  let microphoneCalls = 0;
  Object.defineProperty(dom.window.navigator, "mediaDevices", {
    configurable: true,
    value: {
      getUserMedia: async () => {
        microphoneCalls += 1;
        throw new Error(
          "getUserMedia must not be called while ambientVoice is off",
        );
      },
    },
  });

  const globals = [
    "window",
    "document",
    "navigator",
    "localStorage",
    "HTMLElement",
    "Node",
    "Event",
    "StorageEvent",
    "IS_REACT_ACT_ENVIRONMENT",
  ];
  const previous = new Map(
    globals.map((name) => [
      name,
      Object.getOwnPropertyDescriptor(globalThis, name),
    ]),
  );
  const install = (name, value) =>
    Object.defineProperty(globalThis, name, {
      configurable: true,
      writable: true,
      value,
    });

  install("window", dom.window);
  install("document", dom.window.document);
  install("navigator", dom.window.navigator);
  install("localStorage", dom.window.localStorage);
  install("HTMLElement", dom.window.HTMLElement);
  install("Node", dom.window.Node);
  install("Event", dom.window.Event);
  install("StorageEvent", dom.window.StorageEvent);
  install("IS_REACT_ACT_ENVIRONMENT", true);

  let cleanup = () => {};
  try {
    const React = await import("react");
    const testing = await import("@testing-library/react");
    const { AmbientVoiceProvider } = await import(
      "../AmbientVoiceProvider.tsx"
    );
    cleanup = testing.cleanup;

    const view = testing.render(
      React.createElement(AmbientVoiceProvider, {
        ownsAudioSession: true,
        activeHuddleChannelId: null,
      }),
    );
    await testing.act(async () => {
      await Promise.resolve();
    });

    assert.equal(microphoneCalls, 0);
    view.unmount();
  } finally {
    cleanup();
    dom.window.close();
    for (const [name, descriptor] of previous) {
      if (descriptor) Object.defineProperty(globalThis, name, descriptor);
      else delete globalThis[name];
    }
  }
});

test("captures only when every gate agrees", () => {
  assert.equal(shouldCaptureAmbientAudio(inputs()), true);
});

test("an unknown native state never opens the microphone", () => {
  assert.equal(shouldCaptureAmbientAudio(inputs({ report: null })), false);
});

test("the huddle wins the microphone", () => {
  assert.equal(
    shouldCaptureAmbientAudio(inputs({ huddleActive: true })),
    false,
  );
  assert.equal(
    shouldCaptureAmbientAudio(
      inputs({ report: { ...runningReport, suspendedByHuddle: true } }),
    ),
    false,
  );
});

test("only the window that owns the audio session captures", () => {
  // The dedicated huddle room window renders the same providers.
  assert.equal(
    shouldCaptureAmbientAudio(inputs({ ownsAudioSession: false })),
    false,
  );
});

test("mute releases the device rather than discarding frames", () => {
  assert.equal(
    shouldCaptureAmbientAudio(
      inputs({ report: { ...runningReport, muted: true } }),
    ),
    false,
  );
});

test("a settings-enabled session with no live worker holds no device", () => {
  assert.equal(
    shouldCaptureAmbientAudio(
      inputs({ report: { ...runningReport, capturing: false } }),
    ),
    false,
  );
  assert.equal(
    shouldCaptureAmbientAudio(
      inputs({ report: { ...runningReport, enabled: false } }),
    ),
    false,
  );
});

test("the reply watcher binds to the destination only when the feature is live", () => {
  assert.equal(
    ambientReplyChannel(true, runningReport),
    runningReport.destinationChannelId,
  );
  assert.equal(ambientReplyChannel(false, runningReport), null);
  assert.equal(ambientReplyChannel(true, null), null);
  assert.equal(
    ambientReplyChannel(true, { ...runningReport, muted: true }),
    null,
  );
  assert.equal(
    ambientReplyChannel(true, { ...runningReport, suspendedByHuddle: true }),
    null,
  );
  assert.equal(
    ambientReplyChannel(true, { ...runningReport, destinationChannelId: null }),
    null,
  );
});

test("the reply watcher can still speak before the worker is capturing", () => {
  // The destination is resolved at session start; a reply that lands while the
  // worker is transcribing must not be dropped just because `capturing` is
  // momentarily false in a stale report.
  assert.equal(
    ambientReplyChannel(true, { ...runningReport, capturing: false }),
    runningReport.destinationChannelId,
  );
});
