import type { RelayEvent } from "@/shared/api/types";

export type ProjectIssueStatus =
  | "Triage"
  | "Backlog"
  | "In Progress"
  | "In Review"
  | "Done"
  | "Closed";

export type ProjectIssueComment = {
  id: string;
  content: string;
  tags: string[][];
  author: string;
  createdAt: number;
};

export type ProjectIssue = {
  id: string;
  title: string;
  content: string;
  tags: string[][];
  author: string;
  createdAt: number;
  repoAddress: string | null;
  channelId: string | null;
  originAgentName: string | null;
  labels: string[];
  recipients: string[];
  status: ProjectIssueStatus;
  statusEventId: string | null;
  statusCreatedAt: number | null;
  updatedAt: number;
  comments: ProjectIssueComment[];
};

export const PROJECT_ISSUE_STATUS: {
  TRIAGE: "Triage";
  BACKLOG: "Backlog";
  IN_PROGRESS: "In Progress";
  IN_REVIEW: "In Review";
  DONE: "Done";
  CLOSED: "Closed";
};

export const PROJECT_ROOT_STATUS_KINDS: number[];

/** Author + repo coordinate of a parsed issue or pull-request root. */
export type ProjectRootLifecycleActors = {
  author: string;
  repoAddress: string | null;
};

export function allowedActorsForRoot(rootEvent: RelayEvent): Set<string>;
export function allowedActorsForProjectRoot(
  root: ProjectRootLifecycleActors,
): Set<string>;
export function referencesProjectRoot(
  event: RelayEvent,
  rootId: string,
  allowUppercase?: boolean,
): boolean;
/** The `e` target and `E` route of a project tombstone, or null. */
export function projectDeletionTargets(
  event: RelayEvent,
): { rootId: string; targetId: string } | null;
/** Whether a tombstone deletes an item, checked against the item's author. */
export function projectDeletionRemoves(
  event: RelayEvent,
  item: { id: string; author: string },
): boolean;
export function mergeProjectIssueEvent(
  issue: ProjectIssue,
  event: RelayEvent,
): ProjectIssue;
export function mergeProjectIssuesEvent(
  issues: ProjectIssue[],
  event: RelayEvent,
): ProjectIssue[];
export function getTag(event: RelayEvent, name: string): string | undefined;
export function getAllTags(event: RelayEvent, name: string): string[];
export function getImetaTags(event: RelayEvent): string[][];
export function eventToProjectIssue(
  issue: RelayEvent,
  statusEvents?: RelayEvent[],
  commentEvents?: RelayEvent[],
): ProjectIssue;
export function projectIssueEventsToIssues(
  issueEvents: RelayEvent[],
  statusEvents?: RelayEvent[],
  commentEvents?: RelayEvent[],
): ProjectIssue[];
export function nextProjectIssueCommentCreatedAt(
  issue: ProjectIssue,
  now: number,
  author: string,
): number;
export function buildGitIssueTags(input: {
  repoAddress: string;
  repoOwner: string;
  title: string;
  labels?: string[];
  recipients?: string[];
}): string[][];
export function buildGitStatusTags(input: {
  issueId: string;
  repoAddress?: string | null;
  repoOwner?: string | null;
  issueAuthor?: string | null;
}): string[][];
export function nextProjectIssueStatusCreatedAt(
  issue: ProjectIssue,
  now: number,
): number;
