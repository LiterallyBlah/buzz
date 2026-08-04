import { useEffect, useEffectEvent, useMemo, useRef, useState } from "react";

import {
  getChannelIdFromTags,
  getThreadReference,
} from "@/features/messages/lib/threading";
import { relayClient } from "@/shared/api/relayClient";
import type { Channel, RelayEvent } from "@/shared/api/types";
import {
  KIND_STREAM_MESSAGE,
  KIND_STREAM_MESSAGE_DIFF,
  KIND_TYPING_INDICATOR,
} from "@/shared/constants/kinds";
import { resolveEventAuthorPubkey } from "@/shared/lib/authors";
import {
  createLivenessMap,
  type LivenessState,
} from "@/shared/lib/livenessStore";
import { useLivenessSweep } from "@/shared/lib/useLivenessSweep";

export type TypingIndicatorEntry = {
  pubkey: string;
  threadHeadId: string | null;
};

const TYPING_INDICATOR_TTL_MS = 8_000;
const TYPING_PRUNE_INTERVAL_MS = 1_000;
const TYPING_POST_MESSAGE_SUPPRESS_MS = 2_000;

/**
 * Typing is a liveness signal like any other: a 20002 frame puts one typist on
 * the channel, the next frame keeps them there, the message they were writing
 * takes them off, and silence expires them after the TTL. The shared core owns
 * that; what stays in this hook is what is specific to typing — whose frames
 * count, which frames the composer's own message should suppress, and the
 * thread scoping.
 *
 * Ordering is by when a typist first appeared, so the list does not reshuffle
 * under the reader every time somebody's indicator refreshes. `firstSeenAt` is
 * therefore liveness metadata, not part of the entry: it survives refreshes and
 * is not something the UI renders.
 */
const typingLiveness = createLivenessMap<TypingIndicatorEntry>({
  ttlMs: TYPING_INDICATOR_TTL_MS,
  // Consecutive frames from one typist in one thread say exactly the same
  // thing, so they are pure refreshes: the deadline moves and React does not
  // re-render.
  sameValue: (existing, incoming) =>
    existing.pubkey === incoming.pubkey &&
    existing.threadHeadId === incoming.threadHeadId,
  compare: (a, b) => a.firstSeenAt - b.firstSeenAt,
});

type TypingState = LivenessState<TypingIndicatorEntry>;

function isTypingCompletionEvent(event: RelayEvent | null | undefined) {
  if (!event) {
    return false;
  }

  return (
    event.kind === KIND_STREAM_MESSAGE ||
    event.kind === KIND_STREAM_MESSAGE_DIFF
  );
}

function getTypingScopeId(event: RelayEvent) {
  return getThreadReference(event.tags).parentId ?? null;
}

function getTypingStateKey(pubkey: string, threadHeadId: string | null) {
  return `${pubkey}:${threadHeadId ?? "channel"}`;
}

