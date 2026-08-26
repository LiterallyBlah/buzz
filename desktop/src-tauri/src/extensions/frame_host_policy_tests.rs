//! Frame-host tests over the *documents and policies* the host emits.
//!
//! Split from `frame_host_tests.rs` at the 1000-line ceiling. These assert the
//! shape of what is served — the wrapper's relay contract and the CSP strings.
//!
//! Most need no listener. `a_post_install_non_utf8_html_asset_is_refused` does,
//! since refusing an asset is something only the serving path can do, and it
//! takes `lifecycle_guard()` accordingly — as every test that starts the
//! process-global host must.

use std::fs;

use super::frame_host_test_support::{installed, wait_until_closed};
use super::*;

// ── §2 mediator: the wrapper is a conduit, not an endpoint ───────────────────

#[test]
fn the_wrapper_transfers_the_port_through_rather_than_bridging_it() {
    // BRIDGE_SPEC §2: "the wrapper re-posts with the received port in the
    // transfer list, so after the handshake the MessageChannel runs directly
    // between the host's port1 and the extension's port2; the wrapper holds no
    // port. A wrapper that instead kept the port and bridged two channels would
    // be a permanent man-in-the-middle and is non-conformant."
    //
    // A bridging wrapper is indistinguishable from a conforming one by
    // observation from either end — both deliver working messaging. What
    // separates them is that a bridge must *construct its own channel* and
    // *retain* a port. So those are what this asserts against the served
    // document.
    let document = wrapper_document("http://127.0.0.1:51234/ext/demo/index.html");

    // The through-transfer itself: the received ports travel onward in the
    // transfer list of the re-post.
    assert!(
        document.contains(r#"postMessage(event.data, "*", event.ports)"#),
        "the wrapper must re-post the port through; got: {document}"
    );

    // A bridge needs a channel of its own. There must be none.
    assert!(
        !document.contains("MessageChannel"),
        "a wrapper that constructs a channel is bridging, not relaying; got: {document}"
    );

    // And it must not keep a reference. `event.ports` may appear only as the
    // transfer argument above — never on the right-hand side of an assignment
    // and never indexed into.
    assert_eq!(
        document.matches("event.ports").count(),
        1,
        "event.ports may be referenced exactly once, as the transfer list; got: {document}"
    );
    assert!(
        !document.contains("event.ports["),
        "indexing event.ports means taking a reference to a port; got: {document}"
    );
}

#[test]
fn the_wrapper_relays_each_direction_only_from_its_one_expected_source() {
    // §2: up only when the source is its single embedded extension frame, down
    // only when the source is its parent. Mirrored source-identity, because an
    // opaque frame has no usable origin.
    let document = wrapper_document("http://127.0.0.1:51234/ext/demo/index.html");
    assert!(
        document.contains("event.source === frame.contentWindow"),
        "upward relay must be gated on the embedded frame's identity; got: {document}"
    );
    assert!(
        document.contains("event.source === parent"),
        "downward relay must be gated on the parent's identity; got: {document}"
    );
}

#[test]
fn the_wrapper_relays_only_the_two_handshake_envelopes() {
    // §2: narrowing to the handshake keeps "relays the handshake" literal and
    // avoids disclosing unrelated parent messages to the extension.
    let document = wrapper_document("http://127.0.0.1:51234/ext/demo/index.html");
    assert!(
        document.contains(r#"envelope(event.data, "ready")"#),
        "upward relay must be limited to the ready envelope; got: {document}"
    );
    assert!(
        document.contains(r#"envelope(event.data, "port")"#),
        "downward relay must be limited to the port envelope; got: {document}"
    );
}

#[test]
fn the_wrapper_forwards_no_port_upward() {
    // §2: the host originates the channel and MUST NOT adopt a port arriving
    // from the frame side. The wrapper's upward re-post therefore carries no
    // transfer list at all.
    let document = wrapper_document("http://127.0.0.1:51234/ext/demo/index.html");
    assert!(
        document.contains(r#"parent.postMessage(event.data, "*")"#),
        "the upward relay must post without a transfer list; got: {document}"
    );
}

// ── Blocker 2 (round 3): the wrapper's own style must not be blocked ─────────

#[test]
fn the_wrapper_policy_permits_its_own_layout_style() {
    // The wrapper carries inline CSS that makes the extension fill the surface.
    // With `default-src 'none'` and no `style-src`, that CSS was rejected and
    // the extension rendered in a 300x150 default box with a border.
    let policy = wrapper_content_security_policy("http://127.0.0.1:4321");
    assert!(
        policy.contains("style-src 'unsafe-inline'"),
        "got: {policy}"
    );
}

#[test]
fn the_wrapper_relay_carries_only_handshake_envelopes() {
    let document = wrapper_document("http://127.0.0.1:4321/ext/demo/index.html");
    assert!(
        document.contains(r#"envelope(event.data, "ready")"#),
        "{document}"
    );
    assert!(
        document.contains(r#"envelope(event.data, "port")"#),
        "{document}"
    );
    // Ports are forwarded down but never up: the host originates the channel
    // and must not adopt one arriving from the frame side (BRIDGE_SPEC §2).
    let up = document
        .find(r#"parent.postMessage(event.data, "*")"#)
        .expect("up-relay present");
    let up_line_end = document[up..].find('\n').map_or(document.len(), |n| up + n);
    assert!(
        !document[up..up_line_end].contains("event.ports"),
        "the up-relay must not carry ports: {}",
        &document[up..up_line_end]
    );
}

// ── Round 5: `script-src 'none'` on active non-HTML documents (route 2) ──────
//
// Scope: route 2 only. Route 1 (the `srcdoc` child) is open and belongs to the
// isolation phase — nothing below covers it.

#[tokio::test]
async fn a_post_install_non_utf8_html_asset_is_refused() {
    // Hermes should-fix 1: the fail-closed serving branch had no direct test.
    let _guard = lifecycle_guard().await;
    let base = installed(&[("index.html", b"<!doctype html>")]);
    let mut broken = b"<!doctype html><script src=\"x.js\"></script>".to_vec();
    broken.push(0xff);
    fs::write(base.path().join("demo").join("broken.html"), &broken).expect("broken");

    let claim = acquire(base.path().to_path_buf(), "demo")
        .await
        .expect("acquire");
    let origin = origin_for_port(claim.extension_port);
    let response = reqwest::get(format!(
        "{origin}/{EXTENSION_ROUTE_PREFIX}/demo/broken.html"
    ))
    .await
    .expect("request");
    assert_eq!(
        response.status(),
        404,
        "a non-UTF-8 HTML body must be refused, not served untouched"
    );

    release(&claim.lease);
    assert!(wait_until_closed(claim.extension_port).await);
}
