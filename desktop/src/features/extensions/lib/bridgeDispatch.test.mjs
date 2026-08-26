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
    return call ? call(lease, v, method) : Promise.resolve(reply);
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

/** Poll until `predicate` holds, yielding between checks. */
async function waitFor(predicate, what, timeoutMs = 2000) {
  const started = process.hrtime.bigint();
  while (!predicate()) {
    if (Number(process.hrtime.bigint() - started) / 1e6 > timeoutMs) {
      throw new Error(`timed out waiting for ${what}`);
    }
    await new Promise((resolve) => setTimeout(resolve, 5));
  }
}

/** Collect every reply the host writes, in the order it writes them. */
function collect(channel) {
  const seen = [];
  channel.port2.onmessage = (event) => seen.push(event.data);
  return seen;
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

test("an uncorrelatable frame is dropped, not answered", async (t) => {
  // Silence is reserved for the one case where a reply is impossible. §2
  // correlates by `id`; without a usable one, answering would mean inventing
  // a correlation the caller never established.
  const { channel, calls } = harness(t);

  for (const frame of [
    null,
    "identity.getPublicKey",
    42,
    {},
    { v: 1, method: "identity.getPublicKey" }, // no id
    { id: "", v: 1, method: "identity.getPublicKey" }, // empty id
    { id: 7, v: 1, method: "identity.getPublicKey" }, // non-string id
    { id: "i".repeat(129), v: 1, method: "identity.getPublicKey" }, // over the id cap
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

test("a correlatable frame outside the wire shape is refused, not dropped", async (t) => {
  // §9 wants in-flight requests to settle rather than dangle, so anything
  // carrying a usable id gets an answer — and an `invalid_params` refusal
  // here, never a handler call.
  const { channel, calls } = harness(t);

  const cyclic = { id: "cyc", v: 1, method: "identity.getPublicKey" };
  cyclic.self = cyclic;

  for (const frame of [
    { id: "a", method: "identity.getPublicKey" }, // no version
    { id: "b", v: "1", method: "identity.getPublicKey" }, // non-numeric version
    { id: "c", v: Number.NaN, method: "identity.getPublicKey" },
    { id: "d", v: Number.POSITIVE_INFINITY, method: "identity.getPublicKey" },
    { id: "e", v: 1 }, // no method
    { id: "f", v: 1, method: 9 }, // non-string method
    { id: "g", v: 1, method: "" }, // empty method
    { id: "h", v: 1, method: "m".repeat(65) }, // over the method cap
    { id: "i", v: 1, method: "a.b", params: { blob: "x".repeat(64 * 1024) } },
    cyclic,
  ]) {
    const reply = await roundTrip(channel, frame, 200);
    assert.notEqual(reply, null, `frame ${frame.id} must be answered`);
    assert.equal(reply.id, frame.id, "the refusal correlates to the request");
    assert.equal(reply.ok, false);
    assert.equal(
      reply.error.code,
      "invalid_params",
      `frame ${frame.id} must be refused as invalid_params`,
    );
  }

  assert.equal(
    calls.length,
    0,
    "a frame outside the wire shape must never reach a handler",
  );
});

test("a frame just inside the caps is dispatched", async (t) => {
  // The caps are limits, not off-by-ones: at exactly the boundary the frame is
  // still legitimate and must reach the host.
  const { channel, calls } = harness(t);

  const reply = await roundTrip(channel, {
    id: "j".repeat(128),
    v: 1,
    method: "m".repeat(64),
  });

  assert.equal(calls.length, 1, "a frame at the caps must be dispatched");
  assert.equal(calls[0].method, "m".repeat(64));
  assert.equal(reply.id, "j".repeat(128));
});

test("replies do not cross-talk between concurrent requests", async (t) => {
  // Two requests in flight at once, completed in the opposite order. Each
  // reply must carry its own id and its own result — a response that carried
  // another request's id would hand one caller another's answer.
  const pending = new Map();
  const { channel } = harness(t, {
    call: (_lease, _v, method) =>
      new Promise((resolve) => pending.set(method, resolve)),
  });

  const seen = collect(channel);
  channel.port2.postMessage({ id: "first", v: 1, method: "a.one" });
  channel.port2.postMessage({ id: "second", v: 1, method: "a.two" });
  await waitFor(() => pending.size === 2, "both requests to reach the host");

  pending.get("a.two")({ ok: true, result: { which: "two" } });
  await waitFor(() => seen.length === 1, "the second request's reply");
  pending.get("a.one")({ ok: true, result: { which: "one" } });
  await waitFor(() => seen.length === 2, "the first request's reply");

  assert.equal(
    seen[0].id,
    "second",
    "replies follow completion order, not arrival order",
  );
  const byId = new Map(seen.map((r) => [r.id, r]));
  assert.deepEqual(byId.get("first").result, { which: "one" });
  assert.deepEqual(byId.get("second").result, { which: "two" });
});

test("a reply that completes after teardown is not written to the port", async (t) => {
  // BX-07: after teardown the port belongs to the handshake, which closes it.
  // An in-flight call that resolves later must not write to a dead channel.
  let release;
  const { channel, handle } = harness(t, {
    call: () =>
      new Promise((resolve) => {
        release = resolve;
      }),
  });

  const seen = collect(channel);
  channel.port2.postMessage({
    id: "req-5",
    v: 1,
    method: "identity.getPublicKey",
  });
  await waitFor(() => release !== undefined, "the request to reach the host");

  handle.dispose();
  release({ ok: true, result: { pubkey: "a".repeat(64) } });
  await new Promise((resolve) => setTimeout(resolve, 50));

  assert.equal(
    seen.length,
    0,
    "an in-flight reply must not be written to a torn-down port",
  );
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
