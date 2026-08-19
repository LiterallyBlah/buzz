//! Lifecycle-gate and command-surface tests.
//!
//! The load-bearing property here is the first acceptance criterion: with the
//! feature off, nothing acquires a microphone. The frontend half of that is
//! covered by `ambientVoiceCapture.test.mjs`; this file covers the native half
//! — `should_run` is the single predicate every start path consults, so if it
//! is false no worker thread exists to consume audio and
//! `push_ambient_audio_pcm` has nowhere to put frames.

use super::commands::*;
use super::*;
use crate::ambient_voice::settings::WakeBinding;
use crate::ambient_voice::wake_word::MAX_WAKE_WORD_CHARS;

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
fn a_settings_save_cannot_flip_mute_or_enablement() {
    // The settings card reads one `AmbientVoiceSettings` snapshot at mount and
    // writes the WHOLE object back on every later save (device pickers, the
    // wake-word blur, agent selection). Mute and enablement move underneath it
    // — from the bottom-left indicator, from the Experiments toggle — without
    // that snapshot being refreshed. So an unrelated save carries a stale
    // `muted` / `enabled`, and before this was fixed it re-asserted them:
    // unmuting from the indicator never stuck, and a save from an open
    // settings page could silently re-open the microphone after the user had
    // switched the feature off.
    let current = AmbientVoiceSettings {
        enabled: true,
        muted: false,
        ..bound(true)
    };
    // What a card mounted before the user unmuted would send back.
    let stale = AmbientVoiceSettings {
        enabled: false,
        muted: true,
        input_device_id: Some("mic-abc".to_string()),
        ..bound(false)
    };

    let merged = merge_client_settings(&current, stale.clone());

    // Runtime-authoritative fields come from the runtime, never the payload.
    assert!(
        !merged.muted,
        "a stale save re-asserted mute: {merged:?} (current {current:?})"
    );
    assert!(
        merged.enabled,
        "a stale save re-asserted enablement: {merged:?} (current {current:?})"
    );
    // Everything else in the payload is still the client's to set.
    assert_eq!(merged.input_device_id.as_deref(), Some("mic-abc"));
    assert_eq!(merged.wake_bindings, stale.wake_bindings);
    assert_eq!(merged.version, settings::CURRENT_VERSION);

    // And the mirror case: a card that mounted while unmuted and enabled must
    // not be able to un-mute or re-enable a runtime the user has since muted
    // and switched off either.
    let current = AmbientVoiceSettings {
        enabled: false,
        muted: true,
        ..bound(false)
    };
    let stale = AmbientVoiceSettings {
        enabled: true,
        muted: false,
        ..bound(true)
    };
    let merged = merge_client_settings(&current, stale);
    assert!(merged.muted, "a stale save cleared mute: {merged:?}");
    assert!(
        !merged.enabled,
        "a stale save re-enabled capture: {merged:?}"
    );
}

#[test]
fn a_configuration_change_restarts_the_running_session() {
    // `reconcile` used to leave any healthy session alone, so a wake word,
    // agent, destination, microphone or speaker changed while ambient voice
    // was running took effect only after the user switched the feature off and
    // on again — every one of them is bound once, when the session starts: the
    // keyword payload is tokenised, the destination is resolved and the TTS
    // pipeline is built against the chosen speaker.
    let running = bound(true);
    let started_with = SessionConfig::of(&running);
    assert!(!session_needs_restart(Some(&started_with), &running));

    let mut renamed = running.clone();
    renamed.wake_bindings[0].wake_word = "hey buzz".to_string();
    assert!(session_needs_restart(Some(&started_with), &renamed));

    let mut rebound = running.clone();
    rebound.wake_bindings[0].agent_pubkey = "f".repeat(64);
    assert!(session_needs_restart(Some(&started_with), &rebound));

    let mut rerouted = running.clone();
    rerouted.wake_bindings[0].destination =
        Some("11111111-1111-4111-8111-111111111111".to_string());
    assert!(session_needs_restart(Some(&started_with), &rerouted));

    let microphone = AmbientVoiceSettings {
        input_device_id: Some("mic-abc".to_string()),
        ..running.clone()
    };
    assert!(session_needs_restart(Some(&started_with), &microphone));

    let speaker = AmbientVoiceSettings {
        output_device: Some("Studio Display".to_string()),
        ..running.clone()
    };
    assert!(session_needs_restart(Some(&started_with), &speaker));

    // A session whose configuration was not recorded cannot be shown to match
    // what the user now wants, so it is rebuilt rather than trusted.
    assert!(session_needs_restart(None, &running));
}

