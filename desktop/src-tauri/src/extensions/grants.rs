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
    granted_at      INTEGER NOT NULL,
    PRIMARY KEY (identity_pubkey, extension_id, scope, kind, channel)
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
    Ok(conn)
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

#[cfg(test)]
#[path = "grants_tests.rs"]
mod grants_tests;
