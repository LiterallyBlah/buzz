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
//!             on decode → Transcriber → transcript
//!                         (sherpa-onnx Parakeet here, or a speech server)
//!   → transcript_tx  [tokio mpsc]
//!   → publisher task → kind:9 to the bound agent's destination
//! ```
//!
//! Everything above the decode step is local whatever the settings say: only
//! the finished utterance is swappable ([`super::transcriber`]).
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
use super::utterance::{FrameOutcome, UtteranceMachine, UtteranceTiming, VAD_FRAME_SAMPLES};

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
/// worker, and a millisecond of skew between the fields cannot matter to a
/// five-second staleness window. Every write is made by the worker thread
/// alone, so the fields cannot interleave with each other.
///
/// ## Why transcription time is subtracted
///
/// The worker is a single loop: while it is decoding an utterance it is not
/// draining its audio queue, and with speech-to-text pointed at a server that
/// decode is a network round trip which runs to the budget
/// `super::speech_http` gives it — ten seconds for a short utterance and longer
/// for a long one — before the local fallback even starts. Measured naively, an
/// ordinary utterance through a slow server therefore reads as five seconds of
/// nothing arriving — so the pill claimed "No audio arriving from the
/// microphone" and the webview's watchdog rebuilt the whole capture pipeline,
/// once per utterance, against a microphone that was working perfectly.
///
/// What the watchdog needs to know is whether the worker is *starved*, not
/// whether time has passed, so the time the worker spent doing its own work is
/// excluded. The audio the webview pushed meanwhile is not lost: it waits in
/// the bounded queue and is stamped when the worker takes it.
#[derive(Debug)]
pub struct AudioFlow {
    started_at: Instant,
    batches: AtomicU64,
    /// Milliseconds from `started_at` to the last batch. Zero until one
    /// arrives, which is why `batches` is what distinguishes "none yet" from
    /// "one arrived in the first millisecond".
    last_batch_ms: AtomicU64,
    /// Milliseconds the worker has spent inside finished transcriptions since
    /// the last batch arrived. Reset by [`AudioFlow::record`]: a batch that has
    /// arrived settles the starvation question on its own.
    busy_since_last_batch_ms: AtomicU64,
    /// When the transcription now in flight started, as milliseconds from
    /// `started_at` **plus one**; `0` means none is. The offset is what keeps a
    /// transcription that began in the session's first millisecond from reading
    /// as "not transcribing", in one atomic rather than two that could be read
    /// out of step.
    transcribing_since_ms: AtomicU64,
}

/// One read of [`AudioFlow`], for one status report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioFlowSnapshot {
    /// Batches the worker has taken off the queue since the session started.
    pub batches: u64,
    /// How long the worker has been free to receive audio and received none —
    /// since the last batch, or since the session started when none ever
    /// arrived. The session start is the right zero point: a session that has
    /// never been fed is precisely the state being watched for.
    ///
    /// Time the worker spent transcribing is not counted, because during it the
    /// worker was not listening to the queue at all. See [`AudioFlow`].
    pub since_last_batch: Duration,
}

impl AudioFlow {
    fn new() -> Self {
        Self {
            started_at: Instant::now(),
            batches: AtomicU64::new(0),
            last_batch_ms: AtomicU64::new(0),
            busy_since_last_batch_ms: AtomicU64::new(0),
            transcribing_since_ms: AtomicU64::new(0),
        }
    }

    fn elapsed_ms(&self) -> u64 {
        self.started_at.elapsed().as_millis() as u64
    }

    /// Record that `batches` arrived from the webview, now.
    fn record(&self, batches: u64) {
        self.batches.fetch_add(batches, Ordering::Release);
        self.busy_since_last_batch_ms.store(0, Ordering::Release);
        self.last_batch_ms
            .store(self.elapsed_ms(), Ordering::Release);
    }

