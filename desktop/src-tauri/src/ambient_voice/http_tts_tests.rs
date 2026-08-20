//! Remote-speech pipeline tests.
//!
//! Driven through the real worker against the loopback stub server, with the
//! audio device replaced by a player the test can hold open: CI has no sound
//! card, and the two behaviours that matter here — `tts_active` gating capture
//! and barge-in silencing playback — are exactly the ones that only exist
//! while audio is outstanding.

use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Mutex,
};

use super::*;
use crate::ambient_voice::speech_stub_server::{StubReply, StubRequest, StubSpeechServer};

const WAIT: Duration = Duration::from_secs(5);
const REPLY_RATE: u32 = 24_000;

/// A player the test drives: audio stays "playing" until the test says
/// otherwise, which is the only way to observe the flags that exist during
/// playback.
#[derive(Default)]
struct TestPlayer {
    played: Mutex<Vec<DecodedAudio>>,
    playing: AtomicBool,
    stops: AtomicUsize,
}

impl SpeechPlayer for Arc<TestPlayer> {
    fn play(&self, audio: DecodedAudio) -> Result<(), String> {
        self.played
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push(audio);
        self.playing.store(true, Ordering::Release);
        Ok(())
    }

    fn is_playing(&self) -> bool {
        self.playing.load(Ordering::Acquire)
    }

    fn stop(&self) {
        self.stops.fetch_add(1, Ordering::Release);
        self.playing.store(false, Ordering::Release);
    }
}

impl TestPlayer {
    fn played(&self) -> Vec<DecodedAudio> {
        self.played
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    /// Playback finished on its own.
    fn finish(&self) {
        self.playing.store(false, Ordering::Release);
    }
}

/// A reply the stub speaks: 0.1 s of audible tone at the reply rate.
fn spoken_wav() -> Vec<u8> {
    let samples: Vec<f32> = (0..REPLY_RATE as usize / 10)
        .map(|i| (i as f32 / 30.0).sin() * 0.4)
        .collect();
    speech_wav::encode_pcm16_mono(&samples, REPLY_RATE)
}

struct Harness {
    pipeline: HttpTtsPipeline,
    player: Arc<TestPlayer>,
    tts_active: Arc<AtomicBool>,
    tts_cancel: Arc<AtomicBool>,
}

fn harness(base_url: &str) -> Harness {
    let player = Arc::new(TestPlayer::default());
    let tts_active = Arc::new(AtomicBool::new(false));
    let tts_cancel = Arc::new(AtomicBool::new(false));
    let for_worker = Arc::clone(&player);
    let pipeline = HttpTtsPipeline::with_player(
        SpeechEndpoint::parse(base_url).expect("endpoint"),
        Box::new(move || Ok(Box::new(for_worker))),
        Arc::clone(&tts_active),
        Arc::clone(&tts_cancel),
    )
    .expect("pipeline");
    Harness {
        pipeline,
        player,
        tts_active,
        tts_cancel,
    }
}

/// Spin until `condition` holds, or give up. Returns whether it held.
fn wait_until(condition: impl Fn() -> bool) -> bool {
    let deadline = std::time::Instant::now() + WAIT;
    while std::time::Instant::now() < deadline {
        if condition() {
            return true;
        }
        thread::sleep(Duration::from_millis(5));
    }
    condition()
}

#[test]
fn a_reply_is_spoken_by_the_server_and_gates_capture_while_it_plays() {
    // Acceptance criterion (b). The reply text goes to `/v1/audio/speech`, the
    // audio that comes back reaches the player, and `tts_active` is true for
    // exactly as long as it is sounding — that flag is what stops the
    // utterance machine transcribing the agent's own voice as the user's next
    // sentence.
    let server = StubSpeechServer::always(StubReply::wav(spoken_wav()));
    let harness = harness(server.base_url());

    assert!(!harness.tts_active.load(Ordering::Acquire));
    harness
        .pipeline
        .speak("Your calendar is clear tomorrow.".to_string())
        .expect("speak");

    assert!(
        wait_until(|| !harness.player.played().is_empty()),
        "the reply never reached the player"
    );
    assert!(
        wait_until(|| harness.tts_active.load(Ordering::Acquire)),
        "capture was never gated while the agent was speaking"
    );

    let played = harness.player.played();
    assert_eq!(played.len(), 1);
    assert_eq!(played[0].sample_rate, REPLY_RATE);
    assert_eq!(played[0].channels, 1);
    assert!(
        played[0].samples.iter().any(|sample| sample.abs() > 0.1),
        "the audio handed to the device was silence"
    );

    let requests = server.wait_for_requests(1, WAIT);
    let request = requests.first().expect("one request");
    assert_eq!(request.path, "/v1/audio/speech");
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&request.body).expect("json"),
        serde_json::json!({ "input": "Your calendar is clear tomorrow." })
    );

    // And when the audio drains, the microphone gate reopens — otherwise the
    // user could never be heard again.
    harness.player.finish();
    assert!(
        wait_until(|| !harness.tts_active.load(Ordering::Acquire)),
        "capture stayed gated after playback finished"
    );
}

