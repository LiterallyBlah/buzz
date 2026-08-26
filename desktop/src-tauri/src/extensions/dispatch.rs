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
use tauri::{AppHandle, Manager};

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
    pub(crate) const INTERNAL: &str = "internal";
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
    /// Refuse, with this §8 code and message.
    Refuse { code: &'static str, message: String },
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
pub(crate) fn identity_get_public_key(
    pubkey: &str,
    granted: bool,
    extension_id: &str,
) -> BridgeReply {
    let _ = extension_id;
    if pubkey.is_empty() {
        return BridgeReply::err(code::IDENTITY_UNAVAILABLE, "no identity is loaded");
    }
    if !granted {
        // §8: name the missing scope so the client can prompt, without
        // revealing anything the extension was not granted.
        return BridgeReply::err(code::DENIED, "missing scope: identity");
    }
    BridgeReply::ok(serde_json::json!({ "pubkey": pubkey }))
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
pub(crate) fn dispatch<R: tauri::Runtime>(
    app: &AppHandle<R>,
    lease: &str,
    version: u32,
    method: &str,
) -> BridgeReply {
    match route(
        super::frame_host::extension_for_lease,
        lease,
        version,
        method,
    ) {
        Route::Refuse { code, message } => BridgeReply::err(code, message),
        Route::IdentityGetPublicKey { extension_id } => {
            let state = app.state::<crate::AppState>();
            let pubkey = match state.keys.lock() {
                Ok(keys) => keys.public_key().to_hex(),
                Err(_) => {
                    return BridgeReply::err(code::INTERNAL, "identity is not readable");
                }
            };

            // Fail closed at every step: a path we cannot resolve or a store we
            // cannot open has granted nothing.
            let granted = grant_db_path(app)
                .ok()
                .and_then(|path| grants::open_grant_db(&path).ok())
                .map(|conn| {
                    grants::has_scope(&conn, &pubkey, &extension_id, grants::SCOPE_IDENTITY)
                })
                .unwrap_or(false);

            identity_get_public_key(&pubkey, granted, &extension_id)
        }
    }
}

#[cfg(test)]
#[path = "dispatch_tests.rs"]
mod dispatch_tests;
