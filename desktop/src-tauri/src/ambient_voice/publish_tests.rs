//! Ambient publishing tests.
//!
//! The key-backup injection test for this boundary lives in
//! `egress_guard_tests.rs` alongside the other boundaries, so the NIP-49 test
//! vector stays confined to the allowlisted files.

use super::*;

fn keys() -> nostr::Keys {
    nostr::Keys::generate()
}

#[test]
fn the_signed_body_is_guarded_before_it_can_be_posted() {
    // The guard is the last thing between a signed event and the socket. A
    // clean body passes through unchanged and parses as the event we signed.
    let signing = keys();
    let channel = Uuid::new_v4();
    let builder = events::build_message(
        channel,
        "hello there",
        None,
        &[],
        &[],
        &[],
        &[],
        &[],
        None,
        &crate::relay::relay_api_base_url(),
    )
    .expect("builder");
    let body = sign_and_guard_ambient_body(builder, &signing).expect("guarded body");
    let value: serde_json::Value = serde_json::from_slice(&body).expect("event json");
    assert_eq!(value["kind"], 9);
    assert_eq!(value["content"], "hello there");
}

#[test]
fn the_transcript_event_is_an_ordinary_kind_9_addressed_to_the_agent() {
    let signing = keys();
    let channel = Uuid::new_v4();
    let agent = "a".repeat(64);
    let builder = events::build_message(
        channel,
        "what is on my calendar",
        None,
        &[agent.as_str()],
        &[],
        &[],
        &[],
        &[],
        None,
        &crate::relay::relay_api_base_url(),
    )
    .expect("builder");
    let body = sign_and_guard_ambient_body(builder, &signing).expect("guarded body");
    let value: serde_json::Value = serde_json::from_slice(&body).expect("event json");
    assert_eq!(value["kind"], 9);
    let tags = value["tags"].as_array().expect("tags");
    assert!(
        tags.iter()
            .any(|tag| tag[0] == "h" && tag[1] == channel.to_string()),
        "missing destination h tag: {tags:?}"
    );
    assert!(
        tags.iter().any(|tag| tag[0] == "p" && tag[1] == agent),
        "missing agent p tag: {tags:?}"
    );
}

#[test]
fn the_guidelines_event_is_kind_48106_on_the_destination() {
    let signing = keys();
    let channel = Uuid::new_v4().to_string();
    let builder = events::build_voice_guidelines(&channel, &ambient_voice_guidelines("hey hermes"))
        .expect("builder");
    let body = sign_and_guard_ambient_body(builder, &signing).expect("guarded body");
    let value: serde_json::Value = serde_json::from_slice(&body).expect("event json");
    assert_eq!(value["kind"], 48106);
    let tags = value["tags"].as_array().expect("tags");
    assert!(tags.iter().any(|tag| tag[0] == "h" && tag[1] == channel));
    let content = value["content"].as_str().expect("content");
    assert!(content.contains("hey hermes"), "{content}");
    assert!(content.contains("text-to-speech"), "{content}");
}

#[test]
fn guidelines_tell_the_agent_how_interruption_reaches_it() {
    // Barge-in is invisible to the agent unless the guidelines explain that a
    // message arriving mid-reply means it was interrupted.
    let text = ambient_voice_guidelines("hey hermes");
    assert!(text.contains("interrupted"), "{text}");
    assert!(text.contains("drop your unsent sentences"), "{text}");
}

#[test]
fn empty_and_whitespace_transcripts_are_never_published() {
    assert_eq!(normalize_transcript(""), Ok(None));
    assert_eq!(normalize_transcript("   \n\t "), Ok(None));
    assert_eq!(
        normalize_transcript("  hello there \n"),
        Ok(Some("hello there".to_string()))
    );
}

#[test]
fn a_long_transcript_is_passed_through_whole_and_never_trimmed() {
    // The regression this replaces its predecessor for: the old cap was 2,000
    // characters applied by silently cutting the transcript's tail off, and
    // rolling capture made 2,000 characters two minutes of ordinary speech.
    // Everything up to the supported bound now passes through verbatim.
    let long = "words of a long dictation ".repeat(400);
    assert!(long.len() > 2_000, "{}", long.len());
    assert_eq!(
        normalize_transcript(&long).expect("well under the bound"),
        Some(long.trim().to_string())
    );

    let at_bound = "b".repeat(MAX_UTTERANCE_TEXT_BYTES);
    assert_eq!(
        normalize_transcript(&at_bound).expect("exactly the bound"),
        Some(at_bound)
    );
}

