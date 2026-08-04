//! The sender half of `buzz-acp`'s drain: an owner-signed control frame that
//! says *stop admitting, finish the batch, then exit 0*.
//!
//! The receiving contract is normative and lives in
//! `crates/buzz-acp/src/drain.rs`. This module is the reference sender for it,
//! and every field below is a transcription of that document rather than a
//! choice made here:
//!
//! | field | value |
//! |---|---|
//! | kind | `24200` — the agent observer frame |
//! | tags | `["p", <agent>]`, `["agent", <agent>]`, `["frame", "control"]` |
//! | content | NIP-44 v2 ciphertext, **caller's secret → agent pubkey** |
//! | plaintext | `{"type":"drain"}`, or `{"type":"drain","reason":"…"}` |
//! | `created_at` | now; the agent rejects anything beyond ±300 s |
//! | signature | the caller's key, which **must be the agent's owner** |
//!
//! Both `p` and `agent` carry the *agent's* pubkey. `p` is what the relay
//! routes on and what the agent's control REQ filters for; `agent` names whose
//! observer stream the frame belongs to. They are the same value here and
//! different values in `agent_management.rs` (where an agent writes *to* its
//! owner), which is exactly why the builder takes them separately.
//!
//! ## What this cannot do, and why the command says so out loud
//!
//! Publishing proves the relay accepted the frame. It does not prove the agent
//! received it, honoured it, or finished draining — that is process-side, and
//! the only evidence of it is the agent's own log line and its exit. A sender
//! that reported "drained" on a successful publish would be asserting something
//! it has no way to observe, and the deployer that trusts it would install a
//! binary over a process still mid-turn. So the ack is deliberately narrow: it
//! reports delivery, names what delivery does not mean, and leaves the waiting
//! to the caller (`deploy.sh` polls `systemctl is-active`).
//!
//! ## Why there is no owner check here
//!
//! The frame must be signed by the agent's *resolved owner* — but which pubkey
//! that is, is the agent's belief, not the CLI's. `handle_relay_observer_control_event`
//! drops any control frame whose author is not the owner it resolved, so a
//! wrongly-signed drain is refused where the truth lives. Re-deciding it here
//! from an ambient `BUZZ_AUTH_TAG` would add a second opinion that can be wrong
//! in a direction the first one cannot: it would refuse to send a frame the
//! agent would have honoured.

use buzz_core::observer::{encrypt_observer_payload, OBSERVER_FRAME_CONTROL};
use nostr::{Event, Keys, PublicKey};
use serde::Serialize;

use crate::error::CliError;

/// The payload `type` that names a drain, matching
/// `buzz_acp::drain::CONTROL_TYPE_DRAIN`. A constant on both sides because the
/// wire contract is one string and two implementations of it would eventually
/// be two strings.
pub const DRAIN_CONTROL_TYPE: &str = "drain";

/// How long an operator-supplied reason may be.
///
/// The runtime trims a reason to 200 characters before it reaches a log line
/// (`buzz_acp::drain::REASON_LOG_CAP`) and the frame as a whole is capped at
/// 64 KiB, so anything past a few hundred characters is write-only: it travels,
/// costs bytes, and is never read back. Refused here rather than trimmed
/// because the operator is standing at the terminal with nothing in flight yet
/// — an error they can fix beats an audit trail that quietly lost its second
/// half. The runtime makes the opposite choice for the opposite reason: by then
/// the instruction is more important than the label on it.
const MAX_REASON_CHARS: usize = 500;

/// The drain payload, exactly as the runtime's `payload.get("type")` match
/// expects it: a flat object, not the `ObserverEvent` telemetry envelope that
/// `agent_management.rs` wraps its requests in. Control frames and telemetry
/// frames share a kind and a tag set but not a payload shape, and the runtime
/// reads `type` at the top level.
#[derive(Debug, Serialize)]
struct DrainPayload<'a> {
    #[serde(rename = "type")]
    payload_type: &'static str,
    /// Omitted entirely when absent rather than sent as `null`: the contract
    /// spells the reason as optional, and a null would be a value the runtime's
    /// `as_str()` has to decline.
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<&'a str>,
}

