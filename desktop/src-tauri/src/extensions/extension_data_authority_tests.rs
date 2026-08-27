//! Query-authority hardening: what happens to an extension-data read when
//! authority changes around the unbounded admission wait.
//!
//! Split from [`super::extension_data_tests`] because these prove a different
//! thing: not what §4's methods mean, but that the identity which authenticates
//! the read is still the one that was admitted by the time the request goes out.

use super::extension_data_test_support::*;
use super::*;

// ── query-authority hardening: authority transitions across the real gate ────
//
// The defect these defend: the generic query path re-derives its NIP-98 key
// from `state` *after* the unbounded admission wait, so a transition into
// recovery during that wait could sign with the ephemeral boot key. Entry
// gating does not cover it — the key used is fetched later than the gate.
//
// Every probe below arms the *real* process-global gate rather than injecting a
// refusal: the point is the production transition that creates a stale
// authority, not that a refusal suppresses a request.

/// A parked read: real armed gate, real grant store, real lease, counting
/// listener. The closure runs while the read is parked in the gate.
async fn read_parked_at_the_gate(
    disturb: impl FnOnce(&crate::AppState, &str, &std::path::Path) + Send + 'static,
) -> (BridgeReply, usize) {
    use tauri::Manager as _;

    let keys = nostr::Keys::generate();
    let identity = keys.public_key().to_hex();
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("grants").join("extension-grants.db");
    {
        let conn = super::super::grants::open_grant_db(&db_path).expect("open");
        super::super::grants::grant_boolean_scope(&conn, &identity, EXTID, SCOPE_EXTENSION_DATA)
            .expect("grant");
    }

    // A listener that accepts and counts, so "no request" is observed rather
    // than asserted. Nothing answers: a parked read must never reach it.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let connections = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = connections.clone();
    std::thread::spawn(move || {
        for _ in listener.incoming() {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    });

    let state = crate::app_state::build_app_state();
    *state.keys.lock().unwrap() = keys;
    *state.relay_url_override.lock().unwrap() = Some(format!("http://{addr}"));

    let app = tauri::test::mock_app();
    app.manage(state);
    if let Ok(prod) = super::super::dispatch::grant_db_path(app.handle()) {
        if let Some(parent) = prod.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::copy(&db_path, &prod);
    }

    super::super::frame_host::insert_lease_for_test(LEASE, EXTID);

    // Arm a real bounded wait, then disturb authority while the read sits in it.
    crate::relay_admission::activate_rate_limit(Some(1));
    let armed_at = std::time::Instant::now();

    let handle = app.handle().clone();
    let read = extension_data_get(
        &handle,
        EXTID,
        LEASE,
        Some(serde_json::json!({ "key": KEY })),
    );

    let state_ref = app.state::<crate::AppState>();
    let prod_db = super::super::dispatch::grant_db_path(app.handle()).unwrap_or(db_path.clone());
    let identity_for_disturb = identity.clone();
    let disturb_task = async {
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        disturb(&state_ref, &identity_for_disturb, &prod_db);
    };

    let (reply, ()) = tokio::join!(read, disturb_task);
    crate::relay_admission::reset_rate_limit_gate();

    // Without a real armed wait the disturbance would land after the read had
    // already finished, and the probe would prove nothing.
    assert!(
        armed_at.elapsed() >= std::time::Duration::from_millis(300),
        "the read must actually have parked in the gate"
    );
    (reply, connections.load(std::sync::atomic::Ordering::SeqCst))
}

#[tokio::test]
async fn identity_lost_during_the_gate_wait_denies_with_no_request() {
    let _gate = crate::relay_admission::gate_guard().await;
    let _host = super::super::frame_host::lifecycle_guard().await;
    let (reply, connections) = read_parked_at_the_gate(|state, _, _| {
        // Production's recovery boot, in production's order: `resolve_persisted_identity`
        // writes an **ephemeral key** into `state.keys` and *then* stores the flag with
        // Release. The key swap is the half that makes this adversarial — with the flag
        // alone, `state.keys` still holds the admitted identity, so an implementation
        // that re-read `state` after the wait would sign correctly anyway and the
        // zero-connection result would not distinguish a fix from the defect.
        *state.keys.lock().unwrap() = nostr::Keys::generate();
        state
            .identity_lost
            .store(true, std::sync::atomic::Ordering::Release);
    })
    .await;
    assert!(denied(&reply), "recovery must refuse: {:?}", reply.error);
    assert_eq!(connections, 0, "no request may reach the relay");
}

#[tokio::test]
async fn keyring_locked_during_the_gate_wait_denies_with_no_request() {
    let _gate = crate::relay_admission::gate_guard().await;
    let _host = super::super::frame_host::lifecycle_guard().await;
    // The sibling recovery state, same shape: ephemeral key first, then the flag.
    let (reply, connections) = read_parked_at_the_gate(|state, _, _| {
        *state.keys.lock().unwrap() = nostr::Keys::generate();
        state
            .keyring_locked
            .store(true, std::sync::atomic::Ordering::Release);
    })
    .await;
    assert!(denied(&reply));
    assert_eq!(connections, 0);
}

#[tokio::test]
async fn an_identity_change_during_the_gate_wait_denies_with_no_request() {
    let _gate = crate::relay_admission::gate_guard().await;
    let _host = super::super::frame_host::lifecycle_guard().await;
    // A *different valid* identity — not recovery. The request was admitted for
    // one user; it must not go out authenticated as another.
    //
    // The incoming identity is granted the same scope on the way in. Without
    // that, the grant lookup refuses first — it is keyed by pubkey — and this
    // probe passes with the identity-equality check deleted, proving only that
    // *something* refused.
    let (reply, connections) = read_parked_at_the_gate(|state, _, db| {
        let other = nostr::Keys::generate();
        let conn = super::super::grants::open_grant_db(db).expect("reopen");
        super::super::grants::grant_boolean_scope(
            &conn,
            &other.public_key().to_hex(),
            EXTID,
            SCOPE_EXTENSION_DATA,
        )
        .expect("grant the incoming identity");
        *state.keys.lock().unwrap() = other;
    })
    .await;
    assert!(denied(&reply));
    assert_eq!(connections, 0);
}

#[tokio::test]
async fn a_grant_revoked_during_the_gate_wait_denies_with_no_request() {
    let _gate = crate::relay_admission::gate_guard().await;
    let _host = super::super::frame_host::lifecycle_guard().await;
    let (reply, connections) = read_parked_at_the_gate(|_, identity, db| {
        let conn = super::super::grants::open_grant_db(db).expect("reopen");
        let removed = super::super::grants::revoke_all(&conn, identity, EXTID).expect("revoke");
        assert_eq!(removed, 1, "the grant must have existed to be revoked");
    })
    .await;
    assert!(denied(&reply));
    assert_eq!(connections, 0);
}

#[tokio::test]
async fn a_lease_released_during_the_gate_wait_denies_with_no_request() {
    let _gate = crate::relay_admission::gate_guard().await;
    let _host = super::super::frame_host::lifecycle_guard().await;
    let (reply, connections) = read_parked_at_the_gate(|_, _, _| {
        super::super::frame_host::release(LEASE);
    })
    .await;
    assert!(denied(&reply));
    assert_eq!(connections, 0);
}

#[tokio::test]
async fn the_connection_counter_sees_a_request_that_is_actually_made() {
    let _gate = crate::relay_admission::gate_guard().await;
    let _host = super::super::frame_host::lifecycle_guard().await;

    // The instrument's own control. Five probes above read `connections == 0`,
    // and that number means nothing until the same listener, on the same port,
    // is shown reporting 1 when a request *is* made — otherwise a counter that
    // never increments passes all five for no reason at all.
    let (reply, connections) = read_parked_at_the_gate(|_, _, _| {}).await;

    assert_eq!(
        connections, 1,
        "an undisturbed read must reach the relay, or the zero-counts prove nothing"
    );
    // The listener accepts and drops without answering, so the read fails at
    // the relay rather than at authority — which is also the assertion that it
    // got past revalidation instead of being refused for some other reason.
    assert_eq!(
        reply.error.as_ref().map(|e| e.code.as_str()),
        Some(code::RELAY_ERROR),
        "an undisturbed read must not be denied: {:?}",
        reply.error
    );
}

#[tokio::test]
async fn the_waiting_wrapper_waits_and_the_extracted_seam_does_not() {
    let _gate = crate::relay_admission::gate_guard().await;

    // The one behavioural risk the extraction creates. `query_relay_at_with_keys`
    // is now a two-line wrapper, so the wait it owes its three existing callers
    // can be deleted without touching anything those callers can see. Stated as
    // a contrast, because "it was slow" is not evidence on its own.
    let keys = nostr::Keys::generate();
    let state = crate::app_state::build_app_state();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    std::thread::spawn(move || for _ in listener.incoming() {});
    let url = format!("http://{addr}");
    let filter = serde_json::json!({ "kinds": [30800], "limit": 1 });

    crate::relay_admission::activate_rate_limit(Some(1));
    let seam_at = std::time::Instant::now();
    let _ = crate::relay::query_relay_at_with_keys_no_wait(
        &state,
        &url,
        std::slice::from_ref(&filter),
        &keys,
        None,
    )
    .await;
    let seam = seam_at.elapsed();

    crate::relay_admission::activate_rate_limit(Some(1));
    let wrapper_at = std::time::Instant::now();
    let _ = crate::relay::query_relay_at_with_keys(
        &state,
        &url,
        std::slice::from_ref(&filter),
        &keys,
        None,
    )
    .await;
    let wrapper = wrapper_at.elapsed();
    crate::relay_admission::reset_rate_limit_gate();

    assert!(
        seam < std::time::Duration::from_millis(500),
        "the extracted seam must not wait — the caller owns the gate: {seam:?}"
    );
    assert!(
        wrapper >= std::time::Duration::from_millis(900),
        "the wrapper must still park its callers on the armed gate: {wrapper:?}"
    );
}

/// A fake relay that records the `Authorization` header it was given, and can
/// disturb authority *after* receiving the request but before answering.
fn capturing_relay(
    seen_auth: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    on_request: Option<Box<dyn Fn() + Send>>,
) -> String {
    use std::io::{Read as _, Write as _};

    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");

    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let mut raw = Vec::new();
            let mut buf = [0u8; 8192];
            while let Ok(n) = stream.read(&mut buf) {
                if n == 0 {
                    break;
                }
                raw.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&raw).to_string();
                if let Some((head, body)) = text.split_once("\r\n\r\n") {
                    let want = head
                        .lines()
                        .find_map(|l| {
                            l.strip_prefix("Content-Length: ")
                                .or_else(|| l.strip_prefix("content-length: "))
                        })
                        .and_then(|v| v.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    if body.len() >= want {
                        break;
                    }
                }
            }
            let text = String::from_utf8_lossy(&raw).to_string();
            if let Some(line) = text
                .lines()
                .find(|l| l.to_ascii_lowercase().starts_with("authorization:"))
            {
                *seen_auth.lock().unwrap() =
                    Some(line.split_once(':').map_or("", |x| x.1).trim().to_string());
            }
            // The request has been received; authority may now be withdrawn
            // before the host sees the answer.
            if let Some(hook) = &on_request {
                hook();
            }
            let payload = "[]";
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                payload.len()
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });
    format!("http://{addr}")
}

