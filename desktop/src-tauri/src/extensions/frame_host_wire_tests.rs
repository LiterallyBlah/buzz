//! Frame-host tests that drive a **live listener**.
//!
//! Split from `frame_host_tests.rs` at the 1000-line ceiling. Everything here
//! takes `frame_host::lifecycle_guard()` because it touches the process-wide
//! host; see that function for why the guard must be shared across modules.

use std::fs;

use super::frame_host_test_support::{installed, is_listening, wait_until_closed};
use super::*;

// ── Lifecycle ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn the_host_starts_serves_and_leaves_no_listener_behind() {
    let _guard = lifecycle_guard().await;
    let base = installed(&[("index.html", b"<!doctype html><title>demo</title>")]);

    let claim = acquire(base.path().to_path_buf(), "demo")
        .await
        .expect("host should start");
    let port = claim.extension_port;
    assert!(is_listening(port).await, "nothing is listening on {port}");
    assert_eq!(running_port(), Some(port));

    let body = reqwest::get(format!(
        "{}/{EXTENSION_ROUTE_PREFIX}/{}/{}/demo/index.html",
        origin_for_port(port),
        claim.static_context,
        claim.package_digest
    ))
    .await
    .expect("request");
    assert_eq!(body.status(), 200);
    assert_eq!(
        body.headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
        Some("text/html; charset=utf-8")
    );
    let policy = body
        .headers()
        .get(header::CONTENT_SECURITY_POLICY)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(policy.contains("connect-src 'none'"), "got: {policy}");
    assert_eq!(
        body.headers()
            .get(header::X_CONTENT_TYPE_OPTIONS)
            .and_then(|value| value.to_str().ok()),
        Some("nosniff")
    );
    assert!(body.text().await.expect("body").contains("demo"));

    release(&claim.lease);
    assert!(
        wait_until_closed(port).await,
        "the listener outlived its last frame on port {port}"
    );
    assert_eq!(running_port(), None);
}

#[tokio::test]
async fn a_second_frame_keeps_the_host_up_until_both_release() {
    let _guard = lifecycle_guard().await;
    let base = installed(&[("index.html", b"x")]);

    let first = acquire(base.path().to_path_buf(), "demo")
        .await
        .expect("first");
    let second = acquire(base.path().to_path_buf(), "demo")
        .await
        .expect("second");
    assert_eq!(
        first.extension_port, second.extension_port,
        "a second frame must reuse the one listener"
    );
    assert_ne!(
        first.lease, second.lease,
        "each frame must get its own lease, or one can release the other"
    );

    release(&first.lease);
    assert!(
        is_listening(first.extension_port).await,
        "the host stopped while a frame was still open"
    );

    release(&second.lease);
    assert!(
        wait_until_closed(first.extension_port).await,
        "the last release must stop it"
    );
}

#[tokio::test]
async fn shutdown_stops_the_host_even_with_holders_outstanding() {
    // A crashed or reloaded webview never releases. App exit must still not
    // leave a listener behind.
    let _guard = lifecycle_guard().await;
    let base = installed(&[("index.html", b"x")]);

    let claim = acquire(base.path().to_path_buf(), "demo")
        .await
        .expect("acquire");
    let port = claim.extension_port;
    let second = acquire(base.path().to_path_buf(), "demo")
        .await
        .expect("acquire");
    assert!(is_listening(port).await);

    shutdown_now();
    assert!(
        wait_until_closed(port).await,
        "shutdown left a listener on {port}"
    );
    assert_eq!(running_port(), None);

    // Releasing after shutdown must not underflow or resurrect anything.
    release(&claim.lease);
    release(&second.lease);
    release("never-issued");
    assert_eq!(running_port(), None);
}

#[tokio::test]
async fn a_restarted_host_serves_again_on_a_fresh_port() {
    let _guard = lifecycle_guard().await;
    let base = installed(&[("index.html", b"x")]);

    let first = acquire(base.path().to_path_buf(), "demo")
        .await
        .expect("first");
    release(&first.lease);
    assert!(wait_until_closed(first.extension_port).await);

    let second = acquire(base.path().to_path_buf(), "demo")
        .await
        .expect("second");
    assert!(
        is_listening(second.extension_port).await,
        "the host did not come back"
    );
    release(&second.lease);
    assert!(wait_until_closed(second.extension_port).await);
}