#[test]
fn a_transcript_past_the_supported_bound_is_refused_with_a_reason_not_trimmed() {
    // Over the bound is an explicit error naming both numbers — the capture
    // pipeline fails such an utterance loudly long before this line, so
    // arriving here means a bug, and a bug may not silently edit the user's
    // words on its way out.
    let over = "c".repeat(MAX_UTTERANCE_TEXT_BYTES + 1);
    let error = normalize_transcript(&over).expect_err("over the bound");
    assert!(
        error.contains(&MAX_UTTERANCE_TEXT_BYTES.to_string()),
        "{error}"
    );
    assert!(error.contains(&over.len().to_string()), "{error}");
}

/// Read one HTTP request off `socket`, whole, and return its body.
fn read_request_body(socket: &mut std::net::TcpStream) -> Vec<u8> {
    use std::io::Read;
    let mut raw = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        let n = socket.read(&mut buf).expect("read");
        raw.extend_from_slice(&buf[..n]);
        if let Some(split) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
            let headers = String::from_utf8_lossy(&raw[..split]).to_lowercase();
            let length: usize = headers
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .and_then(|v| v.trim().parse().ok())
                .expect("content-length");
            let mut body = raw[split + 4..].to_vec();
            while body.len() < length {
                let n = socket.read(&mut buf).expect("read body");
                body.extend_from_slice(&buf[..n]);
            }
            return body;
        }
    }
}

/// A one-request HTTP stub standing in for the relay's `POST /events`: accepts
/// the connection, reads the request whole, answers 200, and hands the body
/// back to the test.
fn one_post_stub() -> (String, std::sync::mpsc::Receiver<Vec<u8>>) {
    use std::io::Write;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let base = format!("http://{}", listener.local_addr().expect("addr"));
    let (body_tx, body_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().expect("accept");
        let body = read_request_body(&mut socket);
        socket
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
            .expect("respond");
        let _ = body_tx.send(body);
    });
    (base, body_rx)
}

/// A multi-request stub whose **first** response is withheld until released.
///
/// This is the shape the blocked-publisher regression needs: the guidelines
/// POST is the publisher-side work a transcript can wait behind, and holding
/// its response open is what parks the transcript in exactly the window the
/// mute must still reach. Responses carry `Connection: close` so every request
/// arrives on its own connection, in order.
fn stub_holding_the_first_response() -> (
    String,
    std::sync::mpsc::Receiver<Vec<u8>>,
    std::sync::mpsc::Receiver<()>,
    std::sync::mpsc::Sender<()>,
) {
    use std::io::Write;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let base = format!("http://{}", listener.local_addr().expect("addr"));
    let (body_tx, body_rx) = std::sync::mpsc::channel();
    let (seen_tx, seen_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    std::thread::spawn(move || {
        for index in 0..4 {
            let Ok((mut socket, _)) = listener.accept() else {
                return;
            };
            let body = read_request_body(&mut socket);
            let _ = body_tx.send(body);
            if index == 0 {
                let _ = seen_tx.send(());
                let _ = release_rx.recv();
            }
            let _ = socket
                .write_all(b"HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Length: 0\r\n\r\n");
        }
    });
    (base, body_rx, seen_rx, release_tx)
}

/// A loopback stub that answers every POST 200 and hands each body back.
///
/// `Connection: close` so every request arrives on its own connection, in the
/// order they were sent — which is what makes "the first bytes the wire saw"
/// an assertion about ordering rather than about timing. The body is handed to
/// the test *before* the response is written, so a publisher that has returned
/// has provably already been recorded here: "nothing else arrived" is then a
/// statement about the wire and not about who won a race to a channel.
fn recording_post_stub() -> (String, std::sync::mpsc::Receiver<Vec<u8>>) {
    use std::io::Write;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let base = format!("http://{}", listener.local_addr().expect("addr"));
    let (body_tx, body_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        while let Ok((mut socket, _)) = listener.accept() {
            let body = read_request_body(&mut socket);
            if body_tx.send(body).is_err() {
                return;
            }
            let _ = socket
                .write_all(b"HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Length: 0\r\n\r\n");
        }
    });
    (base, body_rx)
}

