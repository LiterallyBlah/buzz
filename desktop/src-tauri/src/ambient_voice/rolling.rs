//! Rolling capture: one utterance, however many chunks it takes.
//!
//! The capture buffer has a ceiling ([`super::utterance`]), and it used to end
//! the utterance: at 30 s plus twenty silence holds the capture closed, the
//! chunk was transcribed and sent, and the machine went back to needing the wake
//! word. Someone dictating a long conversation lost everything after that
//! moment with nothing on screen to say so, and the stop phrase they eventually
//! said closed nothing at all.
//!
//! So the ceiling closes a *chunk* now. This is where the chunk goes:
//!
//! ```text
//!   worker thread                     ambient-voice-transcriber thread
//!   ─────────────                     ────────────────────────────────
//!   ceiling → hand_off(chunk) ──jobs──►  decode(chunk)
//!   …keeps draining audio…              │
//!   close  → finish(last chunk) ──────► decode(last chunk)
//!            ◄──────────────results──── in the order they were sent
//!            stitch → one transcript
//! ```
//!
//! ## Why a thread rather than a longer call
//!
//! The worker is a single loop and its audio queue is 50 batches deep with a
//! non-blocking push, so audio that arrives while the worker is inside a
//! transcription is dropped on the floor once the queue fills — about five
//! seconds' worth, against a network round trip that `super::speech_http` gives
//! ten seconds or more. A capture that "continued" while the worker blocked
//! would be a capture with a hole in it. The chunk therefore leaves the worker
//! thread entirely, and the worker returns to the queue in the same breath.
//!
//! ## What is bounded, and what happens when the bound is reached
//!
//! Two things, one per direction, and both fail the utterance loudly on the
//! indicator rather than quietly shedding part of it — a message stitched from
//! whatever happened to survive would be the user's words with a hole in the
//! middle and nothing to show it.
//!
//! **Audio going out** is capped at [`MAX_PENDING_CHUNKS`] chunks held by the
//! transcription thread at once, each at most a ceiling's worth of PCM, so a
//! transcriber that has stopped answering holds a bounded amount and not a
//! growing one. A transcriber that keeps up is never near it.
//!
//! **Text coming back** is collected into the worker as each chunk completes
//! (every [`RollingCapture::hand_off`] drains what is ready, so nothing
//! accumulates in the channel between them) and its total is capped at
//! [`MAX_UTTERANCE_TEXT_BYTES`] — the most one published message may carry.
//! Real speech cannot reach it: an hour of talking is under the bound. What
//! can is a pathological transcription server answering close to a mebibyte
//! per chunk, and without this bound a long capture against one would grow
//! process memory without end. Over the bound, nothing further is queued and
//! the close fails with [`PAST_ONE_MESSAGE`].
//!
//! Ordering is the channels': one thread takes jobs in order and answers in
//! order, so the stitched transcript is in the order the user spoke.

use std::sync::{
    atomic::{AtomicUsize, Ordering},
    mpsc::{self, Receiver, SyncSender},
    Arc,
};
use std::thread;

use super::publish::MAX_UTTERANCE_TEXT_BYTES;

/// Chunks of audio the transcription thread may hold at once, at most.
///
/// The bound is on **PCM**, so a chunk stops counting when it has been decoded
/// and its samples dropped — not when its text is collected, which does not
/// happen until the capture ends. Counting it to the end of the capture would
/// put a four-chunk ceiling back on an utterance, which is the ceiling this
/// whole module exists to remove.
///
/// Four is room for a transcriber slower than real time without being room for
/// one that has stopped: at the default hold a chunk is 46 s of 16 kHz mono f32
/// (~2.9 MB), and at the longest hold it is 230 s (~14.7 MB). The pipeline
/// therefore holds tens of megabytes at worst rather than as much as the user
/// can say.
const MAX_PENDING_CHUNKS: usize = 4;

/// What the indicator says when the bound above is reached.
const TOO_FAR_BEHIND: &str = "Speech-to-text fell too far behind to finish this message";

/// What it says when the transcription thread is gone.
const TRANSCRIBER_GONE: &str = "Speech-to-text stopped before this message was finished";

