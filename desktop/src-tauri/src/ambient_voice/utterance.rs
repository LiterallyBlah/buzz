//! The ambient utterance state machine.
//!
//! One wake word admits exactly one utterance. Between utterances the machine
//! is `Idle` and every microphone frame is dropped on the floor without ever
//! reaching the transcriber — that is what makes an always-open microphone
//! defensible, and it is why this is a state machine rather than a flag.
//!
//! ```text
//!            wake word fires
//!   Idle ──────────────────────► Armed ──── speech starts ───► Capturing
//!    ▲                             │                               │
//!    │                             │ nothing said in ARM_TIMEOUT   │ the silence
//!    │                             ▼                               │ hold or the cap
//!    └───────────── Drop ──────────┴───────────── Decode ──────────┘
//! ```
//!
//! ## Barge-in and echo gating are both honoured here
//!
//! The keyword spotter (in `session.rs`) is fed **every** frame, including
//! while the app's own text-to-speech is playing — that is precisely what
//! makes the barge-in acceptance criterion reachable. This machine covers the
//! *other* stage: capture/transcription. While `tts_active` is set, or during
//! the short cooldown after it clears, frames are dropped and the arm window
//! is held open, so the app never transcribes its own speech and the user's
//! post-barge-in sentence is not eaten by the tail of the interrupted reply.
//! The two requirements coexist because they apply to two different consumers
//! of the same PCM stream.
//!
//! Constants deliberately match `huddle::stt`, which is tuned against real
//! huddle audio; diverging would mean re-tuning from scratch. The one
//! exception is how long a pause is allowed to last, which is the user's to
//! choose ([`UtteranceTiming`]) because it is the difference between "finish
//! my sentence for me" and "let me think mid-sentence".

use std::time::{Duration, Instant};

/// earshot requires exactly 256 samples per frame at 16 kHz (16 ms).
pub const VAD_FRAME_SAMPLES: usize = 256;

/// Milliseconds of audio in one VAD frame.
const FRAME_MS: u32 = 1_000 * VAD_FRAME_SAMPLES as u32 / 16_000;

/// Samples of 16 kHz audio in one millisecond.
const SAMPLES_PER_MS: usize = 16;

/// Shortest silence hold the settings slider offers.
///
/// The value this feature shipped with, and `huddle::stt`'s own tuning: quick
/// enough to feel immediate, short enough to cut someone off mid-thought.
pub const MIN_SILENCE_HOLD_MS: u32 = 300;

/// Longest silence hold the settings slider offers.
pub const MAX_SILENCE_HOLD_MS: u32 = 10_000;

/// The hold an install with nothing stored gets.
///
/// Longer than the shipped 300 ms because dogfood kept losing the second half
/// of a sentence to an ordinary breath, and short enough that a finished
/// sentence still reaches the agent without a wait anyone would notice.
pub const DEFAULT_SILENCE_HOLD_MS: u32 = 800;

/// Base ceiling on one utterance, before the hold is accounted for.
const BASE_CAP_MS: u32 = 30_000;

/// How many silence holds of headroom the cap carries on top of that base.
///
/// The cap exists so a stuck VAD cannot grow the buffer without bound, but a
/// long hold spends real time *inside* an utterance waiting for pauses to end.
/// Leaving the ceiling fixed would mean the pauses the user asked for became
/// the thing that truncated them.
const CAP_HOLDS: u32 = 20;

/// Minimum voiced frames before an utterance may be transcribed (~192 ms).
/// Below this it is room noise, and transcribing it invites hallucinated text.
pub const MIN_VOICED_FRAMES: usize = 12;

/// How long after TTS stops before the microphone is trusted again.
pub const TTS_COOLDOWN: Duration = Duration::from_millis(150);

/// How long a wake word keeps the capture stage armed with nothing said.
///
/// Long enough to survive a false start ("hey hermes… um…"), short enough that
/// a false-positive wake word cannot leave the transcriber armed indefinitely.
pub const WAKE_ARM_TIMEOUT: Duration = Duration::from_secs(6);

