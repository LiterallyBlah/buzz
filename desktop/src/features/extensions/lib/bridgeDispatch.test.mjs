import assert from "node:assert/strict";
import test from "node:test";

import { startBridgeDispatch } from "./bridgeDispatch.ts";
import {
  createRegistry,
  MAX_IN_FLIGHT,
  MAX_REQUESTS_PER_PORT,
} from "./bridgeRegistry.ts";

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
    registry,
  } = options;
  const channel = new MessageChannel();
  const calls = [];
  const record = (lease, v, method) => {
    calls.push({ lease, v, method });
    return call ? call(lease, v, method) : Promise.resolve(reply);
  };
  let closeCount = 0;
  const realClose = channel.port1.close.bind(channel.port1);
  channel.port1.close = () => {
    closeCount += 1;
    realClose();
  };

  const handle = startBridgeDispatch({
    port: channel.port1,
    lease: LEASE,
    call: record,
    registry,
  });
  t.after(() => {
    handle.dispose();
    channel.port1.close();
    channel.port2.close();
  });
  return { channel, calls, handle, closeCount: () => closeCount };
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
  channel.port2.onmessage = (event) => seen.push(event.data);
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

// ── the lease is the host's ──────────────────────────────────────────────────

test("a host-minted lease is required", () => {
  const channel = new MessageChannel();
  try {
    assert.throws(
      () => startBridgeDispatch({ port: channel.port1, lease: "lease-a" }),
      /uuid/,
      "a malformed lease is our bug and must not be served with",
    );
  } finally {
    channel.port1.close();
    channel.port2.close();
  }
});

test("the lease is the host's, never the caller's", async (t) => {
  const { channel, calls } = harness(t);
  await roundTrip(channel, {
    id: uuid(1),
    v: 1,
    method: "identity.getPublicKey",
  });
  assert.equal(calls.length, 1);
  assert.equal(calls[0].lease, LEASE);
});

test("a well-formed request is forwarded and its reply correlated", async (t) => {
  const { channel, calls } = harness(t);
  const reply = await roundTrip(channel, {
    id: uuid(1),
    v: 1,
    method: "identity.getPublicKey",
  });
  assert.deepEqual(calls, [
    { lease: LEASE, v: 1, method: "identity.getPublicKey" },
  ]);
  assert.equal(reply.id, uuid(1));
  assert.equal(reply.ok, true);
});

// ── admission ────────────────────────────────────────────────────────────────

test("an 8 MiB ArrayBuffer never reaches the host", async (t) => {
  // End to end: the frame is refused with a correlated `invalid_params` and
  // no IPC call is made.
  const { channel, calls } = harness(t);
  const reply = await roundTrip(channel, {
    id: uuid(1),
    v: 1,
    method: "identity.getPublicKey",
    params: { blob: new ArrayBuffer(8 * 1024 * 1024) },
  });
  assert.equal(reply.id, uuid(1));
  assert.equal(reply.error.code, "invalid_params");
  assert.equal(calls.length, 0, "an oversized frame must not be dispatched");
});

test("a duplicate request id is refused, before and after completion", async (t) => {
  const { channel, calls } = harness(t);

  const first = await roundTrip(channel, {
    id: uuid(1),
    v: 1,
    method: "identity.getPublicKey",
  });
  assert.equal(first.ok, true);

  // The id has completed. Replaying it must still be refused — dedup covers
  // the port's whole life, not just the in-flight window.
  const replay = await roundTrip(channel, {
    id: uuid(1),
    v: 1,
    method: "identity.getPublicKey",
  });
  assert.equal(replay.id, uuid(1));
  assert.equal(replay.ok, false);
  assert.equal(replay.error.code, "invalid_params");
  assert.equal(calls.length, 1, "the replay must not reach the host");
});

test("the in-flight ceiling is enforced before the IPC call", async (t) => {
  // Bounding frame size without bounding concurrency closes one tap and leaves
  // the bath running: each call can open a SQLite connection host-side.
  const pending = [];
  const { channel, calls } = harness(t, {
    call: () => new Promise((resolve) => pending.push(resolve)),
  });

  const seen = collect(channel);
  for (let n = 0; n < MAX_IN_FLIGHT + 5; n += 1) {
    channel.port2.postMessage({
      id: uuid(n),
      v: 1,
      method: "identity.getPublicKey",
    });
  }
  await waitFor(() => seen.length === 5, "the five refusals");

  assert.equal(
    calls.length,
    MAX_IN_FLIGHT,
    "exactly the ceiling reached the host",
  );
  for (const refusal of seen) {
    assert.equal(refusal.ok, false);
    assert.equal(refusal.error.code, "rate_limited");
  }

  for (const resolve of pending) {
    resolve({ ok: true, result: {} });
  }
  await waitFor(
    () => seen.length === 5 + MAX_IN_FLIGHT,
    "the admitted replies",
  );
});

