/**
 * Parsing the NIP-34 repository coordinate — `30617:<owner-pubkey>:<repo-id>`
 * — that every project-scoped root event carries in its `a` tag.
 *
 * The coordinate *is* the repository's identity. An issue, a pull request, or
 * a status event names the repo it belongs to without anyone needing a project
 * "selected" in the UI, so a flow that has the root event in hand never has to
 * demand a separate selection to know which repo it is acting on.
 *
 * `hooks.ts` builds this string (its private `projectCoordinate`), but until
 * now nothing read one back apart from `repoOwnerFromAddress` in
 * projectIssues.mjs, which keeps the owner and throws the repo id away. Pull
 * requests reached directly — from a desktop notification, or a CLI-authored
 * release-train PR — need both halves to reconstruct their repo context, so
 * the parse lives here once instead of being re-split per caller.
 */

import { KIND_REPO_ANNOUNCEMENT } from "@/shared/constants/kinds";

export type RepoCoordinate = {
  /** Lowercased 64-hex owner pubkey — the repo announcement's author. */
  owner: string;
  /** The announcement's `d` tag. Opaque, and may itself contain `:`. */
  identifier: string;
};

const OWNER_PATTERN = /^[0-9a-f]{64}$/i;

/**
 * Splits a repo coordinate into its owner and repo id, or returns `null` when
 * the string is not a well-formed repo-announcement address.
 *
 * Fails closed on a foreign kind: an `a` tag pointing at some other
 * addressable event would otherwise "resolve" to a repository that does not
 * exist, and the resulting failure would once again name the wrong thing.
 */
export function parseRepoCoordinate(
  coordinate: string | null | undefined,
): RepoCoordinate | null {
  if (!coordinate) return null;

  // Split on the first two separators only. A `d` tag may legally contain a
  // colon, so a greedy `split(":")` would truncate identifiers like
  // `release:train` — the exact CLI-authored shape this parse exists to serve.
  const firstSeparator = coordinate.indexOf(":");
  if (firstSeparator < 0) return null;
  const secondSeparator = coordinate.indexOf(":", firstSeparator + 1);
  if (secondSeparator < 0) return null;

  const kind = coordinate.slice(0, firstSeparator);
  const owner = coordinate.slice(firstSeparator + 1, secondSeparator);
  const identifier = coordinate.slice(secondSeparator + 1);

  if (kind !== String(KIND_REPO_ANNOUNCEMENT)) return null;
  if (!OWNER_PATTERN.test(owner)) return null;
  if (!identifier) return null;

  return { owner: owner.toLowerCase(), identifier };
}

/**
 * The `<owner>:<repo-id>` route-id form — what `Project.id` holds and what
 * `useProjectQuery` resolves — for a repo coordinate, or `null` when the
 * coordinate is malformed.
 *
 * This is the bridge from "a root event names its repo" to "the app can fetch
 * that repo's announcement", without the caller having to know that the route
 * id drops the `30617:` kind prefix the coordinate carries.
 */
export function repoCoordinateToProjectId(
  coordinate: string | null | undefined,
): string | null {
  const parsed = parseRepoCoordinate(coordinate);
  return parsed ? `${parsed.owner}:${parsed.identifier}` : null;
}
