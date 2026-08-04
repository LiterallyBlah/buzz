import {
  KIND_GIT_PR_UPDATE,
  KIND_GIT_STATUS_CLOSED,
  KIND_GIT_STATUS_DRAFT,
  KIND_GIT_STATUS_MERGED,
  KIND_GIT_STATUS_OPEN,
} from "@/shared/constants/kinds";
import type { RelayEvent } from "@/shared/api/types";
import type { WatchedProjectRoot } from "@/features/projects/projectUnreadRoots";

import type { NotificationSettings } from "../hooks";
import { requestDockBounce, sendDesktopNotification } from "./desktop";
import {
  formatNotificationTitle,
  truncateNotificationBody,
} from "./notificationFormat";
import { playNotificationSound, resolveSlotSound } from "./sound";

/**
 * Presentation + delivery for project work-item alerts.
 *
 * Everything here routes through the same primitives the DM and thread-reply
 * alerts use (`sendDesktopNotification` → Linux-safe native path, then
 * `playNotificationSound` + `requestDockBounce`). No parallel notifier: a
 * second delivery path would mean a second set of permission, sound, and
 * Linux D-Bus bugs to keep in sync.
 */

/**
 * Sound/alert slot borrowed for project activity.
 *
 * Projects have no slot of their own. `thread_reply` is the closest existing
 * semantics ("someone replied on something you posted in or follow") and
 * reusing it means the user's existing preference is respected on day one.
 * The cost is that project alerts cannot be silenced independently of thread
 * replies; adding a dedicated `project_activity` slot means touching the
 * settings surface, which is tracked as follow-up rather than smuggled in.
 */
export const PROJECT_NOTIFICATION_SLOT = "thread_reply" as const;

/** Longest work-item title embedded in a notification title. */
const TITLE_MAX_LENGTH = 48;

function shortenTitle(title: string): string {
  const trimmed = title.trim();
  if (trimmed.length === 0) {
    return "a work item";
  }
  return trimmed.length <= TITLE_MAX_LENGTH
    ? trimmed
    : `${trimmed.slice(0, TITLE_MAX_LENGTH - 1).trimEnd()}…`;
}

/**
 * Human label for what happened, by kind.
 *
 * Statuses carry no body text of their own, so the kind IS the message —
 * which is why they get distinct verbs rather than a generic "update".
 */
export function projectNotificationAction(kind: number): string {
  switch (kind) {
    case KIND_GIT_STATUS_OPEN:
      return "Reopened";
    case KIND_GIT_STATUS_MERGED:
      return "Merged";
    case KIND_GIT_STATUS_CLOSED:
      return "Closed";
    case KIND_GIT_STATUS_DRAFT:
      return "Marked draft";
    case KIND_GIT_PR_UPDATE:
      return "New revision on";
    default:
      return "New comment on";
  }
}

export function projectNotificationTitle(
  root: Pick<WatchedProjectRoot, "title" | "projectName">,
  kind: number,
): string {
  return formatNotificationTitle({
    prefix: `${projectNotificationAction(kind)} ${shortenTitle(root.title)}`,
    channelLabel: root.projectName?.trim() || null,
  });
}

export function projectNotificationBody(
  event: Pick<RelayEvent, "content" | "kind">,
): string {
  return truncateNotificationBody(
    event.content,
    // Status events are empty by design; the title already says what changed.
    event.kind === KIND_GIT_PR_UPDATE
      ? "A new revision was pushed."
      : "Opened in Projects.",
  );
}

/**
 * Fire the desktop notification for one project event.
 *
 * `settings` is read by the caller at delivery time (not snapshotted at
 * mount) so toggling alerts off in Settings takes effect immediately.
 */
export async function deliverProjectDesktopNotification(input: {
  event: RelayEvent;
  root: WatchedProjectRoot;
  settings: NotificationSettings;
}): Promise<boolean> {
  const { event, root, settings } = input;

  if (
    !settings.desktopEnabled ||
    !settings.slotAlertsEnabled[PROJECT_NOTIFICATION_SLOT]
  ) {
    return false;
  }

  const didSend = await sendDesktopNotification({
    title: projectNotificationTitle(root, event.kind),
    body: projectNotificationBody(event),
    target: {
      // Projects are not channels; `projectId` is what the action handler
      // routes on.
      channelId: null,
      content: event.content,
      createdAt: event.created_at,
      eventId: event.id,
      kind: event.kind,
      projectId: root.projectId,
      pubkey: event.pubkey,
      threadRootId: root.rootId,
    },
  });

  if (didSend) {
    playNotificationSound(
      resolveSlotSound(settings, PROJECT_NOTIFICATION_SLOT),
    );
    void requestDockBounce();
  }

  return didSend;
}
