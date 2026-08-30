import assert from "node:assert/strict";
import test from "node:test";

import {
  MAX_TS_QUEUED_BATCHES_PER_SUB,
  MAX_TS_QUEUED_BYTES_PER_PORT,
  startBridgeDispatch,
} from "./bridgeDispatch.ts";
import { createRegistry } from "./bridgeRegistry.ts";

const LEASE = "9f1c2d3e-4a5b-4c6d-8e7f-0a1b2c3d4e5f";
const uuid = (n) => `3f2504e0-4f89-41d3-9a0c-${String(n).padStart(12, "0")}`;

/**
 * A port pair plus a recording stand-in for the Rust call.
 *
 * Teardown is registered with `t.after` rather than written at the end of each
 * test: a started `MessagePort` holds the event loop open, so cleanup placed
 * after the assertions is skipped exactly when a test fails, and the runner
 * hangs instead of reporting.
 */
function harness(t, options = {}) {
  const {
    reply = { ok: true, result: { pubkey: "a".repeat(64) } },
    call,
    control,
    listen: listenOption,
    registry,
    lease = LEASE,
  } = options;
  const channel = new MessageChannel();
  const calls = [];
  const controls = [];
  const timeline = [];
  const record = (lease, v, method, params) => {
    calls.push({ lease, v, method, params });
    return call ? call(lease, v, method, params) : Promise.resolve(reply);
  };
  const recordControl = (lease, action, sub, detail) => {
    timeline.push({ kind: "control", action, sub });
    controls.push({ lease, action, sub, detail });
    return control ? control(lease, action, sub, detail) : Promise.resolve();
  };
  let closeCount = 0;
  const realPost = channel.port1.postMessage.bind(channel.port1);
  channel.port1.postMessage = (value, transfer) => {
    timeline.push({ kind: "post", value });
    return realPost(value, transfer);
  };
  const realClose = channel.port1.close.bind(channel.port1);
  channel.port1.close = () => {
    closeCount += 1;
    realClose();
  };

  let handler = null;
  let raw = null;
  let unlistened = 0;
  const defaultListen = (_event, fn) => {
    handler = fn;
    raw = fn;
    return Promise.resolve(() => {
      unlistened += 1;
      handler = null;
    });
  };
  const listen = listenOption ?? defaultListen;
  const streamSeq = new Map();
  const batch = (payload) => {
    if (!payload?.frame) return payload;
    const { frame } = payload;
    const streamKey = `${payload.lease}:${frame.sub}`;
    const seq = (streamSeq.get(streamKey) ?? 0) + 1;
    streamSeq.set(streamKey, seq);
    const encodedBytes = new TextEncoder().encode(
      JSON.stringify(frame),
    ).byteLength;
    return {
      generation: payload.lease,
      sub: frame.sub,
      seq,
      token: uuid(900000 + seq),
      frames: [frame],
      frameCount: 1,
      encodedBytes,
      terminal: frame.kind === "closed",
    };
  };
  const emit = (payload) => {
    if (!handler) {
      throw new Error("no stream listener is installed");
    }
    handler({ payload: batch(payload) });
  };
  const rawEmit = (payload) => raw({ payload: batch(payload) });

  const handle = startBridgeDispatch({
    port: channel.port1,
    lease,
    call: record,
    control: recordControl,
    registry,
    listen,
  });
  t.after(() => {
    handle.dispose();
    channel.port1.close();
    channel.port2.close();
  });
  return {
    channel,
    calls,
    controls,
    timeline,
    handle,
    emit,
    rawEmit,
    listening: () => handler !== null,
    unlistened: () => unlistened,
    closeCount: () => closeCount,
  };
}

async function waitFor(predicate, what, timeoutMs = 2000) {
  const started = process.hrtime.bigint();
  while (!predicate()) {
    if (Number(process.hrtime.bigint() - started) / 1e6 > timeoutMs) {
      throw new Error(`timed out waiting for ${what}`);
    }
    await new Promise((resolve) => setTimeout(resolve, 5));
  }
}