/// The two limits a session's silence hold decides.
///
/// Derived once, when the session is built, rather than recomputed per frame —
/// and clamped here as well as in `settings`, so a machine constructed from a
/// value that never went through the settings door is still safe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UtteranceTiming {
    silence_flush_frames: usize,
    max_speech_samples: usize,
}

impl Default for UtteranceTiming {
    fn default() -> Self {
        Self::from_silence_hold_ms(DEFAULT_SILENCE_HOLD_MS)
    }
}

impl UtteranceTiming {
    /// Derive both limits from the persisted hold.
    pub fn from_silence_hold_ms(silence_hold_ms: u32) -> Self {
        let hold_ms = silence_hold_ms.clamp(MIN_SILENCE_HOLD_MS, MAX_SILENCE_HOLD_MS);
        let cap_ms = BASE_CAP_MS + CAP_HOLDS * hold_ms;
        Self {
            // Rounded up: quantising to whole frames must never make the hold
            // shorter than the user asked for.
            silence_flush_frames: hold_ms.div_ceil(FRAME_MS) as usize,
            max_speech_samples: cap_ms as usize * SAMPLES_PER_MS,
        }
    }

    /// Consecutive silent frames that end an utterance. Test-facing: the
    /// machine reads the field, and the worker reads the [`FrameOutcome`].
    #[cfg(test)]
    pub fn silence_flush_frames(&self) -> usize {
        self.silence_flush_frames
    }

    /// Hard ceiling on one utterance, in 16 kHz samples. Test-facing for the
    /// same reason.
    #[cfg(test)]
    pub fn max_speech_samples(&self) -> usize {
        self.max_speech_samples
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UtterancePhase {
    /// Nothing is captured. The default, and where the machine spends almost
    /// all of its time.
    Idle,
    /// A wake word fired; waiting for the user to start speaking.
    Armed,
    /// Accumulating an utterance.
    Capturing,
}

/// What the caller must do with the frame it just supplied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameOutcome {
    /// Discard the frame; the buffer is already empty.
    Idle,
    /// Append the frame to the speech buffer.
    Buffer,
    /// Append the frame, then transcribe and clear the buffer.
    Decode,
    /// Clear the buffer without transcribing.
    Drop,
}

#[derive(Debug)]
pub struct UtteranceMachine {
    /// The limits this session was built with. Fixed for the machine's life —
    /// a hold changed in settings reaches the audio path through
    /// `super::reconcile`, which builds a new session.
    timing: UtteranceTiming,
    phase: UtterancePhase,
    /// When the arm window started. Refreshed while the microphone is gated so
    /// TTS playback does not consume the user's speaking window.
    armed_at: Option<Instant>,
    silence_frames: usize,
    voiced_frames: usize,
    buffered_samples: usize,
    tts_was_active: bool,
    tts_stopped_at: Option<Instant>,
}

impl Default for UtteranceMachine {
    fn default() -> Self {
        Self::new(UtteranceTiming::default())
    }
}

impl UtteranceMachine {
    pub fn new(timing: UtteranceTiming) -> Self {
        Self {
            timing,
            phase: UtterancePhase::Idle,
            armed_at: None,
            silence_frames: 0,
            voiced_frames: 0,
            buffered_samples: 0,
            tts_was_active: false,
            tts_stopped_at: None,
        }
    }

    pub fn phase(&self) -> UtterancePhase {
        self.phase
    }

    /// The limits this machine is running with. Test-facing: production reads
    /// them through the outcomes, never directly.
    #[cfg(test)]
    pub fn timing(&self) -> UtteranceTiming {
        self.timing
    }

    /// Voiced frames accumulated in the current utterance. Test-facing: the
    /// worker reads the [`FrameOutcome`], not the counter.
    #[cfg(test)]
    pub fn voiced_frames(&self) -> usize {
        self.voiced_frames
    }

