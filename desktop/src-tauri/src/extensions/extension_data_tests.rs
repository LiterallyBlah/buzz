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
