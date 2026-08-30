use super::*;

fn large_event() -> nostr::Event {
    nostr::EventBuilder::new(nostr::Kind::from(9u16), "x".repeat(480 * 1024))
        .sign_with_keys(&nostr::Keys::generate())
        .expect("sign")
}

#[test]
fn post_eose_pre_activation_bytes_close_far_below_the_count_ceiling() {
    let mut aggregate = Aggregate::new(vec!["b1".to_string()]).expect("aggregate");
    assert_eq!(aggregate.on_branch_eose("b1"), Emit::Nothing);
    let mut accepted = 0usize;
    loop {
        match aggregate.on_event("b1", large_event()) {
            Emit::Nothing => accepted += 1,
            Emit::Closed(reason) => {
                assert_eq!(reason, CloseReason::BoundExceeded);
                break;
            }
            other => panic!("unexpected {other:?}"),
        }
    }
    assert!(
        accepted < 16,
        "the 4 MiB encoded-byte bound, not the 2048-event count, must close"
    );
    assert!(accepted < MAX_AWAITING_REPLY_EVENTS);
}

#[test]
fn branch_skew_held_live_bytes_are_independently_bounded() {
    let mut aggregate = Aggregate::new(vec!["a".to_string(), "b".to_string()]).expect("aggregate");
    assert!(aggregate.mark_reply_written().is_empty());
    assert_eq!(aggregate.on_branch_eose("a"), Emit::Nothing);
    let mut accepted = 0usize;
    loop {
        match aggregate.on_event("a", large_event()) {
            Emit::Nothing => accepted += 1,
            Emit::Closed(reason) => {
                assert_eq!(reason, CloseReason::BoundExceeded);
                break;
            }
            other => panic!("live skew must be held, got {other:?}"),
        }
    }
    assert!(accepted < 16);
    assert!(accepted < MAX_HELD_LIVE_EVENTS);
}
