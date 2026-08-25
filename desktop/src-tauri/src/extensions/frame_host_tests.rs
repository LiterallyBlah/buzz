//! Tests for the extension frame host.
//!
//! Split in two:
//!
//! - **Resolution/header tests** are pure functions over a temp directory and
//!   run in parallel like any other test.
//! - **Lifecycle tests** drive the process-wide listener, so they take
//!   [`TEST_LOCK`] and reset the global on both sides. Rust runs tests in one
//!   process in parallel by default; without this they would be measuring each
//!   other rather than the host.

use std::fs;
use std::io::Write;

use super::*;

/// Serialises tests that touch the process-wide frame-host state.
///
/// Async-aware on purpose: these tests hold the guard across `await`, and a
/// `std::sync::Mutex` held across an await point is a real deadlock hazard on a
/// multi-threaded runtime (and `clippy::await_holding_lock` says so).
static TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn lifecycle_guard() -> tokio::sync::MutexGuard<'static, ()> {
    let guard = TEST_LOCK.lock().await;
    // Whatever a previous test left behind is not this test's starting state.
    shutdown_now();
    guard
}

/// An installed package containing `files`, under a fresh extensions base dir.
fn installed(files: &[(&str, &[u8])]) -> tempfile::TempDir {
    let base = tempfile::tempdir().expect("tempdir");
    let root = base.path().join("demo");
    for (name, body) in files {
        let path = root.join(name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("mkdir");
        }
        let mut file = fs::File::create(&path).expect("create");
        file.write_all(body).expect("write");
    }
    base
}

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

// ── Lifecycle ────────────────────────────────────────────────────────────────

/// Does anything accept a TCP connection on this port?
async fn is_listening(port: u16) -> bool {
    tokio::net::TcpStream::connect((Ipv4Addr::LOCALHOST, port))
        .await
        .is_ok()
}

/// Wait for the listener to stop, so a graceful shutdown is not read as a leak.
async fn wait_until_closed(port: u16) -> bool {
    for _ in 0..50 {
        if !is_listening(port).await {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    false
}

#[tokio::test]
async fn the_host_starts_serves_and_leaves_no_listener_behind() {
    let _guard = lifecycle_guard().await;
    let base = installed(&[("index.html", b"<!doctype html><title>demo</title>")]);

    let port = acquire(base.path().to_path_buf())
        .await
        .expect("host should start");
    assert!(is_listening(port).await, "nothing is listening on {port}");
    assert_eq!(running_port(), Some(port));

    let body = reqwest::get(format!(
        "{}/{EXTENSION_ROUTE_PREFIX}/demo/index.html",
        origin_for_port(port)
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

    release();
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

    let first = acquire(base.path().to_path_buf()).await.expect("first");
    let second = acquire(base.path().to_path_buf()).await.expect("second");
    assert_eq!(first, second, "a second frame must reuse the one listener");

    release();
    assert!(
        is_listening(first).await,
        "the host stopped while a frame was still open"
    );

    release();
    assert!(
        wait_until_closed(first).await,
        "the last release must stop it"
    );
}

#[tokio::test]
async fn shutdown_stops_the_host_even_with_holders_outstanding() {
    // A crashed or reloaded webview never releases. App exit must still not
    // leave a listener behind.
    let _guard = lifecycle_guard().await;
    let base = installed(&[("index.html", b"x")]);

    let port = acquire(base.path().to_path_buf()).await.expect("acquire");
    let _ = acquire(base.path().to_path_buf()).await.expect("acquire");
    assert!(is_listening(port).await);

    shutdown_now();
    assert!(
        wait_until_closed(port).await,
        "shutdown left a listener on {port}"
    );
    assert_eq!(running_port(), None);

    // Releasing after shutdown must not underflow or resurrect anything.
    release();
    release();
    release();
    assert_eq!(running_port(), None);
}

#[tokio::test]
async fn a_restarted_host_serves_again_on_a_fresh_port() {
    let _guard = lifecycle_guard().await;
    let base = installed(&[("index.html", b"x")]);

    let first = acquire(base.path().to_path_buf()).await.expect("first");
    release();
    assert!(wait_until_closed(first).await);

    let second = acquire(base.path().to_path_buf()).await.expect("second");
    assert!(is_listening(second).await, "the host did not come back");
    release();
    assert!(wait_until_closed(second).await);
}

#[tokio::test]
async fn traversal_is_refused_over_the_wire_too() {
    let _guard = lifecycle_guard().await;
    let base = installed(&[("index.html", b"x")]);
    fs::write(base.path().join("secret.txt"), b"top secret").expect("secret");

    let port = acquire(base.path().to_path_buf()).await.expect("acquire");
    let origin = origin_for_port(port);

    // Sent raw so the client cannot normalise the traversal away before it
    // reaches the server — the point is what the server does with it.
    for attempt in [
        "/ext/demo/../secret.txt",
        "/ext/../secret.txt",
        "/ext/demo/..%2fsecret.txt",
        "/ext/demo%2f..%2fsecret.txt",
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

    release();
    assert!(wait_until_closed(port).await);
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

    let port = acquire(base.path().to_path_buf()).await.expect("acquire");
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

    release();
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
