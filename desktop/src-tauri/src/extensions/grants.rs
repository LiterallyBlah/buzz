//! The host-side permission store: what the user actually granted an
//! extension.
//!
//! # Fail-closed is the whole contract
//!
//! Every lookup answers "is this granted?" and answers **no** for anything it
//! has not been told about — a missing row, a missing table, an unreadable
//! database. There is deliberately no path that returns "allowed" by default,
//! because the failure mode of the opposite choice is silent over-permission.
//!
//! # Why the schema carries kind and channel already
//!
//! Only the boolean `identity` scope is checked in this increment, but
//! BRIDGE_SPEC §7 grants `sign`/`read` as `(kind(s), channels)` — an explicit
//! list of channel UUIDs, never "all channels", no sentinels. Modelling that
//! now means the later increments add rows, not a migration.
//!
//! Boolean scopes store `kind = -1` and `channel = ''`. Those are *not*
//! wildcards: [`has_scope`] matches them literally, and the scoped lookups the
//! later increments will add must ask for a concrete kind and channel. A
//! sentinel that reads as "any" is exactly the shape §7 forbids.
//!
//! Grants are per **identity**, because the same installed extension under a
//! different Buzz identity has not been granted anything by that user.

use std::path::Path;

use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

/// Boolean scope gating `identity.getPublicKey` (§3).
pub(crate) const SCOPE_IDENTITY: &str = "identity";

/// Stored for a scope that is not kind-qualified. Not a wildcard.
const NO_KIND: i64 = -1;
/// Stored for a scope that is not channel-qualified. Not a wildcard.
const NO_CHANNEL: &str = "";

pub(crate) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS extension_grants (
    identity_pubkey TEXT    NOT NULL,
    extension_id    TEXT    NOT NULL,
    scope           TEXT    NOT NULL,
    kind            INTEGER NOT NULL DEFAULT -1,
    channel         TEXT    NOT NULL DEFAULT '',
    package_digest  TEXT    NOT NULL DEFAULT '',
    granted_at      INTEGER NOT NULL,
    PRIMARY KEY (identity_pubkey, extension_id, scope, kind, channel)
);
CREATE TABLE IF NOT EXISTS extension_egress_grants (
    identity_pubkey TEXT NOT NULL,
    extension_id TEXT NOT NULL,
    origin TEXT NOT NULL,
    package_digest TEXT NOT NULL,
    granted_at INTEGER NOT NULL,
    PRIMARY KEY (identity_pubkey, extension_id, origin)
);
CREATE TABLE IF NOT EXISTS extension_activation (
    identity_pubkey TEXT NOT NULL,
    extension_id TEXT NOT NULL,
    package_digest TEXT NOT NULL,
    enabled INTEGER NOT NULL DEFAULT 0,
    consented_at INTEGER NOT NULL,
    PRIMARY KEY (identity_pubkey, extension_id)
);
";

/// Open (creating if needed) the grant database.
pub(crate) fn open_grant_db(path: &Path) -> Result<Connection, String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("could not create the grant store directory: {error}"))?;
    }
    let conn = Connection::open(path)
        .map_err(|error| format!("could not open the grant store: {error}"))?;
    conn.pragma_update(None, "journal_mode", "WAL")
        .map_err(|error| format!("could not set journal_mode: {error}"))?;
    conn.pragma_update(None, "busy_timeout", 5000)
        .map_err(|error| format!("could not set busy_timeout: {error}"))?;
    conn.execute_batch(SCHEMA)
        .map_err(|error| format!("could not initialise the grant store: {error}"))?;
    ensure_package_digest_column(&conn)?;
    Ok(conn)
}

