//! The shared coordinate validator (§4).

use super::*;

fn code_of(reply: &BridgeReply) -> Option<&str> {
    reply.error.as_ref().map(|e| e.code.as_str())
}

#[test]
fn a_valid_pair_builds_the_host_owned_coordinate() {
    assert_eq!(
        build_coordinate("equation-explorer", "graph.v1").unwrap(),
        "ext:equation-explorer:graph.v1"
    );
}

// ── the two grammars are not one grammar ─────────────────────────────────────

#[test]
fn a_dot_is_legal_in_a_key_but_not_in_an_extension_id() {
    // The single character by which §4's key grammar and §7's extension-id
    // grammar differ. One shared regex would either reject the first line or
    // accept the second, and the second is the namespace wall.
    assert!(
        build_coordinate("demo", "graph.v1").is_ok(),
        "a dotted key is valid"
    );
    let reply = build_coordinate("demo.evil", "graph").expect_err("a dotted extid is invalid");
    assert_eq!(code_of(&reply), Some(code::INVALID_PARAMS));
}

#[test]
fn the_grammars_are_anchored_over_the_whole_string() {
    // A valid prefix must not admit an invalid tail — the unanchored-search
    // failure the spec calls out explicitly.
    for bad in [
        "graph/../other",
        "graph key",
        "graph\n",
        "graph:extra",
        "gráph",
    ] {
        let reply =
            build_coordinate("demo", bad).expect_err(&format!("key {bad:?} must be refused"));
        assert_eq!(code_of(&reply), Some(code::INVALID_PARAMS), "key {bad:?}");
    }
}

#[test]
fn neither_field_may_begin_with_a_separator() {
    // `[a-z0-9_]` first: a leading `-` or `.` would let a key sort or read as
    // a flag, and an empty string has no first character at all.
    for bad in ["-graph", ".graph", ""] {
        assert!(build_coordinate("demo", bad).is_err(), "key {bad:?}");
    }
    for bad in ["-demo", ""] {
        assert!(build_coordinate(bad, "graph").is_err(), "extid {bad:?}");
    }
}

// ── byte bounds ──────────────────────────────────────────────────────────────

#[test]
fn a_key_at_the_cap_is_accepted_and_one_byte_over_is_not() {
    let at_cap = "k".repeat(KEY_MAX_BYTES);
    assert!(
        build_coordinate("demo", &at_cap).is_ok(),
        "256 bytes is legal"
    );

    let over = "k".repeat(KEY_MAX_BYTES + 1);
    let reply = build_coordinate("demo", &over).expect_err("257 bytes is not");
    assert_eq!(code_of(&reply), Some(code::INVALID_PARAMS));
}

#[test]
fn an_extension_id_at_the_cap_is_accepted_and_one_byte_over_is_not() {
    let at_cap = "e".repeat(EXTID_MAX_BYTES);
    assert!(
        build_coordinate(&at_cap, "graph").is_ok(),
        "64 bytes is legal"
    );

    let over = "e".repeat(EXTID_MAX_BYTES + 1);
    let reply = build_coordinate(&over, "graph").expect_err("65 bytes is not");
    assert_eq!(code_of(&reply), Some(code::INVALID_PARAMS));
}

#[test]
fn the_field_caps_compose_to_stay_under_the_relay_coordinate_bound() {
    // The coordinate check inside `build_coordinate` is **unreachable through
    // this function today**: the widest legal pair is far under the bound, so
    // no input can trip it and no mutant deleting it would turn a test red.
    //
    // Rather than pretend a probe exercises that branch, this asserts the
    // relationship that makes it unreachable. It fails the day someone raises a
    // field cap past the relay's `D_TAG_MAX_LEN` — which is the only way the
    // branch becomes reachable, and exactly when the guard starts to matter.
    let widest = COORDINATE_PREFIX.len() + EXTID_MAX_BYTES + 1 + KEY_MAX_BYTES;
    assert!(
        widest <= COORDINATE_MAX_BYTES,
        "widest legal coordinate ({widest}B) must stay within the relay bound ({COORDINATE_MAX_BYTES}B)"
    );

    // And the widest legal pair really does build, so the arithmetic above is
    // about coordinates that exist rather than a bound nothing reaches.
    let built = build_coordinate(&"e".repeat(EXTID_MAX_BYTES), &"k".repeat(KEY_MAX_BYTES))
        .expect("the widest legal pair must build");
    assert_eq!(built.len(), widest);
}

// ── the revalidator's branches ───────────────────────────────────────────────
//
// One isolated test per branch, each asserting the fixture passes *before*
// breaking one thing — three of these refusals are the same `denied`, so a test
// that only observed a refusal could not say which check produced it.

const EXTID: &str = "demo";
const KEY: &str = "graph.v1";
const LEASE: &str = "lease-for-extension-data-tests";

/// Inputs with every check satisfied.
fn passing() -> (
    tempfile::TempDir,
    std::path::PathBuf,
    crate::AppState,
    String,
    String,
) {
    let keys = nostr::Keys::generate();
    let identity = keys.public_key().to_hex();
    let state = crate::app_state::build_app_state();
    *state.keys.lock().unwrap() = keys;

    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("grants").join("extension-grants.db");
    {
        let conn = super::super::grants::open_grant_db(&db_path).expect("open");
        super::super::grants::grant_boolean_scope(&conn, &identity, EXTID, SCOPE_EXTENSION_DATA)
            .expect("grant");
    }
    let coordinate = build_coordinate(EXTID, KEY).expect("coordinate");
    (dir, db_path, state, identity, coordinate)
}

fn revalidation<'a>(
    identity: &'a str,
    coordinate: &'a str,
    state: &'a crate::AppState,
    db: Option<std::path::PathBuf>,
    created_at: i64,
) -> ExtensionDataRevalidation<'a> {
    ExtensionDataRevalidation {
        lease: LEASE,
        extension_id: EXTID,
        key: KEY,
        identity_at_entry: identity,
        coordinate_at_entry: coordinate,
        created_at,
        state,
        grant_db: db,
    }
}

#[tokio::test]
async fn a_released_lease_refuses_the_extension_data_revalidation() {
    let _host = super::super::frame_host::lifecycle_guard().await;
    let (_dir, db, state, identity, coordinate) = passing();
    let now = super::super::publish::now_unix();
    let r = revalidation(&identity, &coordinate, &state, Some(db), now);

    super::super::frame_host::insert_lease_for_test(LEASE, EXTID);
    r.check().expect("the fixture must otherwise pass");

    super::super::frame_host::release(LEASE);
    assert_eq!(r.check(), Err(code::DENIED));
}

#[tokio::test]
async fn an_identity_that_changed_refuses_the_extension_data_revalidation() {
    let _host = super::super::frame_host::lifecycle_guard().await;
    let (_dir, db, state, identity, coordinate) = passing();
    super::super::frame_host::insert_lease_for_test(LEASE, EXTID);
    let now = super::super::publish::now_unix();

    revalidation(&identity, &coordinate, &state, Some(db.clone()), now)
        .check()
        .expect("the fixture must otherwise pass");

    // A different identity entered than the one now signing. The grant still
    // resolves for the *current* pubkey, so only the equality check can refuse.
    let entered_as = nostr::Keys::generate().public_key().to_hex();
    assert_eq!(
        revalidation(&entered_as, &coordinate, &state, Some(db), now).check(),
        Err(code::DENIED)
    );
}

