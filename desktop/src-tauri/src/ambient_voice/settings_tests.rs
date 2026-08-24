//! Ambient-voice settings load/save/migration tests.
//!
//! Mirrors `huddle::tts_settings`'s test set: defaults, unversioned migration,
//! future-version refusal, and round-tripping of fields a newer client may
//! own. Adds the checks specific to this feature — an empty binding set must
//! stay empty (no default wake phrase), and a malformed binding must not be
//! able to reach the keyword spotter.

use super::*;

const AGENT: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// The KWS model's own vocabulary pair — the same fixture `wake_word_tests`
/// segments against. Used here so the stop phrase's vocabulary rule is pinned
/// to the model this app ships rather than to whatever the machine running the
/// tests happens to have downloaded.
const BPE_MODEL: &[u8] = include_bytes!("../../resources/ambient-voice-test-vocab/bpe.model");
const TOKENS_TXT: &str = include_str!("../../resources/ambient-voice-test-vocab/tokens.txt");

fn fixture_tokenizer() -> WakeWordTokenizer {
    WakeWordTokenizer::from_parts(BPE_MODEL, TOKENS_TXT).expect("load fixture tokenizer")
}

fn binding() -> WakeBinding {
    WakeBinding {
        wake_word: "hey hermes".to_string(),
        agent_pubkey: AGENT.to_string(),
        destination: None,
    }
}

/// Settings carrying `phrase` as the stop phrase, bound to the usual wake word.
fn with_stop_phrase(phrase: &str) -> AmbientVoiceSettings {
    AmbientVoiceSettings {
        enabled: true,
        wake_bindings: vec![binding()],
        stop_phrase: Some(phrase.to_string()),
        ..AmbientVoiceSettings::default()
    }
}

#[test]
fn defaults_are_off_with_no_wake_word() {
    let settings = AmbientVoiceSettings::default();
    assert_eq!(settings.version, CURRENT_VERSION);
    assert!(!settings.enabled);
    assert!(!settings.muted);
    assert!(settings.wake_bindings.is_empty());
    assert_eq!(settings.stt.backend, SpeechBackend::Local);
    assert_eq!(settings.tts.backend, SpeechBackend::Local);
    assert_eq!(settings.silence_hold_ms, DEFAULT_SILENCE_HOLD_MS);
    // No stop phrase by default: the second keyword is opt-in, like the first.
    assert!(settings.stop_phrase.is_none());
    assert!(settings.armed_stop_phrase().is_none());
    assert!(settings.input_device_id.is_none());
    assert!(settings.output_device.is_none());
    // No stored indicator position: the frontend's default corner applies
    // until the user drags the pill somewhere else.
    assert!(settings.indicator_position.is_none());
    assert!(!settings.is_runnable());
}

#[test]
fn a_missing_file_loads_defaults() {
    let dir = tempfile::tempdir().expect("temp dir");
    assert_eq!(
        load_from_path(&dir.path().join(SETTINGS_FILE)).expect("load"),
        AmbientVoiceSettings::default()
    );
}

#[test]
fn settings_survive_a_save_load_round_trip() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join(SETTINGS_FILE);
    let settings = AmbientVoiceSettings {
        version: CURRENT_VERSION,
        enabled: true,
        muted: false,
        wake_bindings: vec![binding()],
        stt: SpeechBackendSettings::default(),
        tts: SpeechBackendSettings::default(),
        silence_hold_ms: 2_500,
        stop_phrase: Some("buzz stop".to_string()),
        input_device_id: Some("mic-abc".to_string()),
        output_device: Some("Speakers (Realtek)".to_string()),
        indicator_position: Some(IndicatorPosition {
            x: 1180.0,
            y: 690.5,
        }),
    };
    save_to_path(&path, &settings).expect("save");
    assert_eq!(load_from_path(&path).expect("load"), settings);
}

#[test]
fn a_dragged_indicator_survives_a_restart() {
    // The pill is dragged in the webview; the only thing that carries it
    // across a restart is this file, and it is read back before the first
    // paint of the indicator.
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join(SETTINGS_FILE);
    let parked = IndicatorPosition { x: 42.0, y: 17.25 };
    save_to_path(
        &path,
        &AmbientVoiceSettings {
            indicator_position: Some(parked),
            ..AmbientVoiceSettings::default()
        },
    )
    .expect("save");
    assert_eq!(
        load_from_path(&path).expect("load").indicator_position,
        Some(parked)
    );

    let value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).expect("read")).expect("json");
    assert_eq!(value["indicatorPosition"]["x"], 42.0);
    assert_eq!(value["indicatorPosition"]["y"], 17.25);
}

