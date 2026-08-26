import assert from "node:assert/strict";
import test from "node:test";

import { startBridgeDispatch } from "./bridgeDispatch.ts";

/**
 * A port pair plus a recording stand-in for the Rust call.
 *
 * `calls` is what actually crossed the IPC boundary — the assertions that
 * matter are about what this layer forwarded, and with which lease.
 *
 * Teardown is registered with `t.after` rather than written at the end of each
 * test: a started `MessagePort` holds the event loop open, so cleanup placed
 * after the assertions is skipped exactly when a test fails, and the runner
 * hangs instead of reporting. Failures must be legible.
 */
function harness(t, options = {}) {
  const { reply = { ok: true, result: { pubkey: "a".repeat(64) } }, call } =
    options;
  const channel = new MessageChannel();
  const calls = [];
  const record = (lease, v, method) => {
    calls.push({ lease, v, method });
    return call ? call() : Promise.resolve(reply);
  };
  const handle = startBridgeDispatch({
    port: channel.port1,
    lease: "lease-a",
    call: record,
  });
  t.after(() => {
    handle.dispose();
    channel.port1.close();
    channel.port2.close();
  });
  return { channel, calls, handle };
}

/** Send a frame from the extension side and wait for the reply, or null. */
function roundTrip(channel, frame, timeoutMs = 200) {
  return new Promise((resolve) => {
    const timer = setTimeout(() => resolve(null), timeoutMs);
    channel.port2.onmessage = (event) => {
      clearTimeout(timer);
      resolve(event.data);
    };
    channel.port2.postMessage(frame);
  });
}

test("a well-formed request is forwarded and its reply correlated", async (t) => {
  const { channel, calls } = harness(t);

  const reply = await roundTrip(channel, {
    id: "req-1",
    v: 1,
    method: "identity.getPublicKey",
  });

  assert.deepEqual(calls, [
    { lease: "lease-a", v: 1, method: "identity.getPublicKey" },
  ]);
  assert.equal(reply.id, "req-1", "the reply carries the request's id");
  assert.equal(reply.ok, true);
  assert.deepEqual(reply.result, { pubkey: "a".repeat(64) });
});

test("the lease is the host's, never the caller's", async (t) => {
  const { channel, calls } = harness(t);

  // The extension supplies its own lease and extensionId. Neither may reach
  // the call: attribution is the host-minted lease captured at start.
  await roundTrip(channel, {
    id: "req-1",
    v: 1,
    method: "identity.getPublicKey",
    lease: "lease-forged",
    params: { extensionId: "some-other-extension" },
  });

  assert.equal(calls.length, 1);
  assert.equal(calls[0].lease, "lease-a", "the forged lease must be ignored");
});

test("malformed frames are ignored, not answered", async (t) => {
  const { channel, calls } = harness(t);

  for (const frame of [
    null,
    "identity.getPublicKey",
    42,
    {},
    { v: 1, method: "identity.getPublicKey" }, // no id
    { id: "", v: 1, method: "identity.getPublicKey" }, // empty id
    { id: 7, v: 1, method: "identity.getPublicKey" }, // non-string id
    { id: "x", method: "identity.getPublicKey" }, // no version
    { id: "x", v: "1", method: "identity.getPublicKey" }, // non-numeric version
    { id: "x", v: Number.NaN, method: "identity.getPublicKey" },
    { id: "x", v: 1 }, // no method
    { id: "x", v: 1, method: 9 }, // non-string method
  ]) {
    const reply = await roundTrip(channel, frame, 60);
    assert.equal(
      reply,
      null,
      `frame ${JSON.stringify(frame)} must not be answered`,
    );
  }

  assert.equal(calls.length, 0, "no malformed frame may reach the host");
});

test("a bad version is forwarded, not judged here", async (t) => {
  // This layer must not decide that a version is unsupported: §8 gives that a
  // code and Rust owns it. The frame is correlatable, so it gets an answer.
  const { channel, calls } = harness(t, {
    reply: {
      ok: false,
      error: {
        code: "unsupported_version",
        message: "this host speaks bridge version 1",
      },
    },
  });

  const reply = await roundTrip(channel, {
    id: "req-9",
    v: 99,
    method: "identity.getPublicKey",
  });

  assert.equal(calls.length, 1, "the host decides, not this layer");
  assert.equal(calls[0].v, 99);
  assert.equal(reply.id, "req-9");
  assert.equal(reply.error.code, "unsupported_version");
});

test("an unknown method is forwarded and its refusal correlated", async (t) => {
  const { channel } = harness(t, {
    reply: {
      ok: false,
      error: {
        code: "unknown_method",
        message: "unknown method: publish.event",
      },
    },
  });

  const reply = await roundTrip(channel, {
    id: "req-2",
    v: 1,
    method: "publish.event",
  });
  assert.equal(reply.id, "req-2");
  assert.equal(reply.ok, false);
  assert.equal(reply.error.code, "unknown_method");
});

test("an IPC failure still settles the request", async (t) => {
  // A hung promise on the extension side would be worse than an error: §9 says
  // in-flight requests reject rather than dangle.
  const { channel } = harness(t, {
    call: () => Promise.reject(new Error("ipc exploded")),
  });

  const reply = await roundTrip(channel, {
    id: "req-3",
    v: 1,
    method: "identity.getPublicKey",
  });
  assert.equal(reply.id, "req-3");
  assert.equal(reply.ok, false);
  assert.equal(reply.error.code, "internal");
});

test("a disposed dispatcher serves nothing", async (t) => {
  const { channel, calls, handle } = harness(t);
  handle.dispose();
  handle.dispose();

  const reply = await roundTrip(
    channel,
    { id: "req-4", v: 1, method: "identity.getPublicKey" },
    60,
  );
  assert.equal(reply, null);
  assert.equal(calls.length, 0);
});
