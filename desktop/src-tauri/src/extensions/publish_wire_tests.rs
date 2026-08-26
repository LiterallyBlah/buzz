//! The signer's real signing and submission path, over a live socket.

use super::publish_test_support::*;
use super::*;
use crate::extensions::dispatch::code;

// ── the real signing + submission path ───────────────────────────────────────

/// Pull the `"id"` field out of a submitted nostr event body.
///
/// The fake relays echo it, because a real relay does — and the host now
/// verifies the acknowledgement names the event it actually signed.
fn submitted_event_id(body: &str) -> String {
    let marker = "\"id\":\"";
    body.find(marker)
        .map(|at| body[at + marker.len()..].chars().take(64).collect())
        .unwrap_or_default()
}

/// Serve one `POST /events`, hand back the request body it received.
///
/// `std::net` on a `std::thread`, matching the pattern `relay.rs` documents:
/// a tokio listener converted with `into_std()` leaves the socket nonblocking,
/// and answering before the client has finished sending produces hyper
/// `UnexpectedMessage` failures under load. Both races are real; this shape
/// avoids them.
fn one_shot_relay() -> (String, std::sync::mpsc::Receiver<String>) {
    use std::io::{Read as _, Write as _};

    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let (tx, rx) = std::sync::mpsc::channel();

    std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let mut raw = Vec::new();
            let mut buf = [0u8; 8192];
            // Read until the body arrives. Content-Length is present, so the
            // headers tell us when to stop.
            while let Ok(read) = stream.read(&mut buf) {
                if read == 0 {
                    break;
                }
                raw.extend_from_slice(&buf[..read]);
                let text = String::from_utf8_lossy(&raw).to_string();
                if let Some((head, body)) = text.split_once("\r\n\r\n") {
                    let expected = head
                        .lines()
                        .find_map(|line| {
                            line.strip_prefix("Content-Length: ")
                                .or_else(|| line.strip_prefix("content-length: "))
                        })
                        .and_then(|v| v.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    if body.len() >= expected {
                        let _ = tx.send(body.to_string());
                        break;
                    }
                }
            }
            let echoed = submitted_event_id(&String::from_utf8_lossy(&raw));
            let body = format!(r#"{{"event_id":"{echoed}","accepted":true,"message":""}}"#);
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });

    (format!("http://{addr}"), rx)
}

#[tokio::test]
async fn the_signed_event_on_the_wire_is_the_canonical_event() {
    // The checks are worthless if the bytes that reach the relay differ from
    // the ones that were checked. This drives the real path —
    // `sign_and_publish` → `submit_signed_event_with_keys` → HTTP POST — and
    // asserts against what actually crossed the socket, not against a return
    // value the same code produced.
    use nostr::JsonUtil as _;

    let (relay_url, received) = one_shot_relay();
    let keys = nostr::Keys::generate();
    let state = crate::app_state::build_app_state();
    *state.keys.lock().unwrap() = keys.clone();
    *state.relay_url_override.lock().unwrap() = Some(relay_url);

    let event = CanonicalEvent {
        kind: kind::KIND_STREAM_MESSAGE,
        content: "published through the bridge".to_string(),
        tags: vec![tag(&["h", CHANNEL]), tag(&["p", &"a".repeat(64)])],
        created_at: 1_700_000_000,
    };

    let result = sign_and_publish(&event, &keys, &state, || Ok(()))
        .await
        .expect("the publish path must succeed against an accepting relay");

    let body = received
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("the relay must have received a POST body");
    let on_wire = nostr::Event::from_json(&body).expect("the body must be a nostr event");

    // The signature is real and covers these bytes.
    on_wire.verify().expect("the event on the wire must verify");
    assert_eq!(
        on_wire.pubkey,
        keys.public_key(),
        "the event must be signed by the user's identity"
    );

    // And the signed bytes are the canonical event, field for field.
    assert_eq!(u32::from(on_wire.kind.as_u16()), event.kind);
    assert_eq!(on_wire.content, event.content);
    assert_eq!(on_wire.created_at.as_secs(), event.created_at as u64);
    let wire_tags: Vec<Vec<String>> = on_wire.tags.iter().map(|t| t.clone().to_vec()).collect();
    assert_eq!(
        wire_tags, event.tags,
        "the tags checked must be the tags signed"
    );

    // The reply reports the relay's own acceptance rather than assuming it.
    assert_eq!(result["relay"]["accepted"], serde_json::json!(true));
    assert_eq!(
        result["event"]["id"],
        serde_json::json!(on_wire.id.to_hex())
    );
}