fn ensure_package_digest_column(conn: &Connection) -> Result<(), String> {
    let mut statement = conn
        .prepare("PRAGMA table_info(extension_grants)")
        .map_err(|error| format!("could not inspect the grant store: {error}"))?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|error| format!("could not inspect the grant store: {error}"))?;
    let mut has_digest = false;
    for column in columns {
        if matches!(column, Ok(name) if name == "package_digest") {
            has_digest = true;
        }
    }
    if !has_digest {
        conn.execute(
            "ALTER TABLE extension_grants ADD COLUMN package_digest TEXT NOT NULL DEFAULT ''",
            [],
        )
        .map_err(|error| format!("could not migrate the grant store: {error}"))?;
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GrantPair {
    pub kind: u32,
    pub channel: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GrantSelection {
    #[serde(default)]
    pub identity: bool,
    #[serde(default)]
    pub storage: bool,
    #[serde(default)]
    pub extension_data: bool,
    #[serde(default)]
    pub sign: Vec<GrantPair>,
    #[serde(default)]
    pub read: Vec<GrantPair>,
    #[serde(default)]
    pub egress: Vec<String>,
}

/// Record a boolean scope grant (`identity`, `storage`, `extensionData`).
#[allow(dead_code)] // Called by the grants UX; increment 1 ships the store only.
pub(crate) fn grant_boolean_scope(
    conn: &Connection,
    identity_pubkey: &str,
    extension_id: &str,
    scope: &str,
) -> Result<(), String> {
    conn.execute(
        "INSERT OR REPLACE INTO extension_grants
             (identity_pubkey, extension_id, scope, kind, channel, granted_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            identity_pubkey,
            extension_id,
            scope,
            NO_KIND,
            NO_CHANNEL,
            now_unix()
        ],
    )
    .map_err(|error| format!("could not record the grant: {error}"))?;
    Ok(())
}

/// Is this boolean scope granted to this extension, for this identity?
///
/// Returns `false` for anything not explicitly stored, including on a database
/// error — a store we cannot read is not a store that grants anything.
pub(crate) fn has_scope(
    conn: &Connection,
    identity_pubkey: &str,
    extension_id: &str,
    scope: &str,
) -> bool {
    let found: Result<i64, _> = conn.query_row(
        "SELECT COUNT(*) FROM extension_grants
          WHERE identity_pubkey = ?1 AND extension_id = ?2 AND scope = ?3
            AND kind = ?4 AND channel = ?5",
        params![identity_pubkey, extension_id, scope, NO_KIND, NO_CHANNEL],
        |row| row.get(0),
    );
    matches!(found, Ok(count) if count > 0)
}

/// Scope gating `publish.event` (§4). Qualified by `(kind, channel)`.
pub(crate) const SCOPE_SIGN: &str = "sign";

/// Record a `(kind, channel)` sign grant (§7).
///
/// `channel` is one concrete channel id. §7 forbids an "all channels"
/// sentinel, so granting two channels means two rows — there is no value of
/// `channel` that means "any", and [`has_sign_scope`] matches literally.
#[allow(dead_code)] // Called by the grants UX; the store capability lands first.
pub(crate) fn grant_sign_scope(
    conn: &Connection,
    identity_pubkey: &str,
    extension_id: &str,
    kind: u32,
    channel: &str,
) -> Result<(), String> {
    conn.execute(
        "INSERT OR REPLACE INTO extension_grants
             (identity_pubkey, extension_id, scope, kind, channel, granted_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            identity_pubkey,
            extension_id,
            SCOPE_SIGN,
            i64::from(kind),
            channel,
            now_unix()
        ],
    )
    .map_err(|error| format!("could not record the sign grant: {error}"))?;
    Ok(())
}

/// May this extension sign this kind in this channel, for this identity?
///
/// Fail-closed on every arm, including a database error: a store we cannot
/// read has granted nothing. An empty `channel` can never match, because a
/// boolean row stores `channel = ''` and must not be readable as a sign grant.
pub(crate) fn has_sign_scope(
    conn: &Connection,
    identity_pubkey: &str,
    extension_id: &str,
    kind: u32,
    channel: &str,
) -> bool {
    if channel.is_empty() {
        return false;
    }
    let found: Result<i64, _> = conn.query_row(
        "SELECT COUNT(*) FROM extension_grants
          WHERE identity_pubkey = ?1 AND extension_id = ?2 AND scope = ?3
            AND kind = ?4 AND channel = ?5",
        params![
            identity_pubkey,
            extension_id,
            SCOPE_SIGN,
            i64::from(kind),
            channel
        ],
        |row| row.get(0),
    );
    matches!(found, Ok(count) if count > 0)
}

/// Scope gating `query.events` (§5). Qualified by `(kind, channel)`.
pub(crate) const SCOPE_READ: &str = "read";

/// Record a `(kind, channel)` read grant (§7).
///
/// One row per pair, exactly as the sign side. A manifest `read` scope listing
/// three kinds across two channels flattens to six rows — §5 is explicit that
/// there is no entry grouping to reconstruct, so none is stored.
///
/// **Policy is enforced here, before the INSERT.** §5 names grant time as one
/// of the four sites the floor and the allowlist are checked at, alongside
/// manifest validation, rewrite and per event. Validating only at install would
/// leave the store itself able to hold a pair the host would never construct —
/// and the store, not the manifest, is what the read path reads. A row that
/// cannot legally exist should be impossible to write, not merely unlikely.
#[allow(dead_code)] // Called by the grants UX; the store capability lands first.
pub(crate) fn grant_read_scope(
    conn: &Connection,
    identity_pubkey: &str,
    extension_id: &str,
    kind: u32,
    channel: &str,
) -> Result<(), String> {
    if super::manifest::is_read_denied_kind(kind) {
        return Err(format!(
            "kind {kind} is on the read floor and may never be granted"
        ));
    }
    if !super::manifest::is_channel_readable_kind(kind) {
        return Err(format!("kind {kind} is not channel-readable"));
    }
    if !super::manifest::is_canonical_channel_uuid(channel) {
        return Err(format!("{channel:?} is not a channel UUID"));
    }
    conn.execute(
        "INSERT OR REPLACE INTO extension_grants
             (identity_pubkey, extension_id, scope, kind, channel, granted_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            identity_pubkey,
            extension_id,
            SCOPE_READ,
            i64::from(kind),
            channel,
            now_unix()
        ],
    )
    .map_err(|error| format!("could not record the read grant: {error}"))?;
    Ok(())
}

/// Write a grant row **without policy validation**, for tests only.
///
/// The defence-in-depth checks further down the read path — the rewrite-time
/// re-check, the per-event allowlist and the canonical-UUID verifier — exist
/// for rows that should not be there: a row written before a kind left the
/// allowlist, or by a future writer with a bug. Reaching those branches needs
/// exactly such a row.
///
/// This is the honest way to produce one. The alternative is to leave
/// [`grant_read_scope`] permissive so tests can reach the lower checks, which
/// weakens production to make a test convenient — and is how the grant-time
/// site came to be missing in the first place.
#[cfg(test)]
pub(crate) fn insert_unchecked_grant_row_for_test(
    conn: &Connection,
    identity_pubkey: &str,
    extension_id: &str,
    scope: &str,
    kind: i64,
    channel: &str,
) -> Result<(), String> {
    conn.execute(
        "INSERT OR REPLACE INTO extension_grants
             (identity_pubkey, extension_id, scope, kind, channel, granted_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            identity_pubkey,
            extension_id,
            scope,
            kind,
            channel,
            now_unix()
        ],
    )
    .map_err(|error| format!("could not record the row: {error}"))?;
    Ok(())
}

/// Every concrete `(kind, channel)` pair this extension may read.
///
/// This is the **construction** input: §5 builds the emitted relay filters from
/// these pairs, so a pair that is not here cannot appear in a query. Returns an
/// empty vector for any failure — an unreadable store has granted nothing, and
/// zero pairs is `denied` at the call site rather than an unscoped read.
///
/// Sentinel rows are excluded at the SQL level: a boolean grant stores
/// `kind = -1, channel = ''`, and neither is a pair. That is the same
/// refuse-empty-channel discipline [`has_sign_scope`] applies, moved to
/// enumeration so a boolean row can never *become* a pair on the way out.
///
/// Ordered so filter construction is deterministic for a given grant set.
pub(crate) fn list_read_pairs(
    conn: &Connection,
    identity_pubkey: &str,
    extension_id: &str,
) -> Vec<(u32, String)> {
    let mut statement = match conn.prepare(
        "SELECT kind, channel FROM extension_grants
          WHERE identity_pubkey = ?1 AND extension_id = ?2 AND scope = ?3
            AND kind >= 0 AND channel <> ''
          ORDER BY channel ASC, kind ASC",
    ) {
        Ok(statement) => statement,
        Err(_) => return Vec::new(),
    };
    let rows = statement.query_map(params![identity_pubkey, extension_id, SCOPE_READ], |row| {
        let kind: i64 = row.get(0)?;
        let channel: String = row.get(1)?;
        Ok((kind, channel))
    });
    let Ok(rows) = rows else {
        return Vec::new();
    };
    let mut pairs = Vec::new();
    for row in rows {
        // A row we cannot read is skipped rather than defaulted: there is no
        // value of `kind` that should stand in for one the store lost.
        let Ok((kind, channel)) = row else { continue };
        let Ok(kind) = u32::try_from(kind) else {
            continue;
        };
        if channel.is_empty() {
            continue;
        }
        pairs.push((kind, channel));
    }
    pairs
}

/// May this extension read this kind in this channel, for this identity?
///
/// The **per-event admission** check, read live for every returned event, so a
/// revocation between construction and exposure is caught. Fail-closed on every
/// arm including a database error, and an empty `channel` can never match.
pub(crate) fn has_read_scope(
    conn: &Connection,
    identity_pubkey: &str,
    extension_id: &str,
    kind: u32,
    channel: &str,
) -> bool {
    if channel.is_empty() {
        return false;
    }
    let found: Result<i64, _> = conn.query_row(
        "SELECT COUNT(*) FROM extension_grants
          WHERE identity_pubkey = ?1 AND extension_id = ?2 AND scope = ?3
            AND kind = ?4 AND channel = ?5",
        params![
            identity_pubkey,
            extension_id,
            SCOPE_READ,
            i64::from(kind),
            channel
        ],
        |row| row.get(0),
    );
    matches!(found, Ok(count) if count > 0)
}

/// Drop every grant for one extension under one identity.
///
/// Revocation takes effect on the next request (§9): there is no cached copy
/// of a grant anywhere, so removing the row is the whole operation.
#[allow(dead_code)] // Called by the grants UX; increment 1 ships the store only.
pub(crate) fn revoke_all(
    conn: &Connection,
    identity_pubkey: &str,
    extension_id: &str,
) -> Result<usize, String> {
    conn.execute(
        "DELETE FROM extension_grants WHERE identity_pubkey = ?1 AND extension_id = ?2",
        params![identity_pubkey, extension_id],
    )
    .map_err(|error| format!("could not revoke grants: {error}"))
}

#[allow(dead_code)] // Reachable only through `grant_boolean_scope`, above.
fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
}

fn requested_pairs(
    manifest: &super::manifest::ExtensionManifest,
) -> (
    std::collections::BTreeSet<GrantPair>,
    std::collections::BTreeSet<GrantPair>,
) {
    let mut sign = std::collections::BTreeSet::new();
    let mut read = std::collections::BTreeSet::new();
    for scope in &manifest.scopes.sign {
        for channel in &scope.channels {
            sign.insert(GrantPair {
                kind: scope.kind,
                channel: channel.clone(),
            });
        }
    }
    for scope in &manifest.scopes.read {
        for kind in &scope.kinds {
            for channel in &scope.channels {
                read.insert(GrantPair {
                    kind: *kind,
                    channel: channel.clone(),
                });
            }
        }
    }
    (sign, read)
}

pub(crate) fn validate_selection(
    manifest: &super::manifest::ExtensionManifest,
    selected: &GrantSelection,
) -> Result<(), String> {
    if selected.identity && !manifest.scopes.identity {
        return Err("identity was not requested by the prepared manifest".to_string());
    }
    if selected.storage && !manifest.scopes.storage {
        return Err("storage was not requested by the prepared manifest".to_string());
    }
    if selected.extension_data && !manifest.scopes.extension_data {
        return Err("extensionData was not requested by the prepared manifest".to_string());
    }
    let (requested_sign, requested_read) = requested_pairs(manifest);
    for pair in &selected.sign {
        if !requested_sign.contains(pair)
            || !super::manifest::EXTENSION_SIGNABLE_KINDS.contains(&pair.kind)
            || !super::manifest::is_canonical_channel_uuid(&pair.channel)
        {
            return Err("a selected sign grant is not a permitted manifest subset".to_string());
        }
    }
    for pair in &selected.read {
        if !requested_read.contains(pair)
            || super::manifest::is_read_denied_kind(pair.kind)
            || !super::manifest::is_channel_readable_kind(pair.kind)
            || !super::manifest::is_canonical_channel_uuid(&pair.channel)
        {
            return Err("a selected read grant is not a permitted manifest subset".to_string());
        }
    }
    let requested_egress: std::collections::BTreeSet<&str> =
        manifest.egress.iter().map(String::as_str).collect();
    for origin in &selected.egress {
        super::manifest::validate_egress_origin(origin)?;
        if !requested_egress.contains(origin.as_str()) {
            return Err(
                "a selected egress origin was not declared by the prepared manifest".to_string(),
            );
        }
    }
    Ok(())
}

fn insert_selection(
    tx: &rusqlite::Transaction<'_>,
    identity_pubkey: &str,
    extension_id: &str,
    digest: &str,
    selected: &GrantSelection,
) -> Result<(), String> {
    let now = now_unix();
    for (scope, granted) in [
        (SCOPE_IDENTITY, selected.identity),
        ("storage", selected.storage),
        ("extensionData", selected.extension_data),
    ] {
        if granted {
            tx.execute(
                "INSERT INTO extension_grants (identity_pubkey, extension_id, scope, kind, channel, package_digest, granted_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![identity_pubkey, extension_id, scope, NO_KIND, NO_CHANNEL, digest, now],
            )
            .map_err(|error| format!("could not record boolean grant: {error}"))?;
        }
    }
    for (scope, pairs) in [(SCOPE_SIGN, &selected.sign), (SCOPE_READ, &selected.read)] {
        let unique: std::collections::BTreeSet<_> = pairs.iter().collect();
        for pair in unique {
            tx.execute(
                "INSERT INTO extension_grants (identity_pubkey, extension_id, scope, kind, channel, package_digest, granted_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![identity_pubkey, extension_id, scope, i64::from(pair.kind), pair.channel, digest, now],
            )
            .map_err(|error| format!("could not record pair grant: {error}"))?;
        }
    }
    let unique_egress: std::collections::BTreeSet<_> = selected.egress.iter().collect();
    for origin in unique_egress {
        tx.execute(
            "INSERT INTO extension_egress_grants (identity_pubkey, extension_id, origin, package_digest, granted_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![identity_pubkey, extension_id, origin, digest, now],
        )
        .map_err(|error| format!("could not record egress grant: {error}"))?;
    }
    Ok(())
}

pub(crate) fn replace_for_install(
    conn: &mut Connection,
    identity_pubkey: &str,
    manifest: &super::manifest::ExtensionManifest,
    digest: &str,
    selected: &GrantSelection,
) -> Result<(), String> {
    validate_selection(manifest, selected)?;
    let tx = conn
        .transaction()
        .map_err(|error| format!("could not begin grant transaction: {error}"))?;
    tx.execute(
        "DELETE FROM extension_grants WHERE extension_id = ?1",
        params![manifest.id],
    )
    .map_err(|error| format!("could not replace grants: {error}"))?;
    tx.execute(
        "DELETE FROM extension_egress_grants WHERE extension_id = ?1",
        params![manifest.id],
    )
    .map_err(|error| format!("could not replace egress grants: {error}"))?;
    tx.execute(
        "DELETE FROM extension_activation WHERE extension_id = ?1",
        params![manifest.id],
    )
    .map_err(|error| format!("could not replace activation state: {error}"))?;
    insert_selection(&tx, identity_pubkey, &manifest.id, digest, selected)?;
    tx.execute(
        "INSERT INTO extension_activation (identity_pubkey, extension_id, package_digest, enabled, consented_at) VALUES (?1, ?2, ?3, 0, ?4)",
        params![identity_pubkey, manifest.id, digest, now_unix()],
    )
    .map_err(|error| format!("could not record install consent: {error}"))?;
    tx.commit()
        .map_err(|error| format!("could not commit grant transaction: {error}"))
}

pub(crate) fn replace_for_identity(
    conn: &mut Connection,
    identity_pubkey: &str,
    manifest: &super::manifest::ExtensionManifest,
    digest: &str,
    selected: &GrantSelection,
) -> Result<(), String> {
    validate_selection(manifest, selected)?;
    let enabled = is_enabled(conn, identity_pubkey, &manifest.id, digest);
    let tx = conn
        .transaction()
        .map_err(|error| format!("could not begin grant transaction: {error}"))?;
    tx.execute(
        "DELETE FROM extension_grants WHERE identity_pubkey = ?1 AND extension_id = ?2",
        params![identity_pubkey, manifest.id],
    )
    .map_err(|error| format!("could not replace grants: {error}"))?;
    tx.execute(
        "DELETE FROM extension_egress_grants WHERE identity_pubkey = ?1 AND extension_id = ?2",
        params![identity_pubkey, manifest.id],
    )
    .map_err(|error| format!("could not replace egress grants: {error}"))?;
    insert_selection(&tx, identity_pubkey, &manifest.id, digest, selected)?;
    tx.execute(
        "INSERT OR REPLACE INTO extension_activation (identity_pubkey, extension_id, package_digest, enabled, consented_at) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![identity_pubkey, manifest.id, digest, if enabled { 1 } else { 0 }, now_unix()],
    )
    .map_err(|error| format!("could not retain activation state: {error}"))?;
    tx.commit()
        .map_err(|error| format!("could not commit grant transaction: {error}"))
}

#[cfg(test)]
pub(crate) fn has_consent(
    conn: &Connection,
    identity_pubkey: &str,
    extension_id: &str,
    digest: &str,
) -> bool {
    conn.query_row(
        "SELECT COUNT(*) FROM extension_activation WHERE identity_pubkey = ?1 AND extension_id = ?2 AND package_digest = ?3",
        params![identity_pubkey, extension_id, digest],
        |row| row.get::<_, i64>(0),
    )
    .is_ok_and(|count| count == 1)
}

pub(crate) fn is_enabled(
    conn: &Connection,
    identity_pubkey: &str,
    extension_id: &str,
    digest: &str,
) -> bool {
    conn.query_row(
        "SELECT enabled FROM extension_activation WHERE identity_pubkey = ?1 AND extension_id = ?2 AND package_digest = ?3",
        params![identity_pubkey, extension_id, digest],
        |row| row.get::<_, i64>(0),
    )
    .is_ok_and(|enabled| enabled == 1)
}

pub(crate) fn disable_all_for_extension(
    conn: &Connection,
    extension_id: &str,
) -> Result<usize, String> {
    conn.execute(
        "UPDATE extension_activation SET enabled = 0 WHERE extension_id = ?1",
        params![extension_id],
    )
    .map_err(|error| format!("could not fence extension activation: {error}"))
}

pub(crate) fn set_enabled(
    conn: &Connection,
    identity_pubkey: &str,
    extension_id: &str,
    digest: &str,
    enabled: bool,
) -> Result<(), String> {
    let changed = conn
        .execute(
            "UPDATE extension_activation SET enabled = ?4 WHERE identity_pubkey = ?1 AND extension_id = ?2 AND package_digest = ?3",
            params![identity_pubkey, extension_id, digest, if enabled { 1 } else { 0 }],
        )
        .map_err(|error| format!("could not update extension state: {error}"))?;
    if changed != 1 {
        return Err("no consent exists for this identity and installed package".to_string());
    }
    Ok(())
}

pub(crate) fn list_selection(
    conn: &Connection,
    identity_pubkey: &str,
    extension_id: &str,
    digest: &str,
) -> GrantSelection {
    let boolean = |scope: &str| {
        conn.query_row(
            "SELECT COUNT(*) FROM extension_grants WHERE identity_pubkey = ?1 AND extension_id = ?2 AND package_digest = ?3 AND scope = ?4 AND kind = -1 AND channel = ''",
            params![identity_pubkey, extension_id, digest, scope],
            |row| row.get::<_, i64>(0),
        )
        .is_ok_and(|count| count > 0)
    };
    let pairs = |scope: &str| -> Vec<GrantPair> {
        let Ok(mut statement) = conn.prepare(
            "SELECT kind, channel FROM extension_grants WHERE identity_pubkey = ?1 AND extension_id = ?2 AND package_digest = ?3 AND scope = ?4 AND kind >= 0 AND channel <> '' ORDER BY channel, kind",
        ) else {
            return Vec::new();
        };
        let Ok(rows) = statement.query_map(
            params![identity_pubkey, extension_id, digest, scope],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
        ) else {
            return Vec::new();
        };
        rows.filter_map(Result::ok)
            .filter_map(|(kind, channel)| {
                u32::try_from(kind)
                    .ok()
                    .map(|kind| GrantPair { kind, channel })
            })
            .collect()
    };
    let egress = conn
        .prepare("SELECT origin FROM extension_egress_grants WHERE identity_pubkey = ?1 AND extension_id = ?2 AND package_digest = ?3 ORDER BY origin")
        .ok()
        .and_then(|mut statement| {
            statement
                .query_map(params![identity_pubkey, extension_id, digest], |row| row.get::<_, String>(0))
                .ok()
                .map(|rows| rows.filter_map(Result::ok).collect())
        })
        .unwrap_or_default();
    GrantSelection {
        identity: boolean(SCOPE_IDENTITY),
        storage: boolean("storage"),
        extension_data: boolean("extensionData"),
        sign: pairs(SCOPE_SIGN),
        read: pairs(SCOPE_READ),
        egress,
    }
}

pub(crate) fn delete_all_for_extension(
    conn: &mut Connection,
    extension_id: &str,
) -> Result<(), String> {
    let tx = conn
        .transaction()
        .map_err(|error| format!("could not begin removal transaction: {error}"))?;
    tx.execute(
        "DELETE FROM extension_grants WHERE extension_id = ?1",
        params![extension_id],
    )
    .map_err(|error| format!("could not remove grants: {error}"))?;
    tx.execute(
        "DELETE FROM extension_egress_grants WHERE extension_id = ?1",
        params![extension_id],
    )
    .map_err(|error| format!("could not remove egress grants: {error}"))?;
    tx.execute(
        "DELETE FROM extension_activation WHERE extension_id = ?1",
        params![extension_id],
    )
    .map_err(|error| format!("could not remove activation state: {error}"))?;
    tx.commit()
        .map_err(|error| format!("could not commit removal transaction: {error}"))
}

#[cfg(test)]
#[path = "grants_tests.rs"]
mod grants_tests;