#[tokio::test]
async fn a_revoked_grant_refuses_the_extension_data_revalidation() {
    let _host = super::super::frame_host::lifecycle_guard().await;
    let (_dir, db, state, identity, coordinate) = passing();
    super::super::frame_host::insert_lease_for_test(LEASE, EXTID);
    let now = super::super::publish::now_unix();
    let r = revalidation(&identity, &coordinate, &state, Some(db.clone()), now);
    r.check().expect("the fixture must otherwise pass");

    {
        let conn = super::super::grants::open_grant_db(&db).expect("reopen");
        let removed = super::super::grants::revoke_all(&conn, &identity, EXTID).expect("revoke");
        assert_eq!(removed, 1, "the grant must have existed to be revoked");
    }
    assert_eq!(r.check(), Err(code::DENIED));
}

#[tokio::test]
async fn a_coordinate_that_no_longer_derives_refuses_the_revalidation() {
    let _host = super::super::frame_host::lifecycle_guard().await;
    let (_dir, db, state, identity, coordinate) = passing();
    super::super::frame_host::insert_lease_for_test(LEASE, EXTID);
    let now = super::super::publish::now_unix();

    revalidation(&identity, &coordinate, &state, Some(db.clone()), now)
        .check()
        .expect("the fixture must otherwise pass");

    // The namespace wall: a coordinate carried since entry that the host would
    // no longer derive for this (extension, key) must not be signed — this is
    // what stops one extension's write landing in another's namespace.
    let foreign = build_coordinate("other-extension", KEY).expect("coordinate");
    assert_eq!(
        revalidation(&identity, &foreign, &state, Some(db), now).check(),
        Err(code::DENIED)
    );
}

#[tokio::test]
async fn a_timestamp_that_left_the_window_refuses_the_revalidation() {
    let _host = super::super::frame_host::lifecycle_guard().await;
    let (_dir, db, state, identity, coordinate) = passing();
    super::super::frame_host::insert_lease_for_test(LEASE, EXTID);
    let now = super::super::publish::now_unix();

    revalidation(&identity, &coordinate, &state, Some(db.clone()), now)
        .check()
        .expect("the fixture must otherwise pass");

    assert_eq!(
        revalidation(&identity, &coordinate, &state, Some(db), now - 3_600).check(),
        Err(code::INVALID_PARAMS)
    );
}

// ── the read revalidator's own branches ──────────────────────────────────────
//
// The write revalidator is defined as this one *plus* the acceptance window, so
// every check below is also reachable through `ExtensionDataRevalidation`. These
// exist anyway: the tests above are named for the write path, and a check that
// moved out of the shared revalidator into the write-only wrapper would leave
// every one of them green while the read path silently lost it.

fn read_revalidation<'a>(
    identity: &'a str,
    coordinate: &'a str,
    state: &'a crate::AppState,
    db: Option<std::path::PathBuf>,
) -> ReadRevalidation<'a> {
    ReadRevalidation {
        lease: LEASE,
        extension_id: EXTID,
        key: KEY,
        identity_at_entry: identity,
        coordinate_at_entry: coordinate,
        state,
        grant_db: db,
    }
}

#[tokio::test]
async fn a_released_lease_refuses_the_read_revalidation() {
    let _host = super::super::frame_host::lifecycle_guard().await;
    let (_dir, db, state, identity, coordinate) = passing();
    let r = read_revalidation(&identity, &coordinate, &state, Some(db));

    super::super::frame_host::insert_lease_for_test(LEASE, EXTID);
    r.check().expect("the fixture must otherwise pass");

    super::super::frame_host::release(LEASE);
    assert_eq!(r.check(), Err(code::DENIED));
}

#[tokio::test]
async fn an_identity_that_changed_refuses_the_read_revalidation() {
    let _host = super::super::frame_host::lifecycle_guard().await;
    let (_dir, db, state, identity, coordinate) = passing();
    super::super::frame_host::insert_lease_for_test(LEASE, EXTID);

    read_revalidation(&identity, &coordinate, &state, Some(db.clone()))
        .check()
        .expect("the fixture must otherwise pass");

    let entered_as = nostr::Keys::generate().public_key().to_hex();
    assert_eq!(
        read_revalidation(&entered_as, &coordinate, &state, Some(db)).check(),
        Err(code::DENIED)
    );
}

#[tokio::test]
async fn a_revoked_grant_refuses_the_read_revalidation() {
    let _host = super::super::frame_host::lifecycle_guard().await;
    let (_dir, db, state, identity, coordinate) = passing();
    super::super::frame_host::insert_lease_for_test(LEASE, EXTID);
    let r = read_revalidation(&identity, &coordinate, &state, Some(db.clone()));
    r.check().expect("the fixture must otherwise pass");

    {
        let conn = super::super::grants::open_grant_db(&db).expect("reopen");
        let removed = super::super::grants::revoke_all(&conn, &identity, EXTID).expect("revoke");
        assert_eq!(removed, 1, "the grant must have existed to be revoked");
    }
    assert_eq!(r.check(), Err(code::DENIED));
}

#[tokio::test]
async fn a_coordinate_that_no_longer_derives_refuses_the_read_revalidation() {
    let _host = super::super::frame_host::lifecycle_guard().await;
    let (_dir, db, state, identity, coordinate) = passing();
    super::super::frame_host::insert_lease_for_test(LEASE, EXTID);

    read_revalidation(&identity, &coordinate, &state, Some(db.clone()))
        .check()
        .expect("the fixture must otherwise pass");

    let foreign = build_coordinate("other-extension", KEY).expect("coordinate");
    assert_eq!(
        read_revalidation(&identity, &foreign, &state, Some(db)).check(),
        Err(code::DENIED)
    );
}

#[tokio::test]
async fn the_read_revalidation_has_no_acceptance_window() {
    let _host = super::super::frame_host::lifecycle_guard().await;
    let (_dir, db, state, identity, coordinate) = passing();
    super::super::frame_host::insert_lease_for_test(LEASE, EXTID);
    let stale = super::super::publish::now_unix() - 3_600;

    // The same authority state, told from both sides. A template this old may
    // not be *signed* …
    assert_eq!(
        revalidation(&identity, &coordinate, &state, Some(db.clone()), stale).check(),
        Err(code::INVALID_PARAMS)
    );
    // … but it must not stop a *read*. The event is already stored by the time
    // the confirmation runs; refusing here would report a parameter error for
    // something that has already succeeded. The paired assertion is the point:
    // a window check added to the read revalidator reds this and nothing else.
    read_revalidation(&identity, &coordinate, &state, Some(db))
        .check()
        .expect("a read must not be gated on the write acceptance window");
}

// ── the write path's own timestamp contract ──────────────────────────────────

