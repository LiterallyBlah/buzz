//! Frame-host tests over pure functions: path resolution, header shape and
//! URL construction.
//!
//! Nothing here binds a socket. Document and policy shape live in
//! `frame_host_policy_tests`; anything driving a live listener lives in
//! `frame_host_wire_tests`, which is why the lifecycle guard is not used here.

use std::fs;

use super::frame_host_test_support::{installed, wait_until_closed};
use super::*;

/// An installed package containing `files`, under a fresh extensions base dir.
// ── Resolution ───────────────────────────────────────────────────────────────

#[test]
fn resolves_an_asset_inside_the_package() {
    let base = installed(&[("index.html", b"<!doctype html>")]);
    let resolved = resolve_asset(base.path(), "demo", "index.html").expect("should resolve");
    assert!(resolved.ends_with("index.html"));
    assert!(resolved.is_file());
}

#[test]
fn resolves_a_nested_asset() {
    let base = installed(&[("assets/app.js", b"console.log(1)")]);
    resolve_asset(base.path(), "demo", "assets/app.js").expect("nested asset should resolve");
}

#[test]
fn rejects_ids_outside_the_install_grammar() {
    let base = installed(&[("index.html", b"x")]);
    // The id names a directory, so the grammar is the wall: none of these can
    // address anything but a folder that cannot exist.
    for id in ["../demo", "..", ".", "De.mo", "demo/", "a/b", "a\\b", ""] {
        assert_eq!(
            resolve_asset(base.path(), id, "index.html"),
            Err(AssetError::InvalidId),
            "id {id:?} must be refused"
        );
    }
}

#[test]
fn rejects_traversing_and_rooted_asset_paths() {
    let base = installed(&[("index.html", b"x")]);
    // A real secret one level up, to make the traversal worth attempting.
    fs::write(base.path().join("secret.txt"), b"top secret").expect("secret");

    for asset in [
        "../secret.txt",
        "../../etc/passwd",
        "/etc/passwd",
        "\\Windows\\win.ini",
        "C:\\Windows\\win.ini",
        "assets/../../secret.txt",
    ] {
        let outcome = resolve_asset(base.path(), "demo", asset);
        assert!(
            matches!(
                outcome,
                Err(AssetError::UnsafePath) | Err(AssetError::NotFound)
            ),
            "asset {asset:?} must not resolve, got {outcome:?}"
        );
    }
    // And the secret is genuinely reachable by a path that does not traverse,
    // so the assertions above are not passing because the file is missing.
    assert!(base.path().join("secret.txt").is_file());
}

#[test]
fn never_lists_a_directory() {
    let base = installed(&[("assets/app.js", b"x")]);
    assert_eq!(
        resolve_asset(base.path(), "demo", "assets"),
        Err(AssetError::NotFound)
    );
}

#[test]
fn a_missing_package_is_not_found() {
    let base = installed(&[("index.html", b"x")]);
    assert_eq!(
        resolve_asset(base.path(), "other", "index.html"),
        Err(AssetError::NotFound)
    );
}

#[cfg(unix)]
#[test]
fn rejects_a_symlink_escaping_the_package_root() {
    // The installer refuses symlinks, but an installed tree lives on disk where
    // anything may happen to it afterwards. Canonical containment is the gate
    // that survives a package tampered with after install.
    let base = installed(&[("index.html", b"x")]);
    let outside = base.path().join("outside.txt");
    fs::write(&outside, b"not yours").expect("outside file");
    std::os::unix::fs::symlink(&outside, base.path().join("demo").join("link.txt"))
        .expect("symlink");

    assert_eq!(
        resolve_asset(base.path(), "demo", "link.txt"),
        Err(AssetError::UnsafePath),
        "a symlink out of the package must not be served"
    );
}

// ── Headers ──────────────────────────────────────────────────────────────────

#[test]
fn content_types_come_from_the_name_not_the_bytes() {
    assert_eq!(
        content_type_for(Path::new("a/index.html")),
        "text/html; charset=utf-8"
    );
    assert_eq!(
        content_type_for(Path::new("a/app.js")),
        "text/javascript; charset=utf-8"
    );
    assert_eq!(content_type_for(Path::new("a/x.wasm")), "application/wasm");
    assert_eq!(content_type_for(Path::new("a/x.png")), "image/png");
    // Unknown stays opaque rather than being guessed at.
    assert_eq!(
        content_type_for(Path::new("a/payload.bin")),
        "application/octet-stream"
    );
    assert_eq!(
        content_type_for(Path::new("a/noextension")),
        "application/octet-stream"
    );
}

#[test]
fn the_document_policy_is_egress_default_deny() {
    let policy = content_security_policy("http://127.0.0.1:4321");
    assert!(policy.contains("connect-src 'none'"), "got: {policy}");
    assert!(policy.contains("default-src 'none'"), "got: {policy}");
}