#[test]
fn the_wake_word_stops_a_remote_reply_exactly_as_it_stops_a_local_one() {
    // Barge-in. The audio worker sets `tts_cancel` the moment the wake word
    // fires, whatever is speaking, so this pipeline has to honour it within a
    // tick and drop what was queued behind it: the user interrupted, and
    // finishing the old answer first is the opposite of what they asked for.
    let server = StubSpeechServer::always(StubReply::wav(spoken_wav()));
    let harness = harness(server.base_url());
    harness
        .pipeline
        .speak("The first half of a long answer.".to_string())
        .expect("speak");
    assert!(wait_until(|| harness.tts_active.load(Ordering::Acquire)));

    harness
        .pipeline
        .speak("And the rest of it.".to_string())
        .expect("speak");
    harness.tts_cancel.store(true, Ordering::Release);

    assert!(
        wait_until(|| harness.player.stops.load(Ordering::Acquire) > 0),
        "playback was not stopped by the wake word"
    );
    assert!(
        wait_until(|| !harness.tts_active.load(Ordering::Acquire)),
        "capture stayed gated after a barge-in, so the user's next words would be dropped"
    );
    // The flag is consumed, not left set for the next reply to trip over.
    assert!(
        wait_until(|| !harness.tts_cancel.load(Ordering::Acquire)),
        "the cancel flag was never consumed"
    );
    // The queued remainder of the interrupted answer is dropped rather than
    // played after the user has moved on.
    thread::sleep(Duration::from_millis(200));
    assert_eq!(
        harness.player.played().len(),
        1,
        "the rest of the interrupted answer was spoken anyway"
    );
}

#[test]
fn a_server_that_will_not_speak_costs_a_reply_and_not_the_session() {
    // Criterion (d) for TTS: non-fatal, exactly as a missing local voice model
    // is. The failure is logged, nothing is played, capture is never gated,
    // and the next reply is attempted against the same server.
    let calls = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&calls);
    let server = StubSpeechServer::start(move |_: &StubRequest| {
        if counted.fetch_add(1, Ordering::AcqRel) == 0 {
            StubReply::status(500, "voice pack missing")
        } else {
            StubReply::wav(spoken_wav())
        }
    });
    let harness = harness(server.base_url());

    harness
        .pipeline
        .speak("A lost reply.".to_string())
        .expect("speak");
    assert_eq!(server.wait_for_requests(1, WAIT).len(), 1);
    thread::sleep(Duration::from_millis(100));
    assert!(
        harness.player.played().is_empty(),
        "a 500 reached the device"
    );
    assert!(
        !harness.tts_active.load(Ordering::Acquire),
        "a failed reply gated the microphone with nothing playing"
    );

    harness
        .pipeline
        .speak("The next one, on the same server.".to_string())
        .expect("speak");
    assert!(
        wait_until(|| !harness.player.played().is_empty()),
        "the pipeline gave up after one failure"
    );
}

#[test]
fn audio_this_build_cannot_decode_is_dropped_rather_than_played() {
    // A 200 carrying something that is not a WAV this build can play. It must
    // never reach the device as samples, and it must not gate capture.
    let server = StubSpeechServer::always(StubReply::wav(b"RIFFnot really a wav".to_vec()));
    let harness = harness(server.base_url());
    harness
        .pipeline
        .speak("Anything at all.".to_string())
        .expect("speak");

    assert_eq!(server.wait_for_requests(1, WAIT).len(), 1);
    thread::sleep(Duration::from_millis(100));
    assert!(harness.player.played().is_empty());
    assert!(!harness.tts_active.load(Ordering::Acquire));
}

#[test]
fn an_audio_device_that_will_not_open_is_reported_rather_than_silently_mute() {
    // `start_ambient_tts` treats an unavailable pipeline as non-fatal and runs
    // the session without speech. It can only do that if a device failure is
    // an error here rather than a silence discovered on the first reply.
    let error = HttpTtsPipeline::with_player(
        SpeechEndpoint::parse("http://speech.example:30121").expect("endpoint"),
        Box::new(|| Err("no audio device".to_string())),
        Arc::new(AtomicBool::new(false)),
        Arc::new(AtomicBool::new(false)),
    )
    .expect_err("a device that will not open");
    assert!(error.contains("no audio device"), "{error}");
}