// ── teardown ─────────────────────────────────────────────────────────────────

test("an in-flight request is settled at teardown, not silently dropped", async (t) => {
  // The caller-visible no-hang property. Suppressing the write leaves the
  // extension's promise pending for the life of the frame, which it cannot
  // distinguish from a host still working.
  const { channel, handle, calls } = harness(t, {
    call: () => new Promise(() => {}), // never resolves
  });

  const seen = collect(channel);
  channel.port2.postMessage({
    id: uuid(1),
    v: 1,
    method: "identity.getPublicKey",
  });
  await waitFor(() => calls.length === 1, "the request to reach the host");

  handle.dispose();
  await waitFor(() => seen.length === 1, "the teardown settlement");

  assert.equal(seen[0].id, uuid(1), "the settlement is correlated");
  assert.equal(seen[0].ok, false);
  assert.equal(seen[0].error.code, "internal");
});

test("every outstanding request is settled, not just the first", async (t) => {
  const { channel, handle, calls } = harness(t, {
    call: () => new Promise(() => {}),
  });

  const seen = collect(channel);
  for (let n = 0; n < 5; n += 1) {
    channel.port2.postMessage({
      id: uuid(n),
      v: 1,
      method: "identity.getPublicKey",
    });
  }
  await waitFor(() => calls.length === 5, "all five to reach the host");

  handle.dispose();
  await waitFor(() => seen.length === 5, "five settlements");
  assert.deepEqual(
    seen.map((r) => r.id).sort(),
    [uuid(0), uuid(1), uuid(2), uuid(3), uuid(4)].sort(),
  );
});

test("a completion after teardown cannot produce a second result", async (t) => {
  let release;
  const { channel, handle, calls } = harness(t, {
    call: () =>
      new Promise((resolve) => {
        release = resolve;
      }),
  });

  const seen = collect(channel);
  channel.port2.postMessage({
    id: uuid(1),
    v: 1,
    method: "identity.getPublicKey",
  });
  await waitFor(() => calls.length === 1, "the request to reach the host");

  handle.dispose();
  await waitFor(() => seen.length === 1, "the teardown settlement");

  release({ ok: true, result: { pubkey: "a".repeat(64) } });
  await new Promise((resolve) => setTimeout(resolve, 50));

  assert.equal(
    seen.length,
    1,
    "the late completion must not emit a second reply for the same id",
  );
  assert.equal(seen[0].error.code, "internal");
});

test("dispose is idempotent and serves nothing afterwards", async (t) => {
  const { channel, calls, handle } = harness(t);
  handle.dispose();
  handle.dispose();

  const reply = await roundTrip(
    channel,
    { id: uuid(1), v: 1, method: "identity.getPublicKey" },
    100,
  );
  assert.equal(reply, null);
  assert.equal(calls.length, 0);
});

// ── correlation ──────────────────────────────────────────────────────────────

test("replies do not cross-talk between concurrent requests", async (t) => {
  const pending = new Map();
  const { channel } = harness(t, {
    call: (_lease, _v, method) =>
      new Promise((resolve) => pending.set(method, resolve)),
  });

  const seen = collect(channel);
  channel.port2.postMessage({ id: uuid(1), v: 1, method: "a.one" });
  channel.port2.postMessage({ id: uuid(2), v: 1, method: "a.two" });
  await waitFor(() => pending.size === 2, "both requests to reach the host");

  pending.get("a.two")({ ok: true, result: { which: "two" } });
  await waitFor(() => seen.length === 1, "the second request's reply");
  pending.get("a.one")({ ok: true, result: { which: "one" } });
  await waitFor(() => seen.length === 2, "the first request's reply");

  assert.equal(seen[0].id, uuid(2), "replies follow completion order");
  const byId = new Map(seen.map((r) => [r.id, r]));
  assert.deepEqual(byId.get(uuid(1)).result, { which: "one" });
  assert.deepEqual(byId.get(uuid(2)).result, { which: "two" });
});

test("an IPC failure still settles the request", async (t) => {
  const { channel } = harness(t, {
    call: () => Promise.reject(new Error("ipc exploded")),
  });
  const reply = await roundTrip(channel, {
    id: uuid(1),
    v: 1,
    method: "identity.getPublicKey",
  });
  assert.equal(reply.id, uuid(1));
  assert.equal(reply.error.code, "internal");
});

test("an uncorrelatable frame is dropped and a malformed one is answered", async (t) => {
  const { channel, calls } = harness(t);

  assert.equal(
    await roundTrip(channel, { v: 1, method: "m" }, 100),
    null,
    "no usable id: nothing to correlate a reply to",
  );

  const answered = await roundTrip(channel, {
    id: uuid(1),
    v: 1.5,
    method: "identity.getPublicKey",
  });
  assert.equal(answered.id, uuid(1));
  assert.equal(
    answered.error.code,
    "unsupported_version",
    "a numeric v that is not 1 is unsupported, whatever u32 can carry",
  );
  assert.equal(calls.length, 0);
});