function collect(channel) {
  const seen = [];
  channel.port2.onmessage = (event) => {
    const value = event.data;
    if (value?.buzz === "stream-batch") {
      // Stand-in for the extension-side bridge client: adopt/validate the whole
      // batch, expose only public stream frames, then ACK after dequeue.
      assert.equal(value.frameCount, value.frames.length);
      for (const frame of value.frames) {
        assert.equal(frame.sub, value.sub);
        seen.push(frame);
      }
      if (!value.terminal) {
        channel.port2.postMessage({
          buzz: "stream-ack",
          generation: value.generation,
          sub: value.sub,
          seq: value.seq,
          token: value.token,
          frameCount: value.frameCount,
          encodedBytes: value.encodedBytes,
        });
      }
      return;
    }
    seen.push(value);
  };
  return seen;
}

function roundTrip(channel, frame, timeoutMs = 300) {
  return new Promise((resolve) => {
    const timer = setTimeout(() => resolve(null), timeoutMs);
    channel.port2.onmessage = (event) => {
      clearTimeout(timer);
      resolve(event.data);
    };
    channel.port2.postMessage(frame);
  });
}

// ── stream-listener readiness and activation barrier ─────────────────────────

test("subscribe waits for the Tauri stream listener before host admission", async (t) => {
  let release;
  const listen = (_event, handler) =>
    new Promise((resolve) => {
      release = () =>
        resolve(() => {
          void handler;
        });
    });
  const h = harness(t, {
    reply: { ok: true, result: { sub: "sub-ready" } },
    listen,
  });
  const pending = roundTrip(h.channel, {
    id: uuid(700),
    v: 1,
    method: "subscribe",
    params: { filter: { kinds: [9] } },
  });
  await new Promise((resolve) => setTimeout(resolve, 30));
  assert.equal(
    h.calls.length,
    0,
    "deferred readiness permits no host subscribe",
  );
  release();
  const reply = await pending;
  assert.equal(reply.ok, true);
  assert.equal(
    h.calls.filter((entry) => entry.method === "subscribe").length,
    1,
  );
});

test("listener failure is terminal and opens no host subscription", async (t) => {
  const h = harness(t, {
    listen: () => Promise.reject(new Error("listener failed")),
  });
  const reply = await roundTrip(h.channel, {
    id: uuid(701),
    v: 1,
    method: "subscribe",
    params: { filter: { kinds: [9] } },
  });
  assert.equal(reply, null, "the failed transport tears the port down");
  assert.equal(h.calls.length, 0, "no host subscribe was opened");
  assert.equal(h.closeCount(), 1, "the unusable port is terminal");
});

test("disposal while listener readiness is pending never opens later", async (t) => {
  let resolveListen;
  let stopped = 0;
  const h = harness(t, {
    listen: () =>
      new Promise((resolve) => {
        resolveListen = () =>
          resolve(() => {
            stopped += 1;
          });
      }),
  });
  h.channel.port2.postMessage({
    id: uuid(702),
    v: 1,
    method: "subscribe",
    params: { filter: { kinds: [9] } },
  });
  await new Promise((resolve) => setTimeout(resolve, 20));
  h.handle.dispose();
  resolveListen();
  await new Promise((resolve) => setTimeout(resolve, 20));
  assert.equal(h.calls.length, 0);
  assert.equal(stopped, 1, "the late listener is removed immediately");
});

// ── the stream forwarder ─────────────────────────────────────────────────────
//
// Rust mints the `sub` and decides what may be delivered; this layer decides
// *where*. The rows below are about that "where", plus the one thing this side
// genuinely owns — the per-port subscription ceiling.

/** Open a subscription through the dispatcher and return its id. */
async function openSubscription(h, id, sub) {
  const reply = await roundTrip(h.channel, {
    id,
    v: 1,
    method: "subscribe",
    params: { filter: { kinds: [9] } },
  });
  assert.deepEqual(reply, { id, ok: true, result: { sub } });
  return sub;
}

