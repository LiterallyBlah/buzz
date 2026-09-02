import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import vm from "node:vm";

class FakePort {
  listeners = new Set();
  peer = null;
  closed = false;

  addEventListener(name, handler) {
    if (name === "message") this.listeners.add(handler);
  }

  removeEventListener(name, handler) {
    if (name === "message") this.listeners.delete(handler);
  }

  start() {}

  close() {
    this.closed = true;
  }

  postMessage(data) {
    if (!this.peer || this.peer.closed) return;
    queueMicrotask(() => {
      for (const handler of this.peer.listeners) handler({ data });
    });
  }
}

class FakeMessageChannel {
  static created = 0;

  constructor() {
    FakeMessageChannel.created += 1;
    this.port1 = new FakePort();
    this.port2 = new FakePort();
    this.port1.peer = this.port2;
    this.port2.peer = this.port1;
  }
}

function harness() {
  FakeMessageChannel.created = 0;
  const listeners = new Map();
  const invocations = [];
  const childMessages = [];
  const callbacks = new Map();
  let nextCallback = 1;
  let nativeChannel;
  const childWindow = {
    postMessage(data, _origin, ports) {
      childMessages.push({ data, ports });
    },
  };
  const frame = { contentWindow: childWindow };
  const window = {
    __TAURI_INTERNALS__: {
      transformCallback(handler) {
        const id = nextCallback;
        nextCallback += 1;
        callbacks.set(id, handler);
        return id;
      },
      unregisterCallback(id) {
        callbacks.delete(id);
      },
      async invoke(command, args) {
        invocations.push({ command, args });
        if (command === "plugin:extension-bridge|native_stream_bind") {
          nativeChannel = args.channel;
          return null;
        }
        if (command === "plugin:extension-bridge|invoke") {
          if (args.method === "subscribe") {
            return {
              ok: true,
              result: { sub: "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa" },
            };
          }
          return { ok: true, result: { publicKey: "owner" } };
        }
        return null;
      },
    },
    addEventListener(name, handler) {
      const group = listeners.get(name) ?? new Set();
      group.add(handler);
      listeners.set(name, group);
    },
    removeEventListener(name, handler) {
      listeners.get(name)?.delete(handler);
    },
    dispatch(name, event) {
      for (const handler of listeners.get(name) ?? []) handler(event);
    },
  };
  const source = readFileSync(
    new URL(
      "../../../../src-tauri/src/extensions/native_wrapper.js",
      import.meta.url,
    ),
    "utf8",
  ).replace(
    "__BUZZ_NATIVE_LEASE_JSON__",
    JSON.stringify("11111111-1111-4111-8111-111111111111"),
  );
  vm.runInNewContext(source, {
    window,
    document: { getElementById: () => frame },
    MessageChannel: FakeMessageChannel,
    TextEncoder,
    queueMicrotask,
    Promise,
    JSON,
    Object,
    Set,
    Map,
    WeakSet,
    Number,
    RegExp,
    console,
  });
  return {
    window,
    childWindow,
    childMessages,
    invocations,
    emitNative(batch, index = 0) {
      const callback = nativeChannel && callbacks.get(nativeChannel.id);
      if (callback) callback({ message: batch, index });
    },
  };
}

const settle = () => new Promise((resolve) => setTimeout(resolve, 0));