/// What it says when the collected text outgrows what one message may carry.
///
/// [`MAX_UTTERANCE_TEXT_BYTES`] of transcript — which real speech does not
/// produce in one utterance; only a server answering absurd volumes of text
/// per chunk does, and this is both the memory bound against such a server and
/// the honest alternative to trimming the user's words at publication.
const PAST_ONE_MESSAGE: &str = "This message grew longer than one message can carry";

/// One closed chunk on its way to be transcribed.
struct ChunkJob {
    /// Which utterance it belongs to. A chunk whose utterance was abandoned is
    /// still decoded — the thread is already inside it, or it is queued behind
    /// one that is — and this is what keeps its text out of the next one.
    generation: u64,
    samples: Vec<f32>,
}

/// What came back for one chunk.
struct ChunkDone {
    generation: u64,
    text: Result<String, String>,
}

/// The transcription thread, plus the chunks of the utterance in progress.
///
/// Owned by the audio worker and never shared: every method here is called from
/// that one thread, which is what makes `owed` an ordinary counter. `in_flight`
/// is the exception, because it is the transcription thread that says when a
/// chunk's audio is gone.
pub(crate) struct RollingCapture {
    /// `None` only while dropping, where closing it is what tells the thread to
    /// finish.
    jobs: Option<SyncSender<ChunkJob>>,
    results: Receiver<ChunkDone>,
    /// The utterance being captured. Bumped by [`RollingCapture::abort`], which
    /// is how results already in flight are told apart from this utterance's.
    generation: u64,
    /// Chunks of PCM the transcription thread is still holding, whether queued
    /// or being decoded. Shared with that thread, which drops the count as it
    /// drops the audio. This is the bound.
    in_flight: Arc<AtomicUsize>,
    /// Results this utterance is still owed. Unlike `in_flight` this counts to
    /// the end of the capture, because the transcripts are what gets stitched.
    owed: usize,
    /// Transcripts of this utterance's chunks, in the order they were spoken.
    /// Moved here from the results channel as each completes ([`Self::absorb`])
    /// so the channel never accumulates a capture's worth of text.
    collected: Vec<String>,
    /// Total bytes in `collected`. Over [`MAX_UTTERANCE_TEXT_BYTES`] the
    /// utterance has failed ([`PAST_ONE_MESSAGE`]) and nothing further is
    /// queued, which is what makes this a memory bound and not a statistic.
    collected_bytes: usize,
    /// Set when this utterance can no longer be sent — a chunk that failed to
    /// decode, one that could not be handed off, or text past the bound above.
    /// The close fails with this text; until then nothing more is queued.
    failed: Option<String>,
    thread: Option<thread::JoinHandle<()>>,
}

impl RollingCapture {
    /// Start the transcription thread.
    ///
    /// `decode` is the session's transcriber, moved onto that thread because it
    /// is where every call to it now happens. Taking a closure rather than a
    /// [`super::transcriber::Transcriber`] is what lets the pipeline be tested
    /// against a stub without an ONNX model or a server.
    pub(crate) fn spawn(
        decode: impl Fn(&[f32]) -> Result<String, String> + Send + 'static,
    ) -> Result<Self, String> {
        let (jobs_tx, jobs_rx) = mpsc::sync_channel::<ChunkJob>(MAX_PENDING_CHUNKS);
        let (results_tx, results_rx) = mpsc::channel::<ChunkDone>();
        let in_flight = Arc::new(AtomicUsize::new(0));
        let held = Arc::clone(&in_flight);
        let thread = thread::Builder::new()
            .name("ambient-voice-transcriber".into())
            .spawn(move || {
                while let Ok(job) = jobs_rx.recv() {
                    let ChunkJob {
                        generation,
                        samples,
                    } = job;
                    let text = decode(&samples);
                    // The audio is what the bound counts, so it is released
                    // before the count is — a chunk that has been decoded is
                    // room for the next one whether or not the capture has
                    // ended.
                    drop(samples);
                    let sent = results_tx.send(ChunkDone { generation, text });
                    held.fetch_sub(1, Ordering::Release);
                    if sent.is_err() {
                        break;
                    }
                }
            })
            .map_err(|error| {
                format!("failed to spawn ambient-voice-transcriber thread: {error}")
            })?;

        Ok(Self {
            jobs: Some(jobs_tx),
            results: results_rx,
            generation: 0,
            in_flight,
            owed: 0,
            collected: Vec::new(),
            collected_bytes: 0,
            failed: None,
            thread: Some(thread),
        })
    }