#[test]
fn an_unusable_indicator_position_is_forgotten_rather_than_fatal() {
    // A hand-edited file must not cost the user their wake binding, and a
    // non-finite value cannot be encoded at all — it has to be refused where
    // the caller can see it rather than as a JSON error on the whole file.
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join(SETTINGS_FILE);
    std::fs::write(
        &path,
        format!(
            r#"{{"version":1,"enabled":true,"indicatorPosition":null,
                "wakeBindings":[{{"wakeWord":"hey hermes","agentPubkey":"{AGENT}"}}]}}"#
        ),
    )
    .expect("fixture write");
    let settings = load_from_path(&path).expect("load");
    assert!(settings.indicator_position.is_none());
    assert_eq!(settings.wake_bindings, vec![binding()]);

    let error = save_to_path(
        &path,
        &AmbientVoiceSettings {
            indicator_position: Some(IndicatorPosition {
                x: f64::NAN,
                y: 0.0,
            }),
            ..AmbientVoiceSettings::default()
        },
    )
    .expect_err("non-finite position");
    assert!(error.contains("finite pixel offset"), "{error}");
}

#[test]
fn the_persisted_file_uses_the_documented_camel_case_schema() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join(SETTINGS_FILE);
    let settings = AmbientVoiceSettings {
        enabled: true,
        wake_bindings: vec![binding()],
        input_device_id: Some("mic-abc".to_string()),
        ..AmbientVoiceSettings::default()
    };
    save_to_path(&path, &settings).expect("save");
    let value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).expect("read")).expect("json");
    assert_eq!(value["version"], 1);
    assert_eq!(value["enabled"], true);
    assert_eq!(value["wakeBindings"][0]["wakeWord"], "hey hermes");
    assert_eq!(value["wakeBindings"][0]["agentPubkey"], AGENT);
    assert!(value["wakeBindings"][0]["destination"].is_null());
    assert_eq!(value["stt"]["backend"], "local");
    assert_eq!(value["inputDeviceId"], "mic-abc");
}

// ── The silence hold and the stop phrase ─────────────────────────────────────

/// A v1 file exactly as builds before these two settings existed wrote it.
fn legacy_file() -> String {
    format!(
        r#"{{"version":1,"enabled":true,"muted":false,
            "wakeBindings":[{{"wakeWord":"hey hermes","agentPubkey":"{AGENT}","destination":null}}],
            "stt":{{"backend":"local","endpointUrl":null}},
            "tts":{{"backend":"local","endpointUrl":null}},
            "inputDeviceId":null,"outputDevice":null,"indicatorPosition":null}}"#
    )
}

/// Load `json`, save it back, and return the file the next launch would read.
///
/// Round-tripping through the file rather than asserting on the struct is the
/// point: these settings are read at boot by a process that has only the bytes.
fn reloaded_json(json: &str) -> serde_json::Value {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join(SETTINGS_FILE);
    std::fs::write(&path, json).expect("fixture write");
    let settings = load_from_path(&path).expect("load");
    let out = dir.path().join("reloaded.json");
    save_to_path(&out, &settings).expect("save");
    serde_json::from_slice(&std::fs::read(&out).expect("read")).expect("json")
}

#[test]
fn a_file_written_before_these_settings_existed_gets_the_defaults() {
    // The upgrade path for every install that already has this feature on: no
    // stored key, no breakage, and the documented default rather than a zero.
    let value = reloaded_json(&legacy_file());
    assert_eq!(value["silenceHoldMs"], 800);
    assert!(value["stopPhrase"].is_null());
    // And nothing the file did carry was lost on the way through.
    assert_eq!(value["wakeBindings"][0]["wakeWord"], "hey hermes");
    assert_eq!(value["enabled"], true);
}

#[test]
fn a_chosen_hold_and_stop_phrase_survive_a_restart() {
    let value = reloaded_json(&format!(
        r#"{{"version":1,"enabled":true,
            "wakeBindings":[{{"wakeWord":"hey hermes","agentPubkey":"{AGENT}"}}],
            "silenceHoldMs":2500,"stopPhrase":"buzz stop"}}"#
    ));
    assert_eq!(value["silenceHoldMs"], 2500);
    assert_eq!(value["stopPhrase"], "buzz stop");
}

