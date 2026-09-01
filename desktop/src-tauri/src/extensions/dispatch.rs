//! The request/response spine every `window.buzz` method rides on.
//!
//! BRIDGE_SPEC §2 defines the wire frames; this is the host end of them. It is
//! deliberately the *shared* path — `publish`, `query` and `subscribe` land as
//! arms of [`route`], not as parallel plumbing.
//!
//! # Attribution comes from the lease, never the payload
//!
//! §2: *"The host attributes every request to the calling extension by the
//! `port1` handle it holds, **never** by any id in the payload. A
//! `params.extensionId` (or similar) MUST be ignored if present."*
//!
//! The host end of the port lives in the frontend, so the frontend passes the
//! opaque host-minted **lease** with each call and Rust resolves it through
//! [`super::frame_host::extension_for_lease`]. **`params` is not a parameter of
//! [`route`] at all** — "ignored" is a property of the signature rather than a
//! check somebody could forget to write.
//!
//! # Enforcement is in Rust
//!
//! The scope check reads the grant store (§7) behind the IPC boundary, not in
//! the frontend. The frontend is our own code, but it is the layer an extension
//! is adjacent to; deciding here means a frontend bug cannot widen a grant.
//!
//! # Shape
//!
//! [`route`] is pure — a lease resolver in, a decision out — so every §2 rule
//! is testable without a Tauri app. [`dispatch`] is the thin wiring that gives
//! it the real resolver and executes the decision.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tauri::AppHandle;
#[cfg(test)]
use tauri::Manager;

use super::grants;

/// The wire version this host speaks (§2).
const SUPPORTED_VERSION: u32 = 1;

/// Longest `method` this host will look at, in bytes.
///
/// §2 methods are `<area>.<name>`; the longest plausible one is a fraction of
/// this. The spec sets no number, so this is ours: far above any legitimate
/// caller, far below anything that makes the host do unbounded work. The
/// bound also makes the `unknown_method` message — which echoes the caller's
/// own method back for debuggability — a bounded string.
const MAX_METHOD_LEN: usize = 64;

/// Longest `lease` this host will look up, in bytes.
///
/// The lease is host-minted, so a longer one cannot be legitimate. Checked
/// anyway: the frontend is our code, but it is the layer an extension is
/// adjacent to, and this module does not assume its caller validated anything.
const MAX_LEASE_LEN: usize = 128;

/// `error.code` values from §8 — only those this increment can produce.
pub(crate) mod code {
    pub(crate) const UNSUPPORTED_VERSION: &str = "unsupported_version";
    pub(crate) const UNKNOWN_METHOD: &str = "unknown_method";
    pub(crate) const INVALID_PARAMS: &str = "invalid_params";
    pub(crate) const DENIED: &str = "denied";
    pub(crate) const IDENTITY_UNAVAILABLE: &str = "identity_unavailable";
    /// The signer could not complete a step that is the host's fault, not the
    /// caller's — an unreadable identity, an event that would not sign.
    pub(crate) const INTERNAL: &str = "internal";
    /// The relay refused the event. Its own text is discarded: it is written
    /// for an operator and can name hosts and internal reasons.
    pub(crate) const RELAY_ERROR: &str = "relay_error";
    /// The request is well-formed but asks for more work than the host allows
    /// — too large a `limit`, too many channels, too big a rewritten query.
    ///
    /// Declared here as of §5's read path, which emits it *before* any network
    /// work. It was previously withheld on the rule below, which it now meets.
    pub(crate) const QUOTA_EXCEEDED: &str = "quota_exceeded";
    // §8 also defines `rate_limited`. That remains frontend-only — it is an
    // admission decision the spine makes before a request reaches Rust — so it
    // is not declared here. A constant nothing emits is a vocabulary entry
    // pretending to be a code path.
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BridgeError {
    pub code: String,
    pub message: String,
}

/// A §2 response minus the `id`: the frontend owns the port, so it correlates.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BridgeReply {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<BridgeError>,
}

impl BridgeReply {
    pub(crate) fn ok(result: Value) -> Self {
        Self {
            ok: true,
            result: Some(result),
            error: None,
        }
    }

    pub(crate) fn err(code: &str, message: impl Into<String>) -> Self {
        Self {
            ok: false,
            result: None,
            error: Some(BridgeError {
                code: code.to_string(),
                message: message.into(),
            }),
        }
    }

    #[cfg(test)]
    pub(crate) fn error_code(&self) -> Option<&str> {
        self.error.as_ref().map(|error| error.code.as_str())
    }
}

