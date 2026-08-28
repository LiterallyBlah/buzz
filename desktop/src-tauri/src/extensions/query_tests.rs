//! §5 `query.events` — grammar, constructive rewriting, and the per-event
//! verifier.
//!
//! The three hostile cases this module exists for, each with its own test:
//! the **cross product** (a kind granted only in channel B becoming readable in
//! A), the **stray `h`** (a global event carrying a signed `h` naming a granted
//! channel), and the **valueless `["h"]`** that makes a two-`h` event read as
//! one. Each is a way an extension could see an event nobody granted it.

use super::*;

const CHAN_A: &str = "11111111-1111-4111-8111-111111111111";
const CHAN_B: &str = "22222222-2222-4222-8222-222222222222";
const EXTID: &str = "demo";

fn temp_db() -> (tempfile::TempDir, rusqlite::Connection) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("grants").join("extension-grants.db");
    let conn = super::super::grants::open_grant_db(&path).expect("open");
    (dir, conn)
}

fn identity() -> String {
    "a".repeat(64)
}

/// Always go through the real grammar: a hand-built `ValidatedRequest` would
/// let a test assert on a shape the grammar would never have produced.
fn request(filter: serde_json::Value) -> Result<ValidatedRequest, QueryError> {
    validate_request(&serde_json::json!({ "filter": filter }))
}

fn ok_request(filter: serde_json::Value) -> ValidatedRequest {
    match request(filter) {
        Ok(request) => request,
        Err(_) => panic!("filter should have been accepted"),
    }
}

fn code_of(error: &QueryError) -> &'static str {
    match error {
        QueryError::InvalidParams(_) => "invalid_params",
        QueryError::Denied(_) => "denied",
        QueryError::QuotaExceeded(_) => "quota_exceeded",
        QueryError::Relay => "relay_error",
    }
}

fn signed_event(keys: &nostr::Keys, kind: u32, tags: Vec<Vec<String>>) -> nostr::Event {
    let mut builder = nostr::EventBuilder::new(nostr::Kind::from(kind as u16), "{}");
    for tag in tags {
        builder = builder.tag(nostr::Tag::parse(tag).expect("tag"));
    }
    builder.sign_with_keys(keys).expect("sign")
}

fn h(channel: &str) -> Vec<String> {
    vec!["h".to_string(), channel.to_string()]
}

// ── grammar ────────────────────────────────────────────────────────────────

#[test]
fn an_unknown_filter_key_is_refused_rather_than_dropped() {
    // Silently ignoring it would answer a different question than the one
    // asked, which the caller cannot detect.
    let error = request(serde_json::json!({ "kinds": [9], "search": "secrets" }))
        .err()
        .expect("unknown key must be refused");
    assert_eq!(code_of(&error), "invalid_params");
}

#[test]
fn an_empty_array_is_refused_rather_than_read_as_absent() {
    // The load-bearing one. Treating `kinds: []` as "unset" would WIDEN the
    // request to every granted kind — the one direction a filter mistake must
    // never move — so it is a parameter error, not a synonym for omission.
    let error = request(serde_json::json!({ "kinds": [] }))
        .err()
        .expect("empty kinds must be refused");
    assert_eq!(code_of(&error), "invalid_params");

    let error = request(serde_json::json!({ "#h": [] }))
        .err()
        .expect("empty #h must be refused");
    assert_eq!(code_of(&error), "invalid_params");
}

#[test]
fn hex_axes_require_exactly_64_lowercase_hex() {
    for bad in [
        serde_json::json!({ "ids": ["abc"] }),              // prefix
        serde_json::json!({ "ids": ["A".repeat(64)] }),     // uppercase
        serde_json::json!({ "authors": ["g".repeat(64)] }), // non-hex
        serde_json::json!({ "authors": ["a".repeat(63)] }), // short
        serde_json::json!({ "ids": [1] }),                  // not a string
    ] {
        let error = request(bad).err().expect("bad hex axis must be refused");
        assert_eq!(code_of(&error), "invalid_params");
    }
    assert!(request(serde_json::json!({ "ids": ["a".repeat(64)] })).is_ok());
}