test("the correlated reply is written before exact-generation activation", async (t) => {
  const sub = "sub-activation";
  const h = harness(t, { reply: { ok: true, result: { sub } } });
  await openSubscription(h, uuid(710), sub);
  await waitFor(
    () => h.controls.some((entry) => entry.action === "activate"),
    "the activation receipt",
  );
  const replyIndex = h.timeline.findIndex(
    (entry) => entry.kind === "post" && entry.value?.id === uuid(710),
  );
  const activationIndex = h.timeline.findIndex(
    (entry) => entry.kind === "control" && entry.action === "activate",
  );
  assert.ok(replyIndex >= 0 && activationIndex > replyIndex);
  assert.deepEqual(h.controls[0], {
    lease: LEASE,
    action: "activate",
    sub,
    detail: undefined,
  });
});

test("a stream frame for this lease reaches the extension", async (t) => {
  const sub = "sub-1";
  const h = harness(t, { reply: { ok: true, result: { sub } } });
  await openSubscription(h, uuid(1), sub);
  await waitFor(() => h.listening(), "the stream listener");

  const seen = collect(h.channel);
  h.emit({ lease: LEASE, frame: { sub, kind: "eose" } });
  await waitFor(() => seen.length === 1, "the eose frame");
  assert.deepEqual(seen[0], { sub, kind: "eose" });
  assert.equal(
    "id" in seen[0],
    false,
    "a stream frame carries no id, so it cannot settle a request",
  );
});

test("a stream frame addressed to another lease is not delivered", async (t) => {
  // The second of the two independent walls. Rust keys its registry by
  // (lease, sub) and cannot address a successor port; this refuses one that
  // arrives anyway. The two fall at different times, so neither is redundant.
  const sub = "sub-1";
  const h = harness(t, { reply: { ok: true, result: { sub } } });
  await openSubscription(h, uuid(1), sub);
  await waitFor(() => h.listening(), "the stream listener");

  const seen = collect(h.channel);
  h.emit({ lease: uuid(99), frame: { sub, kind: "eose" } });
  // Then a frame that *should* arrive, so "nothing was delivered" cannot be
  // satisfied by a forwarder that delivers nothing at all.
  h.emit({ lease: LEASE, frame: { sub, kind: "eose" } });
  await waitFor(() => seen.length === 1, "the addressed frame");
  assert.equal(seen.length, 1, "only the frame for this lease");
});

test("a frame for a sub this port does not hold is dropped", async (t) => {
  const h = harness(t, { reply: { ok: true, result: { sub: "sub-1" } } });
  await openSubscription(h, uuid(1), "sub-1");
  await waitFor(() => h.listening(), "the stream listener");

  const seen = collect(h.channel);
  h.emit({ lease: LEASE, frame: { sub: "never-opened", kind: "eose" } });
  h.emit({ lease: LEASE, frame: { sub: "sub-1", kind: "eose" } });
  await waitFor(() => seen.length === 1, "the live sub's frame");
  assert.deepEqual(seen[0], { sub: "sub-1", kind: "eose" });
});

test("closed is delivered, and nothing follows it", async (t) => {
  // `closed` is terminal in §5. Forwarding anything after it would contradict
  // the frame the extension just used to tear its own state down.
  const sub = "sub-1";
  const h = harness(t, { reply: { ok: true, result: { sub } } });
  await openSubscription(h, uuid(1), sub);
  await waitFor(() => h.listening(), "the stream listener");

  const seen = collect(h.channel);
  h.emit({
    lease: LEASE,
    frame: { sub, kind: "closed", reason: "relay_closed" },
  });
  await waitFor(() => seen.length === 1, "the closed frame");
  h.emit({ lease: LEASE, frame: { sub, kind: "event", event: {} } });
  await new Promise((resolve) => setTimeout(resolve, 20));
  assert.equal(seen.length, 1, "no frame may follow closed");
  assert.equal(seen[0].kind, "closed");
});

