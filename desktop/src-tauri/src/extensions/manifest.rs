//! `extension.json` — the strict manifest every extension package declares.
//!
//! Format and rules come from the buzz-extensions project:
//!
//! - decision 006 (`decisions/006-manifest-format.md`) — strict JSON, unknown
//!   fields rejected, id grammar `[a-z0-9_][a-z0-9_-]*` reusing the
//!   traversal-blocking rule from `managed_agents::custom_harnesses`.
//! - `docs/BRIDGE_SPEC.md` §7 — the concrete field layout implemented here.
//! - `docs/BRIDGE_SPEC.md` §4 — the v1 signable-kind allowlist.
//! - `docs/BRIDGE_SPEC.md` §5 — the read denylist floor.
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

/// Longest accepted extension id.
///
/// The id is also the directory name under `<app-data>/extensions/`, so this
/// keeps a manifest from producing a name the filesystem rejects with an
/// opaque errno instead of a readable message.
const MAX_EXTENSION_ID_LEN: usize = 64;

/// Reserved extension-data kind.
///
/// buzz-extensions decision 009 / `docs/BRIDGE_SPEC.md` §4 assign kind 30800 to
/// extension-owned addressable state. It is not (yet) in `buzz-core`'s kind
/// registry — the relay accept-and-store change is a separate M1 work item — so
/// it is named here and will move to `buzz-core` when that lands.
pub(crate) const KIND_EXTENSION_DATA: u32 = 30800;

/// The v1 signable-kind allowlist.
///
/// Source of truth: buzz-extensions `docs/BRIDGE_SPEC.md` §4, "v1 signable-kind
/// allowlist". **Grantability is an allowlist, not a blocklist**: a manifest
/// that requests a kind outside this list is rejected at install, not silently
/// dropped, so a kind added to Buzz later is non-grantable by construction
/// until someone edits this list. Growing it is a spec change with a decision
/// record — which is exactly this one line.
pub(crate) const EXTENSION_SIGNABLE_KINDS: &[u32] = &[
    kind::KIND_REACTION,            // 7     — reaction
    kind::KIND_STREAM_MESSAGE,      // 9     — channel message
    KIND_EXTENSION_DATA,            // 30800 — extension data (decision 009)
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
/// `AUTHOR_ONLY_KINDS` ∪ `P_GATED_KINDS` ∪ `{1059}` ∪ the `41xxx` DM kinds,
/// plus relay-only kinds. The first two are read from `buzz-core`'s maintained
/// sets rather than copied as numbers, so the floor tracks the kind registry
/// instead of drifting from it.
pub(crate) fn is_read_denied_kind(kind_value: u32) -> bool {
    kind::AUTHOR_ONLY_KINDS.contains(&kind_value)
        || kind::P_GATED_KINDS.contains(&kind_value)
        || kind_value == kind::KIND_GIFT_WRAP
        || DM_KIND_RANGE.contains(&kind_value)
        || kind::is_relay_only_kind(kind_value)
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
            "extension id {:?} is not valid; ids must match [a-z0-9_][a-z0-9_-]* and be at most {MAX_EXTENSION_ID_LEN} characters",
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
    Ok(())
}

/// Load `extension.json` from a package root and apply every rule.
pub(crate) fn load_and_validate_manifest(root: &Path) -> Result<ExtensionManifest, String> {
    let manifest_path = root.join(MANIFEST_FILE_NAME);
    if !manifest_path.is_file() {
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
