use std::sync::{Arc, Mutex};

use super::*;
use tauri::Listener as _;

const EXTENSION_ID: &str = "hello-world-p5";

type Seen = Arc<Mutex<Vec<serde_json::Value>>>;

async fn post_event(http: &str, keys: &nostr::Keys, event: &nostr::Event) {
    use nostr::JsonUtil as _;
    let response = reqwest::Client::new()
        .post(format!("{http}/{}", "events"))
        .header("X-Pubkey", keys.public_key().to_hex())
        .header("Content-Type", "application/json")
        .body(event.as_json())
        .send()
        .await
        .expect("post");
    assert!(response.status().is_success(), "{}", response.status());
}

async fn create_channel(http: &str, keys: &nostr::Keys) -> String {
    let channel = uuid::Uuid::new_v4().to_string();
    let event = nostr::EventBuilder::new(nostr::Kind::Custom(9007), "")
        .tags(vec![
            nostr::Tag::parse(["h", &channel]).expect("h"),
            nostr::Tag::parse(["name", "P5 live proof"]).expect("name"),
            nostr::Tag::parse(["channel_type", "stream"]).expect("type"),
            nostr::Tag::parse(["visibility", "open"]).expect("visibility"),
        ])
        .sign_with_keys(keys)
        .expect("sign");
    post_event(http, keys, &event).await;
    channel
}

async fn publish_message(http: &str, keys: &nostr::Keys, channel: &str, body: &str) -> String {
    let event = nostr::EventBuilder::new(nostr::Kind::from(9u16), body)
        .tags(vec![nostr::Tag::parse(["h", channel]).expect("h")])
        .sign_with_keys(keys)
        .expect("sign");
    let id = event.id.to_hex();
    post_event(http, keys, &event).await;
    id
}

