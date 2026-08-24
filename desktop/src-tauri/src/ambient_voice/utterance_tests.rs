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

/// Frames of speech it takes to fill one buffer to the ceiling.
fn frames_to_ceiling(machine: &UtteranceMachine) -> usize {
    machine
        .timing()
        .max_speech_samples()
        .div_ceil(VAD_FRAME_SAMPLES)
}

/// Speak until the buffer hits the ceiling, and return every outcome on the way.
fn fill_to_ceiling(machine: &mut UtteranceMachine, now: Instant) -> Vec<FrameOutcome> {
    let frames = frames_to_ceiling(machine);
    feed(machine, frames, true, false, now)
}

/// How many of these outcomes were a chunk handed off.
fn chunks_in(outcomes: &[FrameOutcome]) -> usize {
    outcomes
        .iter()
        .filter(|outcome| **outcome == FrameOutcome::Chunk)
        .count()
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
    assert_eq!(outcomes.last().copied(), Some(FrameOutcome::Chunk));
    assert_eq!(
        outcomes
            .iter()
            .filter(|o| **o == FrameOutcome::Chunk)
            .count(),
        1
    );
}

// ── The ceiling closes a chunk, not the capture ──────────────────────────────

#[test]
fn the_ceiling_hands_a_chunk_off_and_goes_on_capturing() {
    // The shipped fault: at the ceiling the capture ended, silently, and
    // everything said afterwards was lost until the next wake word. What the
    // ceiling ends now is the buffer.
    let mut machine = UtteranceMachine::default();
    let now = t0();
    machine.on_wake(now);
    let outcomes = fill_to_ceiling(&mut machine, now);

    assert_eq!(outcomes.last().copied(), Some(FrameOutcome::Chunk));
    assert_eq!(machine.phase(), UtterancePhase::Capturing);
    // And the very next word is captured, into a buffer that started again
    // empty rather than into nothing at all.
    assert_eq!(machine.on_frame(true, false, now), FrameOutcome::Buffer);
    assert_eq!(machine.phase(), UtterancePhase::Capturing);
}

#[test]
fn every_chunk_is_bounded_by_the_same_ceiling() {
    // The ceiling's engineering purpose is unchanged: no buffer may grow
    // without bound. A conversation three ceilings long is three chunks, each
    // one closed at the same size.
    let mut machine = UtteranceMachine::default();
    let now = t0();
    machine.on_wake(now);
    let to_ceiling = frames_to_ceiling(&machine);

    let mut chunk_frames = Vec::new();
    let mut since_chunk = 0;
    for _ in 0..to_ceiling * 3 {
        since_chunk += 1;
        if machine.on_frame(true, false, now) == FrameOutcome::Chunk {
            chunk_frames.push(since_chunk);
            since_chunk = 0;
        }
    }
    assert_eq!(chunk_frames, vec![to_ceiling, to_ceiling, to_ceiling]);
    assert_eq!(machine.phase(), UtterancePhase::Capturing);
}

#[test]
fn a_capture_that_rolled_once_and_was_stopped_is_two_chunks() {
    // The whole shape of the feature, end to end through the machine: the
    // ceiling takes the first chunk, the user carries on talking, and the stop
    // phrase closes the second and last one.
    let mut machine = machine_holding(MAX_SILENCE_HOLD_MS);
    let now = t0();
    machine.on_wake(now);
    let outcomes = fill_to_ceiling(&mut machine, now);
    assert_eq!(chunks_in(&outcomes), 1);

    let more = feed(&mut machine, MIN_VOICED_FRAMES, true, false, now);
    assert_eq!(chunks_in(&more), 0, "a second ceiling arrived early");
    assert_eq!(machine.on_stop_phrase(), FrameOutcome::Decode);
    assert_eq!(machine.phase(), UtterancePhase::Idle);
}