export function useChannelTyping(
  channel: Channel | null,
  currentPubkey?: string,
  latestMessageEvent?: RelayEvent | null,
  relaySelfPubkey?: string | null,
) {
  const channelId = channel?.id ?? null;
  const channelType = channel?.channelType ?? null;
  const [typingByPubkey, setTypingByPubkey] = useState<TypingState>(
    typingLiveness.empty,
  );
  const normalizedCurrentPubkey = currentPubkey?.toLowerCase();
  const typingSuppressUntilByPubkeyRef = useRef<Record<string, number>>({});
  const latestMessageCreatedAtByPubkeyRef = useRef<Record<string, number>>({});

  const registerTyping = useEffectEvent((event: RelayEvent) => {
    if (!channelId || event.kind !== KIND_TYPING_INDICATOR) {
      return;
    }

    const now = Date.now();
    const eventExpiresAt = event.created_at * 1_000 + TYPING_INDICATOR_TTL_MS;
    if (eventExpiresAt <= now) {
      return;
    }

    if (getChannelIdFromTags(event.tags) !== channelId) {
      return;
    }

    const typingPubkey = event.pubkey.toLowerCase();
    const threadHeadId = getTypingScopeId(event);
    const typingKey = getTypingStateKey(typingPubkey, threadHeadId);
    if (normalizedCurrentPubkey && typingPubkey === normalizedCurrentPubkey) {
      return;
    }

    const suppressUntil =
      typingSuppressUntilByPubkeyRef.current[typingKey] ?? 0;
    if (suppressUntil > Date.now()) {
      return;
    }
    if (suppressUntil > 0) {
      delete typingSuppressUntilByPubkeyRef.current[typingKey];
    }

    const latestMessageCreatedAt =
      latestMessageCreatedAtByPubkeyRef.current[typingKey] ?? 0;
    if (event.created_at <= latestMessageCreatedAt) {
      return;
    }

    setTypingByPubkey((current) =>
      typingLiveness.upsert(
        typingLiveness.prune(current, now),
        typingKey,
        { pubkey: typingPubkey, threadHeadId },
        // The frame's own `created_at` caps the deadline: a typist's client
        // declares how long its claim is good for, and a slow relay must not
        // extend it past that.
        { nowMs: now, frameAtMs: event.created_at * 1_000 },
      ),
    );
  });

  // biome-ignore lint/correctness/useExhaustiveDependencies: channel changes should clear local typing state
  useEffect(() => {
    setTypingByPubkey(typingLiveness.empty);
    typingSuppressUntilByPubkeyRef.current = {};
    latestMessageCreatedAtByPubkeyRef.current = {};
  }, [channelId]);

  useEffect(() => {
    if (
      !channelId ||
      !latestMessageEvent ||
      !isTypingCompletionEvent(latestMessageEvent)
    ) {
      return;
    }

    if (getChannelIdFromTags(latestMessageEvent.tags) !== channelId) {
      return;
    }

    const authorPubkey = resolveEventAuthorPubkey({
      event: latestMessageEvent,
      preferActorTag: true,
      relaySelfPubkey,
      requireChannelTagForPTags: true,
    }).toLowerCase();
    const threadHeadId = getTypingScopeId(latestMessageEvent);
    const typingKey = getTypingStateKey(authorPubkey, threadHeadId);
    latestMessageCreatedAtByPubkeyRef.current[typingKey] = Math.max(
      latestMessageCreatedAtByPubkeyRef.current[typingKey] ?? 0,
      latestMessageEvent.created_at,
    );
    typingSuppressUntilByPubkeyRef.current[typingKey] =
      Date.now() + TYPING_POST_MESSAGE_SUPPRESS_MS;
    setTypingByPubkey((current) =>
      typingLiveness.drop(typingLiveness.prune(current), typingKey),
    );
  }, [channelId, latestMessageEvent, relaySelfPubkey]);

  useEffect(() => {
    if (!channelId || channelType === "forum") {
      return;
    }

    let isDisposed = false;
    let cleanup: (() => Promise<void>) | undefined;

    relayClient
      .subscribeToTypingIndicators(channelId, (event) => {
        if (!isDisposed) {
          registerTyping(event);
        }
      })
      .then((dispose) => {
        if (isDisposed) {
          void dispose();
          return;
        }

        cleanup = dispose;
      })
      .catch((error) => {
        console.error(
          "Failed to subscribe to typing indicators",
          channelId,
          error,
        );
      });

    return () => {
      isDisposed = true;
      if (cleanup) {
        void cleanup();
      }
    };
  }, [channelId, channelType]);

  const hasActiveTypers = typingLiveness.size(typingByPubkey) > 0;

  useLivenessSweep(hasActiveTypers, TYPING_PRUNE_INTERVAL_MS, () => {
    setTypingByPubkey((current) => typingLiveness.prune(current));
  });

  return useMemo(() => typingLiveness.list(typingByPubkey), [typingByPubkey]);
}
