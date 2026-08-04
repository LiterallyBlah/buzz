import { useMutation, useQueryClient } from "@tanstack/react-query";

import { relayClient } from "@/shared/api/relayClient";
import { signRelayEvent } from "@/shared/api/tauri";
import { KIND_TEXT_NOTE } from "@/shared/constants/kinds";
import type { RelayEvent } from "@/shared/api/types";
import type { Project } from "./hooks";
import type { ProjectIssue } from "./projectIssues.mjs";
import type {
  ProjectPullRequest,
  ProjectPullRequestCommentAnchor,
} from "./projectPullRequests.mjs";
import {
  nextProjectPullRequestReviewCreatedAt,
  normalizeProjectPullRequestCommentAnchor,
  PR_CHANGES_REQUESTED_LABEL,
  PR_INLINE_COMMENT_LABEL,
} from "./projectPullRequests.mjs";
import { applyProjectRootEvent } from "./projectRootLiveUpdates";

export type ProjectPullRequestCommentDecision = "request-changes";

// Issue/PR comments are published as kind:1 text notes because the relay
// does not register NIP-22 kind 1111 (current NIP-34 reply convention).
// Pulse feeds filter these out via the repo-address `a` tag (see
// features/pulse/lib/projectComments.ts). If the relay ever allowlists
// 1111, migrate these to NIP-22 comments and drop that filter.
async function createProjectPullRequestComment({
  anchor,
  content,
  decision,
  mediaTags,
  mentionPubkeys = [],
  project,
  pullRequest,
}: {
  anchor?: ProjectPullRequestCommentAnchor;
  content: string;
  decision?: ProjectPullRequestCommentDecision;
  mediaTags?: string[][];
  mentionPubkeys?: string[];
  project: Project;
  pullRequest: ProjectPullRequest;
}): Promise<RelayEvent> {
  const body = content.trim();
  if (!body) {
    throw new Error("Comment cannot be empty.");
  }
  const normalizedAnchor = anchor
    ? normalizeProjectPullRequestCommentAnchor(anchor)
    : null;
  if (anchor && !normalizedAnchor) {
    throw new Error("Comment location is invalid.");
  }
  if ((normalizedAnchor || decision) && !pullRequest.commit) {
    throw new Error("Pull request commit is required for review comments.");
  }

  const recipients = new Set([
    project.owner.toLowerCase(),
    pullRequest.author.toLowerCase(),
    ...pullRequest.recipients.map((recipient) => recipient.toLowerCase()),
    ...mentionPubkeys.map((pubkey) => pubkey.toLowerCase()),
  ]);
  const tags = [
    ["e", pullRequest.id, "", "root"],
    ["a", project.repoAddress],
    ...[...recipients].map((recipient) => ["p", recipient]),
    ...(normalizedAnchor
      ? [
          ["t", PR_INLINE_COMMENT_LABEL],
          ["c", pullRequest.commit as string],
          ["file", normalizedAnchor.path],
          ["side", normalizedAnchor.side],
          ["line", String(normalizedAnchor.line)],
        ]
      : []),
    ...(decision
      ? [
          ["t", PR_CHANGES_REQUESTED_LABEL],
          ...(!normalizedAnchor ? [["c", pullRequest.commit as string]] : []),
        ]
      : []),
    ...(mediaTags ?? []),
  ];

  const event = await signRelayEvent({
    kind: KIND_TEXT_NOTE,
    content: body,
    ...(decision
      ? {
          createdAt: nextProjectPullRequestReviewCreatedAt(
            pullRequest,
            Math.floor(Date.now() / 1_000),
          ),
        }
      : {}),
    tags,
  });

  await relayClient.publishEvent(
    event,
    "Timed out posting pull request comment.",
    "Failed to post pull request comment.",
  );

  // The accepted event is handed back so the caller can show it immediately.
  // It already carries the id the caches dedupe on, so merging it and then
  // seeing it again through the live subscription costs nothing.
  return event;
}

async function createProjectIssueComment({
  content,
  mediaTags,
  mentionPubkeys = [],
  issue,
  project,
}: {
  content: string;
  mediaTags?: string[][];
  mentionPubkeys?: string[];
  issue: ProjectIssue;
  project: Project;
}): Promise<RelayEvent> {
  const body = content.trim();
  if (!body) {
    throw new Error("Comment cannot be empty.");
  }

  const recipients = new Set([
    project.owner.toLowerCase(),
    issue.author.toLowerCase(),
    ...issue.recipients.map((recipient) => recipient.toLowerCase()),
    ...mentionPubkeys.map((pubkey) => pubkey.toLowerCase()),
  ]);
  const tags = [
    ["e", issue.id, "", "root"],
    ["a", project.repoAddress],
    ...[...recipients].map((recipient) => ["p", recipient]),
    ...(mediaTags ?? []),
  ];

  const event = await signRelayEvent({
    kind: KIND_TEXT_NOTE,
    content: body,
    tags,
  });

  await relayClient.publishEvent(
    event,
    "Timed out posting issue comment.",
    "Failed to post issue comment.",
  );

  return event;
}

export function useCreateProjectIssueCommentMutation(
  project: Project | null | undefined,
) {
  const queryClient = useQueryClient();

  return useMutation({
    mutationFn: ({
      content,
      mediaTags,
      mentionPubkeys,
      issue,
    }: {
      content: string;
      mediaTags?: string[][];
      mentionPubkeys?: string[];
      issue: ProjectIssue;
    }) => {
      if (!project) throw new Error("No project selected.");
      return createProjectIssueComment({
        content,
        mediaTags,
        mentionPubkeys,
        issue,
        project,
      });
    },
    onSuccess: (event, variables) => {
      // Your own comment appears the moment the relay accepts it, through the
      // same merge the live subscription uses. The invalidations below still
      // run — they are the safety net that reconciles with the relay — but the
      // comment no longer waits on a round trip to become visible.
      applyProjectRootEvent(queryClient, {
        event,
        projectId: project?.id ?? "none",
        rootId: variables.issue.id,
      });
      void queryClient.invalidateQueries({
        queryKey: ["project", project?.id ?? "none", "issues"],
      });
      void queryClient.invalidateQueries({
        queryKey: ["projects", "work-items"],
      });
      void queryClient.invalidateQueries({
        queryKey: ["projects", "activity-summaries"],
      });
    },
  });
}

export function useCreateProjectPullRequestCommentMutation(
  project: Project | null | undefined,
) {
  const queryClient = useQueryClient();

  return useMutation({
    mutationFn: ({
      anchor,
      content,
      decision,
      mediaTags,
      mentionPubkeys,
      pullRequest,
    }: {
      anchor?: ProjectPullRequestCommentAnchor;
      content: string;
      decision?: ProjectPullRequestCommentDecision;
      mediaTags?: string[][];
      mentionPubkeys?: string[];
      pullRequest: ProjectPullRequest;
    }) => {
      if (!project) throw new Error("No project selected.");
      return createProjectPullRequestComment({
        anchor,
        content,
        decision,
        mediaTags,
        mentionPubkeys,
        project,
        pullRequest,
      });
    },
    onSuccess: (event, variables) => {
      applyProjectRootEvent(queryClient, {
        event,
        projectId: project?.id ?? "none",
        rootId: variables.pullRequest.id,
      });
      void queryClient.invalidateQueries({
        queryKey: ["project", project?.id ?? "none", "pull-requests"],
      });
      void queryClient.invalidateQueries({
        queryKey: ["projects", "work-items"],
      });
      void queryClient.invalidateQueries({
        queryKey: ["projects", "activity-summaries"],
      });
    },
  });
}
