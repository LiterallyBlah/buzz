//! Ambient voice — the `ambientVoice` preview feature.
//!
//! A wake word and one agent. The user says the wake word, speaks, and the
//! utterance arrives in the bound agent's DM as an ordinary kind:9 message;
//! the agent's reply is read back aloud.
//!
//! Every stage runs on this computer by default, and the wake word always
//! does. Recognition and the voice can each be pointed at a server instead —
//! see "Backend choice" below, which is also where what leaves the machine in
//! that case is written down.
//!
//! ## Layout
//!
//! | Module | Responsibility |
//! |---|---|
//! | [`commands`] | the Tauri command surface the frontend calls |
//! | [`settings`] | versioned `ambient-voice-settings.json` |
//! | [`launch`] | what kind of launch this is (update-relaunch diagnostics) |
//! | [`wake_word`] | tokenisation + the strict validation the engine needs |
//! | [`utterance`] | the capture state machine (pure, clock-injected) |
//! | [`session`] | the audio worker: spotter → barge-in → VAD → recogniser |
//! | [`publish`] | egress boundary 9: kind:9 transcripts and kind:48106 |
//! | [`models`] | wake-word model access over the shared download manager |
//! | [`speech_http`] | the wire contract for a role that runs on a server |
//! | [`speech_text`] | Markdown flattened to what a voice should read |
//! | [`speech_wav`] | PCM16 WAV coding for that wire |
//! | [`status`] | what the listening indicator shows |
//! | [`transcriber`] | which recogniser an utterance goes to |
//! | [`tts_backend`] | which pipeline speaks the replies |
//! | [`http_tts`] | that pipeline, when the replies are spoken by a server |
//!
//! ## Lifecycle
//!
//! Nothing runs unless the preview flag is on **and** the persisted settings
//! say enabled **and** a wake binding exists **and** no huddle owns the
//! microphone. Every one of those is a separate gate on purpose: an always-open
//! microphone should be hard to switch on by accident and trivial to switch
//! off.
//!
//! ## Backend choice
//!
//! Each speech role — hearing and speaking — runs either on this computer or
//! on a server the user names (`settings::SpeechBackend`). The wake word, the
//! voice activity detector and the utterance machine are always local: they
//! decide *whether* there is anything to send, and a server that saw the
//! microphone continuously would be a different feature. Only a finished
//! utterance, and only a reply already published to the relay, ever leave.

pub mod commands;
pub mod http_tts;
pub mod launch;
pub mod models;
pub mod publish;
pub mod session;
pub mod settings;
pub mod speech_http;
pub mod speech_text;
pub mod speech_wav;
pub mod status;
pub mod transcriber;
pub mod tts_backend;
pub mod utterance;
pub mod wake_word;

/// A real HTTP server on loopback, so the speech backends are tested against
/// the wire rather than against a mock that shares their assumptions.
#[cfg(test)]
#[path = "speech_stub_server.rs"]
mod speech_stub_server;

