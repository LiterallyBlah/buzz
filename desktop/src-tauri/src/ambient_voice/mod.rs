//! Ambient voice — the `ambientVoice` preview feature.
//!
//! A wake word, one agent, all inference on-device. The user says the wake
//! word, speaks, and the utterance arrives in the bound agent's DM as an
//! ordinary kind:9 message; the agent's reply is read back aloud. Nothing but
//! relay traffic leaves the machine.
//!
//! ## Layout
//!
//! | Module | Responsibility |
//! |---|---|
//! | [`settings`] | versioned `ambient-voice-settings.json` |
//! | [`wake_word`] | tokenisation + the strict validation the engine needs |
//! | [`utterance`] | the capture state machine (pure, clock-injected) |
//! | [`session`] | the audio worker: spotter → barge-in → VAD → recogniser |
//! | [`publish`] | egress boundary 9: kind:9 transcripts and kind:48106 |
//! | [`models`] | wake-word model access over the shared download manager |
//! | [`status`] | what the listening indicator shows |
//!
//! ## Lifecycle
//!
//! Nothing runs unless the preview flag is on **and** the persisted settings
//! say enabled **and** a wake binding exists **and** no huddle owns the
//! microphone. Every one of those is a separate gate on purpose: an always-open
//! microphone should be hard to switch on by accident and trivial to switch
//! off.
//!
//! ## Backend seam
//!
//! `settings::SpeechBackend` has one variant today. It exists so server-side
//! speech (a later milestone) becomes a new variant plus a new implementation
//! behind the same call sites, rather than a restructure. There is deliberately
//! **no** HTTP code here.

pub mod models;
pub mod publish;
pub mod session;
pub mod settings;
pub mod status;
pub mod utterance;
pub mod wake_word;

use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc, Mutex,
};

use tauri::{AppHandle, Emitter, Manager, State};
use uuid::Uuid;

use crate::app_state::AppState;
use crate::huddle::{state::HuddlePhase, tts::TtsPipeline, tts_settings};

use publish::{AmbientDestination, AmbientPublisher};
use session::{AmbientSession, AmbientSessionConfig};
use settings::AmbientVoiceSettings;
use status::AmbientStatus;
use wake_word::{WakeWordTokenizer, MAX_WAKE_WORD_CHARS};

/// Event name the frontend listens on for every ambient state transition.
pub const STATE_CHANGED_EVENT: &str = "ambient-voice-state-changed";

/// Sample rate of the PCM the AudioWorklet pushes in.
const WORKLET_SAMPLE_RATE: u32 = 48_000;

/// Upper bound on one raw-binary audio batch, mirroring `push_audio_pcm`.
const MAX_AUDIO_BATCH_BYTES: usize = 1024 * 1024;

// ── Runtime state ────────────────────────────────────────────────────────────

/// Live session objects. Everything here is torn down together.
#[derive(Default)]
pub struct AmbientRuntime {
    session: Option<AmbientSession>,
    tts: Option<Arc<TtsPipeline>>,
    destination: Option<Arc<AmbientDestination>>,
    /// Set when a huddle claimed the microphone; the session resumes on
    /// huddle teardown without the user touching anything.
    suspended_by_huddle: bool,
}

/// The `AppState`-held ambient feature state.
pub struct AmbientVoiceState {
    pub settings: Mutex<AmbientVoiceSettings>,
    /// Non-`None` when the settings file could not be read. Writes are
    /// refused while it is set so a half-understood file is never clobbered.
    pub load_error: Mutex<Option<String>>,
    runtime: Mutex<AmbientRuntime>,
    /// Serializes start/stop so two toggles cannot build two sessions.
    transition: tokio::sync::Mutex<()>,
    /// Shared with the ambient TTS pipeline and the audio worker.
    tts_active: Arc<AtomicBool>,
    tts_cancel: Arc<AtomicBool>,
    muted: Arc<AtomicBool>,
    reported: Arc<Mutex<AmbientStatus>>,
    /// Invalidates in-flight publisher tasks from a previous session.
    generation: Arc<AtomicU64>,
}

impl Default for AmbientVoiceState {
    fn default() -> Self {
        Self {
            settings: Mutex::new(AmbientVoiceSettings::default()),
            load_error: Mutex::new(None),
            runtime: Mutex::new(AmbientRuntime::default()),
            transition: tokio::sync::Mutex::new(()),
            tts_active: Arc::new(AtomicBool::new(false)),
            tts_cancel: Arc::new(AtomicBool::new(false)),
            muted: Arc::new(AtomicBool::new(false)),
            reported: Arc::new(Mutex::new(AmbientStatus::Off)),
            generation: Arc::new(AtomicU64::new(0)),
        }
    }
}

