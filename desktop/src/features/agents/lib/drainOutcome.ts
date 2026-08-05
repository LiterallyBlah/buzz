import type { ControlResultFrame } from "@/shared/api/types";

/**
 * What the owner learned about a drain they requested.
 *
 * These are statements about the *instruction*, never about the process. The
 * runtime acknowledges a drain the moment it closes admission — it then keeps
 * running until the work already in hand is finished, which is the whole point
 * of drain rather than a kill. So the best case here is `draining`, and there
 * is deliberately no member that means "stopped": this desktop has no evidence
 * for that claim and must not paint it.
 *
 * `no_acknowledgement` is a report about our own knowledge, not a failure of
 * the agent. A running agent whose owner telemetry is switched off honours the
 * drain and never acks it (`observer` is `Option` at the emit site in
 * `buzz-acp`), and so does one that received the frame a moment after we gave
 * up waiting. The copy that renders it therefore says we did not hear back,
 * not that nothing happened.
 */
export type DrainAcknowledgement =
  | "draining"
  | "already_draining"
  | "no_acknowledgement";

/**
 * The `status` values `handle_drain_control` acks with, via
 * `DrainOnset::status` in `crates/buzz-acp/src/lib.rs`. A `ControlResultFrame`
 * carries the answers to `cancel_turn` and `switch_model` on the same
 * listener, so `type` — not `status` — is what tells them apart.
 */
const DRAIN_ACK_STATUS: Record<string, DrainAcknowledgement> = {
  draining: "draining",
  already_draining: "already_draining",
};

/**
 * Send an owner-signed drain and wait for the agent's own acknowledgement.
 *
 * The wire contract is `crates/buzz-acp/src/drain.rs`: an owner-signed kind
 * 24200 control frame carrying `{"type":"drain"}`, answered by a
 * `control_result` telemetry frame carrying `{"type":"drain","status":…}`.
 * Only the agent can produce that answer — it is encrypted to the owner by the
 * agent's own key — so an ack here is evidence the *agent* acted, not evidence
 * the relay accepted a publish.
 *
 * **The listener is registered before the send**, exactly as
 * `awaitLiveSwitchOutcome` does. An idle agent acks in the same round trip, so
 * subscribing after `sendDrain` resolves would lose the fast ack and report a
 * timeout for a drain that plainly worked.
 *
 * A send failure is not caught here. The caller distinguishes "we never got it
 * onto the relay" (a thrown publish error, retryable) from "we sent it and
 * heard nothing" (`no_acknowledgement`, not retryable in any useful sense),
 * and those two want different words in front of the owner.
 *
 * Counting and clock live here, isolated from React and the relay, so the
 * whole state machine is unit-testable with synthetic frames and a fake timer.
 */
export async function awaitDrainAcknowledgement({
  subscribe,
  sendDrain,
  scheduleTimeout,
}: {
  /** Register a control-result listener; returns an unsubscribe function. */
  subscribe: (listener: (frame: ControlResultFrame) => void) => () => void;
  /** Publish the owner-signed drain frame. Rejects if it never reached the relay. */
  sendDrain: () => Promise<void>;
  /** Schedule the no-reply fallback; returns a cancel function. */
  scheduleTimeout: (onTimeout: () => void) => () => void;
}): Promise<DrainAcknowledgement> {
  const settled = new Promise<DrainAcknowledgement>((resolve) => {
    let unsubscribe = () => {};
    let cancelTimeout = () => {};
    const finish = (outcome: DrainAcknowledgement) => {
      cancelTimeout();
      unsubscribe();
      resolve(outcome);
    };
    cancelTimeout = scheduleTimeout(() => finish("no_acknowledgement"));
    unsubscribe = subscribe((frame) => {
      // One agent can have a `cancel_turn` or `switch_model` result in flight
      // at the same moment; both arrive on this listener. Matching on `type`
      // is what stops an unrelated ack from resolving this drain.
      if (frame.type !== "drain") {
        return;
      }
      const acknowledgement = DRAIN_ACK_STATUS[frame.status];
      if (acknowledgement) {
        finish(acknowledgement);
      }
      // An unrecognised status is ignored rather than treated as success. A
      // future runtime that grows a "refused" status must not be read as
      // "draining" by an older desktop; falling through to the timeout is the
      // safe reading, because it claims nothing.
    });
  });

  await sendDrain();

  return settled;
}

/**
 * Everything the card knows about one drain the owner asked for.
 *
 * `failed` and `settled: "no_acknowledgement"` are kept apart because they are
 * different facts with different remedies: the first means the frame never
 * reached the relay and retrying may work, the second means it did and the
 * agent did not answer, where retrying changes nothing.
 */
export type DrainRequest =
  | { phase: "idle" }
  | { phase: "sending" }
  | { phase: "settled"; acknowledgement: DrainAcknowledgement }
  | { phase: "failed"; message: string };

export type DrainPresentation = {
  /** Badge copy, or `null` when there is nothing to say. */
  badge: string | null;
  variant: "info" | "success" | "warning" | "destructive";
  /** Whether a request is in flight — the button is disabled while it is. */
  busy: boolean;
};

/**
 * Turn a drain request into the words the owner reads.
 *
 * **The rule this function exists to hold: no state here may say the agent
 * stopped.** A drain acknowledgement means the runtime closed admission; it
 * then keeps running until the work it already held is finished, which can be
 * as long as one full turn. "Stopped", "Offline" or a past-tense "Drained"
 * would each assert an exit that this desktop has not observed and cannot
 * observe from a `control_result` frame. The strongest true statement is that
 * the agent is draining.
 *
 * Copy, not just a state name, so the contract is assertable: the tests below
 * pin these strings, and a future edit that reaches for "Stopped" fails there
 * rather than shipping.
 */
const ACKNOWLEDGEMENT_PRESENTATION: Record<
  DrainAcknowledgement,
  DrainPresentation
> = {
  draining: {
    badge: "Draining — finishing current work",
    variant: "success",
    busy: false,
  },
  already_draining: {
    badge: "Already draining",
    variant: "info",
    busy: false,
  },
  // Says what we know (we heard nothing) rather than what we would be guessing
  // (that it was ignored). A running agent whose owner telemetry is switched
  // off drains without ever acking.
  no_acknowledgement: {
    badge: "Sent — no reply from the agent",
    variant: "warning",
    busy: false,
  },
};

export function describeDrainRequest(request: DrainRequest): DrainPresentation {
  switch (request.phase) {
    case "idle":
      return { badge: null, variant: "info", busy: false };
    case "sending":
      return { badge: "Sending drain…", variant: "info", busy: true };
    case "settled":
      return ACKNOWLEDGEMENT_PRESENTATION[request.acknowledgement];
    case "failed":
      return {
        badge: `Could not send drain: ${request.message}`,
        variant: "destructive",
        busy: false,
      };
  }
}
