import * as React from "react";
import { useQueryClient } from "@tanstack/react-query";

import { channelsQueryKey } from "@/features/channels/hooks";
import { mergeTimelineCacheMessages } from "@/features/messages/hooks";
import { channelMessagesKey } from "@/features/messages/lib/messageQueryKeys";
import {
  getChannelIdFromTags,
  isThreadReply,
} from "@/features/messages/lib/threading";
import { shouldNotifyForEvent } from "@/features/notifications/lib/shouldNotify";
import {
  createLiveSubscriptionSet,
  DEFAULT_LIVE_SUBSCRIPTION_RETRY,
  type LiveSubscriptionSet,
} from "@/shared/api/liveSubscriptionSet";
import { relayClient } from "@/shared/api/relayClient";
import {
  CHANNEL_EVENT_KINDS,
  CHANNEL_MESSAGE_EVENT_KINDS,
} from "@/shared/constants/kinds";
import type { Channel, RelayEvent } from "@/shared/api/types";
import {
  createTrailingDebounce,
  type TrailingDebounce,
} from "@/shared/lib/trailingDebounce";

import { isDmNotifiableKind } from "./isDmNotifiableKind";
import { refreshChannelsWhenIdle } from "./refreshChannelsWhenIdle";

export type UseLiveChannelUpdatesOptions = {
  currentPubkey?: string;
  /**
   * When true, DM notifications also fire for the channel the user is
   * currently viewing (normally suppressed).
   */
  notifyForActiveChannel?: boolean;
  onDmMessage?: (event: RelayEvent, channel: Channel) => void;
  onLiveMention?: () => void;
  /**
   * Fired for live "new content" events in a member channel authored by
   * someone other than the current user. Thread replies also fire
   * onThreadReplyNotification so Home inbox activity stays in sync. Used to
   * drive the observed unread-event map that powers sidebar unread state.
   * See `UNREAD_TRIGGER_KINDS` for the exact kind set.
   */
  onChannelMessage?: (channelId: string, event: RelayEvent) => void;
  /**
   * Fired for thread replies that should be surfaced as Home inbox activity.
   */
  onThreadReplyNotification?: (channelId: string, event: RelayEvent) => void;
  /**
   * Fired for external thread replies that do not match the locally-known
   * interest sets. Callers can perform an async backfill and then decide
   * whether to surface the event.
   */
  onThreadReplyCandidate?: (channelId: string, event: RelayEvent) => void;
  /**
   * Fired for replies in threads the user authored, participated in, or
   * follows (non-DM channels only — the DM path owns those). Follows the DM
   * active-channel rule: suppressed for the channel being viewed unless
   * notifyForActiveChannel opts in.
   */
  onThreadReplyDesktopNotification?: (
    channelId: string,
    event: RelayEvent,
  ) => void;
  onSelfChannelMessage?: (event: RelayEvent) => void;
  participatedRootIds?: ReadonlySet<string>;
  followedRootIds?: ReadonlySet<string>;
  authoredRootIds?: ReadonlySet<string>;
  mutedRootIds?: ReadonlySet<string>;
  mutedChannelIds?: ReadonlySet<string>;
};

/**
 * Mention subscriptions are identified by (pubkey, channel), not by channel.
 *
 * The REQ carries the reader's pubkey, so signing in as a different account
 * makes every open mention subscription wrong — it is still watching for the
 * previous account's name. Putting both in the key lets the ordinary set diff
 * perform the swap (every stale key leaves, every new one arrives) instead of
 * a bespoke "did the pubkey change?" reset path alongside it.
 */
const MENTION_KEY_SEPARATOR = "|";

type MentionSubscriptionRequest = { channelId: string; pubkey: string };

function mentionSubscriptionKey(pubkey: string, channelId: string) {
  return `${pubkey}${MENTION_KEY_SEPARATOR}${channelId}`;
}