/// Drive the real `publish_extension_data` with a listener wired into state, so
/// "no POST" is observed rather than assumed.
async fn refusal_through_the_production_path(params: serde_json::Value) -> (BridgeReply, usize) {
    use tauri::Manager as _;
    let _gate = crate::relay_admission::gate_guard().await;

    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let connections = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = connections.clone();
    std::thread::spawn(move || {
        for _ in listener.incoming() {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    });

    let state = crate::app_state::build_app_state();
    *state.keys.lock().unwrap() = nostr::Keys::generate();
    *state.relay_url_override.lock().unwrap() = Some(format!("http://{addr}"));

    let app = tauri::test::mock_app();
    app.manage(state);
    let reply = publish_extension_data(app.handle(), EXTID, LEASE, Some(params)).await;
    (reply, connections.load(std::sync::atomic::Ordering::SeqCst))
}

#[tokio::test]
async fn a_missing_created_at_is_refused_before_any_extension_data_post() {
    // No default-to-now: a default would give every retry a different id and
    // publish twice on the first uncertain completion.
    let (reply, connections) =
        refusal_through_the_production_path(serde_json::json!({ "key": KEY, "content": "{}" }))
            .await;
    assert_eq!(
        reply.error.as_ref().map(|e| e.code.as_str()),
        Some(code::INVALID_PARAMS)
    );
    assert_eq!(connections, 0, "nothing may reach the socket");
}

#[tokio::test]
async fn an_out_of_window_created_at_is_rejected_not_clamped() {
    // Rejected, never adjusted. Clamping would silently move the event id the
    // caller will retry with — the double-publish this contract prevents.
    let stale = super::super::publish::now_unix() - 3_600;
    let (reply, connections) = refusal_through_the_production_path(
        serde_json::json!({ "key": KEY, "content": "{}", "created_at": stale }),
    )
    .await;
    assert_eq!(
        reply.error.as_ref().map(|e| e.code.as_str()),
        Some(code::INVALID_PARAMS)
    );
    assert_eq!(connections, 0, "nothing may reach the socket");
}

// ── the read path's four verifications ───────────────────────────────────────
//
// The host's willingness to believe a relay honoured its filter. Each test
// starts from an event that *does* match, then breaks exactly one property.

fn signed(keys: &nostr::Keys, kind: u16, tags: Vec<Vec<String>>) -> nostr::Event {
    let mut builder = nostr::EventBuilder::new(nostr::Kind::from(kind), "{}");
    for tag in tags {
        builder = builder.tag(nostr::Tag::parse(tag).expect("tag"));
    }
    builder.sign_with_keys(keys).expect("sign")
}

fn d_tag(coordinate: &str) -> Vec<String> {
    vec!["d".to_string(), coordinate.to_string()]
}

#[test]
fn a_matching_event_is_accepted_and_each_broken_property_is_not() {
    let keys = nostr::Keys::generate();
    let me = keys.public_key().to_hex();
    let coordinate = build_coordinate(EXTID, KEY).expect("coordinate");

    // Control: everything matches.
    let good = signed(&keys, 30800, vec![d_tag(&coordinate)]);
    assert!(
        event_matches_coordinate(&good, &me, &coordinate),
        "the control must match, or the refusals below prove nothing"
    );

    // Wrong kind — a different addressable kind at the same coordinate.
    let wrong_kind = signed(&keys, 30801, vec![d_tag(&coordinate)]);
    assert!(!event_matches_coordinate(&wrong_kind, &me, &coordinate));

    // Wrong author — another user's row for the same coordinate.
    let stranger = nostr::Keys::generate();
    let foreign = signed(&stranger, 30800, vec![d_tag(&coordinate)]);
    assert!(!event_matches_coordinate(&foreign, &me, &coordinate));

    // Wrong coordinate — another extension's namespace.
    let other = build_coordinate("other-extension", KEY).expect("coordinate");
    let elsewhere = signed(&keys, 30800, vec![d_tag(&other)]);
    assert!(!event_matches_coordinate(&elsewhere, &me, &coordinate));
}

#[test]
fn an_event_carrying_two_d_tags_is_refused_rather_than_resolved() {
    // Ambiguous addressable identity. Picking one would let a crafted event
    // carry both a granted coordinate and a foreign one and be accepted for
    // whichever the host happened to read first.
    let keys = nostr::Keys::generate();
    let me = keys.public_key().to_hex();
    let mine = build_coordinate(EXTID, KEY).expect("coordinate");
    let theirs = build_coordinate("other-extension", KEY).expect("coordinate");

    let two = signed(&keys, 30800, vec![d_tag(&mine), d_tag(&theirs)]);
    assert!(!event_matches_coordinate(&two, &me, &mine));
    assert!(!event_matches_coordinate(&two, &me, &theirs));

    let none = signed(&keys, 30800, vec![]);
    assert!(!event_matches_coordinate(&none, &me, &mine));
}

#[test]
fn an_event_whose_signature_does_not_cover_it_is_refused() {
    // Tampering after signing: the id and sig no longer match the content.
    let keys = nostr::Keys::generate();
    let me = keys.public_key().to_hex();
    let coordinate = build_coordinate(EXTID, KEY).expect("coordinate");
    let good = signed(&keys, 30800, vec![d_tag(&coordinate)]);
    assert!(event_matches_coordinate(&good, &me, &coordinate));

    use nostr::JsonUtil as _;
    let mut raw: serde_json::Value = serde_json::from_str(&good.as_json()).expect("event json");
    raw["content"] = serde_json::json!("{\"tampered\":true}");
    let tampered = nostr::Event::from_json(raw.to_string()).expect("still parses");
    assert!(
        !event_matches_coordinate(&tampered, &me, &coordinate),
        "a tampered event must not be exposed"
    );
}

// ── the two host walls, demonstrated positively ──────────────────────────────
//
// The negative rows above show a mismatched coordinate is refused. These show
// *why* one extension cannot reach another's namespace in the first place:
// the extension id comes from host state, and the read filter is built from
// the same derivation as the write.

#[tokio::test]
async fn one_extension_cannot_name_or_read_another_extensions_namespace() {
    let _host = super::super::frame_host::lifecycle_guard().await;

    // Two extensions, same key. The coordinates are disjoint because the host
    // supplies the extension id — no caller parameter contributes to it.
    let mine = build_coordinate("extension-a", KEY).expect("coordinate");
    let theirs = build_coordinate("extension-b", KEY).expect("coordinate");
    assert_ne!(mine, theirs);
    assert_eq!(mine, "ext:extension-a:graph.v1");
    assert_eq!(theirs, "ext:extension-b:graph.v1");

    // Write wall: the id is resolved from the lease, so holding A's lease can
    // only ever derive A's coordinate. A caller cannot present B's id.
    super::super::frame_host::insert_lease_for_test(LEASE, "extension-a");
    assert_eq!(
        super::super::frame_host::extension_for_lease(LEASE).as_deref(),
        Some("extension-a"),
        "the lease is the single producer of extension identity"
    );

    // Read wall: B's own signed event, stored at B's coordinate, is refused
    // when A asks — the same user authored it, so authorship alone would not
    // have separated them. Only the coordinate does.
    let keys = nostr::Keys::generate();
    let same_user = keys.public_key().to_hex();
    let b_event = signed(&keys, 30800, vec![d_tag(&theirs)]);
    assert!(
        event_matches_coordinate(&b_event, &same_user, &theirs),
        "B's event is valid at B's coordinate"
    );
    assert!(
        !event_matches_coordinate(&b_event, &same_user, &mine),
        "and must not satisfy a read for A's coordinate"
    );
}

// ── `current` — the truthful-reporting mechanism ─────────────────────────────
//
// `current` is the whole answer to the relay's ambiguous acknowledgement, and
// until these tests existed nothing defended it: the only test driving the real
// `publish_extension_data` used a listener that could count connections but not
// *serve* a write, so both its users were refusals. A mutant hardcoding
// `current: true`, inverting the id compare, or breaking the head read survived
// the entire suite.
//
// This fake serves both endpoints the write path uses — `POST /events` to
// accept the submission and `POST /query` for the head read-back — which is the
// minimum needed to reach the compare at all.

/// What the fake relay should return from the head query.
enum HeadReply {
    /// Echo the event that was just submitted — the fresh-write case.
    EchoSubmitted,
    /// Serve a different event — the superseded-before-read-back case.
    ServeOther(String),
    /// Fail the query — the read-back-unavailable case.
    Fail,
}

fn fake_relay(mode: HeadReply) -> String {
    fake_relay_with(mode, Disturb::default())
}

/// Where a fake relay should disturb host authority during a publish.
///
/// The two windows are different production branches, and only the relay can
/// tell them apart: `after_submit` fires once the write has committed but
/// before the confirmation query goes out, so the read-back's **pre-send**
/// recheck sees it; `before_head_reply` fires with the query already on the
/// wire, which only the **post-response** recheck can catch.
#[derive(Default)]
struct Disturb {
    after_submit: Option<Box<dyn Fn() + Send>>,
    before_head_reply: Option<Box<dyn Fn() + Send>>,
}

fn fake_relay_with(mode: HeadReply, disturb: Disturb) -> String {
    use std::io::{Read as _, Write as _};
    let Disturb {
        after_submit,
        before_head_reply,
    } = disturb;

    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");

    std::thread::spawn(move || {
        let mut submitted: Option<String> = None;
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let mut raw = Vec::new();
            let mut buf = [0u8; 8192];
            // Read headers plus the declared body.
            while let Ok(n) = stream.read(&mut buf) {
                if n == 0 {
                    break;
                }
                raw.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&raw).to_string();
                if let Some((head, body)) = text.split_once("\r\n\r\n") {
                    let want = head
                        .lines()
                        .find_map(|l| {
                            l.strip_prefix("Content-Length: ")
                                .or_else(|| l.strip_prefix("content-length: "))
                        })
                        .and_then(|v| v.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    if body.len() >= want {
                        break;
                    }
                }
            }
            let text = String::from_utf8_lossy(&raw).to_string();
            let (head, body) = text.split_once("\r\n\r\n").unwrap_or(("", ""));
            let is_query = head.starts_with("POST /query");

            let (status, payload) = if is_query {
                // The query is already on the wire; anything from here can only
                // be caught after the response.
                if let Some(hook) = &before_head_reply {
                    hook();
                }
                match &mode {
                    HeadReply::Fail => ("500 Internal Server Error", "{}".to_string()),
                    HeadReply::ServeOther(other) => ("200 OK", format!("[{other}]")),
                    HeadReply::EchoSubmitted => match &submitted {
                        Some(event) => ("200 OK", format!("[{event}]")),
                        None => ("200 OK", "[]".to_string()),
                    },
                }
            } else {
                // A submission: remember it so the head query can echo it back.
                submitted = Some(body.to_string());
                // The event is now committed as far as the host can tell. Any
                // disturbance from here lands strictly between the write and
                // its confirmation.
                if let Some(hook) = &after_submit {
                    hook();
                }
                let id = body
                    .split_once("\"id\":\"")
                    .and_then(|(_, rest)| rest.get(..64))
                    .unwrap_or_default()
                    .to_string();
                (
                    "200 OK",
                    format!(r#"{{"event_id":"{id}","accepted":true,"message":""}}"#),
                )
            };

            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                payload.len()
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });

    format!("http://{addr}")
}

/// Drive the real `publish_extension_data` against a relay that can serve a
/// write, with the grant and lease in place so it reaches the submission.
async fn successful_write(mode: HeadReply) -> BridgeReply {
    successful_write_with(|_, _| mode).await
}

/// As [`successful_write`], but the head is built **from the identity that is
/// actually installed**.
///
/// The distinction is the whole point of the superseded case: a head signed by
/// a different keypair is rejected at the author check, so `current` comes back
/// false without the ID comparison ever running. The closure receives the real
/// signing keys and the host-derived coordinate so the served head can differ
/// from the submitted event in **exactly one** respect — its id.
async fn successful_write_with(
    make_mode: impl FnOnce(&nostr::Keys, &str) -> HeadReply,
) -> BridgeReply {
    use tauri::Manager as _;

    let keys = nostr::Keys::generate();
    let identity = keys.public_key().to_hex();
    let coordinate = build_coordinate(EXTID, KEY).expect("coordinate");
    let mode = make_mode(&keys, &coordinate);
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("grants").join("extension-grants.db");
    {
        let conn = super::super::grants::open_grant_db(&db_path).expect("open");
        super::super::grants::grant_boolean_scope(&conn, &identity, EXTID, SCOPE_EXTENSION_DATA)
            .expect("grant");
    }

    let state = crate::app_state::build_app_state();
    *state.keys.lock().unwrap() = keys;
    *state.relay_url_override.lock().unwrap() = Some(fake_relay(mode));

    let app = tauri::test::mock_app();
    app.manage(state);
    // The grant store the production path reads is derived from the app, so
    // point the test's grants at it by copying into place.
    if let Ok(prod) = super::super::dispatch::grant_db_path(app.handle()) {
        if let Some(parent) = prod.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::copy(&db_path, &prod);
    }

    publish_extension_data(
        app.handle(),
        EXTID,
        LEASE,
        Some(serde_json::json!({
            "key": KEY,
            "content": "{\"v\":1}",
            "created_at": super::super::publish::now_unix(),
        })),
    )
    .await
}

/// Publish a value against a relay that revokes the grant in a chosen window,
/// and return the reply. `window` picks which of the two production rechecks is
/// the only one that can still catch it.
async fn write_with_grant_revoked(window: fn(Box<dyn Fn() + Send>) -> Disturb) -> BridgeReply {
    use tauri::Manager as _;

    let keys = nostr::Keys::generate();
    let identity = keys.public_key().to_hex();
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("grants").join("extension-grants.db");
    {
        let conn = super::super::grants::open_grant_db(&db_path).expect("open");
        super::super::grants::grant_boolean_scope(&conn, &identity, EXTID, SCOPE_EXTENSION_DATA)
            .expect("grant");
    }

    let state = crate::app_state::build_app_state();
    *state.keys.lock().unwrap() = keys;
    let app = tauri::test::mock_app();
    let prod_db = super::super::dispatch::grant_db_path(app.handle()).unwrap_or(db_path.clone());
    if let Some(parent) = prod_db.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::copy(&db_path, &prod_db);

    let revoked_identity = identity.clone();
    let revoked_db = prod_db.clone();
    let revoke: Box<dyn Fn() + Send> = Box::new(move || {
        let conn = super::super::grants::open_grant_db(&revoked_db).expect("reopen");
        let removed =
            super::super::grants::revoke_all(&conn, &revoked_identity, EXTID).expect("revoke");
        assert_eq!(removed, 1, "the grant must have existed to be revoked");
    });
    let url = fake_relay_with(HeadReply::EchoSubmitted, window(revoke));
    *state.relay_url_override.lock().unwrap() = Some(url);
    app.manage(state);

    let reply = publish_extension_data(
        app.handle(),
        EXTID,
        LEASE,
        Some(serde_json::json!({
            "key": KEY,
            "content": "{\"v\":1}",
            "created_at": super::super::publish::now_unix(),
        })),
    )
    .await;
    reply
}

/// `denied`, not `invalid_params`: nothing about the parameters was wrong, and
/// the event may well be stored. `current` is not guessed either way — no
/// result is returned at all, so the caller retries the exact request rather
/// than acting on a fabricated answer.
fn assert_confirmation_refused(reply: &BridgeReply) {
    assert!(
        denied(reply),
        "authority lost around the confirmation must deny, got {:?}",
        reply.error
    );
    assert!(
        reply.result.is_none(),
        "a refused confirmation must not invent `current`"
    );
}

#[tokio::test]
async fn a_write_whose_authority_dies_before_the_read_back_denies_without_inventing_current() {
    let _gate = crate::relay_admission::gate_guard().await;
    let _host = super::super::frame_host::lifecycle_guard().await;
    super::super::frame_host::insert_lease_for_test(LEASE, EXTID);

    // Revoked once the submission is in: the write has committed and the
    // confirmation has not gone out, so the read-back's pre-send recheck owns
    // this one.
    let reply = write_with_grant_revoked(|revoke| Disturb {
        after_submit: Some(revoke),
        ..Default::default()
    })
    .await;
    assert_confirmation_refused(&reply);
}

#[tokio::test]
async fn a_write_whose_authority_dies_after_the_read_back_is_sent_denies_without_inventing_current()
{
    let _gate = crate::relay_admission::gate_guard().await;
    let _host = super::super::frame_host::lifecycle_guard().await;
    super::super::frame_host::insert_lease_for_test(LEASE, EXTID);

    // The same outcome one branch further along: the confirmation query is
    // already on the wire when the grant dies, so only the post-response
    // recheck can refuse. Its twin above passes with that recheck deleted —
    // the pre-send one answers first — which is why both windows are named.
    let reply = write_with_grant_revoked(|revoke| Disturb {
        before_head_reply: Some(revoke),
        ..Default::default()
    })
    .await;
    assert_confirmation_refused(&reply);
}

#[tokio::test]
async fn a_fresh_write_reports_current_true() {
    let _gate = crate::relay_admission::gate_guard().await;
    let _host = super::super::frame_host::lifecycle_guard().await;
    super::super::frame_host::insert_lease_for_test(LEASE, EXTID);

    let reply = successful_write(HeadReply::EchoSubmitted).await;
    assert!(reply.ok, "the write must succeed: {:?}", reply.error);
    let result = reply.result.expect("result");
    assert_eq!(
        result["current"],
        serde_json::json!(true),
        "the head read returned the submitted event, so it is current"
    );
    assert!(
        result["event"]["sig"].is_string(),
        "the signed event is returned"
    );
}

#[tokio::test]
async fn a_write_already_superseded_at_read_back_reports_current_false() {
    let _gate = crate::relay_admission::gate_guard().await;
    let _host = super::super::frame_host::lifecycle_guard().await;
    super::super::frame_host::insert_lease_for_test(LEASE, EXTID);

    // The head query answers with a different event at the same coordinate,
    // signed by the **same identity that is installed** — someone else's write
    // landed first, or ours was superseded between the POST and the read.
    //
    // Signing it with a fresh keypair, as this test first did, is not the case
    // it names: that head is rejected at the author check, so `current` comes
    // back false without the id comparison ever running, and
    // `Ok(head) => head.is_some()` survives. The whole point here is two
    // *valid, same-author* heads distinguished only by id.
    let observed = std::sync::Arc::new(std::sync::Mutex::new(None::<(String, String, String)>));
    let seen = observed.clone();

    let reply = successful_write_with(move |keys, coordinate| {
        use nostr::JsonUtil as _;
        let other = nostr::EventBuilder::new(nostr::Kind::from(30800u16), "{\"other\":true}")
            .tag(nostr::Tag::parse(d_tag(coordinate)).expect("d tag"))
            .sign_with_keys(keys)
            .expect("sign");
        *seen.lock().unwrap() = Some((
            keys.public_key().to_hex(),
            coordinate.to_string(),
            other.id.to_hex(),
        ));
        HeadReply::ServeOther(other.as_json())
    })
    .await;

    assert!(reply.ok, "the submission itself still succeeded");
    let result = reply.result.expect("result");
    assert_eq!(
        result["current"],
        serde_json::json!(false),
        "a valid same-author head that is not ours must report current: false"
    );

    // Pin the preconditions, so this can never silently decay into the
    // different-author case again.
    let (head_author, head_coordinate, head_id) =
        observed.lock().unwrap().clone().expect("head was built");
    assert_eq!(
        result["event"]["pubkey"].as_str().expect("pubkey"),
        head_author,
        "the served head must share the submitted event's author"
    );
    assert_eq!(
        result["event"]["tags"][0][1].as_str().expect("d value"),
        head_coordinate,
        "and its coordinate"
    );
    assert_ne!(
        result["event"]["id"].as_str().expect("id"),
        head_id,
        "and differ only by id — otherwise there is nothing for the compare to distinguish"
    );
}

#[tokio::test]
async fn a_failed_read_back_is_a_normalised_failure_not_a_guess() {
    let _gate = crate::relay_admission::gate_guard().await;
    let _host = super::super::frame_host::lifecycle_guard().await;
    super::super::frame_host::insert_lease_for_test(LEASE, EXTID);

    // The relay's acknowledgement cannot distinguish an exact retry from a
    // rejected stale write, so with the read-back unavailable there is nothing
    // honest to derive `current` from. Guessing either way is worse than
    // refusing: the caller can safely retry the exact request.
    let reply = successful_write(HeadReply::Fail).await;
    assert!(!reply.ok, "an unconfirmable write must not report success");
    assert_eq!(
        reply.error.as_ref().map(|e| e.code.as_str()),
        Some(code::RELAY_ERROR)
    );
}

#[test]
fn a_valueless_d_plus_the_coordinate_is_refused_as_two_d_tags() {
    // Increment 3's valueless-`["h"]` bypass, in a second place. Counting by
    // *value* drops the empty occurrence before the multiplicity check, so this
    // correctly-signed event reads as carrying exactly one `d` and is accepted
    // at a coordinate whose addressable identity is in fact ambiguous.
    let keys = nostr::Keys::generate();
    let me = keys.public_key().to_hex();
    let coordinate = build_coordinate(EXTID, KEY).expect("coordinate");

    let crafted = signed(
        &keys,
        30800,
        vec![vec!["d".to_string()], d_tag(&coordinate)],
    );
    assert!(
        !event_matches_coordinate(&crafted, &me, &coordinate),
        "every d-tag occurrence must count, including a valueless one"
    );

    // The reverse order too — a counter that looks at the first or the last
    // occurrence would pass one of these and fail the other.
    let reversed = signed(
        &keys,
        30800,
        vec![d_tag(&coordinate), vec!["d".to_string()]],
    );
    assert!(
        !event_matches_coordinate(&reversed, &me, &coordinate),
        "order must not decide it"
    );

    // And a lone valueless `["d"]` is an occurrence with no usable value.
    let lone = signed(&keys, 30800, vec![vec!["d".to_string()]]);
    assert!(!event_matches_coordinate(&lone, &me, &coordinate));
}

// ── query-authority hardening: authority transitions across the real gate ────
//
// The defect these defend: the generic query path re-derives its NIP-98 key
// from `state` *after* the unbounded admission wait, so a transition into
// recovery during that wait could sign with the ephemeral boot key. Entry
// gating does not cover it — the key used is fetched later than the gate.
//
// Every probe below arms the *real* process-global gate rather than injecting a
// refusal: the point is the production transition that creates a stale
// authority, not that a refusal suppresses a request.

/// A parked read: real armed gate, real grant store, real lease, counting
/// listener. The closure runs while the read is parked in the gate.
async fn read_parked_at_the_gate(
    disturb: impl FnOnce(&crate::AppState, &str, &std::path::Path) + Send + 'static,
) -> (BridgeReply, usize) {
    use tauri::Manager as _;

    let keys = nostr::Keys::generate();
    let identity = keys.public_key().to_hex();
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("grants").join("extension-grants.db");
    {
        let conn = super::super::grants::open_grant_db(&db_path).expect("open");
        super::super::grants::grant_boolean_scope(&conn, &identity, EXTID, SCOPE_EXTENSION_DATA)
            .expect("grant");
    }

    // A listener that accepts and counts, so "no request" is observed rather
    // than asserted. Nothing answers: a parked read must never reach it.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let connections = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = connections.clone();
    std::thread::spawn(move || {
        for _ in listener.incoming() {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    });

    let state = crate::app_state::build_app_state();
    *state.keys.lock().unwrap() = keys;
    *state.relay_url_override.lock().unwrap() = Some(format!("http://{addr}"));

    let app = tauri::test::mock_app();
    app.manage(state);
    if let Ok(prod) = super::super::dispatch::grant_db_path(app.handle()) {
        if let Some(parent) = prod.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::copy(&db_path, &prod);
    }

    super::super::frame_host::insert_lease_for_test(LEASE, EXTID);

    // Arm a real bounded wait, then disturb authority while the read sits in it.
    crate::relay_admission::activate_rate_limit(Some(1));
    let armed_at = std::time::Instant::now();

    let handle = app.handle().clone();
    let read = extension_data_get(
        &handle,
        EXTID,
        LEASE,
        Some(serde_json::json!({ "key": KEY })),
    );

    let state_ref = app.state::<crate::AppState>();
    let prod_db = super::super::dispatch::grant_db_path(app.handle()).unwrap_or(db_path.clone());
    let identity_for_disturb = identity.clone();
    let disturb_task = async {
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        disturb(&state_ref, &identity_for_disturb, &prod_db);
    };

    let (reply, ()) = tokio::join!(read, disturb_task);
    crate::relay_admission::reset_rate_limit_gate();

    // Without a real armed wait the disturbance would land after the read had
    // already finished, and the probe would prove nothing.
    assert!(
        armed_at.elapsed() >= std::time::Duration::from_millis(300),
        "the read must actually have parked in the gate"
    );
    (reply, connections.load(std::sync::atomic::Ordering::SeqCst))
}

fn denied(reply: &BridgeReply) -> bool {
    reply.error.as_ref().map(|e| e.code.as_str()) == Some(code::DENIED)
}

#[tokio::test]
async fn identity_lost_during_the_gate_wait_denies_with_no_request() {
    let _gate = crate::relay_admission::gate_guard().await;
    let _host = super::super::frame_host::lifecycle_guard().await;
    let (reply, connections) = read_parked_at_the_gate(|state, _, _| {
        state
            .identity_lost
            .store(true, std::sync::atomic::Ordering::Release);
    })
    .await;
    assert!(denied(&reply), "recovery must refuse: {:?}", reply.error);
    assert_eq!(connections, 0, "no request may reach the relay");
}

#[tokio::test]
async fn keyring_locked_during_the_gate_wait_denies_with_no_request() {
    let _gate = crate::relay_admission::gate_guard().await;
    let _host = super::super::frame_host::lifecycle_guard().await;
    let (reply, connections) = read_parked_at_the_gate(|state, _, _| {
        state
            .keyring_locked
            .store(true, std::sync::atomic::Ordering::Release);
    })
    .await;
    assert!(denied(&reply));
    assert_eq!(connections, 0);
}

#[tokio::test]
async fn an_identity_change_during_the_gate_wait_denies_with_no_request() {
    let _gate = crate::relay_admission::gate_guard().await;
    let _host = super::super::frame_host::lifecycle_guard().await;
    // A *different valid* identity — not recovery. The request was admitted for
    // one user; it must not go out authenticated as another.
    //
    // The incoming identity is granted the same scope on the way in. Without
    // that, the grant lookup refuses first — it is keyed by pubkey — and this
    // probe passes with the identity-equality check deleted, proving only that
    // *something* refused.
    let (reply, connections) = read_parked_at_the_gate(|state, _, db| {
        let other = nostr::Keys::generate();
        let conn = super::super::grants::open_grant_db(db).expect("reopen");
        super::super::grants::grant_boolean_scope(
            &conn,
            &other.public_key().to_hex(),
            EXTID,
            SCOPE_EXTENSION_DATA,
        )
        .expect("grant the incoming identity");
        *state.keys.lock().unwrap() = other;
    })
    .await;
    assert!(denied(&reply));
    assert_eq!(connections, 0);
}

#[tokio::test]
async fn a_grant_revoked_during_the_gate_wait_denies_with_no_request() {
    let _gate = crate::relay_admission::gate_guard().await;
    let _host = super::super::frame_host::lifecycle_guard().await;
    let (reply, connections) = read_parked_at_the_gate(|_, identity, db| {
        let conn = super::super::grants::open_grant_db(db).expect("reopen");
        let removed = super::super::grants::revoke_all(&conn, identity, EXTID).expect("revoke");
        assert_eq!(removed, 1, "the grant must have existed to be revoked");
    })
    .await;
    assert!(denied(&reply));
    assert_eq!(connections, 0);
}

#[tokio::test]
async fn a_lease_released_during_the_gate_wait_denies_with_no_request() {
    let _gate = crate::relay_admission::gate_guard().await;
    let _host = super::super::frame_host::lifecycle_guard().await;
    let (reply, connections) = read_parked_at_the_gate(|_, _, _| {
        super::super::frame_host::release(LEASE);
    })
    .await;
    assert!(denied(&reply));
    assert_eq!(connections, 0);
}

#[tokio::test]
async fn the_connection_counter_sees_a_request_that_is_actually_made() {
    let _gate = crate::relay_admission::gate_guard().await;
    let _host = super::super::frame_host::lifecycle_guard().await;

    // The instrument's own control. Five probes above read `connections == 0`,
    // and that number means nothing until the same listener, on the same port,
    // is shown reporting 1 when a request *is* made — otherwise a counter that
    // never increments passes all five for no reason at all.
    let (reply, connections) = read_parked_at_the_gate(|_, _, _| {}).await;

    assert_eq!(
        connections, 1,
        "an undisturbed read must reach the relay, or the zero-counts prove nothing"
    );
    // The listener accepts and drops without answering, so the read fails at
    // the relay rather than at authority — which is also the assertion that it
    // got past revalidation instead of being refused for some other reason.
    assert_eq!(
        reply.error.as_ref().map(|e| e.code.as_str()),
        Some(code::RELAY_ERROR),
        "an undisturbed read must not be denied: {:?}",
        reply.error
    );
}

#[tokio::test]
async fn the_waiting_wrapper_waits_and_the_extracted_seam_does_not() {
    let _gate = crate::relay_admission::gate_guard().await;

    // The one behavioural risk the extraction creates. `query_relay_at_with_keys`
    // is now a two-line wrapper, so the wait it owes its three existing callers
    // can be deleted without touching anything those callers can see. Stated as
    // a contrast, because "it was slow" is not evidence on its own.
    let keys = nostr::Keys::generate();
    let state = crate::app_state::build_app_state();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    std::thread::spawn(move || for _ in listener.incoming() {});
    let url = format!("http://{addr}");
    let filter = serde_json::json!({ "kinds": [30800], "limit": 1 });

    crate::relay_admission::activate_rate_limit(Some(1));
    let seam_at = std::time::Instant::now();
    let _ = crate::relay::query_relay_at_with_keys_no_wait(
        &state,
        &url,
        std::slice::from_ref(&filter),
        &keys,
        None,
    )
    .await;
    let seam = seam_at.elapsed();

    crate::relay_admission::activate_rate_limit(Some(1));
    let wrapper_at = std::time::Instant::now();
    let _ = crate::relay::query_relay_at_with_keys(
        &state,
        &url,
        std::slice::from_ref(&filter),
        &keys,
        None,
    )
    .await;
    let wrapper = wrapper_at.elapsed();
    crate::relay_admission::reset_rate_limit_gate();

    assert!(
        seam < std::time::Duration::from_millis(500),
        "the extracted seam must not wait — the caller owns the gate: {seam:?}"
    );
    assert!(
        wrapper >= std::time::Duration::from_millis(900),
        "the wrapper must still park its callers on the armed gate: {wrapper:?}"
    );
}

/// A fake relay that records the `Authorization` header it was given, and can
/// disturb authority *after* receiving the request but before answering.
fn capturing_relay(
    seen_auth: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    on_request: Option<Box<dyn Fn() + Send>>,
) -> String {
    use std::io::{Read as _, Write as _};

    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");

    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let mut raw = Vec::new();
            let mut buf = [0u8; 8192];
            while let Ok(n) = stream.read(&mut buf) {
                if n == 0 {
                    break;
                }
                raw.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&raw).to_string();
                if let Some((head, body)) = text.split_once("\r\n\r\n") {
                    let want = head
                        .lines()
                        .find_map(|l| {
                            l.strip_prefix("Content-Length: ")
                                .or_else(|| l.strip_prefix("content-length: "))
                        })
                        .and_then(|v| v.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    if body.len() >= want {
                        break;
                    }
                }
            }
            let text = String::from_utf8_lossy(&raw).to_string();
            if let Some(line) = text
                .lines()
                .find(|l| l.to_ascii_lowercase().starts_with("authorization:"))
            {
                *seen_auth.lock().unwrap() =
                    Some(line.splitn(2, ':').nth(1).unwrap_or("").trim().to_string());
            }
            // The request has been received; authority may now be withdrawn
            // before the host sees the answer.
            if let Some(hook) = &on_request {
                hook();
            }
            let payload = "[]";
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                payload.len()
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });
    format!("http://{addr}")
}

#[tokio::test]
async fn proof_6_1_the_full_route_emits_nip98_authored_by_the_admitted_identity() {
    // The route positive. It deliberately does **not** try to prove
    // "A rather than a later B": once revalidation asserts the identity is
    // unchanged, `signing_keys()` and a raw `state.keys` read return the same
    // key whenever the route succeeds, so no honest full-route fixture can
    // separate them. That property is pinned one seam down, in 6.2.
    let _gate = crate::relay_admission::gate_guard().await;
    let _host = super::super::frame_host::lifecycle_guard().await;
    use base64::Engine as _;
    use tauri::Manager as _;

    let keys = nostr::Keys::generate();
    let admitted = keys.public_key().to_hex();
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("grants").join("extension-grants.db");
    {
        let conn = super::super::grants::open_grant_db(&db_path).expect("open");
        super::super::grants::grant_boolean_scope(&conn, &admitted, EXTID, SCOPE_EXTENSION_DATA)
            .expect("grant");
    }

    let seen = std::sync::Arc::new(std::sync::Mutex::new(None));
    let url = capturing_relay(seen.clone(), None);

    let state = crate::app_state::build_app_state();
    *state.keys.lock().unwrap() = keys;
    *state.relay_url_override.lock().unwrap() = Some(url);

    let app = tauri::test::mock_app();
    app.manage(state);
    if let Ok(prod) = super::super::dispatch::grant_db_path(app.handle()) {
        if let Some(parent) = prod.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::copy(&db_path, &prod);
    }
    super::super::frame_host::insert_lease_for_test(LEASE, EXTID);

    let reply = extension_data_get(
        app.handle(),
        EXTID,
        LEASE,
        Some(serde_json::json!({ "key": KEY })),
    )
    .await;
    assert!(reply.ok, "the read must succeed: {:?}", reply.error);

    let header = seen.lock().unwrap().clone().expect("a request was made");
    let token = header.strip_prefix("Nostr ").expect("NIP-98 scheme");
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(token)
        .expect("base64");
    let auth: serde_json::Value = serde_json::from_slice(&decoded).expect("auth event");
    assert_eq!(auth["pubkey"].as_str().expect("pubkey"), admitted);
}

#[tokio::test]
async fn proof_6_2_the_query_signs_with_its_explicit_keys_not_with_state() {
    // The key-source property, pinned at the seam that owns it. `state.keys`
    // deliberately holds B while the caller passes A — a state the full route
    // cannot reach, which is exactly why this is tested here and not there.
    // Real wire bytes; no pretence of being a successful `extensionData.get`.
    let _gate = crate::relay_admission::gate_guard().await;
    use base64::Engine as _;

    let admitted_keys = nostr::Keys::generate();
    let a = admitted_keys.public_key().to_hex();
    let other_keys = nostr::Keys::generate();
    let b = other_keys.public_key().to_hex();
    assert_ne!(a, b);

    let seen = std::sync::Arc::new(std::sync::Mutex::new(None));
    let url = capturing_relay(seen.clone(), None);

    let state = crate::app_state::build_app_state();
    *state.keys.lock().unwrap() = other_keys; // B in state …

    let filter = serde_json::json!({ "kinds": [30800], "limit": 1 });
    let _ = crate::relay::query_relay_at_with_keys_no_wait(
        &state,
        &url,
        &[filter],
        &admitted_keys, // … A passed explicitly
        None,
    )
    .await;

    let header = seen.lock().unwrap().clone().expect("a request was made");
    let token = header.strip_prefix("Nostr ").expect("NIP-98 scheme");
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(token)
        .expect("base64");
    let auth: serde_json::Value = serde_json::from_slice(&decoded).expect("auth event");
    assert_eq!(
        auth["pubkey"].as_str().expect("pubkey"),
        a,
        "the wire must carry the explicitly passed identity, never state's"
    );
    assert_ne!(auth["pubkey"].as_str().expect("pubkey"), b);
}

/// Drive a read whose authority is withdrawn *after* the relay has the request.
///
/// The relay signals when it has received the request and waits; the test
/// disturbs authority with full access to the app, then releases the relay to
/// answer. So the post-response recheck is the only thing that can catch it.
async fn read_disturbed_after_send(
    disturb: impl FnOnce(&tauri::AppHandle<tauri::test::MockRuntime>, &str, &std::path::Path),
) -> BridgeReply {
    use std::io::{Read as _, Write as _};
    use tauri::Manager as _;

    let keys = nostr::Keys::generate();
    let identity = keys.public_key().to_hex();
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("grants").join("extension-grants.db");
    {
        let conn = super::super::grants::open_grant_db(&db_path).expect("open");
        super::super::grants::grant_boolean_scope(&conn, &identity, EXTID, SCOPE_EXTENSION_DATA)
            .expect("grant");
    }

    // Arrival is signalled on a *tokio* channel: the disturbance and the read
    // run on one task, so a blocking wait here would stop the read being polled
    // and the request it is waiting for could never be sent.
    let (arrived_tx, arrived_rx) = tokio::sync::oneshot::channel::<()>();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let mut buf = [0u8; 8192];
            let _ = stream.read(&mut buf);
            let _ = arrived_tx.send(());
            let _ = release_rx.recv();
            let payload = "[]";
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                payload.len()
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });

    let state = crate::app_state::build_app_state();
    *state.keys.lock().unwrap() = keys;
    *state.relay_url_override.lock().unwrap() = Some(format!("http://{addr}"));

    let app = tauri::test::mock_app();
    app.manage(state);
    let prod_db = super::super::dispatch::grant_db_path(app.handle()).unwrap_or(db_path.clone());
    if let Some(parent) = prod_db.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::copy(&db_path, &prod_db);
    super::super::frame_host::insert_lease_for_test(LEASE, EXTID);

    let handle = app.handle().clone();
    let read = extension_data_get(
        &handle,
        EXTID,
        LEASE,
        Some(serde_json::json!({ "key": KEY })),
    );

    let disturber = async {
        // Wait until the relay actually has the request in hand — bounded, so a
        // request that never goes out fails this probe rather than hanging it.
        let arrived = tokio::time::timeout(std::time::Duration::from_secs(10), arrived_rx)
            .await
            .is_ok_and(|r| r.is_ok());
        if arrived {
            disturb(app.handle(), &identity, &prod_db);
        }
        let _ = release_tx.send(());
        arrived
    };

    let (reply, arrived) = tokio::join!(read, disturber);
    // Without this the probe would silently degrade into "authority was already
    // gone before the send", which the *pre*-send recheck catches — a different
    // production line from the one these proofs name.
    assert!(
        arrived,
        "the request must have reached the relay before authority was disturbed"
    );
    reply
}