#[test]
fn a_capture_that_rolled_once_still_closes_on_the_silence_hold() {
    // The other close, unchanged by any of this: the pause the user chose ends
    // the utterance whether or not the ceiling took a chunk out of it first.
    let mut machine = UtteranceMachine::default();
    let hold = machine.timing().silence_flush_frames();
    let now = t0();
    machine.on_wake(now);
    fill_to_ceiling(&mut machine, now);
    feed(&mut machine, MIN_VOICED_FRAMES, true, false, now);

    let outcomes = feed(&mut machine, hold, false, false, now);
    assert_eq!(outcomes.last().copied(), Some(FrameOutcome::Decode));
    assert_eq!(machine.phase(), UtterancePhase::Idle);
}

#[test]
fn a_pause_running_across_a_chunk_boundary_still_ends_the_utterance_on_time() {
    // The silence run belongs to the utterance, not to the buffer the ceiling
    // happened to close in the middle of it. Resetting it there would hand the
    // user a hold longer than the one they asked for, exactly once per chunk.
    let mut machine = machine_holding(MIN_SILENCE_HOLD_MS);
    let now = t0();
    machine.on_wake(now);
    let hold = machine.timing().silence_flush_frames();
    // Speak up to one hold short of the ceiling, then fall silent through it.
    let speaking = frames_to_ceiling(&machine) - hold + 1;
    feed(&mut machine, speaking, true, false, now);
    let outcomes = feed(&mut machine, hold, false, false, now);

    assert_eq!(
        chunks_in(&outcomes),
        1,
        "the ceiling was not crossed: {outcomes:?}"
    );
    assert_eq!(outcomes.last().copied(), Some(FrameOutcome::Decode));
    assert_eq!(machine.phase(), UtterancePhase::Idle);
}

#[test]
fn a_short_word_after_a_chunk_does_not_throw_the_whole_capture_away() {
    // The voiced-frame floor keeps room noise out of the recogniser, and it is
    // measured against the buffer. Applied to the last chunk alone it would
    // discard minutes of already-captured speech because the user said one
    // short word before stopping.
    let mut machine = machine_holding(MAX_SILENCE_HOLD_MS);
    let now = t0();
    machine.on_wake(now);
    fill_to_ceiling(&mut machine, now);
    feed(&mut machine, MIN_VOICED_FRAMES - 1, true, false, now);

    assert!(machine.voiced_frames() < MIN_VOICED_FRAMES);
    assert_eq!(machine.on_stop_phrase(), FrameOutcome::Decode);
}

#[test]
fn a_wake_word_mid_roll_abandons_the_chunks_already_handed_off() {
    // A second wake word is a restart, and the caller learns to throw away what
    // it is holding from this answer. After a chunk handoff its buffer is empty
    // and the audio is elsewhere — answering "nothing to abandon" would stitch
    // the abandoned sentence into the next one.
    let mut machine = UtteranceMachine::default();
    let now = t0();
    machine.on_wake(now);
    let outcomes = fill_to_ceiling(&mut machine, now);
    assert_eq!(outcomes.last().copied(), Some(FrameOutcome::Chunk));

    assert!(
        machine.on_wake(now),
        "a rolled capture was restarted without being abandoned"
    );
    assert_eq!(machine.phase(), UtterancePhase::Armed);
}

#[test]
fn playback_starting_mid_roll_abandons_the_chunks_already_handed_off() {
    // Same rule, reached the other way: the app started speaking, so the
    // half-captured sentence is dropped — including the part of it that is
    // already on its way to the recogniser.
    let mut machine = UtteranceMachine::default();
    let now = t0();
    machine.on_wake(now);
    fill_to_ceiling(&mut machine, now);

    assert_eq!(machine.on_frame(true, true, now), FrameOutcome::Drop);
    assert_eq!(machine.phase(), UtterancePhase::Armed);
}

// ── The stop phrase ──────────────────────────────────────────────────────────