/**
 * Split on the *first* separator only: the pubkey is normalized lowercase hex
 * and cannot contain one, while a channel id is whatever the channel says it
 * is — parsing from the left is the half that is guaranteed unambiguous.
 */
function parseMentionSubscriptionKey(key: string): MentionSubscriptionRequest {
  const separatorIndex = key.indexOf(MENTION_KEY_SEPARATOR);
  if (separatorIndex < 0) {
    return { channelId: key, pubkey: "" };
  }
  return {
    channelId: key.slice(separatorIndex + MENTION_KEY_SEPARATOR.length),
    pubkey: key.slice(0, separatorIndex),
  };
}

/**
 * One live subscription per visible channel, reconciled against the channel
 * list: a refetch that returns the same ids costs zero REQs, and a channel
 * that stays in the list keeps the subscription it already had.
 *
 * A factory rather than an inline literal because the hook has to be able to
 * build a second one: a disposed set stays closed by design, and StrictMode's
 * simulated unmount/remount would otherwise leave the hook holding a set that
 * can never open again.
 */
function createChannelMessageSubscriptions(params: {
  onChannelEvent: (event: RelayEvent, channelId: string) => void;
  onSubscriptionWindowOpen: (nowSeconds: number) => void;
}): LiveSubscriptionSet {
  return createLiveSubscriptionSet({
    buildGroup: (channelId, { nowSeconds }) => [
      {
        kinds: [...CHANNEL_EVENT_KINDS],
        "#h": [channelId],
        limit: 1000,
        since: nowSeconds,
      },
    ],
    open: (filter, onEvent) => relayClient.subscribeLive(filter, onEvent),
    // A channel is one filter, and each one stands alone: a channel whose REQ
    // was rejected must not cost its neighbours their subscriptions.
    groupOpenPolicy: "perFilter",
    onEvent: params.onChannelEvent,
    retry: DEFAULT_LIVE_SUBSCRIPTION_RETRY,
    onBeforeOpen: (channelIds, { nowSeconds }) => {
      if (channelIds.length > 0) params.onSubscriptionWindowOpen(nowSeconds);
    },
    onError: (error, channelId) => {
      console.error(
        "Failed to subscribe to live channel updates",
        channelId,
        error,
      );
    },
    // Reconnects are handled by the hook's own listener, not here: the relay
    // session replays the REQs it accepted, and what this hook owes a
    // reconnect is a channel-list refetch plus a fresh backlog cutoff, neither
    // of which is subscription management.
  });
}

/**
 * Mention subscriptions: the same lifecycle, opened through the relay client's
 * own mention-filter builder rather than a filter this file writes, and keyed
 * by (pubkey, channel) because both are baked into the REQ.
 */
function createMentionSubscriptions(
  onMentionEvent: (event: RelayEvent) => void,
): LiveSubscriptionSet {
  return createLiveSubscriptionSet<MentionSubscriptionRequest>({
    buildGroup: (key) => [parseMentionSubscriptionKey(key)],
    open: (request, onEvent) =>
      relayClient.subscribeToChannelMentionEvents(
        request.channelId,
        request.pubkey,
        onEvent,
      ),
    groupOpenPolicy: "perFilter",
    onEvent: (event) => onMentionEvent(event),
    retry: DEFAULT_LIVE_SUBSCRIPTION_RETRY,
    onError: (error, key) => {
      console.error(
        "Failed to subscribe to mention events",
        parseMentionSubscriptionKey(key).channelId,
        error,
      );
    },
  });
}

// get_channels is an expensive O(channels) relay fan-out. Incoming traffic for
// non-active channels arrives in bursts, so coalesce the refetch into a single
// trailing invalidation instead of one per event.
const CHANNELS_INVALIDATE_DEBOUNCE_MS = 500;

