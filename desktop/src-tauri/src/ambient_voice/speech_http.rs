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

use std::io::Read;
use std::time::Duration;

use serde::Serialize;
use url::Position;

use super::speech_wav;

const TRANSCRIPTIONS_PATH: &str = "/v1/audio/transcriptions";
const SPEECH_PATH: &str = "/v1/audio/speech";
const HEALTH_PATH: &str = "/v1/health/ready";

/// The part of an utterance's budget that does not depend on its length.
///
/// The connection, the server's own scheduling, and the model warm-up. The
/// utterance machine has already decided the user stopped talking, so all of
/// this is dead air on the user's side; ten seconds of it is the value this
/// shipped with, and it stays the floor so an ordinary utterance behaves
/// exactly as it did.
const TRANSCRIBE_BASE_TIMEOUT: Duration = Duration::from_secs(10);

/// Milliseconds of budget added for every second of audio uploaded.
///
/// The flat ten seconds was set when an utterance could not exceed thirty, and
/// the silence-hold slider raised that ceiling with it: at the ten-second hold
/// one utterance may carry 230 seconds of audio (see
/// [`super::utterance::MAX_SILENCE_HOLD_MS`]), which is a seven-megabyte upload
/// a server is unlikely to transcribe inside a budget meant for a sentence. A
/// long recording would therefore always have timed out and fallen back to this
/// computer — the server the user chose being used for exactly the utterances
/// it was least likely to be needed for.
///
/// Half a second per second of audio: a server that needs longer than half of
/// what was said to write it down cannot hold a conversation, whatever it
/// eventually answers, and waiting on it is worse than the local recogniser.
const TRANSCRIBE_MS_PER_AUDIO_SECOND: u64 = 500;

/// The most any one utterance may wait, however long it is.
///
/// Set just above what the longest recording this app can make asks for
/// (230 seconds of audio → 125 seconds), so no honest request is clipped by it
/// and a wrong address still cannot hold the audio worker indefinitely.
const TRANSCRIBE_MAX_TIMEOUT: Duration = Duration::from_secs(130);

/// Characters of text that come back as one second of speech.
///
/// Ordinary narration runs at about 150 words a minute, and an English word is
/// close to six characters once its space is counted: 900 characters a minute,
/// fifteen a second. Both reply budgets below are really statements about
/// seconds of speech and reach them through this, because what a reply costs a
/// server — time to make it, bytes to send it — is set by how long it takes to
/// say and not by how much of it there is to read.
const SPOKEN_CHARS_PER_SECOND: u64 = 15;

/// The part of a reply's synthesis budget that does not depend on its length.
///
/// The connection, the server's own scheduling and the model warm-up — the same
/// costs [`TRANSCRIBE_BASE_TIMEOUT`] covers, and twice as many of them because
/// some servers synthesise a reply in one pass and because nothing is waiting on
/// this request: the microphone stays open throughout it. Twenty seconds is the
/// value this shipped with, and it stays the floor so a one-sentence reply
/// behaves exactly as it did.
const SPEAK_BASE_TIMEOUT: Duration = Duration::from_secs(20);

/// Milliseconds of budget added for every second of speech the reply comes to.
///
/// The flat twenty seconds was sized for a reply of a sentence or two, and the
/// silence-hold slider moved the other end of the conversation out from under
/// it: one utterance may now carry 230 seconds of audio (see
/// [`super::utterance::MAX_SILENCE_HOLD_MS`]), and an agent given four minutes
/// of speech to answer answers at length. In testing a server synthesised such
/// a reply in about 24 seconds and answered HTTP 200 — four seconds after this
/// client had hung up, so the finished audio arrived at a closed socket and the
/// user heard nothing at all. The server was not too slow; the budget was flat
/// while the thing it was paying for had grown.
///
/// Two seconds of budget per second of speech, which makes the margin over that
/// observation a ratio rather than a remainder that thins as replies lengthen:
/// the reply above (a minute of speech, some 900 characters) is now given 140
/// seconds where the server needed 24, and a reply twice as long is given twice
/// as much again. Deliberately looser than [`TRANSCRIBE_MS_PER_AUDIO_SECOND`]'s
/// half a second, for a reason about what expiry costs rather than about the
/// work: a transcription that runs out of budget falls back to the on-device
/// recogniser and the utterance survives, while a synthesis that runs out of
/// budget is exactly the silence this constant exists to prevent. There is
/// nothing behind it to catch the reply.
const SPEAK_MS_PER_SPOKEN_SECOND: u64 = 2_000;

/// The most any one reply may wait, however long it is.
///
/// Four minutes, taken from the other end of the same conversation: the longest
/// recording this app can make is 230 seconds, and a reply is not owed more
/// wall clock than the recording that prompted it. Only a reply of close to two
/// minutes of speech — around 1,600 characters — reaches it, so no plausible
/// answer is clipped by it, and past that it does what every ceiling here does:
/// the address is whatever the user typed, and a wrong one may not hold the
/// speaking worker for good.
const SPEAK_MAX_TIMEOUT: Duration = Duration::from_secs(240);