#[tokio::test]
async fn traversal_is_refused_over_the_wire_too() {
    let _guard = lifecycle_guard().await;
    let base = installed(&[("index.html", b"x")]);
    fs::write(base.path().join("secret.txt"), b"top secret").expect("secret");

    let claim = acquire(base.path().to_path_buf(), "demo")
        .await
        .expect("acquire");
    let port = claim.extension_port;
    let origin = origin_for_port(port);

    // Sent raw so the client cannot normalise the traversal away before it
    // reaches the server — the point is what the server does with it.
    for attempt in [
        format!(
            "/ext/{}/{}/demo/../secret.txt",
            claim.static_context, claim.package_digest
        ),
        format!(
            "/ext/{}/{}/../secret.txt",
            claim.static_context, claim.package_digest
        ),
        format!(
            "/ext/{}/{}/demo/..%2fsecret.txt",
            claim.static_context, claim.package_digest
        ),
        format!(
            "/ext/{}/{}/demo%2f..%2fsecret.txt",
            claim.static_context, claim.package_digest
        ),
    ] {
        let response = reqwest::get(format!("{origin}{attempt}"))
            .await
            .expect("request");
        assert_ne!(
            response.status(),
            200,
            "{attempt} was served with status {}",
            response.status()
        );
        let body = response.text().await.unwrap_or_default();
        assert!(
            !body.contains("top secret"),
            "{attempt} leaked the file above the package"
        );
    }

    release(&claim.lease);
    assert!(wait_until_closed(port).await);
}

// ── Blocker 2: a failed open must not release someone else's lease ───────────

#[tokio::test]
async fn a_failed_open_cannot_stop_a_healthy_frame() {
    // Hermes's named regression. Frame A opens; frame B's open fails before it
    // ever acquires; B unmounts and runs its cleanup anyway. With a bare
    // counter that took the host down under A. With leases, B has nothing to
    // present, so its cleanup is a no-op.
    let _guard = lifecycle_guard().await;
    let base = installed(&[("index.html", b"x")]);

    let healthy = acquire(base.path().to_path_buf(), "demo")
        .await
        .expect("frame A");
    assert!(is_listening(healthy.extension_port).await);

    // Frame B never got a lease — its open failed. Cleanup still runs.
    release("");
    release("lease-b-never-existed");
    release(&uuid::Uuid::new_v4().to_string());

    assert!(
        is_listening(healthy.extension_port).await,
        "a failed frame's cleanup stopped the host still serving a healthy one"
    );
    assert_eq!(running_port(), Some(healthy.extension_port));

    release(&healthy.lease);
    assert!(wait_until_closed(healthy.extension_port).await);
}

#[tokio::test]
async fn releasing_the_same_lease_twice_does_not_close_another_frame() {
    // The unmount/promise race: cleanup can run more than once. A second
    // release of an already-returned lease must not consume a live one.
    let _guard = lifecycle_guard().await;
    let base = installed(&[("index.html", b"x")]);

    let first = acquire(base.path().to_path_buf(), "demo")
        .await
        .expect("first");
    let second = acquire(base.path().to_path_buf(), "demo")
        .await
        .expect("second");

    release(&first.lease);
    release(&first.lease);
    release(&first.lease);

    assert!(
        is_listening(second.extension_port).await,
        "a repeated release consumed a different frame's lease"
    );

    release(&second.lease);
    assert!(wait_until_closed(second.extension_port).await);
}

// ── Blocker 1: the wrapper is the navigation container ───────────────────────

