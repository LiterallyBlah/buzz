//! The ambient audio worker: keyword spotting → barge-in → utterance → text.
//!
//! ```text
//! AudioWorklet (48 kHz f32 PCM, webview)
//!   → push_ambient_audio_pcm (Tauri cmd)
//!   → AmbientSession::push_audio  [bounded sync_channel]
//!   → ambient_worker thread
//!       rubato: 48 kHz → 16 kHz mono
//!       ├─ sherpa-onnx KeywordSpotter   (ALWAYS fed, including during TTS)
//!       │     on fire → cancel TTS (barge-in) → arm the utterance machine
//!       └─ earshot VAD → UtteranceMachine (gated while TTS plays)
//!             on decode → sherpa-onnx Parakeet → transcript
//!   → transcript_tx  [tokio mpsc]
//!   → publisher task → kind:9 to the bound agent's destination
//! ```
//!
//! One resample feeds both consumers. The model wants 16 kHz/80-dim fbank and
//! `feat_config.sample_rate` therefore stays at the model's rate — it is a
//! description of the features, not of the input, and setting it to the input
//! rate silently destroys detection.
//!
//! The worker runs on a dedicated `std::thread` because sherpa-onnx is
//! CPU-bound and not `Send`-safe across await points, exactly as
//! `huddle::stt` does.

use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, Receiver, SyncSender},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

use tokio::sync::mpsc as tokio_mpsc;

use super::status::AmbientStatus;
use super::transcriber::Transcriber;
use super::utterance::{FrameOutcome, UtteranceMachine, VAD_FRAME_SAMPLES};

/// Bounded audio queue capacity — same shape and reasoning as `huddle::stt`:
/// 100 ms batches at 48 kHz ≈ 19 KB each, so 50 slots ≈ 5 s / ~1 MB.
const AUDIO_QUEUE_DEPTH: usize = 50;

/// How long the worker waits on the audio channel before re-checking shutdown.
const RECV_TIMEOUT: Duration = Duration::from_millis(50);

/// VAD probability above which a frame counts as speech.
const VAD_THRESHOLD: f32 = 0.5;

/// ONNX intra-op threads for both models.
///
/// One thread each. The M0 spike measured the spotter at ~1.1% of one core
/// fp32 / ~0.95% int8 at one thread, and **two threads strictly worse**; the
/// recognizer follows `huddle::stt`'s conservative default for the same
/// oversubscription reason.
pub(crate) const NUM_THREADS: i32 = 1;

/// Decoder beam width.
///
/// sherpa's default is 4, which the M0 spike proved **drops true detections**
/// once more than four keywords are armed. M1 arms one, but the cost of a
/// wider beam was measured at +0.05 percentage points of a core, so there is
/// no reason to ship the footgun for M2 to trip over.
const MAX_ACTIVE_PATHS: i32 = 16;

/// Blank frames required after a keyword before it is emitted.
///
/// This — not `keywords_threshold` — is the discriminative knob: the M0 sweep
/// found the threshold flat from 0.005 to 0.6 and a hard cliff at ≥0.9, while
/// trailing blanks grade smoothly over 2–4. Two is the low end of the useful
/// range: a wake word is naturally followed by a short pause, but users run
/// straight on ("hey hermes what's the weather") often enough that demanding a
/// longer silence would cost real detections.
const NUM_TRAILING_BLANKS: i32 = 2;

// ── Public handle ────────────────────────────────────────────────────────────

/// Called on every status transition the audio worker makes.
///
/// The indicator is event-driven — it never polls — so a transition the worker
/// keeps to itself is a pill frozen on whatever the last lifecycle change
/// (on/off, mute, huddle) reported. [`super::start_session`] binds this to the
/// same `STATE_CHANGED_EVENT` emit every lifecycle change already uses, so the
/// engine's states reach the frontend over the one existing channel.
pub type AmbientStatusNotifier = Arc<dyn Fn(&AmbientStatus) + Send + Sync>;

/// The worker's handle on the shared status cell plus the frontend notifier.
///
/// Two properties are load-bearing:
///
/// * **Only transitions are announced.** The worker re-asserts the same status
///   on most VAD frames; an event per 32 ms frame would be an IPC flood for a
///   pill that does not change.
/// * **The status lock is released before the notifier runs.** The notifier
///   rebuilds the whole status report, which takes the runtime lock and then
///   reads this same cell — announcing while holding it would deadlock the
///   audio thread against the command thread.
pub(crate) struct StatusSink {
    status: Arc<Mutex<AmbientStatus>>,
    notify: Option<AmbientStatusNotifier>,
}

