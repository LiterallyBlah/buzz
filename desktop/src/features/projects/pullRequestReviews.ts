import { useMutation, useQueryClient } from "@tanstack/react-query";
import * as React from "react";

import {
  signProjectPullRequestReviewRequest,
  signProjectPullRequestStatus,
} from "@/shared/api/projectGit";
import { relayClient } from "@/shared/api/relayClient";
import { signRelayEvent } from "@/shared/api/tauri";
import { normalizePubkey } from "@/shared/lib/pubkey";
import {
  KIND_GIT_STATUS_CLOSED,
  KIND_GIT_STATUS_DRAFT,
  KIND_GIT_STATUS_OPEN,
  KIND_TEXT_NOTE,
} from "@/shared/constants/kinds";
import type { Project } from "./hooks";
import {
  pullRequestRepoOwner,
  resolvePullRequestRepoContext,
  type PullRequestRepoContext,
} from "./lib/pullRequestRepoContext";
import {
  nextProjectPullRequestStatusCreatedAt,
  type ProjectPullRequest,
} from "./projectPullRequests.mjs";
import {
  PR_APPROVAL_LABEL,
  PR_CHANGES_REQUESTED_LABEL,
  PR_REVIEW_REQUEST_LABEL,
} from "./projectPullRequests.mjs";

/** NIP-34 lifecycle states the desktop can publish for a PR. Merged (1631)
 * is intentionally excluded — merges happen through git, not this UI. */
export type ProjectPullRequestLifecycleStatus = "open" | "draft" | "closed";

const PR_STATUS_KIND_BY_LIFECYCLE: Record<
  ProjectPullRequestLifecycleStatus,
  number
> = {
  open: KIND_GIT_STATUS_OPEN,
  draft: KIND_GIT_STATUS_DRAFT,
  closed: KIND_GIT_STATUS_CLOSED,
};

// Same shape as `buzz pr status` (buzz-sdk build_git_status): root `e` tag,
// repo `a` tag, and `p` tags for the repo owner + PR author. Only the PR
// author or repo owner are trusted for status changes (allowedActorsForRoot).
async function updateProjectPullRequestStatus({
  repo,
  pullRequest,
  signAsManagedOwner,
  status,
}: {
  repo: PullRequestRepoContext;
  pullRequest: ProjectPullRequest;
  signAsManagedOwner: boolean;
  status: ProjectPullRequestLifecycleStatus;
}): Promise<void> {
  const createdAt = nextProjectPullRequestStatusCreatedAt(
    pullRequest,
    Math.floor(Date.now() / 1_000),
  );
  if (signAsManagedOwner) {
    await signProjectPullRequestStatus({
      targetOwner: repo.owner,
      repoAddress: repo.repoAddress,
      pullRequestId: pullRequest.id,
      pullRequestAuthor: pullRequest.author,
      status,
      createdAt,
    });
    return;
  }
  const recipients = new Set([
    repo.owner.toLowerCase(),
    pullRequest.author.toLowerCase(),
  ]);
  const event = await signRelayEvent({
    kind: PR_STATUS_KIND_BY_LIFECYCLE[status],
    content: "",
    createdAt,
    tags: [
      ["e", pullRequest.id, "", "root"],
      ["a", repo.repoAddress],
      ...[...recipients].map((recipient) => ["p", recipient]),
    ],
  });

  await relayClient.publishEvent(
    event,
    "Timed out updating pull request status.",
    "Failed to update pull request status.",
  );
}

// Review requests and approvals are labeled kind:1 comments (see
// projectPullRequests.mjs) — NIP-34 has no dedicated review kinds, and the
// relay does not register kind 1111. `p` tags on a review request are the
// requested reviewers; parsing only trusts requests signed by the PR author
// or repo owner.
async function requestProjectPullRequestReview({
  repo,
  pullRequest,
  reviewers,
  reviewerLabel,
  signAsManagedOwner,
}: {
  repo: PullRequestRepoContext;
  pullRequest: ProjectPullRequest;
  reviewers: string[];
  reviewerLabel: string;
  signAsManagedOwner: boolean;
}): Promise<void> {
  if (reviewers.length === 0) {
    throw new Error("Select at least one reviewer.");
  }
  const reviewerPubkeys = [
    ...new Set(reviewers.map((pubkey) => pubkey.toLowerCase())),
  ];
  if (signAsManagedOwner) {
    await signProjectPullRequestReviewRequest({
      targetOwner: repo.owner,
      repoAddress: repo.repoAddress,
      pullRequestId: pullRequest.id,
      reviewers: reviewerPubkeys,
      reviewerLabel,
    });
    return;
  }
  const event = await signRelayEvent({
    kind: KIND_TEXT_NOTE,
    content: `Requested a review from ${reviewerLabel}`,
    tags: [
      ["e", pullRequest.id, "", "root"],
      ["a", repo.repoAddress],
      ...reviewerPubkeys.map((pubkey) => ["p", pubkey]),
      ["t", PR_REVIEW_REQUEST_LABEL],
    ],
  });

  await relayClient.publishEvent(
    event,
    "Timed out requesting review.",
    "Failed to request review.",
  );
}

type ProjectPullRequestReviewDecision = "approve" | "request-changes";

/**
 * Whether a viewer may submit a review decision for this pull request.
 *
 * `project` is optional: the repo owner — the only thing it was consulted for
 * — is also recoverable from the PR's own coordinate, so a PR rendered without
 * a workspace selection still shows its review controls instead of silently
 * hiding them.
 */
