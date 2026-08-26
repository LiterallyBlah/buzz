import assert from "node:assert/strict";
import test from "node:test";

import {
  checkFrame,
  isUuid,
  MAX_DEPTH,
  MAX_FRAME_BYTES,
  MAX_METHOD_BYTES,
  MAX_NODES,
  MAX_STRING_BYTES,
  utf8Length,
} from "./bridgeFrame.ts";

const ID = "3f2504e0-4f89-41d3-9a0c-0305e82c3301";

/** A well-formed frame with `params` replaced by whatever is under test. */
function frameWith(params) {
  return { id: ID, v: 1, method: "identity.getPublicKey", params };
}

function refusalFor(params) {
  const checked = checkFrame(frameWith(params));
  assert.equal(
    checked.kind,
    "refuse",
    `expected a refusal, got ${checked.kind}`,
  );
  assert.equal(checked.id, ID, "a refusal must stay correlated");
  assert.equal(checked.code, "invalid_params");
  return checked;
}

// ── the structured-clone seam ────────────────────────────────────────────────

test("an 8 MiB ArrayBuffer is rejected, not dispatched", () => {
  // The counterexample that motivated this module. `JSON.stringify` renders an
  // ArrayBuffer as `{}` — two characters — so a bound measured on the
  // serialised length reads 8 MiB of payload as nothing at all.
  const buffer = new ArrayBuffer(8 * 1024 * 1024);
  assert.equal(
    JSON.stringify({ blob: buffer }).length,
    11,
    "precondition: JSON.stringify really does flatten it to nothing",
  );
  refusalFor({ blob: buffer });
});

test("every structured-clone-only value the protocol forbids is rejected", () => {
  // Structured clone carries all of these; JSON carries none of them. Each
  // would otherwise arrive as `{}` or vanish.
  const cases = {
    arrayBuffer: new ArrayBuffer(8),
    uint8: new Uint8Array([1, 2, 3]),
    float64: new Float64Array([1.5]),
    dataView: new DataView(new ArrayBuffer(8)),
    map: new Map([["a", 1]]),
    set: new Set([1, 2]),
    date: new Date(0),
    regexp: /x/,
    error: new Error("nope"),
    bigint: 10n,
    fn: () => {},
    symbol: Symbol("s"),
    undef: undefined,
  };
  for (const [name, value] of Object.entries(cases)) {
    refusalFor({ [name]: value });
  }
});

test("a MessagePort in the frame is rejected", () => {
  const channel = new MessageChannel();
  try {
    refusalFor({ port: channel.port1 });
  } finally {
    channel.port1.close();
    channel.port2.close();
  }
});

test("a class instance is not a plain object", () => {
  class Sneaky {
    constructor() {
      this.looksPlain = true;
    }
  }
  refusalFor({ instance: new Sneaky() });
});

test("a null-prototype object is accepted as plain", () => {
  // `Object.create(null)` is a legitimate JSON-compatible bag and survives
  // structured clone; refusing it would be strictness without a reason.
  const bag = Object.create(null);
  bag.ok = true;
  assert.equal(checkFrame(frameWith(bag)).kind, "ok");
});

// ── cycles and shared references ─────────────────────────────────────────────

test("a cycle is rejected rather than traversed forever", () => {
  // Two guards cover this: the `seen` set refuses it as a repeat, and the node
  // budget would stop the walk regardless. That redundancy is deliberate — it
  // is why removing the `seen` set alone does not make this test fail, and why
  // the amplification property below is the one that pins it.
  const cyclic = { name: "loop" };
  cyclic.self = cyclic;
  refusalFor(cyclic);
});

test("a repeated reference is rejected because it amplifies the encoding", () => {
  // Not a cycle: legal JSON. But `JSON.stringify` expands the shared subtree
  // once per reference, so counting it once would make the byte budget
  // unsound — ten references to a large subtree encode ten times.
  const shared = { padding: "x".repeat(1000) };
  refusalFor({ a: shared, b: shared });
});