impl AmbientVoiceState {
    fn settings_snapshot(&self) -> Result<AmbientVoiceSettings, String> {
        self.settings
            .lock()
            .map(|settings| settings.clone())
            .map_err(|error| format!("ambient voice settings lock poisoned: {error}"))
    }

    fn runtime(&self) -> Result<std::sync::MutexGuard<'_, AmbientRuntime>, String> {
        self.runtime
            .lock()
            .map_err(|error| format!("ambient voice runtime lock poisoned: {error}"))
    }

    fn set_status(&self, next: AmbientStatus) {
        *self.reported.lock().unwrap_or_else(|e| e.into_inner()) = next;
    }

    /// The status a caller should see: the worker's own view while it is
    /// running, otherwise the lifecycle status recorded here.
    fn current_status(&self) -> AmbientStatus {
        let running = self
            .runtime()
            .ok()
            .and_then(|runtime| runtime.session.as_ref().map(AmbientSession::status));
        running.unwrap_or_else(|| {
            self.reported
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
        })
    }
}

/// What the frontend needs to render the indicator and bind its reply watcher.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AmbientVoiceStatusReport {
    /// The persisted `enabled` flag, not whether a worker happens to be alive.
    pub enabled: bool,
    pub muted: bool,
    pub suspended_by_huddle: bool,
    /// True only when a worker thread is running and consuming audio. The
    /// frontend uses this to decide whether to hold the microphone open.
    pub capturing: bool,
    pub status: AmbientStatus,
    /// True while audio is actually being processed. The indicator uses this
    /// rather than re-deriving it from `status`, so "muted" and "suspended"
    /// cannot accidentally read as live in the UI.
    pub live: bool,
    /// Destination channel for transcripts and the reply watcher. `None` until
    /// the session resolves it.
    pub destination_channel_id: Option<String>,
    pub agent_pubkey: Option<String>,
    pub wake_word: Option<String>,
    /// Persisted input device. The frontend re-acquires the microphone when
    /// this changes, so a device chosen in settings takes effect without a
    /// restart.
    pub input_device_id: Option<String>,
    /// Where the user parked the indicator, or `None` for the default corner.
    /// Carried on the report so the pill can restore itself from the same
    /// snapshot it already fetches, rather than a second settings read.
    pub indicator_position: Option<settings::IndicatorPosition>,
    /// Set when settings could not be loaded; writes are refused meanwhile.
    pub load_error: Option<String>,
}

fn emit_state_changed(app: Option<&AppHandle>, report: &AmbientVoiceStatusReport) {
    if let Some(app) = app {
        let _ = app.emit(STATE_CHANGED_EVENT, report);
    }
}

fn app_handle(state: &AppState) -> Option<AppHandle> {
    state.app_handle.lock().ok().and_then(|guard| guard.clone())
}

fn build_report(state: &AppState) -> Result<AmbientVoiceStatusReport, String> {
    let ambient = &state.ambient_voice;
    let settings = ambient.settings_snapshot()?;
    let runtime = ambient.runtime()?;
    let destination = runtime.destination.clone();
    let capturing = runtime
        .session
        .as_ref()
        .is_some_and(|session| !session.is_finished());
    let suspended_by_huddle = runtime.suspended_by_huddle;
    drop(runtime);
    let status = ambient.current_status();
    Ok(AmbientVoiceStatusReport {
        enabled: settings.enabled,
        muted: ambient.muted.load(Ordering::Acquire),
        suspended_by_huddle,
        capturing,
        live: status.is_live(),
        status,
        destination_channel_id: destination
            .as_ref()
            .map(|destination| destination.channel_id.to_string()),
        agent_pubkey: destination
            .as_ref()
            .map(|destination| destination.agent_pubkey.clone()),
        wake_word: settings
            .primary_binding()
            .map(|binding| binding.wake_word.clone()),
        input_device_id: settings.input_device_id.clone(),
        indicator_position: settings.indicator_position,
        load_error: ambient
            .load_error
            .lock()
            .map(|error| error.clone())
            .unwrap_or(None),
    })
}

fn publish_report(state: &AppState) {
    match build_report(state) {
        Ok(report) => emit_state_changed(app_handle(state).as_ref(), &report),
        Err(error) => eprintln!("buzz-desktop: ambient state report failed: {error}"),
    }
}

