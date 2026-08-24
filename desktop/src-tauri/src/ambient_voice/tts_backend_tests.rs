//! What actually reaches a voice.
//!
//! `speech_text_tests` pins what the flattener does; these pin that the
//! flattener is *on the path*. A reply is pushed through the real
//! [`AmbientTts`] door into the real HTTP pipeline and a real loopback server,
//! and the assertion is on the bytes that server received — the same place a
//! speech server would see "star star".
//!
//! Only the server-backed arm can be driven here: the local arm is a
//! `huddle::tts::TtsPipeline`, which loads ONNX voices CI does not have. The
//! flattening happens before the match on the variant, so one arm is enough to
//! prove which side of the door it is on.

use std::{
    sync::{atomic::AtomicBool, Arc, Mutex},
    time::Duration,
};

use super::*;
use crate::ambient_voice::http_tts::{HttpTtsPipeline, SpeechPlayer};
use crate::ambient_voice::speech_http::SpeechEndpoint;
use crate::ambient_voice::speech_stub_server::{StubReply, StubRequest, StubSpeechServer};
use crate::ambient_voice::speech_wav::{self, DecodedAudio};

const WAIT: Duration = Duration::from_secs(5);

/// A player that accepts everything and is never "playing", so the worker
/// takes the next reply straight away.
#[derive(Default)]
struct SilentPlayer {
    played: Mutex<usize>,
}

impl SpeechPlayer for Arc<SilentPlayer> {
    fn play(&self, _audio: DecodedAudio) -> Result<(), String> {
        *self
            .played
            .lock()
            .unwrap_or_else(|error| error.into_inner()) += 1;
        Ok(())
    }

    fn is_playing(&self) -> bool {
        false
    }

    fn stop(&self) {}
}

fn speakable_wav() -> Vec<u8> {
    speech_wav::encode_pcm16_mono(&[0.1_f32; 240], 24_000)
}

/// An `AmbientTts` speaking through `server`, with no sound card involved.
fn tts_through(server: &StubSpeechServer) -> AmbientTts {
    let player = Arc::new(SilentPlayer::default());
    let pipeline = HttpTtsPipeline::with_player(
        SpeechEndpoint::parse(server.base_url()).expect("endpoint"),
        Box::new(move || Ok(Box::new(player))),
        Arc::new(AtomicBool::new(false)),
        Arc::new(AtomicBool::new(false)),
    )
    .expect("pipeline");
    AmbientTts::Http(Arc::new(pipeline))
}

fn spoken_input(request: &StubRequest) -> String {
    serde_json::from_slice::<serde_json::Value>(&request.body)
        .ok()
        .and_then(|body| {
            body.get("input")
                .and_then(|input| input.as_str())
                .map(str::to_string)
        })
        .unwrap_or_else(|| request.body_text())
}

#[test]
fn a_markdown_reply_reaches_the_voice_as_plain_text() {
    let server = StubSpeechServer::always(StubReply::wav(speakable_wav()));
    let tts = tts_through(&server);

    tts.speak(
        "## Deploy\nThe **build** on [main](https://example.test/x) is `green`.\n- ship it"
            .to_string(),
    )
    .expect("speak");

    let requests = server.wait_for_requests(1, WAIT);
    let request = requests.first().expect("the reply reached the server");
    assert_eq!(
        spoken_input(request),
        "Deploy.\nThe build on main is green.\nship it."
    );
}

#[test]
fn an_ordinary_reply_is_unchanged_on_its_way_to_the_voice() {
    // The flattener sits in front of every reply, so the reply that carries no
    // Markdown at all has to arrive byte for byte.
    let server = StubSpeechServer::always(StubReply::wav(speakable_wav()));
    let tts = tts_through(&server);

    tts.speak("Your calendar is clear tomorrow.".to_string())
        .expect("speak");

    let requests = server.wait_for_requests(1, WAIT);
    assert_eq!(
        spoken_input(requests.first().expect("one request")),
        "Your calendar is clear tomorrow."
    );
}

#[test]
fn a_reply_that_is_nothing_but_marks_is_not_sent_to_be_synthesised() {
    // A horizontal rule on its own has nothing in it to say. Queueing the
    // empty string would make the server synthesise silence and gate the
    // microphone while it played.
    let server = StubSpeechServer::always(StubReply::wav(speakable_wav()));
    let tts = tts_through(&server);

    tts.speak("---".to_string()).expect("speak");

    std::thread::sleep(Duration::from_millis(200));
    assert!(
        server.requests().is_empty(),
        "an empty reply was sent to the speech server: {:?}",
        server.requests()
    );
}

/// Skipping an empty reply must not cost the pipeline the next one: `speak`
/// returns early, so nothing downstream may be left half-set.
#[test]
fn the_pipeline_still_speaks_after_an_empty_reply_was_skipped() {
    let server = StubSpeechServer::always(StubReply::wav(speakable_wav()));
    let tts = tts_through(&server);

    tts.speak("***".to_string()).expect("speak");
    tts.speak("**Now** for the real one.".to_string())
        .expect("speak");

    let requests = server.wait_for_requests(1, WAIT);
    assert_eq!(requests.len(), 1, "{requests:?}");
    assert_eq!(
        spoken_input(requests.first().expect("one request")),
        "Now for the real one."
    );
}
