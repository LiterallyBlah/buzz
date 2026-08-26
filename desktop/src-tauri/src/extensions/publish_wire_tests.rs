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
    let _gate = crate::relay_admission::gate_guard().await;
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
    let _gate = crate::relay_admission::gate_guard().await;
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
    let _gate = crate::relay_admission::gate_guard().await;
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
async fn revoking_the_grant_during_a_real_rate_limit_wait_produces_no_post() {
    // Proof 9, exercising the production transition rather than an injected
    // refusal.
    //
    // The previous version passed a closure that returned `Err` immediately.
    // That proved a refusal emits zero POSTs; it did **not** prove the
    // transition that creates one — the gate is global and unbounded from the
    // caller's view, and authority checked before it is checked at the wrong
    // time. This arms a real wait, revokes a real grant from a real store while
    // the request is parked in it, and lets the production revalidator see the
    // change.
    //
    // The shared guard is mandatory: the gate is a process-wide static, so
    // without it another suite's armed expiry bleeds into this one.
    let _gate = crate::relay_admission::gate_guard().await;

    // The lease map is a process-wide global too, and this registers a real
    // entry in it so the production lease check resolves rather than being
    // stepped around.
    let _host = crate::extensions::frame_host::lifecycle_guard().await;
    crate::extensions::frame_host::insert_lease_for_test(LEASE, "demo");

    // A listener that counts connections, so "no POST" is observed.
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

    // A current timestamp, because the production revalidator checks the
    // window and the shared fixture is dated 2023. The hand-written stand-in
    // this replaces checked only a grant row, so it never noticed.
    let event = CanonicalEvent {
        created_at: now_unix(),
        ..message(vec![tag(&["h", CHANNEL])], "hello")
    };

    // A real grant store, keyed by the pubkey that will actually sign. The
    // previous fixture granted to a literal `"aaaa..."` identity that no part
    // of the run ever signed with, which is what let it pass while production
    // checked something else entirely.
    let identity = keys.public_key().to_hex();
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("grants").join("extension-grants.db");
    {
        let conn = crate::extensions::grants::open_grant_db(&db_path).expect("open");
        crate::extensions::grants::grant_sign_scope(
            &conn,
            &identity,
            "demo",
            kind::KIND_STREAM_MESSAGE,
            CHANNEL,
        )
        .expect("grant");
    }

    // The production revalidator itself — the same type `publish_event`
    // constructs, so the lease, identity, full `authorise` and timestamp
    // checks under test are the ones that run in the signer. A hand-written
    // stand-in here is why this proof previously survived deleting them.
    let revalidation = Revalidation {
        lease: LEASE,
        extension_id: "demo",
        identity_at_entry: &identity,
        event: &event,
        state: &state,
        grant_db: Some(db_path.clone()),
    };
    let revalidate = || revalidation.check();

    // The grant is live before the wait is armed, so the refusal below can
    // only come from the revocation rather than from a fixture that never
    // granted anything.
    revalidate().expect("authority must hold at entry");

    // Arm a real wait, then revoke while the submission is parked in it.
    crate::relay_admission::activate_rate_limit(Some(1));
    let armed_at = std::time::Instant::now();

    let submit = sign_and_publish(&event, &keys, &state, revalidate);
    let revoke = async {
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let conn = crate::extensions::grants::open_grant_db(&db_path).expect("reopen");
        let removed =
            crate::extensions::grants::revoke_all(&conn, &identity, "demo").expect("revoke");
        assert_eq!(removed, 1, "the grant must have existed to be revoked");
    };
    let (result, ()) = tokio::join!(submit, revoke);

    crate::relay_admission::reset_rate_limit_gate();

    // The wait is what makes this a proof rather than a coincidence: the
    // revocation fires 300ms in, so without a real armed gate the submission
    // would already have completed and the revocation would be irrelevant.
    assert!(
        armed_at.elapsed() >= std::time::Duration::from_millis(900),
        "the submission must actually have parked in the gate (elapsed {:?})",
        armed_at.elapsed()
    );

    let refused = result.expect_err("a revoked grant must not publish");
    assert_eq!(
        refused.error.expect("error").code,
        code::DENIED,
        "a revocation during the wait is a denial, not a relay fault"
    );
    std::thread::sleep(std::time::Duration::from_millis(50));
    assert_eq!(
        connections.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "the grant was revoked while parked, so nothing may have reached the relay"
    );
}

