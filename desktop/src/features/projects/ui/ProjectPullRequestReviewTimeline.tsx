import {
  Check,
  ChevronDown,
  ChevronUp,
  FileCode2,
  History,
  MessageSquare,
  TriangleAlert,
  UserPlus,
} from "lucide-react";
import * as React from "react";

import type {
  Project,
  ProjectPullRequest,
  ProjectPullRequestCommentAnchor,
} from "@/features/projects/hooks";
import {
  formatExactTimestamp,
  relativeTime,
} from "@/features/projects/lib/projectsViewHelpers";
import { projectPullRequestCommentTimelineKind } from "@/features/projects/projectPullRequests.mjs";
import type { UserProfileLookup } from "@/features/profile/lib/identity";
import type { OpenMergeRecoveryTerminal } from "./MergePullRequestButton";
import { labelForPubkey } from "./projectMemberLabels";
import { ProjectItemDeleteMenu } from "./ProjectItemDeleteMenu";
import { ProfileAuthorName } from "./ProjectProfileIdentity";
import { ProjectRichContent } from "./ProjectRichContent";
import { PullRequestReviewCard } from "./PullRequestReviewCard";

/**
 * The pull request's review history: comments, approvals, change requests, and
 * review requests on one vertical timeline, with the review actions card at the
 * bottom.
 *
 * Its own component because the conversation panel it was carved out of had
 * grown to the file-size ceiling, and because the collapse/expand state and the
 * per-entry actions belong to the timeline rather than to the panel around it.
 * The state is still keyed by pull-request id, so switching pull requests and
 * back inside one mounted detail view remembers what was collapsed.
 */
