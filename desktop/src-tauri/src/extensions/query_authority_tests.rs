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
            // An empty status is the marker for "answer without a
            // `Content-Length`", so the body ends at EOF. That is the shape
            // that makes the streaming byte ceiling the *only* defence: with a
            // declared length the early header check refuses first, and a probe
            // against it proves nothing about the accumulation loop.
            let response = if status.is_empty() {
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{}",
                    events_json
                )
            } else {
                format!(
                    "HTTP/1.1 {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    status,
                    events_json.len(),
                    events_json
                )
            };
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

#[tokio::test]
async fn an_oversized_response_without_a_content_length_is_still_refused() {
    // The isolating probe for the streaming ceiling. A relay that declares no
    // `Content-Length` gets past the header check entirely, so the only thing
    // that can stop an unbounded body is the accumulation loop aborting as it
    // reads. Without this, deleting that loop leaves every test green — the
    // header check answers for it.
    let _host = super::super::frame_host::lifecycle_guard().await;
    super::super::frame_host::insert_lease_for_test(QLEASE, QEXTID);

    // **The body must be one the host would otherwise accept.** Junk filler
    // does not isolate this: with the ceiling deleted the deserialiser fails on
    // it and returns the same `relay_error`, so the probe passes either way.
    // This is a single genuinely signed, granted, verifiable event whose
    // content pushes it past the ceiling — so without the ceiling the read
    // *succeeds* and returns it, and the assertion below can actually fail.
    let keys = nostr::Keys::generate();
    let huge = nostr::EventBuilder::new(nostr::Kind::from(9u16), "x".repeat(9 * 1024 * 1024))
        .tag(nostr::Tag::parse(vec!["h".to_string(), QCHAN.to_string()]).expect("tag"))
        .sign_with_keys(&keys)
        .expect("sign");
    use nostr::JsonUtil as _;
    let body = format!("[{}]", huge.as_json());
    assert!(
        body.len() > 8 * 1024 * 1024,
        "the fixture must exceed the ceiling it probes"
    );

    let reply = query_against_relay_body("", Some(body), || {}).await;
    assert!(
        relay_error(&reply),
        "an unbounded chunked body must be refused: {:?}",
        reply
    );
    assert!(reply.result.is_none(), "and expose nothing");
}

// ── the seven-step live proof ──────────────────────────────────────────────
//
// Everything above this line is state without a socket. This is the opposite:
// a real relay with real Postgres and Redis behind it, a real NIP-42 handshake,
// real `REQ`/`EVENT`/`EOSE` frames, and the production sink — `app.emit` into a
// real Tauri listener, not an injected fake. A fake sink here would prove the
// aggregate works and say nothing about whether it is wired to anything.
//
// It publishes, so it runs against a relay this test owns. Pointing it at
// shared infrastructure would seed channels and messages into somebody else's
// database.

use tauri::{Listener as _, Manager as _};

const LEXTID: &str = "live-sub-probe";
const LLEASE: &str = "8b7c6d5e-4f3a-42b1-9c8d-7e6f5a4b3c2d";

/// Create a channel on the relay under test, and return its uuid.
///
/// Kind 9007 with `h`/`name`/`channel_type`/`visibility` is the relay's
/// create-channel event; the signer becomes a member, which is what makes
/// the kind-9 publishes below admissible.
async fn create_channel(http: &str, keys: &nostr::Keys) -> String {
    let channel = uuid::Uuid::new_v4().to_string();
    let event = nostr::EventBuilder::new(nostr::Kind::Custom(9007), "")
        .tags(vec![
            nostr::Tag::parse(["h", &channel]).unwrap(),
            nostr::Tag::parse(["name", &format!("5b-live-{channel}")]).unwrap(),
            nostr::Tag::parse(["channel_type", "stream"]).unwrap(),
            nostr::Tag::parse(["visibility", "open"]).unwrap(),
        ])
        .sign_with_keys(keys)
        .unwrap();
    post_event(http, keys, &event).await;
    channel
}

/// Publish a kind-9 message into a channel and return its event id.
async fn publish(http: &str, keys: &nostr::Keys, channel: &str, body: &str) -> String {
    let event = nostr::EventBuilder::new(nostr::Kind::from(9u16), body)
        .tags(vec![nostr::Tag::parse(["h", channel]).unwrap()])
        .sign_with_keys(keys)
        .unwrap();
    let id = event.id.to_hex();
    post_event(http, keys, &event).await;
    id
}