#[tokio::test(flavor = "multi_thread")]
async fn proof_7a_lease_released_after_send_denies_and_exposes_nothing() {
    let _gate = crate::relay_admission::gate_guard().await;
    let _host = super::super::frame_host::lifecycle_guard().await;
    let reply = read_disturbed_after_send(|_, _, _| {
        super::super::frame_host::release(LEASE);
    })
    .await;
    assert!(denied(&reply), "got {:?}", reply.error);
    assert!(reply.result.is_none(), "no event bytes may be exposed");
}

#[tokio::test(flavor = "multi_thread")]
async fn proof_7b_identity_changed_after_send_denies_and_exposes_nothing() {
    let _gate = crate::relay_admission::gate_guard().await;
    let _host = super::super::frame_host::lifecycle_guard().await;
    // As in the held-gate identity probe: the incoming identity is granted the
    // same scope, so the grant lookup cannot be what refuses. Only the
    // identity-equality branch is left to catch this.
    let reply = read_disturbed_after_send(|app, _, db| {
        use tauri::Manager as _;
        let other = nostr::Keys::generate();
        let conn = super::super::grants::open_grant_db(db).expect("reopen");
        super::super::grants::grant_boolean_scope(
            &conn,
            &other.public_key().to_hex(),
            EXTID,
            SCOPE_EXTENSION_DATA,
        )
        .expect("grant the incoming identity");
        let state = app.state::<crate::AppState>();
        *state.keys.lock().unwrap() = other;
    })
    .await;
    assert!(denied(&reply), "got {:?}", reply.error);
    assert!(reply.result.is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn proof_7c_grant_revoked_after_send_denies_and_exposes_nothing() {
    let _gate = crate::relay_admission::gate_guard().await;
    let _host = super::super::frame_host::lifecycle_guard().await;
    let reply = read_disturbed_after_send(|_, identity, db| {
        let conn = super::super::grants::open_grant_db(db).expect("reopen");
        let removed = super::super::grants::revoke_all(&conn, identity, EXTID).expect("revoke");
        assert_eq!(removed, 1, "the grant must have existed to be revoked");
    })
    .await;
    assert!(denied(&reply), "got {:?}", reply.error);
    assert!(reply.result.is_none());
}

// ── against a real relay ─────────────────────────────────────────────────────

/// Both extension-data methods, end to end against a relay built from this
/// checkout with real PostgreSQL and Redis behind it.
///
/// Ignored by default; pointed at a relay with `BUZZ_B1_REAL_RELAY`. It adds
/// the one thing every fake listener above cannot: the NIP-98 header this path
/// emits is *accepted* by a real auth pipeline, on the submission and on the
/// confirmation query. The fakes admit anything, so they can prove which key
/// signed but never that the signature is one a relay would take.
///
/// The read is the point. `head_for_coordinate` signs with keys captured at
/// entry and carried across the admission wait, through the extracted
/// [`crate::relay::query_relay_at_with_keys_no_wait`] — so a live
/// `current: true` is that whole sequence working against something that
/// answers 401 when it does not.
#[tokio::test]
#[ignore = "needs a live relay: BUZZ_B1_REAL_RELAY=http://127.0.0.1:PORT"]
async fn against_a_live_relay_both_extension_data_methods_authenticate() {
    use tauri::Manager as _;
    let url = std::env::var("BUZZ_B1_REAL_RELAY").expect("BUZZ_B1_REAL_RELAY must be set");

    let _gate = crate::relay_admission::gate_guard().await;
    let _host = super::super::frame_host::lifecycle_guard().await;

    let keys = nostr::Keys::generate();
    let identity = keys.public_key().to_hex();
    // A fresh coordinate per run: the relay's replacement key is
    // (community, kind, pubkey, d), and a reused one would let an earlier
    // run's head answer this run's confirmation.
    let key = format!("live.{}", &identity[..16]);
    let coordinate = build_coordinate(EXTID, &key).expect("coordinate");
    // Printed so the transcript can match this run against the relay's own
    // request log rather than taking the test's word for which key signed.
    println!("LIVE identity={identity}");
    println!("LIVE coordinate={coordinate}");

    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("grants").join("extension-grants.db");
    {
        let conn = super::super::grants::open_grant_db(&db_path).expect("open");
        super::super::grants::grant_boolean_scope(&conn, &identity, EXTID, SCOPE_EXTENSION_DATA)
            .expect("grant");
    }

    let state = crate::app_state::build_app_state();
    *state.keys.lock().unwrap() = keys;
    *state.relay_url_override.lock().unwrap() = Some(url);

    let app = tauri::test::mock_app();
    app.manage(state);
    if let Ok(prod) = super::super::dispatch::grant_db_path(app.handle()) {
        if let Some(parent) = prod.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::copy(&db_path, &prod);
    }

    super::super::frame_host::insert_lease_for_test(LEASE, EXTID);

    let written = publish_extension_data(
        app.handle(),
        EXTID,
        LEASE,
        Some(serde_json::json!({
            "key": key,
            "content": "{\"v\":\"live\"}",
            "created_at": super::super::publish::now_unix(),
        })),
    )
    .await;
    assert!(
        written.ok,
        "the live write must be accepted: {:?}",
        written.error
    );
    let written = written.result.expect("result");
    // `current: true` means the confirmation query authenticated and the relay
    // answered with the event just submitted. A rejected NIP-98 would have
    // surfaced as a relay error instead.
    assert_eq!(
        written["current"],
        serde_json::json!(true),
        "the live read-back must confirm the submitted event"
    );
    let submitted_id = written["event"]["id"].as_str().expect("id").to_string();
    println!("LIVE submitted_id={submitted_id}");

    let read = extension_data_get(
        app.handle(),
        EXTID,
        LEASE,
        Some(serde_json::json!({ "key": key })),
    )
    .await;
    assert!(read.ok, "the live read must succeed: {:?}", read.error);
    let event = read.result.expect("result")["event"].clone();
    assert_eq!(
        event["id"].as_str(),
        Some(submitted_id.as_str()),
        "the live read must return the event this test wrote"
    );
    assert_eq!(
        event["pubkey"].as_str(),
        Some(identity.as_str()),
        "and it must be authored by the admitted identity"
    );
    // The host-derived coordinate survived the round trip, so the relay stored
    // the event under the namespace the host built rather than under anything
    // the caller could name.
    let d = event["tags"]
        .as_array()
        .expect("tags")
        .iter()
        .find(|t| t[0] == "d")
        .expect("a d tag");
    assert_eq!(d[1].as_str(), Some(coordinate.as_str()));
}
