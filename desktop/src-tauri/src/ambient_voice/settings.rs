//! Versioned on-disk settings for the `ambientVoice` preview feature.
//!
//! Mirrors the `huddle::tts_settings` convention exactly: a `version` field,
//! serde camelCase, explicit migration rules, an atomic restricted write, an
//! in-memory mirror in `AppState`, and boot hydration from `lib.rs`.
//!
//! The file is deliberately separate from the stable `tts-settings.json` so
//! the preview feature stays self-contained for an eventual upstream PR — no
//! migration of the stable file is performed or required.
//!
//! Schema (v1):
//!
//! ```jsonc
//! {
//!   "version": 1,
//!   "enabled": false,
//!   "muted": false,
//!   "wakeBindings": [
//!     { "wakeWord": "hey hermes",
//!       "agentPubkey": "<64 hex>",
//!       "destination": null }      // null → DM with the agent
//!   ],
//!   "stt": { "backend": "local",           // or "http"
//!            "endpointUrl": null },        // base URL when "http"
//!   "tts": { "backend": "local", "endpointUrl": null },
//!   "inputDeviceId": null,
//!   "outputDevice": null,
//!   "indicatorPosition": null    // null → the frontend's default corner
//! }
//! ```
//!
//! `wakeBindings` is a LIST in v1 even though M1's runtime and UI use exactly
//! one binding. Keeping the list shape now means per-agent wake words (M2)
//! need no schema migration.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};

use crate::managed_agents::storage::atomic_write_json_restricted;

use super::wake_word::{validate_wake_word, WakeWordError, MAX_WAKE_WORD_CHARS};

pub(crate) const SETTINGS_FILE: &str = "ambient-voice-settings.json";
pub(crate) const CURRENT_VERSION: u32 = 1;

/// Upper bound on persisted bindings. M1 uses one; the cap keeps a corrupt or
/// hand-edited file from arming an unbounded keyword set at boot.
pub(crate) const MAX_WAKE_BINDINGS: usize = 16;

/// Speech backend selection, per role.
///
/// `Local` is the on-device model — the default, and what every install runs
/// until the user says otherwise. `Http` sends that role's audio to an
/// OpenAI-compatible speech server instead (`super::speech_http` holds the
/// wire contract). The choice is per role because the two run on separate
/// ports, and because sending utterances away is a different decision from
/// having replies spoken by a remote voice.
///
/// A file naming a backend this build does not know falls back to `Local`
/// rather than failing to load, so a newer client's file still opens here and
/// no audio leaves the device on a name we cannot interpret.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum SpeechBackend {
    #[default]
    Local,
    Http,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SpeechBackendSettings {
    pub backend: SpeechBackend,
    /// Base URL of the speech server, e.g. `http://your-server:30120`. The
    /// role's paths are appended to it. Kept verbatim across load/save even
    /// while the role runs locally, so switching back and forth does not cost
    /// the user the URL they typed.
    pub endpoint_url: Option<String>,
}

impl SpeechBackendSettings {
    /// The base URL this role should talk to, or `None` when it runs on-device.
    ///
    /// A blank URL under `Http` reads as "not configured yet" rather than as an
    /// error: the settings field is written as the user types, and a session
    /// that refused to start on a half-typed URL would be worse than one that
    /// keeps running locally until the URL is there.
    pub fn http_base_url(&self) -> Option<&str> {
        if self.backend != SpeechBackend::Http {
            return None;
        }
        self.endpoint_url
            .as_deref()
            .map(str::trim)
            .filter(|url| !url.is_empty())
    }
}

/// Where the user parked the listening indicator, in CSS pixels from the top
/// left of the viewport.
///
/// Stored raw rather than as a corner or a fraction because the pill is
/// free-dragged, not snapped. It is the frontend that clamps this back into
/// the window on restore and on resize — the window size is not knowable here,
/// and a value saved on a large display must not be discarded merely because
/// the app reopened on a small one.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IndicatorPosition {
    pub x: f64,
    pub y: f64,
}

impl IndicatorPosition {
    /// Whether this is a position that can be written and read back.
    ///
    /// `serde_json` refuses to encode NaN and infinities, so an unchecked
    /// value would fail the whole settings write with a JSON error rather than
    /// something a caller can act on.
    fn is_storable(&self) -> bool {
        self.x.is_finite() && self.y.is_finite()
    }
}