#[test]
fn a_hold_no_slider_could_produce_is_clamped_on_load_and_refused_on_save() {
    // Load is forgiving because the alternative is a file that will not open;
    // save is strict because a client sending this is a defect, and a hold of
    // zero would close every utterance on its first silent frame.
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join(SETTINGS_FILE);
    for (stored, clamped) in [(0, MIN_SILENCE_HOLD_MS), (999_999, MAX_SILENCE_HOLD_MS)] {
        std::fs::write(
            &path,
            format!(r#"{{"version":1,"enabled":true,"silenceHoldMs":{stored}}}"#),
        )
        .expect("fixture write");
        assert_eq!(
            load_from_path(&path).expect("load").silence_hold_ms,
            clamped
        );

        let error = save_to_path(
            &path,
            &AmbientVoiceSettings {
                silence_hold_ms: stored,
                ..AmbientVoiceSettings::default()
            },
        )
        .expect_err("out-of-range hold");
        assert!(error.contains("between 0.3 and 10 seconds"), "{error}");
    }
}

#[test]
fn a_blank_stop_phrase_arms_nothing() {
    // The field is written as the user types. Half a word must not become a
    // keyword, and an empty one is "switched off" rather than an error.
    for stored in [None, Some(String::new()), Some("   ".to_string())] {
        let settings = AmbientVoiceSettings {
            stop_phrase: stored.clone(),
            ..AmbientVoiceSettings::default()
        };
        assert!(
            settings.armed_stop_phrase().is_none(),
            "{stored:?} armed a keyword"
        );
    }
    let typed = AmbientVoiceSettings {
        stop_phrase: Some("  buzz stop  ".to_string()),
        ..AmbientVoiceSettings::default()
    };
    assert_eq!(typed.armed_stop_phrase(), Some("buzz stop"));
}

#[test]
fn saving_refuses_a_stop_phrase_the_spotter_could_not_arm() {
    // It is armed on the same spotter as the wake word, and a phrase the model
    // cannot encode terminates the process rather than erroring. The same gate,
    // therefore, and one more: a stop phrase equal to the wake word would arm
    // one keyword twice with no answer to which job a detection is doing.
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join(SETTINGS_FILE);
    for (phrase, expected) in [
        ("the", "fire constantly"),
        ("x", "at least"),
        ("hey hermes", "different from the wake word"),
        ("HEY   HERMES", "different from the wake word"),
    ] {
        let settings = AmbientVoiceSettings {
            enabled: true,
            wake_bindings: vec![binding()],
            stop_phrase: Some(phrase.to_string()),
            ..AmbientVoiceSettings::default()
        };
        let error = save_to_path(&path, &settings).expect_err(phrase);
        assert!(error.contains(expected), "{phrase}: {error}");
    }
    let too_long = AmbientVoiceSettings {
        enabled: true,
        wake_bindings: vec![binding()],
        stop_phrase: Some("x".repeat(MAX_WAKE_WORD_CHARS + 1)),
        ..AmbientVoiceSettings::default()
    };
    assert!(save_to_path(&path, &too_long)
        .expect_err("over-long stop phrase")
        .contains("too long"));
}

#[test]
fn a_stop_phrase_the_model_cannot_encode_is_refused() {
    // The whole of F1: these three pass every shape check — long enough, more
    // than one word, under the length cap — and the tokenizer refuses all of
    // them. Before the vocabulary check reached this door they saved cleanly
    // and then failed `keywords_buf` at arm time, which takes the session down
    // and with it the wake word, the microphone and the transcript path.
    //
    // The expectations come from what the model can represent (500 uppercase
    // ASCII pieces: no digits, no accents, no sentence punctuation), not from
    // running the code and writing down its answer.
    let tokenizer = fixture_tokenizer();
    for (phrase, cannot_hear) in [("buzz stop.", "."), ("stop now 2", "2"), ("café stop", "É")] {
        let error = validate_stop_phrase_against(&with_stop_phrase(phrase), Some(&tokenizer))
            .expect_err(phrase);
        assert!(
            error.starts_with("Stop phrase: "),
            "{phrase}: the message must name the field it came from: {error}"
        );
        assert!(
            error.contains(cannot_hear),
            "{phrase}: the message must name what the model cannot hear: {error}"
        );
    }

    // And the phrase the settings field offers as its example must work, or
    // the first thing anyone types is the thing that breaks it.
    assert!(
        validate_stop_phrase_against(&with_stop_phrase("that's all"), Some(&tokenizer)).is_ok(),
        "the placeholder the stop-phrase field suggests must be usable"
    );
    assert!(validate_stop_phrase_against(&with_stop_phrase("buzz stop"), Some(&tokenizer)).is_ok());
}

#[test]
fn a_stop_phrase_an_older_build_saved_is_dropped_the_first_time_it_is_read() {
    // The upgrade path. A build without the vocabulary check at the save door
    // could persist "buzz stop.", and that file is still on disk after the
    // update. Reading it must cost the user the stop phrase and nothing else —
    // in particular not the wake word, which is what arm-time refusal used to
    // take with it.
    let stored = with_stop_phrase("buzz stop.");
    let loaded = sanitize_loaded(stored, Some(&fixture_tokenizer()));
    assert!(loaded.stop_phrase.is_none());
    assert!(loaded.armed_stop_phrase().is_none());
    assert_eq!(
        loaded.primary_binding().map(|b| b.wake_word.as_str()),
        Some("hey hermes"),
        "dropping the stop phrase must not disturb the binding beside it"
    );

    // A usable one survives the same pass untouched.
    let kept = sanitize_loaded(with_stop_phrase("that's all"), Some(&fixture_tokenizer()));
    assert_eq!(kept.armed_stop_phrase(), Some("that's all"));
}

#[test]
fn without_the_model_the_stop_phrase_keeps_its_shape_checks() {
    // Settings are saved long before the wake-word model is downloaded, so the
    // vocabulary check is not always available. What must not happen is the
    // other rules going with it: the field is still refused when it is too
    // short to discriminate or identical to the wake word, exactly as the wake
    // word's own check degrades.
    assert!(validate_stop_phrase_against(&with_stop_phrase("buzz stop."), None).is_ok());
    assert!(validate_stop_phrase_against(&with_stop_phrase("the"), None)
        .expect_err("too short")
        .contains("fire constantly"));
    assert!(
        validate_stop_phrase_against(&with_stop_phrase("hey hermes"), None)
            .expect_err("clashes")
            .contains("different from the wake word")
    );
    // No phrase at all is the default and needs no model to be valid.
    assert!(validate_stop_phrase_against(&AmbientVoiceSettings::default(), None).is_ok());
}

#[test]
fn an_unusable_stop_phrase_is_dropped_on_load_rather_than_armed() {
    // A hand-edited file must cost the user one setting, not the whole file —
    // and must never hand the engine a phrase that would kill the process.
    let value = reloaded_json(&format!(
        r#"{{"version":1,"enabled":true,
            "wakeBindings":[{{"wakeWord":"hey hermes","agentPubkey":"{AGENT}"}}],
            "stopPhrase":"the"}}"#
    ));
    assert!(value["stopPhrase"].is_null());
    assert_eq!(value["wakeBindings"][0]["wakeWord"], "hey hermes");
}

