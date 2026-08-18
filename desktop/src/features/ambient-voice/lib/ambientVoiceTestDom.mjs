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
 * @param {(context: {dom: JSDOM, calls: {command: string, args: object}[]}) => Promise<void>} body
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
  dom.window.__TAURI_INTERNALS__ = {
    invoke: async (command, args) => {
      calls.push({ command, args: args ?? {} });
      // `listen()` and its teardown go through the same door; answering them
      // here keeps the event subscription from rejecting.
      if (command === "plugin:event|listen") return 1;
      if (command === "plugin:event|unlisten") return null;
      return invoke(command, args ?? {});
    },
    transformCallback: (callback) => {
      const id = calls.length + 1;
      dom.window[`_${id}`] = callback;
      return id;
    },
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
    await body({ dom, calls });
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

/** A status report with every field the components read. */
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
    ...overrides,
  };
}