#[tokio::test]
async fn proof_6_1_the_full_route_emits_nip98_authored_by_the_admitted_identity() {
    // The route positive. It deliberately does **not** try to prove
    // "A rather than a later B": once revalidation asserts the identity is
    // unchanged, `signing_keys()` and a raw `state.keys` read return the same
    // key whenever the route succeeds, so no honest full-route fixture can
    // separate them. That property is pinned one seam down, in 6.2.
    let _gate = crate::relay_admission::gate_guard().await;
    let _host = super::super::frame_host::lifecycle_guard().await;
    use base64::Engine as _;
    use tauri::Manager as _;

    let keys = nostr::Keys::generate();
    let admitted = keys.public_key().to_hex();
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("grants").join("extension-grants.db");
    {
        let conn = super::super::grants::open_grant_db(&db_path).expect("open");
        super::super::grants::grant_boolean_scope(&conn, &admitted, EXTID, SCOPE_EXTENSION_DATA)
            .expect("grant");
    }

    let seen = std::sync::Arc::new(std::sync::Mutex::new(None));
    let url = capturing_relay(seen.clone(), None);

    let state = crate::app_state::build_app_state();
    *state.keys.lock().unwrap() = keys;
    *state.relay_url_override.lock().unwrap() = Some(url);

    let app = tauri::test::mock_app();
    app.manage(state);
    if let Ok(prod) = super::super::dispatch::grant_db_path(app.handle()) {
        if let Some(parent) = prod.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::copy(&db_path, &prod);
    }
    super::super::frame_host::insert_lease_for_test(LEASE, EXTID);

    let reply = extension_data_get(
        app.handle(),
        EXTID,
        LEASE,
        Some(serde_json::json!({ "key": KEY })),
    )
    .await;
    assert!(reply.ok, "the read must succeed: {:?}", reply.error);

    let header = seen.lock().unwrap().clone().expect("a request was made");
    let token = header.strip_prefix("Nostr ").expect("NIP-98 scheme");
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(token)
        .expect("base64");
    let auth: serde_json::Value = serde_json::from_slice(&decoded).expect("auth event");
    assert_eq!(auth["pubkey"].as_str().expect("pubkey"), admitted);
}