#[test]
fn unversioned_settings_migrate_to_v1_defaults() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join(SETTINGS_FILE);
    std::fs::write(&path, r#"{"wakeWord":"legacy","enabled":true}"#).expect("fixture write");
    assert_eq!(
        load_from_path(&path).expect("migration"),
        AmbientVoiceSettings::default()
    );
}

#[test]
fn future_schema_versions_are_refused_rather_than_reset() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join(SETTINGS_FILE);
    std::fs::write(&path, r#"{"version":99,"enabled":true}"#).expect("fixture write");
    let error = load_from_path(&path).expect_err("future version");
    assert!(
        error.contains("newer than this Buzz build supports"),
        "{error}"
    );
}

#[test]
fn a_v1_file_missing_optional_sections_still_loads() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join(SETTINGS_FILE);
    std::fs::write(&path, r#"{"version":1,"enabled":true}"#).expect("fixture write");
    let settings = load_from_path(&path).expect("load");
    assert!(settings.enabled);
    assert!(settings.wake_bindings.is_empty());
    assert_eq!(settings.stt.backend, SpeechBackend::Local);
    // Enabled with no binding is configured-but-not-runnable, never a default
    // wake phrase.
    assert!(!settings.is_runnable());
}

#[test]
fn an_unknown_speech_backend_degrades_to_local() {
    // A file written by a future build that ships a backend this one does not
    // have must still open, and must not leave a role pointing at something
    // this build cannot interpret. Local is the safe reading: no audio leaves
    // the device.
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join(SETTINGS_FILE);
    std::fs::write(
        &path,
        r#"{"version":1,"enabled":true,"stt":{"backend":"grpc","endpointUrl":"https://example.invalid/v1"},"tts":{"backend":42,"endpointUrl":null}}"#,
    )
    .expect("fixture write");
    let settings = load_from_path(&path).expect("load");
    assert_eq!(settings.stt.backend, SpeechBackend::Local);
    assert_eq!(settings.tts.backend, SpeechBackend::Local);
    // The URL is preserved so switching back on a newer build is lossless.
    assert_eq!(
        settings.stt.endpoint_url.as_deref(),
        Some("https://example.invalid/v1")
    );
}

