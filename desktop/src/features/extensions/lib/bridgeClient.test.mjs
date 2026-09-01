import assert from "node:assert/strict";
import test from "node:test";

import { installBridgeStreamClient } from "./bridgeClient.ts";

const TOKEN = "11111111-1111-4111-8111-111111111111";
const frame = { sub: "s1", kind: "eose" };
const bytes = new TextEncoder().encode(JSON.stringify(frame)).byteLength;

function batch(overrides = {}) {
  return {
    buzz: "stream-batch",
    generation: "lease-1",
    sub: "s1",
    seq: 1,
    token: TOKEN,
    frames: [frame],
    frameCount: 1,
    encodedBytes: bytes,
    terminal: false,
    ...overrides,
  };
}

async function waitFor(predicate) {
  for (let i = 0; i < 100; i += 1) {
    if (predicate()) return;
    await new Promise((resolve) => setTimeout(resolve, 2));
  }
  throw new Error("timeout");
}

test("ACK is emitted only after the dequeued batch is adopted", async (t) => {
  const channel = new MessageChannel();
  t.after(() => {
    channel.port1.close();
    channel.port2.close();
  });
  const order = [];
  const stop = installBridgeStreamClient(channel.port2, (got) => {
    order.push(`frame:${got.kind}`);
  });
  t.after(stop);
  channel.port1.onmessage = (event) => order.push(`ack:${event.data.seq}`);
  channel.port1.postMessage(batch());
  await waitFor(() => order.length === 2);
  assert.deepEqual(order, ["frame:eose", "ack:1"]);
});

test("a stale or malformed batch is neither exposed nor acknowledged", async (t) => {
  const channel = new MessageChannel();
  t.after(() => {
    channel.port1.close();
    channel.port2.close();
  });
  const frames = [];
  const acks = [];
  const stop = installBridgeStreamClient(channel.port2, (got) =>
    frames.push(got),
  );
  t.after(stop);
  channel.port1.onmessage = (event) => acks.push(event.data);
  channel.port1.postMessage(batch({ encodedBytes: bytes + 1 }));
  channel.port1.postMessage(batch({ seq: 2 }));
  await new Promise((resolve) => setTimeout(resolve, 30));
  assert.deepEqual(frames, []);
  assert.deepEqual(acks, []);
});

test("a client callback failure withholds credit", async (t) => {
  const channel = new MessageChannel();
  t.after(() => {
    channel.port1.close();
    channel.port2.close();
  });
  const acks = [];
  const stop = installBridgeStreamClient(channel.port2, () => {
    throw new Error("extension rejected frame");
  });
  t.after(stop);
  channel.port1.onmessage = (event) => acks.push(event.data);
  channel.port1.postMessage(batch());
  await new Promise((resolve) => setTimeout(resolve, 30));
  assert.deepEqual(acks, [], "no ACK before complete adoption");
});
