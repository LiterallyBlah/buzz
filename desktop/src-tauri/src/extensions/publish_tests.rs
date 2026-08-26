//! Tests for the mediated signer (§4).

use super::*;
use crate::extensions::dispatch::code;
use crate::extensions::manifest::EXTENSION_SIGNABLE_KINDS;

const CHANNEL: &str = "11111111-2222-3333-4444-555555555555";
const OTHER_CHANNEL: &str = "99999999-8888-7777-6666-555555555555";

/// A grant of kind 9 in `CHANNEL`, and nothing else.
fn granted_kind9_in_channel(kind_value: u32, channel: &str) -> bool {
    kind_value == kind::KIND_STREAM_MESSAGE && channel == CHANNEL
}

fn tag(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|p| (*p).to_string()).collect()
}

/// A well-formed kind-9 message in the granted channel.
fn message(tags: Vec<Vec<String>>, content: &str) -> CanonicalEvent {
    CanonicalEvent {
        kind: kind::KIND_STREAM_MESSAGE,
        content: content.to_string(),
        tags,
        created_at: 1_700_000_000,
    }
}

/// Refuse nothing. Used to isolate a single gate: with every `(kind, channel)`
/// granted, the only thing left that can refuse is the check under test.
///
/// Without this, an earlier gate is untestable — the checks are ordered so
/// later ones catch the same cases, so deleting the denylist left every
/// "is it denied?" test green. Asserting *which* gate refused, with the others
/// unable to fire, is what makes each one independently defended.
fn grants_everything(_kind: u32, _channel: &str) -> bool {
    true
}

fn refusal(event: &CanonicalEvent) -> Option<Refusal> {
    authorise(event, granted_kind9_in_channel).err()
}

fn refusal_with_everything_granted(event: &CanonicalEvent) -> Option<Refusal> {
    authorise(event, grants_everything).err()
}

#[test]
fn a_granted_message_in_a_granted_channel_is_authorised() {
    // The positive control. Without it, every negative below could be passing
    // because `authorise` refuses everything.
    let event = message(vec![tag(&["h", CHANNEL])], "hello");
    assert_eq!(
        authorise(&event, granted_kind9_in_channel),
        Ok(()),
        "an ordinary granted message must be signable"
    );
}

#[test]
fn ordinary_tags_are_permitted_on_a_granted_message() {
    // §4: a bare `p` mention on kind 9 is a benign notify, and `e`/`q`/`t`/
    // `emoji` are ordinary content tags. If these were refused the signer would
    // be unusable for the scenario it exists to serve.
    let event = message(
        vec![
            tag(&["h", CHANNEL]),
            tag(&["p", &"a".repeat(64)]),
            tag(&["e", &"b".repeat(64)]),
            tag(&["q", &"c".repeat(64)]),
            tag(&["t", "topic"]),
            tag(&["emoji", "shortcode", "https://example.invalid/e.png"]),
        ],
        "hello",
    );
    assert_eq!(authorise(&event, granted_kind9_in_channel), Ok(()));
}

// ── check 2: allowlist ∩ granted ─────────────────────────────────────────────

#[test]
fn a_non_allowlisted_kind_is_denied() {
    let event = CanonicalEvent {
        kind: 1, // note — dropped from v1 (§4)
        ..message(vec![tag(&["h", CHANNEL])], "hello")
    };
    // Everything granted, so only the allowlist can be the refuser.
    assert_eq!(
        refusal_with_everything_granted(&event),
        Some(Refusal::NotAllowlisted)
    );
}

#[test]
fn an_allowlisted_kind_without_a_grant_is_denied() {
    // Granted-but-out-of-scope in the *kind* direction: 45001 is allowlisted
    // and this extension holds no grant for it.
    let event = CanonicalEvent {
        kind: kind::KIND_FORUM_POST,
        ..message(vec![tag(&["h", CHANNEL])], "a post")
    };
    assert_eq!(refusal(&event), Some(Refusal::ChannelNotGranted));
}

