import type { ControlResultFrame } from "@/shared/api/types";

export type CancelAllAcknowledgement =
  | {
      kind: "accepted";
      activeTurns: number;
      signalledTurns: number;
      queuedEvents: number;
    }
  | { kind: "no_work" }
  | { kind: "no_acknowledgement" };

export async function awaitCancelAllAcknowledgement({
  subscribe,
  sendCancelAll,
  scheduleTimeout,
}: {
  subscribe: (listener: (frame: ControlResultFrame) => void) => () => void;
  sendCancelAll: () => Promise<void>;
  scheduleTimeout: (onTimeout: () => void) => () => void;
}): Promise<CancelAllAcknowledgement> {
  const settled = new Promise<CancelAllAcknowledgement>((resolve) => {
    let unsubscribe = () => {};
    let cancelTimeout = () => {};
    const finish = (outcome: CancelAllAcknowledgement) => {
      cancelTimeout();
      unsubscribe();
      resolve(outcome);
    };
    cancelTimeout = scheduleTimeout(() =>
      finish({ kind: "no_acknowledgement" }),
    );
    unsubscribe = subscribe((frame) => {
      if (frame.type !== "cancel_all") return;
      if (frame.status === "no_work") {
        finish({ kind: "no_work" });
      } else if (frame.status === "accepted") {
        finish({
          kind: "accepted",
          activeTurns: Math.max(0, frame.activeTurns ?? 0),
          signalledTurns: Math.max(0, frame.signalledTurns ?? 0),
          queuedEvents: Math.max(0, frame.queuedEvents ?? 0),
        });
      }
    });
  });
  await sendCancelAll();
  return settled;
}

export type CancelAllRequest =
  | { phase: "idle" }
  | { phase: "sending" }
  | { phase: "settled"; acknowledgement: CancelAllAcknowledgement }
  | { phase: "failed"; message: string };

export type CancelAllPresentation = {
  badge: string | null;
  variant: "info" | "success" | "warning" | "destructive";
  busy: boolean;
};

export function describeCancelAllRequest(
  request: CancelAllRequest,
): CancelAllPresentation {
  if (request.phase === "idle")
    return { badge: null, variant: "info", busy: false };
  if (request.phase === "sending")
    return { badge: "Stopping current work…", variant: "info", busy: true };
  if (request.phase === "failed") {
    return {
      badge: `Could not stop current work: ${request.message}`,
      variant: "destructive",
      busy: false,
    };
  }
  const acknowledgement = request.acknowledgement;
  if (acknowledgement.kind === "no_acknowledgement") {
    return {
      badge: "Sent — no reply from the agent",
      variant: "warning",
      busy: false,
    };
  }
  if (acknowledgement.kind === "no_work") {
    return { badge: "No work was active", variant: "info", busy: false };
  }
  const parts = [];
  if (acknowledgement.signalledTurns > 0)
    parts.push(
      "Stop requested for " +
        acknowledgement.signalledTurns +
        " active " +
        (acknowledgement.signalledTurns === 1 ? "turn" : "turns"),
    );
  const alreadyStopping =
    acknowledgement.activeTurns - acknowledgement.signalledTurns;
  if (alreadyStopping > 0)
    parts.push(
      alreadyStopping +
        " active " +
        (alreadyStopping === 1 ? "turn was" : "turns were") +
        " already stopping",
    );
  if (acknowledgement.queuedEvents > 0)
    parts.push(
      acknowledgement.queuedEvents +
        " queued " +
        (acknowledgement.queuedEvents === 1 ? "item" : "items") +
        " discarded",
    );
  return {
    badge: parts.length > 0 ? parts.join("; ") : "Stop request accepted",
    variant: "success",
    busy: false,
  };
}
