//! The wire contract for a speech role that runs on a server.
//!
//! Generic on purpose: this speaks the OpenAI-compatible audio API, so any
//! server offering it will do and no address of ours is written anywhere in
//! the app. What the user types is a **base** URL; the role's path is appended
//! here:
//!
//! | Role | Request | Response |
//! |---|---|---|
//! | STT | `POST {base}/v1/audio/transcriptions`, multipart `file` (PCM16 WAV) | `{"text": "…"}` |
//! | TTS | `POST {base}/v1/audio/speech`, JSON `{"input": "…"}` | WAV bytes |
//! | Check | `GET {base}/v1/health/ready` | any 2xx |
//!
//! STT and TTS carry their own base URL because they are usually separate
//! processes on separate ports.
//!
//! ## Authentication
//!
//! V1 sends no credentials: this is a private-network feature, and a token
//! this app does not hold is a token it cannot leak into a log, a settings
//! file or a crash report. The seam for one is a single line — see
//! [`post_to`], which is the only place either request is built.
//!
//! ## Threading
//!
//! The two request functions are **blocking**, and both callers own a
//! dedicated OS thread (the audio worker and the reply-speaking worker); this
//! is the same reason `huddle::stt` is a thread rather than a task. The health
//! probe is async because its caller is a Tauri command on the runtime.

use std::time::Duration;

use serde::Serialize;
use url::Position;

use super::speech_wav;

const TRANSCRIPTIONS_PATH: &str = "/v1/audio/transcriptions";
const SPEECH_PATH: &str = "/v1/audio/speech";
const HEALTH_PATH: &str = "/v1/health/ready";

/// Upload-to-transcript budget for one utterance.
///
/// The utterance machine has already decided the user stopped talking, so this
/// is dead air on the user's side; a server that has not answered in ten
/// seconds has failed as far as a conversation is concerned, and the local
/// recogniser (when installed) is a better answer than a longer wait.
const TRANSCRIBE_TIMEOUT: Duration = Duration::from_secs(10);

/// Synthesis budget for one reply. Longer than transcription because the text
/// is the whole reply and some servers synthesise it in one pass, and because
/// nothing is waiting on it — the microphone stays open throughout.
const SPEAK_TIMEOUT: Duration = Duration::from_secs(20);

/// Health-probe budget. The user is watching a button, so this fails fast.
const HEALTH_TIMEOUT: Duration = Duration::from_secs(5);

/// How much of a failing server's body is quoted back to the user.
const MAX_DETAIL_CHARS: usize = 200;

/// A speech server's base URL, validated and normalised.
///
/// Held as a type rather than a `String` so "this was checked" is carried in
/// the signature: a role only reaches the request functions with a URL that
/// parsed, and the settings UI reports the parse failure where the user typed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpeechEndpoint {
    base: String,
}

impl SpeechEndpoint {
    /// Validate and normalise what the user typed.
    ///
    /// Normalisation is deliberate rather than cosmetic: a trailing slash
    /// would produce `//v1/audio/speech`, which some servers 404, and a query
    /// or fragment pasted from a browser is not part of a base URL.
    pub fn parse(raw: &str) -> Result<Self, String> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err("Enter the server's address, for example http://your-server:30120".into());
        }
        let parsed = url::Url::parse(trimmed).map_err(|error| {
            format!("That is not a URL ({error}), for example http://your-server:30120")
        })?;
        // A bare `host:port` parses as a URL whose *scheme* is the host name,
        // which is the most common thing to type, so the scheme check is what
        // catches it. A missing host needs no check of its own: `Url::parse`
        // refuses an empty host for http and https.
        if !matches!(parsed.scheme(), "http" | "https") {
            return Err(format!(
                "The address must start with http:// or https://, not {}://",
                parsed.scheme()
            ));
        }
        Ok(Self {
            base: parsed[..Position::AfterPath]
                .trim_end_matches('/')
                .to_string(),
        })
    }

    pub fn transcriptions_url(&self) -> String {
        format!("{}{TRANSCRIPTIONS_PATH}", self.base)
    }

    pub fn speech_url(&self) -> String {
        format!("{}{SPEECH_PATH}", self.base)
    }

    pub fn health_url(&self) -> String {
        format!("{}{HEALTH_PATH}", self.base)
    }
}