#[test]
fn a_reaction_is_refused_like_any_other_non_signable_kind() {
    // Kind 7 is out of the v1 allowlist (design-repo §4, `d640883`) because a
    // reaction's channel comes from its `e` target, not from `h` — so a grant
    // for channel A could reach channel B and the host could not tell without
    // resolving the target.
    //
    // It is refused as an ordinary non-allowlisted kind: no reaction-specific
    // code, and no `e`-target resolution. The three targets below — one in the
    // granted channel, one elsewhere, one that does not resolve — are refused
    // identically, so a refused extension learns nothing about whether the
    // target exists or where it lives.
    let targets = [
        tag(&["e", &"a".repeat(64)]),
        tag(&["e", &"b".repeat(64)]),
        tag(&["e", &"c".repeat(64)]),
    ];
    let mut rendered = Vec::new();
    for target in targets {
        let event = CanonicalEvent {
            kind: kind::KIND_REACTION,
            ..message(vec![tag(&["h", CHANNEL]), target], "+")
        };
        assert_eq!(
            refusal_with_everything_granted(&event),
            Some(Refusal::NotAllowlisted),
            "a reaction must be refused by the allowlist, whatever it points at"
        );
        let refused = refusal_with_everything_granted(&event).expect("refused");
        rendered.push(format!("{}|{}", refused.code(), refused.message()));
    }
    assert_eq!(
        rendered
            .iter()
            .collect::<std::collections::HashSet<_>>()
            .len(),
        1,
        "every reaction refusal must be byte-identical, whatever the target"
    );
}

#[test]
fn kind_30800_is_not_reachable_through_publish_event() {
    // Allowlisted, but §4 routes it through `extensionData` so the *host*
    // builds `d = ext:<extid>:<key>`. Reaching it here would let an extension
    // supply its own `d` and address another extension's namespace.
    let event = CanonicalEvent {
        kind: kind::KIND_EXTENSION_DATA,
        ..message(vec![tag(&["h", CHANNEL])], "{}")
    };
    assert_eq!(
        refusal_with_everything_granted(&event),
        Some(Refusal::WrongMethodForKind)
    );
}

// ── check 3: channel scope ───────────────────────────────────────────────────

#[test]
fn a_message_with_no_channel_tag_is_denied() {
    let event = message(vec![], "hello");
    assert_eq!(
        refusal_with_everything_granted(&event),
        Some(Refusal::ChannelTagNotSingular)
    );
}

#[test]
fn a_message_in_an_ungranted_channel_is_denied() {
    // Granted-but-out-of-scope in the *channel* direction: right kind, wrong
    // channel. This is the grant model's whole point.
    let event = message(vec![tag(&["h", OTHER_CHANNEL])], "hello");
    assert_eq!(refusal(&event), Some(Refusal::ChannelNotGranted));
}

#[test]
fn a_second_channel_tag_cannot_redirect_the_event() {
    // The D-2a escape, in its tag form: a grant for channel X plus a smuggled
    // second `h` naming channel Y. Both orderings are refused, so the check
    // cannot be beaten by putting the granted channel first.
    for tags in [
        vec![tag(&["h", CHANNEL]), tag(&["h", OTHER_CHANNEL])],
        vec![tag(&["h", OTHER_CHANNEL]), tag(&["h", CHANNEL])],
    ] {
        let event = message(tags, "hello");
        assert_eq!(
            refusal_with_everything_granted(&event),
            Some(Refusal::ChannelTagNotSingular),
            "a second channel tag must not be accepted in either order"
        );
    }
}

// ── check 4: tag scope ───────────────────────────────────────────────────────

#[test]
fn a_privilege_bearing_tag_is_denied() {
    // decision 003's concrete escape: a `role` tag turning an innocuous grant
    // into an admin action. `expiration` is the authority-flavoured one §4
    // names, and `a` is refused because no allowlisted kind needs a coordinate.
    for privileged in [
        tag(&["role", "admin"]),
        tag(&["expiration", "9999999999"]),
        tag(&["a", "39000:deadbeef:other-channel"]),
        tag(&["d", "ext:someone-else:key"]),
    ] {
        let event = message(vec![tag(&["h", CHANNEL]), privileged.clone()], "hello");
        assert_eq!(
            refusal_with_everything_granted(&event),
            Some(Refusal::TagNotPermitted),
            "tag {privileged:?} must not be settable by an extension"
        );
    }
}