impl StatusSink {
    pub(crate) fn new(
        status: Arc<Mutex<AmbientStatus>>,
        notify: Option<AmbientStatusNotifier>,
    ) -> Self {
        Self { status, notify }
    }

    /// Record `next`, and tell the frontend when it is a change.
    pub(crate) fn set(&self, next: AmbientStatus) {
        {
            let mut current = self.status.lock().unwrap_or_else(|e| e.into_inner());
            if *current == next {
                return;
            }
            *current = next.clone();
        }
        if let Some(notify) = self.notify.as_ref() {
            notify(&next);
        }
    }
}

// ── Audio arrival ────────────────────────────────────────────────────────────

/// When audio last reached the worker, and how much of it has.
///
/// `capturing` in the status report only ever meant "the worker thread is
/// alive" — it says nothing about frames arriving, and the shipped deafness bug
/// is exactly that gap: a pill reading "Listening for the wake word" while the
/// spotter is fed nothing at all. This is the missing half. The worker stamps
/// every batch it takes off the queue, so [`super::build_report`] can tell a
/// live session from a deaf one.
///
/// Written from the audio thread and read from command threads, so it is
/// atomics rather than a mutex: a status report must never be able to block the
/// worker, and a millisecond of skew between the two fields cannot matter to a
/// five-second staleness window.
#[derive(Debug)]
pub struct AudioFlow {
    started_at: Instant,
    batches: AtomicU64,
    /// Milliseconds from `started_at` to the last batch. Zero until one
    /// arrives, which is why `batches` is what distinguishes "none yet" from
    /// "one arrived in the first millisecond".
    last_batch_ms: AtomicU64,
}

/// One read of [`AudioFlow`], for one status report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioFlowSnapshot {
    /// Batches the worker has taken off the queue since the session started.
    pub batches: u64,
    /// Since the last batch reached the worker — or since the session started,
    /// when none ever has. The session start is the right zero point: a session
    /// that has never been fed is precisely the state being watched for.
    pub since_last_batch: Duration,
}

impl AudioFlow {
    fn new() -> Self {
        Self {
            started_at: Instant::now(),
            batches: AtomicU64::new(0),
            last_batch_ms: AtomicU64::new(0),
        }
    }

    /// Record that `batches` arrived from the webview, now.
    fn record(&self, batches: u64) {
        self.batches.fetch_add(batches, Ordering::Release);
        self.last_batch_ms.store(
            self.started_at.elapsed().as_millis() as u64,
            Ordering::Release,
        );
    }

    pub fn snapshot(&self) -> AudioFlowSnapshot {
        let elapsed = self.started_at.elapsed().as_millis() as u64;
        let last_batch_ms = self.last_batch_ms.load(Ordering::Acquire);
        AudioFlowSnapshot {
            batches: self.batches.load(Ordering::Acquire),
            since_last_batch: Duration::from_millis(elapsed.saturating_sub(last_batch_ms)),
        }
    }
}

/// A running ambient session.
///
/// Not `Clone` — the owner is `AmbientVoiceRuntime` in `AppState`. Dropping it
/// signals the worker and joins the thread, so tearing the session down cannot
/// leave a microphone consumer running.
pub struct AmbientSession {
    audio_tx: SyncSender<Vec<u8>>,
    shutdown: Arc<AtomicBool>,
    muted: Arc<AtomicBool>,
    status: Arc<Mutex<AmbientStatus>>,
    flow: Arc<AudioFlow>,
    thread: Option<thread::JoinHandle<()>>,
}

/// Everything the worker needs, resolved before the thread is spawned so no
/// `AppState` lock is ever taken from the audio thread.
pub struct AmbientSessionConfig {
    pub kws_model_dir: PathBuf,
    pub stt_model_dir: PathBuf,
    /// Base URL of the speech server this session transcribes through, or
    /// `None` to use the model in `stt_model_dir`. Resolved from settings
    /// before the thread starts, like everything else here — the worker takes
    /// no locks.
    pub stt_endpoint: Option<String>,
    /// Pre-validated, pre-tokenised keyword payload. **Must** come from
    /// [`super::wake_word::WakeWordTokenizer::keywords_buf`] — raw text here
    /// kills the process.
    pub keywords_buf: String,
    /// Shared with the ambient TTS pipeline: true while it is playing.
    pub tts_active: Arc<AtomicBool>,
    /// Shared with the ambient TTS pipeline: set to cancel playback.
    pub tts_cancel: Arc<AtomicBool>,
    pub muted: Arc<AtomicBool>,
    pub status: Arc<Mutex<AmbientStatus>>,
    /// Announces every worker status transition to the frontend. `None` in
    /// tests and when the app handle is not available yet; the session still
    /// runs, the indicator just does not live-update.
    pub on_status_change: Option<AmbientStatusNotifier>,
    /// Sample rate of the PCM pushed in. 48 kHz from the AudioWorklet.
    pub input_sample_rate: u32,
}