#[test]
fn a_since_after_until_is_refused() {
    let error = request(serde_json::json!({ "since": 100, "until": 99 }))
        .err()
        .expect("inverted window must be refused");
    assert_eq!(code_of(&error), "invalid_params");
    assert!(request(serde_json::json!({ "since": 99, "until": 100 })).is_ok());
}

#[test]
fn a_limit_over_the_cap_is_quota_exceeded_not_silently_lowered() {
    // Quietly clamping would hand back a short page the caller reads as the
    // whole answer.
    let error = request(serde_json::json!({ "limit": OVERALL_RESULT_CAP + 1 }))
        .err()
        .expect("over-cap limit must be refused");
    assert_eq!(code_of(&error), "quota_exceeded");

    let error = request(serde_json::json!({ "limit": 0 }))
        .err()
        .expect("zero limit must be refused");
    assert_eq!(code_of(&error), "invalid_params");
}

#[test]
fn an_absent_limit_injects_the_overall_cap() {
    assert_eq!(ok_request(serde_json::json!({})).limit, OVERALL_RESULT_CAP);
}

#[test]
fn an_h_value_that_is_not_a_channel_uuid_is_refused() {
    let error = request(serde_json::json!({ "#h": ["not-a-uuid"] }))
        .err()
        .expect("non-uuid #h must be refused");
    assert_eq!(code_of(&error), "invalid_params");
}

#[test]
fn an_axis_over_the_value_bound_is_refused() {
    let many: Vec<u32> = (0..(MAX_AXIS_VALUES as u32 + 1)).collect();
    let error = request(serde_json::json!({ "kinds": many }))
        .err()
        .expect("over-bound axis must be refused");
    assert_eq!(code_of(&error), "invalid_params");
}

#[test]
fn size_is_checked_before_shape() {
    // A huge document is refused without walking it, so an attacker cannot
    // make the host do unbounded parsing work to reach a rejection.
    let big = "a".repeat(MAX_REQUEST_BYTES + 1);
    let error = validate_request(&serde_json::json!({ "filter": { "#t": [big] } }))
        .err()
        .expect("oversize params must be refused");
    assert_eq!(code_of(&error), "invalid_params");
}

// ── constructive rewriting ─────────────────────────────────────────────────

#[test]
fn one_filter_per_channel_each_carrying_exactly_one_h() {
    // The relay pushes `#h` to the strict `channel_id = C` predicate only for a
    // single value; two values fall back to one that admits global rows.
    let granted = vec![(9u32, CHAN_A.to_string()), (9u32, CHAN_B.to_string())];
    let filters = construct_filters(&granted, &ok_request(serde_json::json!({}))).expect("build");
    assert_eq!(filters.as_filters().len(), 2);
    for filter in filters.as_filters() {
        let hs = filter["#h"].as_array().expect("#h array");
        assert_eq!(
            hs.len(),
            1,
            "every emitted filter must carry exactly one #h"
        );
    }
}

#[test]
fn a_kind_granted_only_in_one_channel_does_not_become_readable_in_another() {
    // THE CROSS-PRODUCT PROBE. Grant 9 in A and 45001 in B, then ask for 45001
    // in A. An axis-wise intersection (`kinds ∩ granted` × `#h ∩ granted`)
    // would emit `kinds:[45001], #h:[A]` — a pair nobody granted.
    let granted = vec![(9u32, CHAN_A.to_string()), (45001u32, CHAN_B.to_string())];
    let error = construct_filters(
        &granted,
        &ok_request(serde_json::json!({ "kinds": [45001], "#h": [CHAN_A] })),
    )
    .err()
    .expect("the cross product must not survive construction");
    assert_eq!(code_of(&error), "denied");
}

