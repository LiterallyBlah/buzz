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

/// Ceiling on a single transcribed utterance, in UTF-8 bytes.
///
/// One utterance is one kind:9 message, so the bound is the relay's: it
/// advertises `max_content_len` 65,536 in its NIP-11 document, and this stops
/// 4 KiB short of that so no supported transcript is ever the relay's to
/// refuse. Rolling capture made the old 2,000-character cap reachable —
/// sixty seconds of ordinary speech — and it was applied by silently cutting
/// the transcript's tail off, which is the user's words, edited, with nothing
/// to say so. Sixty kibibytes is over an hour of continuous speech at the
/// fifteen characters a second `speech_http` budgets by, so no utterance a
/// person can produce is near it; what it bounds is a pathological
/// transcription server, and [`super::rolling`] fails such an utterance
/// loudly at this same line long before it reaches here. This copy of the
/// bound is the belt-and-braces: over it is an explicit error, never a trim.
pub(crate) const MAX_UTTERANCE_TEXT_BYTES: usize = 60 * 1024;

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

/// Pass a transcript through whole, or say exactly why it cannot be sent.
///
/// `Ok(None)` is audio that carried no words — an ordinary outcome, nothing to
/// send. `Err` is a transcript over [`MAX_UTTERANCE_TEXT_BYTES`], which the
/// capture pipeline fails loudly before publication ever sees it; if one
/// arrives here anyway it is refused with a reason, because the one thing this
/// function may never do is deliver the user's words with the end cut off and
/// nothing to show for it.
pub fn normalize_transcript(text: &str) -> Result<Option<String>, String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    if trimmed.len() > MAX_UTTERANCE_TEXT_BYTES {
        return Err(format!(
            "transcript is {} bytes, over the {} this app will post as one message",
            trimmed.len(),
            MAX_UTTERANCE_TEXT_BYTES
        ));
    }
    Ok(Some(trimmed.to_string()))
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
        let request = self.build_post(body_bytes, what)?;
        relay_outcome(request.send().await, what).await
    }

    /// Authenticate a signed body and build the request that carries it.
    ///
    /// Separate from sending it because the transcript path must have the
    /// whole request in hand — signed, authenticated, built — *before* it takes
    /// the authority gate: everything that can fail, wait or allocate happens
    /// out here, so the critical section below contains one decision and one
    /// spawn and nothing else.
    fn build_post(
        &self,
        body_bytes: Vec<u8>,
        what: &'static str,
    ) -> Result<reqwest::RequestBuilder, String> {
        let url = format!("{}/events", self.relay_base_url);
        let auth_header = crate::relay::build_nip98_auth_header_for_keys(
            &self.keys,
            &reqwest::Method::POST,
            &url,
            &body_bytes,
        )
        .map_err(|e| format!("ambient {what} auth: {e}"))?;

        Ok(self
            .http_client
            .post(&url)
            .header("Authorization", auth_header)
            .header("Content-Type", "application/json")
            .body(body_bytes))
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
    ///
    /// The transcript may have waited behind the guidelines POST and the relay
    /// admission gate, and a mute that landed anywhere in that time means the
    /// user muted this capture — it does not get to speak because it was
    /// already in the pipeline. This is the last hand on the fence that starts
    /// at `finish_capture`, and what makes it a fence rather than a narrower
    /// window is [`DispatchGate`]: the request is prepared in full, and only
    /// then is the gate taken, the epoch checked, and the send **started while
    /// the gate is still held**. A mute is exactly as fast as it was — it takes
    /// the same lock for the length of two stores, never for a round trip — but
    /// it is now ordered with respect to this dispatch instead of racing it.
    /// The check before the signing stays as an advisory: there is no point
    /// signing and authenticating a capture that is already dead.
    pub(crate) async fn publish_transcript(
        &self,
        channel_id: Uuid,
        agent_pubkey: &str,
        text: &str,
        gate: &DispatchGate<'_>,
    ) -> Result<(), String> {
        let Some(content) = normalize_transcript(text)? else {
            return Ok(());
        };
        let builder = events::build_message(
            channel_id,
            &content,
            None,
            &[agent_pubkey],
            &[],
            &[],
            &[],
            &[],
            None,
            &self.relay_base_url,
        )?;
        // The same steps `post` takes, unrolled so the whole request exists
        // before the gate is taken. Signing stays after the admission wait so
        // both timestamps are fresh when the request leaves.
        crate::relay_admission::wait_for_rate_limit().await;
        if !(gate.still_wanted)() {
            return Ok(());
        }
        let body_bytes = sign_and_guard_ambient_body(builder, &self.keys)?;
        let request = self.build_post(body_bytes, "transcript")?;
        #[cfg(test)]
        gate.wait_for_test_hold().await;
        let dispatch = {
            let _authority = gate.take_authority();
            if !(gate.still_wanted)() {
                // A mute, or the teardown of the session these words were
                // spoken into, got here first. Both counters are bumped under
                // this same lock, so this answer cannot be stale, and the
                // prepared request is dropped unsent.
                return Ok(());
            }
            // Started, not awaited: by the time the gate is released the send
            // is irrevocably under way, so a mute waiting on the lock is
            // ordered *after* this transcript and takes effect from the next
            // one. Nothing is awaited inside the critical section — a mute must
            // never wait on a network round trip.
            tauri::async_runtime::spawn(async move { request.send().await })
        };
        let response = dispatch
            .await
            .map_err(|error| format!("ambient transcript dispatch failed: {error}"))?;
        relay_outcome(response, "transcript").await
    }
}