test("a refused subscription is closed host-side rather than leaked", async (t) => {
  // The port ceiling is this layer's own rule, so the host does not know it was
  // broken. A refusal that only answered the caller would leave a live relay
  // branch nothing forwards, for the life of the frame.
  const registry = createRegistry();
  let minted = 0;
  const h = harness(t, {
    registry,
    call: (_l, _v, method) => {
      if (method !== "subscribe") {
        return Promise.resolve({ ok: true, result: {} });
      }
      minted += 1;
      return Promise.resolve({ ok: true, result: { sub: `sub-${minted}` } });
    },
  });

  // Fill the ceiling, then ask for one more.
  for (let i = 0; i < 64; i += 1) {
    assert.equal(registry.adoptSub(`filler-${i}`).kind, "opened");
  }
  const reply = await roundTrip(h.channel, {
    id: uuid(1),
    v: 1,
    method: "subscribe",
    params: { filter: { kinds: [9] } },
  });
  assert.equal(reply.ok, false);
  assert.equal(reply.error.code, "quota_exceeded");

  await waitFor(
    () => h.calls.some((c) => c.method === "unsubscribe"),
    "the host-side close",
  );
  const closed = h.calls.find((c) => c.method === "unsubscribe");
  assert.deepEqual(
    closed.params,
    { sub: "sub-1" },
    "and it must close the id the host actually minted",
  );
});

test("a subscribe that names no sub is an internal error", async (t) => {
  // `{ok:true}` with nothing to forward would tell the caller a stream is
  // running that can never deliver.
  const h = harness(t, { reply: { ok: true, result: {} } });
  const reply = await roundTrip(h.channel, {
    id: uuid(1),
    v: 1,
    method: "subscribe",
    params: { filter: { kinds: [9] } },
  });
  assert.equal(reply.ok, false);
  assert.equal(reply.error.code, "internal");
});

test("teardown closes every live subscription on both sides", async (t) => {
  const sub = "sub-1";
  const h = harness(t, { reply: { ok: true, result: { sub } } });
  await openSubscription(h, uuid(1), sub);
  await waitFor(() => h.listening(), "the stream listener");

  const seen = collect(h.channel);
  h.handle.dispose();
  await waitFor(() => seen.length >= 1, "the teardown close frame");
  assert.deepEqual(
    seen[0],
    { sub, kind: "closed", reason: "unsubscribed" },
    "the extension is told the stream ended",
  );
  await waitFor(
    () => h.calls.some((c) => c.method === "unsubscribe"),
    "the host-side release",
  );
  assert.equal(h.unlistened(), 1, "and the stream listener is removed");
});

test("a frame already in flight at teardown is not delivered", async (t) => {
  // Removing the listener does not recall a frame the event bus has already
  // dispatched, so nothing may be delivered after teardown.
  //
  // **This row does not isolate which mechanism refuses it, and says so.**
  // Teardown drains every sub (so the liveness check would refuse) *and* closes
  // the port (so `postMessage` is a silent no-op). Deleting the liveness check
  // leaves this row green, which is how the battery caught the over-claim in
  // the comment that used to live here. The liveness check is isolated by
  // `closed is delivered, and nothing follows it`, where the port is still
  // open. What this row proves is the end-to-end property: after teardown, no
  // frame reaches the extension.
  const sub = "sub-1";
  const h = harness(t, { reply: { ok: true, result: { sub } } });
  await openSubscription(h, uuid(1), sub);
  await waitFor(() => h.listening(), "the stream listener");

  const seen = collect(h.channel);
  h.handle.dispose();
  await waitFor(() => seen.length === 1, "the teardown close frame");

  // `rawEmit` bypasses the stand-in's unlisten, which is the whole point: the
  // handler itself must refuse, with no listener removal to hide behind.
  h.rawEmit({ lease: LEASE, frame: { sub, kind: "eose" } });
  await new Promise((resolve) => setTimeout(resolve, 20));
  assert.equal(seen.length, 1, "only the teardown close — nothing after it");
});