test("exhausting the budget tears the port down and settles what is in flight", async (t) => {
  // The refusal is not the end of it: the port can never serve again, so
  // leaving it open would mean the extension talks to a channel that will
  // never answer. Everything outstanding is settled and the dispatcher stops.
  const registry = createRegistry();
  const spent = (n) => `3f2504e0-4f89-41d3-9a0c-${String(n).padStart(12, "b")}`;
  for (let n = 0; n < MAX_REQUESTS_PER_PORT - 1; n += 1) {
    registry.admit(spent(n));
    registry.settle(spent(n));
  }

  const { channel, calls } = harness(t, {
    registry,
    call: () => new Promise(() => {}), // never resolves
  });
  const seen = collect(channel);

  // Takes the last slot in the budget, and stays outstanding.
  channel.port2.postMessage({
    id: uuid(1),
    v: 1,
    method: "identity.getPublicKey",
  });
  await waitFor(() => calls.length === 1, "the in-flight request");

  // Exhausts it.
  channel.port2.postMessage({
    id: uuid(2),
    v: 1,
    method: "identity.getPublicKey",
  });
  await waitFor(() => seen.length === 2, "the refusal and the settlement");

  const byId = new Map(seen.map((r) => [r.id, r]));
  assert.equal(byId.get(uuid(2)).error.code, "quota_exceeded");
  assert.equal(
    byId.get(uuid(1)).error.code,
    "internal",
    "the in-flight request must be settled, not abandoned",
  );

  // And the port is done: nothing further is served.
  const after = await roundTrip(
    channel,
    { id: uuid(3), v: 1, method: "identity.getPublicKey" },
    100,
  );
  assert.equal(after, null, "an exhausted port must not serve again");
  assert.equal(calls.length, 1);
});

test("terminal exhaustion closes the host port, exactly once", async (t) => {
  // Removing the listener is not the same as closing. An open but unserved
  // channel accepts a later request and never answers it — the hang the
  // terminal contract exists to remove, merely arriving after a warning.
  const registry = createRegistry();
  const spent = (n) => `3f2504e0-4f89-41d3-9a0c-${String(n).padStart(12, "b")}`;
  for (let n = 0; n < MAX_REQUESTS_PER_PORT - 1; n += 1) {
    registry.admit(spent(n));
    registry.settle(spent(n));
  }

  let release;
  const { channel, calls, handle, closeCount } = harness(t, {
    registry,
    call: () =>
      new Promise((resolve) => {
        release = resolve;
      }),
  });
  const seen = collect(channel);

  // Takes the last budget slot and stays outstanding.
  channel.port2.postMessage({
    id: uuid(1),
    v: 1,
    method: "identity.getPublicKey",
  });
  await waitFor(() => calls.length === 1, "the in-flight request");
  assert.equal(closeCount(), 0, "nothing is closed before exhaustion");

  // Exhausts the budget.
  channel.port2.postMessage({
    id: uuid(2),
    v: 1,
    method: "identity.getPublicKey",
  });
  await waitFor(() => seen.length === 2, "the refusal and the settlement");

  // 1. The port is closed, exactly once.
  assert.equal(closeCount(), 1, "terminal exhaustion must close the port");

  // 2. The quota reply and the outstanding settlement are still delivered.
  const byId = new Map(seen.map((r) => [r.id, r]));
  assert.equal(byId.get(uuid(2)).error.code, "quota_exceeded");
  assert.equal(
    byId.get(uuid(1)).error.code,
    "internal",
    "the in-flight request is settled, not abandoned",
  );

  // 3. A later request receives nothing at all.
  channel.port2.postMessage({
    id: uuid(3),
    v: 1,
    method: "identity.getPublicKey",
  });
  await new Promise((resolve) => setTimeout(resolve, 60));
  assert.equal(seen.length, 2, "a closed port answers nothing further");
  assert.equal(calls.length, 1, "and dispatches nothing further");

  // 4. A late completion from before the close cannot emit.
  release({ ok: true, result: { pubkey: "a".repeat(64) } });
  await new Promise((resolve) => setTimeout(resolve, 60));
  assert.equal(
    seen.length,
    2,
    "a late completion must not emit a second result",
  );

  // 5. Ordinary cleanup stays idempotent — no second close.
  handle.dispose();
  handle.dispose();
  assert.equal(
    closeCount(),
    1,
    "dispose after exhaustion must not close again",
  );
});

test("ordinary teardown closes the port once", async (t) => {
  const { channel, handle, closeCount } = harness(t);
  await roundTrip(channel, {
    id: uuid(1),
    v: 1,
    method: "identity.getPublicKey",
  });
  assert.equal(closeCount(), 0);

  handle.dispose();
  assert.equal(closeCount(), 1, "teardown closes the channel it served");
  handle.dispose();
  assert.equal(closeCount(), 1, "and is idempotent");
});
