//! What the user is told about their speech servers.

use super::*;

#[test]
fn a_role_that_runs_on_this_computer_never_reports_a_server_problem() {
    // The default, and the shape of every existing settings file. There is no
    // server, so there is nothing that can be down — and a health line beside
    // a local model would be an invented worry.
    let health = SpeechHealth::default();
    let report = health.report();
    assert!(!report.stt.configured);
    assert!(!report.stt.failing);
    assert!(!report.tts.failing);

    // Even if something did record a failure against an unconfigured role, it
    // is not shown: `configured` is the gate, so a stale write cannot surface.
    health.stt.failed("the server did not answer");
    let report = health.report();
    assert!(!report.stt.failing);
    assert_eq!(report.stt.last_error, None);
}

#[test]
fn a_failing_server_is_visible_and_carries_what_it_said() {
    let health = SpeechHealth::default();
    health.configure(true, false);

    health
        .stt
        .failed("speech server answered HTTP 502: gateway");
    let report = health.report();
    assert!(report.stt.configured);
    assert!(report.stt.failing);
    assert_eq!(report.stt.consecutive_failures, 1);
    assert_eq!(
        report.stt.last_error.as_deref(),
        Some("speech server answered HTTP 502: gateway")
    );

    // A server that keeps failing says how long it has been doing it.
    health.stt.failed("speech server did not answer: timed out");
    assert_eq!(health.report().stt.consecutive_failures, 2);

    // The other role is untouched: two servers, two answers.
    assert!(!health.report().tts.failing);
}

#[test]
fn a_server_that_comes_back_stops_being_complained_about() {
    // This answers "is it failing now", not "has it ever failed". A server
    // that recovers must clear, or the line becomes furniture the user learns
    // to ignore — and the next utterance is what proves it recovered.
    let health = SpeechHealth::default();
    health.configure(true, true);
    health.tts.failed("connection refused");
    assert!(health.report().tts.failing);

    health.tts.succeeded();
    let report = health.report();
    assert!(!report.tts.failing);
    assert_eq!(report.tts.consecutive_failures, 0);
    assert_eq!(report.tts.last_error, None);
    assert!(!report.stt.failing);
}

#[test]
fn record_takes_the_outcome_a_caller_already_has() {
    let health = SpeechHealth::default();
    health.configure(true, false);

    health
        .stt
        .record(&Err::<(), String>("HTTP 500".to_string()));
    assert_eq!(health.report().stt.last_error.as_deref(), Some("HTTP 500"));

    health.stt.record(&Ok::<&str, String>("book me a room"));
    assert!(!health.report().stt.failing);
}

#[test]
fn a_failure_from_a_previous_session_does_not_outlive_it() {
    // `configure` runs at both ends of a session's life. A red line about a
    // server that failed an hour ago, beside a session that has not tried yet,
    // is the same class of lie as a green one beside a broken server.
    let health = SpeechHealth::default();
    health.configure(true, true);
    health.stt.failed("connection refused");
    health.tts.failed("connection refused");

    health.configure(false, false);
    let report = health.report();
    assert!(!report.stt.failing);
    assert!(!report.tts.failing);
    assert_eq!(report.stt.last_error, None);
    assert_eq!(report.tts.last_error, None);
    assert_eq!(report.stt.consecutive_failures, 0);
}

#[test]
fn a_servers_own_words_are_kept_to_one_line() {
    // The message is shown under the status in settings, which is one short
    // line wide, and it comes from a server that may answer with a whole HTML
    // page.
    let health = SpeechHealth::default();
    health.configure(true, false);
    health.stt.failed(&"very long ".repeat(500));

    let error = health.report().stt.last_error.expect("an error");
    assert_eq!(error.chars().count(), MAX_ERROR_CHARS);
}