use std::{
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

use tauri::{AppHandle, Emitter, Manager};
use uuid::Uuid;

use crate::app_state::AppState;
use crate::huddle::state::HuddlePhase;

use launch::LaunchDiagnostics;
use publish::{AmbientDestination, AmbientPublisher};
use session::{AmbientSession, AmbientSessionConfig, AudioFlowSnapshot};
use settings::AmbientVoiceSettings;
use status::AmbientStatus;
use tts_backend::{start_ambient_tts, AmbientTts};
use wake_word::WakeWordTokenizer;

/// Event name the frontend listens on for every ambient state transition.
pub const STATE_CHANGED_EVENT: &str = "ambient-voice-state-changed";

/// Sample rate of the PCM the AudioWorklet pushes in.
const WORKLET_SAMPLE_RATE: u32 = 48_000;

/// Upper bound on one raw-binary audio batch, mirroring `push_audio_pcm`.
const MAX_AUDIO_BATCH_BYTES: usize = 1024 * 1024;

/// Upper bound on a capture-failure message from the webview. It is shown
/// verbatim on the indicator, which is one short line wide.
const MAX_CAPTURE_ERROR_CHARS: usize = 200;

/// How long a reported capture failure paces the automatic re-arm.
///
/// The microphone lives in the webview, so a device it cannot open fails again
/// the moment a session exists to fail against — and the hot-start poll rebuilds
/// one every three seconds, at two ONNX model loads a rebuild. Thirty seconds is
/// the trade: a device plugged back in recovers on its own within about half a
/// minute rather than three seconds, and one that stays broken costs a single
/// rebuild every thirty seconds rather than ten. Nothing the user does waits on
/// it — settings, mute and the Experiments toggle all call [`reconcile`]
/// directly.
const CAPTURE_ERROR_BACKOFF: Duration = Duration::from_secs(30);

/// How long a capturing session may receive nothing before it is called deaf.
///
/// The shipped bug is a session that reports `capturing` — worker alive — while
/// no audio reaches it at all, so the indicator says "Listening for the wake
/// word" and the wake word is never heard. Five seconds is comfortably longer
/// than the webview needs to open a microphone and build its worklet after a
/// session starts, and short enough that the user reads the truth rather than a
/// lie for the rest of the run.
const STALE_AUDIO_AFTER: Duration = Duration::from_secs(5);

// ── Runtime state ────────────────────────────────────────────────────────────

/// Live session objects. Everything here is torn down together.
#[derive(Default)]
pub struct AmbientRuntime {
    session: Option<AmbientSession>,
    tts: Option<AmbientTts>,
    destination: Option<Arc<AmbientDestination>>,
    /// What [`start_session`] built the live session from, or `None` when no
    /// session is running.
    session_config: Option<SessionConfig>,
    /// When the webview last reported that it could not hold the microphone.
    ///
    /// Deliberately outlives the session it ended — [`stop_session`] does not
    /// clear it — because it describes the device, not the session, and it is
    /// what paces the hot-start retry.
    last_capture_error: Option<Instant>,
    /// What the webview last said about its own half of the audio path.
    ///
    /// Cleared with the session it described: the counters are per capture
    /// pipeline, and a count from the pipeline that fed a session which has
    /// since been torn down would answer the wrong question.
    webview_capture: Option<WebviewCaptureFlow>,
    /// Set when a huddle claimed the microphone; the session resumes on
    /// huddle teardown without the user touching anything.
    suspended_by_huddle: bool,
}

/// What the webview reports about the audio it believes it is sending.
///
/// The two halves of the path are in different processes, and the shipped
/// deafness bug is somewhere between them: this is the webview's own count, so
/// the next occurrence says which link dropped — batches pushed but none
/// received is an IPC or session-lifetime fault, none pushed is the microphone,
/// the worklet or the AudioContext.
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WebviewCaptureFlow {
    /// PCM batches the worklet has handed the main thread to push.
    pub batches_pushed: u64,
    /// Whether the webview currently holds a built capture pipeline. False
    /// while `getUserMedia` or the worklet setup is still in flight — which is
    /// itself the answer, if it never becomes true.
    pub capture_ready: bool,
}

/// The settings a live session was built from.
///
/// The keyword payload, the resolved destination and the TTS pipeline are all
/// bound once, at start. [`reconcile`] otherwise leaves a healthy session
/// alone, so without this record a wake word, agent, destination, microphone
/// or speaker chosen while the session is up would only take effect after the
/// feature was switched off and on again.
///
/// Mute is deliberately absent: [`commands::set_ambient_voice_muted`] applies it
/// to the live worker, and a restart to close a microphone the worker can
/// close itself would drop the destination and reload two ONNX models. So is the
/// indicator position, which no session reads.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SessionConfig {
    /// The one binding M1's runtime arms: wake word, agent and destination.
    binding: Option<settings::WakeBinding>,
    /// Consumed by the AudioWorklet in the webview, which re-acquires the
    /// device from the status report — but a session started against one
    /// microphone must not go on claiming another one's frames.
    input_device_id: Option<String>,
    /// Consumed by `start_ambient_tts` when it builds the pipeline.
    output_device: Option<String>,
    /// Which backend hears the user, and where. The worker builds its
    /// transcriber once, at start, so a role switched to a server — or pointed
    /// at a different one — only reaches the audio path through a restart.
    stt: settings::SpeechBackendSettings,
    /// Which backend speaks the replies. Bound once for the same reason: the
    /// pipeline is built at start, against one endpoint or one local model.
    tts: settings::SpeechBackendSettings,
    /// How long a pause closes an utterance. The capture machine derives both
    /// of its limits from this once, when the worker thread starts.
    silence_hold_ms: u32,
    /// The phrase that ends a capture, armed on the same spotter as the wake
    /// word — so adding, changing or clearing it is a new keyword set, which
    /// only exists at construction.
    stop_phrase: Option<String>,
}

