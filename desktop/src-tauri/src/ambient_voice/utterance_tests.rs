//! Utterance state-machine tests.
//!
//! The machine takes its clock as a parameter, so every timing rule here is
//! exercised deterministically rather than by sleeping.

use super::*;

fn t0() -> Instant {
    Instant::now()
}

/// Feed `count` frames and return the last outcome plus every outcome seen.
fn feed(
    machine: &mut UtteranceMachine,
    count: usize,
    is_speech: bool,
    tts_active: bool,
    now: Instant,
) -> Vec<FrameOutcome> {
    (0..count)
        .map(|_| machine.on_frame(is_speech, tts_active, now))
        .collect()
}

#[test]
fn idle_drops_every_frame_until_a_wake_word() {
    let mut machine = UtteranceMachine::new();
    let now = t0();
    assert_eq!(machine.phase(), UtterancePhase::Idle);
    for outcome in feed(&mut machine, 200, true, false, now) {
        assert_eq!(outcome, FrameOutcome::Idle);
    }
    assert_eq!(machine.phase(), UtterancePhase::Idle);
}

#[test]
fn a_wake_word_arms_capture() {
    let mut machine = UtteranceMachine::new();
    let now = t0();
    assert!(!machine.on_wake(now));
    assert_eq!(machine.phase(), UtterancePhase::Armed);
    assert_eq!(machine.on_frame(true, false, now), FrameOutcome::Buffer);
    assert_eq!(machine.phase(), UtterancePhase::Capturing);
}

#[test]
fn an_utterance_decodes_after_the_silence_threshold() {
    let mut machine = UtteranceMachine::new();
    let now = t0();
    machine.on_wake(now);
    for outcome in feed(&mut machine, MIN_VOICED_FRAMES, true, false, now) {
        assert_eq!(outcome, FrameOutcome::Buffer);
    }
    // Silence up to (not including) the threshold keeps buffering: brief gaps
    // inside a sentence must not split it.
    for outcome in feed(&mut machine, SILENCE_FLUSH_FRAMES - 1, false, false, now) {
        assert_eq!(outcome, FrameOutcome::Buffer);
    }
    assert_eq!(machine.on_frame(false, false, now), FrameOutcome::Decode);
    assert_eq!(machine.phase(), UtterancePhase::Idle);
}

#[test]
fn a_short_blip_is_dropped_rather_than_transcribed() {
    let mut machine = UtteranceMachine::new();
    let now = t0();
    machine.on_wake(now);
    feed(&mut machine, MIN_VOICED_FRAMES - 1, true, false, now);
    feed(&mut machine, SILENCE_FLUSH_FRAMES - 1, false, false, now);
    assert_eq!(machine.on_frame(false, false, now), FrameOutcome::Drop);
    assert_eq!(machine.phase(), UtterancePhase::Idle);
}

#[test]
fn one_wake_word_admits_exactly_one_utterance() {
    let mut machine = UtteranceMachine::new();
    let now = t0();
    machine.on_wake(now);
    feed(&mut machine, MIN_VOICED_FRAMES, true, false, now);
    feed(&mut machine, SILENCE_FLUSH_FRAMES - 1, false, false, now);
    assert_eq!(machine.on_frame(false, false, now), FrameOutcome::Decode);
    // Speaking again without a wake word must not be captured.
    for outcome in feed(&mut machine, 100, true, false, now) {
        assert_eq!(outcome, FrameOutcome::Idle);
    }
}

#[test]
fn the_arm_window_expires_when_nothing_is_said() {
    let mut machine = UtteranceMachine::new();
    let now = t0();
    machine.on_wake(now);
    assert_eq!(machine.on_frame(false, false, now), FrameOutcome::Idle);
    let later = now + WAKE_ARM_TIMEOUT - Duration::from_millis(1);
    assert_eq!(machine.on_frame(false, false, later), FrameOutcome::Idle);
    assert_eq!(machine.phase(), UtterancePhase::Armed);
    let expired = now + WAKE_ARM_TIMEOUT;
    assert_eq!(machine.on_frame(false, false, expired), FrameOutcome::Drop);
    assert_eq!(machine.phase(), UtterancePhase::Idle);
}

#[test]
fn the_machine_never_transcribes_the_apps_own_speech() {
    let mut machine = UtteranceMachine::new();
    let now = t0();
    machine.on_wake(now);
    // TTS playing: loud "speech" frames are the app's own voice.
    for outcome in feed(&mut machine, 500, true, true, now) {
        assert!(matches!(outcome, FrameOutcome::Idle | FrameOutcome::Drop));
    }
    assert_eq!(machine.phase(), UtterancePhase::Armed);
}

