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
//!   "silenceHoldMs": 800,        // pause that closes an utterance
//!   "stopPhrase": null,          // null/blank → no stop phrase is armed
//!   "inputDeviceId": null,
//!   "outputDevice": null,
//!   "indicatorPosition": null    // null → the frontend's default corner
//! }
//! ```
//!
//! `silenceHoldMs` and `stopPhrase` were added after v1 shipped and are read
//! with serde defaults rather than a version bump: an install with neither key
//! gets the default hold and no stop phrase, which is exactly what it was
//! already doing. A version bump would have made every existing file
//! unreadable to the build that wrote it.
//!
//! `wakeBindings` is a LIST in v1 even though M1's runtime and UI use exactly
//! one binding. Keeping the list shape now means per-agent wake words (M2)
//! need no schema migration.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};

use crate::managed_agents::storage::atomic_write_json_restricted;

use super::utterance::{DEFAULT_SILENCE_HOLD_MS, MAX_SILENCE_HOLD_MS, MIN_SILENCE_HOLD_MS};
use super::wake_word::{
    engine_keyword, validate_wake_word, WakeWordError, WakeWordTokenizer, MAX_WAKE_WORD_CHARS,
};

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
    /// How long a pause must last before it closes an utterance.
    ///
    /// Absent from every file written before this setting existed, which is why
    /// it carries a serde default rather than a migration: those installs get
    /// [`DEFAULT_SILENCE_HOLD_MS`] and nothing about them breaks. Clamped to
    /// the slider's range on load, so a hand-edited or newer file cannot ask
    /// the capture machine for a hold it will not honour.
    #[serde(default = "default_silence_hold_ms")]
    pub silence_hold_ms: u32,
    /// Optional phrase that ends a capture the moment it is heard. `None`, and
    /// a blank string, both mean none is armed.
    ///
    /// It is armed as a second keyword on the same spotter as the wake word, so
    /// it is held to exactly the same validation: a phrase the model cannot
    /// encode terminates the process rather than erroring (see
    /// [`super::wake_word`]).
    #[serde(default)]
    pub stop_phrase: Option<String>,
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
            silence_hold_ms: DEFAULT_SILENCE_HOLD_MS,
            stop_phrase: None,
            input_device_id: None,
            output_device: None,
            indicator_position: None,
        }
    }
}

