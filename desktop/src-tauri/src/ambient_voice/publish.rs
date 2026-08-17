//! Relay publishing for the ambient session.
//!
//! Ambient speech becomes an **ordinary** channel message: a kind:9 event with
//! an `h` tag for the destination and a `p` tag for the bound agent, submitted
//! over the same guarded HTTP path the huddle transcriber uses. Nothing about
//! the transport is special-cased for voice — that is the point of the design,
//! and it is why the transcript reads back as a normal DM thread.
//!
//! This is egress boundary 9 (see `crate::egress_guard`). The key-backup guard
//! is called deliberately at [`sign_and_guard_ambient_body`]: the structural
//! tripwire in `egress_guard_tests.rs` pairs `/events` sites with guard calls
//! per file, so this module's inventory row is what keeps the two together.

use std::sync::Arc;

use nostr::JsonUtil;
use uuid::Uuid;

use crate::app_state::AppState;
use crate::events;

/// Ceiling on a single transcribed utterance, in characters.
///
/// The utterance machine already caps capture at 30 s of audio; this is the
/// belt-and-braces text-side bound so a pathological transcript cannot be
/// posted as an enormous message.
const MAX_TRANSCRIPT_CHARS: usize = 2_000;

/// Voice-mode guidelines posted as kind:48106 before the first ambient
/// message of a session.
///
/// Agents already honour this event from huddles (`huddle::agents`), so an
/// ambient conversation gets speakable replies with no agent-side change. The
/// text differs from the huddle variant in the ways the situations differ:
/// there is one human and one agent, the exchange is hands-free, and the human
/// interrupts by saying the wake word rather than by pressing anything.
pub fn ambient_voice_guidelines(wake_word: &str) -> String {
    format!(
        "\
You are in a hands-free ambient voice conversation with one person.
Their speech reaches you as ordinary messages here; your replies are read
aloud by text-to-speech, message by message, in the order sent.

Latency matters most: reply IMMEDIATELY — do not compose your full reply
before sending anything. The moment your first sentence is formed, send it
as its own `buzz messages send` tool call. Then send each following sentence
the same way — one sentence per separate call.

- Keep the whole reply short — a few sentences at most. Start with the answer, no preamble.
- No markdown, code blocks, lists, or structured data — say it naturally.
- To share code or detailed data: say \"I'll post that in the main channel\" and do so.
- When you need a tool, say one short sentence first (e.g. \"Let me check.\"), then run it, then summarize the key finding verbally.
- The person speaks to you by saying \"{wake_word}\" first, so a message arriving while you are mid-reply means they interrupted you: drop your unsent sentences and answer the new message instead.
- Speech-to-text makes mistakes. If a message reads like a mis-hearing, ask for a repeat rather than guessing.
- Use your Buzz tools proactively when asked."
    )
}

/// Trim a transcript to something publishable, or `None` if there is nothing
/// worth sending.
pub fn normalize_transcript(text: &str) -> Option<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.chars().count() <= MAX_TRANSCRIPT_CHARS {
        return Some(trimmed.to_string());
    }
    Some(trimmed.chars().take(MAX_TRANSCRIPT_CHARS).collect())
}

/// Sign an ambient event and produce the guarded POST body.
///
/// Factored out exactly as `huddle::pipeline::sign_and_guard_stt_body` is, so
/// egress boundary 9 has one directly testable seam and the guard provably
/// runs before any bytes can reach the network.
pub(crate) fn sign_and_guard_ambient_body(
    builder: nostr::EventBuilder,
    keys: &nostr::Keys,
) -> Result<Vec<u8>, String> {
    let event = builder
        .sign_with_keys(keys)
        .map_err(|e| format!("sign event: {e}"))?;
    let body_bytes = event.as_json().into_bytes();
    crate::egress_guard::assert_no_key_backup_bytes(&body_bytes, "ambient voice publish")?;
    Ok(body_bytes)
}

/// Everything the publisher needs, captured once so the audio worker never
/// touches `AppState` locks from its own thread.
#[derive(Clone)]
pub(crate) struct AmbientPublisher {
    http_client: reqwest::Client,
    keys: nostr::Keys,
    relay_base_url: String,
}

impl AmbientPublisher {
    pub(crate) fn from_state(state: &AppState) -> Result<Self, String> {
        let keys = state.signing_keys()?;
        Ok(Self {
            http_client: state.http_client.clone(),
            keys,
            relay_base_url: crate::relay::relay_api_base_url_with_override(state),
        })
    }

    /// Sign, guard, authenticate and POST one event.
    ///
    /// Order is deliberate and matches the huddle transcriber: wait on the
    /// relay admission gate FIRST, then sign, then mint the auth header, so
    /// both timestamps are fresh when the request leaves.
    pub(crate) async fn post(
        &self,
        builder: nostr::EventBuilder,
        what: &'static str,
    ) -> Result<(), String> {
        crate::relay_admission::wait_for_rate_limit().await;
        let body_bytes = sign_and_guard_ambient_body(builder, &self.keys)?;
        self.post_body(body_bytes, what).await
    }

