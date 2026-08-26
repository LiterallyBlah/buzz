//! `extension.json` — the strict manifest every extension package declares.
//!
//! Format and rules come from the buzz-extensions project:
//!
//! - decision 006 (`decisions/006-manifest-format.md`) — strict JSON, unknown
//!   fields rejected, id grammar `[a-z0-9_][a-z0-9_-]*` reusing the
//!   traversal-blocking rule from `managed_agents::custom_harnesses`.
//! - `docs/BRIDGE_SPEC.md` §7 — the concrete field layout implemented here.
//! - `docs/BRIDGE_SPEC.md` §4 — the v1 signable-kind allowlist.
//! - `docs/BRIDGE_SPEC.md` §5 — the read denylist floor, whose complement is
//!   the "read-allowed set" §7 refers to; v1 has no enumerated read allowlist.
//! - decision 004 — egress is default-deny, widened only per declared origin.
//!
//! Everything in this module is pure validation over an already-staged package
//! directory: no network, no install side effects.

use std::path::Path;

use buzz_core_pkg::kind;
use serde::{Deserialize, Serialize};

use super::package_path::check_package_relative_path;

/// File name of the manifest, at the root of the package.
pub(crate) const MANIFEST_FILE_NAME: &str = "extension.json";

/// Longest accepted extension id, in bytes.
///
/// `docs/BRIDGE_SPEC.md` §7: `[a-z0-9_][a-z0-9_-]*, ≤ 64 bytes`. The cap exists
/// because an unbounded id can name a kind-30800 `d`-tag coordinate the relay
/// refuses (`D_TAG_MAX_LEN` = 1024) — so an over-long id would install fine and
/// then fail at publish time. It also keeps a manifest from producing a
/// directory name the filesystem rejects with an opaque errno.
///
/// Bytes and characters coincide here: the grammar admits ASCII only, so
/// `str::len()` is the byte length the spec means.
const MAX_EXTENSION_ID_LEN: usize = 64;

/// The v1 signable-kind allowlist.
///
/// Source of truth: buzz-extensions `docs/BRIDGE_SPEC.md` §4, "v1 signable-kind
/// allowlist". **Grantability is an allowlist, not a blocklist**: a manifest
/// that requests a kind outside this list is rejected at install, not silently
/// dropped, so a kind added to Buzz later is non-grantable by construction
/// until someone edits this list. Growing it is a spec change with a decision
/// record — which is exactly this one line.
///
/// **This const is the single source of truth for what an extension may sign.**
/// Install-time validation here is one consumer; the bridge's signer
/// enforcement (P4) is the other and must import this rather than re-declare
/// the set. The frontend deliberately does not mirror it — zod validates shape
/// and unknown fields, this side owns the semantics, so there is no second copy
/// to drift.
///
/// **Kind 7 (reaction) was removed** (design-repo §4, `d640883`). A reaction is
/// not `h`-scoped: the relay derives its channel from the event its `e` tag
/// points at (`derive_reaction_channel`, `ingest.rs`), so the target *is* the
/// channel selector. An extension granted channel A could therefore react to an
/// event in channel B and have the relay scope it to B — acting outside its
/// grant — and the host cannot tell without resolving the target, which is not
/// locally decidable. Restoring it needs either lookup-free scoping or a
/// reviewed target-resolution policy, not a one-line edit here.
pub(crate) const EXTENSION_SIGNABLE_KINDS: &[u32] = &[
    kind::KIND_STREAM_MESSAGE,      // 9     — channel message
    kind::KIND_EXTENSION_DATA,      // 30800 — extension data (decision 009)
    kind::KIND_STREAM_MESSAGE_EDIT, // 40003 — edit of the user's own event
    kind::KIND_FORUM_POST,          // 45001 — forum post
    kind::KIND_FORUM_VOTE,          // 45002 — forum vote
    kind::KIND_FORUM_COMMENT,       // 45003 — forum comment
];

/// The `41xxx` DM kinds named by `docs/BRIDGE_SPEC.md` §5.
///
/// Expressed as a range rather than an enumeration so a DM kind added to
/// `buzz-core` later is denied from the moment it exists.
const DM_KIND_RANGE: std::ops::RangeInclusive<u32> = 41000..=41999;

