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
/// A second channel for the live cross-product probe. It never needs to exist
/// on the relay: construction must refuse before any request goes out.
const CROSS_CHANNEL: &str = "44444444-4444-4444-8444-444444444444";

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
    query_against_relay_body(status, None, before_reply).await
}

/// As [`query_against_relay`], but the caller supplies the exact response body.
///
/// Needed for the response-bound probes: the point of those is a body the host
/// did not ask for and would not build, so it cannot be expressed by varying
/// the one valid event this harness signs.
async fn query_against_relay_body(
    status: &'static str,
    body_override: Option<String>,
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
    let body = body_override.unwrap_or_else(|| format!("[{}]", event.as_json()));
    let url = fake_query_relay_with_status(status, body, before_reply);

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

/// The whole §5 read path against a relay that answers 401 when authentication
/// is wrong, with real Postgres and Redis behind it.
///
/// The read is the point: a constructed filter, signed with the keys the grant
/// admitted, carried through the `_no_wait` seam, verified per event, and
/// rechecked before exposure. A live event coming back is that whole sequence
/// working end to end rather than against a fake that agrees with us.
#[tokio::test]
#[ignore = "needs a live relay: BUZZ_5A_REAL_RELAY=http://127.0.0.1:PORT"]
async fn against_a_live_relay_the_read_path_authenticates_and_returns_real_events() {
    use tauri::Manager as _;
    let url = std::env::var("BUZZ_5A_REAL_RELAY").expect("BUZZ_5A_REAL_RELAY must be set");

    let _gate = crate::relay_admission::gate_guard().await;
    let _host = super::super::frame_host::lifecycle_guard().await;

    // **Read-only, and the identity comes from outside.**
    //
    // A freshly generated key cannot seed a channel: the relay enforces
    // membership on ingest (`restricted: not a channel member`), and creating a
    // channel so a test can read it would be a write to shared infrastructure
    // for the test's own convenience. So the identity and the channel are
    // supplied, and this probe **never publishes anything** — every request it
    // makes is a read, which is also why it is safe to point at real infra.
    let secret = std::env::var("BUZZ_5A_READ_KEY").expect("BUZZ_5A_READ_KEY must be set");
    let channel = std::env::var("BUZZ_5A_READ_CHANNEL").expect("BUZZ_5A_READ_CHANNEL must be set");
    let keys = nostr::Keys::parse(&secret).expect("BUZZ_5A_READ_KEY must be a nostr secret key");
    let identity = keys.public_key().to_hex();
    assert!(
        super::super::manifest::is_canonical_channel_uuid(&channel),
        "the probe's channel must be canonical: {channel}"
    );
    println!("LIVE identity={identity}");
    println!("LIVE channel={channel}");

    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("grants").join("extension-grants.db");
    {
        let conn = super::super::grants::open_grant_db(&db_path).expect("open");
        super::super::grants::grant_read_scope(&conn, &identity, QEXTID, 9, &channel)
            .expect("grant");
        // 45001 granted in a *different* channel, so the probe below is a real
        // cross-product: the kind is granted somewhere, just not here. Granting
        // it nowhere would only have proved an ordinary ungranted-kind denial.
        // Channel B needs no real existence — construction must refuse before
        // any relay traffic, which is the property under test.
        super::super::grants::grant_read_scope(&conn, &identity, QEXTID, 45001, CROSS_CHANNEL)
            .expect("grant");
    }

    let state = crate::app_state::build_app_state();
    *state.keys.lock().unwrap() = keys.clone();
    *state.relay_url_override.lock().unwrap() = Some(url.clone());

    let app = tauri::test::mock_app();
    app.manage(state);
    if let Ok(prod) = super::super::dispatch::grant_db_path(app.handle()) {
        if let Some(parent) = prod.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::copy(&db_path, &prod);
    }
    super::super::frame_host::insert_lease_for_test(QLEASE, QEXTID);

    // 1. The granted read returns real, stored events, and every one of them
    //    satisfies the constraints the host constructed — checked here rather
    //    than trusted, because the relay is the untrusted party.
    let reply = query_events(
        app.handle(),
        QEXTID,
        QLEASE,
        Some(serde_json::json!({ "filter": { "kinds": [9], "#h": [channel], "limit": 5 } })),
    )
    .await;
    println!("LIVE granted_read_error={:?}", reply.error);
    assert!(
        reply.error.is_none(),
        "granted read must succeed: {:?}",
        reply.error
    );
    let events = reply
        .result
        .as_ref()
        .and_then(|r| r.get("events"))
        .and_then(|e| e.as_array())
        .expect("events array");
    println!("LIVE returned_events={}", events.len());
    assert!(
        !events.is_empty(),
        "the granted channel must return real stored events"
    );
    assert!(events.len() <= 5, "the overall cap must be honoured");
    for event in events {
        assert_eq!(event["kind"].as_u64(), Some(9), "only the granted kind");
        let hs: Vec<&str> = event["tags"]
            .as_array()
            .expect("tags")
            .iter()
            .filter(|t| t[0].as_str() == Some("h"))
            .filter_map(|t| t[1].as_str())
            .collect();
        assert_eq!(
            hs,
            vec![channel.as_str()],
            "every event must carry exactly the granted channel"
        );
    }
    println!("LIVE first_event_id={}", events[0]["id"]);

    // 2. A genuine cross product against the real relay: 45001 IS granted, in
    //    CROSS_CHANNEL. Asking for it in *this* channel names a pair nobody
    //    granted, and construction must refuse before any request goes out.
    let cross = query_events(
        app.handle(),
        QEXTID,
        QLEASE,
        Some(serde_json::json!({ "filter": { "kinds": [45001], "#h": [channel] } })),
    )
    .await;
    println!("LIVE cross_product={cross:?}");
    assert!(
        denied_reply(&cross),
        "cross product must be denied: {cross:?}"
    );

    // 3. A **non-readable-kind construction refusal** — labelled honestly.
    //    This is not the stray-`h` verifier path: no stray-`h` event is placed
    //    or retrieved here. Kind 1 is simply not channel-readable, so the
    //    request dies at construction. The verifier clause that drops a real
    //    stray-`h` event is proven by the signed unit probe in `query_tests`.
    let stray = query_events(
        app.handle(),
        QEXTID,
        QLEASE,
        Some(serde_json::json!({ "filter": { "kinds": [1], "#h": [channel] } })),
    )
    .await;
    println!("LIVE non_readable_kind_refusal={stray:?}");
    assert!(
        denied_reply(&stray),
        "a non-readable kind must be denied: {stray:?}"
    );

    // 4. Authority revoked: denied, and nothing is exposed.
    super::super::frame_host::release(QLEASE);
    let revoked = query_events(
        app.handle(),
        QEXTID,
        QLEASE,
        Some(serde_json::json!({ "filter": { "kinds": [9], "#h": [channel] } })),
    )
    .await;
    println!("LIVE after_release={revoked:?}");
    assert!(
        denied_reply(&revoked),
        "a released lease must deny: {revoked:?}"
    );
    assert!(revoked.result.is_none(), "a refusal must expose nothing");
}

// ── the response is bounded, not just the request ──────────────────────────
//
// `MAX_FETCHED_CANDIDATES` bounds the work the relay was *asked* for. These
// bound what the host is willing to receive, which is a different question:
// the relay is untrusted and under no obligation to honour a limit it was sent.

fn relay_error(reply: &BridgeReply) -> bool {
    reply.error.as_ref().map(|e| e.code.as_str()) == Some(super::super::dispatch::code::RELAY_ERROR)
}

#[tokio::test]
async fn an_oversized_response_body_is_refused_before_it_is_parsed() {
    // A body past the byte ceiling. Previously the whole thing was downloaded
    // and deserialised before any cap could apply — a cap on the wrong side of
    // the allocation.
    // The lease map is process-global and a sibling test releases QLEASE, so
    // take the lifecycle guard and re-register — otherwise this races into a
    // `denied` and would "pass" for the wrong reason on the refusal probes.
    let _host = super::super::frame_host::lifecycle_guard().await;
    super::super::frame_host::insert_lease_for_test(QLEASE, QEXTID);
    let filler = "x".repeat(9 * 1024 * 1024);
    let body = format!("[{{\"junk\":\"{filler}\"}}]");
    let reply = query_against_relay_body("200 OK", Some(body), || {}).await;
    assert!(
        relay_error(&reply),
        "an oversized body must be refused: {:?}",
        reply.error
    );
    assert!(reply.result.is_none(), "and expose nothing");
}

#[tokio::test]
async fn more_events_than_were_asked_for_is_refused_not_silently_trimmed() {
    // A relay returning more than the emitted limits is misbehaving. Keeping
    // the first N and carrying on hides that, and the events kept are the ones
    // the relay chose to put first.
    let _host = super::super::frame_host::lifecycle_guard().await;
    super::super::frame_host::insert_lease_for_test(QLEASE, QEXTID);
    let keys = nostr::Keys::generate();
    let event = nostr::EventBuilder::new(nostr::Kind::from(9u16), "{}")
        .tag(nostr::Tag::parse(vec!["h".to_string(), QCHAN.to_string()]).expect("tag"))
        .sign_with_keys(&keys)
        .expect("sign");
    use nostr::JsonUtil as _;
    let one = event.as_json();
    let body = format!("[{}]", vec![one; 4097].join(","));
    let reply = query_against_relay_body("200 OK", Some(body), || {}).await;
    assert!(
        relay_error(&reply),
        "an over-count array must be refused: {:?}",
        reply.error
    );
}

#[tokio::test]
async fn a_response_inside_both_bounds_is_accepted() {
    // THE POSITIVE CONTROL for the two refusals above: the bounds must not have
    // become "refuse every response". This is the same harness, one valid
    // event, well inside both ceilings.
    let _host = super::super::frame_host::lifecycle_guard().await;
    super::super::frame_host::insert_lease_for_test(QLEASE, QEXTID);
    let reply = query_against_relay_body("200 OK", None, || {}).await;
    assert!(
        reply.error.is_none(),
        "a well-formed bounded response must be accepted: {:?}",
        reply.error
    );
}