async fn wait_for(seen: &Seen, predicate: impl Fn(&[serde_json::Value]) -> bool, what: &str) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        let snapshot = seen.lock().expect("seen").clone();
        if predicate(&snapshot) {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out: {what}: {snapshot:?}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs disposable relay: BUZZ_M1_REAL_RELAY=ws://127.0.0.1:PORT"]
async fn hello_world_production_flow_uses_only_digest_bound_bridge_authority() {
    let ws = std::env::var("BUZZ_M1_REAL_RELAY").expect("relay");
    let http = crate::relay::relay_http_base_url(&ws);
    let _gate = crate::relay_admission::gate_guard().await;
    let _host = super::super::frame_host::lifecycle_guard().await;

    let keys = nostr::Keys::generate();
    let identity = keys.public_key().to_hex();
    let channel = create_channel(&http, &keys).await;
    let stored = publish_message(&http, &keys, &channel, "stored-before-subscribe").await;

    let source = tempfile::tempdir().expect("source");
    fs::write(
        source.path().join("extension.json"),
        serde_json::json!({
            "id": EXTENSION_ID,
            "name": "Hello World P5",
            "version": "1",
            "entry": "index.html",
            "scopes": {
                "identity": true,
                "extensionData": true,
                "sign": [{ "kind": 9, "channels": [channel] }],
                "read": [{ "kinds": [9], "channels": [channel] }]
            },
            "egress": []
        })
        .to_string(),
    )
    .expect("manifest");
    fs::write(
        source.path().join("index.html"),
        "<!doctype html><h1>Hello</h1>",
    )
    .expect("entry");

    let state = crate::app_state::build_app_state();
    *state.keys.lock().expect("keys") = keys.clone();
    *state.relay_url_override.lock().expect("relay override") = Some(ws.clone());
    let app = tauri::test::mock_app();
    app.manage(state);
    let base = super::super::extensions_base_dir(app.handle()).expect("base");
    let _ = fs::remove_dir_all(base.join(EXTENSION_ID));

    let prepared =
        prepare_in(&base, source.path(), "directory", identity.clone()).expect("prepare");
    let package = take_prepared(&prepared.token, &identity).expect("consume");
    let destination = base.join(EXTENSION_ID);
    super::super::install::swap_into_place(&base, &package.staged_path, &destination)
        .expect("install exact staged bytes");
    let manifest = super::super::manifest::load_and_validate_manifest(&destination)
        .expect("installed manifest");
    let selected = super::super::grants::GrantSelection {
        identity: true,
        extension_data: true,
        sign: vec![super::super::grants::GrantPair {
            kind: 9,
            channel: channel.clone(),
        }],
        read: vec![super::super::grants::GrantPair {
            kind: 9,
            channel: channel.clone(),
        }],
        ..Default::default()
    };
    let db_path = super::super::dispatch::grant_db_path(app.handle()).expect("db path");
    let mut conn = super::super::grants::open_grant_db(&db_path).expect("db");
    super::super::grants::replace_for_install(
        &mut conn,
        &identity,
        &manifest,
        &prepared.digest,
        &selected,
    )
    .expect("consent");
    assert!(!super::super::grants::is_enabled(
        &conn,
        &identity,
        EXTENSION_ID,
        &prepared.digest
    ));
    super::super::grants::set_enabled(&conn, &identity, EXTENSION_ID, &prepared.digest, true)
        .expect("enable");

    let claim = super::super::frame_host::acquire_authorized(
        base.clone(),
        EXTENSION_ID,
        &identity,
        &prepared.digest,
        Vec::new(),
    )
    .await
    .expect("frame");
    let lease = claim.lease.clone();

    let identity_reply =
        super::super::dispatch::dispatch(app.handle(), &lease, 1, "identity.getPublicKey", None)
            .await;
    assert_eq!(identity_reply.result.expect("identity")["pubkey"], identity);

    let query = super::super::dispatch::dispatch(
        app.handle(),
        &lease,
        1,
        "query.events",
        Some(serde_json::json!({ "filter": { "kinds": [9], "#h": [channel] } })),
    )
    .await;
    assert!(query.ok, "{:?}", query.error);
    assert!(query.result.expect("query")["events"]
        .as_array()
        .is_some_and(|events| events.iter().any(|event| event["id"] == stored)));

    let now = super::super::publish::now_unix();
    let published = super::super::dispatch::dispatch(
        app.handle(),
        &lease,
        1,
        "publish.event",
        Some(serde_json::json!({
            "kind": 9,
            "content": "bridge-published",
            "created_at": now,
            "tags": [["h", channel]]
        })),
    )
    .await;
    assert!(published.ok, "{:?}", published.error);

    let data = super::super::dispatch::dispatch(
        app.handle(),
        &lease,
        1,
        "publish.extensionData",
        Some(serde_json::json!({ "key": "progress", "content": "42", "created_at": now })),
    )
    .await;
    assert!(data.ok, "{:?}", data.error);
    let read_back = super::super::dispatch::dispatch(
        app.handle(),
        &lease,
        1,
        "extensionData.get",
        Some(serde_json::json!({ "key": "progress" })),
    )
    .await;
    assert_eq!(read_back.result.expect("data")["event"]["content"], "42");

    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let listener_seen = Arc::clone(&seen);
    let handle = app.handle().clone();
    app.handle().listen("extension-stream", move |event| {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(event.payload()) else {
            return;
        };
        if let Some(frames) = value["frames"].as_array() {
            listener_seen
                .lock()
                .expect("sink")
                .extend(frames.iter().cloned());
        }
        if value["terminal"] != true {
            super::super::query::acknowledge_subscription_batch(
                &handle,
                value["generation"].as_str().unwrap_or_default(),
                value["sub"].as_str().unwrap_or_default(),
                value["seq"].as_u64().unwrap_or_default(),
                value["token"].as_str().unwrap_or_default(),
                value["frameCount"].as_u64().unwrap_or_default() as usize,
                value["encodedBytes"].as_u64().unwrap_or_default() as usize,
            );
        }
    });
    let subscribed = super::super::dispatch::dispatch(
        app.handle(),
        &lease,
        1,
        "subscribe",
        Some(serde_json::json!({ "filter": { "kinds": [9], "#h": [channel] } })),
    )
    .await;
    let sub = subscribed.result.expect("subscribe")["sub"]
        .as_str()
        .expect("sub")
        .to_string();
    super::super::query::activate_subscription(app.handle(), &lease, &sub);
    wait_for(
        &seen,
        |frames| frames.iter().any(|frame| frame["kind"] == "eose"),
        "aggregate eose",
    )
    .await;

    let denied = super::super::dispatch::dispatch(
        app.handle(),
        &lease,
        1,
        "publish.event",
        Some(serde_json::json!({
            "kind": 9,
            "content": "wrong-channel",
            "created_at": super::super::publish::now_unix(),
            "tags": [["h", uuid::Uuid::new_v4().to_string()]]
        })),
    )
    .await;
    assert_eq!(denied.error_code(), Some("denied"));

    super::super::grants::set_enabled(&conn, &identity, EXTENSION_ID, &prepared.digest, false)
        .expect("disable");
    assert_eq!(
        super::super::frame_host::release_for_identity_extension(&identity, EXTENSION_ID),
        1
    );
    wait_for(
        &seen,
        |frames| {
            frames
                .iter()
                .any(|frame| frame["sub"] == sub && frame["kind"] == "closed")
        },
        "quiet subscription close",
    )
    .await;
    let after_disable =
        super::super::dispatch::dispatch(app.handle(), &lease, 1, "identity.getPublicKey", None)
            .await;
    assert_eq!(after_disable.error_code(), Some("denied"));

    super::super::grants::delete_all_for_extension(&mut conn, EXTENSION_ID).expect("remove state");
    fs::remove_dir_all(destination).expect("remove package");
    println!(
        "M1 LIVE identity={identity} channel={channel} digest={} stored={stored} publish_ok=true query_ok=true subscribe_ok=true extension_data_ok=true denied_ok=true disable_closed=true",
        prepared.digest
    );
}
