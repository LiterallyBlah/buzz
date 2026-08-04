import { CircleCheck, CircleDot, CircleX, MessageSquare } from "lucide-react";
import * as React from "react";
import { toast } from "sonner";

import { ForumComposer } from "@/features/forum/ui/ForumComposer";
import { useCreateProjectIssueCommentMutation } from "@/features/projects/commentMutations";
import {
  type Project,
  type ProjectIssue,
  useProjectIssuesQuery,
} from "@/features/projects/hooks";
import {
  type ProjectIssueLifecycleStatus,
  useUpdateProjectIssueStatusMutation,
} from "@/features/projects/issueMutations";
import { allowedActorsForProjectRoot } from "@/features/projects/projectIssues.mjs";
import { useLiveProjectRoot } from "@/features/projects/useLiveProjectRoot";
import { useIdentityQuery } from "@/shared/api/hooks";
import { Button } from "@/shared/ui/button";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from "@/shared/ui/dropdown-menu";
import {
  resolveUserLabel,
  type UserProfileLookup,
} from "@/features/profile/lib/identity";
import { relativeTime } from "@/features/projects/lib/projectsViewHelpers";
import type { ChannelMember } from "@/shared/api/types";
import { cn } from "@/shared/lib/cn";
import { normalizePubkey } from "@/shared/lib/pubkey";
import {
  ProjectFeedRow,
  ProjectFeedRowCluster,
  ProjectFeedRowMonoCell,
} from "./ProjectFeedRow";
import { OverviewRailSection } from "./ProjectOverviewPanel";
import { ProfileIdentityButton } from "./ProjectProfileIdentity";
import { ProjectActivityIndicator } from "./ProjectActivityIndicator";
import { ProjectRichContent } from "./ProjectRichContent";

export function issueStatusClassName(status: ProjectIssue["status"]) {
  if (status === "Done") return "text-purple-400";
  if (status === "Closed") return "text-destructive";
  return "text-green-500";
}

function issueStatusVisual(status: ProjectIssue["status"]) {
  if (status === "Done") {
    return { className: "text-purple-400", icon: CircleCheck };
  }
  if (status === "Closed") {
    return { className: "text-destructive", icon: CircleX };
  }
  return { className: "text-green-500", icon: CircleDot };
}

function issueMembers(
  project: Project,
  issue: ProjectIssue,
  profiles?: UserProfileLookup,
): ChannelMember[] {
  return [
    ...new Set([
      project.owner,
      issue.author,
      ...project.contributors,
      ...issue.recipients,
    ]),
  ].map((pubkey) => {
    const profile = profiles?.[normalizePubkey(pubkey)];
    return {
      pubkey,
      role: "member" as const,
      isAgent: profile?.isAgent === true,
      joinedAt: new Date(0).toISOString(),
      displayName:
        profile?.displayName?.trim() || profile?.nip05Handle?.trim() || null,
    };
  });
}

function AuthorIdentity({
  profiles,
  pubkey,
  role,
}: {
  profiles?: UserProfileLookup;
  pubkey: string;
  role?: React.ReactNode;
}) {
  const profile = profiles?.[normalizePubkey(pubkey)];
  return (
    <ProfileIdentityButton
      align="center"
      avatarSize="xs"
      avatarUrl={profile?.avatarUrl ?? null}
      isAgent={profile?.isAgent === true}
      label={resolveUserLabel({ profiles, pubkey })}
      pubkey={pubkey}
      role={role}
    />
  );
}

