import assert from "node:assert/strict";
import { describe, it } from "node:test";

import {
  awaitDrainAcknowledgement,
  describeDrainRequest,
} from "./drainOutcome.ts";

/**
 * A controllable stand-in for the observer relay: `emit` plays a
 * `control_result` frame to whoever is listening, and `listeners` exposes how
 * many subscriptions are outstanding so the cleanup assertions have something
 * to check.
 */
function fakeControlBus() {
  const listeners = new Set();
  return {
    subscribe(listener) {
      listeners.add(listener);
      return () => listeners.delete(listener);
    },
    emit(frame) {
      for (const listener of [...listeners]) {
        listener(frame);
      }
    },
    get listenerCount() {
      return listeners.size;
    },
  };
}

/** A timeout that only fires when the test says so. */
function fakeTimer() {
  let pending = null;
  return {
    schedule(onTimeout) {
      pending = onTimeout;
      return () => {
        pending = null;
      };
    },
    fire() {
      const callback = pending;
      pending = null;
      callback?.();
    },
    get scheduled() {
      return pending !== null;
    },
  };
}

describe("awaitDrainAcknowledgement", () => {
  it("resolves with the agent's own acknowledgement", async () => {
    const bus = fakeControlBus();
    const timer = fakeTimer();

    const outcome = awaitDrainAcknowledgement({
      subscribe: bus.subscribe,
      sendDrain: async () => {},
      scheduleTimeout: timer.schedule,
    });
    bus.emit({ type: "drain", status: "draining" });

    assert.equal(await outcome, "draining");
  });

  it("reports a repeat drain as already draining, not as a fresh one", async () => {
    const bus = fakeControlBus();
    const timer = fakeTimer();

    const outcome = awaitDrainAcknowledgement({
      subscribe: bus.subscribe,
      sendDrain: async () => {},
      scheduleTimeout: timer.schedule,
    });
    bus.emit({ type: "drain", status: "already_draining" });

    assert.equal(await outcome, "already_draining");
  });

  /**
   * **The race this ordering exists to lose.** An idle agent acks inside the
   * same round trip, so a listener registered after the send would miss it.
   * The frame here is emitted *while the send is still in flight*, which is
   * only observable if `subscribe` already ran.
   */
  it("hears an ack that arrives before the send resolves", async () => {
    const bus = fakeControlBus();
    const timer = fakeTimer();

    const outcome = awaitDrainAcknowledgement({
      subscribe: bus.subscribe,
      sendDrain: async () => {
        assert.equal(
          bus.listenerCount,
          1,
          "the listener must be registered before the frame is published",
        );
        bus.emit({ type: "drain", status: "draining" });
      },
      scheduleTimeout: timer.schedule,
    });

    assert.equal(await outcome, "draining");
  });

  it("reports no acknowledgement when the agent never answers", async () => {
    const bus = fakeControlBus();
    const timer = fakeTimer();

    const outcome = awaitDrainAcknowledgement({
      subscribe: bus.subscribe,
      sendDrain: async () => {},
      scheduleTimeout: timer.schedule,
    });
    timer.fire();

    assert.equal(await outcome, "no_acknowledgement");
  });

  /**
   * A `cancel_turn` or `switch_model` result can be in flight for the same
   * agent at the same moment. Reading one as a drain ack would tell the owner
   * their agent is draining when nothing of the sort happened.
   */
  it("ignores control results for other commands", async () => {
    const bus = fakeControlBus();
    const timer = fakeTimer();

    const outcome = awaitDrainAcknowledgement({
      subscribe: bus.subscribe,
      sendDrain: async () => {},
      scheduleTimeout: timer.schedule,
    });
    bus.emit({ type: "switch_model", status: "switched", modelId: "gpt-5" });
    bus.emit({ type: "cancel_turn", status: "cancelled" });
    timer.fire();

    assert.equal(await outcome, "no_acknowledgement");
  });

  /**
   * Forward compatibility, in the safe direction: a status this desktop does
   * not know must never be optimistically read as success.
   */
  it("does not read an unknown drain status as success", async () => {
    const bus = fakeControlBus();
    const timer = fakeTimer();

    const outcome = awaitDrainAcknowledgement({
      subscribe: bus.subscribe,
      sendDrain: async () => {},
      scheduleTimeout: timer.schedule,
    });
    bus.emit({ type: "drain", status: "refused_by_some_future_runtime" });
    timer.fire();

    assert.equal(await outcome, "no_acknowledgement");
  });

  it("unsubscribes and cancels the timer once settled", async () => {
    const bus = fakeControlBus();
    const timer = fakeTimer();

    const outcome = awaitDrainAcknowledgement({
      subscribe: bus.subscribe,
      sendDrain: async () => {},
      scheduleTimeout: timer.schedule,
    });
    bus.emit({ type: "drain", status: "draining" });
    await outcome;

    assert.equal(bus.listenerCount, 0, "the listener must be released");
    assert.equal(
      timer.scheduled,
      false,
      "the fallback timer must be cancelled",
    );
  });

  /**
   * A publish that never reached the relay is a different fact from an agent
   * that never answered, and the caller needs to be able to tell them apart.
   */
  it("propagates a send failure instead of reporting a timeout", async () => {
    const bus = fakeControlBus();
    const timer = fakeTimer();

    await assert.rejects(
      awaitDrainAcknowledgement({
        subscribe: bus.subscribe,
        sendDrain: async () => {
          throw new Error("relay unreachable");
        },
        scheduleTimeout: timer.schedule,
      }),
      /relay unreachable/,
    );
  });
});