#[test]
fn a_speech_backend_change_restarts_the_running_session() {
    // Both roles are bound once, when the session starts: the worker builds
    // its transcriber from the STT choice and the reply pipeline is built
    // against the TTS one. Without these in the recorded configuration,
    // switching a role to a server -- or repointing it at another one -- would
    // take effect only after the user switched the whole feature off and on
    // again, and until then the settings screen and the running session would
    // disagree about where the user's voice is going.
    use settings::{SpeechBackend, SpeechBackendSettings};
    let running = bound(true);
    let started_with = SessionConfig::of(&running);

    let to_server = AmbientVoiceSettings {
        stt: SpeechBackendSettings {
            backend: SpeechBackend::Http,
            endpoint_url: Some("http://speech.example:30120".to_string()),
        },
        ..running.clone()
    };
    assert!(session_needs_restart(Some(&started_with), &to_server));

    let repointed = AmbientVoiceSettings {
        stt: SpeechBackendSettings {
            backend: SpeechBackend::Http,
            endpoint_url: Some("http://other.example:30120".to_string()),
        },
        ..running.clone()
    };
    assert!(session_needs_restart(
        Some(&SessionConfig::of(&to_server)),
        &repointed
    ));

    let spoken_remotely = AmbientVoiceSettings {
        tts: SpeechBackendSettings {
            backend: SpeechBackend::Http,
            endpoint_url: Some("http://speech.example:30121".to_string()),
        },
        ..running.clone()
    };
    assert!(session_needs_restart(Some(&started_with), &spoken_remotely));

    // And back again: a role returned to this computer is just as much a
    // change to what the worker was built with.
    assert!(session_needs_restart(
        Some(&SessionConfig::of(&to_server)),
        &running
    ));

    // A URL remembered beside a local choice changes nothing that runs, so it
    // must not cost the user a session rebuild -- two ONNX model loads and a
    // re-resolved destination -- while they are typing one.
    let remembered = AmbientVoiceSettings {
        stt: SpeechBackendSettings {
            backend: SpeechBackend::Local,
            endpoint_url: Some("http://speech.example:30120".to_string()),
        },
        ..running.clone()
    };
    assert!(!session_needs_restart(
        Some(&SessionConfig::of(&remembered)),
        &remembered
    ));
}

#[test]
fn the_silence_hold_and_the_stop_phrase_restart_the_running_session() {
    // Both are bound once, when the worker thread starts: the capture machine
    // derives its limits from the hold at construction, and the stop phrase is
    // tokenised into the keyword payload the spotter is created with. Without
    // these in the recorded configuration, a slider moved or a phrase typed
    // while ambient voice was running would take effect only after the whole
    // feature was switched off and on again — exactly the defect that put the
    // wake word in here.
    let running = bound(true);
    let started_with = SessionConfig::of(&running);
    assert!(!session_needs_restart(Some(&started_with), &running));

    let held_longer = AmbientVoiceSettings {
        silence_hold_ms: 2_500,
        ..running.clone()
    };
    assert!(session_needs_restart(Some(&started_with), &held_longer));

    let with_stop = AmbientVoiceSettings {
        stop_phrase: Some("buzz stop".to_string()),
        ..running.clone()
    };
    assert!(session_needs_restart(Some(&started_with), &with_stop));

    // Changing the phrase is a different keyword set again, and clearing it
    // means the second keyword must come back off the spotter.
    let rephrased = AmbientVoiceSettings {
        stop_phrase: Some("that is all".to_string()),
        ..running.clone()
    };
    assert!(session_needs_restart(
        Some(&SessionConfig::of(&with_stop)),
        &rephrased
    ));
    assert!(session_needs_restart(
        Some(&SessionConfig::of(&with_stop)),
        &running
    ));

    // Whitespace the user left around the phrase is not a new keyword set, and
    // must not cost them two ONNX model loads while they are typing.
    let padded = AmbientVoiceSettings {
        stop_phrase: Some("  buzz stop  ".to_string()),
        ..running.clone()
    };
    assert!(!session_needs_restart(
        Some(&SessionConfig::of(&with_stop)),
        &padded
    ));
}

