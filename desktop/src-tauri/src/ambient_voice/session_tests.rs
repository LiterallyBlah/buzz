//! Audio-worker seam tests.
//!
//! Two groups:
//!
//! * Pure seams (resampling, PCM decoding, engine pre-flight) — always run.
//! * Fixture-driven tests through the **real** sherpa-onnx keyword spotter.
//!   Those need the ~18 MB model, which is downloaded at runtime and is not in
//!   the repository, so they are `#[ignore]`d and read the model directory from
//!   `BUZZ_AMBIENT_KWS_MODEL_DIR`. Run them with:
//!
//!   ```text
//!   BUZZ_AMBIENT_KWS_MODEL_DIR=/path/to/sherpa-onnx-kws-zipformer-… \
//!     cargo test --lib ambient_voice::session -- --ignored --nocapture
//!   ```
//!
//!   They are the only place the tokenizer → engine contract is exercised
//!   against the real library. Everything else about that contract is
//!   unobservable from Rust: bad input kills the process rather than erroring.

use super::*;
use crate::ambient_voice::speech_stub_server::{StubReply, StubSpeechServer};
use crate::ambient_voice::wake_word::WakeWordTokenizer;

const MODEL_DIR_ENV: &str = "BUZZ_AMBIENT_KWS_MODEL_DIR";

fn f32_bytes(samples: &[f32]) -> Vec<u8> {
    samples.iter().flat_map(|s| s.to_le_bytes()).collect()
}

#[test]
fn pcm_bytes_decode_as_little_endian_f32() {
    let samples = [0.0f32, 1.0, -1.0, 0.25];
    assert_eq!(bytes_to_f32(&f32_bytes(&samples)), samples);
    // A trailing partial sample is ignored rather than misaligning the stream.
    let mut bytes = f32_bytes(&samples);
    bytes.push(0x7f);
    assert_eq!(bytes_to_f32(&bytes).len(), 4);
}

#[test]
fn sixteen_kilohertz_input_passes_straight_through() {
    // The pass-through exists so a 16 kHz WAV fixture reaches the engines
    // without the resampler standing in between.
    let mut resampler = Resampler::new(16_000).expect("resampler");
    let input: Vec<f32> = (0..VAD_FRAME_SAMPLES * 3).map(|i| i as f32).collect();
    let chunks = resampler.push(&input);
    assert_eq!(chunks.len(), 3);
    assert_eq!(chunks.concat(), input);
}

#[test]
fn forty_eight_kilohertz_input_is_resampled_to_a_third_the_samples() {
    let mut resampler = Resampler::new(48_000).expect("resampler");
    // One second of a 440 Hz tone at 48 kHz.
    let input: Vec<f32> = (0..48_000)
        .map(|i| (i as f32 * 440.0 * std::f32::consts::TAU / 48_000.0).sin())
        .collect();
    let produced: usize = resampler.push(&input).iter().map(Vec::len).sum();
    // 3:1 decimation, minus whatever the final partial chunk holds back.
    assert!(
        (15_000..=16_000).contains(&produced),
        "expected ~16000 output samples, got {produced}"
    );
}

#[test]
fn the_resampler_holds_partial_chunks_instead_of_emitting_short_ones() {
    let mut resampler = Resampler::new(16_000).expect("resampler");
    assert!(resampler.push(&vec![0.0; VAD_FRAME_SAMPLES - 1]).is_empty());
    let chunks = resampler.push(&[0.0]);
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].len(), VAD_FRAME_SAMPLES);
}

#[test]
fn an_incomplete_model_directory_is_refused_before_the_engine_is_touched() {
    // sherpa-onnx does not report a missing file — it fails deep inside ONNX
    // or, worse, succeeds with a half-built spotter. Pre-flight in app code.
    let dir = tempfile::tempdir().expect("temp dir");
    let Err(error) = create_keyword_spotter(dir.path(), "\u{2581}HE Y\n") else {
        panic!("an incomplete model directory must be refused");
    };
    assert!(error.contains("incomplete"), "{error}");
}

#[test]
fn an_empty_keywords_payload_is_refused() {
    // "" is not the same as "\n": the empty string means "the caller forgot",
    // while a single newline is the proven-safe representation of no keywords.
    let dir = tempfile::tempdir().expect("temp dir");
    let Err(error) = create_keyword_spotter(dir.path(), "") else {
        panic!("an empty keywords payload must be refused");
    };
    assert!(error.contains("empty payload"), "{error}");
}

#[test]
fn the_beam_and_trailing_blank_settings_match_the_spike_findings() {
    // Regression guard on two constants the M0 spike measured, both of which
    // are silent failures if a later change reverts them to sherpa's defaults:
    // beam 4 drops true detections with >4 armed keywords, and 1 trailing
    // blank makes the spotter fire on partial phrases.
    assert!(
        (8..=16).contains(&MAX_ACTIVE_PATHS),
        "max_active_paths must stay in the 8–16 band, got {MAX_ACTIVE_PATHS}"
    );
    assert!(
        (2..=4).contains(&NUM_TRAILING_BLANKS),
        "num_trailing_blanks must stay in the 2–4 band, got {NUM_TRAILING_BLANKS}"
    );
}

#[test]
fn an_utterance_that_could_not_be_transcribed_stays_on_the_indicator() {
    // With speech on a server, a failed utterance is the one thing the user
    // cannot see any other way: they spoke, nothing was published, and the
    // agent never answers. Going straight back to "listening for the wake
    // word" would leave them with a pill that claims to work.
    assert_eq!(
        status_after_decode(Err("Speech server failed: HTTP 502".to_string())),
        AmbientStatus::Error("Speech server failed: HTTP 502".to_string())
    );
    // Audio that simply carried no words is not a failure, and must not put a
    // red state on screen every time someone clears their throat.
    assert_eq!(status_after_decode(Ok(())), AmbientStatus::Listening);
}

// ── Telling the two armed keywords apart ─────────────────────────────────────

