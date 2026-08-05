import type { UserProfileLookup } from "@/features/profile/lib/identity";
import { normalizePubkey, truncatePubkey } from "@/shared/lib/pubkey";

/** Profile for a pubkey from a batch lookup, or null when it is not loaded. */
export function profileForPubkey(pubkey: string, profiles?: UserProfileLookup) {
  return profiles?.[normalizePubkey(pubkey)] ?? null;
}

/**
 * How the pull-request surfaces name a participant.
 *
 * Shared by the panel and the review timeline beside it so the same author
 * reads the same way in a row, a header, and a timeline entry — two copies of
 * this fallback chain is how one surface starts showing a truncated pubkey for
 * someone the next one names.
 */
export function labelForPubkey(pubkey: string, profiles?: UserProfileLookup) {
  const profile = profileForPubkey(pubkey, profiles);
  return (
    profile?.displayName?.trim() ||
    profile?.nip05Handle?.trim() ||
    truncatePubkey(pubkey)
  );
}