// ── Lifecycle ────────────────────────────────────────────────────────────────

fn huddle_owns_microphone(state: &AppState) -> bool {
    state
        .huddle()
        .map(|huddle| !matches!(huddle.phase, HuddlePhase::Idle))
        .unwrap_or(false)
}

/// Whether a session should be running right now.
///
/// Split out as a pure predicate so the "flag off ⇒ nothing runs" property is
/// directly testable without models, a relay, or a microphone.
pub fn should_run(settings: &AmbientVoiceSettings, huddle_active: bool) -> bool {
    settings.is_runnable() && !huddle_active
}

/// Stop the worker, the TTS pipeline and any in-flight publisher.
///
/// Locks are never held across the thread joins: pipeline `Drop` joins worker
/// threads, and ONNX teardown is not instant.
fn stop_session(state: &AppState, next: AmbientStatus) -> Result<(), String> {
    let ambient = &state.ambient_voice;
    ambient.generation.fetch_add(1, Ordering::Release);
    let (session, tts) = {
        let mut runtime = ambient.runtime()?;
        runtime.destination = None;
        (runtime.session.take(), runtime.tts.take())
    };
    if let Some(ref session) = session {
        session.shutdown();
    }
    if let Some(ref tts) = tts {
        tts.shutdown();
    }
    drop(session);
    drop(tts);
    ambient.tts_cancel.store(false, Ordering::Release);
    ambient.tts_active.store(false, Ordering::Release);
    ambient.set_status(next);
    Ok(())
}

/// Bring the session up if it should be, or take it down if it should not.
///
/// Idempotent, and the only place a session is created. Callers that changed
/// settings, mute, the huddle phase or the toggle all funnel through here.
pub async fn reconcile(state: &AppState) -> Result<(), String> {
    let ambient = &state.ambient_voice;
    let _transition = ambient.transition.lock().await;

    let settings = ambient.settings_snapshot()?;
    let huddle_active = huddle_owns_microphone(state);

    if huddle_active {
        let was_running = ambient.runtime()?.session.is_some();
        // Record the suspension whenever a huddle is up, so a session started
        // later resumes automatically on teardown.
        if settings.is_runnable() {
            ambient.runtime()?.suspended_by_huddle = true;
        }
        if was_running {
            stop_session(state, AmbientStatus::Suspended)?;
        } else if settings.is_runnable() {
            ambient.set_status(AmbientStatus::Suspended);
        }
        publish_report(state);
        return Ok(());
    }
    ambient.runtime()?.suspended_by_huddle = false;

    if !should_run(&settings, huddle_active) {
        if ambient.runtime()?.session.is_some() {
            stop_session(state, AmbientStatus::Off)?;
        } else {
            ambient.set_status(AmbientStatus::Off);
        }
        publish_report(state);
        return Ok(());
    }

    // Already running and healthy — nothing to do.
    let alive = ambient
        .runtime()?
        .session
        .as_ref()
        .is_some_and(|session| !session.is_finished());
    if alive {
        publish_report(state);
        return Ok(());
    }
    if ambient.runtime()?.session.is_some() {
        // A worker that exited (bad model, engine failure) must be cleared
        // before a retry, exactly as huddle hot-start does.
        stop_session(state, AmbientStatus::Starting)?;
    }

    ambient.set_status(AmbientStatus::Starting);
    publish_report(state);

    if let Err(error) = start_session(state, &settings).await {
        eprintln!("buzz-desktop: ambient session could not start: {error}");
        let _ = stop_session(state, AmbientStatus::Error(error));
    }
    publish_report(state);
    Ok(())
}

