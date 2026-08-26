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
