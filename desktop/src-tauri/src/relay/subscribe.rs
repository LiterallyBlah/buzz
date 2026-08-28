//! Bounded WebSocket transport and relay-frame parsing for extension
//! subscriptions (§5 `subscribe`, §9 lifecycle).
//!
//! **Transport only.** Authority, aggregation, quota and the public-subscription
//! lifecycle live in `extensions::query::subscription`, inside the sealed
//! boundary that owns constrained filters. This module knows how to open an
//! authenticated socket and turn bytes into typed frames; it makes no decision
//! about who may see them.
//!
//! # Why not reuse `commands::pairing`
//!
//! `pairing_ws_task_inner` is a good precedent for socket ownership,
//! cancellation and generation fencing, and its `tokio-tungstenite` dependency
//! is why this needs no new crate. Its **authentication and reader are not
//! safe to copy**, and both were checked rather than assumed:
//!
//! - `handle_nip42_auth` returns `Ok(())` when the challenge times out, so **no
//!   challenge reads as authenticated**;
//! - it discards the result of its OK wait (`let _ = timeout(...)`), so a
//!   timeout or a rejection also reads as authenticated;
//! - it accepts any text containing `"OK"` — no event-id match, no `success`
//!   field.
//!
//! For pairing that is a survivable optimism. Here the authenticated pubkey
//! *is* the authority witness every delivered event is checked against, so
//! every one of those becomes "an unauthenticated socket may stream a granted
//! channel". [`authenticate`] below is fail-closed at each step instead.
//!
//! Likewise `wait_for_eose` reads until EOSE and **discards every EVENT it
//! passes**, which is fine for pairing and would silently drop the stored
//! events §5 requires be delivered before the aggregate `eose`. The reader here
//! multiplexes from the first frame.

// The consumer is `extensions::query::subscription`, which lands next: this
// module is the transport half of one seam and is deliberately committed first
// so its strict-NIP-42 behaviour can be reviewed on its own. Matches the
// `grants.rs` precedent, where the store shipped ahead of the grants UX.
// **Remove this attribute when the aggregation module wires it up** — a
// permanent allow here would hide a genuinely orphaned transport.
#![allow(dead_code)]

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;

/// Largest WebSocket message this transport will accept, before parsing.
///
/// Checked against the frame as received: an untrusted relay is under no
/// obligation to keep its frames small, and a cap applied after parsing is a
/// cap on the wrong side of the allocation.
pub(crate) const MAX_WS_MESSAGE_BYTES: usize = 512 * 1024;

/// Longest the host waits for the relay's AUTH challenge.
pub(crate) const AUTH_CHALLENGE_TIMEOUT: Duration = Duration::from_secs(10);

/// Longest the host waits for the relay's OK acknowledging its AUTH event.
pub(crate) const AUTH_OK_TIMEOUT: Duration = Duration::from_secs(10);

/// Why a subscription transport step failed.
///
/// Deliberately coarse and free of relay text: these reach an extension only
/// after §8 normalisation, and a relay's own error strings are written for an
/// operator.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum TransportError {
    /// The socket could not be opened, or died.
    Connect,
    /// Strict NIP-42 did not complete. **No witness, and no `REQ`.**
    Auth(AuthFailure),
    /// A frame exceeded [`MAX_WS_MESSAGE_BYTES`] or was not parseable.
    Frame,
}

/// Exactly which step of strict NIP-42 refused.
///
/// Separated so each has its own named probe: "auth failed" as a single bucket
/// is a test that cannot distinguish a timeout from a forged acknowledgement.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum AuthFailure {
    /// No `AUTH` challenge arrived inside [`AUTH_CHALLENGE_TIMEOUT`].
    NoChallenge,
    /// The host could not sign the challenge with the admitted keys.
    CannotSign,
    /// No `OK` for the host's AUTH event id inside [`AUTH_OK_TIMEOUT`].
    NoAcknowledgement,
    /// An `OK` arrived, but for a **different** event id.
    AcknowledgedOtherEvent,
    /// A well-formed `OK` for the right id carrying `success == false`.
    Rejected,
}