#[test]
fn an_http_backend_and_its_url_survive_a_save_and_load_verbatim() {
    // The whole point of the M3 choice: a role the user pointed at a server
    // stays pointed there across a restart. It is loaded before the first
    // session starts, so anything lost here is a session that silently ran on
    // the wrong backend.
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join(SETTINGS_FILE);
    let settings = AmbientVoiceSettings {
        enabled: true,
        wake_bindings: vec![binding()],
        stt: SpeechBackendSettings {
            backend: SpeechBackend::Http,
            endpoint_url: Some("http://speech.example:30120".to_string()),
        },
        tts: SpeechBackendSettings {
            backend: SpeechBackend::Http,
            endpoint_url: Some("http://speech.example:30121".to_string()),
        },
        ..AmbientVoiceSettings::default()
    };
    save_to_path(&path, &settings).expect("save");
    assert_eq!(load_from_path(&path).expect("load"), settings);

    // And on the wire, in the shape `AmbientVoiceSettings` in
    // `ambientVoiceApi.ts` reads.
    let value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).expect("read")).expect("json");
    assert_eq!(
        value["stt"],
        serde_json::json!({ "backend": "http", "endpointUrl": "http://speech.example:30120" })
    );
    assert_eq!(
        value["tts"],
        serde_json::json!({ "backend": "http", "endpointUrl": "http://speech.example:30121" })
    );
}

#[test]
fn a_role_only_talks_to_a_server_when_one_is_both_chosen_and_named() {
    // `http_base_url` is the single question every consumer asks, and both
    // halves have to be true. A URL kept beside a local choice is remembered,
    // not armed; a blank URL under `http` is a field the user has not finished
    // typing, and a session that refused to run on it would be worse than one
    // that stays local until it is there.
    let remembered = SpeechBackendSettings {
        backend: SpeechBackend::Local,
        endpoint_url: Some("http://speech.example:30120".to_string()),
    };
    assert_eq!(remembered.http_base_url(), None);

    let unnamed = SpeechBackendSettings {
        backend: SpeechBackend::Http,
        endpoint_url: Some("   ".to_string()),
    };
    assert_eq!(unnamed.http_base_url(), None);
    assert_eq!(
        SpeechBackendSettings {
            backend: SpeechBackend::Http,
            endpoint_url: None,
        }
        .http_base_url(),
        None
    );

    let armed = SpeechBackendSettings {
        backend: SpeechBackend::Http,
        endpoint_url: Some("  http://speech.example:30120  ".to_string()),
    };
    assert_eq!(armed.http_base_url(), Some("http://speech.example:30120"));
}

#[test]
fn malformed_bindings_are_dropped_on_load() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join(SETTINGS_FILE);
    std::fs::write(
        &path,
        format!(
            r#"{{"version":1,"enabled":true,"wakeBindings":[
                {{"wakeWord":"the","agentPubkey":"{AGENT}"}},
                {{"wakeWord":"hey hermes","agentPubkey":"nothex"}},
                {{"wakeWord":"hey hermes","agentPubkey":"{AGENT}"}}
            ]}}"#
        ),
    )
    .expect("fixture write");
    let settings = load_from_path(&path).expect("load");
    assert_eq!(settings.wake_bindings, vec![binding()]);
}

#[test]
fn saving_refuses_a_binding_the_spotter_could_not_arm() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join(SETTINGS_FILE);
    for bad in [
        WakeBinding {
            wake_word: "the".to_string(),
            ..binding()
        },
        WakeBinding {
            agent_pubkey: "not-a-pubkey".to_string(),
            ..binding()
        },
        WakeBinding {
            destination: Some("not-a-uuid".to_string()),
            ..binding()
        },
        WakeBinding {
            wake_word: "x".repeat(MAX_WAKE_WORD_CHARS + 1),
            ..binding()
        },
    ] {
        let settings = AmbientVoiceSettings {
            enabled: true,
            wake_bindings: vec![bad.clone()],
            ..AmbientVoiceSettings::default()
        };
        assert!(
            save_to_path(&path, &settings).is_err(),
            "expected {bad:?} to be refused"
        );
    }
}

#[test]
fn a_channel_destination_round_trips_for_a_later_milestone() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join(SETTINGS_FILE);
    let channel = uuid::Uuid::new_v4().to_string();
    let settings = AmbientVoiceSettings {
        enabled: true,
        wake_bindings: vec![WakeBinding {
            destination: Some(channel.clone()),
            ..binding()
        }],
        ..AmbientVoiceSettings::default()
    };
    save_to_path(&path, &settings).expect("save");
    assert_eq!(
        load_from_path(&path).expect("load").wake_bindings[0]
            .destination
            .as_deref(),
        Some(channel.as_str())
    );
}

