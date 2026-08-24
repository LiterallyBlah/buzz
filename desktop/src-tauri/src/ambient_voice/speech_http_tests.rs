//! Wire-contract tests for the HTTP speech backends.
//!
//! Every request assertion is made against a real socket
//! ([`super::super::speech_stub_server`]) rather than a mock built from the
//! same assumptions as the code: the failure mode being guarded against is a
//! request that is well-formed HTTP and wrong — a misnamed multipart part, a
//! path without its `/v1` prefix — which a server accepts silently and answers
//! with nothing useful.

use super::*;
use crate::ambient_voice::speech_stub_server::{StubReply, StubSpeechServer};

const UTTERANCE_RATE: u32 = 16_000;
const WAIT: Duration = Duration::from_secs(5);

fn endpoint(base: &str) -> SpeechEndpoint {
    SpeechEndpoint::parse(base).expect("endpoint")
}

// ── URL handling ─────────────────────────────────────────────────────────────

#[test]
fn a_base_url_becomes_the_three_paths_the_api_defines() {
    let endpoint = endpoint("http://speech.example:30120");
    assert_eq!(
        endpoint.transcriptions_url(),
        "http://speech.example:30120/v1/audio/transcriptions"
    );
    assert_eq!(
        endpoint.speech_url(),
        "http://speech.example:30120/v1/audio/speech"
    );
    assert_eq!(
        endpoint.health_url(),
        "http://speech.example:30120/v1/health/ready"
    );
}

#[test]
fn what_the_user_pastes_is_normalised_into_a_base() {
    // A trailing slash would produce `//v1/audio/speech`, which some servers
    // 404, and a query or fragment pasted from a browser is not part of a base
    // URL. A path prefix, on the other hand, is: servers behind a reverse
    // proxy live under one.
    for typed in [
        "http://speech.example:30120/",
        "  http://speech.example:30120  ",
        "http://speech.example:30120/?token=abc#top",
    ] {
        assert_eq!(
            endpoint(typed).speech_url(),
            "http://speech.example:30120/v1/audio/speech",
            "typed: {typed}"
        );
    }
    assert_eq!(
        endpoint("https://speech.example/speech/").speech_url(),
        "https://speech.example/speech/v1/audio/speech"
    );
}

#[test]
fn an_address_that_could_not_work_is_refused_where_it_was_typed() {
    // Each of these is a plausible thing to type, and each has to come back as
    // a sentence under the field rather than as a session that starts and then
    // fails on every utterance.
    for (typed, expected) in [
        ("", "Enter the server's address"),
        ("   ", "Enter the server's address"),
        // The most common thing to type. It parses as a URL whose scheme is
        // the host name, so only the scheme check catches it.
        ("speech.example:30120", "must start with http"),
        ("ftp://speech.example", "must start with http"),
        ("http://", "not a URL"),
    ] {
        let error = SpeechEndpoint::parse(typed).expect_err(typed);
        assert!(error.contains(expected), "typed {typed:?}: {error}");
    }
}

// ── Transcription ────────────────────────────────────────────────────────────

#[test]
fn an_utterance_is_posted_as_a_wav_file_part_and_the_text_comes_back() {
    // Acceptance criterion (a), everything below the wake word: the buffer the
    // worker captured goes to `/v1/audio/transcriptions` as a multipart `file`
    // part holding a PCM16 WAV, and the server's `text` is what the publisher
    // will send.
    let server = StubSpeechServer::always(StubReply::json(
        r#"{"text": "  what is on my calendar tomorrow  "}"#,
    ));
    let client = blocking_client().expect("client");
    let samples: Vec<f32> = (0..1_600).map(|i| (i as f32 / 40.0).sin() * 0.5).collect();

    let text = transcribe(
        &client,
        &endpoint(server.base_url()),
        &samples,
        UTTERANCE_RATE,
    )
    .expect("transcribe");
    assert_eq!(text, "what is on my calendar tomorrow");

    let requests = server.wait_for_requests(1, WAIT);
    let request = requests.first().expect("one request");
    assert_eq!(request.method, "POST");
    assert_eq!(request.path, "/v1/audio/transcriptions");
    let content_type = request.content_type.clone().unwrap_or_default();
    assert!(
        content_type.starts_with("multipart/form-data; boundary="),
        "{content_type}"
    );

    // The part the API names. A server ignoring a misnamed part looks exactly
    // like a quiet one, which is why this is asserted rather than inferred.
    let body = request.body_text();
    assert!(
        body.contains("name=\"file\"; filename=\"utterance.wav\""),
        "{body:.400}"
    );
    assert!(body.contains("Content-Type: audio/wav"), "{body:.400}");
    let boundary = content_type
        .split("boundary=")
        .nth(1)
        .expect("boundary")
        .to_string();
    assert!(
        body.starts_with(&format!("--{boundary}\r\n")),
        "{body:.200}"
    );
    assert!(body.ends_with(&format!("\r\n--{boundary}--\r\n")), "tail");

    // And the audio inside it is the utterance, not a header the server would
    // decode as silence.
    let wav_start = request
        .body
        .windows(4)
        .position(|window| window == b"RIFF")
        .expect("wav in the part");
    let decoded = crate::ambient_voice::speech_wav::decode_pcm16(&request.body[wav_start..])
        .expect("decode the uploaded wav");
    assert_eq!(decoded.sample_rate, UTTERANCE_RATE);
    assert_eq!(decoded.channels, 1);
    assert_eq!(decoded.samples.len(), samples.len());
    assert!(
        decoded.samples.iter().any(|sample| sample.abs() > 0.1),
        "the uploaded audio was silence"
    );
}

