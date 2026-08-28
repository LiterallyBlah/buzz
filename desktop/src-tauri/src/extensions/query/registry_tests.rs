//! Registry behaviour: ownership, the lease wall, and `unsubscribe`.
//!
//! Split from `subscription_tests.rs` when that file passed the repo's
//! 1000-line ratchet. These are the rows that exercise `registry.rs` rather
//! than the aggregate, so the split follows the module boundary rather than
//! cutting an arbitrary line.

use std::sync::Arc;

use super::super::super::dispatch::BridgeReply;
use super::super::subscription::{Aggregate, CloseReason, SubscriptionQuota};
use super::*;

const IDENTITY: &str = "aaaa";
const EXTID: &str = "demo";

fn conn() -> (String, String) {
    ("ws://relay.test".to_string(), IDENTITY.to_string())
}

/// Admission that always admits — the registry's bookkeeping is the subject
/// here, so admission must not be what makes an assertion pass.
fn permissive() -> SubAdmission {
    SubAdmission {
        authority: Box::new(|| Ok(())),
        verify: Box::new(|_| true),
    }
}

fn aggregate(branches: &[&str]) -> Aggregate {
    let mut agg =
        Aggregate::new(branches.iter().map(|b| b.to_string()).collect()).expect("aggregate");
    assert!(
        agg.mark_reply_written().is_empty(),
        "a fresh aggregate holds nothing"
    );
    agg
}

// ── the lease wall ─────────────────────────────────────────────────────────
//
// The lease *is* the generation on this side: `frame_host::acquire` mints a
// fresh UUID per frame mount, so a successor port carries a different one.

const LEASE: &str = "lease-1";
const SUCCESSOR_LEASE: &str = "lease-2";

fn registered(
    registry: &SubscriptionRegistry,
    quota: &Arc<SubscriptionQuota>,
    lease: &str,
    sub: &str,
) {
    let reservation = quota.reserve(IDENTITY, EXTID, 2).expect("reserve").commit();
    registry.insert(
        lease,
        sub,
        aggregate(&["b1"]),
        permissive(),
        reservation,
        conn(),
    );
}

#[test]
fn a_frame_for_a_released_lease_is_dropped() {
    // THE NO-MIGRATION RULE. Keying by (lease, sub) rather than sub alone means
    // there is no code path that could hand a late completion to the frame that
    // replaced the one which asked for it.
    let quota = SubscriptionQuota::new();
    let registry = SubscriptionRegistry::new();
    registered(&registry, &quota, LEASE, "s1");

    assert!(registry.with_aggregate(LEASE, "s1", |_| ()).is_some());
    assert!(
        registry
            .with_aggregate(SUCCESSOR_LEASE, "s1", |_| ())
            .is_none(),
        "the same sub id under a successor lease must not resolve"
    );
}

#[test]
fn the_same_sub_id_on_two_leases_is_two_subscriptions() {
    let quota = SubscriptionQuota::new();
    let registry = SubscriptionRegistry::new();
    registered(&registry, &quota, LEASE, "s1");
    registered(&registry, &quota, SUCCESSOR_LEASE, "s1");
    assert_eq!(registry.live_count(), 2);

    registry.close_for_lease(LEASE, CloseReason::Unsubscribed);
    assert_eq!(registry.live_count(), 1);
    assert!(registry
        .with_aggregate(SUCCESSOR_LEASE, "s1", |_| ())
        .is_some());
}

#[test]
fn the_lease_wall_closes_its_subs_and_releases_their_quota() {
    let quota = SubscriptionQuota::new();
    let registry = SubscriptionRegistry::new();
    registered(&registry, &quota, LEASE, "s1");
    registered(&registry, &quota, LEASE, "s2");
    registered(&registry, &quota, SUCCESSOR_LEASE, "s3");
    assert_eq!(quota.held_by(IDENTITY, EXTID), 6);

    let closed = registry.close_for_lease(LEASE, CloseReason::AuthorityLost);
    assert_eq!(closed.len(), 2);
    assert_eq!(registry.live_count(), 1, "the other lease survives");
    assert_eq!(
        quota.held_by(IDENTITY, EXTID),
        2,
        "only the closed subs' branches came back"
    );
}

#[test]
fn tearing_down_the_same_lease_twice_releases_once() {
    let quota = SubscriptionQuota::new();
    let registry = SubscriptionRegistry::new();
    registered(&registry, &quota, LEASE, "s1");
    let _other = quota.reserve(IDENTITY, EXTID, 3).expect("reserve").commit();
    assert_eq!(quota.held_by(IDENTITY, EXTID), 5);

    registry.close_for_lease(LEASE, CloseReason::AuthorityLost);
    registry.close_for_lease(LEASE, CloseReason::Unsubscribed);
    assert_eq!(
        quota.held_by(IDENTITY, EXTID),
        3,
        "the surviving reservation must not be refunded by a second teardown"
    );
}