#[test]
fn the_binding_list_is_capped() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join(SETTINGS_FILE);
    let settings = AmbientVoiceSettings {
        enabled: true,
        wake_bindings: vec![binding(); MAX_WAKE_BINDINGS + 1],
        ..AmbientVoiceSettings::default()
    };
    assert!(save_to_path(&path, &settings).is_err());

    let bindings = (0..MAX_WAKE_BINDINGS + 4)
        .map(|_| {
            serde_json::json!({"wakeWord": "hey hermes", "agentPubkey": AGENT, "destination": null})
        })
        .collect::<Vec<_>>();
    std::fs::write(
        &path,
        serde_json::to_vec(&serde_json::json!({
            "version": 1,
            "enabled": true,
            "wakeBindings": bindings,
        }))
        .expect("encode"),
    )
    .expect("fixture write");
    assert_eq!(
        load_from_path(&path).expect("load").wake_bindings.len(),
        MAX_WAKE_BINDINGS
    );
}

#[test]
fn only_the_first_binding_drives_the_m1_runtime() {
    let second = WakeBinding {
        wake_word: "good morning buzz".to_string(),
        ..binding()
    };
    let settings = AmbientVoiceSettings {
        enabled: true,
        wake_bindings: vec![binding(), second],
        ..AmbientVoiceSettings::default()
    };
    assert_eq!(settings.primary_binding(), Some(&binding()));
    assert!(settings.is_runnable());
}

#[test]
fn invalid_json_is_an_error_so_the_file_is_preserved() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join(SETTINGS_FILE);
    std::fs::write(&path, "{not json").expect("fixture write");
    assert!(load_from_path(&path)
        .expect_err("invalid json")
        .contains("not valid JSON"));
}

// ── The wake binding's own save door ─────────────────────────────────────────
//
// `patch_primary_binding` exists because saving a wake word by posting the
// whole settings object made every other field in that object a condition of
// the wake word being written. Every test below reads the FILE back after the
// call: the return value is a claim about what was written, and the thing that
// actually failed in dogfood was the file not changing at all.

/// A populated settings file, written through the same door the app writes it.
///
/// The tokenizer is the fixture rather than whatever this machine downloaded,
/// so the save door these tests cross is the one the shipped model produces.
fn stored_settings(stop_phrase: Option<&str>) -> AmbientVoiceSettings {
    AmbientVoiceSettings {
        enabled: true,
        muted: true,
        wake_bindings: vec![binding()],
        stt: SpeechBackendSettings {
            backend: SpeechBackend::Http,
            endpoint_url: Some("http://speech.example:30120".to_string()),
        },
        silence_hold_ms: 2_500,
        stop_phrase: stop_phrase.map(str::to_string),
        input_device_id: Some("mic-abc".to_string()),
        output_device: Some("Speakers (Realtek)".to_string()),
        indicator_position: Some(IndicatorPosition {
            x: 1180.0,
            y: 690.5,
        }),
        ..AmbientVoiceSettings::default()
    }
}

/// Write `settings` to a fresh temp file and hand back the directory and path.
fn stored_file(settings: &AmbientVoiceSettings) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join(SETTINGS_FILE);
    save_to_path_with(&path, settings, Some(&fixture_tokenizer())).expect("fixture write");
    (dir, path)
}

/// The binding a user typing a new wake word produces: same agent, new phrase.
fn rebound(wake_word: &str) -> WakeBinding {
    WakeBinding {
        wake_word: wake_word.to_string(),
        ..binding()
    }
}

#[test]
fn a_wake_word_saved_on_its_own_leaves_every_other_stored_field_alone() {
    // The isolation property, from the other side: what a binding write must
    // NOT do. The card used to post its whole loaded copy back, so every field
    // in the file was rewritten from a snapshot taken at mount — and mute, the
    // indicator position and the speech backends have all moved underneath it
    // since.
    let tokenizer = fixture_tokenizer();
    let stored = stored_settings(Some("that's all"));
    let (_dir, path) = stored_file(&stored);

    let returned =
        patch_primary_binding(&path, rebound("okay hermes"), Some(&tokenizer)).expect("patch");

    let on_disk = load_from_path_with(&path, Some(&tokenizer)).expect("reload");
    assert_eq!(
        on_disk.primary_binding().map(|b| b.wake_word.as_str()),
        Some("okay hermes"),
        "the wake word never reached the file"
    );
    assert_eq!(
        on_disk, returned,
        "the answer must be the file as it now reads, not the candidate"
    );
    assert_eq!(
        on_disk,
        AmbientVoiceSettings {
            wake_bindings: vec![rebound("okay hermes")],
            ..stored
        },
        "a binding write moved a field that was not the binding"
    );
}