/// Proof that a specific connection authenticated as a specific pubkey.
///
/// **Sealed by construction.** The fields are private and [`authenticate`] is
/// the only thing in the crate that can build one, so a witness cannot be
/// forged from mutable state or from a pubkey the caller merely claims. Holding
/// one is the evidence that the strict sequence completed — which is the whole
/// reason the aggregate can compare it against the identity that opened a
/// subscription.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IdentityWitness {
    connection_generation: u64,
    authenticated_pubkey: String,
}

impl IdentityWitness {
    pub(crate) fn connection_generation(&self) -> u64 {
        self.connection_generation
    }

    pub(crate) fn authenticated_pubkey(&self) -> &str {
        &self.authenticated_pubkey
    }
}

/// A relay frame this transport understands, after structural parsing.
///
/// Anything else — `NOTICE`, an unknown verb, a malformed array — is not
/// silently treated as one of these.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum RelayFrame {
    Event {
        sub_id: String,
        event: Box<nostr::Event>,
    },
    Eose {
        sub_id: String,
    },
    Closed {
        sub_id: String,
        reason: String,
    },
    /// `NOTICE`, carried because a rate-limit notice must arm the admission
    /// gate rather than be discarded.
    Notice {
        message: String,
    },
    /// Any other well-formed frame the subscription path does not act on.
    Other,
}

/// Parse one relay text frame.
///
/// Size is checked before shape, and every arm is structural: a frame is an
/// `EVENT` because its first element is the string `"EVENT"` and its parts are
/// present, never because the text happened to contain a substring. That is the
/// distinction `pairing.rs`'s `text.contains("\"OK\"")` loses.
pub(crate) fn parse_frame(text: &str) -> Result<RelayFrame, TransportError> {
    if text.len() > MAX_WS_MESSAGE_BYTES {
        return Err(TransportError::Frame);
    }
    let value: serde_json::Value = serde_json::from_str(text).map_err(|_| TransportError::Frame)?;
    let array = value.as_array().ok_or(TransportError::Frame)?;
    let verb = array
        .first()
        .and_then(serde_json::Value::as_str)
        .ok_or(TransportError::Frame)?;

    match verb {
        "EVENT" => {
            let sub_id = array
                .get(1)
                .and_then(serde_json::Value::as_str)
                .ok_or(TransportError::Frame)?;
            let raw = array.get(2).ok_or(TransportError::Frame)?;
            let event: nostr::Event =
                serde_json::from_value(raw.clone()).map_err(|_| TransportError::Frame)?;
            Ok(RelayFrame::Event {
                sub_id: sub_id.to_string(),
                event: Box::new(event),
            })
        }
        "EOSE" => {
            let sub_id = array
                .get(1)
                .and_then(serde_json::Value::as_str)
                .ok_or(TransportError::Frame)?;
            Ok(RelayFrame::Eose {
                sub_id: sub_id.to_string(),
            })
        }
        "CLOSED" => {
            let sub_id = array
                .get(1)
                .and_then(serde_json::Value::as_str)
                .ok_or(TransportError::Frame)?;
            // A reason is optional on the wire; its absence is not a parse
            // failure, because CLOSED is terminal either way.
            let reason = array
                .get(2)
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            Ok(RelayFrame::Closed {
                sub_id: sub_id.to_string(),
                reason: reason.to_string(),
            })
        }
        "NOTICE" => Ok(RelayFrame::Notice {
            message: array
                .get(1)
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string(),
        }),
        _ => Ok(RelayFrame::Other),
    }
}

/// The `AUTH` challenge, if this frame is one.
fn parse_auth_challenge(text: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    let array = value.as_array()?;
    if array.first()?.as_str()? != "AUTH" {
        return None;
    }
    Some(array.get(1)?.as_str()?.to_string())
}

