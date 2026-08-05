import { normalizePubkey } from "@/shared/lib/pubkey";

/**
 * Verified NIP-OA owner index: agent pubkey → owner pubkey, both normalised.
 *
 * The ONLY ownership input is `ownerPubkey` on a users-batch profile summary.
 * The Tauri layer populates that field solely from a kind:0 `auth` tag whose
 * NIP-OA signature verified (`profile_valid_oa_owner_pubkey` →
 * `nip_oa::verify_auth_tag`), which is the same evidence the relay's
 * `is_agent_owner` gate uses. kind:10100 directory content, agent names and
 * the local managed-agents file are NOT ownership evidence — an unowned or
 * foreign-owned agent can publish any name or capability it likes, so nothing
 * here may fall back to them.
 */
export function buildVerifiedAgentOwnerIndex(
  profiles:
    | Readonly<Record<string, { ownerPubkey?: string | null }>>
    | undefined,
): ReadonlyMap<string, string> {
  const ownerByPubkey = new Map<string, string>();
  for (const [pubkey, summary] of Object.entries(profiles ?? {})) {
    if (summary.ownerPubkey) {
      // Key and value both normalised so lookups and ownership comparisons
      // never depend on the casing the relay happened to send.
      ownerByPubkey.set(
        normalizePubkey(pubkey),
        normalizePubkey(summary.ownerPubkey),
      );
    }
  }
  return ownerByPubkey;
}

/**
 * The single producer of "relay agents this identity owns but does not manage
 * on this desktop".
 *
 * Two surfaces consume it — the Agents tab listing and owner-global observer
 * ingestion — and they must agree exactly: an agent listed as owned whose
 * observer frames are not ingested (or the reverse) is a lie in one of the two
 * places. Derive both from this function rather than re-implementing the
 * predicate beside either caller.
 *
 * Locally managed agents are excluded, not merely deduplicated: they already
 * have a richer local card, and the caller's `managedAgents` list is the
 * authority for those. Structurally typed on `{ pubkey }` so callers can pass
 * `RelayAgent[]` or bare pubkey records.
 */
export function selectOwnedRelayAgents<T extends { pubkey: string }>(
  relayAgents: readonly T[] | undefined,
  managedAgents: readonly { pubkey: string }[] | undefined,
  ownerByPubkey: ReadonlyMap<string, string>,
  currentPubkey: string | null | undefined,
): T[] {
  // Before identity resolves there is nobody to compare an owner against, and
  // an unresolved identity must never read as "owned by me".
  if (!currentPubkey) {
    return [];
  }

  const me = normalizePubkey(currentPubkey);
  const managed = new Set(
    (managedAgents ?? []).map((agent) => normalizePubkey(agent.pubkey)),
  );
  const seen = new Set<string>();
  const owned: T[] = [];

  for (const agent of relayAgents ?? []) {
    const key = normalizePubkey(agent.pubkey);
    if (managed.has(key) || seen.has(key)) {
      continue;
    }
    const owner = ownerByPubkey.get(key);
    if (!owner || normalizePubkey(owner) !== me) {
      continue;
    }
    seen.add(key);
    owned.push(agent);
  }

  return owned;
}
