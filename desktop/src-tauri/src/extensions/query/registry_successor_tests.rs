use std::sync::{Arc, Mutex};

use super::*;

const IDENTITY: &str = "successor-identity";
const EXTENSION: &str = "successor-extension";

fn instance(generation: u64) -> ConnectionInstance {
    ConnectionInstance {
        key: ("ws://relay.test".to_string(), IDENTITY.to_string()),
        generation,
    }
}

fn admission() -> SubAdmission {
    SubAdmission {
        authority: Box::new(|| Ok(())),
        verify: Box::new(|_| true),
    }
}

fn signed(content: &str) -> nostr::Event {
    nostr::EventBuilder::new(nostr::Kind::from(9u16), content)
        .sign_with_keys(&nostr::Keys::generate())
        .expect("sign")
}

fn event_frame(branch: &str, event: nostr::Event) -> crate::relay::subscribe::RelayFrame {
    crate::relay::subscribe::RelayFrame::Event {
        sub_id: branch.to_string(),
        event: Box::new(event),
    }
}

fn eose(branch: &str) -> crate::relay::subscribe::RelayFrame {
    crate::relay::subscribe::RelayFrame::Eose {
        sub_id: branch.to_string(),
    }
}

fn register(
    registry: &SubscriptionRegistry,
    quota: &Arc<SubscriptionQuota>,
    lease: &str,
    sub: &str,
    branches: &[&str],
    connection: ConnectionInstance,
    activate: bool,
    closer: RelayCloser,
) {
    let aggregate = Aggregate::new(branches.iter().map(|value| value.to_string()).collect())
        .expect("aggregate");
    let reservation = quota
        .reserve(IDENTITY, EXTENSION, branches.len())
        .expect("reserve")
        .commit();
    registry.insert(
        lease,
        sub,
        aggregate,
        admission(),
        Arc::new(|_| Ok(())),
        closer,
        reservation,
        connection,
    );
    if activate {
        let activated = registry.activate(lease, sub).expect("activate");
        assert!(activated.batches.is_empty());
    }
}

fn wire(delivery: &Delivery) -> Vec<serde_json::Value> {
    delivery
        .batches
        .iter()
        .flat_map(|batch| batch.frames.iter().cloned())
        .collect()
}

#[test]
fn g1_frames_and_cleanup_cannot_touch_g2_on_the_same_key() {
    let quota = SubscriptionQuota::new();
    let registry = SubscriptionRegistry::new();
    let g1 = instance(1);
    let g2 = instance(2);
    register(
        &registry,
        &quota,
        "lease-g2",
        "sub-g2",
        &["branch-g2"],
        g2.clone(),
        true,
        Box::new(|_| {}),
    );

    assert!(
        registry
            .route_by_branch(&g1, "branch-g2", eose("branch-g2"))
            .is_none(),
        "an old reader cannot route a newer generation's branch"
    );
    assert!(registry.close_for_connection(&g1).is_empty());
    assert_eq!(registry.live_count(), 1, "G1 cleanup cannot sweep G2");
    assert!(
        registry
            .route_by_branch(&g2, "branch-g2", eose("branch-g2"))
            .is_some(),
        "positive control: G2 still routes its own branch"
    );
}

#[test]
fn eose_skew_keeps_all_stored_before_eose_and_live_after() {
    let quota = SubscriptionQuota::new();
    let registry = SubscriptionRegistry::new();
    register(
        &registry,
        &quota,
        "lease-order",
        "sub-order",
        &["a", "b"],
        instance(1),
        true,
        Box::new(|_| {}),
    );
    assert!(wire(
        &registry
            .route_by_branch(&instance(1), "a", eose("a"))
            .expect("route")
    )
    .is_empty());
    let live_a = signed("live-a");
    assert!(wire(
        &registry
            .route_by_branch(&instance(1), "a", event_frame("a", live_a.clone()))
            .expect("route")
    )
    .is_empty());
    let stored_b = signed("stored-b");
    let stored = registry
        .route_by_branch(&instance(1), "b", event_frame("b", stored_b.clone()))
        .expect("route");
    assert_eq!(wire(&stored)[0]["event"]["id"], stored_b.id.to_hex());
    let finish = registry
        .route_by_branch(&instance(1), "b", eose("b"))
        .expect("route");
    let frames = wire(&finish);
    assert_eq!(frames[0]["kind"], "eose");
    assert_eq!(frames[1]["event"]["id"], live_a.id.to_hex());
}

