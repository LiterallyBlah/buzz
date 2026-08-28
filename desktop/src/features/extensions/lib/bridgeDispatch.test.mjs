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
    lease = LEASE,
  } = options;
  const channel = new MessageChannel();
  const calls = [];
  const record = (lease, v, method, params) => {
    calls.push({ lease, v, method, params });
    return call ? call(lease, v, method, params) : Promise.resolve(reply);
  };
  let closeCount = 0;
  const realClose = channel.port1.close.bind(channel.port1);
  channel.port1.close = () => {
    closeCount += 1;
    realClose();
  };

  // A stand-in for Tauri's event `listen`, so the host stream can be driven
  // without an IPC bridge. `emit` is what a test uses to play the host.
  let handler = null;
  let unlistened = 0;
  const listen = (_event, fn) => {
    handler = fn;
    raw = fn;
    return Promise.resolve(() => {
      unlistened += 1;
      handler = null;
    });
  };
  const emit = (payload) => {
    if (!handler) {
      throw new Error("no stream listener is installed");
    }
    handler({ payload });
  };
  // Keeps working after unlisten, so a test can play the one case the real
  // event bus produces and the stand-in otherwise cannot: a frame already
  // dispatched when the listener was removed.
  let raw = null;
  const rawEmit = (payload) => raw({ payload });

  const handle = startBridgeDispatch({
    port: channel.port1,
    lease,
    call: record,
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
    { lease: LEASE, v: 1, method: "identity.getPublicKey", params: undefined },
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

test("a publish template reaches the host byte-for-byte", async (t) => {
  // §4's template rides `params`. The dispatcher bounds and type-checks the
  // frame but must not reshape the template — the signer checks the canonical
  // event it will actually sign, and a rewrite here would be a second opinion
  // for the two to disagree over.
  const { channel, calls } = harness(t);

  const template = {
    kind: 9,
    content: "hello from an extension",
    tags: [
      ["h", "11111111-2222-3333-4444-555555555555"],
      ["p", "a".repeat(64)],
      ["e", "b".repeat(64)],
    ],
    created_at: 1700000000,
  };
  await roundTrip(channel, {
    id: uuid(1),
    v: 1,
    method: "publish.event",
    params: template,
  });

  assert.equal(calls.length, 1, "a legitimate template must reach the host");
  assert.deepEqual(
    calls[0].params,
    template,
    "and must arrive exactly as sent",
  );
});

test("a hostile template is refused before the signer sees it", async (t) => {
  // The bound has to hold on the publish path specifically, since that is the
  // one that reaches a key. Each of these is refused by the frame validator,
  // so `calls` never grows.
  const { channel, calls } = harness(t);

  const cyclic = { kind: 9, content: "x" };
  cyclic.self = cyclic;

  for (const [label, params] of [
    [
      "an ArrayBuffer smuggled in a tag",
      { kind: 9, blob: new ArrayBuffer(8 * 1024 * 1024) },
    ],
    [
      "an oversized content string",
      { kind: 9, content: "x".repeat(70 * 1024) },
    ],
    ["a cyclic template", cyclic],
    ["a Map instead of a tag array", { kind: 9, tags: new Map() }],
  ]) {
    const reply = await roundTrip(channel, {
      id: uuid(2),
      v: 1,
      method: "publish.event",
      params,
    });
    assert.equal(
      reply.error.code,
      "invalid_params",
      `${label} must be refused`,
    );
  }

  assert.equal(calls.length, 0, "nothing hostile may reach the signer");
});

// ── proof 8R: the raw-wire contract cannot acquire a host-minted identity ────

test("8R: the frame layer forwards a missing created_at without inventing one", async (t) => {
  // Part of 8R, and **only** this part: the frame layer must not invent an
  // operation identity on the caller's behalf.
  //
  // The title used to say "is refused, and nothing is dispatched" — which this
  // test never observed. The injected host call returns the harness's ordinary
  // success, so `calls` is deliberately non-empty and no `invalid_params` is
  // reached from here. The refusal is Rust's, and it is proved in Rust by
  // `a_missing_created_at_is_refused_before_signing_or_network`. Claiming it
  // here would have been a title asserting more than its body.
  const { channel, calls } = harness(t);

  for (const [label, params] of [
    ["omitted", { kind: 9, content: "hi", tags: [["h", "c"]] }],
    ["null", { kind: 9, content: "hi", tags: [["h", "c"]], created_at: null }],
  ]) {
    const reply = await roundTrip(channel, {
      id: uuid(1),
      v: 1,
      method: "publish.event",
      params,
    });
    // The frame itself is well formed, so it is admitted and answered — the
    // refusal is the *host's*, correlated to the request.
    assert.notEqual(reply, null, `${label} must be answered, not dropped`);
    assert.equal(reply.id, uuid(1));
  }

  // The template *does* reach the host: rejecting a missing `created_at` is
  // Rust's decision, not the validator's, and a validator that began
  // interpreting templates would be a second opinion for the two to disagree
  // over. What this pins is that the frame layer added nothing on the way.
  for (const call of calls) {
    assert.equal(
      Object.hasOwn(call.params, "created_at") &&
        call.params.created_at !== null &&
        call.params.created_at !== undefined,
      false,
      "the frame layer must not invent a created_at on the way through",
    );
  }
});

test("8R: two sends of one template are byte-identical at the signer boundary", async (t) => {
  // Part 3. The operation identity is the *complete* canonical projection, so
  // this asserts every field that feeds the event id — value, count and order —
  // survives the wire path unchanged on both sends.
  const { channel, calls } = harness(t);

  const template = {
    kind: 9,
    content: "the same logical publish, sent twice",
    tags: [
      ["h", "11111111-2222-3333-4444-555555555555"],
      ["p", "a".repeat(64)],
      ["t", "topic"],
    ],
    created_at: 1700000000,
  };

  // A caller retaining and resubmitting the exact template — the v1 contract.
  await roundTrip(channel, {
    id: uuid(1),
    v: 1,
    method: "publish.event",
    params: template,
  });
  await roundTrip(channel, {
    id: uuid(2),
    v: 1,
    method: "publish.event",
    params: template,
  });

  assert.equal(calls.length, 2, "both sends reach the host");
  const [first, second] = calls.map((c) => c.params);

  assert.equal(first.created_at, template.created_at, "created_at preserved");
  assert.equal(second.created_at, template.created_at, "and on the retry");
  assert.equal(first.kind, second.kind);
  assert.equal(first.content, second.content);
  assert.equal(first.tags.length, second.tags.length, "tag count preserved");
  assert.deepEqual(first.tags, second.tags, "tag values and order preserved");
  // The decisive one: the two are indistinguishable, so Rust rebuilds the same
  // canonical event and therefore the same id.
  assert.equal(
    JSON.stringify(first),
    JSON.stringify(second),
    "the two sends must be byte-identical at the boundary",
  );
});

test("8R: no frontend path inserts, defaults or clamps created_at", async (t) => {
  // Part 4, on the frontend half. The Rust half is pinned by
  // `the_host_never_originates_a_created_at`.
  //
  // Behavioural first: a template with an absurd timestamp crosses untouched —
  // the frontend has no opinion about the window, so it cannot silently move a
  // value the caller will retry with.
  const { channel, calls } = harness(t);
  const absurd = { kind: 9, content: "x", tags: [], created_at: 1 };
  await roundTrip(channel, {
    id: uuid(1),
    v: 1,
    method: "publish.event",
    params: absurd,
  });

  assert.equal(calls.length, 1);
  assert.equal(
    calls[0].params.created_at,
    1,
    "the frontend must pass the caller's timestamp through unchanged, however wrong",
  );
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