/// One wake word bound to one agent and one destination.
///
/// `destination` is `None` for "the DM with `agent_pubkey`" — the M1 default
/// and the only shape the M1 runtime resolves. A channel id may be stored by a
/// later milestone without a schema change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WakeBinding {
    pub wake_word: String,
    pub agent_pubkey: String,
    #[serde(default)]
    pub destination: Option<String>,
}

// Not `Eq`: the indicator position is measured in CSS pixels, which are
// fractional on a scaled display.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AmbientVoiceSettings {
    pub version: u32,
    /// Whether the native ambient session should run. Independent of the
    /// `ambientVoice` preview flag, which lives in the frontend manifest
    /// store: the flag decides whether the feature exists for this user, this
    /// field records what the user did with it.
    pub enabled: bool,
    /// User-facing mute. Muting keeps the session configured but stops the
    /// keyword spotter from arming, so no audio is processed.
    #[serde(default)]
    pub muted: bool,
    #[serde(default)]
    pub wake_bindings: Vec<WakeBinding>,
    #[serde(default)]
    pub stt: SpeechBackendSettings,
    #[serde(default)]
    pub tts: SpeechBackendSettings,
    /// Persisted `getUserMedia` input device id. Closes the existing gap where
    /// huddle device choices do not survive restart.
    #[serde(default)]
    pub input_device_id: Option<String>,
    /// Persisted rodio output device name, matching `set_audio_output_device`.
    #[serde(default)]
    pub output_device: Option<String>,
    /// Where the user dragged the listening indicator. `None` until they move
    /// it, which is what makes the frontend default apply to everyone else.
    #[serde(default)]
    pub indicator_position: Option<IndicatorPosition>,
}

impl Default for AmbientVoiceSettings {
    fn default() -> Self {
        Self {
            version: CURRENT_VERSION,
            enabled: false,
            muted: false,
            // No default wake phrase: an empty binding set arms nothing.
            wake_bindings: Vec::new(),
            stt: SpeechBackendSettings::default(),
            tts: SpeechBackendSettings::default(),
            input_device_id: None,
            output_device: None,
            indicator_position: None,
        }
    }
}

impl AmbientVoiceSettings {
    /// The single binding M1's runtime and UI operate on.
    ///
    /// M2 replaces every caller of this with per-binding routing; until then
    /// the first binding is authoritative and any extras are persisted but
    /// unused.
    pub fn primary_binding(&self) -> Option<&WakeBinding> {
        self.wake_bindings.first()
    }

    /// Whether the runtime has everything it needs to arm the spotter.
    ///
    /// Deliberately does not consider mute or huddle arbitration — those are
    /// runtime states, not configuration.
    pub fn is_runnable(&self) -> bool {
        self.enabled && self.primary_binding().is_some()
    }
}

pub(crate) fn settings_path(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .app_data_dir()
        .map(|dir| dir.join(SETTINGS_FILE))
        .map_err(|error| format!("could not locate Buzz settings storage: {error}"))
}

/// Validate one binding for persistence.
///
/// The wake word must pass the same strict tokenizer-independent checks the
/// settings UI applies, minus the model-vocabulary check — the model may not
/// be downloaded yet when settings are saved, and an un-tokenizable phrase is
/// caught again (fatally, before the engine) at arm time.
pub(crate) fn validate_binding_shape(binding: &WakeBinding) -> Result<(), String> {
    if binding.wake_word.chars().count() > MAX_WAKE_WORD_CHARS {
        return Err(format!(
            "Wake word is too long (max {MAX_WAKE_WORD_CHARS} characters)"
        ));
    }
    validate_wake_word(&binding.wake_word).map_err(|error: WakeWordError| error.to_string())?;
    let pubkey = binding.agent_pubkey.trim();
    if pubkey.len() != 64 || !pubkey.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err("Agent public key must be 64 hex characters".to_string());
    }
    if let Some(destination) = binding.destination.as_deref() {
        if uuid::Uuid::parse_str(destination).is_err() {
            return Err("Destination must be a channel id".to_string());
        }
    }
    Ok(())
}

