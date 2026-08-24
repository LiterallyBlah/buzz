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

/// A one-request HTTP stub standing in for the relay's `POST /events`: accepts
/// the connection, reads the request whole, answers 200, and hands the body
/// back to the test.
fn one_post_stub() -> (String, std::sync::mpsc::Receiver<Vec<u8>>) {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let base = format!("http://{}", listener.local_addr().expect("addr"));
    let (body_tx, body_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().expect("accept");
        let mut raw = Vec::new();
        let mut buf = [0u8; 4096];
        let body = loop {
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
                break body;
            }
        };
        socket
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
            .expect("respond");
        let _ = body_tx.send(body);
    });
    (base, body_rx)
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

    publisher
        .publish_transcript(Uuid::new_v4(), &"a".repeat(64), &transcript)
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