/// What a well-formed frame resolves to, before anything is executed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Route {
    /// Run `identity.getPublicKey` for this extension (§3).
    IdentityGetPublicKey { extension_id: String },
    /// Run `publish.event` for this extension (§4).
    ///
    /// Carries only the identity. The template travels separately, so the
    /// value that decides *who the caller is* is still produced by a function
    /// that never sees the payload.
    PublishEvent { extension_id: String },
    /// Run `publish.extensionData` for this extension (§4).
    ///
    /// A distinct route from [`Route::PublishEvent`], not a flag on it: kind
    /// 30800 is refused outright by the generic signer, and the two methods
    /// keep separate authority owners all the way down.
    PublishExtensionData { extension_id: String },
    /// Run `extensionData.get` for this extension (§4).
    ExtensionDataGet { extension_id: String },
    /// §5 `query.events` — a one-shot channel-scoped read.
    QueryEvents { extension_id: String },
    /// §5 `subscribe` — open a live channel-scoped stream.
    Subscribe { extension_id: String },
    /// §5 `unsubscribe` — ensure a subscription is not live on this lease.
    Unsubscribe { extension_id: String },
    /// Refuse, with this §8 code and message.
    Refuse { code: &'static str, message: String },
}

impl Route {
    fn extension_id(&self) -> Option<&str> {
        match self {
            Route::IdentityGetPublicKey { extension_id }
            | Route::PublishEvent { extension_id }
            | Route::PublishExtensionData { extension_id }
            | Route::ExtensionDataGet { extension_id }
            | Route::QueryEvents { extension_id }
            | Route::Subscribe { extension_id }
            | Route::Unsubscribe { extension_id } => Some(extension_id),
            Route::Refuse { .. } => None,
        }
    }
}

/// Decide what a frame means. Pure: no app, no database, no I/O.
///
/// Note the signature — there is no `params`. A caller-supplied
/// `extensionId` cannot influence attribution because it never arrives here.
pub(crate) fn route(
    resolve_lease: impl Fn(&str) -> Option<String>,
    lease: &str,
    version: u32,
    method: &str,
) -> Route {
    // Version first: a frame whose semantics we cannot rely on must not reach
    // a method, even a known one.
    if version != SUPPORTED_VERSION {
        return Route::Refuse {
            code: code::UNSUPPORTED_VERSION,
            message: format!("this host speaks bridge version {SUPPORTED_VERSION}"),
        };
    }

    // Bounds before any lookup or echo. A frame this far outside the wire
    // shape gets a §8 code and no further work — in particular the message
    // does not repeat the oversized input back.
    if method.len() > MAX_METHOD_LEN || lease.len() > MAX_LEASE_LEN {
        return Route::Refuse {
            code: code::INVALID_PARAMS,
            message: "request exceeds the wire limits".to_string(),
        };
    }

    // Identity from the host-minted lease. Unknown or released means there is
    // no live frame to attribute the call to.
    let Some(extension_id) = resolve_lease(lease) else {
        return Route::Refuse {
            code: code::DENIED,
            message: "no live extension frame for this lease".to_string(),
        };
    };

    match method {
        "identity.getPublicKey" => Route::IdentityGetPublicKey { extension_id },
        "publish.event" => Route::PublishEvent { extension_id },
        "publish.extensionData" => Route::PublishExtensionData { extension_id },
        "extensionData.get" => Route::ExtensionDataGet { extension_id },
        "query.events" => Route::QueryEvents { extension_id },
        "subscribe" => Route::Subscribe { extension_id },
        "unsubscribe" => Route::Unsubscribe { extension_id },
        _ => Route::Refuse {
            code: code::UNKNOWN_METHOD,
            message: format!("unknown method: {method}"),
        },
    }
}

/// §3 `identity.getPublicKey() → { pubkey }`, over already-resolved inputs.
///
/// The only value this reads from the identity is `public_key()`. There is no
/// branch that reads, derives or reports secret-key material — §3 is explicit
/// that no method returns the nsec and none will be added.
/// `pubkey` is `None` when no usable identity exists — recovery mode, where
/// `signing_keys()` refuses and `state.keys` holds only an ephemeral boot key.
pub(crate) fn identity_get_public_key(
    pubkey: Option<&str>,
    granted: bool,
    extension_id: &str,
) -> BridgeReply {
    let _ = extension_id;
    // Authority first. Testing identity availability before the grant would
    // make the *choice of error code* an oracle: an ungranted extension would
    // get one code when an identity is loaded and another when it is not, and
    // could poll the difference. Two refusals that are usefully distinct to a
    // granted caller must be indistinguishable to one that was refused.
    if !granted {
        // §8: name the missing scope so the client can prompt, without
        // revealing anything the extension was not granted.
        return BridgeReply::err(code::DENIED, "missing scope: identity");
    }
    // Reachable only once the scope is held, so it discloses nothing to a
    // caller without it. Kept distinct from `denied` because collapsing them
    // would tell a *granted* extension it had been un-granted when the identity
    // is merely unavailable.
    match pubkey {
        Some(pubkey) if !pubkey.is_empty() => {
            BridgeReply::ok(serde_json::json!({ "pubkey": pubkey }))
        }
        _ => BridgeReply::err(code::IDENTITY_UNAVAILABLE, "no identity is loaded"),
    }
}

