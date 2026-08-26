//! Tests for the §2 dispatch spine.
//!
//! [`route`] is pure, so every §2 rule is exercised here without a Tauri app.

use super::*;

/// A lease map standing in for the frame host's.
fn resolver(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
    move |lease: &str| {
        pairs
            .iter()
            .find(|(l, _)| *l == lease)
            .map(|(_, id)| (*id).to_string())
    }
}

const LIVE: &[(&str, &str)] = &[("lease-a", "demo"), ("lease-b", "other")];

#[test]
fn a_known_method_routes_to_its_handler_for_the_leased_extension() {
    let decision = route(resolver(LIVE), "lease-a", 1, "identity.getPublicKey");
    assert_eq!(
        decision,
        Route::IdentityGetPublicKey {
            extension_id: "demo".to_string()
        }
    );
}

#[test]
fn attribution_follows_the_lease_not_the_caller() {
    // Two live extensions. The lease alone decides which one a frame is.
    let a = route(resolver(LIVE), "lease-a", 1, "identity.getPublicKey");
    let b = route(resolver(LIVE), "lease-b", 1, "identity.getPublicKey");
    assert_eq!(
        a,
        Route::IdentityGetPublicKey {
            extension_id: "demo".into()
        }
    );
    assert_eq!(
        b,
        Route::IdentityGetPublicKey {
            extension_id: "other".into()
        }
    );
}

#[test]
fn a_payload_extension_id_cannot_reach_attribution() {
    // §2: `params.extensionId` MUST be ignored.
    //
    // The structural reason it is: `route` has no `params` argument, so there
    // is nothing a caller could populate — "ignored" is enforced by the
    // signature, not by a check somebody could forget. Adding a params
    // argument would break every call below, which is the intended tripwire.
    //
    // Behaviourally: `lease-a` belongs to "demo". A caller claiming to be
    // "other" — which is a real, live extension, so the id is not merely
    // invalid — still gets attributed to "demo", because only the lease is
    // consulted.
    let decision = route(resolver(LIVE), "lease-a", 1, "identity.getPublicKey");
    assert_eq!(
        decision,
        Route::IdentityGetPublicKey {
            extension_id: "demo".to_string()
        },
        "attribution must come from the lease alone"
    );
}

#[test]
fn an_unsupported_version_is_refused_before_the_method_is_looked_at() {
    // Even a valid method and a live lease must not execute under a version
    // whose semantics this host does not implement.
    for version in [0, 2, 99] {
        let decision = route(resolver(LIVE), "lease-a", version, "identity.getPublicKey");
        assert_eq!(
            decision,
            Route::Refuse {
                code: code::UNSUPPORTED_VERSION,
                message: "this host speaks bridge version 1".to_string()
            },
            "version {version} must be refused"
        );
    }
}

