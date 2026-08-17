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
//!    │                             │ nothing said in ARM_TIMEOUT   │ 300 ms silence
//!    │                             ▼                               │ or 30 s cap
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
//! huddle audio; diverging would mean re-tuning from scratch.

use std::time::{Duration, Instant};

/// earshot requires exactly 256 samples per frame at 16 kHz (16 ms).
pub const VAD_FRAME_SAMPLES: usize = 256;

/// 300 ms of silence ends an utterance (19 frames × 16 ms).
pub const SILENCE_FLUSH_FRAMES: usize = 19;

/// Minimum voiced frames before an utterance may be transcribed (~192 ms).
/// Below this it is room noise, and transcribing it invites hallucinated text.
pub const MIN_VOICED_FRAMES: usize = 12;

/// 30 seconds at 16 kHz — hard cap so a stuck VAD cannot grow the buffer.
pub const MAX_SPEECH_SAMPLES: usize = 16_000 * 30;

/// How long after TTS stops before the microphone is trusted again.
pub const TTS_COOLDOWN: Duration = Duration::from_millis(150);

/// How long a wake word keeps the capture stage armed with nothing said.
///
/// Long enough to survive a false start ("hey hermes… um…"), short enough that
/// a false-positive wake word cannot leave the transcriber armed indefinitely.
pub const WAKE_ARM_TIMEOUT: Duration = Duration::from_secs(6);

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
        Self::new()
    }
}

impl UtteranceMachine {
    pub fn new() -> Self {
        Self {
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

                let ended = self.silence_frames >= SILENCE_FLUSH_FRAMES;
                let capped = self.buffered_samples >= MAX_SPEECH_SAMPLES;
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