// ── size, depth and breadth ──────────────────────────────────────────────────

test("a frame over the byte budget is rejected", () => {
  refusalFor({ big: "x".repeat(MAX_FRAME_BYTES + 10) });
});

test("size is measured in UTF-8 bytes, not code units", () => {
  // Each of these is one UTF-16 code unit but three UTF-8 bytes. A frame of
  // them measures a third of its true size by `.length`, so a `.length`-based
  // bound would admit roughly three times the intended payload.
  const multibyte = "€";
  assert.equal(multibyte.length, 1);
  assert.equal(utf8Length(multibyte), 3);

  // Sized to isolate *this* bound: 10 000 code units is comfortably under the
  // per-string cap, but 30 000 bytes is over it. Counting code units admits
  // the string; counting bytes refuses it. The frame total stays under its own
  // budget either way, so nothing else can be the refuser.
  const underByCodeUnits = multibyte.repeat(10_000);
  assert.ok(
    underByCodeUnits.length < MAX_STRING_BYTES,
    "precondition: a code-unit count would admit this string",
  );
  assert.ok(
    utf8Length(underByCodeUnits) > MAX_STRING_BYTES,
    "precondition: a byte count must refuse it",
  );
  assert.ok(
    utf8Length(underByCodeUnits) < MAX_FRAME_BYTES,
    "precondition: the frame budget must not be what refuses it",
  );
  refusalFor({ text: underByCodeUnits });
});

test("an astral character counts as four bytes", () => {
  assert.equal(utf8Length("😀"), 4);
  assert.equal("😀".length, 2, "a surrogate pair is two code units");
});

test("a deeply nested frame is rejected without overflowing the stack", () => {
  // Deep, but deliberately *inside* the node and byte budgets so the depth
  // limit is the only bound that can refuse it. A 100 000-deep frame would be
  // caught by the node budget instead, which would leave the depth check
  // untested — and a recursive validator would overflow before reporting.
  let deep = {};
  const root = deep;
  for (let i = 0; i < 2000; i += 1) {
    deep.next = {};
    deep = deep.next;
  }
  refusalFor(root);
});

test("a frame at the depth limit is accepted", () => {
  let node = {};
  const root = node;
  // params sits one level in, so build to just inside the budget.
  for (let i = 0; i < MAX_DEPTH - 3; i += 1) {
    node.next = {};
    node = node.next;
  }
  assert.equal(checkFrame(frameWith(root)).kind, "ok");
});

test("a very wide frame is rejected on node count", () => {
  // Many values, few bytes: an array of nulls encodes to about 50 KiB, inside
  // the frame budget, so the node budget is the only bound that can refuse it.
  // Keyed objects would blow the byte budget first and leave this untested.
  const wide = new Array(MAX_NODES + 10).fill(null);
  refusalFor(wide);
});

test("an oversized single string is rejected", () => {
  refusalFor({ text: "x".repeat(MAX_STRING_BYTES + 1) });
});

test("a non-finite number is rejected rather than silently nulled", () => {
  // `JSON.stringify` turns these into `null`, which would change the value the
  // host acts on without anyone noticing.
  for (const value of [Number.NaN, Number.POSITIVE_INFINITY, -Infinity]) {
    refusalFor({ value });
  }
});

// ── the frame envelope ───────────────────────────────────────────────────────

test("a frame with no usable id is dropped, not refused", () => {
  for (const data of [
    null,
    "string",
    42,
    [],
    {},
    { id: 7, v: 1, method: "m" },
    { id: "", v: 1, method: "m" },
    { id: "not-a-uuid", v: 1, method: "m" },
    { v: 1, method: "m" },
  ]) {
    assert.equal(
      checkFrame(data).kind,
      "drop",
      `${JSON.stringify(data)} is uncorrelatable and must be dropped`,
    );
  }
});