export function canReviewProjectPullRequest(
  project: Pick<Project, "owner"> | null | undefined,
  pullRequest: ProjectPullRequest,
  viewerPubkey: string | null | undefined,
) {
  if (
    !viewerPubkey ||
    !pullRequest.commit ||
    (pullRequest.status !== "Open" && pullRequest.status !== "Draft")
  ) {
    return false;
  }
  const viewer = normalizePubkey(viewerPubkey);
  if (viewer === normalizePubkey(pullRequest.author)) return false;
  const owner = pullRequestRepoOwner(project, pullRequest);
  return (
    (owner !== null && viewer === normalizePubkey(owner)) ||
    pullRequest.reviewers.some(
      (reviewer) => normalizePubkey(reviewer) === viewer,
    )
  );
}

const REVIEW_DECISION_DETAILS: Record<
  ProjectPullRequestReviewDecision,
  {
    content: string;
    errorMessage: string;
    label: string;
    timeoutMessage: string;
  }
> = {
  approve: {
    content: "Approved these changes",
    errorMessage: "Failed to approve pull request.",
    label: PR_APPROVAL_LABEL,
    timeoutMessage: "Timed out approving pull request.",
  },
  "request-changes": {
    content: "Requested changes",
    errorMessage: "Failed to request changes.",
    label: PR_CHANGES_REQUESTED_LABEL,
    timeoutMessage: "Timed out requesting changes.",
  },
};

async function submitProjectPullRequestReview({
  content,
  createdAt,
  decision,
  repo,
  pullRequest,
}: {
  content?: string;
  createdAt: number;
  decision: ProjectPullRequestReviewDecision;
  repo: PullRequestRepoContext;
  pullRequest: ProjectPullRequest;
}): Promise<void> {
  if (!pullRequest.commit) {
    throw new Error("The pull request has no commit to review.");
  }
  const details = REVIEW_DECISION_DETAILS[decision];
  const recipients = new Set([
    repo.owner.toLowerCase(),
    pullRequest.author.toLowerCase(),
  ]);
  const event = await signRelayEvent({
    kind: KIND_TEXT_NOTE,
    content: content?.trim() || details.content,
    createdAt,
    tags: [
      ["e", pullRequest.id, "", "root"],
      ["a", repo.repoAddress],
      ...[...recipients].map((recipient) => ["p", recipient]),
      ["t", details.label],
      ["c", pullRequest.commit],
    ],
  });

  await relayClient.publishEvent(
    event,
    details.timeoutMessage,
    details.errorMessage,
  );
}

export function useProjectPullRequestWriteInvalidation(
  project: Project | null | undefined,
) {
  const queryClient = useQueryClient();
  return React.useCallback(() => {
    void queryClient.invalidateQueries({
      queryKey: ["project", project?.id ?? "none", "pull-requests"],
    });
    void queryClient.invalidateQueries({
      queryKey: ["projects", "work-items"],
    });
    void queryClient.invalidateQueries({
      queryKey: ["projects", "activity-summaries"],
    });
  }, [project?.id, queryClient]);
}

export function useUpdateProjectPullRequestStatusMutation(
  project: Project | null | undefined,
) {
  const invalidate = useProjectPullRequestWriteInvalidation(project);

  return useMutation({
    mutationFn: ({
      pullRequest,
      signAsManagedOwner = false,
      status,
    }: {
      pullRequest: ProjectPullRequest;
      signAsManagedOwner?: boolean;
      status: ProjectPullRequestLifecycleStatus;
    }) => {
      // A status change writes the repo's `a` tag and addresses its owner —
      // both of which the PR root already names. Requiring a selection on top
      // blocked the flow for PRs opened directly.
      const resolved = resolvePullRequestRepoContext(project, pullRequest);
      if (!resolved.ok) throw new Error(resolved.error);
      return updateProjectPullRequestStatus({
        repo: resolved.context,
        pullRequest,
        signAsManagedOwner,
        status,
      });
    },
    onSuccess: invalidate,
  });
}

export function useRequestProjectPullRequestReviewMutation(
  project: Project | null | undefined,
) {
  const invalidate = useProjectPullRequestWriteInvalidation(project);

  return useMutation({
    mutationFn: ({
      pullRequest,
      reviewers,
      reviewerLabel,
      signAsManagedOwner,
    }: {
      pullRequest: ProjectPullRequest;
      reviewers: string[];
      reviewerLabel: string;
      signAsManagedOwner: boolean;
    }) => {
      const resolved = resolvePullRequestRepoContext(project, pullRequest);
      if (!resolved.ok) throw new Error(resolved.error);
      return requestProjectPullRequestReview({
        repo: resolved.context,
        pullRequest,
        reviewers,
        reviewerLabel,
        signAsManagedOwner,
      });
    },
    onSuccess: invalidate,
  });
}

function useProjectPullRequestReviewDecisionMutation(
  project: Project | null | undefined,
  decision: ProjectPullRequestReviewDecision,
) {
  const invalidate = useProjectPullRequestWriteInvalidation(project);

  return useMutation({
    mutationFn: ({
      content,
      createdAt,
      pullRequest,
    }: {
      content?: string;
      createdAt: number;
      pullRequest: ProjectPullRequest;
    }) => {
      const resolved = resolvePullRequestRepoContext(project, pullRequest);
      if (!resolved.ok) throw new Error(resolved.error);
      return submitProjectPullRequestReview({
        content,
        createdAt,
        decision,
        repo: resolved.context,
        pullRequest,
      });
    },
    onSuccess: invalidate,
  });
}

export function useApproveProjectPullRequestMutation(
  project: Project | null | undefined,
) {
  return useProjectPullRequestReviewDecisionMutation(project, "approve");
}

export function useRequestProjectPullRequestChangesMutation(
  project: Project | null | undefined,
) {
  return useProjectPullRequestReviewDecisionMutation(
    project,
    "request-changes",
  );
}
