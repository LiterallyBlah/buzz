import * as React from "react";

import {
  awaitDrainAcknowledgement,
  describeDrainRequest,
  type DrainRequest,
} from "@/features/agents/lib/drainOutcome";
import { subscribeControlResults } from "@/features/agents/observerRelayStore";
import { drainAgent } from "@/shared/api/agentControl";
import {
  AlertDialog,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
} from "@/shared/ui/alert-dialog";
import { Badge } from "@/shared/ui/badge";
import { Button } from "@/shared/ui/button";

/**
 * How long we wait for the agent's `control_result` before reporting silence.
 *
 * The runtime acks inside `handle_drain_control` without awaiting anything, so
 * the round trip is relay latency and nothing else. Ten seconds is therefore
 * long enough that a slow relay is not mistaken for an unresponsive agent, and
 * short enough that an owner is not left watching a spinner. Slightly longer
 * than the model-switch fallback (8 s), which waits on a busy harness rather
 * than on a straight-line handler.
 */
const DRAIN_ACK_TIMEOUT_MS = 10_000;

/**
 * Drive one drain request for one agent.
 *
 * State is deliberately per-card and not persisted: it describes *this*
 * desktop's knowledge of a request it just made, and there is no honest way to
 * restore that after a remount. A card that came back saying "Draining" would
 * be quoting an ack it never heard.
 */
export function useAgentDrainRequest(agentPubkey: string) {
  const [request, setRequest] = React.useState<DrainRequest>({ phase: "idle" });
  // Guards against a `setState` on an unmounted card when the owner navigates
  // away while an ack is outstanding.
  const mounted = React.useRef(true);
  React.useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
    };
  }, []);

  const send = React.useCallback(async () => {
    setRequest({ phase: "sending" });
    try {
      const acknowledgement = await awaitDrainAcknowledgement({
        subscribe: (listener) => subscribeControlResults(agentPubkey, listener),
        sendDrain: () => drainAgent(agentPubkey),
        scheduleTimeout: (onTimeout) => {
          const timeout = window.setTimeout(onTimeout, DRAIN_ACK_TIMEOUT_MS);
          return () => window.clearTimeout(timeout);
        },
      });
      if (mounted.current) {
        setRequest({ phase: "settled", acknowledgement });
      }
    } catch (error) {
      if (mounted.current) {
        setRequest({
          phase: "failed",
          message:
            error instanceof Error ? error.message : "the relay rejected it",
        });
      }
    }
  }, [agentPubkey]);

  return { request, send };
}

/**
 * The one remote control an externally hosted agent can actually execute.
 *
 * Start, Restart, Deploy, Edit and Delete are deliberately absent, and their
 * absence is not an oversight to be filled in later by this component. Drain
 * is reachable because the *running agent* receives and verifies the frame
 * itself; a stopped process receives nothing, and this tree has no host
 * controller that could act on its behalf. Painting a Start button here would
 * be a control that can only ever fail.
 */
export function OwnedRelayAgentDrainButton({
  agentName,
  busy,
  onConfirm,
}: {
  agentName: string;
  busy: boolean;
  onConfirm: () => void;
}) {
  const [confirming, setConfirming] = React.useState(false);

  return (
    <>
      <Button
        // The card's own full-bleed button sits underneath at z-10 and would
        // otherwise swallow this click into "open the profile".
        className="pointer-events-auto"
        disabled={busy}
        onClick={(event) => {
          event.stopPropagation();
          setConfirming(true);
        }}
        size="sm"
        type="button"
        variant="outline"
      >
        Drain
      </Button>
      <AlertDialog onOpenChange={setConfirming} open={confirming}>
        <AlertDialogContent>
          <AlertDialogHeader>
            <AlertDialogTitle>Drain {agentName}?</AlertDialogTitle>
            <AlertDialogDescription>
              {agentName} will stop taking new work, finish what it is already
              doing, and then exit. Because it runs somewhere else, this app
              cannot start it again — whoever runs its host has to.
            </AlertDialogDescription>
          </AlertDialogHeader>
          <AlertDialogFooter>
            <Button
              onClick={() => setConfirming(false)}
              size="sm"
              type="button"
              variant="outline"
            >
              Cancel
            </Button>
            <Button
              onClick={() => {
                setConfirming(false);
                onConfirm();
              }}
              size="sm"
              type="button"
              variant="destructive"
            >
              Drain
            </Button>
          </AlertDialogFooter>
        </AlertDialogContent>
      </AlertDialog>
    </>
  );
}

/** The agent's own answer, or our admission that we did not get one. */
export function OwnedRelayAgentDrainBadge({
  request,
}: {
  request: DrainRequest;
}) {
  const { badge, variant } = describeDrainRequest(request);
  if (!badge) {
    return null;
  }
  return (
    <Badge className="max-w-full truncate" variant={variant}>
      {badge}
    </Badge>
  );
}