/// A signed drain frame and the agent it names.
#[derive(Debug)]
pub struct BuiltDrainFrame {
    pub event: Event,
    /// Lowercase hex, re-derived from the parsed key rather than echoed from
    /// the argument, so the ack reports the pubkey the frame actually carries.
    pub agent: String,
}

/// Build and sign a drain frame for `agent_pubkey`.
///
/// `keys` is the caller's identity (`BUZZ_PRIVATE_KEY`), which is both the
/// signer and the NIP-44 sender. For a deploy that is the owner's key; see the
/// module docs for why this function does not try to prove that.
pub fn build_drain(
    keys: &Keys,
    agent_pubkey: &str,
    reason: Option<&str>,
) -> Result<BuiltDrainFrame, CliError> {
    let agent = PublicKey::parse(agent_pubkey)
        .map_err(|error| CliError::Usage(format!("invalid --agent pubkey: {error}")))?;
    let agent_hex = agent.to_hex().to_ascii_lowercase();

    // An empty or whitespace-only `--reason` is the same statement as no
    // reason, so it travels as none. Sending `"reason": ""` would put a field
    // in the agent's log line that says nothing.
    let reason = reason.map(str::trim).filter(|value| !value.is_empty());
    if let Some(value) = reason {
        let chars = value.chars().count();
        if chars > MAX_REASON_CHARS {
            return Err(CliError::Usage(format!(
                "--reason is {chars} characters; the maximum is {MAX_REASON_CHARS} \
                 (the agent logs only the first 200)"
            )));
        }
    }

    let payload = DrainPayload {
        payload_type: DRAIN_CONTROL_TYPE,
        reason,
    };
    let encrypted = encrypt_observer_payload(keys, &agent, &payload)
        .map_err(|error| CliError::Other(format!("could not encrypt drain frame: {error}")))?;
    let event = buzz_sdk::build_agent_observer_frame(
        &agent_hex,
        &agent_hex,
        OBSERVER_FRAME_CONTROL,
        &encrypted,
    )
    .map_err(|error| CliError::Other(format!("could not build drain frame: {error}")))?
    .sign_with_keys(keys)
    .map_err(|error| CliError::Other(format!("could not sign drain frame: {error}")))?;

    Ok(BuiltDrainFrame {
        event,
        agent: agent_hex,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use buzz_core::observer::{
        decrypt_observer_payload, OBSERVER_AGENT_TAG, OBSERVER_FRAME_TAG, OBSERVER_FRAME_TELEMETRY,
    };

    fn tags_of(event: &Event) -> Vec<Vec<String>> {
        event
            .tags
            .iter()
            .map(|tag| tag.as_slice().to_vec())
            .collect()
    }

    /// The envelope, field by field, against `buzz-acp/src/drain.rs`'s table.
    /// This test is the reason a change to either side breaks loudly.
    #[test]
    fn the_frame_is_the_envelope_the_runtime_documents() {
        let owner = Keys::generate();
        let agent = Keys::generate();
        let built = build_drain(&owner, &agent.public_key().to_hex(), Some("binary swap")).unwrap();

        assert_eq!(built.event.kind.as_u16(), 24_200);
        let tags = tags_of(&built.event);
        let agent_hex = agent.public_key().to_hex();
        assert!(
            tags.iter().any(|tag| tag == &["p", &agent_hex]),
            "the relay routes on `p`, and a drain is routed to the agent"
        );
        assert!(
            tags.iter()
                .any(|tag| tag == &[OBSERVER_AGENT_TAG, &agent_hex]),
            "`agent` names whose observer stream this frame belongs to"
        );
        assert!(tags
            .iter()
            .any(|tag| tag == &[OBSERVER_FRAME_TAG, OBSERVER_FRAME_CONTROL]));
        assert!(
            !tags
                .iter()
                .any(|tag| tag == &[OBSERVER_FRAME_TAG, OBSERVER_FRAME_TELEMETRY]),
            "a control frame must never be tagged as telemetry — the runtime \
             subscribes to the two separately"
        );
        assert!(
            !tags
                .iter()
                .any(|tag| tag.first().map(String::as_str) == Some("h")),
            "an observer frame names no channel"
        );
    }

    /// Encryption is to the *agent*, so the agent's key opens it and nothing
    /// else does. Round-tripped through the same helper the runtime calls.
    #[test]
    fn the_agent_key_opens_the_payload_and_a_stranger_s_does_not() {
        let owner = Keys::generate();
        let agent = Keys::generate();
        let built = build_drain(&owner, &agent.public_key().to_hex(), Some("  swap  ")).unwrap();

        let payload: serde_json::Value = decrypt_observer_payload(&agent, &built.event).unwrap();
        assert_eq!(payload["type"], DRAIN_CONTROL_TYPE);
        assert_eq!(
            payload["reason"], "swap",
            "a reason is trimmed before it is sent"
        );

        assert!(
            decrypt_observer_payload::<serde_json::Value>(&Keys::generate(), &built.event).is_err(),
            "a third party must not be able to read an owner-to-agent control frame"
        );
    }

    /// The runtime drops any control frame whose author is not the resolved
    /// owner, so the signer identity is load-bearing, not incidental.
    #[test]
    fn the_owner_signs_and_the_signature_verifies() {
        let owner = Keys::generate();
        let agent = Keys::generate();
        let built = build_drain(&owner, &agent.public_key().to_hex(), None).unwrap();

        assert_eq!(built.event.pubkey, owner.public_key());
        assert_ne!(
            built.event.pubkey,
            agent.public_key(),
            "an agent signing its own drain is exactly what the owner check refuses"
        );
        built
            .event
            .verify()
            .expect("a drain frame is a valid event");
    }

    /// `created_at` is what the ±300 s freshness window is checked against. A
    /// frame built now is trivially inside it; the assertion exists so that a
    /// future builder that back-dates or fixes the timestamp fails here rather
    /// than in production, where the symptom is a drain that is silently
    /// ignored.
    #[test]
    fn the_frame_is_stamped_now_so_it_lands_inside_the_freshness_window() {
        let built = build_drain(
            &Keys::generate(),
            &Keys::generate().public_key().to_hex(),
            None,
        )
        .unwrap();
        let now = chrono::Utc::now().timestamp();
        let skew = (built.event.created_at.as_secs() as i64 - now).abs();
        assert!(skew < 300, "created_at is {skew}s from now");
    }

    /// No reason means no field, not an empty one.
    #[test]
    fn an_absent_or_blank_reason_is_omitted_entirely() {
        let agent = Keys::generate();
        for reason in [None, Some(""), Some("   ")] {
            let built = build_drain(&Keys::generate(), &agent.public_key().to_hex(), reason)
                .expect("a drain needs no reason");
            let payload: serde_json::Value =
                decrypt_observer_payload(&agent, &built.event).unwrap();
            assert_eq!(payload["type"], DRAIN_CONTROL_TYPE);
            assert!(
                payload.get("reason").is_none(),
                "blank reason {reason:?} should not have produced a field"
            );
        }
    }

    #[test]
    fn an_oversized_reason_is_refused_before_anything_is_signed() {
        let error = build_drain(
            &Keys::generate(),
            &Keys::generate().public_key().to_hex(),
            Some(&"x".repeat(MAX_REASON_CHARS + 1)),
        )
        .unwrap_err();
        assert!(matches!(error, CliError::Usage(ref m) if m.contains("--reason")));
    }

    #[test]
    fn a_malformed_agent_pubkey_is_a_usage_error() {
        let error = build_drain(&Keys::generate(), "not-a-pubkey", None).unwrap_err();
        assert!(matches!(error, CliError::Usage(_)));
    }
}
