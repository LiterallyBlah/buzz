//! Transport tests: frame parsing, and every refusal of strict NIP-42.
//!
//! Each auth failure gets its own probe. "Authentication failed" as a single
//! bucket cannot tell a timeout from a forged acknowledgement, and those are
//! the two the precedent implementation conflated.

use super::*;

use std::pin::Pin;
use std::task::{Context, Poll};

type WsError = tokio_tungstenite::tungstenite::Error;

/// A `Stream` over a tokio channel.
///
/// `futures_util`'s own channels are not enabled in this crate's feature set,
/// and the happy-path exchange genuinely needs a live pair: the host's AUTH
/// event id is chosen at runtime, so the only way to answer *that exact id* is
/// to read what the host sent and reply to it.
struct RecvStream(tokio::sync::mpsc::UnboundedReceiver<Result<Message, WsError>>);

impl futures_util::Stream for RecvStream {
    type Item = Result<Message, WsError>;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.0.poll_recv(cx)
    }
}

/// A `Sink` that forwards into a tokio channel.
struct SendSink(tokio::sync::mpsc::UnboundedSender<Message>);

impl futures_util::Sink<Message> for SendSink {
    type Error = WsError;
    fn poll_ready(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }
    fn start_send(self: Pin<&mut Self>, item: Message) -> Result<(), Self::Error> {
        self.0.send(item).map_err(|_| WsError::ConnectionClosed)
    }
    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }
    fn poll_close(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }
}

fn keys() -> nostr::Keys {
    nostr::Keys::generate()
}

/// A read stream over canned frames, then end-of-stream.
fn reader(frames: Vec<String>) -> impl StreamExt<Item = Result<Message, WsError>> + Unpin {
    futures_util::stream::iter(
        frames
            .into_iter()
            .map(|f| Ok(Message::Text(f.into())))
            .collect::<Vec<_>>(),
    )
}

/// A sink that accepts and discards, with the tungstenite error type.
fn writer() -> impl SinkExt<Message, Error = WsError> + Unpin {
    futures_util::sink::drain()
        .sink_map_err(|_: std::convert::Infallible| WsError::ConnectionClosed)
}

fn signed(kind: u32, tags: Vec<Vec<String>>) -> nostr::Event {
    let k = keys();
    let mut builder = nostr::EventBuilder::new(nostr::Kind::from(kind as u16), "{}");
    for tag in tags {
        builder = builder.tag(nostr::Tag::parse(tag).expect("tag"));
    }
    builder.sign_with_keys(&k).expect("sign")
}

// ── frame parsing ──────────────────────────────────────────────────────────

#[test]
fn an_event_frame_parses_with_its_subscription_id() {
    use nostr::JsonUtil as _;
    let event = signed(9, vec![vec!["h".into(), "c".into()]]);
    let text = format!("[\"EVENT\",\"branch-1\",{}]", event.as_json());
    match parse_frame(&text).expect("parses") {
        RelayFrame::Event { sub_id, event: got } => {
            assert_eq!(sub_id, "branch-1");
            assert_eq!(got.id, event.id);
        }
        other => panic!("expected an EVENT, got {other:?}"),
    }
}

#[test]
fn eose_and_closed_carry_the_exact_branch_id() {
    match parse_frame("[\"EOSE\",\"branch-2\"]").expect("parses") {
        RelayFrame::Eose { sub_id } => assert_eq!(sub_id, "branch-2"),
        other => panic!("expected EOSE, got {other:?}"),
    }
    match parse_frame("[\"CLOSED\",\"branch-3\",\"rate-limited\"]").expect("parses") {
        RelayFrame::Closed { sub_id, reason } => {
            assert_eq!(sub_id, "branch-3");
            assert_eq!(reason, "rate-limited");
        }
        other => panic!("expected CLOSED, got {other:?}"),
    }
}

#[test]
fn a_closed_without_a_reason_is_still_closed() {
    // CLOSED is terminal whether or not the relay explains itself; treating a
    // missing reason as a parse failure would turn a clean terminal frame into
    // a transport error and lose which branch died.
    match parse_frame("[\"CLOSED\",\"branch-4\"]").expect("parses") {
        RelayFrame::Closed { sub_id, reason } => {
            assert_eq!(sub_id, "branch-4");
            assert!(reason.is_empty());
        }
        other => panic!("expected CLOSED, got {other:?}"),
    }
}

#[test]
fn a_notice_is_carried_rather_than_discarded() {
    // A rate-limit notice has to reach the admission gate.
    match parse_frame("[\"NOTICE\",\"rate limited\"]").expect("parses") {
        RelayFrame::Notice { message } => assert_eq!(message, "rate limited"),
        other => panic!("expected NOTICE, got {other:?}"),
    }
}

