//! §4 extension-data — `publish.extensionData` and `extensionData.get` (kind 30800).
//!
//! A second method, not a branch beside `publish.event`. The host builds the
//! coordinate `d = ext:<extid>:<key>`; the extension supplies only `key`,
//! `content` and `created_at`. `publish.event` continues to refuse kind 30800
//! outright ([`super::publish::Refusal::WrongMethodForKind`]) — that refusal is
//! the proof an extension cannot name another extension's namespace, and this
//! module adds the positive path without touching it.
//!
//! # Why the coordinate is community-global
//!
//! Kind 30800 is NIP-33 parameterized-replaceable and the relay's replacement
//! key is `(community_id, kind, pubkey, d_tag)` — `channel_id` is stored for
//! query scoping but is *not* part of identity. So one extension's data for a
//! key is a single resource per community, per user: writes from different
//! channels intentionally share one head (decision 009). That is specified
//! behaviour, not an accident of storage.

use super::dispatch::{code, BridgeReply};

/// Boolean scope gating both extension-data methods (§7).
pub(crate) const SCOPE_EXTENSION_DATA: &str = "extensionData";

/// The coordinate prefix the host owns. An extension never supplies this.
const COORDINATE_PREFIX: &str = "ext:";

/// §4 caps. The key and extension-id caps already keep the coordinate under the
/// relay bound; [`COORDINATE_MAX_BYTES`] is defence in depth against grammar
/// drift, and equals `buzz_db::event::D_TAG_MAX_LEN` so no key this module
/// calls valid can name a coordinate the relay would refuse to store.
const KEY_MAX_BYTES: usize = 256;
const EXTID_MAX_BYTES: usize = 64;
const COORDINATE_MAX_BYTES: usize = 1024;

/// Does `s` match `[a-z0-9_][a-z0-9_-]*` as a **full string**?
///
/// The §7 extension-id grammar. Deliberately a separate function from
/// [`key_grammar`] rather than one shared regex with a wider character class:
/// the two grammars differ by exactly one character (`.`), and sharing the
/// permissive one would silently admit an extension id the manifest rejects,
/// which is the half of the coordinate that carries the namespace wall.
fn extid_grammar(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

/// Does `s` match `[a-z0-9_][a-z0-9_.-]*` as a **full string**?
///
/// The §4 key grammar — as [`extid_grammar`] plus `.`. Anchored by
/// construction: every character is tested, so there is no unanchored-search
/// failure mode where a valid prefix admits an invalid tail.
fn key_grammar(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-' || c == '.')
}

/// Build and validate the host-owned coordinate for `(extension_id, key)`.
///
/// **One validator, both methods.** The write path and the read path must agree
/// on exactly which coordinates exist; two validators that drift would let a
/// key be writable but unreadable, or vice versa.
///
/// Byte lengths, not character counts — the relay's bound is on bytes, and a
/// multi-byte character would otherwise pass a `chars().count()` check and be
/// refused downstream. The grammars admit ASCII only, so the two agree today;
/// measuring bytes keeps them agreeing if a grammar is ever widened.
pub(crate) fn build_coordinate(extension_id: &str, key: &str) -> Result<String, BridgeReply> {
    let invalid = |message: &str| Err(BridgeReply::err(code::INVALID_PARAMS, message));

    if key.len() > KEY_MAX_BYTES {
        return invalid("key is too long");
    }
    if !key_grammar(key) {
        return invalid("key does not match the permitted grammar");
    }
    // The extension id is host-derived from the lease, so a failure here is a
    // host invariant violation rather than caller error — but it is checked
    // anyway: this is the field that separates one extension's namespace from
    // another's, and an unchecked separator is not a wall.
    if extension_id.len() > EXTID_MAX_BYTES {
        return invalid("extension id is too long");
    }
    if !extid_grammar(extension_id) {
        return invalid("extension id does not match the permitted grammar");
    }

    let coordinate = format!("{COORDINATE_PREFIX}{extension_id}:{key}");
    if coordinate.len() > COORDINATE_MAX_BYTES {
        return invalid("coordinate is too long");
    }
    Ok(coordinate)
}

/// The authority recheck run at the last moment before an extension-data POST.
///
/// **A separate owner from [`super::publish::Revalidation`], deliberately.**
/// That one re-runs `authorise`, which refuses kind 30800 with
/// `WrongMethodForKind` — the wall this increment preserves. Routing 30800
/// through it, or adding a "skip the wrong-method check" switch, would collapse
/// the very separation the two methods exist to enforce. The submission
/// machinery is shared; the authority decision is not.
pub(crate) struct ExtensionDataRevalidation<'a> {
    pub(crate) lease: &'a str,
    pub(crate) extension_id: &'a str,
    pub(crate) key: &'a str,
    pub(crate) identity_at_entry: &'a str,
    pub(crate) coordinate_at_entry: &'a str,
    pub(crate) created_at: i64,
    pub(crate) state: &'a crate::AppState,
    pub(crate) grant_db: Option<std::path::PathBuf>,
}

