//! Regression test for the project-protocol ingress refusal (Phase 6, 2026-08-02).
//!
//! `required_scope_for_kind` in `buzz-relay/src/handlers/ingest.rs` mapped every
//! kind it did not know to `restricted: unknown event kind`. The accepted agent
//! clients had meanwhile started publishing three kinds it did not know:
//! NIP-PA project activity (20003) and the NIP-PC call pair (43001/43004). Every
//! one came back HTTP 400, so project activity was invisible and no peer call
//! could begin — and the 400 read like a malformed request, so a client could
//! not tell "I sent something wrong" from "this relay does not implement the
//! protocol I am speaking".
//!
//! This drives the **real shared ingestion boundary**: signed NIP-98
//! `POST /events`, the same seam authenticated WebSocket writes reach through
//! `ingest_event`. It asserts the whole contract rather than "the 400 stopped":
//!
//! - the three kinds are accepted;
//! - activity is **not stored** — it is ephemeral, and a "working" indicator
//!   that outlives its turn by the lifetime of the database is not a fix;
//! - the call and result **are** stored, because a caller in a separate process
//!   has nowhere else to learn its own call is outstanding;
//! - neighbouring kinds in both ranges are still refused, so this is an
//!   admission of three kinds and not an opening of two ranges.
//!
//! Requires a running relay and its Postgres. Ignored by default:
//!   REPRO_RELAY_HTTP=http://localhost:3030 REPRO_HOST=localhost:3030 \
//!   DATABASE_URL=postgres://buzz:buzz_dev@localhost:5471/buzz \
//!   cargo test -p buzz-test-client --test regression_project_kind_ingress \
//!     -- --ignored --nocapture

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use nostr::{EventBuilder, Keys, Kind, Tag};
use sha2::{Digest, Sha256};
use uuid::Uuid;

const KIND_PROJECT_ACTIVITY: u16 = 20003;
const KIND_PEER_CALL: u16 = 43001;
const KIND_PEER_CALL_RESULT: u16 = 43004;

fn http_base() -> String {
    std::env::var("REPRO_RELAY_HTTP").unwrap_or_else(|_| "http://localhost:3030".into())
}
fn host() -> String {
    std::env::var("REPRO_HOST").unwrap_or_else(|_| "localhost:3030".into())
}
fn db_url() -> String {
    std::env::var("DATABASE_URL").expect("DATABASE_URL required")
}

fn sha256_hex(b: &[u8]) -> String {
    hex::encode(Sha256::digest(b))
}

fn nip98(keys: &Keys, url: &str, body: &str) -> String {
    let ev = EventBuilder::new(Kind::Custom(27_235), "")
        .tags(vec![
            Tag::parse(["u", url]).unwrap(),
            Tag::parse(["method", "POST"]).unwrap(),
            Tag::parse(["payload", &sha256_hex(body.as_bytes())]).unwrap(),
            Tag::parse(["nonce", &Uuid::new_v4().to_string()]).unwrap(),
        ])
        .sign_with_keys(keys)
        .unwrap();
    format!(
        "Nostr {}",
        BASE64.encode(serde_json::to_string(&ev).unwrap())
    )
}

async fn post_event(keys: &Keys, event: &nostr::Event) -> (u16, String) {
    let body = serde_json::to_string(event).unwrap();
    let signed_url = format!("http://{}/events", host());
    let r = reqwest::Client::new()
        .post(format!("{}/events", http_base()))
        .header("Host", host())
        .header("Content-Type", "application/json")
        .header("Authorization", nip98(keys, &signed_url, &body))
        .body(body)
        .send()
        .await
        .expect("POST /events");
    let status = r.status().as_u16();
    (status, r.text().await.unwrap_or_default())
}

fn signed(keys: &Keys, kind: u16, content: &str, tags: Vec<Tag>) -> nostr::Event {
    EventBuilder::new(Kind::Custom(kind), content)
        .tags(tags)
        .sign_with_keys(keys)
        .unwrap()
}

async fn pool() -> sqlx::Pool<sqlx::Postgres> {
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(&db_url())
        .await
        .expect("connect Postgres")
}

async fn community_id(p: &sqlx::Pool<sqlx::Postgres>) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO communities (id, host) VALUES ($1, $2) ON CONFLICT (lower(host)) DO NOTHING",
    )
    .bind(id)
    .bind(host())
    .execute(p)
    .await
    .unwrap();
    sqlx::query_scalar("SELECT id FROM communities WHERE lower(host) = lower($1)")
        .bind(host())
        .fetch_one(p)
        .await
        .unwrap()
}