impl SessionConfig {
    fn of(settings: &AmbientVoiceSettings) -> Self {
        Self {
            binding: settings.primary_binding().cloned(),
            input_device_id: settings.input_device_id.clone(),
            output_device: settings.output_device.clone(),
            stt: settings.stt.clone(),
            tts: settings.tts.clone(),
            silence_hold_ms: settings.silence_hold_ms,
            stop_phrase: settings.armed_stop_phrase().map(str::to_string),
        }
    }
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
    /// Whether the last report announced to the frontend said the audio was
    /// stale. The staleness of a running session changes with the clock, not
    /// with a transition, so [`commands::report_ambient_audio_flow`] is what
    /// notices — and it announces the two edges rather than re-emitting the
    /// same report to every window every few seconds.
    stale_announced: AtomicBool,
    /// What kind of launch this is. Recorded once, by [`hydrate_at_boot`].
    launch: Mutex<Option<LaunchDiagnostics>>,
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
            stale_announced: AtomicBool::new(false),
            launch: Mutex::new(None),
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
    /// A session is running and unmuted, and nothing has reached its worker for
    /// [`STALE_AUDIO_AFTER`]. Deliberately separate from `capturing`, which
    /// only ever meant "the worker thread is alive": this is what the indicator
    /// needs to stop claiming to listen while it is deaf.
    pub audio_stale: bool,
    /// Batches the worker has taken off the queue since the session started.
    pub audio_batches_received: u64,
    /// Milliseconds since the last batch reached the worker, or since the
    /// session started when none has. `None` when no session is running.
    pub ms_since_last_audio: Option<u64>,
    /// The webview's own view of the same audio path, as of its last report.
    /// `None` until it sends one.
    pub webview_capture: Option<WebviewCaptureFlow>,
    /// What kind of launch this is. `None` before boot hydration has run.
    pub launch: Option<LaunchDiagnostics>,
}

/// Whether a session is running and being fed nothing.
///
/// Pure, with the snapshot passed in, so every edge is testable without a
/// microphone. Four gates, each load-bearing:
///
/// * `capturing` — with no worker there is nothing for audio to arrive at.
/// * `muted` — a muted session releases the device in the webview by design, so
///   silence is what was asked for, not a fault.
/// * `status.is_live()` — this replaces one true statement with another only
///   where the current one is false. A worker whose engines failed keeps its
///   thread alive draining the queue, and answering "no audio arriving" over
///   "the wake-word model is incomplete" would bury the fault that matters.
///   `Starting` is excluded by the same rule.
/// * the window — the webview needs a moment after a session starts to open the
///   microphone and build its worklet, and calling that deaf would be its own
///   false state.
fn audio_is_stale(
    capturing: bool,
    muted: bool,
    status: &AmbientStatus,
    flow: Option<AudioFlowSnapshot>,
) -> bool {
    capturing
        && !muted
        && status.is_live()
        && flow.is_some_and(|flow| flow.since_last_batch >= STALE_AUDIO_AFTER)
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
    let live_session = runtime
        .session
        .as_ref()
        .filter(|session| !session.is_finished());
    let capturing = live_session.is_some();
    let flow = live_session.map(AmbientSession::audio_flow);
    let webview_capture = runtime.webview_capture;
    let suspended_by_huddle = runtime.suspended_by_huddle;
    drop(runtime);
    let status = ambient.current_status();
    let muted = ambient.muted.load(Ordering::Acquire);
    // Before the struct literal, which moves `status`.
    let audio_stale = audio_is_stale(capturing, muted, &status, flow);
    Ok(AmbientVoiceStatusReport {
        enabled: settings.enabled,
        muted,
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
        audio_stale,
        audio_batches_received: flow.map(|flow| flow.batches).unwrap_or(0),
        ms_since_last_audio: flow.map(|flow| flow.since_last_batch.as_millis() as u64),
        webview_capture,
        launch: ambient
            .launch
            .lock()
            .map(|launch| launch.clone())
            .unwrap_or(None),
    })
}