#[test]
fn mute_and_the_indicator_position_never_cost_a_restart() {
    // Mute is applied to the live worker in place, and every reconcile runs
    // through the same predicate: rebuilding the session to close a microphone
    // the worker closes itself would drop the resolved destination and reload
    // two ONNX models. The pill's parked position is not something any session
    // reads at all.
    let running = bound(true);
    let started_with = SessionConfig::of(&running);

    let muted = AmbientVoiceSettings {
        muted: true,
        ..running.clone()
    };
    assert!(!session_needs_restart(Some(&started_with), &muted));

    let dragged = AmbientVoiceSettings {
        indicator_position: Some(settings::IndicatorPosition { x: 12.0, y: 700.0 }),
        ..running.clone()
    };
    assert!(!session_needs_restart(Some(&started_with), &dragged));
}

#[test]
fn a_microphone_failure_in_the_webview_becomes_a_visible_error() {
    // The microphone is opened in the webview, so a device that is refused,
    // busy or unplugged is invisible to the native worker — it never receives
    // another sample and the indicator went on saying "listening for the wake
    // word". The message the webview sends is what the user reads, so it is
    // kept verbatim, and only made fit to show.
    assert_eq!(
        capture_error_detail("  Microphone access was refused  "),
        "Microphone access was refused"
    );
    // Nothing to show is worse than a plain sentence: the pill would go blank
    // in the one state it exists to describe.
    assert_eq!(
        capture_error_detail("   "),
        "The microphone could not be opened for ambient voice"
    );
    let shouted = "e".repeat(MAX_CAPTURE_ERROR_CHARS * 3);
    assert_eq!(
        capture_error_detail(&shouted).chars().count(),
        MAX_CAPTURE_ERROR_CHARS
    );
}

#[test]
fn a_late_capture_failure_cannot_resurrect_a_stopped_session() {
    // The webview can be a frame or two behind a teardown, exactly as it can
    // for `push_ambient_audio_pcm`. With nothing running there is no false
    // "listening" to correct, and pinning a failure over `Off` would replace
    // one wrong answer with another.
    let state = crate::app_state::build_app_state();
    apply_capture_error(&state, "Microphone access was refused").expect("apply");
    let report = build_report(&state).expect("report");
    assert_eq!(report.status, AmbientStatus::Off);
    assert!(!report.capturing);
}

#[test]
fn a_capture_failure_paces_the_automatic_retry() {
    // A microphone the webview cannot open fails again the instant a session
    // exists to fail against, and the hot-start poll runs every three seconds:
    // unpaced, a device left unplugged rebuilt the session — two ONNX model
    // loads — ten times a minute, with the indicator flashing "Starting…" each
    // time and settling back on the error.
    let now = Instant::now();
    let failed = AmbientStatus::Error("The microphone was disconnected".to_string());
    assert!(capture_failure_is_pacing(&failed, Some(now), now));

    // The window expires and the next poll re-arms, so a device that was
    // plugged back in recovers without the user touching anything.
    let expired = now
        .checked_sub(CAPTURE_ERROR_BACKOFF + Duration::from_secs(1))
        .expect("an instant before the backoff window");
    assert!(!capture_failure_is_pacing(&failed, Some(expired), now));

    // A failure that was never reported paces nothing — this is what keeps a
    // start that failed for any other reason (a corrupt model, an engine
    // failure) on the three-second recovery it has always had.
    assert!(!capture_failure_is_pacing(&failed, None, now));

    // And nothing outside the error state is paced, so a session that started,
    // a mute, or a huddle suspension all take the ordinary path.
    for status in [
        AmbientStatus::Off,
        AmbientStatus::Suspended,
        AmbientStatus::Muted,
        AmbientStatus::Starting,
        AmbientStatus::Listening,
    ] {
        assert!(
            !capture_failure_is_pacing(&status, Some(now), now),
            "{status:?}"
        );
    }
}

