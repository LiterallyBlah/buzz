//! Speaking an agent's reply through a speech server.
//!
//! The local pipeline (`huddle::tts`) is a large machine because it does the
//! synthesis itself: a warmed ONNX engine, sentence chunking, cross-item
//! lookahead. This one has none of that to do — it posts text and gets audio
//! back — so it is a thread, a queue and a player, and everything it shares
//! with the local pipeline is the part the rest of the feature depends on:
//!
//! * `tts_active` is true from the moment audio is queued until it has
//!   drained. That is the flag the utterance machine gates capture on, so the
//!   agent's own voice is not transcribed as the user's next sentence.
//! * `tts_cancel` is barge-in. When the wake word fires mid-reply the audio
//!   worker sets it, and playback stops within one monitor tick and drops
//!   whatever else was queued — the user interrupted, and finishing the old
//!   answer first would be the opposite of what they asked for.
//!
//! A server that will not speak is non-fatal, exactly as a missing local TTS
//! model is: the transcript path is what carries the conversation, and losing
//! the spoken half is worth logging, not worth stopping the session for.

use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender},
        Arc,
    },
    thread,
    time::Duration,
};

use super::speech_http::{self, SpeechEndpoint};
use super::speech_wav::{self, DecodedAudio};

/// Queued replies. Small: this is one agent talking to one person, and a
/// backlog longer than a few sentences means the conversation has moved on.
const TEXT_QUEUE_DEPTH: usize = 8;

/// How long the worker waits on the queue before re-checking shutdown.
const RECV_TIMEOUT: Duration = Duration::from_millis(100);

/// Barge-in poll interval while audio is draining. Matches the local
/// pipeline's monitor tick, so the flag-to-silence latency is the same
/// whichever backend is speaking.
const MONITOR_TICK: Duration = Duration::from_millis(10);

/// Where synthesised audio goes.
///
/// A trait with one shipping implementation, so the worker's `tts_active` and
/// barge-in handling are testable without an audio device — CI has none, and
/// those two behaviours are the whole reason this pipeline has to match the
/// local one.
pub(crate) trait SpeechPlayer: Send {
    /// Queue decoded audio for playback.
    fn play(&self, audio: DecodedAudio) -> Result<(), String>;
    /// Whether anything is still queued or sounding.
    fn is_playing(&self) -> bool;
    /// Drop everything queued and go quiet now.
    fn stop(&self);
}

/// Builds the player on the worker thread.
///
/// The audio device is opened where it is used: rodio's device handle is not
/// something to hand across threads, and opening it here is also what lets
/// `HttpTtsPipeline::new` report a device failure to a caller that treats it
/// as non-fatal.
type PlayerFactory = Box<dyn FnOnce() -> Result<Box<dyn SpeechPlayer>, String> + Send>;

/// A rodio player on the ambient session's chosen output device.
struct RodioSpeechPlayer {
    // Declaration order is drop order: the player must go before the device
    // sink it was connected to.
    player: rodio::Player,
    _device: rodio::MixerDeviceSink,
}

impl RodioSpeechPlayer {
    fn open(output_device: Option<String>) -> Result<Box<dyn SpeechPlayer>, String> {
        let device =
            crate::huddle::audio_output::open_output_sink_by_name(output_device.as_deref())
                .map_err(|error| format!("audio output could not be opened: {error}"))?;
        let player = rodio::Player::connect_new(device.mixer());
        Ok(Box::new(Self {
            player,
            _device: device,
        }))
    }
}

impl SpeechPlayer for RodioSpeechPlayer {
    fn play(&self, audio: DecodedAudio) -> Result<(), String> {
        let channels = std::num::NonZero::new(audio.channels)
            .ok_or("speech audio claims zero channels".to_string())?;
        let rate = std::num::NonZero::new(audio.sample_rate)
            .ok_or("speech audio claims a zero sample rate".to_string())?;
        self.player.append(rodio::buffer::SamplesBuffer::new(
            channels,
            rate,
            audio.samples,
        ));
        Ok(())
    }

    fn is_playing(&self) -> bool {
        !self.player.empty()
    }

    fn stop(&self) {
        // `clear()` also pauses (rodio 0.22), and this player outlives the
        // utterance it is silencing — without the `play()` every later append
        // would queue silently forever.
        self.player.clear();
        self.player.play();
    }
}