test("an unrecognised top-level field makes the frame malformed", () => {
  const checked = checkFrame({
    id: ID,
    v: 1,
    method: "identity.getPublicKey",
    extensionId: "someone-else",
  });
  assert.equal(checked.kind, "refuse");
  assert.equal(checked.code, "invalid_params");
});

test("version classification", () => {
  const ok = (v) => checkFrame({ id: ID, v, method: "m" }).kind;
  // Representable integers are forwarded; Rust decides which it supports.
  assert.equal(ok(1), "ok");
  assert.equal(ok(0), "ok");
  assert.equal(ok(99), "ok");
  assert.equal(ok(0xff_ff_ff_ff), "ok");
  // Anything Rust's u32 cannot hold is settled here, never left to become
  // `internal` on the far side of the IPC boundary.
  for (const v of [
    1.5,
    -1,
    Number.NaN,
    Number.POSITIVE_INFINITY,
    Number.NEGATIVE_INFINITY,
    0x1_00_00_00_00,
    Number.MAX_SAFE_INTEGER,
    "1",
    null,
    undefined,
  ]) {
    const checked = checkFrame({ id: ID, v, method: "m" });
    assert.equal(checked.kind, "refuse", `v=${String(v)} must be refused`);
    assert.equal(checked.code, "invalid_params");
  }
});

test("method must be a non-empty string within the cap", () => {
  const kind = (method) => checkFrame({ id: ID, v: 1, method }).kind;
  assert.equal(kind("m"), "ok");
  assert.equal(kind("m".repeat(MAX_METHOD_BYTES)), "ok");
  assert.equal(kind("m".repeat(MAX_METHOD_BYTES + 1)), "refuse");
  assert.equal(kind(""), "refuse");
  assert.equal(kind(9), "refuse");
  // Multibyte: 32 three-byte characters is 96 bytes, over the 64-byte cap
  // even though it is only 32 code units.
  assert.equal(kind("€".repeat(32)), "refuse");
});

// ── uuid grammar ─────────────────────────────────────────────────────────────

test("uuid grammar accepts real uuids and rejects near misses", () => {
  for (const good of [
    "3f2504e0-4f89-41d3-9a0c-0305e82c3301",
    "00000000-0000-0000-0000-000000000000",
    "3F2504E0-4F89-41D3-9A0C-0305E82C3301", // RFC 4122 permits upper case
  ]) {
    assert.ok(isUuid(good), `${good} is a uuid`);
  }
  for (const bad of [
    "",
    "3f2504e0-4f89-41d3-9a0c-0305e82c330", // one short
    "3f2504e0-4f89-41d3-9a0c-0305e82c33011", // one long
    "3f2504e04f8941d39a0c0305e82c3301", // no hyphens
    "3f2504e0-4f89-41d3-9a0c_0305e82c3301", // wrong separator
    "3f2504e0-4f89-41d3-9a0c-0305e82c330g", // non-hex
    " 3f2504e0-4f89-41d3-9a0c-0305e82c3301", // leading space
    "3f2504e0-4f89-41d3-9a0c-0305e82c3301 ", // trailing space
    "3f2504e0-4f89-41d3-9a0c-0305e82c3301\n", // trailing newline
    "3f2504e0-4f89-41d3--9a0c-0305e82c3301", // doubled hyphen
  ]) {
    assert.ok(!isUuid(bad), `${JSON.stringify(bad)} is not a uuid`);
  }
});

// ── the happy path still works ───────────────────────────────────────────────

test("an ordinary frame with a plain params tree is accepted", () => {
  const checked = checkFrame({
    id: ID,
    v: 1,
    method: "some.method",
    params: {
      kind: 9,
      content: "hello",
      tags: [["h", "channel-a"]],
      created_at: 1700000000,
      nested: { list: [1, 2, 3], flag: true, nothing: null },
    },
  });
  assert.equal(checked.kind, "ok");
  assert.equal(checked.frame.id, ID);
  assert.equal(checked.frame.method, "some.method");
  assert.equal(checked.frame.params.kind, 9);
});