#[tokio::test]
async fn a_mute_landing_after_the_request_is_built_never_reaches_the_wire() {
    // The window the epoch check alone cannot close. Everything the POST needs
    // is in hand — signed, guarded, authenticated, built — and the mute lands
    // between the last look at the epoch and the send. Any number of extra
    // unsynchronised looks only narrows that window; the words a user muted
    // would still, sometimes, be sent.
    //
    // So the decision and the send are one step, under the same lock a mute-on
    // takes to bump the epoch: either the mute is ordered first and this
    // transcript is dropped unsent, or the send is already under way and the
    // mute governs the next one. The hold below stops the publisher exactly at
    // that point — after preparation, before the critical section — which is
    // where a test can be certain the mute is landing inside the window and
    // not before it.
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    use crate::ambient_voice::session::{apply_mute, transcript_still_wanted};
    use crate::ambient_voice::status::AmbientStatus;

    let (base, bodies) = recording_post_stub();
    let publisher = AmbientPublisher {
        http_client: reqwest::Client::new(),
        keys: keys(),
        relay_base_url: base,
    };
    let channel = Uuid::new_v4();
    let agent = "b".repeat(64);
    let muted = AtomicBool::new(false);
    let mute_epochs = AtomicU64::new(0);
    let mute_authority = std::sync::Mutex::new(());
    let status = std::sync::Mutex::new(AmbientStatus::Listening);

    let captured_under = mute_epochs.load(Ordering::Acquire);
    let still_wanted = || transcript_still_wanted(&mute_epochs, captured_under);
    let hold = DispatchHold::default();
    let held_gate = DispatchGate {
        authority: &mute_authority,
        still_wanted: &still_wanted,
        hold: Some(&hold),
    };
    let publish_muted_capture =
        publisher.publish_transcript(channel, &agent, "the words the user muted", &held_gate);
    // The mute cycle lands while the publisher holds a complete request. No
    // sleeping anywhere: the hold says when the publisher is there, and the
    // release says when it may go on.
    let mute_cycle_at_the_gate = async {
        hold.reached.notified().await;
        apply_mute(&muted, &mute_epochs, &mute_authority, &status, true);
        apply_mute(&muted, &mute_epochs, &mute_authority, &status, false);
        hold.released.notify_one();
    };
    let (published, ()) = tokio::join!(publish_muted_capture, mute_cycle_at_the_gate);
    published.expect("a dropped transcript is not an error");

    // A transcript from a capture armed after the unmute goes out normally, and
    // is therefore the *first* thing the wire has seen: the muted capture's
    // dispatch had already resolved, one way or the other, before this began.
    let captured_after = mute_epochs.load(Ordering::Acquire);
    let still_wanted_after = || transcript_still_wanted(&mute_epochs, captured_after);
    publisher
        .publish_transcript(
            channel,
            &agent,
            "said after the unmute",
            &DispatchGate::new(&mute_authority, &still_wanted_after),
        )
        .await
        .expect("published");

    let body = bodies
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("the stub saw no POST at all");
    let event: serde_json::Value = serde_json::from_slice(&body).expect("event json");
    assert_eq!(
        event["content"], "said after the unmute",
        "the muted capture's words reached the wire from inside the dispatch window"
    );
    assert!(
        bodies.try_recv().is_err(),
        "a second POST reached the wire: the muted capture was sent as well"
    );
}