// Only "new content" kinds should bump unread state. Shared with the
// catch-up query in useUnreadChannels so the two paths stay in lockstep.
const UNREAD_TRIGGER_KINDS = new Set<number>(CHANNEL_MESSAGE_EVENT_KINDS);

export const EMPTY_SET: ReadonlySet<string> = new Set();

export function isChannelUnreadTriggerKind(kind: number, isDmChannel: boolean) {
  return isDmChannel
    ? isDmNotifiableKind(kind)
    : UNREAD_TRIGGER_KINDS.has(kind);
}

export function isHomeActivityEvent(
  isDmChannel: boolean,
  isThreadedReply: boolean,
) {
  return isThreadedReply || isDmChannel;
}

export function withChannelTagFallback(
  event: RelayEvent,
  channelId: string,
): RelayEvent {
  return getChannelIdFromTags(event.tags)
    ? event
    : { ...event, tags: [...event.tags, ["h", channelId]] };
}

function isExternalMentionEvent(event: RelayEvent, currentPubkey: string) {
  return (
    currentPubkey.length > 0 && event.pubkey.toLowerCase() !== currentPubkey
  );
}

const SEEN_NOTIFICATION_EVENT_LIMIT = 5_000;

export function trackSeenEvent(
  seenEventIds: Set<string>,
  eventId: string,
  limit = 200,
): boolean {
  if (seenEventIds.has(eventId)) {
    return false;
  }

  seenEventIds.add(eventId);
  if (seenEventIds.size > limit) {
    const oldestEventId = seenEventIds.values().next().value;
    if (oldestEventId) {
      seenEventIds.delete(oldestEventId);
    }
  }

  return true;
}