async fn start_session(state: &AppState, settings: &AmbientVoiceSettings) -> Result<(), String> {
    let binding = settings
        .primary_binding()
        .cloned()
        .ok_or("no wake word is bound to an agent")?;

    // The wake-word model downloads on demand, not at launch — this is a
    // preview feature and most installs will never need its ~18 MB.
    models::ensure_kws_download(state.http_client.clone());

    let kws_dir = models::kws_model_dir().ok_or(
        "the wake-word model is still downloading — ambient voice will start when it is ready",
    )?;
    let stt_dir = models::stt_model_dir().ok_or(
        "the speech-to-text model is still downloading — ambient voice will start when it is ready",
    )?;

    // Strict validation, always, before anything reaches the engine.
    let tokenizer = WakeWordTokenizer::load(&kws_dir)?;
    let keywords_buf = tokenizer
        .keywords_buf(std::slice::from_ref(&binding.wake_word))
        .map_err(|(phrase, error)| format!("wake word \"{phrase}\" cannot be used: {error}"))?;

    let destination_channel =
        publish::resolve_destination(state, &binding.agent_pubkey, binding.destination.as_deref())
            .await?;
    let channel_id = Uuid::parse_str(&destination_channel)
        .map_err(|_| "ambient destination is not a channel".to_string())?;

    let ambient = &state.ambient_voice;
    let tts = start_ambient_tts(state, settings).await?;

    let (session, transcript_rx) = AmbientSession::new(AmbientSessionConfig {
        kws_model_dir: kws_dir,
        stt_model_dir: stt_dir,
        keywords_buf,
        tts_active: Arc::clone(&ambient.tts_active),
        tts_cancel: Arc::clone(&ambient.tts_cancel),
        muted: Arc::clone(&ambient.muted),
        status: Arc::clone(&ambient.reported),
        on_status_change: worker_status_notifier(state),
        input_sample_rate: WORKLET_SAMPLE_RATE,
    })?;

    let destination = Arc::new(AmbientDestination {
        channel_id,
        agent_pubkey: binding.agent_pubkey.clone(),
        wake_word: binding.wake_word.clone(),
        guidelines_sent: Arc::new(AtomicBool::new(false)),
    });
    let publisher = AmbientPublisher::from_state(state)?;
    spawn_publisher_task(
        transcript_rx,
        Arc::clone(&destination),
        publisher,
        Arc::clone(&ambient.generation),
    );

    {
        let mut runtime = ambient.runtime()?;
        runtime.session = Some(session);
        runtime.tts = tts;
        runtime.destination = Some(destination);
    }
    Ok(())
}

/// The audio worker's route to the indicator.
///
/// Without this the only `STATE_CHANGED_EVENT` emits are the lifecycle ones —
/// on/off, mute, huddle arbitration — so the pill shows whatever the session
/// was doing when it last started and never moves through armed → heard →
/// capturing → transcribing → speaking. Reusing `publish_report` rather than a
/// second, thinner event keeps one payload shape on the wire.
///
/// `None` when there is no app handle (unit tests, early boot): the session
/// still runs, the indicator simply does not live-update.
fn worker_status_notifier(state: &AppState) -> Option<session::AmbientStatusNotifier> {
    let app = app_handle(state)?;
    let notifier: session::AmbientStatusNotifier = Arc::new(move |_status: &AmbientStatus| {
        // The status is read back out of `AppState` by `build_report`, so the
        // argument is only the transition that woke us.
        let Some(state) = app.try_state::<AppState>() else {
            return;
        };
        publish_report(&state);
    });
    Some(notifier)
}

/// Build the ambient TTS pipeline.
///
/// Deliberately its own pipeline rather than the huddle's: the huddle's is
/// gated on `HuddlePhase::Active` and is torn down with the huddle, and the
/// two must never contend for the output device. A missing TTS model is not
/// fatal — the transcript path still works, the replies just are not spoken.
async fn start_ambient_tts(
    state: &AppState,
    settings: &AmbientVoiceSettings,
) -> Result<Option<Arc<TtsPipeline>>, String> {
    let Some(model_dir) = models::tts_model_dir() else {
        eprintln!("buzz-desktop: ambient voice started without TTS (model not ready)");
        return Ok(None);
    };
    let app = app_handle(state);
    let preferences = state
        .huddle_audio
        .tts
        .lock()
        .map_err(|error| format!("text-to-speech settings lock poisoned: {error}"))
        .map(|tts| tts.voice_preferences.clone())?;
    let voice = match app.as_ref() {
        Some(app) => tts_settings::pocket_voice_reference(app, &preferences)?,
        None => tts_settings::bundled_pocket_voice_reference(&preferences),
    };

    let ambient = &state.ambient_voice;
    let tts_active = Arc::clone(&ambient.tts_active);
    let tts_cancel = Arc::clone(&ambient.tts_cancel);
    tts_cancel.store(false, Ordering::Release);
    let output_device = settings.output_device.clone();

    // Construction loads ONNX sessions (~200 ms) — off the async runtime.
    let built = tokio::task::spawn_blocking(move || {
        TtsPipeline::new_with_voice(
            model_dir,
            tts_active,
            tts_cancel,
            &voice,
            output_device,
            app,
        )
    })
    .await
    .map_err(|error| format!("ambient TTS startup panicked: {error}"))?;

    match built {
        Ok(pipeline) => Ok(Some(Arc::new(pipeline))),
        Err(error) => {
            eprintln!("buzz-desktop: ambient TTS unavailable: {error}");
            Ok(None)
        }
    }
}