describe("describeDrainRequest", () => {
  const EVERY_REQUEST = [
    { phase: "idle" },
    { phase: "sending" },
    { phase: "settled", acknowledgement: "draining" },
    { phase: "settled", acknowledgement: "already_draining" },
    { phase: "settled", acknowledgement: "no_acknowledgement" },
    { phase: "failed", message: "relay unreachable" },
  ];

  it("says nothing before a drain is asked for", () => {
    assert.deepEqual(describeDrainRequest({ phase: "idle" }), {
      badge: null,
      variant: "info",
      busy: false,
    });
  });

  it("marks only the in-flight send as busy", () => {
    const busyPhases = EVERY_REQUEST.filter(
      (request) => describeDrainRequest(request).busy,
    );

    assert.deepEqual(busyPhases, [{ phase: "sending" }]);
  });

  /**
   * **The contract this whole module exists to hold.** A drain acknowledgement
   * means admission is closed; the runtime then keeps working until the turn
   * it already held is finished. No state may tell the owner the process
   * stopped, because no state here has observed that.
   */
  it("never claims the agent stopped, in any state", () => {
    for (const request of EVERY_REQUEST) {
      const { badge } = describeDrainRequest(request);
      if (!badge) continue;
      assert.doesNotMatch(
        badge,
        /stopped|shut down|offline|terminated|killed|exited/i,
        `"${badge}" asserts an exit this desktop cannot observe`,
      );
    }
  });

  it("reports an acknowledged drain as still finishing its work", () => {
    assert.deepEqual(
      describeDrainRequest({ phase: "settled", acknowledgement: "draining" }),
      {
        badge: "Draining — finishing current work",
        variant: "success",
        busy: false,
      },
    );
  });

  it("distinguishes a repeat drain from a fresh one", () => {
    assert.equal(
      describeDrainRequest({
        phase: "settled",
        acknowledgement: "already_draining",
      }).badge,
      "Already draining",
    );
  });

  /**
   * Silence is reported as silence. Claiming the drain failed would be a
   * guess: the agent may have honoured it with owner telemetry switched off.
   */
  it("reports silence as sent-but-unanswered, not as a failure", () => {
    const { badge, variant } = describeDrainRequest({
      phase: "settled",
      acknowledgement: "no_acknowledgement",
    });

    assert.equal(badge, "Sent — no reply from the agent");
    assert.equal(variant, "warning");
    assert.notEqual(variant, "destructive");
  });

  /** A send that never reached the relay is a different fact, and says so. */
  it("names a send failure with its cause", () => {
    assert.deepEqual(
      describeDrainRequest({ phase: "failed", message: "relay unreachable" }),
      {
        badge: "Could not send drain: relay unreachable",
        variant: "destructive",
        busy: false,
      },
    );
  });
});
