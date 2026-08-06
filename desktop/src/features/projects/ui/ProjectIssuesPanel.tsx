import {
  ArrowDown,
  CircleCheck,
  CircleDot,
  CircleX,
  MessageSquare,
} from "lucide-react";
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
import { useOpenProjectRoot } from "@/features/projects/useLiveProjectRoot";
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
import { workingAgentsKey } from "@/features/projects/lib/projectThreadPin";
import { relativeTime } from "@/features/projects/lib/projectsViewHelpers";
import { useProjectAgentActivity } from "@/features/projects/useProjectAgentActivity";
import type { ChannelMember } from "@/shared/api/types";
import { useElementWidthBreakpoint } from "@/shared/hooks/use-mobile";
import { cn } from "@/shared/lib/cn";
import { normalizePubkey } from "@/shared/lib/pubkey";
import {
  ProjectFeedRow,
  ProjectFeedRowCluster,
  ProjectFeedRowMonoCell,
} from "./ProjectFeedRow";
import { ProjectItemDeleteMenu } from "./ProjectItemDeleteMenu";
import { OverviewRailSection } from "./ProjectOverviewPanel";
import { ProfileIdentityButton } from "./ProjectProfileIdentity";
import { ProjectRichContent } from "./ProjectRichContent";
import { ProjectRootAgentsSection } from "./ProjectRootAgentsSection";
import { useProjectThreadPin } from "./useProjectThreadPin";

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
  project,
}: {
  issue: ProjectIssue;
  onOpen: () => void;
  profiles?: UserProfileLookup;
  project: Project;
}) {
  const authorProfile = profiles?.[normalizePubkey(issue.author)];
  const authorLabel = resolveUserLabel({ profiles, pubkey: issue.author });
  const status = issueStatusVisual(issue.status);

  return (
    <ProjectFeedRow
      // Matches the pull-request row, which has carried its id since it was
      // written. Without it an issue row is the one work item a test can see
      // but cannot address.
      eventId={issue.id}
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
          <ProjectItemDeleteMenu
            author={issue.author}
            label={`More options for ${issue.title}`}
            project={project}
            rootId={issue.id}
            subject="issue"
            targetId={issue.id}
            testId={`issue-${issue.id}`}
            title={issue.title}
          />
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

/**
 * Detail width below which the meta rail cannot sit beside the conversation.
 *
 * Measured on the detail element rather than the viewport because this
 * component renders in two shells of different widths at the same window
 * size — the project screen and the Home inbox pane — and a viewport media
 * query would put the rail beside a column too narrow to hold it in the
 * second one.
 */
const ISSUE_RAIL_MIN_WIDTH_PX = 880;

/** How much of a long issue description is shown before "Show more". */
const ISSUE_BODY_CLAMP_CLASS = "line-clamp-[8]";

function IssueStatusChip({ status }: { status: ProjectIssue["status"] }) {
  const visual = issueStatusVisual(status);
  return (
    <span
      className={`inline-flex shrink-0 items-center gap-1.5 rounded-md border border-border/60 px-2 py-0.5 text-xs font-medium ${visual.className}`}
      data-testid="project-issue-status-chip"
    >
      <visual.icon className="h-3.5 w-3.5" />
      {status}
    </span>
  );
}

/**
 * Whether an element is currently taller than the clamp showing it.
 *
 * Latches: once a body has been measured as overflowing, expanding it removes
 * the clamp and the same measurement reads "fits", so a non-latching version
 * would answer the toggle's own question by hiding the toggle that answered it.
 */
function useClampOverflows<T extends HTMLElement>(
  content: string,
): [React.RefObject<T | null>, boolean] {
  const ref = React.useRef<T>(null);
  const [overflows, setOverflows] = React.useState(false);

  React.useLayoutEffect(() => {
    // Re-measured whenever the description changes: the latch is per-body, so
    // a shorter one has to be able to retire the toggle a longer one earned.
    setOverflows(false);
    const element = ref.current;
    if (!element || !content) return;
    let latched = false;
    const measure = () => {
      if (latched) return;
      if (element.scrollHeight - element.clientHeight > 1) {
        latched = true;
        setOverflows(true);
      }
    };
    measure();
    if (typeof ResizeObserver === "undefined") return;
    // Fonts and images settle after the first layout pass, so a body that
    // overflows by two lines can measure as fitting on mount.
    const observer = new ResizeObserver(measure);
    observer.observe(element);
    return () => observer.disconnect();
  }, [content]);

  return [ref, overflows];
}

/**
 * The issue description, clamped by default.
 *
 * Clamped rather than collapsed-once-scrolled-past, which is what the design
 * discussion asked for: coupling the body's height to the scroll position
 * feeds the container's own scrollTop back into its content height, and the
 * standard result is an oscillation at the collapse boundary. Clamping
 * achieves the thing that was actually wanted — a long description never costs
 * a screen of scrolling — without any scroll-driven layout change at all, and
 * it does so on first paint instead of only after the reader has already paid
 * the scroll once.
 */
function IssueBody({ issue }: { issue: ProjectIssue }) {
  const [expanded, setExpanded] = React.useState(false);
  const [bodyRef, overflows] = useClampOverflows<HTMLDivElement>(issue.content);

  if (!issue.content) return null;

  return (
    <div className="space-y-1.5 px-4 pb-4">
      <div className={cn(!expanded && ISSUE_BODY_CLAMP_CLASS)} ref={bodyRef}>
        <ProjectRichContent content={issue.content} tags={issue.tags} />
      </div>
      {overflows ? (
        <button
          className="rounded-sm text-xs font-medium text-muted-foreground hover:text-foreground focus-visible:outline-hidden focus-visible:ring-1 focus-visible:ring-ring"
          data-testid="project-issue-body-toggle"
          onClick={() => setExpanded((current) => !current)}
          type="button"
        >
          {expanded ? "Show less" : "Show more"}
        </button>
      ) : null}
    </div>
  );
}

/**
 * Title, id and status — the "where am I" line.
 *
 * Sticky in the pinned layout, so the one thing a reader loses by standing at
 * the bottom of a long thread is the one thing that never leaves the viewport.
 * The status chip is rendered only when the rail is not beside us: with the
 * rail visible it would be the same duplicated status this layout exists to
 * remove, and with the rail stacked below every comment it is the only copy
 * a reader can reach.
 */
function IssueThreadHeader({
  authorLabel,
  issue,
  project,
  showStatusChip,
  sticky,
}: {
  authorLabel: string;
  issue: ProjectIssue;
  project: Project;
  showStatusChip: boolean;
  sticky: boolean;
}) {
  return (
    <header
      className={cn(
        "min-w-0 px-4 pb-3 pt-4",
        sticky &&
          "sticky top-0 z-20 border-b border-border/50 bg-background/95 backdrop-blur-sm supports-backdrop-filter:bg-background/80",
      )}
      data-testid="project-issue-thread-header"
    >
      <p className="flex items-center gap-1.5 text-xs font-medium text-muted-foreground">
        <CircleDot className="h-3.5 w-3.5" />
        Issue from {authorLabel}
      </p>
      <div className="mt-1 flex items-start justify-between gap-3">
        <h3
          className={cn(
            "min-w-0 text-base font-semibold text-foreground",
            sticky ? "truncate" : "line-clamp-2",
          )}
          title={issue.title}
        >
          {issue.title}{" "}
          <span className="font-normal text-muted-foreground">
            #{issue.id.slice(0, 8)}
          </span>
        </h3>
        <div className="flex shrink-0 items-center gap-2">
          {showStatusChip ? <IssueStatusChip status={issue.status} /> : null}
          <ProjectIssueStatusControls issue={issue} project={project} />
        </div>
      </div>
    </header>
  );
}

function IssueComments({
  issue,
  padded = false,
  profiles,
  project,
}: {
  issue: ProjectIssue;
  /**
   * Carry the gutter. Set in the pinned layout, where the comments sit
   * directly in the scroll region; the stacked layout nests them in a padded
   * section beside a heading, and a second gutter there would indent the
   * conversation away from everything it belongs to.
   */
  padded?: boolean;
  profiles?: UserProfileLookup;
  project: Project;
}) {
  if (issue.comments.length === 0) {
    return (
      <p className={cn("text-sm text-muted-foreground", padded && "px-4 py-3")}>
        No comments yet.
      </p>
    );
  }
  return (
    <div className={cn("space-y-3", padded && "px-4 py-3")}>
      {issue.comments.map((item) => (
        <article key={item.id}>
          <div className="mb-2 flex min-w-0 items-center justify-between gap-2">
            <AuthorIdentity
              profiles={profiles}
              pubkey={item.author}
              role={relativeTime(item.createdAt)}
            />
            <ProjectItemDeleteMenu
              author={item.author}
              label="More options for this comment"
              project={project}
              rootId={issue.id}
              subject="comment"
              targetId={item.id}
              testId={`comment-${item.id}`}
            />
          </div>
          <ProjectRichContent content={item.content} tags={item.tags} />
        </article>
      ))}
    </div>
  );
}

export function ProjectIssueDetail({
  fillHeight = false,
  issue,
  profiles,
  project,
  stackMetaRail = false,
}: {
  /**
   * Own the height given by the parent: the thread scrolls inside this
   * component instead of adding to a page that scrolls around it.
   *
   * Opt-in rather than the only behaviour, because the Home inbox renders this
   * detail inside its own scroll container with its own padding; giving it a
   * second nested scroll region is the trackpad hazard the design discussion
   * flagged, and it is not what this phase was scoped to change.
   */
  fillHeight?: boolean;
  issue: ProjectIssue;
  profiles?: UserProfileLookup;
  project: Project;
  stackMetaRail?: boolean;
}) {
  const commentMutation = useCreateProjectIssueCommentMutation(project);
  // Mounted on the detail view rather than the panel: the Home inbox renders
  // this component directly, and an issue open there is just as open.
  useOpenProjectRoot(project.id, issue.id);
  const authorLabel = resolveUserLabel({ profiles, pubkey: issue.author });
  const [detailRef, isNarrow] = useElementWidthBreakpoint<HTMLDivElement>(
    ISSUE_RAIL_MIN_WIDTH_PX,
  );
  // Subscribed only in the pinned layout, which is the only one with a pill to
  // feed. `null` is the hook's own "do not open a REQ", so the Home inbox does
  // not pay for a second activity subscription to answer a question its layout
  // never asks.
  const liveActivity = useProjectAgentActivity(fillHeight ? issue.id : null);
  const pin = useProjectThreadPin({
    commentCount: issue.comments.length,
    rootId: issue.id,
    workingAgents: workingAgentsKey(liveActivity),
  });
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

  const composer = (
    <ForumComposer
      className="border border-border/60 bg-background/45"
      compact={fillHeight}
      disabled={commentMutation.isPending}
      isSending={commentMutation.isPending}
      members={members}
      onSubmit={handleCommentSubmit}
      placeholder="Add a comment…"
      profiles={profiles}
    />
  );

  if (!fillHeight) {
    return (
      <div
        className={cn(
          "grid",
          !stackMetaRail && "xl:grid-cols-[minmax(0,1fr)_18rem]",
        )}
      >
        <div className="min-w-0 divide-y divide-border/50">
          <div>
            <IssueThreadHeader
              authorLabel={authorLabel}
              issue={issue}
              project={project}
              showStatusChip={false}
              sticky={false}
            />
            <IssueBody issue={issue} />
          </div>
          <section className="space-y-3 p-4">
            <h4 className="flex items-center gap-1.5 text-sm font-semibold text-foreground">
              <MessageSquare className="h-3.5 w-3.5" />
              Conversation
            </h4>
            <IssueComments
              issue={issue}
              profiles={profiles}
              project={project}
            />
            {composer}
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

  const rail = (
    <IssueMetaRail
      className={
        isNarrow
          ? "border-y border-border/60"
          : "min-h-0 overflow-y-auto border-l border-border/60"
      }
      issue={issue}
      profiles={profiles}
    />
  );

  return (
    <div
      className={cn(
        "flex min-h-0 flex-1 flex-col",
        !isNarrow && "grid grid-cols-[minmax(0,1fr)_18rem]",
      )}
      data-testid="project-issue-detail"
      ref={detailRef}
    >
      <div className="flex min-h-0 min-w-0 flex-col">
        <div
          className="min-h-0 flex-1 overflow-y-auto overscroll-contain"
          data-testid="project-issue-thread-scroll"
          onScroll={pin.handleScroll}
          ref={pin.scrollRef}
        >
          <div ref={pin.contentRef}>
            <IssueThreadHeader
              authorLabel={authorLabel}
              issue={issue}
              project={project}
              showStatusChip={isNarrow}
              sticky
            />
            <IssueBody issue={issue} />
            {/* Too narrow for a column beside the conversation, so the rail
                joins the scroll region — above the comments, not below them.
                Below, it would be what the thread pins to: the reader would
                be dropped at the bottom of the page and find reference
                material where the newest comment should be. Status stays
                reachable from the sticky header instead. */}
            {isNarrow ? rail : null}
            <IssueComments
              issue={issue}
              padded
              profiles={profiles}
              project={project}
            />
          </div>
        </div>

        <div className="relative shrink-0 border-t border-border/50 p-3">
          {pin.unreadBelow > 0 || pin.activitySettledBelow ? (
            <button
              className="-translate-x-1/2 absolute -top-4 left-1/2 z-10 flex items-center gap-1.5 rounded-full border border-border/60 bg-background px-3 py-1 text-xs font-medium text-foreground shadow-md hover:bg-muted focus-visible:outline-hidden focus-visible:ring-1 focus-visible:ring-ring"
              data-testid="project-issue-jump-to-latest"
              onClick={() => pin.scrollToBottom("smooth")}
              type="button"
            >
              {/* A settled turn with nothing to count says so instead of
                  claiming a comment. An agent that finished without posting one
                  is still the thing the reader came back for, but "1 new" would
                  send them to the bottom looking for a reply that is not
                  there. */}
              {pin.unreadBelow > 0 ? `${pin.unreadBelow} new` : "New activity"}
              <ArrowDown className="h-3.5 w-3.5" />
            </button>
          ) : null}
          {/* Focusing the composer from a scrolled-up position brings the
              newest comment with it. Typing at the bottom of the viewport
              while the comment being answered sits off-screen above is the
              thing that makes a sent reply feel like it went nowhere. */}
          <div onFocusCapture={() => pin.scrollToBottom("smooth")}>
            {composer}
          </div>
        </div>
      </div>

      {isNarrow ? null : rail}
    </div>
  );
}

/**
 * Right-hand meta column for the issue detail view: status, author, agents,
 * labels and dates — keeps the conversation column focused.
 *
 * In the pinned layout this column no longer scrolls with the thread. It does
 * not get that by being `sticky`: the conversation beside it owns its own
 * scroll region, so the rail is simply a grid cell that never moves, which is
 * the same outcome without a stacking context or a top offset to keep in sync.
 */
function IssueMetaRail({
  className,
  issue,
  profiles,
  stacked = false,
}: {
  /** Layout override for the pinned layout, which places the rail itself. */
  className?: string;
  issue: ProjectIssue;
  profiles?: UserProfileLookup;
  stacked?: boolean;
}) {
  const authorProfile = profiles?.[normalizePubkey(issue.author)];
  const authorLabel = resolveUserLabel({ profiles, pubkey: issue.author });

  return (
    <aside
      className={cn(
        "space-y-6 border-border/60 p-4",
        className ??
          (stacked ? "border-t" : "border-t xl:border-l xl:border-t-0"),
      )}
      data-testid="project-issue-meta-rail"
    >
      <OverviewRailSection title="Status">
        <IssueStatusChip status={issue.status} />
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
      {/* Above Labels and Activity, below Author: the rail runs from "who owns
          this" to "what has happened to it", and who has been working on it
          belongs on the people side of that line. */}
      <ProjectRootAgentsSection
        commentAuthors={issue.comments}
        profiles={profiles}
        rootEventId={issue.id}
      />
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
  fillHeight = false,
  onSelectedIssueIdChange,
  profiles,
  project,
  selectedIssueId,
}: {
  /** Forwarded to the open issue: the thread owns the scroll region. */
  fillHeight?: boolean;
  onSelectedIssueIdChange: (id: string | null) => void;
  profiles?: UserProfileLookup;
  project: Project;
  selectedIssueId: string | null;
}) {
  const issuesQuery = useProjectIssuesQuery(project);
  const issues = issuesQuery.data ?? [];
  const selectedIssue =
    issues.find((issue) => issue.id === selectedIssueId) ?? null;

  // An issue that is gone — deleted here or by its author elsewhere — must not
  // leave the surrounding view pointing at it, or the Issues tab stays selected
  // on a detail nothing can render. Gated on loaded data so a refetch in flight
  // does not drop a selection that is about to come back.
  React.useEffect(() => {
    if (!selectedIssueId || !issuesQuery.data) return;
    if (!issuesQuery.data.some((issue) => issue.id === selectedIssueId)) {
      onSelectedIssueIdChange(null);
    }
  }, [issuesQuery.data, onSelectedIssueIdChange, selectedIssueId]);

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
        fillHeight={fillHeight}
        issue={selectedIssue}
        profiles={profiles}
        project={project}
      />
    );
  }

  return (
    // The list keeps its own overflow under `fillHeight` so that the moment
    // between an open issue disappearing and the selection clearing shows a
    // scrollable list rather than one clipped by a container sized for a
    // thread.
    <div
      className={cn(
        "divide-y divide-border/50",
        fillHeight && "min-h-0 flex-1 overflow-y-auto",
      )}
    >
      {issues.map((issue) => (
        <IssueRow
          issue={issue}
          key={issue.id}
          onOpen={() => onSelectedIssueIdChange(issue.id)}
          profiles={profiles}
          project={project}
        />
      ))}
    </div>
  );
}