#[tokio::test]
async fn the_wrapper_document_carries_the_navigation_wall() {
    let _guard = lifecycle_guard().await;
    let base = tempfile::tempdir().expect("tempdir");
    let root = base.path().join("demo");
    fs::create_dir_all(&root).expect("root");
    fs::write(
        root.join("extension.json"),
        br#"{ "id": "demo", "name": "Demo", "version": "1", "entry": "index.html" }"#,
    )
    .expect("manifest");
    fs::write(root.join("index.html"), b"<!doctype html>hello").expect("entry");

    let claim = acquire(base.path().to_path_buf(), "demo")
        .await
        .expect("acquire");
    let origin = origin_for_port(claim.extension_port);
    let wrapper_origin = origin_for_port(claim.wrapper_port);
    assert_ne!(
        origin, wrapper_origin,
        "wrapper and package content must not share an origin"
    );

    let response = reqwest::get(wrapper_url(
        &wrapper_origin,
        &claim.static_context,
        &claim.package_digest,
        "demo",
    ))
    .await
    .expect("request");
    assert_eq!(response.status(), 200);
    let policy = response
        .headers()
        .get(header::CONTENT_SECURITY_POLICY)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let body = response.text().await.expect("body");

    // `frame-src` pinned to this origin is the wall: a nested context's
    // navigation is checked against its container's policy, so the extension
    // cannot navigate itself to an external sink.
    assert!(
        policy.contains(&format!("frame-src {origin}")),
        "the wrapper must bound where its child may navigate; got: {policy}"
    );
    assert!(
        !policy.contains("frame-src *") && !policy.contains("frame-src https:"),
        "the wall must not admit an external origin; got: {policy}"
    );
    // It embeds the entry from the manifest, sandboxed, and nothing else.
    assert!(
        body.contains(&frame_url(
            &origin,
            &claim.static_context,
            &claim.package_digest,
            "demo",
            "index.html",
        )),
        "wrapper should embed the manifest entry; got: {body}"
    );
    assert!(
        body.contains(r#"sandbox="allow-scripts""#),
        "the inner frame must keep the sandbox; got: {body}"
    );

    release(&claim.lease);
    assert!(wait_until_closed(claim.extension_port).await);
}

// ── Distinct origins (P4 scaffolding) ────────────────────────────────────────
//
// The wrapper and package content are served from separate origins so a hostile
// extension cannot reach the privileged document by same-origin navigation.
// Each direction is asserted, because a split that leaks either way is not a
// split.

#[tokio::test]
async fn the_wrapper_is_not_served_from_the_extension_origin() {
    let _guard = lifecycle_guard().await;
    let base = tempfile::tempdir().expect("tempdir");
    let root = base.path().join("demo");
    fs::create_dir_all(&root).expect("root");
    fs::write(
        root.join("extension.json"),
        br#"{ "id": "demo", "name": "Demo", "version": "1", "entry": "index.html" }"#,
    )
    .expect("manifest");
    fs::write(root.join("index.html"), b"<!doctype html>hello").expect("entry");

    let claim = acquire(base.path().to_path_buf(), "demo")
        .await
        .expect("acquire");
    let extension_origin = origin_for_port(claim.extension_port);
    let wrapper_origin = origin_for_port(claim.wrapper_port);

    // CONTROL: the wrapper really is reachable on its own origin, so the 404
    // below is the split working rather than a broken package or dead host.
    let ok = reqwest::get(wrapper_url(
        &wrapper_origin,
        &claim.static_context,
        &claim.package_digest,
        "demo",
    ))
    .await
    .expect("wrapper request");
    assert_eq!(ok.status(), 200, "wrapper must serve on the wrapper origin");

    // The property: the extension's own origin has no wrapper route at all, so
    // package content cannot navigate to the privileged document.
    let denied = reqwest::get(wrapper_url(
        &extension_origin,
        &claim.static_context,
        &claim.package_digest,
        "demo",
    ))
    .await
    .expect("request");
    assert_eq!(
        denied.status(),
        404,
        "the wrapper must NOT be reachable from the package-content origin"
    );

    release(&claim.lease);
    assert!(wait_until_closed(claim.extension_port).await);
}

#[tokio::test]
async fn package_content_is_not_served_from_the_wrapper_origin() {
    let _guard = lifecycle_guard().await;
    let base = installed(&[("index.html", b"<!doctype html>hello")]);
    let claim = acquire(base.path().to_path_buf(), "demo")
        .await
        .expect("acquire");
    let extension_origin = origin_for_port(claim.extension_port);
    let wrapper_origin = origin_for_port(claim.wrapper_port);

    // CONTROL: the asset is genuinely servable, on its own origin.
    let ok = reqwest::get(frame_url(
        &extension_origin,
        &claim.static_context,
        &claim.package_digest,
        "demo",
        "index.html",
    ))
    .await
    .expect("asset request");
    assert_eq!(ok.status(), 200);

    // Nothing package-authored is ever served from the privileged origin.
    let denied = reqwest::get(frame_url(
        &wrapper_origin,
        &claim.static_context,
        &claim.package_digest,
        "demo",
        "index.html",
    ))
    .await
    .expect("request");
    assert_eq!(
        denied.status(),
        404,
        "package bytes must never come from the wrapper origin"
    );

    release(&claim.lease);
    assert!(wait_until_closed(claim.extension_port).await);
}

#[tokio::test]
async fn exact_static_contexts_serve_only_their_own_csp_and_fail_as_identical_404s() {
    let _guard = lifecycle_guard().await;
    let base = installed(&[("index.html", b"<!doctype html>same package")]);
    let digest =
        super::super::management::package_digest(&base.path().join("demo")).expect("digest");
    // Deliberately insert the wider B owner first. Exact routing must make map
    // order irrelevant.
    let b = acquire_authorized(
        base.path().to_path_buf(),
        "demo",
        "identity-b",
        &digest,
        "index.html",
        vec!["https://b.example".into()],
    )
    .await
    .expect("B");
    let a = acquire_authorized(
        base.path().to_path_buf(),
        "demo",
        "identity-a",
        &digest,
        "index.html",
        Vec::new(),
    )
    .await
    .expect("A");
    let origin = origin_for_port(a.extension_port);
    let url_a = frame_url(&origin, &a.static_context, &digest, "demo", "index.html");
    let url_b = frame_url(&origin, &b.static_context, &digest, "demo", "index.html");
    super::super::management::reset_package_tree_walks(&base.path().join("demo"));
    let write_fence = super::super::management::lifecycle_write_fence().await;
    let request_url = url_a.clone();
    let mut request_a = tokio::spawn(async move { reqwest::get(request_url).await });
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(75), &mut request_a)
            .await
            .is_err(),
        "asset admission crossed the package-mutation write fence"
    );
    drop(write_fence);
    let response_a = request_a.await.expect("A join").expect("A asset");
    let response_b = reqwest::get(&url_b).await.expect("B asset");
    let csp_a = response_a
        .headers()
        .get(header::CONTENT_SECURITY_POLICY)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    let csp_b = response_b
        .headers()
        .get(header::CONTENT_SECURITY_POLICY)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    assert!(csp_a.contains("connect-src 'none'"), "A: {csp_a}");
    assert!(
        csp_b.contains("connect-src https://b.example"),
        "B: {csp_b}"
    );
    assert!(!csp_a.contains("b.example"), "A adopted B: {csp_a}");

    let foreign = [
        frame_url(&origin, "unknown", &digest, "demo", "index.html"),
        frame_url(&origin, &a.static_context, "wrong", "demo", "index.html"),
        frame_url(&origin, &a.static_context, &digest, "other", "index.html"),
    ];
    for url in foreign {
        let response = reqwest::get(url).await.expect("foreign");
        assert_eq!(response.status(), 404);
        assert!(response.bytes().await.expect("body").is_empty());
    }
    release(&a.lease);
    let stale = reqwest::get(url_a).await.expect("stale");
    assert_eq!(stale.status(), 404);
    assert!(stale.bytes().await.expect("body").is_empty());
    assert_eq!(
        super::super::management::package_tree_walks(),
        0,
        "HTTP asset admission must not walk the package tree"
    );
    release(&b.lease);
    assert!(wait_until_closed(b.extension_port).await);
}