#[tokio::test]
async fn proof_6_2_the_query_signs_with_its_explicit_keys_not_with_state() {
    // The key-source property, pinned at the seam that owns it. `state.keys`
    // deliberately holds B while the caller passes A — a state the full route
    // cannot reach, which is exactly why this is tested here and not there.
    // Real wire bytes; no pretence of being a successful `extensionData.get`.
    let _gate = crate::relay_admission::gate_guard().await;
    use base64::Engine as _;

    let admitted_keys = nostr::Keys::generate();
    let a = admitted_keys.public_key().to_hex();
    let other_keys = nostr::Keys::generate();
    let b = other_keys.public_key().to_hex();
    assert_ne!(a, b);

    let seen = std::sync::Arc::new(std::sync::Mutex::new(None));
    let url = capturing_relay(seen.clone(), None);

    let state = crate::app_state::build_app_state();
    *state.keys.lock().unwrap() = other_keys; // B in state …

    let filter = serde_json::json!({ "kinds": [30800], "limit": 1 });
    let _ = crate::relay::query_relay_at_with_keys_no_wait(
        &state,
        &url,
        &[filter],
        &admitted_keys, // … A passed explicitly
        None,
    )
    .await;

    let header = seen.lock().unwrap().clone().expect("a request was made");
    let token = header.strip_prefix("Nostr ").expect("NIP-98 scheme");
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(token)
        .expect("base64");
    let auth: serde_json::Value = serde_json::from_slice(&decoded).expect("auth event");
    assert_eq!(
        auth["pubkey"].as_str().expect("pubkey"),
        a,
        "the wire must carry the explicitly passed identity, never state's"
    );
    assert_ne!(auth["pubkey"].as_str().expect("pubkey"), b);
}