impl AmbientSession {
    /// Spawn the worker.
    ///
    /// Returns the transcript receiver separately so the caller can move it
    /// straight into an async task without holding a mutex across an await.
    pub fn new(
        config: AmbientSessionConfig,
    ) -> Result<(Self, tokio_mpsc::Receiver<String>), String> {
        let (audio_tx, audio_rx) = mpsc::sync_channel::<Vec<u8>>(AUDIO_QUEUE_DEPTH);
        let (transcript_tx, transcript_rx) = tokio_mpsc::channel::<String>(16);
        let shutdown = Arc::new(AtomicBool::new(false));

        let muted = Arc::clone(&config.muted);
        let status = Arc::clone(&config.status);
        let shutdown_worker = Arc::clone(&shutdown);
        // Built here rather than taken from the caller: the clock starts when
        // the worker does, and "nothing has arrived since the session started"
        // is only meaningful against that instant.
        let flow = Arc::new(AudioFlow::new());
        let flow_worker = Arc::clone(&flow);

        let handle = thread::Builder::new()
            .name("ambient-voice-worker".into())
            .spawn(move || {
                ambient_worker(
                    config,
                    audio_rx,
                    transcript_tx,
                    shutdown_worker,
                    flow_worker,
                )
            })
            .map_err(|e| format!("failed to spawn ambient-voice-worker thread: {e}"))?;

        Ok((
            Self {
                audio_tx,
                shutdown,
                muted,
                status,
                flow,
                thread: Some(handle),
            },
            transcript_rx,
        ))
    }

    /// Feed raw PCM (f32 LE, `input_sample_rate`, mono).
    ///
    /// Non-blocking: audio is dropped rather than allowed to stall the webview.
    pub fn push_audio(&self, pcm_bytes: Vec<u8>) -> Result<(), String> {
        if !pcm_bytes.len().is_multiple_of(4) {
            return Err(format!(
                "audio input not 4-byte aligned ({} bytes) — expected f32 LE samples",
                pcm_bytes.len()
            ));
        }
        let _ = self.audio_tx.try_send(pcm_bytes);
        Ok(())
    }

    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Release);
    }

    pub fn is_finished(&self) -> bool {
        self.thread.as_ref().is_none_or(|h| h.is_finished())
    }

    /// Apply mute to the live worker.
    ///
    /// Deliberately writes the status cell directly rather than through a
    /// [`StatusSink`]: the caller (`set_ambient_voice_muted`) holds the runtime
    /// lock here and publishes its own report immediately afterwards, so
    /// announcing from inside would re-enter that lock for an event the
    /// command is about to send anyway.
    pub fn set_muted(&self, muted: bool) {
        self.muted.store(muted, Ordering::Release);
        let mut status = self.status.lock().unwrap_or_else(|e| e.into_inner());
        *status = if muted {
            AmbientStatus::Muted
        } else {
            AmbientStatus::Listening
        };
    }

    pub fn status(&self) -> AmbientStatus {
        self.status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// What the worker has been fed since it started.
    pub fn audio_flow(&self) -> AudioFlowSnapshot {
        self.flow.snapshot()
    }
}

