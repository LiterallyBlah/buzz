//! Utterance state-machine tests.
//!
//! The machine takes its clock as a parameter, so every timing rule here is
//! exercised deterministically rather than by sleeping.

use super::*;

fn t0() -> Instant {
    Instant::now()
}

/// A machine running the hold a user picked, in milliseconds.
fn machine_holding(silence_hold_ms: u32) -> UtteranceMachine {
    UtteranceMachine::new(UtteranceTiming::from_silence_hold_ms(silence_hold_ms))
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

/// Wake, then say enough to be worth transcribing.
fn start_speaking(machine: &mut UtteranceMachine, now: Instant) {
    machine.on_wake(now);
    for outcome in feed(machine, MIN_VOICED_FRAMES, true, false, now) {
        assert_eq!(outcome, FrameOutcome::Buffer);
    }
}

#[test]
fn idle_drops_every_frame_until_a_wake_word() {
    let mut machine = UtteranceMachine::default();
    let now = t0();
    assert_eq!(machine.phase(), UtterancePhase::Idle);
    for outcome in feed(&mut machine, 200, true, false, now) {
        assert_eq!(outcome, FrameOutcome::Idle);
    }
    assert_eq!(machine.phase(), UtterancePhase::Idle);
}

#[test]
fn a_wake_word_arms_capture() {
    let mut machine = UtteranceMachine::default();
    let now = t0();
    assert!(!machine.on_wake(now));
    assert_eq!(machine.phase(), UtterancePhase::Armed);
    assert_eq!(machine.on_frame(true, false, now), FrameOutcome::Buffer);
    assert_eq!(machine.phase(), UtterancePhase::Capturing);
}

#[test]
fn an_utterance_decodes_after_the_silence_threshold() {
    let mut machine = UtteranceMachine::default();
    let hold = machine.timing().silence_flush_frames();
    let now = t0();
    start_speaking(&mut machine, now);
    // Silence up to (not including) the threshold keeps buffering: brief gaps
    // inside a sentence must not split it.
    for outcome in feed(&mut machine, hold - 1, false, false, now) {
        assert_eq!(outcome, FrameOutcome::Buffer);
    }
    assert_eq!(machine.on_frame(false, false, now), FrameOutcome::Decode);
    assert_eq!(machine.phase(), UtterancePhase::Idle);
}

// ── The configurable silence hold ────────────────────────────────────────────

#[test]
fn a_pause_shorter_than_the_hold_never_closes_the_utterance() {
    // The property the setting exists for, asserted across the slider's whole
    // range rather than at one point: whatever the user picked, a pause under
    // it is a pause *inside* their sentence.
    for hold_ms in [
        MIN_SILENCE_HOLD_MS,
        500,
        DEFAULT_SILENCE_HOLD_MS,
        2_500,
        MAX_SILENCE_HOLD_MS,
    ] {
        let mut machine = machine_holding(hold_ms);
        let frames = machine.timing().silence_flush_frames();
        let now = t0();
        start_speaking(&mut machine, now);
        for outcome in feed(&mut machine, frames - 1, false, false, now) {
            assert_eq!(
                outcome,
                FrameOutcome::Buffer,
                "a pause under {hold_ms} ms closed the utterance"
            );
        }
        assert_eq!(machine.phase(), UtterancePhase::Capturing);
    }
}

#[test]
fn a_pause_that_reaches_the_hold_closes_the_utterance() {
    for hold_ms in [
        MIN_SILENCE_HOLD_MS,
        500,
        DEFAULT_SILENCE_HOLD_MS,
        2_500,
        MAX_SILENCE_HOLD_MS,
    ] {
        let mut machine = machine_holding(hold_ms);
        let frames = machine.timing().silence_flush_frames();
        let now = t0();
        start_speaking(&mut machine, now);
        let outcomes = feed(&mut machine, frames, false, false, now);
        assert_eq!(
            outcomes.last().copied(),
            Some(FrameOutcome::Decode),
            "a pause of {hold_ms} ms did not close the utterance"
        );
        assert_eq!(machine.phase(), UtterancePhase::Idle);
    }
}

#[test]
fn the_hold_is_the_number_of_whole_frames_that_covers_it() {
    // 16 ms per frame, rounded up — quantising must never hand back a hold
    // shorter than the one the user chose.
    for (hold_ms, frames) in [(300, 19), (500, 32), (800, 50), (2_500, 157), (10_000, 625)] {
        let timing = UtteranceTiming::from_silence_hold_ms(hold_ms);
        assert_eq!(timing.silence_flush_frames(), frames, "hold {hold_ms} ms");
        assert!(
            timing.silence_flush_frames() * 16 >= hold_ms as usize,
            "hold {hold_ms} ms was quantised short"
        );
    }
}

#[test]
fn a_hold_outside_the_sliders_range_is_clamped_rather_than_honoured() {
    // A hand-edited file, or a build with a wider slider, must not be able to
    // ask for a hold of zero — which would close every utterance on its first
    // silent frame — or for one long enough to hold the microphone all day.
    assert_eq!(
        UtteranceTiming::from_silence_hold_ms(0),
        UtteranceTiming::from_silence_hold_ms(MIN_SILENCE_HOLD_MS)
    );
    assert_eq!(
        UtteranceTiming::from_silence_hold_ms(u32::MAX),
        UtteranceTiming::from_silence_hold_ms(MAX_SILENCE_HOLD_MS)
    );
}

#[test]
fn an_ordinary_breath_no_longer_ends_the_sentence() {
    // The shipped 300 ms closed an utterance after 19 silent frames, which is
    // roughly one breath — the defect this setting exists to fix. The default
    // is 800 ms, so those same 19 frames are now part of the sentence.
    let mut machine = UtteranceMachine::default();
    let now = t0();
    start_speaking(&mut machine, now);
    for outcome in feed(&mut machine, 49, false, false, now) {
        assert_eq!(outcome, FrameOutcome::Buffer);
    }
    assert_eq!(machine.on_frame(false, false, now), FrameOutcome::Decode);
}

// ── The cap that follows the hold ────────────────────────────────────────────

#[test]
fn the_cap_grows_by_twenty_holds_on_top_of_thirty_seconds() {
    // A long hold spends real time inside an utterance waiting for pauses to
    // end. A fixed 30 s ceiling would make the pauses the user asked for the
    // thing that truncated them.
    for (hold_ms, cap_ms) in [
        (300, 36_000),
        (800, 46_000),
        (2_500, 80_000),
        (10_000, 230_000),
    ] {
        assert_eq!(
            UtteranceTiming::from_silence_hold_ms(hold_ms).max_speech_samples(),
            16 * cap_ms,
            "hold {hold_ms} ms"
        );
    }
}

#[test]
fn changing_the_hold_moves_the_cap_with_it() {
    // The two are not independently configurable, and nothing may set one
    // without the other following.
    let short = UtteranceTiming::from_silence_hold_ms(MIN_SILENCE_HOLD_MS);
    let long = UtteranceTiming::from_silence_hold_ms(MAX_SILENCE_HOLD_MS);
    assert!(long.silence_flush_frames() > short.silence_flush_frames());
    assert!(long.max_speech_samples() > short.max_speech_samples());
    assert_eq!(
        long.max_speech_samples() - short.max_speech_samples(),
        16 * 20 * (MAX_SILENCE_HOLD_MS - MIN_SILENCE_HOLD_MS) as usize
    );
}

#[test]
fn the_cap_leaves_room_for_the_pauses_the_hold_allows() {
    // Driven through the machine rather than read off the timing, so what is
    // pinned is the frame the buffer actually stops growing on: 46 s at the
    // default hold, not the 30 s a fixed cap would give.
    let mut machine = UtteranceMachine::default();
    let now = t0();
    machine.on_wake(now);
    let frames_to_cap = 46_000 * 16 / VAD_FRAME_SAMPLES;
    let outcomes = feed(&mut machine, frames_to_cap, true, false, now);
    assert_eq!(outcomes.last().copied(), Some(FrameOutcome::Decode));
    assert_eq!(
        outcomes
            .iter()
            .filter(|o| **o == FrameOutcome::Decode)
            .count(),
        1
    );
    assert_eq!(machine.phase(), UtterancePhase::Idle);
}

// ── Everything the silence hold did not change ───────────────────────────────

#[test]
fn a_short_blip_is_dropped_rather_than_transcribed() {
    let mut machine = UtteranceMachine::default();
    let hold = machine.timing().silence_flush_frames();
    let now = t0();
    machine.on_wake(now);
    feed(&mut machine, MIN_VOICED_FRAMES - 1, true, false, now);
    feed(&mut machine, hold - 1, false, false, now);
    assert_eq!(machine.on_frame(false, false, now), FrameOutcome::Drop);
    assert_eq!(machine.phase(), UtterancePhase::Idle);
}

#[test]
fn one_wake_word_admits_exactly_one_utterance() {
    let mut machine = UtteranceMachine::default();
    let hold = machine.timing().silence_flush_frames();
    let now = t0();
    start_speaking(&mut machine, now);
    feed(&mut machine, hold - 1, false, false, now);
    assert_eq!(machine.on_frame(false, false, now), FrameOutcome::Decode);
    // Speaking again without a wake word must not be captured.
    for outcome in feed(&mut machine, 100, true, false, now) {
        assert_eq!(outcome, FrameOutcome::Idle);
    }
}

#[test]
fn the_arm_window_expires_when_nothing_is_said() {
    let mut machine = UtteranceMachine::default();
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
fn the_wake_to_speech_window_is_the_same_whatever_the_hold_is() {
    // The silence hold is about how a sentence ends. How long a wake word waits
    // for one to start is a different question with a different answer, and the
    // slider must not have quietly moved it.
    for hold_ms in [
        MIN_SILENCE_HOLD_MS,
        DEFAULT_SILENCE_HOLD_MS,
        MAX_SILENCE_HOLD_MS,
    ] {
        let mut machine = machine_holding(hold_ms);
        let now = t0();
        machine.on_wake(now);
        let almost = now + WAKE_ARM_TIMEOUT - Duration::from_millis(1);
        assert_eq!(machine.on_frame(false, false, almost), FrameOutcome::Idle);
        let expired = now + WAKE_ARM_TIMEOUT;
        assert_eq!(machine.on_frame(false, false, expired), FrameOutcome::Drop);
    }
    assert_eq!(WAKE_ARM_TIMEOUT, Duration::from_secs(6));
}

#[test]
fn the_machine_never_transcribes_the_apps_own_speech() {
    let mut machine = UtteranceMachine::default();
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
    let mut machine = UtteranceMachine::default();
    let now = t0();
    machine.on_wake(now);
    feed(&mut machine, 4, true, false, now);
    assert_eq!(machine.phase(), UtterancePhase::Capturing);
    assert_eq!(machine.on_frame(true, true, now), FrameOutcome::Drop);
    assert_eq!(machine.phase(), UtterancePhase::Armed);
}

#[test]
fn the_cooldown_holds_the_microphone_shut_after_playback_stops() {
    let mut machine = UtteranceMachine::default();
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
    let mut machine = UtteranceMachine::default();
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
    let mut machine = UtteranceMachine::default();
    let now = t0();
    machine.on_wake(now);
    feed(&mut machine, 5, true, false, now);
    assert!(machine.on_wake(now), "buffered audio should be abandoned");
    assert_eq!(machine.phase(), UtterancePhase::Armed);
    assert_eq!(machine.voiced_frames(), 0);
}

#[test]
fn reset_abandons_everything_in_flight() {
    let mut machine = UtteranceMachine::default();
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
    // rather than left to comments. The silence hold is the one value that is
    // now the user's, and its floor is still exactly what huddle uses.
    assert_eq!(VAD_FRAME_SAMPLES, 256);
    assert_eq!(MIN_VOICED_FRAMES, 12);
    assert_eq!(TTS_COOLDOWN, Duration::from_millis(150));
    let floor = UtteranceTiming::from_silence_hold_ms(MIN_SILENCE_HOLD_MS);
    assert_eq!(floor.silence_flush_frames(), 19);
}