/// Drive a read whose authority is withdrawn *after* the relay has the request.
///
/// The relay signals when it has received the request and waits; the test
/// disturbs authority with full access to the app, then releases the relay to
/// answer. So the post-response recheck is the only thing that can catch it.
async fn read_disturbed_after_send(
    disturb: impl FnOnce(&tauri::AppHandle<tauri::test::MockRuntime>, &str, &std::path::Path),
) -> BridgeReply {
    use std::io::{Read as _, Write as _};
    use tauri::Manager as _;

    let keys = nostr::Keys::generate();
    let identity = keys.public_key().to_hex();
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("grants").join("extension-grants.db");
    {
        let conn = super::super::grants::open_grant_db(&db_path).expect("open");
        super::super::grants::grant_boolean_scope(&conn, &identity, EXTID, SCOPE_EXTENSION_DATA)
            .expect("grant");
    }

    // Arrival is signalled on a *tokio* channel: the disturbance and the read
    // run on one task, so a blocking wait here would stop the read being polled
    // and the request it is waiting for could never be sent.
    let (arrived_tx, arrived_rx) = tokio::sync::oneshot::channel::<()>();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let mut buf = [0u8; 8192];
            let _ = stream.read(&mut buf);
            let _ = arrived_tx.send(());
            let _ = release_rx.recv();
            let payload = "[]";
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                payload.len()
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });

    let state = crate::app_state::build_app_state();
    *state.keys.lock().unwrap() = keys;
    *state.relay_url_override.lock().unwrap() = Some(format!("http://{addr}"));

    let app = tauri::test::mock_app();
    app.manage(state);
    let prod_db = super::super::dispatch::grant_db_path(app.handle()).unwrap_or(db_path.clone());
    if let Some(parent) = prod_db.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::copy(&db_path, &prod_db);
    super::super::frame_host::insert_lease_for_test(LEASE, EXTID);

    let handle = app.handle().clone();
    let read = extension_data_get(
        &handle,
        EXTID,
        LEASE,
        Some(serde_json::json!({ "key": KEY })),
    );

    let disturber = async {
        // Wait until the relay actually has the request in hand — bounded, so a
        // request that never goes out fails this probe rather than hanging it.
        let arrived = tokio::time::timeout(std::time::Duration::from_secs(10), arrived_rx)
            .await
            .is_ok_and(|r| r.is_ok());
        if arrived {
            disturb(app.handle(), &identity, &prod_db);
        }
        let _ = release_tx.send(());
        arrived
    };

    let (reply, arrived) = tokio::join!(read, disturber);
    // Without this the probe would silently degrade into "authority was already
    // gone before the send", which the *pre*-send recheck catches — a different
    // production line from the one these proofs name.
    assert!(
        arrived,
        "the request must have reached the relay before authority was disturbed"
    );
    reply
}

