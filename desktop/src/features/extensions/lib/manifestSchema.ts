// Schema for an extension package's `extension.json`.
//
// Decision 006 (BX-01) makes the manifest strict JSON validated on BOTH sides
// of the bridge — but the two sides are split by AUTHORITY, not duplicated:
//
//   this layer  — shape, field types, and unknown-field strictness, so the
//                 install UI can explain a malformed manifest in its own terms.
//   Rust loader — AUTHORITATIVE for the semantic checks: the §4 signable-kind
//                 allowlist and the §5 read denylist floor, which it reads from
//                 `buzz-core`'s own maintained kind sets.
//
// The kind sets are deliberately NOT mirrored here. A second copy in another
// language has nothing binding it to the first, so it drifts silently — and the
// copy that drifts is the one that decides nothing, which is the worst kind of
// wrong. A manifest requesting a non-signable kind therefore parses fine here
// and is rejected by the loader, which is the intended division.
//
// Field layout follows BRIDGE_SPEC.md §7 (authoritative for M1 per Fable).

import { z } from "zod";

/**
 * Extension id grammar: `[a-z0-9_][a-z0-9_-]*`.
 *
 * Reused verbatim from `custom_harnesses.rs`'s `is_valid_harness_id`, where it
 * is deliberately more restrictive than the filesystem to block path-traversal
 * tricks. The id is also the directory name under `<app-data>/extensions/`, so
 * this grammar is load-bearing, not cosmetic.
 */
export const EXTENSION_ID_PATTERN = /^[a-z0-9_][a-z0-9_-]*$/;

/**
 * Longest accepted extension id, in bytes (BRIDGE_SPEC.md §7).
 *
 * Unbounded ids can name a kind-30800 `d`-tag coordinate the relay refuses
 * (`D_TAG_MAX_LEN` = 1024), so an over-long id would install and then fail at
 * publish time. Bytes and UTF-16 length coincide for anything that satisfies
 * the ASCII-only grammar above, so `.max()` is the byte cap the spec means.
 */
export const MAX_EXTENSION_ID_LENGTH = 64;

const ExtensionIdSchema = z
  .string()
  .regex(
    EXTENSION_ID_PATTERN,
    "id must match [a-z0-9_][a-z0-9_-]* (lowercase, no dots, no separators)",
  )
  .max(
    MAX_EXTENSION_ID_LENGTH,
    `id must be at most ${MAX_EXTENSION_ID_LENGTH} bytes`,
  );

// Channels are an explicit, non-empty list of channel UUIDs. BRIDGE_SPEC §7:
// "required, never 'all channels', no sentinels". An empty list is a rejection,
// not an "unset" that some later default fills in.
//
// Canonical lowercase-hyphenated form only, matching the Rust loader's
// `is_canonical_channel_uuid`. `z.uuid()` alone accepts uppercase, which would
// let a manifest pass here and then be rejected by the authoritative validator.
// The case rule is not cosmetic: BRIDGE_SPEC §5 enforces read scope by AND-ing
// `#h` into the filter, and the relay matches tag values as bytes — an
// uppercase UUID would parse fine and then silently match nothing.
const GrantedChannelsSchema = z
  .array(
    z
      .uuid("channels must be channel UUIDs")
      .refine(
        (channel) => channel === channel.toLowerCase(),
        "channel UUIDs must be lowercase",
      ),
  )
  .min(1, "channels must list at least one channel UUID");

// `kind` is shape-checked only. Whether the kind is *signable* is the §4
// allowlist question, and the Rust loader's `EXTENSION_SIGNABLE_KINDS` is the
// single source of truth for it (the bridge's signer enforcement imports that
// same const). See the authority split at the top of this file.
const SignScopeSchema = z.strictObject({
  channels: GrantedChannelsSchema,
  kind: z.number().int().nonnegative(),
});

const ReadScopeSchema = z.strictObject({
  channels: GrantedChannelsSchema,
  kinds: z
    .array(z.number().int().nonnegative())
    .min(1, "kinds must list at least one event kind"),
});

const ScopesSchema = z.strictObject({
  extensionData: z.boolean().optional().default(false),
  identity: z.boolean().optional().default(false),
  read: z.array(ReadScopeSchema).optional().default([]),
  sign: z.array(SignScopeSchema).optional().default([]),
  storage: z.boolean().optional().default(false),
});

// `entry` is resolved inside the installed package. The traversal rules are
// enforced authoritatively in Rust against the real extracted tree (which is
// also where "does this file exist" can be answered); the checks here are the
// same rules applied to the string so the UI can say why before that call.
const EntrySchema = z
  .string()
  .min(1, "entry must not be empty")
  .refine(
    (entry) => !entry.startsWith("/") && !entry.startsWith("\\"),
    "entry must be a relative path",
  )
  .refine((entry) => !/^[a-zA-Z]:/.test(entry), "entry must be a relative path")
  .refine(
    (entry) => !entry.split(/[/\\]/).includes(".."),
    "entry must not contain a '..' path component",
  );

export const ExtensionManifestSchema = z.strictObject({
  egress: z.array(z.string().min(1)).optional().default([]),
  entry: EntrySchema,
  id: ExtensionIdSchema,
  name: z.string().min(1, "name must not be empty"),
  scopes: ScopesSchema.optional().default({
    extensionData: false,
    identity: false,
    read: [],
    sign: [],
    storage: false,
  }),
  version: z.string().min(1, "version must not be empty"),
});

export type ExtensionManifest = z.infer<typeof ExtensionManifestSchema>;

/**
 * Parse a manifest, returning either the parsed manifest or the list of
 * human-readable problems with it. Callers render the problems verbatim.
 */
export function parseExtensionManifest(
  input: unknown,
): { ok: true; manifest: ExtensionManifest } | { ok: false; errors: string[] } {
  const result = ExtensionManifestSchema.safeParse(input);
  if (result.success) {
    return { manifest: result.data, ok: true };
  }
  return {
    errors: result.error.issues.map((issue) => {
      const path = issue.path.join(".");
      return path ? `${path}: ${issue.message}` : issue.message;
    }),
    ok: false,
  };
}
