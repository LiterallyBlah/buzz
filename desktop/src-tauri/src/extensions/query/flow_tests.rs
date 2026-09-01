use super::*;

fn eose(n: usize) -> StreamFrame {
    StreamFrame::Eose {
        sub: format!("sub-{n}"),
    }
}

fn large_event() -> StreamFrame {
    let event = nostr::EventBuilder::new(nostr::Kind::from(9u16), "x".repeat(450 * 1024))
        .sign_with_keys(&nostr::Keys::generate())
        .expect("sign");
    StreamFrame::Event {
        sub: "sub".to_string(),
        event: Box::new(event),
    }
}

#[test]
fn activation_is_exactly_once_and_releases_nothing_early() {
    let mut flow = FlowState::default();
    flow.enqueue(vec![eose(1)], 0, 0).expect("queue");
    assert!(flow.drain("lease", "sub", 0, 0).is_empty());
    flow.activate().expect("first activation");
    assert_eq!(flow.drain("lease", "sub", 0, 0).len(), 1);
    assert_eq!(flow.activate(), Err(FlowError::ActivationViolation));
}

#[test]
fn a_paused_consumer_cannot_grow_the_browser_window_without_bound() {
    let mut flow = FlowState::default();
    flow.activate().expect("activate");
    let frames = (0..100).map(eose).collect();
    flow.enqueue(frames, 0, 0).expect("bounded Rust queue");
    let batches = flow.drain("lease", "sub", 0, 0);
    assert_eq!(
        batches.len(),
        4,
        "only the exact in-flight count window reaches MessagePort"
    );
    let (queued, _) = flow.queued_totals();
    assert_eq!(queued, 100 - 4 * MAX_STREAM_BATCH_FRAMES);
    assert!(
        flow.drain("lease", "sub", batches.len(), 0).is_empty(),
        "without an ACK no more browser credit exists"
    );
}

#[test]
fn the_encoded_byte_queue_bound_is_independent_of_the_count_bound() {
    let mut flow = FlowState::default();
    for _ in 0..4 {
        flow.enqueue(vec![large_event()], 0, 0)
            .expect("four large frames fit below two MiB");
    }
    let (count, bytes) = flow.queued_totals();
    assert_eq!(count, 4, "far below the 128-frame count ceiling");
    assert!(bytes < MAX_QUEUED_BYTES_PER_SUB);
    assert_eq!(
        flow.enqueue(vec![large_event()], 0, 0),
        Err(FlowError::BoundExceeded),
        "the fifth crosses bytes while count still has ample room"
    );
}

#[test]
fn the_in_flight_byte_window_stops_before_the_batch_count_window() {
    let mut flow = FlowState::default();
    flow.activate().expect("activate");
    flow.enqueue(vec![large_event(), large_event(), large_event()], 0, 0)
        .expect("queue");
    let batches = flow.drain("lease", "sub", 0, 0);
    assert_eq!(batches.len(), 2, "two near-450 KiB batches fit");
    assert!(
        batches.len() < MAX_IN_FLIGHT_BATCHES_PER_SUB,
        "bytes, not count, stopped the third"
    );
}

#[test]
fn per_port_byte_windows_are_independent_of_count_windows() {
    let mut queue_limited = FlowState::default();
    assert_eq!(
        queue_limited.enqueue(vec![large_event()], 1, 8 * 1024 * 1024 - 100 * 1024,),
        Err(FlowError::BoundExceeded),
        "port queued bytes close while frame count remains tiny"
    );

    let mut blocked = FlowState::default();
    blocked.activate().expect("activate");
    blocked.enqueue(vec![large_event()], 0, 0).expect("queue");
    assert!(
        blocked
            .drain("lease", "sub", 1, 3 * 1024 * 1024 - 100 * 1024)
            .is_empty(),
        "port in-flight bytes stop a batch while batch count has room"
    );

    let mut control = FlowState::default();
    control.activate().expect("activate");
    control.enqueue(vec![large_event()], 0, 0).expect("queue");
    assert_eq!(
        control.drain("lease", "sub", 1, 1024 * 1024).len(),
        1,
        "positive control: the same batch drains with byte credit"
    );
}

#[test]
fn only_the_exact_oldest_batch_ack_releases_credit() {
    let mut flow = FlowState::default();
    flow.activate().expect("activate");
    flow.enqueue((0..40).map(eose).collect(), 0, 0)
        .expect("queue");
    let batches = flow.drain("lease", "sub", 0, 0);
    let first = &batches[0];
    let second = &batches[1];

    assert_eq!(
        flow.ack(
            second.seq,
            &second.token,
            second.frame_count,
            second.encoded_bytes,
        ),
        Err(FlowError::AckViolation),
        "an over-window/out-of-order ACK releases nothing"
    );
    assert_eq!(
        flow.ack(
            first.seq,
            "00000000-0000-4000-8000-000000000000",
            first.frame_count,
            first.encoded_bytes,
        ),
        Err(FlowError::AckViolation),
        "the monotonic seq is insufficient without its unguessable token"
    );
    flow.ack(
        first.seq,
        &first.token,
        first.frame_count,
        first.encoded_bytes,
    )
    .expect("exact ACK");
    assert_eq!(
        flow.ack(
            first.seq,
            &first.token,
            first.frame_count,
            first.encoded_bytes,
        ),
        Err(FlowError::AckViolation),
        "a duplicate/stale ACK cannot release credit twice"
    );
}

#[test]
fn terminal_bypasses_data_credit_once_and_clears_retained_state() {
    let mut flow = FlowState::default();
    flow.activate().expect("activate");
    flow.enqueue((0..10).map(eose).collect(), 0, 0)
        .expect("queue");
    let _ = flow.drain("lease", "sub", 0, 0);
    let terminal = flow.terminal_batch("lease", "sub", CloseReason::BoundExceeded);
    assert!(terminal.terminal);
    assert_eq!(terminal.frames[0]["kind"], "closed");
    assert_eq!(terminal.frames[0]["reason"], "bound_exceeded");
    assert_eq!(flow.queued_totals(), (0, 0));
    assert_eq!(flow.in_flight_totals(), (0, 0));
}