/// Turn the relay's answer — or the absence of one — into this module's result.
async fn relay_outcome(
    response: Result<reqwest::Response, reqwest::Error>,
    what: &'static str,
) -> Result<(), String> {
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

/// The authority a prepared transcript needs before its bytes may leave.
///
/// Two things, because either alone leaves a race. `still_wanted` answers
/// *whether* these words are still allowed out, and it is two questions in one
/// closure: the capture's mute epoch is unmoved
/// ([`super::session::transcript_still_wanted`]) **and** the session that
/// captured them is still the live one (the generation
/// `super::spawn_publisher_task` was spawned under). `authority` answers *in
/// which order* a revocation and a dispatch happened when they happened at the
/// same moment: [`super::session::apply_mute`] bumps the epoch and
/// `super::stop_session` bumps the generation, each while holding this same
/// lock, so one side wins outright. Either the revoking stores land first and
/// the check under the lock sees them, or the send is already under way and the
/// revocation governs the next transcript instead. An unsynchronised second
/// look at either counter would only make the window narrower, and a narrower
/// window is still a window: the words a user muted — or spoke into a session
/// they have since switched off — would still, sometimes, be sent.
///
/// Only transcripts pass through here. The kind:48106 guidelines are not
/// fenced — they carry no speech, only the etiquette an agent needs to answer
/// in a voice conversation, and holding them against the mute epoch would give
/// a muted session no way to be ready for the next thing the user says.
pub(crate) struct DispatchGate<'a> {
    authority: &'a std::sync::Mutex<()>,
    still_wanted: &'a (dyn Fn() -> bool + Send + Sync),
    /// Where a test stops the publisher between preparation and dispatch.
    /// Absent from release builds along with the branch that reads it.
    #[cfg(test)]
    hold: Option<&'a DispatchHold>,
}

impl<'a> DispatchGate<'a> {
    /// The gate a live session publishes through.
    pub(crate) fn new(
        authority: &'a std::sync::Mutex<()>,
        still_wanted: &'a (dyn Fn() -> bool + Send + Sync),
    ) -> Self {
        Self {
            authority,
            still_wanted,
            #[cfg(test)]
            hold: None,
        }
    }

    /// Hold the authority for the length of one dispatch decision.
    ///
    /// Poisoning is recovered rather than propagated: a panic elsewhere under
    /// this lock must not be able to stop the user's words being fenced, and
    /// the data it guards is `()` — there is no invariant left to be broken.
    fn take_authority(&self) -> std::sync::MutexGuard<'_, ()> {
        self.authority
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Park at the hold point a test installed, if it installed one.
    #[cfg(test)]
    async fn wait_for_test_hold(&self) {
        if let Some(hold) = self.hold {
            hold.reached.notify_one();
            hold.released.notified().await;
        }
    }
}

/// A test's grip on the moment between a prepared request and its dispatch.
///
/// The window this fence closes is a few instructions wide, so the regression
/// that proves it closed has to be able to stop the publisher inside it and
/// mute there. Compiled only for tests, like the branch that consults it.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct DispatchHold {
    /// Fires when the publisher has a complete request and has arrived at the
    /// gate with it.
    pub(crate) reached: tokio::sync::Notify,
    /// Awaited there until the test lets go.
    pub(crate) released: tokio::sync::Notify,
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
    pub(crate) async fn publish(
        &self,
        publisher: &AmbientPublisher,
        text: &str,
        gate: &DispatchGate<'_>,
    ) {
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
            .publish_transcript(self.channel_id, &self.agent_pubkey, text, gate)
            .await
        {
            eprintln!("buzz-desktop: ambient transcript publish failed: {error}");
        }
    }
}

#[cfg(test)]
#[path = "publish_tests.rs"]
mod publish_tests;