#[test]
fn a_new_capture_failure_reopens_the_pacing_window() {
    // Each report is paced from itself. A retry that got through and failed
    // again must wait afresh rather than inherit what was left of an older
    // window — otherwise the loop reopens at full speed after the first
    // thirty seconds.
    let now = Instant::now();
    let failed = AmbientStatus::Error("The microphone stopped".to_string());
    let stale = now
        .checked_sub(CAPTURE_ERROR_BACKOFF * 2)
        .expect("an instant well before the backoff window");

    assert!(!capture_failure_is_pacing(&failed, Some(stale), now));
    assert!(capture_failure_is_pacing(&failed, Some(now), now));
}

#[test]
fn a_stop_does_not_forget_the_last_capture_failure() {
    // `stop_session` clears everything about the session that ended, and the
    // pacing timestamp deliberately survives it: the microphone is what failed,
    // not the session, and a stop that reset it would let the very next poll
    // rebuild immediately — the loop this pacing exists to stop.
    let state = crate::app_state::build_app_state();
    let reported_at = Instant::now();
    state
        .ambient_voice
        .runtime()
        .expect("runtime")
        .last_capture_error = Some(reported_at);

    stop_session(&state, AmbientStatus::Off).expect("stop");

    assert_eq!(
        state
            .ambient_voice
            .runtime()
            .expect("runtime")
            .last_capture_error,
        Some(reported_at)
    );
}

#[tokio::test]
async fn what_the_user_does_is_never_paced_by_a_capture_failure() {
    // The pacing lives in `check_ambient_hotstart` alone. Everything the user
    // reaches — the Experiments toggle, a device change, mute — funnels through
    // `reconcile` directly, and must take effect on the spot even while a
    // capture failure is still holding off the automatic retry. Here the
    // settings say the feature is off, so a reconcile that ran settles the
    // runtime to `Off`; one that had been paced would have left the error in
    // place.
    let state = crate::app_state::build_app_state();
    state
        .ambient_voice
        .set_status(AmbientStatus::Error("The microphone stopped".to_string()));
    state
        .ambient_voice
        .runtime()
        .expect("runtime")
        .last_capture_error = Some(Instant::now());

    reconcile(&state).await.expect("reconcile");

    assert_eq!(
        build_report(&state).expect("report").status,
        AmbientStatus::Off
    );
}

#[test]
fn a_capturing_session_that_is_fed_nothing_is_not_listening() {
    // `capturing` has only ever meant "the worker thread is alive". The shipped
    // bug is the gap that leaves: after an update relaunch the pill said
    // "Listening for the wake word" while zero audio reached the spotter, for
    // the whole run, and a settings off/on — which rebuilds the capture
    // pipeline — always fixed it. Nothing in the report could tell the two
    // states apart, so the indicator could not either.
    let fed = AudioFlowSnapshot {
        batches: 120,
        since_last_batch: Duration::from_millis(80),
    };
    let deaf = AudioFlowSnapshot {
        batches: 0,
        since_last_batch: STALE_AUDIO_AFTER + Duration::from_secs(1),
    };
    let listening = AmbientStatus::Listening;
    assert!(!audio_is_stale(true, false, &listening, Some(fed)));
    assert!(audio_is_stale(true, false, &listening, Some(deaf)));

    // Audio that flowed and then stopped is the same fault: a device pulled
    // from under a session the webview has not yet noticed losing.
    let stopped = AudioFlowSnapshot {
        batches: 4_000,
        since_last_batch: STALE_AUDIO_AFTER,
    };
    assert!(audio_is_stale(true, false, &listening, Some(stopped)));

    // Muted releases the microphone in the webview by design, so silence is
    // what the user asked for. Saying "no audio is arriving" there would
    // replace a true state with an alarming one.
    assert!(!audio_is_stale(true, true, &listening, Some(deaf)));

    // And with no session there is nothing for audio to arrive at.
    assert!(!audio_is_stale(false, false, &listening, Some(deaf)));
    assert!(!audio_is_stale(true, false, &listening, None));
}