#[test]
fn preactivation_frames_release_only_after_the_exact_receipt() {
    let quota = SubscriptionQuota::new();
    let registry = SubscriptionRegistry::new();
    register(
        &registry,
        &quota,
        "lease-activate",
        "sub-activate",
        &["b1"],
        instance(1),
        false,
        Box::new(|_| {}),
    );
    let stored = signed("stored");
    let before_event = registry
        .route_by_branch(&instance(1), "b1", event_frame("b1", stored.clone()))
        .expect("route");
    let before_eose = registry
        .route_by_branch(&instance(1), "b1", eose("b1"))
        .expect("route");
    assert!(before_event.batches.is_empty() && before_eose.batches.is_empty());

    let activated = registry
        .activate("lease-activate", "sub-activate")
        .expect("exact receipt");
    let frames = wire(&activated);
    assert_eq!(frames[0]["event"]["id"], stored.id.to_hex());
    assert_eq!(frames[1]["kind"], "eose");
}

#[test]
fn duplicate_or_stale_ack_closes_once_without_releasing_credit_twice() {
    let quota = SubscriptionQuota::new();
    let registry = SubscriptionRegistry::new();
    register(
        &registry,
        &quota,
        "lease-ack",
        "sub-ack",
        &["b1"],
        instance(1),
        true,
        Box::new(|_| {}),
    );
    let delivery = registry
        .route_by_branch(&instance(1), "b1", eose("b1"))
        .expect("batch");
    let batch = &delivery.batches[0];
    let exact = registry
        .acknowledge(
            "lease-ack",
            "sub-ack",
            batch.seq,
            &batch.token,
            batch.frame_count,
            batch.encoded_bytes,
        )
        .expect("exact ack");
    assert!(exact.batches.is_empty());
    let duplicate = registry
        .acknowledge(
            "lease-ack",
            "sub-ack",
            batch.seq,
            &batch.token,
            batch.frame_count,
            batch.encoded_bytes,
        )
        .expect("violation closes");
    assert_eq!(wire(&duplicate)[0]["kind"], "closed");
    assert_eq!(wire(&duplicate)[0]["reason"], "bound_exceeded");
    assert_eq!(quota.held_by(IDENTITY, EXTENSION), 0);
    assert!(registry
        .acknowledge(
            "lease-ack",
            "sub-ack",
            batch.seq,
            &batch.token,
            batch.frame_count,
            batch.encoded_bytes,
        )
        .is_none());
}

#[test]
fn ack_timeout_closes_relay_and_returns_quota_once() {
    let quota = SubscriptionQuota::new();
    let registry = SubscriptionRegistry::new();
    let closed = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&closed);
    register(
        &registry,
        &quota,
        "lease-timeout",
        "sub-timeout",
        &["b1"],
        instance(1),
        true,
        Box::new(move |branches| log.lock().unwrap().extend_from_slice(branches)),
    );
    let delivery = registry
        .route_by_branch(&instance(1), "b1", eose("b1"))
        .expect("batch");
    let batch = &delivery.batches[0];
    let terminal = registry
        .close_on_ack_timeout("lease-timeout", "sub-timeout", batch.seq, &batch.token)
        .expect("timeout");
    assert_eq!(wire(&terminal)[0]["kind"], "closed");
    assert_eq!(closed.lock().unwrap().as_slice(), ["b1"]);
    assert_eq!(quota.held_by(IDENTITY, EXTENSION), 0);
    assert!(registry
        .close_on_ack_timeout("lease-timeout", "sub-timeout", batch.seq, &batch.token,)
        .is_none());
}

#[tokio::test]
async fn real_frame_release_closes_relay_registry_and_quota() {
    let _guard = crate::extensions::frame_host::lifecycle_guard().await;
    let lease = "successor-release-lease";
    crate::extensions::frame_host::insert_lease_for_test(lease, EXTENSION);
    let closed = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&closed);
    register(
        registry(),
        quota(),
        lease,
        "sub-release",
        &["b1", "b2"],
        instance(1),
        true,
        Box::new(move |branches| log.lock().unwrap().extend_from_slice(branches)),
    );
    assert_eq!(quota().held_by(IDENTITY, EXTENSION), 2);

    crate::extensions::frame_host::release(lease);
    assert!(registry()
        .with_aggregate(lease, "sub-release", |_| ())
        .is_none());
    assert_eq!(quota().held_by(IDENTITY, EXTENSION), 0);
    let mut got = closed.lock().unwrap().clone();
    got.sort();
    assert_eq!(got, ["b1", "b2"]);
}