#[tokio::test]
async fn a_suppressed_duplicate_is_reported_as_success() {
    // Ruling B, and the payoff of requiring a stable `created_at`: when the
    // relay recognises the id and answers `accepted: true, message:
    // "duplicate:"`, the event is committed — so the caller gets the *same*
    // success a fresh publish returns.
    //
    // Two things this pins. That a duplicate is not an error, and that it is
    // not even distinguishable: the relay's `message` must not reach the
    // reply, or "you already did this" becomes an observable side channel and
    // an idempotent retry stops looking idempotent.
    use std::io::{Read as _, Write as _};

    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let mut buf = [0u8; 8192];
            let read = stream.read(&mut buf).unwrap_or(0);
            let echoed = submitted_event_id(&String::from_utf8_lossy(&buf[..read]));
            let body =
                format!(r#"{{"event_id":"{echoed}","accepted":true,"message":"duplicate:"}}"#);
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });

    let keys = nostr::Keys::generate();
    let state = crate::app_state::build_app_state();
    *state.keys.lock().unwrap() = keys.clone();
    *state.relay_url_override.lock().unwrap() = Some(format!("http://{addr}"));

    let event = message(vec![tag(&["h", CHANNEL])], "a retry of an earlier publish");
    let result = sign_and_publish(&event, &keys, &state, || Ok(()))
        .await
        .expect("a suppressed duplicate must not be an error");

    assert_eq!(
        result["relay"]["accepted"],
        serde_json::json!(true),
        "a duplicate is committed, so it reports accepted"
    );
    let rendered = serde_json::to_string(&result).expect("serialise");
    assert!(
        !rendered.contains("duplicate"),
        "the relay's duplicate marker must not reach the caller: {rendered}"
    );
}

/// Sign a canonical event and return its id, the way `sign_and_publish` does.
fn event_id_of(event: &CanonicalEvent, keys: &nostr::Keys) -> String {
    let mut tags = Vec::new();
    for parts in &event.tags {
        tags.push(nostr::Tag::parse(parts.clone()).expect("tag"));
    }
    nostr::EventBuilder::new(
        nostr::Kind::Custom(u16::try_from(event.kind).expect("kind")),
        event.content.clone(),
    )
    .tags(tags)
    .custom_created_at(nostr::Timestamp::from(
        u64::try_from(event.created_at).expect("created_at"),
    ))
    .sign_with_keys(keys)
    .expect("sign")
    .id
    .to_hex()
}

#[test]
fn a_retry_at_a_different_now_produces_the_same_event_id() {
    // Proof 1, carried all the way to the id rather than stopping at the
    // canonical struct: the id is what the relay deduplicates on, so equality
    // of the intermediate value is not the property that matters.
    let keys = nostr::Keys::generate();
    let now = 1_700_000_000i64;
    let params = template_params(Some(now - 5));

    let first = canonicalise(&parse_template(Some(params.clone()), now).expect("first"));
    let retry = canonicalise(&parse_template(Some(params), now + 240).expect("retry"));

    assert_eq!(first, retry, "the canonical event must be reproduced");
    assert_eq!(
        event_id_of(&first, &keys),
        event_id_of(&retry, &keys),
        "and so must the id the relay deduplicates on"
    );
}

#[test]
fn changing_any_canonical_field_changes_the_event_id() {
    // Proof 7, and a check on the proof above: if the id were constant for
    // some unrelated reason, the retry test would pass while proving nothing.
    // Every field that goes into the canonical event must move the id.
    let keys = nostr::Keys::generate();
    let base = CanonicalEvent {
        kind: kind::KIND_STREAM_MESSAGE,
        content: "hello".to_string(),
        tags: vec![tag(&["h", CHANNEL])],
        created_at: 1_700_000_000,
    };
    let baseline = event_id_of(&base, &keys);

    let variants = [
        (
            "kind",
            CanonicalEvent {
                kind: kind::KIND_FORUM_POST,
                ..base.clone()
            },
        ),
        (
            "content",
            CanonicalEvent {
                content: "hello.".to_string(),
                ..base.clone()
            },
        ),
        (
            "created_at",
            CanonicalEvent {
                created_at: base.created_at + 1,
                ..base.clone()
            },
        ),
        (
            "tag value",
            CanonicalEvent {
                tags: vec![tag(&["h", OTHER_CHANNEL])],
                ..base.clone()
            },
        ),
        (
            "tag count",
            CanonicalEvent {
                tags: vec![tag(&["h", CHANNEL]), tag(&["t", "topic"])],
                ..base.clone()
            },
        ),
    ];
    for (field, variant) in variants {
        assert_ne!(
            event_id_of(&variant, &keys),
            baseline,
            "changing {field} must change the event id"
        );
    }

    // Tag *order* is part of the canonical event too, so reordering is a
    // different operation rather than the same one.
    let reordered = CanonicalEvent {
        tags: vec![tag(&["t", "topic"]), tag(&["h", CHANNEL])],
        ..base.clone()
    };
    let ordered = CanonicalEvent {
        tags: vec![tag(&["h", CHANNEL]), tag(&["t", "topic"])],
        ..base
    };
    assert_ne!(
        event_id_of(&reordered, &keys),
        event_id_of(&ordered, &keys),
        "tag order is part of the operation identity"
    );
}