#[test]
fn closing_one_sub_releases_only_its_own_branches() {
    let quota = SubscriptionQuota::new();
    let registry = SubscriptionRegistry::new();
    registered(&registry, &quota, LEASE, "s1");
    registered(&registry, &quota, LEASE, "s2");
    assert_eq!(quota.held_by(IDENTITY, EXTID), 4);

    let emit = registry.close_one(LEASE, "s1", CloseReason::Unsubscribed);
    assert_eq!(emit, Some(Emit::Closed(CloseReason::Unsubscribed)));
    assert_eq!(quota.held_by(IDENTITY, EXTID), 2);
    assert!(registry
        .close_one(LEASE, "s1", CloseReason::Unsubscribed)
        .is_none());
    assert_eq!(quota.held_by(IDENTITY, EXTID), 2, "still exactly once");
}

// ── unsubscribe: idempotent, lease-scoped, no existence oracle ─────────────

fn code_of_reply(reply: &BridgeReply) -> Option<&str> {
    reply.error.as_ref().map(|e| e.code.as_str())
}

#[test]
fn unsubscribe_reports_the_same_thing_whether_or_not_the_sub_was_live() {
    // THE ORACLE PROBE. If the reply differed, an extension could enumerate
    // which ids exist — including ones minted for somebody else's frame.
    let quota = SubscriptionQuota::new();
    let live_reservation = quota.reserve(IDENTITY, EXTID, 1).expect("reserve").commit();
    registry().insert(
        "oracle-lease",
        "known-sub",
        aggregate(&["b1"]),
        permissive(),
        live_reservation,
        conn(),
    );

    let hit = unsubscribe(
        "oracle-lease",
        Some(serde_json::json!({ "sub": "known-sub" })),
    );
    let miss = unsubscribe(
        "oracle-lease",
        Some(serde_json::json!({ "sub": "never-existed" })),
    );
    assert!(hit.error.is_none());
    assert_eq!(
        hit.result, miss.result,
        "a live sub and an invented one must be indistinguishable"
    );
    assert_eq!(hit.error.is_none(), miss.error.is_none());
}

#[test]
fn unsubscribe_is_idempotent() {
    let quota = SubscriptionQuota::new();
    let reservation = quota.reserve(IDENTITY, EXTID, 1).expect("reserve").commit();
    registry().insert(
        "idem-lease",
        "s1",
        aggregate(&["b1"]),
        permissive(),
        reservation,
        conn(),
    );

    let first = unsubscribe("idem-lease", Some(serde_json::json!({ "sub": "s1" })));
    let second = unsubscribe("idem-lease", Some(serde_json::json!({ "sub": "s1" })));
    assert!(first.error.is_none() && second.error.is_none());
    assert_eq!(first.result, second.result);
}

#[test]
fn unsubscribe_cannot_reach_another_leases_subscription() {
    // Scoped to the calling lease: one frame must not be able to cancel
    // another's stream by guessing or replaying its id.
    let quota = SubscriptionQuota::new();
    let reservation = quota.reserve(IDENTITY, EXTID, 1).expect("reserve").commit();
    registry().insert(
        "owner-lease",
        "victim",
        aggregate(&["b1"]),
        permissive(),
        reservation,
        conn(),
    );

    let reply = unsubscribe(
        "attacker-lease",
        Some(serde_json::json!({ "sub": "victim" })),
    );
    assert!(reply.error.is_none(), "still an indistinguishable success");
    assert!(
        registry().with_aggregate("owner-lease", "victim", |a| a.is_closed()) == Some(false),
        "the other lease's subscription must be untouched"
    );
    registry().close_one("owner-lease", "victim", CloseReason::Unsubscribed);
}

#[test]
fn a_malformed_sub_is_invalid_params() {
    // The one distinguishable outcome, and it is a statement about the
    // caller's own request rather than about the host's state.
    for bad in [
        None,
        Some(serde_json::json!("not-an-object")),
        Some(serde_json::json!({})),
        Some(serde_json::json!({ "sub": 7 })),
        Some(serde_json::json!({ "sub": "" })),
        Some(serde_json::json!({ "sub": "x".repeat(MAX_SUB_ID_LEN + 1) })),
    ] {
        let reply = unsubscribe("some-lease", bad.clone());
        assert_eq!(code_of_reply(&reply), Some("invalid_params"), "for {bad:?}");
    }
}
