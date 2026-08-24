//! The Tauri command surface for the ambient feature.
//!
//! Its own file rather than the tail of [`super`], which is what the audio
//! worker, the huddle and boot hydration reach into. Nothing here decides
//! anything by itself: every command reads or writes the settings and then
//! funnels through [`super::reconcile`], which remains the only place a session
//! is created.

use std::{sync::atomic::Ordering, time::Instant};

use tauri::{AppHandle, State};

use crate::app_state::AppState;

use super::{
    app_handle, build_report, capture_failure_is_pacing, emit_state_changed, models,
    publish_report, reconcile, settings, speech_http, status::AmbientStatus, stop_session,
    wake_word, AmbientVoiceSettings, AmbientVoiceStatusReport, WebviewCaptureFlow,
    MAX_AUDIO_BATCH_BYTES, MAX_CAPTURE_ERROR_CHARS,
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
///
/// The one retry it paces is a reported capture failure, by
/// `CAPTURE_ERROR_BACKOFF`: a microphone the webview cannot open fails again as
/// soon as this rebuilds a session, and at a three-second poll that is two ONNX
/// model loads every three seconds for as long as the device stays broken.
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
    // Read the timestamp out before the predicate so no runtime guard is held
    // across the await below.
    let last_capture_error = ambient.runtime()?.last_capture_error;
    if capture_failure_is_pacing(
        &ambient.current_status(),
        last_capture_error,
        Instant::now(),
    ) {
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

/// Validate a candidate stop phrase.
///
/// The stop-phrase field's counterpart to [`check_ambient_wake_word`], and it
/// exists for the same reason: the phrase is armed on the same keyword spotter,
/// so a phrase the model cannot encode fails the whole session at arm time. It
/// went unchecked while the wake word beside it was checked on every keystroke,
/// which made a full stop or a digit in this one field enough to take ambient
/// voice down.
///
/// Two rules the wake word does not have: an empty phrase is *valid* (it is how
/// the feature is switched off), and the phrase must differ from the wake word —
/// one keyword armed twice leaves no answer to which job a detection is doing.
/// Both live in `settings::validate_stop_phrase_against`, so the rule the UI
/// reports is the rule the save door enforces.
///
/// `wake_word` is the settings card's field as it currently reads, which is not
/// always the wake word the save door will see — hence the stored binding as
/// well; [`stop_phrase_check`] carries the reason.
#[tauri::command]
pub fn check_ambient_stop_phrase(
    stop_phrase: String,
    wake_word: String,
    state: State<'_, AppState>,
) -> WakeWordCheck {
    let saved = state
        .ambient_voice
        .settings_snapshot()
        .ok()
        .and_then(|settings| {
            settings
                .primary_binding()
                .map(|binding| binding.wake_word.clone())
        });
    stop_phrase_check(
        &stop_phrase,
        &wake_word,
        saved.as_deref(),
        settings::installed_tokenizer().as_ref(),
    )
}

/// Whether `stop_phrase` can be saved, against both wake words in play.
///
/// `typed` is what the wake-word field reads right now; `saved` is the wake word
/// the stored binding still carries. They agree until the field is edited, and
/// the field is not what the save door validates against: the settings card
/// posts a stop phrase over the settings object it loaded, so the binding in
/// that payload — and therefore in `save_to_path` — is the stored one until the
/// wake word is saved in its own right.
///
/// Checking the typed one alone therefore answered "valid" for a phrase the very
/// next save refused, with an error about a wake word that was no longer on the
/// screen. Both are asked now, and a clash with the stored one names it, because
/// it is the one thing here that cannot be read off the screen.
///
/// Naming it is as far as this goes. The settings card gates every field's save
/// on one verdict, so this message is also the line telling the user why nothing
/// else is saving — including the wake word, which is what would resolve the
/// clash. A message from here that told them to go and save it would be the
/// reason they could not.
pub(super) fn stop_phrase_check(
    stop_phrase: &str,
    typed: &str,
    saved: Option<&str>,
    tokenizer: Option<&WakeWordTokenizer>,
) -> WakeWordCheck {
    let refused = |message: String| WakeWordCheck {
        valid: false,
        message: Some(message),
        tokens: None,
        checked_against_model: tokenizer.is_some(),
    };
    let probe = AmbientVoiceSettings {
        wake_bindings: wake_binding_for_check(typed),
        stop_phrase: Some(stop_phrase.to_string()),
        ..AmbientVoiceSettings::default()
    };
    if let Err(message) = settings::validate_stop_phrase_against(&probe, tokenizer) {
        return refused(message);
    }
    // The same question again, of the wake word the save door will see. Only
    // the clash rule reads the binding, so this is the only rule that can
    // answer differently from the pass above.
    if let Some(saved) = saved.filter(|saved| !saved.trim().is_empty() && *saved != typed) {
        let stored = AmbientVoiceSettings {
            wake_bindings: wake_binding_for_check(saved),
            ..probe.clone()
        };
        if let Err(message) = settings::validate_stop_phrase_against(&stored, tokenizer) {
            return refused(format!(
                "{message}, and \"{saved}\" is still the saved wake word"
            ));
        }
    }
    WakeWordCheck {
        valid: true,
        message: None,
        tokens: tokenizer
            .zip(probe.armed_stop_phrase())
            .and_then(|(tokenizer, phrase)| tokenizer.tokenize(phrase).ok()),
        checked_against_model: tokenizer.is_some(),
    }
}

/// A stand-in binding carrying `wake_word`, for the clash rule alone.
///
/// The clash check reads `primary_binding()`, and the settings UI can be asked
/// about a stop phrase before an agent has been chosen. The agent key is
/// therefore a placeholder that is never validated, never persisted and never
/// leaves this function — an empty wake word yields no binding at all, which is
/// the honest answer to "does this clash with nothing".
fn wake_binding_for_check(wake_word: &str) -> Vec<settings::WakeBinding> {
    if wake_word.trim().is_empty() {
        return Vec::new();
    }
    vec![settings::WakeBinding {
        wake_word: wake_word.to_string(),
        agent_pubkey: String::new(),
        destination: None,
    }]
}

/// Ask a speech server whether it is there, for the settings "Check" button.
///
/// Its own command rather than a side effect of saving, because the answer is
/// about the address the user is typing and not about the session: nothing
/// here starts, stops or reconfigures anything. A URL that cannot be reached
/// is still saved — the server may simply be off — and the session goes on
/// running the way it already was.
///
/// The probe is a `GET` on the health path. Two hundred means ready; anything
/// else, including nothing at all, is `unreachable` with the reason attached;
/// an address that could never be probed is `malformed`, which is a different
/// fault in a different place (the field, not the network) and reads as one.
#[tauri::command]
pub async fn check_speech_endpoint(
    url: String,
) -> Result<speech_http::SpeechEndpointCheck, String> {
    Ok(speech_http::probe_endpoint(&url).await)
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

/// The webview's own count of the audio it has pushed.
///
/// Called on a slow cadence while the webview holds a capture pipeline. Two
/// jobs, both diagnostic:
///
/// 1. It carries the other half of the audio path into the report. "Capturing
///    but deaf" has two very different causes — batches pushed that never
///    arrive, or batches never pushed at all — and only the webview can count
///    the second.
/// 2. It is what notices that a running session has gone quiet. Staleness moves
///    with the clock rather than with a session transition, so nothing else
///    would ever emit it; the announcement is edge-triggered so a quiet session
///    does not re-emit the same report to every window every few seconds.
///
/// Recording a count is not a lifecycle event: nothing here starts, stops or
/// paces a session.
#[tauri::command]
pub fn report_ambient_audio_flow(
    pushed: u64,
    capture_ready: bool,
    state: State<'_, AppState>,
) -> Result<AmbientVoiceStatusReport, String> {
    state.ambient_voice.runtime()?.webview_capture = Some(WebviewCaptureFlow {
        batches_pushed: pushed,
        capture_ready,
    });
    let report = build_report(&state)?;
    let was_stale = state
        .ambient_voice
        .stale_announced
        .swap(report.audio_stale, Ordering::AcqRel);
    if was_stale != report.audio_stale {
        emit_state_changed(app_handle(&state).as_ref(), &report);
    }
    Ok(report)
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
    // Recorded after the stop, which deliberately leaves it alone: this paces
    // the automatic retry, and the session that just ended is not what failed.
    state.ambient_voice.runtime()?.last_capture_error = Some(Instant::now());
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
/// the webview needs no retry loop of its own. That automatic re-arm is paced
/// by `CAPTURE_ERROR_BACKOFF`, so a device that stays broken costs one session
/// rebuild every thirty seconds rather than one per poll; anything the user
/// does reaches [`super::reconcile`] directly and is never paced.
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