#[test]
fn only_the_configured_stop_phrase_reads_as_a_stop() {
    // The engine reports keywords in its own uppercase, space-joined form
    // (measured: `KeywordResult::keyword` came back as "LOVELY CHILD" for a
    // phrase typed "lovely child"), so the comparison is against that form.
    let stop = crate::ambient_voice::wake_word::engine_keyword("  buzz   Stop  ");
    assert_eq!(stop, "BUZZ STOP");
    assert!(is_stop_keyword(Some(&stop), "BUZZ STOP"));
    assert!(is_stop_keyword(Some(&stop), "buzz stop"));
    // The wake word firing on the same spotter must not be mistaken for it.
    assert!(!is_stop_keyword(Some(&stop), "HEY HERMES"));
    // With no stop phrase configured every detection is a wake word.
    assert!(!is_stop_keyword(None, "BUZZ STOP"));
}

// ── Status announcements ─────────────────────────────────────────────────────

/// A shared, thread-safe list of the statuses a notifier was handed.
type Announced = Arc<Mutex<Vec<AmbientStatus>>>;

/// Build a notifier that appends every announced status to `announced`.
fn recorder(announced: &Announced) -> AmbientStatusNotifier {
    let announced = Arc::clone(announced);
    Arc::new(move |next: &AmbientStatus| {
        announced
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(next.clone());
    })
}

#[test]
fn every_transition_is_announced_and_repeats_are_not() {
    // The indicator never polls, so a transition the worker does not announce
    // is a pill frozen on the last lifecycle event — the M1 dogfood bug. The
    // other half matters just as much: the worker re-asserts the same status
    // on most VAD frames, and one event per 32 ms frame would be an IPC flood
    // for a pill that does not change.
    let cell = Arc::new(Mutex::new(AmbientStatus::Off));
    let announced: Announced = Arc::new(Mutex::new(Vec::new()));
    let sink = StatusSink::new(Arc::clone(&cell), Some(recorder(&announced)));

    sink.set(AmbientStatus::Listening);
    sink.set(AmbientStatus::Listening);
    sink.set(AmbientStatus::Heard);
    sink.set(AmbientStatus::Capturing);
    sink.set(AmbientStatus::Capturing);
    sink.set(AmbientStatus::Transcribing);
    sink.set(AmbientStatus::Speaking);
    sink.set(AmbientStatus::Listening);

    assert_eq!(
        *announced.lock().expect("announced"),
        vec![
            AmbientStatus::Listening,
            AmbientStatus::Heard,
            AmbientStatus::Capturing,
            AmbientStatus::Transcribing,
            AmbientStatus::Speaking,
            AmbientStatus::Listening,
        ]
    );
    // The shared cell every reader (`AmbientSession::status`) sees stays in
    // step with what was announced.
    assert_eq!(*cell.lock().expect("cell"), AmbientStatus::Listening);
}

#[test]
fn a_session_without_a_notifier_still_records_its_status() {
    // The notifier is optional: no app handle must degrade to "the pill does
    // not live-update", never to "the session stops recording state".
    let cell = Arc::new(Mutex::new(AmbientStatus::Off));
    let sink = StatusSink::new(Arc::clone(&cell), None);
    sink.set(AmbientStatus::Heard);
    assert_eq!(*cell.lock().expect("cell"), AmbientStatus::Heard);
}

#[test]
fn the_worker_announces_the_transitions_it_makes() {
    // End-to-end through the production worker rather than the sink alone: an
    // empty model directory makes `create_keyword_spotter` fail, which is a
    // real worker-thread transition, and it has to arrive at the notifier and
    // not only in the shared cell.
    let dir = tempfile::tempdir().expect("temp dir");
    let announced: Announced = Arc::new(Mutex::new(Vec::new()));
    let cell = Arc::new(Mutex::new(AmbientStatus::Starting));

    let (session, _transcripts) = AmbientSession::new(AmbientSessionConfig {
        kws_model_dir: dir.path().to_path_buf(),
        stt_model_dir: dir.path().to_path_buf(),
        stt_endpoint: None,
        keywords_buf: "\u{2581}HE Y\n".to_string(),
        stop_keyword: None,
        stop_phrase: None,
        silence_hold_ms: super::super::utterance::DEFAULT_SILENCE_HOLD_MS,
        stt_health: Arc::new(crate::ambient_voice::speech_health::RoleHealth::default()),
        tts_active: Arc::new(AtomicBool::new(false)),
        tts_cancel: Arc::new(AtomicBool::new(false)),
        muted: Arc::new(AtomicBool::new(false)),
        mute_epochs: Arc::new(AtomicU64::new(0)),
        mute_authority: Arc::new(Mutex::new(())),
        status: Arc::clone(&cell),
        on_status_change: Some(recorder(&announced)),
        input_sample_rate: 16_000,
    })
    .expect("session");

    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if !announced.lock().expect("announced").is_empty() {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    session.shutdown();

    let announced = announced.lock().expect("announced").clone();
    assert!(
        announced
            .iter()
            .any(|status| matches!(status, AmbientStatus::Error(_))),
        "the worker's own transition never reached the frontend seam: {announced:?}"
    );
}

#[test]
fn the_worker_stamps_the_audio_it_takes_off_the_queue() {
    // The whole watchdog rests on this: `capturing` means "the worker thread is
    // alive", and only a stamp taken where the queue is consumed can say
    // whether anything is arriving at it. Driven through the production
    // `AmbientSession` — pushing into the real channel and reading the real
    // handle — because a counter that is correct but never called is exactly
    // the failure this feature already shipped once.
    let dir = tempfile::tempdir().expect("temp dir");
    let (session, _transcripts) = AmbientSession::new(AmbientSessionConfig {
        kws_model_dir: dir.path().to_path_buf(),
        stt_model_dir: dir.path().to_path_buf(),
        stt_endpoint: None,
        keywords_buf: "\u{2581}HE Y\n".to_string(),
        stop_keyword: None,
        stop_phrase: None,
        silence_hold_ms: super::super::utterance::DEFAULT_SILENCE_HOLD_MS,
        stt_health: Arc::new(crate::ambient_voice::speech_health::RoleHealth::default()),
        tts_active: Arc::new(AtomicBool::new(false)),
        tts_cancel: Arc::new(AtomicBool::new(false)),
        muted: Arc::new(AtomicBool::new(false)),
        mute_epochs: Arc::new(AtomicU64::new(0)),
        mute_authority: Arc::new(Mutex::new(())),
        status: Arc::new(Mutex::new(AmbientStatus::Starting)),
        on_status_change: None,
        input_sample_rate: 16_000,
    })
    .expect("session");

    // A session nobody has fed reports exactly that, and its silence is
    // measured from the moment it started rather than from an absent batch.
    let fresh = session.audio_flow();
    assert_eq!(fresh.batches, 0);
    assert!(fresh.since_last_batch < Duration::from_secs(1), "{fresh:?}");

    for _ in 0..3 {
        session
            .push_audio(f32_bytes(&[0.0; VAD_FRAME_SAMPLES]))
            .expect("push");
    }

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut flow = session.audio_flow();
    while flow.batches == 0 && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
        flow = session.audio_flow();
    }
    session.shutdown();

    assert!(
        flow.batches > 0,
        "audio reached the worker and was never stamped: {flow:?}"
    );
    assert!(flow.since_last_batch < Duration::from_secs(1), "{flow:?}");
}