impl ExtensionDataRevalidation<'_> {
    /// Re-run every authority decision against the exact event being signed.
    pub(crate) fn check(&self) -> Result<(), &'static str> {
        // The lease must still resolve to *this* extension. A reissued lease
        // pointing elsewhere is a different caller.
        match super::frame_host::extension_for_lease(self.lease) {
            Some(current) if current == self.extension_id => {}
            _ => return Err(code::DENIED),
        }

        // The signing identity must still be available and unchanged. Recovery
        // swaps in an ephemeral key, so "available" is not enough alone.
        let now_pubkey =
            super::dispatch::resolve_identity_pubkey(self.state).ok_or(code::DENIED)?;
        if now_pubkey != self.identity_at_entry {
            return Err(code::DENIED);
        }

        // The grant must still be held, read from the store *as it is now*.
        if !grant_lookup(self.grant_db.as_deref(), self.extension_id, &now_pubkey) {
            return Err(code::DENIED);
        }

        // The namespace wall, re-derived rather than trusted. The coordinate
        // about to be signed must still be the one the host builds from live
        // state for this extension and key — not a value carried since entry.
        match build_coordinate(self.extension_id, self.key) {
            Ok(now) if now == self.coordinate_at_entry => {}
            _ => return Err(code::DENIED),
        }

        // The wait is unbounded, so a template inside the window on arrival may
        // not be now. Signing anyway would publish an event the host would
        // refuse if asked again.
        if !super::publish::timestamp_in_window(self.created_at, super::publish::now_unix()) {
            return Err(code::INVALID_PARAMS);
        }
        Ok(())
    }
}

/// Does the store grant this extension the `extensionData` scope?
///
/// Fail closed: a store that cannot be opened has granted nothing.
fn grant_lookup(grant_db: Option<&std::path::Path>, extension_id: &str, pubkey: &str) -> bool {
    grant_db
        .and_then(|path| super::grants::open_grant_db(path).ok())
        .is_some_and(|conn| {
            super::grants::has_scope(&conn, pubkey, extension_id, SCOPE_EXTENSION_DATA)
        })
}