/// Extensions an entry document may have.
///
/// The entry is the document the host writes its prologue into. It is **not**
/// the only file that can become a realm — a `srcdoc` child and an SVG asset
/// reached by navigation are realms too, and neither receives the prologue.
///
/// Their coverage differs, and the difference matters:
/// - the **SVG/XML asset** is closed, by the host serving that document family
///   with `script-src 'none'` (see `asset_content_security_policy`);
/// - the **`srcdoc` child** is **not** closed. It is assigned to the isolation
///   phase. There is no script-layer wall covering it — an earlier revision of
///   this comment claimed one, and that mechanism was deliberately reverted.
///
/// This restriction is still worth having: it keeps the *entry* to a shape the
/// host can write into, so the accurate claim is "accepted entry documents and
/// all served HTML receive the prologue", not "every active document does".
///
/// - **HTML only.** An SVG entry is a document too — it is served
///   `image/svg+xml`, can load package script, and would receive no lockdown.
///   Rejecting it at install keeps the entry to a document the host can write a
///   prologue into, rather than relying on the serving layer's MIME table.
/// - **UTF-8 only.** A body that fails `str::from_utf8` used to be served
///   untouched on the assumption it could not execute. That is wrong: a browser
///   replacement-decodes it and a valid prefix runs normally.
const ENTRY_DOCUMENT_EXTENSIONS: &[&str] = &["html", "htm"];

/// URL schemes an `egress` origin may use.
///
/// `http`/`ws` are kept alongside the TLS schemes because a localhost
/// development origin is a legitimate declaration; decision 004 makes the
/// *default* deny, not the scheme.
const EGRESS_SCHEMES: &[&str] = &["http", "https", "ws", "wss"];

/// A `(kind, channels)` signing grant the manifest requests.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SignScope {
    /// The event kind the extension asks to publish. Must be in
    /// [`EXTENSION_SIGNABLE_KINDS`].
    pub kind: u32,
    /// Channel UUIDs the grant is confined to. Required and non-empty — there
    /// is no "all channels" sentinel.
    pub channels: Vec<String>,
}

/// A `(kinds, channels)` read grant the manifest requests.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReadScope {
    /// Event kinds the extension asks to query/subscribe. Must not touch the
    /// read denylist floor.
    pub kinds: Vec<u32>,
    /// Channel UUIDs the grant is confined to. Required and non-empty.
    pub channels: Vec<String>,
}

/// The `scopes` object of a manifest. Every field defaults to "nothing".
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExtensionScopes {
    /// Requests `identity.getPublicKey()`.
    #[serde(default)]
    pub identity: bool,
    /// Requests device-local bridge storage.
    #[serde(default)]
    pub storage: bool,
    /// Requests `publish.extensionData` (kind 30800).
    #[serde(default)]
    pub extension_data: bool,
    /// Requested signing grants.
    #[serde(default)]
    pub sign: Vec<SignScope>,
    /// Requested read grants.
    #[serde(default)]
    pub read: Vec<ReadScope>,
}

/// A parsed `extension.json`.
///
/// Deserialization is strict (`deny_unknown_fields`): a manifest carrying a
/// field this version does not understand is rejected rather than ignored, so a
/// package cannot smuggle a declaration past an older host.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExtensionManifest {
    /// Package id, `[a-z0-9_][a-z0-9_-]*`. Also the install directory name.
    pub id: String,
    /// Human-readable name shown in the UI.
    pub name: String,
    /// Version string. Non-empty; not otherwise interpreted.
    pub version: String,
    /// Package-relative path to the document the extension is hosted from.
    pub entry: String,
    /// Requested scopes; absent means "none of them".
    #[serde(default)]
    pub scopes: ExtensionScopes,
    /// Declared egress origins; absent means the decision-004 default of none.
    #[serde(default)]
    pub egress: Vec<String>,
}