// ── A busy worker is not a starved one ───────────────────────────────────────

#[test]
fn time_spent_transcribing_is_not_counted_as_time_without_audio() {
    // Every edge of the measurement, with the clock passed in. `now`, the last
    // batch, the finished transcriptions and the one in flight are four reads
    // of four atomics, and the answer has to stay a sane duration for all of
    // them — including the orderings only a torn read could produce.
    //
    // The `+ 1` on the in-flight stamp is why a transcription that started at
    // millisecond zero still counts: 1 means "started at 0", 0 means "none".
    assert_eq!(starved_ms(10_000, 4_000, 0, 0), 6_000);
    // The same six seconds, all of it spent transcribing: nothing was starved.
    assert_eq!(starved_ms(10_000, 4_000, 6_000, 0), 0);
    // Half of it finished, half still in flight.
    assert_eq!(starved_ms(10_000, 4_000, 3_000, 7_000 + 1), 0);
    // Quiet, then a transcription that is still running: only the quiet counts.
    assert_eq!(starved_ms(10_000, 1_000, 0, 9_000 + 1), 8_000);
    // A transcription that began in the session's first millisecond.
    assert_eq!(starved_ms(5_000, 0, 0, 1), 0);
    // Impossible orderings saturate instead of wrapping.
    assert_eq!(starved_ms(1_000, 4_000, 0, 0), 0);
    assert_eq!(starved_ms(10_000, 4_000, 99_000, 0), 0);
    // A stamp later than `now` is the one torn read that can actually happen:
    // `snapshot` reads the clock before the atomics, so a transcription that
    // starts in between is stamped after the `now` it will be compared with.
    // Nothing is subtracted for it, which errs towards reporting a starved
    // worker rather than towards hiding a genuinely deaf one — and the time
    // being ignored is the microsecond that race is wide.
    assert_eq!(starved_ms(10_000, 4_000, 0, 99_000 + 1), 6_000);
}

#[test]
fn a_slow_speech_server_does_not_make_a_fed_session_look_deaf() {
    // The shipped fault this fixes, driven through the production
    // `finish_capture` against a real loopback server that takes its time.
    //
    // The worker is one loop: while it waits for a transcript it is not
    // draining its audio queue, so an utterance sent to a slow server used to
    // read as that many seconds of "no audio arriving from the microphone" —
    // which put the wrong words on the pill and had the webview's watchdog
    // rebuild the whole capture pipeline, once per utterance, against a
    // microphone that was working.
    const SERVER_TAKES: Duration = Duration::from_millis(400);

    let server = StubSpeechServer::start(|_| {
        thread::sleep(SERVER_TAKES);
        StubReply::json(r#"{"text": "book me a room"}"#)
    });
    let dir = tempfile::tempdir().expect("temp dir");
    let transcriber = Transcriber::build(
        dir.path(),
        Some(server.base_url()),
        Arc::new(crate::ambient_voice::speech_health::RoleHealth::default()),
    )
    .expect("transcriber");

    let flow = AudioFlow::new();
    flow.record(1);
    let (transcript_tx, mut transcripts) = tokio_mpsc::channel::<Transcript>(4);
    let status = StatusSink::new(Arc::new(Mutex::new(AmbientStatus::Capturing)), None);
    let mut speech_buf = vec![0.05_f32; 16_000];
    let mut rolling =
        RollingCapture::spawn(move |samples| transcriber.transcribe(samples)).expect("rolling");

    finish_capture(
        &mut rolling,
        &mut speech_buf,
        &transcript_tx,
        &status,
        None,
        &flow,
        MuteSignal {
            muted: &AtomicBool::new(false),
            epochs: &AtomicU64::new(0),
            armed_under: Some(0),
        },
    );

    // The utterance really did go to the server and come back — otherwise this
    // would be measuring a call that never blocked.
    assert_eq!(
        transcripts.try_recv().ok().map(|t| t.text).as_deref(),
        Some("book me a room")
    );
    assert_eq!(server.requests().len(), 1);

    let flow = flow.snapshot();
    assert!(
        flow.since_last_batch < SERVER_TAKES / 2,
        "a worker busy with the server's own latency was reported starved of audio: {flow:?}"
    );
}

#[test]
fn a_worker_that_is_transcribing_nothing_is_still_measured_as_before() {
    // The other half: subtracting the worker's own work must not make a
    // genuinely deaf session look fed. Nothing is transcribing here, so time
    // passing is exactly what it was before this existed.
    let flow = AudioFlow::new();
    flow.record(1);
    thread::sleep(Duration::from_millis(120));
    let quiet = flow.snapshot().since_last_batch;
    assert!(
        quiet >= Duration::from_millis(100),
        "silence stopped being measured at all: {quiet:?}"
    );
}

#[test]
fn a_transcription_that_unwinds_gives_the_watchdog_back() {
    // The mark is what switches the staleness watchdog off, so an exit that
    // skipped its removal would leave the watchdog off for the life of the
    // session — silently, since a session with the watchdog disabled looks
    // exactly like a session that is being fed. That is a worse failure than
    // the one the marking was added to fix, so it must not depend on control
    // reaching a second call.
    let flow = AudioFlow::new();
    flow.record(1);

    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let died = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _busy = flow.transcribing();
        panic!("the recogniser gave up mid-utterance");
    }));
    std::panic::set_hook(previous);
    assert!(died.is_err(), "the panic did not happen");

    thread::sleep(Duration::from_millis(120));
    let quiet = flow.snapshot().since_last_batch;
    assert!(
        quiet >= Duration::from_millis(100),
        "the watchdog stayed marked busy after an unwind: {quiet:?}"
    );
}

