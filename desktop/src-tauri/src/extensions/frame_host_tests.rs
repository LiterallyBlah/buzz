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

    let claim = acquire(base.path().to_path_buf(), "demo")
        .await
        .expect("host should start");
    let port = claim.extension_port;
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

    release(&claim.lease);
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

    let response = reqwest::get(wrapper_url(&wrapper_origin, "demo"))
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
        body.contains(&frame_url(&origin, "demo", "index.html")),
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
    let ok = reqwest::get(wrapper_url(&wrapper_origin, "demo"))
        .await
        .expect("wrapper request");
    assert_eq!(ok.status(), 200, "wrapper must serve on the wrapper origin");

    // The property: the extension's own origin has no wrapper route at all, so
    // package content cannot navigate to the privileged document.
    let denied = reqwest::get(wrapper_url(&extension_origin, "demo"))
        .await
        .expect("request");
    assert_eq!(
        denied.status(),
        404,
        "the wrapper must NOT be reachable from the package-content origin"
    );

    release(&claim.lease);
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
    let ok = reqwest::get(frame_url(&extension_origin, "demo", "index.html"))
        .await
        .expect("asset request");
    assert_eq!(ok.status(), 200);

    // Nothing package-authored is ever served from the privileged origin.
    let denied = reqwest::get(frame_url(&wrapper_origin, "demo", "index.html"))
        .await
        .expect("request");
    assert_eq!(
        denied.status(),
        404,
        "package bytes must never come from the wrapper origin"
    );

    release(&claim.lease);
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
        let response = reqwest::get(format!("{origin}/{FRAME_ROUTE_PREFIX}/{id}"))
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

    let html = reqwest::get(format!("{origin}/{EXTENSION_ROUTE_PREFIX}/demo/index.html"))
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
    let js = reqwest::get(format!("{origin}/{EXTENSION_ROUTE_PREFIX}/demo/app.js"))
        .await
        .expect("request")
        .text()
        .await
        .expect("body");
    assert_eq!(js, "// RTCPeerConnection stays a word in JS source");

    release(&claim.lease);
    assert!(wait_until_closed(claim.extension_port).await);
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

    let response = reqwest::get(format!("{origin}/{EXTENSION_ROUTE_PREFIX}/demo/asset.svg"))
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