    /// Mark the worker busy until the returned guard is dropped.
    ///
    /// A guard rather than a matching call because the mark is what switches
    /// the staleness watchdog off: an exit that skipped the closing call —
    /// an unwind out of the recogniser, a `?` added to the block later —
    /// would leave `transcribing_since_ms` set for the life of the session,
    /// and `starved_ms` would answer "not starved" forever. The watchdog
    /// would be off with nothing to show that it was.
    fn transcribing(&self) -> Transcribing<'_> {
        self.begin_transcription();
        Transcribing { flow: self }
    }

    /// The worker is about to block on turning an utterance into text.
    fn begin_transcription(&self) {
        self.transcribing_since_ms
            .store(self.elapsed_ms() + 1, Ordering::Release);
    }

    /// It has finished, however it went.
    fn end_transcription(&self) {
        let started = self.transcribing_since_ms.swap(0, Ordering::AcqRel);
        if started == 0 {
            return;
        }
        let spent = self.elapsed_ms().saturating_sub(started - 1);
        self.busy_since_last_batch_ms
            .fetch_add(spent, Ordering::Release);
    }

    pub fn snapshot(&self) -> AudioFlowSnapshot {
        AudioFlowSnapshot {
            batches: self.batches.load(Ordering::Acquire),
            since_last_batch: Duration::from_millis(starved_ms(
                self.elapsed_ms(),
                self.last_batch_ms.load(Ordering::Acquire),
                self.busy_since_last_batch_ms.load(Ordering::Acquire),
                self.transcribing_since_ms.load(Ordering::Acquire),
            )),
        }
    }
}

/// Held for as long as the worker is inside a transcription.
///
/// Its whole job is the `Drop`: however the transcription ends, the mark comes
/// off with it.
struct Transcribing<'a> {
    flow: &'a AudioFlow,
}

impl Drop for Transcribing<'_> {
    fn drop(&mut self) {
        self.flow.end_transcription();
    }
}