export function PullRequestReviewTimeline({
  onOpenInlineComment,
  onOpenTerminal,
  profiles,
  project,
  pullRequest,
}: {
  onOpenInlineComment?: (anchor: ProjectPullRequestCommentAnchor) => void;
  onOpenTerminal?: OpenMergeRecoveryTerminal;
  profiles?: UserProfileLookup;
  project: Project;
  pullRequest: ProjectPullRequest;
}) {
  const [
    expandedReviewHistoryPullRequestIds,
    setExpandedReviewHistoryPullRequestIds,
  ] = React.useState<Set<string>>(() => new Set());
  const [
    collapsedReviewHistoryPullRequestIds,
    setCollapsedReviewHistoryPullRequestIds,
  ] = React.useState<Set<string>>(() => new Set());

  const reviewHistory = pullRequest.comments
    .map((item) => ({
      item,
      timelineKind: projectPullRequestCommentTimelineKind(item),
    }))
    .sort(
      (left, right) =>
        left.item.createdAt - right.item.createdAt ||
        left.item.id.localeCompare(right.item.id),
    );
  const reviewHistoryCollapsed = collapsedReviewHistoryPullRequestIds.has(
    pullRequest.id,
  );
  const reviewHistoryExpanded = expandedReviewHistoryPullRequestIds.has(
    pullRequest.id,
  );
  const earlierReviewHistoryCount = Math.max(0, reviewHistory.length - 3);
  const visibleReviewHistory =
    reviewHistoryExpanded || earlierReviewHistoryCount === 0
      ? reviewHistory
      : reviewHistory.slice(-3);
  const displayedReviewHistory = reviewHistoryCollapsed
    ? []
    : visibleReviewHistory;

  return (
    <div className="group/timeline -mx-4 overflow-hidden border-border/50 border-b">
      {reviewHistory.length > 0 ? (
        <button
          aria-expanded={!reviewHistoryCollapsed}
          className="flex min-h-10 w-full items-center gap-2 px-3 py-2.5 text-sm font-semibold text-muted-foreground transition-colors hover:text-foreground"
          data-testid="project-pull-request-review-history-toggle"
          onClick={() => {
            setCollapsedReviewHistoryPullRequestIds((current) => {
              const next = new Set(current);
              if (reviewHistoryCollapsed) {
                next.delete(pullRequest.id);
              } else {
                next.add(pullRequest.id);
              }
              return next;
            });
          }}
          type="button"
        >
          <span className="relative flex w-5 shrink-0 justify-center self-stretch">
            {reviewHistoryCollapsed ? (
              <span className="absolute top-2.5 -bottom-11 hidden w-px bg-border/80 group-has-[.pull-request-action-timeline]/timeline:block" />
            ) : (
              <span className="absolute top-2.5 -bottom-[1.875rem] w-px bg-border/80" />
            )}
            <span className="relative z-10 flex h-5 w-5 items-center justify-center rounded-full bg-primary/10 text-primary ring-1 ring-primary/35">
              <History className="h-3 w-3" />
            </span>
          </span>
          <span className="flex min-h-5 min-w-0 flex-1 items-center text-left">
            {reviewHistoryCollapsed
              ? `Show ${reviewHistory.length} earlier ${
                  reviewHistory.length === 1 ? "activity" : "activities"
                }`
              : "Collapse review history"}
          </span>
          {reviewHistoryCollapsed ? (
            <ChevronDown className="mt-0.5 h-3.5 w-3.5" />
          ) : (
            <ChevronUp className="mt-0.5 h-3.5 w-3.5" />
          )}
        </button>
      ) : null}
      {!reviewHistoryCollapsed &&
      earlierReviewHistoryCount > 0 &&
      !reviewHistoryExpanded ? (
        <button
          className="flex min-h-10 w-full items-center gap-2 px-3 py-2.5 text-sm font-semibold text-muted-foreground transition-colors hover:text-foreground"
          data-testid="project-pull-request-earlier-activities"
          onClick={() => {
            setExpandedReviewHistoryPullRequestIds((current) => {
              const next = new Set(current);
              next.add(pullRequest.id);
              return next;
            });
          }}
          type="button"
        >
          <span className="relative flex w-5 shrink-0 justify-center self-stretch">
            <span className="absolute top-2.5 -bottom-[1.875rem] w-px bg-border/80" />
            <span className="relative z-10 flex h-5 w-5 items-center justify-center rounded-full bg-background ring-1 ring-border/70">
              <ChevronDown className="h-3 w-3" />
            </span>
          </span>
          <span className="min-w-0 flex-1 text-left">
            Show {earlierReviewHistoryCount} earlier{" "}
            {earlierReviewHistoryCount === 1 ? "activity" : "activities"}
          </span>
        </button>
      ) : null}
      {displayedReviewHistory.map(({ item, timelineKind }, index) => {
        const isHistoricalDecision = item.reviewDecisionStatus === "historical";
        const trimmedContent = item.content.trim();
        const activityContent =
          timelineKind === null
            ? trimmedContent
            : timelineKind === "changes-requested" &&
                !/^requested changes\.?$/i.test(trimmedContent)
              ? trimmedContent
              : timelineKind === "approved" &&
                  !/^approved (these )?changes\.?$/i.test(trimmedContent)
                ? trimmedContent
                : null;
        return (
          <div
            className="flex min-h-10 min-w-0 items-start gap-2 px-3 py-2.5 text-sm text-muted-foreground"
            data-testid="project-pull-request-timeline-row"
            key={item.id}
          >
            <div className="relative flex w-5 shrink-0 justify-center self-stretch">
              {index < displayedReviewHistory.length - 1 ? (
                <span className="absolute top-2.5 -bottom-[1.875rem] w-px bg-border/80" />
              ) : (
                <span className="absolute top-2.5 -bottom-11 hidden w-px bg-border/80 group-has-[.pull-request-action-timeline]/timeline:block" />
              )}
              <span className="relative z-10 flex h-5 w-5 items-center justify-center rounded-full bg-background ring-1 ring-border/70">
                {timelineKind === "approved" ? (
                  <Check
                    className={`h-3 w-3 ${
                      isHistoricalDecision
                        ? "text-muted-foreground"
                        : "text-green-600 dark:text-green-500"
                    }`}
                  />
                ) : timelineKind === "changes-requested" ? (
                  <TriangleAlert
                    className={`h-3 w-3 ${
                      isHistoricalDecision
                        ? "text-muted-foreground"
                        : "text-amber-600 dark:text-amber-400"
                    }`}
                  />
                ) : timelineKind === "review-request" ? (
                  <UserPlus className="h-3 w-3" />
                ) : (
                  <MessageSquare className="h-3 w-3" />
                )}
              </span>
            </div>
            <div className="min-w-0 flex-1">
              <div className="flex min-w-0 items-center">
                <span className="min-w-0 truncate">
                  <ProfileAuthorName pubkey={item.author}>
                    {labelForPubkey(item.author, profiles)}
                  </ProfileAuthorName>
                  {timelineKind ? (
                    <>
                      {" "}
                      {timelineKind === "approved"
                        ? isHistoricalDecision
                          ? "approved an earlier commit"
                          : "approved these changes"
                        : timelineKind === "changes-requested"
                          ? isHistoricalDecision
                            ? "requested changes on an earlier commit"
                            : "requested changes"
                          : trimmedContent || "requested a review"}
                    </>
                  ) : null}
                </span>
                <span
                  className="ml-auto w-20 shrink-0 text-right text-xs text-muted-foreground/70"
                  title={formatExactTimestamp(item.createdAt)}
                >
                  {relativeTime(item.createdAt)}
                </span>
                <ProjectItemDeleteMenu
                  author={item.author}
                  label="More options for this comment"
                  project={project}
                  rootId={pullRequest.id}
                  subject="comment"
                  targetId={item.id}
                  testId={`comment-${item.id}`}
                />
              </div>
              {activityContent ? (
                <ProjectRichContent
                  className="mt-1 text-sm text-foreground/90"
                  content={activityContent}
                  tags={item.tags}
                />
              ) : null}
              {item.anchor ? (
                <button
                  aria-label={`Open ${item.anchor.path} ${item.anchor.side} line ${item.anchor.line} in Files changed`}
                  className="mt-1 inline-flex min-w-0 items-center gap-1 rounded-md bg-muted/65 px-1.5 py-0.5 font-mono text-2xs text-muted-foreground transition-colors hover:bg-muted hover:text-foreground"
                  onClick={() => {
                    if (item.anchor) onOpenInlineComment?.(item.anchor);
                  }}
                  type="button"
                >
                  <FileCode2 className="h-3 w-3 shrink-0" />
                  <span className="truncate">{item.anchor.path}</span>
                  <span className="shrink-0">
                    {item.anchor.side === "new" ? "+" : "-"}
                    {item.anchor.line}
                  </span>
                  {item.inlineCommentStatus === "outdated" ? (
                    <span className="shrink-0 text-destructive">Outdated</span>
                  ) : null}
                </button>
              ) : null}
            </div>
          </div>
        );
      })}
      <div className="flex min-h-12 items-start justify-start px-3 py-2.5">
        <PullRequestReviewCard
          onOpenTerminal={onOpenTerminal}
          project={project}
          pullRequest={pullRequest}
        />
      </div>
    </div>
  );
}