/// The identity a bridge call may act under, or `None` in recovery.
///
/// Goes through [`crate::AppState::signing_keys`] rather than locking
/// `state.keys`, because the two disagree in exactly the case that matters:
/// `identity_lost` and `keyring_locked` both boot with an **ephemeral key**
/// (`app_state.rs`), so `state.keys` yields a real 64-character pubkey that is
/// not the user's and reads as a healthy identity.
///
/// Extracted so the recovery path is testable against a production-shaped
/// `AppState` — the seam itself, not a re-derivation of it in a test.
pub(crate) fn resolve_identity_pubkey(state: &crate::AppState) -> Option<String> {
    state.signing_keys().ok().map(|k| k.public_key().to_hex())
}

/// Where the grant store lives: beside the installed packages, under a name
/// the id grammar (`[a-z0-9_][a-z0-9_-]*`) cannot produce, so it can never
/// collide with an extension's directory.
pub(crate) fn grant_db_path<R: tauri::Runtime>(
    app: &AppHandle<R>,
) -> Result<std::path::PathBuf, String> {
    Ok(super::extensions_base_dir(app)?
        .join(".grants")
        .join("extension-grants.db"))
}

/// Wire [`route`] to the real lease map, identity and grant store.
///
/// `params` is threaded to the handlers but never to [`route`] — attribution
/// is decided before the payload is looked at, and the signature keeps it that
/// way.
pub(crate) async fn dispatch<R: tauri::Runtime>(
    app: &AppHandle<R>,
    lease: &str,
    version: u32,
    method: &str,
    params: Option<Value>,
) -> BridgeReply {
    let _fence = super::management::lifecycle_read_fence().await;
    let authority = super::frame_host::lease_authority_snapshot(lease);
    let routed = route(
        |candidate| {
            (candidate == lease)
                .then(|| authority.as_ref().map(|owner| owner.extension_id.clone()))
                .flatten()
        },
        lease,
        version,
        method,
    );
    if routed.extension_id().is_some()
        && !authority
            .as_ref()
            .is_some_and(|owner| super::management::lease_authority_current_for_app(app, owner))
    {
        // Exact-owner failure is terminal for this frame. In particular, a
        // stale A frame cannot keep probing after the app switches to B.
        super::frame_host::release(lease);
        return BridgeReply::err(code::DENIED, "extension authority is no longer current");
    }
    match routed {
        Route::Refuse { code, message } => BridgeReply::err(code, message),
        Route::PublishEvent { extension_id } => {
            // The lease travels so the signer can **revalidate authority**
            // before the irreversible step — not as a liveness signal, a term
            // the design explicitly rejected: budget exhaustion closes a port
            // without releasing its lease, so a live lease does not mean a live
            // port. Attribution already came from `route`, which resolved it
            // without seeing `params`.
            super::publish::publish_event(app, &extension_id, lease, params).await
        }
        Route::PublishExtensionData { extension_id } => {
            super::extension_data::publish_extension_data(app, &extension_id, lease, params).await
        }
        Route::ExtensionDataGet { extension_id } => {
            super::extension_data::extension_data_get(app, &extension_id, lease, params).await
        }
        Route::QueryEvents { extension_id } => {
            super::query::query_events(app, &extension_id, lease, params).await
        }
        Route::Subscribe { extension_id } => {
            // Same lease-carrying shape as the other authority-bearing routes:
            // the stream revalidates against it for as long as it lives, not
            // just at the moment it opens.
            super::query::subscribe(app, &extension_id, lease, params).await
        }
        Route::Unsubscribe { extension_id } => {
            let _ = extension_id;
            super::query::unsubscribe(lease, params)
        }
        Route::IdentityGetPublicKey { extension_id } => {
            let Some(owner) = authority.as_ref() else {
                return BridgeReply::err(code::DENIED, "extension authority is unavailable");
            };
            // The pubkey and grant lookup both come from the exact lease owner
            // already proven current above. Neither may be rebound to whatever
            // identity happens to be loaded after this frame was minted.
            let granted = grant_db_path(app)
                .ok()
                .and_then(|path| grants::open_grant_db(&path).ok())
                .is_some_and(|conn| {
                    grants::list_selection(
                        &conn,
                        &owner.identity_pubkey,
                        &owner.extension_id,
                        &owner.package_digest,
                    )
                    .identity
                });
            identity_get_public_key(Some(&owner.identity_pubkey), granted, &extension_id)
        }
    }
}

#[cfg(test)]
#[path = "dispatch_tests.rs"]
mod dispatch_tests;
