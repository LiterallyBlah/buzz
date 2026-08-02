#![deny(unsafe_code)]

mod acp;
mod config;
mod engram_fetch;
mod filter;
mod observer;
mod peer_call;
mod pool;
mod pool_lifecycle;
mod project;
mod queue;
mod relay;
mod setup_mode;
mod usage;

pub use usage::TurnUsage;

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use acp::{AcpClient, EnvVar, McpServer};
use anyhow::Result;
use buzz_core::kind::{
    KIND_MEMBER_ADDED_NOTIFICATION, KIND_MEMBER_REMOVED_NOTIFICATION, KIND_STREAM_MESSAGE,
    KIND_STREAM_REMINDER, KIND_WORKFLOW_APPROVAL_REQUESTED,
};
use buzz_core::observer::{
    decrypt_observer_payload, encrypt_observer_payload, OBSERVER_FRAME_TELEMETRY,
    OBSERVER_MAX_PLAINTEXT_LEN,
};
use clap::Parser;
use config::{
    AuthAgentArgs, AuthMethodsArgs, AuthenticateArgs, Config, DedupMode, ModelsArgs,
    MultipleEventHandling, RespondTo, SubscribeMode,
};
use filter::SubscriptionRule;
use futures_util::FutureExt;
use nostr::{PublicKey, ToBech32};
use pool::{
    AgentPool, ControlSignal, IdleSwitchResult, OwnedAgent, PromptContext, PromptOutcome,
    PromptResult, PromptSource, SessionState, TimeoutKind,
};
use pool_lifecycle::PoolLifecycle;
use queue::{CancelReason, EventQueue, FlushBatch, QueuedEvent, ThreadTags};
use relay::{BuzzEvent, HarnessRelay, RelayEventPublisher};
use tokio::sync::{mpsc, watch};
use tracing_subscriber::EnvFilter;
use uuid::Uuid;

/// Check if argv[1] matches a subcommand name, before any clap parsing.
///
/// This avoids clap rejecting harness flags (like `--private-key`) that aren't
/// declared on the subcommand's `Parser`. The `models` path has its own
/// dedicated parser; the default path uses the existing `CliArgs`.
///
/// **Constraint**: subcommand must be argv[1] — flags before the subcommand
/// name (e.g., `buzz-acp --verbose models`) are not supported.
fn is_subcommand(name: &str) -> bool {
    std::env::args().nth(1).map(|a| a == name).unwrap_or(false)
}

/// Timeout for lightweight helper subcommands (spawn + initialize + model/method probes).
const MODELS_TIMEOUT: Duration = Duration::from_secs(10);

/// Timeout for `buzz-acp authenticate`. Browser-based vendor auth can require
/// human interaction, so it must not share the short probe timeout.
const AUTHENTICATE_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// Publish a kind:20001 presence update event via the WebSocket connection.
///
/// Ephemeral kinds (20000-29999) are rejected by the HTTP bridge, so presence
/// updates must be routed through the WS path.
///
/// Content is a bare status string (`"online"`, `"away"`, `"offline"`) matching
/// the desktop client's format. The relay stores this in Redis and synthesizes
/// it back on presence queries.
async fn publish_presence(
    publisher: &relay::RelayEventPublisher,
    keys: &nostr::Keys,
    status: &str,
) -> Result<(), relay::RelayError> {
    use buzz_core::kind::KIND_PRESENCE_UPDATE;
    use nostr::{EventBuilder, Kind};

    let event = EventBuilder::new(Kind::Custom(KIND_PRESENCE_UPDATE as u16), status)
        .tags([])
        .sign_with_keys(keys)
        .map_err(|e| relay::RelayError::Http(format!("presence sign error: {e}")))?;
    publisher.publish_event(event).await?;
    Ok(())
}

fn emit_runtime_lifecycle(
    observer: Option<&observer::ObserverHandle>,
    start_nonce: &str,
    pubkey: &str,
    relay_url: &str,
    lifecycle: &str,
    error: Option<&str>,
) {
    if let Some(observer) = observer {
        observer.emit(
            "managed_agent_runtime_lifecycle",
            None,
            &observer::ObserverContext::default(),
            serde_json::json!({
                "pubkey": pubkey,
                "relayUrl": relay_url,
                "startNonce": start_nonce,
                "lifecycle": lifecycle,
                "error": error,
            }),
        );
    }
}

/// Resolve the agent's owner pubkey at startup.
///
/// Priority:
/// 1. `BUZZ_AUTH_TAG` env var — NIP-OA attestation signed by the owner.
///    Verified against the agent's own pubkey to extract the owner pubkey.
/// 2. `--agent-owner` CLI flag / `BUZZ_ACP_AGENT_OWNER` env var.
fn resolve_agent_owner(config: &Config) -> Option<String> {
    // Try BUZZ_AUTH_TAG first (NIP-OA attestation).
    if let Ok(auth_tag) = std::env::var("BUZZ_AUTH_TAG") {
        if !auth_tag.is_empty() {
            let agent_pk = config.keys.public_key();
            match buzz_sdk::nip_oa::verify_auth_tag(&auth_tag, &agent_pk) {
                Ok(owner_pk) => {
                    let owner_hex = owner_pk.to_hex().to_ascii_lowercase();
                    tracing::info!("owner resolved from BUZZ_AUTH_TAG: {owner_hex}");
                    return Some(owner_hex);
                }
                Err(e) => {
                    tracing::warn!("BUZZ_AUTH_TAG verification failed: {e} — falling back");
                }
            }
        }
    }

    // Fall back to --agent-owner config.
    config.agent_owner.clone()
}

/// Cache for the agent's owner pubkey.
///
/// Owner is now provided via `--agent-owner` config flag (no REST lookup).
/// Cache for the agent's owner pubkey + sibling lookups.
///
/// Siblings are other agents whose NIP-OA auth tag proves the same owner.
/// Lookup results are cached for the process lifetime (attestations are immutable).
struct OwnerCache {
    pubkey: Option<String>,
    /// author_hex → is_sibling (true = same owner, false = not)
    siblings: std::sync::Mutex<HashMap<String, bool>>,
}

impl OwnerCache {
    fn new(initial: Option<String>) -> Self {
        Self {
            pubkey: initial,
            siblings: std::sync::Mutex::new(HashMap::new()),
        }
    }

    /// Return the cached owner pubkey.
    fn get(&self) -> Option<&str> {
        self.pubkey.as_deref()
    }

    /// Check if author is a known sibling (cached result).
    fn is_known_sibling(&self, author: &str) -> Option<bool> {
        self.siblings.lock().ok()?.get(author).copied()
    }

    /// Cache a sibling lookup result.
    fn cache_sibling(&self, author: String, is_sibling: bool) {
        if let Ok(mut map) = self.siblings.lock() {
            // Cap at 256 entries to prevent unbounded growth.
            if map.len() >= 256 {
                map.clear();
            }
            map.insert(author, is_sibling);
        }
    }
}

/// Check if `author` is the owner OR a sibling (same owner via NIP-OA).
///
/// For unknown authors, queries their kind:0 profile to extract the NIP-OA
/// auth tag and verify the owner matches. Result is cached.
async fn is_owner_or_sibling(
    author: &str,
    owner_cache: &OwnerCache,
    rest_client: &relay::RestClient,
) -> bool {
    let my_owner = match owner_cache.get() {
        Some(o) => o,
        None => return false, // no owner configured — fail closed
    };

    // Direct owner check.
    if author == my_owner {
        return true;
    }

    // Check sibling cache.
    if let Some(cached) = owner_cache.is_known_sibling(author) {
        return cached;
    }

    // Query the author's kind:0 profile to check for NIP-OA auth tag.
    let is_sibling = check_sibling_via_profile(author, my_owner, rest_client).await;
    owner_cache.cache_sibling(author.to_string(), is_sibling);
    is_sibling
}

/// Inbound author gate decision: does this author's event fire a turn?
///
/// Coarse security policy applied before subscription rules. Both `OwnerOnly`
/// and `Allowlist` accept the owner and same-owner siblings; `Allowlist`
/// additionally accepts the explicit external pubkey list.
///
/// # DM hardening (`is_dm`)
///
/// Clients auto-p-tag every DM participant, so in a DM *any* participant's
/// message looks like a mention and would fire a turn. Combined with
/// agent-initiated DMs (the agent can be asked to DM a third party), that
/// turns `anyone`/`allowlist` modes into transitive access grants: whoever
/// lands in a DM with the agent can prompt it. To close that hole, when
/// `is_dm` is true only the owner and cryptographically verified same-owner
/// siblings may fire a turn — the explicit allowlist and `anyone` mode do
/// NOT apply inside DMs. `Nobody` still drops everything. Callers must
/// resolve `is_dm` fail-closed: unknown channel type ⇒ treat as DM.
async fn author_allowed(
    respond_to: &RespondTo,
    allowlist: &HashSet<String>,
    author: &str,
    is_dm: bool,
    owner_cache: &OwnerCache,
    rest_client: &relay::RestClient,
) -> bool {
    if is_dm {
        return match respond_to {
            RespondTo::Nobody => false,
            _ => is_owner_or_sibling(author, owner_cache, rest_client).await,
        };
    }
    match respond_to {
        RespondTo::Anyone => true,
        RespondTo::Nobody => false,
        RespondTo::OwnerOnly => is_owner_or_sibling(author, owner_cache, rest_client).await,
        RespondTo::Allowlist => {
            allowlist.contains(author)
                || is_owner_or_sibling(author, owner_cache, rest_client).await
        }
    }
}

/// Resolve whether `channel_id` is a DM, for the inbound author gate.
///
/// Resolution order:
/// 1. Startup discovery metadata (`startup_info`) — covers channels known at
///    process start.
/// 2. Per-loop resolution cache (`cache`) — covers channels resolved since.
/// 3. Lazy REST fetch of the channel's kind:39000 metadata — covers channels
///    the agent was added to *after* startup (the exploit path: an
///    agent-initiated DM is exactly such a channel).
///
/// Fail-closed: if the fetch fails or times out, the channel is treated as a
/// DM for this event and the result is NOT cached, so a later event retries
/// the fetch instead of pinning a mis-classification.
pub(crate) async fn is_dm_channel(
    channel_id: Uuid,
    channel_info: &pool::ChannelInfoResolver,
) -> bool {
    match channel_info.resolve(channel_id).await {
        Some(info) => info.channel_type == "dm",
        None => {
            tracing::warn!(
                channel_id = %channel_id,
                "channel type unresolved — treating as DM for author gate (fail closed)"
            );
            true
        }
    }
}

/// Resolve how far a peer-call author is trusted, for NIP-PC admission.
///
/// Deliberately **not** [`author_allowed`]. The channel gate answers "may this
/// author's message reach the agent at all", and two of its modes are broad by
/// design: `RespondTo::Anyone` accepts the whole relay, and an allowlist entry
/// is an approval for a *person*. Invocation is a narrower grant, so it asks its
/// own question: the owner, a cryptographically verified NIP-OA same-owner
/// sibling, or a pubkey the owner explicitly listed as an external agent —
/// nothing else, whatever the channel policy says.
async fn resolve_peer_trust(
    author: &str,
    agent_hex: &str,
    approved_external_agents: &std::collections::BTreeSet<String>,
    owner_cache: &OwnerCache,
    rest_client: &relay::RestClient,
) -> peer_call::PeerTrust {
    let author = author.to_ascii_lowercase();
    if author.eq_ignore_ascii_case(agent_hex) {
        return peer_call::PeerTrust::SelfAuthored;
    }
    // An explicit external-agent listing is an operator decision and does not
    // depend on a resolved owner: it is the one grant that survives an agent
    // with no NIP-OA owner at all.
    if approved_external_agents.contains(&author) {
        return peer_call::PeerTrust::TrustedAgent;
    }
    match owner_cache.get() {
        Some(owner) if author == owner => peer_call::PeerTrust::Owner,
        // No owner configured — fail closed. An unowned agent has no siblings,
        // and there is nothing left for a caller to be verified against.
        None => peer_call::PeerTrust::Untrusted,
        Some(_) => {
            if is_owner_or_sibling(&author, owner_cache, rest_client).await {
                peer_call::PeerTrust::TrustedAgent
            } else {
                peer_call::PeerTrust::Untrusted
            }
        }
    }
}

/// Query an author's kind:0 profile and check if their NIP-OA auth tag
/// proves the same owner as us.
async fn check_sibling_via_profile(
    author: &str,
    expected_owner: &str,
    rest_client: &relay::RestClient,
) -> bool {
    let filter = nostr::Filter::new()
        .kind(nostr::Kind::Metadata)
        .author(match nostr::PublicKey::from_hex(author) {
            Ok(pk) => pk,
            Err(_) => return false,
        })
        .limit(1);

    let resp = match tokio::time::timeout(Duration::from_millis(2000), rest_client.query(&[filter]))
        .await
    {
        Ok(Ok(v)) => v,
        _ => return false, // timeout or error — fail closed
    };

    // Look for an "auth" tag in the profile event.
    let events = match resp.as_array() {
        Some(arr) => arr,
        None => return false,
    };
    let event = match events.first() {
        Some(e) => e,
        None => return false,
    };
    let tags = match event.get("tags").and_then(|t| t.as_array()) {
        Some(t) => t,
        None => return false,
    };

    // Find ["auth", owner_pk, conditions, sig] and verify the Schnorr signature.
    // Don't trust the relay — verify ourselves.
    let agent_pk = match nostr::PublicKey::from_hex(author) {
        Ok(pk) => pk,
        Err(_) => return false,
    };

    for tag in tags {
        let parts = match tag.as_array() {
            Some(p) if p.len() >= 4 => p,
            _ => continue,
        };
        if parts[0].as_str() != Some("auth") {
            continue;
        }
        let tag_owner = match parts[1].as_str() {
            Some(o) => o,
            None => continue,
        };
        // Only verify if the owner field matches ours.
        if !tag_owner.eq_ignore_ascii_case(expected_owner) {
            continue;
        }
        // Cryptographically verify the NIP-OA attestation signature.
        let tag_json = serde_json::to_string(tag).unwrap_or_default();
        match buzz_sdk::nip_oa::verify_auth_tag(&tag_json, &agent_pk) {
            Ok(_) => {
                tracing::debug!(author, expected_owner, "sibling verified via NIP-OA");
                return true;
            }
            Err(e) => {
                tracing::debug!(author, "NIP-OA auth tag verification failed: {e}");
            }
        }
    }

    false
}

const OBSERVER_PUBLISH_INTERVAL: Duration = Duration::from_millis(167);
const OBSERVER_PUBLISH_LIMIT_PER_MINUTE: usize = 90;

struct ObserverPublishPacer {
    next_publish: tokio::time::Instant,
    published: VecDeque<tokio::time::Instant>,
}

impl ObserverPublishPacer {
    fn new() -> Self {
        Self {
            // No initial burst: even the first snapshot frame waits for its slot.
            next_publish: tokio::time::Instant::now() + OBSERVER_PUBLISH_INTERVAL,
            published: VecDeque::with_capacity(OBSERVER_PUBLISH_LIMIT_PER_MINUTE),
        }
    }

    async fn wait(&mut self) {
        loop {
            let now = tokio::time::Instant::now();
            while self
                .published
                .front()
                .is_some_and(|sent| now.duration_since(*sent) >= Duration::from_secs(60))
            {
                self.published.pop_front();
            }

            let minute_slot = self.published.front().and_then(|sent| {
                (self.published.len() >= OBSERVER_PUBLISH_LIMIT_PER_MINUTE)
                    .then_some(*sent + Duration::from_secs(60))
            });
            let publish_at =
                minute_slot.map_or(self.next_publish, |slot| slot.max(self.next_publish));
            if publish_at > now {
                tokio::time::sleep_until(publish_at).await;
                continue;
            }

            let published_at = tokio::time::Instant::now();
            self.published.push_back(published_at);
            self.next_publish = published_at + OBSERVER_PUBLISH_INTERVAL;
            return;
        }
    }
}

fn spawn_relay_observer_publisher(
    observer: observer::ObserverHandle,
    publisher: RelayEventPublisher,
    keys: nostr::Keys,
    agent_pubkey_hex: String,
    owner_pubkey_hex: String,
    owner_pubkey: PublicKey,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // Subscribe BEFORE snapshotting so an event emitted between the two
        // calls is never lost: it lands in the snapshot, the live receiver, or
        // both. The overlap is deduped in the run loop via the snapshot's
        // high-water `seq` (monotonic, assigned at emit).
        let rx = observer.subscribe();
        let snapshot = observer.snapshot();
        run_relay_observer_publisher(
            snapshot,
            rx,
            publisher,
            keys,
            agent_pubkey_hex,
            owner_pubkey_hex,
            owner_pubkey,
        )
        .await;
    })
}

async fn run_relay_observer_publisher(
    snapshot: Vec<observer::ObserverEvent>,
    mut rx: tokio::sync::broadcast::Receiver<observer::ObserverEvent>,
    publisher: RelayEventPublisher,
    keys: nostr::Keys,
    agent_pubkey_hex: String,
    owner_pubkey_hex: String,
    owner_pubkey: PublicKey,
) {
    let mut coalescer = ObserverChunkCoalescer::default();
    let mut pacer = ObserverPublishPacer::new();
    let max_snapshot_seq = snapshot.iter().map(|event| event.seq).max().unwrap_or(0);
    for event in snapshot {
        for event in coalescer.ingest(event) {
            publish_relay_observer_event(
                &publisher,
                &keys,
                &agent_pubkey_hex,
                &owner_pubkey_hex,
                &owner_pubkey,
                &mut pacer,
                event,
            )
            .await;
        }
    }

    let mut flush_interval = tokio::time::interval(std::time::Duration::from_millis(500));
    flush_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            result = rx.recv() => {
                match result {
                    Ok(event) => {
                        // Skip live events already delivered via the snapshot
                        // (the subscribe-before-snapshot overlap).
                        if event.seq <= max_snapshot_seq {
                            continue;
                        }
                        for event in coalescer.ingest(event) {
                            publish_relay_observer_event(
                                &publisher, &keys, &agent_pubkey_hex,
                                &owner_pubkey_hex, &owner_pubkey, &mut pacer, event,
                            ).await;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(count)) => {
                        for event in coalescer.flush() {
                            publish_relay_observer_event(
                                &publisher, &keys, &agent_pubkey_hex,
                                &owner_pubkey_hex, &owner_pubkey, &mut pacer, event,
                            ).await;
                        }
                        tracing::warn!(dropped = count, "relay observer publisher lagged");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        for event in coalescer.flush() {
                            publish_relay_observer_event(
                                &publisher, &keys, &agent_pubkey_hex,
                                &owner_pubkey_hex, &owner_pubkey, &mut pacer, event,
                            ).await;
                        }
                        break;
                    }
                }
            }
            _ = flush_interval.tick() => {
                // Periodic flush ensures live streaming even during continuous chunk delivery.
                for event in coalescer.flush() {
                    publish_relay_observer_event(
                        &publisher, &keys, &agent_pubkey_hex,
                        &owner_pubkey_hex, &owner_pubkey, &mut pacer, event,
                    ).await;
                }
            }
        }
    }
}

#[derive(Default)]
struct ObserverChunkCoalescer {
    pending: Vec<PendingObserverChunk>,
}

struct PendingObserverChunk {
    key: ObserverChunkKey,
    event: observer::ObserverEvent,
    text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ObserverChunkKey {
    update_type: String,
    message_id: Option<String>,
    channel_id: Option<String>,
    session_id: Option<String>,
    turn_id: Option<String>,
    agent_index: Option<usize>,
}

/// Flush coalesced chunks before they exceed the NIP-44 plaintext limit (65,535 bytes).
/// Leave headroom for the JSON envelope wrapping the text. This is a SOFT pre-flush
/// of raw text below the hard cap; `fit_observer_event_to_budget` (the final ceiling,
/// keyed to `OBSERVER_MAX_PLAINTEXT_LEN` in buzz-core/observer.rs:25) is what actually
/// guarantees the serialized frame fits. Edit one of these two and review the other.
const OBSERVER_CHUNK_MAX_TEXT_BYTES: usize = 60_000;

impl ObserverChunkCoalescer {
    fn ingest(&mut self, event: observer::ObserverEvent) -> Vec<observer::ObserverEvent> {
        let Some((key, text)) = observer_chunk_key_and_text(&event) else {
            let mut events = self.flush();
            events.push(event);
            return events;
        };

        if let Some(pending) = self.pending.iter_mut().find(|pending| pending.key == key) {
            // Flush before appending if this would exceed the plaintext size limit.
            if pending.text.len() + text.len() >= OBSERVER_CHUNK_MAX_TEXT_BYTES {
                let events = self.flush();
                // Start a new pending entry with the current chunk.
                self.pending.push(PendingObserverChunk { key, event, text });
                return events;
            }
            pending.text.push_str(&text);
            pending.event.seq = event.seq;
            pending.event.timestamp = event.timestamp;
            return Vec::new();
        }

        self.pending.push(PendingObserverChunk { key, event, text });
        Vec::new()
    }

    fn flush(&mut self) -> Vec<observer::ObserverEvent> {
        self.pending
            .drain(..)
            .map(|mut pending| {
                set_observer_chunk_text(&mut pending.event.payload, pending.text);
                pending.event
            })
            .collect()
    }
}

fn observer_chunk_key_and_text(
    event: &observer::ObserverEvent,
) -> Option<(ObserverChunkKey, String)> {
    let update = event.payload.get("params")?.get("update")?;
    let update_type = update.get("sessionUpdate")?.as_str()?;
    if !matches!(
        update_type,
        "agent_message_chunk" | "user_message_chunk" | "agent_thought_chunk"
    ) {
        return None;
    }

    let text = update.get("content")?.get("text")?.as_str()?.to_string();
    let message_id = update
        .get("messageId")
        .and_then(|value| value.as_str())
        .map(ToOwned::to_owned);

    Some((
        ObserverChunkKey {
            update_type: update_type.to_string(),
            message_id,
            channel_id: event.channel_id.clone(),
            session_id: event.session_id.clone(),
            turn_id: event.turn_id.clone(),
            agent_index: event.agent_index,
        },
        text,
    ))
}

fn set_observer_chunk_text(payload: &mut serde_json::Value, text: String) {
    let Some(content) = payload
        .get_mut("params")
        .and_then(|params| params.get_mut("update"))
        .and_then(|update| update.get_mut("content"))
    else {
        return;
    };

    if let Some(content_object) = content.as_object_mut() {
        content_object.insert("text".to_string(), serde_json::Value::String(text));
    }
}

/// Bytes of head and tail to retain from an elided string leaf — the value
/// shown to the renderer at each end. The ONLY tuning knob here: large enough
/// that a clipped diff/tool-result still shows real content, small enough that
/// eliding actually shrinks the frame.
const OBSERVER_LEAF_RETAIN_BYTES: usize = 3_000;

/// Trim an oversized observer telemetry frame so its SERIALIZED form fits under
/// `OBSERVER_MAX_PLAINTEXT_LEN`, instead of dropping the whole frame (silent
/// telemetry loss). The common case — a frame already under budget — is left
/// byte-identical.
///
/// The cap is measured in SERIALIZED bytes (JSON escaping makes serialized
/// length differ from raw), so the stop condition is always a full reserialize
/// of the whole frame: that counts the envelope, the variable `Option<String>`
/// IDs, and any elision markers exactly. No separate margin constant is needed.
///
/// Termination is provable: each iteration elides the largest string leaf that
/// would STRICTLY shrink the serialized frame, then reserializes. Shrinkability
/// is re-evaluated against each leaf's CURRENT value, so a leaf already at its
/// retained floor can never be re-elided — the loop strictly decreases the
/// serialized length each pass and is bounded by the leaf count. When no leaf
/// can shrink the frame and it still overflows, the payload is replaced with a
/// tiny stub, which trivially fits. Monotone decrease, bounded below by the stub.
///
/// **Signature choice (`&mut`, double-serialize accepted):** on the common
/// under-budget path this serializes the frame once to decide it fits, then
/// `encrypt_observer_payload` serializes it again — one extra `to_string` of an
/// already-small frame. Reusing that string would mean changing buzz-core's
/// `encrypt_observer_payload` signature or adding a parallel encrypt path; both
/// are out of this change's scope (buzz-core stays untouched). The clean `&mut`
/// signature with one cheap redundant serialize is the deliberate tradeoff.
fn fit_observer_event_to_budget(event: &mut observer::ObserverEvent) {
    if serialized_len(event) <= OBSERVER_MAX_PLAINTEXT_LEN {
        return;
    }

    // Raw size of the payload we are about to trim, captured before mutation so
    // the stub's `originalBytes` reports source bytes discarded, not serialized
    // overflow — consistent with the per-leaf marker's raw byte count.
    let original_payload_bytes = serde_json::to_string(&event.payload)
        .map(|s| s.len())
        .unwrap_or(0);

    // Elide the largest shrinkable leaf, reserialize, repeat. Each successful
    // elision strictly shrinks the serialized frame, and a floored leaf can
    // never be re-elided, so the loop is bounded by the leaf count.
    while let Some(leaf) = largest_shrinkable_leaf(&mut event.payload) {
        elide_leaf(leaf);
        if serialized_len(event) <= OBSERVER_MAX_PLAINTEXT_LEN {
            return;
        }
    }

    // No leaf can shrink the frame further and it still overflows: replace the
    // whole payload with a stub that is trivially under-cap.
    event.payload = serde_json::json!({
        "elided": format!("{} payload too large", event.kind),
        "originalBytes": original_payload_bytes,
    });
}

fn serialized_len(event: &observer::ObserverEvent) -> usize {
    serde_json::to_string(event).map(|s| s.len()).unwrap_or(0)
}

/// Find the longest string leaf that would STRICTLY shrink if elided, returning
/// a mutable handle to it. A leaf shrinks only if `head + marker + tail` is
/// shorter than its current value (the marker-pushback guard); a leaf already at
/// its retained floor fails this test and is skipped, which is what bounds the
/// loop. Returns `None` when no leaf can shrink.
fn largest_shrinkable_leaf(value: &mut serde_json::Value) -> Option<&mut serde_json::Value> {
    // First pass: find the byte length of the best candidate without holding a
    // borrow, then re-descend to return the matching mutable reference. Two
    // immutable-style passes keep the borrow checker happy without unsafe.
    let best_len = max_shrinkable_len(value)?;
    find_leaf_with_len(value, best_len)
}

/// Largest current length among string leaves that can strictly shrink.
fn max_shrinkable_len(value: &serde_json::Value) -> Option<usize> {
    match value {
        serde_json::Value::String(s) if leaf_shrinks(s) => Some(s.len()),
        serde_json::Value::String(_) => None,
        serde_json::Value::Array(items) => items.iter().filter_map(max_shrinkable_len).max(),
        serde_json::Value::Object(map) => map.values().filter_map(max_shrinkable_len).max(),
        _ => None,
    }
}

/// Return the first string leaf whose current length equals `target` and that
/// can strictly shrink. Used after `max_shrinkable_len` to re-acquire a mutable
/// borrow of the chosen leaf.
fn find_leaf_with_len(
    value: &mut serde_json::Value,
    target: usize,
) -> Option<&mut serde_json::Value> {
    match value {
        serde_json::Value::String(s) if s.len() == target && leaf_shrinks(s) => Some(value),
        serde_json::Value::Array(items) => items
            .iter_mut()
            .find_map(|item| find_leaf_with_len(item, target)),
        serde_json::Value::Object(map) => map
            .values_mut()
            .find_map(|item| find_leaf_with_len(item, target)),
        _ => None,
    }
}

/// True when eliding `s` to head + marker + tail yields a strictly shorter raw
/// string. The marker width grows with `N` (bytes removed), so a leaf only
/// marginally larger than the retained ends must NOT be touched.
fn leaf_shrinks(s: &str) -> bool {
    let (head_end, tail_start) = elision_boundaries(s);
    tail_start > head_end && {
        let removed = tail_start - head_end;
        let marker = elision_marker(removed);
        head_end + marker.len() + (s.len() - tail_start) < s.len()
    }
}

/// Replace the middle of a string leaf with `…[elided N bytes]…`, keeping a head
/// and tail slice on UTF-8 char boundaries. `N` is RAW bytes removed.
fn elide_leaf(leaf: &mut serde_json::Value) {
    let serde_json::Value::String(s) = leaf else {
        return;
    };
    let (head_end, tail_start) = elision_boundaries(s);
    let removed = tail_start - head_end;
    let mut elided = String::with_capacity(head_end + 32 + (s.len() - tail_start));
    elided.push_str(&s[..head_end]);
    elided.push_str(&elision_marker(removed));
    elided.push_str(&s[tail_start..]);
    *s = elided;
}

fn elision_marker(removed_bytes: usize) -> String {
    format!("…[elided {removed_bytes} bytes]…")
}

/// Byte offsets bounding the elided middle, snapped to char boundaries so we
/// never split a multi-byte char. Returns `(head_end, tail_start)` with
/// `head_end <= tail_start`.
fn elision_boundaries(s: &str) -> (usize, usize) {
    let head_end = floor_char_boundary(s, OBSERVER_LEAF_RETAIN_BYTES.min(s.len()));
    let tail_start = ceil_char_boundary(s, s.len().saturating_sub(OBSERVER_LEAF_RETAIN_BYTES));
    (head_end, tail_start.max(head_end))
}

fn floor_char_boundary(s: &str, mut i: usize) -> usize {
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn ceil_char_boundary(s: &str, mut i: usize) -> usize {
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

async fn publish_relay_observer_event(
    publisher: &RelayEventPublisher,
    keys: &nostr::Keys,
    agent_pubkey_hex: &str,
    owner_pubkey_hex: &str,
    owner_pubkey: &PublicKey,
    pacer: &mut ObserverPublishPacer,
    mut event: observer::ObserverEvent,
) {
    pacer.wait().await;
    // Trim oversized frames to fit the plaintext cap rather than letting
    // encrypt_observer_payload reject and drop them whole (silent telemetry loss).
    fit_observer_event_to_budget(&mut event);
    let encrypted = match encrypt_observer_payload(keys, owner_pubkey, &event) {
        Ok(encrypted) => encrypted,
        Err(error) => {
            tracing::warn!("failed to encrypt relay observer event: {error}");
            return;
        }
    };
    let builder = match buzz_sdk::build_agent_observer_frame(
        owner_pubkey_hex,
        agent_pubkey_hex,
        OBSERVER_FRAME_TELEMETRY,
        &encrypted,
    ) {
        Ok(builder) => builder,
        Err(error) => {
            tracing::warn!("failed to build relay observer event: {error}");
            return;
        }
    };
    let signed = match builder.sign_with_keys(keys) {
        Ok(event) => event,
        Err(error) => {
            tracing::warn!("failed to sign relay observer event: {error}");
            return;
        }
    };
    if let Err(error) = publisher.publish_event(signed).await {
        tracing::warn!("relay observer event dropped: {error}");
    }
}

/// Maximum age (seconds) for an observer control frame to be considered fresh.
const OBSERVER_CONTROL_FRESHNESS_SECS: i64 = 300;

fn handle_relay_observer_control_event(
    keys: &nostr::Keys,
    event: nostr::Event,
    pool: &mut AgentPool,
    observer: Option<&observer::ObserverHandle>,
    owner_pubkey_hex: &str,
) {
    // Defense-in-depth: verify signature even though the relay already checked.
    if let Err(e) = buzz_core::verify_event(&event) {
        tracing::warn!(error = %e, "observer control frame failed signature verification");
        return;
    }

    // Defense-in-depth: verify the sender is the resolved owner.
    if event.pubkey.to_hex() != owner_pubkey_hex {
        tracing::warn!(
            sender = %event.pubkey,
            expected = %owner_pubkey_hex,
            "observer control frame from non-owner — dropping"
        );
        return;
    }

    // Freshness: reject stale/replayed frames outside ±5 minute window.
    let now = chrono::Utc::now().timestamp();
    let event_ts = event.created_at.as_secs() as i64;
    if (event_ts - now).unsigned_abs() > OBSERVER_CONTROL_FRESHNESS_SECS as u64 {
        tracing::warn!(
            event_ts,
            now,
            "observer control frame outside freshness window — dropping"
        );
        return;
    }

    let payload = match decrypt_observer_payload::<serde_json::Value>(keys, &event) {
        Ok(payload) => payload,
        Err(error) => {
            tracing::warn!("failed to decrypt observer control frame: {error}");
            return;
        }
    };

    let command_type = payload.get("type").and_then(|value| value.as_str());
    match command_type {
        Some("cancel_turn") => {
            handle_cancel_turn_control(&payload, pool, observer);
        }
        Some("switch_model") => {
            handle_switch_model_control(&payload, pool, observer);
        }
        _ => {
            tracing::debug!(payload = %payload, "ignoring unknown observer control frame");
        }
    }
}

/// Handle a `cancel_turn` control frame: signal the in-flight task to cancel.
fn handle_cancel_turn_control(
    payload: &serde_json::Value,
    pool: &mut AgentPool,
    observer: Option<&observer::ObserverHandle>,
) {
    let Some(channel_id) = payload
        .get("channelId")
        .and_then(|value| value.as_str())
        .and_then(|value| value.parse::<Uuid>().ok())
    else {
        tracing::warn!("observer cancel_turn control frame missing valid channelId");
        return;
    };

    let fired = signal_in_flight_task(pool, channel_id, ControlSignal::Cancel);
    let status = if fired { "sent" } else { "no_active_turn" };
    if let Some(observer) = observer {
        observer.emit(
            "control_result",
            None,
            &observer::ObserverContext {
                channel_id: Some(channel_id.to_string()),
                session_id: None,
                turn_id: None,
                started_at: None,
            },
            serde_json::json!({
                "type": "cancel_turn",
                "status": status,
            }),
        );
    }
}

/// Handle a `switch_model` control frame (Phase 3a, Option ii).
///
/// Busy path: deliver `SwitchModel` over the in-flight task's oneshot — the
/// task cancels the turn, sets `desired_model`, and requeues the batch so it
/// re-runs on a fresh session under the new model. A catalog miss surfaces
/// post-cancel via `create_session_and_apply_model` (the turn restarts on the
/// unchanged model + an `unsupported_model` result).
///
/// Idle path: validate against the cached catalog *before* invalidating
/// (pre-cancel guard), then set `desired_model` + invalidate. The override
/// takes visible effect on the agent's next turn.
fn handle_switch_model_control(
    payload: &serde_json::Value,
    pool: &mut AgentPool,
    observer: Option<&observer::ObserverHandle>,
) {
    let Some(channel_id) = payload
        .get("channelId")
        .and_then(|value| value.as_str())
        .and_then(|value| value.parse::<Uuid>().ok())
    else {
        tracing::warn!("observer switch_model control frame missing valid channelId");
        return;
    };
    let Some(model_id) = payload.get("modelId").and_then(|value| value.as_str()) else {
        tracing::warn!("observer switch_model control frame missing modelId");
        return;
    };

    // A turn is in flight for this channel iff a task_map entry exists. The
    // agent is moved out of the pool during a turn, so the control oneshot is
    // the only reachable lever; an idle channel has no such entry.
    let turn_in_flight = pool
        .task_map()
        .values()
        .any(|m| m.channel_id == Some(channel_id));

    let status = if turn_in_flight {
        // Busy path: deliver over the oneshot. `false` means the oneshot was
        // already consumed this turn (a prior cancel/interrupt) — the turn is
        // already ending, so the switch cannot land on it.
        if signal_in_flight_task(
            pool,
            channel_id,
            ControlSignal::SwitchModel(model_id.to_string()),
        ) {
            "sent"
        } else {
            "turn_ending"
        }
    } else {
        // Idle path: validate against the cached catalog before invalidating.
        match pool.switch_idle_agent_model(channel_id, model_id) {
            IdleSwitchResult::Switched => "switched",
            IdleSwitchResult::UnsupportedModel => "unsupported_model",
            IdleSwitchResult::NoIdleAgent => "no_active_turn",
        }
    };

    if let Some(observer) = observer {
        observer.emit(
            "control_result",
            None,
            &observer::ObserverContext {
                channel_id: Some(channel_id.to_string()),
                session_id: None,
                turn_id: None,
                started_at: None,
            },
            serde_json::json!({
                "type": "switch_model",
                "status": status,
                "modelId": model_id,
            }),
        );
    }
}

/// Maximum crashes in a 60-second window before a slot's circuit opens.
const CIRCUIT_BREAKER_THRESHOLD: usize = 3;
/// Window for circuit-breaker crash counting.
const CIRCUIT_BREAKER_WINDOW: Duration = Duration::from_secs(60);
/// Cooldown before a tripped circuit breaker allows a probe respawn.
const CIRCUIT_BREAKER_COOLDOWN: Duration = Duration::from_secs(300); // 5 minutes
/// Base backoff delay for respawn (doubles per recent crash, capped at 30s).
const RESPAWN_BASE_DELAY: Duration = Duration::from_secs(1);
/// Maximum respawn backoff delay.
const RESPAWN_MAX_DELAY: Duration = Duration::from_secs(30);

/// Per-slot circuit breaker state.
///
/// `crash_times` holds timestamps of recent crashes within `CIRCUIT_BREAKER_WINDOW`.
/// `open_until` is set when the threshold is hit; the circuit stays open until that
/// instant, then allows one probe respawn (half-open). If the probe crashes, the
/// circuit re-opens for another `CIRCUIT_BREAKER_COOLDOWN` period.
///
/// All state transitions go through methods on this struct — callers never
/// manipulate `crash_times` or `open_until` directly.
struct SlotCircuit {
    crash_times: Vec<std::time::Instant>,
    open_until: Option<std::time::Instant>,
    /// True while a background respawn/refill task is in flight for this slot.
    /// Prevents duplicate spawns from maintenance ticks that fire before the
    /// previous spawn_and_init completes.
    respawn_in_flight: bool,
}

/// Result of [`SlotCircuit::record_crash`].
enum CrashVerdict {
    /// Respawn is allowed after sleeping for this duration (jittered backoff).
    Respawn(Duration),
    /// Circuit is open — do not respawn.
    CircuitOpen,
    /// Circuit was open but cooldown has elapsed — one probe respawn is allowed
    /// (no backoff sleep). If the probe crashes, the next `record_crash` will
    /// immediately re-open the circuit.
    HalfOpenProbe,
}

impl SlotCircuit {
    /// Record a crash and decide whether to respawn.
    ///
    /// This is the **single canonical path** for all crash → respawn decisions.
    /// Called by `respawn_agent_into`, `recover_panicked_agent`, and slot refill.
    fn record_crash(&mut self) -> CrashVerdict {
        let now = std::time::Instant::now();

        // Half-open: cooldown elapsed → allow one probe.
        if let Some(open_until) = self.open_until {
            if now >= open_until {
                // Pre-seed crash_times to threshold-1 so that if the probe
                // itself crashes on the *next* call, the threshold is hit
                // immediately and the circuit re-opens. This implements a
                // "prove stability for one full window" policy.
                self.crash_times.clear();
                for _ in 0..(CIRCUIT_BREAKER_THRESHOLD - 1) {
                    self.crash_times.push(now);
                }
                self.open_until = None;
                return CrashVerdict::HalfOpenProbe;
            } else {
                return CrashVerdict::CircuitOpen;
            }
        }

        // Record this crash and prune old entries.
        self.crash_times.push(now);
        self.crash_times
            .retain(|&t| now.duration_since(t) < CIRCUIT_BREAKER_WINDOW);

        let recent = self.crash_times.len();

        if recent >= CIRCUIT_BREAKER_THRESHOLD {
            self.open_until = Some(now + CIRCUIT_BREAKER_COOLDOWN);
            return CrashVerdict::CircuitOpen;
        }

        // Exponential backoff: 1s * 2^(recent-1), capped at 30s, with ±20% jitter.
        let base = RESPAWN_BASE_DELAY.saturating_mul(1u32 << (recent - 1).min(5));
        let capped = base.min(RESPAWN_MAX_DELAY);
        let jitter = (std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos() as f64)
            / 1_000_000_000.0; // 0.0..1.0
        let factor = 0.8 + jitter * 0.4; // 0.8..1.2
        CrashVerdict::Respawn(capped.mul_f64(factor))
    }

    /// Mark a spawn failure — opens the circuit so the slot isn't retried
    /// on every heartbeat tick. Uses fresh `Instant::now()` so spawn latency
    /// doesn't shorten the effective cooldown.
    fn mark_spawn_failed(&mut self) {
        self.open_until = Some(std::time::Instant::now() + CIRCUIT_BREAKER_COOLDOWN);
    }

    /// Check if an empty slot can be refilled. Unlike `record_crash`, this
    /// does NOT record a new crash — it only checks whether the circuit
    /// allows a respawn attempt.
    ///
    /// Returns `true` if respawn is allowed. For half-open probes, pre-seeds
    /// crash_times so the next crash re-opens immediately. For normal refills
    /// (no circuit was ever opened), crash history is preserved so the breaker
    /// can still trip if the refilled agent crashes quickly.
    fn can_refill(&mut self) -> bool {
        let now = std::time::Instant::now();
        match self.open_until {
            Some(open_until) => {
                if now >= open_until {
                    // Half-open probe: pre-seed crash_times.
                    self.crash_times.clear();
                    for _ in 0..(CIRCUIT_BREAKER_THRESHOLD - 1) {
                        self.crash_times.push(now);
                    }
                    self.open_until = None;
                    true
                } else {
                    false // cooldown not elapsed
                }
            }
            None => true, // no circuit open — normal refill, preserve crash history
        }
    }
}

/// True if any slot has a respawn task in flight. Used to prevent premature
/// "all agents dead" exits — a respawning agent may succeed in seconds.
fn any_respawn_in_flight(crash_history: &[SlotCircuit]) -> bool {
    crash_history.iter().any(|s| s.respawn_in_flight)
}

/// Result of a background respawn task.
struct RespawnResult {
    index: usize,
    /// Tuple: (initialized client, protocol version, agent name).
    result: Result<(AcpClient, u32, String)>,
}

/// Outcome of a non-cancelling steer attempt, forwarded from a per-attempt
/// watcher task (which awaits the `SteerRequest.ack_tx` oneshot) back to
/// the main loop's `select!`. The main loop drives queue side-effects from
/// this — it cannot await the oneshot itself without blocking the relay
/// stream.
///
/// Carries enough identity to operate on the right withheld event in
/// `EventQueue::withheld_native_steer`: `channel_id` is the routing key,
/// `event_id` is the hex id of the single event the steer carried.
struct SteerAckEvent {
    channel_id: Uuid,
    event_id: String,
    /// `Ok` if the read loop sent any of the locked `SteerAck` variants.
    /// `Err` if the oneshot was dropped without a send — should not happen
    /// under the current read-loop drains, but if it ever does the main
    /// loop treats it as `PromptCompletedNeutral` (release withheld, no
    /// fallback signal) to avoid leaking the withheld event.
    ack: std::result::Result<pool::SteerAck, tokio::sync::oneshot::error::RecvError>,
}

/// RAII guard that ensures a `RespawnResult` is sent even if the task panics.
/// Without this, a panicked respawn task would leave `respawn_in_flight = true`
/// permanently, silently losing the slot forever.
struct RespawnGuard {
    index: usize,
    tx: mpsc::Sender<RespawnResult>,
    sent: bool,
}

impl RespawnGuard {
    fn new(index: usize, tx: mpsc::Sender<RespawnResult>) -> Self {
        Self {
            index,
            tx,
            sent: false,
        }
    }

    /// Send the result and disarm the guard. Uses `try_send` (sync) so there
    /// is no await boundary between marking `sent` and actually enqueueing —
    /// cancellation cannot slip between the two.
    fn send(mut self, result: Result<(AcpClient, u32, String)>) {
        // Invariant: try_send succeeds because the channel capacity equals the
        // slot count, and respawn_in_flight guarantees at most one outstanding
        // result per slot. If this ever fails, the channel sizing or the
        // respawn_in_flight guard has drifted — that's a bug, not a transient.
        match self.tx.try_send(RespawnResult {
            index: self.index,
            result,
        }) {
            Ok(()) => self.sent = true,
            Err(e) => {
                tracing::error!(
                    agent = self.index,
                    "respawn result channel full or closed: {e}"
                );
                // Drop will fire and send a failure result as fallback.
            }
        }
    }
}

impl Drop for RespawnGuard {
    fn drop(&mut self) {
        if !self.sent {
            tracing::error!(
                agent = self.index,
                "respawn task exited without sending result — sending failure"
            );
            // Best-effort: try_send in Drop (can't await).
            let _ = self.tx.try_send(RespawnResult {
                index: self.index,
                result: Err(anyhow::anyhow!("respawn task panicked or was cancelled")),
            });
        }
    }
}

//
// Sync env-var propagation must run before the tokio runtime starts so that
// any child processes inherit the correct environment. This must happen in the
// sync entry point — `std::env::set_var` is only safe before tokio spawns
// worker threads (Rust 2024 edition safety requirement).

pub fn run() -> Result<()> {
    config::propagate_legacy_env_vars();
    tokio_main()
}

#[tokio::main]
async fn tokio_main() -> Result<()> {
    // Install the ring crypto provider for rustls (required for wss:// connections).
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("failed to install rustls crypto provider");
    if is_subcommand("models") {
        // Strip the subcommand token so clap doesn't reject it as a positional.
        // Keeps argv[0] (binary name) and passes everything after the subcommand.
        let filtered: Vec<String> = std::env::args()
            .enumerate()
            .filter(|(i, _)| *i != 1)
            .map(|(_, a)| a)
            .collect();
        let args = ModelsArgs::parse_from(&filtered);
        return run_models(args).await;
    }

    if is_subcommand("auth-methods") {
        let filtered: Vec<String> = std::env::args()
            .enumerate()
            .filter(|(i, _)| *i != 1)
            .map(|(_, a)| a)
            .collect();
        let args = AuthMethodsArgs::parse_from(&filtered);
        return run_auth_methods(args).await;
    }

    if is_subcommand("authenticate") {
        let filtered: Vec<String> = std::env::args()
            .enumerate()
            .filter(|(i, _)| *i != 1)
            .map(|(_, a)| a)
            .collect();
        let args = AuthenticateArgs::parse_from(&filtered);
        return run_authenticate(args).await;
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("buzz_acp=info")),
        )
        .compact()
        .init();

    let mut config = Config::from_cli().map_err(|e| anyhow::anyhow!("configuration error: {e}"))?;

    // ── Setup-mode early branch ───────────────────────────────────────────────
    //
    // When the desktop determines an agent is not ready (missing credentials,
    // model, or provider), it spawns buzz-acp with BUZZ_ACP_SETUP_PAYLOAD set.
    // We enter the minimal setup-listener path and never start the agent pool.
    if let Some(payload) = setup_mode::SetupPayload::from_env()
        .map_err(|e| anyhow::anyhow!("setup payload error: {e}"))?
    {
        tracing::info!("buzz-acp: setup payload present, entering setup-listener mode");
        return setup_mode::run_setup_listener(config, payload).await;
    }

    tracing::info!("buzz-acp starting: {}", config.summary());

    let observer = config
        .relay_observer
        .then(observer::ObserverHandle::in_process);
    if let Some(handle) = &observer {
        handle.emit(
            "harness_started",
            None,
            &observer::ObserverContext::default(),
            serde_json::json!({
                "relayUrl": config.relay_url,
                "agentCommand": config.agent_command,
                "agentArgs": config.agent_args,
                "parallelism": config.agents,
                "relayObserver": config.relay_observer,
            }),
        );
    }

    let mut pool = if config.lazy_pool {
        AgentPool::from_slots((0..config.agents).map(|_| None).collect())
    } else {
        initialize_agent_pool(&PoolStartup::from_config(&config, observer.clone()), None).await?
    };
    let mut pool_ready = !config.lazy_pool;
    let mut pool_lifecycle: PoolLifecycle<AgentPool> = PoolLifecycle::listening();

    // Capture a startup watermark BEFORE connecting to the relay. This timestamp
    // is used for membership notification replay (via startup_watermark) and as
    // the initial subscribe_since for channels discovered at startup. The Subscribe
    // handler falls back to subscribe_since when last_seen is None, closing the
    // blind spot between "agents ready" and "first REQ sent".
    let startup_watermark: u64 = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let pubkey_hex = config.keys.public_key().to_hex();

    // Parse BUZZ_AUTH_TAG into a nostr::Tag for NIP-OA relay membership delegation.
    let relay_auth_tag: Option<nostr::Tag> = std::env::var("BUZZ_AUTH_TAG")
        .ok()
        .filter(|s| !s.is_empty())
        .and_then(|s| buzz_sdk::nip_oa::parse_auth_tag(&s).ok());

    let mut relay =
        HarnessRelay::connect(&config.relay_url, &config.keys, &pubkey_hex, relay_auth_tag)
            .await
            .map_err(|e| anyhow::anyhow!("relay connect error: {e}"))?;

    // Tell the relay background task the watermark so it can use
    // `since = watermark - 5s` on the first REQ instead of `since=now`.
    // Best-effort: a failure here is non-fatal (we just lose the startup window
    // protection, which is the same as the pre-fix behaviour).
    if let Err(e) = relay.set_startup_watermark(startup_watermark).await {
        tracing::warn!("failed to set startup watermark: {e}");
    }

    tracing::info!("connected to relay at {}", config.relay_url);

    relay
        .subscribe_membership_notifications()
        .await
        .map_err(|e| anyhow::anyhow!("membership notification subscribe error: {e}"))?;
    tracing::info!("subscribed to membership notifications");

    // NIP-PC peer calls. Global and unconditional: a call is delivered on its
    // own subscription rather than through channel rules, because whether one
    // trusted agent can reach another must not depend on how `--subscribe-mode`
    // and `--kinds` happen to be set. Admission is still gated — an untrusted
    // relay identity's call is refused after delivery, not before it.
    relay
        .subscribe_peer_calls()
        .await
        .map_err(|e| anyhow::anyhow!("peer call subscribe error: {e}"))?;
    tracing::info!("subscribed to peer calls");

    // Repository discovery, behind the flag. This is the one project
    // subscription that depends on no prior state — `kind:30617` announcements
    // are what *produces* the discovered set, so it can be opened at startup.
    // Enrolment and watched-root REQs derive their filters from discovery and
    // enrolment state, so they belong to the driver, not here.
    //
    // The class passed here is what every inbound frame on this id will be
    // classified as; the id's spelling carries no authority. Registration
    // happens in lockstep with the write inside the relay task.
    open_startup_project_subscriptions(&config, &relay).await;

    let presence_publisher = relay.event_publisher();
    let presence_keys = config.keys.clone();

    // Priority: BUZZ_AUTH_TAG (NIP-OA attestation) → --agent-owner flag.
    let startup_owner: Option<String> = resolve_agent_owner(&config);
    if let Some(ref owner) = startup_owner {
        tracing::info!("agent owner: {owner}");
    } else {
        tracing::info!("no agent owner configured");
    }
    // Warn if owner-dependent mode but no owner resolved yet.
    if startup_owner.is_none() {
        match &config.respond_to {
            RespondTo::OwnerOnly => {
                tracing::warn!(
                    "respond-to=owner-only but no owner is set — all events will be \
                     dropped. Set BUZZ_AUTH_TAG or --agent-owner, or use --respond-to=anyone."
                );
            }
            RespondTo::Allowlist => {
                tracing::warn!(
                    "respond-to=allowlist but no owner is set — allowlisted pubkeys \
                     will still be accepted, but owner-based matching is unavailable \
                     until owner is resolved."
                );
            }
            _ => {} // anyone/nobody don't depend on owner
        }
    }
    let owner_cache = OwnerCache::new(startup_owner.clone());

    let mut relay_observer_control_rx = None;
    let mut relay_observer_publisher_task = None;
    let mut relay_observer_publisher = None;
    if config.relay_observer {
        if let (Some(observer), Some(owner_pubkey_hex)) =
            (observer.clone(), owner_cache.pubkey.clone())
        {
            match PublicKey::from_hex(&owner_pubkey_hex) {
                Ok(owner_pubkey) => {
                    relay_observer_publisher = Some((
                        observer,
                        relay.event_publisher(),
                        config.keys.clone(),
                        pubkey_hex.clone(),
                        owner_pubkey_hex,
                        owner_pubkey,
                    ));
                    relay
                        .subscribe_observer_controls()
                        .await
                        .map_err(|e| anyhow::anyhow!("observer control subscribe error: {e}"))?;
                    relay_observer_control_rx = relay.take_observer_control_rx();
                    tracing::info!("relay observer enabled");
                }
                Err(error) => {
                    tracing::warn!("relay observer disabled: invalid owner pubkey: {error}");
                }
            }
        } else {
            tracing::warn!(
                "relay observer requested but no agent owner was resolved at startup; \
                 observer frames will not be published"
            );
        }
    }

    let channel_info_map = relay
        .discover_channels()
        .await
        .map_err(|e| anyhow::anyhow!("channel discovery error: {e}"))?;

    tracing::info!("discovered {} channel(s)", channel_info_map.len());
    let channel_ids: Vec<Uuid> = channel_info_map.keys().copied().collect();

    let rules: Vec<SubscriptionRule> = match config.subscribe_mode {
        SubscribeMode::Mentions => {
            vec![SubscriptionRule {
                name: "mentions".into(),
                channels: filter::ChannelScope::All("all".into()),
                kinds: config.kinds_override.clone().unwrap_or_else(|| {
                    vec![
                        KIND_STREAM_MESSAGE,
                        KIND_WORKFLOW_APPROVAL_REQUESTED,
                        KIND_STREAM_REMINDER,
                    ]
                }),
                require_mention: !config.no_mention_filter,
                filter: None,
                compiled_filter: None,
                consecutive_timeouts: std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0)),
                prompt_tag: Some("@mention".into()),
            }]
        }
        SubscribeMode::All => {
            vec![SubscriptionRule {
                name: "all".into(),
                channels: filter::ChannelScope::All("all".into()),
                kinds: config.kinds_override.clone().unwrap_or_default(),
                require_mention: false,
                filter: None,
                compiled_filter: None,
                consecutive_timeouts: std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0)),
                prompt_tag: Some("all".into()),
            }]
        }
        SubscribeMode::Config => {
            // load_rules() already warns if the config file has zero rules.
            config::load_rules(&config.config_path)?
        }
    };

    let channel_filters = config::resolve_channel_filters(&config, &channel_ids, &rules);
    if channel_filters.is_empty() {
        tracing::warn!("no channel subscriptions resolved — agent will sit idle");
    }
    let mut subscribed_channel_ids = HashSet::with_capacity(channel_filters.len());
    for (channel_id, filter) in &channel_filters {
        if let Err(e) = relay.subscribe_channel(*channel_id, filter.clone()).await {
            tracing::warn!("failed to subscribe to channel {channel_id}: {e}");
        } else {
            subscribed_channel_ids.insert(*channel_id);
            tracing::info!("subscribed to channel {channel_id}");
        }
    }

    if let Some((observer, publisher, keys, agent_pubkey, owner_pubkey, owner)) =
        relay_observer_publisher.take()
    {
        relay_observer_publisher_task = Some(spawn_relay_observer_publisher(
            observer,
            publisher,
            keys,
            agent_pubkey,
            owner_pubkey,
            owner,
        ));
    }

    let runtime_start_nonce = std::env::var("BUZZ_MANAGED_AGENT_START_NONCE").unwrap_or_default();
    let dedup_mode = config.dedup_mode;
    let mut queue =
        EventQueue::new(dedup_mode).with_in_flight_deadline(config.max_turn_duration_secs);

    // Online means the harness can receive work, not merely that its socket is
    // connected. Publishing after channel subscriptions gives desktop callers
    // a durable readiness boundary before they send a startup mention.
    if config.presence_enabled {
        match publish_presence(&presence_publisher, &presence_keys, "online").await {
            Ok(_) => tracing::info!("presence set to online"),
            Err(e) => tracing::warn!("failed to set initial presence: {e}"),
        }
    }

    if config.lazy_pool {
        emit_runtime_lifecycle(
            observer.as_ref(),
            &runtime_start_nonce,
            &pubkey_hex,
            &config.relay_url,
            "listening",
            None,
        );
    }

    let base_prompt_content = config.base_prompt_content.take();
    let ctx = Arc::new(PromptContext {
        mcp_servers: build_mcp_servers(&config),
        initial_message: config.initial_message.clone(),
        idle_timeout: Duration::from_secs(config.idle_timeout_secs),
        max_turn_duration: Duration::from_secs(config.max_turn_duration_secs),
        turn_liveness_interval: Duration::from_secs(config.turn_liveness_secs),
        dedup_mode: config.dedup_mode,
        system_prompt: config.system_prompt.clone(),
        session_title: config.session_title.clone(),
        team_instructions: config.team_instructions.clone(),
        base_prompt: if config.no_base_prompt {
            None
        } else if let Some(content) = base_prompt_content {
            Some(Box::leak(content.into_boxed_str()))
        } else {
            Some(include_str!("base_prompt.md"))
        },
        heartbeat_prompt: config.heartbeat_prompt.clone(),
        cwd: std::env::current_dir()
            .unwrap_or_else(|_| std::path::PathBuf::from("/"))
            .to_string_lossy()
            .to_string(),
        rest_client: relay.rest_client(),
        channel_info: pool::ChannelInfoResolver::new(channel_info_map, relay.rest_client()),
        context_message_limit: config.context_message_limit,
        max_turns_per_session: config.max_turns_per_session,
        permission_mode: config.permission_mode,
        agent_keys: config.keys.clone(),
        agent_owner_pubkey: startup_owner
            .as_deref()
            .and_then(|hex| nostr::PublicKey::from_hex(hex).ok()),
        memory_enabled: config.memory_enabled,
        harness_name: crate::config::normalize_agent_command_identity(&config.agent_command),
        relay_url: config.relay_url.clone(),
    });

    if !config.memory_enabled {
        tracing::info!(
            target: "engram::core",
            "NIP-AE core memory injection disabled (re-enable by removing --no-memory / BUZZ_ACP_NO_MEMORY)"
        );
    }

    let mut heartbeat = if config.heartbeat_interval_secs > 0 {
        let interval = Duration::from_secs(config.heartbeat_interval_secs);
        Some(tokio::time::interval_at(
            tokio::time::Instant::now() + interval,
            interval,
        ))
    } else {
        None
    };
    let mut heartbeat_in_flight = false;

    let mut presence_heartbeat = if config.presence_enabled {
        let interval = Duration::from_secs(60);
        Some(tokio::time::interval_at(
            tokio::time::Instant::now() + interval,
            interval,
        ))
    } else {
        None
    };

    let mut typing_refresh = if config.typing_enabled {
        let interval = Duration::from_secs(3);
        Some(tokio::time::interval_at(
            tokio::time::Instant::now() + interval,
            interval,
        ))
    } else {
        None
    };
    let mut typing_channels: HashMap<Uuid, ThreadTags> = HashMap::new();
    let mut presence_task: Option<tokio::task::JoinHandle<()>> = None;

    // Runs at the TOP of every loop iteration via Instant check — cannot be
    // starved by the biased select. Slot refill spawns background tasks so
    // spawn_and_init never blocks the main loop.
    let maintenance_interval = Duration::from_secs(30);
    let mut last_maintenance = std::time::Instant::now();

    // Channel for background respawn tasks to return completed agents.
    // Bounded to agent count — at most one respawn per slot in flight.
    let (respawn_tx, mut respawn_rx) = mpsc::channel::<RespawnResult>(config.agents as usize);
    // JoinSet for respawn tasks so shutdown can abort them.
    let mut respawn_tasks: tokio::task::JoinSet<()> = tokio::task::JoinSet::new();
    let (wake_tx, mut wake_rx) = mpsc::channel::<(u32, Result<AgentPool, String>)>(1);
    let mut wake_tasks: tokio::task::JoinSet<()> = tokio::task::JoinSet::new();

    // Channel for non-cancelling steer ack watchers to forward outcomes back
    // to the main loop. Each `pool.send_steer(...) == Ok(())` spawns a
    // short-lived task that awaits the `SteerRequest.ack_tx` oneshot and
    // forwards a `SteerAckEvent`. Unbounded because:
    //   1. The producer count is bounded by in-flight goose turns
    //      (`agents` slots, capacity-1 `steer_tx` each), so the channel
    //      cannot legitimately back up under steady state.
    //   2. We must never drop a steer outcome — losing an ack would leak a
    //      withheld event in `EventQueue::withheld_native_steer` until
    //      `IN_FLIGHT_DEADLINE_SECS` expires.
    let (steer_ack_tx, mut steer_ack_rx) = mpsc::unbounded_channel::<SteerAckEvent>();

    // ── Step 7: Shutdown signal ───────────────────────────────────────────────
    let (shutdown_tx, mut shutdown_rx) = watch::channel(());

    let tx = shutdown_tx.clone();
    tokio::spawn(async move {
        tokio::signal::ctrl_c().await.ok();
        let _ = tx.send(());
    });

    #[cfg(unix)]
    {
        let tx = shutdown_tx.clone();
        tokio::spawn(async move {
            use tokio::signal::unix::{signal, SignalKind};
            let mut sigterm = signal(SignalKind::terminate()).expect("SIGTERM handler");
            sigterm.recv().await;
            let _ = tx.send(());
        });
    }

    // Track the newest membership notification timestamp per channel.
    // On reconnect the relay replays events newest-first, so the first event
    // per channel is authoritative. Any later event with ts < newest is stale.
    // Exact duplicates (same event ID) are caught by seen_membership_ids.
    //
    // Uses strict `<` (not `<=`) so that legitimate live events at the same
    // second are both processed. The seen_membership_ids set handles exact
    // replays that share the same timestamp.
    let mut membership_newest_ts: HashMap<Uuid, u64> = HashMap::new();
    // Two-generation dedup for membership event replays (bounded, no amnesia).
    // Rotates at 1000 entries instead of clearing the entire set at 2000.
    let mut seen_membership_current: HashSet<String> = HashSet::new();
    let mut seen_membership_previous: HashSet<String> = HashSet::new();

    // Channels the agent has been removed from. When a checked-out agent is
    // returned to the pool, its sessions for these channels are stripped, and
    // failed/panicked batches for these channels are dropped instead of requeued.
    //
    // Cleared on re-add (KIND_MEMBER_ADDED_NOTIFICATION) so re-joined channels
    // regain session affinity.
    //
    // Known limitation: if a batch is in-flight when the channel is removed AND
    // re-added before the batch returns, the stale batch may be requeued. This
    // is acceptable because: (a) the agent is a member again and has access,
    // (b) the events are from the agent's authorized history, (c) the window
    // is extremely narrow (membership changes are rare, prompt turns are seconds),
    // and (d) fixing this would require per-channel epoch tracking on TaskMeta
    // and PromptResult — significant complexity for a benign edge case. If strict
    // causal invalidation is needed, add a monotonic epoch counter per channel
    // and capture it in TaskMeta at dispatch time.
    let mut removed_channels: HashSet<Uuid> = HashSet::new();

    //
    // One SlotCircuit per agent slot. crash_times entries are pruned to the last
    // CIRCUIT_BREAKER_WINDOW on each respawn attempt. The Vec is indexed by
    // agent slot index, so it must be sized to the configured pool capacity
    // (not the live count, which may be smaller after partial startup).
    // Repositories this agent has actually discovered, from signature-verified
    // announcements. Owned by the run loop rather than reconstructed per event,
    // because it is the set enrolment validates against: a root may only enrol
    // on a repository whose announcement we saw, and whose coordinate came from
    // that announcement's signer.
    //
    // Empty and unused while `project_routing_enabled` is false — nothing
    // subscribes to discovery, so nothing ingests.
    let mut discovered_repositories = project::DiscoveredRepositories::new();

    // Which roots this process is watching, and whether each is active or
    // dormant. In-process only: Phase A makes no restart-history claim, so a
    // restart legitimately begins with an empty set and re-enrols on the next
    // authorised mention. Restoring this from the relay is the durability
    // phase's job and is deliberately absent here rather than half-present.
    let mut project_enrolments = project::ProjectEnrolments::new();

    // NIP-PC call state for this process: which call ids have been admitted,
    // and which of this agent's own calls are still awaiting a result. One
    // ledger for both surfaces — a call id is derived from its route, so a
    // channel call and a project call can never collide, but two ledgers could
    // disagree about having seen the same id and answer it twice.
    //
    // In-process only. A restart legitimately forgets outstanding calls: their
    // results then fail to correlate and are ignored rather than arriving as
    // unsolicited prompts, which is the safe direction to lose state in.
    let mut call_ledger = peer_call::CallLedger::new();

    // No project-subscription state is held here. What is installed, which
    // generation it carries and which predecessor it retires all live in the
    // background registry, because that is the only component that knows.
    // A copy on this side could only ever be a guess about the far side of a
    // channel, and the guess is what went wrong.

    // The agent's own identity, in both spellings, from one source — so the
    // `p`-tag check and visible-mention detection cannot end up pointed at
    // different keys.
    let project_agent_identity = project::AgentIdentity::new(&config.keys.public_key())
        .map_err(|e| anyhow::anyhow!("agent identity: {e}"))?;

    // Project invocation authority is **not** inherited from channel config.
    // `RespondTo::Anyone` and an empty allow-list both mean "permissive" for
    // channels; neither may grant a stranger the right to wake this agent on a
    // repository issue. Only the explicitly listed pubkeys count here, and the
    // owner is added separately by `classify_project_author`.
    let project_approved_humans: std::collections::BTreeSet<String> = config
        .respond_to_allowlist
        .iter()
        .filter_map(|p| project::canonical_root_id(p))
        .collect();

    // Agents under a *different* owner that this owner has explicitly approved
    // to call this one. Same-owner NIP-OA siblings are trusted without being
    // listed (locked decision 2); everything else has to be named, which is why
    // this set is populated from its own option rather than from
    // `respond_to_allowlist` — that list approves *people*, and inheriting it
    // here would turn "may talk to the agent" into "may invoke the agent".
    let project_approved_external_agents: std::collections::BTreeSet<String> = config
        .peer_agents
        .iter()
        .filter_map(|p| project::canonical_root_id(p))
        .collect();

    let mut crash_history: Vec<SlotCircuit> = (0..config.agents as usize)
        .map(|_| SlotCircuit {
            crash_times: Vec::new(),
            open_until: None,
            respawn_in_flight: false,
        })
        .collect();

    //
    // Branches 1 & 2 both need to borrow `pool`, but they access different
    // fields (result_rx vs join_set). We use `rx_and_join_set()` to split the
    // borrow, yielding a typed enum so the outer code can dispatch cleanly.
    enum PoolEvent {
        Result(Box<PromptResult>),
        Panic(tokio::task::JoinError),
        SteerAck(SteerAckEvent),
        Wake(u32, Result<AgentPool, String>),
    }

    loop {
        // Whether buffered work is waiting on a lazy pool. Also gates the
        // retry-deadline sleep arm below: a `Failed` lifecycle keeps its
        // (possibly past) `retry_at` until the next wake, so sleeping on it
        // unconditionally would complete instantly on every iteration — a
        // busy spin — whenever the queued work drained after a failed wake.
        let mut lazy_wake_work_pending = false;
        if config.lazy_pool && !pool_ready {
            lazy_wake_work_pending = queue.has_flushable_work();
            if let Some(attempt) = pool_lifecycle
                .start_wake_if_due(lazy_wake_work_pending, tokio::time::Instant::now())
            {
                emit_runtime_lifecycle(
                    observer.as_ref(),
                    &runtime_start_nonce,
                    &pubkey_hex,
                    &config.relay_url,
                    "waking",
                    None,
                );
                let startup = PoolStartup::from_config(&config, observer.clone());
                let wake_tx = wake_tx.clone();
                let wake_shutdown = shutdown_rx.clone();
                wake_tasks.spawn(async move {
                    let result = initialize_agent_pool(&startup, Some(wake_shutdown))
                        .await
                        .map_err(|error| error.to_string());
                    if let Err(error) = wake_tx.send((attempt, result)).await {
                        let (_attempt, result) = error.0;
                        if let Ok(mut abandoned_pool) = result {
                            shutdown_agent_pool(&mut abandoned_pool).await;
                        }
                    }
                });
            }
        }

        if pool_ready && last_maintenance.elapsed() >= maintenance_interval {
            last_maintenance = std::time::Instant::now();
            queue.compact_expired_state();

            // Slot refill: spawn background tasks for empty slots whose
            // circuit breaker allows it. spawn_and_init runs off the main
            // loop so it never blocks event processing.
            for (idx, slot) in crash_history.iter_mut().enumerate() {
                if pool.slot_alive(idx) || slot.respawn_in_flight {
                    continue;
                }
                if !slot.can_refill() {
                    continue;
                }
                slot.respawn_in_flight = true;
                tracing::info!(agent = idx, "slot refill: spawning background respawn");
                let cmd = config.agent_command.clone();
                let args = config.agent_args.clone();
                let env = config.persona_env_vars.clone();
                let has_codex = config.has_generated_codex_config;
                let observer = observer.clone();
                let guard = RespawnGuard::new(idx, respawn_tx.clone());
                respawn_tasks.spawn(async move {
                    let result = spawn_and_init(&cmd, &args, &env, has_codex, idx, observer).await;
                    guard.send(result);
                });
            }

            // Flush requeued batches whose retry_after has expired. Without
            // this, a batch requeued during crash recovery can sit idle
            // indefinitely on quiet channels — dispatch_pending is only
            // called on relay events or pool results, neither of which
            // arrive when the channel is silent.
            if queue.has_flushable_work() {
                for (channel_id, thread_tags) in dispatch_pending(&mut pool, &mut queue, &ctx) {
                    typing_channels.insert(channel_id, thread_tags);
                }
            }
        }

        let mut respawn_collected = false;
        while let Ok(rr) = respawn_rx.try_recv() {
            crash_history[rr.index].respawn_in_flight = false;
            match rr.result {
                Ok((acp, protocol_version, agent_name)) => {
                    let agent = OwnedAgent {
                        index: rr.index,
                        acp,
                        state: SessionState::default(),
                        model_capabilities: None,
                        desired_model: config.model.clone(),
                        model_overridden: false,
                        agent_name,
                        goose_system_prompt_supported: None,
                        protocol_version,
                    };
                    pool.return_agent(agent);
                    tracing::info!(agent = rr.index, "respawn complete");
                    respawn_collected = true;
                }
                Err(e) => {
                    crash_history[rr.index].mark_spawn_failed();
                    tracing::warn!(agent = rr.index, "respawn failed: {e} — circuit re-opened");
                }
            }
        }
        // Flush requeued events that were waiting for a live agent. Without
        // this, batches requeued during crash recovery sit idle until the
        // next relay event arrives — which can be minutes on quiet channels.
        if respawn_collected {
            for (channel_id, thread_tags) in dispatch_pending(&mut pool, &mut queue, &ctx) {
                typing_channels.insert(channel_id, thread_tags);
            }
        }

        // Borrow result_rx and join_set simultaneously via split-borrow helper.
        let pool_event: Option<PoolEvent> = {
            let (result_rx, join_set) = pool.rx_and_join_set();
            tokio::select! {
                biased;
                // recv() returning None means all senders dropped (pool was torn down).
                // Break cleanly instead of panicking.
                r = result_rx.recv(), if pool_ready => match r {
                    Some(result) => Some(PoolEvent::Result(Box::new(result))),
                    None => {
                        tracing::info!("result channel closed — exiting main loop");
                        break;
                    }
                },
                // Guard: join_next() returns None immediately when JoinSet is
                // empty, which would cause a tight spin. Only poll when there
                // are in-flight tasks.
                Some(Err(e)) = join_set.join_next(), if !join_set.is_empty() => {
                    Some(PoolEvent::Panic(e))
                }
                // Goose-native steer ack from a watcher task. Outcomes drive
                // queue side-effects (drop / release withheld event) and
                // optionally the cancel+merge fallback signal. See the
                // `Some(PoolEvent::SteerAck(...))` match arm below for the
                // locked semantics (Eva + Max + Perci).
                Some(ack_event) = steer_ack_rx.recv() => {
                    Some(PoolEvent::SteerAck(ack_event))
                }
                Some((attempt, result)) = wake_rx.recv(), if config.lazy_pool && !pool_ready => {
                    Some(PoolEvent::Wake(attempt, result))
                }
                // Gated on pending work: with an empty queue there is nothing
                // for the retry to dispatch, and a past `retry_at` would
                // otherwise complete instantly on every iteration (busy spin).
                // The next accepted event re-enables the arm.
                _ = async {
                    match pool_lifecycle.retry_at() {
                        Some(retry_at) if lazy_wake_work_pending => {
                            tokio::time::sleep_until(retry_at).await
                        }
                        _ => std::future::pending().await,
                    }
                } => None,
                Some(Err(error)) = wake_tasks.join_next(), if !wake_tasks.is_empty() => {
                    if let Some(attempt) = pool_lifecycle.waking_attempt() {
                        let message = format!("pool wake task failed: {error}");
                        if pool_lifecycle.cancel_wake(
                            attempt,
                            message.clone(),
                            tokio::time::Instant::now(),
                        ) {
                            emit_runtime_lifecycle(
                                observer.as_ref(),
                                &runtime_start_nonce,
                                &pubkey_hex,
                                &config.relay_url,
                                "failed",
                                Some(&message),
                            );
                        }
                    }
                    None
                }
                control_event = async {
                    match relay_observer_control_rx.as_mut() {
                        Some(rx) => rx.recv().await,
                        None => std::future::pending().await,
                    }
                } => {
                    let _ = result_rx;
                    match control_event {
                        Some(event) => {
                            if let Some(ref owner_hex) = owner_cache.pubkey {
                                handle_relay_observer_control_event(&config.keys, event, &mut pool, observer.as_ref(), owner_hex);
                            } else {
                                tracing::warn!("observer control frame received but no owner resolved — dropping");
                            }
                        }
                        None => {
                            relay_observer_control_rx = None;
                            tracing::warn!("relay observer control channel closed");
                        }
                    }
                    None
                }
                // Remaining branches don't touch pool — evaluated when pool is idle.
                buzz_event = relay.next_event() => {
                    let _ = result_rx; // end split borrow before relay handling
                    match buzz_event {
                        Some(BuzzEvent::Project(project_event)) => {
                            // Project dispatch. Deliberately separate from the
                            // channel arm rather than folded into it: the
                            // channel path resolves `is_dm_channel` against a
                            // real channel UUID, and a project route key names
                            // no channel, so that lookup would fail and its
                            // fail-closed default would silently reinterpret
                            // every project event as DM policy.
                            // The dispatch context is built here, per event,
                            // from live state. It holds no channel resolver and
                            // no relay capability, so neither a channel lookup
                            // nor a subscription is reachable from the gate.
                            //
                            // Subscription upkeep travels with dispatch in
                            // `dispatch_project_event` rather than being open
                            // -coded here: inline, the code that decides when
                            // to issue a REQ was unreachable from any test, and
                            // that is exactly where the enrolment-widening
                            // defect lived.
                            // Resolved before the gate, because the NIP-OA
                            // lookup is async and the gate is not.
                            let project_sibling = attest_project_sibling(
                                &project_event,
                                owner_cache.get(),
                                &owner_cache,
                                &ctx.rest_client,
                            )
                            .await;
                            let dispatched = dispatch_project_event(
                                &mut ProjectDispatch {
                                    identity: project::ProjectIdentity {
                                        agent: &project_agent_identity,
                                        agent_owner: owner_cache.get(),
                                        approved_humans: &project_approved_humans,
                                        approved_external_agents:
                                            &project_approved_external_agents,
                                    },
                                    discovered: &mut discovered_repositories,
                                    enrolments: &mut project_enrolments,
                                    queue: &mut queue,
                                    sibling: project_sibling,
                                    ledger: &mut call_ledger,
                                },
                                &relay,
                                &pubkey_hex,
                                startup_watermark,
                                &project_event,
                            )
                            .await;
                            tracing::debug!(?dispatched, "project dispatch");
                        }
                        Some(BuzzEvent::Channel { channel_id, event }) => {
                            let buzz_event = ChannelEvent { channel_id, event };
                            let kind_u32 = buzz_event.event.kind.as_u16() as u32;

                            if kind_u32 == KIND_MEMBER_ADDED_NOTIFICATION
                                || kind_u32 == KIND_MEMBER_REMOVED_NOTIFICATION
                            {
                                let ch = buzz_event.channel_id;
                                let ts = buzz_event.event.created_at.as_secs();
                                let eid = buzz_event.event.id.to_hex();

                                // Two-layer membership dedup:
                                //
                                // 1. Exact duplicate rejection (seen_membership_ids):
                                //    Catches the same event replayed on reconnect.
                                //
                                // 2. Timestamp watermark (membership_newest_ts):
                                //    Uses strict `<` so that older events from reconnect
                                //    replay are dropped, but legitimate live events at the
                                //    same second are both processed. This is safe because
                                //    exact duplicates are already caught by layer 1.
                                //
                                // Why not `<=`? That would suppress legitimate live
                                // add→remove (or remove→add) sequences in the same second,
                                // leaving the harness in the wrong membership state.
                                // Two-generation dedup: check both sets before inserting.
                                if seen_membership_current.contains(&eid)
                                    || seen_membership_previous.contains(&eid)
                                {
                                    tracing::debug!(
                                        channel_id = %ch,
                                        kind = kind_u32,
                                        "skipping duplicate membership notification (same event_id)"
                                    );
                                    continue;
                                }
                                seen_membership_current.insert(eid);
                                // Rotate at 1000: current → previous, no amnesia window.
                                if seen_membership_current.len() >= 1000 {
                                    seen_membership_previous =
                                        std::mem::take(&mut seen_membership_current);
                                }
                                if let Some(&newest) = membership_newest_ts.get(&ch) {
                                    if ts < newest {
                                        tracing::debug!(
                                            channel_id = %ch,
                                            kind = kind_u32,
                                            ts,
                                            newest,
                                            "skipping stale membership notification (older than newest)"
                                        );
                                        continue;
                                    }
                                }
                                membership_newest_ts.insert(ch, ts);

                                if kind_u32 == KIND_MEMBER_ADDED_NOTIFICATION {
                                    // Clear removal tracking so sessions are not
                                    // stripped for a legitimately re-added channel.
                                    removed_channels.remove(&ch);

                                    if subscribed_channel_ids.contains(&ch) {
                                        tracing::debug!(channel_id = %ch, "membership notification: channel already subscribed");
                                    } else if let Some(filter) = config::resolve_dynamic_channel_filter(&config, ch, &rules) {
                                        tracing::info!(channel_id = %ch, "membership notification: subscribing to new channel");
                                        if let Err(e) = relay.subscribe_channel_from(ch, filter, Some(ts)).await {
                                            tracing::warn!("failed to subscribe to new channel {ch}: {e}");
                                        } else {
                                            subscribed_channel_ids.insert(ch);
                                        }
                                    } else {
                                        tracing::debug!(channel_id = %ch, "membership notification: no matching rules — skipping");
                                    }
                                } else {
                                    subscribed_channel_ids.remove(&ch);
                                    tracing::info!(channel_id = %ch, "membership notification: unsubscribing from channel");
                                    if let Err(e) = relay.unsubscribe_channel(ch).await {
                                        tracing::warn!("failed to unsubscribe from channel {ch}: {e}");
                                    }
                                    // Drain queued events and invalidate sessions for the
                                    // removed channel. Events already in-flight will
                                    // complete normally (the relay may reject actions if
                                    // the agent lost access).
                                    let drained_ids = queue.drain_channel(ch);
                                    let invalidated = if pool_ready {
                                        pool.invalidate_channel_sessions(ch)
                                    } else {
                                        0
                                    };
                                    // Track removed channels so checked-out agents get
                                    // their sessions stripped when they return to the pool.
                                    removed_channels.insert(ch);
                                    typing_channels.remove(&ch);
                                    // Best-effort: clean up 👀 on drained events.
                                    // Note: the relay revokes membership before
                                    // emitting the notification, so this DELETE may
                                    // 403 on non-open channels. Stale 👀 in that
                                    // case is a known limitation — fix belongs in
                                    // the relay (clean up bot reactions on removal).
                                    if !drained_ids.is_empty() {
                                        let rc = ctx.rest_client.clone();
                                        let ids = drained_ids.clone();
                                        tokio::spawn(async move {
                                            for eid in &ids {
                                                pool::reaction_remove(&rc, eid, "👀").await;
                                            }
                                        });
                                    }
                                    if !drained_ids.is_empty() || invalidated > 0 {
                                        tracing::info!(
                                            channel_id = %ch,
                                            drained = drained_ids.len(),
                                            invalidated,
                                            "cleaned up after membership removal"
                                        );
                                    }
                                }
                                continue;
                            }

                            // ── NIP-PC: channel-routed peer calls ────────────
                            //
                            // Ahead of `ignore_self` on purpose. This agent's
                            // *own* calls have to be seen here: the harness
                            // never publishes one — the agent subprocess runs
                            // `buzz agents call` — so its own event coming back
                            // off the wire is the only place the outstanding-
                            // call ledger can learn the call exists. Dropped as
                            // self-authored, every returned result would
                            // correlate to nothing.
                            //
                            // Ahead of the channel author gate for the same
                            // reason it does not reuse it: invocation is a
                            // narrower grant than "may speak to this agent",
                            // and `RespondTo::Anyone` must not confer it.
                            if is_peer_call_kind(kind_u32) {
                                let author = buzz_event.event.pubkey.to_hex();
                                let trust = resolve_peer_trust(
                                    &author,
                                    &pubkey_hex,
                                    &project_approved_external_agents,
                                    &owner_cache,
                                    &ctx.rest_client,
                                )
                                .await;
                                match decide_channel_peer_event(
                                    &buzz_event.event,
                                    buzz_event.channel_id,
                                    &pubkey_hex,
                                    trust,
                                    &mut call_ledger,
                                ) {
                                    ChannelPeerOutcome::Turn {
                                        channel_id,
                                        prompt_tag,
                                    } => {
                                        let event_id_hex = buzz_event.event.id.to_hex();
                                        let accepted = queue.push(QueuedEvent {
                                            channel_id,
                                            event: buzz_event.event,
                                            received_at: std::time::Instant::now(),
                                            prompt_tag: prompt_tag.into(),
                                            // A channel call keys on the
                                            // channel, exactly as an ordinary
                                            // message does.
                                            project: None,
                                        });
                                        if accepted {
                                            let rc = ctx.rest_client.clone();
                                            tokio::spawn(async move {
                                                pool::reaction_add(&rc, &event_id_hex, "👀").await;
                                            });
                                        }
                                    }
                                    ChannelPeerOutcome::Consumed
                                    | ChannelPeerOutcome::NotPeerCall => {}
                                }
                                continue;
                            }

                            if config.ignore_self && buzz_event.event.pubkey.to_hex() == pubkey_hex {
                                tracing::debug!(channel_id = %buzz_event.channel_id, "dropping self-authored event");
                                continue;
                            }

                            // Check: kind:9, content "!shutdown", from owner, mentions THIS agent.
                            let is_shutdown = is_owner_control_command(
                                &buzz_event.event,
                                kind_u32,
                                "!shutdown",
                                &pubkey_hex,
                            );
                            if is_shutdown {
                                let owner = owner_cache.get();
                                if let Some(owner) = owner {
                                    if buzz_event.event.pubkey.to_hex() == *owner {
                                        tracing::info!(
                                            channel_id = %buzz_event.channel_id,
                                            sender = %buzz_event.event.pubkey.to_hex(),
                                            "shutdown command from owner — exiting gracefully"
                                        );
                                        let _ = shutdown_tx.send(());
                                        continue;
                                    }
                                }
                                // Not from owner — fall through to normal prompt handling.
                                // Don't drop it — it's a regular message that happens to
                                // contain "!shutdown" from a non-owner.
                            }

                            // Mirrors !shutdown: kind:9, content "!cancel", from
                            // owner, mentions THIS agent. Must be BEFORE
                            // queue.push() — the event content is moved by push.
                            //
                            // Mode-independent: !cancel fires regardless of
                            // --multiple-event-handling. It is explicit user
                            // intent, not an automatic policy decision.
                            let is_cancel = is_owner_control_command(
                                &buzz_event.event,
                                kind_u32,
                                "!cancel",
                                &pubkey_hex,
                            );
                            if is_cancel {
                                if let Some(owner) = owner_cache.get() {
                                    if buzz_event.event.pubkey.to_hex() == *owner {
                                        let fired = signal_in_flight_task(
                                            &mut pool,
                                            buzz_event.channel_id,
                                            ControlSignal::Cancel,
                                        );
                                        if !fired {
                                            tracing::warn!(
                                                channel_id = %buzz_event.channel_id,
                                                "!cancel received but no in-flight task — no-op"
                                            );
                                        }
                                        continue; // consume event — do NOT push to queue
                                    }
                                }
                                // Not from owner — fall through to normal prompt handling.
                            }

                            // Mirrors !shutdown / !cancel: kind:9, content
                            // "!rotate", from owner, mentions THIS agent.
                            //
                            // Rotation is explicit owner intent to start the
                            // next turn in this channel with a fresh ACP
                            // session. It is consumed by the harness and never
                            // forwarded to the agent. If a turn is in-flight,
                            // cancel it, drop its triggering batch, and
                            // invalidate the channel session when the task
                            // returns. If idle, invalidate the cached channel
                            // session immediately. Queued future events remain
                            // queued and will create a fresh session on dispatch.
                            let is_rotate = is_owner_control_command(
                                &buzz_event.event,
                                kind_u32,
                                "!rotate",
                                &pubkey_hex,
                            );
                            if is_rotate {
                                if let Some(owner) = owner_cache.get() {
                                    if buzz_event.event.pubkey.to_hex() == *owner {
                                        let fired = signal_in_flight_task(
                                            &mut pool,
                                            buzz_event.channel_id,
                                            ControlSignal::Rotate,
                                        );
                                        if fired {
                                            tracing::info!(
                                                channel_id = %buzz_event.channel_id,
                                                "!rotate received — cancelling in-flight turn and rotating session"
                                            );
                                        } else {
                                            let invalidated = pool.invalidate_channel_sessions(buzz_event.channel_id);
                                            tracing::info!(
                                                channel_id = %buzz_event.channel_id,
                                                invalidated,
                                                "!rotate received — invalidated idle channel session(s)"
                                            );
                                        }
                                        continue; // consume event — do NOT push to queue
                                    }
                                }
                                // Not from owner — fall through to normal prompt handling.
                            }

                            // Coarse security policy: drop events from disallowed
                            // authors before they reach subscription rules or the
                            // agent. Must be AFTER !shutdown (owner can always
                            // shut down regardless of gate mode).
                            //
                            // Both OwnerOnly and Allowlist accept events from
                            // "siblings" — pubkeys whose agent_owner_pubkey
                            // matches this agent's owner (e.g. other bots
                            // launched by the same human). Allowlist adds the
                            // explicit pubkey list on top, for external people;
                            // it never revokes same-owner team bots.
                            {
                                let author = buzz_event.event.pubkey.to_hex();
                                // DM hardening: resolve channel type (fail-closed
                                // to DM) so allowlist/anyone modes cannot be
                                // exercised by non-owner authors inside DMs.
                                let is_dm =
                                    is_dm_channel(buzz_event.channel_id, &ctx.channel_info).await;
                                let allowed = author_allowed(
                                    &config.respond_to,
                                    &config.respond_to_allowlist,
                                    &author,
                                    is_dm,
                                    &owner_cache,
                                    &ctx.rest_client,
                                )
                                .await;
                                if !allowed {
                                    tracing::debug!(
                                        channel_id = %buzz_event.channel_id,
                                        author = %buzz_event.event.pubkey.to_hex(),
                                        mode = %config.respond_to,
                                        is_dm,
                                        "inbound author gate — dropping event"
                                    );
                                    continue;
                                }
                            }

                            let matched = filter::match_event(&buzz_event.event, buzz_event.channel_id, &rules, &pubkey_hex).await;
                            let prompt_tag = match matched {
                                Some(m) => m.prompt_tag,
                                None => {
                                    tracing::debug!(channel_id = %buzz_event.channel_id, kind = buzz_event.event.kind.as_u16(), "event matched no rule — dropping");
                                    continue;
                                }
                            };
                            // Capture author pubkey before queue.push() moves
                            // buzz_event.event (needed for mode gate below).
                            let author_hex = buzz_event.event.pubkey.to_hex();
                            let event_id_hex = buzz_event.event.id.to_hex();
                            // Clone for the non-cancelling steer fork, which
                            // needs the event to render the steer body. The
                            // clone is unconditional because we don't know
                            // yet whether the mode gate will demand a steer
                            // — checking `multiple_event_handling` here
                            // would couple the queueing path to the mode
                            // and break the existing invariant that every
                            // accepted event goes through `queue.push`
                            // first. `nostr::Event::clone` is cheap (Arc-
                            // backed payload) so the cost is negligible.
                            let event_for_steer = buzz_event.event.clone();
                            let prompt_tag_for_steer = prompt_tag.clone();
                            let accepted = queue.push(QueuedEvent {
                                channel_id: buzz_event.channel_id,
                                event: buzz_event.event,
                                received_at: std::time::Instant::now(),
                                prompt_tag,
                                // Channel events are never project-routed; the project
                                // branch has its own queue insertion.
                                project: None,
                            });
                            // 👀 — immediate "seen" reaction, only if the event
                            // was actually queued (not dropped by DedupMode::Drop).
                            // Fire-and-forget: on rare fast-failure paths the
                            // guard's cleanup may race with this add, leaving a
                            // cosmetic stale 👀. Acceptable — see ReactionGuard docs.
                            if accepted {
                                let rc = ctx.rest_client.clone();
                                let eid = event_id_hex.clone();
                                tokio::spawn(async move {
                                    pool::reaction_add(&rc, &eid, "👀").await;
                                });
                            }
                            // Event is already queued. If mode requires it AND
                            // the channel has an in-flight task, fire cancel —
                            // OR take the non-cancelling (ACP steer) fork for Steer signals.
                            if accepted && queue.is_channel_in_flight(buzz_event.channel_id) {
                                // Author eligibility (owner ∪ allowlist ∪ siblings)
                                // is already enforced by the inbound author gate
                                // above, so the mid-turn signal fires for every
                                // event that reaches here.
                                let signal = mode_gate_signal(
                                    config.multiple_event_handling,
                                    &author_hex,
                                    owner_cache.get(),
                                );
                                if let Some(signal) = signal {
                                    // Non-cancelling fork: when the mode
                                    // wants a Steer, attempt the
                                    // non-cancelling path first. On accept,
                                    // withhold the queued event and spawn an
                                    // ack watcher; the main loop's
                                    // `PoolEvent::SteerAck` arm decides
                                    // success/release/fallback. On reject
                                    // (including agents that advertise no
                                    // steer transport at all), fall through
                                    // to the universal cancel+merge `Steer`
                                    // signal so the event still reaches the
                                    // agent.
                                    let native_attempted = matches!(signal, ControlSignal::Steer)
                                        && try_native_steer(
                                            &mut pool,
                                            &mut queue,
                                            buzz_event.channel_id,
                                            event_for_steer,
                                            prompt_tag_for_steer,
                                            &steer_ack_tx,
                                        );
                                    if !native_attempted {
                                        signal_in_flight_task(
                                            &mut pool,
                                            buzz_event.channel_id,
                                            signal,
                                        );
                                    }
                                }
                            }
                            if pool_ready {
                                for (channel_id, thread_tags) in
                                    dispatch_pending(&mut pool, &mut queue, &ctx)
                                {
                                    typing_channels.insert(channel_id, thread_tags);
                                }
                            }
                        }
                        None => {
                            tracing::warn!("relay event stream ended — requesting reconnect");
                            if let Err(e) = relay.reconnect().await {
                                tracing::error!("relay background task is gone: {e} — exiting");
                                tokio::time::sleep(Duration::from_secs(1)).await;
                                break;
                            }
                        }
                    }
                    None
                }
                _ = async {
                    match heartbeat.as_mut() {
                        Some(hb) => hb.tick().await,
                        None => std::future::pending().await,
                    }
                } => {
                    let _ = result_rx;
                    if !pool_ready {
                        tracing::debug!("heartbeat_skipped_pool_not_ready");
                    } else if queue.has_flushable_work() {
                        tracing::debug!("heartbeat_skipped_events");
                        for (channel_id, thread_tags) in
                            dispatch_pending(&mut pool, &mut queue, &ctx)
                        {
                            typing_channels.insert(channel_id, thread_tags);
                        }
                    } else if pool.any_idle() {
                        dispatch_heartbeat(&mut pool, &ctx, &mut heartbeat_in_flight);
                    } else {
                        tracing::debug!("heartbeat_skipped_busy");
                    }
                    None
                }
                _ = async {
                    match presence_heartbeat.as_mut() {
                        Some(t) => t.tick().await,
                        None => std::future::pending().await,
                    }
                } => {
                    let _ = result_rx;
                    // Abort previous heartbeat if still in flight (prevents race on shutdown).
                    if let Some(h) = presence_task.take() {
                        h.abort();
                    }
                    let pp = presence_publisher.clone();
                    let pk = presence_keys.clone();
                    presence_task = Some(tokio::spawn(async move {
                        if let Err(e) = publish_presence(&pp, &pk, "online").await {
                            tracing::warn!("presence heartbeat failed: {e}");
                        }
                    }));
                    None
                }
                _ = async {
                    match typing_refresh.as_mut() {
                        Some(t) => t.tick().await,
                        None => std::future::pending().await,
                    }
                } => {
                    let _ = result_rx;
                    // Use try_publish (non-blocking) for typing indicators —
                    // they're ephemeral and must not block the main loop during
                    // relay reconnection (#35).
                    for (&ch, thread_tags) in &typing_channels {
                        if let Ok(event) = relay.build_typing_event(
                            ch,
                            thread_tags.root_event_id.as_deref(),
                            thread_tags.parent_event_id.as_deref(),
                        ) {
                            if let Err(e) = relay.try_publish_event(event) {
                                tracing::debug!("typing indicator dropped for {ch}: {e}");
                            }
                        }
                    }
                    None
                }
                _ = shutdown_rx.changed() => {
                    tracing::info!("shutting down");
                    break;
                }
            }
        };

        match pool_event {
            Some(PoolEvent::Result(result)) => {
                // Stop typing indicator for the completed channel.
                if let PromptSource::Channel(ch) = &result.source {
                    typing_channels.remove(ch);
                }
                if handle_prompt_result(
                    &mut pool,
                    &mut queue,
                    &config,
                    *result,
                    &mut heartbeat_in_flight,
                    &removed_channels,
                    &mut crash_history,
                    &respawn_tx,
                    &mut respawn_tasks,
                    observer.clone(),
                    Some(&ctx.rest_client),
                ) == LoopAction::Exit
                {
                    break;
                }
                if drain_ready_join_results(
                    &mut pool,
                    &mut queue,
                    &config,
                    &mut heartbeat_in_flight,
                    &removed_channels,
                    &mut typing_channels,
                    &mut crash_history,
                    &respawn_tx,
                    &mut respawn_tasks,
                    observer.clone(),
                ) == LoopAction::Exit
                {
                    break;
                }
                for (channel_id, thread_tags) in dispatch_pending(&mut pool, &mut queue, &ctx) {
                    typing_channels.insert(channel_id, thread_tags);
                }
            }
            Some(PoolEvent::Panic(join_error)) => {
                tracing::error!("agent task panicked: {join_error}");
                recover_panicked_agent(
                    &mut pool,
                    &mut queue,
                    &config,
                    join_error,
                    &mut heartbeat_in_flight,
                    &removed_channels,
                    &mut typing_channels,
                    &mut crash_history,
                    &respawn_tx,
                    &mut respawn_tasks,
                    observer.clone(),
                );
                if pool.live_count() == 0 && !any_respawn_in_flight(&crash_history) {
                    tracing::error!("all agents dead — exiting");
                    break;
                }
                for (channel_id, thread_tags) in dispatch_pending(&mut pool, &mut queue, &ctx) {
                    typing_channels.insert(channel_id, thread_tags);
                }
            }
            Some(PoolEvent::SteerAck(SteerAckEvent {
                channel_id,
                event_id,
                ack,
            })) => {
                // Mid-turn steer attempt resolved (either transport:
                // `_goose/unstable/session/steer` or `_session/steering`).
                // Locked semantics (Eva + Max + Perci, unanimous on Option X):
                //
                //   Success
                //     The agent received the steer via the non-cancelling
                //     path. Drop the withheld event so normal dispatch
                //     never redelivers it.
                //
                //     Also covers `_session/steering`'s `startedNewTurn`
                //     outcome: the message was delivered, but into a fresh
                //     turn because the one being steered had already
                //     finished. Delivery is what this arm keys on, so the
                //     event is still dropped. The read loop deliberately
                //     does NOT renew its hard deadline in that case (the
                //     awaited turn is settled), while
                //     `extend_in_flight_deadline` below still applies —
                //     the agent really is running more work, so the
                //     channel's in-flight budget should reflect it.
                //
                //   Err(_) where the write never landed (Transport /
                //   ExpectedRunIdMissing):
                //     Delivery state of the underlying message is "never
                //     attempted on the wire". Release withheld back to the
                //     queue front AND issue the cancel+merge fallback so
                //     the message still reaches the agent.
                //
                //   Err(OutcomeRejected { .. })
                //     A `_session/steering` request returned a JSON-RPC
                //     success whose `outcome` was not `injected` or
                //     `startedNewTurn` (codex's `failed`, an unknown value,
                //     or a bare `{}` with no `outcome` at all). The steer
                //     did not land, so this is treated exactly like a write
                //     that never happened: release withheld AND fire the
                //     cancel+merge fallback. Handled by the catch-all
                //     `Err(_)` arm below.
                //
                //   Err(AgentError { code: -32601, .. })
                //     The agent returned method_not_found — it does not
                //     implement the steer extension. Release withheld AND
                //     fire the cancel+merge fallback so the message still
                //     reaches the agent via the universal path.
                //
                //   Err(AgentError { code: other, .. })
                //     The write landed and the agent returned a JSON-RPC
                //     error at the application level (e.g. wrong run id).
                //     The agent's turn is still running (or just completed).
                //     Release withheld for normal dispatch; do NOT fire the
                //     fallback signal — the agent already saw the steer
                //     attempt. If the turn is still running, normal dispatch
                //     re-delivers when it completes. If the turn already
                //     ended, there is nothing to cancel.
                //
                //   PromptCompletedNeutral
                //     The read loop wrote the steer (or was preparing to)
                //     but the prompt completed before the response landed.
                //     Delivery state is unknown — but the prompt completing
                //     means there is no in-flight turn to signal anymore.
                //     Release withheld for normal dispatch; do NOT fire
                //     the fallback signal (it would target a turn that
                //     just ended; normal dispatch already handles
                //     redelivery via the released queue entry).
                //
                //   Err(PromptCompleted)
                //     `SteerError::PromptCompleted` is returned synchronously
                //     by `pool::send_steer` when no task is in flight (handled
                //     in `try_native_steer`'s Err branch, which falls through
                //     to cancel+merge). It is never routed through the ack
                //     channel, so this variant never appears in `SteerAckEvent`.
                //
                //   Watcher Err (oneshot dropped)
                //     Should not happen — the read loop drains
                //     pending_steer on every return path. If it does,
                //     treat as PromptCompletedNeutral to avoid leaking
                //     the withheld event in `withheld_native_steer`.
                let (release_withheld, drop_withheld, signal_fallback) = match &ack {
                    Ok(pool::SteerAck::Success) => (false, true, false),
                    // -32601 = method_not_found: agent does not implement the
                    // steer extension. Fire cancel+merge so the message still
                    // reaches the agent.
                    Ok(pool::SteerAck::Err(pool::SteerError::AgentError { code, .. }))
                        if *code == -32601 =>
                    {
                        (true, false, true)
                    }
                    // AgentError: write landed, agent rejected it at the
                    // application level (e.g. wrong run id). Release for
                    // normal dispatch; no fallback signal (the turn is still
                    // running or just ended — either way there is nothing to
                    // cancel).
                    Ok(pool::SteerAck::Err(pool::SteerError::AgentError { .. })) => {
                        (true, false, false)
                    }
                    // Transport / ExpectedRunIdMissing / OutcomeRejected: the
                    // steer did not land. Release and fire the cancel+merge
                    // fallback so the message still reaches the agent.
                    Ok(pool::SteerAck::Err(_)) => (true, false, true),
                    Ok(pool::SteerAck::PromptCompletedNeutral) => (true, false, false),
                    Err(_recv_err) => (true, false, false),
                };
                tracing::info!(
                    channel = %channel_id,
                    event_id = %event_id,
                    ?ack,
                    release_withheld,
                    drop_withheld,
                    signal_fallback,
                    "non-cancelling steer ack received"
                );
                if matches!(ack, Ok(pool::SteerAck::Success)) {
                    queue.extend_in_flight_deadline(channel_id, config.max_turn_duration_secs);
                }
                if drop_withheld {
                    queue.remove_event(channel_id, &event_id);
                }
                if release_withheld {
                    queue.release_native_steer(channel_id, &event_id);
                }
                if signal_fallback {
                    // Universal cancel+merge fallback. Note: the
                    // queued event has already been released to the
                    // front of `queues[channel_id]`, so the cancel
                    // will pick it up as part of the merged batch and
                    // re-prompt the agent.
                    signal_in_flight_task(&mut pool, channel_id, ControlSignal::Steer);
                }
                // After releasing a withheld event, give dispatch a chance
                // to re-flush. If the prompt is still in flight, the
                // channel stays `in_flight_channels` and `flush_next`
                // skips it — but a Steer fallback signal sent above will
                // tear down the in-flight task; on its completion the
                // queue drains. We still try here in case the in-flight
                // task has already returned.
                for (channel_id, thread_tags) in dispatch_pending(&mut pool, &mut queue, &ctx) {
                    typing_channels.insert(channel_id, thread_tags);
                }
            }
            Some(PoolEvent::Wake(attempt, result)) => {
                let completion = result.as_ref().map(|_| ()).map_err(|error| error.clone());
                if let Err(error) =
                    pool_lifecycle.complete_wake(attempt, result, tokio::time::Instant::now())
                {
                    tracing::warn!(attempt, error, "discarding stale pool wake result");
                    continue;
                }
                match completion {
                    Ok(()) => {
                        pool = pool_lifecycle
                            .take_ready()
                            .expect("successful wake stores a ready pool");
                        pool_ready = true;
                        emit_runtime_lifecycle(
                            observer.as_ref(),
                            &runtime_start_nonce,
                            &pubkey_hex,
                            &config.relay_url,
                            "ready",
                            None,
                        );
                        for (channel_id, thread_tags) in
                            dispatch_pending(&mut pool, &mut queue, &ctx)
                        {
                            typing_channels.insert(channel_id, thread_tags);
                        }
                    }
                    Err(error) => {
                        debug_assert_eq!(pool_lifecycle.failed_error(), Some(error.as_str()));
                        emit_runtime_lifecycle(
                            observer.as_ref(),
                            &runtime_start_nonce,
                            &pubkey_hex,
                            &config.relay_url,
                            "failed",
                            Some(&error),
                        );
                    }
                }
            }
            None => {} // relay/heartbeat/shutdown branches handled inline above
        }
    }

    // Drain wake tasks gracefully rather than aborting: an in-flight
    // initialize_agent_pool observes the shutdown watch at its biased per-slot
    // select and reaps its partially-spawned agents itself. `shutdown()` here
    // would abort the task mid-init and drop those AcpClients via best-effort
    // Drop — the exact zombie class the eager path's spawn-outside-the-timeout
    // comment exists to prevent. Fire the watch first so exits that bypass the
    // signal handlers (result channel closed, LoopAction::Exit) cancel the wake
    // just as promptly. Timeout is a backstop for a slot stuck outside the
    // select (e.g. in spawn); only then do we fall back to aborting.
    let _ = shutdown_tx.send(());
    let wake_drain = tokio::time::timeout(Duration::from_secs(30), async {
        while wake_tasks.join_next().await.is_some() {}
    })
    .await;
    if wake_drain.is_err() {
        tracing::warn!("wake task did not drain within grace period — aborting");
        wake_tasks.shutdown().await;
    }
    while let Ok((_attempt, result)) = wake_rx.try_recv() {
        if let Ok(mut awakened_pool) = result {
            shutdown_agent_pool(&mut awakened_pool).await;
        }
    }

    tracing::info!("shutdown: waiting for in-flight prompts");
    // 30 s is generous for in-flight prompts to be cancelled; using
    // max_turn_duration here would cause Ctrl+C to hang for up to an hour.
    let grace = Duration::from_secs(30);
    // Best-effort drain of both join_set and result_rx during the grace period.
    // Tasks that finish normally send their OwnedAgent through result_rx — we
    // explicitly shut them down here to reap child processes. If the grace
    // period expires, remaining tasks are aborted and fall back to
    // AcpClient::Drop (start_kill + try_wait — best-effort, not guaranteed).
    let (rx_ref, js_ref) = pool.rx_and_join_set();
    let shutdown_result = tokio::time::timeout(grace, async {
        loop {
            tokio::select! {
                result = js_ref.join_next() => {
                    match result {
                        Some(Err(e)) => tracing::warn!("task error during shutdown: {e}"),
                        Some(Ok(())) => {}
                        None => break, // join_set empty
                    }
                }
                maybe_result = rx_ref.recv() => {
                    if let Some(mut pr) = maybe_result {
                        let idx = pr.agent.index;
                        let reap = pr.agent.acp.shutdown().await;
                        report_reap(idx, "checked-out", reap);
                    }
                    // If None, channel closed — tasks are done.
                }
            }
        }
    })
    .await;
    if shutdown_result.is_err() {
        tracing::warn!("grace period expired, aborting remaining tasks");
        pool.join_set.shutdown().await;
    }
    // Drain any remaining results that arrived after join_set drained but
    // before tasks were aborted.
    while let Ok(mut pr) = pool.result_rx_try_recv() {
        let idx = pr.agent.index;
        let reap = pr.agent.acp.shutdown().await;
        report_reap(idx, "late-arriving", reap);
    }
    // Explicitly shut down idle agents still sitting in their slots.
    for slot in pool.agents_mut().iter_mut() {
        if let Some(agent) = slot.take() {
            let idx = agent.index;
            let mut acp = agent.acp;
            let reap = acp.shutdown().await;
            report_reap(idx, "idle", reap);
        }
    }
    drop(pool);

    // Abort any in-flight respawn tasks. They may be sleeping in backoff or
    // running spawn_and_init — either way, we don't want them spawning new
    // children after the main loop has exited. RespawnGuard::Drop sends a
    // failure result for aborted tasks, so respawn_in_flight is cleared.
    respawn_tasks.shutdown().await;

    // Drain any respawn results that completed before the abort. Explicitly
    // shut down returned agents instead of relying on AcpClient::Drop.
    while let Ok(rr) = respawn_rx.try_recv() {
        if let Ok((mut acp, _, _)) = rr.result {
            let reap = acp.shutdown().await;
            report_reap(rr.index, "respawned", reap);
        }
    }

    // Cancel any in-flight presence heartbeat before sending offline.
    if let Some(h) = presence_task.take() {
        h.abort();
    }

    // Best-effort: set presence to offline before exiting.
    if config.presence_enabled {
        match tokio::time::timeout(
            Duration::from_secs(2),
            publish_presence(&presence_publisher, &presence_keys, "offline"),
        )
        .await
        {
            Ok(Ok(_)) => tracing::info!("presence set to offline"),
            Ok(Err(e)) => tracing::warn!("failed to set offline presence: {e}"),
            Err(_) => tracing::warn!("offline presence timed out"),
        }
    }

    if let Some(handle) = relay_observer_publisher_task.take() {
        handle.abort();
    }

    // Graceful relay shutdown — sends WebSocket close frame and waits up to 5s
    // for the background task to finish, rather than aborting immediately (#40).
    relay.shutdown().await;

    tracing::info!("buzz-acp stopped");
    Ok(())
}

/// Should a refusal be reported at all?
///
/// The transition always is — degradation must be visible. After that, only
/// powers of two, so a stream of refusals produces a logarithmic number of
/// records instead of one each.
///
/// An earlier version logged every subsequent refusal at `debug` and called
/// that bounded. It is not: a log level is a filter, not a rate limiter, and
/// turning diagnostics on to investigate the very flood in question would have
/// reopened the amplifier at exactly the wrong moment.
///
/// Split out as a pure function so the caller's behaviour is testable. Proving
/// the outcome type distinguishes transitions says nothing about whether the
/// caller acts on that distinction.
fn should_report_refusal(degradation: project::Degradation, refused_total: u64) -> bool {
    match degradation {
        project::Degradation::BecameDegraded => true,
        project::Degradation::AlreadyDegraded => refused_total.is_power_of_two(),
    }
}

/// The one relay capability project dispatch needs.
///
/// Deliberately not the relay handle. A handle can subscribe to channels,
/// unsubscribe, publish and reconnect; dispatch needs exactly one of those and
/// should not be able to reach the rest. It is also a *replacement* capability
/// rather than a subscribe one, because retirement is where the two
/// subscription defects lived and a method that can only add cannot express it.
/// The one capability startup needs: open a project subscription.
///
/// Separate from [`ProjectSubscriber`] because startup opens and never
/// replaces, and the driver replaces and never opens. Merging them would hand
/// each side a lever it has no business holding.
/// **Discovery only.** It took a `sub_id` and a `ProjectSubscription` until
/// that was found to be a second producer of watched generations, reachable by
/// any crate caller. Startup opens exactly one subscription and has no id or
/// class to choose; the background task supplies both.
pub(crate) trait ProjectOpener {
    fn submit_project_discovery(
        &self,
        filters: Vec<serde_json::Value>,
    ) -> impl std::future::Future<Output = Result<(), relay::RelayError>>;
}

impl ProjectOpener for relay::HarnessRelay {
    async fn submit_project_discovery(
        &self,
        filters: Vec<serde_json::Value>,
    ) -> Result<(), relay::RelayError> {
        relay::HarnessRelay::submit_project_discovery(self, filters).await
    }
}

/// Open the startup project subscriptions this configuration calls for.
///
/// **Extracted from `tokio_main` so the decision is reachable.** Inline, it was
/// provable only by running all of startup, so the control test that stood in
/// for it asserted on `project_req_frames` — a helper no production code calls.
/// A gate that had stopped working would not have shown up there.
///
/// Discovery is the one project subscription that depends on no prior state:
/// `kind:30617` announcements are what *produces* the discovered set, so it can
/// be opened at startup. Enrolment and watched-root REQs derive their filters
/// from discovery and enrolment state, so they belong to the driver.
///
/// Startup names neither the id nor the class — it submits filters, and the
/// relay task stamps both. Registration happens in lockstep with the write
/// there, so a failed send leaves nothing answerable.
pub(crate) async fn open_startup_project_subscriptions(
    config: &config::Config,
    opener: &impl ProjectOpener,
) {
    match project::discovery_subscription(config.project_routing_enabled) {
        Some(filters) => {
            if let Err(e) = opener.submit_project_discovery(filters).await {
                tracing::warn!("repository discovery subscribe error: {e}");
            } else {
                tracing::info!("submitted the repository-announcement subscription");
            }
        }
        None => {
            tracing::debug!("project routing disabled — no project subscriptions opened");
        }
    }
}

/// **Submission, not installation.** The future resolves when the background
/// task has accepted the command, which says nothing about whether a REQ was
/// written or a subscription installed. Implementors must not report anything
/// stronger, and callers must not read anything stronger into it.
pub(crate) trait ProjectSubscriber {
    fn submit_project_replacement(
        &self,
        replacement: project::ProjectReplacement,
        filters: Vec<serde_json::Value>,
    ) -> impl std::future::Future<Output = Result<(), relay::RelayError>>;
}

impl ProjectSubscriber for relay::HarnessRelay {
    async fn submit_project_replacement(
        &self,
        replacement: project::ProjectReplacement,
        filters: Vec<serde_json::Value>,
    ) -> Result<(), relay::RelayError> {
        relay::HarnessRelay::submit_project_replacement(self, replacement, filters).await
    }
}

/// Dispatch one project event **and bring subscriptions into line with it.**
///
/// Extracted from the run loop's `select!` so the loop and the connected
/// harness call the same function. Inline, it was unreachable: no test could
/// enter `run()`, so the code that decides *when* to issue a REQ had none — and
/// that is exactly where the enrolment-widening defect lived.
///
/// Dispatch decides; this performs the I/O the decision implies. The gate still
/// holds no relay capability of its own, so a refused event cannot reach a
/// subscription.
pub(crate) async fn dispatch_project_event(
    dispatch: &mut ProjectDispatch<'_>,
    subscriber: &impl ProjectSubscriber,
    agent_pubkey_hex: &str,
    since: u64,
    project_event: &project::ProjectEvent,
) -> ProjectDispatched {
    let dispatched = handle_project_event(dispatch, project_event);

    match dispatched {
        // A newly discovered coordinate widens the enrolment filter, and the
        // filter is derived from the discovered set — so the REQ is *replaced*,
        // not re-opened. Re-opening kept the first repository's identity
        // forever, because the id is fixed and opening refuses to change it.
        ProjectDispatched::DiscoveryChanged => {
            if let Some(filter) =
                project::enrolment_filter(dispatch.discovered, agent_pubkey_hex, since)
            {
                if let Err(e) = subscriber
                    .submit_project_replacement(
                        project::ProjectReplacement::Enrolment,
                        vec![filter],
                    )
                    .await
                {
                    tracing::warn!("enrolment replacement submission error: {e}");
                }
            }
        }
        // A root joined or rejoined the watched set. The watched REQ names every
        // root explicitly, so the successor takes a fresh generation and the
        // predecessor is retired once the successor is installed.
        ProjectDispatched::Enrolled
        | ProjectDispatched::Queued {
            watch_changed: true,
            ..
        } => {
            let filters = project::watched_roots_filters(dispatch.enrolments, since);
            if !filters.is_empty() {
                // **Nothing about the generation is decided here.**
                //
                // This used to allocate the generation, name the predecessor,
                // and advance its own counter when the submission returned
                // `Ok` — which meant only that the command had been enqueued.
                // A generation the registry never installed could therefore be
                // named as the next predecessor, and retiring that nonexistent
                // id left the genuine predecessor durable beside the successor.
                //
                // Checked allocation and fail-closed exhaustion still hold;
                // they hold in `ProjectRequests`, which is the only component
                // that knows what is installed.
                if let Err(e) = subscriber
                    .submit_project_replacement(project::ProjectReplacement::Watched, filters)
                    .await
                {
                    tracing::warn!("watched-roots replacement submission error: {e}");
                }
            }
        }
        _ => {}
    }

    dispatched
}

/// Everything the project branch is permitted to touch.
///
/// **What it does not carry is the point.** There is no
/// [`pool::ChannelInfoResolver`] here and no route to one, so a channel-info
/// or DM-policy lookup from the project path is a compile error rather than a
/// runtime lookup that fails closed. The project route key is a UUIDv5 of a
/// root; it names no channel, and every channel-shaped question asked of it
/// would answer from a default rather than from fact.
///
/// Asserting "the project path performs no channel lookup" in a test would
/// only describe today's code. Not holding the resolver is a property the next
/// edit inherits.
struct ProjectDispatch<'a> {
    identity: project::ProjectIdentity<'a>,
    discovered: &'a mut project::DiscoveredRepositories,
    enrolments: &'a mut project::ProjectEnrolments,
    queue: &'a mut queue::EventQueue,
    /// The NIP-OA attestation for *this event's* author, resolved before
    /// dispatch.
    ///
    /// Phase 1 passed `None` here and said so: the lookup is async, and a phase
    /// that admitted no trusted-agent wake gained nothing from an authority it
    /// refused to use. Phase 1b needs it, because "a trusted agent may call
    /// another" is exactly the grant that `None` withholds — without an
    /// attestation every sibling classifies as `Untrusted` and every peer call
    /// becomes untrusted context.
    ///
    /// Resolved in the async caller rather than here so `handle_project_event`
    /// stays synchronous and directly testable. The proof binds the author and
    /// owner it was computed for, so carrying it per event is not a way to
    /// reuse one lookup for a different author.
    sibling: Option<project::VerifiedSibling>,
    /// This process's NIP-PC ledger: which call ids it has admitted, and which
    /// of its own calls are still awaiting a result.
    ///
    /// Shared with the channel path rather than owned per surface. A call id is
    /// derived from its route, so two ledgers could not collide — but an agent
    /// that answered the same call twice because two halves of one process
    /// disagreed about having seen it is exactly the loop this phase exists to
    /// prevent, and one ledger is the cheapest way for them not to disagree.
    ledger: &'a mut peer_call::CallLedger,
}

/// A NIP-OA lookup that already happened, in the shape [`project::SiblingResolver`]
/// requires.
///
/// The trait is synchronous and the lookup is not, so the boolean is carried in
/// rather than fetched here. This is not a way to manufacture a proof: the
/// resolver answers `true` only for the exact `(author, owner)` pair the lookup
/// was performed for, so a caller holding one for author A cannot obtain a
/// `VerifiedSibling` for author B, and `attest` remains private to `project`.
struct ProjectSiblingLookup {
    author: String,
    owner: String,
    verified: bool,
}

impl project::SiblingResolver for ProjectSiblingLookup {
    fn is_same_owner_sibling(&self, author: &str, owner: &str) -> bool {
        self.verified
            && self.author.eq_ignore_ascii_case(author)
            && self.owner.eq_ignore_ascii_case(owner)
    }
}

/// Resolve the sibling attestation for a routed project event's author.
///
/// Returns `None` for discovery events (no author decision to make), when no
/// agent owner is configured (fail closed — an unowned agent has no siblings),
/// and for the owner itself, whose authority comes from being the owner and is
/// classified before any sibling check.
async fn attest_project_sibling(
    project_event: &project::ProjectEvent,
    agent_owner: Option<&str>,
    owner_cache: &OwnerCache,
    rest_client: &relay::RestClient,
) -> Option<project::VerifiedSibling> {
    use project::SiblingResolver as _;

    let project::ProjectEvent::Routed { event, .. } = project_event else {
        return None;
    };
    let owner = agent_owner?.to_ascii_lowercase();
    let author = event.author().to_ascii_lowercase();
    if author == owner {
        return None;
    }

    // `is_owner_or_sibling` also returns true for the owner, which is why the
    // owner is excluded above: a `true` reaching here means a genuine NIP-OA
    // same-owner sibling and nothing else.
    let verified = is_owner_or_sibling(&author, owner_cache, rest_client).await;
    ProjectSiblingLookup {
        author: author.clone(),
        owner: owner.clone(),
        verified,
    }
    .resolve(&author, &owner)
}

/// The prompt label an event is queued under.
///
/// A call and a result read very differently to the agent receiving them — one
/// is work to do, the other is an answer to work it asked for — so they are not
/// both flattened into `@mention`.
///
/// [`queue::PEER_CALL_PROMPT_TAG`] is load-bearing rather than decorative: the
/// prompt path reads it to decide that a turn owes a *result* instead of a
/// reply, and it is set only here, on the admission path, after the envelope was
/// parsed and addressed to this agent. Nothing else in the harness writes it.
fn peer_prompt_tag(kind: u32) -> &'static str {
    match kind {
        buzz_core::peer_call::KIND_PEER_CALL => queue::PEER_CALL_PROMPT_TAG,
        buzz_core::peer_call::KIND_PEER_CALL_RESULT => "@call-result",
        _ => "@mention",
    }
}

/// Record a call **this agent published** in the outstanding-call ledger.
///
/// The harness does not publish calls itself: the agent subprocess runs
/// `buzz agents call`, and the only place this process can learn that a call
/// exists is its own event coming back off the wire. Without this the ledger
/// would be empty and every returned result would correlate to nothing.
///
/// Registration is unconditional for a well-formed own call. The fan-out
/// ceiling is not enforced here and must not be: by the time an event returns
/// from the relay the callee may already be running the task, so refusing to
/// record it would discard the answer to work that happened anyway. The ceiling
/// runs in `buzz agents call` before publication.
fn register_outgoing_call(
    ledger: &mut peer_call::CallLedger,
    peer: &peer_call::VerifiedPeerEvent,
    agent_hex: &str,
) {
    if peer.kind() != buzz_core::peer_call::KIND_PEER_CALL {
        return;
    }
    let Ok(envelope) = peer_call::CallEnvelope::parse(peer) else {
        tracing::debug!(
            event_id = %peer.id(),
            "own kind:43001 event is not a well-formed call — not registered"
        );
        return;
    };
    if !envelope.caller().eq_ignore_ascii_case(agent_hex) {
        return;
    }
    ledger.register_outgoing(envelope.call_id(), envelope.callee(), envelope.route());
    tracing::debug!(
        call_id = %envelope.call_id(),
        callee = %envelope.callee(),
        outstanding_on_route = ledger.outstanding_on_route(envelope.route()),
        outstanding_total = ledger.outstanding_count(),
        "outgoing peer call registered"
    );
}

/// Map the project authority gate's verdict onto the peer-call trust classes.
///
/// The two enums stay separate because they answer different questions —
/// `ProjectAuthor` also decides enrolment, `PeerTrust` only decides invocation —
/// but there is exactly one gate, and this is the seam. Deriving trust a second
/// time from the same inputs would be a second place for the answer to differ
/// from the one the effect was chosen by.
fn peer_trust_of(author: project::ProjectAuthor) -> peer_call::PeerTrust {
    match author {
        project::ProjectAuthor::SelfAuthored => peer_call::PeerTrust::SelfAuthored,
        project::ProjectAuthor::AuthorisedHuman => peer_call::PeerTrust::Owner,
        project::ProjectAuthor::TrustedAgent => peer_call::PeerTrust::TrustedAgent,
        project::ProjectAuthor::Untrusted => peer_call::PeerTrust::Untrusted,
    }
}

/// Is this one of the two NIP-PC kinds?
fn is_peer_call_kind(kind: u32) -> bool {
    matches!(
        kind,
        buzz_core::peer_call::KIND_PEER_CALL | buzz_core::peer_call::KIND_PEER_CALL_RESULT
    )
}

/// What a channel-routed NIP-PC event resolves to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChannelPeerOutcome {
    /// Not a peer-call kind. The ordinary channel path owns this event.
    NotPeerCall,
    /// Handled without a turn: this agent's own call was registered, or the
    /// envelope was refused. Either way the channel path must not see it — a
    /// call is not a message and has no rule to match.
    Consumed,
    /// Queue a turn for this event on `channel_id`.
    Turn {
        channel_id: uuid::Uuid,
        prompt_tag: &'static str,
    },
}

/// Decide a channel-routed peer call or result.
///
/// Synchronous and total: trust is resolved by the caller (the NIP-OA lookup is
/// async) and handed in, so everything this function does is a function of the
/// event, this agent's identity and the ledger. That is what makes the loop
/// controls testable against the same code production runs, rather than against
/// a reconstruction of it.
fn decide_channel_peer_event(
    event: &nostr::Event,
    channel_id: uuid::Uuid,
    agent_hex: &str,
    trust: peer_call::PeerTrust,
    ledger: &mut peer_call::CallLedger,
) -> ChannelPeerOutcome {
    if !is_peer_call_kind(u32::from(event.kind.as_u16())) {
        return ChannelPeerOutcome::NotPeerCall;
    }

    // Verified here rather than trusted from the transport. The relay is not
    // an authority on who signed anything, and "the caller is the author" is
    // the one fact the whole envelope rests on.
    let Some(peer) = peer_call::VerifiedPeerEvent::verify(event.clone()) else {
        tracing::warn!(
            event_id = %event.id.to_hex(),
            "peer-call event failed signature verification — dropping"
        );
        return ChannelPeerOutcome::Consumed;
    };

    if trust == peer_call::PeerTrust::SelfAuthored || peer.author().eq_ignore_ascii_case(agent_hex)
    {
        register_outgoing_call(ledger, &peer, agent_hex);
        return ChannelPeerOutcome::Consumed;
    }

    match peer_call::call_marker(&peer, agent_hex) {
        // Malformed, or addressed to another agent. The peer-call REQ also
        // carries this agent's own authored calls, and a channel it shares with
        // two peers will show it traffic that is not its to answer.
        project::CallMarker::None => ChannelPeerOutcome::Consumed,

        project::CallMarker::Invocation => {
            let Ok(envelope) = peer_call::CallEnvelope::parse(&peer) else {
                return ChannelPeerOutcome::Consumed;
            };
            match peer_call::admit_call(envelope, agent_hex, trust, ledger) {
                Ok(call) => {
                    // The route the envelope declares must be the channel it
                    // was delivered on. They are derived from the same `h`, so
                    // a mismatch means the transport and the envelope disagree
                    // — and the envelope is the thing the result will be sent
                    // back against.
                    if call.session_key() != channel_id {
                        tracing::warn!(
                            call_id = %call.envelope().call_id(),
                            delivered_on = %channel_id,
                            declared = %call.session_key(),
                            "peer call route does not match its delivery channel — refusing"
                        );
                        return ChannelPeerOutcome::Consumed;
                    }
                    tracing::info!(
                        call_id = %call.envelope().call_id(),
                        caller = %call.envelope().caller(),
                        hop = call.envelope().hop(),
                        path = %call.envelope().visited().join(","),
                        channel_id = %channel_id,
                        "peer call admitted on a channel route"
                    );
                    ledger.record_admitted(&call);
                    ChannelPeerOutcome::Turn {
                        channel_id,
                        prompt_tag: peer_prompt_tag(buzz_core::peer_call::KIND_PEER_CALL),
                    }
                }
                Err(refusal) => {
                    tracing::info!(?refusal, "peer call refused by a loop control");
                    ChannelPeerOutcome::Consumed
                }
            }
        }

        project::CallMarker::Result => {
            let Ok(envelope) = peer_call::ResultEnvelope::parse(&peer) else {
                return ChannelPeerOutcome::Consumed;
            };
            match peer_call::admit_result(envelope, agent_hex, ledger) {
                Ok(result) => {
                    if result.session_key() != channel_id {
                        tracing::warn!(
                            call_id = %result.envelope().call_id(),
                            "peer call result route does not match its delivery channel — refusing"
                        );
                        return ChannelPeerOutcome::Consumed;
                    }
                    tracing::info!(
                        call_id = %result.envelope().call_id(),
                        callee = %result.envelope().callee(),
                        channel_id = %channel_id,
                        "peer call result correlated on a channel route"
                    );
                    ledger.record_answered(&result);
                    ChannelPeerOutcome::Turn {
                        channel_id,
                        prompt_tag: peer_prompt_tag(buzz_core::peer_call::KIND_PEER_CALL_RESULT),
                    }
                }
                Err(refusal) => {
                    tracing::info!(?refusal, "peer call result refused");
                    ChannelPeerOutcome::Consumed
                }
            }
        }
    }
}

/// What the NIP-PC loop controls decided about a project-routed event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PeerAdmission {
    /// Not a peer-call envelope addressed to this agent, or one the authority
    /// gate already declined. The project decision stands untouched.
    NotPeerCall,
    /// A call or result this agent accepted. The ledger has been written.
    Admitted,
    /// A peer-call envelope refused by a loop control. Nothing was written —
    /// a refused call must not consume the replay slot that would then refuse
    /// the honest retry.
    Refused,
}

/// Run the NIP-PC loop controls over a project-routed event.
///
/// The authority gate that ran before this decided *whether this author may
/// direct us*. It says nothing about whether this particular call has already
/// been answered, or whether it loops back to an agent already in its own path.
/// Those are properties of the call and of what this process has seen, so they
/// are checked here, against the ledger, before anything is queued.
///
/// Engaging only on effects the marker actually produced is deliberate: an
/// untrusted author's call is already `UntrustedContext` by the time it arrives
/// here, and turning that into a peer-call refusal would change what Phase 1
/// does with an ordinary untrusted comment.
fn admit_peer_call_event(
    dispatch: &mut ProjectDispatch<'_>,
    decision: &project::ProjectDecision,
    event: &project::VerifiedProjectEvent,
) -> PeerAdmission {
    let peer = peer_call::VerifiedPeerEvent::from_project(event);
    let agent_hex = dispatch.identity.agent.hex().to_ascii_lowercase();

    if peer.author().eq_ignore_ascii_case(&agent_hex) {
        register_outgoing_call(dispatch.ledger, &peer, &agent_hex);
        return PeerAdmission::NotPeerCall;
    }

    match peer_call::call_marker(&peer, &agent_hex) {
        project::CallMarker::None => PeerAdmission::NotPeerCall,

        project::CallMarker::Invocation => {
            if !matches!(
                decision.effect,
                project::ProjectEffect::Wake | project::ProjectEffect::EnrolAndWake
            ) {
                return PeerAdmission::NotPeerCall;
            }
            // The marker is `Invocation` only because the envelope already
            // parsed and named this agent, so this arm is unreachable; it
            // refuses rather than unwrapping because "unreachable" is a claim
            // about two functions agreeing, and the safe answer if they ever
            // stop agreeing is no turn.
            let Ok(envelope) = peer_call::CallEnvelope::parse(&peer) else {
                return PeerAdmission::Refused;
            };
            match peer_call::admit_call(
                envelope,
                &agent_hex,
                peer_trust_of(decision.author),
                dispatch.ledger,
            ) {
                Ok(call) => {
                    tracing::info!(
                        call_id = %call.envelope().call_id(),
                        caller = %call.envelope().caller(),
                        hop = call.envelope().hop(),
                        // The path is the diagnostic that makes a later
                        // `Revisit` refusal readable: without it the refusal
                        // names a rule but not the chain that tripped it.
                        path = %call.envelope().visited().join(","),
                        "peer call admitted on a project route"
                    );
                    dispatch.ledger.record_admitted(&call);
                    PeerAdmission::Admitted
                }
                Err(refusal) => {
                    tracing::info!(?refusal, "peer call refused by a loop control");
                    PeerAdmission::Refused
                }
            }
        }

        project::CallMarker::Result => {
            if !matches!(decision.effect, project::ProjectEffect::ResumeCall) {
                return PeerAdmission::NotPeerCall;
            }
            let Ok(envelope) = peer_call::ResultEnvelope::parse(&peer) else {
                return PeerAdmission::Refused;
            };
            match peer_call::admit_result(envelope, &agent_hex, dispatch.ledger) {
                Ok(result) => {
                    tracing::info!(
                        call_id = %result.envelope().call_id(),
                        callee = %result.envelope().callee(),
                        "peer call result correlated on a project route"
                    );
                    dispatch.ledger.record_answered(&result);
                    PeerAdmission::Admitted
                }
                Err(refusal) => {
                    tracing::info!(?refusal, "peer call result refused");
                    PeerAdmission::Refused
                }
            }
        }
    }
}

/// What dispatch did, for the caller to act on and for tests to observe.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ProjectDispatched {
    /// Nothing happened. Refusals land here, and they spend nothing.
    Ignored,
    /// Discovery state changed; the enrolment filter may now be wider.
    DiscoveryChanged,
    /// Discovery was seen and changed nothing.
    DiscoveryUnchanged,
    /// The root is now watched. No turn was run.
    Enrolled,
    /// Queued under the root's route key. `queued` is false when the queue's
    /// own dedup refused it, which is not a refusal by this gate.
    ///
    /// `watch_changed` says whether the watched-root set actually gained or
    /// reactivated a root, which is the only thing that warrants replacing the
    /// watched-root REQ. A re-mention of a root already active reports
    /// `EnrolOutcome::Unchanged` and must not churn the subscription.
    Queued {
        key: uuid::Uuid,
        queued: bool,
        watch_changed: bool,
    },
}

/// Project dispatch entry point.
///
/// **Both arms do work.** Discovery updates the known-repository set; routed
/// events go through the authority gate and, when authorised, enrol a root and
/// queue a turn. See the two sections below for what each of those costs.
///
/// Ingesting an announcement grants no **authority**: it adds a coordinate to a
/// set and nothing else — no session woken, no model turn, no invocation right.
/// The coordinate is derived from the announcement's *signer*, so a flood of
/// valid announcements cannot make anyone an owner they are not, and enrolment
/// still has to match a root's own signed `a` against the set.
///
/// It is **not** free, though. An earlier version of this comment said the
/// worst case was "a set containing repositories nobody cares about", which
/// treated valid hostile input as harmless because it lacked authority. The
/// allocator is less philosophical: the discovery REQ is global, so every
/// distinct announcement anyone publishes is a real coordinate held in memory.
/// That is bounded by [`project::DISCOVERY_CEILING`], which refuses rather than
/// evicts and marks the set permanently incomplete when it trips.
///
/// The set is **not durable**. It lives in this run loop and is gone on
/// restart. That is deliberate — the plan rejects a second authoritative local
/// database — but it means restart recovery is relay-derived reconstruction,
/// which does not exist yet: there is no paginated discovery reconstruction and
/// no recovery for announcements dropped under backpressure.
///
/// **Neither catch-up frames nor end-of-backlog boundaries arrive here.** Both
/// are handled in the relay task, beside the registry that admitted them and
/// the reconstructions that hold pages. This arm used to receive catch-up frames
/// as ordinary routed events and drop them, leaving the page short with nothing
/// to say so; it also used to receive the boundary itself, as a capability this
/// side could not use and did not consume.
///
/// **Routed events are dispatched, not dropped.** The authority gate composes
/// [`project::classify_kind`], [`project::classify_project_author`],
/// [`project::resolve_addressing`] and [`project::classify_project_event`]
/// against live runtime state in [`project::decide_project_event`]; an
/// authorised effect enrols the root and queues a turn under
/// [`project::ProjectRoute::key`].
///
/// An earlier revision of this comment described the opposite — an unbuilt
/// gate, discarded events, and a tree unfit to commit. Each clause stopped
/// being true when the gate landed, and the comment outlived all three. That
/// is worse than no comment: a reader who trusts it concludes the feature does
/// not exist. The false text is deliberately not quoted here, so a search for
/// it finds nothing.
fn handle_project_event(
    dispatch: &mut ProjectDispatch<'_>,
    project_event: &project::ProjectEvent,
) -> ProjectDispatched {
    let discovered = &mut *dispatch.discovered;
    match project_event {
        project::ProjectEvent::Discovery { announcement } => {
            match discovered.ingest(announcement) {
                project::Discovered::Added(coordinate) => {
                    tracing::info!(
                        coordinate,
                        known = discovered.len(),
                        "discovered repository"
                    );
                    ProjectDispatched::DiscoveryChanged
                }
                project::Discovered::AlreadyKnown(coordinate) => {
                    tracing::debug!(coordinate, "repository already discovered");
                    ProjectDispatched::DiscoveryUnchanged
                }
                project::Discovered::Refused {
                    because,
                    degradation,
                } => {
                    // Visible and fail-closed, but said **once**.
                    //
                    // The first version logged the refused coordinate at
                    // `warn` on every refusal. That bounded the heap and left
                    // the log unbounded, with the publisher choosing the
                    // contents of both — a coordinate carries an
                    // attacker-chosen `d` of arbitrary size. The refusal
                    // outcome deliberately carries no coordinate, so there is
                    // nothing here to leak even by accident; what is reported
                    // is the reason, the ceilings, and a running count.
                    let refused_total = discovered.refused_count();
                    if !should_report_refusal(degradation, refused_total) {
                        return ProjectDispatched::DiscoveryUnchanged;
                    }
                    match degradation {
                        project::Degradation::BecameDegraded => tracing::warn!(
                            ?because,
                            count_ceiling = project::DISCOVERY_CEILING,
                            byte_ceiling = project::DISCOVERY_RETAINED_BYTES,
                            retained = discovered.len(),
                            retained_bytes = discovered.retained_bytes(),
                            "repository discovery is now refusing announcements — the discovered \
                             set is permanently incomplete and must not be treated as a complete \
                             enrolment filter"
                        ),
                        project::Degradation::AlreadyDegraded => tracing::debug!(
                            ?because,
                            refused_total,
                            "repository discovery is still refusing announcements"
                        ),
                    }
                    ProjectDispatched::DiscoveryUnchanged
                }
            }
        }
        project::ProjectEvent::Routed {
            source,
            route,
            event,
        } => {
            // Phase A holds no reconstructed history, so readiness is `Unknown`
            // and stays honest about it. `resolve_addressing` reads that
            // conservatively, which is the intended direction to be wrong in.
            //
            // The sibling attestation was resolved by the async caller for this
            // event's author. It is what makes a NIP-PC call from a same-owner
            // sibling classify as `TrustedAgent` instead of `Untrusted`.
            let decision = project::decide_project_event(
                source,
                route,
                event,
                dispatch.identity,
                project::ProjectState {
                    discovered: dispatch.discovered,
                    enrolments: dispatch.enrolments,
                    readiness: &project::RootHistoryReadiness::Unknown,
                    sibling: dispatch.sibling.as_ref(),
                },
            );

            // ── NIP-PC loop controls ─────────────────────────────────────────
            //
            // The authority gate above decided *whether this author may direct
            // us*. It says nothing about whether this particular call is one we
            // have already answered, or whether it loops back to an agent
            // already in its own path. Those are properties of the call and of
            // what this process has seen, so they are checked here, against the
            // ledger, before anything is queued or recorded.
            //
            // Refusals return `Ignored` and write nothing: a refused call must
            // not consume the replay slot that would then refuse the honest
            // retry.
            let peer_admission = admit_peer_call_event(dispatch, &decision, event);
            if matches!(peer_admission, PeerAdmission::Refused) {
                return ProjectDispatched::Ignored;
            }

            let Some(origin) = decision.origin.clone() else {
                tracing::debug!(
                    ?source,
                    root = %route.root(),
                    kind = event.kind(),
                    effect = ?decision.effect,
                    "project event refused — nothing enrolled, queued or spent"
                );
                return ProjectDispatched::Ignored;
            };

            let mut watch_changed = false;

            // Enrolment happens before any queue insertion. A wake whose
            // enrolment was refused must not be queued: the binding refusal is
            // the whole authority for watching this root, and queueing anyway
            // would run a turn on a root we declined to watch.
            if matches!(
                decision.effect,
                project::ProjectEffect::Enrol | project::ProjectEffect::EnrolAndWake
            ) {
                let candidate = project::validate_enrolment_candidate(event, dispatch.discovered);
                let Some(candidate) = candidate else {
                    return ProjectDispatched::Ignored;
                };
                match dispatch.enrolments.enrol(&candidate) {
                    // Only a genuine join or rejoin warrants replacing the
                    // watched-root REQ. A re-mention of an already-active root
                    // reports `Unchanged`, and churning the subscription for it
                    // would replace a live request with an identical one.
                    Ok(outcome) => {
                        watch_changed = !matches!(outcome, project::EnrolOutcome::Unchanged);
                    }
                    Err(mismatch) => {
                        tracing::warn!(
                            root = %route.root(),
                            ?mismatch,
                            "refusing to move an enrolled root to a different binding"
                        );
                        return ProjectDispatched::Ignored;
                    }
                }
            }

            match decision.effect {
                project::ProjectEffect::Enrol => {
                    tracing::info!(
                        root = %route.root(),
                        coordinate = origin.coordinate(),
                        "project root enrolled without a turn"
                    );
                    ProjectDispatched::Enrolled
                }
                // `ResumeCall` queues beside the wakes. A correlated result is
                // the answer to work this agent asked for, so it must reach the
                // session that asked — leaving it in the "not implemented"
                // arm below meant a result was admitted by the ledger and then
                // silently dropped, and the caller's outstanding call closed
                // without the agent ever seeing the answer.
                project::ProjectEffect::EnrolAndWake
                | project::ProjectEffect::Wake
                | project::ProjectEffect::ResumeCall => {
                    let key = route.key();
                    let queued = dispatch.queue.push(queue::QueuedEvent {
                        channel_id: key,
                        event: event.event().clone(),
                        received_at: std::time::Instant::now(),
                        prompt_tag: peer_prompt_tag(event.kind()).into(),
                        project: Some(origin.clone()),
                    });
                    tracing::info!(
                        root = %route.root(),
                        coordinate = origin.coordinate(),
                        class = origin.class_noun(),
                        key = %key,
                        queued,
                        "project event queued for a turn"
                    );
                    ProjectDispatched::Queued {
                        key,
                        queued,
                        watch_changed,
                    }
                }
                // Everything else is state or context, not a turn. Phase A
                // implements neither lifecycle application nor stored context,
                // so these are recorded and dropped rather than silently
                // treated as a wake.
                other => {
                    tracing::debug!(
                        root = %route.root(),
                        effect = ?other,
                        "project effect not implemented in this phase — no turn"
                    );
                    ProjectDispatched::Ignored
                }
            }
        }
    }
}

/// A channel event destructured out of [`relay::BuzzEvent`].
///
/// The main loop's channel path predates the route enum and reads
/// `buzz_event.channel_id` / `buzz_event.event` in three dozen places. Rebinding
/// into this shape keeps that body untouched, so the enum split is a routing
/// change rather than a rewrite of working delivery logic.
struct ChannelEvent {
    channel_id: uuid::Uuid,
    event: nostr::Event,
}

#[derive(PartialEq)]
enum LoopAction {
    Continue,
    Exit,
}

fn event_mentions_agent(event: &nostr::Event, agent_pubkey_hex: &str) -> bool {
    event.tags.iter().any(|t| {
        t.as_slice().first().map(|s| s.as_str()) == Some("p")
            && t.as_slice().get(1).map(|s| s.as_str()) == Some(agent_pubkey_hex)
    })
}

fn is_owner_control_command(
    event: &nostr::Event,
    kind_u32: u32,
    command: &str,
    agent_pubkey_hex: &str,
) -> bool {
    kind_u32 == KIND_STREAM_MESSAGE
        && event.content.trim() == command
        && event_mentions_agent(event, agent_pubkey_hex)
}

// ── signal_in_flight_task ─────────────────────────────────────────────────────

/// Decide which [`ControlSignal`] (if any) to send to an in-flight turn when a
/// new, already-author-gated event arrives for that channel.
///
/// Returns `None` to leave the in-flight turn untouched (the event waits in the
/// queue and is delivered when the turn completes). Author eligibility — owner
/// ∪ allowlist ∪ siblings — is enforced upstream by the inbound author gate, so
/// `Steer`/`Interrupt` apply to every event that reaches this point; only
/// `OwnerInterrupt` re-checks authorship (owner-only) here.
///
/// `owner` is the resolved owner pubkey hex, if known.
fn mode_gate_signal(
    handling: MultipleEventHandling,
    author_hex: &str,
    owner: Option<&str>,
) -> Option<ControlSignal> {
    match handling {
        MultipleEventHandling::Queue => None,
        MultipleEventHandling::Steer => Some(ControlSignal::Steer),
        MultipleEventHandling::Interrupt => Some(ControlSignal::Interrupt),
        MultipleEventHandling::OwnerInterrupt => match owner {
            Some(o) if author_hex == o => Some(ControlSignal::Interrupt),
            _ => None,
        },
    }
}

/// Send a control signal to the in-flight task for `channel_id`.
/// Returns `true` if a signal was sent, `false` if no in-flight task was found.
fn signal_in_flight_task(
    pool: &mut AgentPool,
    channel_id: uuid::Uuid,
    mode: ControlSignal,
) -> bool {
    let entry = pool
        .task_map_mut()
        .values_mut()
        .find(|m| m.channel_id == Some(channel_id));

    if let Some(meta) = entry {
        if let Some(tx) = meta.control_tx.take() {
            tracing::info!(channel = %channel_id, ?mode, "control signal sent to in-flight task");
            let _ = tx.send(mode);
            return true;
        }
    }
    false
}

/// Attempt the non-cancelling (ACP) steer for a freshly-queued event.
///
/// Caller invariants:
/// - `event` has already been pushed into `EventQueue::queues[channel_id]`
///   via [`EventQueue::push`] — its `event.id` must still be locatable
///   there so [`EventQueue::mark_native_steer_pending`] can move it to the
///   side table.
/// - `multiple_event_handling` resolved to `ControlSignal::Steer`; this
///   function is the non-cancelling fork of that signal.
///
/// Returns `true` if the native attempt was accepted by the read loop
/// (capacity-1 mpsc `try_send` succeeded, event withheld synchronously,
/// ack watcher spawned). On `true` the caller MUST NOT issue the
/// universal cancel+merge `ControlSignal::Steer` fallback — the watcher
/// will issue it from the ack arm if the native attempt fails.
///
/// Returns `false` if `pool.send_steer` failed (no in-flight task,
/// `steer_tx` already full from a prior in-flight steer, or read loop
/// torn down). The caller MUST fall through to
/// `signal_in_flight_task(channel_id, ControlSignal::Steer)` so the
/// event still reaches the agent via the universal path.
///
/// The withheld event is NOT released here on `false` because no withhold
/// was established: `mark_native_steer_pending` only runs on `Ok(())`.
fn try_native_steer(
    pool: &mut AgentPool,
    queue: &mut EventQueue,
    channel_id: uuid::Uuid,
    event: nostr::Event,
    prompt_tag: String,
    steer_ack_tx: &mpsc::UnboundedSender<SteerAckEvent>,
) -> bool {
    // Build the steer body: framing strings come from
    // `queue::native_steer_framing()` (Eva's drift-proof requirement —
    // native and cancel+merge fallback share these so the agent gets the
    // same orientation regardless of transport). The single event block
    // is rendered by `queue::format_event_block`, the same function
    // `queue::format_prompt` uses internally for `[Buzz event: …]`
    // sections, so the rendering also cannot drift.
    //
    // Passing `None` for `channel_info` / `profile_lookup` is intentional:
    // native steer is a *delta* into a live turn — the agent already saw
    // channel context and the actor's profile in the original prompt,
    // duplicating it here would defeat the point of non-cancelling
    // steering (which is to inject only what's new).
    let (header, closing) = queue::native_steer_framing();
    let event_id_hex = event.id.to_hex();
    let be = queue::BatchEvent {
        event,
        prompt_tag: prompt_tag.clone(),
        received_at: std::time::Instant::now(),
        // Native steer is reached only from the channel arm, which never
        // carries a project origin. A project mid-turn signal would need this
        // populated; Phase A does not route one here.
        project: None,
    };
    let event_block = queue::format_event_block(channel_id, None, &be, None);
    let body = format!("{header}\n\n[Buzz event: {prompt_tag}]\n{event_block}\n\n{closing}");

    let (ack_tx, ack_rx) = tokio::sync::oneshot::channel::<pool::SteerAck>();
    let request = pool::SteerRequest {
        prompt_blocks: vec![body],
        ack_tx,
    };

    match pool.send_steer(channel_id, request) {
        Ok(()) => {
            // Withhold the queued event synchronously BEFORE spawning
            // the watcher: this closes the race where `mark_complete`
            // clears `in_flight_channels` and a stray `flush_next` could
            // re-deliver the event via normal dispatch. See
            // `EventQueue::mark_native_steer_pending` docs at queue.rs:606.
            let withheld = queue.mark_native_steer_pending(channel_id, &event_id_hex);
            if !withheld {
                // Race: the event was already drained out of the queue
                // before we got here (e.g. a concurrent flush picked it
                // up). The steer is on the wire; if it succeeds the
                // agent gets it via the native path AND normal
                // dispatch — duplicate delivery is benign (agent gets
                // the same message twice). Log so this is visible if it
                // ever happens in production.
                tracing::warn!(
                    channel = %channel_id,
                    event_id = %event_id_hex,
                    "native steer accepted by read loop but event was not in queue to withhold \
                     — possible duplicate delivery if steer succeeds"
                );
            }
            let ack_tx_clone = steer_ack_tx.clone();
            let event_id_for_watcher = event_id_hex.clone();
            tokio::spawn(async move {
                let ack = ack_rx.await;
                let _ = ack_tx_clone.send(SteerAckEvent {
                    channel_id,
                    event_id: event_id_for_watcher,
                    ack,
                });
            });
            true
        }
        Err(e) => {
            tracing::info!(
                channel = %channel_id,
                error = ?e,
                "non-cancelling steer not accepted — falling back to cancel+merge"
            );
            false
        }
    }
}

// ── dispatch_pending ──────────────────────────────────────────────────────────

/// Flush queued work to available agents.
fn dispatch_pending(
    pool: &mut AgentPool,
    queue: &mut EventQueue,
    ctx: &Arc<PromptContext>,
) -> Vec<(Uuid, ThreadTags)> {
    let mut dispatched_channels = Vec::new();
    loop {
        let batch = match queue.flush_next() {
            Some(b) => b,
            None => break,
        };
        let channel_id = batch.channel_id;
        let typing_scope = batch
            .events
            .last()
            .map(|event| queue::parse_thread_tags(&event.event))
            .unwrap_or_default();
        let affinity_hit = pool.has_session_for(channel_id);
        let mut agent = match pool.try_claim(Some(channel_id)) {
            Some(a) => a,
            None => {
                let pending = queue.pending_channels();
                tracing::debug!(pending_channels = pending, "pool_exhausted");
                queue.requeue_preserve_timestamps(batch);
                queue.mark_complete(channel_id);
                break;
            }
        };
        tracing::debug!(agent = agent.index, channel = %channel_id, affinity_hit, "agent_claimed");

        let recoverable_batch = match ctx.dedup_mode {
            DedupMode::Queue => Some(batch.clone()),
            DedupMode::Drop => None,
        };

        let result_tx = pool.result_tx();
        let ctx_clone = Arc::clone(ctx);
        let agent_index = agent.index;

        // Mid-turn non-cancelling steer seam: install the per-turn steer
        // receiver on the read loop so the main loop's mode-gate fork
        // (see the `if accepted && queue.is_channel_in_flight(...)` block
        // in the relay event branch of the main `select!` loop) can drive
        // it via the matching sender stored in `TaskMeta.steer_tx`.
        // Installed for every prompt task: the read loop picks the steer
        // transport at write time from `active_run_id` and the agent's
        // advertised `_session/steering` capability, and acks
        // `ExpectedRunIdMissing` (→ cancel+merge) when it has neither.
        let (tx, rx) = tokio::sync::mpsc::channel::<pool::SteerRequest>(1);
        agent.acp.install_steer_rx(rx);
        let steer_tx = Some(tx);

        // Prompt text is now built inside run_prompt_task (needs async for
        // context fetching). Pass None for prompt_text; batch carries the data.
        let (control_tx, control_rx) = tokio::sync::oneshot::channel::<ControlSignal>();
        let turn_id = Uuid::new_v4().to_string();
        let task_turn_id = turn_id.clone();

        let abort_handle = pool.join_set.spawn(async move {
            pool::run_prompt_task(
                agent,
                Some(batch),
                None,
                ctx_clone,
                result_tx,
                Some(control_rx),
                task_turn_id,
            )
            .await;
        });

        pool.task_map_mut().insert(
            abort_handle.id(),
            pool::TaskMeta {
                agent_index,
                channel_id: Some(channel_id),
                turn_id,
                recoverable_batch,
                control_tx: Some(control_tx),
                steer_tx,
            },
        );
        dispatched_channels.push((channel_id, typing_scope));
    }
    tracing::debug!(
        dispatched = dispatched_channels.len(),
        queue_depth = queue.pending_channels(),
        "dispatch_pending"
    );
    dispatched_channels
}

/// Returns `true` when `error` is a non-retryable authentication failure.
///
/// Retrying auth errors is harmful: the token won't self-repair between
/// attempts, so each retry wastes an attempt slot, delays the visible failure,
/// and burns the user's context window. Dead-letter immediately and surface a
/// re-authentication hint instead.
///
/// # Classification rationale
///
/// Auth failures arrive as [`acp::AcpError::AgentError`] with a message
/// surfaced from the upstream CLI. Two narrow patterns reliably identify
/// non-transient auth failures observed in the field:
///
/// - `"Re-authenticate"` — emitted by the Claude CLI when an OAuth token has
///   expired ("OAuth access token has expired. Re-authenticate to continue.").
///   Specific to the auth-expiry flow; does not appear in unrelated errors.
/// - `"API Error: 401"` — present in Claude/Codex HTTP-401 responses; 401 is
///   the standard auth-failure status and does not arise from network blips.
///
/// False positives (misclassifying a transient error as non-retryable) silently
/// drop a user message, which is worse than a false negative (extra retries on
/// an auth error). Both patterns are therefore chosen for high precision.
fn is_auth_error(error: &acp::AcpError) -> bool {
    let acp::AcpError::AgentError { message, .. } = error else {
        return false;
    };
    message.contains("Re-authenticate") || message.contains("API Error: 401")
}

/// Spawn a task that posts a user-visible failure notice to the relay.
///
/// Shared by the hard-cap immediate dead-letter path and the retries-exhausted
/// dead-letter path so neither duplicates the tokio::spawn block.
fn spawn_failure_notice(
    rest_client: Option<&relay::RestClient>,
    batch: &FlushBatch,
    content: String,
) {
    if let Some(rest) = rest_client {
        let thread_tags = batch
            .events
            .last()
            .map(|be| queue::parse_thread_tags(&be.event))
            .unwrap_or_default();
        let rest = rest.clone();
        let channel_id = batch.channel_id;
        tokio::spawn(async move {
            pool::post_failure_notice(&rest, channel_id, &thread_tags, &content).await;
        });
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_prompt_result(
    pool: &mut AgentPool,
    queue: &mut EventQueue,
    config: &Config,
    mut result: PromptResult,
    heartbeat_in_flight: &mut bool,
    removed_channels: &HashSet<Uuid>,
    crash_history: &mut [SlotCircuit],
    respawn_tx: &mpsc::Sender<RespawnResult>,
    respawn_tasks: &mut tokio::task::JoinSet<()>,
    observer: Option<observer::ObserverHandle>,
    rest_client: Option<&relay::RestClient>,
) -> LoopAction {
    let before = pool.task_map().len();
    let agent_index = result.agent.index;
    pool.task_map_mut()
        .retain(|_, meta| meta.agent_index != agent_index);
    debug_assert_eq!(before, pool.task_map().len() + 1);

    // The hard-timeout death_message (below) must describe the batch's
    // *actual* fate, not just the `recently_active` eligibility flag — a
    // recently-active batch that exhausts the retry budget in queue.requeue()
    // is dead-lettered same as an immediate one, and both differ from a
    // channel-removed drop or a heartbeat call with no batch at all. Each
    // branch below records what actually happened; only the hard-timeout
    // match arm in the death_message construction reads it.
    let mut hard_timeout_fate_suffix: Option<&'static str> = None;

    // Requeue BEFORE mark_complete: requeue() sets retry_after with a future
    // deadline, and mark_complete() checks for it to decide whether to preserve
    // retry_counts. If mark_complete runs first, retry_counts is cleared and
    // every retry starts at attempt 1 — defeating exponential backoff and
    // dead-letter protection.
    if let Some(batch) = result.batch.take() {
        // Don't requeue batches for channels the agent was removed from —
        // those events are stale and should be silently dropped.
        if !removed_channels.contains(&batch.channel_id) {
            if matches!(
                result.outcome,
                PromptOutcome::Cancelled | PromptOutcome::CancelDrainTimeout(_)
            ) {
                // Cancel re-prompt: store as cancelled events so flush_next()
                // merges them into the next FlushBatch.cancelled_events,
                // enabling the annotated merged-prompt format. The batch's
                // cancel_reason (set by the pool task per the control signal)
                // selects steer vs interrupt framing. It is always set on this
                // path; if somehow unset, fall back to the gentler Steer framing
                // — consistent with MergeFraming::for_reason(None) and the
                // system default — rather than telling the agent to supersede.
                //
                // CancelDrainTimeout shares this path with Cancelled: a failed
                // 5s drain after a control-signal cancel is a cleanup-deadline
                // problem, not the deterministic hard-cap death below — the
                // original batch must survive with no retry/dead-letter
                // accounting, same as a clean cancel.
                let reason = batch.cancel_reason.unwrap_or(CancelReason::Steer);
                queue.requeue_as_cancelled(batch, reason);
            } else if matches!(
                result.outcome,
                PromptOutcome::Timeout(TimeoutKind::Hard {
                    recently_active: false
                })
            ) {
                tracing::error!(
                    channel_id = %batch.channel_id,
                    events = batch.events.len(),
                    "dead-lettering batch after hard-cap timeout (no recent activity) — discarding {} events",
                    batch.events.len(),
                );
                let content = format!(
                    "⚠️ I couldn't process the last request (the turn exceeded the maximum duration ({}s)). Please re-send if it's still needed.",
                    config.max_turn_duration_secs
                );
                spawn_failure_notice(rest_client, &batch, content);
                hard_timeout_fate_suffix = Some(" — dead-lettered (no recent activity)");
            } else if matches!(
                result.outcome,
                PromptOutcome::Timeout(TimeoutKind::Hard {
                    recently_active: true
                })
            ) {
                tracing::warn!(
                    channel_id = %batch.channel_id,
                    events = batch.events.len(),
                    "hard-cap timeout with recent activity — requeueing for retry"
                );
                if let Some(dead) = queue.requeue(batch) {
                    let content = format!(
                        "⚠️ I couldn't process the last request after multiple retries (the turn exceeded the maximum duration ({}s)). Please re-send if it's still needed.",
                        config.max_turn_duration_secs
                    );
                    spawn_failure_notice(rest_client, &dead, content);
                    hard_timeout_fate_suffix = Some(" — dead-lettered (retry budget exhausted)");
                } else {
                    hard_timeout_fate_suffix = Some(" — requeued for retry (recently active)");
                }
            } else if matches!(&result.outcome, PromptOutcome::Error(e) if is_auth_error(e)) {
                // Auth errors are non-retryable: the token won't self-repair
                // between retries, so requeueing only wastes attempt slots and
                // delays the visible failure. Dead-letter immediately and tell
                // the user to re-authenticate the CLI.
                tracing::warn!(
                    channel_id = %batch.channel_id,
                    events = batch.events.len(),
                    "dead-lettering batch immediately — non-retryable auth error"
                );
                let content = "⚠️ I couldn't process the last request: authentication failed. \
                    Please re-authenticate the CLI (e.g. run `claude /login` or `codex login`) \
                    and then re-send."
                    .to_string();
                spawn_failure_notice(rest_client, &batch, content);
            } else if let Some(dead) = queue.requeue(batch) {
                let reason = match &result.outcome {
                    PromptOutcome::Timeout(TimeoutKind::Idle) => "the turn timed out".to_string(),
                    PromptOutcome::Timeout(TimeoutKind::Hard { .. }) => {
                        "the turn exceeded the maximum duration".to_string()
                    }
                    PromptOutcome::AgentExited => "the agent process exited".to_string(),
                    PromptOutcome::Error(e) => format!("{e}"),
                    _ => "repeated failures".to_string(),
                };
                let content = format!(
                    "⚠️ I couldn't process the last request after multiple retries ({reason}). Please re-send if it's still needed."
                );
                spawn_failure_notice(rest_client, &dead, content);
            }
        } else {
            tracing::debug!(
                channel_id = %batch.channel_id,
                events = batch.events.len(),
                "dropping failed batch for removed channel"
            );
            hard_timeout_fate_suffix = Some(" — batch dropped (channel removed)");
        }
    }

    match &result.source {
        PromptSource::Channel(ch) => queue.mark_complete(*ch),
        PromptSource::Heartbeat => *heartbeat_in_flight = false,
    }

    // Strip sessions for channels the agent was removed from while this
    // agent was checked out. This covers the gap where invalidate_channel_sessions
    // only touches idle agents.
    for ch in removed_channels {
        result.agent.state.invalidate_channel(ch);
    }

    let outcome_label = match &result.outcome {
        PromptOutcome::Ok(_) => "ok",
        PromptOutcome::Error(_) => "error",
        PromptOutcome::Timeout(TimeoutKind::Idle) => "idle_timeout",
        PromptOutcome::Timeout(TimeoutKind::Hard { .. }) => "hard_timeout",
        PromptOutcome::AgentExited => "exited",
        PromptOutcome::Cancelled => "cancelled",
        PromptOutcome::CancelDrainTimeout(_) => "cancel_drain_timeout",
    };
    let agent_index = result.agent.index;
    // Capture the spawn-time configured model and our PID before the agent is
    // moved into match arms below. `desired_model` reflects the config/persona
    // model at spawn time — it does NOT reflect `session/set_model` overrides,
    // which live in buzz-agent's session state and are what `llm: (model) …`
    // errors carry. The two can legitimately differ; `configured_model=` is
    // still valuable for identifying a stale orphan running an old model.
    let harness_configured_model = result
        .agent
        .desired_model
        .as_deref()
        .unwrap_or("<none>")
        .to_string();
    let harness_pid = std::process::id();

    let channel_id = match &result.source {
        PromptSource::Channel(ch) => Some(*ch),
        PromptSource::Heartbeat => None,
    };
    let turn_id = result.turn_id.clone();
    let emit_turn_error = |error_msg: &str, error_code: Option<i64>| {
        if let Some(ref observer) = observer {
            let mut payload = serde_json::json!({
                "outcome": outcome_label,
                "error": error_msg,
            });
            if let Some(code) = error_code {
                payload["code"] = serde_json::json!(code);
            }
            observer.emit(
                "turn_error",
                Some(agent_index),
                &observer::context_for(channel_id, None, Some(turn_id.clone())),
                payload,
            );
        }
    };

    match result.outcome {
        // Successful prompt — return agent to pool.
        PromptOutcome::Ok(_) => {
            tracing::debug!(
                agent = agent_index,
                outcome = outcome_label,
                "agent_returned"
            );
            pool.return_agent(result.agent);
        }
        // Fatal outcomes: the agent subprocess is dead or poisoned — respawn it.
        PromptOutcome::AgentExited | PromptOutcome::Timeout(_) => {
            tracing::warn!(
                agent = agent_index,
                outcome = outcome_label,
                configured_model = %harness_configured_model,
                pid = harness_pid,
                "agent_returned — respawning"
            );
            let death_message: String = match outcome_label {
                "exited" => "Agent process exited unexpectedly".to_string(),
                "hard_timeout" => {
                    // Neutral wording when no fate was recorded above: a
                    // heartbeat hard timeout carries no batch at all, so
                    // nothing was requeued or dead-lettered.
                    let suffix = hard_timeout_fate_suffix.unwrap_or(" (no batch to retry)");
                    format!(
                        "Agent turn exceeded the maximum duration ({}s){}",
                        config.max_turn_duration_secs, suffix
                    )
                }
                _ => "Agent session timed out due to inactivity".to_string(),
            };
            emit_turn_error(&death_message, None);

            let index = result.agent.index;
            let slot_history = &mut crash_history[index];
            if !spawn_respawn_task(
                result.agent,
                config,
                slot_history,
                respawn_tx,
                respawn_tasks,
                observer.clone(),
            ) {
                // Circuit open — slot stays empty until maintenance refill.
                if pool.live_count() == 0 && !any_respawn_in_flight(crash_history) {
                    tracing::error!("all agents dead — exiting");
                    return LoopAction::Exit;
                }
            }
        }
        // Cancel-drain expiry: a control-signal cancel (steer fallback,
        // interrupt, or explicit stop) did not drain within its bounded
        // grace window. The process is poisoned/uncertain like a hard
        // timeout — respawn it — but this is NOT the configured max-turn
        // cap, so the message must name the actual grace, not
        // `max_turn_duration_secs`. The triggering batch's fate (preserved
        // for Steer/Interrupt, dropped for explicit Cancel/Rotate or a
        // removed channel) is decided above — the message stays fate-neutral
        // since it must be true in every case.
        PromptOutcome::CancelDrainTimeout(grace) => {
            tracing::warn!(
                agent = agent_index,
                outcome = outcome_label,
                configured_model = %harness_configured_model,
                pid = harness_pid,
                grace = ?grace,
                "agent_returned — respawning (cancel-drain timeout)"
            );
            let death_message = format!(
                "Agent did not stop within {grace:?} after cancellation; the agent process is being replaced."
            );
            emit_turn_error(&death_message, None);

            let index = result.agent.index;
            let slot_history = &mut crash_history[index];
            if !spawn_respawn_task(
                result.agent,
                config,
                slot_history,
                respawn_tx,
                respawn_tasks,
                observer.clone(),
            ) {
                // Circuit open — slot stays empty until maintenance refill.
                if pool.live_count() == 0 && !any_respawn_in_flight(crash_history) {
                    tracing::error!("all agents dead — exiting");
                    return LoopAction::Exit;
                }
            }
        }
        // Errors fall into two categories:
        //
        // 1. Transport-class (Io, WriteTimeout, Timeout, Protocol): the stdio
        //    pipe may be corrupted or the agent desynchronized. These are fatal
        //    to the agent regardless of whether they occurred during session
        //    creation or an active prompt — respawn unconditionally.
        //
        // 2. Application-class (IdleTimeout, HardTimeout, Json): the pipe is
        //    intact but the prompt failed. Return the agent to the pool so it
        //    can be reused for the next event.

        // Intentional cancel — agent is healthy, return it to the pool.
        // No respawn, no retry penalty. The cancelled batch was already stored
        // via requeue_as_cancelled() above and will be merged into the next
        // FlushBatch by flush_next().
        PromptOutcome::Cancelled => {
            tracing::debug!(
                agent = agent_index,
                outcome = outcome_label,
                configured_model = %harness_configured_model,
                pid = harness_pid,
                "agent_returned (cancelled)"
            );
            pool.return_agent(result.agent);
        }
        PromptOutcome::Error(ref e) => {
            let is_transport_error = matches!(
                e,
                acp::AcpError::Io(_)
                    | acp::AcpError::WriteTimeout(_)
                    | acp::AcpError::Timeout(_)
                    | acp::AcpError::Protocol(_)
            );
            let error_code = match &e {
                acp::AcpError::AgentError { code, .. } => Some(*code),
                _ => None,
            };
            if is_transport_error {
                tracing::warn!(
                    agent = agent_index,
                    outcome = outcome_label,
                    configured_model = %harness_configured_model,
                    pid = harness_pid,
                    error = %e,
                    "transport/protocol error — respawning agent"
                );
                emit_turn_error(&e.to_string(), error_code);

                let index = result.agent.index;
                let slot_history = &mut crash_history[index];
                if !spawn_respawn_task(
                    result.agent,
                    config,
                    slot_history,
                    respawn_tx,
                    respawn_tasks,
                    observer,
                ) && pool.live_count() == 0
                    && !any_respawn_in_flight(crash_history)
                {
                    tracing::error!("all agents dead — exiting");
                    return LoopAction::Exit;
                }
            } else {
                tracing::warn!(
                    agent = agent_index,
                    outcome = outcome_label,
                    configured_model = %harness_configured_model,
                    pid = harness_pid,
                    error = %e,
                    "agent_returned (application error — pipe intact)"
                );
                emit_turn_error(&e.to_string(), error_code);
                pool.return_agent(result.agent);
            }
        }
    }
    LoopAction::Continue
}

#[allow(clippy::too_many_arguments)]
fn recover_panicked_agent(
    pool: &mut AgentPool,
    queue: &mut EventQueue,
    config: &Config,
    join_error: tokio::task::JoinError,
    heartbeat_in_flight: &mut bool,
    removed_channels: &HashSet<Uuid>,
    typing_channels: &mut HashMap<Uuid, ThreadTags>,
    crash_history: &mut [SlotCircuit],
    respawn_tx: &mpsc::Sender<RespawnResult>,
    respawn_tasks: &mut tokio::task::JoinSet<()>,
    observer: Option<observer::ObserverHandle>,
) {
    let task_id = join_error.id();
    let Some(meta) = pool.task_map_mut().remove(&task_id) else {
        tracing::error!("panic for unknown task {task_id:?} — bug");
        return;
    };
    let i = meta.agent_index;

    // Requeue BEFORE mark_complete (same rationale as handle_prompt_result).
    if let Some(batch) = meta.recoverable_batch {
        if let Some(ch) = meta.channel_id {
            if !removed_channels.contains(&ch) {
                // Dead-letter on exhaustion is logged inside requeue(); a
                // panic path has no outcome to report, so no notice here.
                let _ = queue.requeue(batch);
                tracing::warn!("requeued batch for panicked agent {i}");
            } else {
                tracing::debug!(
                    channel_id = %ch,
                    "dropping panicked batch for removed channel"
                );
            }
        }
    }

    if let Some(ch) = meta.channel_id {
        queue.mark_complete(ch);
        typing_channels.remove(&ch);
        tracing::warn!("cleared wedged in-flight channel {ch} from panicked agent {i}");
    } else {
        *heartbeat_in_flight = false;
        tracing::warn!("cleared wedged heartbeat_in_flight from panicked agent {i}");
    }

    if let Some(ref observer) = observer {
        observer.emit(
            "agent_panic",
            Some(i),
            &observer::context_for(meta.channel_id, None, Some(meta.turn_id)),
            serde_json::json!({
                "outcome": "panic",
                "error": format!("Agent task panicked: {join_error}"),
            }),
        );
    }

    // Panics count as crashes for the circuit breaker.
    // The panicked task already dropped the AcpClient, so we just need to
    // check the circuit and spawn a fresh agent in the background.
    let slot = &mut crash_history[i];

    let delay = match slot.record_crash() {
        CrashVerdict::CircuitOpen => {
            tracing::error!(agent = i, "circuit open after panic — not respawning");
            return;
        }
        CrashVerdict::HalfOpenProbe => {
            tracing::info!(agent = i, "circuit half-open — probe respawn after panic");
            Duration::ZERO
        }
        CrashVerdict::Respawn(d) => {
            tracing::info!(
                agent = i,
                delay_ms = d.as_millis(),
                "respawn backoff after panic"
            );
            d
        }
    };

    // Spawn respawn work off the main loop.
    slot.respawn_in_flight = true;
    let cmd = config.agent_command.clone();
    let args = config.agent_args.clone();
    let env = config.persona_env_vars.clone();
    let has_codex = config.has_generated_codex_config;
    let guard = RespawnGuard::new(i, respawn_tx.clone());
    respawn_tasks.spawn(async move {
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        let result = spawn_and_init(&cmd, &args, &env, has_codex, i, observer).await;
        guard.send(result);
    });
}

#[allow(clippy::too_many_arguments)]
fn drain_ready_join_results(
    pool: &mut AgentPool,
    queue: &mut EventQueue,
    config: &Config,
    heartbeat_in_flight: &mut bool,
    removed_channels: &HashSet<Uuid>,
    typing_channels: &mut HashMap<Uuid, ThreadTags>,
    crash_history: &mut [SlotCircuit],
    respawn_tx: &mpsc::Sender<RespawnResult>,
    respawn_tasks: &mut tokio::task::JoinSet<()>,
    observer: Option<observer::ObserverHandle>,
) -> LoopAction {
    while let Some(Some(join_result)) = pool.join_set.join_next().now_or_never() {
        if let Err(join_error) = join_result {
            tracing::error!("agent task panicked: {join_error}");
            recover_panicked_agent(
                pool,
                queue,
                config,
                join_error,
                heartbeat_in_flight,
                removed_channels,
                typing_channels,
                crash_history,
                respawn_tx,
                respawn_tasks,
                observer.clone(),
            );
            if pool.live_count() == 0 && !any_respawn_in_flight(crash_history) {
                return LoopAction::Exit;
            }
        }
    }
    LoopAction::Continue
}

fn dispatch_heartbeat(
    pool: &mut AgentPool,
    ctx: &Arc<PromptContext>,
    heartbeat_in_flight: &mut bool,
) {
    if *heartbeat_in_flight {
        return;
    }
    let agent = match pool.try_claim(None) {
        Some(a) => a,
        None => return,
    };

    let prompt_text = ctx
        .heartbeat_prompt
        .clone()
        .unwrap_or_else(default_heartbeat_prompt);
    let result_tx = pool.result_tx();
    let ctx_clone = Arc::clone(ctx);
    let agent_index = agent.index;
    let turn_id = Uuid::new_v4().to_string();
    let task_turn_id = turn_id.clone();

    let abort_handle = pool.join_set.spawn(async move {
        pool::run_prompt_task(
            agent,
            None,
            Some(prompt_text),
            ctx_clone,
            result_tx,
            None,
            task_turn_id,
        )
        .await;
    });

    pool.task_map_mut().insert(
        abort_handle.id(),
        pool::TaskMeta {
            agent_index,
            channel_id: None,
            turn_id,
            recoverable_batch: None,
            control_tx: None,
            steer_tx: None,
        },
    );
    *heartbeat_in_flight = true;
    tracing::info!(agent = agent_index, "heartbeat_fired");
}

#[cfg(test)]
mod project_discovery_ingestion_tests {
    use super::*;
    use nostr::{EventBuilder, Keys};

    async fn proven_announcement(keys: &Keys, identifier: &str) -> project::VerifiedAnnouncement {
        let event = EventBuilder::new(
            nostr::Kind::Custom(buzz_core::kind::KIND_GIT_REPO_ANNOUNCEMENT as u16),
            "announcement",
        )
        .tags([nostr::Tag::parse(vec!["d".to_string(), identifier.to_string()]).expect("d tag")])
        .sign_with_keys(keys)
        .expect("sign");
        project::VerifiedAnnouncement::prove(
            project::VerifiedProjectEvent::verify(event)
                .await
                .expect("valid"),
        )
        .expect("well-formed")
    }

    /// Build a dispatch context over the given state, the way the run loop
    /// does. Test-local so every test names the same production entry point;
    /// nothing here classifies or decides.
    #[allow(clippy::too_many_arguments)]
    fn dispatch_over<'a>(
        agent: &'a project::AgentIdentity,
        owner: Option<&'a str>,
        humans: &'a std::collections::BTreeSet<String>,
        externals: &'a std::collections::BTreeSet<String>,
        discovered: &'a mut project::DiscoveredRepositories,
        enrolments: &'a mut project::ProjectEnrolments,
        queue: &'a mut EventQueue,
        ledger: &'a mut peer_call::CallLedger,
    ) -> ProjectDispatch<'a> {
        ProjectDispatch {
            identity: project::ProjectIdentity {
                agent,
                agent_owner: owner,
                approved_humans: humans,
                approved_external_agents: externals,
            },
            discovered,
            enrolments,
            queue,
            // No attestation: an agent author is untrusted here, which is what
            // every pre-Phase-1b case in this module was written against.
            // `dispatch_over_sibling` is the variant for peer-call cases.
            sibling: None,
            ledger,
        }
    }

    /// [`dispatch_over`] with a NIP-OA attestation in hand.
    ///
    /// Separate rather than an extra parameter on `dispatch_over` so the
    /// dozens of existing cases keep asserting against the untrusted default,
    /// and so a case that grants trust has to say so at its call site.
    #[allow(clippy::too_many_arguments)]
    fn dispatch_over_sibling<'a>(
        agent: &'a project::AgentIdentity,
        owner: Option<&'a str>,
        humans: &'a std::collections::BTreeSet<String>,
        externals: &'a std::collections::BTreeSet<String>,
        discovered: &'a mut project::DiscoveredRepositories,
        enrolments: &'a mut project::ProjectEnrolments,
        queue: &'a mut EventQueue,
        ledger: &'a mut peer_call::CallLedger,
        sibling: Option<project::VerifiedSibling>,
    ) -> ProjectDispatch<'a> {
        ProjectDispatch {
            identity: project::ProjectIdentity {
                agent,
                agent_owner: owner,
                approved_humans: humans,
                approved_external_agents: externals,
            },
            discovered,
            enrolments,
            queue,
            sibling,
            ledger,
        }
    }

    /// Dispatch a discovery announcement into `discovered` alone — the shape
    /// every pre-existing discovery test used before dispatch grew a context.
    fn dispatch_discovery(
        discovered: &mut project::DiscoveredRepositories,
        event: &project::ProjectEvent,
    ) -> ProjectDispatched {
        let agent = project::AgentIdentity::new(&Keys::generate().public_key()).unwrap();
        let humans = std::collections::BTreeSet::new();
        let externals = std::collections::BTreeSet::new();
        let mut enrolments = project::ProjectEnrolments::new();
        let mut queue = EventQueue::new(config::DedupMode::Queue);
        let mut ledger = peer_call::CallLedger::new();
        handle_project_event(
            &mut dispatch_over(
                &agent,
                None,
                &humans,
                &externals,
                discovered,
                &mut enrolments,
                &mut queue,
                &mut ledger,
            ),
            event,
        )
    }

    /// Dispatch one already-signed event through the production entry with a
    /// discovered repository in place. Returns what dispatch decided.
    ///
    /// Builds nothing the gate consumes: the caller supplies a real signed
    /// event, and verification, route derivation and every classification
    /// happen inside `handle_project_event`.
    async fn dispatch_routed(
        owner: &Keys,
        agent: &Keys,
        repo_id: &str,
        source: project::ProjectSubscription,
        preceding: Option<nostr::Event>,
        event: nostr::Event,
    ) -> ProjectDispatched {
        let agent_identity = project::AgentIdentity::new(&agent.public_key()).unwrap();
        let owner_hex = owner.public_key().to_hex();
        let humans = std::collections::BTreeSet::new();
        let externals = std::collections::BTreeSet::new();
        let mut discovered = project::DiscoveredRepositories::new();
        let mut enrolments = project::ProjectEnrolments::new();
        let mut queue = EventQueue::new(config::DedupMode::Queue);
        let mut ledger = peer_call::CallLedger::new();

        handle_project_event(
            &mut dispatch_over(
                &agent_identity,
                Some(&owner_hex),
                &humans,
                &externals,
                &mut discovered,
                &mut enrolments,
                &mut queue,
                &mut ledger,
            ),
            &project::ProjectEvent::Discovery {
                announcement: proven_announcement(owner, repo_id).await,
            },
        );

        // A comment carries no enrolment candidate of its own — only a root
        // does — so a comment can only wake a root the process already bound.
        // Dispatching the root first is what production does; without it the
        // gate has no validated coordinate and fails closed.
        if let Some(first) = preceding {
            let verified = project::VerifiedProjectEvent::verify(first)
                .await
                .expect("valid");
            let route = project::ProjectRoute::derive(&verified).expect("routes");
            handle_project_event(
                &mut dispatch_over(
                    &agent_identity,
                    Some(&owner_hex),
                    &humans,
                    &externals,
                    &mut discovered,
                    &mut enrolments,
                    &mut queue,
                    &mut ledger,
                ),
                &project::ProjectEvent::Routed {
                    source: project::ProjectSubscription::Enrolment,
                    route,
                    event: verified,
                },
            );
        }

        let verified = project::VerifiedProjectEvent::verify(event)
            .await
            .expect("valid");
        let route = project::ProjectRoute::derive(&verified).expect("routes");
        handle_project_event(
            &mut dispatch_over(
                &agent_identity,
                Some(&owner_hex),
                &humans,
                &externals,
                &mut discovered,
                &mut enrolments,
                &mut queue,
                &mut ledger,
            ),
            &project::ProjectEvent::Routed {
                source,
                route,
                event: verified,
            },
        )
    }

    fn root_event(
        owner: &Keys,
        agent: &Keys,
        repo_id: &str,
        kind: u32,
        body: &str,
    ) -> nostr::Event {
        let coord = format!("30617:{}:{repo_id}", owner.public_key().to_hex());
        EventBuilder::new(nostr::Kind::Custom(kind as u16), body)
            .tags([
                nostr::Tag::parse(["a", &coord]).unwrap(),
                nostr::Tag::parse(["p", &agent.public_key().to_hex()]).unwrap(),
            ])
            .sign_with_keys(owner)
            .expect("sign")
    }

    /// Mint a sibling attestation the way production does.
    ///
    /// Goes through [`ProjectSiblingLookup`] — the same resolver the run loop
    /// uses — rather than around it, because `VerifiedSibling`'s constructor is
    /// private to `project` and a test that could fabricate one would prove
    /// nothing about the path that grants trust.
    fn attested(author: &str, owner: &str) -> Option<project::VerifiedSibling> {
        use project::SiblingResolver as _;
        ProjectSiblingLookup {
            author: author.to_ascii_lowercase(),
            owner: owner.to_ascii_lowercase(),
            verified: true,
        }
        .resolve(author, owner)
    }

    /// NIP-PC, end to end on the project surface: a call from a verified
    /// same-owner sibling, on a root the agent is enrolled in, becomes a queued
    /// turn under that root's session key.
    ///
    /// This is the production path, not a neighbouring one: the event is built
    /// by the same `buzz-sdk` builder `buzz agents call` uses, and it is
    /// dispatched through `handle_project_event` — the function the run loop
    /// calls — against live discovery and enrolment state.
    ///
    /// Three controls travel with it, because "a call woke the agent" is only
    /// meaningful if the neighbouring cases do not:
    ///
    /// - the identical call with **no attestation** is refused, so the wake is
    ///   attributable to verified sibling trust rather than to the envelope
    ///   merely being well-formed;
    /// - an ordinary `kind:1` reply from the same trusted agent, `p`-tagging
    ///   the agent exactly as Desktop writes it, is refused — this is the reply
    ///   loop the envelope exists to prevent;
    /// - a call from an attested sibling addressed to **somebody else** is
    ///   refused, so `p` is doing real work.
    #[tokio::test]
    async fn a_sibling_call_on_an_enrolled_root_becomes_a_turn_on_that_root() {
        use buzz_sdk::builders::{build_peer_call, PeerCallMeta};

        let owner = Keys::generate();
        let agent = Keys::generate();
        let peer = Keys::generate();
        let agent_identity = project::AgentIdentity::new(&agent.public_key()).unwrap();
        let owner_hex = owner.public_key().to_hex();
        let agent_hex = agent.public_key().to_hex().to_ascii_lowercase();
        let peer_hex = peer.public_key().to_hex().to_ascii_lowercase();
        let humans = std::collections::BTreeSet::new();
        let externals = std::collections::BTreeSet::new();
        let mut discovered = project::DiscoveredRepositories::new();
        let mut enrolments = project::ProjectEnrolments::new();
        let mut queue = EventQueue::new(config::DedupMode::Queue);
        let mut ledger = peer_call::CallLedger::new();

        handle_project_event(
            &mut dispatch_over(
                &agent_identity,
                Some(&owner_hex),
                &humans,
                &externals,
                &mut discovered,
                &mut enrolments,
                &mut queue,
                &mut ledger,
            ),
            &project::ProjectEvent::Discovery {
                announcement: proven_announcement(&owner, "proj").await,
            },
        );

        // The owner opens an issue naming the agent, which is what enrols it.
        let root = root_event(
            &owner,
            &agent,
            "proj",
            buzz_core::kind::KIND_GIT_ISSUE,
            "please look at this",
        );
        let root_id = root.id.to_hex().to_ascii_lowercase();
        let coordinate = format!("30617:{owner_hex}:proj");
        let verified = project::VerifiedProjectEvent::verify(root)
            .await
            .expect("valid");
        let route = project::ProjectRoute::derive(&verified).expect("routes");
        let enrolled = handle_project_event(
            &mut dispatch_over(
                &agent_identity,
                Some(&owner_hex),
                &humans,
                &externals,
                &mut discovered,
                &mut enrolments,
                &mut queue,
                &mut ledger,
            ),
            &project::ProjectEvent::Routed {
                source: project::ProjectSubscription::Enrolment,
                route,
                event: verified,
            },
        );
        assert!(
            matches!(enrolled, ProjectDispatched::Queued { .. }),
            "the owner's mention must enrol the root, got {enrolled:?}"
        );

        let peer_route = buzz_core::peer_call::PeerCallRoute::Project {
            coordinate: coordinate.clone(),
            root: root_id.clone(),
        };
        let call = |callee: &str, nonce: &str| {
            build_peer_call(
                &peer_hex,
                "summarise the discussion so far",
                &PeerCallMeta {
                    callee: callee.to_string(),
                    route: peer_route.clone(),
                    nonce: nonce.to_string(),
                    hop: 1,
                    visited: vec![peer_hex.clone()],
                },
            )
            .expect("well-formed call")
            .sign_with_keys(&peer)
            .expect("sign")
        };

        // ── The outcome ──────────────────────────────────────────────────────
        let verified = project::VerifiedProjectEvent::verify(call(
            &agent_hex,
            "0123456789abcdef0123456789abcdef",
        ))
        .await
        .expect("valid");
        let route = project::ProjectRoute::derive(&verified).expect("a call routes to its root");
        let expected_key = route.key();
        let dispatched = handle_project_event(
            &mut dispatch_over_sibling(
                &agent_identity,
                Some(&owner_hex),
                &humans,
                &externals,
                &mut discovered,
                &mut enrolments,
                &mut queue,
                &mut ledger,
                attested(&peer_hex, &owner_hex),
            ),
            &project::ProjectEvent::Routed {
                source: project::ProjectSubscription::Watched { generation: 0 },
                route,
                event: verified,
            },
        );
        match dispatched {
            ProjectDispatched::Queued { queued, .. } => assert!(
                queued,
                "the call was accepted but nothing was queued for the agent to run"
            ),
            other => panic!("a trusted sibling's call must wake the agent, got {other:?}"),
        }
        assert_eq!(
            expected_key,
            project::project_route_key(&root_id).expect("root keys"),
            "the turn must run under the issue's own session, not a new one"
        );

        // ── Control: the same call, unattested ───────────────────────────────
        let verified = project::VerifiedProjectEvent::verify(call(
            &agent_hex,
            "fedcba9876543210fedcba9876543210",
        ))
        .await
        .expect("valid");
        let route = project::ProjectRoute::derive(&verified).expect("routes");
        let unattested = handle_project_event(
            &mut dispatch_over(
                &agent_identity,
                Some(&owner_hex),
                &humans,
                &externals,
                &mut discovered,
                &mut enrolments,
                &mut queue,
                &mut ledger,
            ),
            &project::ProjectEvent::Routed {
                source: project::ProjectSubscription::Watched { generation: 0 },
                route,
                event: verified,
            },
        );
        assert_eq!(
            unattested,
            ProjectDispatched::Ignored,
            "without a NIP-OA attestation the caller is an untrusted relay identity"
        );

        // ── Control: an ordinary reply from the same trusted agent ───────────
        let reply = EventBuilder::new(nostr::Kind::Custom(1), "thanks, taking a look")
            .tags([
                nostr::Tag::parse(["a", &coordinate]).unwrap(),
                nostr::Tag::parse(["e", &root_id, "", "root"]).unwrap(),
                nostr::Tag::parse(["p", &agent_hex]).unwrap(),
            ])
            .sign_with_keys(&peer)
            .expect("sign");
        let verified = project::VerifiedProjectEvent::verify(reply)
            .await
            .expect("valid");
        let route = project::ProjectRoute::derive(&verified).expect("routes");
        let ordinary = handle_project_event(
            &mut dispatch_over_sibling(
                &agent_identity,
                Some(&owner_hex),
                &humans,
                &externals,
                &mut discovered,
                &mut enrolments,
                &mut queue,
                &mut ledger,
                attested(&peer_hex, &owner_hex),
            ),
            &project::ProjectEvent::Routed {
                source: project::ProjectSubscription::Watched { generation: 0 },
                route,
                event: verified,
            },
        );
        assert_eq!(
            ordinary,
            ProjectDispatched::Ignored,
            "a trusted agent's bare p-tagged reply must never become an invocation"
        );

        // ── Control: a call addressed to a third party ───────────────────────
        let elsewhere = Keys::generate().public_key().to_hex().to_ascii_lowercase();
        let verified = project::VerifiedProjectEvent::verify(call(
            &elsewhere,
            "00112233445566778899aabbccddeeff",
        ))
        .await
        .expect("valid");
        let route = project::ProjectRoute::derive(&verified).expect("routes");
        let not_ours = handle_project_event(
            &mut dispatch_over_sibling(
                &agent_identity,
                Some(&owner_hex),
                &humans,
                &externals,
                &mut discovered,
                &mut enrolments,
                &mut queue,
                &mut ledger,
                attested(&peer_hex, &owner_hex),
            ),
            &project::ProjectEvent::Routed {
                source: project::ProjectSubscription::Watched { generation: 0 },
                route,
                event: verified,
            },
        );
        assert_eq!(
            not_ours,
            ProjectDispatched::Ignored,
            "a call naming another agent is not this agent's to answer"
        );
    }

    /// The other half of the loop on the project surface: the agent's own call
    /// is registered from the watched-root stream, and the callee's correlated
    /// result comes back as a turn under the *same* issue session.
    ///
    /// Controls travel with it, because "a result woke the agent" would be a
    /// much worse outcome than no result at all if any of these also woke it:
    ///
    /// - a result for a call this agent never made resumes nothing;
    /// - a second result for the same call is refused;
    /// - a result from somebody other than the callee is refused, so holding a
    ///   call id does not let a third party answer for the agent that was asked.
    #[tokio::test]
    async fn our_own_project_call_is_registered_and_its_result_resumes_the_issue() {
        use buzz_sdk::builders::{build_peer_call, build_peer_call_result, PeerCallMeta};

        let owner = Keys::generate();
        let agent = Keys::generate();
        let peer = Keys::generate();
        let agent_identity = project::AgentIdentity::new(&agent.public_key()).unwrap();
        let owner_hex = owner.public_key().to_hex();
        let agent_hex = agent.public_key().to_hex().to_ascii_lowercase();
        let peer_hex = peer.public_key().to_hex().to_ascii_lowercase();
        let humans = std::collections::BTreeSet::new();
        let externals = std::collections::BTreeSet::new();
        let mut discovered = project::DiscoveredRepositories::new();
        let mut enrolments = project::ProjectEnrolments::new();
        let mut queue = EventQueue::new(config::DedupMode::Queue);
        let mut ledger = peer_call::CallLedger::new();

        handle_project_event(
            &mut dispatch_over(
                &agent_identity,
                Some(&owner_hex),
                &humans,
                &externals,
                &mut discovered,
                &mut enrolments,
                &mut queue,
                &mut ledger,
            ),
            &project::ProjectEvent::Discovery {
                announcement: proven_announcement(&owner, "proj").await,
            },
        );

        let root = root_event(
            &owner,
            &agent,
            "proj",
            buzz_core::kind::KIND_GIT_ISSUE,
            "please look at this",
        );
        let root_id = root.id.to_hex().to_ascii_lowercase();
        let coordinate = format!("30617:{owner_hex}:proj");
        let verified = project::VerifiedProjectEvent::verify(root)
            .await
            .expect("valid");
        let route = project::ProjectRoute::derive(&verified).expect("routes");
        let expected_key = route.key();
        handle_project_event(
            &mut dispatch_over(
                &agent_identity,
                Some(&owner_hex),
                &humans,
                &externals,
                &mut discovered,
                &mut enrolments,
                &mut queue,
                &mut ledger,
            ),
            &project::ProjectEvent::Routed {
                source: project::ProjectSubscription::Enrolment,
                route,
                event: verified,
            },
        );

        let peer_route = buzz_core::peer_call::PeerCallRoute::Project {
            coordinate,
            root: root_id.clone(),
        };
        let (hop, visited) = buzz_core::peer_call::onward_context(&[], &agent_hex);
        let ours = build_peer_call(
            &agent_hex,
            "check the failing test for me",
            &PeerCallMeta {
                callee: peer_hex.clone(),
                route: peer_route.clone(),
                nonce: "0123456789abcdef0123456789abcdef".into(),
                hop,
                visited,
            },
        )
        .expect("well-formed call")
        .sign_with_keys(&agent)
        .expect("sign");
        let call_id = ours
            .tags
            .iter()
            .find_map(|t| {
                let s = t.as_slice();
                (s.first().map(String::as_str) == Some("call")).then(|| s[1].clone())
            })
            .expect("a call carries its id");

        // Our own call comes back down the watched-root REQ. It must not wake
        // us, and it must be registered.
        let verified = project::VerifiedProjectEvent::verify(ours)
            .await
            .expect("valid");
        let route = project::ProjectRoute::derive(&verified).expect("routes");
        let own = handle_project_event(
            &mut dispatch_over(
                &agent_identity,
                Some(&owner_hex),
                &humans,
                &externals,
                &mut discovered,
                &mut enrolments,
                &mut queue,
                &mut ledger,
            ),
            &project::ProjectEvent::Routed {
                source: project::ProjectSubscription::Watched { generation: 0 },
                route,
                event: verified,
            },
        );
        assert_eq!(
            own,
            ProjectDispatched::Ignored,
            "an agent's own call must not wake it"
        );
        assert_eq!(
            ledger.outstanding_count(),
            1,
            "the call must be registered from the wire, or its result correlates to nothing"
        );

        // ── Control: a result nobody asked for ───────────────────────────────
        let unasked = build_peer_call_result(&agent_hex, &"ab".repeat(32), "unasked", &peer_route)
            .expect("well-formed")
            .sign_with_keys(&peer)
            .expect("sign");
        let verified = project::VerifiedProjectEvent::verify(unasked)
            .await
            .expect("valid");
        let route = project::ProjectRoute::derive(&verified).expect("routes");
        assert_eq!(
            handle_project_event(
                &mut dispatch_over_sibling(
                    &agent_identity,
                    Some(&owner_hex),
                    &humans,
                    &externals,
                    &mut discovered,
                    &mut enrolments,
                    &mut queue,
                    &mut ledger,
                    attested(&peer_hex, &owner_hex),
                ),
                &project::ProjectEvent::Routed {
                    source: project::ProjectSubscription::Watched { generation: 0 },
                    route,
                    event: verified,
                },
            ),
            ProjectDispatched::Ignored,
            "a result correlating to no outstanding call is not a prompt"
        );

        // ── Control: a third party answering for the callee ──────────────────
        let impostor = Keys::generate();
        let forged = build_peer_call_result(&agent_hex, &call_id, "me instead", &peer_route)
            .expect("well-formed")
            .sign_with_keys(&impostor)
            .expect("sign");
        let impostor_hex = impostor.public_key().to_hex().to_ascii_lowercase();
        let verified = project::VerifiedProjectEvent::verify(forged)
            .await
            .expect("valid");
        let route = project::ProjectRoute::derive(&verified).expect("routes");
        assert_eq!(
            handle_project_event(
                &mut dispatch_over_sibling(
                    &agent_identity,
                    Some(&owner_hex),
                    &humans,
                    &externals,
                    &mut discovered,
                    &mut enrolments,
                    &mut queue,
                    &mut ledger,
                    attested(&impostor_hex, &owner_hex),
                ),
                &project::ProjectEvent::Routed {
                    source: project::ProjectSubscription::Watched { generation: 0 },
                    route,
                    event: verified,
                },
            ),
            ProjectDispatched::Ignored,
            "only the agent the call was addressed to may answer it"
        );
        assert_eq!(ledger.outstanding_count(), 1, "the call is still open");

        // ── The outcome: the callee's result resumes the issue ───────────────
        let answer =
            build_peer_call_result(&agent_hex, &call_id, "it was the fixture", &peer_route)
                .expect("well-formed")
                .sign_with_keys(&peer)
                .expect("sign");
        let verified = project::VerifiedProjectEvent::verify(answer)
            .await
            .expect("valid");
        let route = project::ProjectRoute::derive(&verified).expect("routes");
        let resumed = handle_project_event(
            &mut dispatch_over_sibling(
                &agent_identity,
                Some(&owner_hex),
                &humans,
                &externals,
                &mut discovered,
                &mut enrolments,
                &mut queue,
                &mut ledger,
                attested(&peer_hex, &owner_hex),
            ),
            &project::ProjectEvent::Routed {
                source: project::ProjectSubscription::Watched { generation: 0 },
                route,
                event: verified,
            },
        );
        match resumed {
            ProjectDispatched::Queued { key, queued, .. } => {
                assert!(queued, "the result was correlated but nothing was queued");
                assert_eq!(
                    key, expected_key,
                    "the result must resume the issue's own session, not a new one"
                );
            }
            other => panic!("a correlated result must resume the call, got {other:?}"),
        }
        assert_eq!(ledger.outstanding_count(), 0, "the call is closed");
    }

    /// The watched-root REQ is replaced only when the watched set actually
    /// changed. Without this, a `watch_changed` that never became true would
    /// leave the subscription unissued and nothing would fail — and a
    /// `watch_changed` always true would replace a live request with an
    /// identical one on every re-mention.
    #[tokio::test]
    async fn only_a_genuine_join_marks_the_watched_set_changed() {
        let owner = Keys::generate();
        let agent = Keys::generate();
        let agent_identity = project::AgentIdentity::new(&agent.public_key()).unwrap();
        let owner_hex = owner.public_key().to_hex();
        let humans = std::collections::BTreeSet::new();
        let externals = std::collections::BTreeSet::new();
        let mut discovered = project::DiscoveredRepositories::new();
        let mut enrolments = project::ProjectEnrolments::new();
        let mut queue = EventQueue::new(config::DedupMode::Queue);
        let mut ledger = peer_call::CallLedger::new();

        handle_project_event(
            &mut dispatch_over(
                &agent_identity,
                Some(&owner_hex),
                &humans,
                &externals,
                &mut discovered,
                &mut enrolments,
                &mut queue,
                &mut ledger,
            ),
            &project::ProjectEvent::Discovery {
                announcement: proven_announcement(&owner, "proj").await,
            },
        );

        let root = root_event(
            &owner,
            &agent,
            "proj",
            buzz_core::kind::KIND_GIT_ISSUE,
            "first",
        );

        let verified = project::VerifiedProjectEvent::verify(root.clone())
            .await
            .expect("valid");
        let route = project::ProjectRoute::derive(&verified).expect("routes");
        let first = handle_project_event(
            &mut dispatch_over(
                &agent_identity,
                Some(&owner_hex),
                &humans,
                &externals,
                &mut discovered,
                &mut enrolments,
                &mut queue,
                &mut ledger,
            ),
            &project::ProjectEvent::Routed {
                source: project::ProjectSubscription::Enrolment,
                route,
                event: verified,
            },
        );
        assert!(
            matches!(
                first,
                ProjectDispatched::Queued {
                    watch_changed: true,
                    ..
                }
            ),
            "a root joining the watched set must replace the watched REQ, got {first:?}"
        );

        let verified = project::VerifiedProjectEvent::verify(root)
            .await
            .expect("valid");
        let route = project::ProjectRoute::derive(&verified).expect("routes");
        let again = handle_project_event(
            &mut dispatch_over(
                &agent_identity,
                Some(&owner_hex),
                &humans,
                &externals,
                &mut discovered,
                &mut enrolments,
                &mut queue,
                &mut ledger,
            ),
            &project::ProjectEvent::Routed {
                source: project::ProjectSubscription::Enrolment,
                route,
                event: verified,
            },
        );
        assert!(
            matches!(
                again,
                ProjectDispatched::Queued {
                    watch_changed: false,
                    ..
                }
            ),
            "the same root re-mentioned must not churn the subscription, got {again:?}"
        );
    }

    /// A subscriber that records what the orchestration asked for.
    ///
    /// The narrow capability is what makes this possible: dispatch needs one
    /// method, so a test can supply one method. Handing it the relay handle
    /// would have required a relay.
    /// Records the *submissions*, which is all this side now produces.
    ///
    /// There is deliberately no id, generation or predecessor to record: those
    /// are the registry's, and a test that could observe them here would be
    /// observing a decision this side no longer makes.
    #[derive(Default)]
    struct RecordingSubscriber {
        calls: std::sync::Mutex<Vec<(project::ProjectReplacement, String)>>,
    }

    impl ProjectSubscriber for RecordingSubscriber {
        async fn submit_project_replacement(
            &self,
            replacement: project::ProjectReplacement,
            filters: Vec<serde_json::Value>,
        ) -> Result<(), relay::RelayError> {
            self.calls
                .lock()
                .unwrap()
                .push((replacement, serde_json::to_string(&filters).unwrap()));
            Ok(())
        }
    }

    /// **The orchestration issues an enrolment replacement, and widens it.**
    ///
    /// This is the coverage whose absence let the enrolment defect ship. The
    /// REQ-issuing code lived inline in `run()`, which no test could enter, so
    /// nothing ever observed whether discovery actually produced a REQ — let
    /// alone a second one carrying the second repository.
    #[tokio::test]
    async fn discovery_drives_an_enrolment_replacement_that_widens() {
        let owner_a = Keys::generate();
        let owner_b = Keys::generate();
        let agent = Keys::generate();
        let agent_identity = project::AgentIdentity::new(&agent.public_key()).unwrap();
        let agent_hex = agent.public_key().to_hex();
        let humans = std::collections::BTreeSet::new();
        let externals = std::collections::BTreeSet::new();
        let mut discovered = project::DiscoveredRepositories::new();
        let mut enrolments = project::ProjectEnrolments::new();
        let mut queue = EventQueue::new(config::DedupMode::Queue);
        let mut ledger = peer_call::CallLedger::new();
        let subscriber = RecordingSubscriber::default();

        for keys in [&owner_a, &owner_b] {
            let announcement = proven_announcement(keys, "repo").await;
            dispatch_project_event(
                &mut dispatch_over(
                    &agent_identity,
                    None,
                    &humans,
                    &externals,
                    &mut discovered,
                    &mut enrolments,
                    &mut queue,
                    &mut ledger,
                ),
                &subscriber,
                &agent_hex,
                0,
                &project::ProjectEvent::Discovery { announcement },
            )
            .await;
        }

        let calls = subscriber.calls.lock().unwrap().clone();
        assert_eq!(
            calls.len(),
            2,
            "each discovery must submit a replacement: {calls:?}"
        );
        assert!(
            calls
                .iter()
                .all(|(class, _)| *class == project::ProjectReplacement::Enrolment),
            "discovery must submit enrolment replacements: {calls:?}"
        );

        // What this side is responsible for is *the question* — that the second
        // submission asks a wider one than the first. The id and predecessor it
        // is installed under are the registry's, and are proved on the wire by
        // the canonical scenario rather than guessed at here.
        assert_ne!(
            calls[0].1, calls[1].1,
            "the second submission carries the same filter as the first: it did not widen"
        );
        assert!(
            calls[1].1.contains(&owner_b.public_key().to_hex()),
            "the second repository is absent from the widened filter"
        );
    }

    /// An authorised root drives a watched-root replacement under a fresh
    /// generation, naming the predecessor it supersedes.
    #[tokio::test]
    async fn an_enrolled_root_drives_a_watched_replacement_with_a_predecessor() {
        let owner = Keys::generate();
        let agent = Keys::generate();
        let agent_identity = project::AgentIdentity::new(&agent.public_key()).unwrap();
        let owner_hex = owner.public_key().to_hex();
        let agent_hex = agent.public_key().to_hex();
        let humans = std::collections::BTreeSet::new();
        let externals = std::collections::BTreeSet::new();
        let mut discovered = project::DiscoveredRepositories::new();
        let mut enrolments = project::ProjectEnrolments::new();
        let mut queue = EventQueue::new(config::DedupMode::Queue);
        let mut ledger = peer_call::CallLedger::new();
        let subscriber = RecordingSubscriber::default();

        macro_rules! drive {
            ($ev:expr) => {
                dispatch_project_event(
                    &mut dispatch_over(
                        &agent_identity,
                        Some(&owner_hex),
                        &humans,
                        &externals,
                        &mut discovered,
                        &mut enrolments,
                        &mut queue,
                        &mut ledger,
                    ),
                    &subscriber,
                    &agent_hex,
                    0,
                    $ev,
                )
                .await
            };
        }

        drive!(&project::ProjectEvent::Discovery {
            announcement: proven_announcement(&owner, "proj").await,
        });

        for body in ["first issue", "second issue"] {
            let root = root_event(
                &owner,
                &agent,
                "proj",
                buzz_core::kind::KIND_GIT_ISSUE,
                body,
            );
            let verified = project::VerifiedProjectEvent::verify(root)
                .await
                .expect("valid");
            let route = project::ProjectRoute::derive(&verified).expect("routes");
            drive!(&project::ProjectEvent::Routed {
                source: project::ProjectSubscription::Enrolment,
                route,
                event: verified,
            });
        }

        let calls = subscriber.calls.lock().unwrap().clone();

        // Exactly one enrolment submission per discovery that widened, and one
        // watched submission per newly enrolled root. This side decides *that*
        // a replacement is wanted and *what* should be asked; it decides
        // nothing about identity, so there is nothing else here to assert.
        let enrolment: Vec<_> = calls
            .iter()
            .filter(|(class, _)| *class == project::ProjectReplacement::Enrolment)
            .collect();
        let watched: Vec<_> = calls
            .iter()
            .filter(|(class, _)| *class == project::ProjectReplacement::Watched)
            .collect();

        assert_eq!(
            enrolment.len(),
            1,
            "one discovery, one enrolment submission: {calls:?}"
        );
        assert_eq!(
            watched.len(),
            2,
            "each newly enrolled root submits a watched replacement: {calls:?}"
        );
        assert_ne!(
            watched[0].1, watched[1].1,
            "the second submission must carry the wider root set, not repeat the first"
        );
    }

    /// A root has no predecessor, so its `p` cannot have been copied forward.
    /// This is the plan's definition of done: a person opens an issue, names an
    /// agent, the agent wakes — with no reconstructed history anywhere.
    #[tokio::test]
    async fn an_issue_root_p_tag_wakes_without_any_history() {
        let owner = Keys::generate();
        let agent = Keys::generate();
        let event = root_event(
            &owner,
            &agent,
            "proj",
            buzz_core::kind::KIND_GIT_ISSUE,
            "please take a look",
        );

        let dispatched = dispatch_routed(
            &owner,
            &agent,
            "proj",
            project::ProjectSubscription::Enrolment,
            None,
            event,
        )
        .await;

        assert!(
            matches!(dispatched, ProjectDispatched::Queued { queued: true, .. }),
            "an issue root naming the agent must queue exactly one turn, got {dispatched:?}"
        );
    }

    /// Same rule, other root kind. A pull request root is equally first on its
    /// own root, so the exception must not be spelled for issues alone.
    #[tokio::test]
    async fn a_pull_request_root_p_tag_wakes_without_any_history() {
        let owner = Keys::generate();
        let agent = Keys::generate();
        let event = root_event(
            &owner,
            &agent,
            "proj",
            buzz_core::kind::KIND_GIT_PULL_REQUEST,
            "review this please",
        );

        let dispatched = dispatch_routed(
            &owner,
            &agent,
            "proj",
            project::ProjectSubscription::Enrolment,
            None,
            event,
        )
        .await;

        assert!(
            matches!(dispatched, ProjectDispatched::Queued { queued: true, .. }),
            "a PR root naming the agent must queue exactly one turn, got {dispatched:?}"
        );
    }

    /// The protection the root exception must not weaken.
    ///
    /// A comment *can* carry a `p` copied from an earlier participant list, and
    /// without complete history nothing can tell the difference — so it cannot
    /// bring an unwatched root into the active set. Note the scope: an
    /// already-active root continues on any comment by design
    /// (`wake_or_enrol`, `RootState::Active => Wake`), because that is the
    /// follow-up the enrolment `#p` REQ alone could never deliver. What is
    /// guarded here is *enrolment*, not delivery.
    #[tokio::test]
    async fn a_comment_p_tag_cannot_enrol_an_unwatched_root() {
        let owner = Keys::generate();
        let agent = Keys::generate();
        let root = root_event(
            &owner,
            &agent,
            "proj",
            buzz_core::kind::KIND_GIT_ISSUE,
            "root",
        );
        let coord = format!("30617:{}:proj", owner.public_key().to_hex());
        let comment = EventBuilder::new(
            nostr::Kind::Custom(buzz_core::kind::KIND_TEXT_NOTE as u16),
            "no visible mention here",
        )
        .tags([
            nostr::Tag::parse(["a", &coord]).unwrap(),
            nostr::Tag::parse(["e", &root.id.to_hex(), "", "root"]).unwrap(),
            nostr::Tag::parse(["p", &agent.public_key().to_hex()]).unwrap(),
        ])
        .sign_with_keys(&owner)
        .expect("sign");

        let dispatched = dispatch_routed(
            &owner,
            &agent,
            "proj",
            project::ProjectSubscription::Enrolment,
            None,
            comment,
        )
        .await;

        assert_eq!(
            dispatched,
            ProjectDispatched::Ignored,
            "a bare `p` on a comment is indistinguishable from propagation \
             without history, so it must not enrol an unwatched root"
        );
    }

    /// Visible mention syntax is evidence the primitive trusts without history,
    /// and the root exception must not have displaced it for comments.
    #[tokio::test]
    async fn a_comment_with_a_visible_mention_still_wakes() {
        let owner = Keys::generate();
        let agent = Keys::generate();
        let agent_npub = {
            use nostr::ToBech32;
            agent.public_key().to_bech32().unwrap()
        };
        let root = root_event(
            &owner,
            &agent,
            "proj",
            buzz_core::kind::KIND_GIT_ISSUE,
            "root",
        );
        let coord = format!("30617:{}:proj", owner.public_key().to_hex());
        let comment = EventBuilder::new(
            nostr::Kind::Custom(buzz_core::kind::KIND_TEXT_NOTE as u16),
            format!("nostr:{agent_npub} could you look at this"),
        )
        .tags([
            nostr::Tag::parse(["a", &coord]).unwrap(),
            nostr::Tag::parse(["e", &root.id.to_hex(), "", "root"]).unwrap(),
            nostr::Tag::parse(["p", &agent.public_key().to_hex()]).unwrap(),
        ])
        .sign_with_keys(&owner)
        .expect("sign");

        let dispatched = dispatch_routed(
            &owner,
            &agent,
            "proj",
            project::ProjectSubscription::Enrolment,
            Some(root),
            comment,
        )
        .await;

        assert!(
            matches!(dispatched, ProjectDispatched::Queued { .. }),
            "visible mention syntax still wakes a comment, got {dispatched:?}"
        );
    }

    /// The root exception is about *addressing*. Author classification is a
    /// separate gate and still suppresses the agent's own root, which is what
    /// stops an agent that opens an issue from waking itself.
    #[tokio::test]
    async fn a_self_authored_root_still_does_not_wake() {
        let agent = Keys::generate();
        // The agent announces the repository and opens the issue itself.
        let event = root_event(
            &agent,
            &agent,
            "proj",
            buzz_core::kind::KIND_GIT_ISSUE,
            "filing this myself",
        );

        let dispatched = dispatch_routed(
            &agent,
            &agent,
            "proj",
            project::ProjectSubscription::Enrolment,
            None,
            event,
        )
        .await;

        assert_eq!(
            dispatched,
            ProjectDispatched::Ignored,
            "self-authorship is suppressed by the author gate regardless of addressing"
        );
    }

    #[tokio::test]
    async fn a_discovered_announcement_enters_the_run_loops_repository_set() {
        // Previously this arm logged and dropped, so the discovery REQ was
        // transport with no destination — every announcement was fetched and
        // thrown away.
        let keys = Keys::generate();
        let signer = keys.public_key().to_hex();
        let mut discovered = project::DiscoveredRepositories::new();
        assert!(discovered.is_empty());

        dispatch_discovery(
            &mut discovered,
            &project::ProjectEvent::Discovery {
                announcement: proven_announcement(&keys, "my-repo").await,
            },
        );

        assert_eq!(discovered.len(), 1);
        assert!(discovered.contains(&format!("30617:{signer}:my-repo")));
    }

    #[tokio::test]
    async fn ingesting_the_same_announcement_twice_adds_one_repository() {
        let keys = Keys::generate();
        let mut discovered = project::DiscoveredRepositories::new();
        for _ in 0..2 {
            dispatch_discovery(
                &mut discovered,
                &project::ProjectEvent::Discovery {
                    announcement: proven_announcement(&keys, "my-repo").await,
                },
            );
        }
        assert_eq!(discovered.len(), 1);
    }

    #[tokio::test]
    async fn discovery_admits_a_repository_without_granting_anyone_authority() {
        // The reason ingesting is safe while the authority gate is unbuilt:
        // it adds a coordinate and nothing else. A stranger who announces a
        // repository gets it into the set — and enrolment still requires a
        // root whose own signed `a` names that exact coordinate, so an
        // unrelated root does not become enrollable.
        let stranger = Keys::generate();
        let mut discovered = project::DiscoveredRepositories::new();
        dispatch_discovery(
            &mut discovered,
            &project::ProjectEvent::Discovery {
                announcement: proven_announcement(&stranger, "theirs").await,
            },
        );

        let unrelated = EventBuilder::new(
            nostr::Kind::Custom(buzz_core::kind::KIND_GIT_ISSUE as u16),
            "an issue naming a repository nobody announced",
        )
        .tags([nostr::Tag::parse(vec![
            "a".to_string(),
            format!("30617:{}:not-announced", stranger.public_key().to_hex()),
        ])
        .expect("a tag")])
        .sign_with_keys(&Keys::generate())
        .expect("sign");
        let verified = project::VerifiedProjectEvent::verify(unrelated)
            .await
            .expect("valid");

        assert!(
            project::validate_enrolment_candidate(&verified, &discovered).is_none(),
            "a discovered repository is not a licence to enrol on a different coordinate"
        );
    }

    #[test]
    fn refusal_reporting_is_logarithmic_not_per_event() {
        // The gap hermes-gateway named: the mutation tests proved the *outcome*
        // distinguishes the transition, and said nothing about whether the
        // caller acts on it. This tests the caller's rule directly.
        assert!(
            should_report_refusal(project::Degradation::BecameDegraded, 1),
            "degradation must always be visible when it happens"
        );

        // After that, powers of two only.
        for total in [2u64, 4, 8, 16, 1024] {
            assert!(
                should_report_refusal(project::Degradation::AlreadyDegraded, total),
                "{total} is a power of two"
            );
        }
        for total in [3u64, 5, 6, 7, 9, 1023, 1025] {
            assert!(
                !should_report_refusal(project::Degradation::AlreadyDegraded, total),
                "{total} is not"
            );
        }
    }

    #[test]
    fn a_flood_of_refusals_produces_a_handful_of_records() {
        // The property that matters is the shape, not the individual answers.
        // A hostile stream must not be able to make the log grow with it —
        // which is exactly what `debug` per refusal did, since a log level
        // filters output rather than bounding it, and enabling diagnostics to
        // investigate a flood would have reopened the amplifier.
        let flood = 1_000_000u64;
        let reported = (1..=flood)
            .filter(|&n| {
                let degradation = if n == 1 {
                    project::Degradation::BecameDegraded
                } else {
                    project::Degradation::AlreadyDegraded
                };
                should_report_refusal(degradation, n)
            })
            .count();

        assert!(
            reported <= 21,
            "a million refusals produced {reported} records"
        );
        assert!(reported >= 2, "but degradation is not silent either");
    }

    #[tokio::test]
    async fn a_routed_project_event_does_not_mutate_discovery_state() {
        // The asymmetry is deliberate: delivering a routed event means deciding
        // who may invoke this agent, and that gate does not exist yet.
        let keys = Keys::generate();
        let mut discovered = project::DiscoveredRepositories::new();
        let root = "a".repeat(64);
        let event = EventBuilder::new(nostr::Kind::TextNote, "comment")
            .tags([nostr::Tag::parse(vec![
                "e".to_string(),
                root.clone(),
                String::new(),
                "root".to_string(),
            ])
            .expect("e tag")])
            .sign_with_keys(&keys)
            .expect("sign");
        let verified = project::VerifiedProjectEvent::verify(event)
            .await
            .expect("valid");
        let route = project::ProjectRoute::derive(&verified).expect("routes");

        dispatch_discovery(
            &mut discovered,
            &project::ProjectEvent::Routed {
                source: project::ProjectSubscription::Watched { generation: 0 },
                route,
                event: verified,
            },
        );

        assert!(
            discovered.is_empty(),
            "a routed event must not mutate discovery state"
        );
    }
}

#[cfg(test)]
mod agent_draft_prompt_tests {
    #[test]
    fn shared_base_prompt_teaches_portable_agent_drafts() {
        let prompt = include_str!("base_prompt.md");
        assert!(prompt.contains("buzz agents draft-create"));
        assert!(prompt.contains("ask for at most two things"));
        assert!(prompt.contains("what it should do day-to-day"));
        assert!(prompt.contains("owner saves it"));
        assert!(prompt.contains("Do not ask about runtime, provider, model, credentials"));
    }

    #[test]
    fn shared_base_prompt_teaches_real_newlines_for_multiline_messages() {
        let prompt = include_str!("base_prompt.md");
        assert!(prompt.contains("pass real newline bytes through stdin"));
        assert!(prompt.contains("single-quoted shell strings preserve `\\n` literally"));
        assert!(prompt.contains("buzz messages send ... --content -"));
    }

    #[test]
    fn shared_base_prompt_teaches_single_command_mentions_and_preflight() {
        let prompt = include_str!("base_prompt.md");
        assert!(prompt.contains("--mention <hex-or-npub>"));
        assert!(prompt.contains("every presentation-only name that should notify"));
        assert!(
            prompt.contains("permits unresolved or ambiguous `@Name` text as presentation-only")
        );
        assert!(prompt.contains("success JSON's `mention_pubkeys`"));
        assert!(prompt.contains("no follow-up verification command is needed"));
        assert!(prompt.contains("stops before sending"));
        assert!(prompt
            .contains("add them explicitly with `buzz channels add-member` only when authorized"));
        assert!(prompt.contains("never changes membership automatically"));
    }
}

fn default_heartbeat_prompt() -> String {
    let now = chrono::Utc::now().to_rfc3339();
    format!(
        "[System: Heartbeat]\nTime: {now}\n\n\
         You have been awakened for a routine heartbeat. You have NO incoming messages or\n\
         active channel context for this turn.\n\n\
         Your tasks:\n\
         1. Run `buzz feed get --types needs_action` to check for pending workflow approvals or\n\
            high-priority requests addressed to you.\n\
         2. Run `buzz feed get --types mentions` to check for unanswered @mentions.\n\
         3. If you find actionable items, address them using the appropriate CLI commands\n\
            (e.g., `buzz workflows approve --token <UUID>`, `buzz messages send`,\n\
            `buzz messages send --reply-to <event-id>`).\n\
         4. If there are no pending actions or mentions, end your turn immediately.\n\n\
         Do not run `buzz channels list` or `buzz messages search` unless you have a specific reason.\n\
         Do not invent work — only act on items surfaced by the feed commands."
    )
}

/// Spawn a background respawn task for a crashed agent slot.
///
/// Does the circuit breaker check synchronously (non-blocking), then spawns
/// the actual shutdown + backoff + spawn_and_init work into a background task.
/// The result comes back through `respawn_tx` so the main loop stays responsive.
///
/// Returns `true` if a respawn task was spawned, `false` if the circuit is open.
fn spawn_respawn_task(
    old_agent: OwnedAgent,
    config: &Config,
    slot: &mut SlotCircuit,
    respawn_tx: &mpsc::Sender<RespawnResult>,
    respawn_tasks: &mut tokio::task::JoinSet<()>,
    observer: Option<observer::ObserverHandle>,
) -> bool {
    let index = old_agent.index;

    // Circuit breaker: record crash, decide whether to respawn.
    let delay = match slot.record_crash() {
        CrashVerdict::CircuitOpen => {
            tracing::error!(agent = index, "circuit open — not respawning");
            return false;
        }
        CrashVerdict::HalfOpenProbe => {
            tracing::info!(agent = index, "circuit half-open — probe respawn");
            Duration::ZERO
        }
        CrashVerdict::Respawn(d) => {
            tracing::info!(agent = index, delay_ms = d.as_millis(), "respawn backoff");
            d
        }
    };

    slot.respawn_in_flight = true;

    // Spawn the actual work (shutdown + sleep + spawn + init) off the main loop.
    let cmd = config.agent_command.clone();
    let args = config.agent_args.clone();
    let env = config.persona_env_vars.clone();
    let has_codex = config.has_generated_codex_config;
    let guard = RespawnGuard::new(index, respawn_tx.clone());
    respawn_tasks.spawn(async move {
        // Shutdown old agent (reap child, prevent zombie).
        let mut agent = old_agent;
        let reap = agent.acp.shutdown().await;
        report_reap(index, "respawn-predecessor", reap);
        drop(agent);

        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }

        let result = spawn_and_init(&cmd, &args, &env, has_codex, index, observer).await;
        guard.send(result);
    });

    true
}

fn normalized_agent_name(init_result: &serde_json::Value) -> String {
    init_result
        .get("agentInfo")
        .or_else(|| init_result.get("serverInfo"))
        .and_then(|info| info.get("name"))
        .and_then(|value| value.as_str())
        .unwrap_or("unknown")
        .trim()
        .to_ascii_lowercase()
}

/// Report what a shutdown actually did to a child process.
///
/// Every one of these call sites previously logged "reaped … agent on
/// shutdown" unconditionally, on the strength of `shutdown` having returned.
/// That is precisely the claim [`acp::ChildReap`] exists to stop anyone making
/// for free: a five-second timeout with the child still running produced the
/// same reassuring line as a clean exit.
fn report_reap(index: usize, stage: &'static str, reap: acp::ChildReap) {
    match reap {
        acp::ChildReap::Reaped(status) => {
            tracing::debug!(agent = index, stage, ?status, "agent reaped on shutdown");
        }
        acp::ChildReap::WaitError(e) => {
            tracing::warn!(agent = index, stage, "agent wait failed on shutdown: {e}");
        }
        acp::ChildReap::TimedOut => {
            tracing::warn!(
                agent = index,
                stage,
                "agent did not exit within the shutdown grace period — it may still be running"
            );
        }
    }
}

async fn shutdown_agent_slots(slots: &mut [Option<OwnedAgent>]) {
    for slot in slots {
        if let Some(mut agent) = slot.take() {
            let idx = agent.index;
            let reap = agent.acp.shutdown().await;
            report_reap(idx, "slot", reap);
        }
    }
}

async fn shutdown_agent_pool(pool: &mut AgentPool) {
    pool.join_set.shutdown().await;
    while let Ok(mut result) = pool.result_rx_try_recv() {
        let idx = result.agent.index;
        let reap = result.agent.acp.shutdown().await;
        report_reap(idx, "pool-result", reap);
    }
    for slot in pool.agents_mut() {
        if let Some(mut agent) = slot.take() {
            let idx = agent.index;
            let reap = agent.acp.shutdown().await;
            report_reap(idx, "pool-slot", reap);
        }
    }
}

struct PoolStartup {
    agents: u32,
    command: String,
    args: Vec<String>,
    extra_env: Vec<(String, String)>,
    has_generated_codex_config: bool,
    model: Option<String>,
    observer: Option<observer::ObserverHandle>,
}

impl PoolStartup {
    fn from_config(config: &Config, observer: Option<observer::ObserverHandle>) -> Self {
        Self {
            agents: config.agents,
            command: config.agent_command.clone(),
            args: config.agent_args.clone(),
            extra_env: config.persona_env_vars.clone(),
            has_generated_codex_config: config.has_generated_codex_config,
            model: config.model.clone(),
            observer,
        }
    }
}

async fn initialize_agent_pool(
    startup: &PoolStartup,
    mut shutdown: Option<watch::Receiver<()>>,
) -> Result<AgentPool> {
    // One agent failing to start must not kill the whole pool.
    // Attempt each spawn under a 60-second timeout; a partial pool is valid.
    let mut agent_slots: Vec<Option<OwnedAgent>> = Vec::with_capacity(startup.agents as usize);
    for i in 0..startup.agents as usize {
        let spawn_result = AcpClient::spawn(
            &startup.command,
            &startup.args,
            &startup.extra_env,
            startup.has_generated_codex_config,
        )
        .await;
        match spawn_result {
            Ok(mut acp) => {
                acp.set_observer(startup.observer.clone(), i);
                let initialize = tokio::time::timeout(Duration::from_secs(60), acp.initialize());
                let initialize_result = match shutdown.as_mut() {
                    Some(shutdown) => tokio::select! {
                        biased;
                        _ = shutdown.changed() => {
                            acp.shutdown().await;
                            shutdown_agent_slots(&mut agent_slots).await;
                            return Err(anyhow::anyhow!("pool initialization cancelled by shutdown"));
                        }
                        result = initialize => result,
                    },
                    None => initialize.await,
                };
                match initialize_result {
                    Ok(Ok(init_result)) => {
                        tracing::info!(agent = i, "agent initialized: {init_result}");
                        let protocol_version =
                            init_result["protocolVersion"].as_u64().unwrap_or(1) as u32;
                        tracing::info!(
                            agent = i,
                            name = init_result
                                .get("agentInfo")
                                .or_else(|| init_result.get("serverInfo"))
                                .and_then(|info| info.get("name"))
                                .and_then(|v| v.as_str())
                                .unwrap_or("unknown"),
                            steering_supported = acp.steering_supported(),
                            "agent initialized"
                        );
                        acp.observe(
                            "agent_initialized",
                            serde_json::json!({
                                "agentIndex": i,
                                "initializeResult": init_result,
                            }),
                        );
                        let agent_name = normalized_agent_name(&init_result);
                        agent_slots.push(Some(OwnedAgent {
                            index: i,
                            acp,
                            state: SessionState::default(),
                            model_capabilities: None,
                            desired_model: startup.model.clone(),
                            model_overridden: false,
                            agent_name,
                            goose_system_prompt_supported: None,
                            protocol_version,
                        }));
                    }
                    Ok(Err(e)) => {
                        tracing::error!(agent = i, "agent initialize failed: {e}");
                        acp.shutdown().await;
                        agent_slots.push(None);
                    }
                    Err(_) => {
                        tracing::error!(agent = i, "agent timed out during init (60s)");
                        acp.shutdown().await;
                        agent_slots.push(None);
                    }
                }
            }
            Err(e) => {
                tracing::error!(agent = i, "agent failed to spawn: {e}");
                agent_slots.push(None);
            }
        }
    }
    let live_count = agent_slots.iter().filter(|slot| slot.is_some()).count();
    if live_count == 0 {
        return Err(anyhow::anyhow!(
            "all {} agents failed to start — cannot continue",
            startup.agents
        ));
    }
    if live_count < startup.agents as usize {
        tracing::warn!(
            "started {}/{} agents — continuing with reduced pool",
            live_count,
            startup.agents
        );
    }
    tracing::info!("agent_pool_ready agents={}", live_count);
    Ok(AgentPool::from_slots(agent_slots))
}

// ── spawn_and_init ────────────────────────────────────────────────────────────
/// Spawn an agent subprocess and run the MCP `initialize` handshake.
///
/// Takes owned args so it can run in a background `tokio::spawn` task without
/// borrowing `Config`. All respawn/refill paths use this.
async fn spawn_and_init(
    command: &str,
    args: &[String],
    extra_env: &[(String, String)],
    has_generated_codex_config: bool,
    agent_index: usize,
    observer: Option<observer::ObserverHandle>,
) -> Result<(AcpClient, u32, String)> {
    let mut acp = AcpClient::spawn(command, args, extra_env, has_generated_codex_config)
        .await
        .map_err(|e| anyhow::anyhow!("failed to spawn agent: {e}"))?;
    acp.set_observer(observer, agent_index);

    match acp.initialize().await {
        Ok(init_result) => {
            tracing::info!("agent initialized: {init_result}");
            let protocol_version = init_result["protocolVersion"].as_u64().unwrap_or(1) as u32;
            acp.observe(
                "agent_initialized",
                serde_json::json!({
                    "agentIndex": agent_index,
                    "initializeResult": init_result,
                }),
            );
            let agent_name = normalized_agent_name(&init_result);
            Ok((acp, protocol_version, agent_name))
        }
        Err(e) => {
            // Explicitly shut down the spawned child to prevent zombie/leak.
            // Drop only does start_kill + try_wait (best-effort); shutdown()
            // does start_kill + bounded wait (guaranteed reap).
            acp.shutdown().await;
            Err(anyhow::anyhow!("agent initialize failed: {e}"))
        }
    }
}

async fn spawn_auth_client(agent: &AuthAgentArgs) -> Result<AcpClient, acp::AcpError> {
    let agent_args = config::normalize_agent_args(&agent.agent_command, agent.agent_args.clone());
    AcpClient::spawn(&agent.agent_command, &agent_args, &[], false).await
}

fn extract_auth_methods(init_result: &serde_json::Value) -> Vec<serde_json::Value> {
    init_result
        .get("authMethods")
        .and_then(|methods| methods.as_array())
        .cloned()
        .unwrap_or_default()
}

/// `buzz-acp auth-methods` — spawn an adapter, initialize it, print authMethods.
async fn run_auth_methods(args: AuthMethodsArgs) -> Result<()> {
    let mut client = match spawn_auth_client(&args.agent).await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: failed to spawn agent: {e}");
            std::process::exit(1);
        }
    };

    let init_result = match tokio::time::timeout(MODELS_TIMEOUT, client.initialize()).await {
        Ok(Ok(result)) => result,
        Ok(Err(e)) => {
            client.shutdown().await;
            eprintln!("error: agent initialize failed: {e}");
            std::process::exit(1);
        }
        Err(_) => {
            client.shutdown().await;
            eprintln!("error: agent timed out ({MODELS_TIMEOUT:?})");
            std::process::exit(1);
        }
    };

    let methods = extract_auth_methods(&init_result);
    client.shutdown().await;

    if args.json {
        let output = serde_json::json!({ "methods": methods });
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else if methods.is_empty() {
        println!("No auth methods advertised.");
    } else {
        for method in methods {
            let id = method
                .get("id")
                .and_then(|value| value.as_str())
                .unwrap_or("unknown");
            let name = method
                .get("name")
                .and_then(|value| value.as_str())
                .unwrap_or(id);
            println!("{id}\t{name}");
        }
    }
    Ok(())
}

/// `buzz-acp authenticate` — invoke one adapter-owned auth method.
async fn run_authenticate(args: AuthenticateArgs) -> Result<()> {
    let mut client = match spawn_auth_client(&args.agent).await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: failed to spawn agent: {e}");
            std::process::exit(1);
        }
    };

    let init_result = match tokio::time::timeout(MODELS_TIMEOUT, client.initialize()).await {
        Ok(Ok(result)) => result,
        Ok(Err(e)) => {
            client.shutdown().await;
            eprintln!("error: agent initialize failed: {e}");
            std::process::exit(1);
        }
        Err(_) => {
            client.shutdown().await;
            eprintln!("error: agent initialize timed out ({MODELS_TIMEOUT:?})");
            std::process::exit(1);
        }
    };

    let supports_method = extract_auth_methods(&init_result)
        .iter()
        .any(|method| method.get("id").and_then(|id| id.as_str()) == Some(args.method_id.as_str()));
    if !supports_method {
        client.shutdown().await;
        eprintln!(
            "error: auth method '{}' is not advertised by this adapter",
            args.method_id
        );
        std::process::exit(1);
    }

    let result =
        tokio::time::timeout(AUTHENTICATE_TIMEOUT, client.authenticate(&args.method_id)).await;

    match result {
        Ok(Ok(_)) => {
            client.shutdown().await;
            Ok(())
        }
        Ok(Err(e)) => {
            client.shutdown().await;
            eprintln!("error: authenticate failed: {e}");
            std::process::exit(1);
        }
        Err(_) => {
            client.shutdown().await;
            eprintln!("error: authenticate timed out ({AUTHENTICATE_TIMEOUT:?})");
            std::process::exit(1);
        }
    }
}

/// Flow: spawn → initialize → session/new → print models → shutdown.
/// No relay connection, no MCP servers, no subscriptions. ~2-5s total.
async fn run_models(args: ModelsArgs) -> Result<()> {
    use acp::{extract_model_config_options, extract_model_state};

    let agent_args = config::normalize_agent_args(&args.agent.agent_command, args.agent.agent_args);
    let cwd = std::env::current_dir()
        .unwrap_or_else(|_| std::path::PathBuf::from("/"))
        .to_string_lossy()
        .to_string();

    // Spawn outside the timeout so we always own the child for cleanup.
    // `models` subcommand doesn't use persona packs — no extra env, no codex config.
    let mut client =
        match AcpClient::spawn(&args.agent.agent_command, &agent_args, &[], false).await {
            Ok(c) => c,
            Err(e) => {
                eprintln!("error: failed to spawn agent: {e}");
                std::process::exit(1);
            }
        };

    // Initialize + session/new under a timeout. Client is owned above,
    // so shutdown() runs on all paths (success, error, timeout).
    let protocol_result = tokio::time::timeout(MODELS_TIMEOUT, async {
        let init = client.initialize().await?;
        let session = client.session_new_full(&cwd, vec![], None, None).await?;
        Ok::<_, acp::AcpError>((init, session))
    })
    .await;

    let (init_result, session_resp) = match protocol_result {
        Ok(Ok(tuple)) => tuple,
        Ok(Err(e)) => {
            client.shutdown().await;
            eprintln!("error: agent communication failed: {e}");
            std::process::exit(1);
        }
        Err(_) => {
            client.shutdown().await;
            eprintln!("error: agent timed out ({MODELS_TIMEOUT:?})");
            std::process::exit(1);
        }
    };

    // Extract agent info from initialize response.
    // ACP spec uses "serverInfo" (MCP heritage); some agents may use "agentInfo".
    let info_obj = init_result
        .get("serverInfo")
        .or_else(|| init_result.get("agentInfo"));
    let agent_name = info_obj
        .and_then(|ai| ai.get("name"))
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    let agent_version = info_obj
        .and_then(|ai| ai.get("version"))
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");

    // Extract model info from session/new response.
    let config_options = extract_model_config_options(&session_resp.raw);
    let model_state = extract_model_state(&session_resp.raw);

    if args.json {
        // Structured JSON output — consumed by Phase 3 `get_agent_models`.
        let output = serde_json::json!({
            "agent": {
                "name": agent_name,
                "version": agent_version,
            },
            "stable": {
                "configOptions": config_options,
            },
            "unstable": model_state.as_ref().map(|ms| serde_json::json!({
                "currentModelId": ms.get("currentModelId"),
                "availableModels": ms.get("availableModels"),
            })),
        });
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        // Human-readable output.
        println!("Agent: {} v{}", agent_name, agent_version);
        println!();

        let mut has_models = false;

        if !config_options.is_empty() {
            println!("Models (stable configOptions):");
            for opt in &config_options {
                let config_id = opt.get("configId").and_then(|v| v.as_str()).unwrap_or("?");
                let display = opt
                    .get("displayName")
                    .and_then(|v| v.as_str())
                    .unwrap_or(config_id);
                println!("  {display} (configId: {config_id})");
                if let Some(options) = opt.get("options").and_then(|v| v.as_array()) {
                    for o in options {
                        let val = o.get("value").and_then(|v| v.as_str()).unwrap_or("?");
                        let name = o.get("displayName").and_then(|v| v.as_str()).unwrap_or(val);
                        println!("    - {name} (value: {val})");
                    }
                }
            }
            has_models = true;
        }

        if let Some(ref ms) = model_state {
            let current = ms
                .get("currentModelId")
                .and_then(|v| v.as_str())
                .unwrap_or("(none)");
            println!("Models (unstable SessionModelState):");
            println!("  Current: {current}");
            if let Some(available) = ms.get("availableModels").and_then(|v| v.as_array()) {
                println!("  Available:");
                for m in available {
                    let id = m.get("modelId").and_then(|v| v.as_str()).unwrap_or("?");
                    let name = m.get("name").and_then(|v| v.as_str()).unwrap_or(id);
                    let desc = m.get("description").and_then(|v| v.as_str()).unwrap_or("");
                    if desc.is_empty() {
                        println!("    - {name} (id: {id})");
                    } else {
                        println!("    - {name} (id: {id}) — {desc}");
                    }
                }
            }
            has_models = true;
        }

        if !has_models {
            println!("No model information available from this agent.");
        }
    }

    client.shutdown().await;
    Ok(())
}

fn build_mcp_servers(config: &Config) -> Vec<McpServer> {
    if config.mcp_command.is_empty() {
        return vec![];
    }
    vec![McpServer {
        name: std::path::Path::new(&config.mcp_command)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("mcp")
            .to_string(),
        command: config.mcp_command.clone(),
        args: vec![],
        env: {
            let mut env = vec![
                EnvVar {
                    name: "BUZZ_RELAY_URL".into(),
                    value: config.relay_url.clone(),
                },
                EnvVar {
                    name: "BUZZ_PRIVATE_KEY".into(),
                    // bech32 encoding of a valid secret key is infallible.
                    // Panic here is correct: injecting a bogus secret would cause
                    // delayed, hard-to-diagnose agent failures downstream.
                    value: config
                        .keys
                        .secret_key()
                        .to_bech32()
                        .expect("secret key bech32 encoding should never fail"),
                },
            ];
            // Forward BUZZ_AUTH_TAG (NIP-OA owner attestation credential)
            // so the MCP server can attach it to every signed event.
            if let Ok(auth_tag) = std::env::var("BUZZ_AUTH_TAG") {
                if !auth_tag.is_empty() {
                    env.push(EnvVar {
                        name: "BUZZ_AUTH_TAG".into(),
                        value: auth_tag,
                    });
                }
            }
            // Forward the agent's display name so dev-mcp can use it as the git
            // author name instead of the raw npub. Read from the process env
            // rather than Config: this is a pass-through of a contract owned
            // upstream, and absent simply means dev-mcp falls back to the npub.
            if let Ok(display_name) = std::env::var("BUZZ_ACP_DISPLAY_NAME") {
                if !display_name.is_empty() {
                    env.push(EnvVar {
                        name: "BUZZ_ACP_DISPLAY_NAME".into(),
                        value: display_name,
                    });
                }
            }
            env
        },
    }]
}

#[cfg(test)]
mod heartbeat_base_prompt_tests {
    use super::*;

    // Pins the heartbeat dispatch path (dispatch_heartbeat, ~line 2359): a
    // legacy agent WITH a base_prompt must get [Base] prepended to the
    // heartbeat user message, composed as `[Base]\n{bp}\n\n{prompt}`. This is
    // the second half of the round-2 regression (the first being initial_message).

    #[test]
    fn test_heartbeat_legacy_agent_gets_base_prepended() {
        // protocol_version 1 + Some(base_prompt): heartbeat prompt is prefixed
        // with the [Base] section exactly as the legacy session/new path would.
        let prompt = "[System: Heartbeat]\nrun feed get";
        let composed = pool::prepend_base_for_legacy(1, Some("you are a helpful agent"), prompt);
        assert_eq!(
            composed,
            "[Base]\nyou are a helpful agent\n\n[System: Heartbeat]\nrun feed get"
        );
        assert!(composed.starts_with("[Base]\nyou are a helpful agent\n\n"));
    }

    #[test]
    fn test_heartbeat_modern_agent_omits_base() {
        // protocol_version 2 gets base_prompt via session/new; the heartbeat
        // prompt is sent verbatim.
        let prompt = "[System: Heartbeat]\nrun feed get";
        let composed = pool::prepend_base_for_legacy(2, Some("you are a helpful agent"), prompt);
        assert_eq!(composed, prompt);
    }
}

#[cfg(test)]
mod owner_control_command_tests {
    use super::*;
    use nostr::{EventBuilder, Keys, Kind, Tag};

    fn make_event(kind: u32, content: &str, p_hex: Option<&str>) -> nostr::Event {
        let keys = Keys::generate();
        let tags = match p_hex {
            Some(hex) => vec![Tag::parse(["p", hex]).expect("p tag")],
            None => vec![],
        };
        EventBuilder::new(Kind::Custom(kind as u16), content)
            .tags(tags)
            .sign_with_keys(&keys)
            .unwrap()
    }

    #[test]
    fn owner_control_command_requires_kind_content_and_agent_mention() {
        let agent = "ab".repeat(32);

        let event = make_event(KIND_STREAM_MESSAGE, " !rotate ", Some(&agent));
        assert!(is_owner_control_command(
            &event,
            KIND_STREAM_MESSAGE,
            "!rotate",
            &agent
        ));

        let wrong_kind = make_event(1, "!rotate", Some(&agent));
        assert!(!is_owner_control_command(&wrong_kind, 1, "!rotate", &agent));

        let wrong_content = make_event(KIND_STREAM_MESSAGE, "!cancel", Some(&agent));
        assert!(!is_owner_control_command(
            &wrong_content,
            KIND_STREAM_MESSAGE,
            "!rotate",
            &agent
        ));

        let no_mention = make_event(KIND_STREAM_MESSAGE, "!rotate", None);
        assert!(!is_owner_control_command(
            &no_mention,
            KIND_STREAM_MESSAGE,
            "!rotate",
            &agent
        ));
    }

    #[test]
    fn mode_gate_signal_maps_handling_to_control_signal() {
        let owner = "a".repeat(64);
        let other = "b".repeat(64);

        // Queue: never signals — events wait for the turn to finish.
        assert!(mode_gate_signal(MultipleEventHandling::Queue, &owner, Some(&owner)).is_none());

        // Steer: always steers (eligibility already enforced upstream).
        assert!(matches!(
            mode_gate_signal(MultipleEventHandling::Steer, &other, Some(&owner)),
            Some(ControlSignal::Steer)
        ));
        // Steer even when owner is unknown — gate doesn't re-check authorship.
        assert!(matches!(
            mode_gate_signal(MultipleEventHandling::Steer, &other, None),
            Some(ControlSignal::Steer)
        ));

        // Interrupt: always interrupts for any eligible author.
        assert!(matches!(
            mode_gate_signal(MultipleEventHandling::Interrupt, &other, Some(&owner)),
            Some(ControlSignal::Interrupt)
        ));

        // OwnerInterrupt: interrupts only for the owner.
        assert!(matches!(
            mode_gate_signal(MultipleEventHandling::OwnerInterrupt, &owner, Some(&owner)),
            Some(ControlSignal::Interrupt)
        ));
        assert!(
            mode_gate_signal(MultipleEventHandling::OwnerInterrupt, &other, Some(&owner)).is_none(),
            "owner-interrupt must not fire for a non-owner author"
        );
        assert!(
            mode_gate_signal(MultipleEventHandling::OwnerInterrupt, &owner, None).is_none(),
            "owner-interrupt must not fire when the owner is unknown"
        );
    }

    #[tokio::test]
    async fn signal_in_flight_task_sends_rotate_once() {
        let mut pool = AgentPool::from_slots(vec![]);
        let channel_id = Uuid::new_v4();
        let other_channel_id = Uuid::new_v4();
        let (control_tx, control_rx) = tokio::sync::oneshot::channel();

        let abort_handle = pool.join_set.spawn(async {});
        pool.task_map_mut().insert(
            abort_handle.id(),
            pool::TaskMeta {
                agent_index: 0,
                channel_id: Some(channel_id),
                turn_id: "test-turn-id".to_string(),
                recoverable_batch: None,
                control_tx: Some(control_tx),
                steer_tx: None,
            },
        );

        assert!(!signal_in_flight_task(
            &mut pool,
            other_channel_id,
            ControlSignal::Rotate
        ));
        assert!(signal_in_flight_task(
            &mut pool,
            channel_id,
            ControlSignal::Rotate
        ));
        assert_eq!(control_rx.await.unwrap(), ControlSignal::Rotate);
        assert!(!signal_in_flight_task(
            &mut pool,
            channel_id,
            ControlSignal::Rotate
        ));
    }
}

#[cfg(test)]
mod owner_cache_tests {
    use super::*;

    #[test]
    fn new_with_some_caches_immediately() {
        let cache = OwnerCache::new(Some("abcd".into()));
        assert_eq!(cache.get(), Some("abcd"));
    }

    #[test]
    fn new_with_none_returns_none() {
        let cache = OwnerCache::new(None);
        assert!(cache.get().is_none());
    }

    #[test]
    fn get_returns_cached_value() {
        let cache = OwnerCache::new(Some("ab".repeat(32)));
        assert_eq!(cache.get(), Some("ab".repeat(32)).as_deref());
    }
}

#[cfg(test)]
mod author_gate_tests {
    use super::*;

    /// A `RestClient` for tests. The author-gate decisions exercised here all
    /// resolve from the owner pubkey or sibling cache before any HTTP call, so
    /// this client is never actually used to make a request.
    fn dummy_rest_client() -> relay::RestClient {
        relay::RestClient {
            http: reqwest::Client::new(),
            base_url: "http://localhost:0".into(),
            keys: nostr::Keys::generate(),
            auth_tag_json: None,
        }
    }

    const OWNER: &str = "00";
    const SIBLING: &str = "11";
    const EXTERNAL: &str = "22";
    const STRANGER: &str = "33";

    /// Owner + a known sibling, none of them on the explicit allowlist.
    fn cache_with_sibling() -> OwnerCache {
        let cache = OwnerCache::new(Some(OWNER.into()));
        cache.cache_sibling(SIBLING.into(), true);
        cache.cache_sibling(STRANGER.into(), false);
        cache.cache_sibling(EXTERNAL.into(), false);
        cache
    }

    #[tokio::test]
    async fn test_allowlist_accepts_sibling_not_in_allowlist() {
        let cache = cache_with_sibling();
        let allowlist = HashSet::from([EXTERNAL.to_string()]);
        assert!(
            author_allowed(
                &RespondTo::Allowlist,
                &allowlist,
                SIBLING,
                false,
                &cache,
                &dummy_rest_client()
            )
            .await,
            "a same-owner sibling must fire a turn under Allowlist even when not listed"
        );
    }

    #[tokio::test]
    async fn test_allowlist_accepts_explicit_external_pubkey() {
        let cache = cache_with_sibling();
        let allowlist = HashSet::from([EXTERNAL.to_string()]);
        assert!(
            author_allowed(
                &RespondTo::Allowlist,
                &allowlist,
                EXTERNAL,
                false,
                &cache,
                &dummy_rest_client()
            )
            .await,
            "an explicitly allowlisted external pubkey must still be accepted"
        );
    }

    #[tokio::test]
    async fn test_allowlist_rejects_non_sibling_not_in_allowlist() {
        let cache = cache_with_sibling();
        let allowlist = HashSet::from([EXTERNAL.to_string()]);
        assert!(
            !author_allowed(
                &RespondTo::Allowlist,
                &allowlist,
                STRANGER,
                false,
                &cache,
                &dummy_rest_client()
            )
            .await,
            "a non-sibling absent from the allowlist must be dropped"
        );
    }

    #[tokio::test]
    async fn test_allowlist_accepts_owner() {
        let cache = cache_with_sibling();
        let allowlist = HashSet::new();
        assert!(
            author_allowed(
                &RespondTo::Allowlist,
                &allowlist,
                OWNER,
                false,
                &cache,
                &dummy_rest_client()
            )
            .await,
            "the owner must always be accepted under Allowlist"
        );
    }

    // The default `respond-to` is OwnerOnly. Under steering, "an ineligible
    // author must NOT steer" is enforced *here* — author_allowed drops the
    // event before it reaches the mode gate — not in the gate itself. These
    // pin that invariant against the default mode.
    #[tokio::test]
    async fn test_owner_only_rejects_stranger_so_no_steer() {
        let cache = cache_with_sibling();
        assert!(
            !author_allowed(
                &RespondTo::OwnerOnly,
                &HashSet::new(),
                STRANGER,
                false,
                &cache,
                &dummy_rest_client()
            )
            .await,
            "under the default OwnerOnly, a stranger must be dropped — so it can never reach the mode gate to steer"
        );
    }

    #[tokio::test]
    async fn test_owner_only_admits_owner_and_sibling_to_steer() {
        let cache = cache_with_sibling();
        for (who, label) in [(OWNER, "owner"), (SIBLING, "sibling")] {
            assert!(
                author_allowed(
                    &RespondTo::OwnerOnly,
                    &HashSet::new(),
                    who,
                    false,
                    &cache,
                    &dummy_rest_client()
                )
                .await,
                "under default OwnerOnly, the {label} must be admitted so steering can fire"
            );
        }
    }

    // ── DM hardening ──────────────────────────────────────────────────────
    //
    // In a DM, clients auto-p-tag every participant, and an agent can be
    // asked to open a DM with a third party. The gate must therefore ignore
    // the allowlist and `anyone` mode inside DMs: only owner + verified
    // siblings fire turns.

    #[tokio::test]
    async fn test_dm_rejects_allowlisted_external_pubkey() {
        let cache = cache_with_sibling();
        let allowlist = HashSet::from([EXTERNAL.to_string()]);
        assert!(
            !author_allowed(
                &RespondTo::Allowlist,
                &allowlist,
                EXTERNAL,
                true,
                &cache,
                &dummy_rest_client()
            )
            .await,
            "an allowlisted external pubkey must NOT fire a turn inside a DM"
        );
    }

    #[tokio::test]
    async fn test_dm_rejects_stranger_under_anyone() {
        let cache = cache_with_sibling();
        assert!(
            !author_allowed(
                &RespondTo::Anyone,
                &HashSet::new(),
                STRANGER,
                true,
                &cache,
                &dummy_rest_client()
            )
            .await,
            "respond_to=anyone must still drop non-owner authors inside a DM"
        );
    }

    #[tokio::test]
    async fn test_dm_admits_owner_and_sibling_in_every_responding_mode() {
        let cache = cache_with_sibling();
        for mode in [
            RespondTo::OwnerOnly,
            RespondTo::Allowlist,
            RespondTo::Anyone,
        ] {
            for (who, label) in [(OWNER, "owner"), (SIBLING, "sibling")] {
                assert!(
                    author_allowed(
                        &mode,
                        &HashSet::new(),
                        who,
                        true,
                        &cache,
                        &dummy_rest_client()
                    )
                    .await,
                    "in a DM under {mode}, the {label} must still be admitted"
                );
            }
        }
    }

    #[tokio::test]
    async fn test_dm_nobody_rejects_even_owner() {
        let cache = cache_with_sibling();
        assert!(
            !author_allowed(
                &RespondTo::Nobody,
                &HashSet::new(),
                OWNER,
                true,
                &cache,
                &dummy_rest_client()
            )
            .await,
            "respond_to=nobody must drop everything, DMs included"
        );
    }

    // ── is_dm_channel resolution ──────────────────────────────────────────

    fn resolver(startup: HashMap<Uuid, relay::ChannelInfo>) -> pool::ChannelInfoResolver {
        pool::ChannelInfoResolver::new(startup, dummy_rest_client())
    }

    #[tokio::test]
    async fn test_is_dm_channel_uses_definitive_startup_metadata() {
        let dm_id = Uuid::new_v4();
        let stream_id = Uuid::new_v4();
        let startup = HashMap::from([
            (
                dm_id,
                relay::ChannelInfo {
                    name: "dm".into(),
                    channel_type: "dm".into(),
                },
            ),
            (
                stream_id,
                relay::ChannelInfo {
                    name: "stream".into(),
                    channel_type: "stream".into(),
                },
            ),
        ]);
        let resolver = resolver(startup);
        assert!(is_dm_channel(dm_id, &resolver).await);
        assert!(!is_dm_channel(stream_id, &resolver).await);
    }

    #[tokio::test]
    async fn test_is_dm_channel_fails_closed_for_unknown_startup_metadata() {
        let id = Uuid::new_v4();
        let startup = HashMap::from([(
            id,
            relay::ChannelInfo {
                name: "unknown".into(),
                channel_type: "unknown".into(),
            },
        )]);
        assert!(
            is_dm_channel(id, &resolver(startup)).await,
            "missing startup metadata must not be trusted as a stream"
        );
    }

    async fn lazy_resolver_with_response(
        response: serde_json::Value,
    ) -> (
        pool::ChannelInfoResolver,
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
        tokio::task::JoinHandle<()>,
    ) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test HTTP server");
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let requests = std::sync::Arc::new(AtomicUsize::new(0));
        let server_requests = requests.clone();
        let body = response.to_string();
        let server = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let mut request = vec![0; 8192];
                let _ = socket.read(&mut request).await;
                server_requests.fetch_add(1, Ordering::SeqCst);
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = socket.write_all(response.as_bytes()).await;
            }
        });
        let rest = relay::RestClient {
            http: reqwest::Client::new(),
            base_url,
            keys: nostr::Keys::generate(),
            auth_tag_json: None,
        };
        (
            pool::ChannelInfoResolver::new(HashMap::new(), rest),
            requests,
            server,
        )
    }

    #[tokio::test]
    async fn test_is_dm_channel_lazy_resolves_declared_dm_and_caches_it() {
        use std::sync::atomic::Ordering;

        let id = Uuid::new_v4();
        let response = serde_json::json!([{
            "tags": [["d", id.to_string()], ["name", "DM"], ["t", "dm"]]
        }]);
        let (resolver, requests, server) = lazy_resolver_with_response(response).await;

        assert!(is_dm_channel(id, &resolver).await);
        assert!(is_dm_channel(id, &resolver).await);
        assert_eq!(
            requests.load(Ordering::SeqCst),
            1,
            "second resolution uses cache"
        );
        server.abort();
    }

    #[tokio::test]
    async fn test_discovery_without_metadata_stays_fail_closed_at_author_gate() {
        let id = Uuid::new_v4();
        let discovered = relay::merge_discovered_channels(vec![id], &serde_json::json!([]));
        let channel_info = resolver(discovered);
        let owner_cache = cache_with_sibling();
        let allowlist = HashSet::from([EXTERNAL.to_string()]);

        let is_dm = is_dm_channel(id, &channel_info).await;
        assert!(is_dm, "unknown startup metadata must fail closed as DM");
        assert!(
            !author_allowed(
                &RespondTo::Allowlist,
                &allowlist,
                EXTERNAL,
                is_dm,
                &owner_cache,
                &dummy_rest_client(),
            )
            .await,
            "an external author must not pass when startup discovery omitted metadata"
        );
    }

    #[tokio::test]
    async fn test_is_dm_channel_fails_closed_when_lazy_resolution_fails() {
        assert!(
            is_dm_channel(Uuid::new_v4(), &resolver(HashMap::new())).await,
            "an unresolvable channel type must be treated as a DM"
        );
    }
}

#[cfg(test)]
mod observer_snapshot_race_tests {
    use super::*;
    use nostr::Keys;

    fn emit_marker(observer: &observer::ObserverHandle, marker: &str) {
        observer.emit(
            "test_event",
            None,
            &observer::context_for(None, None, None),
            serde_json::json!({ "marker": marker }),
        );
    }

    /// An event emitted between `subscribe()` and `snapshot()` lands in BOTH
    /// the snapshot and the live receiver; the seq high-water dedupe must
    /// deliver it exactly once — and never lose events on either side of it.
    #[tokio::test(start_paused = true)]
    async fn overlap_between_subscribe_and_snapshot_publishes_exactly_once() {
        let observer = observer::ObserverHandle::in_process();
        let agent_keys = Keys::generate();
        let owner_keys = Keys::generate();
        let (publisher, mut published_rx) = RelayEventPublisher::test_pair();

        // Before the publisher starts: replay-buffer only.
        emit_marker(&observer, "before");
        // The race window: emitted after subscribe() but before snapshot(),
        // so it is present in the snapshot AND queued on the receiver.
        let rx = observer.subscribe();
        emit_marker(&observer, "overlap");
        let snapshot = observer.snapshot();
        assert_eq!(snapshot.len(), 2, "overlap event must be in the snapshot");
        // After the snapshot: live receiver only.
        emit_marker(&observer, "after");
        // Close the broadcast channel so the run loop drains and exits.
        drop(observer);

        run_relay_observer_publisher(
            snapshot,
            rx,
            publisher,
            agent_keys.clone(),
            agent_keys.public_key().to_hex(),
            owner_keys.public_key().to_hex(),
            owner_keys.public_key(),
        )
        .await;

        // The run loop has exited, dropping the publisher; drain the forwarded
        // events until the channel closes (deterministic — no try_recv race
        // with the test_pair forwarding task).
        let mut markers = Vec::new();
        while let Some(event) = published_rx.recv().await {
            let payload: serde_json::Value =
                decrypt_observer_payload(&owner_keys, &event).expect("decrypt published frame");
            markers.push(payload["payload"]["marker"].as_str().unwrap().to_string());
        }
        assert_eq!(
            markers,
            ["before", "overlap", "after"],
            "each event must be published exactly once, in order"
        );
    }
}

#[cfg(test)]
mod observer_publish_pacer_tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn starts_without_a_burst_and_spaces_frames() {
        let started = tokio::time::Instant::now();
        let mut pacer = ObserverPublishPacer::new();

        pacer.wait().await;
        let first = tokio::time::Instant::now();
        pacer.wait().await;
        let second = tokio::time::Instant::now();

        assert_eq!(first.duration_since(started), OBSERVER_PUBLISH_INTERVAL);
        assert_eq!(second.duration_since(first), OBSERVER_PUBLISH_INTERVAL);
    }

    #[tokio::test(start_paused = true)]
    async fn limits_frames_in_each_rolling_minute() {
        let mut pacer = ObserverPublishPacer::new();
        pacer.wait().await;
        let first = tokio::time::Instant::now();
        for _ in 1..OBSERVER_PUBLISH_LIMIT_PER_MINUTE {
            pacer.wait().await;
        }

        pacer.wait().await;
        let ninety_first = tokio::time::Instant::now();

        assert_eq!(ninety_first.duration_since(first), Duration::from_secs(60));
    }
}

#[cfg(test)]
mod observer_chunk_coalescer_tests {
    use super::*;

    fn chunk_event(
        seq: u64,
        update_type: &str,
        message_id: &str,
        text: &str,
    ) -> observer::ObserverEvent {
        observer::ObserverEvent {
            seq,
            timestamp: format!("2026-04-29T04:00:0{seq}Z"),
            kind: "acp_read".to_string(),
            agent_index: Some(0),
            channel_id: Some("channel-1".to_string()),
            session_id: Some("session-1".to_string()),
            turn_id: Some("turn-1".to_string()),
            started_at: None,
            payload: serde_json::json!({
                "jsonrpc": "2.0",
                "method": "session/update",
                "params": {
                    "sessionId": "session-1",
                    "update": {
                        "sessionUpdate": update_type,
                        "messageId": message_id,
                        "content": {
                            "type": "text",
                            "text": text,
                        },
                    },
                },
            }),
        }
    }

    fn non_chunk_event(seq: u64) -> observer::ObserverEvent {
        observer::ObserverEvent {
            seq,
            timestamp: format!("2026-04-29T04:00:0{seq}Z"),
            kind: "turn_started".to_string(),
            agent_index: Some(0),
            channel_id: Some("channel-1".to_string()),
            session_id: Some("session-1".to_string()),
            turn_id: Some("turn-1".to_string()),
            started_at: None,
            payload: serde_json::json!({ "type": "turn_started" }),
        }
    }

    fn chunk_text(event: &observer::ObserverEvent) -> &str {
        event.payload["params"]["update"]["content"]["text"]
            .as_str()
            .expect("chunk text")
    }

    #[test]
    fn coalesces_chunks_until_non_chunk_event() {
        let mut coalescer = ObserverChunkCoalescer::default();

        assert!(coalescer
            .ingest(chunk_event(1, "agent_message_chunk", "message-1", "hello "))
            .is_empty());
        assert!(coalescer
            .ingest(chunk_event(2, "agent_message_chunk", "message-1", "world"))
            .is_empty());

        let events = coalescer.ingest(non_chunk_event(3));
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].seq, 2);
        assert_eq!(chunk_text(&events[0]), "hello world");
        assert_eq!(events[1].kind, "turn_started");
    }

    #[test]
    fn keeps_independent_chunk_streams_separate() {
        let mut coalescer = ObserverChunkCoalescer::default();

        assert!(coalescer
            .ingest(chunk_event(1, "agent_message_chunk", "message-1", "answer"))
            .is_empty());
        assert!(coalescer
            .ingest(chunk_event(
                2,
                "agent_thought_chunk",
                "thought-1",
                "thinking"
            ))
            .is_empty());

        let events = coalescer.flush();
        assert_eq!(events.len(), 2);
        assert_eq!(chunk_text(&events[0]), "answer");
        assert_eq!(chunk_text(&events[1]), "thinking");
    }
}

#[cfg(test)]
mod build_mcp_servers_tests {
    use super::*;
    use std::sync::Mutex;

    /// Env-var-touching tests must run serially — env vars are process-global.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn test_config() -> Config {
        Config {
            keys: nostr::Keys::generate(),
            relay_url: "ws://localhost:3000".into(),
            agent_command: "goose".into(),
            agent_args: vec!["acp".into()],
            mcp_command: "test-mcp-server".into(),
            idle_timeout_secs: config::DEFAULT_IDLE_TIMEOUT_SECS,
            max_turn_duration_secs: config::DEFAULT_MAX_TURN_DURATION_SECS,
            agents: 1,
            heartbeat_interval_secs: 0,
            turn_liveness_secs: 10,
            heartbeat_prompt: None,
            system_prompt: None,
            team_instructions: None,
            initial_message: None,
            subscribe_mode: config::SubscribeMode::All,
            dedup_mode: config::DedupMode::Queue,
            multiple_event_handling: config::MultipleEventHandling::Queue,
            ignore_self: true,
            kinds_override: None,
            channels_override: None,
            no_mention_filter: false,
            config_path: std::path::PathBuf::from("./buzz-acp.toml"),
            context_message_limit: 12,
            max_turns_per_session: 0,
            presence_enabled: true,
            typing_enabled: true,
            memory_enabled: false,
            model: None,
            session_title: None,
            permission_mode: config::PermissionMode::BypassPermissions,
            respond_to: config::RespondTo::Anyone,
            respond_to_allowlist: std::collections::HashSet::new(),
            allowed_respond_to: vec![],
            persona_env_vars: vec![],
            has_generated_codex_config: false,
            relay_observer: false,
            lazy_pool: false,
            project_routing_enabled: false,
            peer_agents: HashSet::new(),
            agent_owner: None,
            no_base_prompt: false,
            base_prompt_content: None,
        }
    }

    #[test]
    fn session_new_mcp_server_has_required_fields() {
        let config = test_config();
        let servers = build_mcp_servers(&config);
        assert_eq!(servers.len(), 1);
        let server = &servers[0];
        assert_eq!(server.name, "test-mcp-server");

        let names: Vec<&str> = server.env.iter().map(|e| e.name.as_str()).collect();
        assert!(
            names.contains(&"BUZZ_RELAY_URL"),
            "missing BUZZ_RELAY_URL; got {names:?}"
        );
        assert!(
            names.contains(&"BUZZ_PRIVATE_KEY"),
            "missing BUZZ_PRIVATE_KEY; got {names:?}"
        );
    }

    #[test]
    fn session_new_mcp_server_forwards_buzz_auth_tag() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::set_var("BUZZ_AUTH_TAG", "test-attestation-tag");
        let config = test_config();
        let servers = build_mcp_servers(&config);
        std::env::remove_var("BUZZ_AUTH_TAG");

        let server = &servers[0];
        let auth_tag_env = server.env.iter().find(|e| e.name == "BUZZ_AUTH_TAG");
        assert!(
            auth_tag_env.is_some(),
            "BUZZ_AUTH_TAG should be forwarded when set"
        );
        assert_eq!(auth_tag_env.unwrap().value, "test-attestation-tag");
    }

    #[test]
    fn session_new_mcp_server_skips_empty_buzz_auth_tag() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::set_var("BUZZ_AUTH_TAG", "");
        let config = test_config();
        let servers = build_mcp_servers(&config);
        std::env::remove_var("BUZZ_AUTH_TAG");

        let server = &servers[0];
        let has_auth_tag = server.env.iter().any(|e| e.name == "BUZZ_AUTH_TAG");
        assert!(!has_auth_tag, "empty BUZZ_AUTH_TAG should not be forwarded");
    }

    #[test]
    fn test_display_name_set_is_forwarded_to_mcp_server() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::set_var("BUZZ_ACP_DISPLAY_NAME", "Duncan");
        let config = test_config();
        let servers = build_mcp_servers(&config);
        std::env::remove_var("BUZZ_ACP_DISPLAY_NAME");

        let entry = servers[0]
            .env
            .iter()
            .find(|e| e.name == "BUZZ_ACP_DISPLAY_NAME");
        assert_eq!(
            entry.map(|e| e.value.as_str()),
            Some("Duncan"),
            "a set display name should reach the MCP server verbatim"
        );
    }

    #[test]
    fn test_display_name_unset_omits_the_key_entirely() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var("BUZZ_ACP_DISPLAY_NAME");
        let config = test_config();
        let servers = build_mcp_servers(&config);

        // Absent, not empty-valued: dev-mcp distinguishes the two and only
        // falls back to the npub when the key is missing or blank.
        assert!(
            !servers[0]
                .env
                .iter()
                .any(|e| e.name == "BUZZ_ACP_DISPLAY_NAME"),
            "unset display name should not add the key"
        );
    }

    #[test]
    fn test_display_name_empty_omits_the_key_entirely() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::set_var("BUZZ_ACP_DISPLAY_NAME", "");
        let config = test_config();
        let servers = build_mcp_servers(&config);
        std::env::remove_var("BUZZ_ACP_DISPLAY_NAME");

        assert!(
            !servers[0]
                .env
                .iter()
                .any(|e| e.name == "BUZZ_ACP_DISPLAY_NAME"),
            "empty display name should not be forwarded"
        );
    }

    #[test]
    fn empty_mcp_command_returns_no_servers() {
        let mut config = test_config();
        config.mcp_command = "".into();
        let servers = build_mcp_servers(&config);
        assert!(
            servers.is_empty(),
            "empty mcp_command should produce no MCP servers"
        );
    }

    #[test]
    fn absolute_path_mcp_command_uses_file_stem_as_name() {
        let mut config = test_config();
        config.mcp_command = "/opt/bin/my-mcp-server".into();
        let servers = build_mcp_servers(&config);
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].name, "my-mcp-server");
    }

    #[test]
    fn mcp_command_with_no_stem_falls_back_to_mcp() {
        // Path::new("").file_stem() returns None — exercises the unwrap_or("mcp") path.
        let mut config = test_config();
        config.mcp_command = "".into();
        // Empty command returns no servers; test the stem logic directly.
        assert_eq!(
            std::path::Path::new("")
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("mcp"),
            "mcp"
        );

        // Confirm a non-empty command with no stem (e.g. just a dot) also falls back.
        config.mcp_command = ".".into();
        let servers = build_mcp_servers(&config);
        assert_eq!(servers.len(), 1);
        assert_eq!(
            servers[0].name, "mcp",
            "Path::new(\".\").file_stem() is None — should fall back to \"mcp\""
        );
    }
}

#[cfg(test)]
mod error_outcome_emission_tests {
    //! Pins the policy that error-class outcomes surface to the activity feed
    //! and never to the channel:
    //!
    //! - Channel silence is enforced *structurally* — `handle_prompt_result`
    //!   takes no relay handle, so it has no way to post a channel message. A
    //!   future re-introduction of channel notices would have to add the relay
    //!   parameter back, which these tests' construction would then refuse to
    //!   compile against.
    //! - Feed coverage is the regression-prone half and is asserted at runtime:
    //!   each error outcome must emit exactly one `turn_error` observer event.
    //!   If any branch drops its `emit_turn_error` call, the matching test goes
    //!   red.

    use super::*;
    use crate::acp::{AcpClient, AcpError};
    use crate::observer::ObserverHandle;
    use crate::pool::{
        AgentPool, OwnedAgent, PromptOutcome, PromptResult, PromptSource, TimeoutKind,
    };
    use crate::queue::{BatchEvent, FlushBatch};
    use nostr::{EventBuilder, Keys, Kind};
    use std::collections::HashSet;

    fn test_config() -> Config {
        Config {
            keys: nostr::Keys::generate(),
            relay_url: "ws://localhost:3000".into(),
            // `true` exits cleanly, so the async respawn fails fast and
            // harmlessly off the JoinSet — irrelevant to the synchronous
            // feed emission under test.
            agent_command: "true".into(),
            agent_args: vec![],
            mcp_command: "test-mcp-server".into(),
            idle_timeout_secs: config::DEFAULT_IDLE_TIMEOUT_SECS,
            max_turn_duration_secs: config::DEFAULT_MAX_TURN_DURATION_SECS,
            agents: 1,
            heartbeat_interval_secs: 0,
            turn_liveness_secs: 10,
            heartbeat_prompt: None,
            system_prompt: None,
            team_instructions: None,
            initial_message: None,
            subscribe_mode: config::SubscribeMode::All,
            dedup_mode: config::DedupMode::Queue,
            multiple_event_handling: config::MultipleEventHandling::Queue,
            ignore_self: true,
            kinds_override: None,
            channels_override: None,
            no_mention_filter: false,
            config_path: std::path::PathBuf::from("./buzz-acp.toml"),
            context_message_limit: 12,
            max_turns_per_session: 0,
            presence_enabled: true,
            typing_enabled: true,
            memory_enabled: false,
            model: None,
            session_title: None,
            permission_mode: config::PermissionMode::BypassPermissions,
            respond_to: config::RespondTo::Anyone,
            respond_to_allowlist: HashSet::new(),
            allowed_respond_to: vec![],
            persona_env_vars: vec![],
            has_generated_codex_config: false,
            relay_observer: false,
            lazy_pool: false,
            project_routing_enabled: false,
            peer_agents: HashSet::new(),
            agent_owner: None,
            no_base_prompt: false,
            base_prompt_content: None,
        }
    }

    #[test]
    fn normalizes_agent_name_from_initialize_result() {
        assert_eq!(
            normalized_agent_name(&serde_json::json!({
                "agentInfo": { "name": " Goose ", "version": "1.43.0" }
            })),
            "goose"
        );
        assert_eq!(
            normalized_agent_name(&serde_json::json!({
                "serverInfo": { "name": "buzz-agent" }
            })),
            "buzz-agent"
        );
    }

    /// Spawn a real but inert agent subprocess (`cat`) so the error paths have
    /// an `OwnedAgent` to move into respawn or return to the pool. The error
    /// branches never talk to the subprocess.
    async fn dummy_agent(index: usize) -> OwnedAgent {
        OwnedAgent {
            index,
            acp: AcpClient::spawn("cat", &[], &[], false)
                .await
                .expect("spawn cat as inert agent"),
            state: Default::default(),
            model_capabilities: None,
            desired_model: None,
            model_overridden: false,
            agent_name: "unknown".into(),
            goose_system_prompt_supported: None,
            // Error branches under test never read this; 1 is the legacy
            // non-systemPrompt path, the simplest valid value.
            protocol_version: 1,
        }
    }

    /// Drive one error outcome through `handle_prompt_result` and return how
    /// many `turn_error` events it emitted to the observer feed.
    async fn turn_errors_emitted_for(outcome: PromptOutcome) -> usize {
        let agent = dummy_agent(0).await;
        let mut pool = AgentPool::from_slots(vec![None]);

        // `handle_prompt_result` asserts it removes exactly one in-flight task
        // for the completing agent (the slot was checked out, not idle). Mirror
        // the real dispatch path by registering a TaskMeta keyed on a genuine
        // `task::Id` — only obtainable from inside a spawned task.
        let task_id = pool.join_set.spawn(async {}).id();
        pool.task_map_mut().insert(
            task_id,
            crate::pool::TaskMeta {
                agent_index: 0,
                channel_id: None,
                turn_id: "test-turn-id".to_string(),
                recoverable_batch: None,
                control_tx: None,
                steer_tx: None,
            },
        );

        let mut queue = EventQueue::new(config::DedupMode::Queue);
        let config = test_config();
        let mut heartbeat_in_flight = false;
        let removed_channels = HashSet::new();
        let mut crash_history = vec![SlotCircuit {
            crash_times: Vec::new(),
            open_until: None,
            respawn_in_flight: false,
        }];
        let (respawn_tx, _respawn_rx) = mpsc::channel(8);
        let mut respawn_tasks = tokio::task::JoinSet::new();
        let observer = ObserverHandle::in_process();

        let result = PromptResult {
            agent,
            source: PromptSource::Channel(Uuid::new_v4()),
            turn_id: "test-turn-id".to_string(),
            outcome,
            batch: None,
        };

        handle_prompt_result(
            &mut pool,
            &mut queue,
            &config,
            result,
            &mut heartbeat_in_flight,
            &removed_channels,
            &mut crash_history,
            &respawn_tx,
            &mut respawn_tasks,
            Some(observer.clone()),
            None,
        );

        let turn_errors: Vec<_> = observer
            .snapshot()
            .into_iter()
            .filter(|e| e.kind == "turn_error")
            .collect();
        assert!(
            turn_errors
                .iter()
                .all(|event| event.turn_id.as_deref() == Some("test-turn-id")),
            "turn_error must retain the completed turn id"
        );
        turn_errors.len()
    }

    #[tokio::test]
    async fn agent_exited_emits_exactly_one_feed_event() {
        assert_eq!(turn_errors_emitted_for(PromptOutcome::AgentExited).await, 1);
    }

    #[tokio::test]
    async fn panic_event_retains_task_turn_id() {
        let mut pool = AgentPool::from_slots(vec![]);
        let channel_id = Uuid::new_v4();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let abort_handle = pool.join_set.spawn(async move {
            let _ = started_tx.send(());
            std::future::pending::<()>().await;
        });
        let task_id = abort_handle.id();
        pool.task_map_mut().insert(
            task_id,
            crate::pool::TaskMeta {
                agent_index: 0,
                channel_id: Some(channel_id),
                turn_id: "panic-turn-id".to_string(),
                recoverable_batch: None,
                control_tx: None,
                steer_tx: None,
            },
        );
        started_rx.await.unwrap();
        abort_handle.abort();
        let join_error = pool.join_set.join_next().await.unwrap().unwrap_err();

        let mut queue = EventQueue::new(config::DedupMode::Queue);
        let config = test_config();
        let mut heartbeat_in_flight = false;
        let removed_channels = HashSet::new();
        let mut typing_channels = HashMap::new();
        let mut crash_history = vec![SlotCircuit {
            crash_times: Vec::new(),
            open_until: None,
            respawn_in_flight: false,
        }];
        let (respawn_tx, _respawn_rx) = mpsc::channel(8);
        let mut respawn_tasks = tokio::task::JoinSet::new();
        let observer = ObserverHandle::in_process();

        recover_panicked_agent(
            &mut pool,
            &mut queue,
            &config,
            join_error,
            &mut heartbeat_in_flight,
            &removed_channels,
            &mut typing_channels,
            &mut crash_history,
            &respawn_tx,
            &mut respawn_tasks,
            Some(observer.clone()),
        );

        let panic = observer
            .snapshot()
            .into_iter()
            .find(|event| event.kind == "agent_panic")
            .expect("panic recovery emits an observer event");
        assert_eq!(
            panic.channel_id.as_deref(),
            Some(channel_id.to_string().as_str())
        );
        assert_eq!(panic.turn_id.as_deref(), Some("panic-turn-id"));
    }

    #[tokio::test]
    async fn idle_timeout_emits_exactly_one_feed_event() {
        assert_eq!(
            turn_errors_emitted_for(PromptOutcome::Timeout(TimeoutKind::Idle)).await,
            1
        );
    }

    #[tokio::test]
    async fn hard_timeout_emits_exactly_one_feed_event() {
        assert_eq!(
            turn_errors_emitted_for(PromptOutcome::Timeout(TimeoutKind::Hard {
                recently_active: false
            }))
            .await,
            1
        );
    }

    #[tokio::test]
    async fn cancel_drain_timeout_emits_exactly_one_feed_event() {
        assert_eq!(
            turn_errors_emitted_for(PromptOutcome::CancelDrainTimeout(
                std::time::Duration::from_secs(5)
            ))
            .await,
            1
        );
    }

    /// idle_timeout outcome_label is "idle_timeout"; hard_timeout is "hard_timeout".
    #[tokio::test]
    async fn timeout_outcome_labels_differ() {
        let check_label = |outcome: PromptOutcome, expected_label: &'static str| async move {
            let agent = dummy_agent(0).await;
            let mut pool = AgentPool::from_slots(vec![None]);
            let task_id = pool.join_set.spawn(async {}).id();
            pool.task_map_mut().insert(
                task_id,
                crate::pool::TaskMeta {
                    agent_index: 0,
                    channel_id: None,
                    turn_id: "test-turn-id".to_string(),
                    recoverable_batch: None,
                    control_tx: None,
                    steer_tx: None,
                },
            );
            let mut queue = EventQueue::new(config::DedupMode::Queue);
            let config = test_config();
            let mut heartbeat_in_flight = false;
            let removed_channels = HashSet::new();
            let mut crash_history = vec![SlotCircuit {
                crash_times: Vec::new(),
                open_until: None,
                respawn_in_flight: false,
            }];
            let (respawn_tx, _respawn_rx) = mpsc::channel(8);
            let mut respawn_tasks = tokio::task::JoinSet::new();
            let observer = ObserverHandle::in_process();
            let result = PromptResult {
                agent,
                source: PromptSource::Channel(Uuid::new_v4()),
                turn_id: "test-turn-id".to_string(),
                outcome,
                batch: None,
            };
            handle_prompt_result(
                &mut pool,
                &mut queue,
                &config,
                result,
                &mut heartbeat_in_flight,
                &removed_channels,
                &mut crash_history,
                &respawn_tx,
                &mut respawn_tasks,
                Some(observer.clone()),
                None,
            );
            let events = observer.snapshot();
            let turn_error = events.iter().find(|e| e.kind == "turn_error").unwrap();
            assert_eq!(
                turn_error.payload["outcome"].as_str().unwrap(),
                expected_label
            );
        };
        check_label(PromptOutcome::Timeout(TimeoutKind::Idle), "idle_timeout").await;
        check_label(
            PromptOutcome::Timeout(TimeoutKind::Hard {
                recently_active: false,
            }),
            "hard_timeout",
        )
        .await;
        check_label(
            PromptOutcome::CancelDrainTimeout(std::time::Duration::from_secs(5)),
            "cancel_drain_timeout",
        )
        .await;
    }

    /// hard-cap timeout dead-letters immediately (no requeue); idle timeout is requeued.
    #[tokio::test]
    async fn hard_timeout_not_requeued_idle_timeout_is_requeued() {
        let make_batch = || {
            let keys = Keys::generate();
            let event = EventBuilder::new(Kind::Custom(9), "test")
                .sign_with_keys(&keys)
                .unwrap();
            FlushBatch {
                channel_id: Uuid::new_v4(),
                events: vec![BatchEvent {
                    event,
                    prompt_tag: "test".into(),
                    received_at: std::time::Instant::now(),
                    project: None,
                }],
                cancelled_events: vec![],
                cancel_reason: None,
            }
        };

        // Returns (pending_channels, queued_event_count_for_channel).
        let run = |outcome: PromptOutcome, batch: FlushBatch| async move {
            let channel_id = batch.channel_id;
            let agent = dummy_agent(0).await;
            let mut pool = AgentPool::from_slots(vec![None]);
            let task_id = pool.join_set.spawn(async {}).id();
            pool.task_map_mut().insert(
                task_id,
                crate::pool::TaskMeta {
                    agent_index: 0,
                    channel_id: None,
                    turn_id: "test-turn-id".to_string(),
                    recoverable_batch: None,
                    control_tx: None,
                    steer_tx: None,
                },
            );
            let mut queue = EventQueue::new(config::DedupMode::Queue);
            let config = test_config();
            let mut heartbeat_in_flight = false;
            let removed_channels = HashSet::new();
            let mut crash_history = vec![SlotCircuit {
                crash_times: Vec::new(),
                open_until: None,
                respawn_in_flight: false,
            }];
            let (respawn_tx, _respawn_rx) = mpsc::channel(8);
            let mut respawn_tasks = tokio::task::JoinSet::new();
            let result = PromptResult {
                agent,
                source: PromptSource::Channel(channel_id),
                turn_id: "test-turn-id".to_string(),
                outcome,
                batch: Some(batch),
            };
            handle_prompt_result(
                &mut pool,
                &mut queue,
                &config,
                result,
                &mut heartbeat_in_flight,
                &removed_channels,
                &mut crash_history,
                &respawn_tx,
                &mut respawn_tasks,
                None,
                None,
            );
            (
                queue.pending_channels(),
                queue.queued_event_count(&channel_id),
            )
        };

        // Hard timeout (not recently active): dead-lettered immediately.
        let hard_batch = make_batch();
        let (hard_channels, hard_events) = run(
            PromptOutcome::Timeout(TimeoutKind::Hard {
                recently_active: false,
            }),
            hard_batch,
        )
        .await;
        assert_eq!(
            hard_channels, 0,
            "hard-cap timeout (not recently active) must not requeue the batch"
        );
        assert_eq!(
            hard_events, 0,
            "hard-cap timeout (not recently active) must drop all events"
        );

        // Idle timeout: batch IS requeued (first attempt, not yet dead-lettered).
        let idle_batch = make_batch();
        let (idle_channels, idle_events) =
            run(PromptOutcome::Timeout(TimeoutKind::Idle), idle_batch).await;
        assert_eq!(
            idle_channels, 1,
            "idle timeout must requeue the batch for retry"
        );
        assert_eq!(
            idle_events, 1,
            "idle timeout must preserve the event for retry"
        );
    }

    #[tokio::test]
    async fn hard_timeout_recently_active_requeues_batch() {
        let channel_id = Uuid::new_v4();
        let make_batch = || {
            let keys = Keys::generate();
            let event = EventBuilder::new(Kind::Custom(9), "test")
                .sign_with_keys(&keys)
                .unwrap();
            FlushBatch {
                channel_id,
                events: vec![BatchEvent {
                    event,
                    prompt_tag: "test".into(),
                    received_at: std::time::Instant::now(),
                    project: None,
                }],
                cancelled_events: vec![],
                cancel_reason: None,
            }
        };

        let run = |outcome: PromptOutcome, batch: FlushBatch| async move {
            let channel_id = batch.channel_id;
            let agent = dummy_agent(0).await;
            let mut pool = AgentPool::from_slots(vec![None]);
            let task_id = pool.join_set.spawn(async {}).id();
            pool.task_map_mut().insert(
                task_id,
                crate::pool::TaskMeta {
                    agent_index: 0,
                    channel_id: None,
                    turn_id: "test-turn-id".to_string(),
                    recoverable_batch: None,
                    control_tx: None,
                    steer_tx: None,
                },
            );
            let mut queue = EventQueue::new(config::DedupMode::Queue);
            let config = test_config();
            let mut heartbeat_in_flight = false;
            let removed_channels = HashSet::new();
            let mut crash_history = vec![SlotCircuit {
                crash_times: Vec::new(),
                open_until: None,
                respawn_in_flight: false,
            }];
            let (respawn_tx, _respawn_rx) = mpsc::channel(8);
            let mut respawn_tasks = tokio::task::JoinSet::new();
            let result = PromptResult {
                agent,
                source: PromptSource::Channel(channel_id),
                turn_id: "test-turn-id".to_string(),
                outcome,
                batch: Some(batch),
            };
            handle_prompt_result(
                &mut pool,
                &mut queue,
                &config,
                result,
                &mut heartbeat_in_flight,
                &removed_channels,
                &mut crash_history,
                &respawn_tx,
                &mut respawn_tasks,
                None,
                None,
            );
            (
                queue.pending_channels(),
                queue.queued_event_count(&channel_id),
            )
        };

        let batch = make_batch();
        let (channels, events) = run(
            PromptOutcome::Timeout(TimeoutKind::Hard {
                recently_active: true,
            }),
            batch,
        )
        .await;
        assert_eq!(
            channels, 1,
            "hard-cap timeout with recent activity must requeue the batch"
        );
        assert_eq!(
            events, 1,
            "hard-cap timeout with recent activity must preserve the event"
        );
    }

    /// The hard-timeout `death_message` must report what actually happened to
    /// the batch, not just the `recently_active` eligibility flag: a
    /// recently-active batch within its retry budget is requeued, so the
    /// observer payload must say so.
    #[tokio::test]
    async fn hard_timeout_recently_active_requeue_success_reports_requeued_for_retry() {
        let channel_id = Uuid::new_v4();
        let agent = dummy_agent(0).await;
        let mut pool = AgentPool::from_slots(vec![None]);
        let task_id = pool.join_set.spawn(async {}).id();
        pool.task_map_mut().insert(
            task_id,
            crate::pool::TaskMeta {
                agent_index: 0,
                channel_id: None,
                turn_id: "test-turn-id".to_string(),
                recoverable_batch: None,
                control_tx: None,
                steer_tx: None,
            },
        );
        let mut queue = EventQueue::new(config::DedupMode::Queue);
        let config = test_config();
        let mut heartbeat_in_flight = false;
        let removed_channels = HashSet::new();
        let mut crash_history = vec![SlotCircuit {
            crash_times: Vec::new(),
            open_until: None,
            respawn_in_flight: false,
        }];
        let (respawn_tx, _respawn_rx) = mpsc::channel(8);
        let mut respawn_tasks = tokio::task::JoinSet::new();
        let observer = ObserverHandle::in_process();
        let batch = FlushBatch {
            channel_id,
            events: vec![BatchEvent {
                event: EventBuilder::new(Kind::Custom(9), "test")
                    .sign_with_keys(&Keys::generate())
                    .unwrap(),
                prompt_tag: "test".into(),
                received_at: std::time::Instant::now(),
                project: None,
            }],
            cancelled_events: vec![],
            cancel_reason: None,
        };
        let result = PromptResult {
            agent,
            source: PromptSource::Channel(channel_id),
            turn_id: "test-turn-id".to_string(),
            outcome: PromptOutcome::Timeout(TimeoutKind::Hard {
                recently_active: true,
            }),
            batch: Some(batch),
        };
        handle_prompt_result(
            &mut pool,
            &mut queue,
            &config,
            result,
            &mut heartbeat_in_flight,
            &removed_channels,
            &mut crash_history,
            &respawn_tx,
            &mut respawn_tasks,
            Some(observer.clone()),
            None,
        );

        let events = observer.snapshot();
        let turn_error = events
            .iter()
            .find(|e| e.kind == "turn_error")
            .expect("exactly one turn_error event must be emitted");
        assert_eq!(
            turn_error.payload["error"].as_str().unwrap(),
            format!(
                "Agent turn exceeded the maximum duration ({}s) — requeued for retry (recently active)",
                config.max_turn_duration_secs
            ),
        );
        assert_eq!(
            queue.pending_channels(),
            1,
            "batch must be requeued, not dead-lettered, while within the retry budget"
        );
    }

    /// Same recently-active hard timeout, but the channel has already
    /// exhausted its retry budget ([`crate::queue::MAX_RETRIES`] prior
    /// attempts) — `queue.requeue()` dead-letters instead of requeueing, and
    /// the observer payload must report that fate, not the requeue wording
    /// above.
    #[tokio::test]
    async fn hard_timeout_recently_active_budget_exhausted_reports_dead_lettered() {
        let channel_id = Uuid::new_v4();
        let mut queue = EventQueue::new(config::DedupMode::Queue);
        // Simulate MAX_RETRIES prior failed attempts on this channel so the
        // upcoming requeue() call in handle_prompt_result crosses the
        // dead-letter threshold.
        queue.set_retry_count_for_test(channel_id, crate::queue::MAX_RETRIES);

        let agent = dummy_agent(0).await;
        let mut pool = AgentPool::from_slots(vec![None]);
        let task_id = pool.join_set.spawn(async {}).id();
        pool.task_map_mut().insert(
            task_id,
            crate::pool::TaskMeta {
                agent_index: 0,
                channel_id: None,
                turn_id: "test-turn-id".to_string(),
                recoverable_batch: None,
                control_tx: None,
                steer_tx: None,
            },
        );
        let config = test_config();
        let mut heartbeat_in_flight = false;
        let removed_channels = HashSet::new();
        let mut crash_history = vec![SlotCircuit {
            crash_times: Vec::new(),
            open_until: None,
            respawn_in_flight: false,
        }];
        let (respawn_tx, _respawn_rx) = mpsc::channel(8);
        let mut respawn_tasks = tokio::task::JoinSet::new();
        let observer = ObserverHandle::in_process();
        let batch = FlushBatch {
            channel_id,
            events: vec![BatchEvent {
                event: EventBuilder::new(Kind::Custom(9), "final-attempt")
                    .sign_with_keys(&Keys::generate())
                    .unwrap(),
                prompt_tag: "test".into(),
                received_at: std::time::Instant::now(),
                project: None,
            }],
            cancelled_events: vec![],
            cancel_reason: None,
        };
        let result = PromptResult {
            agent,
            source: PromptSource::Channel(channel_id),
            turn_id: "test-turn-id".to_string(),
            outcome: PromptOutcome::Timeout(TimeoutKind::Hard {
                recently_active: true,
            }),
            batch: Some(batch),
        };
        handle_prompt_result(
            &mut pool,
            &mut queue,
            &config,
            result,
            &mut heartbeat_in_flight,
            &removed_channels,
            &mut crash_history,
            &respawn_tx,
            &mut respawn_tasks,
            Some(observer.clone()),
            None,
        );

        let events = observer.snapshot();
        let turn_error = events
            .iter()
            .find(|e| e.kind == "turn_error")
            .expect("exactly one turn_error event must be emitted");
        assert_eq!(
            turn_error.payload["error"].as_str().unwrap(),
            format!(
                "Agent turn exceeded the maximum duration ({}s) — dead-lettered (retry budget exhausted)",
                config.max_turn_duration_secs
            ),
        );
        assert_eq!(
            queue.queued_event_count(&channel_id),
            0,
            "batch with an exhausted retry budget must be dead-lettered, not requeued"
        );
    }

    /// Cancel-drain-timeout batches are requeued as cancelled (merge into the
    /// next flush, `CancelReason` preserved) — never dead-lettered like a real
    /// hard-cap. The agent itself is NOT returned to the idle pool: it is
    /// handed to `spawn_respawn_task` instead, mirroring a fatal `Timeout`.
    ///
    /// This reproduces the full steer-fallback incident, not just the
    /// original batch in isolation: the steer ack handler already released
    /// the new triggering event back to `queue` (`lib.rs`'s
    /// `ExpectedRunIdMissing` path) before the cancel-drain expiry fires. The
    /// next `flush_next()` must merge the surviving original event (via
    /// `cancelled_events`) with that already-queued new event (via `events`)
    /// exactly once each — proving no loss and no duplication.
    #[tokio::test]
    async fn cancel_drain_timeout_requeues_batch_and_does_not_return_agent() {
        let keys = Keys::generate();
        let original_event = EventBuilder::new(Kind::Custom(9), "original")
            .sign_with_keys(&keys)
            .unwrap();
        let new_event = EventBuilder::new(Kind::Custom(9), "new")
            .sign_with_keys(&keys)
            .unwrap();
        assert_ne!(
            original_event.id, new_event.id,
            "test fixture must use two distinct events"
        );
        let channel_id = Uuid::new_v4();
        let batch = FlushBatch {
            channel_id,
            events: vec![BatchEvent {
                event: original_event.clone(),
                prompt_tag: "test".into(),
                received_at: std::time::Instant::now(),
                project: None,
            }],
            cancelled_events: vec![],
            cancel_reason: Some(CancelReason::Steer),
        };

        let agent = dummy_agent(0).await;
        let mut pool = AgentPool::from_slots(vec![None]);
        let task_id = pool.join_set.spawn(async {}).id();
        pool.task_map_mut().insert(
            task_id,
            crate::pool::TaskMeta {
                agent_index: 0,
                channel_id: None,
                turn_id: "test-turn-id".to_string(),
                recoverable_batch: None,
                control_tx: None,
                steer_tx: None,
            },
        );
        let mut queue = EventQueue::new(config::DedupMode::Queue);
        // The steer ack handler releases the new event to the queue BEFORE
        // signaling the fallback ControlSignal::Steer that ultimately times
        // out on drain — so it is already queued by the time
        // handle_prompt_result runs.
        queue.push(QueuedEvent {
            channel_id,
            event: new_event.clone(),
            received_at: std::time::Instant::now(),
            prompt_tag: "test".into(),
            project: None,
        });
        let config = test_config();
        let mut heartbeat_in_flight = false;
        let removed_channels = HashSet::new();
        let mut crash_history = vec![SlotCircuit {
            crash_times: Vec::new(),
            open_until: None,
            respawn_in_flight: false,
        }];
        let (respawn_tx, _respawn_rx) = mpsc::channel(8);
        let mut respawn_tasks = tokio::task::JoinSet::new();
        let observer = ObserverHandle::in_process();
        let grace = std::time::Duration::from_secs(5);
        let result = PromptResult {
            agent,
            source: PromptSource::Channel(channel_id),
            turn_id: "test-turn-id".to_string(),
            outcome: PromptOutcome::CancelDrainTimeout(grace),
            batch: Some(batch),
        };

        handle_prompt_result(
            &mut pool,
            &mut queue,
            &config,
            result,
            &mut heartbeat_in_flight,
            &removed_channels,
            &mut crash_history,
            &respawn_tx,
            &mut respawn_tasks,
            Some(observer.clone()),
            None,
        );

        // Batch preserved as a cancelled merge, not dead-lettered — same
        // treatment as a normal `Cancelled` outcome. `handle_prompt_result`
        // already called `mark_complete` internally, releasing the channel.
        // `flush_next()` must merge the already-queued new event with the
        // preserved original: each exactly once, in the correct bucket.
        let requeued = queue.flush_next().expect("batch must be requeued");
        assert_eq!(
            requeued.events.len(),
            1,
            "exactly one new event must be in the regular events bucket"
        );
        assert_eq!(
            requeued.events[0].event.id, new_event.id,
            "the regular events bucket must hold the new (already-queued) event"
        );
        assert_eq!(
            requeued.cancelled_events.len(),
            1,
            "exactly one original event must be in the cancelled_events bucket"
        );
        assert_eq!(
            requeued.cancelled_events[0].event.id, original_event.id,
            "the cancelled_events bucket must hold the original (interrupted) event"
        );
        assert_ne!(
            requeued.events[0].event.id, requeued.cancelled_events[0].event.id,
            "the new and original events must not be the same event"
        );
        assert_eq!(
            requeued.cancel_reason,
            Some(CancelReason::Steer),
            "CancelReason must ride through to the requeued batch"
        );

        // Agent must NOT be back in the idle pool — it was handed to respawn.
        assert_eq!(
            pool.live_count(),
            0,
            "agent must not be returned to the pool after a cancel-drain timeout"
        );
        assert_eq!(
            respawn_tasks.len(),
            1,
            "a respawn task must be spawned for the poisoned agent"
        );

        // The observer payload must be fate-neutral: it names the grace and
        // the process replacement, and must NOT claim the batch was
        // preserved — that claim is false for explicit Stop/removed-channel
        // drops (see the sibling dropped-Stop test below), so the same
        // wording is used regardless of fate.
        let events = observer.snapshot();
        let turn_error = events
            .iter()
            .find(|e| e.kind == "turn_error")
            .expect("exactly one turn_error event must be emitted");
        assert_eq!(
            turn_error.payload["outcome"].as_str().unwrap(),
            "cancel_drain_timeout"
        );
        assert_eq!(
            turn_error.payload["error"].as_str().unwrap(),
            format!("Agent did not stop within {grace:?} after cancellation; the agent process is being replaced."),
            "observer message must name the actual grace and must not claim preservation"
        );
        assert_eq!(
            events.iter().filter(|e| e.kind == "turn_error").count(),
            1,
            "exactly one turn_error event must be emitted"
        );
    }

    /// Explicit Stop (`ControlSignal::Cancel`) on cancel-drain expiry drops
    /// the triggering batch — `requeue_cancelled_batch` returns `None` for
    /// `Cancel`/`Rotate`. The observer payload must be the SAME fate-neutral
    /// text as the preserved-Steer case above: it must never claim work was
    /// preserved when it was intentionally discarded. The poisoned agent is
    /// still respawned exactly as in the preserved case.
    #[tokio::test]
    async fn cancel_drain_timeout_dropped_stop_batch_none_same_neutral_payload() {
        let agent = dummy_agent(0).await;
        let mut pool = AgentPool::from_slots(vec![None]);
        let task_id = pool.join_set.spawn(async {}).id();
        pool.task_map_mut().insert(
            task_id,
            crate::pool::TaskMeta {
                agent_index: 0,
                channel_id: None,
                turn_id: "test-turn-id".to_string(),
                recoverable_batch: None,
                control_tx: None,
                steer_tx: None,
            },
        );
        let mut queue = EventQueue::new(config::DedupMode::Queue);
        let config = test_config();
        let mut heartbeat_in_flight = false;
        let removed_channels = HashSet::new();
        let mut crash_history = vec![SlotCircuit {
            crash_times: Vec::new(),
            open_until: None,
            respawn_in_flight: false,
        }];
        let (respawn_tx, _respawn_rx) = mpsc::channel(8);
        let mut respawn_tasks = tokio::task::JoinSet::new();
        let observer = ObserverHandle::in_process();
        let grace = std::time::Duration::from_secs(5);
        let result = PromptResult {
            agent,
            source: PromptSource::Channel(Uuid::new_v4()),
            turn_id: "test-turn-id".to_string(),
            outcome: PromptOutcome::CancelDrainTimeout(grace),
            // Explicit Stop already dropped the batch upstream in
            // `classify_control_cancel_failure` — `handle_prompt_result`
            // never sees one to requeue.
            batch: None,
        };

        handle_prompt_result(
            &mut pool,
            &mut queue,
            &config,
            result,
            &mut heartbeat_in_flight,
            &removed_channels,
            &mut crash_history,
            &respawn_tx,
            &mut respawn_tasks,
            Some(observer.clone()),
            None,
        );

        // No batch to merge — the queue has nothing pending for any channel.
        assert_eq!(
            queue.pending_channels(),
            0,
            "a dropped Stop batch must not leave anything queued"
        );

        // Same respawn treatment as the preserved case: never returned idle.
        assert_eq!(
            pool.live_count(),
            0,
            "agent must not be returned to the pool after a cancel-drain timeout"
        );
        assert_eq!(
            respawn_tasks.len(),
            1,
            "a respawn task must be spawned for the poisoned agent"
        );

        // The observer payload is byte-identical to the preserved-Steer case:
        // fate-neutral, naming the grace, with no preservation claim.
        let events = observer.snapshot();
        let turn_error = events
            .iter()
            .find(|e| e.kind == "turn_error")
            .expect("exactly one turn_error event must be emitted");
        assert_eq!(
            turn_error.payload["outcome"].as_str().unwrap(),
            "cancel_drain_timeout"
        );
        assert_eq!(
            turn_error.payload["error"].as_str().unwrap(),
            format!("Agent did not stop within {grace:?} after cancellation; the agent process is being replaced."),
            "observer message must be fate-neutral even though the batch was dropped"
        );
        assert_eq!(
            events.iter().filter(|e| e.kind == "turn_error").count(),
            1,
            "exactly one turn_error event must be emitted"
        );
    }

    #[tokio::test]
    async fn transport_error_emits_exactly_one_feed_event() {
        let io = AcpError::Io(std::io::Error::other("pipe broke"));
        assert_eq!(turn_errors_emitted_for(PromptOutcome::Error(io)).await, 1);
    }

    #[tokio::test]
    async fn application_error_emits_exactly_one_feed_event() {
        let app = AcpError::IdleTimeout(std::time::Duration::from_secs(1));
        assert_eq!(turn_errors_emitted_for(PromptOutcome::Error(app)).await, 1);
    }

    // ── is_auth_error classification ───────────────────────────────────────

    #[test]
    fn is_auth_error_matches_reauthenticate_message() {
        let e = acp::AcpError::AgentError {
            code: -32000,
            message: "API Error: OAuth access token has expired. Re-authenticate to continue."
                .to_string(),
        };
        assert!(
            is_auth_error(&e),
            "Re-authenticate variant must be classified as auth error"
        );
    }

    #[test]
    fn is_auth_error_matches_401_message() {
        let e = acp::AcpError::AgentError {
            code: -32000,
            message: "Internal error: API Error: 401 OAuth access token has expired.".to_string(),
        };
        assert!(
            is_auth_error(&e),
            "API Error: 401 variant must be classified as auth error"
        );
    }

    #[test]
    fn is_auth_error_rejects_other_agent_error_message() {
        let e = acp::AcpError::AgentError {
            code: -32601,
            message: "Usage credits required for 1M context — turn on usage credits".to_string(),
        };
        assert!(
            !is_auth_error(&e),
            "usage-credit error must NOT be classified as auth error"
        );
    }

    #[test]
    fn is_auth_error_rejects_transport_errors() {
        let io = acp::AcpError::Io(std::io::Error::other("pipe broke"));
        assert!(
            !is_auth_error(&io),
            "I/O error must not be classified as auth error"
        );
        let timeout = acp::AcpError::WriteTimeout(std::time::Duration::from_secs(5));
        assert!(
            !is_auth_error(&timeout),
            "WriteTimeout must not be classified as auth error"
        );
    }

    // ── auth error dead-letter behavior ────────────────────────────────────

    /// An auth-class `PromptOutcome::Error` must dead-letter immediately
    /// (the batch is never requeued) so the user sees a re-auth hint at once
    /// rather than after 10 futile retries.
    #[tokio::test]
    async fn auth_error_dead_letters_immediately_without_requeueing() {
        let keys = nostr::Keys::generate();
        let event = nostr::EventBuilder::new(nostr::Kind::Custom(9), "test")
            .sign_with_keys(&keys)
            .unwrap();
        let channel_id = uuid::Uuid::new_v4();
        let batch = FlushBatch {
            channel_id,
            events: vec![BatchEvent {
                event,
                prompt_tag: "test".into(),
                received_at: std::time::Instant::now(),
                project: None,
            }],
            cancelled_events: vec![],
            cancel_reason: None,
        };

        let auth_error = acp::AcpError::AgentError {
            code: -32000,
            message: "API Error: 401 OAuth access token has expired. Re-authenticate to continue."
                .to_string(),
        };

        let agent = dummy_agent(0).await;
        let mut pool = AgentPool::from_slots(vec![None]);
        let task_id = pool.join_set.spawn(async {}).id();
        pool.task_map_mut().insert(
            task_id,
            crate::pool::TaskMeta {
                agent_index: 0,
                channel_id: None,
                turn_id: "test-turn-id".to_string(),
                recoverable_batch: None,
                control_tx: None,
                steer_tx: None,
            },
        );
        let mut queue = EventQueue::new(config::DedupMode::Queue);
        let config = test_config();
        let mut heartbeat_in_flight = false;
        let removed_channels = std::collections::HashSet::new();
        let mut crash_history = vec![SlotCircuit {
            crash_times: Vec::new(),
            open_until: None,
            respawn_in_flight: false,
        }];
        let (respawn_tx, _respawn_rx) = mpsc::channel(8);
        let mut respawn_tasks = tokio::task::JoinSet::new();
        let result = PromptResult {
            agent,
            source: PromptSource::Channel(channel_id),
            turn_id: "test-turn-id".to_string(),
            outcome: PromptOutcome::Error(auth_error),
            batch: Some(batch),
        };
        handle_prompt_result(
            &mut pool,
            &mut queue,
            &config,
            result,
            &mut heartbeat_in_flight,
            &removed_channels,
            &mut crash_history,
            &respawn_tx,
            &mut respawn_tasks,
            None,
            None,
        );

        // The batch must not be requeued: pending_channels returns 0.
        assert_eq!(
            queue.pending_channels(),
            0,
            "auth error must dead-letter immediately — batch must not be requeued"
        );
        assert_eq!(
            queue.queued_event_count(&channel_id),
            0,
            "auth error must dead-letter immediately — no events should be pending"
        );
    }

    /// A non-auth application error (e.g. usage credits) must still follow the
    /// standard requeue path so today's behavior is unchanged.
    #[tokio::test]
    async fn non_auth_application_error_is_requeued() {
        let keys = nostr::Keys::generate();
        let event = nostr::EventBuilder::new(nostr::Kind::Custom(9), "test")
            .sign_with_keys(&keys)
            .unwrap();
        let channel_id = uuid::Uuid::new_v4();
        let batch = FlushBatch {
            channel_id,
            events: vec![BatchEvent {
                event,
                prompt_tag: "test".into(),
                received_at: std::time::Instant::now(),
                project: None,
            }],
            cancelled_events: vec![],
            cancel_reason: None,
        };

        // Usage-credits error — AgentError but NOT an auth error.
        let usage_error = acp::AcpError::AgentError {
            code: -32000,
            message: "Usage credits required for 1M context".to_string(),
        };

        let agent = dummy_agent(0).await;
        let mut pool = AgentPool::from_slots(vec![None]);
        let task_id = pool.join_set.spawn(async {}).id();
        pool.task_map_mut().insert(
            task_id,
            crate::pool::TaskMeta {
                agent_index: 0,
                channel_id: None,
                turn_id: "test-turn-id".to_string(),
                recoverable_batch: None,
                control_tx: None,
                steer_tx: None,
            },
        );
        let mut queue = EventQueue::new(config::DedupMode::Queue);
        let config = test_config();
        let mut heartbeat_in_flight = false;
        let removed_channels = std::collections::HashSet::new();
        let mut crash_history = vec![SlotCircuit {
            crash_times: Vec::new(),
            open_until: None,
            respawn_in_flight: false,
        }];
        let (respawn_tx, _respawn_rx) = mpsc::channel(8);
        let mut respawn_tasks = tokio::task::JoinSet::new();
        let result = PromptResult {
            agent,
            source: PromptSource::Channel(channel_id),
            turn_id: "test-turn-id".to_string(),
            outcome: PromptOutcome::Error(usage_error),
            batch: Some(batch),
        };
        handle_prompt_result(
            &mut pool,
            &mut queue,
            &config,
            result,
            &mut heartbeat_in_flight,
            &removed_channels,
            &mut crash_history,
            &respawn_tx,
            &mut respawn_tasks,
            None,
            None,
        );

        // Non-auth application error: batch IS requeued (first attempt, retry budget > 0).
        assert_eq!(
            queue.pending_channels(),
            1,
            "non-auth application error must requeue the batch for retry"
        );
        assert_eq!(
            queue.queued_event_count(&channel_id),
            1,
            "non-auth application error must preserve the event for retry"
        );
    }
}

#[cfg(test)]
mod observer_payload_trim_tests {
    use super::*;

    fn event_with_payload(kind: &str, payload: serde_json::Value) -> observer::ObserverEvent {
        observer::ObserverEvent {
            seq: 1,
            timestamp: "2026-06-16T00:00:00Z".to_string(),
            kind: kind.to_string(),
            agent_index: Some(0),
            channel_id: Some("11111111-1111-1111-1111-111111111111".to_string()),
            session_id: Some("sess-1".to_string()),
            turn_id: Some("turn-1".to_string()),
            started_at: None,
            payload,
        }
    }

    fn serialized(event: &observer::ObserverEvent) -> String {
        serde_json::to_string(event).unwrap()
    }

    #[test]
    fn test_under_budget_frame_passes_through_byte_identical() {
        let mut event = event_with_payload("acp_read", serde_json::json!({ "body": "small" }));
        let before = serialized(&event);
        fit_observer_event_to_budget(&mut event);
        assert_eq!(
            serialized(&event),
            before,
            "under-budget frame must not be mutated"
        );
    }

    #[test]
    fn test_single_giant_leaf_is_elided_to_fit_with_envelope_intact() {
        let big = "x".repeat(100_000);
        let mut event = event_with_payload("acp_read", serde_json::json!({ "body": big }));
        fit_observer_event_to_budget(&mut event);

        assert!(
            serialized(&event).len() <= OBSERVER_MAX_PLAINTEXT_LEN,
            "frame must fit after trimming"
        );
        // Envelope intact.
        assert_eq!(event.kind, "acp_read");
        assert_eq!(event.turn_id.as_deref(), Some("turn-1"));
        assert_eq!(
            event.channel_id.as_deref(),
            Some("11111111-1111-1111-1111-111111111111")
        );
        assert_eq!(event.seq, 1);

        let leaf = event.payload["body"].as_str().unwrap();
        assert!(
            leaf.starts_with(&"x".repeat(OBSERVER_LEAF_RETAIN_BYTES)),
            "head retained"
        );
        assert!(
            leaf.ends_with(&"x".repeat(OBSERVER_LEAF_RETAIN_BYTES)),
            "tail retained"
        );
        // N in the marker is RAW bytes removed: original len minus retained len.
        let removed = 100_000 - leaf.chars().filter(|c| *c == 'x').count();
        assert!(
            leaf.contains(&format!("…[elided {removed} bytes]…")),
            "marker reports raw bytes removed"
        );
    }

    #[test]
    fn test_multi_block_prompt_retains_every_section_header_after_elision() {
        // The real session/prompt fix: format_prompt now emits one block per
        // section, so the observer payload is params.prompt = [{text: "[Base]…"},
        // {text: "[Agent Memory — core]…"}, … {text: "[Buzz event: …]…<huge>"}].
        // An oversized section is its own leaf, so eliding its body keeps the
        // leaf's head-3000 (which begins with the section's [Header] line) — every
        // header survives, so the desktop "Prompt context" panel counts them all.
        // This is the regression the single-fat-leaf shape caused (the trailing
        // [Buzz event] header fell into the elided middle and the count collapsed
        // to 1).
        let sections = [
            "[Base]\nyou are a helpful agent".to_string(),
            "[System]\npersona text".to_string(),
            "[Agent Memory — core]\nremember this".to_string(),
            "[Context]\nScope: thread".to_string(),
            // The triggering event body, oversized on its own.
            format!("[Buzz event: @mention]\nContent: {}", "E".repeat(90_000)),
        ];
        let block_refs: Vec<&str> = sections.iter().map(String::as_str).collect();
        // Mirror the wire shape build_prompt_params produces: each block is its
        // own {type:"text", text} leaf under params.prompt.
        let prompt_blocks: Vec<serde_json::Value> = block_refs
            .iter()
            .map(|text| serde_json::json!({ "type": "text", "text": text }))
            .collect();
        let mut event = event_with_payload(
            "acp_write",
            serde_json::json!({
                "method": "session/prompt",
                "params": { "sessionId": "sess-1", "prompt": prompt_blocks },
            }),
        );
        assert!(
            serialized(&event).len() > OBSERVER_MAX_PLAINTEXT_LEN,
            "precondition: oversized event body pushes the frame over the cap"
        );

        fit_observer_event_to_budget(&mut event);

        assert!(
            serialized(&event).len() <= OBSERVER_MAX_PLAINTEXT_LEN,
            "frame must fit after trimming"
        );
        let blocks = event.payload["params"]["prompt"]
            .as_array()
            .expect("prompt array survives");
        let texts: Vec<&str> = blocks.iter().map(|b| b["text"].as_str().unwrap()).collect();
        for header in [
            "[Base]",
            "[System]",
            "[Agent Memory — core]",
            "[Context]",
            "[Buzz event: @mention]",
        ] {
            assert!(
                texts.iter().any(|t| t.starts_with(header)),
                "section header {header} must survive at the head of its own block"
            );
        }
        // The oversized event body was elided in place (header kept, middle cut).
        let event_block = texts
            .iter()
            .find(|t| t.starts_with("[Buzz event: @mention]"))
            .unwrap();
        assert!(
            event_block.contains("…[elided"),
            "the oversized event body is elided, not dropped"
        );
    }

    #[test]
    fn test_multi_leaf_elides_largest_shrinkable_first_and_stops_when_it_fits() {
        // One leaf alone over the cap; a second smaller-but-still-large leaf.
        // Eliding the biggest should suffice, leaving the smaller intact.
        let mut event = event_with_payload(
            "acp_write",
            serde_json::json!({
                "huge": "a".repeat(90_000),
                "medium": "b".repeat(20_000),
            }),
        );
        fit_observer_event_to_budget(&mut event);

        assert!(serialized(&event).len() <= OBSERVER_MAX_PLAINTEXT_LEN);
        assert!(
            event.payload["huge"].as_str().unwrap().contains("…[elided"),
            "the largest leaf is elided"
        );
        assert_eq!(
            event.payload["medium"].as_str().unwrap().len(),
            20_000,
            "the smaller leaf is left untouched once the frame fits"
        );
    }

    #[test]
    fn test_coalesced_chunk_nested_leaf_is_reached_by_recursive_walk() {
        // The coalesced-chunk big leaf lives at params.update.content.text,
        // not a top-level field — the walk must recurse to reach it.
        let big = "z".repeat(80_000);
        let mut event = event_with_payload(
            "session_update",
            serde_json::json!({
                "params": {
                    "update": {
                        "sessionUpdate": "agent_message_chunk",
                        "content": { "text": big }
                    }
                }
            }),
        );
        fit_observer_event_to_budget(&mut event);

        assert!(serialized(&event).len() <= OBSERVER_MAX_PLAINTEXT_LEN);
        let text = event.payload["params"]["update"]["content"]["text"]
            .as_str()
            .unwrap();
        assert!(text.contains("…[elided"), "nested leaf was elided");
    }

    #[test]
    fn test_many_medium_leaves_terminate_via_stub() {
        // Many leaves each too small to shrink on their own (below 2x retain),
        // collectively over the cap. No leaf can strictly shrink, so the trimmer
        // must terminate via the stub rather than loop forever.
        let leaf = "m".repeat(OBSERVER_LEAF_RETAIN_BYTES); // shorter than head+tail → cannot shrink
        let items: Vec<serde_json::Value> = (0..40)
            .map(|_| serde_json::Value::String(leaf.clone()))
            .collect();
        let mut event = event_with_payload("acp_read", serde_json::json!({ "items": items }));
        assert!(
            serialized(&event).len() > OBSERVER_MAX_PLAINTEXT_LEN,
            "precondition: frame is over the cap"
        );

        fit_observer_event_to_budget(&mut event);

        assert!(serialized(&event).len() <= OBSERVER_MAX_PLAINTEXT_LEN);
        assert_eq!(
            event.payload["elided"].as_str().unwrap(),
            "acp_read payload too large",
            "fell back to the stub"
        );
        assert!(event.payload.get("originalBytes").is_some());
    }

    #[test]
    fn test_leaf_too_small_to_shrink_is_not_mutated() {
        // A frame already under budget whose only leaf is below the shrink floor:
        // nothing should change. (Under-budget short-circuits, and even if forced,
        // leaf_shrinks would reject it.)
        let short = "s".repeat(OBSERVER_LEAF_RETAIN_BYTES); // == head; cannot strictly shrink
        assert!(
            !leaf_shrinks(&short),
            "a leaf at the retain floor must not shrink"
        );
        let longer = "L".repeat(OBSERVER_LEAF_RETAIN_BYTES * 2 + 100);
        assert!(leaf_shrinks(&longer), "a clearly larger leaf must shrink");
    }

    #[test]
    fn test_utf8_multibyte_leaf_elides_on_char_boundary() {
        // A leaf of 3-byte chars (… = U+2026) — eliding must land on char
        // boundaries and never panic or produce invalid UTF-8.
        let big: String = "…".repeat(40_000); // 120_000 bytes
        let mut event = event_with_payload("acp_read", serde_json::json!({ "body": big }));
        fit_observer_event_to_budget(&mut event);

        assert!(serialized(&event).len() <= OBSERVER_MAX_PLAINTEXT_LEN);
        let leaf = event.payload["body"].as_str().unwrap();
        // Valid UTF-8 by construction (it's a &str); confirm head/tail are whole
        // multi-byte chars and the marker is present.
        assert!(leaf.starts_with('…'));
        assert!(leaf.ends_with('…'));
        assert!(leaf.contains("[elided"));
    }
}

/// NIP-PC on the channel surface.
///
/// These exercise [`decide_channel_peer_event`] and [`resolve_peer_trust`] —
/// the two functions the run loop actually calls for a channel-routed call —
/// against events built by the same `buzz-sdk` builders `buzz agents call`
/// uses. Nothing here reconstructs the decision: the ledger is the production
/// one, and the only thing supplied from outside is the trust class, because
/// resolving it is async and the decision is not.
#[cfg(test)]
mod peer_call_channel_tests {
    use super::*;
    use buzz_core::peer_call::{onward_context, PeerCallRoute};
    use buzz_sdk::builders::{build_peer_call, build_peer_call_result, PeerCallMeta};
    use nostr::{EventBuilder, JsonUtil, Keys};

    /// A `RestClient` that is never used: every trust decision asserted here
    /// resolves from the owner pubkey, the external list or the sibling cache
    /// before any HTTP call is attempted.
    fn dummy_rest_client() -> relay::RestClient {
        relay::RestClient {
            http: reqwest::Client::new(),
            base_url: "http://localhost:0".into(),
            keys: Keys::generate(),
            auth_tag_json: None,
        }
    }

    fn hex_of(k: &Keys) -> String {
        k.public_key().to_hex().to_ascii_lowercase()
    }

    fn channel_route(channel: uuid::Uuid) -> PeerCallRoute {
        PeerCallRoute::Channel {
            channel: channel.to_string(),
            thread_root: None,
        }
    }

    /// A signed call, built the way the CLI builds one.
    fn call(caller: &Keys, callee_hex: &str, route: &PeerCallRoute, nonce: &str) -> nostr::Event {
        let (hop, visited) = onward_context(&[], &hex_of(caller));
        build_peer_call(
            &hex_of(caller),
            "summarise the thread",
            &PeerCallMeta {
                callee: callee_hex.to_string(),
                route: route.clone(),
                nonce: nonce.into(),
                hop,
                visited,
            },
        )
        .expect("well-formed call")
        .sign_with_keys(caller)
        .expect("sign")
    }

    fn call_id_of(event: &nostr::Event) -> String {
        event
            .tags
            .iter()
            .find_map(|t| {
                let s = t.as_slice();
                (s.first().map(String::as_str) == Some("call")).then(|| s[1].clone())
            })
            .expect("a call carries its id")
    }

    fn result(
        callee: &Keys,
        caller_hex: &str,
        call_id: &str,
        route: &PeerCallRoute,
        body: &str,
    ) -> nostr::Event {
        build_peer_call_result(caller_hex, call_id, body, route)
            .expect("well-formed result")
            .sign_with_keys(callee)
            .expect("sign")
    }

    /// The outcome: one explicit trusted call becomes exactly one turn, on the
    /// channel it was made from — and the three neighbouring events that must
    /// not.
    #[test]
    fn a_trusted_call_in_a_channel_becomes_one_turn_on_that_channel() {
        let agent = Keys::generate();
        let peer = Keys::generate();
        let channel = uuid::Uuid::new_v4();
        let route = channel_route(channel);
        let mut ledger = peer_call::CallLedger::new();

        let event = call(
            &peer,
            &hex_of(&agent),
            &route,
            "0123456789abcdef0123456789abcdef",
        );
        assert_eq!(
            decide_channel_peer_event(
                &event,
                channel,
                &hex_of(&agent),
                peer_call::PeerTrust::TrustedAgent,
                &mut ledger,
            ),
            ChannelPeerOutcome::Turn {
                channel_id: channel,
                prompt_tag: "@call",
            },
        );

        // Once. The identical delivery — a reconnect replay, or a caller that
        // retries — must not run the task a second time.
        assert_eq!(
            decide_channel_peer_event(
                &event,
                channel,
                &hex_of(&agent),
                peer_call::PeerTrust::TrustedAgent,
                &mut ledger,
            ),
            ChannelPeerOutcome::Consumed,
            "a replayed call id must not produce a second turn"
        );

        // An ordinary message from the same trusted agent, p-tagging the agent
        // exactly as a client writes a reply, is not a call at all — it leaves
        // this path untouched for the ordinary channel rules to judge.
        let reply = EventBuilder::new(nostr::Kind::Custom(9), "thanks, looking")
            .tags([nostr::Tag::parse(["p", &hex_of(&agent)]).unwrap()])
            .sign_with_keys(&peer)
            .expect("sign");
        assert_eq!(
            decide_channel_peer_event(
                &reply,
                channel,
                &hex_of(&agent),
                peer_call::PeerTrust::TrustedAgent,
                &mut ledger,
            ),
            ChannelPeerOutcome::NotPeerCall,
        );

        // A call naming a third agent arrives here too — the peer-call REQ
        // carries a channel's traffic, not only ours — and is not ours to run.
        let elsewhere = Keys::generate();
        let not_ours = call(
            &peer,
            &hex_of(&elsewhere),
            &route,
            "fedcba9876543210fedcba9876543210",
        );
        assert_eq!(
            decide_channel_peer_event(
                &not_ours,
                channel,
                &hex_of(&agent),
                peer_call::PeerTrust::TrustedAgent,
                &mut ledger,
            ),
            ChannelPeerOutcome::Consumed,
        );
    }

    /// The refusal is about trust and nothing else: the identical envelope from
    /// a trusted author is admitted. Without the control this would also pass
    /// if the envelope were simply malformed.
    #[test]
    fn an_untrusted_relay_identity_cannot_invoke_through_the_channel_path() {
        let agent = Keys::generate();
        let stranger = Keys::generate();
        let channel = uuid::Uuid::new_v4();
        let route = channel_route(channel);
        let event = call(
            &stranger,
            &hex_of(&agent),
            &route,
            "0123456789abcdef0123456789abcdef",
        );

        let mut refused = peer_call::CallLedger::new();
        assert_eq!(
            decide_channel_peer_event(
                &event,
                channel,
                &hex_of(&agent),
                peer_call::PeerTrust::Untrusted,
                &mut refused,
            ),
            ChannelPeerOutcome::Consumed,
        );

        let mut admitted = peer_call::CallLedger::new();
        assert_eq!(
            decide_channel_peer_event(
                &event,
                channel,
                &hex_of(&agent),
                peer_call::PeerTrust::TrustedAgent,
                &mut admitted,
            ),
            ChannelPeerOutcome::Turn {
                channel_id: channel,
                prompt_tag: "@call",
            },
        );
    }

    /// The caller half, end to end through the production decision function:
    /// this agent's own published call is registered from the wire, and the
    /// callee's result comes back as a correlated turn.
    ///
    /// This is the half that cannot work without the `authors` filter on the
    /// peer-call REQ: nothing else tells the harness a call was made, because
    /// the harness never publishes one.
    #[test]
    fn our_own_call_is_registered_from_the_wire_and_its_result_returns() {
        let agent = Keys::generate();
        let callee = Keys::generate();
        let channel = uuid::Uuid::new_v4();
        let route = channel_route(channel);
        let mut ledger = peer_call::CallLedger::new();

        let ours = call(
            &agent,
            &hex_of(&callee),
            &route,
            "0123456789abcdef0123456789abcdef",
        );
        assert_eq!(
            decide_channel_peer_event(
                &ours,
                channel,
                &hex_of(&agent),
                peer_call::PeerTrust::SelfAuthored,
                &mut ledger,
            ),
            ChannelPeerOutcome::Consumed,
            "our own call is not a turn for us"
        );
        assert_eq!(
            ledger.outstanding_count(),
            1,
            "the call must be registered, or its result correlates to nothing"
        );

        let call_id = call_id_of(&ours);
        let answer = result(
            &callee,
            &hex_of(&agent),
            &call_id,
            &route,
            "it was the fixture",
        );
        assert_eq!(
            decide_channel_peer_event(
                &answer,
                channel,
                &hex_of(&agent),
                peer_call::PeerTrust::TrustedAgent,
                &mut ledger,
            ),
            ChannelPeerOutcome::Turn {
                channel_id: channel,
                prompt_tag: "@call-result",
            },
        );
        assert_eq!(ledger.outstanding_count(), 0, "the call is closed");

        // Exactly one result per call.
        assert_eq!(
            decide_channel_peer_event(
                &answer,
                channel,
                &hex_of(&agent),
                peer_call::PeerTrust::TrustedAgent,
                &mut ledger,
            ),
            ChannelPeerOutcome::Consumed,
        );

        // And a third party holding the call id cannot answer for the callee.
        let impostor = Keys::generate();
        let forged = result(&impostor, &hex_of(&agent), &call_id, &route, "me instead");
        let mut fresh = peer_call::CallLedger::new();
        decide_channel_peer_event(
            &ours,
            channel,
            &hex_of(&agent),
            peer_call::PeerTrust::SelfAuthored,
            &mut fresh,
        );
        assert_eq!(
            decide_channel_peer_event(
                &forged,
                channel,
                &hex_of(&agent),
                peer_call::PeerTrust::TrustedAgent,
                &mut fresh,
            ),
            ChannelPeerOutcome::Consumed,
        );
    }

    /// A result correlating to nothing is not a prompt. This is the case that
    /// makes an outstanding-call ledger necessary rather than decorative: a
    /// trusted peer can publish a well-formed result at any time.
    #[test]
    fn a_result_for_a_call_we_never_made_is_not_a_turn() {
        let agent = Keys::generate();
        let peer = Keys::generate();
        let channel = uuid::Uuid::new_v4();
        let route = channel_route(channel);
        let mut ledger = peer_call::CallLedger::new();

        let answer = result(&peer, &hex_of(&agent), &"ab".repeat(32), &route, "unasked");
        assert_eq!(
            decide_channel_peer_event(
                &answer,
                channel,
                &hex_of(&agent),
                peer_call::PeerTrust::TrustedAgent,
                &mut ledger,
            ),
            ChannelPeerOutcome::Consumed,
        );
    }

    /// The whole lifecycle, with nothing about the result hand-written.
    ///
    /// The callee's turn is rendered by the production prompt path; a stub
    /// callee reads the command out of that prompt exactly as an agent would,
    /// and the result is published through the production builder using only
    /// values the prompt supplied. Then the caller's own harness correlates it.
    ///
    /// The earlier round-trip tests constructed the result themselves from
    /// variables they already had in scope, which proved that a well-formed
    /// result correlates — not that a woken callee is ever in a position to
    /// produce one. This starts from the prompt and therefore fails if the
    /// prompt stops carrying the caller, the call id or the route.
    #[test]
    fn a_woken_callee_can_answer_from_its_prompt_alone_and_the_caller_resumes_once() {
        let caller = Keys::generate();
        let callee = Keys::generate();
        let channel = uuid::Uuid::new_v4();
        let route = channel_route(channel);

        // 1. The caller publishes. Its own harness sees the event come back and
        //    records the outstanding call.
        let mut caller_ledger = peer_call::CallLedger::new();
        let call_event = call(
            &caller,
            &hex_of(&callee),
            &route,
            "0123456789abcdef0123456789abcdef",
        );
        assert_eq!(
            decide_channel_peer_event(
                &call_event,
                channel,
                &hex_of(&caller),
                peer_call::PeerTrust::SelfAuthored,
                &mut caller_ledger,
            ),
            ChannelPeerOutcome::Consumed,
        );
        assert_eq!(caller_ledger.outstanding_count(), 1);

        // 2. The callee's harness admits it and queues a turn.
        let mut callee_ledger = peer_call::CallLedger::new();
        let outcome = decide_channel_peer_event(
            &call_event,
            channel,
            &hex_of(&callee),
            peer_call::PeerTrust::TrustedAgent,
            &mut callee_ledger,
        );
        let ChannelPeerOutcome::Turn { prompt_tag, .. } = outcome else {
            panic!("the call did not become a turn: {outcome:?}");
        };

        // 3. The turn is rendered by the production prompt path.
        let batch = queue::FlushBatch {
            channel_id: channel,
            events: vec![queue::BatchEvent {
                event: call_event.clone(),
                prompt_tag: prompt_tag.into(),
                received_at: std::time::Instant::now(),
                project: None,
            }],
            cancelled_events: vec![],
            cancel_reason: None,
        };
        let prompt = queue::format_prompt(&batch, &queue::FormatPromptArgs::default()).join("\n\n");

        // 4. A stub callee runs what it was told to run. It knows nothing this
        //    prompt did not tell it — the parsed flags are the only inputs.
        let flags = parse_result_command(&prompt).expect("the prompt carries a result command");
        let answer = build_peer_call_result(
            &flags.to,
            &flags.call,
            "three questions remain open",
            &flags.route,
        )
        .expect("the prompt's own values build a valid result")
        .sign_with_keys(&callee)
        .expect("sign");

        // 5. The caller's harness correlates it, once.
        assert_eq!(
            decide_channel_peer_event(
                &answer,
                channel,
                &hex_of(&caller),
                peer_call::PeerTrust::TrustedAgent,
                &mut caller_ledger,
            ),
            ChannelPeerOutcome::Turn {
                channel_id: channel,
                prompt_tag: "@call-result",
            },
        );
        assert_eq!(
            caller_ledger.outstanding_count(),
            0,
            "the call the prompt described is the call that closed"
        );
        assert_eq!(
            decide_channel_peer_event(
                &answer,
                channel,
                &hex_of(&caller),
                peer_call::PeerTrust::TrustedAgent,
                &mut caller_ledger,
            ),
            ChannelPeerOutcome::Consumed,
            "a second result must not resume the call again"
        );
    }

    /// The flags of the `buzz agents call-result` command a prompt emitted.
    struct ResultCommandFlags {
        to: String,
        call: String,
        route: PeerCallRoute,
    }

    /// Read the emitted command the way a shell would: tokens, minus the line
    /// continuations. Anything the prompt failed to spell out is missing here
    /// too, which is the point — this cannot fill in a value from the test.
    fn parse_result_command(prompt: &str) -> Option<ResultCommandFlags> {
        let start = prompt.find("buzz agents call-result")?;
        let rest = &prompt[start..];
        let end = rest.find("\n```").unwrap_or(rest.len());
        let tokens: Vec<&str> = rest[..end]
            .split_whitespace()
            .filter(|t| *t != "\\")
            .collect();
        let flag = |name: &str| -> Option<String> {
            tokens
                .iter()
                .position(|t| *t == name)
                .and_then(|i| tokens.get(i + 1))
                .map(|v| (*v).to_string())
        };

        let route = match (flag("--channel"), flag("--project"), flag("--root")) {
            (Some(channel), None, None) => PeerCallRoute::Channel {
                channel,
                thread_root: flag("--thread"),
            },
            (None, Some(coordinate), Some(root)) => PeerCallRoute::Project { coordinate, root },
            _ => return None,
        };
        Some(ResultCommandFlags {
            to: flag("--to")?,
            call: flag("--call")?,
            route,
        })
    }

    /// The envelope's declared route must be the channel it arrived on. A
    /// project-routed envelope reaching the channel path is the same failure in
    /// its sharpest form: its route resolves to an issue's session key, not to
    /// any channel.
    #[test]
    fn a_call_whose_route_is_not_its_delivery_channel_is_refused() {
        let agent = Keys::generate();
        let peer = Keys::generate();
        let declared = uuid::Uuid::new_v4();
        let delivered_on = uuid::Uuid::new_v4();
        let mut ledger = peer_call::CallLedger::new();

        let event = call(
            &peer,
            &hex_of(&agent),
            &channel_route(declared),
            "0123456789abcdef0123456789abcdef",
        );
        assert_eq!(
            decide_channel_peer_event(
                &event,
                delivered_on,
                &hex_of(&agent),
                peer_call::PeerTrust::TrustedAgent,
                &mut ledger,
            ),
            ChannelPeerOutcome::Consumed,
        );

        let project = call(
            &peer,
            &hex_of(&agent),
            &PeerCallRoute::Project {
                coordinate: format!("30617:{}:buzz", hex_of(&peer)),
                root: "48be1cc2000000000000000000000000000000000000000000000000000000ab".into(),
            },
            "0123456789abcdef0123456789abcdef",
        );
        assert_eq!(
            decide_channel_peer_event(
                &project,
                delivered_on,
                &hex_of(&agent),
                peer_call::PeerTrust::TrustedAgent,
                &mut ledger,
            ),
            ChannelPeerOutcome::Consumed,
        );
    }

    /// A tampered event never reaches the envelope parser: verification is done
    /// here, not inherited from the transport.
    #[test]
    fn a_tampered_call_is_not_admitted() {
        let agent = Keys::generate();
        let peer = Keys::generate();
        let channel = uuid::Uuid::new_v4();
        let route = channel_route(channel);
        let mut ledger = peer_call::CallLedger::new();

        let honest = call(
            &peer,
            &hex_of(&agent),
            &route,
            "0123456789abcdef0123456789abcdef",
        );
        let mut json: serde_json::Value =
            serde_json::from_str(&honest.as_json()).expect("event json");
        json["content"] = serde_json::json!("rm -rf /");
        let tampered: nostr::Event =
            serde_json::from_value(json).expect("still a syntactically valid event");

        assert_eq!(
            decide_channel_peer_event(
                &tampered,
                channel,
                &hex_of(&agent),
                peer_call::PeerTrust::TrustedAgent,
                &mut ledger,
            ),
            ChannelPeerOutcome::Consumed,
        );
        assert_eq!(ledger.outstanding_count(), 0);
    }

    /// Invocation trust is its own question. `RespondTo::Anyone` and the
    /// respond-to allowlist do not appear in this function at all, which is the
    /// point: a channel may be permissive without that conferring the right to
    /// invoke.
    #[tokio::test]
    async fn invocation_trust_comes_from_ownership_not_from_channel_policy() {
        let agent = "aa".repeat(32);
        let owner = "bb".repeat(32);
        let sibling = "cc".repeat(32);
        let external = "dd".repeat(32);
        let stranger = "ee".repeat(32);

        let cache = OwnerCache::new(Some(owner.clone()));
        cache.cache_sibling(sibling.clone(), true);
        cache.cache_sibling(stranger.clone(), false);
        cache.cache_sibling(external.clone(), false);
        let approved: std::collections::BTreeSet<String> = [external.clone()].into_iter().collect();
        let rest = dummy_rest_client();

        let trust = |who: String, approved: std::collections::BTreeSet<String>| {
            let cache = &cache;
            let rest = &rest;
            let agent = agent.clone();
            async move { resolve_peer_trust(&who, &agent, &approved, cache, rest).await }
        };

        assert_eq!(
            trust(agent.clone(), approved.clone()).await,
            peer_call::PeerTrust::SelfAuthored
        );
        assert_eq!(
            trust(owner.clone(), approved.clone()).await,
            peer_call::PeerTrust::Owner
        );
        assert_eq!(
            trust(sibling.clone(), approved.clone()).await,
            peer_call::PeerTrust::TrustedAgent,
            "a verified same-owner sibling may call without being listed"
        );
        assert_eq!(
            trust(external.clone(), approved.clone()).await,
            peer_call::PeerTrust::TrustedAgent,
            "an owner-approved external agent may call"
        );
        assert_eq!(
            trust(external.clone(), std::collections::BTreeSet::new()).await,
            peer_call::PeerTrust::Untrusted,
            "the same external agent is untrusted once the owner stops listing it"
        );
        assert_eq!(
            trust(stranger.clone(), approved.clone()).await,
            peer_call::PeerTrust::Untrusted
        );
    }

    /// No owner, no siblings. An agent that cannot verify anyone must not fall
    /// back to trusting everyone.
    #[tokio::test]
    async fn an_agent_with_no_owner_trusts_no_sibling() {
        let agent = "aa".repeat(32);
        let caller = "cc".repeat(32);
        let cache = OwnerCache::new(None);
        cache.cache_sibling(caller.clone(), true);

        assert_eq!(
            resolve_peer_trust(
                &caller,
                &agent,
                &std::collections::BTreeSet::new(),
                &cache,
                &dummy_rest_client(),
            )
            .await,
            peer_call::PeerTrust::Untrusted,
        );
    }
}
