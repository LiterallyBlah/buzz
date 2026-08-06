import { useMutation, useQueryClient } from "@tanstack/react-query";
import type { QueryClient } from "@tanstack/react-query";

import { relayClient } from "@/shared/api/relayClient";
import { signRelayEvent } from "@/shared/api/tauri";
import { getIdentity } from "@/shared/api/tauriIdentity";
import { KIND_DELETION } from "@/shared/constants/kinds";
import type { RelayEvent } from "@/shared/api/types";
import { normalizePubkey } from "@/shared/lib/pubkey";
import type { Repository as Project } from "./hooks";
import { applyProjectRootEvent } from "./projectRootLiveUpdates";

/** What a tombstone published from the project views is deleting. */
export type ProjectDeletionSubject = "issue" | "pull request" | "comment";

export type ProjectDeletionInput = {
  /** Pubkey that authored the event being deleted. */
  author: string;
  /**
   * Canonical issue/pull-request root the deletion is routed to. Equal to
   * `targetId` when the root itself is being deleted.
   */
  rootId: string;
  subject: ProjectDeletionSubject;
  /** Id of the event being deleted. */
  targetId: string;
};

/**
 * May this identity delete this event?
 *
 * Exact author match, normalised, and nothing else. The relay is the authority
 * — it re-checks the signer against the target's author before storing
 * anything — so this only decides whether an affordance is worth offering.
 * Deliberately narrower than the relay, which also lets a human delete their
 * own agent's events: offering that here would need owner lookups this slice
 * does not do, and a control that publishes a tombstone the relay might reject
 * is worse than no control.
 */
export function canDeleteProjectEvent(
  author: string,
  viewerPubkey: string | null | undefined,
): boolean {
  return viewerPubkey
    ? normalizePubkey(author) === normalizePubkey(viewerPubkey)
    : false;
}

/**
 * The event handed to the signer for a deletion.
 *
 * Separated from the publish so the exact submitted shape is assertable
 * without a Tauri signer, the same split `projectIssueEventInput` makes.
 *
 * Exactly one lowercase `e` and no `a`: the relay rejects a kind:5 that names
 * anything but one `e`-or-`a` target, so a second target tag of either kind
 * would make every deletion from these views fail at ingest. The uppercase `E`
 * is not a target — it is the route that carries a comment's tombstone to the
 * issue or pull request whose live subscription is the only one open.
 */
export function projectDeletionEventInput({
  rootId,
  subject,
  targetId,
}: Pick<ProjectDeletionInput, "rootId" | "subject" | "targetId">) {
  if (!/^[a-f0-9]{64}$/i.test(targetId)) {
    throw new Error("Deletion target must be a 64-character event id.");
  }
  if (!/^[a-f0-9]{64}$/i.test(rootId)) {
    throw new Error("Deletion route must be a 64-character event id.");
  }

  return {
    kind: KIND_DELETION,
    content: `Delete ${subject}`,
    tags: [
      ["e", targetId],
      ["E", rootId],
    ],
  };
}

/** Sign and publish the tombstone, refusing anything this identity does not own. */
export async function publishProjectDeletion(
  input: ProjectDeletionInput,
): Promise<RelayEvent> {
  const identity = await getIdentity();
  if (!canDeleteProjectEvent(input.author, identity.pubkey)) {
    throw new Error(`Only the author can delete this ${input.subject}.`);
  }

  const event = await signRelayEvent(projectDeletionEventInput(input));
  await relayClient.publishEvent(
    event,
    `Timed out deleting ${input.subject}.`,
    `Failed to delete ${input.subject}.`,
  );
  return event;
}

/**
 * Publish a deletion, then fold the accepted tombstone into the caches.
 *
 * The order is the contract: nothing is removed from a cache until the relay
 * has accepted the event. A refusal here — the author check above, a signer
 * failure, or a relay rejection — propagates before `applyProjectRootEvent`
 * runs, so a failed delete leaves the panel exactly as it was and the caller
 * shows the error. `publish` is a parameter so that path is assertable without
 * a Tauri signer or a relay.
 */
export async function deleteProjectEvent({
  input,
  projectId,
  publish = publishProjectDeletion,
  queryClient,
}: {
  input: ProjectDeletionInput;
  projectId: string;
  publish?: (input: ProjectDeletionInput) => Promise<RelayEvent>;
  queryClient: QueryClient;
}): Promise<RelayEvent> {
  const event = await publish(input);
  applyProjectRootEvent(queryClient, {
    event,
    projectId,
    rootId: input.rootId,
  });
  return event;
}

/**
 * Delete an issue, a pull request, or one of their comments.
 *
 * The fold above is what makes the row disappear immediately; the
 * invalidations are the safety net that reconciles with the relay, and the
 * activity summaries in particular are an aggregate no single event can be
 * folded into.
 */
export function useDeleteProjectEventMutation(
  project: Project | null | undefined,
) {
  const queryClient = useQueryClient();
  const projectId = project?.id ?? "none";

  return useMutation({
    mutationFn: (input: ProjectDeletionInput) =>
      deleteProjectEvent({ input, projectId, queryClient }),
    onSuccess: () => {
      for (const queryKey of [
        ["project", projectId, "issues"],
        ["project", projectId, "pull-requests"],
        ["projects", "work-items"],
        ["projects", "activity-summaries"],
      ]) {
        void queryClient.invalidateQueries({ queryKey });
      }
    },
  });
}