    async fn post_body(&self, body_bytes: Vec<u8>, what: &'static str) -> Result<(), String> {
        let url = format!("{}/events", self.relay_base_url);
        let auth_header = crate::relay::build_nip98_auth_header_for_keys(
            &self.keys,
            &reqwest::Method::POST,
            &url,
            &body_bytes,
        )
        .map_err(|e| format!("ambient {what} auth: {e}"))?;

        let response = self
            .http_client
            .post(&url)
            .header("Authorization", auth_header)
            .header("Content-Type", "application/json")
            .body(body_bytes)
            .send()
            .await;

        match response {
            Ok(resp) if resp.status().is_success() => Ok(()),
            // Route through relay_error_message so a 429 arms the admission
            // gate for every subsequent relay send, ambient or not.
            Ok(resp) => Err(format!(
                "ambient {what} rejected: {}",
                crate::relay::relay_error_message(resp).await
            )),
            Err(e) => Err(format!("ambient {what} failed: {e}")),
        }
    }

    /// Post the kind:48106 ambient guidelines to `channel_id`.
    pub(crate) async fn publish_guidelines(
        &self,
        channel_id: &str,
        wake_word: &str,
    ) -> Result<(), String> {
        let builder =
            events::build_voice_guidelines(channel_id, &ambient_voice_guidelines(wake_word))?;
        self.post(builder, "guidelines").await
    }

    /// Post one transcribed utterance as a kind:9 message, p-tagging the agent.
    pub(crate) async fn publish_transcript(
        &self,
        channel_id: Uuid,
        agent_pubkey: &str,
        text: &str,
    ) -> Result<(), String> {
        let Some(content) = normalize_transcript(text) else {
            return Ok(());
        };
        let builder =
            events::build_message(channel_id, &content, None, &[agent_pubkey], &[], &[], &[])?;
        self.post(builder, "transcript").await
    }
}

/// Resolve the destination channel for a binding.
///
/// `None` destination means "the DM with this agent", which the relay creates
/// or returns idempotently for a kind:41010 dm-open. A stored channel id is
/// used verbatim (a later milestone's channel destinations).
pub(crate) async fn resolve_destination(
    state: &AppState,
    agent_pubkey: &str,
    destination: Option<&str>,
) -> Result<String, String> {
    if let Some(channel_id) = destination {
        Uuid::parse_str(channel_id)
            .map_err(|_| "ambient destination is not a channel".to_string())?;
        return Ok(channel_id.to_string());
    }
    let builder = events::build_dm_open(&[agent_pubkey.to_string()])?;
    let result = crate::relay::submit_event(builder, state).await?;
    let ack: DmOpenAck = crate::relay::parse_command_response(&result.message)?;
    Uuid::parse_str(&ack.channel_id)
        .map_err(|_| "relay returned a malformed DM channel id".to_string())?;
    Ok(ack.channel_id)
}

#[derive(serde::Deserialize)]
struct DmOpenAck {
    channel_id: String,
}

/// A publisher plus its per-session guidelines bookkeeping.
///
/// Guidelines are posted at most once per session per destination: agents read
/// them via replay when they subscribe, so re-posting on every utterance would
/// be noise in the user's DM thread.
pub(crate) struct AmbientDestination {
    pub(crate) channel_id: Uuid,
    pub(crate) agent_pubkey: String,
    pub(crate) wake_word: String,
    pub(crate) guidelines_sent: Arc<std::sync::atomic::AtomicBool>,
}

impl AmbientDestination {
    /// Publish one utterance, preceded by the guidelines on the first send.
    ///
    /// A guidelines failure is logged but never blocks the transcript: the
    /// user said something and it must reach the agent even if the etiquette
    /// event did not.
    pub(crate) async fn publish(&self, publisher: &AmbientPublisher, text: &str) {
        use std::sync::atomic::Ordering;
        if !self.guidelines_sent.swap(true, Ordering::AcqRel) {
            if let Err(error) = publisher
                .publish_guidelines(&self.channel_id.to_string(), &self.wake_word)
                .await
            {
                eprintln!("buzz-desktop: ambient guidelines (kind:48106) failed: {error}");
                // Allow a retry on the next utterance rather than silently
                // running the whole session without guidelines.
                self.guidelines_sent.store(false, Ordering::Release);
            }
        }
        if let Err(error) = publisher
            .publish_transcript(self.channel_id, &self.agent_pubkey, text)
            .await
        {
            eprintln!("buzz-desktop: ambient transcript publish failed: {error}");
        }
    }
}

#[cfg(test)]
#[path = "publish_tests.rs"]
mod publish_tests;