#[tokio::test]
async fn a_relay_refusal_is_normalised_and_leaks_nothing() {
    let _gate = crate::relay_admission::gate_guard().await;
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

// ── proof 4: commit-then-drop-response ───────────────────────────────────────

/// A relay that **commits** every submission and drops the response the first
/// time, modelling transport ambiguity: the effect happened, the caller cannot
/// know it.
///
/// Deliberately scoped to transport. It records what it was sent and how often;
/// it does **not** implement `ON CONFLICT DO NOTHING`, because a fake that
/// implements the deduplication under test would be reciting the conclusion.
/// Whether Buzz's relay really deduplicates is proved against the real relay,
/// not here.
fn ambiguous_relay() -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
    use std::io::{Read as _, Write as _};

    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let committed: std::sync::Arc<std::sync::Mutex<Vec<String>>> =
        std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let log = committed.clone();

    std::thread::spawn(move || {
        let mut seen = 0usize;
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let mut buf = [0u8; 16384];
            let read = stream.read(&mut buf).unwrap_or(0);
            let body = String::from_utf8_lossy(&buf[..read]).to_string();
            let id = submitted_event_id(&body);

            // The commit happens either way — that is the whole point.
            log.lock().expect("log").push(id.clone());
            seen += 1;

            if seen == 1 {
                // First attempt: committed, then the connection dies before the
                // caller learns anything.
                drop(stream);
                continue;
            }
            let payload =
                format!(r#"{{"event_id":"{id}","accepted":true,"message":"duplicate:"}}"#);
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                payload.len()
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });

    (format!("http://{addr}"), committed)
}

#[tokio::test]
async fn a_dropped_response_then_an_exact_retry_yields_one_event_id() {
    let _gate = crate::relay_admission::gate_guard().await;
    // Proof 4. The first publish commits and the response is lost, so the
    // caller sees a failure for work that actually happened. Retrying the
    // *exact same template* must send the identical event id — which is what
    // lets the relay recognise it as the same operation rather than a second
    // one, and what makes the caller's retry safe.
    let (relay_url, committed) = ambiguous_relay();
    let keys = nostr::Keys::generate();
    let state = crate::app_state::build_app_state();
    *state.keys.lock().unwrap() = keys.clone();
    *state.relay_url_override.lock().unwrap() = Some(relay_url);

    // One template, retained by the caller and resubmitted unchanged.
    let now = 1_700_000_000i64;
    let params = template_params(Some(now));

    let first_event = canonicalise(&parse_template(Some(params.clone()), now).expect("parse"));
    let first = sign_and_publish(&first_event, &keys, &state, || Ok(())).await;
    assert!(
        first.is_err(),
        "a dropped response must surface as a failure, not a false success"
    );

    let retry_event = canonicalise(&parse_template(Some(params), now + 90).expect("reparse"));
    let retry = sign_and_publish(&retry_event, &keys, &state, || Ok(()))
        .await
        .expect("the retry must succeed once the relay answers");

    let log = committed.lock().expect("log");
    assert_eq!(log.len(), 2, "both attempts reached the relay");
    assert_eq!(
        log[0], log[1],
        "the retry must submit the identical event id — that is what makes it \
         the same operation rather than a second publish"
    );
    assert_eq!(
        retry["event"]["id"],
        serde_json::json!(log[1]),
        "and the reported id is the one that was committed"
    );
    // Normalisation: the caller sees an ordinary success, with no trace of the
    // relay's duplicate marker.
    assert_eq!(retry["relay"]["accepted"], serde_json::json!(true));
    let rendered = serde_json::to_string(&retry).expect("serialise");
    assert!(!rendered.contains("duplicate"), "no duplicate marker leaks");
}

#[tokio::test]
async fn a_missing_created_at_is_refused_before_signing_or_network() {
    // The Rust half of 8R, and the half that actually observes the refusal.
    //
    // Driven from the production entry point — `parse_template`, the same call
    // `publish_event` makes — with a listener counting connections, so "zero
    // network" is observed rather than inferred. Signing cannot have happened
    // either: `parse_template` returns before any key is touched.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let connections = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = connections.clone();
    std::thread::spawn(move || {
        for _ in listener.incoming() {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    });

    let now = 1_700_000_000i64;
    for (label, params) in [
        ("omitted", template_params(None)),
        ("null", {
            let mut map = serde_json::Map::new();
            map.insert("kind".into(), serde_json::json!(9));
            map.insert("content".into(), serde_json::json!("hi"));
            map.insert("tags".into(), serde_json::json!([["h", CHANNEL]]));
            map.insert("created_at".into(), Value::Null);
            Value::Object(map)
        }),
    ] {
        let refused = parse_template(Some(params), now)
            .err()
            .and_then(|reply| reply.error)
            .expect("must be refused");
        assert_eq!(
            refused.code,
            code::INVALID_PARAMS,
            "{label} created_at must be invalid_params"
        );
    }

    std::thread::sleep(std::time::Duration::from_millis(50));
    assert_eq!(
        connections.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "a refused template must reach neither the signer nor the network"
    );
}

/// A listener that accepts and counts connections without answering.
///
/// Counting connections rather than parsed requests is deliberate: the claim
/// under test is that nothing was *sent*, and a TCP connection is the earliest
/// observable evidence that it was.
fn counting_listener() -> (
    std::net::SocketAddr,
    std::sync::Arc<std::sync::atomic::AtomicUsize>,
) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let connections = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = connections.clone();
    std::thread::spawn(move || {
        for _ in listener.incoming() {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    });
    (addr, connections)
}

#[tokio::test]
async fn the_returned_event_is_the_signed_event_the_relay_received() {
    // §4's result is the **signed** event. The caller has to be able to verify
    // what it was told had been published, and until this test existed it
    // could not: the only assertion on the returned event compared its `id`,
    // so deleting `"sig"` from the result broke nothing.
    let _gate = crate::relay_admission::gate_guard().await;
    use nostr::JsonUtil as _;

    let (relay_url, received) = one_shot_relay();
    let keys = nostr::Keys::generate();
    let state = crate::app_state::build_app_state();
    *state.keys.lock().unwrap() = keys.clone();
    *state.relay_url_override.lock().unwrap() = Some(relay_url);

    let event = message(
        vec![tag(&["h", CHANNEL]), tag(&["p", &"b".repeat(64)])],
        "verify me",
    );

    let result = sign_and_publish(&event, &keys, &state, || Ok(()))
        .await
        .expect("the publish path must succeed against an accepting relay");

    let body = received
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("the relay must have received a POST body");
    let on_wire = nostr::Event::from_json(&body).expect("the body must be a nostr event");

    // The returned event stands on its own: it parses as a nostr event, and
    // its signature covers its own bytes. A projection without `sig` cannot
    // reach this line.
    let returned = nostr::Event::from_json(result["event"].to_string())
        .expect("the returned event must deserialize as a signed nostr event");
    returned
        .verify()
        .expect("the returned event must verify against its own signature");

    // And it is the same event that crossed the socket — compared field by
    // field rather than with `==`, which for a nostr event is an id
    // comparison and would pass with a different signature attached.
    assert_eq!(
        returned.sig, on_wire.sig,
        "the returned signature must be the one that was submitted"
    );
    assert_eq!(returned.id, on_wire.id);
    assert_eq!(returned.pubkey, on_wire.pubkey);
    assert_eq!(returned.kind, on_wire.kind);
    assert_eq!(returned.content, on_wire.content);
    assert_eq!(returned.created_at, on_wire.created_at);
    let returned_tags: Vec<Vec<String>> =
        returned.tags.iter().map(|t| t.clone().to_vec()).collect();
    let wire_tags: Vec<Vec<String>> = on_wire.tags.iter().map(|t| t.clone().to_vec()).collect();
    assert_eq!(returned_tags, wire_tags);
}

#[tokio::test]
async fn a_refused_template_reaches_neither_key_nor_socket_on_the_production_path() {
    // Proof 8R's production witness. The previous version called
    // `parse_template` directly against a listener that was never wired into
    // any state — zero connections were inevitable, because nothing in the
    // test could have connected. This drives the real `publish_event` entry
    // point with the listener reachable from the state it uses.
    let _gate = crate::relay_admission::gate_guard().await;
    use tauri::Manager as _;

    let (addr, connections) = counting_listener();
    let keys = nostr::Keys::generate();
    let state = crate::app_state::build_app_state();
    *state.keys.lock().unwrap() = keys.clone();
    *state.relay_url_override.lock().unwrap() = Some(format!("http://{addr}"));

    // Control: this exact state does reach that socket. Without it, the zero
    // below is unfalsifiable — an unreachable listener reports zero whatever
    // the code under test does.
    let control = message(vec![tag(&["h", CHANNEL])], "control");
    let _ = sign_and_publish(&control, &keys, &state, || Ok(())).await;
    assert_eq!(
        connections.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the listener must be reachable through this state for the assertion below to mean anything"
    );

    // The identity is now marked lost, which makes the ordering observable:
    // `signing_identity` refuses with `denied`, so if the key boundary were
    // reached before the template was parsed this test would see `denied`
    // instead of `invalid_params`.
    state
        .identity_lost
        .store(true, std::sync::atomic::Ordering::Release);

    let app = tauri::test::mock_app();
    app.manage(state);

    let reply = crate::extensions::publish::publish_event(
        app.handle(),
        "demo",
        LEASE,
        Some(template_params(None)),
    )
    .await;

    assert!(!reply.ok);
    assert_eq!(
        reply.error.as_ref().map(|e| e.code.as_str()),
        Some(code::INVALID_PARAMS),
        "a template with no created_at must be refused by the parser, before the key is consulted"
    );
    assert_eq!(
        connections.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the refused template must not have produced a second connection"
    );
}