#[tokio::test(flavor = "multi_thread")]
async fn proof_7a_lease_released_after_send_denies_and_exposes_nothing() {
    let _gate = crate::relay_admission::gate_guard().await;
    let _host = super::super::frame_host::lifecycle_guard().await;
    let reply = read_disturbed_after_send(|_, _, _| {
        super::super::frame_host::release(LEASE);
    })
    .await;
    assert!(denied(&reply), "got {:?}", reply.error);
    assert!(reply.result.is_none(), "no event bytes may be exposed");
}

#[tokio::test(flavor = "multi_thread")]
async fn proof_7b_identity_changed_after_send_denies_and_exposes_nothing() {
    let _gate = crate::relay_admission::gate_guard().await;
    let _host = super::super::frame_host::lifecycle_guard().await;
    // As in the held-gate identity probe: the incoming identity is granted the
    // same scope, so the grant lookup cannot be what refuses. Only the
    // identity-equality branch is left to catch this.
    let reply = read_disturbed_after_send(|app, _, db| {
        use tauri::Manager as _;
        let other = nostr::Keys::generate();
        let conn = super::super::grants::open_grant_db(db).expect("reopen");
        super::super::grants::grant_boolean_scope(
            &conn,
            &other.public_key().to_hex(),
            EXTID,
            SCOPE_EXTENSION_DATA,
        )
        .expect("grant the incoming identity");
        let state = app.state::<crate::AppState>();
        *state.keys.lock().unwrap() = other;
    })
    .await;
    assert!(denied(&reply), "got {:?}", reply.error);
    assert!(reply.result.is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn proof_7c_grant_revoked_after_send_denies_and_exposes_nothing() {
    let _gate = crate::relay_admission::gate_guard().await;
    let _host = super::super::frame_host::lifecycle_guard().await;
    let reply = read_disturbed_after_send(|_, identity, db| {
        let conn = super::super::grants::open_grant_db(db).expect("reopen");
        let removed = super::super::grants::revoke_all(&conn, identity, EXTID).expect("revoke");
        assert_eq!(removed, 1, "the grant must have existed to be revoked");
    })
    .await;
    assert!(denied(&reply), "got {:?}", reply.error);
    assert!(reply.result.is_none());
}

// ── against a real relay ─────────────────────────────────────────────────────

/// Both extension-data methods, end to end against a relay built from this
/// checkout with real PostgreSQL and Redis behind it.
///
/// Ignored by default; pointed at a relay with `BUZZ_B1_REAL_RELAY`. It adds
/// the one thing every fake listener above cannot: the NIP-98 header this path
/// emits is *accepted* by a real auth pipeline, on the submission and on the
/// confirmation query. The fakes admit anything, so they can prove which key
/// signed but never that the signature is one a relay would take.
///
/// The read is the point. `head_for_coordinate` signs with keys captured at
/// entry and carried across the admission wait, through the extracted
/// [`crate::relay::query_relay_at_with_keys_no_wait`] — so a live
/// `current: true` is that whole sequence working against something that
/// answers 401 when it does not.
#[tokio::test]
#[ignore = "needs a live relay: BUZZ_B1_REAL_RELAY=http://127.0.0.1:PORT"]
async fn against_a_live_relay_both_extension_data_methods_authenticate() {
    use tauri::Manager as _;
    let url = std::env::var("BUZZ_B1_REAL_RELAY").expect("BUZZ_B1_REAL_RELAY must be set");

    let _gate = crate::relay_admission::gate_guard().await;
    let _host = super::super::frame_host::lifecycle_guard().await;

    let keys = nostr::Keys::generate();
    let identity = keys.public_key().to_hex();
    // A fresh coordinate per run: the relay's replacement key is
    // (community, kind, pubkey, d), and a reused one would let an earlier
    // run's head answer this run's confirmation.
    let key = format!("live.{}", &identity[..16]);
    let coordinate = build_coordinate(EXTID, &key).expect("coordinate");
    // Printed so the transcript can match this run against the relay's own
    // request log rather than taking the test's word for which key signed.
    println!("LIVE identity={identity}");
    println!("LIVE coordinate={coordinate}");

    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("grants").join("extension-grants.db");
    {
        let conn = super::super::grants::open_grant_db(&db_path).expect("open");
        super::super::grants::grant_boolean_scope(&conn, &identity, EXTID, SCOPE_EXTENSION_DATA)
            .expect("grant");
    }

    let state = crate::app_state::build_app_state();
    *state.keys.lock().unwrap() = keys;
    *state.relay_url_override.lock().unwrap() = Some(url);

    let app = tauri::test::mock_app();
    app.manage(state);
    if let Ok(prod) = super::super::dispatch::grant_db_path(app.handle()) {
        if let Some(parent) = prod.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::copy(&db_path, &prod);
    }

    super::super::frame_host::insert_lease_for_test(LEASE, EXTID);

    let written = publish_extension_data(
        app.handle(),
        EXTID,
        LEASE,
        Some(serde_json::json!({
            "key": key,
            "content": "{\"v\":\"live\"}",
            "created_at": super::super::publish::now_unix(),
        })),
    )
    .await;
    assert!(
        written.ok,
        "the live write must be accepted: {:?}",
        written.error
    );
    let written = written.result.expect("result");
    // `current: true` means the confirmation query authenticated and the relay
    // answered with the event just submitted. A rejected NIP-98 would have
    // surfaced as a relay error instead.
    assert_eq!(
        written["current"],
        serde_json::json!(true),
        "the live read-back must confirm the submitted event"
    );
    let submitted_id = written["event"]["id"].as_str().expect("id").to_string();
    println!("LIVE submitted_id={submitted_id}");

    let read = extension_data_get(
        app.handle(),
        EXTID,
        LEASE,
        Some(serde_json::json!({ "key": key })),
    )
    .await;
    assert!(read.ok, "the live read must succeed: {:?}", read.error);
    let event = read.result.expect("result")["event"].clone();
    assert_eq!(
        event["id"].as_str(),
        Some(submitted_id.as_str()),
        "the live read must return the event this test wrote"
    );
    assert_eq!(
        event["pubkey"].as_str(),
        Some(identity.as_str()),
        "and it must be authored by the admitted identity"
    );
    // The host-derived coordinate survived the round trip, so the relay stored
    // the event under the namespace the host built rather than under anything
    // the caller could name.
    let d = event["tags"]
        .as_array()
        .expect("tags")
        .iter()
        .find(|t| t[0] == "d")
        .expect("a d tag");
    assert_eq!(d[1].as_str(), Some(coordinate.as_str()));
}