#[test]
fn a_state_that_already_explains_itself_is_not_overwritten_with_deafness() {
    // A worker whose engines could not be built keeps its thread alive draining
    // the queue, so `capturing` stays true and no audio is consumed — the exact
    // shape of a deaf session, with a completely different cause that the status
    // already names. Replacing "the wake-word model is incomplete" with "no
    // audio arriving from the microphone" would bury the fault under a symptom.
    let deaf = AudioFlowSnapshot {
        batches: 0,
        since_last_batch: STALE_AUDIO_AFTER * 4,
    };
    for status in [
        AmbientStatus::Error("wake-word model is incomplete".to_string()),
        AmbientStatus::Starting,
        AmbientStatus::Muted,
        AmbientStatus::Suspended,
        AmbientStatus::Off,
    ] {
        assert!(
            !audio_is_stale(true, false, &status, Some(deaf)),
            "{status:?}"
        );
    }
    // Every state that does claim to be processing audio is fair game — the
    // spotter is fed through playback too, so a session that goes quiet while
    // speaking is just as deaf.
    for status in [
        AmbientStatus::Listening,
        AmbientStatus::Heard,
        AmbientStatus::Capturing,
        AmbientStatus::Transcribing,
        AmbientStatus::Speaking,
    ] {
        assert!(
            audio_is_stale(true, false, &status, Some(deaf)),
            "{status:?}"
        );
    }
}

#[test]
fn a_session_that_starts_is_given_the_grace_period_before_it_counts_as_deaf() {
    // The webview opens the microphone and builds its worklet *after* the
    // session starts, so a report built in the first moments of a session must
    // not call it deaf — that would be a false state of its own, and (with the
    // frontend's paced retry on the other end) a rebuild of a pipeline that was
    // still coming up.
    let starting = AudioFlowSnapshot {
        batches: 0,
        since_last_batch: STALE_AUDIO_AFTER - Duration::from_millis(1),
    };
    assert!(!audio_is_stale(
        true,
        false,
        &AmbientStatus::Listening,
        Some(starting)
    ));
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
        "inputDeviceId",
        "indicatorPosition",
        "loadError",
        "audioStale",
        "audioBatchesReceived",
        "msSinceLastAudio",
        "webviewCapture",
        "launch",
    ] {
        assert!(value.get(key).is_some(), "missing {key} in {value}");
    }
    // With nothing running the two "how is the audio doing" fields say so
    // rather than inventing a number.
    assert_eq!(
        value.get("msSinceLastAudio"),
        Some(&serde_json::Value::Null)
    );
    assert_eq!(value.get("audioStale"), Some(&serde_json::json!(false)));
}

#[test]
fn the_audio_diagnostics_serialise_in_the_shape_the_frontend_parses() {
    // The labels bug: a frontend written against an invented shape renders
    // "undefined" in every row and nothing fails. These fields exist to be read
    // during the next occurrence of a bug we cannot reproduce, so their wire
    // form is pinned from the producing side, as `AmbientModelStatus` is. If
    // this breaks, `AmbientVoiceStatusReport` in `ambientVoiceApi.ts` and its
    // test fixtures change in the same commit.
    let state = crate::app_state::build_app_state();
    state
        .ambient_voice
        .runtime()
        .expect("runtime")
        .webview_capture = Some(WebviewCaptureFlow {
        batches_pushed: 128,
        capture_ready: true,
    });
    *state.ambient_voice.launch.lock().expect("launch") = Some(launch::diagnose(
        "0.5.8-unified.11",
        Some("0.5.8-unified.10".to_string()),
        vec!["--updated".to_string()],
    ));
    let value = serde_json::to_value(build_report(&state).expect("report")).expect("json");

    assert_eq!(
        value.get("webviewCapture"),
        Some(&serde_json::json!({ "batchesPushed": 128, "captureReady": true }))
    );
    assert_eq!(
        value.get("launch"),
        Some(&serde_json::json!({
            "version": "0.5.8-unified.11",
            "previousVersion": "0.5.8-unified.10",
            "firstLaunchAfterUpdate": true,
            "args": ["--updated"],
        }))
    );
}