function IssueRow({
  issue,
  onOpen,
  profiles,
}: {
  issue: ProjectIssue;
  onOpen: () => void;
  profiles?: UserProfileLookup;
}) {
  const authorProfile = profiles?.[normalizePubkey(issue.author)];
  const authorLabel = resolveUserLabel({ profiles, pubkey: issue.author });
  const status = issueStatusVisual(issue.status);

  return (
    <ProjectFeedRow
      meta={
        <>
          <ProfileIdentityButton
            avatarClassName="shrink-0"
            avatarSize="xs"
            avatarUrl={authorProfile?.avatarUrl ?? null}
            isAgent={authorProfile?.isAgent === true}
            label={authorLabel}
            pubkey={issue.author}
            showLabel={false}
          />
          <span className="truncate text-foreground/80">
            <span className="font-medium">{authorLabel}</span> created this
            issue {relativeTime(issue.createdAt)}
          </span>
          <span>·</span>
          <span>{issue.status}</span>
          {issue.labels.map((label) => (
            <span
              className="rounded-full border border-border/60 px-1.5 py-0.5 text-2xs"
              key={label}
            >
              {label}
            </span>
          ))}
        </>
      }
      onOpen={onOpen}
      statusIcon={
        <status.icon className={`h-3.5 w-3.5 shrink-0 ${status.className}`} />
      }
      testId="project-issue-row"
      title={issue.title}
      trailing={
        <>
          {issue.comments.length > 0 ? (
            <span className="flex items-center gap-1 text-xs text-muted-foreground">
              <MessageSquare className="h-3.5 w-3.5" />
              {issue.comments.length}
            </span>
          ) : null}
          <ProjectFeedRowCluster>
            <ProjectFeedRowMonoCell
              label={`#${issue.id.slice(0, 8)}`}
              onClick={onOpen}
              title="View issue"
            />
          </ProjectFeedRowCluster>
        </>
      }
    />
  );
}

/** Full issue conversation and comment composer. */
/**
 * The status changes offered for an issue in its current state.
 *
 * Only transitions that move somewhere: offering "Close" on a closed issue
 * publishes an event that changes nothing and leaves the panel looking
 * unresponsive.
 */
function issueStatusActions(
  status: ProjectIssue["status"],
): { label: string; status: ProjectIssueLifecycleStatus }[] {
  const actions: { label: string; status: ProjectIssueLifecycleStatus }[] = [];
  if (status !== "Done")
    actions.push({ label: "Mark done", status: "resolved" });
  if (status !== "Closed")
    actions.push({ label: "Close issue", status: "closed" });
  if (status === "Done" || status === "Closed") {
    actions.push({ label: "Reopen issue", status: "open" });
  }
  if (status !== "Triage")
    actions.push({ label: "Move to triage", status: "draft" });
  return actions;
}

/**
 * Issue status controls (V11: the desktop could change PR status and not issue
 * status, so closing an issue was CLI-only).
 *
 * Shown only to the two pubkeys `allowedActorsForProjectRoot` trusts — the issue
 * author and the repo owner. Anyone else's status event is discarded by the
 * reader, so offering them the control would produce a published event and a
 * panel that never changes.
 */
function ProjectIssueStatusControls({
  issue,
  project,
}: {
  issue: ProjectIssue;
  project: Project;
}) {
  const identityQuery = useIdentityQuery();
  const statusMutation = useUpdateProjectIssueStatusMutation(project);
  const self = identityQuery.data?.pubkey
    ? normalizePubkey(identityQuery.data.pubkey)
    : null;
  const canChangeStatus = React.useMemo(
    () => (self ? allowedActorsForProjectRoot(issue).has(self) : false),
    [issue, self],
  );

  const handleStatusChange = React.useCallback(
    async (status: ProjectIssueLifecycleStatus) => {
      try {
        await statusMutation.mutateAsync({ issue, status });
        toast.success("Issue status updated.");
      } catch (error) {
        toast.error(
          error instanceof Error
            ? error.message
            : "Failed to update issue status.",
        );
      }
    },
    [issue, statusMutation],
  );

  if (!canChangeStatus) return null;
  const actions = issueStatusActions(issue.status);
  if (actions.length === 0) return null;

  return (
    <DropdownMenu>
      <DropdownMenuTrigger asChild>
        <Button
          data-testid="project-issue-status-trigger"
          disabled={statusMutation.isPending}
          size="sm"
          variant="outline"
        >
          {statusMutation.isPending ? "Updating…" : "Change status"}
        </Button>
      </DropdownMenuTrigger>
      <DropdownMenuContent align="end">
        {actions.map((action) => (
          <DropdownMenuItem
            data-testid={`project-issue-status-${action.status}`}
            key={action.status}
            onSelect={() => void handleStatusChange(action.status)}
          >
            {action.label}
          </DropdownMenuItem>
        ))}
      </DropdownMenuContent>
    </DropdownMenu>
  );
}