#[test]
fn a_wake_word_that_clashes_with_the_stored_stop_phrase_still_saves() {
    // The reviewer's reproducer. Stored: "hey hermes" bound, "buzz stop" as the
    // stop phrase. The user types "buzz stop" as their wake word. The two
    // cannot both be armed — one keyword twice, and no answer to which job a
    // detection is doing — and the whole write used to be refused for it, so
    // the wake word did not persist and the field that would have resolved the
    // clash was the one the user could not save.
    //
    // The wake word wins: without it nothing arms at all. The stop phrase is
    // dropped, exactly as `start_session` already declines to arm one that
    // clashes and as the load door already drops one the model refuses.
    let tokenizer = fixture_tokenizer();
    let (_dir, path) = stored_file(&stored_settings(Some("buzz stop")));

    let returned = patch_primary_binding(&path, rebound("buzz stop"), Some(&tokenizer))
        .expect("the clash took the whole write down with it");

    let on_disk = load_from_path_with(&path, Some(&tokenizer)).expect("reload");
    assert_eq!(
        on_disk.primary_binding().map(|b| b.wake_word.as_str()),
        Some("buzz stop")
    );
    assert!(
        on_disk.stop_phrase.is_none(),
        "a clashing stop phrase survived on disk: {on_disk:?}"
    );
    // And in the answer, which is what takes it off the settings screen.
    assert!(returned.stop_phrase.is_none(), "{returned:?}");

    // In the bytes the next launch reads, since that is the process that arms
    // the spotter and it has only the file.
    let value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).expect("read")).expect("json");
    assert_eq!(value["wakeBindings"][0]["wakeWord"], "buzz stop");
    assert!(value["stopPhrase"].is_null());
}

#[test]
fn a_stop_phrase_that_can_still_stand_beside_the_new_wake_word_is_kept() {
    // The control for the test above: dropping is the answer to a clash, not
    // the price of every binding write. A phrase that is still perfectly
    // armable must be exactly where the user left it.
    let tokenizer = fixture_tokenizer();
    let (_dir, path) = stored_file(&stored_settings(Some("buzz stop")));

    patch_primary_binding(&path, rebound("okay hermes"), Some(&tokenizer)).expect("patch");

    let on_disk = load_from_path_with(&path, Some(&tokenizer)).expect("reload");
    assert_eq!(on_disk.armed_stop_phrase(), Some("buzz stop"));
    assert_eq!(
        on_disk.primary_binding().map(|b| b.wake_word.as_str()),
        Some("okay hermes")
    );
}

#[test]
fn a_wake_word_the_spotter_could_not_arm_is_refused_and_costs_the_file_nothing() {
    // The wake word is not the field that yields. It is handed to a C library
    // that terminates the process on input it cannot tokenise, so a phrase the
    // model refuses is refused here — with the file left byte-for-byte as it
    // was, because a failed save that half-wrote the file would cost the user
    // everything else in it.
    //
    // "hey hermes 7" passes every shape check and the model cannot hear the
    // digit; "the" fails the shape checks alone. Both belts, one door.
    let tokenizer = fixture_tokenizer();
    let (_dir, path) = stored_file(&stored_settings(Some("buzz stop")));
    let before = std::fs::read(&path).expect("read");

    for (wake_word, expected) in [("hey hermes 7", "7"), ("the", "at least")] {
        let error = patch_primary_binding(&path, rebound(wake_word), Some(&tokenizer))
            .expect_err(wake_word);
        assert!(error.contains(expected), "{wake_word}: {error}");
        assert_eq!(
            std::fs::read(&path).expect("read"),
            before,
            "{wake_word}: a refused binding still wrote to the file"
        );
    }
}

#[test]
fn bindings_a_later_milestone_stored_survive_an_m1_wake_word_edit() {
    // `wakeBindings` is a list so per-agent wake words need no migration. The
    // M1 card edits the first row; anything after it belongs to a milestone
    // this build does not render, and silently deleting configuration the user
    // cannot see is the worst kind of loss.
    let tokenizer = fixture_tokenizer();
    let second = WakeBinding {
        wake_word: "good morning buzz".to_string(),
        ..binding()
    };
    let (_dir, path) = stored_file(&AmbientVoiceSettings {
        wake_bindings: vec![binding(), second.clone()],
        ..stored_settings(None)
    });

    patch_primary_binding(&path, rebound("okay hermes"), Some(&tokenizer)).expect("patch");

    assert_eq!(
        load_from_path_with(&path, Some(&tokenizer))
            .expect("reload")
            .wake_bindings,
        vec![rebound("okay hermes"), second]
    );
}