/// The hold a file with no stored value loads with. Serde needs a function.
fn default_silence_hold_ms() -> u32 {
    DEFAULT_SILENCE_HOLD_MS
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

    /// The stop phrase to arm, or `None` when the feature is switched off.
    ///
    /// A blank field reads as "switched off" rather than as an error for the
    /// same reason a blank speech URL does: it is written as the user types,
    /// and half a word must not become a keyword.
    pub fn armed_stop_phrase(&self) -> Option<&str> {
        self.stop_phrase
            .as_deref()
            .map(str::trim)
            .filter(|phrase| !phrase.is_empty())
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
/// caught again (fatally, before the engine) at arm time. A binding arriving
/// through [`patch_primary_binding`] gets that check too, when the model is
/// there: it is the phrase the user just typed, so the answer is worth having
/// before it is written rather than at the next session start.
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

/// Validate the stop phrase for persistence, against a model when there is one.
///
/// It reaches the keyword spotter beside the wake word, so it passes the same
/// checks — and one more: a stop phrase identical to the wake word would arm
/// the same keyword twice, and there would be no answer to which of the two
/// jobs a detection was doing.
///
/// Unlike [`validate_binding_shape`], this **does** run the model-vocabulary
/// check when the model is installed. The wake word can skip it there because
/// the settings UI runs it on every keystroke and refuses to save without it;
/// the stop phrase had no such gate, so a phrase the tokenizer cannot encode
/// used to save cleanly and then fail the whole session at arm time. When the
/// model is not downloaded yet the check is simply unavailable, exactly as it
/// is for the wake word, and the shape checks still run.
///
/// The tokenizer is supplied rather than looked up so the tests can pin the
/// vocabulary rule against the in-repo fixture rather than against whatever
/// this machine has downloaded — a check that silently degrades to "no model,
/// so valid" is a check that proves nothing about the phrases it is supposed
/// to refuse.
pub(crate) fn validate_stop_phrase_against(
    settings: &AmbientVoiceSettings,
    tokenizer: Option<&WakeWordTokenizer>,
) -> Result<(), String> {
    let Some(phrase) = settings.armed_stop_phrase() else {
        return Ok(());
    };
    if phrase.chars().count() > MAX_WAKE_WORD_CHARS {
        return Err(format!(
            "Stop phrase is too long (max {MAX_WAKE_WORD_CHARS} characters)"
        ));
    }
    // Named, because the shared validator's words are about the phrase and not
    // about which field it came from, and this file has two phrase fields.
    validate_wake_word(phrase).map_err(|error: WakeWordError| format!("Stop phrase: {error}"))?;
    if let Some(tokenizer) = tokenizer {
        tokenizer
            .tokenize(phrase)
            .map_err(|error: WakeWordError| format!("Stop phrase: {error}"))?;
    }
    let clashes = settings
        .primary_binding()
        .is_some_and(|binding| engine_keyword(&binding.wake_word) == engine_keyword(phrase));
    if clashes {
        return Err("The stop phrase must be different from the wake word".to_string());
    }
    Ok(())
}

/// The tokenizer for the downloaded wake-word model, when there is one.
///
/// `None` before the model has been downloaded, which is an ordinary state:
/// settings are saved long before a session ever runs.
pub(crate) fn installed_tokenizer() -> Option<WakeWordTokenizer> {
    super::models::kws_model_dir().and_then(|dir| WakeWordTokenizer::load(&dir).ok())
}

/// Load settings from `path`, applying migration rules.
///
/// Load is deliberately forgiving where forgiveness is safe (unknown backend →
/// local, malformed bindings → dropped) and strict where it is not (a
/// future-version file is an error, not a silent reset, so the user's
/// configuration is preserved for the newer build that wrote it).
pub(crate) fn load_from_path(path: &Path) -> Result<AmbientVoiceSettings, String> {
    load_from_path_with(path, installed_tokenizer().as_ref())
}

/// The same load with the tokenizer supplied, for the same reason
/// [`validate_stop_phrase_against`] takes one: a caller that already holds the
/// tokenizer must not have it loaded again underneath it, and a test must be
/// able to pin the vocabulary rule to the in-repo fixture.
pub(crate) fn load_from_path_with(
    path: &Path,
    tokenizer: Option<&WakeWordTokenizer>,
) -> Result<AmbientVoiceSettings, String> {
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

    let settings: AmbientVoiceSettings = serde_json::from_value(sanitize_backends(value))
        .map_err(|error| format!("ambient voice settings are invalid: {error}"))?;
    Ok(sanitize_loaded(settings, tokenizer))
}

/// Bring a freshly-deserialised file back inside the ranges the runtime honours.
///
/// Every rule here is forgiving on purpose: the alternative to dropping one bad
/// field is a file that will not open at all, which costs the user everything
/// else in it. Separated from [`load_from_path`] and given the tokenizer
/// explicitly so the vocabulary rule can be tested against the in-repo fixture
/// rather than against whatever this machine has downloaded.
fn sanitize_loaded(
    mut settings: AmbientVoiceSettings,
    tokenizer: Option<&WakeWordTokenizer>,
) -> AmbientVoiceSettings {
    settings.version = CURRENT_VERSION;

    // Drop bindings that could never arm. A malformed binding is a defect in
    // whatever wrote the file; keeping it would either poison the keyword set
    // or force the whole file to fail to load.
    settings
        .wake_bindings
        .retain(|binding| validate_binding_shape(binding).is_ok());
    settings.wake_bindings.truncate(MAX_WAKE_BINDINGS);

    // Same forgiveness, for the same reason: a stop phrase the spotter could
    // not be given must never reach it, and dropping it costs the user one
    // setting rather than the whole file. This is also the upgrade path for a
    // phrase an older build saved before the vocabulary check reached the save
    // door — it is dropped the first time the fixed build reads the file.
    if validate_stop_phrase_against(&settings, tokenizer).is_err() {
        settings.stop_phrase = None;
    }
    settings.silence_hold_ms = settings
        .silence_hold_ms
        .clamp(MIN_SILENCE_HOLD_MS, MAX_SILENCE_HOLD_MS);

    // A hand-edited or truncated file can carry a position no window could
    // ever show. Forgetting it falls back to the default corner, which is
    // strictly better than an indicator the user cannot find.
    settings.indicator_position = settings
        .indicator_position
        .filter(IndicatorPosition::is_storable);
    settings
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
    save_to_path_with(path, settings, installed_tokenizer().as_ref())
}

/// The same write with the tokenizer supplied.
///
/// Every rule the save door applies is applied here; the seam exists so the
/// one rule that needs a model is asked of a known vocabulary. A caller that
/// has already decided something against a tokenizer — [`patch_primary_binding`]
/// decides whether the stored stop phrase can stand beside a new wake word —
/// must be able to hand this door the *same* one, or the door can still refuse
/// a write the caller believed it had made acceptable.
pub(crate) fn save_to_path_with(
    path: &Path,
    settings: &AmbientVoiceSettings,
    tokenizer: Option<&WakeWordTokenizer>,
) -> Result<(), String> {
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
    validate_stop_phrase_against(settings, tokenizer)?;
    if !(MIN_SILENCE_HOLD_MS..=MAX_SILENCE_HOLD_MS).contains(&settings.silence_hold_ms) {
        return Err(format!(
            "The pause before Buzz stops listening must be between \
             {:.1} and {:.0} seconds",
            f64::from(MIN_SILENCE_HOLD_MS) / 1000.0,
            f64::from(MAX_SILENCE_HOLD_MS) / 1000.0
        ));
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

/// Write one wake binding into the stored file, and nothing else.
///
/// The settings card used to save a wake word by posting the whole settings
/// object it had loaded, which made every other field in that object a
/// condition of the wake word being written: a stored stop phrase the new wake
/// word clashes with, or one an older build saved that the model cannot encode,
/// had the save door refuse the write entire — so the wake word did not
/// persist, and the field that would have resolved the clash was the one the
/// user could not save. Nothing on the card's side could fix that, because the
/// refusal is the save door's and the door is right to refuse what it was
/// handed. So the binding gets a door of its own that is handed the binding
/// alone.
///
/// Three rules, in this order:
///
/// 1. The stored file is the base — never a copy the client sent. The card
///    holds one from whenever it mounted, and mute, enablement and the
///    indicator have all moved underneath it since.
/// 2. The binding is validated as strictly as the check command validates it:
///    [`validate_binding_shape`] plus the model vocabulary when `tokenizer` is
///    `Some`, which is the belt `commands::check_ambient_wake_word` applies on
///    every keystroke. A wake word that fails is refused and nothing is
///    written.
/// 3. A stored stop phrase that cannot stand beside the new binding is
///    **dropped**, and the write goes through. The wake word is the primary
///    control — without it nothing arms at all — and a stop phrase is already
///    the field that yields elsewhere: [`sanitize_loaded`] drops one the model
///    refuses, and `super::start_session` filters out one that clashes rather
///    than arming one keyword twice. The caller sees it gone in the returned
///    settings, which is what lets the card take it off the screen.
///
/// Returns the file as re-read from disk, so what the caller adopts is what
/// the next launch will load rather than the candidate that was written.
pub(crate) fn patch_primary_binding(
    path: &Path,
    binding: WakeBinding,
    tokenizer: Option<&WakeWordTokenizer>,
) -> Result<AmbientVoiceSettings, String> {
    validate_binding_shape(&binding)?;
    if let Some(tokenizer) = tokenizer {
        tokenizer
            .tokenize(&binding.wake_word)
            .map_err(|error: WakeWordError| error.to_string())?;
    }
    let mut next = load_from_path_with(path, tokenizer)?;
    // Replace the first binding and keep any extras a later milestone stored:
    // editing the M1 row must never silently delete M2 configuration. An empty
    // list is the first save an install ever makes.
    if next.wake_bindings.is_empty() {
        next.wake_bindings.push(binding);
    } else {
        next.wake_bindings[0] = binding;
    }
    if validate_stop_phrase_against(&next, tokenizer).is_err() {
        next.stop_phrase = None;
    }
    save_to_path_with(path, &next, tokenizer)?;
    load_from_path_with(path, tokenizer)
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
