//! Disposable real-infrastructure proof for the extension subscription path.

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
    let body = event.as_json();
    // Wired rather than exempted. The repo's events-URL inventory would have
    // accepted a "test-only fixture, no guard" row here, and the precedent for
    // one exists — but this probe signs and posts real events, and a fixture
    // that grows a key-bearing body later would then leak in silence. One line
    // makes that unrepresentable instead of merely unlikely.
    crate::egress_guard::assert_no_key_backup(&body, "5b live proof publish")
        .expect("the live proof must never post key material");
    let resp = reqwest::Client::new()
        .post(format!("{http}/events"))
        .header("X-Pubkey", keys.public_key().to_hex())
        .header("Content-Type", "application/json")
        .body(body)
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
    super::super::frame_host::shutdown_now();
    let frame_lease = super::super::frame_host::acquire(dir.path().to_path_buf(), LEXTID)
        .await
        .expect("start a real frame host lease");
    let live_lease = frame_lease.lease.clone();
    println!(
        "LIVE frame_lease={} extension_port={} wrapper_port={}",
        live_lease, frame_lease.extension_port, frame_lease.wrapper_port
    );

    // The PRODUCTION sink: `subscribe` emits on the real Tauri bus and this
    // listens on it. Injecting a sink would have skipped the seam that
    // actually carries frames to a port.
    let seen: Seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let acknowledge = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    {
        let seen = std::sync::Arc::clone(&seen);
        let acknowledge = std::sync::Arc::clone(&acknowledge);
        let app_handle = app.handle().clone();
        app.handle().listen("extension-stream", move |event| {
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(event.payload()) {
                if let Some(frames) = value["frames"].as_array() {
                    let mut sink = seen.lock().unwrap();
                    for frame in frames {
                        sink.push(serde_json::json!({
                            "lease": value["generation"],
                            "frame": frame
                        }));
                    }
                }
                if value["terminal"] != true
                    && acknowledge.load(std::sync::atomic::Ordering::SeqCst)
                {
                    super::super::query::acknowledge_subscription_batch(
                        &app_handle,
                        value["generation"].as_str().unwrap_or_default(),
                        value["sub"].as_str().unwrap_or_default(),
                        value["seq"].as_u64().unwrap_or_default(),
                        value["token"].as_str().unwrap_or_default(),
                        value["frameCount"].as_u64().unwrap_or_default() as usize,
                        value["encodedBytes"].as_u64().unwrap_or_default() as usize,
                    );
                }
            }
        });
    }

    // ── step 3: subscribe ──────────────────────────────────────────────
    let reply = super::super::query::subscribe(
        app.handle(),
        LEXTID,
        &live_lease,
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
    // The real frontend writes the correlated reply first, then sends this
    // exact-generation internal receipt. Nothing above may have emitted yet.
    assert!(seen.lock().unwrap().is_empty());
    super::super::query::activate_subscription(app.handle(), &live_lease, &sub);

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
        assert_eq!(
            frame["lease"], live_lease,
            "every frame names the owning lease"
        );
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
    let reply =
        super::super::query::unsubscribe(&live_lease, Some(serde_json::json!({ "sub": sub })));
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
    println!("LIVE frames_total_after_unsubscribe={}", final_frames.len());

    // ── step 8: a genuinely stalled browser consumer is bounded ─────────
    acknowledge.store(false, std::sync::atomic::Ordering::SeqCst);
    let stalled_reply = super::super::query::subscribe(
        app.handle(),
        LEXTID,
        &live_lease,
        Some(serde_json::json!({ "filter": { "kinds": [9], "#h": [chan_a] } })),
    )
    .await;
    assert!(
        stalled_reply.error.is_none(),
        "stalled subscription must open: {:?}",
        stalled_reply.error
    );
    let stalled_sub = stalled_reply.result.as_ref().unwrap()["sub"]
        .as_str()
        .unwrap()
        .to_string();
    super::super::query::activate_subscription(app.handle(), &live_lease, &stalled_sub);

    let large_body = "x".repeat(320 * 1024);
    for index in 0..31 {
        publish(
            &http,
            &keys,
            &chan_a,
            &format!("stalled-{index}-{large_body}"),
        )
        .await;
    }
    let stalled_deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    let stalled_closed = loop {
        let found = seen.lock().unwrap().iter().find_map(|value| {
            let frame = &value["frame"];
            (frame["sub"] == stalled_sub && frame["kind"] == "closed").then(|| frame.clone())
        });
        if let Some(frame) = found {
            break frame;
        }
        assert!(
            std::time::Instant::now() < stalled_deadline,
            "a stalled consumer must reach a bounded terminal close"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    };
    assert_eq!(
        stalled_closed["reason"], "bound_exceeded",
        "the Rust-to-port queue closes rather than growing without bound"
    );
    println!("LIVE stalled_sub={stalled_sub} closed=bound_exceeded");

    // ── step 9: real frame-lease release closes an active real socket ───
    acknowledge.store(true, std::sync::atomic::Ordering::SeqCst);
    let release_reply = super::super::query::subscribe(
        app.handle(),
        LEXTID,
        &live_lease,
        Some(serde_json::json!({ "filter": { "kinds": [9], "#h": [chan_b] } })),
    )
    .await;
    assert!(
        release_reply.error.is_none(),
        "release subscription must open: {:?}",
        release_reply.error
    );
    let release_sub = release_reply.result.as_ref().unwrap()["sub"]
        .as_str()
        .unwrap()
        .to_string();
    super::super::query::activate_subscription(app.handle(), &live_lease, &release_sub);
    let release_start = seen.lock().unwrap().len();
    super::super::frame_host::release(&live_lease);
    assert!(
        super::super::frame_host::extension_for_lease(&live_lease).is_none(),
        "the exact lease must be gone"
    );
    assert!(
        super::super::frame_host::running_port().is_none(),
        "the final frame lease must stop both host listeners"
    );

    let release_deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let closed = seen.lock().unwrap()[release_start..]
            .iter()
            .filter(|value| {
                value["frame"]["sub"] == release_sub && value["frame"]["kind"] == "closed"
            })
            .count();
        if closed == 1 {
            break;
        }
        assert!(
            std::time::Instant::now() < release_deadline,
            "frame lease release must emit exactly one terminal close"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    let after_release = seen.lock().unwrap().len();
    let after_release_event = publish(&http, &keys, &chan_b, "after-frame-release").await;
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    let post_release = seen.lock().unwrap().clone();
    assert!(
        !post_release[after_release..].iter().any(|value| {
            value["frame"]["kind"] == "event"
                && value["frame"]["event"]["id"] == after_release_event
        }),
        "the released frame must not receive later relay traffic"
    );
    println!("LIVE release_sub={release_sub} closed_exactly_once=true");
    println!("LIVE frames_total={}", post_release.len());
    println!("LIVE PROOF COMPLETE");
}
