// Schema for an extension package's `extension.json`.
//
// Decision 006 (BX-01) makes the manifest strict JSON validated on BOTH sides
// of the bridge: zod here in the install UI, serde in the Rust loader. The
// Rust loader is the security boundary — it is what actually gates the
// install. This layer exists so the UI can explain a bad manifest in the same
// terms before/alongside the Rust call, not to be relied on for enforcement.
//
// Field layout follows BRIDGE_SPEC.md §7. That spec is DRAFT/in review;
// decision 006 names the fields but not their layout, so §7 is the only
// concrete schema available.

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
 * The v1 signable-kind allowlist (BRIDGE_SPEC.md §4).
 *
 * Grantability is an allowlist, not a blocklist: a manifest requesting a kind
 * outside this set is rejected at install rather than silently undefaulted.
 * Growing this list is a spec change with a decision record — keep it as this
 * one const so the change stays one line.
 *
 * 9 channel message · 7 reaction · 45001/45002/45003 forum post/vote/comment ·
 * 40003 edit (own events only) · 30800 extension data.
 */
export const V1_SIGNABLE_KINDS = [
  9, 7, 45001, 45002, 45003, 40003, 30800,
] as const;

const SIGNABLE_KIND_SET = new Set<number>(V1_SIGNABLE_KINDS);

const ExtensionIdSchema = z
  .string()
  .regex(
    EXTENSION_ID_PATTERN,
    "id must match [a-z0-9_][a-z0-9_-]* (lowercase, no dots, no separators)",
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

const SignScopeSchema = z.strictObject({
  channels: GrantedChannelsSchema,
  kind: z
    .number()
    .int()
    .refine((kind) => SIGNABLE_KIND_SET.has(kind), {
      error: (issue) =>
        `kind ${issue.input} is not in the v1 signable-kind allowlist (${V1_SIGNABLE_KINDS.join(", ")})`,
    }),
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