/// Origin the committed E2E fixture is generated for. Any literal works; it
/// only has to match on both sides.
#[cfg(test)]
const WRAPPER_CSP_FIXTURE_ORIGIN: &str = "http://127.0.0.1:51234";

#[test]
fn the_e2e_wrapper_csp_fixture_matches_production() {
    // Closes the drift that let 27 E2E specs stay green over a blank surface:
    // the browser-side regression hand-wrote its own policy, so it could never
    // notice production growing a header that refuses framing.
    //
    // The fixture below is the string the E2E spec serves. This test is what
    // makes it the REAL one. If you changed the wrapper policy, regenerate:
    //
    //   cargo test -p buzz-desktop the_e2e_wrapper_csp_fixture -- --nocapture
    //
    // and paste the printed value into the fixture file. Then run the E2E
    // regression, because a policy that refuses embedding will turn it red —
    // which is the entire point.
    let fixture_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../tests/e2e/fixtures/wrapper-csp.txt");
    let produced = wrapper_content_security_policy(WRAPPER_CSP_FIXTURE_ORIGIN);
    // Printed BEFORE the read, so a missing fixture still tells you what to
    // write into it rather than only that it is missing.
    println!("PRODUCTION WRAPPER CSP:\n{produced}");
    let committed = std::fs::read_to_string(&fixture_path)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", fixture_path.display()));

    assert_eq!(
        produced.trim(),
        committed.trim(),
        "the E2E wrapper-CSP fixture has drifted from production. Regenerate it \
         from the value printed above, then rerun the E2E regression."
    );
}