/// Health-probe budget. The user is watching a button, so this fails fast.
const HEALTH_TIMEOUT: Duration = Duration::from_secs(5);

/// How much of a failing server's body is quoted back to the user.
const MAX_DETAIL_CHARS: usize = 200;

/// The most a transcription answer may weigh.
///
/// The answer is `{"text": "…"}` for one utterance — a few hundred bytes at
/// the outside, and a megabyte is about eight hours of speech written down.
/// The cap is not about the honest case: the address is whatever the user
/// typed, so it may be a mistyped host, a captive portal or a server that has
/// been replaced by something else entirely, and none of those are obliged to
/// stop sending. Reading a body to the end with no ceiling makes any of them an
/// out-of-memory kill of the whole app.
const MAX_TRANSCRIPT_BYTES: u64 = 1024 * 1024;

/// The least a synthesised reply may be allowed to weigh.
///
/// Eight megabytes: the flat cap this shipped with, kept as the floor of
/// [`speech_bytes_cap`] so a short reply is bounded exactly as it was. On its
/// own it is about 175 seconds of the audio these servers return, which is
/// generous for a sentence and a wall for a reply to four minutes of speech —
/// audio that arrived whole and inside its budget was discarded for being long.
const SPEECH_BASE_BYTES: u64 = 8 * 1024 * 1024;

/// Bytes one second of a spoken reply is expected to weigh.
///
/// 24 kHz, 16-bit, one channel — what these servers answer with: 48,000 bytes a
/// second.
const SPEECH_BYTES_PER_SPOKEN_SECOND: u64 = 48_000;

/// How much wider than that expectation a reply's cap is drawn.
///
/// The cap is a bound, not a prediction, and it does not have to be a close
/// one: the question it answers is "may this server reply with a gigabyte", not
/// "how big should this WAV be". Four times covers the widest shape an
/// OpenAI-compatible server plausibly answers with — 48 kHz stereo is four
/// times 24 kHz mono — and half as much again covers a voice slower than
/// [`SPOKEN_CHARS_PER_SECOND`], or silence left around the words.
const SPEECH_BYTES_MARGIN: u64 = 6;

/// The most a synthesised reply may weigh, whatever its text says.
///
/// Same reasoning as [`MAX_TRANSCRIPT_BYTES`] — the thing answering may not be
/// a speech server at all, and none of the things it may be instead are obliged
/// to stop sending — so the scaling in [`speech_bytes_cap`] needs an end.
/// Sixty-four megabytes is eight times the floor and some twenty minutes of
/// speech, so no honest reply is near it, and it is a body this app survives:
/// [`speech_wav::decode_pcm16`] turns it into f32 samples twice its size and
/// both are held at once, which is a bad moment for a desktop app rather than a
/// killed process. A gigabyte is what the cap is for.
const MAX_SPEECH_BYTES: u64 = 64 * 1024 * 1024;

/// The most of a failing server's body that is read before it is clipped to
/// [`MAX_DETAIL_CHARS`] for display.
///
/// Its own cap because the error path must not be the way in: a server that
/// answers HTTP 500 with a gigabyte would otherwise be read to the end purely
/// to quote its first 200 characters.
const MAX_ERROR_BODY_BYTES: u64 = 64 * 1024;

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
        .timeout(transcribe_timeout(samples.len(), sample_rate))
        .body(multipart_wav_body(&boundary, "utterance.wav", &wav))
        .send()
        .map_err(|error| format!("speech server did not answer: {error}"))?;

    let status = response.status();
    if !status.is_success() {
        return Err(format!(
            "speech server answered HTTP {}: {}",
            status.as_u16(),
            clip(&error_body(response))
        ));
    }
    let declared = response.content_length();
    let body = read_capped(declared, response, MAX_TRANSCRIPT_BYTES, "transcript")?;
    let body = String::from_utf8_lossy(&body).into_owned();
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
///
/// Both bounds on the answer — how long the server is given, and how much audio
/// it may answer with — are drawn from the length of `text` rather than fixed;
/// see [`speak_timeout`] and [`speech_bytes_cap`]. A reply is as long as the
/// conversation asks it to be, and a flat allowance for something that is not
/// flat is a working server hung up on mid-sentence.
pub(crate) fn synthesize(
    client: &reqwest::blocking::Client,
    endpoint: &SpeechEndpoint,
    text: &str,
) -> Result<Vec<u8>, String> {
    let response = post_to(client, endpoint.speech_url())
        .json(&serde_json::json!({ "input": text }))
        .timeout(speak_timeout(text.len()))
        .send()
        .map_err(|error| format!("speech server did not answer: {error}"))?;

    let status = response.status();
    if !status.is_success() {
        return Err(format!(
            "speech server answered HTTP {}: {}",
            status.as_u16(),
            clip(&error_body(response))
        ));
    }
    let declared = response.content_length();
    read_capped(declared, response, speech_bytes_cap(text.len()), "audio")
}