/// A speech server standing in for the local TTS pipeline.
#[derive(Debug)]
pub struct HttpTtsPipeline {
    text_tx: SyncSender<String>,
    shutdown: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl HttpTtsPipeline {
    /// Build the pipeline and wait for its worker to open the audio device.
    ///
    /// Waiting matters: `start_ambient_tts` treats an unavailable pipeline as
    /// non-fatal and goes on without speech, and it can only do that if a
    /// device that will not open is an error here rather than a silence
    /// discovered on the first reply.
    pub(crate) fn new(
        endpoint: SpeechEndpoint,
        output_device: Option<String>,
        tts_active: Arc<AtomicBool>,
        tts_cancel: Arc<AtomicBool>,
    ) -> Result<Self, String> {
        Self::with_player(
            endpoint,
            Box::new(move || RodioSpeechPlayer::open(output_device)),
            tts_active,
            tts_cancel,
        )
    }

    /// The same pipeline with the audio device replaced.
    ///
    /// Visible to the rest of the feature, not just to this file's tests, so
    /// `tts_backend`'s tests can drive a real `AmbientTts::Http` against a
    /// loopback server without a sound card — the alternative is asserting
    /// that the door flattens by reading the door.
    pub(super) fn with_player(
        endpoint: SpeechEndpoint,
        player_factory: PlayerFactory,
        tts_active: Arc<AtomicBool>,
        tts_cancel: Arc<AtomicBool>,
    ) -> Result<Self, String> {
        let (text_tx, text_rx) = mpsc::sync_channel::<String>(TEXT_QUEUE_DEPTH);
        let (startup_tx, startup_rx) = mpsc::sync_channel::<Result<(), String>>(1);
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_worker = Arc::clone(&shutdown);

        let thread = thread::Builder::new()
            .name("ambient-http-tts".into())
            .spawn(move || {
                http_tts_worker(
                    endpoint,
                    text_rx,
                    player_factory,
                    (tts_active, tts_cancel, shutdown_worker),
                    startup_tx,
                )
            })
            .map_err(|error| format!("failed to spawn ambient-http-tts thread: {error}"))?;

        match startup_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                text_tx,
                shutdown,
                thread: Some(thread),
            }),
            Ok(Err(error)) => {
                let _ = thread.join();
                Err(error)
            }
            // The worker died without reporting, which is a bug rather than a
            // configuration problem — say so instead of hanging.
            Err(error) => {
                let _ = thread.join();
                Err(format!("ambient speech worker exited at startup: {error}"))
            }
        }
    }

    /// Queue a reply to be spoken. Non-blocking.
    pub fn speak(&self, text: String) -> Result<(), String> {
        self.text_tx.try_send(text).map_err(|error| {
            eprintln!("buzz-desktop: ambient speech queue saturated, dropping a reply: {error}");
            format!("speech queue full, dropping: {error}")
        })
    }

    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Release);
    }
}

impl Drop for HttpTtsPipeline {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The three flags the worker shares with the rest of the session.
type WorkerFlags = (Arc<AtomicBool>, Arc<AtomicBool>, Arc<AtomicBool>);

fn http_tts_worker(
    endpoint: SpeechEndpoint,
    text_rx: Receiver<String>,
    player_factory: PlayerFactory,
    flags: WorkerFlags,
    startup_tx: SyncSender<Result<(), String>>,
) {
    let (tts_active, tts_cancel, shutdown) = flags;
    let player = match player_factory() {
        Ok(player) => player,
        Err(error) => {
            let _ = startup_tx.send(Err(error));
            return;
        }
    };
    let client = match speech_http::blocking_client() {
        Ok(client) => client,
        Err(error) => {
            let _ = startup_tx.send(Err(error));
            return;
        }
    };
    if startup_tx.send(Ok(())).is_err() {
        return;
    }

    loop {
        if shutdown.load(Ordering::Acquire) {
            break;
        }
        let text = match text_rx.recv_timeout(RECV_TIMEOUT) {
            Ok(text) => text,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        // A barge-in that arrived while this reply was queued: the user has
        // spoken since, so the reply is stale before it is even synthesised.
        if tts_cancel.swap(false, Ordering::AcqRel) {
            continue;
        }

        let Some(audio) = fetch_speech(&client, &endpoint, &text) else {
            continue;
        };
        if audio.samples.is_empty() {
            eprintln!("buzz-desktop: ambient speech server returned no audio for a reply");
            continue;
        }
        // Synthesis is a network round trip, and the wake word stays armed
        // throughout it. Anything the user said in that window wins.
        if tts_cancel.swap(false, Ordering::AcqRel) || shutdown.load(Ordering::Acquire) {
            continue;
        }
        if let Err(error) = player.play(audio) {
            eprintln!("buzz-desktop: ambient speech could not be played: {error}");
            continue;
        }
        // Set only after the audio is queued, so capture stays open during the
        // request itself — the same order the local pipeline uses.
        tts_active.store(true, Ordering::Release);
        if drain_playback(player.as_ref(), &tts_cancel, &shutdown) {
            // Barge-in: everything else queued belongs to the interrupted
            // answer, so it goes too.
            while text_rx.try_recv().is_ok() {}
        }
        tts_active.store(false, Ordering::Release);
    }

    player.stop();
    tts_active.store(false, Ordering::Release);
}

/// Ask the server for `text` as audio. `None` on any failure, already logged.
///
/// Both failures are non-fatal by design and for the same reason a missing
/// local TTS model is: the transcript path is what carries the conversation.
fn fetch_speech(
    client: &reqwest::blocking::Client,
    endpoint: &SpeechEndpoint,
    text: &str,
) -> Option<DecodedAudio> {
    let bytes = match speech_http::synthesize(client, endpoint, text) {
        Ok(bytes) => bytes,
        Err(error) => {
            eprintln!("buzz-desktop: ambient speech server could not speak: {error}");
            return None;
        }
    };
    match speech_wav::decode_pcm16(&bytes) {
        Ok(audio) => Some(audio),
        Err(error) => {
            eprintln!("buzz-desktop: ambient speech audio could not be decoded: {error}");
            None
        }
    }
}

/// Wait for playback to finish. Returns `true` if it was cut short by a
/// barge-in, which is what tells the caller to drop the rest of the queue.
fn drain_playback(
    player: &dyn SpeechPlayer,
    tts_cancel: &AtomicBool,
    shutdown: &AtomicBool,
) -> bool {
    while player.is_playing() {
        if shutdown.load(Ordering::Acquire) {
            player.stop();
            return false;
        }
        if tts_cancel.swap(false, Ordering::AcqRel) {
            player.stop();
            return true;
        }
        thread::sleep(MONITOR_TICK);
    }
    false
}

#[cfg(test)]
#[path = "http_tts_tests.rs"]
mod http_tts_tests;