    /// The ceiling closed a chunk. Take it and return at once.
    ///
    /// Never blocks and never waits on the transcriber: the caller is the audio
    /// worker, and every millisecond it spends here is a millisecond its queue
    /// is not being drained. Finished results are collected on the way through,
    /// which is what keeps the channel from holding a capture's worth of text.
    pub(crate) fn hand_off(&mut self, samples: Vec<f32>) {
        self.drain_ready();
        if self.failed.is_some() {
            // This utterance is already going to fail at the close. Decoding
            // more of it would spend the bound on audio nobody will read.
            return;
        }
        if let Err(error) = self.send(samples) {
            // Recorded rather than raised: nothing is submitted until the close,
            // and this is what makes the close say so.
            self.failed = Some(error);
        }
    }

    /// The capture ended. Transcribe the last chunk, stitch, and say what to
    /// send — `None` when the utterance carried no words.
    ///
    /// Blocks until every chunk of this utterance has come back, which is what
    /// makes the ordinary one-chunk utterance behave exactly as it always has:
    /// the worker sets `Transcribing`, waits here, and publishes once.
    ///
    /// `trim` is the stop phrase when one ended the capture. It is applied to
    /// the stitched transcript rather than to the last chunk, so a phrase the
    /// ceiling happened to split across two chunks is still removed.
    pub(crate) fn finish(
        &mut self,
        samples: Vec<f32>,
        trim: Option<&str>,
    ) -> Result<Option<String>, String> {
        self.drain_ready();
        if let Some(error) = self.failed.take() {
            self.abort();
            return Err(error);
        }
        if let Err(error) = self.send(samples) {
            self.abort();
            return Err(error);
        }
        if let Err(error) = self.collect_remaining() {
            // Whatever is still in flight belongs to an utterance that is not
            // going to be sent, and must not be stitched into the next one.
            self.abort();
            return Err(error);
        }
        let parts = std::mem::take(&mut self.collected);
        self.collected_bytes = 0;
        let text = stitch(&parts);
        let text = match trim {
            Some(phrase) => strip_trailing_phrase(&text, phrase),
            None => text,
        };
        if text.len() > MAX_UTTERANCE_TEXT_BYTES {
            // The final gate, on the exact bytes that would be published. The
            // per-chunk accounting in `absorb` cannot see the spaces `stitch`
            // puts between chunks, so a capture landing within a few bytes of
            // the bound can stitch to just over it — and over the bound must
            // fail here, on the indicator, not one step later in the publisher
            // where the refusal is a log line the user never sees.
            return Err(PAST_ONE_MESSAGE.to_string());
        }
        Ok(if text.is_empty() { None } else { Some(text) })
    }

    /// Abandon this utterance: nothing of it is ever submitted.
    ///
    /// Mute, a huddle, playback starting and a second wake word all land here,
    /// and none of them may block the worker — so chunks still in flight are not
    /// waited for. They are disowned instead: the results come back against a
    /// generation nobody is collecting for and are discarded when the next
    /// utterance reads the channel. Their audio still counts against the bound
    /// until the thread is done with it, which is the honest accounting: the
    /// memory is held whether or not anyone still wants the words.
    pub(crate) fn abort(&mut self) {
        self.generation += 1;
        self.owed = 0;
        self.collected.clear();
        self.collected_bytes = 0;
        self.failed = None;
    }