#[test]
fn the_wrapper_policy_does_not_yet_refuse_being_embedded() {
    // Deliberately inverted, and this test is the tripwire.
    //
    // `frame-ancestors 'none'` is the confused-deputy wall and it belongs here
    // EVENTUALLY. It cannot ship yet: Buzz frames this document itself
    // (`ExtensionFrame.tsx` renders `<iframe src={target.url}>` and that url is
    // now the wrapper's), so the header refuses the composition that ships and
    // blanks the extension surface. Measured in Chromium: framed → 0 markers,
    // top-level → 1.
    //
    // If you are here because this assertion failed, you added the header. Add
    // it only together with the migration that makes the wrapper the TOP-LEVEL
    // document of the dedicated native webview — otherwise you have just
    // broken extensions.
    let policy = wrapper_content_security_policy("http://127.0.0.1:4321");
    assert!(
        !policy.contains("frame-ancestors"),
        "frame-ancestors cannot ship while Buzz still frames the wrapper; got: {policy}"
    );
}

#[test]
fn deferring_frame_ancestors_leaves_no_hole_in_todays_composition() {
    // Why the deferral above is sequencing rather than an accepted hole: a
    // hostile package cannot frame anything at all, so it cannot embed a
    // wrapper to become a confused deputy in the first place.
    let extension_policy = content_security_policy("http://127.0.0.1:4321");
    assert!(
        extension_policy.contains("default-src 'none'"),
        "the extension document must default-deny; got: {extension_policy}"
    );
    assert!(
        !extension_policy.contains("frame-src"),
        "the extension document must not be able to frame anything, which is \
         what makes deferring frame-ancestors safe today; got: {extension_policy}"
    );
}

#[test]
fn the_wrapper_frames_the_extension_origin_not_its_own() {
    // If `frame-src` named the wrapper's own origin the split would be
    // decorative: the wrapper could only frame documents from the privileged
    // origin, and the extension would have to live there.
    let policy = wrapper_content_security_policy("http://127.0.0.1:4321");
    assert!(
        policy.contains("frame-src http://127.0.0.1:4321"),
        "the wrapper must frame the extension origin; got: {policy}"
    );
}

#[tokio::test]
async fn the_wrapper_refuses_an_unknown_or_invalid_extension() {
    let _guard = lifecycle_guard().await;
    let base = installed(&[("index.html", b"x")]);
    let claim = acquire(base.path().to_path_buf(), "demo")
        .await
        .expect("acquire");
    let origin = origin_for_port(claim.wrapper_port);

    for id in ["../demo", "nope", "Evil"] {
        let response = reqwest::get(format!(
            "{origin}/{FRAME_ROUTE_PREFIX}/{}/{}/{id}",
            claim.static_context, claim.package_digest
        ))
        .await
        .expect("request");
        assert_eq!(response.status(), 404, "id {id:?} must not get a wrapper");
    }

    release(&claim.lease);
    assert!(wait_until_closed(claim.extension_port).await);
}

// ── Blocker 1 (round 3): the WebRTC wall ─────────────────────────────────────

