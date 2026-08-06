import assert from "node:assert/strict";
import { describe, it } from "node:test";
import {
  awaitCancelAllAcknowledgement,
  describeCancelAllRequest,
} from "./cancelAllOutcome.ts";

function bus() {
  const listeners = new Set();
  return {
    subscribe(listener) {
      listeners.add(listener);
      return () => listeners.delete(listener);
    },
    emit(frame) {
      for (const listener of listeners) listener(frame);
    },
  };
}

describe("awaitCancelAllAcknowledgement", () => {
  it("returns authoritative counts and ignores unrelated control results", async () => {
    const events = bus();
    const outcome = awaitCancelAllAcknowledgement({
      subscribe: events.subscribe,
      sendCancelAll: async () => {
        events.emit({ type: "drain", status: "draining" });
        events.emit({
          type: "cancel_all",
          status: "accepted",
          activeTurns: 2,
          signalledTurns: 1,
          queuedEvents: 3,
        });
      },
      scheduleTimeout: () => () => {},
    });
    assert.deepEqual(await outcome, {
      kind: "accepted",
      activeTurns: 2,
      signalledTurns: 1,
      queuedEvents: 3,
    });
  });

  it("distinguishes no work, silence, and send failure", async () => {
    const events = bus();
    assert.deepEqual(
      await awaitCancelAllAcknowledgement({
        subscribe: events.subscribe,
        sendCancelAll: async () =>
          events.emit({ type: "cancel_all", status: "no_work" }),
        scheduleTimeout: () => () => {},
      }),
      { kind: "no_work" },
    );
    assert.deepEqual(
      await awaitCancelAllAcknowledgement({
        subscribe: events.subscribe,
        sendCancelAll: async () => {},
        scheduleTimeout: (timeout) => {
          queueMicrotask(timeout);
          return () => {};
        },
      }),
      { kind: "no_acknowledgement" },
    );
    await assert.rejects(
      awaitCancelAllAcknowledgement({
        subscribe: events.subscribe,
        sendCancelAll: async () => {
          throw new Error("relay refused");
        },
        scheduleTimeout: () => () => {},
      }),
      /relay refused/,
    );
  });
});

describe("describeCancelAllRequest", () => {
  it("renders accepted work, no work, timeout, and send failure distinctly", () => {
    assert.equal(
      describeCancelAllRequest({
        phase: "settled",
        acknowledgement: {
          kind: "accepted",
          activeTurns: 2,
          signalledTurns: 1,
          queuedEvents: 3,
        },
      }).badge,
      "Stop requested for 1 active turn; 1 active turn was already stopping; 3 queued items discarded",
    );
    assert.equal(
      describeCancelAllRequest({
        phase: "settled",
        acknowledgement: { kind: "no_work" },
      }).badge,
      "No work was active",
    );
    assert.equal(
      describeCancelAllRequest({
        phase: "settled",
        acknowledgement: { kind: "no_acknowledgement" },
      }).badge,
      "Sent — no reply from the agent",
    );
    assert.match(
      describeCancelAllRequest({ phase: "failed", message: "relay refused" })
        .badge,
      /Could not stop current work/,
    );
  });
});