    /// Hand one chunk to the transcription thread, honouring the bound.
    fn send(&mut self, samples: Vec<f32>) -> Result<(), String> {
        if samples.is_empty() {
            return Ok(());
        }
        if self.in_flight.load(Ordering::Acquire) >= MAX_PENDING_CHUNKS {
            return Err(TOO_FAR_BEHIND.to_string());
        }
        let job = ChunkJob {
            generation: self.generation,
            samples,
        };
        let Some(jobs) = self.jobs.as_ref() else {
            return Err(TRANSCRIBER_GONE.to_string());
        };
        // Counted before the send, so the thread can only ever decrement a
        // count that is already there.
        self.in_flight.fetch_add(1, Ordering::AcqRel);
        match jobs.try_send(job) {
            Ok(()) => {}
            Err(error) => {
                self.in_flight.fetch_sub(1, Ordering::AcqRel);
                // Full is unreachable while the bound above is the tighter of
                // the two, and is kept because which one is tighter is not this
                // call's business.
                return Err(match error {
                    mpsc::TrySendError::Full(_) => TOO_FAR_BEHIND.to_string(),
                    mpsc::TrySendError::Disconnected(_) => TRANSCRIBER_GONE.to_string(),
                });
            }
        }
        self.owed += 1;
        Ok(())
    }

    /// Take every result the transcription thread has already finished,
    /// without waiting for the ones it has not.
    fn drain_ready(&mut self) {
        while let Ok(done) = self.results.try_recv() {
            self.absorb(done);
        }
    }

    /// Wait for every chunk of this utterance still in flight.
    ///
    /// One chunk that failed fails the utterance, and the rest are still
    /// collected before it does: a result left in the channel would be
    /// stitched into whatever the user says next.
    fn collect_remaining(&mut self) -> Result<(), String> {
        while self.owed > 0 {
            let Ok(done) = self.results.recv() else {
                // The thread died. Nothing further is coming, so waiting for the
                // rest would be waiting forever.
                self.owed = 0;
                return Err(TRANSCRIBER_GONE.to_string());
            };
            self.absorb(done);
        }
        match self.failed.take() {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    /// File one finished chunk where it belongs: this utterance's transcript,
    /// its failure, or — for an abandoned utterance's chunk — nowhere.
    fn absorb(&mut self, done: ChunkDone) {
        if done.generation != self.generation {
            // An abandoned utterance's chunk, finally decoded and discarded.
            return;
        }
        self.owed = self.owed.saturating_sub(1);
        match done.text {
            Ok(text) => {
                self.collected_bytes += text.len();
                self.collected.push(text);
                if self.collected_bytes > MAX_UTTERANCE_TEXT_BYTES {
                    // The bound on collected text, enforced where the text
                    // arrives. `hand_off` stops queueing the moment this is
                    // set, so what is held stays within one chunk of the
                    // bound rather than growing with the capture.
                    self.failed
                        .get_or_insert_with(|| PAST_ONE_MESSAGE.to_string());
                }
            }
            Err(error) => {
                self.failed.get_or_insert(error);
            }
        }
    }

    /// Bytes of transcript currently held for the utterance in progress.
    /// Test-facing: the memory-bound regression asserts on it.
    #[cfg(test)]
    pub(crate) fn resident_text_bytes(&self) -> usize {
        self.collected_bytes
    }
}

impl Drop for RollingCapture {
    /// Closing the job channel is what ends the thread, so it is closed before
    /// the join rather than whenever the struct's fields happen to drop.
    ///
    /// The join can wait for one chunk that is already being decoded — the same
    /// wait a session teardown has always had, when the worker itself was the
    /// thing inside the recogniser.
    fn drop(&mut self) {
        self.jobs.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Chunk transcripts, back into one utterance.
///
/// A single space between them: the chunks are consecutive speech, and the
/// boundary is where the buffer filled up rather than anywhere the user paused.
/// A chunk that carried no words contributes nothing — a stretch of silence
/// inside a long dictation is ordinary, and neither an extra space nor a reason
/// to fail.
fn stitch(parts: &[String]) -> String {
    parts
        .iter()
        .map(|part| part.trim())
        .filter(|part| !part.is_empty())
        .collect::<Vec<&str>>()
        .join(" ")
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
pub(crate) fn strip_trailing_phrase(text: &str, phrase: &str) -> String {
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

#[cfg(test)]
#[path = "rolling_tests.rs"]
mod rolling_tests;