/// Drain transcripts and publish them, until the session generation moves.
fn spawn_publisher_task(
    mut transcript_rx: tokio::sync::mpsc::Receiver<String>,
    destination: Arc<AmbientDestination>,
    publisher: AmbientPublisher,
    generation: Arc<AtomicU64>,
) {
    let spawned_generation = generation.load(Ordering::Acquire);
    tauri::async_runtime::spawn(async move {
        while let Some(text) = transcript_rx.recv().await {
            if generation.load(Ordering::Acquire) != spawned_generation {
                // The session was replaced or torn down; a transcript captured
                // under the old configuration must not reach the new one.
                break;
            }
            destination.publish(&publisher, &text).await;
        }
    });
}

// ── Huddle arbitration ───────────────────────────────────────────────────────

/// A huddle is claiming the microphone — suspend the ambient session.
///
/// Called from the huddle start/join paths before media is acquired. Blocking
/// and synchronous so the huddle never races the ambient worker for the device.
pub fn suspend_for_huddle(state: &AppState) {
    let ambient = &state.ambient_voice;
    let running = ambient
        .runtime()
        .map(|runtime| runtime.session.is_some())
        .unwrap_or(false);
    let configured = ambient
        .settings_snapshot()
        .map(|settings| settings.is_runnable())
        .unwrap_or(false);
    if !running && !configured {
        return;
    }
    if let Ok(mut runtime) = ambient.runtime() {
        runtime.suspended_by_huddle = true;
    }
    if running {
        if let Err(error) = stop_session(state, AmbientStatus::Suspended) {
            eprintln!("buzz-desktop: ambient suspend failed: {error}");
        }
    } else {
        ambient.set_status(AmbientStatus::Suspended);
    }
    publish_report(state);
}

/// The huddle is gone — resume the ambient session if it was suspended.
///
/// Called from huddle teardown. Reconciliation is async (it resolves the DM
/// destination and loads models), so it is spawned rather than awaited: huddle
/// teardown must not block on ambient startup.
pub fn resume_after_huddle(state: &AppState) {
    let ambient = &state.ambient_voice;
    let suspended = ambient
        .runtime()
        .map(|runtime| runtime.suspended_by_huddle)
        .unwrap_or(false);
    if !suspended {
        return;
    }
    let Some(app) = app_handle(state) else {
        return;
    };
    tauri::async_runtime::spawn(async move {
        let state = app.state::<AppState>();
        if let Err(error) = reconcile(&state).await {
            eprintln!("buzz-desktop: ambient resume failed: {error}");
        }
    });
}

// ── Boot hydration ───────────────────────────────────────────────────────────

/// Load persisted settings into `AppState` at launch.
///
/// Does **not** start a session. The frontend provider does that, by polling
/// [`check_ambient_hotstart`] once the relay and identity are usable — and only
/// while the preview flag is on, which is what keeps the flag authoritative
/// over a stale `enabled` on disk. Enablement, devices and the wake binding all
/// come from here, which is what makes them survive restart.
pub fn hydrate_at_boot(app: &AppHandle, state: &AppState) {
    let (loaded, error) = settings::load_for_app(app);
    if let Ok(mut guard) = state.ambient_voice.settings.lock() {
        *guard = loaded.clone();
    }
    if let Ok(mut guard) = state.ambient_voice.load_error.lock() {
        *guard = error;
    }
    state
        .ambient_voice
        .muted
        .store(loaded.muted, Ordering::Release);
    if loaded.is_runnable() {
        // Start fetching the wake-word model now so the session can come up
        // without the user waiting for an 18 MB download at first wake.
        models::ensure_kws_download(state.http_client.clone());
    }
}

// ── Commands ─────────────────────────────────────────────────────────────────

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
fn keep_stored_indicator_position(
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

/// Replace the ambient settings and reconcile the runtime.
#[tauri::command]
pub async fn set_ambient_voice_settings(
    settings: AmbientVoiceSettings,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<AmbientVoiceStatusReport, String> {
    let mut next = settings;
    next.version = settings::CURRENT_VERSION;
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

#[cfg(test)]
#[path = "mod_tests.rs"]
mod mod_tests;