#[test]
fn an_unknown_verb_is_other_not_an_error() {
    assert_eq!(
        parse_frame("[\"COUNT\",\"x\",{}]").expect("parses"),
        RelayFrame::Other
    );
}

#[test]
fn a_frame_over_the_size_bound_is_refused_before_parsing() {
    let huge = format!("[\"EOSE\",\"{}\"]", "x".repeat(MAX_WS_MESSAGE_BYTES));
    assert_eq!(parse_frame(&huge), Err(TransportError::Frame));
}

#[test]
fn malformed_frames_are_refused() {
    for bad in [
        "not json",
        "{\"not\":\"an array\"}",
        "[]",
        "[123]",
        "[\"EVENT\"]",
        "[\"EVENT\",\"sub\"]",
        "[\"EVENT\",\"sub\",{\"not\":\"an event\"}]",
        "[\"EOSE\"]",
    ] {
        assert_eq!(parse_frame(bad), Err(TransportError::Frame), "for {bad}");
    }
}

#[test]
fn an_event_frame_is_matched_structurally_not_by_substring() {
    // The precedent accepted any text containing `"OK"`. A NOTICE whose message
    // merely mentions another verb must not be read as that verb.
    match parse_frame("[\"NOTICE\",\"[\\\"EOSE\\\",\\\"branch-9\\\"]\"]").expect("parses") {
        RelayFrame::Notice { .. } => {}
        other => panic!("a NOTICE quoting EOSE must stay a NOTICE, got {other:?}"),
    }
}

// ── strict NIP-42 ──────────────────────────────────────────────────────────

fn challenge_frame() -> String {
    "[\"AUTH\",\"server-challenge-abc\"]".to_string()
}

/// The relay's OK for whatever AUTH event the host signs.
///
/// The host's event id is not knowable in advance here, so the happy-path test
/// drives the exchange in two steps instead; this helper builds an OK for an
/// explicitly chosen id, which is what the mismatch probe needs.
fn ok_frame(event_id: &str, success: bool) -> String {
    format!("[\"OK\",\"{event_id}\",{success},\"\"]")
}

#[tokio::test]
async fn no_challenge_means_no_witness() {
    // The precedent returned Ok(()) here — a socket that never authenticated
    // read as authenticated.
    let mut read = reader(vec![]);
    let mut write = writer();
    let result = authenticate(&mut read, &mut write, &keys(), "wss://relay.test", 1).await;
    assert_eq!(
        result.err(),
        Some(TransportError::Auth(AuthFailure::NoChallenge))
    );
}

#[tokio::test(start_paused = true)]
async fn a_challenge_that_never_arrives_times_out_into_a_refusal() {
    // Distinct from the stream simply ending: here the relay holds the socket
    // open and says nothing, which is exactly the case the precedent's
    // `Err(_) => Ok(())` turned into success.
    let mut read = futures_util::stream::pending::<Result<Message, WsError>>();
    let mut write = writer();
    let result = authenticate(&mut read, &mut write, &keys(), "wss://relay.test", 1).await;
    assert_eq!(
        result.err(),
        Some(TransportError::Auth(AuthFailure::NoChallenge))
    );
}

#[tokio::test]
async fn no_acknowledgement_means_no_witness() {
    // Challenge answered, then silence. The precedent discarded this result.
    let mut read = reader(vec![challenge_frame()]);
    let mut write = writer();
    let result = authenticate(&mut read, &mut write, &keys(), "wss://relay.test", 1).await;
    assert_eq!(
        result.err(),
        Some(TransportError::Auth(AuthFailure::NoAcknowledgement))
    );
}

#[tokio::test]
async fn an_ok_for_another_event_is_not_this_events_acknowledgement() {
    // The id must match. Accepting any OK is how a relay's unrelated
    // acknowledgement — or a replayed one — becomes a witness.
    let other_id = "b".repeat(64);
    let mut read = reader(vec![challenge_frame(), ok_frame(&other_id, true)]);
    let mut write = writer();
    let result = authenticate(&mut read, &mut write, &keys(), "wss://relay.test", 1).await;
    assert_eq!(
        result.err(),
        Some(TransportError::Auth(AuthFailure::AcknowledgedOtherEvent))
    );
}