#[test]
fn a_server_that_fails_a_transcription_says_why_and_returns_no_text() {
    // Criterion (d): the caller has to be able to tell "no words" from "the
    // server refused", because only one of them falls back to the local model.
    let server = StubSpeechServer::always(StubReply::status(503, "model is loading"));
    let client = blocking_client().expect("client");
    let error = transcribe(
        &client,
        &endpoint(server.base_url()),
        &[0.0; 320],
        UTTERANCE_RATE,
    )
    .expect_err("503");
    assert!(error.contains("503"), "{error}");
    assert!(error.contains("model is loading"), "{error}");
}

#[test]
fn a_transcription_answer_this_build_cannot_read_is_an_error() {
    for (reply, expected) in [
        (StubReply::json("not json at all"), "did not answer JSON"),
        (
            StubReply::json(r#"{"result": "hello"}"#),
            "without a text field",
        ),
        (StubReply::json(r#"{"text": 12}"#), "without a text field"),
    ] {
        let server = StubSpeechServer::always(reply);
        let client = blocking_client().expect("client");
        let error = transcribe(
            &client,
            &endpoint(server.base_url()),
            &[0.0; 320],
            UTTERANCE_RATE,
        )
        .expect_err("unreadable answer");
        assert!(error.contains(expected), "{error}");
    }
}

#[test]
fn an_address_nothing_is_listening_on_fails_rather_than_hangs() {
    // A port with no server is the ordinary case while a user is typing a URL,
    // and the worker thread must come back from it.
    let dead = {
        let server = StubSpeechServer::always(StubReply::json(r#"{"text": ""}"#));
        server.base_url().to_string()
        // dropped here: the port is closed again
    };
    let client = blocking_client().expect("client");
    let error = transcribe(&client, &endpoint(&dead), &[0.0; 320], UTTERANCE_RATE)
        .expect_err("closed port");
    assert!(error.contains("did not answer"), "{error}");
}

#[test]
fn a_redirect_does_not_carry_the_utterance_to_another_host() {
    // The module guarantees (speech_http.rs module docs) that the microphone
    // audio only ever goes to the base URL the user typed. A 307/308 re-POSTs
    // the identical body, so a configured server that redirects — a compromised
    // box, or a captive portal / transparent proxy intercepting plain http —
    // must NOT be followed: the utterance may not reach the redirect target,
    // and the target's answer may not become the user's transcript.
    let attacker = StubSpeechServer::always(StubReply::json(r#"{"text": "pwned"}"#));
    let configured = StubSpeechServer::always(StubReply::redirect(
        307,
        &format!("{}/v1/audio/transcriptions", attacker.base_url()),
    ));
    let client = blocking_client().expect("client");
    let samples: Vec<f32> = (0..1_600).map(|i| (i as f32 / 40.0).sin() * 0.5).collect();

    let error = transcribe(
        &client,
        &endpoint(configured.base_url()),
        &samples,
        UTTERANCE_RATE,
    )
    .expect_err("a redirect is not a transcript");
    assert!(error.contains("307"), "{error}");
    assert!(
        !error.contains("pwned"),
        "the redirect target's answer became the transcript: {error}"
    );

    // The utterance reached the configured server (we did send it there)…
    assert_eq!(configured.wait_for_requests(1, WAIT).len(), 1);
    // …and never the redirect target.
    assert!(
        attacker.requests().is_empty(),
        "the microphone audio was re-POSTed to the redirect target"
    );
}

// ── Synthesis ────────────────────────────────────────────────────────────────

#[test]
fn a_reply_is_posted_as_json_input_and_comes_back_as_playable_audio() {
    let spoken = crate::ambient_voice::speech_wav::encode_pcm16_mono(&[0.25; 240], 24_000);
    let server = StubSpeechServer::always(StubReply::wav(spoken.clone()));
    let client = blocking_client().expect("client");

    let audio = synthesize(
        &client,
        &endpoint(server.base_url()),
        "Your calendar is clear.",
    )
    .expect("synthesize");
    assert_eq!(audio, spoken);

    let requests = server.wait_for_requests(1, WAIT);
    let request = requests.first().expect("one request");
    assert_eq!(request.method, "POST");
    assert_eq!(request.path, "/v1/audio/speech");
    assert_eq!(
        request.content_type.as_deref(),
        Some("application/json"),
        "{request:?}"
    );
    // The exact JSON the API defines. `voice` and `speed` are deliberately
    // absent: the server's default is the voice the user chose the server for.
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&request.body).expect("json body"),
        serde_json::json!({ "input": "Your calendar is clear." })
    );
}

#[test]
fn a_server_that_will_not_speak_says_why() {
    let server = StubSpeechServer::always(StubReply::status(500, "voice pack missing"));
    let client = blocking_client().expect("client");
    let error = synthesize(&client, &endpoint(server.base_url()), "hello").expect_err("500");
    assert!(error.contains("500"), "{error}");
    assert!(error.contains("voice pack missing"), "{error}");
}

// ── How much a server is allowed to send back ────────────────────────────────

#[test]
fn a_body_bigger_than_its_cap_is_refused_however_the_server_describes_it() {
    // Both halves of the guard, at the seam, because they answer different
    // attacks. `Content-Length` stops a huge body being transferred at all —
    // but it is optional, it can be wrong, and a hostile server can simply
    // lie, so the read itself is bounded too.
    let limit = 8u64;

    // Declared over the cap: refused before a byte of it is read.
    let error = read_capped(Some(limit + 1), &b"ignored"[..], limit, "audio")
        .expect_err("a declared over-cap body");
    assert!(error.contains("over the 8-byte limit"), "{error}");

    // Undeclared and streamed over the cap: refused on what actually arrived.
    let error = read_capped(None, &vec![b'x'; 64][..], limit, "audio")
        .expect_err("an undeclared over-cap body");
    assert!(error.contains("more than the 8-byte"), "{error}");

    // Declared honestly but sent long anyway.
    let error = read_capped(Some(1), &vec![b'x'; 64][..], limit, "audio")
        .expect_err("a body longer than it claimed");
    assert!(error.contains("more than the 8-byte"), "{error}");

    // Exactly the cap is not over it: the boundary must not cost a legitimate
    // answer, which is why the read asks for `limit + 1` rather than `limit`.
    assert_eq!(
        read_capped(Some(limit), &vec![b'x'; 8][..], limit, "audio"),
        Ok(vec![b'x'; 8])
    );
    assert_eq!(read_capped(None, &b""[..], limit, "audio"), Ok(Vec::new()));
}

#[test]
fn an_oversized_transcription_answer_is_a_server_failure_and_not_an_allocation() {
    // The address is whatever the user typed, so the thing answering may not be
    // a speech server at all. Reading its answer to the end with no ceiling
    // makes any of them an out-of-memory kill of the whole app; over the cap is
    // a failure like any other, which the transcriber already answers by
    // falling back to the on-device recogniser.
    let huge = format!(
        r#"{{"text": "{}"}}"#,
        "a".repeat(MAX_TRANSCRIPT_BYTES as usize)
    );
    let server = StubSpeechServer::always(StubReply::json(&huge));
    let client = blocking_client().expect("client");

    let error = transcribe(
        &client,
        &endpoint(server.base_url()),
        &[0.0; 320],
        UTTERANCE_RATE,
    )
    .expect_err("an over-cap transcription answer");
    assert!(error.contains("transcript"), "{error}");
    assert!(error.contains(&MAX_TRANSCRIPT_BYTES.to_string()), "{error}");
}

#[test]
fn an_oversized_speech_answer_is_a_server_failure_and_not_an_allocation() {
    let server = StubSpeechServer::always(StubReply::wav(vec![0u8; MAX_SPEECH_BYTES as usize + 1]));
    let client = blocking_client().expect("client");

    let error =
        synthesize(&client, &endpoint(server.base_url()), "hello").expect_err("an over-cap reply");
    assert!(error.contains("audio"), "{error}");
    assert!(error.contains(&MAX_SPEECH_BYTES.to_string()), "{error}");
}

#[test]
fn an_answer_that_fits_is_still_delivered_whole() {
    // The control for both caps: a body one byte under the limit is not
    // truncated, clipped or refused. A guard that quietly shortened a long but
    // legitimate reply would be a worse bug than the one it prevents.
    let audio = vec![7u8; MAX_SPEECH_BYTES as usize - 1];
    let server = StubSpeechServer::always(StubReply::wav(audio.clone()));
    let client = blocking_client().expect("client");

    assert_eq!(
        synthesize(&client, &endpoint(server.base_url()), "hello"),
        Ok(audio)
    );
}

#[test]
fn a_failing_server_cannot_flood_the_error_path_either() {
    // The message quotes the server's own words, so the error path reads a body
    // too — and would read an unbounded one purely to print its first 200
    // characters.
    let server = StubSpeechServer::always(StubReply::status(
        500,
        &"detail ".repeat(MAX_ERROR_BODY_BYTES as usize),
    ));
    let client = blocking_client().expect("client");

    let error = synthesize(&client, &endpoint(server.base_url()), "hello").expect_err("500");
    // The status still reaches the user — that is the actionable half — but
    // the body was never read, so there is nothing to quote from it. An error
    // page over the cap is refused like any other over-cap body rather than
    // read to the end for its first 200 characters.
    assert!(error.contains("500"), "{error}");
    assert!(error.contains("(no detail)"), "{error}");
    assert!(error.len() < MAX_DETAIL_CHARS, "{}", error.len());

    // A short error page is still quoted in full, so nothing is lost in the
    // case that actually happens.
    let short = StubSpeechServer::always(StubReply::status(500, "voice pack missing"));
    let error = synthesize(&client, &endpoint(short.base_url()), "hello").expect_err("500");
    assert!(error.contains("voice pack missing"), "{error}");
}

// ── Health probe ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn the_check_button_reports_ready_unreachable_and_malformed_apart() {
    let ready = StubSpeechServer::always(StubReply::json(r#"{"ready": true}"#));

    let check = probe_endpoint(ready.base_url()).await;
    assert_eq!(check.status, SpeechEndpointStatus::Ready);
    assert_eq!(check.detail, None);
    assert_eq!(
        check.probed_url.as_deref(),
        Some(format!("{}/v1/health/ready", ready.base_url()).as_str())
    );
    let requests = ready.wait_for_requests(1, WAIT);
    assert_eq!(
        requests.first().map(|request| request.path.as_str()),
        Some("/v1/health/ready")
    );

    // A server that is there but not serving speech is not "ready".
    let refusing = StubSpeechServer::always(StubReply::status(404, "no such path"));
    let check = probe_endpoint(refusing.base_url()).await;
    assert_eq!(check.status, SpeechEndpointStatus::Unreachable);
    assert!(
        check.detail.unwrap_or_default().contains("404"),
        "the user needs the code to tell a wrong URL from a dead box"
    );

    // Nothing listening.
    let closed = {
        let server = StubSpeechServer::always(StubReply::json("{}"));
        server.base_url().to_string()
    };
    assert_eq!(
        probe_endpoint(&closed).await.status,
        SpeechEndpointStatus::Unreachable
    );

    // And a URL that could never be probed is a different answer again: the
    // fault is in the field, not on the network.
    let check = probe_endpoint("speech.example:30120").await;
    assert_eq!(check.status, SpeechEndpointStatus::Malformed);
    assert_eq!(check.probed_url, None);
    assert!(check
        .detail
        .unwrap_or_default()
        .contains("must start with http"));
}

#[test]
fn the_check_result_serialises_in_the_shape_the_frontend_parses() {
    // Pinned from the producing side, as the status report and the model
    // status already are: this feature shipped a frontend written against an
    // invented shape once, and every row rendered "undefined". If this
    // assertion breaks, `SpeechEndpointCheck` in `ambientVoiceApi.ts` and the
    // fixtures in `ambientSpeechBackend.test.mjs` change in the same commit.
    assert_eq!(
        serde_json::to_value(SpeechEndpointCheck::ready(
            "http://speech.example:30120/v1/health/ready".to_string()
        ))
        .expect("json"),
        serde_json::json!({
            "status": "ready",
            "detail": null,
            "probedUrl": "http://speech.example:30120/v1/health/ready",
        })
    );
    assert_eq!(
        serde_json::to_value(SpeechEndpointCheck::malformed(
            "The address is missing a host name".to_string()
        ))
        .expect("json"),
        serde_json::json!({
            "status": "malformed",
            "detail": "The address is missing a host name",
            "probedUrl": null,
        })
    );
    assert_eq!(
        serde_json::to_value(SpeechEndpointCheck::unreachable(
            "http://speech.example:30120/v1/health/ready".to_string(),
            "The server answered HTTP 404 at its health path.".to_string(),
        ))
        .expect("json"),
        serde_json::json!({
            "status": "unreachable",
            "detail": "The server answered HTTP 404 at its health path.",
            "probedUrl": "http://speech.example:30120/v1/health/ready",
        })
    );
}