#[test]
fn the_document_policy_blocks_webrtc() {
    let policy = content_security_policy("http://127.0.0.1:4321");
    assert!(policy.contains("webrtc 'block'"), "got: {policy}");
}

#[test]
fn the_lockdown_precedes_every_package_byte() {
    let html = "<!DOCTYPE html>\n<html><head><script src=\"theirs.js\"></script></head></html>";
    let served = document_with_lockdown(html, "http://127.0.0.1:4321");
    let lockdown = served.find(LOCKDOWN_ROUTE).expect("lockdown present");
    let theirs = served.find("theirs.js").expect("their script present");
    assert!(lockdown < theirs, "the wall must run first: {served}");
    // Our own doctype leads, so the document is standards-mode regardless of
    // what the package declared.
    assert!(
        served.to_ascii_lowercase().starts_with("<!doctype html>"),
        "got: {served}"
    );
}

#[test]
fn a_commented_out_doctype_cannot_hide_the_lockdown() {
    // The reported bypass: splicing after the first `<!doctype` put the tag
    // inside a comment, so it never ran while the real document below executed
    // unprotected. The host writes its own prologue now, so attacker markup
    // cannot choose where the lockdown lands.
    let hostile = "<!-- <!doctype html> -->\n<!doctype html>\n<script src=\"theirs.js\"></script>";
    let served = document_with_lockdown(hostile, "http://127.0.0.1:4321");

    let lockdown = served.find(LOCKDOWN_ROUTE).expect("lockdown present");
    let comment_open = served.find("<!--").expect("comment present");
    let comment_close = served.find("-->").expect("comment close present");
    assert!(
        lockdown < comment_open || lockdown > comment_close,
        "the lockdown must not sit inside the package's comment: {served}"
    );
    // And it still precedes their script.
    assert!(
        lockdown < served.find("theirs.js").expect("their script"),
        "got: {served}"
    );
}

#[test]
fn no_package_markup_can_precede_the_lockdown() {
    // Whatever a package opens with, the host's prologue is earlier in the byte
    // stream — which is the property the previous landmark-splicing lacked.
    for opener in [
        "<!-- <!doctype html> -->",
        "<!DoCtYpE html>",
        "<script src=\"first.js\"></script>",
        "\u{feff}<!doctype html>",
        "<!--",
        "",
    ] {
        let served = document_with_lockdown(opener, "http://127.0.0.1:4321");
        let lockdown = served.find(LOCKDOWN_ROUTE).expect("lockdown present");
        let package_starts = served.len() - opener.len();
        assert!(
            lockdown < package_starts,
            "package markup {opener:?} preceded the lockdown: {served}"
        );
    }
}

#[test]
fn the_lockdown_makes_the_constructor_unrestorable() {
    // `configurable: false` is the load-bearing part: a plain assignment or
    // `delete` could be undone by extension script running afterwards.
    assert!(
        REALM_LOCKDOWN_SOURCE.contains("configurable:false"),
        "the neutralisation must not be reversible"
    );
    assert!(REALM_LOCKDOWN_SOURCE.contains("webkitRTCPeerConnection"));
}

#[test]
fn the_extension_policy_forbids_inline_script() {
    // This is what actually closes the nested-realm escape: a `srcdoc` child
    // inherits this policy, so with no `'unsafe-inline'` its inline script does
    // not run and it cannot hand back a pristine RTCPeerConnection. The first
    // attempt here allowed inline script and was defeated exactly that way.
    let policy = content_security_policy("http://127.0.0.1:4321");
    let script_src = policy
        .split(';')
        .map(str::trim)
        .find(|clause| clause.starts_with("script-src"))
        .expect("script-src present");
    assert!(
        !script_src.contains("'unsafe-inline'"),
        "inline script re-opens the nested-realm escape; got: {script_src}"
    );
    assert_eq!(script_src, "script-src http://127.0.0.1:4321");
}