// ── Closing a capture, with and without a chunk behind it ────────────────────

/// A capture about to be closed: the status recorder, the transcript channel
/// and a buffer with something in it.
struct ClosingCapture {
    announced: Announced,
    status: StatusSink,
    transcript_tx: tokio_mpsc::Sender<Transcript>,
    transcripts: tokio_mpsc::Receiver<Transcript>,
    speech_buf: Vec<f32>,
    flow: AudioFlow,
}

impl ClosingCapture {
    fn new() -> Self {
        let announced: Announced = Arc::new(Mutex::new(Vec::new()));
        let status = StatusSink::new(
            Arc::new(Mutex::new(AmbientStatus::Capturing)),
            Some(recorder(&announced)),
        );
        let (transcript_tx, transcripts) = tokio_mpsc::channel::<Transcript>(4);
        Self {
            announced,
            status,
            transcript_tx,
            transcripts,
            speech_buf: vec![0.05_f32; 16_000],
            flow: AudioFlow::new(),
        }
    }

    fn close(&mut self, rolling: &mut RollingCapture, trim: Option<&str>) {
        finish_capture(
            rolling,
            &mut self.speech_buf,
            &self.transcript_tx,
            &self.status,
            trim,
            &self.flow,
            MuteSignal {
                muted: &AtomicBool::new(false),
                epochs: &AtomicU64::new(0),
                armed_under: Some(0),
            },
        );
    }

    fn announced(&self) -> Vec<AmbientStatus> {
        self.announced.lock().expect("announced").clone()
    }

    /// Every transcript submitted so far. One entry is one message to the
    /// agent, which is what "submitted once" is counted in.
    fn submitted(&mut self) -> Vec<String> {
        let mut sent = Vec::new();
        while let Ok(transcript) = self.transcripts.try_recv() {
            sent.push(transcript.text);
        }
        sent
    }
}

/// A transcriber that answers each chunk with the next line of `script`.
fn scripted_capture(script: &'static [&'static str]) -> RollingCapture {
    let next = AtomicU64::new(0);
    RollingCapture::spawn(move |_| {
        let index = next.fetch_add(1, Ordering::AcqRel) as usize;
        Ok(script.get(index).copied().unwrap_or_default().to_string())
    })
    .expect("rolling")
}

#[test]
fn an_ordinary_utterance_announces_transcribing_once_and_sends_one_message() {
    // The parity regression. Almost every utterance is one chunk, and for those
    // the rolling machinery must be invisible: the pill goes Transcribing while
    // the recogniser has it and back to Listening when it is done, and the
    // agent is sent exactly one message.
    let mut capture = ClosingCapture::new();
    let mut rolling = scripted_capture(&["book me a room"]);

    capture.close(&mut rolling, None);

    assert_eq!(
        capture.announced(),
        vec![AmbientStatus::Transcribing, AmbientStatus::Listening]
    );
    assert_eq!(capture.submitted(), vec!["book me a room".to_string()]);
    assert!(
        capture.speech_buf.is_empty(),
        "the buffer was left holding audio"
    );
}

#[test]
fn a_chunk_handed_off_mid_capture_announces_nothing_until_the_close() {
    // Status honesty: from the user's side the ceiling is not an event. They
    // are still talking, so the pill still says so — and the two chunks arrive
    // as one message, in the order they were spoken.
    //
    // The handoff has no way to announce anything (it is not given the sink);
    // what is asserted here is the sequence a whole rolled capture produces,
    // which is the one an extra `Transcribing` per chunk would show up in.
    let mut capture = ClosingCapture::new();
    let mut rolling = scripted_capture(&["the first half", "and the second"]);

    rolling.hand_off(vec![0.05_f32; 16_000]);
    capture.close(&mut rolling, None);

    assert_eq!(
        capture.announced(),
        vec![AmbientStatus::Transcribing, AmbientStatus::Listening],
        "a rolled capture moved the indicator more than an ordinary one"
    );
    assert_eq!(
        capture.submitted(),
        vec!["the first half and the second".to_string()],
        "a rolled capture must reach the agent as one message"
    );
}

#[test]
fn a_stop_phrase_that_closed_a_rolled_capture_is_still_trimmed_off() {
    // The stop phrase closes the last chunk exactly as it closes a capture
    // today, and what it must not do is end up in the message.
    let mut capture = ClosingCapture::new();
    let mut rolling = scripted_capture(&["remind me to buy milk", "buzz stop"]);

    rolling.hand_off(vec![0.05_f32; 16_000]);
    capture.close(&mut rolling, Some("buzz stop"));

    assert_eq!(
        capture.submitted(),
        vec!["remind me to buy milk".to_string()]
    );
}

#[test]
fn a_chunk_that_failed_leaves_the_failure_on_the_indicator_and_sends_nothing() {
    // A hole in the middle of a message, silently sent, would be worse than no
    // message: the user would read their own words back with a sentence missing
    // and nothing to say one had been lost.
    let mut capture = ClosingCapture::new();
    let mut rolling = RollingCapture::spawn(|_| Err("Speech server failed: HTTP 502".to_string()))
        .expect("rolling");

    rolling.hand_off(vec![0.05_f32; 16_000]);
    capture.close(&mut rolling, None);

    assert_eq!(
        capture.announced(),
        vec![
            AmbientStatus::Transcribing,
            AmbientStatus::Error("Speech server failed: HTTP 502".to_string()),
        ]
    );
    assert!(capture.submitted().is_empty());
}

#[test]
fn a_chunk_decoding_off_the_worker_thread_leaves_the_audio_watchdog_on() {
    // The other half of `a_slow_speech_server_does_not_make_a_fed_session_look
    // _deaf`, and the edge that arrives with rolling capture: the busy mark
    // measures the *worker* being blocked, and a chunk being decoded elsewhere
    // does not block it. Marking one would switch the staleness watchdog off
    // for as long as a capture runs — minutes — and a microphone that died
    // mid-conversation would never be reported at all.
    const DECODE_TAKES: Duration = Duration::from_millis(400);

    let flow = AudioFlow::new();
    flow.record(1);
    let mut rolling = RollingCapture::spawn(move |_| {
        thread::sleep(DECODE_TAKES);
        Ok("the first half".to_string())
    })
    .expect("rolling");

    rolling.hand_off(vec![0.05_f32; 16_000]);
    thread::sleep(Duration::from_millis(120));

    let quiet = flow.snapshot().since_last_batch;
    assert!(
        quiet >= Duration::from_millis(100),
        "a chunk decoding off the worker thread switched the audio watchdog off: {quiet:?}"
    );
}