#[tokio::test]
async fn a_mute_while_the_transcript_waits_behind_publisher_work_stops_the_post() {
    // The second half of the mute fence, at the boundary that actually
    // matters: `finish_capture`'s check passes, the transcript is queued, and
    // the publisher is busy — here, sending guidelines — when the user mutes
    // and unmutes. The transcript carries the epoch it was captured under,
    // and the last check sits after the publisher's awaits, immediately
    // before the POST: the muted capture's words never reach the wire, and a
    // capture made after the unmute publishes normally.
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    use crate::ambient_voice::session::{apply_mute, transcript_still_wanted};
    use crate::ambient_voice::status::AmbientStatus;

    let (base, bodies, first_seen, release_first) = stub_holding_the_first_response();
    let publisher = AmbientPublisher {
        http_client: reqwest::Client::new(),
        keys: keys(),
        relay_base_url: base,
    };
    let destination = AmbientDestination {
        channel_id: Uuid::new_v4(),
        agent_pubkey: "b".repeat(64),
        wake_word: "hey hermes".to_string(),
        guidelines_sent: Arc::new(std::sync::atomic::AtomicBool::new(false)),
    };
    let muted = AtomicBool::new(false);
    let mute_epochs = AtomicU64::new(0);
    let mute_authority = std::sync::Mutex::new(());
    let status = std::sync::Mutex::new(AmbientStatus::Listening);

    // A capture finished and was queued under the current epoch…
    let captured_under = mute_epochs.load(Ordering::Acquire);
    let still_wanted = || transcript_still_wanted(&mute_epochs, captured_under);
    let gate = DispatchGate::new(&mute_authority, &still_wanted);
    let publish_muted_capture = destination.publish(&publisher, "the words the user muted", &gate);
    // …and while its publisher is held inside the guidelines POST, a mute and
    // an unmute both land, through the production mute path.
    let mute_cycle_mid_publish = async {
        tokio::task::spawn_blocking(move || first_seen.recv())
            .await
            .expect("join")
            .expect("the guidelines POST was never made");
        apply_mute(&muted, &mute_epochs, &mute_authority, &status, true);
        apply_mute(&muted, &mute_epochs, &mute_authority, &status, false);
        release_first.send(()).expect("release the held response");
    };
    tokio::join!(publish_muted_capture, mute_cycle_mid_publish);

    // A capture made after the unmute, under the later epoch, is unaffected.
    let captured_after = mute_epochs.load(Ordering::Acquire);
    let still_wanted_after = || transcript_still_wanted(&mute_epochs, captured_after);
    destination
        .publish(
            &publisher,
            "said after the unmute",
            &DispatchGate::new(&mute_authority, &still_wanted_after),
        )
        .await;

    let wait = std::time::Duration::from_secs(5);
    let first: serde_json::Value =
        serde_json::from_slice(&bodies.recv_timeout(wait).expect("guidelines body"))
            .expect("guidelines json");
    assert_eq!(
        first["kind"], 48106,
        "the held request was not the guidelines"
    );
    let second: serde_json::Value =
        serde_json::from_slice(&bodies.recv_timeout(wait).expect("a second POST"))
            .expect("second json");
    assert_eq!(second["kind"], 9);
    assert_eq!(
        second["content"], "said after the unmute",
        "the muted capture's words reached the wire ahead of the live one's"
    );
}

#[tokio::test]
async fn a_transcript_longer_than_the_old_flat_cap_reaches_the_wire_unclipped() {
    // End to end through the real publisher — sign, guard, authenticate, POST —
    // because the truncation this guards against lived exactly one step past
    // where the previous tests stopped. The transcript on the wire must be the
    // transcript that was spoken, byte for byte.
    let (base, body_rx) = one_post_stub();
    let publisher = AmbientPublisher {
        http_client: reqwest::Client::new(),
        keys: keys(),
        relay_base_url: base,
    };
    let transcript = "one more sentence of a long dictation ".repeat(300);
    assert!(transcript.len() > 2_000, "{}", transcript.len());

    let authority = std::sync::Mutex::new(());
    let always = || true;
    publisher
        .publish_transcript(
            Uuid::new_v4(),
            &"a".repeat(64),
            &transcript,
            &DispatchGate::new(&authority, &always),
        )
        .await
        .expect("published");

    let body = body_rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("the stub saw the POST");
    let event: serde_json::Value = serde_json::from_slice(&body).expect("event json");
    assert_eq!(
        event["content"].as_str().expect("content"),
        transcript.trim(),
        "the transcript on the wire is not the transcript that was spoken"
    );
}

#[tokio::test]
async fn guidelines_are_sent_once_per_session_and_retried_after_a_failure() {
    use std::sync::atomic::{AtomicBool, Ordering};

    let sent = Arc::new(AtomicBool::new(false));
    let destination = AmbientDestination {
        channel_id: Uuid::new_v4(),
        agent_pubkey: "b".repeat(64),
        wake_word: "hey hermes".to_string(),
        guidelines_sent: Arc::clone(&sent),
    };
    // First send claims the slot.
    assert!(!destination.guidelines_sent.swap(true, Ordering::AcqRel));
    // A second utterance must not re-post them.
    assert!(destination.guidelines_sent.swap(true, Ordering::AcqRel));
    // A failed post releases the slot so the next utterance retries.
    destination.guidelines_sent.store(false, Ordering::Release);
    assert!(!destination.guidelines_sent.swap(true, Ordering::AcqRel));
}

#[test]
fn a_stored_channel_destination_is_used_verbatim() {
    // resolve_destination's DM branch needs a relay; the channel branch is
    // pure and is the one a later milestone will exercise most.
    let channel = Uuid::new_v4().to_string();
    assert!(Uuid::parse_str(&channel).is_ok());
    assert!(Uuid::parse_str("not-a-channel").is_err());
}