/// How long this utterance's server is given to answer.
///
/// [`TRANSCRIBE_BASE_TIMEOUT`] plus [`TRANSCRIBE_MS_PER_AUDIO_SECOND`] for each
/// second of audio, up to [`TRANSCRIBE_MAX_TIMEOUT`]. Saturating throughout: the
/// answer is a wait, and no length of buffer may turn into a shorter one.
fn transcribe_timeout(samples: usize, sample_rate: u32) -> Duration {
    // A zero rate is not reachable through the callers — it is a constant — but
    // dividing by it here would be a panic on the audio thread.
    let rate = u64::from(sample_rate.max(1));
    let audio_ms = (samples as u64).saturating_mul(1_000) / rate;
    let for_the_audio = audio_ms.saturating_mul(TRANSCRIBE_MS_PER_AUDIO_SECOND) / 1_000;
    TRANSCRIBE_BASE_TIMEOUT
        .saturating_add(Duration::from_millis(for_the_audio))
        .min(TRANSCRIBE_MAX_TIMEOUT)
}

/// How long the text of one reply takes to say, in milliseconds.
///
/// Counted in UTF-8 bytes rather than characters, and generously on purpose: an
/// ASCII character is one byte and about a fifteenth of a second, while a CJK
/// character is three bytes and a good deal more than a fifteenth of a second
/// to say — so a byte count errs upward in exactly the scripts where a
/// character count would err downward, and is the same number for ASCII.
fn spoken_ms(text_len: usize) -> u64 {
    (text_len as u64).saturating_mul(1_000) / SPOKEN_CHARS_PER_SECOND
}

/// How long this reply's server is given to speak it.
///
/// [`SPEAK_BASE_TIMEOUT`] plus [`SPEAK_MS_PER_SPOKEN_SECOND`] for every second
/// of speech the text comes to, up to [`SPEAK_MAX_TIMEOUT`]. Saturating
/// throughout, for the reason [`transcribe_timeout`] is: the answer is a wait,
/// and no length of text may turn into a shorter one.
fn speak_timeout(text_len: usize) -> Duration {
    let for_the_speech = spoken_ms(text_len).saturating_mul(SPEAK_MS_PER_SPOKEN_SECOND) / 1_000;
    SPEAK_BASE_TIMEOUT
        .saturating_add(Duration::from_millis(for_the_speech))
        .min(SPEAK_MAX_TIMEOUT)
}

/// The most audio this reply is allowed to come back as.
///
/// What its text is expected to weigh at [`SPEECH_BYTES_PER_SPOKEN_SECOND`],
/// widened by [`SPEECH_BYTES_MARGIN`] and then held between
/// [`SPEECH_BASE_BYTES`] and [`MAX_SPEECH_BYTES`]: a short reply keeps the bound
/// this shipped with, a long one is allowed the audio it asked for, and nothing
/// is allowed to be unbounded.
fn speech_bytes_cap(text_len: usize) -> u64 {
    let for_the_speech = spoken_ms(text_len)
        .saturating_mul(SPEECH_BYTES_PER_SPOKEN_SECOND)
        .saturating_mul(SPEECH_BYTES_MARGIN)
        / 1_000;
    for_the_speech.clamp(SPEECH_BASE_BYTES, MAX_SPEECH_BYTES)
}

/// Read a response body, refusing one that is bigger than `limit`.
///
/// Both checks are load-bearing and neither replaces the other: `declared` is
/// what the server claims, which stops a huge body being transferred at all,
/// and the read itself is bounded because `Content-Length` may be absent
/// (chunked), wrong, or a deliberate lie. Reading `limit + 1` is what tells an
/// over-cap body apart from one that exactly fills the cap — truncating
/// silently would hand a half a WAV to the decoder and call it corrupt.
///
/// Over the cap is an `Err`, which every caller already treats as a server
/// failure: the utterance falls back to the on-device recogniser, and the reply
/// simply is not spoken.
fn read_capped(
    declared: Option<u64>,
    reader: impl std::io::Read,
    limit: u64,
    what: &str,
) -> Result<Vec<u8>, String> {
    if declared.is_some_and(|length| length > limit) {
        return Err(format!(
            "speech server offered {} bytes of {what}, over the {limit}-byte limit",
            declared.unwrap_or_default()
        ));
    }
    let mut body = Vec::new();
    reader
        .take(limit + 1)
        .read_to_end(&mut body)
        .map_err(|error| format!("speech server {what} could not be read: {error}"))?;
    if body.len() as u64 > limit {
        return Err(format!(
            "speech server sent more than the {limit}-byte {what} limit"
        ));
    }
    Ok(body)
}

/// A failing server's own words, bounded, for quoting back to the user.
///
/// Never fails: this is already the error path, and an unreadable body is
/// exactly the "(no detail)" case [`clip`] exists for. An error page over the
/// cap is therefore quoted as nothing rather than as its first 200 characters
/// — the same rule as everywhere else here, and the HTTP status, which is the
/// actionable half, is still reported either way.
fn error_body(response: reqwest::blocking::Response) -> String {
    let declared = response.content_length();
    let bytes = read_capped(declared, response, MAX_ERROR_BODY_BYTES, "error").unwrap_or_default();
    String::from_utf8_lossy(&bytes).into_owned()
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