/// How long the worker has gone without audio while it was free to receive it.
///
/// Pure, with the clock passed in, so every edge is testable without sleeping:
/// nothing transcribed, a transcription still in flight, one that has finished,
/// and the impossible orderings a torn read could produce. Saturating
/// throughout — the answer is a duration, and no combination of reads may
/// produce a longer one than the session has existed for.
fn starved_ms(
    now_ms: u64,
    last_batch_ms: u64,
    busy_since_last_batch_ms: u64,
    transcribing_since_ms: u64,
) -> u64 {
    let in_flight = match transcribing_since_ms {
        0 => 0,
        started => now_ms.saturating_sub(started - 1),
    };
    now_ms
        .saturating_sub(last_batch_ms)
        .saturating_sub(busy_since_last_batch_ms)
        .saturating_sub(in_flight)
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
    /// kills the process. Carries the wake word and, when one is configured,
    /// the stop phrase: both are armed on this one spotter.
    pub keywords_buf: String,
    /// Which of the armed keywords ends a capture rather than starting one, in
    /// the form the engine reports (see
    /// [`super::wake_word::engine_keyword`]). `None` when no stop phrase is
    /// configured, in which case every detection is a wake word.
    pub stop_keyword: Option<String>,
    /// The phrase as the user typed it, for trimming it back out of the
    /// transcript it inevitably ends up in.
    pub stop_phrase: Option<String>,
    /// How long a pause closes an utterance, in milliseconds.
    pub silence_hold_ms: u32,
    /// Where the speech server's answers are recorded when this session runs
    /// speech-to-text on one, so a server that is failing softly is visible
    /// rather than only on stderr.
    pub stt_health: Arc<super::speech_health::RoleHealth>,
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
        stop_keyword,
        stop_phrase,
        silence_hold_ms,
        stt_health,
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
    let transcriber = match Transcriber::build(&stt_model_dir, stt_endpoint.as_deref(), stt_health)
    {
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
    let mut machine = UtteranceMachine::new(UtteranceTiming::from_silence_hold_ms(silence_hold_ms));
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
                    if is_stop_keyword(stop_keyword.as_deref(), &keyword) {
                        // Deliberately none of the wake-word side effects: no
                        // barge-in, no arming, no `Heard`. Outside a capture
                        // the machine answers `Idle` and this is a no-op.
                        eprintln!("buzz-desktop: ambient stop phrase fired ({keyword})");
                        match machine.on_stop_phrase() {
                            FrameOutcome::Decode => finish_capture(
                                &transcriber,
                                &mut speech_buf,
                                &transcript_tx,
                                &status,
                                stop_phrase.as_deref(),
                                &flow,
                            ),
                            FrameOutcome::Drop => {
                                speech_buf.clear();
                                status.set(current_idle_status(
                                    &machine,
                                    tts_active.load(Ordering::Acquire),
                                ));
                            }
                            FrameOutcome::Idle | FrameOutcome::Buffer => {}
                        }
                        continue;
                    }
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
                            finish_capture(
                                &transcriber,
                                &mut speech_buf,
                                &transcript_tx,
                                &status,
                                None,
                                &flow,
                            );
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

/// Whether a detection is the configured stop phrase rather than the wake word.
///
/// The engine reports the keyword in its own uppercase, space-joined form, so
/// the comparison is against [`super::wake_word::engine_keyword`]'s output and
/// not against what the user typed.
fn is_stop_keyword(stop_keyword: Option<&str>, fired: &str) -> bool {
    stop_keyword.is_some_and(|stop| fired.trim().eq_ignore_ascii_case(stop))
}

/// Transcribe, publish, and clear — the tail every close shares.
///
/// `trim` is the stop phrase when one ended this capture, so the phrase the
/// user said to stop talking is not itself sent to the agent.
fn finish_capture(
    transcriber: &Transcriber,
    speech_buf: &mut Vec<f32>,
    transcript_tx: &tokio_mpsc::Sender<String>,
    status: &StatusSink,
    trim: Option<&str>,
    flow: &AudioFlow,
) {
    status.set(AmbientStatus::Transcribing);
    // The worker cannot drain its audio queue while this blocks, and against a
    // speech server it blocks for a network round trip. Marked for the length
    // of the call so the staleness window measures a starved worker rather
    // than a busy one — see [`AudioFlow`]. Marked around the whole call, not
    // just the HTTP one, because the local recogniser blocks the same loop for
    // the same reason.
    let outcome = {
        let _busy = flow.transcribing();
        transcribe(transcriber, speech_buf, transcript_tx, trim)
    };
    speech_buf.clear();
    status.set(status_after_decode(outcome));
}

/// Turn the captured utterance into a published transcript.
///
/// The error is the transcriber's own — already logged there, and returned
/// here so the caller can leave it on the indicator. Audio that carried no
/// words is `Ok`: an utterance the recogniser found nothing in is an ordinary
/// outcome, not a fault to report. That includes an utterance which was only
/// the stop phrase, and is therefore empty once trimmed.
fn transcribe(
    transcriber: &Transcriber,
    speech_buf: &[f32],
    transcript_tx: &tokio_mpsc::Sender<String>,
    trim: Option<&str>,
) -> Result<(), String> {
    if speech_buf.is_empty() {
        return Ok(());
    }
    let text = transcriber.transcribe(speech_buf)?;
    let text = match trim {
        Some(stop_phrase) => strip_trailing_phrase(&text, stop_phrase),
        None => text,
    };
    if text.is_empty() {
        return Ok(());
    }
    if let Err(error) = transcript_tx.blocking_send(text) {
        eprintln!("buzz-desktop: ambient transcript channel closed: {error}");
    }
    Ok(())
}

/// Drop `phrase` from the end of `text`, if that is where it is.
///
/// The stop phrase is what *ended* the capture, so its audio is already in the
/// buffer the recogniser was handed — the spotter only emits a keyword a couple
/// of trailing blank frames after the phrase finishes. Trimming the audio
/// instead would need the keyword's position in the buffer, and the engine does
/// not give one that can be used: `KeywordResult::start_time` is always 0.00,
/// and `timestamps` are measured from the spotter's last internal reset, which
/// is a clock this crate would have to shadow and which was measured 0.1–0.2 s
/// away from the true phrase boundary. Cutting audio on that estimate would
/// take the user's last word about as often as it took the stop phrase.
///
/// Words are compared on their letters and digits alone, so "Buzz, stop." ends
/// on the same match as "buzz stop", and only a whole-word run at the very end
/// is removed — a sentence that merely mentions the phrase keeps it.
fn strip_trailing_phrase(text: &str, phrase: &str) -> String {
    let phrase_keys: Vec<String> = phrase.split_whitespace().map(word_key).collect();
    let phrase_keys: Vec<&String> = phrase_keys.iter().filter(|key| !key.is_empty()).collect();
    if phrase_keys.is_empty() {
        return text.trim().to_string();
    }
    let words: Vec<&str> = text.split_whitespace().collect();
    if words.len() < phrase_keys.len() {
        return text.trim().to_string();
    }
    let tail = &words[words.len() - phrase_keys.len()..];
    if !tail
        .iter()
        .zip(&phrase_keys)
        .all(|(word, key)| word_key(word) == **key)
    {
        return text.trim().to_string();
    }
    words[..words.len() - phrase_keys.len()].join(" ")
}

/// One spoken word reduced to what two transcriptions of it must share:
/// uppercase letters and digits, with punctuation dropped.
fn word_key(word: &str) -> String {
    word.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_uppercase)
        .collect()
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
