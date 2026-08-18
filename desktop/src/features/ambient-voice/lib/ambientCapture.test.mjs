import assert from "node:assert/strict";
import test from "node:test";
import { JSDOM } from "jsdom";

import {
  ambientCaptureErrorMessage,
  ambientReplyChannel,
  shouldCaptureAmbientAudio,
} from "./ambientCapture.ts";
import { ambientReport, withAmbientDom } from "./ambientVoiceTestDom.mjs";

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

test("a microphone failure names what the user can do about it", () => {
  // The string goes on the indicator and in the settings section verbatim, so
  // "NotAllowedError" would be the app telling the user nothing at all.
  const named = (name) => {
    const error = new Error("nope");
    error.name = name;
    return ambientCaptureErrorMessage(error);
  };
  assert.match(named("NotAllowedError"), /refused/);
  assert.match(named("NotFoundError"), /not available/);
  assert.match(named("OverconstrainedError"), /not available/);
  assert.match(named("NotReadableError"), /another application/);
  // Anything unrecognised, including a non-Error, still has to read as a
  // sentence rather than "undefined" or "[object Object]".
  assert.match(
    ambientCaptureErrorMessage("boom"),
    /^The microphone could not be opened/,
  );
});

test("a microphone the webview cannot open is reported, not left as listening", async () => {
  // The native worker has no device handle — only an audio queue that stops
  // filling — so a refused microphone left the indicator claiming to listen
  // for the wake word while nothing was reaching it.
  const refused = new Error("Permission denied");
  refused.name = "NotAllowedError";
  // No destination: the reply watcher stays out of a capture test.
  const capturing = ambientReport({ destinationChannelId: null });

  await withAmbientDom(
    {
      invoke: (command) => {
        switch (command) {
          case "get_ambient_voice_status":
          case "check_ambient_hotstart":
            return capturing;
          case "get_identity":
            return { pubkey: "b".repeat(64) };
          case "report_ambient_capture_error":
            return {
              ...capturing,
              capturing: false,
              live: false,
              status: {
                state: "error",
                detail: "Microphone access was refused",
              },
            };
          default:
            throw new Error(`unexpected command: ${command}`);
        }
      },
    },
    async ({ calls, dom }) => {
      Object.defineProperty(dom.window.navigator, "mediaDevices", {
        configurable: true,
        value: {
          getUserMedia: async () => {
            throw refused;
          },
        },
      });
      const { OVERRIDES_KEY } = await import("@/shared/features/store.ts");
      dom.window.localStorage.setItem(
        OVERRIDES_KEY,
        JSON.stringify({ ambientVoice: true }),
      );

      const React = await import("react");
      const testing = await import("@testing-library/react");
      const { AmbientVoiceProvider } = await import(
        "../AmbientVoiceProvider.tsx"
      );

      testing.render(
        React.createElement(AmbientVoiceProvider, {
          ownsAudioSession: true,
          activeHuddleChannelId: null,
        }),
      );
      await testing.act(async () => {
        await new Promise((resolve) => setTimeout(resolve, 0));
      });
      try {
        assert.equal(
          calls.find((call) => call.command === "report_ambient_capture_error")
            ?.args.message,
          "Microphone access was refused — allow it for Buzz in your system settings",
        );
      } finally {
        testing.cleanup();
      }
    },
  );
});

test("a microphone that disappears mid-session is reported too", async () => {
  // Unplugging the device ends its track: no error is thrown anywhere, the
  // frames simply stop, and the worker has no way to tell that apart from
  // silence in the room.
  const capturing = ambientReport({ destinationChannelId: null });
  const reported = [];

  await withAmbientDom(
    {
      invoke: (command, args) => {
        switch (command) {
          case "get_ambient_voice_status":
          case "check_ambient_hotstart":
            return capturing;
          case "get_identity":
            return { pubkey: "b".repeat(64) };
          case "report_ambient_capture_error":
            reported.push(args.message);
            // Deliberately still "capturing": the real native side stops the
            // session here, which would unmount the effect under test.
            return capturing;
          default:
            throw new Error(`unexpected command: ${command}`);
        }
      },
    },
    async ({ dom }) => {
      const track = new dom.window.EventTarget();
      track.stop = () => {};
      Object.defineProperty(dom.window.navigator, "mediaDevices", {
        configurable: true,
        value: {
          getUserMedia: async () => ({
            getTracks: () => [track],
            getAudioTracks: () => [track],
          }),
        },
      });
      const { OVERRIDES_KEY } = await import("@/shared/features/store.ts");
      dom.window.localStorage.setItem(
        OVERRIDES_KEY,
        JSON.stringify({ ambientVoice: true }),
      );

      const React = await import("react");
      const testing = await import("@testing-library/react");
      const { AmbientVoiceProvider } = await import(
        "../AmbientVoiceProvider.tsx"
      );

      testing.render(
        React.createElement(AmbientVoiceProvider, {
          ownsAudioSession: true,
          activeHuddleChannelId: null,
        }),
      );
      await testing.act(async () => {
        await new Promise((resolve) => setTimeout(resolve, 0));
      });
      try {
        // jsdom has no AudioContext, so the worklet setup fails after the
        // device was acquired — which is itself a capture failure worth
        // reporting, and proves the listener was attached before it.
        assert.match(reported[0] ?? "", /^The microphone could not be opened/);

        await testing.act(async () => {
          track.dispatchEvent(new dom.window.Event("ended"));
        });
        await testing.act(async () => {
          await new Promise((resolve) => setTimeout(resolve, 0));
        });

        assert.equal(
          reported[1],
          "The microphone stopped — reconnect it or choose another one in settings",
        );
      } finally {
        testing.cleanup();
      }
    },
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
