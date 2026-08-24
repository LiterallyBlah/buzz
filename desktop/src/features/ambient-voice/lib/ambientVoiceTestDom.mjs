/**
 * jsdom + fake-IPC harness for the ambient-voice UI tests.
 *
 * The two components worth rendering (the listening pill, the settings
 * section) both talk to the native side through `@tauri-apps/api`, which reads
 * `window.__TAURI_INTERNALS__` at call time. Without it every command rejects
 * and the components render their "nothing is known yet" state, which is not
 * the state under test.
 *
 * Same globals dance as `ambientCapture.test.mjs`, factored out because two
 * suites now need it, plus a `PointerEvent` shim: jsdom does not implement one,
 * and without it `fireEvent.pointerDown` degrades to a plain `Event` whose
 * `clientX` is lost — which would make a drag test silently measure nothing.
 */

import { JSDOM } from "jsdom";

const GLOBALS = [
  "window",
  "document",
  "navigator",
  "localStorage",
  "HTMLElement",
  "Node",
  "Event",
  "MouseEvent",
  "PointerEvent",
  "StorageEvent",
  "getComputedStyle",
  "requestAnimationFrame",
  "cancelAnimationFrame",
  "IS_REACT_ACT_ENVIRONMENT",
];

/**
 * Run `body` with a jsdom window installed globally and Tauri IPC faked.
 *
 * @param {object} options
 * @param {(command: string, args: object) => unknown} options.invoke
 *   Handles every `invoke` the component makes. Throw to make one fail.
 * @param {number} [options.width] Window width in CSS pixels.
 * @param {number} [options.height] Window height in CSS pixels.
 * @param {(context: {dom: JSDOM, calls: {command: string, args: object}[], emit: (event: string, payload: unknown) => void}) => Promise<void>} body
 */
export async function withAmbientDom({ invoke, width, height }, body) {
  const dom = new JSDOM("<!doctype html><html><body></body></html>", {
    url: "http://localhost/",
  });

  class PointerEventShim extends dom.window.MouseEvent {
    constructor(type, init = {}) {
      super(type, init);
      this.pointerId = init.pointerId ?? 1;
      this.pointerType = init.pointerType ?? "mouse";
      this.isPrimary = init.isPrimary ?? true;
    }
  }
  dom.window.PointerEvent = PointerEventShim;
  if (typeof width === "number") setViewport(dom, { width });
  if (typeof height === "number") setViewport(dom, { height });

  const calls = [];
  // Every registered `listen()`, so a test can push a native state event at a
  // mounted component the same way the backend does.
  const listeners = [];
  let nextCallbackId = 0;
  dom.window.__TAURI_INTERNALS__ = {
    invoke: async (command, args) => {
      calls.push({ command, args: args ?? {} });
      // `listen()` and its teardown go through the same door; answering them
      // here keeps the event subscription from rejecting.
      if (command === "plugin:event|listen") {
        listeners.push({ event: args.event, handler: args.handler });
        return listeners.length;
      }
      if (command === "plugin:event|unlisten") return null;
      return invoke(command, args ?? {});
    },
    // Ids are monotonic rather than derived from `calls.length`, which two
    // callbacks registered between the same pair of invokes would share — the
    // second would silently replace the first and `emit` would wake the wrong
    // component.
    transformCallback: (callback) => {
      nextCallbackId += 1;
      dom.window[`_${nextCallbackId}`] = callback;
      return nextCallbackId;
    },
  };
  /** Deliver `payload` to every listener registered for `event`. */
  const emit = (event, payload) => {
    for (const listener of listeners) {
      if (listener.event !== event) continue;
      dom.window[`_${listener.handler}`]?.({
        event,
        id: listener.handler,
        payload,
      });
    }
  };
  // `unlisten()` reaches for this before it invokes anything; without it an
  // unmount rejects after the test has finished and surfaces as an
  // unhandledRejection rather than a failure anyone can read.
  dom.window.__TAURI_EVENT_PLUGIN_INTERNALS__ = {
    unregisterListener: () => {},
  };

  const previous = new Map(
    GLOBALS.map((name) => [
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
  for (const name of GLOBALS) {
    if (name === "IS_REACT_ACT_ENVIRONMENT") continue;
    install(name, dom.window[name]);
  }
  install("IS_REACT_ACT_ENVIRONMENT", true);

  try {
    await body({ dom, calls, emit });
  } finally {
    // Unmount teardown (`unlisten`) is asynchronous. Let it finish while the
    // window it reads from still exists.
    await new Promise((resolve) => setTimeout(resolve, 0));
    dom.window.close();
    for (const [name, descriptor] of previous) {
      if (descriptor) Object.defineProperty(globalThis, name, descriptor);
      else delete globalThis[name];
    }
  }
}

/** Resize the jsdom window. `innerWidth`/`innerHeight` are read-only by default. */
export function setViewport(dom, { width, height }) {
  if (typeof width === "number") {
    Object.defineProperty(dom.window, "innerWidth", {
      configurable: true,
      value: width,
      writable: true,
    });
  }
  if (typeof height === "number") {
    Object.defineProperty(dom.window, "innerHeight", {
      configurable: true,
      value: height,
      writable: true,
    });
  }
}

/**
 * A status report with every field the components read.
 *
 * The audio-flow fields describe a healthy session on purpose: they are what
 * separates "listening" from "alive but deaf", so a fixture that left them out
 * would let the deafness copy pass a test that never exercised it. Their shape
 * is pinned from the producing side by
 * `the_audio_diagnostics_serialise_in_the_shape_the_frontend_parses`.
 */
export function ambientReport(overrides = {}) {
  return {
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
    indicatorPosition: null,
    loadError: null,
    audioStale: false,
    audioBatchesReceived: 640,
    msSinceLastAudio: 96,
    webviewCapture: { batchesPushed: 642, captureReady: true },
    speechBackends: {
      stt: {
        configured: false,
        failing: false,
        consecutiveFailures: 0,
        lastError: null,
      },
      tts: {
        configured: false,
        failing: false,
        consecutiveFailures: 0,
        lastError: null,
      },
    },
    launch: {
      version: "0.5.8-unified.11",
      previousVersion: "0.5.8-unified.11",
      firstLaunchAfterUpdate: false,
      args: [],
    },
    ...overrides,
  };
}

/**
 * The report a session whose speech-to-text server has stopped answering
 * produces. Its shape is pinned from the producing side by
 * `the_speech_backend_health_serialises_in_the_shape_the_frontend_parses`.
 */
export function failingSpeechServerReport(overrides = {}) {
  return ambientReport({
    speechBackends: {
      stt: {
        configured: true,
        failing: true,
        consecutiveFailures: 3,
        lastError: "speech server did not answer: connection refused",
      },
      tts: {
        configured: false,
        failing: false,
        consecutiveFailures: 0,
        lastError: null,
      },
    },
    ...overrides,
  });
}

/** The report a session that is running but receiving nothing produces. */
export function deafAmbientReport(overrides = {}) {
  return ambientReport({
    audioStale: true,
    audioBatchesReceived: 0,
    msSinceLastAudio: 12_000,
    webviewCapture: { batchesPushed: 0, captureReady: true },
    ...overrides,
  });
}