#[test]
fn an_unknown_tag_is_denied_rather_than_ignored() {
    // The allowlist's reason for existing: a privilege tag Buzz adds next month
    // is refused today, without anyone remembering to deny it.
    let event = message(
        vec![tag(&["h", CHANNEL]), tag(&["some-tag-invented-later", "x"])],
        "hello",
    );
    assert_eq!(
        refusal_with_everything_granted(&event),
        Some(Refusal::TagNotPermitted)
    );
}

#[test]
fn a_nameless_tag_is_invalid_params_not_denied() {
    // An empty tag array is a malformed template, not an authority failure.
    let event = message(vec![tag(&["h", CHANNEL]), vec![]], "hello");
    let refused = refusal_with_everything_granted(&event).expect("must refuse");
    assert_eq!(refused, Refusal::MalformedTag);
    assert_eq!(
        refused.code(),
        code::INVALID_PARAMS,
        "a nameless tag is a malformed template, not an authority failure"
    );
}

// ── check 5: content-directive guard ─────────────────────────────────────────

#[test]
fn agent_control_content_is_denied_on_kind_9() {
    // §4 check 6: publishing a message must not smuggle an agent-control
    // action through content. The `p` tag is not required — refusing only the
    // exact `!shutdown` + `p` pair would leave the host guessing which
    // harnesses match on content alone.
    for content in ["!shutdown", "  !shutdown", "!restart now", "!Shutdown"] {
        let event = message(vec![tag(&["h", CHANNEL])], content);
        assert_eq!(
            refusal(&event),
            Some(Refusal::AgentControlDirective),
            "content {content:?} matches the directive convention and must be refused"
        );
    }
}

#[test]
fn ordinary_content_that_merely_starts_with_a_bang_is_allowed() {
    // The guard must not swallow ordinary writing. `!` followed by anything
    // that is not a command word is a normal message.
    for content in ["!", "!!", "! spaced", "hello !shutdown", "!1 first"] {
        let event = message(vec![tag(&["h", CHANNEL])], content);
        assert_eq!(
            authorise(&event, granted_kind9_in_channel),
            Ok(()),
            "content {content:?} is ordinary and must be signable"
        );
    }
}

// ── canonicalisation ─────────────────────────────────────────────────────────

/// `parse_template` over a minimal valid publish body.
fn template_params(created_at: Option<i64>) -> Value {
    let mut map = serde_json::Map::new();
    map.insert("kind".into(), serde_json::json!(9));
    map.insert("content".into(), serde_json::json!("hello"));
    map.insert("tags".into(), serde_json::json!([["h", CHANNEL]]));
    if let Some(value) = created_at {
        map.insert("created_at".into(), serde_json::json!(value));
    }
    Value::Object(map)
}

fn parse_code(params: Value, now: i64) -> Result<EventTemplate, String> {
    parse_template(Some(params), now).map_err(|reply| {
        reply
            .error
            .map(|e| e.code)
            .unwrap_or_else(|| "(none)".to_string())
    })
}

#[test]
fn created_at_is_required() {
    // No default-to-now path. A caller that omits it would get a different
    // event id on every retry and double-publish the first time anything went
    // wrong — the id is a hash over the timestamp, so an unstable timestamp is
    // an unstable operation identity.
    let now = 1_700_000_000i64;
    assert_eq!(
        parse_code(template_params(None), now).unwrap_err(),
        code::INVALID_PARAMS
    );
}

#[test]
fn a_timestamp_inside_the_window_is_used_exactly() {
    // The load-bearing half: it must arrive unmodified, because adjusting it
    // would change the id the caller will retry with.
    let now = 1_700_000_000i64;
    for offset in [
        0,
        -30,
        30,
        -CREATED_AT_SKEW_SECONDS,
        CREATED_AT_SKEW_SECONDS,
    ] {
        let pinned = now + offset;
        let template =
            parse_template(Some(template_params(Some(pinned))), now).expect("inside the window");
        assert_eq!(
            template.created_at, pinned,
            "offset {offset} must be preserved exactly, not adjusted"
        );
        assert_eq!(
            canonicalise(&template).created_at,
            pinned,
            "and must survive canonicalisation unchanged"
        );
    }
}