#[test]
fn a_mute_landing_during_the_final_close_is_not_talked_over() {
    // The close blocks the worker for as long as transcription takes — against
    // a slow server, minutes — and the mute button works the whole time:
    // `set_muted` stores the flag and writes `Muted` to the pill from the
    // command thread. The worker only reads the flag between audio batches, so
    // without a fence the close queued the muted words for publication anyway
    // and then wrote `Listening` straight over the `Muted` the user was shown.
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let mut rolling = RollingCapture::spawn(move |_| {
        let _ = release_rx.recv();
        Ok("said just before the mute".to_string())
    })
    .expect("rolling");

    let muted = Arc::new(AtomicBool::new(false));
    let mute_epochs = Arc::new(AtomicU64::new(0));
    let mute_authority = Mutex::new(());
    let status_cell = Arc::new(Mutex::new(AmbientStatus::Capturing));
    let announced: Announced = Arc::new(Mutex::new(Vec::new()));
    let status = StatusSink::new(Arc::clone(&status_cell), Some(recorder(&announced)));
    let (transcript_tx, mut transcripts) = tokio_mpsc::channel::<Transcript>(4);

    let worker = {
        let muted = Arc::clone(&muted);
        let mute_epochs = Arc::clone(&mute_epochs);
        thread::spawn(move || {
            let flow = AudioFlow::new();
            finish_capture(
                &mut rolling,
                &mut vec![0.05_f32; 16_000],
                &transcript_tx,
                &status,
                None,
                &flow,
                MuteSignal {
                    muted: &muted,
                    epochs: &mute_epochs,
                    // Armed before the mute this test lands mid-close.
                    armed_under: Some(mute_epochs.load(Ordering::Acquire)),
                },
            );
        })
    };

    // Wait until the close is genuinely inside its blocking wait…
    let deadline = Instant::now() + Duration::from_secs(5);
    while !matches!(
        *status_cell.lock().expect("status"),
        AmbientStatus::Transcribing
    ) {
        assert!(Instant::now() < deadline, "the close never started");
        thread::sleep(Duration::from_millis(1));
    }
    // …then mute through the exact writes the `set_muted` command performs…
    apply_mute(&muted, &mute_epochs, &mute_authority, &status_cell, true);
    // …and only then let the transcription finish.
    drop(release_tx);
    worker.join().expect("the close panicked");

    assert!(
        transcripts.try_recv().is_err(),
        "words muted mid-close were queued for publication anyway"
    );
    assert_eq!(
        *status_cell.lock().expect("status"),
        AmbientStatus::Muted,
        "the close talked over the mute the user was shown"
    );
    assert_eq!(
        announced.lock().expect("announced").clone(),
        vec![AmbientStatus::Transcribing],
        "nothing after the mute may announce a status"
    );
}

#[test]
fn an_unmute_before_the_close_returns_does_not_revive_the_muted_capture() {
    // The sharper race: mute lands while the close is waiting, and the user
    // unmutes before the decoder returns. A fence that reads the flag's
    // present value finds it false again — the unmute erased the evidence —
    // and the capture the user muted is queued anyway. The mute is a death
    // sentence for the capture in progress, pronounced at the event; an
    // unmute may open the microphone for the next capture, never revive the
    // last one. `apply_mute` therefore latches an epoch on every mute-on, and
    // the close compares epochs around its wait instead of trusting the flag.
    let next = AtomicU64::new(0);
    let (tokens_tx, tokens_rx) = mpsc::channel::<()>();
    let mut rolling = RollingCapture::spawn(move |_| {
        let _ = tokens_rx.recv();
        match next.fetch_add(1, Ordering::AcqRel) {
            0 => Ok("said before the mute".to_string()),
            1 => Ok("said after the unmute".to_string()),
            _ => Err("Speech server failed: HTTP 502".to_string()),
        }
    })
    .expect("rolling");

    let muted = Arc::new(AtomicBool::new(false));
    let mute_epochs = Arc::new(AtomicU64::new(0));
    let mute_authority = Mutex::new(());
    let status_cell = Arc::new(Mutex::new(AmbientStatus::Capturing));
    let announced: Announced = Arc::new(Mutex::new(Vec::new()));
    let status = StatusSink::new(Arc::clone(&status_cell), Some(recorder(&announced)));
    let (transcript_tx, mut transcripts) = tokio_mpsc::channel::<Transcript>(8);
    let flow = AudioFlow::new();

    let wait_for_transcribing = |cell: &Arc<Mutex<AmbientStatus>>| {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !matches!(*cell.lock().expect("status"), AmbientStatus::Transcribing) {
            assert!(Instant::now() < deadline, "the close never started");
            thread::sleep(Duration::from_millis(1));
        }
    };
    let close_while = |rolling: &mut RollingCapture, mid_close: &dyn Fn()| {
        // The capture being closed was armed under the epoch in force when the
        // close begins: every mute in this test lands mid-close, after it.
        let armed_under = Some(mute_epochs.load(Ordering::Acquire));
        thread::scope(|scope| {
            let worker = scope.spawn(|| {
                finish_capture(
                    rolling,
                    &mut vec![0.05_f32; 16_000],
                    &transcript_tx,
                    &status,
                    None,
                    &flow,
                    MuteSignal {
                        muted: &muted,
                        epochs: &mute_epochs,
                        armed_under,
                    },
                );
            });
            mid_close();
            tokens_tx.send(()).expect("release the decoder");
            worker.join().expect("the close panicked");
        });
    };

    // A mute and an unmute both land while the close is waiting: the words
    // muted mid-flight are not sent, and the pill reads what the unmute wrote.
    close_while(&mut rolling, &|| {
        wait_for_transcribing(&status_cell);
        apply_mute(&muted, &mute_epochs, &mute_authority, &status_cell, true);
        apply_mute(&muted, &mute_epochs, &mute_authority, &status_cell, false);
    });
    assert!(
        transcripts.try_recv().is_err(),
        "an unmute revived the capture the mute had killed"
    );
    assert_eq!(
        *status_cell.lock().expect("status"),
        AmbientStatus::Listening
    );

    // The unmute opened the microphone for what comes next: a genuinely new
    // capture on the same machinery decodes, stitches and publishes.
    close_while(&mut rolling, &|| {});
    assert_eq!(
        transcripts.try_recv().ok().map(|t| t.text).as_deref(),
        Some("said after the unmute"),
        "the fence must kill the muted capture, not the session"
    );

    // And a stale close cannot repaint the state the mute cycle established:
    // this decode *fails* after the mute→unmute, and the failure belongs to a
    // dead capture — the pill keeps reading `Listening`, not `Error`.
    announced.lock().expect("announced").clear();
    close_while(&mut rolling, &|| {
        wait_for_transcribing(&status_cell);
        apply_mute(&muted, &mute_epochs, &mute_authority, &status_cell, true);
        apply_mute(&muted, &mute_epochs, &mute_authority, &status_cell, false);
    });
    assert_eq!(
        *status_cell.lock().expect("status"),
        AmbientStatus::Listening,
        "a dead capture's failure repainted the pill"
    );
    assert_eq!(
        announced.lock().expect("announced").clone(),
        vec![AmbientStatus::Transcribing],
        "a dead capture announced something after the mute cycle"
    );
}

