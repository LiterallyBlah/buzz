//! Ambient-voice settings load/save/migration tests.
//!
//! Mirrors `huddle::tts_settings`'s test set: defaults, unversioned migration,
//! future-version refusal, and round-tripping of fields a newer client may
//! own. Adds the checks specific to this feature — an empty binding set must
//! stay empty (no default wake phrase), and a malformed binding must not be
//! able to reach the keyword spotter.

use super::*;

const AGENT: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn binding() -> WakeBinding {
    WakeBinding {
        wake_word: "hey hermes".to_string(),
        agent_pubkey: AGENT.to_string(),
        destination: None,
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

// ── The silence hold ─────────────────────────────────────────────────────────

/// A v1 file exactly as builds before this setting existed wrote it.
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
    // And nothing the file did carry was lost on the way through.
    assert_eq!(value["wakeBindings"][0]["wakeWord"], "hey hermes");
    assert_eq!(value["enabled"], true);
}

#[test]
fn a_chosen_hold_survives_a_restart() {
    let value = reloaded_json(&format!(
        r#"{{"version":1,"enabled":true,
            "wakeBindings":[{{"wakeWord":"hey hermes","agentPubkey":"{AGENT}"}}],
            "silenceHoldMs":2500}}"#
    ));
    assert_eq!(value["silenceHoldMs"], 2500);
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
