import assert from "node:assert/strict";
import test from "node:test";

import { startHostHandshake } from "./bridgeHandshake.ts";

/** A `message` event carrying the fields §2 attributes on. */
class FakeMessageEvent extends Event {
  constructor({ data, source, ports = [] }) {
    super("message");
    this.data = data;
    this.source = source;
    this.ports = ports;
  }
}

/**
 * A host view plus a frame whose `contentWindow` records what it was sent.
 *
 * `sent` is the whole observable surface: the handshake's only output is the
 * `{buzz:"port"}` message and the port in its transfer list.
 */
function harness() {
  const view = new EventTarget();
  const sent = [];
  const contentWindow = {
    postMessage: (data, targetOrigin, transfer) => {
      sent.push({ data, targetOrigin, transfer });
    },
  };
  const frame = { contentWindow };
  return { view, frame, contentWindow, sent };
}

function ready() {
  return { buzz: "ready" };
}

test("completes the handshake for the frame it created", () => {
  const { view, frame, contentWindow, sent } = harness();
  const handshake = startHostHandshake({ frame, view });

  view.dispatchEvent(
    new FakeMessageEvent({ data: ready(), source: contentWindow }),
  );

  assert.equal(sent.length, 1, "exactly one port message");
  assert.deepEqual(sent[0].data, { buzz: "port", v: 1 });
  assert.equal(sent[0].targetOrigin, "*");
  assert.equal(
    sent[0].transfer.length,
    1,
    "port2 travels in the transfer list",
  );
  assert.ok(handshake.port(), "the host retains port1");
  // The transferred port is the *other* end, never the one the host kept.
  assert.notEqual(sent[0].transfer[0], handshake.port());

  handshake.dispose();
});

test("ignores a ready from any source that is not the host's own frame", () => {
  const { view, frame, sent } = harness();
  const handshake = startHostHandshake({ frame, view });

  // The impersonator sends a byte-identical envelope. Only identity separates
  // it from the real frame — `event.origin` is "null" for both.
  const impostor = { postMessage: () => {} };
  view.dispatchEvent(new FakeMessageEvent({ data: ready(), source: impostor }));

  assert.equal(
    sent.length,
    0,
    "no port may be issued to an unattributed source",
  );
  assert.equal(handshake.port(), null);

  handshake.dispose();
});

test("ignores anything that is not the ready envelope", () => {
  const { view, frame, contentWindow, sent } = harness();
  const handshake = startHostHandshake({ frame, view });

  for (const data of [
    { buzz: "port", v: 1 },
    { buzz: "hello" },
    { ready: true },
    "ready",
    null,
    42,
  ]) {
    view.dispatchEvent(new FakeMessageEvent({ data, source: contentWindow }));
  }

  assert.equal(sent.length, 0, "only {buzz:'ready'} starts the handshake");
  assert.equal(handshake.port(), null);

  handshake.dispose();
});

test("accepts exactly one ready per frame", () => {
  const { view, frame, contentWindow, sent } = harness();
  const handshake = startHostHandshake({ frame, view });

  view.dispatchEvent(
    new FakeMessageEvent({ data: ready(), source: contentWindow }),
  );
  const first = handshake.port();
  view.dispatchEvent(
    new FakeMessageEvent({ data: ready(), source: contentWindow }),
  );
  view.dispatchEvent(
    new FakeMessageEvent({ data: ready(), source: contentWindow }),
  );

  assert.equal(sent.length, 1, "a repeat ready must not mint a second channel");
  assert.equal(handshake.port(), first, "the retained port is unchanged");

  handshake.dispose();
});

test("never adopts a port supplied by the frame side", () => {
  const { view, frame, contentWindow, sent } = harness();
  const handshake = startHostHandshake({ frame, view });

  // A hostile frame offers its own channel with the ready. If the host adopted
  // it, the frame would have chosen the channel the host then trusts.
  const offered = new MessageChannel();
  view.dispatchEvent(
    new FakeMessageEvent({
      data: ready(),
      source: contentWindow,
      ports: [offered.port2],
    }),
  );

  const held = handshake.port();
  assert.ok(held, "the host still originates its own channel");
  assert.notEqual(held, offered.port2, "the offered port must not be adopted");
  assert.notEqual(held, offered.port1);
  assert.equal(sent.length, 1);
  assert.notEqual(
    sent[0].transfer[0],
    offered.port2,
    "the port sent down is the host's, not the frame's",
  );

  offered.port1.close();
  offered.port2.close();
  handshake.dispose();
});

test("dispose stops listening and is idempotent", () => {
  const { view, frame, contentWindow, sent } = harness();
  const handshake = startHostHandshake({ frame, view });

  handshake.dispose();
  handshake.dispose();

  view.dispatchEvent(
    new FakeMessageEvent({ data: ready(), source: contentWindow }),
  );

  assert.equal(sent.length, 0, "a disposed handshake must not answer");
  assert.equal(handshake.port(), null);
});
