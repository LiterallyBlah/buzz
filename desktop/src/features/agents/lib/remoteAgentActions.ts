import {
  describeCancelAllRequest,
  type CancelAllRequest,
} from "@/features/agents/lib/cancelAllOutcome";
import {
  describeDrainRequest,
  type DrainRequest,
} from "@/features/agents/lib/drainOutcome";

export type RemoteAgentControlId = "cancel_all" | "drain";
export type RemoteAgentActionId = "cancel_all" | "drain" | "ban" | "unban";
export type RemoteAgentAction = {
  id: RemoteAgentActionId;
  label: string;
  section: "control" | "destructive";
};

export function describeLatestRemoteAgentControl(
  cancelRequest: CancelAllRequest,
  drainRequest: DrainRequest,
  latestControl: RemoteAgentControlId | null,
) {
  const cancel = describeCancelAllRequest(cancelRequest);
  const drain = describeDrainRequest(drainRequest);
  if (cancel.busy) return cancel;
  if (drain.busy) return drain;
  return latestControl === "drain" ? drain : cancel;
}

export function buildRemoteAgentActions(input: {
  relayRole: string | null | undefined;
  banned: boolean;
}): RemoteAgentAction[] {
  const actions: RemoteAgentAction[] = [
    { id: "cancel_all", label: "Stop current work", section: "control" },
    { id: "drain", label: "Finish work and shut down", section: "control" },
  ];
  if (input.relayRole === "owner" || input.relayRole === "admin") {
    actions.push({
      id: input.banned ? "unban" : "ban",
      label: input.banned ? "Lift ban" : "Ban from this community",
      section: "destructive",
    });
  }
  return actions;
}

export function remoteAgentConfirmationCopy(
  action: RemoteAgentActionId,
  name: string,
): { title: string; description: string; confirmLabel: string } {
  switch (action) {
    case "cancel_all":
      return {
        title: `Stop ${name}’s current work?`,
        description: `Cancel everything ${name} is working on now? It will stay online and can accept new work afterwards.`,
        confirmLabel: "Stop current work",
      };
    case "drain":
      return {
        title: `Finish ${name}’s work and shut it down?`,
        description: `${name} will stop taking new work, finish everything already admitted, and then exit. Because it runs somewhere else, this app cannot start it again — whoever runs its host has to.`,
        confirmLabel: "Finish work and shut down",
      };
    case "ban":
      return {
        title: `Ban ${name} from this community?`,
        description: `${name} will be disconnected and blocked from relay reads and writes, including Buzz memories. Its host process may continue running; this does not shut it down.`,
        confirmLabel: "Ban from this community",
      };
    case "unban":
      return {
        title: `Lift ${name}’s community ban?`,
        description: `${name} will be allowed to connect to this community again. This does not start or otherwise control its host process.`,
        confirmLabel: "Lift ban",
      };
  }
}