/// Predicate for a valid extension ID.
///
/// IDs must match `[a-z0-9_][a-z0-9_-]*` — lowercase alphanumeric plus
/// hyphens and underscores, starting with an alphanumeric or underscore.
/// This mirrors `managed_agents::custom_harnesses::is_valid_harness_id` and is
/// intentionally more restrictive than the filesystem to prevent path-traversal
/// tricks: the id names a directory under `<app-data>/extensions/`.
pub(crate) fn is_valid_extension_id(id: &str) -> bool {
    if id.is_empty() || id.len() > MAX_EXTENSION_ID_LEN {
        return false;
    }
    let mut chars = id.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

/// The read denylist floor from `docs/BRIDGE_SPEC.md` §5.
///
/// Exactly the spec's enumeration — `AUTHOR_ONLY_KINDS` ∪ `P_GATED_KINDS` ∪
/// `{1059}` ∪ the `41xxx` DM kinds — plus kind 30800. The first two are read
/// from `buzz-core`'s maintained sets rather than copied as numbers, so the
/// floor tracks the kind registry instead of drifting from it.
///
/// **Relay-only kinds are deliberately NOT on this floor.** "Relay-only" means
/// a client may not *author* the kind; it says nothing about reading it. Reads
/// are channel-scoped, so a relay-authored event in a granted channel is
/// channel-public to the user doing the granting — denying it costs real
/// capability (thread summaries, system messages) for no threat-model gain. The
/// sign side still refuses relay-only kinds, which is §4's separate rule.
///
/// The **read-allowed set** §7 refers to is this predicate's complement — v1
/// has no separately enumerated read allowlist (§5). Reach is then bounded
/// twice more at query time, by the user's channel grants and the granted-kind
/// intersection.
///
/// Kind 30800 is included because extension data is never served through
/// `query.events`/`subscribe` (§5); its only read path is `extensionData.get`.
/// Keeping it here means the bridge's query proxy (P4) inherits the rule from
/// the same predicate install-time validation uses, rather than re-deriving it.
pub(crate) fn is_read_denied_kind(kind_value: u32) -> bool {
    kind::AUTHOR_ONLY_KINDS.contains(&kind_value)
        || kind::P_GATED_KINDS.contains(&kind_value)
        || kind_value == kind::KIND_GIFT_WRAP
        || kind_value == kind::KIND_EXTENSION_DATA
        || DM_KIND_RANGE.contains(&kind_value)
}

/// Parse a manifest from raw bytes with unknown fields rejected.
pub(crate) fn parse_manifest(bytes: &[u8]) -> Result<ExtensionManifest, String> {
    serde_json::from_slice::<ExtensionManifest>(bytes)
        .map_err(|error| format!("{MANIFEST_FILE_NAME}: {error}"))
}

/// Validate every manifest rule that does not need the package on disk.
pub(crate) fn validate_manifest(manifest: &ExtensionManifest) -> Result<(), String> {
    if !is_valid_extension_id(&manifest.id) {
        return Err(format!(
            "extension id {:?} is not valid; ids must match [a-z0-9_][a-z0-9_-]* and be at most {MAX_EXTENSION_ID_LEN} bytes",
            manifest.id
        ));
    }
    if manifest.name.trim().is_empty() {
        return Err(format!("{MANIFEST_FILE_NAME}: \"name\" must not be empty"));
    }
    if manifest.version.trim().is_empty() {
        return Err(format!(
            "{MANIFEST_FILE_NAME}: \"version\" must not be empty"
        ));
    }
    validate_entry_path(&manifest.entry)?;

    for scope in &manifest.scopes.sign {
        if !EXTENSION_SIGNABLE_KINDS.contains(&scope.kind) {
            return Err(format!(
                "{MANIFEST_FILE_NAME}: scopes.sign requests kind {}, which extensions may not sign (v1 signable kinds: {})",
                scope.kind,
                format_kind_list(EXTENSION_SIGNABLE_KINDS)
            ));
        }
        validate_channels(&scope.channels, "scopes.sign")?;
    }

    for scope in &manifest.scopes.read {
        if scope.kinds.is_empty() {
            return Err(format!(
                "{MANIFEST_FILE_NAME}: scopes.read must list at least one kind"
            ));
        }
        for kind_value in &scope.kinds {
            // 30800 is not a grantable read kind: its only read path is
            // `extensionData.get`, gated by the `extensionData` boolean scope
            // (BRIDGE_SPEC §5/§7). Say so rather than emitting the generic
            // "may never read", which would send an author looking for the
            // wrong fix.
            if *kind_value == kind::KIND_EXTENSION_DATA {
                return Err(format!(
                    "{MANIFEST_FILE_NAME}: scopes.read requests kind {}; extension data is read through extensionData.get under the \"extensionData\" scope, not a read grant",
                    kind::KIND_EXTENSION_DATA
                ));
            }
            if is_read_denied_kind(*kind_value) {
                return Err(format!(
                    "{MANIFEST_FILE_NAME}: scopes.read requests kind {kind_value}, which extensions may never read"
                ));
            }
        }
        validate_channels(&scope.channels, "scopes.read")?;
    }

    for origin in &manifest.egress {
        validate_egress_origin(origin)?;
    }

    Ok(())
}

/// Validate the manifest's `entry` as a package-relative path.
fn validate_entry_path(entry: &str) -> Result<(), String> {
    if let Err(reason) = check_package_relative_path(entry) {
        return Err(format!(
            "{MANIFEST_FILE_NAME}: \"entry\" must be a relative path inside the package, but is {}: {entry}",
            reason.describe()
        ));
    }
    if entry.ends_with('/') || entry.ends_with('\\') {
        return Err(format!(
            "{MANIFEST_FILE_NAME}: \"entry\" must name a file, not a directory: {entry}"
        ));
    }
    Ok(())
}

/// Confirm the manifest's `entry` resolves to a regular file inside `root`.
///
/// `symlink_metadata` is used deliberately: a symlinked entry is rejected
/// rather than followed, which keeps this consistent with the directory
/// installer's blanket symlink rejection. The resolved path is additionally
/// re-checked for containment under `root` — the belt to the string rules'
/// braces.
pub(crate) fn validate_entry_file(root: &Path, entry: &str) -> Result<(), String> {
    validate_entry_path(entry)?;
    let candidate = root.join(entry);

    let metadata = std::fs::symlink_metadata(&candidate).map_err(|_| {
        format!("{MANIFEST_FILE_NAME}: \"entry\" file is missing from the package: {entry}")
    })?;
    if !metadata.is_file() {
        return Err(format!(
            "{MANIFEST_FILE_NAME}: \"entry\" is not a regular file in the package: {entry}"
        ));
    }

    let real_root = std::fs::canonicalize(root)
        .map_err(|error| format!("could not resolve the package directory: {error}"))?;
    let real_entry = std::fs::canonicalize(&candidate).map_err(|error| {
        format!("{MANIFEST_FILE_NAME}: could not resolve \"entry\" {entry}: {error}")
    })?;
    if !real_entry.starts_with(&real_root) {
        return Err(format!(
            "{MANIFEST_FILE_NAME}: \"entry\" escapes the package directory: {entry}"
        ));
    }
    validate_entry_document(&real_entry, entry)?;
    Ok(())
}

/// The entry must be an HTML document the host can lock down, and it must
/// decode as UTF-8 so the prologue can be prepended to real text.
///
/// Fail-closed on both counts: a package the host cannot protect is not
/// installed, rather than installed and served unprotected.
fn validate_entry_document(path: &Path, entry: &str) -> Result<(), String> {
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if !ENTRY_DOCUMENT_EXTENSIONS.contains(&extension.as_str()) {
        return Err(format!(
            "{MANIFEST_FILE_NAME}: \"entry\" must be an HTML document ({}), got: {entry}",
            ENTRY_DOCUMENT_EXTENSIONS.join(", ")
        ));
    }
    let bytes = std::fs::read(path)
        .map_err(|error| format!("{MANIFEST_FILE_NAME}: could not read \"entry\": {error}"))?;
    if std::str::from_utf8(&bytes).is_err() {
        return Err(format!(
            "{MANIFEST_FILE_NAME}: \"entry\" is not valid UTF-8 text: {entry}"
        ));
    }
    Ok(())
}

/// Load `extension.json` from a package root and apply every rule.
/// The name of the single top-level directory that itself holds the manifest,
/// if the package is shaped that way.
///
/// This is detection only — it never changes what installs. v1 does not
/// auto-unwrap (decision 008: an inference step in a security-sensitive install
/// path, and with re-install-as-update a silent unwrap would quietly redefine
/// what "the package root" means). But "zip the folder" is the obvious mistake
/// to make, and an error that only says "no extension.json at its root" sends
/// the author looking in the wrong place.
fn single_wrapper_directory(root: &Path) -> Option<String> {
    let mut directories = Vec::new();
    for entry in std::fs::read_dir(root).ok()? {
        let entry = entry.ok()?;
        // Any file at the top level means this is not a plain wrapper.
        if entry.path().is_dir() {
            directories.push(entry);
        } else {
            return None;
        }
    }
    let [only] = directories.as_slice() else {
        return None;
    };
    if !only.path().join(MANIFEST_FILE_NAME).is_file() {
        return None;
    }
    only.file_name().into_string().ok()
}

pub(crate) fn load_and_validate_manifest(root: &Path) -> Result<ExtensionManifest, String> {
    let manifest_path = root.join(MANIFEST_FILE_NAME);
    if !manifest_path.is_file() {
        if let Some(wrapper) = single_wrapper_directory(root) {
            return Err(format!(
                "{MANIFEST_FILE_NAME}: the package has no {MANIFEST_FILE_NAME} at its root — it is wrapped in \"{wrapper}/\". Package the folder's *contents*, not the folder."
            ));
        }
        return Err(format!(
            "{MANIFEST_FILE_NAME}: the package has no {MANIFEST_FILE_NAME} at its root"
        ));
    }
    let bytes = std::fs::read(&manifest_path)
        .map_err(|error| format!("{MANIFEST_FILE_NAME}: could not be read: {error}"))?;
    let manifest = parse_manifest(&bytes)?;
    validate_manifest(&manifest)?;
    validate_entry_file(root, &manifest.entry)?;
    Ok(manifest)
}

/// Every scope carries an explicit, non-empty list of channel UUIDs.
///
/// `docs/BRIDGE_SPEC.md` §7: "an explicit list of channel UUIDs — required,
/// never 'all channels', no sentinels". The canonical lowercase hyphenated form
/// is required because §5 enforces scope by rewriting a filter's `#h` tag,
/// which the relay matches as a byte-for-byte string: a UUID in any other
/// casing or form would parse here and then silently match nothing.
fn validate_channels(channels: &[String], field: &str) -> Result<(), String> {
    if channels.is_empty() {
        return Err(format!(
            "{MANIFEST_FILE_NAME}: {field} must list at least one channel; there is no \"all channels\" value"
        ));
    }
    for channel in channels {
        if !is_canonical_channel_uuid(channel) {
            return Err(format!(
                "{MANIFEST_FILE_NAME}: {field} channel {channel:?} is not a channel UUID"
            ));
        }
    }
    Ok(())
}

/// A channel id in the lowercase hyphenated form Buzz publishes.
fn is_canonical_channel_uuid(value: &str) -> bool {
    match uuid::Uuid::parse_str(value) {
        Ok(parsed) => value == parsed.hyphenated().to_string(),
        Err(_) => false,
    }
}

/// An `egress` entry is a bare origin: scheme + host (+ optional port).
///
/// Anything carrying a path, query, fragment or credentials is rejected —
/// decision 004 grants an *origin*, and a path-bearing string would give the
/// grant UI something narrower to show than what the CSP actually widens.
fn validate_egress_origin(origin: &str) -> Result<(), String> {
    let reject = || {
        Err(format!(
            "{MANIFEST_FILE_NAME}: egress entry {origin:?} is not a bare origin (scheme://host[:port], no path, query or fragment)"
        ))
    };

    let Ok(parsed) = url::Url::parse(origin) else {
        return reject();
    };
    if parsed.cannot_be_a_base() {
        return reject();
    }
    if !EGRESS_SCHEMES.contains(&parsed.scheme()) {
        return Err(format!(
            "{MANIFEST_FILE_NAME}: egress entry {origin:?} uses scheme {:?}; allowed schemes are {}",
            parsed.scheme(),
            EGRESS_SCHEMES.join(", ")
        ));
    }
    if parsed.host_str().is_none_or(str::is_empty) {
        return reject();
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return reject();
    }
    if !matches!(parsed.path(), "" | "/") {
        return reject();
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        return reject();
    }
    Ok(())
}

/// Render a kind list for an error message, smallest first.
fn format_kind_list(kinds: &[u32]) -> String {
    kinds
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[path = "manifest_tests.rs"]
mod manifest_tests;