fn publish_report(state: &AppState) {
    match build_report(state) {
        Ok(report) => {
            // Every emit is an announcement, so every emit is what the edge in
            // `commands::report_ambient_audio_flow` is measured against. Without
            // this the flag drifts from what the windows were actually told —
            // a session that was stale when the user muted it, or when it was
            // stopped, would leave the flag set, and the next deaf session
            // would find no edge to announce and go on saying "listening".
            state
                .ambient_voice
                .stale_announced
                .store(report.audio_stale, Ordering::Release);
            emit_state_changed(app_handle(state).as_ref(), &report);
        }
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

/// Whether a live session has to be rebuilt for `settings` to take effect.
///
/// Split out as a pure predicate for the same reason as [`should_run`]: this
/// is the whole of "a wake word changed in settings applies to the session
/// that is already running", and it is testable without models, a relay or a
/// microphone. An unrecorded configuration cannot be shown to match, so it
/// restarts — the next start records one, so it cannot loop.
fn session_needs_restart(
    started_with: Option<&SessionConfig>,
    settings: &AmbientVoiceSettings,
) -> bool {
    started_with != Some(&SessionConfig::of(settings))
}

/// Whether a reported capture failure is still pacing the hot-start retry.
///
/// Pure, with `now` passed in, so the window is testable without a clock to
/// mock. Only [`commands::check_ambient_hotstart`] consults it: the automatic
/// retry is the only caller that would otherwise rebuild a session every three
/// seconds against a microphone the webview has just said it cannot open.
///
/// Scoped to [`AmbientStatus::Error`] because the timestamp is only ever
/// written by a capture failure that stopped a session; once anything else has
/// moved the runtime out of the error state there is nothing left to pace.
fn capture_failure_is_pacing(
    status: &AmbientStatus,
    last_capture_error: Option<Instant>,
    now: Instant,
) -> bool {
    matches!(status, AmbientStatus::Error(_))
        && last_capture_error
            .and_then(|reported_at| now.checked_duration_since(reported_at))
            .is_some_and(|elapsed| elapsed < CAPTURE_ERROR_BACKOFF)
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
        runtime.session_config = None;
        // Unlike `last_capture_error`, these counters describe the capture
        // pipeline that fed the session that is ending. Carrying them into the
        // next one would answer a question nobody asked.
        runtime.webview_capture = None;
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

    // Already running and healthy — nothing to do, unless what it was built
    // from has changed since. A new wake word, agent, destination or device
    // only reaches the engines through a fresh session.
    let alive = ambient
        .runtime()?
        .session
        .as_ref()
        .is_some_and(|session| !session.is_finished());
    if alive {
        let started_with = ambient.runtime()?.session_config.clone();
        if !session_needs_restart(started_with.as_ref(), &settings) {
            publish_report(state);
            return Ok(());
        }
        stop_session(state, AmbientStatus::Starting)?;
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

    // Strict validation, always, before anything reaches the engine — for the
    // stop phrase exactly as for the wake word, since both are armed on the one
    // spotter and either can kill the process.
    let tokenizer = WakeWordTokenizer::load(&kws_dir)?;
    let stop_phrase = settings
        .armed_stop_phrase()
        // A stop phrase equal to the wake word would arm one keyword twice and
        // leave no answer to which job a detection is doing. Saving one is
        // refused; a file that carries one anyway simply does not arm it.
        .filter(|phrase| {
            wake_word::engine_keyword(phrase) != wake_word::engine_keyword(&binding.wake_word)
        })
        .map(str::to_string);
    let mut phrases = vec![binding.wake_word.clone()];
    phrases.extend(stop_phrase.clone());
    let keywords_buf = tokenizer
        .keywords_buf(&phrases)
        .map_err(|(phrase, error)| format!("the phrase \"{phrase}\" cannot be used: {error}"))?;

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
        stt_endpoint: settings.stt.http_base_url().map(str::to_string),
        keywords_buf,
        stop_keyword: stop_phrase.as_deref().map(wake_word::engine_keyword),
        stop_phrase,
        silence_hold_ms: settings.silence_hold_ms,
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
        runtime.session_config = Some(SessionConfig::of(settings));
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

/// Load persisted settings into `AppState` at launch, and record what kind of
/// launch this is.
///
/// Does **not** start a session. The frontend provider does that, by polling
/// [`commands::check_ambient_hotstart`] once the relay and identity are usable
/// — and only while the preview flag is on, which is what keeps the flag
/// authoritative over a stale `enabled` on disk. Enablement, devices and the
/// wake binding all come from here, which is what makes them survive restart.
pub fn hydrate_at_boot(app: &AppHandle, state: &AppState) {
    // Before anything else can fail: the breadcrumb this leaves behind is what
    // the *next* launch compares itself against, and both deafness reports were
    // a first launch after an update.
    if let Ok(mut guard) = state.ambient_voice.launch.lock() {
        *guard = Some(launch::detect(app));
    }
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

#[cfg(test)]
#[path = "mod_tests.rs"]
mod mod_tests;