/// The worker's capture stage, driven frame by frame without the engines.
///
/// The wake word and the VAD's verdict are the two answers the ONNX models
/// supply, and both are inputs here; everything the worker does with them —
/// arming, buffering, closing — is the production machine. That is what lets
/// the mute fence be driven from where a capture *starts*, which is where the
/// epoch is now bound and therefore where the orderings below have to begin.
/// One instant serves throughout: nothing in this fence depends on time
/// passing, and a test that slept would be proving something else.
struct CaptureRun {
    machine: UtteranceMachine,
    speech_buf: Vec<f32>,
    now: Instant,
}

impl CaptureRun {
    fn new() -> Self {
        Self {
            machine: UtteranceMachine::new(UtteranceTiming::from_silence_hold_ms(
                crate::ambient_voice::utterance::DEFAULT_SILENCE_HOLD_MS,
            )),
            speech_buf: Vec::new(),
            now: Instant::now(),
        }
    }

    /// A wake word fired: the capture begins here.
    fn wake(&mut self) {
        self.machine.on_wake(self.now);
    }

    /// `frames` frames the VAD called speech, buffered as the worker buffers.
    fn speak(&mut self, frames: usize) {
        for _ in 0..frames {
            match self.machine.on_frame(true, false, self.now) {
                FrameOutcome::Buffer => self.buffer_frame(),
                other => panic!("a speech frame produced {other:?}"),
            }
        }
    }

    /// Silence until the machine closes the capture, and how it closed.
    fn silence_until_close(&mut self) -> FrameOutcome {
        // Generous: the close comes after `silence_flush_frames`, and anything
        // that does not reach it is a hung capture rather than a slow one.
        for _ in 0..10_000 {
            match self.machine.on_frame(false, false, self.now) {
                FrameOutcome::Buffer => self.buffer_frame(),
                closed => {
                    self.buffer_frame();
                    return closed;
                }
            }
        }
        panic!("the capture never closed")
    }

    fn buffer_frame(&mut self) {
        self.speech_buf
            .extend_from_slice(&[0.05_f32; VAD_FRAME_SAMPLES]);
    }
}

#[test]
fn a_mute_and_unmute_before_the_close_begins_does_not_revive_the_capture() {
    // The ordering the close cannot see from where it stands. The worker reads
    // mute once per batch, so a mute and an unmute can both complete while a
    // capture is collecting — between two batches, or inside one before the
    // frame that ends the utterance. The flag is back to false and the close
    // has not started yet, so a fence that snapshots the epoch when the close
    // *begins* snapshots the epoch the mute already moved to, compares it
    // against itself, and publishes the capture the user muted.
    //
    // The capture's authority is therefore bound when the capture starts and
    // carried for its whole life: the close is handed the epoch its capture
    // was armed under and has nothing of its own to snapshot.
    //
    // Counted rather than scripted: a capture that is dead when its close
    // begins must never reach the transcriber at all, which is a stronger
    // statement than "its words were not published" and the only one that
    // holds against a speech server the close would otherwise wait on.
    let decodes = Arc::new(AtomicU64::new(0));
    let mut rolling = {
        let decodes = Arc::clone(&decodes);
        RollingCapture::spawn(move |_| {
            decodes.fetch_add(1, Ordering::AcqRel);
            Ok("said after the unmute".to_string())
        })
        .expect("rolling")
    };
    let muted = Arc::new(AtomicBool::new(false));
    let mute_epochs = Arc::new(AtomicU64::new(0));
    let mute_authority = Mutex::new(());
    let status_cell = Arc::new(Mutex::new(AmbientStatus::Listening));
    let announced: Announced = Arc::new(Mutex::new(Vec::new()));
    let status = StatusSink::new(Arc::clone(&status_cell), Some(recorder(&announced)));
    let (transcript_tx, mut transcripts) = tokio_mpsc::channel::<Transcript>(8);
    let flow = AudioFlow::new();

    // A real capture, under the epoch in force when the wake word fired.
    let mut capture_epoch = CaptureEpoch::default();
    let mut capture = CaptureRun::new();
    capture.wake();
    capture_epoch.arm(&mute_epochs);
    let armed_under = mute_epochs.load(Ordering::Acquire);
    capture.speak(crate::ambient_voice::utterance::MIN_VOICED_FRAMES + 2);
    assert!(
        !capture.speech_buf.is_empty(),
        "the capture collected no audio to be muted"
    );

    // The user mutes and unmutes while the capture is still collecting, through
    // the writes the `set_ambient_voice_muted` command performs.
    apply_mute(&muted, &mute_epochs, &mute_authority, &status_cell, true);
    apply_mute(&muted, &mute_epochs, &mute_authority, &status_cell, false);
    announced.lock().expect("announced").clear();

    // Only now does the capture reach its close.
    assert_eq!(capture.silence_until_close(), FrameOutcome::Decode);
    finish_capture(
        &mut rolling,
        &mut capture.speech_buf,
        &transcript_tx,
        &status,
        None,
        &flow,
        MuteSignal {
            muted: &muted,
            epochs: &mute_epochs,
            armed_under: capture_epoch.disarm(),
        },
    );

    assert!(
        transcripts.try_recv().is_err(),
        "old capture was revived because close snapshots epoch after mute"
    );
    assert_eq!(
        *status_cell.lock().expect("status"),
        AmbientStatus::Listening,
        "the dead capture repainted the pill the unmute wrote"
    );
    assert_eq!(
        announced.lock().expect("announced").clone(),
        Vec::new(),
        "the dead capture announced a status of its own"
    );
    assert!(
        !transcript_still_wanted(&mute_epochs, armed_under),
        "the epoch the capture was armed under is still current"
    );
    assert_eq!(
        decodes.load(Ordering::Acquire),
        0,
        "a capture that was already dead was still sent to the transcriber"
    );

    // The unmute opened the microphone for what comes next: a capture armed
    // after it carries the later epoch and publishes normally.
    let mut next = CaptureRun::new();
    next.wake();
    capture_epoch.arm(&mute_epochs);
    let next_armed_under = mute_epochs.load(Ordering::Acquire);
    next.speak(crate::ambient_voice::utterance::MIN_VOICED_FRAMES + 2);
    assert_eq!(next.silence_until_close(), FrameOutcome::Decode);
    finish_capture(
        &mut rolling,
        &mut next.speech_buf,
        &transcript_tx,
        &status,
        None,
        &flow,
        MuteSignal {
            muted: &muted,
            epochs: &mute_epochs,
            armed_under: capture_epoch.disarm(),
        },
    );

    let published = transcripts
        .try_recv()
        .expect("the live capture was dropped");
    assert_eq!(published.text, "said after the unmute");
    assert_eq!(
        published.mute_epoch, next_armed_under,
        "the transcript was stamped with an epoch its capture did not begin under"
    );
    assert!(transcript_still_wanted(&mute_epochs, published.mute_epoch));
    assert_eq!(
        decodes.load(Ordering::Acquire),
        1,
        "the live capture did not reach the transcriber"
    );
    assert_eq!(
        *status_cell.lock().expect("status"),
        AmbientStatus::Listening
    );
}