#[test]
fn a_timestamp_outside_the_window_is_rejected_not_clamped() {
    // The other half, and the one a clamp would silently break: an
    // out-of-window timestamp must be refused, never pulled to the edge. A
    // clamp would hand back an event whose id the caller cannot reproduce.
    let now = 1_700_000_000i64;
    for offset in [
        -CREATED_AT_SKEW_SECONDS - 1,
        CREATED_AT_SKEW_SECONDS + 1,
        -86_400,
        86_400,
    ] {
        assert_eq!(
            parse_code(template_params(Some(now + offset)), now).unwrap_err(),
            code::INVALID_PARAMS,
            "offset {offset} must be refused"
        );
    }
}

#[test]
fn the_window_boundary_is_inclusive_on_both_sides() {
    // Both sides, individually — the edge is where an off-by-one lives, and
    // asserting only the inside would not see it.
    let now = 1_700_000_000i64;
    assert!(timestamp_in_window(now - CREATED_AT_SKEW_SECONDS, now));
    assert!(timestamp_in_window(now + CREATED_AT_SKEW_SECONDS, now));
    assert!(!timestamp_in_window(now - CREATED_AT_SKEW_SECONDS - 1, now));
    assert!(!timestamp_in_window(now + CREATED_AT_SKEW_SECONDS + 1, now));
}

#[test]
fn a_retry_of_the_same_template_rebuilds_the_same_event() {
    // The property the whole mechanism rests on: same template in, byte-identical
    // canonical event out, so the signed id is the same and the relay recognises
    // the retry as the operation it already committed.
    let now = 1_700_000_000i64;
    let params = template_params(Some(now - 5));
    let first = canonicalise(&parse_template(Some(params.clone()), now).expect("first"));
    // A later "now" must not perturb it — only the pinned value is used.
    let second = canonicalise(&parse_template(Some(params), now + 120).expect("retry"));
    assert_eq!(
        first, second,
        "a retry must rebuild the identical canonical event"
    );
}

#[test]
fn a_non_integer_created_at_is_rejected() {
    let now = 1_700_000_000i64;
    let mut map = serde_json::Map::new();
    map.insert("kind".into(), serde_json::json!(9));
    map.insert("created_at".into(), serde_json::json!("1700000000"));
    assert_eq!(
        parse_code(Value::Object(map), now).unwrap_err(),
        code::INVALID_PARAMS
    );
}

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
    let reply = sign_and_publish(&event, &keys, &state, || {
        Err("the grant was revoked while the request was queued".to_string())
    })
    .await
    .expect_err("a revoked grant must not publish");
    assert_eq!(reply.error.expect("error").code, code::RELAY_ERROR);

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

/// An independent transcription of the never-grantable denylist.
///
/// Written from `docs/DESIGN_AUDIT.md` D-2a and BRIDGE_SPEC §4 check 1 — **not**
/// from the implementation, and deliberately in integer literals rather than
/// `kind::KIND_*` constants. Sharing the constants would make both sides move
/// together if one were renumbered, which is the failure this oracle exists to
/// catch. Every entry cites the D-2a bullet it comes from.
///
/// If the spec changes, this changes first and the implementation follows.
fn spec_never_grantable(kind_value: u32) -> bool {
    matches!(
        kind_value,
        // §4 check 1: "relay-only (`is_relay_only_kind`)" — the six kinds only
        // the relay may author.
        13534 | 40901 | 40902 | 30622 | 39005 | 39006
        // D-2a "Deploy / workflow / approval".
        | 30620 | 46020 | 46030 | 46031
        // D-2a "Membership / group admin" — NIP-29, relay admin, DM membership.
        | 9000 | 9001 | 9002 | 9005 | 9007 | 9008 | 9009
        | 9030 | 9031 | 9032 | 9033
        | 41010 | 41011 | 41012
        // D-2a "Moderation" — 9040–9044.
        | 9040 | 9041 | 9042 | 9043 | 9044
        // D-2a "Identity archival".
        | 9035 | 9036
        // D-2a "Auth / bearer-credential".
        | 22242 | 24242 | 27235 | 24243
        // D-2a "Agent control".
        | 24200
        // D-2a "Git push / ref authority".
        | 30617 | 30618 | 1631 | 1632
        // D-2a "Deletion".
        | 5
    )
}