#[tokio::test]
async fn a_malformed_ok_is_not_an_acknowledgement() {
    // No boolean, so nothing said the AUTH succeeded. Substring matching would
    // have accepted every one of these.
    for bad_ok in [
        "[\"OK\"]".to_string(),
        "[\"OK\",\"id\"]".to_string(),
        "[\"OK\",\"id\",\"true\"]".to_string(),
        "[\"NOTICE\",\"contains OK in its text\"]".to_string(),
    ] {
        let mut read = reader(vec![challenge_frame(), bad_ok.clone()]);
        let mut write = writer();
        let result = authenticate(&mut read, &mut write, &keys(), "wss://relay.test", 1).await;
        assert_eq!(
            result.err(),
            Some(TransportError::Auth(AuthFailure::NoAcknowledgement)),
            "for {bad_ok}"
        );
    }
}

/// Drive the exchange far enough to learn the host's AUTH event id, then answer
/// it — the only way to build an OK for an id the host chooses at runtime.
#[derive(Clone, Copy)]
enum OkShape {
    Complete,
    Truncated,
    Extra,
    NonStringMessage,
}

async fn authenticate_with_relay_reply(
    keys: &nostr::Keys,
    success: bool,
    generation: u64,
    shape: OkShape,
) -> Result<IdentityWitness, TransportError> {
    let (to_host, from_relay) = tokio::sync::mpsc::unbounded_channel::<Result<Message, WsError>>();
    let (to_relay, mut from_host) = tokio::sync::mpsc::unbounded_channel::<Message>();

    to_host
        .send(Ok(Message::Text(challenge_frame().into())))
        .expect("send challenge");

    let mut read = RecvStream(from_relay);
    let mut write = SendSink(to_relay);

    let keys_for_task = keys.clone();
    let auth = tokio::spawn(async move {
        authenticate(
            &mut read,
            &mut write,
            &keys_for_task,
            "wss://relay.test",
            generation,
        )
        .await
    });

    // Read the host's AUTH frame, extract the exact event id, answer it.
    let sent = from_host.recv().await.expect("host sends AUTH");
    let Message::Text(text) = sent else {
        panic!("expected a text AUTH frame");
    };
    let value: serde_json::Value = serde_json::from_str(&text).expect("AUTH json");
    let event_id = value[1]["id"].as_str().expect("auth event id").to_string();
    let reply = match shape {
        OkShape::Complete => ok_frame(&event_id, success),
        OkShape::Truncated => format!("[\"OK\",\"{event_id}\",{success}]"),
        OkShape::Extra => format!("[\"OK\",\"{event_id}\",{success},\"\",\"extra\"]"),
        OkShape::NonStringMessage => format!("[\"OK\",\"{event_id}\",{success},7]"),
    };
    to_host
        .send(Ok(Message::Text(reply.into())))
        .expect("send OK");

    auth.await.expect("join")
}

#[tokio::test]
async fn a_rejected_auth_means_no_witness() {
    let keys = keys();
    let result = authenticate_with_relay_reply(&keys, false, 7, OkShape::Complete).await;
    assert_eq!(
        result.err(),
        Some(TransportError::Auth(AuthFailure::Rejected)),
        "success=false must refuse"
    );
}

#[tokio::test]
async fn a_complete_strict_exchange_yields_a_witness_for_the_signing_key() {
    // THE POSITIVE CONTROL. Without it the five refusals above are satisfied by
    // an implementation that refuses everything.
    let keys = keys();
    let witness = authenticate_with_relay_reply(&keys, true, 7, OkShape::Complete)
        .await
        .expect("a complete exchange must authenticate");
    assert_eq!(witness.connection_generation(), 7);
    assert_eq!(
        witness.authenticated_pubkey(),
        keys.public_key().to_hex(),
        "the witness carries the key that actually signed, not a claimed one"
    );
}

#[tokio::test]
async fn a_truncated_exact_id_ok_never_mints_a_witness() {
    // The host's dynamic exact AUTH id is echoed correctly; only the required
    // NIP-01 message field is absent, isolating structural completeness.
    let keys = keys();
    let result = authenticate_with_relay_reply(&keys, true, 9, OkShape::Truncated).await;
    assert_eq!(
        result.err(),
        Some(TransportError::Auth(AuthFailure::NoAcknowledgement))
    );
}

#[tokio::test]
async fn an_overlong_exact_id_ok_never_mints_a_witness() {
    let keys = keys();
    let result = authenticate_with_relay_reply(&keys, true, 10, OkShape::Extra).await;
    assert_eq!(
        result.err(),
        Some(TransportError::Auth(AuthFailure::NoAcknowledgement))
    );
}

#[tokio::test]
async fn a_non_string_ok_message_never_mints_a_witness() {
    let keys = keys();
    let result = authenticate_with_relay_reply(&keys, true, 11, OkShape::NonStringMessage).await;
    assert_eq!(
        result.err(),
        Some(TransportError::Auth(AuthFailure::NoAcknowledgement))
    );
}