#[test]
fn an_unsupported_version_outranks_an_unknown_lease() {
    // Ordering is observable: a bad version on a dead lease reports the
    // version, so a client cannot probe lease validity by sending junk frames.
    let decision = route(resolver(LIVE), "no-such-lease", 7, "identity.getPublicKey");
    match decision {
        Route::Refuse { code, .. } => assert_eq!(code, code::UNSUPPORTED_VERSION),
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[test]
fn an_unknown_method_is_refused() {
    for method in [
        "identity.getSecretKey",
        "publish.event",
        "",
        "identity.getPublicKey ",
    ] {
        let decision = route(resolver(LIVE), "lease-a", 1, method);
        match decision {
            Route::Refuse { code, .. } => assert_eq!(
                code,
                code::UNKNOWN_METHOD,
                "method {method:?} must not resolve"
            ),
            other => panic!("method {method:?} resolved to {other:?}"),
        }
    }
}

#[test]
fn an_unknown_or_released_lease_is_denied() {
    let decision = route(
        resolver(LIVE),
        "lease-that-never-existed",
        1,
        "identity.getPublicKey",
    );
    match decision {
        Route::Refuse { code, .. } => assert_eq!(code, code::DENIED),
        other => panic!("expected denial, got {other:?}"),
    }
}

// ── wire bounds ──────────────────────────────────────────────────────────────

#[test]
fn an_oversized_method_is_refused_without_being_looked_up() {
    let method = "a".repeat(MAX_METHOD_LEN + 1);
    let decision = route(resolver(LIVE), "lease-a", 1, &method);
    match decision {
        Route::Refuse { code, message } => {
            assert_eq!(code, code::INVALID_PARAMS);
            assert!(
                !message.contains(&method),
                "the refusal must not echo the oversized input back: {message}"
            );
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[test]
fn a_method_at_the_limit_is_still_dispatched() {
    // The boundary is a limit, not an off-by-one: a method of exactly
    // MAX_METHOD_LEN is legal and must reach the method table (where it is
    // simply unknown), rather than being refused as oversized.
    let method = "a".repeat(MAX_METHOD_LEN);
    match route(resolver(LIVE), "lease-a", 1, &method) {
        Route::Refuse { code, .. } => assert_eq!(
            code,
            code::UNKNOWN_METHOD,
            "a method at exactly the limit must be looked up, not rejected as oversized"
        ),
        other => panic!("expected unknown_method, got {other:?}"),
    }
}

#[test]
fn an_oversized_lease_is_refused_before_it_is_resolved() {
    // A lease this long cannot be one the host minted. Refusing before the
    // lookup means an absurd input costs nothing to reject.
    let lease = "b".repeat(MAX_LEASE_LEN + 1);
    let decision = route(resolver(LIVE), &lease, 1, "identity.getPublicKey");
    match decision {
        Route::Refuse { code, message } => {
            assert_eq!(code, code::INVALID_PARAMS);
            assert!(!message.contains(&lease), "must not echo the lease back");
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[test]
fn a_version_refusal_still_outranks_a_bounds_refusal() {
    // Ordering stays observable and stable: an unsupported version is reported
    // even when the frame is also oversized, so a client cannot use an
    // oversized field to discover anything about lease or method handling.
    let decision = route(
        resolver(LIVE),
        "lease-a",
        99,
        &"a".repeat(MAX_METHOD_LEN + 1),
    );
    match decision {
        Route::Refuse { code, .. } => assert_eq!(code, code::UNSUPPORTED_VERSION),
        other => panic!("expected a refusal, got {other:?}"),
    }
}

// ── error normalisation (§8) ─────────────────────────────────────────────────

/// Every `error.code` §8 defines. A code outside this set is not a code an
/// extension's client library can be expected to handle.
const SPEC_CODES: &[&str] = &[
    "unsupported_version",
    "unknown_method",
    "invalid_params",
    "denied",
    "quota_exceeded",
    "rate_limited",
    "relay_error",
    "identity_unavailable",
    "internal",
];

/// Substrings that would mean host internals reached the wire.
const INTERNAL_MARKERS: &[&str] = &[
    "rusqlite",
    "sqlite",
    "os error",
    "panicked",
    "/opt/",
    "/home/",
    "unwrap",
    "Error {",
    "extension-grants",
];

#[test]
fn every_refusal_this_module_can_produce_is_normalised() {
    // §8: a stable code from the defined set, and a message that carries no
    // host internals — no rust error text, no filesystem path, no panic
    // string. The grant store's own errors are deliberately discarded rather
    // than wrapped, so there is no path by which one reaches a reply.
    let long = "z".repeat(MAX_METHOD_LEN + 10);
    let mut replies = Vec::new();

    for (lease, version, method) in [
        ("lease-a", 1u32, "identity.getPublicKey"),
        ("lease-a", 99, "identity.getPublicKey"),
        ("lease-a", 1, "publish.event"),
        ("lease-a", 1, ""),
        ("dead-lease", 1, "identity.getPublicKey"),
        ("lease-a", 1, long.as_str()),
        (long.as_str(), 1, "identity.getPublicKey"),
    ] {
        if let Route::Refuse { code, message } = route(resolver(LIVE), lease, version, method) {
            replies.push(BridgeReply::err(code, message));
        }
    }
    replies.push(identity_get_public_key(&"a".repeat(64), false, "demo"));
    replies.push(identity_get_public_key("", true, "demo"));
    replies.push(BridgeReply::err(code::INTERNAL, "identity is not readable"));

    assert!(
        replies.len() >= 8,
        "expected the full refusal matrix, got {}",
        replies.len()
    );

    for reply in replies {
        let error = reply.error.as_ref().expect("a refusal carries an error");
        assert!(
            SPEC_CODES.contains(&error.code.as_str()),
            "code {:?} is not one §8 defines",
            error.code
        );
        assert!(
            !error.message.is_empty(),
            "code {:?} carried an empty message",
            error.code
        );
        let lowered = error.message.to_lowercase();
        for marker in INTERNAL_MARKERS {
            assert!(
                !lowered.contains(&marker.to_lowercase()),
                "message for {:?} leaked {marker:?}: {}",
                error.code,
                error.message
            );
        }
    }
}

// ── identity.getPublicKey (§3) ───────────────────────────────────────────────

#[test]
fn the_pubkey_is_returned_when_the_scope_is_granted() {
    let pubkey = "a".repeat(64);
    let reply = identity_get_public_key(&pubkey, true, "demo");
    assert!(reply.ok);
    assert_eq!(
        reply.result,
        Some(serde_json::json!({ "pubkey": pubkey })),
        "the 64-char hex pubkey and nothing else"
    );
}

#[test]
fn without_the_grant_it_is_denied_fail_closed() {
    let reply = identity_get_public_key(&"a".repeat(64), false, "demo");
    assert!(!reply.ok);
    assert_eq!(reply.error_code(), Some(code::DENIED));
    assert_eq!(
        reply.error.as_ref().map(|e| e.message.as_str()),
        Some("missing scope: identity"),
        "§8: name the missing scope so the client can prompt, and nothing more"
    );
}

#[test]
fn a_denial_carries_no_pubkey() {
    // The denial path must not leak the value it refused to hand over.
    let pubkey = "abcdef0123456789".repeat(4);
    let reply = identity_get_public_key(&pubkey, false, "demo");
    let rendered = serde_json::to_string(&reply).expect("serialise");
    assert!(
        !rendered.contains(&pubkey),
        "a denied reply must not contain the pubkey; got: {rendered}"
    );
}

#[test]
fn no_identity_reports_identity_unavailable_not_denied() {
    // §8 distinguishes "keyring locked / identity lost" from a scope refusal;
    // collapsing them would tell a granted extension it had been un-granted.
    let reply = identity_get_public_key("", true, "demo");
    assert_eq!(reply.error_code(), Some(code::IDENTITY_UNAVAILABLE));
}

#[test]
fn no_reply_on_any_path_mentions_secret_key_material() {
    // §3: key material never crosses the bridge. Belt on the shape of every
    // reply this module can produce.
    let pubkey = "a".repeat(64);
    for reply in [
        identity_get_public_key(&pubkey, true, "demo"),
        identity_get_public_key(&pubkey, false, "demo"),
        identity_get_public_key("", true, "demo"),
    ] {
        let rendered = serde_json::to_string(&reply).expect("serialise");
        for forbidden in ["nsec", "secret", "privkey", "private_key", "seckey"] {
            assert!(
                !rendered.to_lowercase().contains(forbidden),
                "reply mentioned {forbidden:?}: {rendered}"
            );
        }
    }
}