async fn post_event(http: &str, keys: &nostr::Keys, event: &nostr::Event) {
    use nostr::JsonUtil as _;
    let resp = reqwest::Client::new()
        .post(format!("{http}/events"))
        .header("X-Pubkey", keys.public_key().to_hex())
        .header("Content-Type", "application/json")
        .body(event.as_json())
        .send()
        .await
        .expect("submit event");
    assert!(
        resp.status().is_success(),
        "publish failed: {} {}",
        resp.status(),
        resp.text().await.unwrap_or_default()
    );
}

/// Frames the production sink delivered, in arrival order.
type Seen = std::sync::Arc<std::sync::Mutex<Vec<serde_json::Value>>>;

/// Wait for the sink to have delivered at least `want` frames.
///
/// `tokio::time::sleep`, not `std::thread::sleep`: the reader task shares this
/// runtime, so blocking the thread here would stop the very task the test is
/// waiting on and every run would time out.
async fn wait_for(seen: &Seen, want: usize, what: &str) -> Vec<serde_json::Value> {
    let started = std::time::Instant::now();
    loop {
        let got = seen.lock().unwrap().clone();
        if got.len() >= want {
            return got;
        }
        assert!(
            started.elapsed() < std::time::Duration::from_secs(25),
            "timed out waiting for {what}: have {} of {want}: {got:?}",
            got.len()
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

fn kinds_of(frames: &[serde_json::Value]) -> Vec<String> {
    frames
        .iter()
        .map(|f| f["frame"]["kind"].as_str().unwrap_or("?").to_string())
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs a live relay: BUZZ_5B_REAL_RELAY=ws://127.0.0.1:PORT"]
async fn against_a_live_relay_a_subscription_streams_two_channels_as_one() {
    let ws = std::env::var("BUZZ_5B_REAL_RELAY").expect("BUZZ_5B_REAL_RELAY must be set");
    let http = crate::relay::relay_http_base_url(&ws);

    let _gate = crate::relay_admission::gate_guard().await;
    let _host = super::super::frame_host::lifecycle_guard().await;

    // The identity is generated here and the relay is this test's own, so
    // nothing depends on ambient credentials and nothing lands in shared
    // infrastructure.
    let keys = nostr::Keys::generate();
    let identity = keys.public_key().to_hex();
    println!("LIVE identity={identity}");
    println!("LIVE relay_ws={ws}");
    println!("LIVE relay_http={http}");

    // ── step 1: two granted channels ───────────────────────────────────
    let chan_a = create_channel(&http, &keys).await;
    let chan_b = create_channel(&http, &keys).await;
    println!("LIVE channel_a={chan_a}");
    println!("LIVE channel_b={chan_b}");

    // ── step 2: stored history in both, before anyone subscribes ───────
    let stored_a = publish(&http, &keys, &chan_a, "stored-a").await;
    let stored_b = publish(&http, &keys, &chan_b, "stored-b").await;
    println!("LIVE stored_a={stored_a}");
    println!("LIVE stored_b={stored_b}");

    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("grants").join("extension-grants.db");
    {
        let conn = super::super::grants::open_grant_db(&db_path).expect("open");
        super::super::grants::grant_read_scope(&conn, &identity, LEXTID, 9, &chan_a)
            .expect("grant a");
        super::super::grants::grant_read_scope(&conn, &identity, LEXTID, 9, &chan_b)
            .expect("grant b");
    }

    let state = crate::app_state::build_app_state();
    *state.keys.lock().unwrap() = keys.clone();
    *state.relay_url_override.lock().unwrap() = Some(ws.clone());

    let app = tauri::test::mock_app();
    app.manage(state);
    if let Ok(prod) = super::super::dispatch::grant_db_path(app.handle()) {
        if let Some(parent) = prod.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::copy(&db_path, &prod);
    }
    super::super::frame_host::insert_lease_for_test(LLEASE, LEXTID);

    // The PRODUCTION sink: `subscribe` emits on the real Tauri bus and this
    // listens on it. Injecting a sink would have skipped the seam that
    // actually carries frames to a port.
    let seen: Seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    {
        let seen = std::sync::Arc::clone(&seen);
        app.handle().listen("extension-stream", move |event| {
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(event.payload()) {
                seen.lock().unwrap().push(value);
            }
        });
    }

    // ── step 3: subscribe ──────────────────────────────────────────────
    let reply = super::super::query::subscribe(
        app.handle(),
        LEXTID,
        LLEASE,
        Some(serde_json::json!({ "filter": { "kinds": [9] } })),
    )
    .await;
    println!("LIVE subscribe_error={:?}", reply.error);
    assert!(
        reply.error.is_none(),
        "subscribe must succeed: {:?}",
        reply.error
    );
    let sub = reply.result.as_ref().unwrap()["sub"]
        .as_str()
        .unwrap()
        .to_string();
    println!("LIVE sub={sub}");

    // ── steps 4+5: both stored events, then EXACTLY ONE eose ───────────
    let frames = wait_for(&seen, 3, "two stored events and one eose").await;
    println!("LIVE frames_after_eose={:?}", kinds_of(&frames));
    let eose_count = frames
        .iter()
        .filter(|f| f["frame"]["kind"] == "eose")
        .count();
    assert_eq!(
        eose_count,
        1,
        "the aggregate emits exactly one eose across both branches: {:?}",
        kinds_of(&frames)
    );
    let stored_ids: Vec<String> = frames
        .iter()
        .filter(|f| f["frame"]["kind"] == "event")
        .map(|f| f["frame"]["event"]["id"].as_str().unwrap().to_string())
        .collect();
    assert!(
        stored_ids.contains(&stored_a) && stored_ids.contains(&stored_b),
        "both channels' stored history must arrive: {stored_ids:?}"
    );
    // The eose is last: stored events precede it, which is the ordering the
    // whole aggregate exists to produce.
    assert_eq!(
        frames.last().unwrap()["frame"]["kind"],
        "eose",
        "the eose terminates the stored phase"
    );
    for frame in &frames {
        assert_eq!(frame["lease"], LLEASE, "every frame names the owning lease");
        assert_eq!(frame["frame"]["sub"], sub, "and the sub it belongs to");
    }

    // ── step 6: live events in both channels arrive after the eose ─────
    let before_live = seen.lock().unwrap().len();
    let live_a = publish(&http, &keys, &chan_a, "live-a").await;
    let live_b = publish(&http, &keys, &chan_b, "live-b").await;
    println!("LIVE live_a={live_a}");
    println!("LIVE live_b={live_b}");
    let frames = wait_for(&seen, before_live + 2, "two live events").await;
    let live_ids: Vec<String> = frames[before_live..]
        .iter()
        .filter(|f| f["frame"]["kind"] == "event")
        .map(|f| f["frame"]["event"]["id"].as_str().unwrap().to_string())
        .collect();
    assert!(
        live_ids.contains(&live_a) && live_ids.contains(&live_b),
        "both channels must stream live after the eose: {live_ids:?}"
    );
    assert_eq!(
        frames
            .iter()
            .filter(|f| f["frame"]["kind"] == "eose")
            .count(),
        1,
        "and still exactly one eose — the live phase does not produce another"
    );

    // ── step 7: teardown stops the stream ──────────────────────────────
    let reply = super::super::query::unsubscribe(LLEASE, Some(serde_json::json!({ "sub": sub })));
    assert!(reply.error.is_none(), "unsubscribe: {:?}", reply.error);
    let after_unsub = seen.lock().unwrap().len();

    let orphan = publish(&http, &keys, &chan_a, "after-unsubscribe").await;
    println!("LIVE orphan={orphan}");
    // Long enough that the live events above would have arrived twice over.
    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    let final_frames = seen.lock().unwrap().clone();
    let delivered: Vec<String> = final_frames[after_unsub..]
        .iter()
        .filter(|f| f["frame"]["kind"] == "event")
        .map(|f| f["frame"]["event"]["id"].as_str().unwrap().to_string())
        .collect();
    assert!(
        !delivered.contains(&orphan),
        "nothing may be delivered after unsubscribe: {delivered:?}"
    );
    println!("LIVE frames_total={}", final_frames.len());
    println!("LIVE PROOF COMPLETE");
}