/// What a "Check" of a speech endpoint found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SpeechEndpointCheck {
    pub status: SpeechEndpointStatus,
    /// Shown verbatim under the field. `None` only when the server is ready.
    pub detail: Option<String>,
    /// The URL actually probed, so the user can see what was made of what they
    /// typed. `None` when nothing could be derived from it.
    pub probed_url: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SpeechEndpointStatus {
    /// The health path answered 2xx.
    Ready,
    /// What the user typed is not a usable base URL.
    Malformed,
    /// A well-formed address that did not answer, or answered an error.
    Unreachable,
}

impl SpeechEndpointCheck {
    fn ready(probed_url: String) -> Self {
        Self {
            status: SpeechEndpointStatus::Ready,
            detail: None,
            probed_url: Some(probed_url),
        }
    }

    fn malformed(detail: String) -> Self {
        Self {
            status: SpeechEndpointStatus::Malformed,
            detail: Some(detail),
            probed_url: None,
        }
    }

    fn unreachable(probed_url: String, detail: String) -> Self {
        Self {
            status: SpeechEndpointStatus::Unreachable,
            detail: Some(detail),
            probed_url: Some(probed_url),
        }
    }
}

/// Build the blocking client a worker thread uses for one session.
///
/// One client per worker, not one per request: each carries a connection pool,
/// and the whole latency argument for a server backend rests on reusing the
/// connection rather than paying a handshake per utterance.
pub(crate) fn blocking_client() -> Result<reqwest::blocking::Client, String> {
    reqwest::blocking::Client::builder()
        // A speech request carries the microphone audio (STT) or the reply text
        // (TTS). Neither may leave for an address the user did not type, so a
        // 307/308 that would re-POST the body to a redirect target is refused,
        // not chased, and the ambient proxy environment cannot reroute it
        // either. This matches the app's other outbound clients
        // (`app_state::build_media_fetch_client`, `commands::link_preview`).
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .build()
        .map_err(|error| format!("could not build the speech HTTP client: {error}"))
}

/// Start a request to a speech endpoint.
///
/// The single door every outgoing speech request goes through, and therefore
/// the auth seam: an upstream build that needs a credential adds an optional
/// token to `SpeechBackendSettings`, threads it to here, and attaches
/// `.bearer_auth(token)` to this builder. V1 deliberately holds no token.
fn post_to(client: &reqwest::blocking::Client, url: String) -> reqwest::blocking::RequestBuilder {
    client.post(url)
}

/// Send one utterance for transcription.
///
/// `samples` are the worker's 16 kHz mono buffer; they are encoded as a PCM16
/// WAV because that is what the OpenAI audio API takes and what every server
/// implementing it can decode without negotiation.
pub(crate) fn transcribe(
    client: &reqwest::blocking::Client,
    endpoint: &SpeechEndpoint,
    samples: &[f32],
    sample_rate: u32,
) -> Result<String, String> {
    let wav = speech_wav::encode_pcm16_mono(samples, sample_rate);
    let boundary = format!("buzz-ambient-{}", uuid::Uuid::new_v4().simple());
    let response = post_to(client, endpoint.transcriptions_url())
        .header(
            reqwest::header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={boundary}"),
        )
        .timeout(TRANSCRIBE_TIMEOUT)
        .body(multipart_wav_body(&boundary, "utterance.wav", &wav))
        .send()
        .map_err(|error| format!("speech server did not answer: {error}"))?;

    let status = response.status();
    let body = response
        .text()
        .map_err(|error| format!("speech server response could not be read: {error}"))?;
    if !status.is_success() {
        return Err(format!(
            "speech server answered HTTP {}: {}",
            status.as_u16(),
            clip(&body)
        ));
    }
    let parsed: serde_json::Value = serde_json::from_str(&body).map_err(|error| {
        format!(
            "speech server did not answer JSON ({error}): {}",
            clip(&body)
        )
    })?;
    let text = parsed
        .get("text")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            format!(
                "speech server answered without a text field: {}",
                clip(&body)
            )
        })?;
    Ok(text.trim().to_string())
}