#[test]
fn the_surviving_pair_set_never_contains_an_ungranted_pair() {
    // The general form of the probe above: whatever the request asks for, every
    // pair that comes out was granted.
    let granted = vec![
        (9u32, CHAN_A.to_string()),
        (45001u32, CHAN_B.to_string()),
        (40002u32, CHAN_A.to_string()),
    ];
    let filters = construct_filters(&granted, &ok_request(serde_json::json!({}))).expect("build");
    for pair in filters.pairs() {
        assert!(
            granted.contains(pair),
            "construction emitted an ungranted pair: {pair:?}"
        );
    }
    // And each emitted filter's kinds are only those granted in its own channel.
    for filter in filters.as_filters() {
        let channel = filter["#h"][0].as_str().expect("channel");
        for kind in filter["kinds"].as_array().expect("kinds") {
            let kind = kind.as_u64().expect("kind") as u32;
            assert!(granted.contains(&(kind, channel.to_string())));
        }
    }
}

#[test]
fn a_request_naming_only_ungranted_channels_is_denied_not_empty() {
    // An empty success would tell the extension the channel is empty, which is
    // a different and false statement.
    let granted = vec![(9u32, CHAN_A.to_string())];
    let error = construct_filters(&granted, &ok_request(serde_json::json!({ "#h": [CHAN_B] })))
        .err()
        .expect("ungranted channel must deny");
    assert_eq!(code_of(&error), "denied");
}

#[test]
fn a_request_naming_only_ungranted_kinds_is_denied() {
    let granted = vec![(9u32, CHAN_A.to_string())];
    let error = construct_filters(
        &granted,
        &ok_request(serde_json::json!({ "kinds": [45001] })),
    )
    .err()
    .expect("ungranted kind must deny");
    assert_eq!(code_of(&error), "denied");
}

#[test]
fn a_mixed_request_naming_a_floor_kind_fails_whole() {
    // Not narrowed to the remaining kinds: answering a smaller question than
    // the one asked is how a caller comes to believe a kind is simply absent.
    let granted = vec![(9u32, CHAN_A.to_string())];
    let error = construct_filters(
        &granted,
        &ok_request(serde_json::json!({ "kinds": [9, 30800] })),
    )
    .err()
    .expect("a floor kind must fail the whole request");
    assert_eq!(code_of(&error), "denied");
}

#[test]
fn extension_data_is_unreachable_through_the_read_path() {
    // 30800's only read path is `extensionData.get`, which carries its own
    // authorship and namespace constraint.
    let granted = vec![(9u32, CHAN_A.to_string())];
    let error = construct_filters(
        &granted,
        &ok_request(serde_json::json!({ "kinds": [30800] })),
    )
    .err()
    .expect("30800 must not be reachable via query.events");
    assert_eq!(code_of(&error), "denied");
}

#[test]
fn a_mixed_request_naming_a_non_allowlisted_kind_fails_whole() {
    // Kind 1 is not on the floor, but it is not channel-readable either: it is
    // a global kind whose stray signed `h` would still match an `#h` filter.
    let granted = vec![(9u32, CHAN_A.to_string())];
    let error = construct_filters(
        &granted,
        &ok_request(serde_json::json!({ "kinds": [9, 1] })),
    )
    .err()
    .expect("a non-allowlisted kind must fail the whole request");
    assert_eq!(code_of(&error), "denied");
}

#[test]
fn a_granted_pair_whose_kind_left_the_allowlist_is_not_emitted() {
    // The grant row outlives the allowlist edit. Re-checking at construction
    // means a kind that leaves the set stops being readable immediately, not at
    // the next install.
    let granted = vec![(1u32, CHAN_A.to_string()), (9u32, CHAN_A.to_string())];
    let filters = construct_filters(&granted, &ok_request(serde_json::json!({}))).expect("build");
    assert_eq!(filters.pairs(), &[(9u32, CHAN_A.to_string())]);
}