#[tokio::test]
async fn an_acknowledgement_naming_a_different_event_is_a_relay_error() {
    // The relay must be talking about the event we signed. Reporting a
    // mismatch as success would tell the caller some *other* event had
    // committed — and under an idempotent contract they would then stop
    // retrying the one that never landed.
    use std::io::{Read as _, Write as _};

    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let mut buf = [0u8; 8192];
            let _ = stream.read(&mut buf);
            // Well-formed, accepted — and about a different event entirely.
            let body = format!(
                r#"{{"event_id":"{}","accepted":true,"message":""}}"#,
                "f".repeat(64)
            );
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });

    let keys = nostr::Keys::generate();
    let state = crate::app_state::build_app_state();
    *state.keys.lock().unwrap() = keys.clone();
    *state.relay_url_override.lock().unwrap() = Some(format!("http://{addr}"));

    let event = message(vec![tag(&["h", CHANNEL])], "hello");
    let reply = sign_and_publish(&event, &keys, &state, || Ok(()))
        .await
        .expect_err("a mismatched acknowledgement must not read as success");
    assert_eq!(reply.error.expect("error").code, code::RELAY_ERROR);
}

#[tokio::test]
async fn revoked_authority_during_the_wait_produces_no_post() {
    // Proof 9. The rate-limit gate is global and unbounded from the caller's
    // point of view, so authority checked before it is checked at the wrong
    // time. The revalidation runs after that wait and immediately before the
    // POST; a refusal there must mean the request never reaches the socket.
    //
    // The listener counts connections, so "no POST" is observed rather than
    // inferred from a return value.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let connections = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = connections.clone();
    std::thread::spawn(move || {
        for _ in listener.incoming() {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    });

    let keys = nostr::Keys::generate();
    let state = crate::app_state::build_app_state();
    *state.keys.lock().unwrap() = keys.clone();
    *state.relay_url_override.lock().unwrap() = Some(format!("http://{addr}"));

    let event = message(vec![tag(&["h", CHANNEL])], "hello");
    let reply = sign_and_publish(&event, &keys, &state, || Err(code::DENIED))
        .await
        .expect_err("a revoked grant must not publish");
    assert_eq!(
        reply.error.expect("error").code,
        code::DENIED,
        "a revoked grant is a denial, not a relay fault — the request never \
         reached the relay, so blaming it would be false"
    );

    // The decisive assertion: nothing was sent.
    std::thread::sleep(std::time::Duration::from_millis(80));
    assert_eq!(
        connections.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "revalidation refused, so no request may have reached the relay"
    );
}

#[tokio::test]
async fn a_relay_refusal_is_normalised_and_leaks_nothing() {
    // §8: the relay's message is written for an operator and can name hosts,
    // kinds and internal reasons. An extension gets a stable code and a fixed
    // string instead.
    use std::io::{Read as _, Write as _};

    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let mut buf = [0u8; 8192];
            let _ = stream.read(&mut buf);
            let body =
                r#"{"error":"restricted: /var/lib/buzz/relay.sock rejected kind 9 for pubkey"}"#;
            let response = format!(
                "HTTP/1.1 403 Forbidden\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });

    let keys = nostr::Keys::generate();
    let state = crate::app_state::build_app_state();
    *state.keys.lock().unwrap() = keys.clone();
    *state.relay_url_override.lock().unwrap() = Some(format!("http://{addr}"));

    let event = message(vec![tag(&["h", CHANNEL])], "hello");
    let reply = sign_and_publish(&event, &keys, &state, || Ok(()))
        .await
        .expect_err("a 403 must not be reported as success");

    let error = reply.error.expect("a refusal carries an error");
    assert_eq!(error.code, code::RELAY_ERROR);
    for leaked in ["/var/lib", "restricted", "pubkey", "sock"] {
        assert!(
            !error.message.contains(leaked),
            "the relay's text leaked {leaked:?}: {}",
            error.message
        );
    }
}

#[test]
fn canonicalisation_carries_tags_and_content_verbatim() {
    // The checks read the canonical event, so it must be what gets signed —
    // no dropped tags, no rewritten content.
    let now = 1_700_000_000i64;
    let template = EventTemplate {
        kind: kind::KIND_STREAM_MESSAGE,
        content: "  spaces preserved  ".to_string(),
        tags: vec![tag(&["h", CHANNEL]), tag(&["p", &"a".repeat(64)])],
        created_at: now,
    };
    let canonical = canonicalise(&template);
    assert_eq!(canonical.content, template.content);
    assert_eq!(canonical.tags, template.tags);
    assert_eq!(canonical.kind, template.kind);
}