async fn seed_member(p: &sqlx::Pool<sqlx::Postgres>, cid: Uuid, keys: &Keys) {
    sqlx::query("INSERT INTO users (community_id, pubkey) VALUES ($1, $2) ON CONFLICT DO NOTHING")
        .bind(cid)
        .bind(keys.public_key().to_bytes().to_vec())
        .execute(p)
        .await
        .ok();
    sqlx::query(
        "INSERT INTO relay_members (community_id, pubkey, role, added_by) VALUES ($1,$2,'member',NULL) \
         ON CONFLICT (community_id, pubkey) DO NOTHING",
    )
    .bind(cid)
    .bind(keys.public_key().to_hex())
    .execute(p)
    .await
    .unwrap();
}

/// Is this event id in the relay's ordinary event storage?
async fn is_stored(p: &sqlx::Pool<sqlx::Postgres>, cid: Uuid, event: &nostr::Event) -> bool {
    let n: i64 =
        sqlx::query_scalar("SELECT count(*) FROM events WHERE community_id = $1 AND id = $2")
            .bind(cid)
            .bind(event.id.to_bytes().to_vec())
            .fetch_one(p)
            .await
            .unwrap_or(0);
    n > 0
}

#[tokio::test]
#[ignore]
async fn project_activity_and_peer_call_kinds_are_ingested_and_neighbours_are_not() {
    let p = pool().await;
    let cid = community_id(&p).await;

    let agent = Keys::generate();
    let callee = Keys::generate();
    seed_member(&p, cid, &agent).await;
    seed_member(&p, cid, &callee).await;

    let owner_hex = Keys::generate().public_key().to_hex();
    let coordinate = format!("30617:{owner_hex}:phase6-ingress");
    let root = "f".repeat(64);

    // ── 1. NIP-PA activity is accepted ───────────────────────────────────────
    let activity = signed(
        &agent,
        KIND_PROJECT_ACTIVITY,
        "",
        vec![
            Tag::parse(["a", &coordinate]).unwrap(),
            Tag::parse(["e", &root, "", "root"]).unwrap(),
            Tag::parse(["agent", &agent.public_key().to_hex()]).unwrap(),
            Tag::parse(["state", "working"]).unwrap(),
            Tag::parse(["turn", "turn-1"]).unwrap(),
        ],
    );
    let (status, body) = post_event(&agent, &activity).await;
    assert_eq!(
        status, 200,
        "kind {KIND_PROJECT_ACTIVITY} refused at ingress: {body}"
    );

    // …and never stored. It is ephemeral: the frame says a turn is running now.
    assert!(
        !is_stored(&p, cid, &activity).await,
        "activity was persisted — an ephemeral 'working' frame must not outlive its turn"
    );

    // ── 2. The NIP-PC pair is accepted, and stored ───────────────────────────
    let nonce = "0".repeat(31) + "1";
    let call = signed(
        &agent,
        KIND_PEER_CALL,
        "PAIR:ingress-regression",
        vec![
            Tag::parse(["p", &callee.public_key().to_hex()]).unwrap(),
            Tag::parse(["call", &"a".repeat(64)]).unwrap(),
            Tag::parse(["nonce", &nonce]).unwrap(),
            Tag::parse(["hop", "1"]).unwrap(),
            Tag::parse(["visited", &agent.public_key().to_hex()]).unwrap(),
            Tag::parse(["a", &coordinate]).unwrap(),
            Tag::parse(["e", &root, "", "root"]).unwrap(),
        ],
    );
    let (status, body) = post_event(&agent, &call).await;
    assert_eq!(
        status, 200,
        "kind {KIND_PEER_CALL} refused at ingress: {body}"
    );

    let result = signed(
        &callee,
        KIND_PEER_CALL_RESULT,
        "done",
        vec![
            Tag::parse(["p", &agent.public_key().to_hex()]).unwrap(),
            Tag::parse(["call", &"a".repeat(64)]).unwrap(),
            Tag::parse(["a", &coordinate]).unwrap(),
            Tag::parse(["e", &root, "", "root"]).unwrap(),
        ],
    );
    let (status, body) = post_event(&callee, &result).await;
    assert_eq!(
        status, 200,
        "kind {KIND_PEER_CALL_RESULT} refused at ingress: {body}"
    );

    // Both must be readable back off the relay. `buzz agents call` runs in a
    // different process from the runtime that has to learn its call is
    // outstanding, so the stored copy is the only place that fact exists.
    assert!(
        is_stored(&p, cid, &call).await,
        "a peer call that is not stored can never be correlated by its caller"
    );
    assert!(
        is_stored(&p, cid, &result).await,
        "a call result that is not stored cannot resume the session that asked"
    );

    // ── 3. Three kinds admitted, not two ranges opened ───────────────────────
    for kind in [20004u16, 43002, 43003, 43005] {
        let stray = signed(&agent, kind, "", vec![]);
        let (status, body) = post_event(&agent, &stray).await;
        assert_eq!(
            status, 400,
            "kind {kind} was admitted — the fix widened a range instead of naming three kinds"
        );
        assert!(
            body.contains("unknown event kind"),
            "kind {kind} refused for the wrong reason: {body}"
        );
    }
}
