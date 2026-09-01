//! Device-local extension storage, keyed by the exact live lease authority.
//!
//! Values never enter a relay. The namespace is `(identity, extension id,
//! package digest, grant generation)`, so an identity switch, reinstall or
//! grant replacement cannot silently adopt predecessor state. Disabling or
//! removing UI leaves bytes intact; an explicitly re-authorised package may
//! migrate them only through a separately reviewed future method.

use std::path::Path;

use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::Deserialize;
use serde_json::Value;
use tauri::AppHandle;

use super::dispatch::{code, BridgeReply};
use super::frame_authority::LeaseAuthority;

const MAX_KEY_BYTES: usize = 128;
const MAX_VALUE_BYTES: usize = 256 * 1024;
const MAX_KEYS_PER_NAMESPACE: i64 = 512;
const MAX_NAMESPACE_BYTES: i64 = 4 * 1024 * 1024;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS extension_storage (
    identity_pubkey TEXT NOT NULL,
    extension_id TEXT NOT NULL,
    package_digest TEXT NOT NULL,
    grant_generation INTEGER NOT NULL,
    storage_key TEXT NOT NULL,
    value_json TEXT NOT NULL,
    revision INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (identity_pubkey, extension_id, package_digest, grant_generation, storage_key)
);
";

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct GetParams {
    key: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SetParams {
    key: String,
    value: Value,
    expected_revision: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DeleteParams {
    key: String,
    expected_revision: Option<u64>,
}

fn valid_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= MAX_KEY_BYTES
        && key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn open_storage_db(path: &Path) -> Result<Connection, ()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|_| ())?;
    }
    let conn = Connection::open(path).map_err(|_| ())?;
    conn.pragma_update(None, "journal_mode", "WAL")
        .map_err(|_| ())?;
    conn.pragma_update(None, "busy_timeout", 5000)
        .map_err(|_| ())?;
    conn.execute_batch(SCHEMA).map_err(|_| ())?;
    Ok(conn)
}

fn namespace_params(authority: &LeaseAuthority) -> (&str, &str, &str, i64) {
    (
        &authority.identity_pubkey,
        &authority.extension_id,
        &authority.package_digest,
        i64::try_from(authority.grant_generation).unwrap_or(i64::MAX),
    )
}

fn parse<T: for<'de> Deserialize<'de>>(params: Option<Value>) -> Result<T, BridgeReply> {
    serde_json::from_value(params.unwrap_or(Value::Null))
        .map_err(|_| BridgeReply::err(code::INVALID_PARAMS, "storage parameters are not valid"))
}