#[test]
fn the_first_binding_an_install_ever_saves_is_written_rather_than_dropped() {
    // The shipping default is an empty binding list, and this door is how the
    // very first wake word reaches the file: there is no first row to replace.
    let tokenizer = fixture_tokenizer();
    let (_dir, path) = stored_file(&AmbientVoiceSettings::default());

    patch_primary_binding(&path, rebound("okay hermes"), Some(&tokenizer)).expect("patch");

    let on_disk = load_from_path_with(&path, Some(&tokenizer)).expect("reload");
    assert_eq!(on_disk.wake_bindings, vec![rebound("okay hermes")]);
}

/// The whole-object write, driven the way `set_ambient_voice_settings` drives it.
///
/// That command merges the client payload over what the runtime holds, applies
/// the indicator guard and saves; adoption and reconciliation follow, need a
/// running Tauri app, and touch no file. The three steps a file can be asked
/// about are *called* here rather than restated — a test that re-implemented
/// the merge would go on passing with the merge itself broken, which is the
/// one thing it exists to catch.
fn full_settings_save(
    path: &Path,
    current: &AmbientVoiceSettings,
    client: AmbientVoiceSettings,
    tokenizer: &WakeWordTokenizer,
) -> Result<(), String> {
    use crate::ambient_voice::commands::{keep_stored_indicator_position, merge_client_settings};

    let next = keep_stored_indicator_position(
        merge_client_settings(current, client),
        current.indicator_position,
    );
    save_to_path_with(path, &next, Some(tokenizer))
}

#[test]
fn a_stale_full_settings_save_cannot_undo_a_wake_word_saved_beside_it() {
    // Two doors write this file, and the reviewer's reproducer runs them in the
    // order a user produces without trying: stored is "hey hermes" bound with
    // "buzz stop" as the stop phrase; the user makes "buzz stop" the wake word,
    // which the binding door writes (dropping the phrase it now clashes with);
    // and before the card has adopted what came back, they clear the stop
    // phrase — which posts the WHOLE settings object the card loaded at mount,
    // stale binding and all.
    //
    // The binding in that payload is never an edit: `set_ambient_wake_binding`
    // is the only door the wake word and the agent are ever changed through.
    // Passing it through therefore put "hey hermes" back over the wake word the
    // user had just saved, in the file the next launch arms from, with the card
    // showing neither the write nor the loss.
    let tokenizer = fixture_tokenizer();
    // An M2 binding this build never renders, to prove the merge holds the
    // stored list rather than a first row it reconstructed.
    let second = WakeBinding {
        wake_word: "good morning buzz".to_string(),
        ..binding()
    };
    let seeded = AmbientVoiceSettings {
        wake_bindings: vec![binding(), second.clone()],
        ..stored_settings(Some("buzz stop"))
    };
    let (_dir, path) = stored_file(&seeded);

    // Door one: the wake word, on its own. It drops the clashing stop phrase.
    let after_binding = patch_primary_binding(&path, rebound("buzz stop"), Some(&tokenizer))
        .expect("the binding door refused the wake word");

    // Door two: the stop-phrase clear, carrying the card's mount-time copy —
    // the old binding, the old mute, the old everything.
    full_settings_save(
        &path,
        &after_binding,
        AmbientVoiceSettings {
            stop_phrase: None,
            ..seeded.clone()
        },
        &tokenizer,
    )
    .expect("the full-settings write was refused");

    let on_disk = load_from_path_with(&path, Some(&tokenizer)).expect("reload");
    assert_eq!(
        on_disk.primary_binding().map(|b| b.wake_word.as_str()),
        Some("buzz stop"),
        "a stale full-settings save rolled the wake word back: {on_disk:?}"
    );
    // In the bytes, because it is the file and not this process that the next
    // launch arms the spotter from.
    let value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).expect("read")).expect("json");
    assert_eq!(value["wakeBindings"][0]["wakeWord"], "buzz stop");

    // The client's own edit still landed — holding the binding back must not
    // cost the user the field they were actually changing.
    assert!(
        on_disk.stop_phrase.is_none(),
        "the stop-phrase clear did not reach the file: {on_disk:?}"
    );
    // And nothing else moved: the M2 binding is still there, and every field
    // the payload merely carried reads as it was seeded.
    assert_eq!(
        on_disk,
        AmbientVoiceSettings {
            wake_bindings: vec![rebound("buzz stop"), second],
            stop_phrase: None,
            ..seeded
        },
        "a full-settings write moved a field that was not the client's edit"
    );
}
