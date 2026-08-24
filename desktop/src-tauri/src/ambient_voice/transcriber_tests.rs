//! Transcriber tests.
//!
//! The server path runs against the loopback stub, so an utterance really is
//! encoded, posted and answered. The on-device path needs the ~600 MB speech
//! model, which is downloaded at runtime and is not in the repository — so the
//! tests that need a working recogniser are `#[ignore]`d and read the model
//! directory from `BUZZ_AMBIENT_STT_MODEL_DIR`, exactly as the keyword-spotter
//! fixtures do:
//!
//! ```text
//! BUZZ_AMBIENT_STT_MODEL_DIR=/path/to/sherpa-onnx-nemo-parakeet-… \
//!   cargo test --lib ambient_voice::transcriber -- --ignored --nocapture
//! ```
//!
//! What that leaves ungated in CI is the *choice* an HTTP failure makes, which
//! is why that choice is a function of its own rather than a branch inside the
//! request path.

use std::time::Duration;

use super::*;
use crate::ambient_voice::speech_stub_server::{StubReply, StubSpeechServer};

/// A health handle configured as a running session's would be.
fn health() -> Arc<RoleHealth> {
    let health = Arc::new(RoleHealth::default());
    health.configure(true);
    health
}

const MODEL_DIR_ENV: &str = "BUZZ_AMBIENT_STT_MODEL_DIR";

/// A second of quiet 16 kHz audio — enough to be a real upload.
fn utterance() -> Vec<f32> {
    (0..UTTERANCE_SAMPLE_RATE as usize)
        .map(|i| (i as f32 / 50.0).sin() * 0.2)
        .collect()
}