export function useLiveChannelUpdates(
  channels: Channel[],
  activeChannelId: string | null,
  options: UseLiveChannelUpdatesOptions = {},
) {
  const queryClient = useQueryClient();
  const normalizedCurrentPubkey =
    options.currentPubkey?.trim().toLowerCase() ?? "";
  const seenMentionEventIdsRef = React.useRef(new Set<string>());
  // Reconnect replay overlaps each live filter by five seconds so no message is
  // lost at the boundary. Keep one shared guard for every notification side
  // effect: the same event can be replayed repeatedly while a relay flaps, and
  // mention events also arrive through both the channel and mention filters.
  const seenNotificationEventIdsRef = React.useRef(new Set<string>());
  const channelsInvalidateRef = React.useRef<TrailingDebounce | null>(null);
  if (channelsInvalidateRef.current === null) {
    channelsInvalidateRef.current = createTrailingDebounce(() => {
      refreshChannelsWhenIdle({
        isFetching: () =>
          queryClient.isFetching({ queryKey: channelsQueryKey }),
        invalidate: () => {
          void queryClient.invalidateQueries({ queryKey: channelsQueryKey });
        },
        reArm: () => channelsInvalidateRef.current?.trigger(),
      });
    }, CHANNELS_INVALIDATE_DEBOUNCE_MS);
  }
  const invalidateChannelsDebounced = React.useCallback(() => {
    channelsInvalidateRef.current?.trigger();
  }, []);
  const liveChannelIds = React.useMemo(
    () => new Set(channels.map((channel) => channel.id)),
    [channels],
  );
  const dmChannelMap = React.useMemo(
    () =>
      new Map(
        channels
          .filter((channel) => channel.channelType === "dm")
          .map((channel) => [channel.id, channel]),
      ),
    [channels],
  );
  const dmSubscriptionStartedAtRef = React.useRef(0);

  // Reset subscription timestamp when identity changes.
  React.useEffect(() => {
    void normalizedCurrentPubkey;
    dmSubscriptionStartedAtRef.current = 0;
  }, [normalizedCurrentPubkey]);

  // Effect deps use primitive keys so refetches that produce new refs with
  // identical contents don't churn subscriptions. The Set/array memos are
  // still handy for closure reads via useEffectEvent.
  const channelIdsKey = React.useMemo(
    () => [...new Set(channels.map((channel) => channel.id))].sort().join(","),
    [channels],
  );

  const handleDmEvent = React.useEffectEvent(
    (event: RelayEvent, isFirstNotificationDelivery: boolean) => {
      // Only human-visible message kinds should fire DM notifications.
      if (!isDmNotifiableKind(event.kind) || !isFirstNotificationDelivery) {
        return;
      }

      // Suppress backlog events that predate our subscription — these are
      // historical replays, not live messages.
      if (event.created_at < dmSubscriptionStartedAtRef.current) {
        return;
      }

      const channelId = getChannelIdFromTags(event.tags);
      if (!channelId) {
        return;
      }

      if (!isExternalMentionEvent(event, normalizedCurrentPubkey)) {
        return;
      }

      const dmChannel = dmChannelMap.get(channelId);
      if (!dmChannel) {
        return;
      }

      // Don't fire a notification for the channel the user is already viewing,
      // unless the notify-while-viewing setting opts in.
      if (channelId === activeChannelId && !options.notifyForActiveChannel) {
        return;
      }

      options.onDmMessage?.(event, dmChannel);
    },
  );

  const handleIncomingMessage = React.useEffectEvent((event: RelayEvent) => {
    const channelId = getChannelIdFromTags(event.tags);
    if (!channelId) {
      return;
    }

    if (!liveChannelIds.has(channelId)) {
      if (channelId !== activeChannelId) {
        invalidateChannelsDebounced();
      }
      return;
    }

    const isDmChannel = dmChannelMap.has(channelId);
    const isUnreadTriggerKind = isChannelUnreadTriggerKind(
      event.kind,
      isDmChannel,
    );

    // Let the caller observe self-authored trigger events (e.g. to track
    // thread participation) before the author-exclusion guard filters them.
    if (
      isUnreadTriggerKind &&
      normalizedCurrentPubkey.length > 0 &&
      event.pubkey.toLowerCase() === normalizedCurrentPubkey
    ) {
      options.onSelfChannelMessage?.(event);
    }

    // Notify the unread tracker. Restricted to human-visible message kinds
    // and to events authored by someone other than the current user — your
    // own outgoing messages should never make a channel unread, and
    // reactions / edits / system messages aren't "new content".
    const isExternalTriggerEvent =
      isUnreadTriggerKind &&
      (normalizedCurrentPubkey.length === 0 ||
        event.pubkey.toLowerCase() !== normalizedCurrentPubkey);
    const isFirstNotificationDelivery =
      !isExternalTriggerEvent ||
      trackSeenEvent(
        seenNotificationEventIdsRef.current,
        event.id,
        SEEN_NOTIFICATION_EVENT_LIMIT,
      );
    const isThreadedReply = isThreadReply(event.tags);

    // DM alerts and every other notification side effect share this delivery
    // decision, preventing a replayed event from escaping through a second
    // callback path.
    handleDmEvent(event, isFirstNotificationDelivery);

    if (isExternalTriggerEvent && isFirstNotificationDelivery) {
      const shouldNotify = shouldNotifyForEvent(
        event,
        normalizedCurrentPubkey,
        {
          participatedRootIds: options.participatedRootIds ?? EMPTY_SET,
          followedRootIds: options.followedRootIds ?? EMPTY_SET,
          authoredRootIds: options.authoredRootIds ?? EMPTY_SET,
          mutedRootIds: options.mutedRootIds ?? EMPTY_SET,
          mutedChannelIds: options.mutedChannelIds ?? EMPTY_SET,
          channelId,
        },
      );

      if (!shouldNotify) {
        if (isThreadedReply) {
          options.onThreadReplyCandidate?.(channelId, event);
        }
      } else {
        options.onChannelMessage?.(channelId, event);
        if (isHomeActivityEvent(isDmChannel, isThreadedReply)) {
          options.onThreadReplyNotification?.(channelId, event);
        }
      }

      if (shouldNotify && isThreadedReply) {
        if (
          !dmChannelMap.has(channelId) &&
          (channelId !== activeChannelId || options.notifyForActiveChannel)
        ) {
          options.onThreadReplyDesktopNotification?.(channelId, event);
        }
      }
    }

    // Merge into the timeline cache for the active channel.
    // useChannelSubscription also writes to this cache, but there's a
    // race window where it hasn't connected yet. Writes are idempotent
    // (mergeTimelineCacheMessages deduplicates by event ID).
    queryClient.setQueryData<RelayEvent[]>(
      channelMessagesKey(channelId),
      (current) => {
        if (!current) {
          return current;
        }

        return mergeTimelineCacheMessages(current, event);
      },
    );
  });

  const handleMentionEvent = React.useEffectEvent((event: RelayEvent) => {
    if (!isExternalMentionEvent(event, normalizedCurrentPubkey)) {
      return;
    }

    if (!trackSeenEvent(seenMentionEventIdsRef.current, event.id)) {
      return;
    }

    handleIncomingMessage(event);
    options.onLiveMention?.();
  });

  React.useEffect(() => {
    return relayClient.subscribeToReconnects(() => {
      void queryClient.invalidateQueries({ queryKey: channelsQueryKey });

      // Update the subscription timestamp so replayed backlog events
      // (which have created_at in the past) are naturally suppressed.
      dmSubscriptionStartedAtRef.current = Math.floor(Date.now() / 1000);
    });
  }, [queryClient]);

  const liveChannelSubsRef = React.useRef<LiveSubscriptionSet | null>(null);
  const mentionSubsRef = React.useRef<LiveSubscriptionSet | null>(null);

  React.useEffect(() => {
    if (liveChannelSubsRef.current === null) {
      liveChannelSubsRef.current = createChannelMessageSubscriptions({
        // handleIncomingMessage is a stable useEffectEvent, so a subscription
        // that outlives the effect run that opened it still reaches the
        // current render's closure — the reason these subs are diffed rather
        // than torn down.
        onChannelEvent: (event, channelId) =>
          handleIncomingMessage(withChannelTagFallback(event, channelId)),
        // Record the subscription start time so handleDmEvent can distinguish
        // backlog replays (created_at < startedAt) from live messages. It has
        // to move on every sync pass, retries included: the window that
        // matters starts when the REQ is sent, not when the list changed.
        onSubscriptionWindowOpen: (nowSeconds) => {
          dmSubscriptionStartedAtRef.current = nowSeconds;
        },
      });
    }

    liveChannelSubsRef.current.setKeys(
      channelIdsKey ? channelIdsKey.split(",") : [],
    );
  }, [channelIdsKey]);

  React.useEffect(() => {
    if (!options.onLiveMention || normalizedCurrentPubkey.length === 0) {
      // Deliberately not a teardown: subscriptions already open stay open
      // until the identity changes (which re-keys them) or the hook unmounts.
      return;
    }

    if (mentionSubsRef.current === null) {
      mentionSubsRef.current = createMentionSubscriptions(handleMentionEvent);
    }

    mentionSubsRef.current.setKeys(
      (channelIdsKey ? channelIdsKey.split(",") : []).map((channelId) =>
        mentionSubscriptionKey(normalizedCurrentPubkey, channelId),
      ),
    );
  }, [channelIdsKey, normalizedCurrentPubkey, options.onLiveMention]);

  React.useEffect(() => {
    return () => {
      channelsInvalidateRef.current?.cancel();

      // Null the refs: a disposed set stays closed, so a remount (StrictMode
      // simulates one) has to build a fresh pair rather than reuse these.
      void liveChannelSubsRef.current?.dispose();
      liveChannelSubsRef.current = null;
      void mentionSubsRef.current?.dispose();
      mentionSubsRef.current = null;
    };
  }, []);
}
