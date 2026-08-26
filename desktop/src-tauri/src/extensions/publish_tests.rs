//! The six ordered §4 checks, and the canonical event they read.

use super::publish_test_support::*;
use super::*;
use crate::extensions::dispatch::code;

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
fn the_host_never_originates_a_created_at() {
    // Proof 8, in the only form it can take on this branch: there is no §11
    // client shim — `window.buzz` is injected nowhere — so no component claims
    // to supply a stable `created_at` on the caller's behalf, and there is
    // nothing that could commit the fraud proof 8 guards against.
    //
    // That is only honest if the absence is enforced rather than incidental.
    //
    // The **behavioural** assertion at the end is the real guard: an omitted
    // `created_at` must be refused. The source-smell list below is narrow
    // defence-in-depth against the one shape the previous default took, and it
    // is deliberately not claimed to be exhaustive — a mutation reintroducing
    // the default as `.or(Some(now))` slips past these needles and is caught by
    // the behavioural check instead. Needles are split so this file's own text
    // cannot satisfy them.
    let source = include_str!("publish.rs");
    for smell in [
        concat!("created_at", ".unwrap_or"),
        concat!("unwrap_or(", "now"),
        concat!("unwrap_or_else(", "now_unix"),
    ] {
        assert!(
            !source.contains(smell),
            "the host must never originate a created_at; found {smell:?}"
        );
    }

    // And behaviourally: a template without one is refused rather than filled.
    let now = 1_700_000_000i64;
    assert_eq!(
        parse_code(template_params(None), now).unwrap_err(),
        code::INVALID_PARAMS,
        "an omitted created_at must be refused, never defaulted"
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