fn get(conn: &Connection, authority: &LeaseAuthority, params_value: Option<Value>) -> BridgeReply {
    let params: GetParams = match parse(params_value) {
        Ok(value) => value,
        Err(reply) => return reply,
    };
    if !valid_key(&params.key) {
        return BridgeReply::err(code::INVALID_PARAMS, "storage key is not valid");
    }
    let (identity, extension, digest, generation) = namespace_params(authority);
    let row: Result<Option<(String, i64)>, _> = conn
        .query_row(
            "SELECT value_json, revision FROM extension_storage
             WHERE identity_pubkey = ?1 AND extension_id = ?2 AND package_digest = ?3
               AND grant_generation = ?4 AND storage_key = ?5",
            params![identity, extension, digest, generation, params.key],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional();
    match row {
        Ok(Some((encoded, revision))) => match serde_json::from_str::<Value>(&encoded) {
            Ok(value) => {
                BridgeReply::ok(serde_json::json!({ "value": value, "revision": revision }))
            }
            Err(_) => BridgeReply::err(code::INTERNAL, "extension storage is unavailable"),
        },
        Ok(None) => BridgeReply::ok(serde_json::json!({ "value": null, "revision": null })),
        Err(_) => BridgeReply::err(code::INTERNAL, "extension storage is unavailable"),
    }
}

fn set(
    conn: &mut Connection,
    authority: &LeaseAuthority,
    params_value: Option<Value>,
) -> BridgeReply {
    let params: SetParams = match parse(params_value) {
        Ok(value) => value,
        Err(reply) => return reply,
    };
    if !valid_key(&params.key) {
        return BridgeReply::err(code::INVALID_PARAMS, "storage key is not valid");
    }
    if params
        .expected_revision
        .is_some_and(|value| i64::try_from(value).is_err())
    {
        return BridgeReply::err(code::INVALID_PARAMS, "storage revision is not valid");
    }
    let encoded = match serde_json::to_string(&params.value) {
        Ok(value) if value.len() <= MAX_VALUE_BYTES => value,
        Ok(_) => {
            return BridgeReply::err(
                code::QUOTA_EXCEEDED,
                "storage value exceeds the per-value limit",
            )
        }
        Err(_) => {
            return BridgeReply::err(code::INVALID_PARAMS, "storage value is not JSON-compatible")
        }
    };
    let tx = match conn.transaction_with_behavior(TransactionBehavior::Immediate) {
        Ok(tx) => tx,
        Err(_) => return BridgeReply::err(code::INTERNAL, "extension storage is unavailable"),
    };
    let (identity, extension, digest, generation) = namespace_params(authority);
    let existing: Option<(i64, i64)> = match tx
        .query_row(
            "SELECT revision, length(CAST(value_json AS BLOB)) FROM extension_storage
             WHERE identity_pubkey = ?1 AND extension_id = ?2 AND package_digest = ?3
               AND grant_generation = ?4 AND storage_key = ?5",
            params![identity, extension, digest, generation, params.key],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
    {
        Ok(value) => value,
        Err(_) => return BridgeReply::err(code::INTERNAL, "extension storage is unavailable"),
    };
    let expected = params
        .expected_revision
        .and_then(|value| i64::try_from(value).ok());
    if existing.map(|row| row.0) != expected {
        return BridgeReply::err(
            code::CONFLICT,
            "storage revision changed; reload before writing",
        );
    }
    let (count, bytes): (i64, i64) = match tx.query_row(
        "SELECT count(*), coalesce(sum(length(CAST(value_json AS BLOB))), 0)
         FROM extension_storage WHERE identity_pubkey = ?1 AND extension_id = ?2
           AND package_digest = ?3 AND grant_generation = ?4",
        params![identity, extension, digest, generation],
        |row| Ok((row.get(0)?, row.get(1)?)),
    ) {
        Ok(value) => value,
        Err(_) => return BridgeReply::err(code::INTERNAL, "extension storage is unavailable"),
    };
    let next_count = count + i64::from(existing.is_none());
    let next_bytes = bytes - existing.map_or(0, |row| row.1) + encoded.len() as i64;
    if next_count > MAX_KEYS_PER_NAMESPACE || next_bytes > MAX_NAMESPACE_BYTES {
        return BridgeReply::err(code::QUOTA_EXCEEDED, "extension storage quota exceeded");
    }
    let revision = existing.map_or(1, |row| row.0.saturating_add(1));
    if tx
        .execute(
            "INSERT INTO extension_storage
             (identity_pubkey, extension_id, package_digest, grant_generation, storage_key, value_json, revision, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, unixepoch())
             ON CONFLICT(identity_pubkey, extension_id, package_digest, grant_generation, storage_key)
             DO UPDATE SET value_json = excluded.value_json, revision = excluded.revision, updated_at = excluded.updated_at",
            params![identity, extension, digest, generation, params.key, encoded, revision],
        )
        .is_err()
        || tx.commit().is_err()
    {
        return BridgeReply::err(code::INTERNAL, "extension storage is unavailable");
    }
    BridgeReply::ok(serde_json::json!({ "revision": revision }))
}

fn delete(
    conn: &mut Connection,
    authority: &LeaseAuthority,
    params_value: Option<Value>,
) -> BridgeReply {
    let params: DeleteParams = match parse(params_value) {
        Ok(value) => value,
        Err(reply) => return reply,
    };
    if !valid_key(&params.key) {
        return BridgeReply::err(code::INVALID_PARAMS, "storage key is not valid");
    }
    if params
        .expected_revision
        .is_some_and(|value| i64::try_from(value).is_err())
    {
        return BridgeReply::err(code::INVALID_PARAMS, "storage revision is not valid");
    }
    let tx = match conn.transaction_with_behavior(TransactionBehavior::Immediate) {
        Ok(tx) => tx,
        Err(_) => return BridgeReply::err(code::INTERNAL, "extension storage is unavailable"),
    };
    let (identity, extension, digest, generation) = namespace_params(authority);
    let existing: Option<i64> = match tx
        .query_row(
            "SELECT revision FROM extension_storage WHERE identity_pubkey = ?1 AND extension_id = ?2
             AND package_digest = ?3 AND grant_generation = ?4 AND storage_key = ?5",
            params![identity, extension, digest, generation, params.key],
            |row| row.get(0),
        )
        .optional()
    {
        Ok(value) => value,
        Err(_) => return BridgeReply::err(code::INTERNAL, "extension storage is unavailable"),
    };
    let expected = params
        .expected_revision
        .and_then(|value| i64::try_from(value).ok());
    if existing != expected {
        return BridgeReply::err(
            code::CONFLICT,
            "storage revision changed; reload before deleting",
        );
    }
    let deleted = if existing.is_some() {
        tx.execute(
            "DELETE FROM extension_storage WHERE identity_pubkey = ?1 AND extension_id = ?2
             AND package_digest = ?3 AND grant_generation = ?4 AND storage_key = ?5",
            params![identity, extension, digest, generation, params.key],
        )
        .is_ok_and(|count| count == 1)
    } else {
        false
    };
    if tx.commit().is_err() {
        return BridgeReply::err(code::INTERNAL, "extension storage is unavailable");
    }
    BridgeReply::ok(serde_json::json!({ "deleted": deleted }))
}

pub(crate) fn storage_db_path<R: tauri::Runtime>(
    app: &AppHandle<R>,
) -> Result<std::path::PathBuf, String> {
    Ok(super::extensions_base_dir(app)?
        .join(".storage")
        .join("extension-storage.db"))
}

pub(crate) fn dispatch<R: tauri::Runtime>(
    app: &AppHandle<R>,
    authority: &LeaseAuthority,
    method: &str,
    params_value: Option<Value>,
) -> BridgeReply {
    let grant_db = match super::dispatch::grant_db_path(app)
        .ok()
        .and_then(|path| super::grants::open_grant_db(&path).ok())
    {
        Some(conn) => conn,
        None => return BridgeReply::err(code::DENIED, "missing scope: storage"),
    };
    let selection = super::grants::list_selection(
        &grant_db,
        &authority.identity_pubkey,
        &authority.extension_id,
        &authority.package_digest,
    );
    let generation = super::grants::current_generation(
        &grant_db,
        &authority.identity_pubkey,
        &authority.extension_id,
        &authority.package_digest,
    );
    if !selection.storage || generation != Some(authority.grant_generation) {
        return BridgeReply::err(code::DENIED, "missing scope: storage");
    }
    let path = match storage_db_path(app) {
        Ok(path) => path,
        Err(_) => return BridgeReply::err(code::INTERNAL, "extension storage is unavailable"),
    };
    let mut conn = match open_storage_db(&path) {
        Ok(conn) => conn,
        Err(_) => return BridgeReply::err(code::INTERNAL, "extension storage is unavailable"),
    };
    match method {
        "storage.get" => get(&conn, authority, params_value),
        "storage.set" => set(&mut conn, authority, params_value),
        "storage.delete" => delete(&mut conn, authority, params_value),
        _ => BridgeReply::err(code::UNKNOWN_METHOD, "unknown storage method"),
    }
}

#[cfg(test)]
#[path = "storage_tests.rs"]
mod storage_tests;
