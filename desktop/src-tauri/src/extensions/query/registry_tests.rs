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
    registered_on(registry, quota, lease, sub, &["b1"], conn(), permissive());
}

/// A registered subscription with an explicit branch set, socket and admission.
fn registered_on(
    registry: &SubscriptionRegistry,
    quota: &Arc<SubscriptionQuota>,
    lease: &str,
    sub: &str,
    branches: &[&str],
    connection: (String, String),
    admission: SubAdmission,
) {
    let reservation = quota.reserve(IDENTITY, EXTID, 2).expect("reserve").commit();
    registry.insert(
        lease,
        sub,
        aggregate(branches),
        admission,
        reservation,
        connection,
    );
}

fn event() -> nostr::Event {
    nostr::EventBuilder::new(nostr::Kind::from(9u16), "{}")
        .sign_with_keys(&nostr::Keys::generate())
        .expect("sign")
}

fn eose_frame(branch: &str) -> crate::relay::subscribe::RelayFrame {
    crate::relay::subscribe::RelayFrame::Eose {
        sub_id: branch.to_string(),
    }
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

// ── the reader's owner lookup ──────────────────────────────────────────────
//
// `route_by_branch` is the only path a relay frame takes into an aggregate.
// It answers two questions the reader must not answer itself: which
// subscription owns this branch, and whose admission judges the event.

#[test]
fn a_frame_is_routed_to_the_subscription_that_owns_its_branch() {
    // The positive control. Every refusal below must not be satisfied by a
    // lookup that never routes anything.
    let quota = SubscriptionQuota::new();
    let registry = SubscriptionRegistry::new();
    registered_on(
        &registry,
        &quota,
        LEASE,
        "s1",
        &["b1", "b2"],
        conn(),
        permissive(),
    );

    let delivery = registry
        .route_by_branch("b1", eose_frame("b1"))
        .expect("b1 is owned");
    assert_eq!(delivery.lease, LEASE);
    assert!(
        delivery.frames.is_empty(),
        "one branch of two does not eose the aggregate"
    );
    let delivery = registry
        .route_by_branch("b2", eose_frame("b2"))
        .expect("b2 is owned");
    assert_eq!(
        delivery.frames,
        vec![StreamFrame::Eose {
            sub: "s1".to_string()
        }],
        "the last branch produces the single public eose, keyed by sub"
    );
}

#[test]
fn a_frame_for_an_unowned_branch_routes_nowhere() {
    // A branch nobody holds is a frame for a torn-down subscription. Routing it
    // to *some* aggregate is how a dead sub's traffic reaches a live one.
    let quota = SubscriptionQuota::new();
    let registry = SubscriptionRegistry::new();
    registered_on(
        &registry,
        &quota,
        LEASE,
        "s1",
        &["b1"],
        conn(),
        permissive(),
    );

    assert!(
        registry
            .route_by_branch("not-ours", eose_frame("not-ours"))
            .is_none(),
        "an unowned branch must find no aggregate"
    );
    assert!(
        registry.route_by_branch("b1", eose_frame("b1")).is_some(),
        "and the owned one still routes — the lookup is not simply dead"
    );
}

#[test]
fn each_subscriptions_own_admission_judges_its_events() {
    // THE MULTIPLEX PROBE. Two subs share a socket; one may see events, one may
    // not. If the reader supplied admission, or the registry used the wrong
    // entry's, the refusing sub's verdict would decide the permissive sub's
    // event — or worse, the other way round.
    let quota = SubscriptionQuota::new();
    let registry = SubscriptionRegistry::new();
    registered_on(
        &registry,
        &quota,
        LEASE,
        "yes",
        &["b-yes"],
        conn(),
        permissive(),
    );
    registered_on(
        &registry,
        &quota,
        LEASE,
        "no",
        &["b-no"],
        conn(),
        SubAdmission {
            authority: Box::new(|| Ok(())),
            verify: Box::new(|_| false),
        },
    );

    let e = event();
    let delivered = registry
        .route_by_branch(
            "b-yes",
            crate::relay::subscribe::RelayFrame::Event {
                sub_id: "b-yes".to_string(),
                event: Box::new(e.clone()),
            },
        )
        .expect("routed");
    assert_eq!(delivered.frames.len(), 1, "the permissive sub is delivered");

    let dropped = registry
        .route_by_branch(
            "b-no",
            crate::relay::subscribe::RelayFrame::Event {
                sub_id: "b-no".to_string(),
                event: Box::new(e),
            },
        )
        .expect("routed");
    assert!(
        dropped.frames.is_empty(),
        "the refusing sub's own verify drops it, and does not close the stream"
    );
    assert!(dropped.close_branches.is_empty());
}

#[test]
fn a_closing_frame_returns_every_branch_and_releases_the_quota() {
    // A half-closed aggregate leaves the relay streaming into a subscription
    // nobody reads, so the CLOSE burst must name all of them — not the one the
    // frame arrived on.
    let quota = SubscriptionQuota::new();
    let registry = SubscriptionRegistry::new();
    registered_on(
        &registry,
        &quota,
        LEASE,
        "s1",
        &["b1", "b2", "b3"],
        conn(),
        permissive(),
    );
    assert_eq!(quota.held_by(IDENTITY, EXTID), 2);

    let delivery = registry
        .route_by_branch(
            "b2",
            crate::relay::subscribe::RelayFrame::Closed {
                sub_id: "b2".to_string(),
                reason: "whatever".to_string(),
            },
        )
        .expect("routed");
    let mut branches = delivery.close_branches.clone();
    branches.sort();
    assert_eq!(
        branches,
        vec!["b1", "b2", "b3"],
        "every branch, not just b2"
    );
    assert_eq!(
        quota.held_by(IDENTITY, EXTID),
        0,
        "and removing the entry gave the budget back"
    );
    assert!(registry.route_by_branch("b1", eose_frame("b1")).is_none());
}

// ── the transport wall ─────────────────────────────────────────────────────

#[test]
fn a_dead_socket_closes_only_the_subscriptions_it_carried() {
    // Scoped to the connection key: one relay's socket dying says nothing about
    // subscriptions on another relay or under another identity.
    let quota = SubscriptionQuota::new();
    let registry = SubscriptionRegistry::new();
    let other = ("ws://elsewhere.test".to_string(), IDENTITY.to_string());
    registered_on(
        &registry,
        &quota,
        LEASE,
        "mine",
        &["b1"],
        conn(),
        permissive(),
    );
    registered_on(
        &registry,
        &quota,
        LEASE,
        "theirs",
        &["b2"],
        other.clone(),
        permissive(),
    );

    let closed = registry.close_for_connection(&conn());
    assert_eq!(closed.len(), 1, "only this socket's subscriptions");
    assert_eq!(
        closed[0].frames,
        vec![StreamFrame::Closed {
            sub: "mine".to_string(),
            reason: CloseReason::RelayClosed,
        }]
    );
    assert!(
        closed[0].close_branches.is_empty(),
        "there is no socket left to CLOSE on"
    );
    assert!(
        registry.route_by_branch("b2", eose_frame("b2")).is_some(),
        "the other socket's subscription survives"
    );
}

#[test]
fn a_dead_socket_releases_the_branch_budget() {
    let quota = SubscriptionQuota::new();
    let registry = SubscriptionRegistry::new();
    registered_on(
        &registry,
        &quota,
        LEASE,
        "s1",
        &["b1"],
        conn(),
        permissive(),
    );
    assert_eq!(quota.held_by(IDENTITY, EXTID), 2);
    registry.close_for_connection(&conn());
    assert_eq!(
        quota.held_by(IDENTITY, EXTID),
        0,
        "a socket dying must not strand its branches forever"
    );
}

// ── the initial-EOSE deadline ──────────────────────────────────────────────

#[test]
fn the_deadline_closes_a_silent_subscription_and_names_its_branches() {
    // Unlike a dead transport there IS still a socket, so the branches come
    // back to be CLOSEd. And no public eose is invented: telling the extension
    // it has seen all stored history when a channel never answered is the one
    // outcome the deadline exists to prevent.
    let quota = SubscriptionQuota::new();
    let registry = SubscriptionRegistry::new();
    registered_on(
        &registry,
        &quota,
        LEASE,
        "s1",
        &["b1", "b2"],
        conn(),
        permissive(),
    );

    let delivery = registry
        .close_on_eose_deadline(LEASE, "s1")
        .expect("the deadline fires");
    assert_eq!(
        delivery.frames,
        vec![StreamFrame::Closed {
            sub: "s1".to_string(),
            reason: CloseReason::EoseDeadline,
        }],
        "closed with the named reason, and no eose before it"
    );
    let mut branches = delivery.close_branches.clone();
    branches.sort();
    assert_eq!(branches, vec!["b1", "b2"]);
    assert_eq!(quota.held_by(IDENTITY, EXTID), 0);
}

#[test]
fn the_deadline_does_nothing_to_a_subscription_that_eosed() {
    // Firing on a healthy stream would close it mid-flight.
    let quota = SubscriptionQuota::new();
    let registry = SubscriptionRegistry::new();
    registered_on(
        &registry,
        &quota,
        LEASE,
        "s1",
        &["b1"],
        conn(),
        permissive(),
    );
    registry
        .route_by_branch("b1", eose_frame("b1"))
        .expect("routed");

    assert!(
        registry.close_on_eose_deadline(LEASE, "s1").is_none(),
        "already eosed — the deadline is a no-op, not a second close"
    );
    assert_eq!(
        quota.held_by(IDENTITY, EXTID),
        2,
        "and the subscription keeps its budget"
    );
}

#[test]
fn the_deadline_for_an_unknown_subscription_is_inert() {
    let quota = SubscriptionQuota::new();
    let registry = SubscriptionRegistry::new();
    registered_on(
        &registry,
        &quota,
        LEASE,
        "s1",
        &["b1"],
        conn(),
        permissive(),
    );
    assert!(registry.close_on_eose_deadline(LEASE, "gone").is_none());
    assert!(registry
        .close_on_eose_deadline(SUCCESSOR_LEASE, "s1")
        .is_none());
    assert!(
        registry.close_on_eose_deadline(LEASE, "s1").is_some(),
        "the real one still fires — the lookup is not simply dead"
    );
}