#[test]
fn the_stop_phrase_finalises_the_capture_at_once() {
    // Said mid-sentence, with the silence hold nowhere near expiring: the point
    // of the phrase is not waiting for the pause.
    let mut machine = machine_holding(MAX_SILENCE_HOLD_MS);
    let now = t0();
    start_speaking(&mut machine, now);
    assert_eq!(machine.phase(), UtterancePhase::Capturing);
    assert_eq!(machine.on_stop_phrase(), FrameOutcome::Decode);
    assert_eq!(machine.phase(), UtterancePhase::Idle);
    // And exactly like a silence-close, one wake word still admits only one
    // utterance: speaking again without waking must not be captured.
    for outcome in feed(&mut machine, 100, true, false, now) {
        assert_eq!(outcome, FrameOutcome::Idle);
    }
}

#[test]
fn the_stop_phrase_takes_the_same_exit_as_a_silence_close() {
    // Too little voice to be worth transcribing is dropped rather than sent,
    // by the same rule and the same threshold that a silence-close applies.
    let mut machine = machine_holding(MAX_SILENCE_HOLD_MS);
    let now = t0();
    machine.on_wake(now);
    feed(&mut machine, MIN_VOICED_FRAMES - 1, true, false, now);
    assert_eq!(machine.on_stop_phrase(), FrameOutcome::Drop);
    assert_eq!(machine.phase(), UtterancePhase::Idle);
}

#[test]
fn the_stop_phrase_is_a_no_op_while_nothing_is_being_captured() {
    // The dangerous failure: a phrase that woke, armed, or otherwise moved an
    // idle machine would turn a stop word into a second wake word, and the
    // microphone is always open.
    let mut machine = UtteranceMachine::default();
    let now = t0();
    assert_eq!(machine.phase(), UtterancePhase::Idle);
    assert_eq!(machine.on_stop_phrase(), FrameOutcome::Idle);
    assert_eq!(machine.phase(), UtterancePhase::Idle);
    // Still deaf afterwards: speech that follows is not captured.
    for outcome in feed(&mut machine, 50, true, false, now) {
        assert_eq!(outcome, FrameOutcome::Idle);
    }
    assert_eq!(machine.phase(), UtterancePhase::Idle);
}

#[test]
fn the_stop_phrase_leaves_an_armed_wake_word_alone() {
    // Armed is "a wake word fired, nothing said yet". There is no capture to
    // finalise, and cancelling the window would be a state change on a user who
    // has not spoken.
    let mut machine = UtteranceMachine::default();
    let now = t0();
    machine.on_wake(now);
    assert_eq!(machine.on_stop_phrase(), FrameOutcome::Idle);
    assert_eq!(machine.phase(), UtterancePhase::Armed);
    assert_eq!(machine.on_frame(true, false, now), FrameOutcome::Buffer);
    assert_eq!(machine.phase(), UtterancePhase::Capturing);
}

// ── Everything the two new settings did not change ───────────────────────────

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
fn the_capture_constants_are_the_ones_this_path_is_tuned_to() {
    // The frame size and the minimum voiced length came from `huddle::stt`,
    // tuned against real huddle audio, and still match it. The endpointing
    // does not: the hold is the user's setting here, and huddle re-tuned its
    // own flush window to 31 frames with a 0.55/0.35 hysteresis band in #6397.
    // These are therefore this path's own values, pinned so a change to them
    // is a decision rather than a drift — the previous name claimed a parity
    // with huddle that a literal in this file could never have detected losing.
    assert_eq!(VAD_FRAME_SAMPLES, 256);
    assert_eq!(MIN_VOICED_FRAMES, 12);
    assert_eq!(TTS_COOLDOWN, Duration::from_millis(150));
    let floor = UtteranceTiming::from_silence_hold_ms(MIN_SILENCE_HOLD_MS);
    assert_eq!(floor.silence_flush_frames(), 19);
}
