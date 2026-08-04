import { useMutation, useQueryClient } from "@tanstack/react-query";

import { relayClient } from "@/shared/api/relayClient";
import { signRelayEvent } from "@/shared/api/tauri";
import {
  KIND_GIT_ISSUE,
  KIND_GIT_STATUS_CLOSED,
  KIND_GIT_STATUS_DRAFT,
  KIND_GIT_STATUS_MERGED,
  KIND_GIT_STATUS_OPEN,
} from "@/shared/constants/kinds";
import type { Project } from "./hooks";
import {
  buildGitIssueTags,
  buildGitStatusTags,
  nextProjectIssueStatusCreatedAt,
  type ProjectIssue,
} from "./projectIssues.mjs";

type CreateProjectIssueInput = {
  title: string;
  body: string;
  /** Pubkeys to `p`-tag, from the dialog's mention picker. */
  recipients?: string[];
};

/**
 * The event handed to the signer for a new issue.
 *
 * Separated from the publish so the exact submitted shape is assertable
 * without a Tauri signer: everything this feature decides is here, and what
 * follows is the shared sign-and-publish both project paths already use.
 */
export function projectIssueEventInput(
  project: Project,
  input: CreateProjectIssueInput,
) {
  return {
    kind: KIND_GIT_ISSUE,
    content: input.body.trim(),
    tags: buildGitIssueTags({
      repoAddress: project.repoAddress,
      repoOwner: project.owner,
      title: input.title,
      recipients: input.recipients ?? [],
    }),
  };
}

export async function publishProjectIssue(
  project: Project,
  input: CreateProjectIssueInput,
) {
  const event = await signRelayEvent(projectIssueEventInput(project, input));
  await relayClient.publishEvent(
    event,
    "Timed out creating issue.",
    "Failed to create issue.",
  );
  return event.id;
}

export function useCreateProjectIssueMutation(
  project: Project | null | undefined,
) {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: (input: CreateProjectIssueInput) => {
      if (!project) throw new Error("No project selected.");
      return publishProjectIssue(project, input);
    },
    onSuccess: async () => {
      await Promise.all([
        queryClient.invalidateQueries({
          queryKey: ["project", project?.id ?? "none", "issues"],
        }),
        queryClient.invalidateQueries({
          queryKey: ["projects", "work-items"],
        }),
        queryClient.invalidateQueries({
          queryKey: ["projects", "activity-summaries"],
        }),
      ]);
    },
  });
}

/**
 * The lifecycle states the desktop can publish for an issue.
 *
 * Exactly the four `statusFromEvent` in `projectIssues.mjs` already reads —
 * publishing a state the reader does not recognise would leave the panel
 * showing the previous status after an accepted event, which reads as a
 * failure that did not happen. `resolved` is NIP-34's 1631, which the reader
 * surfaces as *Done*; `draft` is 1633, surfaced as *Triage*.
 *
 * Nothing here reaches *In Progress* or *In Review*: those come from immutable
 * `t` labels on the kind:1621 root, which the plan records as out of scope.
 */
export type ProjectIssueLifecycleStatus =
  | "open"
  | "resolved"
  | "closed"
  | "draft";

const ISSUE_STATUS_KIND_BY_LIFECYCLE: Record<
  ProjectIssueLifecycleStatus,
  number
> = {
  open: KIND_GIT_STATUS_OPEN,
  resolved: KIND_GIT_STATUS_MERGED,
  closed: KIND_GIT_STATUS_CLOSED,
  draft: KIND_GIT_STATUS_DRAFT,
};

/**
 * The event handed to the signer for a status change (NIP-34 kind 1630–1633).
 *
 * Same event shape as `buzz issues status` and as the pull-request path
 * beside it: the root `e`, the repository `a`, and `p` tags for the two
 * pubkeys `allowedActorsForProjectRoot` trusts. The tags come from
 * `buildGitStatusTags`, which existed unused — this is V11's missing caller,
 * not a second way to write the same event.
 *
 * `now` is a parameter so the monotonic bump is assertable; the publisher
 * below passes the clock.
 */
export function projectIssueStatusEventInput({
  issue,
  now,
  project,
  status,
}: {
  issue: ProjectIssue;
  now: number;
  project: Project;
  status: ProjectIssueLifecycleStatus;
}) {
  return {
    kind: ISSUE_STATUS_KIND_BY_LIFECYCLE[status],
    content: "",
    createdAt: nextProjectIssueStatusCreatedAt(issue, now),
    // The issue's own repository, not the project's: they are the same in the
    // panel that mounts this, and binding to the issue is what keeps a status
    // change on one root from being addressed to another.
    tags: buildGitStatusTags({
      issueId: issue.id,
      repoAddress: issue.repoAddress ?? project.repoAddress,
      repoOwner: project.owner,
      issueAuthor: issue.author,
    }),
  };
}

export async function publishProjectIssueStatus({
  issue,
  project,
  status,
}: {
  issue: ProjectIssue;
  project: Project;
  status: ProjectIssueLifecycleStatus;
}): Promise<void> {
  const event = await signRelayEvent(
    projectIssueStatusEventInput({
      issue,
      now: Math.floor(Date.now() / 1_000),
      project,
      status,
    }),
  );

  await relayClient.publishEvent(
    event,
    "Timed out updating issue status.",
    "Failed to update issue status.",
  );
}

export function useUpdateProjectIssueStatusMutation(
  project: Project | null | undefined,
) {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: (input: {
      issue: ProjectIssue;
      status: ProjectIssueLifecycleStatus;
    }) => {
      if (!project) throw new Error("No project selected.");
      return publishProjectIssueStatus({ ...input, project });
    },
    onSuccess: async () => {
      await Promise.all([
        queryClient.invalidateQueries({
          queryKey: ["project", project?.id ?? "none", "issues"],
        }),
        queryClient.invalidateQueries({
          queryKey: ["projects", "work-items"],
        }),
        queryClient.invalidateQueries({
          queryKey: ["projects", "activity-summaries"],
        }),
      ]);
    },
  });
}