#[test]
fn a_configured_server_transcribes_the_utterance() {
    // Acceptance criterion (a) below the wake word: with STT pointed at a
    // server, what the user said comes back through it.
    let server = StubSpeechServer::always(StubReply::json(r#"{"text": "book me a room"}"#));
    let dir = tempfile::tempdir().expect("temp dir");
    let transcriber =
        Transcriber::build(dir.path(), Some(server.base_url()), health()).expect("transcriber");
    assert!(
        matches!(transcriber, Transcriber::Http { local: None, .. }),
        "no speech model is installed here, so the server has no fallback to keep"
    );

    assert_eq!(
        transcriber.transcribe(&utterance()).expect("transcribe"),
        "book me a room"
    );
    let requests = server.wait_for_requests(1, Duration::from_secs(5));
    assert_eq!(
        requests.first().map(|request| request.path.as_str()),
        Some("/v1/audio/transcriptions")
    );
}

#[test]
fn a_failing_server_with_nothing_to_fall_back_to_reports_the_failure() {
    // Criterion (d), the half that is reachable without the model: the
    // utterance is lost, and the indicator must say so rather than sit on
    // "listening for the wake word" as though nothing had been said.
    let server = StubSpeechServer::always(StubReply::status(502, "upstream is down"));
    let dir = tempfile::tempdir().expect("temp dir");
    let transcriber =
        Transcriber::build(dir.path(), Some(server.base_url()), health()).expect("transcriber");

    let error = transcriber
        .transcribe(&utterance())
        .expect_err("a 502 with no local model");
    assert!(error.contains("no speech model installed"), "{error}");
    assert!(
        error.contains("502"),
        "the server's own words survive: {error}"
    );
}

#[test]
fn what_the_server_did_is_recorded_whether_or_not_anything_fell_back() {
    // The fallback is deliberately quiet — the sentence still reaches the
    // agent — and that quiet is exactly what left a user unable to tell a
    // working server from a broken one. Nothing here changes the fallback; it
    // records the server's own answer beside it, and the pill reads this.
    let health = health();
    let failing = StubSpeechServer::always(StubReply::status(502, "upstream is down"));
    let dir = tempfile::tempdir().expect("temp dir");
    let transcriber = Transcriber::build(dir.path(), Some(failing.base_url()), Arc::clone(&health))
        .expect("transcriber");

    let _ = transcriber.transcribe(&utterance());
    let snapshot = health.snapshot_for_test();
    assert!(snapshot.failing, "a 502 left the server looking healthy");
    assert_eq!(snapshot.consecutive_failures, 1);
    assert!(
        snapshot
            .last_error
            .is_some_and(|error| error.contains("502")),
        "the server's own words were not kept"
    );

    // And a server that answers clears it, so the line describes now rather
    // than ever.
    let answering = StubSpeechServer::always(StubReply::json(r#"{"text": "book me a room"}"#));
    let transcriber =
        Transcriber::build(dir.path(), Some(answering.base_url()), Arc::clone(&health))
            .expect("transcriber");
    transcriber.transcribe(&utterance()).expect("transcribe");
    assert!(!health.snapshot_for_test().failing);
}

#[test]
fn an_address_that_cannot_be_used_at_all_is_reported_like_any_other_failure() {
    // A URL the client cannot even be pointed at is the *permanent* version of
    // a failing server, and it was the one version nothing recorded: the
    // request path is where failures are counted, and this never reaches it.
    // The role sat at "configured, not failing" for the whole session while
    // every utterance was quietly decoded on this computer.
    for unusable in [
        "not a url at all",
        "speech.example:30120", // no scheme — the most common thing to type
        "ftp://speech.example",
    ] {
        let health = health();
        let dir = tempfile::tempdir().expect("temp dir");
        // No speech model here either, so the session cannot start — what
        // matters is that the answer was recorded before that was decided.
        let _ = Transcriber::build(dir.path(), Some(unusable), Arc::clone(&health));

        let snapshot = health.snapshot_for_test();
        assert!(
            snapshot.failing,
            "{unusable:?} left the role looking healthy"
        );
        assert!(
            snapshot.last_error.is_some(),
            "{unusable:?} was recorded with nothing to explain it"
        );
    }
}

#[test]
fn a_server_failure_falls_back_per_utterance_when_a_recogniser_exists() {
    // The rule the fallback rests on, pinned without the model: an installed
    // recogniser answers this utterance, and its absence is what turns the
    // server's failure into something the user is told about. Nothing here
    // switches the session's backend — the user chose a server, and a client
    // that quietly stopped using it would leave them unable to tell a working
    // server from a broken one.
    assert_eq!(
        fallback_after(Some("the on-device recogniser"), "HTTP 502".to_string()),
        Ok("the on-device recogniser")
    );
    let error = fallback_after(None::<&str>, "HTTP 502".to_string()).expect_err("no fallback");
    assert!(error.contains("HTTP 502"), "{error}");
}

#[test]
fn an_address_that_cannot_be_used_keeps_the_session_on_the_local_model() {
    // A typo in a URL must not cost the user their wake word. With no model
    // installed there is nothing to degrade to, so the session start fails and
    // names both faults; the worker turns that into the error the pill shows.
    let dir = tempfile::tempdir().expect("temp dir");
    let Err(error) = Transcriber::build(dir.path(), Some("speech.example:30120"), health()) else {
        panic!("a bare host:port with no model installed must not build a transcriber");
    };
    assert!(error.contains("must start with http"), "{error}");
    assert!(
        error.contains("on-device speech model is unavailable"),
        "{error}"
    );
}

#[test]
fn no_endpoint_means_the_local_recogniser_and_its_absence_is_fatal() {
    // Unchanged M1 behaviour: with no server configured, a missing model is
    // the end of the session, and the worker reports it.
    let dir = tempfile::tempdir().expect("temp dir");
    let Err(error) = Transcriber::build(dir.path(), None, health()) else {
        panic!("a session with neither a model nor a server must not start");
    };
    assert!(error.contains("speech-to-text model not found"), "{error}");
}

// ── Through the real recogniser ──────────────────────────────────────────────

fn model_dir_from_env() -> std::path::PathBuf {
    let dir = std::env::var(MODEL_DIR_ENV).unwrap_or_else(|_| {
        panic!("set {MODEL_DIR_ENV} to a sherpa-onnx speech model directory to run this test")
    });
    let dir = std::path::PathBuf::from(dir);
    assert!(dir.is_dir(), "{MODEL_DIR_ENV} is not a directory: {dir:?}");
    dir
}

#[test]
#[ignore = "needs the downloaded speech model; set BUZZ_AMBIENT_STT_MODEL_DIR"]
fn a_server_failure_is_answered_by_the_installed_recogniser() {
    // The whole of criterion (d) for STT, with a real model: the server is
    // configured and failing, and the utterance is still transcribed here.
    let server = StubSpeechServer::always(StubReply::status(503, "model is loading"));
    let transcriber = Transcriber::build(&model_dir_from_env(), Some(server.base_url()), health())
        .expect("transcriber");
    assert!(
        matches!(transcriber, Transcriber::Http { local: Some(_), .. }),
        "the local recogniser must be kept even when a server is configured"
    );
    transcriber
        .transcribe(&utterance())
        .expect("the local recogniser answers a failed server");
}
