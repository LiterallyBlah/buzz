//! Rolling-capture tests.
//!
//! The transcriber is a stub closure — the seam [`RollingCapture::spawn`] takes
//! for exactly this reason — so a two-chunk utterance is exercised without an
//! ONNX model, a speech server or a microphone, and a transcriber that never
//! answers is a channel nobody sends on rather than a sleep.

use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
};

use super::*;

/// How many samples each chunk in these tests carries, so the recorded lengths
/// name which chunk arrived.
const CHUNK_SAMPLES: [usize; 3] = [10, 20, 30];

/// What the stub transcriber was asked to decode, in order: one entry per
/// chunk, holding its length in samples.
type Decoded = Arc<Mutex<Vec<usize>>>;

fn locked<T>(cell: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    cell.lock().unwrap_or_else(|error| error.into_inner())
}

/// A transcriber that answers from a script and records what it was given.
///
/// A script that runs out answers `Ok("")`, which is the recogniser's own way
/// of saying "no words in this audio" and keeps a test from having to spell out
/// replies it is not asserting on.
fn scripted(
    replies: Vec<Result<&'static str, &'static str>>,
) -> (
    impl Fn(&[f32]) -> Result<String, String> + Send + 'static,
    Decoded,
) {
    let queue: VecDeque<Result<String, String>> = replies
        .into_iter()
        .map(|reply| reply.map(str::to_string).map_err(|error| error.to_string()))
        .collect();
    let queue = Mutex::new(queue);
    let decoded: Decoded = Arc::new(Mutex::new(Vec::new()));
    let recorder = Arc::clone(&decoded);
    let decode = move |samples: &[f32]| {
        locked(&recorder).push(samples.len());
        locked(&queue)
            .pop_front()
            .unwrap_or_else(|| Ok(String::new()))
    };
    (decode, decoded)
}

/// One chunk of `samples` samples of audio.
fn chunk(samples: usize) -> Vec<f32> {
    vec![0.05; samples]
}

#[test]
fn an_ordinary_utterance_is_one_chunk_and_one_transcript() {
    // The path almost every utterance takes, and the one the rolling machinery
    // must not have changed: nothing is handed off before the close, and the
    // close answers with the whole transcript.
    let (decode, decoded) = scripted(vec![Ok("book me a room")]);
    let mut capture = RollingCapture::spawn(decode).expect("spawn");

    let text = capture
        .finish(chunk(CHUNK_SAMPLES[0]), None)
        .expect("finish");

    assert_eq!(text.as_deref(), Some("book me a room"));
    assert_eq!(*locked(&decoded), vec![CHUNK_SAMPLES[0]]);
}

#[test]
fn a_capture_that_rolled_once_is_stitched_in_the_order_it_was_spoken() {
    // The feature: the ceiling closed a chunk mid-sentence, the user kept
    // talking, and what reaches the agent is one message in the order they said
    // it — not two messages, and not the second half first.
    let (decode, decoded) = scripted(vec![Ok("the first half"), Ok("and the second")]);
    let mut capture = RollingCapture::spawn(decode).expect("spawn");

    capture.hand_off(chunk(CHUNK_SAMPLES[0]));
    let text = capture
        .finish(chunk(CHUNK_SAMPLES[1]), None)
        .expect("finish");

    assert_eq!(text.as_deref(), Some("the first half and the second"));
    // And the transcriber saw them in that order too, so the ordering is the
    // pipeline's rather than an accident of two texts that sort correctly.
    assert_eq!(
        *locked(&decoded),
        vec![CHUNK_SAMPLES[0], CHUNK_SAMPLES[1]],
        "chunks reached the transcriber out of order"
    );
}

#[test]
fn a_long_capture_stitches_every_chunk_it_took() {
    let (decode, _) = scripted(vec![Ok("one"), Ok("two"), Ok("three")]);
    let mut capture = RollingCapture::spawn(decode).expect("spawn");

    capture.hand_off(chunk(CHUNK_SAMPLES[0]));
    capture.hand_off(chunk(CHUNK_SAMPLES[1]));
    let text = capture
        .finish(chunk(CHUNK_SAMPLES[2]), None)
        .expect("finish");

    assert_eq!(text.as_deref(), Some("one two three"));
}