/// The one fixed-filter head read, shared by `current` and `extensionData.get`.
///
/// **A dedicated function, not a switch on the generic query authoriser.** §4's
/// implementation note is explicit that a reusable "skip channel scope" flag
/// "has a habit of becoming doors"; the filter here is constructed fresh from
/// host state every call and cannot be widened by a caller.
///
/// Every returned event is independently re-verified before it is exposed:
/// signature, kind, author and the exact coordinate. The constrained filter is
/// what the relay was asked for; this is what the host is willing to believe.
async fn head_for_coordinate(
    state: &crate::AppState,
    identity_pubkey: &str,
    coordinate: &str,
) -> Result<Option<nostr::Event>, String> {
    let filter = serde_json::json!({
        "kinds":   [buzz_core_pkg::kind::KIND_EXTENSION_DATA],
        "authors": [identity_pubkey],
        "#d":      [coordinate],
        "limit":   1,
    });
    let events = crate::relay::query_relay(state, &[filter]).await?;

    for event in events {
        // Fail closed on anything that does not match exactly. A relay that
        // answers with a different author's row, a different kind, or a
        // different coordinate is not trusted to have honoured the filter.
        if event.verify().is_err() {
            continue;
        }
        if u32::from(event.kind.as_u16()) != buzz_core_pkg::kind::KIND_EXTENSION_DATA {
            continue;
        }
        if event.pubkey.to_hex() != identity_pubkey {
            continue;
        }
        let mut d_values = event.tags.iter().filter_map(|tag| {
            let parts = tag.clone().to_vec();
            (parts.len() >= 2 && parts[0] == "d").then(|| parts[1].clone())
        });
        // Exactly one `d`, equal to the coordinate asked for. A second `d`
        // makes the addressable identity ambiguous, so it is refused rather
        // than resolved by picking one.
        match (d_values.next(), d_values.next()) {
            (Some(only), None) if only == coordinate => return Ok(Some(event)),
            _ => continue,
        }
    }
    Ok(None)
}