export function ProjectIssueDetail({
  issue,
  profiles,
  project,
  stackMetaRail = false,
}: {
  issue: ProjectIssue;
  profiles?: UserProfileLookup;
  project: Project;
  stackMetaRail?: boolean;
}) {
  const commentMutation = useCreateProjectIssueCommentMutation(project);
  // Mounted on the detail view rather than the panel: the Home inbox renders
  // this component directly, and an issue open there is just as live.
  useLiveProjectRoot(project.id, issue.id);
  const authorLabel = resolveUserLabel({ profiles, pubkey: issue.author });
  const members = React.useMemo(
    () => issueMembers(project, issue, profiles),
    [issue, profiles, project],
  );
  const handleCommentSubmit = React.useCallback(
    async (
      content: string,
      mentionPubkeys: string[],
      mediaTags?: string[][],
    ) => {
      try {
        await commentMutation.mutateAsync({
          content,
          issue,
          mediaTags,
          mentionPubkeys,
        });
        toast.success("Comment posted.");
      } catch (error) {
        toast.error(
          error instanceof Error ? error.message : "Failed to post comment.",
        );
        throw error;
      }
    },
    [commentMutation, issue],
  );

  return (
    <div
      className={cn(
        "grid",
        !stackMetaRail && "xl:grid-cols-[minmax(0,1fr)_18rem]",
      )}
    >
      <div className="min-w-0 divide-y divide-border/50">
        <header className="space-y-3 p-4">
          <div className="min-w-0">
            <p className="flex items-center gap-1.5 text-xs font-medium text-muted-foreground">
              <CircleDot className="h-3.5 w-3.5" />
              Issue from {authorLabel}
            </p>
            <div className="flex items-start justify-between gap-3">
              <h3 className="mt-1 line-clamp-2 text-base font-semibold text-foreground">
                {issue.title}{" "}
                <span className="font-normal text-muted-foreground">
                  #{issue.id.slice(0, 8)}
                </span>
              </h3>
              <ProjectIssueStatusControls issue={issue} project={project} />
            </div>
          </div>
          {issue.content ? (
            <ProjectRichContent content={issue.content} tags={issue.tags} />
          ) : null}
        </header>

        <ProjectActivityIndicator profiles={profiles} rootEventId={issue.id} />

        <section className="space-y-3 p-4">
          <h4 className="flex items-center gap-1.5 text-sm font-semibold text-foreground">
            <MessageSquare className="h-3.5 w-3.5" />
            Add Your Comment
          </h4>
          {issue.comments.length > 0 ? (
            <div className="space-y-3">
              {issue.comments.map((item) => (
                <article key={item.id}>
                  <div className="mb-2">
                    <AuthorIdentity
                      profiles={profiles}
                      pubkey={item.author}
                      role={relativeTime(item.createdAt)}
                    />
                  </div>
                  <ProjectRichContent content={item.content} tags={item.tags} />
                </article>
              ))}
            </div>
          ) : (
            <p className="text-sm text-muted-foreground">No comments yet.</p>
          )}
          <ForumComposer
            className="border border-border/60 bg-background/45"
            disabled={commentMutation.isPending}
            isSending={commentMutation.isPending}
            members={members}
            onSubmit={handleCommentSubmit}
            placeholder="Add a comment…"
            profiles={profiles}
          />
        </section>
      </div>

      <IssueMetaRail
        issue={issue}
        profiles={profiles}
        stacked={stackMetaRail}
      />
    </div>
  );
}

