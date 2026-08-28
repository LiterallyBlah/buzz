//! §5 read authority across the admission gate.
//!
//! Separate from [`super::query_tests`] because these prove a different thing:
//! not what the rewriting and the verifier mean, but that authority lost while
//! a read is parked in the process-global gate refuses **before any request
//! reaches the relay**. A `denied` that still queried would have leaked the
//! extension's interest — which channels and kinds it is watching — to the
//! relay on behalf of an identity that no longer authorises it.
//!
//! Every probe arms the real gate rather than injecting a refusal, because the
//! thing under test is the production transition that creates a stale
//! authority, not that some refusal suppressed a request.

use super::*;

const QEXTID: &str = "demo-read";
const QLEASE: &str = "lease-for-query-authority-tests";
const QCHAN: &str = "33333333-3333-4333-8333-333333333333";

/// A parked read: real armed gate, real grant store, real lease, and a
/// listener that counts connections so "no request" is *observed* rather than
/// asserted. The closure runs while the read sits in the gate.
async fn query_parked_at_the_gate(
    disturb: impl FnOnce(&crate::AppState, &str, &std::path::Path) + Send + 'static,
) -> (BridgeReply, usize) {
    use tauri::Manager as _;

    let keys = nostr::Keys::generate();
    let identity = keys.public_key().to_hex();
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("grants").join("extension-grants.db");
    {
        let conn = super::super::grants::open_grant_db(&db_path).expect("open");
        super::super::grants::grant_read_scope(&conn, &identity, QEXTID, 9, QCHAN).expect("grant");
    }

    // Accepts and counts, and deliberately never answers: a parked read must
    // not reach it at all, so there is nothing to reply to.
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

    super::super::frame_host::insert_lease_for_test(QLEASE, QEXTID);

    crate::relay_admission::activate_rate_limit(Some(1));
    let armed_at = std::time::Instant::now();

    let handle = app.handle().clone();
    let read = query_events(
        &handle,
        QEXTID,
        QLEASE,
        Some(serde_json::json!({ "filter": { "kinds": [9], "#h": [QCHAN] } })),
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

fn denied_reply(reply: &BridgeReply) -> bool {
    reply.error.as_ref().map(|e| e.code.as_str()) == Some(super::super::dispatch::code::DENIED)
}

/// A relay that answers one `/query` with `events_json`, running `before_reply`
/// with the request **already on the wire**.
///
/// That timing is the whole point: a disturbance here cannot be caught by any
/// pre-send check, only by the recheck that runs after the events come back and
/// have been verified. It is the §5 analogue of Boundary 1's
/// `before_head_reply` window.
///
/// The status is explicit so the failure arm can be driven — and, crucially,
/// shown to actually fail by an undisturbed control.
fn fake_query_relay_with_status(
    status: &'static str,
    events_json: String,
    before_reply: impl Fn() + Send + 'static,
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
            before_reply();
            let response = format!(
                "HTTP/1.1 {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                status,
                events_json.len(),
                events_json
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });
    format!("http://{addr}")
}

/// Run a read against a relay that answers with one valid, granted event and
/// runs `before_reply` while the response is in flight.
async fn query_against_answering_relay(before_reply: impl Fn() + Send + 'static) -> BridgeReply {
    query_against_relay("200 OK", before_reply).await
}

async fn query_against_relay(
    status: &'static str,
    before_reply: impl Fn() + Send + 'static,
) -> BridgeReply {
    use tauri::Manager as _;

    let keys = nostr::Keys::generate();
    let identity = keys.public_key().to_hex();
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("grants").join("extension-grants.db");
    {
        let conn = super::super::grants::open_grant_db(&db_path).expect("open");
        super::super::grants::grant_read_scope(&conn, &identity, QEXTID, 9, QCHAN).expect("grant");
    }

    // A genuinely valid event in the granted channel: the verifier must have
    // nothing to object to, so that only the authority recheck can refuse.
    let event = nostr::EventBuilder::new(nostr::Kind::from(9u16), "{}")
        .tag(nostr::Tag::parse(vec!["h".to_string(), QCHAN.to_string()]).expect("tag"))
        .sign_with_keys(&keys)
        .expect("sign");
    use nostr::JsonUtil as _;
    let url = fake_query_relay_with_status(status, format!("[{}]", event.as_json()), before_reply);

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
    super::super::frame_host::insert_lease_for_test(QLEASE, QEXTID);

    query_events(
        app.handle(),
        QEXTID,
        QLEASE,
        Some(serde_json::json!({ "filter": { "kinds": [9], "#h": [QCHAN] } })),
    )
    .await
}

#[tokio::test]
async fn a_read_whose_authority_dies_after_the_response_denies_and_exposes_nothing() {
    let _host = super::super::frame_host::lifecycle_guard().await;
    // Revoked with the response already on the wire. No pre-send check can see
    // this; only the recheck after verification and immediately before exposure
    // can, which is the clause Boundary 1 established and this increment
    // inherits.
    let reply = query_against_answering_relay(|| {
        super::super::frame_host::release(QLEASE);
    })
    .await;
    assert!(
        denied_reply(&reply),
        "authority lost after the response must refuse: {:?}",
        reply
    );
    assert!(
        reply.result.is_none(),
        "a refusal must expose no events: {:?}",
        reply.result
    );
}

#[tokio::test]
async fn the_same_relay_undisturbed_returns_the_event() {
    // THE CONTROL. Without it, the probe above passes on any refusal at all —
    // including one where the relay arm never actually delivered a usable
    // event — and would be proving the fixture rather than the recheck.
    let _host = super::super::frame_host::lifecycle_guard().await;
    super::super::frame_host::insert_lease_for_test(QLEASE, QEXTID);
    let reply = query_against_answering_relay(|| {}).await;
    assert!(
        reply.error.is_none(),
        "the undisturbed arm must succeed: {:?}",
        reply.error
    );
    let events = reply
        .result
        .as_ref()
        .and_then(|r| r.get("events"))
        .and_then(|e| e.as_array());
    assert_eq!(
        events.map(|e| e.len()),
        Some(1),
        "the undisturbed arm must actually deliver the event: {:?}",
        reply.result
    );
}

#[tokio::test]
async fn a_read_grant_revoked_during_the_gate_wait_denies_with_no_query() {
    let _gate = crate::relay_admission::gate_guard().await;
    let _host = super::super::frame_host::lifecycle_guard().await;
    let (reply, connections) = query_parked_at_the_gate(|_state, identity, db| {
        let conn = super::super::grants::open_grant_db(db).expect("open");
        super::super::grants::revoke_all(&conn, identity, QEXTID).expect("revoke");
    })
    .await;
    assert!(
        denied_reply(&reply),
        "revocation must refuse: {:?}",
        reply.error
    );
    assert_eq!(connections, 0, "no query may reach the relay");
}

#[tokio::test]
async fn an_identity_change_during_the_gate_wait_denies_with_no_query() {
    let _gate = crate::relay_admission::gate_guard().await;
    let _host = super::super::frame_host::lifecycle_guard().await;
    // Production's recovery boot, in production's order: an **ephemeral key**
    // lands in `state.keys` and only then does the flag store with Release. The
    // key swap is the adversarial half — with the flag alone, `state.keys`
    // still holds the admitted identity, so an implementation that re-read
    // `state` after the wait would sign correctly anyway and a zero-connection
    // result would not distinguish a fix from the defect.
    let (reply, connections) = query_parked_at_the_gate(|state, _, _| {
        *state.keys.lock().unwrap() = nostr::Keys::generate();
        state
            .identity_lost
            .store(true, std::sync::atomic::Ordering::Release);
    })
    .await;
    assert!(
        denied_reply(&reply),
        "recovery must refuse: {:?}",
        reply.error
    );
    assert_eq!(connections, 0, "no query may reach the relay");
}

#[tokio::test]
async fn a_locked_keyring_during_the_gate_wait_denies_with_no_query() {
    let _gate = crate::relay_admission::gate_guard().await;
    let _host = super::super::frame_host::lifecycle_guard().await;
    let (reply, connections) = query_parked_at_the_gate(|state, _, _| {
        *state.keys.lock().unwrap() = nostr::Keys::generate();
        state
            .keyring_locked
            .store(true, std::sync::atomic::Ordering::Release);
    })
    .await;
    assert!(
        denied_reply(&reply),
        "a locked keyring must refuse: {:?}",
        reply.error
    );
    assert_eq!(connections, 0, "no query may reach the relay");
}

#[tokio::test]
async fn a_lease_released_during_the_gate_wait_denies_with_no_query() {
    let _gate = crate::relay_admission::gate_guard().await;
    let _host = super::super::frame_host::lifecycle_guard().await;
    // Production's own teardown, not a test-only unregister: `release` is what
    // the frame host calls when the port dies, so this is the transition that
    // actually happens rather than a stand-in for it.
    let (reply, connections) = query_parked_at_the_gate(|_state, _, _| {
        super::super::frame_host::release(QLEASE);
    })
    .await;
    assert!(
        denied_reply(&reply),
        "a released lease must refuse: {:?}",
        reply.error
    );
    assert_eq!(connections, 0, "no query may reach the relay");
}

#[tokio::test]
async fn a_relay_failure_with_authority_lost_in_the_same_window_denies_not_relay_error() {
    // Boundary 1's blocker, carried into §5: an I/O or status failure and a
    // lost lease can land in the same window, and classifying the failure
    // first would hand `relay_error` to a caller who is no longer entitled to
    // ask. Authority outranks the relay's answer.
    let _host = super::super::frame_host::lifecycle_guard().await;
    let reply = query_against_relay("500 Internal Server Error", || {
        super::super::frame_host::release(QLEASE);
    })
    .await;
    assert!(
        denied_reply(&reply),
        "authority loss must outrank the relay failure: {:?}",
        reply.error
    );
}

#[tokio::test]
async fn a_relay_failure_alone_is_still_a_relay_error() {
    // THE CONTROL for the row above, and the one that makes it mean anything.
    // Without it, a probe that releases the lease *and* fails the relay is
    // indistinguishable from a probe that only released the lease — the
    // `denied` could come from the lease alone. This shows the failure arm
    // genuinely fails, so a `denied` from the paired probe is precedence.
    let _host = super::super::frame_host::lifecycle_guard().await;
    super::super::frame_host::insert_lease_for_test(QLEASE, QEXTID);
    let reply = query_against_relay("500 Internal Server Error", || {}).await;
    assert_eq!(
        reply.error.as_ref().map(|e| e.code.as_str()),
        Some(super::super::dispatch::code::RELAY_ERROR),
        "authority intact plus a failed relay is still relay_error: {:?}",
        reply.error
    );
}