/// Ask the server to speak `text`, and return the WAV it answers with.
///
/// `voice` and `speed` are optional in the API and deliberately not sent: the
/// server's own default is the voice the user picked when they chose that
/// server, and the local voice rows in settings describe the local model.
pub(crate) fn synthesize(
    client: &reqwest::blocking::Client,
    endpoint: &SpeechEndpoint,
    text: &str,
) -> Result<Vec<u8>, String> {
    let response = post_to(client, endpoint.speech_url())
        .json(&serde_json::json!({ "input": text }))
        .timeout(SPEAK_TIMEOUT)
        .send()
        .map_err(|error| format!("speech server did not answer: {error}"))?;

    let status = response.status();
    if !status.is_success() {
        let body = response.text().unwrap_or_default();
        return Err(format!(
            "speech server answered HTTP {}: {}",
            status.as_u16(),
            clip(&body)
        ));
    }
    response
        .bytes()
        .map(|bytes| bytes.to_vec())
        .map_err(|error| format!("speech audio could not be read: {error}"))
}

/// Build the async client the health probe uses.
///
/// Its own client, not the app-wide `http_client`, and with the same redirect
/// and proxy stance as [`blocking_client`]: the Check button exists to tell the
/// user whether the address they typed answers, so it must reach that address
/// and not one a redirect or a proxy substitutes for it.
fn probe_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .build()
        .map_err(|error| format!("could not build the speech probe client: {error}"))
}

/// Ask a candidate endpoint whether it is there.
///
/// Async because its caller is a Tauri command: a blocking request on the
/// runtime would stall every other command for as long as an unreachable
/// address takes to time out.
pub(crate) async fn probe_endpoint(raw: &str) -> SpeechEndpointCheck {
    let endpoint = match SpeechEndpoint::parse(raw) {
        Ok(endpoint) => endpoint,
        Err(detail) => return SpeechEndpointCheck::malformed(detail),
    };
    let url = endpoint.health_url();
    let client = match probe_client() {
        Ok(client) => client,
        Err(detail) => return SpeechEndpointCheck::unreachable(url, detail),
    };
    match client.get(&url).timeout(HEALTH_TIMEOUT).send().await {
        Ok(response) if response.status().is_success() => SpeechEndpointCheck::ready(url),
        Ok(response) => SpeechEndpointCheck::unreachable(
            url,
            format!(
                "The server answered HTTP {} at its health path.",
                response.status().as_u16()
            ),
        ),
        Err(error) => SpeechEndpointCheck::unreachable(url, format!("{error}")),
    }
}

/// One `file` part holding `wav`, framed for `multipart/form-data`.
///
/// Hand-built rather than through a form encoder: it is six lines, the field
/// name and filename are ours, and a body assembled here is a body the test
/// can assert byte for byte — which is the whole point, since a server that
/// silently ignores a misnamed part would look exactly like a quiet one.
fn multipart_wav_body(boundary: &str, filename: &str, wav: &[u8]) -> Vec<u8> {
    let mut body = Vec::with_capacity(wav.len() + 256);
    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    body.extend_from_slice(
        format!("Content-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\n")
            .as_bytes(),
    );
    body.extend_from_slice(b"Content-Type: audio/wav\r\n\r\n");
    body.extend_from_slice(wav);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    body
}

/// A server's own words, cut to something an indicator can show.
fn clip(body: &str) -> String {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return "(no detail)".to_string();
    }
    trimmed.chars().take(MAX_DETAIL_CHARS).collect()
}

#[cfg(test)]
#[path = "speech_http_tests.rs"]
mod speech_http_tests;