#[test]
fn the_scalar_axes_are_copied_into_every_emitted_filter() {
    let granted = vec![(9u32, CHAN_A.to_string()), (9u32, CHAN_B.to_string())];
    let filters = construct_filters(
        &granted,
        &ok_request(serde_json::json!({
            "authors": ["b".repeat(64)],
            "since": 10,
            "until": 20,
            "#e": ["c".repeat(64)],
        })),
    )
    .expect("build");
    assert_eq!(filters.as_filters().len(), 2);
    for filter in filters.as_filters() {
        assert_eq!(filter["authors"][0].as_str(), Some("b".repeat(64).as_str()));
        assert_eq!(filter["since"].as_u64(), Some(10));
        assert_eq!(filter["until"].as_u64(), Some(20));
        assert_eq!(filter["#e"][0].as_str(), Some("c".repeat(64).as_str()));
    }
}

#[test]
fn too_many_channels_is_quota_exceeded_before_any_network_work() {
    // `limit: 1` so the aggregate-candidate bound cannot also fire: with the
    // default limit this passes even when the filter-count bound is deleted,
    // which would make it a test of the wrong gate.
    let granted: Vec<(u32, String)> = (0..(MAX_EMITTED_FILTERS + 1))
        .map(|n| (9u32, format!("{n:08}-1111-4111-8111-111111111111")))
        .collect();
    let error = construct_filters(&granted, &ok_request(serde_json::json!({ "limit": 1 })))
        .err()
        .expect("too many channels must be refused");
    assert_eq!(code_of(&error), "quota_exceeded");
}

#[test]
fn more_read_pairs_than_one_query_may_span_is_quota_exceeded() {
    // Spread over few channels, with `limit: 1`, so neither the filter-count
    // bound nor the aggregate bound can answer for this one. 24 allowlisted
    // kinds across 11 channels is 264 pairs — over MAX_READ_PAIRS (256) while
    // staying under MAX_EMITTED_FILTERS (32).
    const CHANNELS: u32 = 11;
    // Compile-time, so the fixture cannot silently start tripping the
    // neighbouring bound if either constant is retuned later.
    const { assert!(CHANNELS as usize <= MAX_EMITTED_FILTERS) };

    let mut granted: Vec<(u32, String)> = Vec::new();
    for channel in 0..CHANNELS {
        for kind in super::super::manifest::EXTENSION_CHANNEL_READABLE_KINDS {
            granted.push((*kind, format!("{channel:08}-1111-4111-8111-111111111111")));
        }
    }
    assert!(
        granted.len() > MAX_READ_PAIRS,
        "fixture must exceed the bound"
    );
    let error = construct_filters(&granted, &ok_request(serde_json::json!({ "limit": 1 })))
        .err()
        .expect("too many pairs must be refused");
    assert_eq!(code_of(&error), "quota_exceeded");
}

#[test]
fn the_aggregate_candidate_bound_is_checked_before_the_network() {
    // The relay runs each emitted filter with its own limit and appends, so the
    // aggregate is what costs — and it must be refused before any of that work
    // happens, not while reading the response.
    let channels = (MAX_FETCHED_CANDIDATES / OVERALL_RESULT_CAP) + 1;
    let granted: Vec<(u32, String)> = (0..channels)
        .map(|n| (9u32, format!("{n:08}-1111-4111-8111-111111111111")))
        .collect();
    let error = construct_filters(&granted, &ok_request(serde_json::json!({})))
        .err()
        .expect("aggregate fetch bound must be enforced");
    assert_eq!(code_of(&error), "quota_exceeded");
}

// ── per-event verification ─────────────────────────────────────────────────

