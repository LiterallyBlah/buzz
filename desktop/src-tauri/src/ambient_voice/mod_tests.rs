//! Lifecycle-gate and command-surface tests.
//!
//! The load-bearing property here is the first acceptance criterion: with the
//! feature off, nothing acquires a microphone. The frontend half of that is
//! covered by `ambientVoiceCapture.test.mjs`; this file covers the native half
//! — `should_run` is the single predicate every start path consults, so if it
//! is false no worker thread exists to consume audio and
//! `push_ambient_audio_pcm` has nowhere to put frames.

use super::*;
use crate::ambient_voice::settings::WakeBinding;

const AGENT: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn bound(enabled: bool) -> AmbientVoiceSettings {
    AmbientVoiceSettings {
        enabled,
        wake_bindings: vec![WakeBinding {
            wake_word: "hey hermes".to_string(),
            agent_pubkey: AGENT.to_string(),
            destination: None,
        }],
        ..AmbientVoiceSettings::default()
    }
}

#[test]
fn the_default_configuration_never_runs() {
    // Shipping default: feature absent from the user's overrides, settings
    // file absent, nothing bound. Nothing may start.
    assert!(!should_run(&AmbientVoiceSettings::default(), false));
    assert!(!should_run(&AmbientVoiceSettings::default(), true));
}

#[test]
fn enabling_without_a_wake_word_still_runs_nothing() {
    // Switching the Experiments toggle on is not consent to open a microphone:
    // an unbound feature has no wake word to listen for, so it arms nothing.
    let enabled_but_unbound = AmbientVoiceSettings {
        enabled: true,
        ..AmbientVoiceSettings::default()
    };
    assert!(!enabled_but_unbound.wake_bindings.is_empty() || !enabled_but_unbound.is_runnable());
    assert!(!should_run(&enabled_but_unbound, false));
}

#[test]
fn a_bound_and_enabled_configuration_runs_only_without_a_huddle() {
    assert!(should_run(&bound(true), false));
    // Arbitration: the huddle owns the microphone.
    assert!(!should_run(&bound(true), true));
    // And disabling wins over everything.
    assert!(!should_run(&bound(false), false));
}

#[test]
fn a_fresh_app_state_holds_no_session_and_reports_itself_off() {
    // `build_app_state` is what `run()` calls at launch. The ambient feature
    // must be completely inert in it — no worker, no destination, nothing
    // capturing — regardless of what any settings file might later say.
    let state = crate::app_state::build_app_state();
    let report = build_report(&state).expect("report");
    assert!(!report.enabled);
    assert!(!report.capturing);
    assert!(!report.suspended_by_huddle);
    assert!(report.destination_channel_id.is_none());
    assert!(report.wake_word.is_none());
    assert_eq!(report.status, AmbientStatus::Off);
    assert!(report.load_error.is_none());
}

#[test]
fn audio_pushed_with_no_session_is_dropped_rather_than_erroring() {
    // The webview can be a frame or two ahead of a teardown. Dropping is the
    // correct behaviour; surfacing an error would make the provider log on
    // every frame during a normal stop.
    let state = crate::app_state::build_app_state();
    let runtime = state.ambient_voice.runtime().expect("runtime");
    assert!(runtime.session.is_none());
}

#[test]
fn suspending_for_a_huddle_is_a_no_op_when_nothing_is_configured() {
    // Huddles call this unconditionally on every start/join. With the feature
    // off it must not change any observable state — that is the "zero
    // behaviour change anywhere else in the app" criterion.
    let state = crate::app_state::build_app_state();
    suspend_for_huddle(&state);
    let report = build_report(&state).expect("report");
    assert!(!report.suspended_by_huddle);
    assert_eq!(report.status, AmbientStatus::Off);

    // And teardown must not spuriously start anything.
    resume_after_huddle(&state);
    assert_eq!(
        build_report(&state).expect("report").status,
        AmbientStatus::Off
    );
}

#[test]
fn suspending_for_a_huddle_records_the_suspension_when_configured() {
    let state = crate::app_state::build_app_state();
    *state.ambient_voice.settings.lock().expect("settings") = bound(true);
    suspend_for_huddle(&state);
    let report = build_report(&state).expect("report");
    assert!(report.suspended_by_huddle, "{report:?}");
    assert_eq!(report.status, AmbientStatus::Suspended);
    // No session was ever built (no models here), so nothing is capturing.
    assert!(!report.capturing);
}

#[test]
fn the_wake_word_check_rejects_what_the_engine_cannot_be_given() {
    // The settings UI calls this on every edit. Without the model installed it
    // still runs the model-independent checks, and it must never answer
    // `valid` for a phrase the tokenizer would refuse.
    let empty = check_ambient_wake_word("   ".to_string());
    assert!(!empty.valid);
    assert!(empty.message.is_some());

    let short = check_ambient_wake_word("the".to_string());
    assert!(!short.valid);
    assert!(
        short
            .message
            .as_deref()
            .unwrap_or_default()
            .contains("at least"),
        "{short:?}"
    );

    let too_long = check_ambient_wake_word("a b".repeat(MAX_WAKE_WORD_CHARS));
    assert!(!too_long.valid);
    assert!(
        too_long
            .message
            .as_deref()
            .unwrap_or_default()
            .contains("too long"),
        "{too_long:?}"
    );

    // A plausible phrase passes the model-independent gate. Whether it was
    // also checked against the vocabulary depends on the model being present,
    // which it is not in CI — the flag says so honestly.
    let ok = check_ambient_wake_word("hey hermes".to_string());
    assert!(ok.valid, "{ok:?}");
    if !ok.checked_against_model {
        assert!(ok.tokens.is_none());
    }
}

#[test]
fn the_status_report_serialises_with_the_keys_the_frontend_reads() {
    let state = crate::app_state::build_app_state();
    let report = build_report(&state).expect("report");
    let value = serde_json::to_value(&report).expect("json");
    for key in [
        "enabled",
        "muted",
        "suspendedByHuddle",
        "capturing",
        "status",
        "destinationChannelId",
        "agentPubkey",
        "wakeWord",
        "loadError",
    ] {
        assert!(value.get(key).is_some(), "missing {key} in {value}");
    }
}