#[test]
fn an_epoch_that_moved_between_batches_kills_the_capture_the_worker_never_saw_muted() {
    // The other half of the same ordering, one batch earlier. The worker reads
    // the flag once per batch and it is false at both looks, so the mute is
    // invisible to it — but the counter moved, and the capture in progress was
    // bound to the value before it. What must happen at that observation is
    // exactly what a mute the worker *did* see would do: the machine goes back
    // to idle, the buffer is dropped, and the chunks already handed off are
    // disowned so they cannot be stitched into whatever is said next.
    let mut rolling = scripted_capture(&["a chunk of the muted capture", "said after the unmute"]);
    let muted = Arc::new(AtomicBool::new(false));
    let mute_epochs = Arc::new(AtomicU64::new(0));
    let mute_authority = Mutex::new(());
    let status_cell = Arc::new(Mutex::new(AmbientStatus::Listening));
    let status = StatusSink::new(Arc::clone(&status_cell), None);
    let (transcript_tx, mut transcripts) = tokio_mpsc::channel::<Transcript>(8);
    let flow = AudioFlow::new();

    // A capture long enough for the ceiling to have taken a chunk off it.
    let mut capture_epoch = CaptureEpoch::default();
    let mut capture = CaptureRun::new();
    capture.wake();
    capture_epoch.arm(&mute_epochs);
    capture.speak(crate::ambient_voice::utterance::MIN_VOICED_FRAMES + 2);
    rolling.hand_off(std::mem::take(&mut capture.speech_buf));

    // The mute and the unmute both land between two batches.
    apply_mute(&muted, &mute_epochs, &mute_authority, &status_cell, true);
    apply_mute(&muted, &mute_epochs, &mute_authority, &status_cell, false);
    assert!(
        !muted.load(Ordering::Acquire),
        "the flag the worker reads is up, so this is not the ordering under test"
    );

    // The next batch arrives. This is the check the worker makes on it.
    assert!(
        capture_epoch.revoked(&mute_epochs),
        "an epoch change between batches was not seen as one"
    );
    abandon_capture(
        &mut capture.machine,
        &mut capture.speech_buf,
        &mut rolling,
        &mut capture_epoch,
    );
    assert_eq!(
        capture.machine.phase(),
        crate::ambient_voice::utterance::UtterancePhase::Idle,
        "the muted capture is still collecting"
    );
    assert!(capture.speech_buf.is_empty());
    assert!(
        capture_epoch.disarm().is_none(),
        "the killed capture is still holding an epoch"
    );

    // A genuinely new capture on the same machinery: what it sends is its own
    // words, with nothing of the abandoned capture stitched in front of them.
    let mut next = CaptureRun::new();
    next.wake();
    capture_epoch.arm(&mute_epochs);
    next.speak(crate::ambient_voice::utterance::MIN_VOICED_FRAMES + 2);
    assert_eq!(next.silence_until_close(), FrameOutcome::Decode);
    finish_capture(
        &mut rolling,
        &mut next.speech_buf,
        &transcript_tx,
        &status,
        None,
        &flow,
        MuteSignal {
            muted: &muted,
            epochs: &mute_epochs,
            armed_under: capture_epoch.disarm(),
        },
    );

    let published = transcripts
        .try_recv()
        .expect("the live capture was dropped");
    assert_eq!(
        published.text, "said after the unmute",
        "a chunk of the abandoned capture was stitched into the next one"
    );
    assert!(transcripts.try_recv().is_err());
}

// ── Fixture-driven tests through the real engine ─────────────────────────────

fn model_dir_from_env() -> PathBuf {
    let dir = std::env::var(MODEL_DIR_ENV).unwrap_or_else(|_| {
        panic!("set {MODEL_DIR_ENV} to a sherpa-onnx KWS model directory to run this test")
    });
    let dir = PathBuf::from(dir);
    assert!(dir.is_dir(), "{MODEL_DIR_ENV} is not a directory: {dir:?}");
    dir
}