/// A structurally parsed `OK` frame: `["OK", <event-id>, <bool>, <message>]`.
///
/// Returns `None` unless every part is present and correctly typed. The id and
/// the boolean are both required, because an `OK` for another event and an `OK`
/// carrying `false` are different refusals and neither is success.
fn parse_ok(text: &str) -> Option<(String, bool)> {
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    let array = value.as_array()?;
    if array.first()?.as_str()? != "OK" {
        return None;
    }
    let event_id = array.get(1)?.as_str()?.to_string();
    let success = array.get(2)?.as_bool()?;
    Some((event_id, success))
}

/// Run strict NIP-42 and produce a witness, or refuse.
///
/// Every step is fail-closed, and the sequence is the contract's:
///
/// ```text
/// challenge within AUTH_CHALLENGE_TIMEOUT
///   → AUTH signed by the captured admitted keys
///   → the exact AUTH event id retained
///   → a structurally parsed OK for that exact id within AUTH_OK_TIMEOUT
///   → success == true
///   → only then a witness
/// ```
///
/// A timeout at either end, an `OK` for a different event, a malformed `OK`, or
/// `success == false` all return `Err` — which means no witness, and the caller
/// must send no `REQ`.
pub(crate) async fn authenticate<R, W>(
    read: &mut R,
    write: &mut W,
    keys: &nostr::Keys,
    relay_url: &str,
    connection_generation: u64,
) -> Result<IdentityWitness, TransportError>
where
    R: StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
    W: SinkExt<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
{
    // 1. The challenge, within bound. A timeout is a refusal, not a shrug.
    let challenge = tokio::time::timeout(AUTH_CHALLENGE_TIMEOUT, async {
        loop {
            let message = read.next().await?.ok()?;
            let Message::Text(text) = message else {
                continue;
            };
            if text.len() > MAX_WS_MESSAGE_BYTES {
                return None;
            }
            if let Some(challenge) = parse_auth_challenge(text.as_str()) {
                return Some(challenge);
            }
        }
    })
    .await
    .ok()
    .flatten()
    .ok_or(TransportError::Auth(AuthFailure::NoChallenge))?;

    // 2. Sign with the captured admitted keys — never a re-read of app state.
    let relay_url_parsed = nostr::RelayUrl::parse(relay_url)
        .map_err(|_| TransportError::Auth(AuthFailure::CannotSign))?;
    let auth_event = nostr::EventBuilder::auth(challenge, relay_url_parsed)
        .sign_with_keys(keys)
        .map_err(|_| TransportError::Auth(AuthFailure::CannotSign))?;
    // 3. Retain the exact id. This is what makes step 5 an identity check
    //    rather than "some OK arrived".
    let auth_event_id = auth_event.id.to_hex();

    use nostr::JsonUtil as _;
    write
        .send(Message::Text(
            format!("[\"AUTH\",{}]", auth_event.as_json()).into(),
        ))
        .await
        .map_err(|_| TransportError::Connect)?;

    // 4/5. A structurally parsed OK for that exact id, within bound. An OK for
    //      another event is not this event's acknowledgement, so it is a
    //      distinct refusal rather than something to keep waiting through.
    let outcome = tokio::time::timeout(AUTH_OK_TIMEOUT, async {
        loop {
            let message = read.next().await?.ok()?;
            let Message::Text(text) = message else {
                continue;
            };
            if text.len() > MAX_WS_MESSAGE_BYTES {
                return None;
            }
            if let Some((event_id, success)) = parse_ok(text.as_str()) {
                return Some((event_id, success));
            }
        }
    })
    .await
    .ok()
    .flatten()
    .ok_or(TransportError::Auth(AuthFailure::NoAcknowledgement))?;

    let (acknowledged_id, success) = outcome;
    if acknowledged_id != auth_event_id {
        return Err(TransportError::Auth(AuthFailure::AcknowledgedOtherEvent));
    }
    if !success {
        return Err(TransportError::Auth(AuthFailure::Rejected));
    }

    // 6. Only now does a witness exist, and it carries the pubkey that actually
    //    authenticated — not one the caller supplied.
    Ok(IdentityWitness {
        connection_generation,
        authenticated_pubkey: keys.public_key().to_hex(),
    })
}

#[cfg(test)]
#[path = "subscribe_tests.rs"]
mod subscribe_tests;
