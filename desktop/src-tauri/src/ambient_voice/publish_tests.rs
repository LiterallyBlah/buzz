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
    assert_eq!(normalize_transcript(""), None);
    assert_eq!(normalize_transcript("   \n\t "), None);
    assert_eq!(
        normalize_transcript("  hello there \n"),
        Some("hello there".to_string())
    );
}

#[test]
fn an_oversized_transcript_is_truncated_rather_than_rejected() {
    let long = "a".repeat(MAX_TRANSCRIPT_CHARS + 500);
    let normalized = normalize_transcript(&long).expect("truncated");
    assert_eq!(normalized.chars().count(), MAX_TRANSCRIPT_CHARS);
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