/// Feed a WAV through the production spotting loop and collect what fired.
fn detections_for(model_dir: &Path, phrases: &[&str], wav: &Path) -> Vec<String> {
    let tokenizer = WakeWordTokenizer::load(model_dir).expect("load tokenizer");
    let owned: Vec<String> = phrases.iter().map(|p| (*p).to_string()).collect();
    let keywords_buf = tokenizer
        .keywords_buf(&owned)
        .unwrap_or_else(|(phrase, error)| panic!("{phrase}: {error}"));

    let spotter = create_keyword_spotter(model_dir, &keywords_buf).expect("create spotter");
    let wave = sherpa_onnx::Wave::read(&wav.to_string_lossy()).expect("read wav fixture");
    assert_eq!(wave.sample_rate(), 16_000, "fixture must be 16 kHz");

    let stream = spotter.create_stream();
    let mut fired = Vec::new();
    // Chunk exactly as the worker does, so the test exercises the streaming
    // path rather than a single bulk accept_waveform.
    for chunk in wave.samples().chunks(VAD_FRAME_SAMPLES) {
        stream.accept_waveform(16_000, chunk);
        fired.extend(drain_detections(&spotter, &stream));
    }
    fired
}

#[test]
#[ignore = "needs the downloaded KWS model; set BUZZ_AMBIENT_KWS_MODEL_DIR"]
fn a_user_typed_wake_word_fires_on_the_matching_fixture() {
    let model_dir = model_dir_from_env();
    let wav = model_dir.join("test_wavs/1.wav");

    // "lovely child" is spoken in 1.wav and not in 0.wav. It is typed here in
    // lower case exactly as a user would type it — normalisation, Viterbi
    // segmentation and vocabulary validation all run before the engine.
    let fired = detections_for(&model_dir, &["lovely child"], &wav);
    assert!(
        fired.iter().any(|k| k.eq_ignore_ascii_case("LOVELY CHILD")),
        "expected LOVELY CHILD in {fired:?}"
    );
}

#[test]
#[ignore = "needs the downloaded KWS model; set BUZZ_AMBIENT_KWS_MODEL_DIR"]
fn a_wake_word_that_is_not_spoken_does_not_fire() {
    let model_dir = model_dir_from_env();
    // The same keyword against the other fixture: the control that makes the
    // positive test meaningful.
    let fired = detections_for(
        &model_dir,
        &["lovely child"],
        &model_dir.join("test_wavs/0.wav"),
    );
    assert!(fired.is_empty(), "unexpected detections: {fired:?}");
}

#[test]
#[ignore = "needs the downloaded KWS model; set BUZZ_AMBIENT_KWS_MODEL_DIR"]
fn a_wake_word_and_a_stop_phrase_both_fire_from_one_spotter() {
    // The stop phrase is armed beside the wake word on the *same* spotter
    // session rather than on a second engine — a second spotter would be a
    // second ONNX model load and a second copy of every frame. This is the
    // check that one engine really does answer for both: 1.wav says "lovely
    // child" and, later, "for ever", and both come back distinguishable by
    // `KeywordResult::keyword` alone. A third armed phrase that is not spoken
    // in this fixture is the control against "everything fires".
    let model_dir = model_dir_from_env();
    let fired = detections_for(
        &model_dir,
        &["lovely child", "for ever", "light up"],
        &model_dir.join("test_wavs/1.wav"),
    );
    assert!(
        fired.iter().any(|k| k.eq_ignore_ascii_case("LOVELY CHILD")),
        "the wake word did not fire: {fired:?}"
    );
    assert!(
        fired.iter().any(|k| k.eq_ignore_ascii_case("FOR EVER")),
        "the second armed keyword did not fire: {fired:?}"
    );
    assert!(
        !fired.iter().any(|k| k.eq_ignore_ascii_case("LIGHT UP")),
        "an armed keyword that was never spoken fired: {fired:?}"
    );
    // And the form the worker matches on is the form the engine reports.
    for keyword in &fired {
        assert_eq!(
            keyword.trim(),
            crate::ambient_voice::wake_word::engine_keyword(keyword)
        );
    }
}

#[test]
#[ignore = "needs the downloaded KWS model; set BUZZ_AMBIENT_KWS_MODEL_DIR"]
fn no_wake_words_arms_nothing() {
    // M0 finding 5: runtime keywords MERGE with the configured set, so "no
    // wake words" has to be representable. A single newline is the proven-safe
    // payload — this asserts it arms nothing rather than everything.
    let model_dir = model_dir_from_env();
    let spotter = create_keyword_spotter(&model_dir, "\n").expect("create spotter");
    let wave = sherpa_onnx::Wave::read(&model_dir.join("test_wavs/1.wav").to_string_lossy())
        .expect("read wav fixture");
    let stream = spotter.create_stream();
    let mut fired = Vec::new();
    for chunk in wave.samples().chunks(VAD_FRAME_SAMPLES) {
        stream.accept_waveform(16_000, chunk);
        fired.extend(drain_detections(&spotter, &stream));
    }
    assert!(fired.is_empty(), "an empty keyword set fired: {fired:?}");
}

#[test]
#[ignore = "needs the downloaded KWS model; set BUZZ_AMBIENT_KWS_MODEL_DIR"]
fn forty_eight_kilohertz_worklet_audio_reaches_the_engine_intact() {
    // The worklet pushes 48 kHz. Upsample the 16 kHz fixture to 48 kHz, push it
    // through the production Resampler, and require the same detection — this
    // is the only check that the capture-rate path does not silently destroy
    // the wake word.
    let model_dir = model_dir_from_env();
    let wave = sherpa_onnx::Wave::read(&model_dir.join("test_wavs/1.wav").to_string_lossy())
        .expect("read wav fixture");
    let upsampled: Vec<f32> = wave
        .samples()
        .iter()
        .flat_map(|s| std::iter::repeat_n(*s, 3))
        .collect();

    let tokenizer = WakeWordTokenizer::load(&model_dir).expect("load tokenizer");
    let keywords_buf = tokenizer
        .keywords_buf(&["lovely child".to_string()])
        .expect("keywords buf");
    let spotter = create_keyword_spotter(&model_dir, &keywords_buf).expect("create spotter");
    let stream = spotter.create_stream();

    let mut resampler = Resampler::new(48_000).expect("resampler");
    let mut fired = Vec::new();
    for batch in upsampled.chunks(4_800) {
        for chunk in resampler.push(batch) {
            stream.accept_waveform(16_000, &chunk);
            fired.extend(drain_detections(&spotter, &stream));
        }
    }
    assert!(
        fired.iter().any(|k| k.eq_ignore_ascii_case("LOVELY CHILD")),
        "48 kHz path lost the wake word: {fired:?}"
    );
}