/// Load settings from `path`, applying migration rules.
///
/// Load is deliberately forgiving where forgiveness is safe (unknown backend →
/// local, malformed bindings → dropped) and strict where it is not (a
/// future-version file is an error, not a silent reset, so the user's
/// configuration is preserved for the newer build that wrote it).
pub(crate) fn load_from_path(path: &Path) -> Result<AmbientVoiceSettings, String> {
    if !path.exists() {
        return Ok(AmbientVoiceSettings::default());
    }
    let bytes = std::fs::read(path)
        .map_err(|error| format!("could not read ambient voice settings: {error}"))?;
    let value: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|error| format!("ambient voice settings are not valid JSON: {error}"))?;

    // Unversioned files predate the schema and cannot be interpreted
    // unambiguously. Use deterministic v1 defaults, exactly as tts_settings.
    if value.get("version").is_none() {
        return Ok(AmbientVoiceSettings::default());
    }
    let version = value
        .get("version")
        .and_then(serde_json::Value::as_u64)
        .ok_or("ambient voice settings version is invalid")?;
    if version > u64::from(CURRENT_VERSION) {
        return Err(format!(
            "ambient voice settings version {version} is newer than this Buzz build supports"
        ));
    }

    let mut settings: AmbientVoiceSettings = serde_json::from_value(sanitize_backends(value))
        .map_err(|error| format!("ambient voice settings are invalid: {error}"))?;
    settings.version = CURRENT_VERSION;

    // Drop bindings that could never arm. A malformed binding is a defect in
    // whatever wrote the file; keeping it would either poison the keyword set
    // or force the whole file to fail to load.
    settings
        .wake_bindings
        .retain(|binding| validate_binding_shape(binding).is_ok());
    settings.wake_bindings.truncate(MAX_WAKE_BINDINGS);

    // A hand-edited or truncated file can carry a position no window could
    // ever show. Forgetting it falls back to the default corner, which is
    // strictly better than an indicator the user cannot find.
    settings.indicator_position = settings
        .indicator_position
        .filter(IndicatorPosition::is_storable);
    Ok(settings)
}

/// Replace unknown `stt`/`tts` backend names with the local default.
///
/// A file written by a build that ships a backend this one does not have must
/// still open here — degrading to local is correct and safe (no audio leaves
/// the device). Known-ness is asked of serde rather than of a second list of
/// names, so adding a variant to [`SpeechBackend`] cannot leave a name that
/// loads in one place and is scrubbed in the other.
fn sanitize_backends(mut value: serde_json::Value) -> serde_json::Value {
    for key in ["stt", "tts"] {
        let Some(section) = value
            .get_mut(key)
            .and_then(serde_json::Value::as_object_mut)
        else {
            continue;
        };
        let known = section.get("backend").is_some_and(|backend| {
            serde_json::from_value::<SpeechBackend>(backend.clone()).is_ok()
        });
        if !known {
            section.insert(
                "backend".to_string(),
                serde_json::Value::String("local".to_string()),
            );
        }
    }
    value
}

pub(crate) fn save_to_path(path: &Path, settings: &AmbientVoiceSettings) -> Result<(), String> {
    if settings.version != CURRENT_VERSION {
        return Err(format!(
            "Unsupported ambient voice settings version: {}",
            settings.version
        ));
    }
    if settings.wake_bindings.len() > MAX_WAKE_BINDINGS {
        return Err(format!(
            "At most {MAX_WAKE_BINDINGS} wake bindings can be saved"
        ));
    }
    for binding in &settings.wake_bindings {
        validate_binding_shape(binding)?;
    }
    if let Some(position) = settings.indicator_position {
        if !position.is_storable() {
            return Err("Indicator position must be a finite pixel offset".to_string());
        }
    }
    let payload = serde_json::to_vec_pretty(settings)
        .map_err(|error| format!("could not encode ambient voice settings: {error}"))?;
    atomic_write_json_restricted(path, &payload)
        .map_err(|error| format!("could not save ambient voice settings: {error}"))
}

/// Boot hydration entry point. Never fails the app: a broken file yields
/// defaults plus a recorded error that makes later writes fail-closed.
pub fn load_for_app(app: &AppHandle) -> (AmbientVoiceSettings, Option<String>) {
    match settings_path(app).and_then(|path| load_from_path(&path)) {
        Ok(settings) => (settings, None),
        Err(error) => {
            eprintln!(
                "buzz-desktop: {error}; ambient voice stays off for this session and the file is preserved"
            );
            (AmbientVoiceSettings::default(), Some(error))
        }
    }
}

#[cfg(test)]
#[path = "settings_tests.rs"]
mod settings_tests;
