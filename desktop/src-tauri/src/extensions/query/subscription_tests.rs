//! Aggregate and quota behaviour.
//!
//! Everything here is pure state: no socket, no port, no app. That is
//! deliberate — the ordering and rollback rules are the parts a live harness
//! would make hard to provoke, and they are exactly the parts that must not be
//! taken on trust.

use super::*;

const IDENTITY: &str = "aaaa";
const OTHER_IDENTITY: &str = "bbbb";
const EXTID: &str = "demo";

fn event() -> nostr::Event {
    nostr::EventBuilder::new(nostr::Kind::from(9u16), "{}")
        .sign_with_keys(&nostr::Keys::generate())
        .expect("sign")
}

fn aggregate(branches: &[&str]) -> Aggregate {
    Aggregate::new(branches.iter().map(|b| b.to_string()).collect()).expect("aggregate")
}

// ── the public eose waits for every branch ─────────────────────────────────

#[test]
fn one_branch_eosing_does_not_eose_the_aggregate() {
    // The whole reason the aggregate exists. Emitting on the first branch would
    // tell the extension "you have seen the stored history" while another
    // channel is still replaying.
    let mut agg = aggregate(&["b1", "b2", "b3"]);
    assert_eq!(agg.on_branch_eose("b1"), Emit::Nothing);
    assert_eq!(agg.on_branch_eose("b2"), Emit::Nothing);
    assert!(!agg.has_eosed());
    assert_eq!(agg.on_branch_eose("b3"), Emit::Eose);
    assert!(agg.has_eosed());
}

#[test]
fn the_public_eose_is_emitted_exactly_once() {
    let mut agg = aggregate(&["b1"]);
    assert_eq!(agg.on_branch_eose("b1"), Emit::Eose);
    // A relay repeating EOSE, or a duplicate frame, must not produce a second
    // public eose — the contract says exactly one, and ordering downstream
    // depends on it.
    assert_eq!(agg.on_branch_eose("b1"), Emit::Nothing);
}

#[test]
fn an_eose_for_an_unknown_branch_cannot_complete_the_aggregate() {
    // Counting a foreign branch id would complete the set early — the same
    // defect as inventing an EOSE on a timer, arrived at from the other side.
    let mut agg = aggregate(&["b1", "b2"]);
    assert_eq!(agg.on_branch_eose("not-ours"), Emit::Nothing);
    assert_eq!(agg.on_branch_eose("b1"), Emit::Nothing);
    assert!(
        !agg.has_eosed(),
        "a foreign EOSE must not count toward the set"
    );
    assert_eq!(agg.on_branch_eose("b2"), Emit::Eose);
}

// ── dedup across branches, bounded ─────────────────────────────────────────

#[test]
fn the_same_stored_event_from_two_branches_is_delivered_once() {
    // One event can match two channels' branches; without dedup the extension
    // sees it twice and its own state doubles.
    let mut agg = aggregate(&["b1", "b2"]);
    let e = event();
    assert!(matches!(agg.on_event("b1", e.clone()), Emit::Event(_)));
    assert_eq!(agg.on_event("b2", e.clone()), Emit::Nothing);
}

#[test]
fn the_dedup_window_is_cleared_when_the_aggregate_eoses() {
    // The window guards the stored phase only. Holding it for the life of a
    // stream is an unbounded set, which is the leak the bound refuses.
    let mut agg = aggregate(&["b1"]);
    let e = event();
    assert!(matches!(agg.on_event("b1", e.clone()), Emit::Event(_)));
    assert_eq!(agg.on_branch_eose("b1"), Emit::Eose);
    // Post-eose the same id passes: live dedup is the relay's job in v1, and
    // this is the observable consequence of clearing rather than growing.
    assert!(matches!(agg.on_event("b1", e), Emit::Event(_)));
}

#[test]
fn exceeding_the_pre_eose_event_bound_closes_rather_than_evicts() {
    // Evicting would silently drop an event the extension was entitled to, and
    // quietly shrink the dedup guarantee. Closing is observable.
    let mut agg = aggregate(&["b1"]);
    let mut emitted = 0usize;
    let mut closed = None;
    for _ in 0..(MAX_PRE_EOSE_EVENTS + 5) {
        match agg.on_event("b1", event()) {
            Emit::Event(_) => emitted += 1,
            Emit::Closed(reason) => {
                closed = Some(reason);
                break;
            }
            other => panic!("unexpected {other:?}"),
        }
    }
    assert_eq!(closed, Some(CloseReason::BoundExceeded));
    assert!(
        emitted <= MAX_PRE_EOSE_EVENTS,
        "must not deliver past the bound"
    );
    assert!(agg.is_closed());
}

// ── terminal close ─────────────────────────────────────────────────────────

#[test]
fn nothing_is_delivered_after_close() {
    let mut agg = aggregate(&["b1"]);
    assert_eq!(
        agg.close(CloseReason::RelayClosed),
        Emit::Closed(CloseReason::RelayClosed)
    );
    assert_eq!(agg.on_event("b1", event()), Emit::Nothing);
    assert_eq!(agg.on_branch_eose("b1"), Emit::Nothing);
}

#[test]
fn the_first_close_reason_wins() {
    // A teardown arriving after an authority failure must not relabel it
    // "unsubscribed" — that would erase why the stream really ended.
    let mut agg = aggregate(&["b1"]);
    assert_eq!(
        agg.close(CloseReason::AuthorityLost),
        Emit::Closed(CloseReason::AuthorityLost)
    );
    assert_eq!(
        agg.close(CloseReason::Unsubscribed),
        Emit::Closed(CloseReason::AuthorityLost),
        "the original cause must survive a later teardown"
    );
}