/// §4 `publish.extensionData({ key, content, created_at }) → { event, current }`.
///
/// `current` is a **point-in-time observation**, not a lease on the coordinate:
/// it reports whether the submitted event is the head when the read-back runs.
/// Another write can supersede it immediately after. A later
/// [`extension_data_get`] remains the source of truth.
///
/// The read-back exists because the relay's acknowledgement is ambiguous: a
/// dominated write — an exact retry *or* a rejected stale one — both come back
/// `accepted: true, message: "duplicate:"` naming the incoming id. Forwarding
/// that as plain success, which is correct for `publish.event`, would tell a
/// stale caller their value is live when it is not.
pub(crate) async fn publish_extension_data<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    extension_id: &str,
    lease: &str,
    params: Option<serde_json::Value>,
) -> BridgeReply {
    use tauri::Manager as _;

    // ── pure request and coordinate validation ──────────────────────────────
    let Some(serde_json::Value::Object(map)) = params else {
        return BridgeReply::err(code::INVALID_PARAMS, "params must be an object");
    };
    let Some(key) = map.get("key").and_then(serde_json::Value::as_str) else {
        return BridgeReply::err(code::INVALID_PARAMS, "key is required and must be a string");
    };
    let Some(content) = map.get("content").and_then(serde_json::Value::as_str) else {
        return BridgeReply::err(
            code::INVALID_PARAMS,
            "content is required and must be a string",
        );
    };
    // Required at the wire, never defaulted to now: a default would give every
    // retry a different id and publish twice on the first failure.
    let Some(created_at) = map.get("created_at").and_then(serde_json::Value::as_i64) else {
        return BridgeReply::err(
            code::INVALID_PARAMS,
            "created_at is required and must be a unix timestamp",
        );
    };
    let coordinate = match build_coordinate(extension_id, key) {
        Ok(coordinate) => coordinate,
        Err(reply) => return reply,
    };
    // Rejected, never clamped. Moving a caller's timestamp would change the id
    // they retry with, which is the deduplication this depends on.
    if !super::publish::timestamp_in_window(created_at, super::publish::now_unix()) {
        return BridgeReply::err(
            code::INVALID_PARAMS,
            "created_at is outside the acceptable window",
        );
    }

    let state = app.state::<crate::AppState>();

    // ── identity, then grant ────────────────────────────────────────────────
    // §7 grants are keyed by identity, so there is nothing to key a lookup by
    // until the identity is known. Both refusals are `denied`, so the order is
    // unobservable to a caller: recovery stays indistinguishable from ungranted.
    let keys = match super::publish::signing_identity(&state) {
        Ok(keys) => keys,
        Err(_) => return BridgeReply::err(code::DENIED, "missing scope: extensionData"),
    };
    let identity_pubkey = keys.public_key().to_hex();
    let grant_db = super::dispatch::grant_db_path(app).ok();
    if !grant_lookup(grant_db.as_deref(), extension_id, &identity_pubkey) {
        return BridgeReply::err(code::DENIED, "missing scope: extensionData");
    }

    // ── canonical event ─────────────────────────────────────────────────────
    let event = super::publish::CanonicalEvent {
        kind: buzz_core_pkg::kind::KIND_EXTENSION_DATA,
        content: content.to_string(),
        tags: vec![vec!["d".to_string(), coordinate.clone()]],
        created_at,
    };

    let revalidation = ExtensionDataRevalidation {
        lease,
        extension_id,
        key,
        identity_at_entry: &identity_pubkey,
        coordinate_at_entry: &coordinate,
        created_at,
        state: &state,
        grant_db,
    };

    // wait → revalidate → sign → pre-POST id check → send, all inside.
    let submitted = match super::publish::sign_and_publish(&event, &keys, &state, || {
        revalidation.check()
    })
    .await
    {
        Ok(result) => result,
        Err(reply) => return reply,
    };

    // ── fixed-filter head read-back ─────────────────────────────────────────
    let submitted_id = submitted["event"]["id"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let current = match head_for_coordinate(&state, &identity_pubkey, &coordinate).await {
        Ok(head) => head.is_some_and(|head| head.id.to_hex() == submitted_id),
        // Never guess from the ambiguous acknowledgement. The caller can safely
        // retry the exact request; reporting a fabricated `current` cannot be
        // undone by them.
        Err(_) => return BridgeReply::err(code::RELAY_ERROR, "could not confirm the stored value"),
    };

    BridgeReply::ok(serde_json::json!({
        "event": submitted["event"].clone(),
        "current": current,
    }))
}

/// §4 `extensionData.get({ key }) → { event }` — `null` when absent.
pub(crate) async fn extension_data_get<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    extension_id: &str,
    params: Option<serde_json::Value>,
) -> BridgeReply {
    use tauri::Manager as _;

    let Some(serde_json::Value::Object(map)) = params else {
        return BridgeReply::err(code::INVALID_PARAMS, "params must be an object");
    };
    let Some(key) = map.get("key").and_then(serde_json::Value::as_str) else {
        return BridgeReply::err(code::INVALID_PARAMS, "key is required and must be a string");
    };
    let coordinate = match build_coordinate(extension_id, key) {
        Ok(coordinate) => coordinate,
        Err(reply) => return reply,
    };

    let state = app.state::<crate::AppState>();
    let keys = match super::publish::signing_identity(&state) {
        Ok(keys) => keys,
        Err(_) => return BridgeReply::err(code::DENIED, "missing scope: extensionData"),
    };
    let identity_pubkey = keys.public_key().to_hex();
    let grant_db = super::dispatch::grant_db_path(app).ok();
    if !grant_lookup(grant_db.as_deref(), extension_id, &identity_pubkey) {
        return BridgeReply::err(code::DENIED, "missing scope: extensionData");
    }

    match head_for_coordinate(&state, &identity_pubkey, &coordinate).await {
        Ok(Some(event)) => {
            use nostr::JsonUtil as _;
            match serde_json::from_str::<serde_json::Value>(&event.as_json()) {
                Ok(value) => BridgeReply::ok(serde_json::json!({ "event": value })),
                Err(_) => BridgeReply::err(code::INTERNAL, "could not encode the stored value"),
            }
        }
        Ok(None) => BridgeReply::ok(serde_json::json!({ "event": serde_json::Value::Null })),
        Err(_) => BridgeReply::err(code::RELAY_ERROR, "could not read the stored value"),
    }
}

#[cfg(test)]
#[path = "extension_data_tests.rs"]
mod extension_data_tests;