test("the TS per-sub queue fails closed before a third undrained batch", async (t) => {
  const sub = "sub-ts-queue";
  const h = harness(t, { reply: { ok: true, result: { sub } } });
  await openSubscription(h, uuid(720), sub);
  await waitFor(() => h.listening(), "listener");
  h.channel.port2.onmessage = () => {};
  assert.equal(MAX_TS_QUEUED_BATCHES_PER_SUB, 2);
  for (let i = 0; i < 3; i += 1) {
    h.emit({ lease: LEASE, frame: { sub, kind: "eose", ordinal: i } });
  }
  await waitFor(
    () => h.controls.some((entry) => entry.action === "violation"),
    "the bounded queue violation",
  );
  assert.equal(
    h.controls.filter((entry) => entry.action === "violation").length,
    1,
    "one terminal violation, not one per queued frame",
  );
});

test("the TS per-port byte bound fires before the batch-count bound", async (t) => {
  let minted = 0;
  const h = harness(t, {
    call: (_lease, _v, method) => {
      if (method === "subscribe") {
        minted += 1;
        return Promise.resolve({
          ok: true,
          result: { sub: `bytes-${minted}` },
        });
      }
      return Promise.resolve({ ok: true, result: {} });
    },
  });
  const subs = [];
  for (let n = 1; n <= 3; n += 1) {
    subs.push(await openSubscription(h, uuid(730 + n), `bytes-${n}`));
  }
  h.channel.port2.onmessage = () => {};
  const content = "x".repeat(600 * 1024);
  let emitted = 0;
  for (const sub of subs) {
    for (let seq = 1; seq <= 2; seq += 1) {
      const frame = { sub, kind: "event", event: { content } };
      const encodedBytes = new TextEncoder().encode(
        JSON.stringify(frame),
      ).byteLength;
      assert.ok(encodedBytes < 640 * 1024);
      h.emit({
        generation: LEASE,
        sub,
        seq,
        token: uuid(950000 + emitted),
        frames: [frame],
        frameCount: 1,
        encodedBytes,
        terminal: false,
      });
      emitted += 1;
    }
  }
  assert.ok(5 * 600 * 1024 < MAX_TS_QUEUED_BYTES_PER_PORT);
  assert.ok(6 * 600 * 1024 > MAX_TS_QUEUED_BYTES_PER_PORT);
  await waitFor(
    () => h.controls.some((entry) => entry.action === "violation"),
    "the per-port byte refusal",
  );
  assert.equal(emitted, 6, "below the eight-batch port count ceiling");
});

test("a foreign-generation ACK is ignored before an exact one is forwarded", async (t) => {
  const sub = "sub-foreign-ack";
  const h = harness(t, { reply: { ok: true, result: { sub } } });
  await openSubscription(h, uuid(729), sub);
  const ack = {
    buzz: "stream-ack",
    generation: uuid(42),
    sub,
    seq: 1,
    token: uuid(998),
    frameCount: 1,
    encodedBytes: 17,
  };
  h.channel.port2.postMessage(ack);
  await new Promise((resolve) => setTimeout(resolve, 20));
  assert.equal(h.controls.filter((entry) => entry.action === "ack").length, 0);
  h.channel.port2.postMessage({ ...ack, generation: LEASE });
  await waitFor(
    () => h.controls.filter((entry) => entry.action === "ack").length === 1,
    "the exact-generation ACK",
  );
});

test("ACK controls are internal and do not consume a request id", async (t) => {
  const sub = "sub-ack-control";
  const registry = createRegistry();
  const h = harness(t, {
    registry,
    reply: { ok: true, result: { sub } },
  });
  await openSubscription(h, uuid(721), sub);
  const before = registry.inFlight();
  const ack = {
    buzz: "stream-ack",
    generation: LEASE,
    sub,
    seq: 1,
    token: uuid(999),
    frameCount: 1,
    encodedBytes: 17,
  };
  h.channel.port2.postMessage(ack);
  h.channel.port2.postMessage(ack);
  await waitFor(
    () => h.controls.filter((entry) => entry.action === "ack").length === 2,
    "both exact controls to reach Rust",
  );
  assert.equal(registry.inFlight(), before, "no public request-id budget used");
});
