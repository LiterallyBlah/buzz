//! The Tauri command surface for the ambient feature.
//!
//! Its own file rather than the tail of [`super`], which is what the audio
//! worker, the huddle and boot hydration reach into. Nothing here decides
//! anything by itself: every command reads or writes the settings and then
//! funnels through [`super::reconcile`], which remains the only place a session
//! is created.

use std::sync::atomic::Ordering;

use tauri::{AppHandle, State};

use crate::app_state::AppState;

use super::{
    build_report, models, publish_report, reconcile, settings, status::AmbientStatus, stop_session,
    wake_word, AmbientVoiceSettings, AmbientVoiceStatusReport, MAX_AUDIO_BATCH_BYTES,
    MAX_CAPTURE_ERROR_CHARS,
};
use wake_word::{WakeWordTokenizer, MAX_WAKE_WORD_CHARS};

fn ensure_writable(state: &AppState) -> Result<(), String> {
    if let Some(error) = state
        .ambient_voice
        .load_error
        .lock()
        .map_err(|error| format!("ambient voice settings lock poisoned: {error}"))?
        .as_ref()
    {
        return Err(format!(
            "Ambient voice settings were not saved because the existing file could not be loaded: {error}"
        ));
    }
    Ok(())
}

/// Protect the dragged indicator position from a whole-object settings write.
///
/// The settings screen loads its `AmbientVoiceSettings` once and posts the
/// whole object back on every change, so a copy fetched before the user
/// dragged the pill would otherwise put it back where it was.
/// [`set_ambient_indicator_position`] is the only writer of this field, and
/// `stored` is what it last wrote.
pub(super) fn keep_stored_indicator_position(
    mut next: AmbientVoiceSettings,
    stored: Option<settings::IndicatorPosition>,
) -> AmbientVoiceSettings {
    next.indicator_position = stored.or(next.indicator_position);
    next
}

async fn persist_and_reconcile(
    app: &AppHandle,
    state: &AppState,
    next: AmbientVoiceSettings,
) -> Result<AmbientVoiceStatusReport, String> {
    ensure_writable(state)?;
    let next = keep_stored_indicator_position(
        next,
        state.ambient_voice.settings_snapshot()?.indicator_position,
    );
    settings::save_to_path(&settings::settings_path(app)?, &next)?;
    state
        .ambient_voice
        .muted
        .store(next.muted, Ordering::Release);
    *state
        .ambient_voice
        .settings
        .lock()
        .map_err(|error| format!("ambient voice settings lock poisoned: {error}"))? = next;
    reconcile(state).await?;
    build_report(state)
}

/// Read the persisted ambient settings.
#[tauri::command]
pub fn get_ambient_voice_settings(
    state: State<'_, AppState>,
) -> Result<AmbientVoiceSettings, String> {
    if let Some(error) = state
        .ambient_voice
        .load_error
        .lock()
        .map_err(|error| format!("ambient voice settings lock poisoned: {error}"))?
        .clone()
    {
        return Err(format!(
            "Ambient voice settings could not be loaded and were left unchanged: {error}"
        ));
    }
    state.ambient_voice.settings_snapshot()
}

/// Apply a client settings payload over what the runtime currently holds.
///
/// `muted` and `enabled` only ever change through their dedicated commands
/// (`set_ambient_voice_muted`, `set_ambient_voice_enabled`); the settings card
/// holds a copy from whenever it mounted, so a later save from it must not be
/// able to re-assert those two fields.
pub(super) fn merge_client_settings(
    current: &AmbientVoiceSettings,
    incoming: AmbientVoiceSettings,
) -> AmbientVoiceSettings {
    AmbientVoiceSettings {
        version: settings::CURRENT_VERSION,
        muted: current.muted,
        enabled: current.enabled,
        ..incoming
    }
}