    /// A wake word fired. Arms the capture stage and abandons any partial
    /// utterance — a second wake word is a restart, not a continuation.
    ///
    /// Returns `true` when a buffered partial utterance was abandoned, so the
    /// caller can clear its buffer.
    pub fn on_wake(&mut self, now: Instant) -> bool {
        let had_buffer = self.buffered_samples > 0;
        self.phase = UtterancePhase::Armed;
        self.armed_at = Some(now);
        self.silence_frames = 0;
        self.voiced_frames = 0;
        self.buffered_samples = 0;
        had_buffer
    }

    /// Abandon whatever is in flight and return to `Idle`.
    ///
    /// Used on mute, huddle suspension, and shutdown, where a half-captured
    /// sentence must never be transcribed later.
    pub fn reset(&mut self) {
        self.phase = UtterancePhase::Idle;
        self.armed_at = None;
        self.silence_frames = 0;
        self.voiced_frames = 0;
        self.buffered_samples = 0;
    }

    /// Feed one VAD-classified frame.
    ///
    /// `is_speech` is the VAD verdict, `tts_active` the shared playback flag,
    /// `now` the caller's clock (injected so tests do not sleep).
    pub fn on_frame(&mut self, is_speech: bool, tts_active: bool, now: Instant) -> FrameOutcome {
        // Edge-trigger the cooldown timer exactly as `huddle::stt` does.
        if self.tts_was_active && !tts_active {
            self.tts_stopped_at = Some(now);
        }
        self.tts_was_active = tts_active;

        if self.phase == UtterancePhase::Idle {
            return FrameOutcome::Idle;
        }

        // Microphone gating: while the app is speaking, or for a moment after,
        // the microphone is carrying our own audio. Drop it and hold the arm
        // window open so a barge-in follow-up still has its full time to start.
        let in_cooldown = self
            .tts_stopped_at
            .is_some_and(|stopped| now.duration_since(stopped) < TTS_COOLDOWN);
        if tts_active || in_cooldown {
            let had_buffer = self.buffered_samples > 0;
            self.phase = UtterancePhase::Armed;
            self.armed_at = Some(now);
            self.silence_frames = 0;
            self.voiced_frames = 0;
            self.buffered_samples = 0;
            return if had_buffer {
                FrameOutcome::Drop
            } else {
                FrameOutcome::Idle
            };
        }
        if self.tts_stopped_at.is_some() {
            self.tts_stopped_at = None;
        }

        match self.phase {
            UtterancePhase::Idle => FrameOutcome::Idle,
            UtterancePhase::Armed => {
                if is_speech {
                    self.phase = UtterancePhase::Capturing;
                    self.voiced_frames = 1;
                    self.silence_frames = 0;
                    self.buffered_samples = VAD_FRAME_SAMPLES;
                    return FrameOutcome::Buffer;
                }
                let expired = self
                    .armed_at
                    .is_some_and(|armed| now.duration_since(armed) >= WAKE_ARM_TIMEOUT);
                if expired {
                    self.reset();
                    return FrameOutcome::Drop;
                }
                FrameOutcome::Idle
            }
            UtterancePhase::Capturing => {
                self.buffered_samples += VAD_FRAME_SAMPLES;
                if is_speech {
                    self.voiced_frames += 1;
                    self.silence_frames = 0;
                } else {
                    self.silence_frames += 1;
                }

                let ended = self.silence_frames >= self.timing.silence_flush_frames;
                let capped = self.buffered_samples >= self.timing.max_speech_samples;
                if !ended && !capped {
                    return FrameOutcome::Buffer;
                }

                let enough_voice = self.voiced_frames >= MIN_VOICED_FRAMES;
                self.reset();
                if enough_voice {
                    FrameOutcome::Decode
                } else {
                    FrameOutcome::Drop
                }
            }
        }
    }
}

#[cfg(test)]
#[path = "utterance_tests.rs"]
mod utterance_tests;