#[test]
fn a_closed_aggregate_cannot_eose_afterwards() {
    // Ordering says nothing follows `closed`. An in-flight branch EOSE arriving
    // after a close must not produce a public eose behind it.
    let mut agg = aggregate(&["b1", "b2"]);
    assert_eq!(agg.on_branch_eose("b1"), Emit::Nothing);
    agg.close(CloseReason::AuthorityLost);
    assert_eq!(agg.on_branch_eose("b2"), Emit::Nothing);
    assert!(!agg.has_eosed());
}

#[test]
fn every_close_reason_has_a_bounded_wire_string() {
    // Normalised: a relay's own text must never reach an extension.
    for reason in [
        CloseReason::Unsubscribed,
        CloseReason::AuthorityLost,
        CloseReason::RelayClosed,
        CloseReason::BoundExceeded,
        CloseReason::EoseDeadline,
    ] {
        let wire = reason.as_wire();
        assert!(!wire.is_empty() && wire.len() <= 32, "for {reason:?}");
        assert!(
            wire.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'),
            "for {reason:?}"
        );
    }
}

// ── construction ───────────────────────────────────────────────────────────

#[test]
fn an_aggregate_needs_at_least_one_branch() {
    // Zero branches would EOSE immediately and read as an empty channel, when
    // in fact nothing was ever asked of the relay.
    assert!(Aggregate::new(vec![]).is_none());
}

#[test]
fn an_aggregate_over_the_branch_bound_is_refused() {
    let too_many: Vec<String> = (0..=MAX_BRANCHES_PER_SUB)
        .map(|n| format!("b{n}"))
        .collect();
    assert!(Aggregate::new(too_many).is_none());
}

// ── quota: atomic reserve, exactly-once release ────────────────────────────

#[test]
fn a_reservation_is_all_or_nothing() {
    // The extension budget is filled by several subscriptions, because one
    // subscription may not exceed MAX_BRANCHES_PER_SUB. Reserving the whole
    // extension budget in a single call is refused by that bound first, which
    // would test the wrong gate.
    let quota = SubscriptionQuota::new();
    let subs = MAX_BRANCHES_PER_EXTENSION / MAX_BRANCHES_PER_SUB;
    let held: Vec<Reservation> = (0..subs)
        .map(|_| {
            quota
                .reserve(IDENTITY, EXTID, MAX_BRANCHES_PER_SUB)
                .expect("each subscription fits")
        })
        .collect();
    assert_eq!(quota.held_by(IDENTITY, EXTID), MAX_BRANCHES_PER_EXTENSION);

    // One more branch does not fit, and the refusal must not have taken a
    // partial bite out of the budget.
    assert!(quota.reserve(IDENTITY, EXTID, 1).is_none());
    assert_eq!(quota.held_by(IDENTITY, EXTID), MAX_BRANCHES_PER_EXTENSION);
    drop(held);
    assert_eq!(quota.held_by(IDENTITY, EXTID), 0);
}

#[test]
fn dropping_a_reservation_releases_it() {
    // The rollback path the contract enumerates — branch-open failure, witness
    // mismatch, teardown mid-open, deadline expiry — is every early return.
    // Making release structural is what stops one of them being missed.
    let quota = SubscriptionQuota::new();
    {
        let _reservation = quota.reserve(IDENTITY, EXTID, 4).expect("reserve");
        assert_eq!(quota.held_by(IDENTITY, EXTID), 4);
    }
    assert_eq!(
        quota.held_by(IDENTITY, EXTID),
        0,
        "an un-committed reservation must not leak"
    );
}

#[test]
fn releasing_twice_gives_back_once() {
    let quota = SubscriptionQuota::new();
    let mut reservation = quota.reserve(IDENTITY, EXTID, 3).expect("reserve");
    reservation.release();
    reservation.release();
    drop(reservation); // and Drop runs too
    assert_eq!(
        quota.held_by(IDENTITY, EXTID),
        0,
        "exactly once, not three times"
    );
}

#[test]
fn a_committed_reservation_survives_until_teardown() {
    let quota = SubscriptionQuota::new();
    let mut live = quota.reserve(IDENTITY, EXTID, 5).expect("reserve").commit();
    assert_eq!(
        quota.held_by(IDENTITY, EXTID),
        5,
        "committing must not release"
    );
    live.release();
    assert_eq!(quota.held_by(IDENTITY, EXTID), 0);
    live.release();
    assert_eq!(quota.held_by(IDENTITY, EXTID), 0, "still exactly once");
}

#[test]
fn the_budget_is_keyed_by_identity_and_extension_not_by_port() {
    // A port re-open must not multiply an extension's footprint, and one
    // identity's usage must not spend another's.
    let quota = SubscriptionQuota::new();
    let _a = quota.reserve(IDENTITY, EXTID, 8).expect("reserve");
    let _b = quota.reserve(OTHER_IDENTITY, EXTID, 8).expect("reserve");
    assert_eq!(quota.held_by(IDENTITY, EXTID), 8);
    assert_eq!(quota.held_by(OTHER_IDENTITY, EXTID), 8);
    assert_eq!(quota.held_by(IDENTITY, "other-extension"), 0);
}

#[test]
fn a_reservation_larger_than_one_sub_may_hold_is_refused() {
    let quota = SubscriptionQuota::new();
    assert!(quota
        .reserve(IDENTITY, EXTID, MAX_BRANCHES_PER_SUB + 1)
        .is_none());
    assert!(quota.reserve(IDENTITY, EXTID, 0).is_none());
    assert_eq!(quota.held_by(IDENTITY, EXTID), 0);
}