#[tokio::test]
async fn real_frame_release_emits_one_terminal_batch_through_the_tauri_sink() {
    use tauri::Listener as _;

    let _guard = crate::extensions::frame_host::lifecycle_guard().await;
    let lease = "successor-release-sink-lease";
    let sub = "sub-release-sink";
    crate::extensions::frame_host::insert_lease_for_test(lease, EXTENSION);

    let app = tauri::test::mock_app();
    let seen = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
    let listener_seen = Arc::clone(&seen);
    app.handle()
        .listen(super::super::connection::STREAM_EVENT, move |event| {
            listener_seen
                .lock()
                .unwrap()
                .push(serde_json::from_str(event.payload()).expect("stream batch json"));
        });

    let closed = Arc::new(Mutex::new(Vec::new()));
    let relay_closed = Arc::clone(&closed);
    let aggregate = Aggregate::new(vec!["b1".to_string()]).expect("aggregate");
    let reservation = quota()
        .reserve(IDENTITY, EXTENSION, 1)
        .expect("reserve")
        .commit();
    registry().insert(
        lease,
        sub,
        aggregate,
        admission(),
        super::super::connection::app_sink(app.handle()),
        Box::new(move |branches| relay_closed.lock().unwrap().extend_from_slice(branches)),
        reservation,
        instance(41),
    );
    let activated = registry().activate(lease, sub).expect("activate");
    assert!(activated.batches.is_empty());

    crate::extensions::frame_host::release(lease);
    let batches = seen.lock().unwrap().clone();
    assert_eq!(batches.len(), 1, "lease release emits one terminal batch");
    assert_eq!(batches[0]["generation"], lease);
    assert_eq!(batches[0]["sub"], sub);
    assert_eq!(batches[0]["terminal"], true);
    assert_eq!(batches[0]["frames"].as_array().unwrap().len(), 1);
    assert_eq!(batches[0]["frames"][0]["kind"], "closed");
    assert_eq!(batches[0]["frames"][0]["reason"], "unsubscribed");
    assert_eq!(closed.lock().unwrap().as_slice(), ["b1"]);
    assert_eq!(quota().held_by(IDENTITY, EXTENSION), 0);
    assert_eq!(registry().live_count(), 0);

    crate::extensions::frame_host::release(lease);
    assert_eq!(
        seen.lock().unwrap().len(),
        1,
        "an idempotent second release cannot emit another terminal batch"
    );
}

#[test]
fn lease_release_before_activation_keeps_terminal_behind_the_reply_receipt() {
    let quota = SubscriptionQuota::new();
    let registry = SubscriptionRegistry::new();
    let closed = Arc::new(Mutex::new(Vec::new()));
    let relay_closed = Arc::clone(&closed);
    register(
        &registry,
        &quota,
        "lease-preactivation-release",
        "sub-preactivation-release",
        &["b1"],
        instance(42),
        false,
        Box::new(move |branches| relay_closed.lock().unwrap().extend_from_slice(branches)),
    );

    let closure =
        registry.close_for_lease("lease-preactivation-release", CloseReason::Unsubscribed);
    assert_eq!(closure.closed, 1);
    assert!(
        closure.deliveries.is_empty(),
        "a terminal frame cannot overtake the correlated subscribe reply"
    );
    assert_eq!(quota.held_by(IDENTITY, EXTENSION), 0);
    assert_eq!(closed.lock().unwrap().as_slice(), ["b1"]);

    let after_reply = registry
        .activate("lease-preactivation-release", "sub-preactivation-release")
        .expect("exact receipt releases the deferred terminal");
    let frames = wire(&after_reply);
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0]["kind"], "closed");
    assert_eq!(frames[0]["reason"], "unsubscribed");
    assert_eq!(registry.live_count(), 0);
}

#[tokio::test]
async fn real_frame_shutdown_closes_every_lease_once() {
    let _guard = crate::extensions::frame_host::lifecycle_guard().await;
    let leases = ["successor-shutdown-a", "successor-shutdown-b"];
    let closed = Arc::new(Mutex::new(Vec::new()));
    for (index, lease) in leases.iter().enumerate() {
        crate::extensions::frame_host::insert_lease_for_test(lease, EXTENSION);
        let log = Arc::clone(&closed);
        register(
            registry(),
            quota(),
            lease,
            &format!("sub-{index}"),
            &[if index == 0 { "ba" } else { "bb" }],
            instance(20 + index as u64),
            true,
            Box::new(move |branches| log.lock().unwrap().extend_from_slice(branches)),
        );
    }
    assert_eq!(quota().held_by(IDENTITY, EXTENSION), 2);
    crate::extensions::frame_host::shutdown_now();
    assert_eq!(quota().held_by(IDENTITY, EXTENSION), 0);
    let mut got = closed.lock().unwrap().clone();
    got.sort();
    assert_eq!(got, ["ba", "bb"]);
    crate::extensions::frame_host::shutdown_now();
    assert_eq!(quota().held_by(IDENTITY, EXTENSION), 0);
}