#[tokio::test]
async fn html_is_locked_down_over_the_wire_and_other_types_are_untouched() {
    let _guard = lifecycle_guard().await;
    let base = installed(&[
        ("index.html", b"<!doctype html><title>x</title>"),
        ("app.js", b"// RTCPeerConnection stays a word in JS source"),
        ("data.json", b"{}"),
    ]);
    let claim = acquire(base.path().to_path_buf(), "demo")
        .await
        .expect("acquire");
    let origin = origin_for_port(claim.extension_port);

    let html = reqwest::get(format!(
        "{origin}/{EXTENSION_ROUTE_PREFIX}/{}/{}/demo/index.html",
        claim.static_context, claim.package_digest
    ))
    .await
    .expect("request")
    .text()
    .await
    .expect("body");
    assert!(
        html.contains(LOCKDOWN_ROUTE),
        "served HTML must pull in the lockdown; got: {html}"
    );
    // And the lockdown itself is actually served.
    let lockdown = reqwest::get(format!("{origin}/{LOCKDOWN_ROUTE}"))
        .await
        .expect("request")
        .text()
        .await
        .expect("body");
    assert!(lockdown.contains("configurable:false"), "got: {lockdown}");

    // A script asset is a subresource, not a realm, and is served byte-for-byte:
    // a worker derives its policy from its own response, so rewriting served
    // JavaScript would break legitimate workers and `importScripts`.
    let js = reqwest::get(format!(
        "{origin}/{EXTENSION_ROUTE_PREFIX}/{}/{}/demo/app.js",
        claim.static_context, claim.package_digest
    ))
    .await
    .expect("request")
    .text()
    .await
    .expect("body");
    assert_eq!(js, "// RTCPeerConnection stays a word in JS source");

    release(&claim.lease);
    assert!(wait_until_closed(claim.extension_port).await);
}

// ── Route 2: non-HTML active documents cannot execute ───────────────────────

#[test]
fn svg_and_xml_documents_are_refused_script() {
    let origin = "http://127.0.0.1:4321";
    for kind in [
        "image/svg+xml",
        "application/xhtml+xml",
        "application/xml",
        "text/xml",
    ] {
        let policy = asset_content_security_policy(origin, kind);
        assert!(
            policy.contains("script-src 'none'"),
            "{kind} is a realm the host cannot write a prologue into: {policy}"
        );
        assert!(
            !policy.contains(&format!("script-src {origin}")),
            "{kind} must not keep the executable script source: {policy}"
        );
    }
}

#[test]
fn subresources_keep_the_policy_they_need() {
    let origin = "http://127.0.0.1:4321";
    // Scripts especially: a worker takes its execution policy from its own
    // response headers, so `script-src 'none'` here would break legitimate
    // workers and `importScripts`.
    for kind in [
        "text/javascript; charset=utf-8",
        "text/css; charset=utf-8",
        "image/png",
        "font/woff2",
        "application/json; charset=utf-8",
        "text/html; charset=utf-8",
    ] {
        let policy = asset_content_security_policy(origin, kind);
        assert!(
            policy.contains(&format!("script-src {origin}")),
            "{kind} should keep the ordinary policy: {policy}"
        );
        assert!(!policy.contains("script-src 'none'"), "{kind}: {policy}");
    }
}

#[tokio::test]
async fn an_svg_asset_is_served_renderable_but_inert() {
    let _guard = lifecycle_guard().await;
    let base = installed(&[
        ("index.html", b"<!doctype html>"),
        (
            "asset.svg",
            b"<svg xmlns=\"http://www.w3.org/2000/svg\"></svg>",
        ),
    ]);
    let claim = acquire(base.path().to_path_buf(), "demo")
        .await
        .expect("acquire");
    let origin = origin_for_port(claim.extension_port);

    let response = reqwest::get(format!(
        "{origin}/{EXTENSION_ROUTE_PREFIX}/{}/{}/demo/asset.svg",
        claim.static_context, claim.package_digest
    ))
    .await
    .expect("request");
    assert_eq!(response.status(), 200, "the SVG must still be servable");
    assert_eq!(
        response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("image/svg+xml"),
        "rendering as an image must keep working"
    );
    let policy = response
        .headers()
        .get(header::CONTENT_SECURITY_POLICY)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(policy.contains("script-src 'none'"), "got: {policy}");

    release(&claim.lease);
    assert!(wait_until_closed(claim.extension_port).await);
}