/// Replace the ambient settings and reconcile the runtime.
#[tauri::command]
pub async fn set_ambient_voice_settings(
    settings: AmbientVoiceSettings,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<AmbientVoiceStatusReport, String> {
    let next = merge_client_settings(&state.ambient_voice.settings_snapshot()?, settings);
    persist_and_reconcile(&app, &state, next).await
}

/// The Experiments toggle's native side effect.
///
/// Mirrors `set_agent_managed_profiles`: flipping the preview flag in the
/// frontend also has to tell the native runtime to start or stop, otherwise
/// the microphone would keep running after the feature was switched off.
#[tauri::command]
pub async fn set_ambient_voice_enabled(
    enabled: bool,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<AmbientVoiceStatusReport, String> {
    let mut next = state.ambient_voice.settings_snapshot()?;
    next.enabled = enabled;
    if enabled {
        models::ensure_kws_download(state.http_client.clone());
    }
    persist_and_reconcile(&app, &state, next).await
}

/// Mute or unmute the microphone without losing the session.
#[tauri::command]
pub async fn set_ambient_voice_muted(
    muted: bool,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<AmbientVoiceStatusReport, String> {
    let mut next = state.ambient_voice.settings_snapshot()?;
    next.muted = muted;
    // Apply to the live worker immediately — the user pressed mute, and a
    // disk write must not sit between them and a closed microphone.
    if let Ok(runtime) = state.ambient_voice.runtime() {
        if let Some(session) = runtime.session.as_ref() {
            session.set_muted(muted);
        }
    }
    persist_and_reconcile(&app, &state, next).await
}

/// Remember where the user dragged the listening indicator.
///
/// Its own command rather than a `set_ambient_voice_settings` round trip for
/// two reasons: dragging a pill must not take the session transition lock or
/// run reconciliation, and the settings screen holds a settings object it
/// fetched before the drag, so a whole-object write from there would clobber
/// the position. Nothing about the running session changes, so no state event
/// is emitted — the caller already has the answer in the returned report.
#[tauri::command]
pub fn set_ambient_indicator_position(
    position: Option<settings::IndicatorPosition>,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<AmbientVoiceStatusReport, String> {
    ensure_writable(&state)?;
    let mut next = state.ambient_voice.settings_snapshot()?;
    next.indicator_position = position;
    settings::save_to_path(&settings::settings_path(&app)?, &next)?;
    *state
        .ambient_voice
        .settings
        .lock()
        .map_err(|error| format!("ambient voice settings lock poisoned: {error}"))? = next;
    build_report(&state)
}

/// Current runtime status, for the indicator and the reply watcher.
#[tauri::command]
pub fn get_ambient_voice_status(
    state: State<'_, AppState>,
) -> Result<AmbientVoiceStatusReport, String> {
    build_report(&state)
}

/// Download status for the models the ambient session needs.
#[tauri::command]
pub fn get_ambient_model_status() -> Result<models::AmbientModelStatus, String> {
    models::ambient_model_status()
}

/// Retry a session start that could not happen earlier.
///
/// The wake-word model downloads on demand, so the first `enable` usually
/// cannot start a session. The frontend provider polls this on the same timer
/// huddles use for `check_pipeline_hotstart`; it is also what recovers a worker
/// that exited (a corrupt model, an engine failure). No-op unless a session
/// should be running and is not.
#[tauri::command]
pub async fn check_ambient_hotstart(
    state: State<'_, AppState>,
) -> Result<AmbientVoiceStatusReport, String> {
    let ambient = &state.ambient_voice;
    // Consume the one-shot "just finished downloading" edge unconditionally so
    // it cannot be observed later by a start that has already happened.
    let just_ready = models::take_kws_ready();

    if !ambient.settings_snapshot()?.is_runnable() {
        return build_report(&state);
    }
    let alive = ambient
        .runtime()?
        .session
        .as_ref()
        .is_some_and(|session| !session.is_finished());
    if alive || !(just_ready || models::is_kws_ready()) {
        return build_report(&state);
    }
    reconcile(&state).await?;
    build_report(&state)
}

/// Result of checking a candidate wake word in the settings UI.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WakeWordCheck {
    pub valid: bool,
    /// Present when invalid — shown verbatim to the user.
    pub message: Option<String>,
    /// The tokenised form, when the model is available. Diagnostic only.
    pub tokens: Option<Vec<String>>,
    /// False when the wake-word model is not installed yet, in which case only
    /// the model-independent checks ran.
    pub checked_against_model: bool,
}

/// Validate a candidate wake word.
///
/// The settings UI calls this on every edit. When the model is installed the
/// phrase is tokenised and every piece checked against the engine vocabulary —
/// the same gate the session applies, so a phrase accepted here cannot kill
/// the process later.
#[tauri::command]
pub fn check_ambient_wake_word(wake_word: String) -> WakeWordCheck {
    if wake_word.chars().count() > MAX_WAKE_WORD_CHARS {
        return WakeWordCheck {
            valid: false,
            message: Some(format!(
                "Wake word is too long (max {MAX_WAKE_WORD_CHARS} characters)"
            )),
            tokens: None,
            checked_against_model: false,
        };
    }
    if let Err(error) = wake_word::validate_wake_word(&wake_word) {
        return WakeWordCheck {
            valid: false,
            message: Some(error.to_string()),
            tokens: None,
            checked_against_model: false,
        };
    }
    let Some(tokenizer) =
        models::kws_model_dir().and_then(|dir| WakeWordTokenizer::load(&dir).ok())
    else {
        return WakeWordCheck {
            valid: true,
            message: None,
            tokens: None,
            checked_against_model: false,
        };
    };
    match tokenizer.tokenize(&wake_word) {
        Ok(tokens) => WakeWordCheck {
            valid: true,
            message: None,
            tokens: Some(tokens),
            checked_against_model: true,
        },
        Err(error) => WakeWordCheck {
            valid: false,
            message: Some(error.to_string()),
            tokens: None,
            checked_against_model: true,
        },
    }
}

/// Raw-binary PCM sink for the ambient AudioWorklet.
///
/// A separate command from `push_audio_pcm` rather than a consumer registry:
/// huddle's is hard-wired to huddle state, and a parallel command keeps the
/// preview feature's blast radius at zero for users who never enable it.
#[tauri::command]
pub fn push_ambient_audio_pcm(
    request: tauri::ipc::Request<'_>,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let tauri::ipc::InvokeBody::Raw(bytes) = request.body() else {
        return Err("expected raw binary body".to_string());
    };
    if bytes.len() > MAX_AUDIO_BATCH_BYTES {
        return Err(format!(
            "audio batch too large: {} bytes (max {MAX_AUDIO_BATCH_BYTES})",
            bytes.len()
        ));
    }
    let runtime = state.ambient_voice.runtime()?;
    let Some(session) = runtime.session.as_ref() else {
        // No session: the webview is ahead of a teardown. Dropping is correct
        // and must not surface as an error the provider would log on repeat.
        return Ok(());
    };
    session.push_audio(bytes.to_vec())
}

/// Make a webview-supplied failure fit to show.
///
/// It reaches the indicator verbatim, so an empty message would leave the pill
/// blank and an unbounded one would push the rest of the app's chrome off it.
pub(super) fn capture_error_detail(message: &str) -> String {
    let message = message.trim();
    if message.is_empty() {
        return "The microphone could not be opened for ambient voice".to_string();
    }
    message.chars().take(MAX_CAPTURE_ERROR_CHARS).collect()
}

/// Turn a webview capture failure into the error state.
///
/// Split from the command so the transition lock stays at the command boundary
/// and this is testable against a plain `AppState`.
pub(super) fn apply_capture_error(state: &AppState, message: &str) -> Result<(), String> {
    if state.ambient_voice.runtime()?.session.is_none() {
        // The session is already down — a stop the webview had not seen yet.
        // There is no false "listening" to correct here, and overwriting `Off`
        // with a late failure would be a lie of its own.
        return Ok(());
    }
    stop_session(state, AmbientStatus::Error(capture_error_detail(message)))?;
    publish_report(state);
    Ok(())
}

/// Report that the webview could not keep a microphone open.
///
/// The microphone is acquired in the webview (`getUserMedia`), so a device that
/// is refused, cannot be opened, or is unplugged mid-session is invisible to
/// the worker: it simply never receives another sample, and goes on reporting
/// that it is listening for the wake word. That false state is the whole
/// problem — the indicator exists to say what is happening to the audio.
///
/// The session is stopped rather than left running under a pinned error for two
/// reasons: nothing is reaching it, and a stopped session is what
/// [`check_ambient_hotstart`] re-arms once the device comes back or the user
/// picks another one — the same recovery a worker that exited already gets, so
/// the webview needs no retry loop of its own.
#[tauri::command]
pub async fn report_ambient_capture_error(
    message: String,
    state: State<'_, AppState>,
) -> Result<AmbientVoiceStatusReport, String> {
    let _transition = state.ambient_voice.transition.lock().await;
    apply_capture_error(&state, &message)?;
    build_report(&state)
}

/// Speak an agent reply through the ambient TTS pipeline.
///
/// Called by the ambient reply watcher, mirroring `speak_agent_message`.
/// Disabled is the only intentional no-op; enabled-but-unavailable is an error
/// so a dropped reply is never mistaken for success.
#[tauri::command]
pub async fn ambient_speak(text: String, state: State<'_, AppState>) -> Result<(), String> {
    let text = text.trim().to_string();
    if text.is_empty() {
        return Ok(());
    }
    let ambient = &state.ambient_voice;
    if !ambient.settings_snapshot()?.enabled {
        return Ok(());
    }
    if ambient.muted.load(Ordering::Acquire) {
        // Mute silences output as well as input: the user asked for quiet.
        return Ok(());
    }
    let pipeline = {
        let runtime = ambient.runtime()?;
        runtime.tts.clone()
    };
    let Some(pipeline) = pipeline else {
        return Err("Ambient voice is enabled but its speech pipeline is unavailable".to_string());
    };
    // Clear a barge-in the TTS worker never got to consume (the wake word
    // fired while nothing was playing). The worker consumes the flag itself
    // when it *is* playing, within one 10 ms monitor tick, so by the time a
    // reply has been transcribed, published, answered and routed back here
    // an outstanding flag means "nothing was interrupted".
    ambient.tts_cancel.store(false, Ordering::Release);
    pipeline.speak(text)
}