#[test]
fn a_capture_may_run_to_more_chunks_than_the_bound_allows_in_flight() {
    // The bound is on audio the transcriber is still holding, not on how long
    // someone may talk. Counting chunks to the end of the capture instead would
    // put a four-chunk ceiling back on an utterance — a worse version of the
    // truncation this module exists to remove, because it fails out loud after
    // several minutes of dictation.
    let (decode, decoded) = scripted(Vec::new());
    let mut capture = RollingCapture::spawn(decode).expect("spawn");

    let chunks = MAX_PENDING_CHUNKS * 3;
    for _ in 0..chunks {
        capture.hand_off(chunk(CHUNK_SAMPLES[0]));
        // A real capture spends a ceiling — tens of seconds — filling the next
        // buffer. Waiting for the transcriber to hand the audio back is that
        // gap, and it is the production count that says when it has.
        wait_until_the_transcriber_catches_up(&capture);
    }
    let text = capture
        .finish(chunk(CHUNK_SAMPLES[1]), None)
        .expect("finish");

    assert_eq!(
        text, None,
        "the script was empty, so the words are not the point"
    );
    assert_eq!(locked(&decoded).len(), chunks + 1);
}

/// Block until the transcription thread is holding no audio, or give up.
///
/// Reads the production counter the bound is taken from, so a change that
/// stopped releasing it stalls here and fails the test that called it.
fn wait_until_the_transcriber_catches_up(capture: &RollingCapture) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while capture.in_flight.load(Ordering::Acquire) > 0 {
        if std::time::Instant::now() > deadline {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

#[test]
fn a_silent_stretch_contributes_nothing_to_the_stitched_utterance() {
    // Someone thinking mid-dictation for longer than a chunk. The empty
    // transcript is not a failure and must not become a double space either.
    let (decode, _) = scripted(vec![Ok("before the pause"), Ok("   "), Ok("after it")]);
    let mut capture = RollingCapture::spawn(decode).expect("spawn");

    capture.hand_off(chunk(CHUNK_SAMPLES[0]));
    capture.hand_off(chunk(CHUNK_SAMPLES[1]));
    let text = capture
        .finish(chunk(CHUNK_SAMPLES[2]), None)
        .expect("finish");

    assert_eq!(text.as_deref(), Some("before the pause after it"));
}

#[test]
fn an_utterance_whose_every_chunk_was_silent_sends_nothing() {
    // The existing empty-transcript case, reached the long way round: audio the
    // recogniser found no words in is an ordinary outcome, not a fault, and
    // publishing an empty message would be worse than publishing none.
    let (decode, _) = scripted(vec![Ok(""), Ok("")]);
    let mut capture = RollingCapture::spawn(decode).expect("spawn");

    capture.hand_off(chunk(CHUNK_SAMPLES[0]));
    let text = capture
        .finish(chunk(CHUNK_SAMPLES[1]), None)
        .expect("finish");

    assert_eq!(text, None);
}

#[test]
fn a_chunk_that_could_not_be_transcribed_fails_the_whole_utterance() {
    // The alternative is a message with a hole in it and nothing to show that
    // it has one — the user's words, edited by a failure they were never told
    // about. A single-chunk utterance already fails this way; a chunked one
    // fails the same way.
    let (decode, _) = scripted(vec![
        Err("Speech server failed: HTTP 502"),
        Ok("and the rest"),
    ]);
    let mut capture = RollingCapture::spawn(decode).expect("spawn");

    capture.hand_off(chunk(CHUNK_SAMPLES[0]));
    let Err(error) = capture.finish(chunk(CHUNK_SAMPLES[1]), None) else {
        panic!("a failed chunk was stitched over");
    };

    assert_eq!(error, "Speech server failed: HTTP 502");
}

#[test]
fn a_failed_utterance_leaves_no_chunk_behind_for_the_next_one() {
    // The failing chunk's siblings were still in flight. Their transcripts must
    // not turn up inside whatever the user says next.
    let (decode, _) = scripted(vec![Err("Speech server failed: HTTP 502"), Ok("stranded")]);
    let mut capture = RollingCapture::spawn(decode).expect("spawn");

    capture.hand_off(chunk(CHUNK_SAMPLES[0]));
    assert!(capture.finish(chunk(CHUNK_SAMPLES[1]), None).is_err());

    let text = capture
        .finish(chunk(CHUNK_SAMPLES[2]), None)
        .expect("finish");
    assert_eq!(
        text, None,
        "a stranded chunk was stitched into the next message"
    );
}

#[test]
fn an_utterance_abandoned_mid_roll_is_never_submitted() {
    // Mute, a huddle, playback starting, a second wake word: all of them
    // abandon a half-captured sentence, and none of them may block the worker
    // waiting for a chunk that is already inside the recogniser.
    let (decode, decoded) = scripted(vec![Ok("half a sentence"), Ok("what came after")]);
    let mut capture = RollingCapture::spawn(decode).expect("spawn");

    capture.hand_off(chunk(CHUNK_SAMPLES[0]));
    capture.abort();

    // The next utterance is its own message, carrying nothing of the abandoned
    // one — the abandoned chunk was decoded anyway (the thread was already
    // holding it) and its text discarded.
    let text = capture
        .finish(chunk(CHUNK_SAMPLES[1]), None)
        .expect("finish");
    assert_eq!(text.as_deref(), Some("what came after"));
    assert_eq!(
        *locked(&decoded),
        vec![CHUNK_SAMPLES[0], CHUNK_SAMPLES[1]],
        "the abandoned chunk never reached the transcriber, so nothing proves it was discarded"
    );
}

#[test]
fn a_transcriber_that_stops_answering_fails_the_utterance_rather_than_growing_the_queue() {
    // The bound exists so a stuck transcriber holds a fixed amount of PCM
    // instead of as much as the user can say. Reaching it fails the utterance
    // out loud: the middle of a message may not go missing quietly.
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let decoded: Decoded = Arc::new(Mutex::new(Vec::new()));
    let recorder = Arc::clone(&decoded);
    let mut capture = RollingCapture::spawn(move |samples: &[f32]| {
        // Blocks until the test lets go, so every chunk after the first stays
        // in the queue rather than in the recogniser.
        let _ = release_rx.recv();
        locked(&recorder).push(samples.len());
        Ok(String::new())
    })
    .expect("spawn");

    for _ in 0..MAX_PENDING_CHUNKS {
        capture.hand_off(chunk(CHUNK_SAMPLES[0]));
    }
    capture.hand_off(chunk(CHUNK_SAMPLES[1]));

    // Releasing before the close, so the failure is the bound rather than a
    // transcriber that never answered at all.
    drop(release_tx);
    let Err(error) = capture.finish(chunk(CHUNK_SAMPLES[2]), None) else {
        panic!("the queue grew past its bound and the utterance was sent anyway");
    };
    assert!(error.contains("fell too far behind"), "{error}");

    // Dropping joins the transcription thread, so what it was given is settled
    // by now: the bound's worth, and neither the chunk that overflowed it nor
    // the one that closed the capture.
    drop(capture);
    assert_eq!(
        *locked(&decoded),
        vec![CHUNK_SAMPLES[0]; MAX_PENDING_CHUNKS],
        "the queue kept growing past its bound"
    );
}

#[test]
fn the_stop_phrase_is_trimmed_off_the_stitched_utterance() {
    // The phrase ends the last chunk, so it is the stitched transcript that
    // carries it — trimming the last chunk alone would leave the phrase behind
    // whenever the ceiling happened to split it in two.
    let (decode, _) = scripted(vec![Ok("remind me to buy milk"), Ok("buzz stop")]);
    let mut capture = RollingCapture::spawn(decode).expect("spawn");

    capture.hand_off(chunk(CHUNK_SAMPLES[0]));
    let text = capture
        .finish(chunk(CHUNK_SAMPLES[1]), Some("buzz stop"))
        .expect("finish");

    assert_eq!(text.as_deref(), Some("remind me to buy milk"));
}

#[test]
fn the_transcription_thread_ends_with_the_capture_that_started_it() {
    // A session torn down mid-roll must not leave a thread holding a
    // microphone's worth of PCM and a recogniser.
    struct ThreadEnded(Arc<AtomicBool>);
    impl Drop for ThreadEnded {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }

    let ended = Arc::new(AtomicBool::new(false));
    let guard = ThreadEnded(Arc::clone(&ended));
    let mut capture = RollingCapture::spawn(move |_| {
        // Lives on the transcription thread; dropped when that thread ends.
        let _guard = &guard;
        Ok(String::new())
    })
    .expect("spawn");
    capture.hand_off(chunk(CHUNK_SAMPLES[0]));

    drop(capture);

    assert!(
        ended.load(Ordering::Acquire),
        "the transcription thread outlived the capture it belonged to"
    );
}

// ── Stitching ────────────────────────────────────────────────────────────────

#[test]
fn chunks_are_joined_by_exactly_one_space() {
    // The boundary is where the buffer filled up, not where the user paused, so
    // there is nothing else to put between two chunks. Whatever the recogniser
    // padded its answer with is not it.
    assert_eq!(
        stitch(&["one".to_string(), "  two  ".to_string()]),
        "one two"
    );
    assert_eq!(stitch(&[]), "");
    assert_eq!(stitch(&["only".to_string()]), "only");
    assert_eq!(stitch(&["".to_string(), "only".to_string()]), "only");
}

// ── Trimming the stop phrase back out of the transcript ──────────────────────

#[test]
fn the_stop_phrase_is_trimmed_off_the_end_of_the_transcript() {
    // It is what ended the capture, so it is in the audio the recogniser was
    // given. Sending it to the agent would mean every hands-free message
    // finished with the words the user said to stop talking.
    assert_eq!(
        strip_trailing_phrase("remind me to buy milk buzz stop", "buzz stop"),
        "remind me to buy milk"
    );
    // Casing and punctuation are the recogniser's to choose, not the user's.
    assert_eq!(
        strip_trailing_phrase("Remind me to buy milk. Buzz, stop.", "buzz stop"),
        "Remind me to buy milk."
    );
    assert_eq!(
        strip_trailing_phrase("  remind me   BUZZ STOP  ", "  Buzz   Stop "),
        "remind me"
    );
}

#[test]
fn an_utterance_that_was_only_the_stop_phrase_leaves_nothing_to_send() {
    assert_eq!(strip_trailing_phrase("buzz stop", "buzz stop"), "");
}

#[test]
fn a_transcript_that_merely_mentions_the_phrase_keeps_it() {
    // Only a whole-word run at the very end is removed. Anything else would
    // quietly edit the user's message.
    assert_eq!(
        strip_trailing_phrase("buzz stop asking me that", "buzz stop"),
        "buzz stop asking me that"
    );
    assert_eq!(
        strip_trailing_phrase("tell me when to stop", "buzz stop"),
        "tell me when to stop"
    );
    // A partial match at the end is not a match.
    assert_eq!(strip_trailing_phrase("stop", "buzz stop"), "stop");
}

#[test]
fn a_transcript_closed_by_silence_is_never_trimmed() {
    // The trim only applies to the close that put the phrase in the buffer.
    // `None` here is the silence-close, which has no phrase to remove and must
    // not lose the user's last words to one.
    assert_eq!(strip_trailing_phrase("buzz stop", ""), "buzz stop");
    assert_eq!(strip_trailing_phrase("buzz stop", "   "), "buzz stop");
}