/// A granted store plus the filters a plain `9 @ A` grant produces.
fn granted_a() -> (
    tempfile::TempDir,
    rusqlite::Connection,
    ConstrainedFilters,
    String,
) {
    let (dir, conn) = temp_db();
    let id = identity();
    super::super::grants::grant_read_scope(&conn, &id, EXTID, 9, CHAN_A).expect("grant");
    let filters = construct_filters(
        &[(9u32, CHAN_A.to_string())],
        &ok_request(serde_json::json!({})),
    )
    .expect("build");
    (dir, conn, filters, id)
}

#[test]
fn a_granted_event_in_a_granted_channel_verifies() {
    let (_dir, conn, filters, id) = granted_a();
    let keys = nostr::Keys::generate();
    let event = signed_event(&keys, 9, vec![h(CHAN_A)]);
    assert!(verify_event(&event, &filters, &conn, &id, EXTID));
}

#[test]
fn an_event_whose_signature_does_not_cover_it_is_refused() {
    let (_dir, conn, filters, id) = granted_a();
    let keys = nostr::Keys::generate();
    let mut event = signed_event(&keys, 9, vec![h(CHAN_A)]);
    // Same event id, different content: the signature no longer covers it.
    event.content = "{\"tampered\":true}".to_string();
    assert!(!verify_event(&event, &filters, &conn, &id, EXTID));
}

#[test]
fn a_global_kind_carrying_a_stray_h_naming_a_granted_channel_is_refused() {
    // THE STRAY-`h` PROBE. Kind 1 is not channel-readable: the relay does not
    // assign its channel, so its `h` is whatever the author signed. Even
    // carrying a granted channel, it must not reach the extension.
    //
    // **Kind 1 is granted in the store on purpose.** Manifest validation would
    // never let such a row exist, which is exactly why the verifier must not
    // lean on that: with kind 1 ungranted, the live grant lookup refuses the
    // event and this test passes even with the allowlist clause deleted. This
    // is the defence-in-depth case — a grant row for a kind that should never
    // have been grantable — so the allowlist clause is the only thing left to
    // reject it.
    let (_dir, conn) = temp_db();
    let id = identity();
    super::super::grants::grant_read_scope(&conn, &id, EXTID, 9, CHAN_A).expect("grant");
    super::super::grants::grant_read_scope(&conn, &id, EXTID, 1, CHAN_A).expect("grant");
    let filters = construct_filters(
        &[(9u32, CHAN_A.to_string())],
        &ok_request(serde_json::json!({})),
    )
    .expect("build");
    let keys = nostr::Keys::generate();
    let event = signed_event(&keys, 1, vec![h(CHAN_A)]);
    assert!(
        super::super::grants::has_read_scope(&conn, &id, EXTID, 1, CHAN_A),
        "the grant lookup must not be what rejects this event"
    );
    assert!(!verify_event(&event, &filters, &conn, &id, EXTID));
}

#[test]
fn an_event_carrying_two_h_tags_is_refused_rather_than_resolved() {
    // Ambiguous placement is refused, not resolved by picking one — picking is
    // how an event carrying both a granted and a foreign channel gets in.
    //
    // **Both channels are granted on purpose.** With only A granted, the last
    // `h` resolves to B, the grant lookup refuses it, and this test passes
    // whether or not the exactly-one-`h` rule exists — it would be a test of
    // the grant lookup wearing this name. Granting both leaves ambiguity as
    // the only thing that can reject the event.
    let (_dir, conn) = temp_db();
    let id = identity();
    super::super::grants::grant_read_scope(&conn, &id, EXTID, 9, CHAN_A).expect("grant");
    super::super::grants::grant_read_scope(&conn, &id, EXTID, 9, CHAN_B).expect("grant");
    let filters = construct_filters(
        &[(9u32, CHAN_A.to_string()), (9u32, CHAN_B.to_string())],
        &ok_request(serde_json::json!({})),
    )
    .expect("build");
    let keys = nostr::Keys::generate();
    let event = signed_event(&keys, 9, vec![h(CHAN_A), h(CHAN_B)]);
    assert!(!verify_event(&event, &filters, &conn, &id, EXTID));
}