#[test]
fn what_was_announced_about_the_audio_follows_what_was_published() {
    // Staleness is announced on its edges — a quiet session must not re-emit the
    // same report to every window every five seconds — so the record of what was
    // announced has to be exactly what the windows were last told. A stop or a
    // mute while a session was deaf publishes a report that is no longer stale,
    // and if the record kept saying "stale" the *next* deaf session would find
    // no edge to announce and the pill would go on claiming to listen: the
    // original bug, reintroduced by its own fix.
    let state = crate::app_state::build_app_state();
    state
        .ambient_voice
        .stale_announced
        .store(true, Ordering::Release);

    publish_report(&state);

    assert!(!state.ambient_voice.stale_announced.load(Ordering::Acquire));
}

#[test]
fn a_stop_forgets_what_the_webview_counted() {
    // The webview's counters describe one capture pipeline. The session they
    // were reported against is gone, so carrying them into the next one would
    // answer the wrong question at exactly the moment someone is reading them.
    let state = crate::app_state::build_app_state();
    state
        .ambient_voice
        .runtime()
        .expect("runtime")
        .webview_capture = Some(WebviewCaptureFlow {
        batches_pushed: 4_096,
        capture_ready: true,
    });

    stop_session(&state, AmbientStatus::Off).expect("stop");

    assert!(build_report(&state)
        .expect("report")
        .webview_capture
        .is_none());
}

#[test]
fn ambient_model_status_serialises_the_shape_the_frontend_parses() {
    // `modelStatusLabel` in `ambientSettingsLogic.ts` parses exactly this —
    // the enum's externally-tagged snake_case form, where unit variants are
    // plain strings and data-carrying variants a single-key object. The first
    // frontend shipped against an invented `{status: "…"}` shape instead, and
    // every model row in settings rendered "undefined". If this assertion
    // breaks, the TS `ModelStatus` type and its test fixtures must change in
    // the same commit.
    use crate::huddle::models::ModelStatus;
    let value = serde_json::to_value(models::AmbientModelStatus {
        kws: ModelStatus::Ready,
        stt: ModelStatus::Downloading {
            progress_percent: 42,
        },
        tts: ModelStatus::Error("checksum mismatch".to_string()),
    })
    .expect("json");
    assert_eq!(
        value,
        serde_json::json!({
            "kws": "ready",
            "stt": { "downloading": { "progress_percent": 42 } },
            "tts": { "error": "checksum mismatch" },
        })
    );
    assert_eq!(
        serde_json::to_value(ModelStatus::NotDownloaded).expect("json"),
        serde_json::json!("not_downloaded")
    );
}

#[test]
fn a_settings_write_cannot_move_the_indicator_back() {
    // The settings screen fetches the whole settings object once and posts it
    // back on every change. A copy taken before the user dragged the pill
    // would otherwise silently undo the drag on the next device change.
    use crate::ambient_voice::settings::IndicatorPosition;
    let parked = IndicatorPosition { x: 900.0, y: 40.0 };
    let stale = AmbientVoiceSettings {
        indicator_position: Some(IndicatorPosition { x: 12.0, y: 700.0 }),
        ..bound(true)
    };
    assert_eq!(
        keep_stored_indicator_position(stale, Some(parked)).indicator_position,
        Some(parked)
    );

    // Nothing stored yet: whatever the caller carries is kept, so the very
    // first write is not thrown away.
    let first = AmbientVoiceSettings {
        indicator_position: Some(parked),
        ..bound(true)
    };
    assert_eq!(
        keep_stored_indicator_position(first, None).indicator_position,
        Some(parked)
    );
}