/** Right-hand meta column for the issue detail view: status, author, labels,
 * and dates — keeps the conversation column focused. */
function IssueMetaRail({
  issue,
  profiles,
  stacked = false,
}: {
  issue: ProjectIssue;
  profiles?: UserProfileLookup;
  stacked?: boolean;
}) {
  const authorProfile = profiles?.[normalizePubkey(issue.author)];
  const authorLabel = resolveUserLabel({ profiles, pubkey: issue.author });
  const status = issueStatusVisual(issue.status);

  return (
    <aside
      className={cn(
        "space-y-6 border-border/60 p-4",
        stacked ? "border-t" : "border-t xl:border-l xl:border-t-0",
      )}
    >
      <OverviewRailSection title="Status">
        <span
          className={`inline-flex items-center gap-1.5 rounded-md border border-border/60 px-2.5 py-1 text-xs font-medium ${status.className}`}
        >
          <status.icon className="h-3.5 w-3.5" />
          {issue.status}
        </span>
      </OverviewRailSection>
      <OverviewRailSection title="Author">
        <ProfileIdentityButton
          align="center"
          avatarSize="xs"
          avatarUrl={authorProfile?.avatarUrl ?? null}
          isAgent={authorProfile?.isAgent === true}
          label={authorLabel}
          pubkey={issue.author}
        />
      </OverviewRailSection>
      {issue.labels.length > 0 ? (
        <OverviewRailSection title="Labels">
          <div className="flex flex-wrap gap-1.5">
            {issue.labels.map((label) => (
              <span
                className="rounded-full border border-border/60 px-1.5 py-0.5 text-2xs text-muted-foreground"
                key={label}
              >
                {label}
              </span>
            ))}
          </div>
        </OverviewRailSection>
      ) : null}
      <OverviewRailSection title="Activity">
        <dl className="space-y-1.5 text-xs text-muted-foreground">
          <div className="flex items-center justify-between gap-3">
            <dt>Created</dt>
            <dd className="font-medium text-foreground">
              {relativeTime(issue.createdAt)}
            </dd>
          </div>
          <div className="flex items-center justify-between gap-3">
            <dt>Updated</dt>
            <dd className="font-medium text-foreground">
              {relativeTime(issue.updatedAt)}
            </dd>
          </div>
        </dl>
      </OverviewRailSection>
    </aside>
  );
}

export function ProjectIssuesPanel({
  onSelectedIssueIdChange,
  profiles,
  project,
  selectedIssueId,
}: {
  onSelectedIssueIdChange: (id: string | null) => void;
  profiles?: UserProfileLookup;
  project: Project;
  selectedIssueId: string | null;
}) {
  const issuesQuery = useProjectIssuesQuery(project);
  const issues = issuesQuery.data ?? [];
  const selectedIssue =
    issues.find((issue) => issue.id === selectedIssueId) ?? null;

  if (issuesQuery.isLoading) {
    return <p className="p-4 text-sm text-muted-foreground">Loading issues…</p>;
  }

  if (issues.length === 0) {
    return (
      <p className="p-4 text-sm text-muted-foreground">
        {issuesQuery.error
          ? "Could not load issues for this repository."
          : "No issues yet."}
      </p>
    );
  }

  if (selectedIssue) {
    return (
      <ProjectIssueDetail
        issue={selectedIssue}
        profiles={profiles}
        project={project}
      />
    );
  }

  return (
    <div className="divide-y divide-border/50">
      {issues.map((issue) => (
        <IssueRow
          issue={issue}
          key={issue.id}
          onOpen={() => onSelectedIssueIdChange(issue.id)}
          profiles={profiles}
        />
      ))}
    </div>
  );
}