#[test]
fn the_denylist_matches_the_decision_exactly_in_both_directions() {
    // Sweeping the whole kind space catches a denylist that is *narrower* than
    // the decision (a hole an extension could sign through) and one that is
    // *wider* (a policy nobody wrote down, which is how a list stops being
    // auditable). `manifest_tests.rs` carries the same shape for §5's read
    // floor, after an earlier revision asserted the implementation against
    // itself.
    let mut denied = 0usize;
    for kind_value in 0..=50_000u32 {
        assert_eq!(
            is_never_grantable_kind(kind_value),
            spec_never_grantable(kind_value),
            "kind {kind_value}: denylist disagrees with D-2a / BRIDGE_SPEC §4"
        );
        if spec_never_grantable(kind_value) {
            denied += 1;
        }
    }
    // Pins the oracle's own size. The sweep above proves implementation and
    // oracle agree; this catches an edit that removes an arm from *both* and
    // would therefore still sweep clean.
    //
    // 6 relay-only + 4 deploy/workflow/approval + 14 membership/group-admin
    // (7 NIP-29 + 4 relay-admin + 3 DM) + 5 moderation + 2 identity-archival
    // + 4 auth/bearer + 1 agent-control + 4 git + 1 deletion.
    assert_eq!(
        denied, 41,
        "the oracle enumerates 41 never-grantable kinds; a change here is a spec change"
    );
}

#[test]
fn the_denylist_is_sourced_from_buzz_core_not_copied() {
    // The point of borrowing buzz-core's predicates is that a kind
    // reclassified there is reclassified here without anyone remembering. If
    // someone replaces the predicate calls with an inline list, these stop
    // agreeing — buzz-core stays the single writer for these families.
    for kind_value in 0..=50_000u32 {
        if kind::is_relay_only_kind(kind_value)
            || kind::is_command_kind(kind_value)
            || kind::is_relay_admin_kind(kind_value)
            || kind::is_moderation_command_kind(kind_value)
            || kind::is_identity_archive_request_kind(kind_value)
        {
            assert!(
                is_never_grantable_kind(kind_value),
                "kind {kind_value} is classified as authority-bearing by buzz-core \
                 but the signer denylist does not refuse it"
            );
        }
    }
}

#[test]
fn the_allowlist_and_the_denylist_do_not_overlap() {
    // A kind in both lists would mean the spec contradicts itself, and the
    // ordering of the two gates would become load-bearing by accident.
    for kind_value in EXTENSION_SIGNABLE_KINDS {
        assert!(
            !is_never_grantable_kind(*kind_value),
            "kind {kind_value} is both signable and never-grantable"
        );
    }
}

#[test]
fn the_denylist_refuses_before_the_allowlist_is_consulted() {
    // §4 check 1 is defence in depth, and the gates are ordered so the
    // allowlist would refuse these anyway. That redundancy is what makes a
    // "was it denied?" assertion worthless here: deleting the denylist leaves
    // such a test green, because the next gate catches the same case.
    //
    // Naming the refusing gate is what makes check 1 independently defended —
    // remove it and these become `NotAllowlisted`, and this fails.
    //
    // 9000 (add-member) is the D-2a escape decision 003 names: a grant to sign
    // in channel X plus a tag redirect would otherwise be a takeover of Y.
    for kind_value in [9000u32, 46020, 22242, 5, 9040, 30617] {
        let event = CanonicalEvent {
            kind: kind_value,
            ..message(vec![tag(&["h", CHANNEL])], "hello")
        };
        assert_eq!(
            refusal_with_everything_granted(&event),
            Some(Refusal::NeverGrantable),
            "kind {kind_value} must be stopped by the denylist, not merely by the allowlist"
        );
    }
}

#[test]
fn no_allowlisted_kind_is_shadowed_by_the_denylist() {
    // The mirror direction: the denylist must not be quietly refusing kinds the
    // spec says are signable. Every allowlisted kind reaches at least as far as
    // the grant check.
    for kind_value in EXTENSION_SIGNABLE_KINDS {
        let event = CanonicalEvent {
            kind: *kind_value,
            ..message(vec![tag(&["h", CHANNEL])], "hello")
        };
        assert_ne!(
            refusal_with_everything_granted(&event),
            Some(Refusal::NeverGrantable),
            "kind {kind_value} is allowlisted but the denylist refuses it"
        );
    }
}