#[test]
fn a_valueless_h_still_counts_as_an_occurrence() {
    // THE VALUELESS-`["h"]` PROBE. Extracting values before counting would drop
    // the bare `["h"]` and let `[["h"], ["h", A]]` read as a single, granted
    // `h`. Counting by tag *name* is what makes it two.
    let (_dir, conn, filters, id) = granted_a();
    let keys = nostr::Keys::generate();
    let event = signed_event(&keys, 9, vec![vec!["h".to_string()], h(CHAN_A)]);
    assert!(!verify_event(&event, &filters, &conn, &id, EXTID));
}

#[test]
fn an_event_with_no_h_is_refused() {
    let (_dir, conn, filters, id) = granted_a();
    let keys = nostr::Keys::generate();
    let event = signed_event(&keys, 9, vec![]);
    assert!(!verify_event(&event, &filters, &conn, &id, EXTID));
}

#[test]
fn an_h_that_is_not_a_channel_uuid_is_refused() {
    // The store is the trust boundary at query time, not the manifest: this
    // grants the non-canonical channel itself, so the grant lookup *passes* and
    // the emitted filter carries that literal value. Only the canonical-UUID
    // check can reject the event, which is the point of asserting it here.
    //
    // Without granting it, the grant lookup refuses first and this test passes
    // with the UUID check deleted.
    let (_dir, conn) = temp_db();
    let id = identity();
    super::super::grants::grant_read_scope(&conn, &id, EXTID, 9, "nonsense").expect("grant");
    let filters = construct_filters(
        &[(9u32, "nonsense".to_string())],
        &ok_request(serde_json::json!({})),
    )
    .expect("build");
    let keys = nostr::Keys::generate();
    let event = signed_event(
        &keys,
        9,
        vec![vec!["h".to_string(), "nonsense".to_string()]],
    );
    assert!(!verify_event(&event, &filters, &conn, &id, EXTID));
}

#[test]
fn a_revocation_between_construction_and_verification_drops_the_event() {
    // `has_read_scope` is read live per event, so a grant removed after the
    // filters were built still stops the event being exposed.
    let (_dir, conn, filters, id) = granted_a();
    let keys = nostr::Keys::generate();
    let event = signed_event(&keys, 9, vec![h(CHAN_A)]);
    assert!(verify_event(&event, &filters, &conn, &id, EXTID));

    super::super::grants::revoke_all(&conn, &id, EXTID).expect("revoke");
    assert!(!verify_event(&event, &filters, &conn, &id, EXTID));
}

#[test]
fn a_misdelivered_event_matching_no_constructed_filter_is_dropped() {
    // The relay is untrusted. Grant 9 in **both** A and B, ask only for A, and
    // have the relay answer with a 9 carrying `h = B`.
    //
    // The pair `(9, B)` *is* granted, so the live grant lookup accepts it and
    // cannot be what rejects this event — only "matches a complete constructed
    // filter" can, because the emitted filter carries `#h: [A]`. An earlier
    // version used an ungranted pair and passed with filter-matching deleted,
    // which made it a second test of the grant lookup.
    let (_dir, conn) = temp_db();
    let id = identity();
    super::super::grants::grant_read_scope(&conn, &id, EXTID, 9, CHAN_A).expect("grant");
    super::super::grants::grant_read_scope(&conn, &id, EXTID, 9, CHAN_B).expect("grant");
    let filters = construct_filters(
        &[(9u32, CHAN_A.to_string()), (9u32, CHAN_B.to_string())],
        &ok_request(serde_json::json!({ "#h": [CHAN_A] })),
    )
    .expect("build");
    assert_eq!(
        filters.as_filters().len(),
        1,
        "only channel A was requested"
    );

    let keys = nostr::Keys::generate();
    let misdelivered = signed_event(&keys, 9, vec![h(CHAN_B)]);
    assert!(super::super::grants::has_read_scope(
        &conn, &id, EXTID, 9, CHAN_B
    ));
    assert!(!verify_event(&misdelivered, &filters, &conn, &id, EXTID));
}