test("native wrapper accepts ready only from its child and originates one channel", async () => {
  const h = harness();
  h.window.dispatch("message", {
    source: {},
    data: { buzz: "ready" },
    ports: [new FakePort()],
  });
  assert.equal(FakeMessageChannel.created, 0);

  h.window.dispatch("message", {
    source: h.childWindow,
    data: { buzz: "ready" },
    ports: [new FakePort()],
  });
  await settle();
  assert.equal(FakeMessageChannel.created, 1);
  assert.equal(h.childMessages.length, 1);
  assert.equal(
    JSON.stringify(h.childMessages[0].data),
    JSON.stringify({ buzz: "port", v: 1 }),
  );
  assert.equal(h.childMessages[0].ports.length, 1);
  assert.ok(
    h.invocations.some(
      ({ command, args }) =>
        command === "plugin:extension-bridge|native_stream_bind" &&
        args.lease === "11111111-1111-4111-8111-111111111111" &&
        args.channel.__TAURI_TO_IPC_KEY__().startsWith("__CHANNEL__:") &&
        args.channel.toJSON().startsWith("__CHANNEL__:"),
    ),
  );
  assert.equal(
    h.invocations.some(({ command }) => command.startsWith("plugin:event|")),
    false,
  );
  assert.ok(
    h.invocations.some(
      ({ command, args }) =>
        command === "plugin:extension-bridge|native_ready" &&
        args.lease === "11111111-1111-4111-8111-111111111111",
    ),
  );

  h.window.dispatch("message", {
    source: h.childWindow,
    data: { buzz: "ready" },
  });
  assert.equal(FakeMessageChannel.created, 1);
});

test("native wrapper carries the host lease to the existing dispatch and correlates", async () => {
  const h = harness();
  h.window.dispatch("message", {
    source: h.childWindow,
    data: { buzz: "ready" },
  });
  await settle();
  const childPort = h.childMessages[0].ports[0];
  const replies = [];
  childPort.addEventListener("message", (event) => replies.push(event.data));
  childPort.start();
  childPort.postMessage({
    id: "22222222-2222-4222-8222-222222222222",
    v: 1,
    method: "identity.getPublicKey",
    params: {},
  });
  await settle();
  await settle();

  const dispatch = h.invocations.find(
    ({ command }) => command === "plugin:extension-bridge|invoke",
  );
  assert.equal(dispatch.args.lease, "11111111-1111-4111-8111-111111111111");
  assert.equal(dispatch.args.method, "identity.getPublicKey");
  assert.equal(
    JSON.stringify(replies),
    JSON.stringify([
      {
        id: "22222222-2222-4222-8222-222222222222",
        ok: true,
        result: { publicKey: "owner" },
      },
    ]),
  );
});

test("native wrapper rejects replay without a second Rust dispatch", async () => {
  const h = harness();
  h.window.dispatch("message", {
    source: h.childWindow,
    data: { buzz: "ready" },
  });
  await settle();
  const childPort = h.childMessages[0].ports[0];
  const replies = [];
  childPort.addEventListener("message", (event) => replies.push(event.data));
  const request = {
    id: "33333333-3333-4333-8333-333333333333",
    v: 1,
    method: "identity.getPublicKey",
    params: {},
  };
  childPort.postMessage(request);
  await settle();
  childPort.postMessage(request);
  await settle();
  assert.equal(
    h.invocations.filter(
      ({ command }) => command === "plugin:extension-bridge|invoke",
    ).length,
    1,
  );
  assert.equal(replies.at(-1).error.code, "replayed_request");
});

test("native wrapper receives stream batches only through its dedicated channel", async () => {
  const h = harness();
  h.window.dispatch("message", {
    source: h.childWindow,
    data: { buzz: "ready" },
  });
  await settle();
  const childPort = h.childMessages[0].ports[0];
  const replies = [];
  childPort.addEventListener("message", (event) => replies.push(event.data));
  childPort.postMessage({
    id: "66666666-6666-4666-8666-666666666666",
    v: 1,
    method: "subscribe",
    params: { filter: { kinds: [9], "#h": ["channel"] } },
  });
  await settle();
  await settle();
  const sub = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
  const frames = [{ sub, kind: "eose" }];
  h.emitNative({
    generation: "11111111-1111-4111-8111-111111111111",
    sub,
    seq: 1,
    token: "77777777-7777-4777-8777-777777777777",
    frames,
    frameCount: 1,
    encodedBytes: new TextEncoder().encode(JSON.stringify(frames[0]))
      .byteLength,
    terminal: false,
  });
  await settle();
  assert.ok(
    replies.some(
      (reply) =>
        reply.buzz === "stream-batch" && reply.sub === sub && reply.seq === 1,
    ),
  );
  assert.equal(
    h.invocations.some(({ command }) => command.startsWith("plugin:event|")),
    false,
  );
});
