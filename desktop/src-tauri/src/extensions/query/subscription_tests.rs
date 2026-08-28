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

/// An aggregate whose `{sub}` reply has already been written.
///
/// That is the steady state for everything except the ordering probes below:
/// production writes the reply immediately after `subscribe` returns, so
/// aggregation behaviour is what happens *after* it. The probes that care
/// about the boundary build a raw aggregate instead.
fn aggregate(branches: &[&str]) -> Aggregate {
    let mut agg = raw_aggregate(branches);
    assert!(
        agg.mark_reply_written().is_empty(),
        "a fresh aggregate holds nothing"
    );
    agg
}

/// An aggregate that has **not** yet written its `{sub}` reply.
fn raw_aggregate(branches: &[&str]) -> Aggregate {
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

// ── two-stage admission ────────────────────────────────────────────────────
//
// `verify_event` returns a bool and cannot say *why* it refused. These pin the
// distinction that bool loses: a bad event is dropped and the stream carries
// on; lost authority ends the whole aggregate.

use std::cell::Cell;

#[test]
fn a_bad_event_is_dropped_and_the_stream_continues() {
    let admission = admit(|| Ok(()), || false);
    assert_eq!(admission, Admission::DropEvent);
}

#[test]
fn a_good_event_under_live_authority_is_delivered() {
    // The positive control. Without it the two refusals below are satisfied by
    // an implementation that admits nothing.
    assert_eq!(admit(|| Ok(()), || true), Admission::Deliver);
}

#[test]
fn lost_authority_closes_the_aggregate_rather_than_dropping_the_event() {
    // The outcome that must not be collapsed into DropEvent: a revoked grant is
    // not a malformed event, and treating it as one leaves the subscription
    // streaming under authority it no longer holds.
    assert_eq!(
        admit(|| Err(CloseReason::AuthorityLost), || true),
        Admission::CloseAggregate(CloseReason::AuthorityLost)
    );
}

#[test]
fn the_per_event_check_is_not_reached_when_authority_has_gone() {
    // THE ORDERING PROBE. If `verify` ran first, a revoked grant arriving
    // alongside a bad event would report DropEvent — the stream would survive
    // an authority failure because the event happened to be invalid too. So the
    // property is not just "the result is CloseAggregate", it is that the
    // per-event check is never consulted at all.
    let verify_ran = Cell::new(false);
    let admission = admit(
        || Err(CloseReason::AuthorityLost),
        || {
            verify_ran.set(true);
            false
        },
    );
    assert_eq!(
        admission,
        Admission::CloseAggregate(CloseReason::AuthorityLost)
    );
    assert!(
        !verify_ran.get(),
        "authority is checked first and short-circuits; verify must not run"
    );
}

#[test]
fn a_quiet_channel_revocation_closes_without_any_event() {
    // Per-event checking alone is not continuous authority: a channel with no
    // traffic would keep a revoked subscription alive indefinitely. The
    // grant-change path closes the aggregate directly.
    let mut agg = aggregate(&["b1"]);
    assert!(!agg.is_closed());
    assert_eq!(
        agg.close(CloseReason::AuthorityLost),
        Emit::Closed(CloseReason::AuthorityLost)
    );
    assert!(
        agg.is_closed(),
        "a quiet revocation must not wait for traffic"
    );
}

// ── the reply precedes the stored drain ────────────────────────────────────
//
// The relay can answer before the host has finished replying to the
// `subscribe` that caused it. Frames for a `sub` the extension has not been
// told the id of are unroutable at best, and at worst get attributed to a sub
// it has seen.

#[test]
fn events_arriving_before_the_reply_are_held_then_drained_in_order() {
    let mut agg = raw_aggregate(&["b1"]);
    let first = event();
    let second = event();
    assert_eq!(agg.on_event("b1", first.clone()), Emit::Nothing);
    assert_eq!(agg.on_event("b1", second.clone()), Emit::Nothing);
    assert!(!agg.reply_written());

    let drained = agg.mark_reply_written();
    assert_eq!(drained.len(), 2, "both held events must be released");
    // Arrival order survives the boundary — the queue is serial.
    assert_eq!(drained[0], Emit::Event(Box::new(first)));
    assert_eq!(drained[1], Emit::Event(Box::new(second)));
}

#[test]
fn an_eose_reached_before_the_reply_lands_after_the_stored_events() {
    // The sharp one. If every branch EOSEs while the reply is still in flight,
    // releasing the eose first would terminate a stored phase whose events had
    // not been delivered yet.
    let mut agg = raw_aggregate(&["b1"]);
    let stored = event();
    assert_eq!(agg.on_event("b1", stored.clone()), Emit::Nothing);
    assert_eq!(
        agg.on_branch_eose("b1"),
        Emit::Nothing,
        "held behind the reply"
    );
    assert!(!agg.has_eosed());

    let drained = agg.mark_reply_written();
    assert_eq!(
        drained,
        vec![Emit::Event(Box::new(stored)), Emit::Eose],
        "stored events first, then the single eose"
    );
    assert!(agg.has_eosed());
}

#[test]
fn marking_the_reply_written_twice_drains_once() {
    let mut agg = raw_aggregate(&["b1"]);
    agg.on_event("b1", event());
    assert_eq!(agg.mark_reply_written().len(), 1);
    assert!(
        agg.mark_reply_written().is_empty(),
        "a second reply must not replay the buffer"
    );
}

#[test]
fn a_close_before_the_reply_discards_the_held_events() {
    // Nothing may follow `closed`, and that includes events that were waiting
    // on a reply which then never mattered.
    let mut agg = raw_aggregate(&["b1"]);
    agg.on_event("b1", event());
    agg.close(CloseReason::AuthorityLost);
    assert!(
        agg.mark_reply_written().is_empty(),
        "held events must not be delivered after a close"
    );
}

#[test]
fn the_pre_reply_buffer_is_bounded() {
    // Holding behind the reply must not become an unbounded buffer the relay
    // controls the size of.
    let mut agg = raw_aggregate(&["b1"]);
    let mut closed = None;
    for _ in 0..(MAX_PRE_EOSE_EVENTS + 5) {
        if let Emit::Closed(reason) = agg.on_event("b1", event()) {
            closed = Some(reason);
            break;
        }
    }
    assert_eq!(closed, Some(CloseReason::BoundExceeded));
}