impl Drop for AmbientSession {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

// ── Engine construction ──────────────────────────────────────────────────────

/// Build the keyword spotter for a model directory and a validated payload.
///
/// `keywords_buf` MUST already have been produced by the tokenizer. The C
/// library terminates the process on un-encodable input, so this function
/// exists partly to give that requirement a single documented door.
pub(crate) fn create_keyword_spotter(
    model_dir: &Path,
    keywords_buf: &str,
) -> Result<sherpa_onnx::KeywordSpotter, String> {
    use sherpa_onnx::KeywordSpotterConfig;

    // Payload first: an empty string is a caller-contract violation, and it is
    // checked before anything model-shaped so the error names the real fault.
    // "" is NOT the same as "\n" — the latter is the proven-safe "no keywords"
    // payload, the former means a caller skipped the tokenizer.
    if keywords_buf.is_empty() {
        return Err("refusing to arm the keyword spotter with an empty payload".to_string());
    }
    for file in super::models::KWS_REQUIRED_FILES {
        if !model_dir.join(file).is_file() {
            return Err(format!(
                "wake-word model is incomplete: {} is missing from {}",
                file,
                model_dir.display()
            ));
        }
    }

    let mut config = KeywordSpotterConfig::default();
    config.model_config.transducer.encoder = Some(path_str(model_dir, super::models::KWS_ENCODER));
    config.model_config.transducer.decoder = Some(path_str(model_dir, super::models::KWS_DECODER));
    config.model_config.transducer.joiner = Some(path_str(model_dir, super::models::KWS_JOINER));
    config.model_config.tokens = Some(path_str(model_dir, super::models::KWS_TOKENS));
    config.model_config.provider = Some("cpu".to_string());
    config.model_config.num_threads = NUM_THREADS;
    config.model_config.debug = false;
    config.max_active_paths = MAX_ACTIVE_PATHS;
    config.num_trailing_blanks = NUM_TRAILING_BLANKS;
    // feat_config stays at the model's 16 kHz / 80-dim default on purpose.
    //
    // Keywords go in via the config rather than `create_stream_with_keywords`
    // because a stream built from keywords the model cannot encode is returned
    // as a null pointer wrapped in a plain `OnlineStream`, which segfaults on
    // first use with nothing observable from Rust. The config path at least
    // fails at construction. Both are only safe because the payload was
    // validated; M2's runtime keyword reload must validate identically.
    config.keywords_buf = Some(keywords_buf.to_string());

    sherpa_onnx::KeywordSpotter::create(&config)
        .ok_or_else(|| "could not create the wake-word spotter".to_string())
}

fn path_str(dir: &Path, file: &str) -> String {
    dir.join(file).to_string_lossy().into_owned()
}

/// Decode everything the stream is ready for and return the keywords that
/// fired, resetting the spotter after each so the next detection starts from a
/// clean beam.
///
/// Shared with `session_tests` so the fixture-driven tests exercise the
/// production spotting loop rather than a copy of it.
pub(crate) fn drain_detections(
    spotter: &sherpa_onnx::KeywordSpotter,
    stream: &sherpa_onnx::OnlineStream,
) -> Vec<String> {
    let mut fired = Vec::new();
    while spotter.is_ready(stream) {
        spotter.decode(stream);
        let Some(result) = spotter.get_result(stream) else {
            continue;
        };
        if result.keyword.trim().is_empty() {
            continue;
        }
        spotter.reset(stream);
        fired.push(result.keyword);
    }
    fired
}

// ── Worker ───────────────────────────────────────────────────────────────────

fn ambient_worker(
    config: AmbientSessionConfig,
    audio_rx: Receiver<Vec<u8>>,
    transcript_tx: tokio_mpsc::Sender<String>,
    shutdown: Arc<AtomicBool>,
    flow: Arc<AudioFlow>,
) {
    let AmbientSessionConfig {
        kws_model_dir,
        stt_model_dir,
        stt_endpoint,
        keywords_buf,
        tts_active,
        tts_cancel,
        muted,
        status,
        on_status_change,
        input_sample_rate,
    } = config;

    // Every `status` write in this thread goes through the sink, which is what
    // makes the indicator follow the session rather than the settings.
    let status = StatusSink::new(status, on_status_change);

    let spotter = match create_keyword_spotter(&kws_model_dir, &keywords_buf) {
        Ok(spotter) => spotter,
        Err(error) => {
            eprintln!("buzz-desktop: ambient wake-word engine unavailable: {error}");
            status.set(AmbientStatus::Error(error));
            drain_until_shutdown(audio_rx, &shutdown, &flow);
            return;
        }
    };
    let transcriber = match Transcriber::build(&stt_model_dir, stt_endpoint.as_deref()) {
        Ok(transcriber) => transcriber,
        Err(error) => {
            eprintln!("buzz-desktop: ambient speech recognizer unavailable: {error}");
            status.set(AmbientStatus::Error(error));
            drain_until_shutdown(audio_rx, &shutdown, &flow);
            return;
        }
    };

    let mut resampler = match Resampler::new(input_sample_rate) {
        Ok(resampler) => resampler,
        Err(error) => {
            eprintln!("buzz-desktop: ambient resampler init failed: {error}");
            status.set(AmbientStatus::Error(error));
            drain_until_shutdown(audio_rx, &shutdown, &flow);
            return;
        }
    };

    use earshot::{DefaultPredictor, Detector};
    let mut vad = Detector::new(DefaultPredictor::new());

    let stream = spotter.create_stream();
    let mut machine = UtteranceMachine::new();
    let mut leftover_16k: Vec<f32> = Vec::new();
    let mut speech_buf: Vec<f32> = Vec::new();

    status.set(if muted.load(Ordering::Acquire) {
        AmbientStatus::Muted
    } else {
        AmbientStatus::Listening
    });

    loop {
        if shutdown.load(Ordering::Acquire) {
            break;
        }
        let bytes = match audio_rx.recv_timeout(RECV_TIMEOUT) {
            Ok(bytes) => bytes,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        let mut batch = vec![bytes];
        while let Ok(more) = audio_rx.try_recv() {
            batch.push(more);
        }
        // Stamped before the mute check, and before anything can drop a batch:
        // this records that the webview is feeding us, not what we did with the
        // audio. Everything below may legitimately discard it.
        flow.record(batch.len() as u64);

        // Mute is a hard stop: nothing is spotted, nothing is captured, and a
        // half-captured utterance is abandoned rather than resumed later.
        if muted.load(Ordering::Acquire) {
            if machine.phase() != super::utterance::UtterancePhase::Idle {
                machine.reset();
                speech_buf.clear();
            }
            leftover_16k.clear();
            continue;
        }

        for bytes in batch {
            for chunk in resampler.push(&bytes_to_f32(&bytes)) {
                // ── Stage 1: keyword spotting, ALWAYS armed ──────────────
                //
                // Deliberately not gated on `tts_active`. Hearing the wake
                // word while the agent is speaking is exactly what barge-in
                // is, so the spotter must stay live through playback; it is
                // the capture stage below that must not hear us.
                stream.accept_waveform(16_000, &chunk);
                for keyword in drain_detections(&spotter, &stream) {
                    // Result timestamps restart at every internal reset and
                    // `start_time` is always 0.00, so the only trustworthy
                    // clock is ours.
                    let now = Instant::now();
                    eprintln!("buzz-desktop: ambient wake word fired ({keyword})");

                    // Barge-in: stop whatever the agent is saying before the
                    // user's next word arrives.
                    tts_cancel.store(true, Ordering::Release);
                    if machine.on_wake(now) {
                        speech_buf.clear();
                    }
                    status.set(AmbientStatus::Heard);
                }

                // ── Stage 2: utterance capture, gated during playback ────
                leftover_16k.extend_from_slice(&chunk);
                while leftover_16k.len() >= VAD_FRAME_SAMPLES {
                    let frame: Vec<f32> = leftover_16k.drain(..VAD_FRAME_SAMPLES).collect();
                    let clamped: Vec<f32> =
                        frame.iter().map(|sample| sample.clamp(-1.0, 1.0)).collect();
                    let is_speech = vad.predict_f32(&clamped) > VAD_THRESHOLD;
                    let playing = tts_active.load(Ordering::Acquire);

                    match machine.on_frame(is_speech, playing, Instant::now()) {
                        FrameOutcome::Idle => {}
                        FrameOutcome::Buffer => {
                            if speech_buf.is_empty() {
                                status.set(AmbientStatus::Capturing);
                            }
                            speech_buf.extend_from_slice(&frame);
                        }
                        FrameOutcome::Drop => {
                            speech_buf.clear();
                            status.set(current_idle_status(&machine, playing));
                        }
                        FrameOutcome::Decode => {
                            speech_buf.extend_from_slice(&frame);
                            status.set(AmbientStatus::Transcribing);
                            let outcome = transcribe(&transcriber, &speech_buf, &transcript_tx);
                            speech_buf.clear();
                            status.set(status_after_decode(outcome));
                        }
                    }
                }
            }
        }
    }

    status.set(AmbientStatus::Off);
}

/// What the indicator shows once an utterance has been decoded.
///
/// A failure stays there until the next transition rather than flashing past.
/// The user has just spoken and heard nothing back, and going straight back to
/// "listening for the wake word" would be the same class of lie the audio
/// watchdog was built to end: a pill claiming to work while the thing it
/// describes is broken. The next wake word replaces it, so nothing has to
/// clear it.
fn status_after_decode(outcome: Result<(), String>) -> AmbientStatus {
    match outcome {
        Ok(()) => AmbientStatus::Listening,
        Err(error) => AmbientStatus::Error(error),
    }
}

fn current_idle_status(machine: &UtteranceMachine, tts_playing: bool) -> AmbientStatus {
    use super::utterance::UtterancePhase;
    match machine.phase() {
        UtterancePhase::Idle => AmbientStatus::Listening,
        _ if tts_playing => AmbientStatus::Speaking,
        _ => AmbientStatus::Heard,
    }
}

/// Turn the captured utterance into a published transcript.
///
/// The error is the transcriber's own — already logged there, and returned
/// here so the caller can leave it on the indicator. Audio that carried no
/// words is `Ok`: an utterance the recogniser found nothing in is an ordinary
/// outcome, not a fault to report.
fn transcribe(
    transcriber: &Transcriber,
    speech_buf: &[f32],
    transcript_tx: &tokio_mpsc::Sender<String>,
) -> Result<(), String> {
    if speech_buf.is_empty() {
        return Ok(());
    }
    let text = transcriber.transcribe(speech_buf)?;
    if text.is_empty() {
        return Ok(());
    }
    if let Err(error) = transcript_tx.blocking_send(text) {
        eprintln!("buzz-desktop: ambient transcript channel closed: {error}");
    }
    Ok(())
}

/// Keep draining until shutdown so a dead worker cannot back-pressure the
/// webview. Mirrors `huddle::drain_until_shutdown`.
///
/// Arrivals are stamped here too. This runs when the engines could not be built,
/// and "the webview is feeding us, the engine is what died" is a materially
/// different bug report from "nothing is arriving at all" — the status already
/// carries the engine failure, so the counter is free to describe the audio.
fn drain_until_shutdown(audio_rx: Receiver<Vec<u8>>, shutdown: &Arc<AtomicBool>, flow: &AudioFlow) {
    while !shutdown.load(Ordering::Acquire) {
        match audio_rx.recv_timeout(RECV_TIMEOUT) {
            Ok(_) => flow.record(1),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
}

/// 48 kHz → 16 kHz, or a pass-through when the input already is 16 kHz.
///
/// The pass-through exists so fixture-driven tests can push a 16 kHz WAV in
/// without the resampler standing between them and the engines.
struct Resampler {
    inner: Option<rubato::Fft<f32>>,
    chunk_in: usize,
    pending: Vec<f32>,
}

impl Resampler {
    fn new(input_sample_rate: u32) -> Result<Self, String> {
        use rubato::{Fft, FixedSync, Resampler as _};
        if input_sample_rate == 16_000 {
            return Ok(Self {
                inner: None,
                chunk_in: VAD_FRAME_SAMPLES,
                pending: Vec::new(),
            });
        }
        let inner = Fft::<f32>::new(
            input_sample_rate as usize,
            16_000,
            1024,
            2,
            1,
            FixedSync::Input,
        )
        .map_err(|e| format!("resampler init: {e}"))?;
        let chunk_in = inner.input_frames_next();
        Ok(Self {
            inner: Some(inner),
            chunk_in,
            pending: Vec::with_capacity(chunk_in * 2),
        })
    }

    /// Accept input samples and yield whole 16 kHz chunks.
    fn push(&mut self, samples: &[f32]) -> Vec<Vec<f32>> {
        self.pending.extend_from_slice(samples);
        let mut out = Vec::new();
        while self.pending.len() >= self.chunk_in {
            let chunk: Vec<f32> = self.pending.drain(..self.chunk_in).collect();
            match self.inner.as_mut() {
                None => out.push(chunk),
                Some(resampler) => {
                    let resampled = resample_chunk(resampler, &chunk);
                    if !resampled.is_empty() {
                        out.push(resampled);
                    }
                }
            }
        }
        out
    }
}

fn resample_chunk(resampler: &mut rubato::Fft<f32>, chunk: &[f32]) -> Vec<f32> {
    use audioadapter_buffers::direct::InterleavedSlice;
    use rubato::Resampler as _;

    let input = match InterleavedSlice::new(chunk, 1, chunk.len()) {
        Ok(adapter) => adapter,
        Err(error) => {
            eprintln!("buzz-desktop: ambient resample input error: {error}");
            return Vec::new();
        }
    };
    match resampler.process(&input, 0, None) {
        Ok(out) => out.take_data(),
        Err(error) => {
            eprintln!("buzz-desktop: ambient resample error: {error}");
            Vec::new()
        }
    }
}

/// f32 little-endian bytes → samples. LE matches every Tauri target and the
/// AudioWorklet's native `Float32Array` layout.
fn bytes_to_f32(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect()
}

#[cfg(test)]
#[path = "session_tests.rs"]
mod session_tests;
