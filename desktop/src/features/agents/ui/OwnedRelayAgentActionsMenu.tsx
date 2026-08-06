import * as React from "react";
import { EllipsisVertical } from "lucide-react";
import { toast } from "sonner";

import { useMyRelayMembershipQuery } from "@/features/community-members/hooks";
import {
  awaitCancelAllAcknowledgement,
  describeCancelAllRequest,
  type CancelAllRequest,
} from "@/features/agents/lib/cancelAllOutcome";
import {
  buildRemoteAgentActions,
  describeLatestRemoteAgentControl,
  remoteAgentConfirmationCopy,
  type RemoteAgentActionId,
  type RemoteAgentControlId,
} from "@/features/agents/lib/remoteAgentActions";
import { subscribeControlResults } from "@/features/agents/observerRelayStore";
import {
  useBanMemberMutation,
  useModerationRestrictionsQuery,
  useUnbanMemberMutation,
} from "@/features/moderation/hooks";
import { cancelAllAgentWork } from "@/shared/api/agentControl";
import { normalizePubkey } from "@/shared/lib/pubkey";
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
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/shared/ui/dropdown-menu";
import { describeDrainRequest } from "@/features/agents/lib/drainOutcome";
import { useAgentDrainRequest } from "./OwnedRelayAgentDrainControl";

const CONTROL_ACK_TIMEOUT_MS = 10_000;

function useAgentCancelAllRequest(agentPubkey: string) {
  const [request, setRequest] = React.useState<CancelAllRequest>({
    phase: "idle",
  });
  const mounted = React.useRef(true);
  React.useEffect(
    () => () => {
      mounted.current = false;
    },
    [],
  );
  const send = React.useCallback(async () => {
    setRequest({ phase: "sending" });
    try {
      const acknowledgement = await awaitCancelAllAcknowledgement({
        subscribe: (listener) => subscribeControlResults(agentPubkey, listener),
        sendCancelAll: () => cancelAllAgentWork(agentPubkey),
        scheduleTimeout: (onTimeout) => {
          const timeout = window.setTimeout(onTimeout, CONTROL_ACK_TIMEOUT_MS);
          return () => window.clearTimeout(timeout);
        },
      });
      if (mounted.current) setRequest({ phase: "settled", acknowledgement });
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

export function useOwnedRelayAgentActions(agentPubkey: string) {
  const cancelRequest = useAgentCancelAllRequest(agentPubkey);
  const drainRequest = useAgentDrainRequest(agentPubkey);
  const [latestControl, setLatestControl] =
    React.useState<RemoteAgentControlId | null>(null);
  const cancel = {
    request: cancelRequest.request,
    send: () => {
      setLatestControl("cancel_all");
      void cancelRequest.send();
    },
  };
  const drain = {
    request: drainRequest.request,
    send: () => {
      setLatestControl("drain");
      void drainRequest.send();
    },
  };
  return { cancel, drain, latestControl };
}

export function OwnedRelayAgentActionBadge({
  cancelRequest,
  drainRequest,
  latestControl,
}: {
  cancelRequest: CancelAllRequest;
  drainRequest: ReturnType<typeof useAgentDrainRequest>["request"];
  latestControl: RemoteAgentControlId | null;
}) {
  const presentation = describeLatestRemoteAgentControl(
    cancelRequest,
    drainRequest,
    latestControl,
  );
  if (!presentation.badge) return null;
  return (
    <Badge className="max-w-full truncate" variant={presentation.variant}>
      {presentation.badge}
    </Badge>
  );
}

export function OwnedRelayAgentActionsMenu({
  agentName,
  agentPubkey,
  cancelRequest,
  drainRequest,
  onCancelAll,
  onDrain,
}: {
  agentName: string;
  agentPubkey: string;
  cancelRequest: CancelAllRequest;
  drainRequest: ReturnType<typeof useAgentDrainRequest>["request"];
  onCancelAll: () => void;
  onDrain: () => void;
}) {
  const membership = useMyRelayMembershipQuery();
  const relayRole = membership.data?.role;
  const canModerate = relayRole === "owner" || relayRole === "admin";
  const restrictions = useModerationRestrictionsQuery(canModerate);
  const banned = (restrictions.data ?? []).some(
    (restriction) =>
      normalizePubkey(restriction.pubkey) === normalizePubkey(agentPubkey) &&
      restriction.banned,
  );
  const ban = useBanMemberMutation();
  const unban = useUnbanMemberMutation();
  const [confirming, setConfirming] =
    React.useState<RemoteAgentActionId | null>(null);
  const actions = buildRemoteAgentActions({ relayRole, banned });
  const busy =
    describeCancelAllRequest(cancelRequest).busy ||
    describeDrainRequest(drainRequest).busy ||
    ban.isPending ||
    unban.isPending;
  const copy = confirming
    ? remoteAgentConfirmationCopy(confirming, agentName)
    : null;

  const confirm = async () => {
    const action = confirming;
    setConfirming(null);
    if (action === "cancel_all") return onCancelAll();
    if (action === "drain") return onDrain();
    try {
      if (action === "ban") {
        await ban.mutateAsync({ pubkey: agentPubkey });
        toast.success("Agent banned from this community");
      } else if (action === "unban") {
        await unban.mutateAsync(agentPubkey);
        toast.success("Agent ban lifted");
      }
    } catch (error) {
      toast.error(
        error instanceof Error ? error.message : "Moderation action failed",
      );
    }
  };

  return (
    <>
      <DropdownMenu modal={false}>
        <DropdownMenuTrigger asChild>
          <button
            aria-label={`Open actions for ${agentName}`}
            className="pointer-events-auto flex h-7 w-7 items-center justify-center rounded-md text-muted-foreground transition-colors hover:bg-muted hover:text-foreground"
            onClick={(event) => event.stopPropagation()}
            type="button"
          >
            <EllipsisVertical className="h-4 w-4" />
          </button>
        </DropdownMenuTrigger>
        <DropdownMenuContent
          align="end"
          onCloseAutoFocus={(event) => event.preventDefault()}
        >
          {actions.map((action, index) => (
            <React.Fragment key={action.id}>
              {index > 0 && actions[index - 1]?.section !== action.section ? (
                <DropdownMenuSeparator />
              ) : null}
              <DropdownMenuItem
                className={
                  action.section === "destructive"
                    ? "text-destructive focus:text-destructive"
                    : undefined
                }
                disabled={busy}
                onClick={(event) => {
                  event.stopPropagation();
                  setConfirming(action.id);
                }}
              >
                {action.label}
              </DropdownMenuItem>
            </React.Fragment>
          ))}
        </DropdownMenuContent>
      </DropdownMenu>
      <AlertDialog
        open={confirming != null}
        onOpenChange={(open) => {
          if (!open) setConfirming(null);
        }}
      >
        <AlertDialogContent>
          <AlertDialogHeader>
            <AlertDialogTitle>{copy?.title}</AlertDialogTitle>
            <AlertDialogDescription>{copy?.description}</AlertDialogDescription>
          </AlertDialogHeader>
          <AlertDialogFooter>
            <Button
              onClick={() => setConfirming(null)}
              size="sm"
              type="button"
              variant="outline"
            >
              Cancel
            </Button>
            <Button
              onClick={() => void confirm()}
              size="sm"
              type="button"
              variant={confirming === "cancel_all" ? "default" : "destructive"}
            >
              {copy?.confirmLabel}
            </Button>
          </AlertDialogFooter>
        </AlertDialogContent>
      </AlertDialog>
    </>
  );
}