#[test]
fn the_document_policy_names_the_origin_and_never_uses_self() {
    // A sandboxed document has an opaque origin, so `'self'` matches nothing —
    // a policy written with it would block the package's own scripts. The
    // serving origin has to be named explicitly.
    let policy = content_security_policy("http://127.0.0.1:4321");
    assert!(
        !policy.contains("'self'"),
        "'self' is meaningless for an opaque origin; got: {policy}"
    );
    assert!(
        policy.contains("script-src http://127.0.0.1:4321"),
        "got: {policy}"
    );
}

#[test]
fn the_document_policy_leaves_framing_to_the_parent() {
    // `frame-ancestors` has no `default-src` fallback, so omitting it is what
    // allows the Buzz window to frame the document. Naming the parent origin
    // would break across platforms.
    let policy = content_security_policy("http://127.0.0.1:4321");
    assert!(!policy.contains("frame-ancestors"), "got: {policy}");
}

// ── Frame URL ────────────────────────────────────────────────────────────────

#[test]
fn the_frame_url_names_the_remote_class_origin() {
    let url = frame_url("http://127.0.0.1:4321", "demo", "index.html");
    assert_eq!(url, "http://127.0.0.1:4321/ext/demo/index.html");
    // A registered custom scheme would be classified local by Tauri and would
    // void the BX-09 containment evidence — the origin must stay plain HTTP.
    assert!(url.starts_with("http://127.0.0.1:"), "got: {url}");
    assert!(!url.contains("://localhost"), "got: {url}");
}

#[test]
fn the_frame_url_keeps_nested_entries_and_encodes_awkward_names() {
    assert_eq!(
        frame_url("http://127.0.0.1:1", "demo", "web/index.html"),
        "http://127.0.0.1:1/ext/demo/web/index.html"
    );
    // A space or '#' would otherwise truncate or mis-address the request.
    assert_eq!(
        frame_url("http://127.0.0.1:1", "demo", "my page.html"),
        "http://127.0.0.1:1/ext/demo/my%20page.html"
    );
    assert_eq!(
        frame_url("http://127.0.0.1:1", "demo", "a#b.html"),
        "http://127.0.0.1:1/ext/demo/a%23b.html"
    );
    // Separators survive as separators, so a nested entry still addresses one
    // file rather than becoming a single encoded segment.
    assert!(!frame_url("http://127.0.0.1:1", "demo", "web/app.js").contains("%2F"));
}

#[tokio::test]
async fn every_served_document_carries_the_egress_policy() {
    // Decision 004 is a property of the *host*, not of HTML: a script or style
    // the page pulls in is as much an egress vector as the document, and a
    // policy that arrived on only one of them would be trivially sidestepped.
    let _guard = lifecycle_guard().await;
    let base = installed(&[
        ("index.html", b"<!doctype html>"),
        ("app.js", b"console.log(1)"),
        ("app.css", b"body{}"),
        ("icon.png", b"\x89PNG\r\n\x1a\n"),
        ("data.json", b"{}"),
    ]);

    let claim = acquire(base.path().to_path_buf(), "demo")
        .await
        .expect("acquire");
    let port = claim.extension_port;
    let origin = origin_for_port(port);

    for asset in ["index.html", "app.js", "app.css", "icon.png", "data.json"] {
        let response = reqwest::get(format!("{origin}/{EXTENSION_ROUTE_PREFIX}/demo/{asset}"))
            .await
            .expect("request");
        assert_eq!(response.status(), 200, "{asset} should be served");
        let policy = response
            .headers()
            .get(header::CONTENT_SECURITY_POLICY)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_string();
        assert!(
            policy.contains("connect-src 'none'"),
            "{asset} was served without the egress policy; got: {policy:?}"
        );
        assert!(
            policy.contains(&format!("script-src {origin}")),
            "{asset} policy must name the serving origin; got: {policy:?}"
        );
    }

    release(&claim.lease);
    assert!(wait_until_closed(port).await);
}

#[test]
fn the_document_policy_does_not_govern_post_message() {
    // BRIDGE_SPEC §2's future handshake is the frame posting `{buzz:"ready"}`
    // to its parent. `postMessage` is not a fetch directive, so no CSP source
    // list can block it — but "no directive we set names it" is the thing to
    // check, not the thing to assume. The browser-side proof that a document
    // under this policy really can reach its parent is the Playwright spec;
    // this is the cheap structural half.
    let policy = content_security_policy("http://127.0.0.1:4321");
    for directive in [
        "script-src",
        "connect-src",
        "default-src",
        "frame-ancestors",
    ] {
        let names_messaging = policy
            .split(';')
            .filter(|clause| clause.trim().starts_with(directive))
            .any(|clause| clause.contains("postMessage") || clause.contains("message"));
        assert!(
            !names_messaging,
            "{directive} must not constrain messaging; got: {policy}"
        );
    }
    // And the sandbox is not tightened through CSP either, which *would* bite:
    // a `sandbox` CSP directive without `allow-scripts` would stop the frame
    // running the script that posts.
    assert!(
        !policy.contains("sandbox"),
        "the served policy must not add its own sandbox; got: {policy}"
    );
}