#[test]
fn an_event_outside_the_requested_window_is_dropped() {
    // A complete filter match is required, not just the pair: the relay may
    // ignore `since`/`until` and the host must not pass on what it did not ask
    // for.
    let (_dir, conn) = temp_db();
    let id = identity();
    super::super::grants::grant_read_scope(&conn, &id, EXTID, 9, CHAN_A).expect("grant");
    let filters = construct_filters(
        &[(9u32, CHAN_A.to_string())],
        &ok_request(serde_json::json!({ "since": 4_000_000_000u64 })),
    )
    .expect("build");
    let keys = nostr::Keys::generate();
    let event = signed_event(&keys, 9, vec![h(CHAN_A)]);
    assert!(!verify_event(&event, &filters, &conn, &id, EXTID));
}

// ── grant store ────────────────────────────────────────────────────────────

#[test]
fn a_boolean_row_never_becomes_a_read_pair() {
    // A boolean grant stores `kind = -1, channel = ''`. Neither is a pair, and
    // a sentinel that reads as "any" is exactly the shape §7 forbids.
    //
    // **The sentinel row is written under the `read` scope itself.** A boolean
    // row under some *other* scope is excluded by the `scope = 'read'` clause,
    // so using one would test that clause instead — and the sentinel guard
    // could be deleted with every test still green. The realistic hazard is a
    // grants UX that writes a boolean `read` row; that must not read as an
    // unscoped read grant.
    let (_dir, conn) = temp_db();
    let id = identity();
    super::super::grants::grant_boolean_scope(&conn, &id, EXTID, super::super::grants::SCOPE_READ)
        .expect("grant");
    assert!(super::super::grants::list_read_pairs(&conn, &id, EXTID).is_empty());
    assert!(!super::super::grants::has_read_scope(
        &conn, &id, EXTID, 9, ""
    ));
}

#[test]
fn a_sign_grant_is_not_a_read_grant() {
    // The two scopes share a table; they must not share an answer.
    let (_dir, conn) = temp_db();
    let id = identity();
    super::super::grants::grant_sign_scope(&conn, &id, EXTID, 9, CHAN_A).expect("grant");
    assert!(!super::super::grants::has_read_scope(
        &conn, &id, EXTID, 9, CHAN_A
    ));
    assert!(super::super::grants::list_read_pairs(&conn, &id, EXTID).is_empty());
}

#[test]
fn read_pairs_are_listed_for_the_granting_identity_only() {
    let (_dir, conn) = temp_db();
    let mine = identity();
    let theirs = "b".repeat(64);
    super::super::grants::grant_read_scope(&conn, &mine, EXTID, 9, CHAN_A).expect("grant");
    assert_eq!(
        super::super::grants::list_read_pairs(&conn, &mine, EXTID),
        vec![(9u32, CHAN_A.to_string())]
    );
    assert!(super::super::grants::list_read_pairs(&conn, &theirs, EXTID).is_empty());
}

// ── manifest validation ────────────────────────────────────────────────────

#[test]
fn a_read_scope_naming_a_non_allowlisted_kind_is_rejected_at_validation() {
    // The allowlist is enforced at the manifest too, not only at query time —
    // a manifest must not request a capability the host could only implement
    // by guessing where an event's channel came from.
    use super::super::manifest::{is_channel_readable_kind, is_read_denied_kind};
    assert!(
        !is_channel_readable_kind(1),
        "kind 1 is global, not channel-readable"
    );
    assert!(
        !is_read_denied_kind(1),
        "and it is deliberately not on the floor"
    );
    assert!(is_channel_readable_kind(9));
}