#[test]
fn a_partial_utterance_is_dropped_when_playback_starts() {
    let mut machine = UtteranceMachine::new();
    let now = t0();
    machine.on_wake(now);
    feed(&mut machine, 4, true, false, now);
    assert_eq!(machine.phase(), UtterancePhase::Capturing);
    assert_eq!(machine.on_frame(true, true, now), FrameOutcome::Drop);
    assert_eq!(machine.phase(), UtterancePhase::Armed);
}

#[test]
fn the_cooldown_holds_the_microphone_shut_after_playback_stops() {
    let mut machine = UtteranceMachine::new();
    let now = t0();
    machine.on_wake(now);
    machine.on_frame(false, true, now);
    // TTS just stopped — the speaker tail is still in the microphone.
    let stopped = now + Duration::from_millis(1);
    assert_eq!(machine.on_frame(true, false, stopped), FrameOutcome::Idle);
    assert_eq!(machine.phase(), UtterancePhase::Armed);
    // Once the cooldown expires the user is heard again.
    let after = stopped + TTS_COOLDOWN;
    assert_eq!(machine.on_frame(true, false, after), FrameOutcome::Buffer);
    assert_eq!(machine.phase(), UtterancePhase::Capturing);
}

#[test]
fn barge_in_keeps_the_full_speaking_window_after_playback_ends() {
    // A wake word fired mid-reply. Playback runs for longer than the arm
    // timeout; the follow-up sentence must still be captured, because the
    // window only counts time the user could actually be heard.
    let mut machine = UtteranceMachine::new();
    let now = t0();
    machine.on_wake(now);
    let mut clock = now;
    for _ in 0..40 {
        clock += WAKE_ARM_TIMEOUT / 4;
        machine.on_frame(false, true, clock);
    }
    assert_eq!(machine.phase(), UtterancePhase::Armed);
    // Playback ends. The frame on which it ends is the one that starts the
    // cooldown, so it is still gated.
    let stopped = clock + Duration::from_millis(1);
    assert_eq!(machine.on_frame(true, false, stopped), FrameOutcome::Idle);
    // Once the tail has passed, the follow-up sentence is captured — the arm
    // window was refreshed throughout playback and has not expired despite far
    // more than WAKE_ARM_TIMEOUT of wall time having elapsed since the wake.
    let speaking = stopped + TTS_COOLDOWN;
    assert_eq!(
        machine.on_frame(true, false, speaking),
        FrameOutcome::Buffer
    );
    assert!(speaking.duration_since(now) > WAKE_ARM_TIMEOUT * 4);
}

#[test]
fn a_second_wake_word_restarts_the_utterance() {
    let mut machine = UtteranceMachine::new();
    let now = t0();
    machine.on_wake(now);
    feed(&mut machine, 5, true, false, now);
    assert!(machine.on_wake(now), "buffered audio should be abandoned");
    assert_eq!(machine.phase(), UtterancePhase::Armed);
    assert_eq!(machine.voiced_frames(), 0);
}

#[test]
fn a_runaway_vad_is_capped_rather_than_growing_without_bound() {
    let mut machine = UtteranceMachine::new();
    let now = t0();
    machine.on_wake(now);
    let frames_to_cap = MAX_SPEECH_SAMPLES / VAD_FRAME_SAMPLES;
    let outcomes = feed(&mut machine, frames_to_cap, true, false, now);
    assert_eq!(outcomes.last().copied(), Some(FrameOutcome::Decode));
    assert_eq!(machine.phase(), UtterancePhase::Idle);
    assert_eq!(
        outcomes
            .iter()
            .filter(|o| **o == FrameOutcome::Decode)
            .count(),
        1
    );
}

#[test]
fn reset_abandons_everything_in_flight() {
    let mut machine = UtteranceMachine::new();
    let now = t0();
    machine.on_wake(now);
    feed(&mut machine, 20, true, false, now);
    machine.reset();
    assert_eq!(machine.phase(), UtterancePhase::Idle);
    assert_eq!(machine.voiced_frames(), 0);
    assert_eq!(machine.on_frame(true, false, now), FrameOutcome::Idle);
}

#[test]
fn the_capture_constants_match_the_huddle_pipeline() {
    // These are tuned against real huddle audio in `huddle::stt`. Divergence
    // would mean re-tuning the ambient path from scratch, so it is asserted
    // rather than left to comments.
    assert_eq!(VAD_FRAME_SAMPLES, 256);
    assert_eq!(SILENCE_FLUSH_FRAMES, 19);
    assert_eq!(MIN_VOICED_FRAMES, 12);
    assert_eq!(MAX_SPEECH_SAMPLES, 16_000 * 30);
    assert_eq!(TTS_COOLDOWN, Duration::from_millis(150));
}
