#![deny(unsafe_code)]

mod acp;
mod config;
mod drain;
mod engram_fetch;
mod filter;
mod observer;
mod peer_call;
mod pool;
mod pool_lifecycle;
mod project;
mod provider_probe;
mod queue;
mod relay;
mod setup_mode;
mod terminal_auth;
mod terminal_auth_store;
mod usage;

pub use usage::TurnUsage;

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use acp::{AcpClient, EnvVar, McpServer};
use anyhow::{ensure, Context, Result};
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
    MultipleEventHandling, ProviderProbeArgs, RespondTo, SubscribeMode,
};
use filter::SubscriptionRule;
use futures_util::FutureExt;
use nostr::{PublicKey, ToBech32};
use pool::{
    AgentPool, ControlSignal, IdleSwitchResult, OwnedAgent, PromptContext, PromptOutcome,
    PromptResult, PromptSource, SessionState, TimeoutKind,
};
use pool_lifecycle::{PoolLifecycle, PoolStartError};
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

/// Resolve the process working directory for ACP session metadata and prompts.
///
/// `std::env::current_dir()` returns an absolute path on every supported
/// platform. Keep the explicit invariant check so a future source cannot
/// silently introduce a relative path, and surface resolution failures instead
/// of substituting a misleading Unix-specific fallback.
fn current_working_directory() -> Result<String> {
    let cwd = std::env::current_dir().context("failed to resolve current working directory")?;
    ensure!(
        cwd.is_absolute(),
        "current working directory is not absolute: {}",
        cwd.display()
    );
    Ok(cwd.to_string_lossy().into_owned())
}

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

/// Observer frames are published at a global rate of AT MOST ONE relay frame
/// per tick — not one per channel, and not one per drain. Everything that
/// accumulates between ticks waits in [`ObserverPublishQueue`] as events and
/// is packed greedily into that single frame. One update per second is smooth
/// enough for a human watching the session viewer, and the global budget is
/// what makes the relay cost model flat: observer frames bill the agent's
/// `LimitType::Messages` quota (`agent_standard_messages_per_min` = 120,
/// enforced in relay `connection.rs::enforce_ws_admission`), shared with the
/// agent's real chat messages. At 1 frame/s telemetry spends at most 60/min —
/// half that budget — regardless of how many channels are active. A slower
/// tick (e.g. 2s → 30/min) would leave more quota headroom for chat at the
/// price of doubled viewer latency; this constant is the knob.
const OBSERVER_PUBLISH_TICK: Duration = Duration::from_secs(1);

/// Byte budget for EVERYTHING retained while awaiting a publish slot: the
/// event FIFO (serialized, post-`fit_observer_event_to_budget` bytes) PLUS
/// the chunk coalescer's pending buffer (serialized event skeletons + raw
/// accumulated text). Both stores count against this one cap — a
/// high-cardinality chunk flood (many distinct coalescer keys) is bounded
/// exactly like a plain event flood; neither buffer is a bypass around the
/// other. Lossless-ness is bounded by this budget: each publish slot packs
/// one ~64KB frame, gathered queue-wide for the front channel, so a single
/// channel drains at ~64KB/s and 4 MiB buys roughly **64 seconds** of
/// sustained over-production before the oldest items are dropped WITH
/// accounting (a warn carrying the dropped-event count). With C channels
/// producing concurrently the slots round-robin between them, so the
/// per-channel drain is ~64KB/Cs and the budget shortens accordingly —
/// still bytes-per-slot, never events-per-slot (see
/// [`ObserverPublishQueue::next_frame`]). Beyond-budget floods therefore
/// degrade to designed, visible loss — strictly better than the
/// pre-batching pacer's silent 90/min drop.
const OBSERVER_PENDING_QUEUE_MAX_BYTES: usize = 4 * 1024 * 1024;

/// Observer event kind for a batch envelope wrapping multiple events.
///
/// The payload is `{"events": [<ObserverEvent>, ...]}` with every inner event
/// carrying its own `seq`/`timestamp`, so consumers process inner events
/// exactly as they would unbatched ones. Single pending events are published
/// unwrapped, so the envelope only appears when there is something to batch.
const OBSERVER_BATCH_KIND: &str = "batch";

/// Collects observer events awaiting a publish slot.
///
/// Chunk-type events ride the [`ObserverChunkCoalescer`]; everything else is
/// appended in arrival order, force-flushing pending chunks first — the same
/// ordering rule the pre-batching publisher enforced, so merged chunk text can
/// never leapfrog a tool call that arrived mid-stream.
///
/// Events wait here as EVENTS, not pre-sealed frames: each publish slot packs
/// one frame at publish time ([`Self::next_frame`]), so a backlog keeps
/// compacting into full frames instead of freezing into a frame queue.
///
/// The queue is bounded by [`OBSERVER_PENDING_QUEUE_MAX_BYTES`]. When a
/// sustained flood outruns the one-frame-per-tick drain for longer than the
/// budget, the OLDEST events are dropped (the viewer wants recent state) with
/// accounting: a warning carrying the dropped-event count, and
/// `dropped_events` for tests.
#[derive(Default)]
struct ObserverPublishQueue {
    coalescer: ObserverChunkCoalescer,
    /// `(serialized_len, source_events, event)`, oldest first. Length is
    /// captured at enqueue (post-fit) so byte accounting never re-serializes
    /// on eviction; `source_events` is how many GENERATED observer events the
    /// entry represents (a merged chunk carries every chunk it absorbed), so
    /// eviction accounting stays in source units after flush.
    events: VecDeque<(usize, u64, observer::ObserverEvent)>,
    pending_bytes: usize,
    /// SOURCE observer events lost to byte-budget eviction. Counted in
    /// generated-event units, not retained entries: a coalesced entry that
    /// merged N chunks accounts for N when evicted. A PUBLISHED merged entry
    /// delivers all N sources' text in one event, so the invariant is
    /// `ingested == dropped_events + Σ source_events over published events`.
    dropped_events: u64,
}

impl ObserverPublishQueue {
    fn ingest(&mut self, event: observer::ObserverEvent) {
        // ObserverChunkCoalescer::ingest returns immediately-publishable events
        // (force-flushed pending chunks + non-chunk passthrough, or a pending
        // set displaced by the 60KB pre-flush); they join the queue in the
        // order the coalescer emitted them, each carrying the count of source
        // events it represents.
        for (source_events, ready) in self.coalescer.ingest(event) {
            self.enqueue(source_events, ready);
        }
        self.enforce_byte_budget();
    }

    fn enqueue(&mut self, source_events: u64, mut event: observer::ObserverEvent) {
        // Pre-trim at enqueue so (a) byte accounting reflects what will ship
        // and (b) one oversized leaf cannot force every frame it touches into
        // whole-envelope elision downstream.
        fit_observer_event_to_budget(&mut event);
        let bytes = serialized_len(&event);
        self.pending_bytes += bytes;
        self.events.push_back((bytes, source_events, event));
    }

    /// Total bytes retained across BOTH stores — the event FIFO and the
    /// coalescer's pending chunk buffer. The budget binds this sum; counting
    /// only the FIFO would let a high-cardinality chunk flood (many distinct
    /// coalescer keys, nothing ever flushing) grow unbounded outside the cap.
    fn total_pending_bytes(&self) -> usize {
        self.pending_bytes + self.coalescer.pending_bytes
    }

    /// Enforce [`OBSERVER_PENDING_QUEUE_MAX_BYTES`] over the total, dropping
    /// OLDEST items first with accounting in SOURCE-event units. Global age
    /// order across the two stores is structural: every enqueue path flushes
    /// the coalescer first, so every pending coalescer entry is strictly newer
    /// than every queued event — eviction is queue front, then coalescer
    /// front. The `> 1` guard never drops the sole remaining item (any single
    /// fitted event or pre-flush-capped chunk entry is far under the budget).
    fn enforce_byte_budget(&mut self) {
        let mut dropped = 0u64;
        while self.total_pending_bytes() > OBSERVER_PENDING_QUEUE_MAX_BYTES
            && self.events.len() + self.coalescer.pending.len() > 1
        {
            if let Some((bytes, source_events, _)) = self.events.pop_front() {
                self.pending_bytes -= bytes;
                dropped += source_events;
            } else {
                dropped += self.coalescer.drop_oldest().expect("guard ensures an item");
            }
        }
        if dropped > 0 {
            self.dropped_events += dropped;
            tracing::warn!(
                dropped,
                total_dropped = self.dropped_events,
                pending_bytes = self.total_pending_bytes(),
                "observer publish queue over byte budget; dropped oldest events"
            );
        }
    }

    /// True when nothing is waiting anywhere — the event queue AND the
    /// coalescer's pending chunk buffer.
    fn is_empty(&self) -> bool {
        self.events.is_empty() && self.coalescer.pending.is_empty()
    }

    /// Pack and remove AT MOST ONE publishable frame: the front event's
    /// channel, gathered queue-wide in FIFO order (packed greedily until
    /// adding the next event would push the envelope over
    /// `OBSERVER_MAX_PLAINTEXT_LEN`). Singletons ship unwrapped.
    ///
    /// Two invariants bound the gather:
    /// - A frame never mixes scopes — neither channels (the desktop archive
    ///   indexes a frame under its decrypted top-level `channelId`) nor
    ///   project roots (project turns all carry `channel_id: None`, so
    ///   channel alone does not separate them), and events keep their
    ///   FIFO order *within* each channel. Cross-channel frame order MAY
    ///   differ from arrival order — the desktop tolerates that everywhere:
    ///   the transcript store sorts + rebuilds on out-of-order arrival, the
    ///   archive is per-channel by construction, and the turn store's
    ///   watermark is keyed per (agent, channel).
    /// - A NULL-channel event is a BARRIER nothing gathers across: null-scope
    ///   events (`agent_panic`-class) can causally couple to any channel, so
    ///   their relative order against every channel is preserved exactly.
    ///   Null-channel events themselves ship only as their contiguous front
    ///   run.
    ///
    /// Gathering queue-wide (not just the front run) is what keeps the drain
    /// rate in BYTES per slot rather than front-run-length events per slot:
    /// with round-robin producers (channel A, B, A, B, ...) a front-run
    /// packer degrades to ~1 event per slot regardless of size, silently
    /// growing latency without ever tripping the byte budget.
    ///
    /// Pending coalesced chunks are flushed into the queue first, so a
    /// publish slot never leaves merged chunk text stranded behind the tick.
    fn next_frame(&mut self) -> Option<observer::ObserverEvent> {
        for (source_events, ready) in self.coalescer.flush() {
            self.enqueue(source_events, ready);
        }
        let channel = self.events.front()?.2.channel_id.clone();
        let project = self.events.front()?.2.project.clone();

        let mut picked: Vec<observer::ObserverEvent> = Vec::new();
        let mut kept: VecDeque<(usize, u64, observer::ObserverEvent)> =
            VecDeque::with_capacity(self.events.len());
        let mut gathering = true;
        while let Some((bytes, source_events, event)) = self.events.pop_front() {
            if gathering && event.channel_id == channel && event.project == project {
                picked.push(event);
                if picked.len() > 1
                    && serialized_len(&batch_envelope(&picked)) > OBSERVER_MAX_PLAINTEXT_LEN
                {
                    // Frame full: the overflow event stays queued and leads
                    // its channel's next slot.
                    let event = picked.pop().expect("len > 1");
                    kept.push_back((bytes, source_events, event));
                    gathering = false;
                } else {
                    self.pending_bytes -= bytes;
                }
            } else {
                if gathering && (channel.is_none() || event.channel_id.is_none()) {
                    // Null-channel barrier (or, for a null-channel frame, the
                    // end of its contiguous front run): stop gathering.
                    gathering = false;
                }
                kept.push_back((bytes, source_events, event));
            }
        }
        self.events = kept;
        Some(seal_batch(picked))
    }
}

/// A single event ships unwrapped; two or more get the batch envelope.
fn seal_batch(mut events: Vec<observer::ObserverEvent>) -> observer::ObserverEvent {
    if events.len() == 1 {
        return events.pop().expect("len == 1");
    }
    batch_envelope(&events)
}

/// Build the batch envelope for a set of same-channel events.
///
/// Envelope metadata mirrors the LAST inner event — the same convention the
/// chunk coalescer uses for merged chunks — so `(timestamp, seq)` ordering and
/// the desktop's latest-live-session tracking see the newest state.
fn batch_envelope(events: &[observer::ObserverEvent]) -> observer::ObserverEvent {
    let last = events
        .last()
        .expect("batch envelope needs at least 1 event");
    observer::ObserverEvent {
        seq: last.seq,
        timestamp: last.timestamp.clone(),
        kind: OBSERVER_BATCH_KIND.to_string(),
        agent_index: last.agent_index,
        channel_id: last.channel_id.clone(),
        project: last.project.clone(),
        session_id: last.session_id.clone(),
        turn_id: last.turn_id.clone(),
        started_at: last.started_at.clone(),
        payload: serde_json::json!({
            "events": serde_json::to_value(events).unwrap_or_default(),
        }),
    }
}

/// May this configuration publish encrypted NIP-AO owner telemetry?
///
/// `relay_observer` and nothing else. Kept as a named predicate beside
/// [`observer_bus_for`] so the two questions cannot drift back together: the
/// bus is shared, the *encryption and publication* of transcripts is not, and
/// the whole point of the correction that produced this pair is that one
/// feature's switch stopped silently answering for the other.
fn encrypted_telemetry_enabled(config: &Config) -> bool {
    config.relay_observer
}

/// The in-process observer bus, when anything in this configuration needs one.
///
/// **Two consumers, one bus.** Encrypted NIP-AO telemetry is one of them and
/// used to be the only one, so the bus was allocated from `relay_observer`
/// alone. Public NIP-PA project activity is the other, and it reads the same
/// frames — so with `project_routing_enabled` on and `relay_observer` off, the
/// activity publisher was spawned against a handle that did not exist and no
/// `20003` was ever emitted. The publisher had been carefully un-gated from
/// `--relay-observer` while its sole input was still gated by it.
///
/// The bus itself is cheap and inert: a broadcast channel and a bounded replay
/// buffer, filled only by `emit` calls the harness makes anyway. What is
/// expensive — encrypting a frame per owner and publishing it — stays behind
/// `relay_observer` at [`spawn_relay_observer_publisher`]. So allocating here
/// for either feature grants neither feature's cost to the other.
///
/// Extracted as a function so the combinations can be asserted from the same
/// `Config` values production reads, rather than only by starting a harness.
fn observer_bus_for(config: &Config) -> Option<observer::ObserverHandle> {
    (config.relay_observer || config.project_routing_enabled)
        .then(observer::ObserverHandle::in_process)
}

// ── NIP-PA: project activity ──────────────────────────────────────────────────

/// How often a live turn re-announces `working` while it runs.
///
/// Kind 20003 is ephemeral, so a desktop that opens an issue mid-turn has
/// already missed every frame sent before it subscribed. Without a refresh the
/// indicator only ever appears for a client that was watching when the turn
/// began, which is the minority case.
const PROJECT_ACTIVITY_REFRESH: std::time::Duration = std::time::Duration::from_secs(15);

/// Observer kinds that end a turn.
///
/// `turn_ending` is deliberately absent: it is emitted while the agent is still
/// finishing, and clearing on it would blank the indicator for the last part of
/// every turn.
const TURN_TERMINAL_KINDS: &[&str] = &["turn_completed", "turn_error"];

/// The observer kind the dispatch gate emits when it queues a project event.
///
/// Synthetic — no ACP message corresponds to it, because what it reports
/// happened before any agent process was involved: the harness read a comment,
/// decided it was addressed, and put it in the queue. Until this existed the
/// wire was silent from "comment posted" until `turn_started`, which on a busy
/// pool is minutes, and silence is also what an unaddressed comment produces.
///
/// It travels on the observer bus rather than being published straight from the
/// dispatch site, and that is the whole point of the seam. The bus is
/// [`ProjectActivityPublisher`]'s single input; a second publisher would keep
/// its own idea of which root is live, and the two would disagree the first
/// time either missed a frame — which is the failure mode this file's other
/// doc comments are already shaped around.
pub(crate) const OBSERVER_PROJECT_QUEUED: &str = "project_event_queued";

/// The `turn` tag a queued announcement carries.
///
/// NIP-PA requires exactly one `turn`, and at queue time there is no turn to
/// name: the id is minted at flush, and one flush may fold every event pending
/// on a root into a single turn. So the announcement is identified by the event
/// that caused it, which is the one fact that exists and is already unique.
///
/// Prefixed rather than bare so it can never be read as a turn id: a real one
/// is a UUID, this is `queued:<64-hex>`, and the two cannot collide even by
/// accident. Nothing downstream ever has to match it against a turn — the
/// `working` frame that follows supersedes it by root, and the only `idle` that
/// ever names it is the one this publisher emits for the queued announcement
/// itself.
fn queued_turn_id(event_id: &str) -> String {
    format!("queued:{event_id}")
}

/// The caption every command execution gets, in place of its command line.
///
/// One phrase for every shell, every adapter and every machine: it is the only
/// sentence that is true of all of them and discloses nothing. See
/// [`ProjectActivityPublisher::command_caption`] for why the command itself
/// cannot go here.
const COMMAND_CAPTION: &str = "running a command";

/// How long a first token may be before it stops being a program name.
///
/// A real program name is short. Past this, the token is far more likely to be
/// a base64 blob, a URL or a quoted argument that happened to have no space in
/// it — none of which belong on a public root.
const MAX_CAPTIONED_PROGRAM_LEN: usize = 20;

/// First tokens that name how a command is run rather than what is run.
///
/// Publishing "running a command (env)" would be strictly worse than the bare
/// label: it is no more informative, and it advertises that the real command is
/// being wrapped — which is exactly the invocation whose arguments are the
/// environment detail this caption exists to keep off a public root.
const COMMAND_WRAPPERS: &[&str] = &[
    "env", "sudo", "doas", "nohup", "nice", "time", "timeout", "xargs",
];

/// What this agent is currently announcing on one project root.
struct LiveProjectTurn {
    turn_id: String,
    coordinate: String,
    stage: Option<String>,
    /// Which state is on the wire for this root right now.
    ///
    /// Stored rather than inferred because two rules need it: the refresh tick
    /// re-announces *what is true* instead of assuming a turn is running, and a
    /// queued announcement must never displace a live one.
    state: buzz_sdk::builders::ProjectActivityState,
    announced_at: tokio::time::Instant,
}

/// Publish NIP-PA activity for project-routed turns.
///
/// Reads the same in-process observer bus the encrypted NIP-AO frames are built
/// from, which is what keeps the two accounts of a turn from drifting: there is
/// one lifecycle, and this is a projection of it rather than a second set of
/// emit calls sprinkled through the pool that somebody will forget to update.
///
/// Runs as its own subscriber rather than inside the NIP-AO publisher, because
/// project activity is public-to-the-project and NIP-AO is owner-scoped
/// telemetry behind its own flag. Making the visible indicator depend on
/// `--relay-observer` would mean an issue silently shows nothing whenever
/// telemetry happens to be off.
struct ProjectActivityPublisher {
    keys: nostr::Keys,
    agent_hex: String,
    live: std::collections::HashMap<String, LiveProjectTurn>,
}

impl ProjectActivityPublisher {
    fn new(keys: nostr::Keys, agent_hex: String) -> Self {
        Self {
            keys,
            agent_hex,
            live: std::collections::HashMap::new(),
        }
    }

    /// A short label for what the agent is doing, or `None` for frames that
    /// say nothing a person would want read aloud.
    ///
    /// **Read from the ACP `session/update` the agent sent, not from the
    /// observer kind.** The two frames that carry work — `acp_read` and
    /// `acp_write` — name a *direction on the pipe*: `acp_read` is "the harness
    /// read a line from the agent" and fires for every message chunk, thought
    /// and notification, while `acp_write` is "the harness wrote to the agent's
    /// stdin" and fires for permission answers and keepalives. Captioning them
    /// "reading files" and "editing files" described the transport and claimed
    /// it was the work: an agent that had touched no file all turn still
    /// announced `working — reading files`, and the caption flapped to
    /// `editing files` whenever the harness answered it. `tool_call` was worse
    /// than wrong — no observer event is ever emitted under that kind, so the
    /// arm was dead and the one frame that does name a tool never reached it.
    ///
    /// So the caption comes from `params.update` of a `session/update`
    /// notification, which is the agent's own account of what it is doing and
    /// is the same payload the desktop transcript already renders. It is ACP
    /// data, so every compliant agent produces it — nothing here knows which
    /// harness is on the other end, and nothing needs to.
    fn stage_for(event: &observer::ObserverEvent) -> Option<String> {
        match event.kind.as_str() {
            "turn_started" => Some("starting".to_string()),
            "session_resolved" => Some("thinking".to_string()),
            // Inbound JSON-RPC. Only a session/update says anything about the
            // work; every other method is transport.
            "acp_read" => {
                if event.payload.get("method").and_then(|v| v.as_str())? != "session/update" {
                    return None;
                }
                Self::stage_for_session_update(event.payload.pointer("/params/update")?)
            }
            _ => None,
        }
    }

    /// The caption for one ACP `session/update`, or `None` when the update is
    /// not about work a reader of the issue would want narrated.
    ///
    /// `tool_call` carries the agent's own `title` — "Read AGENTS.md",
    /// "Running rtk git log" — which is exactly the sentence NIP-PA's `stage`
    /// asks for and is better than anything this file could synthesise. The
    /// builder trims, collapses and bounds it before it reaches the wire.
    ///
    /// **Except for command execution, where the title is not a sentence about
    /// the work — it *is* the command line.** See [`Self::command_caption`] for
    /// why that one kind is captioned rather than quoted.
    ///
    /// `tool_call_update` is folded in for its `title` alone: an agent that
    /// opens a call with a placeholder and names it on the first update would
    /// otherwise never have the real title read. Its `status` is deliberately
    /// ignored — "completed" is not a thing the agent is *doing*, and
    /// announcing it would blank the caption between tools. An update is judged
    /// by the `kind` *it* carries: the publisher keys its state by root rather
    /// than by tool call id, so an update that omits `kind` cannot be married
    /// back to the call that opened it, and its title is taken at face value.
    /// That is the observed adapter shape — a call names its kind and its
    /// updates carry only a status — so the gap costs nothing today, and
    /// closing it would mean tracking every open call id per root.
    fn stage_for_session_update(update: &serde_json::Value) -> Option<String> {
        let str_field = |key: &str| {
            update
                .get(key)
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
        };
        match update.get("sessionUpdate").and_then(|v| v.as_str())? {
            "tool_call" => {
                let kind = str_field("kind");
                if Self::is_command_execution_kind(kind) {
                    return Some(Self::command_caption(str_field("title")));
                }
                Some(
                    str_field("title")
                        .map(str::to_string)
                        .unwrap_or_else(|| Self::stage_for_tool_kind(kind).to_string()),
                )
            }
            "tool_call_update" => {
                let kind = str_field("kind");
                str_field("title").map(|title| {
                    if Self::is_command_execution_kind(kind) {
                        Self::command_caption(Some(title))
                    } else {
                        title.to_string()
                    }
                })
            }
            "agent_thought_chunk" => Some("thinking".to_string()),
            "agent_message_chunk" => Some("writing a reply".to_string()),
            "plan" => Some("planning".to_string()),
            _ => None,
        }
    }

    /// Whether an ACP tool `kind` means "the agent is running a shell command".
    ///
    /// `execute` is ACP's own spelling and the only kind whose `title` is
    /// conventionally the command line itself rather than a description of it —
    /// see [`Self::stage_for_tool_kind`], which is where this build's reading of
    /// the `ToolKind` vocabulary lives. Matched exactly: a kind this build has
    /// never heard of is not silently treated as a shell, because the honest
    /// answer for an unknown kind is its title.
    fn is_command_execution_kind(kind: Option<&str>) -> bool {
        kind == Some("execute")
    }

    /// The caption for a command execution, which is deliberately *not* the
    /// command.
    ///
    /// A `stage` rides a **public, unencrypted** kind:20003 — unlike the NIP-AO
    /// transcript, which is encrypted to the owner — so every reader of the
    /// issue sees it. An `execute` title is the raw command line, and quoting it
    /// there fails twice over. It is unreadable: the indicator is one short line
    /// beside a name, and `env -u BUZZ_RELAY_URL -u BUZZ_PRIVATE_KEY
    /// PYTHONPATH=. /home/…` truncates to noise. And it discloses: absolute
    /// paths, the names of environment variables the operator unsets, hostnames
    /// and flags are all operational detail about a private machine that nobody
    /// chose to publish by filing an issue.
    ///
    /// So the caption says the true, short thing instead. The first token is
    /// appended only when it is a bare program name, because "running a command
    /// (cargo)" tells a reader what is happening while disclosing nothing they
    /// could not infer from the repository. Anything that is a path, an
    /// assignment or a wrapper falls back to the bare label rather than
    /// publishing a fragment of the command line.
    fn command_caption(title: Option<&str>) -> String {
        match title.and_then(Self::bare_program_name) {
            Some(program) => format!("{COMMAND_CAPTION} ({program})"),
            None => COMMAND_CAPTION.to_string(),
        }
    }

    /// The first token of a command line, when it names a program plainly
    /// enough to be worth publishing.
    ///
    /// The test is a whitelist rather than a blacklist, because the thing being
    /// guarded against is unbounded free text: only ASCII letters, digits and
    /// `-_.+` pass. That excludes every disclosing shape at once — `/usr/bin/x`
    /// and `./script` by the slash, `PYTHONPATH=.` by the equals sign, and
    /// quoting, substitution and redirection by everything else — without this
    /// function needing to enumerate them.
    fn bare_program_name(title: &str) -> Option<&str> {
        let token = title.split_whitespace().next()?;
        if token.len() > MAX_CAPTIONED_PROGRAM_LEN {
            return None;
        }
        if !token
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '+'))
        {
            return None;
        }
        if COMMAND_WRAPPERS.contains(&token) {
            return None;
        }
        Some(token)
    }

    /// The fallback caption for a tool call that arrived without a title.
    ///
    /// The words are ACP's own `ToolKind` variants. An unknown one is captioned
    /// honestly rather than guessed at: a kind this build has never heard of is
    /// still a tool, and "running a tool" is the true sentence about it.
    fn stage_for_tool_kind(kind: Option<&str>) -> &'static str {
        match kind.unwrap_or_default() {
            "read" => "reading files",
            "edit" => "editing files",
            "delete" => "deleting files",
            "move" => "moving files",
            "search" => "searching",
            "execute" => "running a command",
            "think" => "thinking",
            "fetch" => "fetching",
            "switch_mode" => "switching mode",
            _ => "running a tool",
        }
    }

    /// Fold one observer event, returning the activity events to publish.
    ///
    /// Pure with respect to the relay: it decides, the caller sends. Returning
    /// the events rather than sending them is what makes the refresh, dedup and
    /// terminal rules testable without a socket.
    fn ingest(
        &mut self,
        event: &observer::ObserverEvent,
        now: tokio::time::Instant,
    ) -> Vec<nostr::EventBuilder> {
        let Some(route) = event.project.as_ref() else {
            return Vec::new();
        };
        let Some(turn_id) = event.turn_id.as_deref() else {
            return Vec::new();
        };

        let repo = buzz_sdk::GitRepoCoord::from_a_tag_value(&route.coordinate);
        let Some(repo) = repo else {
            tracing::warn!(
                coordinate = %route.coordinate,
                "project activity: unreadable repository coordinate — not announcing"
            );
            return Vec::new();
        };

        if event.kind == OBSERVER_PROJECT_QUEUED {
            // A queued announcement never displaces what is already on the
            // root, and the direction matters: `working` is a strictly stronger
            // claim about the same root, made by the same agent, and a second
            // comment arriving mid-turn would otherwise walk the indicator
            // backwards from "working — editing files" to "queued" while the
            // agent was demonstrably still editing files.
            //
            // A root already announcing `queued` says nothing new either. The
            // rendered state is identical, so re-announcing under the newer
            // event's id would buy a different `turn` tag and nothing else, at
            // the cost of a relay publish per comment on a backlogged root.
            if self.live.contains_key(&route.root) {
                return Vec::new();
            }
            self.live.insert(
                route.root.clone(),
                LiveProjectTurn {
                    turn_id: turn_id.to_string(),
                    coordinate: route.coordinate.clone(),
                    // No stage: nothing is happening yet, and a caption here
                    // would describe work that has not begun.
                    stage: None,
                    state: buzz_sdk::builders::ProjectActivityState::Queued,
                    announced_at: now,
                },
            );
            return build_project_activity_or_warn(
                &repo,
                &route.root,
                &self.agent_hex,
                buzz_sdk::builders::ProjectActivityState::Queued,
                turn_id,
                None,
            )
            .into_iter()
            .collect();
        }

        if TURN_TERMINAL_KINDS.contains(&event.kind.as_str()) {
            // Whatever is cleared is cleared under *its own* turn tag, because
            // a consumer ignores an `idle` naming a turn it is not showing —
            // that rule is the reason `idle` is safe at all.
            let announced = match self.live.get(&route.root) {
                // Only the turn that is actually being shown may clear it. A
                // late terminal frame from a turn that ended before this one
                // started would otherwise blank an indicator for work still in
                // progress.
                Some(live) if live.turn_id == turn_id => live.turn_id.clone(),
                // A queued announcement holds no turn id a terminal frame could
                // ever match, so the rule above would strand it. It must not:
                // a turn ending on this root has drained that root's queue — a
                // flush takes every event pending on the key at once — so work
                // still announcing itself as queued after one has ended is
                // stale by construction. This is also the only bound on a
                // queued announcement that the refresh tick keeps alive
                // indefinitely, so it is deliberately the widest safe rule
                // rather than a turn-id match.
                Some(live) if live.state == buzz_sdk::builders::ProjectActivityState::Queued => {
                    live.turn_id.clone()
                }
                _ => return Vec::new(),
            };
            self.live.remove(&route.root);
            return build_project_activity_or_warn(
                &repo,
                &route.root,
                &self.agent_hex,
                buzz_sdk::builders::ProjectActivityState::Idle,
                &announced,
                None,
            )
            .into_iter()
            .collect();
        }

        let stage = Self::stage_for(event);
        let announce = match self.live.get(&route.root) {
            // A different turn on the same root: announce immediately, so the
            // stale turn's id stops being the one an `idle` must match.
            //
            // This is also the queued→working transition, and it needs no rule
            // of its own: a queued announcement is keyed to the event that
            // caused it, never to a turn id, so the turn that starts is always
            // "a different turn" and always replaces it at once.
            Some(live) if live.turn_id != turn_id => true,
            Some(live) => {
                let moved_on = stage.is_some() && stage != live.stage;
                moved_on || now.duration_since(live.announced_at) >= PROJECT_ACTIVITY_REFRESH
            }
            None => true,
        };
        if !announce {
            return Vec::new();
        }

        // Carry the previous stage when this frame has none of its own: a
        // refresh tick during a long tool call should re-announce what the
        // agent is doing, not blank the caption it already showed.
        let stage = stage.or_else(|| {
            self.live
                .get(&route.root)
                .filter(|live| live.turn_id == turn_id)
                .and_then(|live| live.stage.clone())
        });
        self.live.insert(
            route.root.clone(),
            LiveProjectTurn {
                turn_id: turn_id.to_string(),
                coordinate: route.coordinate.clone(),
                stage: stage.clone(),
                state: buzz_sdk::builders::ProjectActivityState::Working,
                announced_at: now,
            },
        );
        build_project_activity_or_warn(
            &repo,
            &route.root,
            &self.agent_hex,
            buzz_sdk::builders::ProjectActivityState::Working,
            turn_id,
            stage.as_deref(),
        )
        .into_iter()
        .collect()
    }

    /// Re-announce every live root whose last announcement has aged out.
    ///
    /// **`queued` refreshes exactly like `working`, and that is a decision.**
    /// The alternative — announce it once and let the consumer's 45-second
    /// expiry cap it — is cheaper on the wire and wrong for the only case the
    /// state exists to cover: a comment waits precisely when the pool is busy,
    /// which is routinely longer than 45 seconds. Letting it expire would put
    /// the issue back into silence while the work was still genuinely pending,
    /// re-creating the ambiguity ("was anyone addressed?") one refresh cycle
    /// later instead of removing it.
    ///
    /// Expiry remains the terminator for a harness that dies, and the terminal
    /// rule in [`Self::ingest`] is what stops a queued announcement outliving
    /// the queue it describes.
    fn refresh(&mut self, now: tokio::time::Instant) -> Vec<nostr::EventBuilder> {
        let mut out = Vec::new();
        for (root, live) in self.live.iter_mut() {
            if now.duration_since(live.announced_at) < PROJECT_ACTIVITY_REFRESH {
                continue;
            }
            let Some(repo) = buzz_sdk::GitRepoCoord::from_a_tag_value(&live.coordinate) else {
                continue;
            };
            live.announced_at = now;
            out.extend(build_project_activity_or_warn(
                &repo,
                root,
                &self.agent_hex,
                live.state,
                &live.turn_id,
                live.stage.as_deref(),
            ));
        }
        out
    }

    fn sign(&self, builder: nostr::EventBuilder) -> Option<nostr::Event> {
        builder
            .sign_with_keys(&self.keys)
            .map_err(|error| {
                tracing::warn!(%error, "project activity: signing failed");
            })
            .ok()
    }
}

/// Build one activity event, or warn and produce nothing.
///
/// A refusal here is a bug in the caller's inputs, not something to retry: the
/// coordinate and root came off a validated project origin. It is logged rather
/// than propagated because an unannounceable turn must still run.
fn build_project_activity_or_warn(
    repo: &buzz_sdk::GitRepoCoord,
    root: &str,
    agent_hex: &str,
    state: buzz_sdk::builders::ProjectActivityState,
    turn_id: &str,
    stage: Option<&str>,
) -> Option<nostr::EventBuilder> {
    match buzz_sdk::builders::build_project_activity(repo, root, agent_hex, state, turn_id, stage) {
        Ok(builder) => Some(builder),
        Err(error) => {
            tracing::warn!(%error, root = %root, "project activity: refused by the builder");
            None
        }
    }
}

/// Say, on the observer bus, that a project event has been accepted for a turn
/// that has not started.
///
/// This is the producing half of [`OBSERVER_PROJECT_QUEUED`], called from the
/// dispatch gate the moment the queue accepts the event. It publishes nothing
/// itself and holds no relay capability: it emits, and
/// [`ProjectActivityPublisher`] decides whether that becomes a `20003`. Keeping
/// the decision there is what lets one place — the publisher — answer "what is
/// this root announcing", instead of the gate and the publisher each having an
/// opinion.
///
/// `None` for the bus is the ordinary configuration in which neither project
/// routing nor telemetry is on, and is silently nothing to do: an unannounceable
/// queue insertion must still queue.
///
/// `agent_index` is `None` because no agent process is involved yet — that is
/// the whole content of the signal.
fn observe_project_event_queued(
    observer: Option<&observer::ObserverHandle>,
    origin: &project::ProjectOrigin,
    event_id: &str,
) {
    let Some(observer) = observer else {
        return;
    };
    observer.emit(
        OBSERVER_PROJECT_QUEUED,
        None,
        &observer::ObserverContext {
            channel_id: None,
            project: Some(observer::ProjectRouteRef {
                coordinate: origin.coordinate().to_string(),
                root: origin.root().to_string(),
            }),
            session_id: None,
            turn_id: Some(queued_turn_id(event_id)),
            started_at: None,
        },
        serde_json::json!({
            "eventId": event_id,
            "class": origin.class_noun(),
        }),
    );
}

/// Take back the `queued` announcements this process will not honour.
///
/// A project event that reached the queue lit an indicator on its issue —
/// [`observe_project_event_queued`] → NIP-PA `state=queued`. That is a promise,
/// and a drain that expired with a backlog is a process leaving with the
/// promise unkept. Emitting a terminal frame per abandoned root clears the
/// indicator now instead of leaving it to the consumer's staleness window,
/// which is the difference between "this agent stopped" and "this agent is
/// still thinking about it" for the 45 seconds after the process is gone.
///
/// Emitted as `turn_error` rather than `turn_completed` because it is true:
/// the work was admitted and never ran. Both are in
/// [`TURN_TERMINAL_KINDS`], so either would clear the indicator — the
/// choice is about what the owner's telemetry records, not about what the
/// issue shows.
///
/// The turn tag is the queued pseudo-id ([`queued_turn_id`]) rather than a real
/// turn id, because no turn ever existed. The publisher's terminal rule clears
/// a root that is announcing `queued` regardless of which id the frame names,
/// which is precisely the clause that exists for announcements no turn id could
/// ever match.
///
/// Uses [`EventQueue::drain_all_pending_batches`], so this both enumerates and
/// empties: after a drain exit there is nothing left to dispatch, and leaving
/// the batches in place would mean the queue and the wire disagreed about
/// whether that work was still pending. Batches belonging to an in-flight turn
/// are deliberately not touched — that turn owns them, its root is announcing
/// `working`, not `queued`, and its own completion is what clears it.
///
/// A clean drain (`DrainExit::Complete`) finds nothing here, because "complete"
/// is defined as the queue holding nothing. That is the intended common case:
/// this function exists for the bounded-out one.
fn clear_queued_project_announcements(
    queue: &mut EventQueue,
    observer: Option<&observer::ObserverHandle>,
) {
    let abandoned = queue.drain_all_pending_batches();
    if abandoned.is_empty() {
        return;
    }
    let events: usize = abandoned.iter().map(|batch| batch.events.len()).sum();
    tracing::warn!(
        batches = abandoned.len(),
        events,
        "drain: abandoning queued work — it stays on the relay for the next process"
    );
    let Some(observer) = observer else {
        return;
    };
    for batch in &abandoned {
        let Some(origin) = batch.project_origin() else {
            // A channel batch announced nothing to take back: the 👀 reaction
            // is the only visible mark it left, and removing it would claim the
            // event was never seen when the successor will see it again.
            continue;
        };
        let Some(first) = batch
            .events
            .first()
            .or_else(|| batch.cancelled_events.first())
        else {
            continue;
        };
        observer.emit(
            "turn_error",
            None,
            &observer::ObserverContext {
                channel_id: None,
                project: Some(observer::ProjectRouteRef {
                    coordinate: origin.coordinate().to_string(),
                    root: origin.root().to_string(),
                }),
                session_id: None,
                turn_id: Some(queued_turn_id(&first.event.id.to_hex())),
                started_at: None,
            },
            serde_json::json!({
                "error": "drained",
                "detail": "the runtime drained before this work ran; the next process will see it again",
            }),
        );
    }
}

/// Drive [`ProjectActivityPublisher`] from the observer bus.
///
/// The snapshot is deliberately **not** replayed. It is a buffer of everything
/// that has already happened, and announcing from it would resurrect turns that
/// finished before this task started.
async fn run_project_activity_publisher(
    mut rx: tokio::sync::broadcast::Receiver<observer::ObserverEvent>,
    publisher: RelayEventPublisher,
    keys: nostr::Keys,
    agent_hex: String,
) {
    let mut state = ProjectActivityPublisher::new(keys, agent_hex);
    let mut refresh = tokio::time::interval(PROJECT_ACTIVITY_REFRESH);
    refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        let builders = tokio::select! {
            result = rx.recv() => match result {
                Ok(event) => state.ingest(&event, tokio::time::Instant::now()),
                // A lagged receiver has missed frames, not turns: the next frame
                // of a live turn re-announces it and the refresh tick covers a
                // quiet one. Nothing here needs the frames that were dropped.
                Err(tokio::sync::broadcast::error::RecvError::Lagged(count)) => {
                    tracing::warn!(dropped = count, "project activity publisher lagged");
                    Vec::new()
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            },
            _ = refresh.tick() => state.refresh(tokio::time::Instant::now()),
        };
        for builder in builders {
            if let Some(event) = state.sign(builder) {
                if let Err(error) = publisher.publish_event(event).await {
                    tracing::warn!(%error, "project activity: publish failed");
                }
            }
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
    let mut queue = ObserverPublishQueue::default();
    let max_snapshot_seq = snapshot.iter().map(|event| event.seq).max().unwrap_or(0);
    for event in snapshot {
        queue.ingest(event);
    }

    // Global pacer: AT MOST ONE relay frame per tick, no matter how many
    // channels are active or how large the backlog is. `interval_at` starts
    // the first tick a full period out, so a pre-loaded snapshot (up to the
    // 1,000-event replay buffer on reconnect) cannot burst at t=0 — the old
    // pacer's explicit "no initial burst" property, restored.
    let mut publish_tick = tokio::time::interval_at(
        tokio::time::Instant::now() + OBSERVER_PUBLISH_TICK,
        OBSERVER_PUBLISH_TICK,
    );
    publish_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut closed = false;
    loop {
        tokio::select! {
            result = rx.recv(), if !closed => {
                match result {
                    Ok(event) => {
                        // Skip live events already delivered via the snapshot
                        // (the subscribe-before-snapshot overlap).
                        if event.seq <= max_snapshot_seq {
                            continue;
                        }
                        queue.ingest(event);
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(count)) => {
                        tracing::warn!(dropped = count, "relay observer publisher lagged");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        // Producer gone: stop selecting on the receiver and let
                        // the tick arm drain what remains — still one frame per
                        // tick. An unpaced final drain would be a burst bypass
                        // around everything the pacer exists to prevent.
                        closed = true;
                    }
                }
            }
            _ = publish_tick.tick() => {
                if let Some(frame) = queue.next_frame() {
                    publish_relay_observer_event(
                        &publisher, &keys, &agent_pubkey_hex,
                        &owner_pubkey_hex, &owner_pubkey, frame,
                    ).await;
                }
                if closed && queue.is_empty() {
                    break;
                }
            }
        }
    }
}

#[derive(Default)]
struct ObserverChunkCoalescer {
    pending: Vec<PendingObserverChunk>,
    /// Approximate serialized bytes retained in `pending` (each entry's
    /// serialized skeleton at creation plus appended chunk text). Counted
    /// against [`OBSERVER_PENDING_QUEUE_MAX_BYTES`] by the owning
    /// [`ObserverPublishQueue`] so this buffer can never grow outside the
    /// queue's byte budget (a distinct-key chunk flood parks everything here
    /// and nothing would otherwise bound it).
    pending_bytes: usize,
}

struct PendingObserverChunk {
    key: ObserverChunkKey,
    event: observer::ObserverEvent,
    text: String,
    /// Bytes this entry contributes to `pending_bytes`.
    bytes: usize,
    /// GENERATED observer events merged into this entry (1 at creation, +1
    /// per absorbed chunk). Evicting the entry loses this many source events,
    /// so drop accounting must charge this count, not 1.
    source_events: u64,
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
    /// Returns immediately-publishable events, each paired with the number of
    /// SOURCE observer events it represents (merged chunks carry the count of
    /// every chunk they absorbed; passthrough events are always 1).
    fn ingest(&mut self, event: observer::ObserverEvent) -> Vec<(u64, observer::ObserverEvent)> {
        let Some((key, text)) = observer_chunk_key_and_text(&event) else {
            let mut events = self.flush();
            events.push((1, event));
            return events;
        };

        if let Some(pending) = self.pending.iter_mut().find(|pending| pending.key == key) {
            // Flush before appending if this would exceed the plaintext size limit.
            if pending.text.len() + text.len() >= OBSERVER_CHUNK_MAX_TEXT_BYTES {
                let events = self.flush();
                // Start a new pending entry with the current chunk.
                self.push_pending(key, event, text);
                return events;
            }
            pending.text.push_str(&text);
            pending.bytes += text.len();
            pending.source_events += 1;
            self.pending_bytes += text.len();
            pending.event.seq = event.seq;
            pending.event.timestamp = event.timestamp;
            return Vec::new();
        }

        self.push_pending(key, event, text);
        Vec::new()
    }

    fn push_pending(
        &mut self,
        key: ObserverChunkKey,
        event: observer::ObserverEvent,
        text: String,
    ) {
        // The entry RETAINS the first chunk's text twice until flush: once
        // inside the serialized skeleton (`event.payload` still carries it)
        // and once as the extracted `text` copy that appends grow. Both are
        // real memory, so both count — charging only `serialized_len` lets a
        // high-cardinality flood retain up to 2x the byte budget (each entry
        // undercounts by exactly its first chunk's length).
        let bytes = serialized_len(&event) + text.len();
        self.pending_bytes += bytes;
        self.pending.push(PendingObserverChunk {
            key,
            event,
            text,
            bytes,
            source_events: 1,
        });
    }

    /// Evict the OLDEST pending entry for byte-budget enforcement. Returns
    /// the number of SOURCE events the entry represented (its merged chunk
    /// count), or `None` when there is nothing to drop.
    fn drop_oldest(&mut self) -> Option<u64> {
        if self.pending.is_empty() {
            return None;
        }
        let removed = self.pending.remove(0);
        self.pending_bytes -= removed.bytes;
        Some(removed.source_events)
    }

    fn flush(&mut self) -> Vec<(u64, observer::ObserverEvent)> {
        self.pending_bytes = 0;
        self.pending
            .drain(..)
            .map(|mut pending| {
                set_observer_chunk_text(&mut pending.event.payload, pending.text);
                (pending.source_events, pending.event)
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
    mut event: observer::ObserverEvent,
) {
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

/// Route one owner-signed observer control frame.
///
/// Returns the drain onset when the frame was a drain, and `None` for every
/// other outcome — refused, unknown, or a different command. The run loop reads
/// that to emit the runtime lifecycle frame, which it can and this cannot: the
/// lifecycle identity (start nonce, relay URL) belongs to the loop, and
/// threading three more strings through here to reach one `emit` would have
/// coupled every control command to the runtime's identity in order to serve
/// one of them.
#[allow(clippy::too_many_arguments)]
fn handle_relay_observer_control_event(
    keys: &nostr::Keys,
    event: nostr::Event,
    pool: &mut AgentPool,
    queue: &mut EventQueue,
    observer: Option<&observer::ObserverHandle>,
    owner_pubkey_hex: &str,
    drain: &mut drain::DrainState,
    drain_bound: Duration,
    event_publisher: RelayEventPublisher,
) -> Option<drain::DrainOnset> {
    // Defense-in-depth: verify signature even though the relay already checked.
    if let Err(e) = buzz_core::verify_event(&event) {
        tracing::warn!(error = %e, "observer control frame failed signature verification");
        return None;
    }

    // Defense-in-depth: verify the sender is the resolved owner.
    if event.pubkey.to_hex() != owner_pubkey_hex {
        tracing::warn!(
            sender = %event.pubkey,
            expected = %owner_pubkey_hex,
            "observer control frame from non-owner — dropping"
        );
        return None;
    }

    // Freshness: reject stale/replayed frames outside ±5 minute window.
    //
    // For drain this is the outer half of the replay answer — it disposes of a
    // captured frame being re-sent tomorrow. The inner half is that drain is
    // idempotent, so a replay *inside* the window changes nothing either; see
    // the `crate::drain` module docs, which spell out why that means no nonce
    // or seen-set is needed here.
    let now = chrono::Utc::now().timestamp();
    let event_ts = event.created_at.as_secs() as i64;
    if (event_ts - now).unsigned_abs() > OBSERVER_CONTROL_FRESHNESS_SECS as u64 {
        tracing::warn!(
            event_ts,
            now,
            "observer control frame outside freshness window — dropping"
        );
        return None;
    }

    let payload = match decrypt_observer_payload::<serde_json::Value>(keys, &event) {
        Ok(payload) => payload,
        Err(error) => {
            tracing::warn!("failed to decrypt observer control frame: {error}");
            return None;
        }
    };

    let command_type = payload.get("type").and_then(|value| value.as_str());
    match command_type {
        Some("cancel_turn") => {
            handle_cancel_turn_control(&payload, pool, observer);
            None
        }
        Some("cancel_all") => {
            handle_cancel_all_control(pool, queue, observer);
            None
        }
        Some("switch_model") => {
            handle_switch_model_control(&payload, pool, observer);
            None
        }
        Some(drain::CONTROL_TYPE_DRAIN) => Some(handle_drain_control(
            &payload,
            observer,
            drain,
            drain_bound,
            tokio::time::Instant::now(),
        )),
        // Unchanged, and load-bearing for the drain rollout: a binary that
        // predates a payload type ignores it instead of failing, so a fleet can
        // be drained mid-rollout with no coordination. Old processes decline
        // and keep serving; new ones honour it. This arm is the reason drain is
        // a new `type` rather than a new kind, tag or subscription — each of
        // which an old binary would have had to be taught to ignore.
        Some("publish_project_owner_announcements") => {
            handle_publish_project_owner_announcements_control(
                &payload,
                keys,
                observer,
                event_publisher,
            );
            None
        }
        _ => {
            tracing::debug!(payload = %payload, "ignoring unknown observer control frame");
            None
        }
    }
}

fn emit_cancelled_queued_project_announcements(
    discarded: &[FlushBatch],
    observer: Option<&observer::ObserverHandle>,
) {
    let Some(observer) = observer else { return };
    for batch in discarded {
        let Some(origin) = batch.project_origin() else {
            continue;
        };
        let Some(first) = batch
            .events
            .first()
            .or_else(|| batch.cancelled_events.first())
        else {
            continue;
        };
        observer.emit(
            "turn_error",
            None,
            &observer::ObserverContext {
                channel_id: None,
                project: Some(observer::ProjectRouteRef {
                    coordinate: origin.coordinate().to_string(),
                    root: origin.root().to_string(),
                }),
                session_id: None,
                turn_id: Some(queued_turn_id(&first.event.id.to_hex())),
                started_at: None,
            },
            serde_json::json!({
                "error": "cancelled_by_owner",
                "detail": "the owner cancelled this queued work before it started",
            }),
        );
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CancelAllOutcome {
    active_turns: usize,
    signalled_turns: usize,
    queued_batches: usize,
    queued_events: usize,
}

impl CancelAllOutcome {
    fn status(self) -> &'static str {
        if self.active_turns == 0 && self.queued_batches == 0 {
            "no_work"
        } else {
            "accepted"
        }
    }
}

/// Establish a no-requeue cutoff for every active channel turn, request
/// cancellation where possible, and discard all work already buffered.
/// Admission remains open, so genuinely new work is still accepted.
fn handle_cancel_all_control(
    pool: &mut AgentPool,
    queue: &mut EventQueue,
    observer: Option<&observer::ObserverHandle>,
) -> CancelAllOutcome {
    let (active_turns, signalled_turns) = apply_cancel_all_cutoff(pool);
    let discarded = queue.discard_all_pending_batches();
    let queued_events = discarded
        .iter()
        .map(|batch| batch.events.len() + batch.cancelled_events.len())
        .sum();
    emit_cancelled_queued_project_announcements(&discarded, observer);
    let outcome = CancelAllOutcome {
        active_turns,
        signalled_turns,
        queued_batches: discarded.len(),
        queued_events,
    };

    tracing::warn!(
        active_turns = outcome.active_turns,
        signalled_turns = outcome.signalled_turns,
        queued_batches = outcome.queued_batches,
        queued_events = outcome.queued_events,
        "owner accepted cancel-all cutoff"
    );

    if let Some(observer) = observer {
        observer.emit(
            "control_result",
            None,
            &observer::ObserverContext {
                project: None,
                channel_id: None,
                session_id: None,
                turn_id: None,
                started_at: None,
            },
            serde_json::json!({
                "type": "cancel_all",
                "status": outcome.status(),
                "activeTurns": outcome.active_turns,
                "signalledTurns": outcome.signalled_turns,
                "queuedBatches": outcome.queued_batches,
                "queuedEvents": outcome.queued_events,
            }),
        );
    }

    outcome
}

/// Handle a `drain` control frame: close admission, and say so loudly.
///
/// Everything that *waits* — for the in-flight turn, for the queue, for the
/// bound — happens in the run loop, because the run loop is the thing that has
/// to keep running while it happens. This function only flips the state and
/// reports; a handler that blocked here would stop servicing the very prompt
/// results it was waiting for.
///
/// Logged at `warn`, not `info`. A drain is an operator instruction that
/// silently changes what the process will accept for the rest of its life, and
/// the one question an operator asks afterwards — "did it get the frame?" — has
/// to be answerable from a default log level.
fn handle_drain_control(
    payload: &serde_json::Value,
    observer: Option<&observer::ObserverHandle>,
    drain: &mut drain::DrainState,
    bound: Duration,
    now: tokio::time::Instant,
) -> drain::DrainOnset {
    let reason = payload
        .get("reason")
        .and_then(|value| value.as_str())
        .map(drain::trim_reason)
        .unwrap_or_default();
    let onset = drain.begin(now, bound);
    match onset {
        drain::DrainOnset::Started => tracing::warn!(
            reason = %reason,
            bound_secs = bound.as_secs(),
            "drain requested by owner — refusing new work, finishing what is in hand, then exiting 0"
        ),
        // Not a warning and not silence. A deployer that retried wants to know
        // the retry landed; an operator reading logs wants to know the second
        // frame did nothing.
        drain::DrainOnset::AlreadyDraining => tracing::info!(
            reason = %reason,
            "drain frame received while already draining — no-op (idempotent)"
        ),
    }
    if let Some(observer) = observer {
        observer.emit(
            "control_result",
            None,
            &observer::ObserverContext::default(),
            serde_json::json!({
                "type": drain::CONTROL_TYPE_DRAIN,
                "status": onset.status(),
                "reason": reason,
            }),
        );
    }
    onset
}

/// Admit one channel event into the queue — unless the runtime is draining.
///
/// **Every channel event that can become a turn goes through here.** There are
/// two such sites in the run loop (an ordinary rule-matched message, and a
/// NIP-PC peer call routed over a channel) and they used to call
/// [`EventQueue::push`] directly. Wrapping both is what makes "take nothing
/// new" a single decision that a test can drive, rather than two `if` statements
/// buried in a `select!` arm no test can enter.
///
/// Returning `push`'s own `bool` is deliberate: a refusal is reported exactly
/// like the queue's other refusals (`DedupMode::Drop`, a terminal-auth
/// disposition, a depth cap), so both call sites' existing `if accepted`
/// handling covers drain without knowing about it. That matters most for the
/// 👀 reaction — it is added only on acceptance, so a drained runtime makes no
/// visible promise about an event it declined.
///
/// **What refusal means for the event.** Nothing is consumed. The event is
/// relay history and stays there; this process simply never held it. The
/// successor resubscribes with `since` derived from its own startup watermark,
/// so a message posted before the swap is redelivered while a message posted
/// during the drain window may not be — which is the same exposure the existing
/// `SIGTERM` grace already has, and strictly smaller than the turn that grace
/// aborts. Drain does not widen that gap; it exists to close the larger one.
///
/// **What is deliberately still admitted while draining.** Owner control
/// commands (`!shutdown`, `!cancel`, `!rotate`) are handled before this gate and
/// stay live, because none of them is *work* — they are the levers an operator
/// needs precisely when a drain is taking longer than expected. Membership
/// notifications also stay live: a removal drains that channel's queue, which
/// keeps the drain honest rather than running a turn for a channel this agent
/// was just removed from.
///
/// **A refused NIP-PC call has already touched the call ledger**, because
/// admission is decided before the queue is offered the event. That leaves an
/// outstanding-call record nobody will answer — which costs nothing, because the
/// ledger is process-local and this process is leaving. The caller sees the same
/// thing it would see if this agent had been killed: no result, and its own
/// timeout. Moving the gate ahead of the ledger would have meant a second
/// admission decision living outside [`decide_channel_peer_event`], which is
/// worse than a record that dies with the process.
fn admit_channel_event(
    drain: &drain::DrainState,
    queue: &mut EventQueue,
    event: QueuedEvent,
) -> bool {
    if !drain.admits_new_work() {
        tracing::info!(
            channel_id = %event.channel_id,
            event_id = %event.event.id.to_hex(),
            kind = event.event.kind.as_u16(),
            "draining — refusing new channel event (it stays on the relay for the next process)"
        );
        return false;
    }
    queue.push(event)
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProjectOwnerAnnouncementControl {
    request_id: String,
    announcements: Vec<ProjectOwnerAnnouncementTemplate>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProjectOwnerAnnouncementTemplate {
    kind: u16,
    content: String,
    created_at: Option<u64>,
    tags: Vec<Vec<String>>,
}

fn handle_publish_project_owner_announcements_control(
    payload: &serde_json::Value,
    keys: &nostr::Keys,
    observer: Option<&observer::ObserverHandle>,
    publisher: RelayEventPublisher,
) {
    let Ok(control) = serde_json::from_value::<ProjectOwnerAnnouncementControl>(payload.clone())
    else {
        tracing::warn!("project announcement control frame has an invalid payload");
        return;
    };
    if Uuid::parse_str(&control.request_id).is_err()
        || control.announcements.is_empty()
        || control.announcements.len() > 2
    {
        tracing::warn!("project announcement control frame has invalid request metadata");
        return;
    }

    let keys = keys.clone();
    let observer = observer.cloned();
    tokio::spawn(async move {
        let events = match build_project_owner_announcement_events(control.announcements, &keys) {
            Ok(events) => events,
            Err(error) => {
                emit_project_owner_control_result(
                    observer.as_ref(),
                    &control.request_id,
                    "error",
                    &[],
                    Some(error.to_string()),
                );
                return;
            }
        };
        let mut published_events = Vec::with_capacity(events.len());
        for event in events {
            if let Err(error) = publisher.publish_event(event.clone()).await {
                emit_project_owner_control_result(
                    observer.as_ref(),
                    &control.request_id,
                    "error",
                    &published_events,
                    Some(format!("publish project announcement: {error}")),
                );
                return;
            }
            published_events.push(event);
        }
        emit_project_owner_control_result(
            observer.as_ref(),
            &control.request_id,
            "ok",
            &published_events,
            None,
        );
    });
}

fn build_project_owner_announcement_events(
    announcements: Vec<ProjectOwnerAnnouncementTemplate>,
    keys: &nostr::Keys,
) -> Result<Vec<nostr::Event>> {
    let now = nostr::Timestamp::now().as_secs();
    announcements
        .into_iter()
        .map(|template| {
            if !matches!(template.kind, 30_617 | 30_621) {
                anyhow::bail!("unsupported project announcement kind");
            }
            if !template.tags.iter().any(|tag| {
                tag.first().is_some_and(|value| value == "d")
                    && tag.get(1).is_some_and(|value| !value.trim().is_empty())
            }) {
                anyhow::bail!("project announcement is missing its address");
            }
            let tags = template
                .tags
                .into_iter()
                .map(|tag| {
                    nostr::Tag::parse(tag)
                        .map_err(|error| anyhow::anyhow!("invalid project tag: {error}"))
                })
                .collect::<Result<Vec<_>>>()?;
            let created_at = template.created_at.unwrap_or(now);
            if created_at > now.saturating_add(300) {
                anyhow::bail!("project announcement timestamp is too far in the future");
            }
            nostr::EventBuilder::new(nostr::Kind::Custom(template.kind), template.content)
                .tags(tags)
                .custom_created_at(nostr::Timestamp::from(created_at))
                .sign_with_keys(keys)
                .map_err(|error| anyhow::anyhow!("sign project announcement: {error}"))
        })
        .collect()
}

fn emit_project_owner_control_result(
    observer: Option<&observer::ObserverHandle>,
    request_id: &str,
    status: &str,
    events: &[nostr::Event],
    error: Option<String>,
) {
    let Some(observer) = observer else {
        return;
    };
    observer.emit(
        "control_result",
        None,
        &observer::ObserverContext {
            channel_id: None,
            session_id: None,
            turn_id: None,
            started_at: None,
            project: None,
        },
        serde_json::json!({
            "type": "publish_project_owner_announcements",
            "requestId": request_id,
            "status": status,
            "events": events,
            "error": error,
        }),
    );
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
                project: None,
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
    // Opaque per-pick correlator, echoed on every result frame so the Desktop
    // can ignore a replayed result for an earlier pick. Optional: absent on
    // older Desktop clients, in which case the frames simply carry no id.
    let request_id = payload
        .get("requestId")
        .and_then(|value| value.as_str())
        .map(str::to_string);

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
            ControlSignal::SwitchModel {
                model_id: model_id.to_string(),
                request_id: request_id.clone(),
            },
        ) {
            "sent"
        } else {
            "turn_ending"
        }
    } else {
        // Idle path: validate against the cached catalog before invalidating.
        match pool.switch_idle_agent_model(channel_id, model_id, request_id.clone()) {
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
                project: None,
                channel_id: Some(channel_id.to_string()),
                session_id: None,
                turn_id: None,
                started_at: None,
            },
            serde_json::json!({
                "type": "switch_model",
                "status": status,
                "modelId": model_id,
                // Echo the correlator on the immediate ack so a `sent` /
                // `turn_ending` / idle-path terminal frame matches the pick.
                "requestId": request_id,
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

fn inactivity_expired(
    last_activity: tokio::time::Instant,
    now: tokio::time::Instant,
    bound: Duration,
    turn_in_flight: bool,
) -> bool {
    !bound.is_zero() && !turn_in_flight && now.duration_since(last_activity) >= bound
}

/// Whether a woken lazy pool may be torn back down to the empty-slot state.
///
/// True only when the pool is ready, the idle bound has elapsed with no
/// dispatched turn or heartbeat in flight and no in-flight prompt tasks, no
/// work is queued, and no wake/respawn task is running. The queue and task
/// gates make teardown race-safe with enqueue/wake: an event that landed in
/// the queue (or a wake/respawn already in flight) blocks this decision, so a
/// queued batch is never stranded — the caller's next loop iteration will
/// dispatch or wake it instead.
#[allow(clippy::too_many_arguments)]
fn idle_pool_sleep_due(
    pool_ready: bool,
    last_activity: tokio::time::Instant,
    now: tokio::time::Instant,
    bound: Duration,
    turn_in_flight: bool,
    prompt_tasks_in_flight: bool,
    work_queued: bool,
    wake_or_respawn_in_flight: bool,
) -> bool {
    pool_ready
        && !work_queued
        && !prompt_tasks_in_flight
        && !wake_or_respawn_in_flight
        && inactivity_expired(last_activity, now, bound, turn_in_flight)
}

#[cfg(test)]
mod inactivity_tests {
    use super::*;

    #[test]
    fn zero_disables_expiry_and_in_flight_turns_defer_it() {
        let started = tokio::time::Instant::now();
        let after_bound = started + Duration::from_secs(61);

        assert!(!inactivity_expired(
            started,
            after_bound,
            Duration::ZERO,
            false
        ));
        assert!(!inactivity_expired(
            started,
            after_bound,
            Duration::from_secs(60),
            true
        ));
        assert!(inactivity_expired(
            started,
            after_bound,
            Duration::from_secs(60),
            false
        ));
    }

    #[test]
    fn dispatched_activity_restarts_the_inactivity_bound() {
        let started = tokio::time::Instant::now();
        let dispatched = started + Duration::from_secs(50);
        let checked = started + Duration::from_secs(61);

        assert!(!inactivity_expired(
            dispatched,
            checked,
            Duration::from_secs(60),
            false
        ));
    }
}

#[cfg(test)]
mod idle_pool_sleep_tests {
    use super::*;

    // The all-clear baseline: pool ready, bound elapsed, nothing busy or
    // queued. Every negative case below flips exactly one gate off this.
    fn ready_after_bound() -> (tokio::time::Instant, tokio::time::Instant, Duration) {
        let started = tokio::time::Instant::now();
        (
            started,
            started + Duration::from_secs(61),
            Duration::from_secs(60),
        )
    }

    #[test]
    fn sleeps_when_ready_idle_and_quiet() {
        let (last, now, bound) = ready_after_bound();
        assert!(idle_pool_sleep_due(
            true, last, now, bound, false, false, false, false
        ));
    }

    #[test]
    fn zero_bound_never_sleeps() {
        let (last, now, _) = ready_after_bound();
        assert!(!idle_pool_sleep_due(
            true,
            last,
            now,
            Duration::ZERO,
            false,
            false,
            false,
            false
        ));
    }

    #[test]
    fn not_ready_never_sleeps() {
        // A still-sleeping (or waking) pool must not "re-sleep".
        let (last, now, bound) = ready_after_bound();
        assert!(!idle_pool_sleep_due(
            false, last, now, bound, false, false, false, false
        ));
    }

    #[test]
    fn active_turn_defers_sleep() {
        let (last, now, bound) = ready_after_bound();
        assert!(!idle_pool_sleep_due(
            true, last, now, bound, true, false, false, false
        ));
    }

    #[test]
    fn in_flight_prompt_task_defers_sleep() {
        let (last, now, bound) = ready_after_bound();
        assert!(!idle_pool_sleep_due(
            true, last, now, bound, false, true, false, false
        ));
    }

    #[test]
    fn queued_work_at_boundary_defers_sleep() {
        // Enqueue-at-teardown protection: a batch sitting in the queue blocks
        // teardown so it is never stranded — the loop dispatches it instead.
        let (last, now, bound) = ready_after_bound();
        assert!(!idle_pool_sleep_due(
            true, last, now, bound, false, false, true, false
        ));
    }

    #[test]
    fn wake_or_respawn_in_flight_defers_sleep() {
        let (last, now, bound) = ready_after_bound();
        assert!(!idle_pool_sleep_due(
            true, last, now, bound, false, false, false, true
        ));
    }

    #[test]
    fn recent_activity_defers_sleep() {
        // Activity 50s ago under a 60s bound: not yet idle.
        let started = tokio::time::Instant::now();
        let recent = started + Duration::from_secs(50);
        let now = started + Duration::from_secs(59);
        assert!(!idle_pool_sleep_due(
            true,
            recent,
            now,
            Duration::from_secs(60),
            false,
            false,
            false,
            false
        ));
    }

    fn slot(respawn_in_flight: bool) -> SlotCircuit {
        SlotCircuit {
            crash_times: Vec::new(),
            open_until: None,
            respawn_in_flight,
        }
    }

    // The call-site signal for the `wake_or_respawn_in_flight` gate is
    // `any_respawn_in_flight(&crash_history)`, NOT `!respawn_tasks.is_empty()`.
    // Regression for the PR #5682 review blocker: completed respawn tasks are
    // never joined from the `respawn_tasks` JoinSet (their payloads arrive
    // out-of-band via `respawn_rx`), so `!is_empty()` stays true forever after
    // the first refill/crash recovery and the pool could never re-sleep. The
    // authoritative signal clears per-slot when the payload is received.
    #[test]
    fn respawn_in_flight_signal_gates_then_clears_for_sleep() {
        let (last, now, bound) = ready_after_bound();

        // A respawn in flight for any slot defers sleep.
        let busy = [slot(false), slot(true), slot(false)];
        assert!(any_respawn_in_flight(&busy));
        assert!(!idle_pool_sleep_due(
            true,
            last,
            now,
            bound,
            false,
            false,
            false,
            any_respawn_in_flight(&busy),
        ));

        // Once the respawn completes (payload received → flag cleared), the
        // signal goes false and the otherwise-quiet pool becomes sleep-eligible
        // — even though a naive `!JoinSet.is_empty()` would still be stuck true.
        let quiet = [slot(false), slot(false), slot(false)];
        assert!(!any_respawn_in_flight(&quiet));
        assert!(idle_pool_sleep_due(
            true,
            last,
            now,
            bound,
            false,
            false,
            false,
            any_respawn_in_flight(&quiet),
        ));
    }

    // The reaper (`respawn_tasks.join_next().now_or_never()` loop) must drain
    // completed handles so the JoinSet does not grow without bound and so
    // `!respawn_tasks.is_empty()` cannot become a permanent busy bit if anyone
    // ever reintroduces it as the gate signal.
    #[tokio::test]
    async fn completed_respawn_tasks_are_reaped_from_the_joinset() {
        let mut respawn_tasks: tokio::task::JoinSet<()> = tokio::task::JoinSet::new();
        respawn_tasks.spawn(async {});
        respawn_tasks.spawn(async {});
        // Let both tasks run to completion.
        tokio::task::yield_now().await;
        tokio::time::sleep(Duration::from_millis(10)).await;

        // The reaper drains finished handles non-blockingly.
        while respawn_tasks.join_next().now_or_never().flatten().is_some() {}

        assert!(
            respawn_tasks.is_empty(),
            "completed respawn tasks must be reaped so the set does not wedge \
             the idle-sleep gate or grow unbounded"
        );
    }
}

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

    if is_subcommand("provider-probe") {
        let filtered: Vec<String> = std::env::args()
            .enumerate()
            .filter(|(i, _)| *i != 1)
            .map(|(_, a)| a)
            .collect();
        let args = ProviderProbeArgs::parse_from(&filtered);
        return run_provider_probe(args).await;
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

    // ── Durable terminal-auth dispositions ───────────────────────────────────
    //
    // Loaded here, immediately after configuration and before any agent is
    // spawned or any relay socket is opened, so the filter is in place before
    // the first event can possibly arrive. Anything other than "missing" that
    // fails to validate stops startup: silently resetting the store would
    // re-arm every request a previous run terminally disposed of.
    let terminal_auth_store = terminal_auth_store::TerminalAuthStore::load(
        &config.state_dir,
        &config.keys.public_key().to_hex(),
    )
    .map_err(|e| anyhow::anyhow!("terminal-auth store error: {e}"))?;

    let observer = observer_bus_for(&config);
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
        initialize_agent_pool(&PoolStartup::from_config(&config, observer.clone()), None)
            .await
            .map_err(|e| anyhow::anyhow!("{}", e.summary()))?
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

    // NIP-PA project activity. Behind the project-routing flag and nothing
    // else: it is what makes an issue show that an agent is working on it, so
    // tying it to `--relay-observer` would mean the indicator silently vanishes
    // wherever owner telemetry happens to be off.
    //
    // Subscribes here rather than inside the task so no frame emitted between
    // this line and the task's first poll is lost.
    let mut project_activity_task = None;
    if config.project_routing_enabled {
        if let Some(observer) = observer.clone() {
            let rx = observer.subscribe();
            let publisher = relay.event_publisher();
            let keys = config.keys.clone();
            let agent_hex = pubkey_hex.clone();
            project_activity_task = Some(tokio::spawn(run_project_activity_publisher(
                rx, publisher, keys, agent_hex,
            )));
            tracing::info!("project activity publisher enabled");
        } else {
            tracing::warn!(
                "project routing is enabled but no observer bus exists; \
                 project activity will not be published"
            );
        }
    }

    let mut relay_observer_control_rx = None;
    let mut relay_observer_publisher_task = None;
    let mut relay_observer_publisher = None;
    if encrypted_telemetry_enabled(&config) {
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
    queue.attach_terminal_auth_store(terminal_auth_store);

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
    let cwd = current_working_directory()?;
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
        cwd,
        rest_client: relay.rest_client(),
        channel_info: pool::ChannelInfoResolver::new(channel_info_map, relay.rest_client()),
        context_message_limit: config.context_message_limit,
        // The `[Peer Agents]` roster a project prompt renders. Built from the
        // same option the project arm's invocation authority is built from, so
        // the agents an agent is *told about* are the agents it may actually
        // exchange work with — a roster naming somebody the gate would refuse
        // is an invitation to a turn that goes nowhere.
        peer_agents: pool::prompt_peer_roster(
            config.peer_agents.iter(),
            &config.keys.public_key().to_hex(),
        ),
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

    // Independent of pool readiness: a never-mentioned lazy agent must still
    // self-terminate. The watch interval is capped so small configured bounds
    // remain reasonably precise without waking long-lived agents frequently.
    let inactivity_bound = Duration::from_secs(config.exit_after_inactivity_secs);
    let mut last_activity = tokio::time::Instant::now();
    let mut inactivity_reaper = if inactivity_bound.is_zero() {
        None
    } else {
        let interval = inactivity_bound.min(Duration::from_secs(30));
        Some(tokio::time::interval_at(
            tokio::time::Instant::now() + interval,
            interval,
        ))
    };

    // ── Drain ─────────────────────────────────────────────────────────────
    //
    // Owner-signed "finish what you have, take nothing new, then exit 0" — the
    // instruction a deployer sends before swapping the binary. Everything about
    // the frame itself lives in `crate::drain`, including the sender contract.
    //
    // Held here rather than inside the control handler because the *waiting* is
    // the run loop's job: admission gates read it on the way in, and the
    // top-of-loop exit check reads it on the way out. The bound is the queue's
    // own in-flight backstop, so a turn running to its configured cap is never
    // cut short by the drain that is waiting for it.
    let mut drain = drain::DrainState::open();
    let drain_bound = drain::drain_bound(config.max_turn_duration_secs);
    // Idle pool re-sleep: tear a woken lazy pool back down to the empty-slot
    // state after `idle_pool_sleep_bound` of quiet, releasing worker
    // subprocesses. The next accepted event re-wakes it through the same lazy
    // path. Only meaningful under `lazy_pool`; the tick arm additionally gates
    // on `pool_ready`, so a still-sleeping pool never re-sleeps. Reuses the
    // `last_activity` clock the dispatch path already maintains.
    let idle_pool_sleep_bound = if config.lazy_pool {
        Duration::from_secs(config.idle_pool_sleep_secs)
    } else {
        Duration::ZERO
    };
    let mut idle_pool_sleep_reaper = if idle_pool_sleep_bound.is_zero() {
        None
    } else {
        let interval = idle_pool_sleep_bound.min(Duration::from_secs(30));
        Some(tokio::time::interval_at(
            tokio::time::Instant::now() + interval,
            interval,
        ))
    };

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
    let (wake_tx, mut wake_rx) = mpsc::channel::<(u32, Result<AgentPool, PoolStartError>)>(1);
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

    let mut project_seen_ids = ProjectSeenIds::new();

    // No project-subscription state is held here. What is installed, which
    // generation it carries and which predecessor it retires all live in the
    // background registry, because that is the only component that knows.
    // A copy on this side could only ever be a guess about the far side of a
    // channel, and the guess is what went wrong.

    // The agent's own identity, in both spellings, from one source — so the
    // `p`-tag check and visible-mention detection cannot end up pointed at
    // different keys.
    // The display name comes from the same env var Desktop forwards to dev-mcp,
    // so the name the agent answers to and the name it is called by are one
    // string. Unset is the safe state: without it the agent simply cannot tell
    // "names another agent" from "names nobody", and every comment on an active
    // root wakes it exactly as before.
    let project_agent_identity = project::AgentIdentity::new(&config.keys.public_key())
        .map_err(|e| anyhow::anyhow!("agent identity: {e}"))?
        .with_display_name(&std::env::var("BUZZ_ACP_DISPLAY_NAME").unwrap_or_default());

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
        Wake(u32, Result<AgentPool, PoolStartError>),
    }

    loop {
        // ── Drain: the exit the deployer asked for ────────────────────────
        //
        // At the top of the iteration, before anything can add work, and by
        // the same reasoning the maintenance block above it sits here: a check
        // living in a `select!` arm can be starved by the biased ordering,
        // and this one must fire the moment the last turn returns.
        //
        // There is no second wait loop. "Finish what you have" is implemented
        // by *not leaving* — the run loop keeps servicing prompt results until
        // the queue says it is holding nothing — after which the existing
        // teardown below runs exactly as it does for `!shutdown`, the
        // inactivity bound and SIGTERM. Duplicating the teardown's own
        // "waiting for in-flight prompts" grace here would have meant two
        // places that both believe they are the last word on when a turn is
        // allowed to end.
        if let Some(exit) = drain.should_exit(
            // The heartbeat turn is tracked separately because it is not
            // channel-keyed and so leaves no trace in the queue — the same
            // reason `inactivity_expired` takes it as its own argument.
            queue.has_undrained_work() || heartbeat_in_flight,
            tokio::time::Instant::now(),
        ) {
            match exit {
                drain::DrainExit::Complete => {
                    tracing::warn!("drain complete — nothing left in hand, exiting 0");
                }
                drain::DrainExit::BoundExpired => {
                    tracing::error!(
                        bound_secs = drain_bound.as_secs(),
                        "drain bound expired with work still outstanding — exiting 0 and \
                         leaving the remainder to the next process"
                    );
                }
            }
            // Before the teardown, while the activity publisher is still
            // running: anything still queued has been announced on its root as
            // `state=queued` and is about to become nobody's work.
            clear_queued_project_announcements(&mut queue, observer.as_ref());
            emit_runtime_lifecycle(
                observer.as_ref(),
                &runtime_start_nonce,
                &pubkey_hex,
                &config.relay_url,
                "drained",
                None,
            );
            break;
        }
        // Copied out before the `select!` rather than borrowed into it: an arm
        // holding `&drain` for the lifetime of the poll collides with the
        // control arm's `&mut drain`.
        let drain_deadline = drain.deadline();

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
                    let result = initialize_agent_pool(&startup, Some(wake_shutdown)).await;
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
                for (channel_id, thread_tags) in
                    dispatch_pending(&mut pool, &mut queue, &ctx, &mut last_activity)
                {
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
                        desired_model_request_id: None,
                        desired_model_pending_ack: false,
                        startup_effort: config.effort_level.clone(),
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
        // Reap completed respawn handles from the JoinSet. Payloads are
        // delivered out-of-band through `respawn_rx` (drained above), so the
        // JoinSet is never joined by the normal flow — Tokio retains finished
        // tasks until `join_next`, so without this the set grows on every
        // refill/crash recovery and `!respawn_tasks.is_empty()` would stay true
        // forever. Non-blocking (`now_or_never`), same pattern as
        // `drain_ready_join_results` for `pool.join_set`. The authoritative
        // in-flight signal is `any_respawn_in_flight(&crash_history)` (each
        // slot's `respawn_in_flight` is cleared when its payload is received),
        // not JoinSet occupancy.
        while respawn_tasks.join_next().now_or_never().flatten().is_some() {}
        // Flush requeued events that were waiting for a live agent. Without
        // this, batches requeued during crash recovery sit idle until the
        // next relay event arrives — which can be minutes on quiet channels.
        if respawn_collected {
            for (channel_id, thread_tags) in
                dispatch_pending(&mut pool, &mut queue, &ctx, &mut last_activity)
            {
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
                // Wake the loop when the drain bound expires, so the
                // top-of-iteration check gets to notice it. Nothing happens
                // here: firing this arm returns `None`, the loop comes round,
                // and `should_exit` reports `BoundExpired` — one extra pass,
                // and one place that decides to leave rather than two.
                //
                // Inert until a drain begins (`None` → `pending()` forever), and
                // it cannot busy-spin afterwards because the very next
                // iteration breaks out of the loop.
                _ = async {
                    match drain_deadline {
                        Some(deadline) => tokio::time::sleep_until(deadline).await,
                        None => std::future::pending().await,
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
                                let onset = handle_relay_observer_control_event(
                                    &config.keys,
                                    event,
                                    &mut pool,
                                    &mut queue,
                                    observer.as_ref(),
                                    owner_hex,
                                    &mut drain,
                                    drain_bound,
                                    relay.event_publisher(),
                                );
                                if onset == Some(drain::DrainOnset::Started) {
                                    // Telemetry first: NIP-AO consumers project
                                    // this lifecycle already, so `draining`
                                    // reaches every surface that can currently
                                    // show `ready` without any of them being
                                    // taught a new event shape.
                                    emit_runtime_lifecycle(
                                        observer.as_ref(),
                                        &runtime_start_nonce,
                                        &pubkey_hex,
                                        &config.relay_url,
                                        "draining",
                                        None,
                                    );
                                    // Then start the backlog moving. Without
                                    // this, a runtime whose pool is idle and
                                    // whose queue is full would sit on that
                                    // queue until the next inbound event or the
                                    // 30-second maintenance tick — and inbound
                                    // events are exactly what a drain has just
                                    // stopped accepting, so on a project-only
                                    // runtime "the next event" may never come.
                                    if pool_ready {
                                        for (channel_id, thread_tags) in
                                            dispatch_pending(&mut pool, &mut queue, &ctx, &mut last_activity)
                                        {
                                            typing_channels.insert(channel_id, thread_tags);
                                        }
                                    }
                                }
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
                            // Subscription upkeep, dedup and the flush all
                            // travel with dispatch rather than being open-coded
                            // here. Inline, none of it was reachable from a
                            // test — and the two defects that lived in this arm
                            // were exactly the parts no test could enter: the
                            // REQ-widening decision, and then the missing flush
                            // that left a project-only runtime queueing turns
                            // nobody ran.
                            // Drain short-circuit. The authoritative refusal is
                            // inside `dispatch_and_flush_project_event`, which
                            // is where the tests drive it and where refusing is
                            // provably ahead of the announcement and the REQ.
                            // This is the same predicate, hoisted, purely so a
                            // drained runtime does not spend two REST
                            // round-trips per inbound comment resolving context
                            // for an event it has already decided to decline —
                            // latency the drain would otherwise pay on its way
                            // to the exit check.
                            if !drain.admits_new_work() {
                                tracing::info!(
                                    "draining — refusing new project event (it stays on the relay for the next process)"
                                );
                                continue;
                            }
                            // Resolved before the gate, because the NIP-OA
                            // lookup is async and the gate is not.
                            let project_sibling = attest_project_sibling(
                                &project_event,
                                owner_cache.get(),
                                &owner_cache,
                                &ctx.rest_client,
                            )
                            .await;
                            let resolved_candidate = match resolve_comment_first_candidate(
                                &project_event,
                                &project_enrolments,
                                &discovered_repositories,
                                &pubkey_hex,
                                &ctx.rest_client,
                            )
                            .await
                            {
                                Ok(candidate) => candidate,
                                Err(error) => {
                                    tracing::warn!(%error, "comment-first root resolution failed; event remains retryable");
                                    continue;
                                }
                            };
                            dispatch_and_flush_project_event(
                                &mut ProjectArm {
                                    identity: &project_agent_identity,
                                    owner: owner_cache.get(),
                                    approved_humans: &project_approved_humans,
                                    approved_external_agents:
                                        &project_approved_external_agents,
                                    discovered: &mut discovered_repositories,
                                    enrolments: &mut project_enrolments,
                                    ledger: &mut call_ledger,
                                    seen: &mut project_seen_ids,
                                    agent_pubkey_hex: &pubkey_hex,
                                    startup_watermark,
                                    observer: observer.as_ref(),
                                    drain: &drain,
                                },
                                project_sibling,
                                resolved_candidate,
                                &relay,
                                &project_event,
                                &mut pool,
                                &mut queue,
                                &ctx,
                                &mut last_activity,
                                &mut typing_channels,
                                pool_ready,
                            )
                            .await;
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
                                        let accepted = admit_channel_event(
                                            &drain,
                                            &mut queue,
                                            QueuedEvent {
                                                channel_id,
                                                event: buzz_event.event,
                                                received_at: std::time::Instant::now(),
                                                prompt_tag: prompt_tag.into(),
                                                // A channel call keys on the
                                                // channel, exactly as an
                                                // ordinary message does.
                                                project: None,
                                            },
                                        );
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
                            let accepted = admit_channel_event(
                                &drain,
                                &mut queue,
                                QueuedEvent {
                                    channel_id: buzz_event.channel_id,
                                    event: buzz_event.event,
                                    received_at: std::time::Instant::now(),
                                    prompt_tag,
                                    // Channel events are never project-routed; the
                                    // project branch has its own queue insertion.
                                    project: None,
                                },
                            );
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
                                    dispatch_pending(&mut pool, &mut queue, &ctx, &mut last_activity)
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
                    match inactivity_reaper.as_mut() {
                        Some(timer) => timer.tick().await,
                        None => std::future::pending().await,
                    }
                } => {
                    let _ = result_rx;
                    if inactivity_expired(
                        last_activity,
                        tokio::time::Instant::now(),
                        inactivity_bound,
                        queue.has_in_flight() || heartbeat_in_flight,
                    ) {
                        tracing::info!(
                            inactivity_seconds = config.exit_after_inactivity_secs,
                            "inactivity bound reached — exiting gracefully"
                        );
                        let _ = shutdown_tx.send(());
                    }
                    None
                }
                _ = async {
                    match idle_pool_sleep_reaper.as_mut() {
                        Some(timer) => timer.tick().await,
                        None => std::future::pending().await,
                    }
                } => {
                    let _ = result_rx; // end split borrow before touching pool
                    // A wake in flight (pool not yet ready) is covered by the
                    // pool_ready gate; respawn tasks and in-flight prompt tasks
                    // are the remaining "busy" signals. Never sleep mid-work:
                    // `has_undispatched_work()` (not `has_flushable_work()`)
                    // keeps `work_queued` true for a retry-throttled batch too,
                    // so a failed turn awaiting backoff is never stranded — the
                    // next iteration dispatches or re-wakes it.
                    if idle_pool_sleep_due(
                        pool_ready,
                        last_activity,
                        tokio::time::Instant::now(),
                        idle_pool_sleep_bound,
                        queue.has_in_flight() || heartbeat_in_flight,
                        !pool.join_set.is_empty(),
                        queue.has_undispatched_work(),
                        !wake_tasks.is_empty()
                            || any_respawn_in_flight(&crash_history),
                    ) {
                        tracing::info!(
                            idle_pool_sleep_seconds = config.idle_pool_sleep_secs,
                            "idle pool sleep bound reached — tearing pool back to lazy state"
                        );
                        shutdown_agent_pool(&mut pool).await;
                        // Return to the exact pre-wake lazy state: empty slots,
                        // Listening lifecycle. The top-of-loop wake path re-wakes
                        // on the next accepted event. No second lifecycle.
                        pool = AgentPool::from_slots(
                            (0..config.agents).map(|_| None).collect(),
                        );
                        pool_ready = false;
                        pool_lifecycle = PoolLifecycle::listening();
                        last_activity = tokio::time::Instant::now();
                        emit_runtime_lifecycle(
                            observer.as_ref(),
                            &runtime_start_nonce,
                            &pubkey_hex,
                            &config.relay_url,
                            "listening",
                            None,
                        );
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
                            dispatch_pending(&mut pool, &mut queue, &ctx, &mut last_activity)
                        {
                            typing_channels.insert(channel_id, thread_tags);
                        }
                    } else if pool.any_idle() {
                        dispatch_heartbeat(&mut pool, &ctx, &mut heartbeat_in_flight, &drain);
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
                for (channel_id, thread_tags) in
                    dispatch_pending(&mut pool, &mut queue, &ctx, &mut last_activity)
                {
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
                for (channel_id, thread_tags) in
                    dispatch_pending(&mut pool, &mut queue, &ctx, &mut last_activity)
                {
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
                    Ok(pool::SteerAck::Success { .. }) => (false, true, false),
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
                if let Ok(pool::SteerAck::Success { session_id }) = &ack {
                    queue.extend_in_flight_deadline(channel_id, config.max_turn_duration_secs);
                    if !pool.record_successful_steer(
                        channel_id,
                        event_id.clone(),
                        session_id.clone(),
                    ) {
                        tracing::warn!(
                            channel = %channel_id,
                            event_id = %event_id,
                            "successful steer lost its in-flight delivery ledger"
                        );
                    }
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
                for (channel_id, thread_tags) in
                    dispatch_pending(&mut pool, &mut queue, &ctx, &mut last_activity)
                {
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
                // A terminal wake never becomes ready. Every batch buffered for
                // it is disposed of here under the same ordering as an
                // in-flight failure — durable commit, then notice — because
                // the events are just as unrunnable and just as revivable.
                if let Some(terminal) = pool_lifecycle.blocked_auth() {
                    let error = completion
                        .as_ref()
                        .err()
                        .map(|e| e.summary())
                        .unwrap_or_default();
                    tracing::error!(
                        attempt,
                        terminal = %terminal,
                        "lazy pool wake blocked on terminal authentication — \
                         disposing of all buffered work; no further wake will be scheduled"
                    );
                    emit_runtime_lifecycle(
                        observer.as_ref(),
                        &runtime_start_nonce,
                        &pubkey_hex,
                        &config.relay_url,
                        "failed",
                        Some(&error),
                    );
                    if dispose_batches_for_terminal_auth(
                        &mut queue,
                        Some(&ctx.rest_client),
                        terminal,
                    )
                    .is_err()
                    {
                        // Break rather than return: the shutdown drain below
                        // still has to reap the wake task's children, and a
                        // bare return would leave them to best-effort `Drop`.
                        break;
                    }
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
                            dispatch_pending(&mut pool, &mut queue, &ctx, &mut last_activity)
                        {
                            typing_channels.insert(channel_id, thread_tags);
                        }
                    }
                    Err(error) => {
                        let summary = error.summary();
                        debug_assert_eq!(pool_lifecycle.failed_error(), Some(summary.as_str()));
                        emit_runtime_lifecycle(
                            observer.as_ref(),
                            &runtime_start_nonce,
                            &pubkey_hex,
                            &config.relay_url,
                            "failed",
                            Some(&summary),
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
    // Aborted rather than drained, like the observer publisher above. Anything
    // still queued here is a `working` announcement for a turn this process is
    // about to stop running, and NIP-PA's staleness window is what clears it —
    // the desktop stops showing the agent as working 45 seconds later without
    // needing a farewell nobody is guaranteed to send.
    if let Some(handle) = project_activity_task.take() {
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

    /// Begin the walk back through the roots this agent is already addressed
    /// on, over the coordinate set discovery has reached.
    fn submit_enrolment_history(
        &self,
        coordinates: Vec<String>,
        agent: String,
    ) -> impl std::future::Future<Output = Result<(), relay::RelayError>>;

    /// Rebuild one restored root's own history — comments, revisions and,
    /// above all, lifecycle.
    ///
    /// Takes the proof rather than a root id: only this side holds the
    /// discovered set a root must be validated against, so minting it here is
    /// what stops the relay task rebuilding a root nothing vouched for.
    fn submit_root_catch_up(
        &self,
        root: project::VerifiedBoundRoot,
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

    async fn submit_enrolment_history(
        &self,
        coordinates: Vec<String>,
        agent: String,
    ) -> Result<(), relay::RelayError> {
        relay::HarnessRelay::submit_enrolment_history(self, coordinates, agent).await
    }

    async fn submit_root_catch_up(
        &self,
        root: project::VerifiedBoundRoot,
    ) -> Result<(), relay::RelayError> {
        relay::HarnessRelay::submit_root_catch_up(self, root).await
    }
}

/// The run loop's project state, grouped so the project arm can be entered
/// without standing up a relay connection.
///
/// Every field is a `&mut` borrow of something [`tokio_main`] owns; this holds
/// no state of its own. It exists because the arm's body — dispatch, then flush
/// — could not otherwise be reached: `run()` parses CLI arguments, installs a
/// global tracing subscriber and connects a relay before the `select!` is even
/// built, so nothing could enter it, and the missing flush lived there
/// unobserved.
pub(crate) struct ProjectArm<'a> {
    pub(crate) identity: &'a project::AgentIdentity,
    pub(crate) owner: Option<&'a str>,
    pub(crate) approved_humans: &'a std::collections::BTreeSet<String>,
    pub(crate) approved_external_agents: &'a std::collections::BTreeSet<String>,
    pub(crate) discovered: &'a mut project::DiscoveredRepositories,
    pub(crate) enrolments: &'a mut project::ProjectEnrolments,
    pub(crate) ledger: &'a mut peer_call::CallLedger,
    pub(crate) seen: &'a mut ProjectSeenIds,
    pub(crate) agent_pubkey_hex: &'a str,
    pub(crate) startup_watermark: u64,
    /// The in-process observer bus, when this configuration has one.
    ///
    /// Passed down so the gate can say that it queued something —
    /// [`observe_project_event_queued`]. `None` whenever neither project
    /// routing nor telemetry is on, in which case nobody is listening.
    pub(crate) observer: Option<&'a observer::ObserverHandle>,
    /// Whether the runtime is still admitting work.
    ///
    /// Shared as `&`, never `&mut`, and the asymmetry is the point: the project
    /// arm is an admission *point*, not somewhere that may decide to drain.
    /// Only [`handle_relay_observer_control_event`] holds the mutable handle.
    pub(crate) drain: &'a drain::DrainState,
}

/// One project event, from arrival to a running turn.
///
/// **Queueing is not dispatching.** This is the whole of finding 2: the arm
/// dispatched the event, recorded that it had queued, and returned to the
/// `select!`. The channel arm has always flushed after admitting an event, and
/// for a runtime with channels the project queue got flushed too — by the next
/// channel event, or by a heartbeat that only flushes when it already knows
/// there is work. A project-only runtime has neither, so "project-only
/// operation is valid" meant accepting work and leaving it there.
///
/// The flush is the ordinary pool path, not a project-specific one, so queue
/// ownership, in-flight accounting and activity stay a single mechanism.
/// A first-contact shape worth paying the exact-root read for: a comment that
/// visibly mentions this agent, or a peer-call envelope naming it as callee.
///
/// The second arm is what makes the decision matrix's
/// `TrustedAgent + Invocation => EnrolAndWake` reachable on an `Unknown` root.
/// Without it the matrix permitted an enrolment the resolver never supplied a
/// root binding for, and a trusted peer's first call on a fresh issue — an
/// envelope that was cryptographically exact down to its recomputed call id —
/// was refused with nothing but a debug line (#0a81a1ca, 2026-08-04). A call
/// addresses by construction — sole `p` naming the callee, verified id — so it
/// is not asked to also @mention in prose: prose is the human grammar, and the
/// envelope is the agent one.
fn first_contact_shape(event: &project::VerifiedProjectEvent, agent_pubkey_hex: &str) -> bool {
    match event.kind() {
        k if k == buzz_core::kind::KIND_TEXT_NOTE => {
            event_mentions_agent(event.event(), agent_pubkey_hex)
        }
        k if k == buzz_core::peer_call::KIND_PEER_CALL => matches!(
            peer_call::call_marker(
                &peer_call::VerifiedPeerEvent::from_project(event),
                agent_pubkey_hex,
            ),
            project::CallMarker::Invocation
        ),
        _ => false,
    }
}

/// Resolve the separately signed root needed by comment-first enrolment.
///
/// `Err` is transient and the caller must not dispatch (or spend dedupe) yet.
/// `Ok(None)` is a definitive non-candidate. Existing active continuations do
/// not pay an exact-root read on every comment.
async fn resolve_comment_first_candidate(
    project_event: &project::ProjectEvent,
    enrolments: &project::ProjectEnrolments,
    discovered: &project::DiscoveredRepositories,
    agent_pubkey_hex: &str,
    rest: &relay::RestClient,
) -> Result<Option<project::EnrolmentCandidate>, String> {
    let project::ProjectEvent::Routed { route, event, .. } = project_event else {
        return Ok(None);
    };
    if !first_contact_shape(event, agent_pubkey_hex)
        || matches!(
            enrolments.state_of(route.root()),
            project::RootState::Active
        )
    {
        return Ok(None);
    }
    let root_id = nostr::EventId::from_hex(route.root())
        .map_err(|e| format!("invalid routed root id: {e}"))?;
    let value = rest
        .query(&[nostr::Filter::new()
            .id(root_id)
            .kinds([
                nostr::Kind::Custom(buzz_core::kind::KIND_GIT_ISSUE as u16),
                nostr::Kind::Custom(buzz_core::kind::KIND_GIT_PULL_REQUEST as u16),
            ])
            .limit(2)])
        .await
        .map_err(|e| e.to_string())?;
    let Ok(events) = serde_json::from_value::<Vec<nostr::Event>>(value) else {
        return Ok(None);
    };
    if events.len() != 1 || events[0].id != root_id {
        return Ok(None);
    }
    let Ok(verified) =
        project::VerifiedProjectEvent::verify(events.into_iter().next().unwrap()).await
    else {
        return Ok(None);
    };
    Ok(project::validate_enrolment_candidate(&verified, discovered))
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn dispatch_and_flush_project_event(
    arm: &mut ProjectArm<'_>,
    sibling: Option<project::VerifiedSibling>,
    resolved_candidate: Option<project::EnrolmentCandidate>,
    subscriber: &impl ProjectSubscriber,
    project_event: &project::ProjectEvent,
    pool: &mut AgentPool,
    queue: &mut EventQueue,
    ctx: &Arc<PromptContext>,
    // Threaded through for the `dispatch_pending` call below: a project turn
    // dispatched from here is work, and the inactivity clock it feeds is what
    // `exit_after_inactivity_secs` reads.
    last_activity: &mut tokio::time::Instant,
    typing_channels: &mut HashMap<Uuid, ThreadTags>,
    pool_ready: bool,
) -> ProjectDispatched {
    // ── Drain: the project admission point ────────────────────────────────
    //
    // Ahead of `dispatch_project_event`, and every word of that ordering is
    // load-bearing. That function is where a project event id is **spent**,
    // where the queue insertion happens, where the NIP-PA `state=queued`
    // announcement is emitted, and where subscription REQs are replaced. A
    // refusal placed after any of those would have consumed the event in some
    // way — spent its dedup id, promised work on a public issue, or widened a
    // subscription — for a process that is about to stop.
    //
    // Refused here, nothing is consumed. `ProjectDispatched::Ignored` is
    // already documented as the disposition that "spends nothing", and that is
    // exactly true of this one: the event stays relay history, the id stays
    // unspent, the root stays unannounced. The successor process's enrolment
    // filter reaches back `ACCEPTED_CLOCK_SKEW_SECS` from its own startup
    // watermark and its enrolment-history walk paginates the root's whole past,
    // so a comment declined here is delivered to the next binary rather than
    // lost with this one.
    //
    // This is also the gate the tests drive. The run loop has the same check
    // hoisted above its two REST round-trips, but that one is a latency
    // optimisation; this one is the decision.
    if !arm.drain.admits_new_work() {
        tracing::info!("draining — project event refused, unspent and still on the relay");
        return ProjectDispatched::Ignored;
    }

    let dispatched = dispatch_project_event(
        &mut ProjectDispatch {
            identity: project::ProjectIdentity {
                agent: arm.identity,
                agent_owner: arm.owner,
                approved_humans: arm.approved_humans,
                approved_external_agents: arm.approved_external_agents,
            },
            discovered: arm.discovered,
            enrolments: arm.enrolments,
            queue,
            sibling,
            ledger: arm.ledger,
            resolved_candidate: resolved_candidate.as_ref(),
            observer: arm.observer,
        },
        arm.seen,
        subscriber,
        arm.agent_pubkey_hex,
        arm.startup_watermark,
        project_event,
    )
    .await;
    tracing::debug!(?dispatched, "project dispatch");

    if pool_ready && matches!(dispatched, ProjectDispatched::Queued { queued: true, .. }) {
        for (channel_id, thread_tags) in dispatch_pending(pool, queue, ctx, last_activity) {
            typing_channels.insert(channel_id, thread_tags);
        }
    }
    dispatched
}

/// Project event ids that have already been dispatched, across every
/// subscription generation.
///
/// The relay task spends an id per *live* project source already, and that is
/// not this. Its domain deliberately excludes catch-up rows — sharing one would
/// make a history page read short by exactly the events already seen live — so
/// a root delivered live on the enrolment REQ and again as a replayed row
/// during the watched-REQ replacement passes both checks and is dispatched
/// twice. On an issue that is two model turns and two replies.
///
/// One set per runtime, consulted at the single point every generation
/// converges on. Bounded and two-generation, the same shape as the channel and
/// relay dedups, so a long-lived runtime cannot grow it without limit.
pub(crate) struct ProjectSeenIds(relay::TwoGenDedup);

impl ProjectSeenIds {
    pub(crate) fn new() -> Self {
        Self(relay::TwoGenDedup::new(relay::SEEN_ID_LIMIT))
    }

    /// Record `id`. Returns `true` when it is new — i.e. when this delivery is
    /// the one that gets to act.
    pub(crate) fn insert(&mut self, id: String) -> bool {
        self.0.insert(id)
    }
}

impl Default for ProjectSeenIds {
    fn default() -> Self {
        Self::new()
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
///
/// It is also where a project event id is **spent**. One delivery, one effect,
/// whatever the event arrived on — see [`ProjectSeenIds`] for why the relay
/// task's own dedup cannot answer that question, and why the answer has to live
/// here rather than in the run loop: the connected harness reaches production
/// through this function and would otherwise bypass the gate entirely.
pub(crate) async fn dispatch_project_event(
    dispatch: &mut ProjectDispatch<'_>,
    seen: &mut ProjectSeenIds,
    subscriber: &impl ProjectSubscriber,
    agent_pubkey_hex: &str,
    since: u64,
    project_event: &project::ProjectEvent,
) -> ProjectDispatched {
    // Spent before the gate, and only for routed events.
    //
    // A routed event carries an id and an immutable meaning: the same id
    // decides the same way every time, so refusing the second delivery cannot
    // lose an effect the first did not already have. Discovery is excluded
    // because ingesting an announcement is a set insert — idempotent by
    // construction, and spending ids for it would make the ceiling a function
    // of how often the relay repeats itself.
    if let project::ProjectEvent::Routed { event, .. } = project_event {
        if !seen.insert(event.id()) {
            tracing::debug!(
                event_id = %event.id(),
                "duplicate project event across subscriptions — already dispatched"
            );
            return ProjectDispatched::Ignored;
        }
    }

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
            // …and the history behind it, as a separate walk.
            //
            // Both, on every discovery change. The tail alone was the restart
            // defect: an issue addressed to this agent before it started is
            // outside a tail's `since` by definition, so the agent held no
            // authority for its own conversations and correctly refused
            // everything referring to them. Widening the tail could not fix it
            // — a fixed-identity REQ cannot paginate, so any reach-back it
            // carried could only sample and call the sample complete.
            let coordinates: Vec<String> = dispatch.discovered.iter().cloned().collect();
            if !coordinates.is_empty() {
                if let Err(e) = subscriber
                    .submit_enrolment_history(coordinates, agent_pubkey_hex.to_string())
                    .await
                {
                    tracing::warn!("enrolment history submission error: {e}");
                }
            }
        }
        // A root joined or rejoined the watched set. The watched REQ names every
        // root explicitly, so the successor takes a fresh generation and the
        // predecessor is retired once the successor is installed.
        //
        // A newly watched root **that predates this process** additionally asks
        // for its own history, and it fires *after* the enrolment above, so the
        // binding the merge and the authority gate read is already in place.
        //
        // Keyed on the root's own timestamp rather than on how it arrived. The
        // processing mode is tempting — `apply_processing_mode` produces a bare
        // `Enrol` only under `Replay`, so `Enrolled` alone reads as "restored"
        // — but it is a *proxy* for "this process watched this root happen",
        // and inside the enrolment tail's clock-skew reach-back that proxy is
        // wrong. `enrolment_filter` starts the live root tail at
        // `watermark - ACCEPTED_CLOCK_SKEW_SECS`, while `watched_roots_filters`
        // starts at the watermark exactly. So a root published in that window
        // arrives **live**, enrols live, and a status event published between
        // that root and startup is inside no REQ at all: too late for the
        // enrolment walk's cutoff, too early for the watched REQ, and never
        // asked for by a catch-up that only fires on replay. The root's own
        // `created_at` is the fact the mode was standing in for.
        //
        // A root created after the watermark has no history this process did
        // not see, so it asks for none.
        ProjectDispatched::Enrolled
        | ProjectDispatched::Queued {
            watch_changed: true,
            ..
        } => {
            if let project::ProjectEvent::Routed { event, .. } = project_event {
                if event.event().created_at.as_secs() <= since {
                    match project::VerifiedBoundRoot::prove(
                        std::slice::from_ref(event),
                        dispatch.discovered,
                    ) {
                        Some(root) => {
                            if let Err(e) = subscriber.submit_root_catch_up(root).await {
                                tracing::warn!("root catch-up submission error: {e}");
                            }
                        }
                        // Not a root at all — the ordinary shape of a comment
                        // that woke a dormant root, which changes the watched
                        // set without being a root to rebuild.
                        //
                        // For `Enrolled` it is a contradiction rather than a
                        // shape: that arm enrolled *this* event, which required
                        // a validated candidate. Worth a word, because a root
                        // watched with no history request is one whose dormancy
                        // will not survive the next restart, silently.
                        None if matches!(dispatched, ProjectDispatched::Enrolled) => {
                            tracing::warn!(
                                event_id = %event.id(),
                                "enrolled root cannot be proven for history reconstruction"
                            )
                        }
                        None => {}
                    }
                }
            }
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
    /// Separately fetched, signature-verified root evidence for a directing
    /// comment. It supplies the binding only; the comment remains the authority.
    resolved_candidate: Option<&'a project::EnrolmentCandidate>,
    /// The in-process observer bus, when this configuration has one.
    ///
    /// **Emit-only, and still no relay capability.** The paragraph above holds:
    /// this cannot open a subscription or send an event. What it can do is say
    /// that something was queued, onto the same bus every other turn fact
    /// travels on — which is why the queued signal reaches the wire through
    /// [`ProjectActivityPublisher`] like every other state, rather than by
    /// giving the gate a publisher of its own.
    observer: Option<&'a observer::ObserverHandle>,
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
    /// An authorised status event moved a watched root between the active and
    /// dormant sets. No turn was run.
    ///
    /// Carries the state the root is now in rather than a bare acknowledgement:
    /// "the close was applied" and "the root is dormant" are the same claim only
    /// if the transition is read the way the kind meant it, and this is the
    /// value a caller can check that against.
    ///
    /// **No subscription replacement follows.** `all_roots` covers active *and*
    /// dormant precisely so a reopen stays observable, so the watched-root REQ
    /// this event arrived on is already the right one. Replacing it here would
    /// churn a live request for an identical successor.
    LifecycleApplied { root_state: project::RootState },
}

/// Move a watched root between active and dormant on an authorised status event.
///
/// Reached only with [`project::ProjectEffect::ApplyLifecycle`], which
/// `classify_project_event` produces only when the signer is the stored root
/// author or the stored repository owner. Nothing here re-decides that.
///
/// A transition that changes nothing — closing a dormant root, reopening an
/// active one — reports the state it found rather than inventing a change, so a
/// duplicate status event is idempotent rather than a second event to explain.
fn apply_project_lifecycle(
    dispatch: &mut ProjectDispatch<'_>,
    root: &str,
    kind: u32,
) -> ProjectDispatched {
    let Some(transition) = project::lifecycle_transition(kind) else {
        // `classify_kind` called this a lifecycle kind and this function does
        // not recognise it. That is two functions disagreeing about the same
        // list, and the safe answer is to change no watch.
        tracing::warn!(root = %root, kind, "authorised lifecycle kind carries no transition");
        return ProjectDispatched::Ignored;
    };
    let changed = match transition {
        project::LifecycleTransition::Activate => dispatch.enrolments.reopen(root),
        project::LifecycleTransition::Suspend => dispatch.enrolments.close(root),
    };
    let root_state = dispatch.enrolments.state_of(root);
    tracing::info!(
        root = %root,
        kind,
        ?transition,
        ?root_state,
        changed,
        "authorised lifecycle applied to a watched root"
    );
    ProjectDispatched::LifecycleApplied { root_state }
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
            mode,
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
                dispatch.resolved_candidate,
            );

            // History restores state; it does not answer anyone.
            //
            // Folded here, on the decision this gate just made, rather than at
            // any of the effect sites below: one place to read, and no effect
            // can be added later that quietly escapes it.
            //
            // `mode` is read off the event, never recomputed from `source`. It
            // was decided in the relay task, against the registration that
            // admitted the frame, at the moment it arrived — and a frame's
            // provenance cannot be recovered afterwards, because a backlog
            // frame may still be sitting in this queue when its boundary
            // arrives and retires the registration that explains it.
            let decision = project::ProjectDecision {
                effect: project::apply_processing_mode(decision.effect, *mode),
                ..decision
            };

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

            // ── Lifecycle ────────────────────────────────────────────────────
            //
            // **Before the origin guard, and that is the point.** A lifecycle
            // event runs no turn, so `decide_project_event` gives it no
            // `ProjectOrigin` — an effect that cannot queue must not carry a
            // binding a caller could queue it by. Reaching the guard first
            // therefore sent every authorised close into the "refused —
            // nothing enrolled, queued or spent" branch, which is the second
            // half of why a valid owner close changed nothing: the authority
            // was unreachable, and the effect it would have produced had
            // nowhere to be applied.
            if matches!(decision.effect, project::ProjectEffect::ApplyLifecycle) {
                return apply_project_lifecycle(dispatch, route.root(), event.kind());
            }

            let Some(origin) = decision.origin.clone() else {
                // A refused peer-call invocation that names this agent gets an
                // INFO line, not debug. Twice in one day (roots bdc226e9 and
                // 0a81a1ca) a silent refusal here was indistinguishable from an
                // outage: the caller re-sent, a human watched nothing happen,
                // and diagnosis required a debug-level restart. The refusal may
                // be entirely correct — that is exactly why it must say so out
                // loud, with enough of the decision to name which gate said no.
                if event.kind() == buzz_core::peer_call::KIND_PEER_CALL
                    && matches!(
                        peer_call::call_marker(
                            &peer_call::VerifiedPeerEvent::from_project(event),
                            &dispatch.identity.agent.hex().to_ascii_lowercase(),
                        ),
                        project::CallMarker::Invocation
                    )
                {
                    tracing::info!(
                        ?source,
                        root = %route.root(),
                        effect = ?decision.effect,
                        caller = %event.author(),
                        "peer call named this agent but was refused — the call \
                         stays on the relay; if the caller is trusted and the \
                         root unknown, an enrolment candidate could not be \
                         resolved or the author gate declined it"
                    );
                } else {
                    tracing::debug!(
                        ?source,
                        root = %route.root(),
                        kind = event.kind(),
                        effect = ?decision.effect,
                        "project event refused — nothing enrolled, queued or spent"
                    );
                }
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
                let candidate = project::validate_enrolment_candidate(event, dispatch.discovered)
                    .or_else(|| dispatch.resolved_candidate.cloned());
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
                    // Tell the issue, not just the log.
                    //
                    // This line and the one above report the same moment, and
                    // until now only the operator's terminal received it. On the
                    // root, a comment that woke this agent and a comment that
                    // addressed nobody looked identical — both were silence —
                    // until the turn actually started, which on a busy pool is
                    // minutes away.
                    //
                    // Gated on `queued` because the push is what decides:
                    // `false` means the event was refused (a durable terminal
                    // disposition, or drop-mode against an in-flight route), and
                    // announcing work that will never run is a worse signal than
                    // none. See `queue::EventQueue::push`.
                    if queued {
                        observe_project_event_queued(dispatch.observer, &origin, &event.id());
                    }
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

/// Mark every active channel task as pre-cutoff work, then request cancellation
/// wherever its one-shot control receiver is still available. The marker is
/// authoritative even when an earlier control already consumed the sender.
fn apply_cancel_all_cutoff(pool: &mut AgentPool) -> (usize, usize) {
    let task_ids: Vec<_> = pool
        .task_map()
        .iter()
        .filter(|(_, meta)| meta.channel_id.is_some())
        .map(|(task_id, _)| *task_id)
        .collect();

    for task_id in &task_ids {
        pool.mark_cancel_all_cutoff(*task_id);
    }

    let mut signalled = 0;
    for task_id in &task_ids {
        let meta = pool
            .task_map_mut()
            .get_mut(task_id)
            .expect("cutoff task remains registered");
        meta.recoverable_batch = None;
        if let Some(tx) = meta.control_tx.take() {
            if tx.send(ControlSignal::Cancel).is_ok() {
                signalled += 1;
            }
        }
    }
    if !task_ids.is_empty() {
        tracing::info!(
            active_turns = task_ids.len(),
            signalled_turns = signalled,
            "cancel-all cutoff applied to active channel tasks"
        );
    }
    (task_ids.len(), signalled)
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
    last_activity: &mut tokio::time::Instant,
) -> Vec<(Uuid, ThreadTags)> {
    let mut dispatched_channels = Vec::new();
    loop {
        let batch = match queue.flush_next() {
            Some(b) => b,
            None => break,
        };
        let channel_id = batch.channel_id;
        let is_project_batch = batch.project_origin().is_some();
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
                successful_steer_deliveries: HashSet::new(),
            },
        );
        // A typing indicator is a channel-scoped NIP-29 write: it carries an
        // `h` tag naming the channel it belongs to. A project route key is a
        // UUIDv5 of a root and names no channel, so reporting one here would
        // publish a typing frame `h`-tagged to a channel that does not exist —
        // the same mistake `observer_route_for` exists to avoid. A project turn
        // already announces itself, as NIP-PA activity on its root.
        //
        // Read from the batch rather than from the key's shape: a UUID cannot
        // be asked whether it names a channel.
        if is_project_batch {
            tracing::debug!(key = %channel_id, "project batch dispatched — no typing indicator");
        } else {
            dispatched_channels.push((channel_id, typing_scope));
        }
        // Outside the typing-indicator gate on purpose: this is the inactivity
        // clock the `exit_after_inactivity_secs` bound reads, and a project
        // batch is work. Updating it only on the channel path would let a
        // project-only runtime dispatch turns continuously and still look idle
        // enough to exit.
        *last_activity = tokio::time::Instant::now();
    }
    tracing::debug!(
        dispatched = dispatched_channels.len(),
        queue_depth = queue.pending_channels(),
        "dispatch_pending"
    );
    dispatched_channels
}

/// The typed terminal-auth disposition carried by an outcome, if any.
///
/// The classification itself happens once, at the ACP seam
/// ([`crate::terminal_auth`]), so nothing outside that module ever matches on
/// provider prose. This helper only reads the already-typed answer.
fn terminal_auth_of(outcome: &PromptOutcome) -> Option<terminal_auth::TerminalAuth> {
    match outcome {
        PromptOutcome::Error(acp::AcpError::TerminalAuth(terminal)) => Some(*terminal),
        _ => None,
    }
}

/// The user-visible copy for a terminal authentication failure.
///
/// One string for every provider: the notice tells the user what to do, and
/// naming the adapter's internal error would only leak provider text into a
/// channel. Deliberately identical to the pre-existing wording so this phase
/// changes disposition, not voice.
const TERMINAL_AUTH_NOTICE: &str =
    "⚠️ I couldn't process the last request: authentication failed. \
     Please re-authenticate the CLI (e.g. run `claude /login` or `codex login`) \
     and then re-send.";

/// Durably dispose of every batch the queue is still holding, then notify.
///
/// Used when the runtime itself is blocked on a terminal authentication
/// failure and no wake will ever deliver these events. Each batch follows the
/// same ordering as the in-flight path — durable commit, then notice — so a
/// crash between the two suppresses a notice rather than reviving a request.
///
/// Returns `Err(())` when any commit fails. The caller must then stop without
/// reporting anything: a batch we could not promise to suppress must not be
/// announced as terminal.
fn dispose_batches_for_terminal_auth(
    queue: &mut EventQueue,
    rest_client: Option<&relay::RestClient>,
    terminal: terminal_auth::TerminalAuth,
) -> Result<usize, ()> {
    let batches = queue.drain_all_pending_batches();
    let mut disposed = 0usize;
    for batch in batches {
        match queue.commit_terminal_auth_disposition(&batch) {
            Ok(count) => {
                disposed += count;
                spawn_failure_notice(rest_client, &batch, TERMINAL_AUTH_NOTICE.to_string());
            }
            Err(e) => {
                tracing::error!(
                    channel_id = %batch.channel_id,
                    events = batch.events.len(),
                    terminal = %terminal,
                    error = %e,
                    "failed to durably record terminal-auth disposition for buffered batch — \
                     stopping harness without notice"
                );
                return Err(());
            }
        }
    }
    Ok(disposed)
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

// ── Auth-required episodes ───────────────────────────────────────────────────

/// The NIP-AO observer kind that tells an owner their agent is locked out.
///
/// A kind of its own rather than another `turn_error`: an expired credential is
/// not a turn that went wrong, it is a harness that cannot run any turn until a
/// human does something, and the Desktop should be able to render it as such
/// without pattern-matching error prose. It rides the existing observer plane,
/// so it is NIP-44 encrypted to the owner and `p`-tagged to them by
/// [`publish_relay_observer_event`] — which is the whole reason this is an
/// observer frame and not new direct-message machinery.
pub(crate) const OBSERVER_AUTH_REQUIRED: &str = "auth_required";

/// What the agent says in public when its credential has expired.
///
/// **Every word of this is chosen for what it leaves out.** It rides a public
/// comment on an issue, a PR or a channel, so it names no authentication
/// method, no provider, no login URL and no CLI command: which credential
/// expired and how it is renewed is operational detail about the operator's
/// machine, and the people reading the issue can do nothing with it anyway.
///
/// What it does say is the two things the person who mentioned the agent
/// actually needs: the agent is not ignoring them, and the one person who can
/// fix it already knows.
const AUTH_REQUIRED_PUBLIC_NOTICE: &str = "⚠️ I can't act on this right now — \
     my operator session needs re-authentication. My owner has been notified. \
     Please re-send once it's sorted.";

/// One authentication outage, from the first failed turn to the next good one.
///
/// The unit here is deliberately the *episode* and not the turn. A credential
/// that has expired fails every turn it is given, so an agent that answered
/// per-turn would post a comment for every mention it had queued — ten mentions
/// on a busy root become ten identical apologies, which is worse than the
/// silence it was meant to replace, and none of the ten helps because the only
/// person who can act has already been told once.
///
/// So the first failure claims both notifications and every subsequent failure
/// claims nothing. A successful turn is the only thing that closes an episode:
/// it is direct evidence the credential works again, which is a stronger and
/// simpler signal than any timer, and it means a re-authenticated agent that
/// later expires again gets a fresh episode and speaks up again.
///
/// One public notice per episode is a real trade: an outage that spans two
/// roots answers on the first and leaves the second silent. That is the
/// documented choice — a provider credential is global to this harness, so the
/// outage is one fact, and the alternative (one notice per root) turns a
/// stale credential into a broadcast across every project the agent watches.
///
/// **Follow-up, deliberately not in this change:** the ACP `authMethods` this
/// frame carries include terminal device-flow methods (`claude-ai-login` and
/// friends), which means the owner could in principle be shown the device code
/// and send the resulting token back over Buzz to re-authenticate the agent
/// in place. That flow needs its own trust review — it moves a live credential
/// across the relay — so this change stops at telling the owner, and the
/// accept-the-code-back path is left for a change that can be reviewed on its
/// own merits.
#[derive(Debug, Default)]
pub(crate) struct AuthEpisode {
    /// The failure that opened the current episode, or `None` when the
    /// credential is believed good.
    opened_by: Option<terminal_auth::TerminalAuth>,
    /// Whether this episode's one public notice has been claimed.
    public_notice_claimed: bool,
    /// Whether this episode's one owner frame has been claimed.
    owner_frame_claimed: bool,
}

impl AuthEpisode {
    /// Record a terminal-auth failure, opening an episode if none is open.
    fn observe_failure(&mut self, terminal: terminal_auth::TerminalAuth) {
        if self.opened_by.is_none() {
            tracing::warn!(
                terminal = %terminal,
                "auth-required episode opened — the agent cannot run turns until \
                 its operator re-authenticates"
            );
            self.opened_by = Some(terminal);
        }
    }

    /// Claim this episode's single public notice, if it is still unclaimed.
    ///
    /// Claims are recorded before the notice is sent rather than after,
    /// because the send is a best-effort background task: a relay that refuses
    /// the comment must not license a retry on the next failed turn, or a
    /// persistently unreachable relay becomes a persistent notice storm the
    /// moment it recovers.
    fn claim_public_notice(&mut self) -> bool {
        !std::mem::replace(&mut self.public_notice_claimed, true)
    }

    /// Claim this episode's single owner frame, if it is still unclaimed.
    fn claim_owner_frame(&mut self) -> bool {
        !std::mem::replace(&mut self.owner_frame_claimed, true)
    }

    /// Close any open episode, because a turn just succeeded.
    ///
    /// Returns whether an episode was actually open, so the caller can log a
    /// recovery exactly once rather than on every good turn thereafter.
    fn resolve(&mut self) -> bool {
        if let Some(terminal) = self.opened_by.take() {
            tracing::info!(
                terminal = %terminal,
                "auth-required episode closed — a turn completed, so the \
                 credential works again"
            );
            self.public_notice_claimed = false;
            self.owner_frame_claimed = false;
            return true;
        }
        false
    }
}

/// Where a public auth-required notice goes, and how it is addressed.
///
/// A project turn and a channel turn need genuinely different events — a
/// kind:1 comment addressed to a repo coordinate and a root, versus a kind:9
/// channel message — and the batch is the only thing that knows which one this
/// turn was. Resolved into this enum first, so the decision is testable
/// without a relay and the sending half stays a dumb executor.
#[derive(Debug, Clone)]
enum AuthNoticeTarget {
    /// An issue or pull-request root, addressed by repo coordinate.
    ProjectRoot {
        coordinate: String,
        meta: buzz_sdk::GitCommentMeta,
    },
    /// A channel, threaded under the triggering message.
    Channel {
        channel_id: Uuid,
        thread_tags: queue::ThreadTags,
    },
}

impl AuthNoticeTarget {
    /// A description of this surface for the owner's frame.
    ///
    /// The owner is the one person entitled to know *where* their agent
    /// addressed its apology, and it is the difference between "somebody was
    /// told" and "this failed silently in a channel I forgot about".
    ///
    /// Says where the notice was *addressed*, not that a relay accepted it:
    /// the send is a best-effort background task like every other notice here,
    /// and a frame that waited for delivery confirmation would be a frame the
    /// owner got late or not at all.
    fn placement(&self) -> serde_json::Value {
        match self {
            Self::ProjectRoot { coordinate, meta } => serde_json::json!({
                "surface": "project_root",
                "coordinate": coordinate,
                "root": meta.root_event,
            }),
            Self::Channel { channel_id, .. } => serde_json::json!({
                "surface": "channel",
                "channelId": channel_id.to_string(),
            }),
        }
    }
}

/// Decide where a failed batch's public notice belongs.
///
/// A project batch runs under a route key that is a UUIDv5 of its root and
/// names no channel, so posting to `batch.channel_id` would send the notice to
/// a channel that does not exist — the reason this cannot simply reuse the
/// existing channel-only failure-notice path. The batch's own validated
/// project origin is what decides, exactly as it does for the observer route.
fn auth_notice_target_for(batch: &FlushBatch) -> AuthNoticeTarget {
    let last = batch
        .events
        .last()
        .or_else(|| batch.cancelled_events.last());
    match batch.project_origin() {
        Some(origin) => AuthNoticeTarget::ProjectRoot {
            coordinate: origin.coordinate().to_string(),
            meta: buzz_sdk::GitCommentMeta {
                root_event: origin.root().to_string(),
                // Threaded under the comment that woke us, so the person who
                // mentioned the agent sees the answer where they asked.
                parent_event: last.map(|be| be.event.id.to_hex()),
                // `p`-tagged for the same reason: on a busy root, an untagged
                // comment is one the asker is never told about.
                recipients: last
                    .map(|be| be.event.pubkey.to_hex())
                    .into_iter()
                    .collect(),
            },
        },
        None => AuthNoticeTarget::Channel {
            channel_id: batch.channel_id,
            thread_tags: last
                .map(|be| queue::parse_thread_tags(&be.event))
                .unwrap_or_default(),
        },
    }
}

/// Build the kind:1 comment that carries an auth-required notice to a root.
///
/// Separated from the send so the event a public root would actually receive
/// can be inspected in a test — the one property that matters about this
/// comment is what it does *not* contain.
fn build_auth_required_comment(
    coordinate: &str,
    meta: &buzz_sdk::GitCommentMeta,
) -> Option<nostr::EventBuilder> {
    let Some(repo) = buzz_sdk::GitRepoCoord::from_a_tag_value(coordinate) else {
        tracing::warn!(
            coordinate = %coordinate,
            "auth-required notice: unreadable repository coordinate — not posting"
        );
        return None;
    };
    buzz_sdk::build_git_comment(&repo, AUTH_REQUIRED_PUBLIC_NOTICE, meta)
        .map_err(|error| {
            tracing::warn!(%error, "auth-required notice: refused by the builder");
        })
        .ok()
}

/// Post the episode's one public notice, best effort.
///
/// Best effort in the same sense as every other notice in this file: a relay
/// that will not take it is logged and swallowed, because an agent that cannot
/// announce its own lockout must still shut down cleanly and must still tell
/// its owner.
fn spawn_auth_required_notice(rest_client: Option<&relay::RestClient>, target: AuthNoticeTarget) {
    let Some(rest) = rest_client.cloned() else {
        return;
    };
    tokio::spawn(async move {
        match target {
            AuthNoticeTarget::Channel {
                channel_id,
                thread_tags,
            } => {
                pool::post_failure_notice(
                    &rest,
                    channel_id,
                    &thread_tags,
                    AUTH_REQUIRED_PUBLIC_NOTICE,
                )
                .await;
            }
            AuthNoticeTarget::ProjectRoot { coordinate, meta } => {
                let Some(builder) = build_auth_required_comment(&coordinate, &meta) else {
                    return;
                };
                let event = match builder.sign_with_keys(&rest.keys) {
                    Ok(event) => event,
                    Err(error) => {
                        tracing::warn!(%error, "auth-required notice: sign failed");
                        return;
                    }
                };
                match tokio::time::timeout(Duration::from_secs(5), rest.submit_event(&event)).await
                {
                    Ok(Ok(_)) => {}
                    Ok(Err(error)) => {
                        tracing::warn!(root = %meta.root_event, %error, "auth-required notice failed")
                    }
                    Err(_) => {
                        tracing::warn!(root = %meta.root_event, "auth-required notice timed out")
                    }
                }
            }
        }
    });
}

/// The owner's frame for an auth-required episode.
///
/// Carries the advertised method labels because "re-authenticate" without
/// naming the method is not actionable: an owner staring at this needs to know
/// whether the agent is asking for a Claude subscription login, an API key, or
/// something else entirely. This is safe *here* and nowhere else — the frame is
/// NIP-44 encrypted to the owner's key before it leaves the process, which is
/// exactly the property the public notice does not have.
fn auth_required_owner_payload(
    terminal: terminal_auth::TerminalAuth,
    methods: &[acp::AuthMethod],
    placement: Option<serde_json::Value>,
) -> serde_json::Value {
    serde_json::json!({
        "adapter": terminal.adapter.as_str(),
        "stage": terminal.stage.as_str(),
        "signal": terminal.signal.as_str(),
        "authMethods": methods
            .iter()
            .map(|method| serde_json::json!({ "id": method.id, "label": method.label }))
            .collect::<Vec<_>>(),
        // `null` when there was no public surface to answer on — a heartbeat
        // turn, or a batch whose channel was removed. The owner should be able
        // to tell "I told them and you" from "I could only tell you".
        "publicNotice": placement.unwrap_or(serde_json::Value::Null),
    })
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
    let task_id = pool
        .task_map()
        .iter()
        .find_map(|(task_id, meta)| (meta.agent_index == agent_index).then_some(*task_id))
        .expect("prompt result has registered task metadata");
    let successful_steer_deliveries = pool
        .task_map()
        .get(&task_id)
        .map(|meta| meta.successful_steer_deliveries.clone())
        .unwrap_or_default();

    pool.task_map_mut().remove(&task_id);
    let cancel_all_cutoff = pool.take_cancel_all_cutoff(task_id);
    debug_assert_eq!(before, pool.task_map().len() + 1);

    if let PromptSource::Channel(channel_id) = &result.source {
        // Do not resurrect delivery state for an invalidated session. A
        // replacement session must receive fresh standing context and history.
        if let Some(live_session_id) = result.agent.state.sessions.get(channel_id).cloned() {
            let event_ids = successful_steer_deliveries
                .into_iter()
                .filter(|delivery| delivery.session_id == live_session_id)
                .map(|delivery| delivery.event_id);
            result
                .agent
                .state
                .mark_channel_delivery_success(*channel_id, false, event_ids);
        }
    }

    if cancel_all_cutoff {
        if let Some(batch) = result.batch.take() {
            tracing::warn!(
                channel_id = %batch.channel_id,
                events = batch.events.len() + batch.cancelled_events.len(),
                "discarding pre-cancel-all result batch"
            );
        }
    }

    // The hard-timeout death_message (below) must describe the batch's
    // *actual* fate, not just the `recently_active` eligibility flag — a
    // recently-active batch that exhausts the retry budget in queue.requeue()
    // is dead-lettered same as an immediate one, and both differ from a
    // channel-removed drop or a heartbeat call with no batch at all. Each
    // branch below records what actually happened; only the hard-timeout
    // match arm in the death_message construction reads it.
    let mut hard_timeout_fate_suffix: Option<&'static str> = None;

    // Where this turn's auth-required notice was posted, when one was. Recorded
    // inside the batch block because the batch — the only thing that knows
    // whether this turn came from a root or a channel — is consumed there, and
    // read after it because the owner's frame is emitted once per turn on every
    // path, including the ones that have no batch at all.
    let mut auth_notice_placement: Option<serde_json::Value> = None;

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
            } else if let Some(terminal) = terminal_auth_of(&result.outcome) {
                // Terminal auth is not a retry candidate and not an ordinary
                // dead-letter: the credential will not repair itself, and a
                // batch left merely dropped from memory comes back on the
                // next restart or history replay.
                //
                // The ordering below is the whole contract:
                //   durable commit → notice → mark_complete
                // and the commit is the linearisation point. Nothing before it
                // is observable; nothing after it can revive.
                match queue.commit_terminal_auth_disposition(&batch) {
                    Ok(disposed) => {
                        tracing::warn!(
                            channel_id = %batch.channel_id,
                            events = batch.events.len(),
                            disposed,
                            terminal = %terminal,
                            "terminal authentication failure — batch durably disposed, no retry"
                        );
                        // Answer where the request came from, once per episode.
                        // The person who mentioned this agent gets a reply
                        // rather than silence; the ten people behind them in
                        // the queue do not each get the same apology.
                        let target = auth_notice_target_for(&batch);
                        pool.auth_episode_mut().observe_failure(terminal);
                        if pool.auth_episode_mut().claim_public_notice() {
                            auth_notice_placement = Some(target.placement());
                            spawn_auth_required_notice(rest_client, target);
                        } else {
                            tracing::debug!(
                                terminal = %terminal,
                                "auth-required notice already posted for this episode — \
                                 staying quiet"
                            );
                        }
                    }
                    Err(e) => {
                        // We could not promise non-revival, so we must not act
                        // as though we had. No notice, no completion, no
                        // release of the in-flight channel — stop instead, and
                        // let a restart retry the whole disposition.
                        tracing::error!(
                            channel_id = %batch.channel_id,
                            events = batch.events.len(),
                            terminal = %terminal,
                            error = %e,
                            "failed to durably record terminal-auth disposition — stopping harness \
                             without notice or completion rather than risking a revived request"
                        );
                        return LoopAction::Exit;
                    }
                }
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

    // Tell the owner directly, once per episode. This is the notification the
    // public notice above promises has been sent, and it is the only one of the
    // two that may name the authentication method: the observer publisher
    // NIP-44 encrypts it to the owner's key and `p`-tags them, whereas the
    // notice is readable by everyone who can read the issue.
    if let Some(terminal) = terminal_auth_of(&result.outcome) {
        pool.auth_episode_mut().observe_failure(terminal);
        if pool.auth_episode_mut().claim_owner_frame() {
            let payload = auth_required_owner_payload(
                terminal,
                result.agent.acp.auth_methods(),
                auth_notice_placement.take(),
            );
            // A frame on the bus only *reaches* the owner when the encrypted
            // telemetry publisher is running: the bus itself also comes up for
            // project routing alone, in which case nothing it carries is ever
            // signed, encrypted or sent. Both halves are checked, because a
            // locked-out agent whose owner is never told is the exact failure
            // this change exists to remove, and it must not fail quietly.
            // Logged at error and gated by the same claim as the frame, so it
            // says its piece once per episode rather than once per failed turn.
            if observer.is_none() || !encrypted_telemetry_enabled(config) {
                tracing::error!(
                    terminal = %terminal,
                    payload = %payload,
                    "auth-required: the encrypted observer plane is not running, so the \
                     owner cannot be told the agent is locked out — restart the harness \
                     with --relay-observer and a resolvable owner"
                );
            }
            if let Some(observer) = observer.as_ref() {
                observer.emit(
                    OBSERVER_AUTH_REQUIRED,
                    Some(agent_index),
                    // Deliberately no project route on this frame.
                    // `ProjectActivityPublisher` announces `working` for any
                    // project-routed frame that is not a turn-terminal kind, so
                    // routing this one would put the agent back on the root as
                    // busy at the moment it had just given up. The root travels
                    // in the payload instead, where only the owner reads it.
                    &observer::context_for(channel_id, None, Some(turn_id.clone())),
                    payload,
                );
            }
        }
    }

    match result.outcome {
        // Successful prompt — return agent to pool.
        PromptOutcome::Ok(_) => {
            tracing::debug!(
                agent = agent_index,
                outcome = outcome_label,
                "agent_returned"
            );
            // A completed turn is direct evidence the credential works, and is
            // the only thing that reopens the agent's ability to announce a
            // future outage.
            pool.auth_episode_mut().resolve();
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
    let cancel_all_cutoff = pool.take_cancel_all_cutoff(task_id);
    let i = meta.agent_index;

    // Requeue BEFORE mark_complete (same rationale as handle_prompt_result).
    if !cancel_all_cutoff {
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

/// Run the idle heartbeat prompt, unless something says not to.
///
/// The drain check lives here rather than in the `select!` arm that calls it,
/// for the same reason the `heartbeat_in_flight` check does: this function is
/// the one place that decides to start a heartbeat turn, and a caller-side
/// guard would be a second opinion that the next caller could forget to hold.
/// A heartbeat is unambiguously new work — it is a turn nobody asked for — so a
/// draining runtime must never begin one. A heartbeat already running when the
/// drain arrived is *not* refused here; it finishes, and the run loop's exit
/// check waits for it.
fn dispatch_heartbeat(
    pool: &mut AgentPool,
    ctx: &Arc<PromptContext>,
    heartbeat_in_flight: &mut bool,
    drain: &drain::DrainState,
) {
    if *heartbeat_in_flight {
        return;
    }
    if !drain.admits_new_work() {
        tracing::debug!("heartbeat_skipped_draining");
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
            successful_steer_deliveries: HashSet::new(),
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
            resolved_candidate: None,
            // No bus: these cases are about what dispatch *decides*, and the
            // queued announcement is a consequence of the decision rather than
            // part of it. `a_queued_project_event_is_announced_on_its_root`
            // drives the seam with a real bus.
            observer: None,
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
            resolved_candidate: None,
            observer: None,
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
        mode: project::ProcessingMode,
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
                    // The setup event that binds the root. Always live: it is
                    // standing in for the moment the agent was first addressed.
                    mode: project::ProcessingMode::Live,
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
                mode,
            },
        )
    }

    /// One project process, kept alive across a sequence of events.
    ///
    /// [`dispatch_routed`] rebuilds the world per call, which is right for a
    /// single-event rule and useless for lifecycle: close, comment, reopen and
    /// comment are four events whose whole meaning is what the *previous* one
    /// left behind. A fresh enrolment set per step would make every one of them
    /// pass against a root that was never closed.
    ///
    /// It holds the four pieces of state the run loop holds and hands each
    /// signed event to [`handle_project_event`], the production entry. It
    /// classifies nothing, and it has no way to reach an enrolment set except
    /// through that entry — so "the root is dormant" here is a fact the
    /// production path produced.
    struct ProjectProcess {
        agent: project::AgentIdentity,
        agent_owner: String,
        humans: std::collections::BTreeSet<String>,
        externals: std::collections::BTreeSet<String>,
        discovered: project::DiscoveredRepositories,
        enrolments: project::ProjectEnrolments,
        queue: EventQueue,
        ledger: peer_call::CallLedger,
    }

    impl ProjectProcess {
        /// A process that has seen `repo_owner`'s announcement of `repo_id`,
        /// and whose own owner is `agent_owner`.
        ///
        /// The two are separate parameters because lifecycle authority admits
        /// two distinct signers — the root's author and the repository's owner
        /// — and a fixture that made them the same key could not tell which one
        /// a passing test had actually exercised.
        async fn new(agent: &Keys, agent_owner: &Keys, repo_owner: &Keys, repo_id: &str) -> Self {
            let mut process = Self {
                agent: project::AgentIdentity::new(&agent.public_key()).unwrap(),
                agent_owner: agent_owner.public_key().to_hex(),
                humans: std::collections::BTreeSet::new(),
                externals: std::collections::BTreeSet::new(),
                discovered: project::DiscoveredRepositories::new(),
                enrolments: project::ProjectEnrolments::new(),
                queue: EventQueue::new(config::DedupMode::Queue),
                ledger: peer_call::CallLedger::new(),
            };
            let announcement = proven_announcement(repo_owner, repo_id).await;
            handle_project_event(
                &mut dispatch_over(
                    &process.agent,
                    Some(&process.agent_owner),
                    &process.humans,
                    &process.externals,
                    &mut process.discovered,
                    &mut process.enrolments,
                    &mut process.queue,
                    &mut process.ledger,
                ),
                &project::ProjectEvent::Discovery { announcement },
            );
            process
        }

        /// Hand one signed event to the production dispatch, on the state every
        /// prior event left.
        async fn deliver(
            &mut self,
            source: project::ProjectSubscription,
            mode: project::ProcessingMode,
            event: nostr::Event,
        ) -> ProjectDispatched {
            let verified = project::VerifiedProjectEvent::verify(event)
                .await
                .expect("valid");
            let route = project::ProjectRoute::derive(&verified).expect("routes");
            handle_project_event(
                &mut dispatch_over(
                    &self.agent,
                    Some(&self.agent_owner),
                    &self.humans,
                    &self.externals,
                    &mut self.discovered,
                    &mut self.enrolments,
                    &mut self.queue,
                    &mut self.ledger,
                ),
                &project::ProjectEvent::Routed {
                    source,
                    route,
                    event: verified,
                    mode,
                },
            )
        }

        /// The ordinary live delivery: the watched-root REQ this agent's own
        /// enrolment installed.
        async fn watched(&mut self, event: nostr::Event) -> ProjectDispatched {
            self.deliver(
                project::ProjectSubscription::Watched { generation: 0 },
                project::ProcessingMode::Live,
                event,
            )
            .await
        }

        fn root_state(&self, root: &nostr::Event) -> project::RootState {
            self.enrolments.state_of(&root.id.to_hex())
        }
    }

    /// A signed status event on `root`, from whoever `actor` is.
    fn status_event(
        actor: &Keys,
        repo_owner: &Keys,
        agent: &Keys,
        repo_id: &str,
        root: &nostr::Event,
        kind: u32,
    ) -> nostr::Event {
        let coord = format!("30617:{}:{repo_id}", repo_owner.public_key().to_hex());
        EventBuilder::new(nostr::Kind::Custom(kind as u16), "")
            .tags([
                nostr::Tag::parse(["a", &coord]).unwrap(),
                nostr::Tag::parse(["e", &root.id.to_hex(), "", "root"]).unwrap(),
                nostr::Tag::parse(["p", &agent.public_key().to_hex()]).unwrap(),
            ])
            .sign_with_keys(actor)
            .expect("sign")
    }

    /// A follow-up comment carrying the agent's inherited `p` tag.
    ///
    /// Deliberately **not** a fresh explicit mention. Desktop copies prior
    /// participants into every later comment, so this is the shape an ordinary
    /// reply has — and the shape a closed root must not answer. A comment that
    /// re-mentions the agent by name is a genuine re-tag and reactivates a
    /// dormant root by design (`wake_or_enrol`), which is a different rule.
    fn follow_up_comment(
        author: &Keys,
        repo_owner: &Keys,
        agent: &Keys,
        repo_id: &str,
        root: &nostr::Event,
        body: &str,
    ) -> nostr::Event {
        let coord = format!("30617:{}:{repo_id}", repo_owner.public_key().to_hex());
        EventBuilder::new(nostr::Kind::TextNote, body)
            .tags([
                nostr::Tag::parse(["a", &coord]).unwrap(),
                nostr::Tag::parse(["e", &root.id.to_hex(), "", "root"]).unwrap(),
                nostr::Tag::parse(["p", &agent.public_key().to_hex()]).unwrap(),
            ])
            .sign_with_keys(author)
            .expect("sign")
    }

    /// A root opened by `author` on `repo_owner`'s repository.
    ///
    /// [`root_event`] signs with the repository owner, which is the common
    /// case and the one that cannot distinguish root-author authority from
    /// owner authority.
    fn root_event_by(
        author: &Keys,
        repo_owner: &Keys,
        agent: &Keys,
        repo_id: &str,
        kind: u32,
        body: &str,
    ) -> nostr::Event {
        let coord = format!("30617:{}:{repo_id}", repo_owner.public_key().to_hex());
        EventBuilder::new(
            nostr::Kind::Custom(kind as u16),
            addressed_body(agent, body),
        )
        .tags([
            nostr::Tag::parse(["a", &coord]).unwrap(),
            nostr::Tag::parse(["p", &agent.public_key().to_hex()]).unwrap(),
        ])
        .sign_with_keys(author)
        .expect("sign")
    }

    /// **The reported blocker, as one ordered scenario.**
    ///
    /// A valid owner close did not suspend an enrolled root: lifecycle
    /// authority was derived from `validate_enrolment_candidate`, which accepts
    /// only root kinds, so a `1632` could never be authorised — and the effect
    /// it would have produced had no application in the dispatcher either. The
    /// close was logged `effect=Ignore`, the watch stayed active, and the next
    /// comment ran a turn on a closed issue.
    ///
    /// Every step here is a signed event through the production entry, on the
    /// state the step before it left. Asserting the four rules separately, each
    /// against a freshly built enrolment set, is exactly the test that passed
    /// while production was broken.
    #[tokio::test]
    async fn a_close_suspends_a_watched_root_and_a_reopen_restores_it() {
        let owner = Keys::generate();
        let agent = Keys::generate();
        let mut process = ProjectProcess::new(&agent, &owner, &owner, "demo").await;

        // ── the root is watched ──────────────────────────────────────────────
        let root = root_event(
            &owner,
            &agent,
            "demo",
            buzz_core::kind::KIND_GIT_ISSUE,
            "please look",
        );
        let enrolled = process
            .deliver(
                project::ProjectSubscription::Enrolment,
                project::ProcessingMode::Live,
                root.clone(),
            )
            .await;
        assert!(
            matches!(enrolled, ProjectDispatched::Queued { queued: true, .. }),
            "precondition: an addressed root enrols and wakes — got {enrolled:?}"
        );
        assert_eq!(process.root_state(&root), project::RootState::Active);

        // ── 1. the owner closes it ───────────────────────────────────────────
        let closed = process
            .watched(status_event(
                &owner,
                &owner,
                &agent,
                "demo",
                &root,
                buzz_core::kind::KIND_GIT_STATUS_CLOSED,
            ))
            .await;
        assert_eq!(
            closed,
            ProjectDispatched::LifecycleApplied {
                root_state: project::RootState::Dormant
            },
            "an owner's close must suspend the watch, not be ignored"
        );

        // ── 2. …and a comment on it answers nothing ──────────────────────────
        let while_closed = process
            .watched(follow_up_comment(
                &owner,
                &owner,
                &agent,
                "demo",
                &root,
                "CLOSED-COMMENT-MUST-NOT-WAKE",
            ))
            .await;
        assert!(
            !matches!(while_closed, ProjectDispatched::Queued { queued: true, .. }),
            "a closed root must not run a turn — got {while_closed:?}"
        );
        assert_eq!(
            process.root_state(&root),
            project::RootState::Dormant,
            "and a comment must not quietly revive it"
        );

        // ── 3. the owner reopens it ──────────────────────────────────────────
        let reopened = process
            .watched(status_event(
                &owner,
                &owner,
                &agent,
                "demo",
                &root,
                buzz_core::kind::KIND_GIT_STATUS_OPEN,
            ))
            .await;
        assert_eq!(
            reopened,
            ProjectDispatched::LifecycleApplied {
                root_state: project::RootState::Active
            },
            "an authorised reopen must restore the watch"
        );

        // ── 4. …and the next addressed comment runs exactly one turn ─────────
        //
        // Addressed, because the watch alone no longer buys a turn: under the
        // target-only rule a reopened root wakes for a comment that names this
        // agent and stays quiet for one that does not.
        let after_reopen = process
            .watched(follow_up_comment(
                &owner,
                &owner,
                &agent,
                "demo",
                &root,
                &format!("@{} and now?", agent.public_key().to_hex()),
            ))
            .await;
        assert!(
            matches!(after_reopen, ProjectDispatched::Queued { queued: true, .. }),
            "a reopened root answers again — got {after_reopen:?}"
        );
    }

    /// An unauthorised signer cannot move a watch in either direction.
    ///
    /// Both directions, because they fail differently: an unauthorised *close*
    /// that succeeded would silence a live conversation, and an unauthorised
    /// *reopen* that succeeded would reanimate one the owner ended. A stranger
    /// who can publish to the relay can publish either.
    #[tokio::test]
    async fn an_unauthorised_actor_cannot_close_or_reopen() {
        let owner = Keys::generate();
        let agent = Keys::generate();
        let stranger = Keys::generate();
        let mut process = ProjectProcess::new(&agent, &owner, &owner, "demo").await;

        let root = root_event(
            &owner,
            &agent,
            "demo",
            buzz_core::kind::KIND_GIT_ISSUE,
            "please look",
        );
        process
            .deliver(
                project::ProjectSubscription::Enrolment,
                project::ProcessingMode::Live,
                root.clone(),
            )
            .await;

        let refused = process
            .watched(status_event(
                &stranger,
                &owner,
                &agent,
                "demo",
                &root,
                buzz_core::kind::KIND_GIT_STATUS_CLOSED,
            ))
            .await;
        assert_eq!(
            refused,
            ProjectDispatched::Ignored,
            "a stranger's close must change nothing"
        );
        assert_eq!(
            process.root_state(&root),
            project::RootState::Active,
            "and the watch must be exactly where the owner left it"
        );

        // Now genuinely closed, so the reopen has something to reanimate.
        process
            .watched(status_event(
                &owner,
                &owner,
                &agent,
                "demo",
                &root,
                buzz_core::kind::KIND_GIT_STATUS_CLOSED,
            ))
            .await;
        let refused = process
            .watched(status_event(
                &stranger,
                &owner,
                &agent,
                "demo",
                &root,
                buzz_core::kind::KIND_GIT_STATUS_OPEN,
            ))
            .await;
        assert_eq!(
            refused,
            ProjectDispatched::Ignored,
            "a stranger's reopen must change nothing"
        );
        assert_eq!(
            process.root_state(&root),
            project::RootState::Dormant,
            "the root the owner closed stays closed"
        );
    }

    /// The root's author may close it, on a repository somebody else owns.
    ///
    /// This is the case the stored binding exists for. The version this
    /// replaces passed the *closing event's* author as the root author, which
    /// made "author" unfalsifiable — anyone signing a close was the author of
    /// the root they were closing. Here the root's author and the repository's
    /// owner are two different keys and only one of them opened the issue, so
    /// an implementation that reads the wrong one refuses a legitimate close.
    #[tokio::test]
    async fn the_stored_root_author_may_close_a_root_on_anothers_repository() {
        let repo_owner = Keys::generate();
        let human = Keys::generate();
        let agent = Keys::generate();
        let mut process = ProjectProcess::new(&agent, &human, &repo_owner, "demo").await;

        // Opened by the agent's own human, on the repository owner's project.
        let root = root_event_by(
            &human,
            &repo_owner,
            &agent,
            "demo",
            buzz_core::kind::KIND_GIT_ISSUE,
            "please look",
        );
        let enrolled = process
            .deliver(
                project::ProjectSubscription::Enrolment,
                project::ProcessingMode::Live,
                root.clone(),
            )
            .await;
        assert!(
            matches!(enrolled, ProjectDispatched::Queued { queued: true, .. }),
            "precondition: the root enrols — got {enrolled:?}"
        );

        let closed = process
            .watched(status_event(
                &human,
                &repo_owner,
                &agent,
                "demo",
                &root,
                buzz_core::kind::KIND_GIT_STATUS_CLOSED,
            ))
            .await;
        assert_eq!(
            closed,
            ProjectDispatched::LifecycleApplied {
                root_state: project::RootState::Dormant
            },
            "the author of the root may close it without owning the repository"
        );
    }

    /// A reconstructed root carries its author into lifecycle authority.
    ///
    /// The restart case. A root restored from history enrols under
    /// `ProcessingMode::Replay` and runs no turn — and if that path dropped the
    /// root's author, every root an agent recovered after a restart would be
    /// permanently unclosable by the person who opened it. The binding has to
    /// survive the reconstruction, not merely the live delivery.
    #[tokio::test]
    async fn a_reconstructed_roots_author_can_still_close_it() {
        let repo_owner = Keys::generate();
        let human = Keys::generate();
        let agent = Keys::generate();
        let mut process = ProjectProcess::new(&agent, &human, &repo_owner, "demo").await;

        let root = root_event_by(
            &human,
            &repo_owner,
            &agent,
            "demo",
            buzz_core::kind::KIND_GIT_ISSUE,
            "please look",
        );
        let restored = process
            .deliver(
                project::ProjectSubscription::EnrolmentHistory { generation: 0 },
                project::ProcessingMode::Replay,
                root.clone(),
            )
            .await;
        assert!(
            matches!(restored, ProjectDispatched::Enrolled),
            "precondition: history restores the watch without a turn — got {restored:?}"
        );

        let closed = process
            .watched(status_event(
                &human,
                &repo_owner,
                &agent,
                "demo",
                &root,
                buzz_core::kind::KIND_GIT_STATUS_CLOSED,
            ))
            .await;
        assert_eq!(
            closed,
            ProjectDispatched::LifecycleApplied {
                root_state: project::RootState::Dormant
            },
            "a root recovered from history must still know who opened it"
        );
    }

    /// A lifecycle event on a root nothing has enrolled is refused.
    ///
    /// Fail-closed on the unbound case. There is no stored binding to
    /// authorise against, and the owner-signed shape of the event is not a
    /// substitute for one: authorising it would let a status event on an
    /// unknown root reach the enrolment sets at all.
    #[tokio::test]
    async fn a_lifecycle_event_on_an_unwatched_root_is_refused() {
        let owner = Keys::generate();
        let agent = Keys::generate();
        let mut process = ProjectProcess::new(&agent, &owner, &owner, "demo").await;

        // Signed by the owner, well-formed, and naming a root this process
        // never enrolled.
        let root = root_event(
            &owner,
            &agent,
            "demo",
            buzz_core::kind::KIND_GIT_ISSUE,
            "never delivered",
        );
        let refused = process
            .watched(status_event(
                &owner,
                &owner,
                &agent,
                "demo",
                &root,
                buzz_core::kind::KIND_GIT_STATUS_CLOSED,
            ))
            .await;
        assert_eq!(
            refused,
            ProjectDispatched::Ignored,
            "no binding, no authority — got {refused:?}"
        );
        assert_eq!(process.root_state(&root), project::RootState::Unknown);
    }

    /// A close that names neither the agent nor the repository still applies.
    ///
    /// The shape `lifecycle_actor_allowed`'s own documentation is about:
    /// `GitStatusMeta.repo` is optional, so an owner-signed close carrying only
    /// its `e` root marker is well-formed, and it arrives on the watched-root
    /// REQ because that REQ selects by `#e`. Nothing about it says who the
    /// agent is or which repository it belongs to — the stored binding says
    /// both. An implementation that reached for the event's own `a` tag, or
    /// that required a `p`, would refuse the ordinary Desktop close.
    #[tokio::test]
    async fn a_close_carrying_only_its_root_marker_still_suspends_the_watch() {
        let owner = Keys::generate();
        let agent = Keys::generate();
        let mut process = ProjectProcess::new(&agent, &owner, &owner, "demo").await;

        let root = root_event(
            &owner,
            &agent,
            "demo",
            buzz_core::kind::KIND_GIT_ISSUE,
            "please look",
        );
        process
            .deliver(
                project::ProjectSubscription::Enrolment,
                project::ProcessingMode::Live,
                root.clone(),
            )
            .await;

        let bare_close = EventBuilder::new(
            nostr::Kind::Custom(buzz_core::kind::KIND_GIT_STATUS_CLOSED as u16),
            "",
        )
        .tags([nostr::Tag::parse(["e", &root.id.to_hex(), "", "root"]).unwrap()])
        .sign_with_keys(&owner)
        .expect("sign");

        let closed = process.watched(bare_close).await;
        assert_eq!(
            closed,
            ProjectDispatched::LifecycleApplied {
                root_state: project::RootState::Dormant
            },
            "an owner's close needs no `a` and no `p` — the binding supplies both"
        );
    }

    /// A merged pull request is finished work; a draft one is not.
    ///
    /// The two kinds beside close and reopen. Both are authorised identically,
    /// so what this pins is the mapping — and the mapping is a judgement, not a
    /// tautology: merged suspends because answering on a merged branch is
    /// answering about work that no longer exists, and draft stays active
    /// because a pull request moved back to draft is unfinished rather than
    /// concluded.
    #[tokio::test]
    async fn merged_suspends_a_pull_request_and_draft_leaves_it_active() {
        let owner = Keys::generate();
        let agent = Keys::generate();
        let mut process = ProjectProcess::new(&agent, &owner, &owner, "demo").await;

        let root = root_event(
            &owner,
            &agent,
            "demo",
            buzz_core::kind::KIND_GIT_PULL_REQUEST,
            "review please",
        );
        process
            .deliver(
                project::ProjectSubscription::Enrolment,
                project::ProcessingMode::Live,
                root.clone(),
            )
            .await;

        let merged = process
            .watched(status_event(
                &owner,
                &owner,
                &agent,
                "demo",
                &root,
                buzz_core::kind::KIND_GIT_STATUS_MERGED,
            ))
            .await;
        assert_eq!(
            merged,
            ProjectDispatched::LifecycleApplied {
                root_state: project::RootState::Dormant
            },
            "a merged pull request is concluded"
        );

        let draft = process
            .watched(status_event(
                &owner,
                &owner,
                &agent,
                "demo",
                &root,
                buzz_core::kind::KIND_GIT_STATUS_DRAFT,
            ))
            .await;
        assert_eq!(
            draft,
            ProjectDispatched::LifecycleApplied {
                root_state: project::RootState::Active
            },
            "a draft pull request is unfinished, not finished"
        );
    }

    /// A root replayed from the enrolment backlog restores authority and wakes
    /// nobody.
    ///
    /// The restart case: an issue addressed to this agent before it started.
    /// Without reconstruction the agent holds no authority for its own
    /// conversations and correctly refuses everything referring to them; with
    /// reconstruction but no replay mode it re-answers every one of them. The
    /// stamp is what separates those two outcomes.
    #[tokio::test]
    async fn a_replayed_root_enrols_without_running_a_turn() {
        let owner = Keys::generate();
        let agent = Keys::generate();
        let root = root_event(
            &owner,
            &agent,
            "demo",
            buzz_core::kind::KIND_GIT_ISSUE,
            "look",
        );

        let live = dispatch_routed(
            &owner,
            &agent,
            "demo",
            project::ProjectSubscription::Enrolment,
            project::ProcessingMode::Live,
            None,
            root.clone(),
        )
        .await;
        assert!(
            matches!(live, ProjectDispatched::Queued { queued: true, .. }),
            "precondition: the same root live is a turn — got {live:?}"
        );

        let replayed = dispatch_routed(
            &owner,
            &agent,
            "demo",
            project::ProjectSubscription::EnrolmentHistory { generation: 0 },
            project::ProcessingMode::Replay,
            None,
            root,
        )
        .await;
        assert!(
            matches!(replayed, ProjectDispatched::Enrolled),
            "history must restore authority and queue nothing — got {replayed:?}"
        );
    }

    /// A replayed comment refreshes context; it does not answer.
    #[tokio::test]
    async fn a_replayed_comment_is_context_not_a_prompt() {
        let owner = Keys::generate();
        let agent = Keys::generate();
        let root = root_event(
            &owner,
            &agent,
            "demo",
            buzz_core::kind::KIND_GIT_ISSUE,
            "look",
        );
        let comment = EventBuilder::new(nostr::Kind::TextNote, "and again?")
            .tags([
                nostr::Tag::parse(["a", &format!("30617:{}:demo", owner.public_key().to_hex())])
                    .unwrap(),
                nostr::Tag::parse(["e", &root.id.to_hex(), "", "root"]).unwrap(),
                nostr::Tag::parse(["p", &agent.public_key().to_hex()]).unwrap(),
            ])
            .sign_with_keys(&owner)
            .expect("sign");

        let replayed = dispatch_routed(
            &owner,
            &agent,
            "demo",
            project::ProjectSubscription::EnrolmentHistory { generation: 0 },
            project::ProcessingMode::Replay,
            Some(root),
            comment,
        )
        .await;
        assert!(
            !matches!(replayed, ProjectDispatched::Queued { queued: true, .. }),
            "a replayed comment must not run a turn — got {replayed:?}"
        );
    }

    /// The mode is carried by the frame, and the dispatcher obeys it.
    ///
    /// This replaces a `processing_mode_for(source)` table test. That table was
    /// the defect, not a description of it: the enrolment class covers both a
    /// tail's stored-events prefix and everything live after its boundary, so
    /// no function of the class alone can be right about both — and the version
    /// that existed answered "live" for the whole class, which is how a stored
    /// root re-answered and, in the other direction, why the fix could not be
    /// expressed at all.
    ///
    /// So the assertion moved to where the value now lives. The *same* class,
    /// the *same* signed root, dispatched twice against fresh state, differing
    /// only in the mode the frame carries: one enrols and wakes, the other
    /// enrols and does not. A mode the dispatcher ignored would make these two
    /// equal, and that is the whole point of the comparison.
    #[tokio::test]
    async fn the_frames_mode_decides_whether_a_root_wakes_anyone() {
        let owner = Keys::generate();
        let agent = Keys::generate();
        let repo = "mode-carried-by-the-frame";
        let root = root_event(
            &owner,
            &agent,
            repo,
            buzz_core::kind::KIND_GIT_ISSUE,
            "please look",
        );

        let live = dispatch_routed(
            &owner,
            &agent,
            repo,
            project::ProjectSubscription::Enrolment,
            project::ProcessingMode::Live,
            None,
            root.clone(),
        )
        .await;
        let replayed = dispatch_routed(
            &owner,
            &agent,
            repo,
            project::ProjectSubscription::Enrolment,
            project::ProcessingMode::Replay,
            None,
            root,
        )
        .await;

        assert!(
            matches!(live, ProjectDispatched::Queued { .. }),
            "a live root is work: {live:?}"
        );
        assert!(
            matches!(replayed, ProjectDispatched::Enrolled),
            "the same root as history restores the watch and wakes nobody: {replayed:?}"
        );
    }

    /// A root that actually addresses `agent`: it names them, and carries their
    /// `p` behind the name.
    ///
    /// The `p` alone is not an address. Desktop stamps the repository owner
    /// onto every root it creates, so on an agent-owned project a bare `p` says
    /// only that the client knows who owns the repo. Every fixture built on
    /// this one is about admission, lifecycle, dedup or subscription
    /// replacement rather than about addressing, so each says outright who its
    /// root is for — and [`unaddressed_root_event`] is the other case.
    fn root_event(
        owner: &Keys,
        agent: &Keys,
        repo_id: &str,
        kind: u32,
        body: &str,
    ) -> nostr::Event {
        signed_root(owner, agent, repo_id, kind, &addressed_body(agent, body))
    }

    /// The same root with the agent's `p` tag and **nothing naming them** — the
    /// shape Buzz Desktop publishes for a root opened on a repository this
    /// agent owns, and the shape that must now wake nobody.
    fn unaddressed_root_event(
        owner: &Keys,
        agent: &Keys,
        repo_id: &str,
        kind: u32,
        body: &str,
    ) -> nostr::Event {
        signed_root(owner, agent, repo_id, kind, body)
    }

    /// The mention text a person writes when they hand a root to an agent.
    fn addressed_body(agent: &Keys, body: &str) -> String {
        use nostr::ToBech32;
        format!(
            "nostr:{} {body}",
            agent.public_key().to_bech32().expect("npub")
        )
    }

    /// The tag set both fixtures share: the repository coordinate and the
    /// agent's `p`. What differs between them is only the content.
    fn signed_root(
        owner: &Keys,
        agent: &Keys,
        repo_id: &str,
        kind: u32,
        content: &str,
    ) -> nostr::Event {
        let coord = format!("30617:{}:{repo_id}", owner.public_key().to_hex());
        EventBuilder::new(nostr::Kind::Custom(kind as u16), content)
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
                mode: project::ProcessingMode::Live,
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
                mode: project::ProcessingMode::Live,
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
                mode: project::ProcessingMode::Live,
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
                mode: project::ProcessingMode::Live,
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
                mode: project::ProcessingMode::Live,
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
                mode: project::ProcessingMode::Live,
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
                mode: project::ProcessingMode::Live,
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
                    mode: project::ProcessingMode::Live,
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
                    mode: project::ProcessingMode::Live,
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
                mode: project::ProcessingMode::Live,
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
                mode: project::ProcessingMode::Live,
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
                mode: project::ProcessingMode::Live,
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
        /// Every enrolment-history walk asked for, in order.
        history: std::sync::Mutex<Vec<(Vec<String>, String)>>,
        /// Every root whose own history was asked for, in order.
        catch_ups: std::sync::Mutex<Vec<String>>,
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

        async fn submit_enrolment_history(
            &self,
            coordinates: Vec<String>,
            agent: String,
        ) -> Result<(), relay::RelayError> {
            self.history.lock().unwrap().push((coordinates, agent));
            Ok(())
        }

        async fn submit_root_catch_up(
            &self,
            root: project::VerifiedBoundRoot,
        ) -> Result<(), relay::RelayError> {
            self.catch_ups
                .lock()
                .unwrap()
                .push(root.binding().root().to_string());
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
        let mut seen = ProjectSeenIds::new();
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
                &mut seen,
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

    /// A root that predates this process asks for its history — however it
    /// arrived.
    ///
    /// The producer half of the restart defect. The catch-up machinery existed
    /// complete — exhaustive paging, generation isolation, fail-closed
    /// degradation, deterministic merge — and nothing in production ever
    /// started one: `ProjectReconstructions::insert` had no caller, so a
    /// restart rebuilt every binding as active and no close was ever fetched.
    ///
    /// **Which roots ask is as load-bearing as the asking**, and the obvious
    /// answer is subtly wrong. "Enrolled from replay" reads as "restored", and
    /// it is nearly right — but it is a proxy for "this process did not watch
    /// this root happen", and the two REQs do not meet where the proxy assumes.
    /// `enrolment_filter` reaches the live root tail back by
    /// [`project::ACCEPTED_CLOCK_SKEW_SECS`] to tolerate drift;
    /// `watched_roots_filters` starts at the watermark exactly. A root
    /// published inside that window therefore arrives **live** and enrols live,
    /// while a close published between it and startup falls into the gap
    /// between the two: after the enrolment walk's cutoff, before the watched
    /// REQ's floor. Keyed on the mode, nothing would ever fetch it — the same
    /// silent dormancy loss, one window narrower.
    ///
    /// So the discriminator is the root's own `created_at` against the
    /// watermark, and this pins all three cases against one watermark: restored
    /// by replay, arrived live from inside the skew window, and created after
    /// startup with no history to miss.
    #[tokio::test]
    async fn a_root_that_predates_this_process_asks_for_its_history_however_it_arrived() {
        const WATERMARK: u64 = 1_785_743_469;

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
        let mut seen = ProjectSeenIds::new();
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
                    &mut seen,
                    &subscriber,
                    &agent_hex,
                    WATERMARK,
                    $ev,
                )
                .await
            };
        }

        drive!(&project::ProjectEvent::Discovery {
            announcement: proven_announcement(&owner, "proj").await,
        });

        // A root at `t`, addressed to the agent on the discovered coordinate.
        let root_at = |body: &str, at: u64| {
            let coord = format!("30617:{}:proj", owner.public_key().to_hex());
            EventBuilder::new(
                nostr::Kind::Custom(buzz_core::kind::KIND_GIT_ISSUE as u16),
                addressed_body(&agent, body),
            )
            .custom_created_at(nostr::Timestamp::from(at))
            .tags([
                nostr::Tag::parse(["a", &coord]).unwrap(),
                nostr::Tag::parse(["p", &agent.public_key().to_hex()]).unwrap(),
            ])
            .sign_with_keys(&owner)
            .expect("sign")
        };

        // Restored by the enrolment walk: two hours old, far outside any tail.
        let restored = root_at("opened before we existed", WATERMARK - 7_200);
        // Inside the skew window the live root tail reaches back over. This one
        // arrives **live** and is the case a mode-keyed producer misses.
        let skewed = root_at(
            "opened just before we started",
            WATERMARK - project::ACCEPTED_CLOCK_SKEW_SECS / 2,
        );
        // Genuinely live: created after this process was already watching, so
        // there is no history it did not see.
        let fresh = root_at("opened while we watched", WATERMARK + 60);

        let restored_id = restored.id.to_hex();
        let skewed_id = skewed.id.to_hex();

        for (event, mode, source) in [
            (
                restored,
                project::ProcessingMode::Replay,
                project::ProjectSubscription::EnrolmentHistory { generation: 0 },
            ),
            (
                skewed,
                project::ProcessingMode::Live,
                project::ProjectSubscription::Enrolment,
            ),
            (
                fresh,
                project::ProcessingMode::Live,
                project::ProjectSubscription::Enrolment,
            ),
        ] {
            let verified = project::VerifiedProjectEvent::verify(event)
                .await
                .expect("valid");
            let route = project::ProjectRoute::derive(&verified).expect("routes");
            drive!(&project::ProjectEvent::Routed {
                source,
                route,
                event: verified,
                mode,
            });
        }

        let catch_ups = subscriber.catch_ups.lock().unwrap().clone();
        assert_eq!(
            catch_ups,
            vec![restored_id, skewed_id],
            "every root older than the watermark asks for its history, and only \
             those: {catch_ups:?}"
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
        let mut seen = ProjectSeenIds::new();
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
                    &mut seen,
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
                mode: project::ProcessingMode::Live,
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

    /// The plan's definition of done: a person opens an issue, **names** an
    /// agent, the agent wakes — with no reconstructed history anywhere.
    ///
    /// There is no history preceding a root to reconstruct, so if enrolment
    /// needed one this path would be unreachable rather than merely slow. What
    /// makes it reachable is the mention, not the `p`: `root_event` writes both.
    #[tokio::test]
    async fn an_issue_root_that_names_the_agent_wakes_without_any_history() {
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
            project::ProcessingMode::Live,
            None,
            event,
        )
        .await;

        assert!(
            matches!(dispatched, ProjectDispatched::Queued { queued: true, .. }),
            "an issue root naming the agent must queue exactly one turn, got {dispatched:?}"
        );
    }

    /// Same rule, other root kind — the mention must not be spelled for issues
    /// alone.
    #[tokio::test]
    async fn a_pull_request_root_that_names_the_agent_wakes_without_any_history() {
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
            project::ProcessingMode::Live,
            None,
            event,
        )
        .await;

        assert!(
            matches!(dispatched, ProjectDispatched::Queued { queued: true, .. }),
            "a PR root naming the agent must queue exactly one turn, got {dispatched:?}"
        );
    }

    /// The other half, and the reported failure: the same root **without** the
    /// mention.
    ///
    /// A `p` and nothing else used to be enough, on the argument that a root
    /// has no predecessor and so cannot have inherited its tag. Desktop does
    /// not need a predecessor to write a `p` — it stamps the repository owner
    /// onto every root it creates — so on an agent-owned project that argument
    /// turned every issue anybody opened into an address, and the agent
    /// answered issues whose entire content was `test`.
    ///
    /// Both root kinds, because the exception was not spelled for issues alone.
    #[tokio::test]
    async fn a_root_carrying_only_a_bare_p_tag_wakes_nobody() {
        for kind in [
            buzz_core::kind::KIND_GIT_ISSUE,
            buzz_core::kind::KIND_GIT_PULL_REQUEST,
        ] {
            let owner = Keys::generate();
            let agent = Keys::generate();
            let event = unaddressed_root_event(&owner, &agent, "proj", kind, "test");

            let dispatched = dispatch_routed(
                &owner,
                &agent,
                "proj",
                project::ProjectSubscription::Enrolment,
                project::ProcessingMode::Live,
                None,
                event,
            )
            .await;

            assert_eq!(
                dispatched,
                ProjectDispatched::Ignored,
                "kind {kind}: a `p` the client wrote by itself must not enrol \
                 or wake — got {dispatched:?}"
            );
        }
    }

    /// The comment half of the same rule.
    ///
    /// A comment *can* carry a `p` copied from an earlier participant list, and
    /// without complete history nothing can tell the difference — so it cannot
    /// bring an unwatched root into the active set. Roots now answer the same
    /// way for the same reason, which is what
    /// `a_root_carrying_only_a_bare_p_tag_wakes_nobody` above asserts; this one
    /// is the case that was always true and must stay true.
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
            project::ProcessingMode::Live,
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

    /// Visible mention syntax is the evidence the primitive trusts without any
    /// history, and it is now the *only* evidence that enrols — on a comment or
    /// on a root.
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
            project::ProcessingMode::Live,
            Some(root),
            comment,
        )
        .await;

        assert!(
            matches!(dispatched, ProjectDispatched::Queued { .. }),
            "visible mention syntax still wakes a comment, got {dispatched:?}"
        );
    }

    /// Addressing is one gate; authorship is another, and it is unchanged.
    /// A self-authored root is suppressed however well it addresses this agent,
    /// which is what stops an agent that opens an issue from waking itself.
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
            project::ProcessingMode::Live,
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
                mode: project::ProcessingMode::Live,
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
        assert!(prompt.contains("use the person's **exact display name as shown in Buzz**"));
        assert!(prompt.contains("Do not expand a short display name, infer a surname"));
        assert!(prompt.contains("Preserve it exactly; do not infer, expand, or look up a surname"));
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
    #[test]
    fn shared_base_prompt_teaches_repo_context_and_learning_loop() {
        let prompt = include_str!("base_prompt.md");
        assert!(prompt.contains("read its root `AGENTS.md`"));
        assert!(prompt.contains("path-local `AGENTS.md`"));
        assert!(
            prompt.contains("product, architecture, and vision documents as design constraints")
        );
        assert!(prompt.contains("CI and live workflow evidence answer different questions"));
        assert!(prompt.contains("record the invariant in the same session"));
        assert!(prompt.contains("update the team's shared guidance"));
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
    effort_level: Option<String>,
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
            effort_level: config.effort_level.clone(),
            observer,
        }
    }
}

async fn initialize_agent_pool(
    startup: &PoolStartup,
    mut shutdown: Option<watch::Receiver<()>>,
) -> Result<AgentPool, PoolStartError> {
    // One agent failing to start must not kill the whole pool.
    // Attempt each spawn under a 60-second timeout; a partial pool is valid.
    let mut agent_slots: Vec<Option<OwnedAgent>> = Vec::with_capacity(startup.agents as usize);
    // The first terminal authentication failure seen while initializing any
    // slot. Kept typed and separate from the partial-pool accounting: a slot
    // that failed because the credential expired is not a slot that might
    // succeed on retry, so if no slot came up we must say so precisely.
    let mut terminal_auth: Option<terminal_auth::TerminalAuth> = None;
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
                            return Err(PoolStartError::Transient(
                                "pool initialization cancelled by shutdown".into(),
                            ));
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
                            desired_model_request_id: None,
                            desired_model_pending_ack: false,
                            startup_effort: startup.effort_level.clone(),
                            agent_name,
                            goose_system_prompt_supported: None,
                            protocol_version,
                        }));
                    }
                    Ok(Err(e)) => {
                        if let acp::AcpError::TerminalAuth(terminal) = &e {
                            tracing::error!(
                                agent = i,
                                terminal = %terminal,
                                "agent initialize rejected our credentials"
                            );
                            terminal_auth.get_or_insert(*terminal);
                        } else {
                            tracing::error!(agent = i, "agent initialize failed: {e}");
                        }
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
        // Terminal auth wins over the generic message: a caller that retries
        // this on a five-second ladder would be retrying an expired
        // credential forever.
        if let Some(terminal) = terminal_auth {
            return Err(PoolStartError::TerminalAuth(terminal));
        }
        return Err(PoolStartError::Transient(format!(
            "all {} agents failed to start — cannot continue",
            startup.agents
        )));
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

/// `buzz-acp provider-probe` — one disposable tool-disabled turn.
///
/// Writes exactly one compact JSON object to stdout and nothing else, on every
/// path including failure. Diagnostics that would be useful to a human go to
/// stderr, never to stdout, so the desktop's strict parser never has to
/// tolerate trailing data.
///
/// The exit code mirrors the verdict (0 ready, 1 not ready) so a caller that
/// cannot parse JSON still gets a usable answer, but the JSON is authoritative.
async fn run_provider_probe(args: ProviderProbeArgs) -> Result<()> {
    let agent_args = config::normalize_agent_args(&args.agent.agent_command, args.agent.agent_args);
    let cwd = match args.cwd {
        Some(cwd) => cwd,
        None => std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("/")),
    };

    let report = provider_probe::run_probe(&args.agent.agent_command, &agent_args, &cwd).await;

    // `to_string` (not `to_string_pretty`): one compact object, one line, no
    // trailing data.
    println!("{}", serde_json::to_string(&report)?);
    if report.is_ready() {
        Ok(())
    } else {
        std::process::exit(1);
    }
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
    let cwd = current_working_directory()?;

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
        let standing = queue::StandingContext {
            base_prompt: Some("you are a helpful agent"),
            ..Default::default()
        };
        let composed = pool::prepend_standing_for_legacy(1, &standing, prompt);
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
        let standing = queue::StandingContext {
            base_prompt: Some("you are a helpful agent"),
            ..Default::default()
        };
        let composed = pool::prepend_standing_for_legacy(2, &standing, prompt);
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
                successful_steer_deliveries: HashSet::new(),
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

/// The drain control frame, exercised through the real envelope.
///
/// Every case here builds a signed `24200` with `buzz_sdk` and hands it to
/// [`handle_relay_observer_control_event`] — the same function the run loop
/// calls, with the same signature, owner and freshness checks in front of it.
/// Nothing is asserted against a hand-rolled payload struct, because the thing
/// under test is precisely that a frame a deployer can actually build is
/// accepted, and one an attacker can build is not.
#[cfg(test)]
mod drain_control_tests {
    use buzz_core::observer::{encrypt_observer_payload, OBSERVER_FRAME_CONTROL};
    use nostr::Keys;

    use super::*;

    /// The bound is irrelevant to acceptance; a short one keeps the assertions
    /// about deadlines readable.
    const BOUND: Duration = Duration::from_secs(600);

    /// A control frame as a sender must build it: NIP-44 to the agent, signed
    /// by the sender's key, `frame=control`, both pubkey tags naming the agent.
    ///
    /// `sender` is a parameter rather than "the owner" so the non-owner case
    /// exercises the identical construction — a refusal that only worked
    /// because the attacker's frame was malformed would prove nothing.
    fn control_frame(
        sender: &Keys,
        agent: &Keys,
        payload: serde_json::Value,
        created_at: Option<nostr::Timestamp>,
    ) -> nostr::Event {
        let agent_hex = agent.public_key().to_hex();
        let encrypted = encrypt_observer_payload(sender, &agent.public_key(), &payload)
            .expect("encrypt control payload");
        let builder = buzz_sdk::build_agent_observer_frame(
            &agent_hex,
            &agent_hex,
            OBSERVER_FRAME_CONTROL,
            &encrypted,
        )
        .expect("build control frame");
        let builder = match created_at {
            Some(ts) => builder.custom_created_at(ts),
            None => builder,
        };
        builder.sign_with_keys(sender).expect("sign control frame")
    }

    fn route(
        agent: &Keys,
        owner_hex: &str,
        event: nostr::Event,
        drain: &mut drain::DrainState,
    ) -> Option<drain::DrainOnset> {
        let mut pool = AgentPool::from_slots(vec![]);
        let mut queue = EventQueue::new(DedupMode::Queue);
        let (publisher, _published) = RelayEventPublisher::test_pair();
        handle_relay_observer_control_event(
            agent, event, &mut pool, &mut queue, None, owner_hex, drain, BOUND, publisher,
        )
    }

    #[tokio::test(start_paused = true)]
    async fn a_drain_frame_from_the_owner_closes_admission() {
        let owner = Keys::generate();
        let agent = Keys::generate();
        let mut drain = drain::DrainState::open();

        let onset = route(
            &agent,
            &owner.public_key().to_hex(),
            control_frame(&owner, &agent, serde_json::json!({"type": "drain"}), None),
            &mut drain,
        );

        assert_eq!(onset, Some(drain::DrainOnset::Started));
        assert!(!drain.admits_new_work());
        assert_eq!(
            drain.deadline(),
            Some(tokio::time::Instant::now() + BOUND),
            "the bound must start when the frame is honoured"
        );
    }

    /// The optional `reason` is accepted and changes nothing about the outcome.
    #[tokio::test(start_paused = true)]
    async fn a_drain_frame_may_carry_a_reason() {
        let owner = Keys::generate();
        let agent = Keys::generate();
        let mut drain = drain::DrainState::open();

        let onset = route(
            &agent,
            &owner.public_key().to_hex(),
            control_frame(
                &owner,
                &agent,
                serde_json::json!({"type": "drain", "reason": "binary swap"}),
                None,
            ),
            &mut drain,
        );

        assert_eq!(onset, Some(drain::DrainOnset::Started));
        assert!(drain.is_draining());
    }

    /// **Anyone but the owner may not stop this agent.** A correctly built,
    /// correctly encrypted, perfectly fresh frame from a stranger is refused on
    /// identity alone — drain is a denial-of-service primitive otherwise.
    #[tokio::test(start_paused = true)]
    async fn a_drain_frame_from_a_non_owner_is_dropped() {
        let owner = Keys::generate();
        let stranger = Keys::generate();
        let agent = Keys::generate();
        let mut drain = drain::DrainState::open();

        let onset = route(
            &agent,
            &owner.public_key().to_hex(),
            control_frame(
                &stranger,
                &agent,
                serde_json::json!({"type": "drain"}),
                None,
            ),
            &mut drain,
        );

        assert_eq!(onset, None);
        assert!(
            drain.admits_new_work(),
            "a stranger must not be able to take an agent out of service"
        );
    }

    /// Outside the freshness window, the owner's own frame is refused too —
    /// which is what stops a captured drain from being replayed at an
    /// attacker's chosen moment.
    #[tokio::test(start_paused = true)]
    async fn a_stale_drain_frame_is_dropped() {
        let owner = Keys::generate();
        let agent = Keys::generate();
        let mut drain = drain::DrainState::open();
        let stale = nostr::Timestamp::from(
            (chrono::Utc::now().timestamp() - OBSERVER_CONTROL_FRESHNESS_SECS - 60) as u64,
        );

        let onset = route(
            &agent,
            &owner.public_key().to_hex(),
            control_frame(
                &owner,
                &agent,
                serde_json::json!({"type": "drain"}),
                Some(stale),
            ),
            &mut drain,
        );

        assert_eq!(onset, None);
        assert!(drain.admits_new_work());
    }

    /// A frame encrypted to somebody else decrypts to nothing here and is
    /// logged and dropped — the pre-existing behaviour, asserted because drain
    /// now depends on it to refuse a frame it cannot read.
    #[tokio::test(start_paused = true)]
    async fn an_undecryptable_drain_frame_is_dropped() {
        let owner = Keys::generate();
        let agent = Keys::generate();
        let other = Keys::generate();
        let mut drain = drain::DrainState::open();

        // Built for `other`, delivered to `agent`.
        let frame = control_frame(&owner, &other, serde_json::json!({"type": "drain"}), None);
        let onset = route(&agent, &owner.public_key().to_hex(), frame, &mut drain);

        assert_eq!(onset, None);
        assert!(drain.admits_new_work());
    }

    /// **Replay inside the freshness window is harmless because drain is
    /// idempotent.** The second frame is acknowledged (`AlreadyDraining`) and
    /// changes nothing — in particular it does not buy the runtime a second
    /// bound, which is the one way an idempotent-looking operation could still
    /// be abused into holding a process open forever.
    #[tokio::test(start_paused = true)]
    async fn a_replayed_drain_frame_is_idempotent() {
        let owner = Keys::generate();
        let agent = Keys::generate();
        let owner_hex = owner.public_key().to_hex();
        let mut drain = drain::DrainState::open();
        let frame = control_frame(&owner, &agent, serde_json::json!({"type": "drain"}), None);

        assert_eq!(
            route(&agent, &owner_hex, frame.clone(), &mut drain),
            Some(drain::DrainOnset::Started)
        );
        let first_deadline = drain.deadline().expect("draining");

        tokio::time::advance(Duration::from_secs(120)).await;
        assert_eq!(
            route(&agent, &owner_hex, frame, &mut drain),
            Some(drain::DrainOnset::AlreadyDraining),
            "the very same signed event, delivered twice"
        );
        assert_eq!(
            drain.deadline(),
            Some(first_deadline),
            "a replay must not extend the drain"
        );
    }

    /// The skew-safety property, from the other side: a payload this binary
    /// does not know is ignored, and in particular does not drain. A future
    /// control type must be able to reach an old binary harmlessly, which is
    /// the same tolerance that lets a drain reach a binary that predates it.
    #[tokio::test(start_paused = true)]
    async fn an_unknown_control_type_does_not_drain() {
        let owner = Keys::generate();
        let agent = Keys::generate();
        let mut drain = drain::DrainState::open();

        let onset = route(
            &agent,
            &owner.public_key().to_hex(),
            control_frame(
                &owner,
                &agent,
                serde_json::json!({"type": "quiesce_forever"}),
                None,
            ),
            &mut drain,
        );

        assert_eq!(onset, None);
        assert!(drain.admits_new_work());
    }

    #[tokio::test]
    async fn cancel_all_marks_every_active_turn_discards_pending_and_keeps_admission_open() {
        let bus = observer::ObserverHandle::in_process();
        let mut rx = bus.subscribe();
        let mut pool = AgentPool::from_slots(vec![]);
        let mut receivers = Vec::new();
        for channel_id in [Uuid::new_v4(), Uuid::new_v4()] {
            let (tx, rx) = tokio::sync::oneshot::channel();
            let abort = pool.join_set.spawn(async {});
            pool.task_map_mut().insert(
                abort.id(),
                pool::TaskMeta {
                    agent_index: 0,
                    channel_id: Some(channel_id),
                    turn_id: Uuid::new_v4().to_string(),
                    recoverable_batch: None,
                    control_tx: Some(tx),
                    steer_tx: None,
                    successful_steer_deliveries: HashSet::new(),
                },
            );
            receivers.push(rx);
        }

        let owner = Keys::generate();
        let mut queue = EventQueue::new(DedupMode::Queue);
        for channel_id in [Uuid::new_v4(), Uuid::new_v4()] {
            assert!(queue.push(QueuedEvent {
                channel_id,
                event: nostr::EventBuilder::new(nostr::Kind::Custom(9), "pending")
                    .sign_with_keys(&owner)
                    .expect("sign"),
                received_at: std::time::Instant::now(),
                prompt_tag: "".into(),
                project: None,
            }));
        }

        let outcome = handle_cancel_all_control(&mut pool, &mut queue, Some(&bus));
        assert_eq!(outcome.active_turns, 2);
        assert_eq!(outcome.signalled_turns, 2);
        assert_eq!(outcome.queued_batches, 2);
        assert_eq!(outcome.queued_events, 2);
        for receiver in receivers {
            assert_eq!(
                receiver.await.expect("cancel signal"),
                ControlSignal::Cancel
            );
        }
        assert!(!queue.has_undrained_work());

        let future_channel = Uuid::new_v4();
        assert!(queue.push(QueuedEvent {
            channel_id: future_channel,
            event: nostr::EventBuilder::new(nostr::Kind::Custom(9), "future")
                .sign_with_keys(&owner)
                .expect("sign"),
            received_at: std::time::Instant::now(),
            prompt_tag: "".into(),
            project: None,
        }));
        assert_eq!(queue.queued_event_count(&future_channel), 1);

        let ack = rx.try_recv().expect("cancel_all acknowledgement");
        assert_eq!(ack.payload["type"], "cancel_all");
        assert_eq!(ack.payload["status"], "accepted");
        assert_eq!(ack.payload["activeTurns"], 2);
        assert_eq!(ack.payload["signalledTurns"], 2);
        assert_eq!(ack.payload["queuedEvents"], 2);
    }

    #[test]
    fn cancel_all_with_no_work_acknowledges_no_work() {
        let bus = observer::ObserverHandle::in_process();
        let mut rx = bus.subscribe();
        let mut pool = AgentPool::from_slots(vec![]);
        let mut queue = EventQueue::new(DedupMode::Queue);

        let outcome = handle_cancel_all_control(&mut pool, &mut queue, Some(&bus));
        assert_eq!(outcome.status(), "no_work");
        let ack = rx.try_recv().expect("no-work acknowledgement");
        assert_eq!(ack.payload["status"], "no_work");
        assert_eq!(ack.payload["activeTurns"], 0);
        assert_eq!(ack.payload["queuedEvents"], 0);
    }

    #[tokio::test]
    async fn cancel_all_uses_the_same_owner_and_freshness_gate() {
        let owner = Keys::generate();
        let stranger = Keys::generate();
        let agent = Keys::generate();
        let bus = observer::ObserverHandle::in_process();
        let mut rx = bus.subscribe();
        let mut pool = AgentPool::from_slots(vec![]);
        let mut queue = EventQueue::new(DedupMode::Queue);
        let mut drain = drain::DrainState::open();
        let stale = nostr::Timestamp::from(
            (chrono::Utc::now().timestamp() - OBSERVER_CONTROL_FRESHNESS_SECS - 60) as u64,
        );

        for frame in [
            control_frame(
                &stranger,
                &agent,
                serde_json::json!({"type": "cancel_all"}),
                None,
            ),
            control_frame(
                &owner,
                &agent,
                serde_json::json!({"type": "cancel_all"}),
                Some(stale),
            ),
        ] {
            let (publisher, _published) = RelayEventPublisher::test_pair();
            assert_eq!(
                handle_relay_observer_control_event(
                    &agent,
                    frame,
                    &mut pool,
                    &mut queue,
                    Some(&bus),
                    &owner.public_key().to_hex(),
                    &mut drain,
                    BOUND,
                    publisher,
                ),
                None
            );
        }
        assert!(
            rx.try_recv().is_err(),
            "rejected controls must not acknowledge"
        );
        assert!(drain.admits_new_work());
    }

    #[tokio::test]
    async fn owner_cancel_all_is_routed_and_acknowledged() {
        let owner = Keys::generate();
        let agent = Keys::generate();
        let bus = observer::ObserverHandle::in_process();
        let mut rx = bus.subscribe();
        let mut pool = AgentPool::from_slots(vec![]);
        let mut queue = EventQueue::new(DedupMode::Queue);
        let mut drain = drain::DrainState::open();
        let (publisher, _published) = RelayEventPublisher::test_pair();

        assert_eq!(
            handle_relay_observer_control_event(
                &agent,
                control_frame(
                    &owner,
                    &agent,
                    serde_json::json!({"type": "cancel_all"}),
                    None
                ),
                &mut pool,
                &mut queue,
                Some(&bus),
                &owner.public_key().to_hex(),
                &mut drain,
                BOUND,
                publisher,
            ),
            None
        );
        let ack = rx.try_recv().expect("owner cancel_all acknowledgement");
        assert_eq!(ack.payload["type"], "cancel_all");
        assert_eq!(ack.payload["status"], "no_work");
        assert!(drain.admits_new_work(), "cancel_all must not drain or exit");
    }

    #[tokio::test]
    async fn owner_announcement_control_does_not_mask_a_later_drain() {
        let owner = Keys::generate();
        let agent = Keys::generate();
        let owner_hex = owner.public_key().to_hex();
        let mut pool = AgentPool::from_slots(vec![]);
        let mut queue = EventQueue::new(DedupMode::Queue);
        let mut drain = drain::DrainState::open();
        let (publisher, _published) = RelayEventPublisher::test_pair();

        let announcement = control_frame(
            &owner,
            &agent,
            serde_json::json!({
                "type": "publish_project_owner_announcements",
                "requestId": "test-request",
                "announcements": [{
                    "kind": 30_621,
                    "content": "",
                    "tags": [["d", "test-project"]]
                }]
            }),
            None,
        );
        assert_eq!(
            handle_relay_observer_control_event(
                &agent,
                announcement,
                &mut pool,
                &mut queue,
                None,
                &owner_hex,
                &mut drain,
                BOUND,
                publisher,
            ),
            None
        );
        assert!(drain.admits_new_work());

        let onset = route(
            &agent,
            &owner_hex,
            control_frame(&owner, &agent, serde_json::json!({"type": "drain"}), None),
            &mut drain,
        );
        assert_eq!(onset, Some(drain::DrainOnset::Started));
        assert!(!drain.admits_new_work());
    }

    /// The owner is acknowledged on the observer bus, so a deployer can see the
    /// frame land rather than inferring it from the absence of new turns.
    #[tokio::test(start_paused = true)]
    async fn a_drain_is_acknowledged_on_the_observer_bus() {
        let bus = observer::ObserverHandle::in_process();
        let mut rx = bus.subscribe();
        let mut drain = drain::DrainState::open();

        for expected in ["draining", "already_draining"] {
            handle_drain_control(
                &serde_json::json!({"type": "drain", "reason": "swap"}),
                Some(&bus),
                &mut drain,
                BOUND,
                tokio::time::Instant::now(),
            );
            let event = rx.try_recv().expect("an acknowledgement per frame");
            assert_eq!(event.kind, "control_result");
            assert_eq!(event.payload["type"], "drain");
            assert_eq!(event.payload["status"], expected);
            assert_eq!(event.payload["reason"], "swap");
        }
    }
}

#[cfg(test)]
mod project_owner_announcement_control_tests {
    use super::*;
    use nostr::Keys;

    #[test]
    fn project_owner_control_signs_only_addressable_project_events() {
        let keys = Keys::generate();
        let events = build_project_owner_announcement_events(
            vec![
                ProjectOwnerAnnouncementTemplate {
                    kind: 30_621,
                    content: String::new(),
                    created_at: Some(1),
                    tags: vec![vec!["d".to_string(), "project".to_string()]],
                },
                ProjectOwnerAnnouncementTemplate {
                    kind: 30_617,
                    content: String::new(),
                    created_at: Some(1),
                    tags: vec![vec!["d".to_string(), "repository".to_string()]],
                },
            ],
            &keys,
        )
        .expect("valid project events");

        assert_eq!(events.len(), 2);
        assert!(events.iter().all(|event| event.pubkey == keys.public_key()));
        assert!(events.iter().all(|event| event.verify().is_ok()));
    }

    #[test]
    fn project_owner_control_rejects_arbitrary_or_unaddressed_events() {
        let keys = Keys::generate();
        let arbitrary = build_project_owner_announcement_events(
            vec![ProjectOwnerAnnouncementTemplate {
                kind: 1,
                content: String::new(),
                created_at: None,
                tags: vec![vec!["d".to_string(), "project".to_string()]],
            }],
            &keys,
        );
        assert!(arbitrary.is_err());

        let unaddressed = build_project_owner_announcement_events(
            vec![ProjectOwnerAnnouncementTemplate {
                kind: 30_621,
                content: String::new(),
                created_at: None,
                tags: vec![],
            }],
            &keys,
        );
        assert!(unaddressed.is_err());
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
                    description: None,
                },
            ),
            (
                stream_id,
                relay::ChannelInfo {
                    name: "stream".into(),
                    channel_type: "stream".into(),
                    description: None,
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
                description: None,
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
        // with the test_pair forwarding task). With per-tick batching the three
        // events arrive inside batch envelopes (or unwrapped when a drain held
        // exactly one event); unwrap both shapes.
        let mut markers = Vec::new();
        while let Some(event) = published_rx.recv().await {
            let payload: serde_json::Value =
                decrypt_observer_payload(&owner_keys, &event).expect("decrypt published frame");
            match payload["payload"]["events"].as_array() {
                Some(inner) => markers.extend(
                    inner
                        .iter()
                        .map(|e| e["payload"]["marker"].as_str().unwrap().to_string()),
                ),
                None => markers.push(payload["payload"]["marker"].as_str().unwrap().to_string()),
            }
        }
        assert_eq!(
            markers,
            ["before", "overlap", "after"],
            "each event must be published exactly once, in order"
        );
    }
}

#[cfg(test)]
mod observer_publish_queue_tests {
    use super::*;

    fn event(seq: u64, kind: &str, channel: Option<&str>) -> observer::ObserverEvent {
        observer::ObserverEvent {
            seq,
            timestamp: format!("2026-04-29T04:00:{:02}Z", seq.min(59)),
            kind: kind.to_string(),
            agent_index: Some(0),
            channel_id: channel.map(ToOwned::to_owned),
            project: None,
            session_id: Some("session-1".to_string()),
            turn_id: Some("turn-1".to_string()),
            started_at: None,
            payload: serde_json::json!({ "seq": seq }),
        }
    }

    /// A project-scoped event: no channel, a route ref instead.
    fn project_event(seq: u64, kind: &str, root: &str) -> observer::ObserverEvent {
        observer::ObserverEvent {
            project: Some(observer::ProjectRouteRef {
                coordinate: "30617:owner:repo".to_string(),
                root: root.to_string(),
            }),
            ..event(seq, kind, None)
        }
    }

    fn queue_of(events: Vec<observer::ObserverEvent>) -> ObserverPublishQueue {
        let mut queue = ObserverPublishQueue::default();
        for event in events {
            queue.ingest(event);
        }
        queue
    }

    /// Collect every frame the queue will produce, one publish slot at a time.
    fn drain_frames(queue: &mut ObserverPublishQueue) -> Vec<observer::ObserverEvent> {
        let mut frames = Vec::new();
        while !queue.is_empty() {
            frames.push(queue.next_frame().expect("queue not empty"));
        }
        frames
    }

    /// Inner seqs of a frame, whether it is an envelope or an unwrapped
    /// singleton.
    fn frame_seqs(frame: &observer::ObserverEvent) -> Vec<u64> {
        match frame.payload.get("events").and_then(|v| v.as_array()) {
            Some(inner) => inner.iter().map(|e| e["seq"].as_u64().unwrap()).collect(),
            None => vec![frame.seq],
        }
    }

    /// Retained bytes computed by WALKING the entries, independently of the
    /// queue's own accumulator. Cap regressions must assert on this, not on
    /// `total_pending_bytes()` — asserting the counter against itself passed
    /// while the process retained ~2x the budget (Sami/Max round 3: each
    /// pending coalescer entry holds the first chunk's text twice, in the
    /// serialized skeleton AND the extracted `text` copy).
    fn walked_retained_bytes(queue: &ObserverPublishQueue) -> usize {
        let fifo: usize = queue
            .events
            .iter()
            .map(|(_, _, event)| serialized_len(event))
            .sum();
        let coalescer: usize = queue
            .coalescer
            .pending
            .iter()
            .map(|pending| serialized_len(&pending.event) + pending.text.len())
            .sum();
        fifo + coalescer
    }

    /// The walker above is itself an instrument, and every cap test asks it
    /// only for `<= CAP` — a blinded walker (missing an arm, or returning 0)
    /// would satisfy all of them while hiding exactly the 2x overshoot it was
    /// added to catch (Sami round 5, M17-M20). Pin it two-sided: it must SEE
    /// the double retention, and it must agree with the accumulator EXACTLY
    /// while both stores are non-empty — neither may drift.
    #[test]
    fn walked_retained_bytes_agrees_with_the_accumulator_exactly() {
        fn chunk(seq: u64, message_id: &str, text: &str) -> observer::ObserverEvent {
            let mut e = event(seq, "acp_read", Some("chan-a"));
            e.payload = serde_json::json!({
                "jsonrpc": "2.0",
                "method": "session/update",
                "params": {
                    "sessionId": "session-1",
                    "update": {
                        "sessionUpdate": "agent_message_chunk",
                        "messageId": message_id,
                        "content": { "type": "text", "text": text },
                    },
                },
            });
            e
        }

        let text = "w".repeat(7_000);
        let mut queue = ObserverPublishQueue::default();
        // One pending chunk: its text lives in the serialized skeleton AND
        // the extracted copy, so a walker blind to either arm reads short.
        queue.ingest(chunk(1, "message-a", &text));
        assert!(
            walked_retained_bytes(&queue) >= 2 * text.len(),
            "the walker must SEE the first chunk's text twice \
             (skeleton + extracted copy), got {}",
            walked_retained_bytes(&queue)
        );

        // Populate BOTH stores: the non-chunk event flushes message-a into
        // the FIFO and queues itself; fresh pending keys (plus a same-key
        // append) rebuild the coalescer side.
        queue.ingest(event(2, "tool_call", Some("chan-a")));
        queue.ingest(chunk(3, "message-b", &text));
        queue.ingest(chunk(4, "message-b", &text));
        queue.ingest(chunk(5, "message-c", &text));
        assert!(
            !queue.events.is_empty() && !queue.coalescer.pending.is_empty(),
            "both arms must be non-empty for the agreement check to bind"
        );
        assert_eq!(
            queue.total_pending_bytes(),
            walked_retained_bytes(&queue),
            "accumulator and entry-walk must agree exactly: neither may drift"
        );
    }

    /// Two or more pending events for one channel ship as a single batch
    /// envelope whose payload carries every inner event in arrival order.
    #[test]
    fn multiple_events_ship_as_one_envelope_in_order() {
        let mut queue = queue_of(vec![
            event(1, "turn_started", Some("chan-a")),
            event(2, "acp_read", Some("chan-a")),
            event(3, "acp_write", Some("chan-a")),
        ]);

        let frame = queue.next_frame().expect("one frame");
        assert!(queue.is_empty(), "one channel, one publish slot");
        assert_eq!(frame.kind, OBSERVER_BATCH_KIND);
        assert_eq!(frame.seq, 3, "envelope mirrors the last inner event");
        assert_eq!(frame_seqs(&frame), [1, 2, 3], "arrival order preserved");
        let inner = frame.payload["events"].as_array().expect("events array");
        assert_eq!(inner[1]["kind"], "acp_read", "inner events keep their kind");
    }

    /// Project turns all carry `channel_id: None`, so channel alone does not
    /// separate them. Two roots must not land in one envelope: the envelope
    /// mirrors the LAST inner event, so a mixed frame would file the first
    /// root's work under the second's.
    #[test]
    fn a_frame_never_mixes_project_roots() {
        let mut queue = queue_of(vec![
            project_event(1, "turn_started", "root-a"),
            project_event(2, "acp_read", "root-a"),
            project_event(3, "acp_read", "root-b"),
        ]);

        let first = queue.next_frame().expect("first frame");
        assert_eq!(first.kind, OBSERVER_BATCH_KIND);
        assert_eq!(frame_seqs(&first), [1, 2], "root-a batches on its own");
        assert_eq!(
            first.project.as_ref().expect("project scope").root,
            "root-a",
        );

        let second = queue.next_frame().expect("second frame");
        assert!(queue.is_empty(), "both roots drained");
        assert_eq!(second.seq, 3, "root-b ships in its own frame");
        assert_eq!(
            second.project.as_ref().expect("project scope").root,
            "root-b",
        );
    }

    /// A single pending event is published unwrapped — no envelope, so
    /// consumers that predate batching still understand quiet periods.
    #[test]
    fn a_single_event_stays_unwrapped() {
        let mut queue = queue_of(vec![event(7, "turn_started", Some("chan-a"))]);
        let frame = queue.next_frame().expect("one frame");
        assert!(queue.is_empty());
        assert_eq!(frame.kind, "turn_started");
        assert_eq!(frame.seq, 7);
    }

    /// An empty queue yields no frame — a tick with nothing pending must not
    /// publish anything.
    #[test]
    fn empty_queue_yields_no_frame() {
        let mut queue = ObserverPublishQueue::default();
        assert!(queue.next_frame().is_none());
        assert!(queue.is_empty());
    }

    /// Frames never mix channels, and each channel's events keep their FIFO
    /// order. Gathering is QUEUE-WIDE: the front event's channel collects its
    /// events from anywhere in the queue (that is what keeps the drain rate
    /// in bytes per slot under interleaving), so cross-channel frame order
    /// MAY differ from arrival order — but a null-channel event is a barrier
    /// nothing gathers across.
    #[test]
    fn frames_never_mix_channels_and_gather_queue_wide() {
        let mut queue = queue_of(vec![
            event(1, "acp_read", Some("chan-a")),
            event(2, "acp_write", Some("chan-a")),
            event(3, "acp_read", Some("chan-b")),
            event(4, "acp_read", Some("chan-a")),
            event(5, "acp_read", None),
        ]);

        let frames = drain_frames(&mut queue);
        assert_eq!(
            frames.len(),
            3,
            "gathered: [1,2,4]@a, [3]@b, [5]@None — one frame each"
        );
        for frame in &frames {
            let channels: HashSet<Option<String>> = match frame.payload.get("events") {
                Some(serde_json::Value::Array(inner)) => inner
                    .iter()
                    .map(|e| e["channelId"].as_str().map(ToOwned::to_owned))
                    .collect(),
                _ => std::iter::once(frame.channel_id.clone()).collect(),
            };
            assert_eq!(channels.len(), 1, "a frame never mixes channels");
        }
        assert_eq!(
            frame_seqs(&frames[0]),
            [1, 2, 4],
            "chan-a gathers queue-wide, FIFO within the channel"
        );
        assert_eq!(frames[0].channel_id.as_deref(), Some("chan-a"));
        assert_eq!(frames[1].kind, "acp_read", "singleton stays unwrapped");
        assert_eq!(frames[1].channel_id.as_deref(), Some("chan-b"));
        assert_eq!(frames[2].channel_id, None);
    }

    /// A NULL-channel event is a barrier: channel events queued BEHIND it
    /// must not gather into a frame ahead of it, so causally-global events
    /// (`agent_panic`-class) keep their exact order against every channel.
    /// The null event itself ships only its contiguous front run.
    #[test]
    fn null_channel_events_are_gather_barriers() {
        let mut queue = queue_of(vec![
            event(1, "acp_read", Some("chan-a")),
            event(2, "acp_read", Some("chan-b")),
            event(3, "agent_panic", None),
            event(4, "acp_write", Some("chan-a")),
        ]);

        let frames = drain_frames(&mut queue);
        let published: Vec<Vec<u64>> = frames.iter().map(frame_seqs).collect();
        assert_eq!(
            published,
            [vec![1], vec![2], vec![3], vec![4]],
            "seq 4 must not gather past the null barrier into frame 1"
        );
    }

    /// The drain-rate regression Sami measured: with two channels strictly
    /// alternating, a front-run packer degrades to ONE event per slot
    /// (~275 B/s regardless of the 64KB frame budget). Queue-wide gathering
    /// must drain an interleaved backlog in ~ceil(events / per-frame-fit)
    /// slots per channel, not one slot per event.
    #[test]
    fn interleaved_channels_drain_at_bytes_per_slot_not_events_per_slot() {
        let mut events = Vec::new();
        for i in 0..100u64 {
            events.push(event(2 * i + 1, "acp_read", Some("chan-a")));
            events.push(event(2 * i + 2, "acp_read", Some("chan-b")));
        }
        let mut queue = queue_of(events);

        let frames = drain_frames(&mut queue);
        assert!(
            frames.len() <= 4,
            "200 tiny alternating events must gather into a few full frames, \
             got {} (front-run packing would need 200 slots)",
            frames.len()
        );
        for frame in &frames {
            assert!(serialized_len(frame) <= OBSERVER_MAX_PLAINTEXT_LEN);
        }
        // Within each channel, FIFO order survives the gather.
        let mut seqs_a = Vec::new();
        let mut seqs_b = Vec::new();
        for frame in &frames {
            match frame.channel_id.as_deref() {
                Some("chan-a") => seqs_a.extend(frame_seqs(frame)),
                Some("chan-b") => seqs_b.extend(frame_seqs(frame)),
                other => panic!("unexpected channel {other:?}"),
            }
        }
        assert!(seqs_a.windows(2).all(|w| w[0] < w[1]), "chan-a FIFO");
        assert!(seqs_b.windows(2).all(|w| w[0] < w[1]), "chan-b FIFO");
        assert_eq!(seqs_a.len() + seqs_b.len(), 200, "nothing lost");
    }

    /// A same-channel backlog that cannot fit one 64KB frame splits across
    /// SUCCESSIVE publish slots — never multiple frames from one slot — with
    /// every frame under the cap and no event lost or reordered.
    #[test]
    fn oversized_backlogs_split_across_publish_slots_under_the_cap() {
        let big_text = "x".repeat(30_000);
        let mut queue = queue_of(
            (1..=6)
                .map(|seq| {
                    let mut e = event(seq, "acp_read", Some("chan-a"));
                    e.payload = serde_json::json!({ "seq": seq, "text": big_text });
                    e
                })
                .collect(),
        );

        let frames = drain_frames(&mut queue);
        assert!(
            frames.len() > 1,
            "six 30KB events cannot fit one 64KB frame"
        );
        let mut seen = Vec::new();
        for frame in &frames {
            assert!(
                serialized_len(frame) <= OBSERVER_MAX_PLAINTEXT_LEN,
                "every emitted frame must fit the plaintext cap"
            );
            seen.extend(frame_seqs(frame));
        }
        assert_eq!(
            seen,
            [1, 2, 3, 4, 5, 6],
            "no event lost or reordered by splitting"
        );
    }

    /// The queue preserves the coalescer's ordering rule: a non-chunk event
    /// force-flushes pending chunk text ahead of itself, so merged chunks can
    /// never leapfrog a tool call that arrived after them.
    #[test]
    fn non_chunk_events_flush_pending_chunks_ahead_of_themselves() {
        fn chunk(seq: u64, text: &str) -> observer::ObserverEvent {
            let mut e = event(seq, "acp_read", Some("chan-a"));
            e.payload = serde_json::json!({
                "params": { "update": {
                    "sessionUpdate": "agent_message_chunk",
                    "messageId": "m1",
                    "content": { "text": text },
                }}
            });
            e
        }

        let mut queue = ObserverPublishQueue::default();
        queue.ingest(chunk(1, "hello "));
        queue.ingest(chunk(2, "world"));
        queue.ingest(event(3, "tool_call", Some("chan-a")));

        let frame = queue.next_frame().expect("one frame");
        assert!(queue.is_empty());
        let inner = frame.payload["events"].as_array().expect("batch of 2");
        assert_eq!(inner.len(), 2, "two chunks coalesce into one event");
        assert_eq!(
            inner[0]["payload"]["params"]["update"]["content"]["text"], "hello world",
            "chunk text merged before the tool call"
        );
        assert_eq!(inner[1]["kind"], "tool_call");
        assert!(inner[0]["seq"].as_u64() < inner[1]["seq"].as_u64());
    }

    /// Chunks still pending inside the coalescer (no non-chunk flushed them)
    /// are picked up by the publish slot itself, not stranded.
    #[test]
    fn a_publish_slot_flushes_pending_coalesced_chunks() {
        let mut e = event(1, "acp_read", Some("chan-a"));
        e.payload = serde_json::json!({
            "params": { "update": {
                "sessionUpdate": "agent_message_chunk",
                "messageId": "m1",
                "content": { "text": "buffered" },
            }}
        });
        let mut queue = ObserverPublishQueue::default();
        queue.ingest(e);
        assert!(!queue.is_empty(), "pending chunk counts as queued work");

        let frame = queue.next_frame().expect("chunk must ship");
        assert!(queue.is_empty());
        assert_eq!(
            frame.payload["params"]["update"]["content"]["text"],
            "buffered"
        );
    }

    /// Sami's ceiling assertion: when sustained input outruns the one-frame
    /// drain budget for longer than the queue's byte budget, the OLDEST events
    /// drop with accounting — never silently — and everything that survives
    /// publishes in order with nothing else lost.
    #[test]
    fn over_budget_floods_drop_oldest_with_accounting() {
        let big_text = "y".repeat(10_000);
        let total = 500usize; // ~5MB of ~10KB events > 4MiB budget
        let mut queue = ObserverPublishQueue::default();
        for seq in 1..=total as u64 {
            let mut e = event(seq, "acp_read", Some("chan-a"));
            e.payload = serde_json::json!({ "seq": seq, "text": big_text });
            queue.ingest(e);
        }

        assert!(
            queue.dropped_events > 0,
            "a 5MB backlog must overflow the 4MiB budget"
        );
        assert!(
            walked_retained_bytes(&queue) <= OBSERVER_PENDING_QUEUE_MAX_BYTES,
            "eviction must restore the byte budget (entry-walked), got {}",
            walked_retained_bytes(&queue)
        );

        let frames = drain_frames(&mut queue);
        let published: Vec<u64> = frames.iter().flat_map(frame_seqs).collect();
        let expected: Vec<u64> = (queue.dropped_events + 1..=total as u64).collect();
        assert_eq!(
            published, expected,
            "exactly the oldest `dropped_events` events are missing; the rest \
             publish in order"
        );
        assert_eq!(
            published.len() as u64 + queue.dropped_events,
            total as u64,
            "accounting: published + dropped == ingested"
        );
    }

    /// Max's coalescer-bypass regression: a flood of chunks with DISTINCT
    /// messageIds never flushes on its own, so every chunk sits in the
    /// coalescer's pending buffer. TRUE retained bytes — walked from the
    /// entries, never the queue's own accumulator — MUST respect the byte
    /// budget with event-level drop accounting. Pre-fix this retained ~25MB
    /// against the 4 MiB cap with `pending_bytes == 0` and zero drops; the
    /// round-3 refinement (Sami/Max) caught the accumulator itself reading
    /// under cap while true retention was 1.99x over.
    #[test]
    fn distinct_key_chunk_floods_are_bounded_by_the_byte_budget() {
        let big_text = "z".repeat(50_000);
        let total = 500u64; // ~25MB pending chunk text vs a 4MiB budget
        let mut queue = ObserverPublishQueue::default();
        for seq in 1..=total {
            let mut e = event(seq, "acp_read", Some("chan-a"));
            e.payload = serde_json::json!({
                "jsonrpc": "2.0",
                "method": "session/update",
                "params": {
                    "sessionId": "session-1",
                    "update": {
                        "sessionUpdate": "agent_message_chunk",
                        "messageId": format!("message-{seq}"),
                        "content": { "type": "text", "text": big_text },
                    },
                },
            });
            queue.ingest(e);
        }

        let walked = walked_retained_bytes(&queue);
        assert!(
            walked <= OBSERVER_PENDING_QUEUE_MAX_BYTES,
            "TRUE retained bytes (walked from entries) must respect the cap, \
             got {walked}"
        );
        assert!(
            queue.total_pending_bytes() >= walked,
            "the accumulator must never under-count true retention \
             (accumulator {} < walked {walked})",
            queue.total_pending_bytes()
        );
        assert!(
            queue.dropped_events > 0,
            "a ~25MB distinct-key chunk flood must record drops"
        );
        // Event-level accounting: everything that survives publishes, and
        // survivors + dropped == ingested.
        let frames = drain_frames(&mut queue);
        let survived: u64 = frames.iter().map(|f| frame_seqs(f).len() as u64).sum();
        assert_eq!(
            survived + queue.dropped_events,
            total,
            "accounting: published + dropped == ingested"
        );
        // The survivors are the NEWEST events (drop-oldest).
        let last_frame_seqs = frame_seqs(frames.last().expect("frames"));
        assert_eq!(*last_frame_seqs.last().expect("seqs"), total);
    }

    /// Max's merged-chunk accounting regression: one coalescer entry can
    /// represent MANY generated observer events (same-messageId chunks merge
    /// in place), so evicting it must charge every merged source event to
    /// `dropped_events`, not 1 per retained entry. Pre-fix, evicting an entry
    /// that merged 50 chunks recorded `dropped_events == 1` and 49 generated
    /// events vanished from the accounting.
    #[test]
    fn evicting_a_merged_chunk_entry_accounts_every_source_event() {
        fn chunk(seq: u64, message_id: &str, text: &str) -> observer::ObserverEvent {
            let mut e = event(seq, "acp_read", Some("chan-a"));
            e.payload = serde_json::json!({
                "jsonrpc": "2.0",
                "method": "session/update",
                "params": {
                    "sessionId": "session-1",
                    "update": {
                        "sessionUpdate": "agent_message_chunk",
                        "messageId": message_id,
                        "content": { "type": "text", "text": text },
                    },
                },
            });
            e
        }

        let mut queue = ObserverPublishQueue::default();
        // 50 × 1KB chunks under ONE messageId merge into a single pending
        // coalescer entry — the oldest item anywhere in the queue.
        let merged_text = "m".repeat(1_000);
        let merged_sources = 50u64;
        for seq in 1..=merged_sources {
            queue.ingest(chunk(seq, "message-merged", &merged_text));
        }
        // Flood with distinct-key 50KB chunks until the byte budget evicts
        // the oldest entries — the merged entry goes first.
        let flood_text = "f".repeat(50_000);
        let flood = 100u64;
        for seq in 1..=flood {
            queue.ingest(chunk(
                merged_sources + seq,
                &format!("message-{seq}"),
                &flood_text,
            ));
        }

        assert!(
            walked_retained_bytes(&queue) <= OBSERVER_PENDING_QUEUE_MAX_BYTES,
            "eviction must restore the byte budget (entry-walked), got {}",
            walked_retained_bytes(&queue)
        );
        let frames = drain_frames(&mut queue);
        assert!(
            !frames
                .iter()
                .flat_map(frame_seqs)
                .any(|seq| seq <= merged_sources),
            "the merged entry (globally oldest) must have been evicted"
        );
        // Every survivor is an unmerged distinct-key chunk (1 source each),
        // so source-event accounting must close exactly: the merged entry's
        // eviction charges all 50 sources.
        let survived: u64 = frames.iter().map(|f| frame_seqs(f).len() as u64).sum();
        assert_eq!(
            survived + queue.dropped_events,
            merged_sources + flood,
            "accounting: published sources + dropped sources == ingested"
        );
    }

    /// Sami's M13 / Max's forced-flush probe: the OTHER eviction arm. A
    /// merged entry FLUSHED into the publish FIFO (by a non-chunk event) must
    /// still charge every absorbed source on eviction — the FIFO stores the
    /// per-entry count precisely so the ledger survives flush. The
    /// coalescer-side regression above never exercises this arm; mutating the
    /// FIFO eviction to `dropped += 1` survived all 687 tests until this one.
    #[test]
    fn evicting_a_flushed_merged_entry_from_the_fifo_accounts_every_source_event() {
        fn chunk(seq: u64, message_id: &str, text: &str) -> observer::ObserverEvent {
            let mut e = event(seq, "acp_read", Some("chan-a"));
            e.payload = serde_json::json!({
                "jsonrpc": "2.0",
                "method": "session/update",
                "params": {
                    "sessionId": "session-1",
                    "update": {
                        "sessionUpdate": "agent_message_chunk",
                        "messageId": message_id,
                        "content": { "type": "text", "text": text },
                    },
                },
            });
            e
        }

        let mut queue = ObserverPublishQueue::default();
        // 50 × 1KB chunks merge under one messageId in the coalescer…
        let merged_text = "m".repeat(1_000);
        let merged_sources = 50u64;
        for seq in 1..=merged_sources {
            queue.ingest(chunk(seq, "message-merged", &merged_text));
        }
        // …then a non-chunk event force-flushes the merged entry into the
        // publish FIFO. From here eviction happens on the FIFO arm.
        queue.ingest(event(merged_sources + 1, "tool_call", Some("chan-a")));
        assert!(
            queue.coalescer.pending.is_empty(),
            "the non-chunk event must have flushed the merged entry"
        );
        assert_eq!(
            queue.events.front().expect("flushed entry queued").1,
            merged_sources,
            "the FIFO front must carry the merged source count"
        );

        // Distinct-key flood forces byte-budget eviction of the FIFO front.
        let flood_text = "f".repeat(50_000);
        let flood = 100u64;
        for seq in 1..=flood {
            queue.ingest(chunk(
                merged_sources + 1 + seq,
                &format!("message-{seq}"),
                &flood_text,
            ));
        }

        assert!(
            walked_retained_bytes(&queue) <= OBSERVER_PENDING_QUEUE_MAX_BYTES,
            "eviction must restore the byte budget (entry-walked), got {}",
            walked_retained_bytes(&queue)
        );
        let frames = drain_frames(&mut queue);
        assert!(
            !frames
                .iter()
                .flat_map(frame_seqs)
                .any(|seq| seq <= merged_sources),
            "the flushed merged entry (globally oldest) must have been evicted"
        );
        // Ledger in source units: survivors are unmerged (1 source each), the
        // evicted merged FIFO entry must charge all 50 sources.
        let survived: u64 = frames.iter().map(|f| frame_seqs(f).len() as u64).sum();
        let ingested = merged_sources + 1 + flood;
        assert_eq!(
            survived + queue.dropped_events,
            ingested,
            "accounting: published sources + dropped sources == ingested"
        );
    }

    /// Under the byte budget the queue is lossless: every ingested event
    /// publishes exactly once.
    #[test]
    fn under_budget_backlogs_are_lossless() {
        let mut queue = queue_of(
            (1..=200)
                .map(|seq| event(seq, "acp_read", Some("chan-a")))
                .collect(),
        );
        let frames = drain_frames(&mut queue);
        let published: Vec<u64> = frames.iter().flat_map(frame_seqs).collect();
        assert_eq!(published, (1..=200).collect::<Vec<u64>>());
        assert_eq!(queue.dropped_events, 0);
    }
}

#[cfg(test)]
mod observer_publish_cadence_tests {
    use super::*;
    use nostr::Keys;

    /// Let every spawned task (publisher loop, test_pair forwarder) run to
    /// quiescence WITHOUT advancing paused time. `yield_now` keeps this task
    /// runnable, so tokio's auto-advance never fires here — time only moves
    /// when the test says so.
    async fn settle() {
        for _ in 0..64 {
            tokio::task::yield_now().await;
        }
    }

    fn recv_all(rx: &mut tokio::sync::mpsc::Receiver<nostr::Event>) -> Vec<nostr::Event> {
        let mut out = Vec::new();
        while let Ok(event) = rx.try_recv() {
            out.push(event);
        }
        out
    }

    fn count_inner(owner: &Keys, event: &nostr::Event) -> usize {
        let payload: serde_json::Value =
            decrypt_observer_payload(owner, event).expect("decrypt frame");
        match payload["payload"]["events"].as_array() {
            Some(inner) => inner.len(),
            None => 1,
        }
    }

    fn emit_on(observer: &observer::ObserverHandle, channel: Option<uuid::Uuid>, marker: &str) {
        observer.emit(
            "test_event",
            None,
            &observer::context_for(channel, None, None),
            serde_json::json!({ "marker": marker }),
        );
    }

    /// THE regression Max demanded: with a backlog needing multiple frames
    /// (two channels — a frame never mixes channels, so the backlog takes two
    /// publish slots), no frame publishes before its tick. Startup publishes
    /// NOTHING at t=0 (Sami's Finding 1: a full replay buffer must not burst
    /// on reconnect), frame 1 arrives at +1s, frame 2 no earlier than +2s.
    #[tokio::test(start_paused = true)]
    async fn one_frame_per_second_and_no_startup_burst() {
        let observer = observer::ObserverHandle::in_process();
        let agent_keys = Keys::generate();
        let owner_keys = Keys::generate();
        let (publisher, mut published_rx) = RelayEventPublisher::test_pair();

        // Interleave channels so the backlog cannot fit one frame: each run
        // boundary forces a new publish slot.
        let chan_a = uuid::Uuid::new_v4();
        let chan_b = uuid::Uuid::new_v4();
        emit_on(&observer, Some(chan_a), "a1");
        emit_on(&observer, Some(chan_b), "b1");
        emit_on(&observer, Some(chan_a), "a2");

        let rx = observer.subscribe();
        let snapshot = observer.snapshot();
        assert_eq!(snapshot.len(), 3, "all three preloaded in the snapshot");

        let task = tokio::spawn(run_relay_observer_publisher(
            snapshot,
            rx,
            publisher,
            agent_keys.clone(),
            agent_keys.public_key().to_hex(),
            owner_keys.public_key().to_hex(),
            owner_keys.public_key(),
        ));

        // t=0: nothing may publish, no matter how full the snapshot was.
        settle().await;
        assert_eq!(
            recv_all(&mut published_rx).len(),
            0,
            "startup must not burst at t=0"
        );

        // t=0.999s: still nothing.
        tokio::time::advance(Duration::from_millis(999)).await;
        settle().await;
        assert_eq!(
            recv_all(&mut published_rx).len(),
            0,
            "no frame may publish before the first tick"
        );

        // t=1s: exactly ONE frame — chan-a gathered queue-wide, so a1 AND a2
        // ride the first slot together.
        tokio::time::advance(Duration::from_millis(1)).await;
        settle().await;
        let frames = recv_all(&mut published_rx);
        assert_eq!(frames.len(), 1, "tick 1 publishes exactly one frame");
        assert_eq!(count_inner(&owner_keys, &frames[0]), 2, "a1 + a2 gathered");

        // t=1.5s: between ticks, nothing.
        tokio::time::advance(Duration::from_millis(500)).await;
        settle().await;
        assert_eq!(
            recv_all(&mut published_rx).len(),
            0,
            "frame 2 must wait for tick 2"
        );

        // t=2s: the chan-b frame drains on its own tick.
        tokio::time::advance(Duration::from_millis(500)).await;
        settle().await;
        assert_eq!(recv_all(&mut published_rx).len(), 1, "tick 2: one frame");

        // Backlog drained; a quiet tick publishes nothing.
        tokio::time::advance(Duration::from_secs(1)).await;
        settle().await;
        assert_eq!(recv_all(&mut published_rx).len(), 0, "quiet tick is quiet");

        task.abort();
    }

    /// Shutdown is NOT a burst bypass: when the producer closes with a
    /// backlog, the remaining frames still publish one per tick, and the loop
    /// exits only after the queue is empty — paced, lossless, in order.
    #[tokio::test(start_paused = true)]
    async fn shutdown_drain_is_paced_and_lossless() {
        let observer = observer::ObserverHandle::in_process();
        let agent_keys = Keys::generate();
        let owner_keys = Keys::generate();
        let (publisher, mut published_rx) = RelayEventPublisher::test_pair();

        let chan_a = uuid::Uuid::new_v4();
        let chan_b = uuid::Uuid::new_v4();
        emit_on(&observer, Some(chan_a), "a1");
        emit_on(&observer, Some(chan_b), "b1");
        emit_on(&observer, Some(chan_a), "a2");

        let rx = observer.subscribe();
        let snapshot = observer.snapshot();
        // Close the broadcast channel immediately: the entire drain happens
        // in "shutdown" mode.
        drop(observer);

        let task = tokio::spawn(run_relay_observer_publisher(
            snapshot,
            rx,
            publisher,
            agent_keys.clone(),
            agent_keys.public_key().to_hex(),
            owner_keys.public_key().to_hex(),
            owner_keys.public_key(),
        ));

        settle().await;
        assert_eq!(
            recv_all(&mut published_rx).len(),
            0,
            "shutdown drain must not burst at t=0"
        );

        let mut markers = Vec::new();
        for tick in 1..=2 {
            tokio::time::advance(Duration::from_secs(1)).await;
            settle().await;
            let frames = recv_all(&mut published_rx);
            assert_eq!(frames.len(), 1, "shutdown tick {tick}: exactly one frame");
            let payload: serde_json::Value =
                decrypt_observer_payload(&owner_keys, &frames[0]).expect("decrypt");
            match payload["payload"]["events"].as_array() {
                Some(inner) => markers.extend(
                    inner
                        .iter()
                        .map(|e| e["payload"]["marker"].as_str().unwrap().to_string()),
                ),
                None => markers.push(payload["payload"]["marker"].as_str().unwrap().to_string()),
            }
        }
        // Gather-packing: chan-a (a1+a2) ships tick 1, chan-b tick 2.
        assert_eq!(markers, ["a1", "a2", "b1"], "paced drain loses nothing");

        // Queue empty + closed: the loop must have exited on its own.
        tokio::time::advance(Duration::from_secs(1)).await;
        settle().await;
        assert!(task.is_finished(), "publisher exits after paced drain");
    }

    /// Pins `MissedTickBehavior::Skip` (Sami's M6 mutant): when the publisher
    /// misses ticks — relay backpressure can stall the tick arm past several
    /// deadlines, since `publish_event` awaits a bounded mpsc — the interval
    /// must fire ONE catch-up tick and realign, not fire once per missed
    /// deadline. With `Burst`, a 10s stall against a multi-frame backlog
    /// would replay all 10 missed ticks back-to-back: an unpaced burst that
    /// bypasses exactly what the pacer exists to prevent.
    #[tokio::test(start_paused = true)]
    async fn missed_ticks_skip_instead_of_bursting() {
        let observer = observer::ObserverHandle::in_process();
        let agent_keys = Keys::generate();
        let owner_keys = Keys::generate();
        let (publisher, mut published_rx) = RelayEventPublisher::test_pair();

        // Three channels => three frames pending (a frame never mixes
        // channels), so a bursting interval would have work for every
        // spurious catch-up tick.
        for chan in 0..3 {
            emit_on(&observer, Some(uuid::Uuid::new_v4()), &format!("c{chan}"));
        }
        let rx = observer.subscribe();
        let snapshot = observer.snapshot();

        let task = tokio::spawn(run_relay_observer_publisher(
            snapshot,
            rx,
            publisher,
            agent_keys.clone(),
            agent_keys.public_key().to_hex(),
            owner_keys.public_key().to_hex(),
            owner_keys.public_key(),
        ));
        settle().await;

        // Jump 10 seconds in ONE advance — the loop was never polled in
        // between, exactly like a stall across 10 deadlines.
        tokio::time::advance(Duration::from_secs(10)).await;
        settle().await;
        assert_eq!(
            recv_all(&mut published_rx).len(),
            1,
            "Skip: one catch-up frame after a stall — Burst would publish \
             one per missed deadline"
        );

        // The interval realigned: the remaining backlog stays paced.
        tokio::time::advance(Duration::from_secs(1)).await;
        settle().await;
        assert_eq!(recv_all(&mut published_rx).len(), 1, "paced after realign");

        task.abort();
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
            project: None,
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
            project: None,
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
        assert_eq!(events[0].1.seq, 2);
        assert_eq!(chunk_text(&events[0].1), "hello world");
        assert_eq!(
            events[0].0, 2,
            "a merged entry reports every source chunk it absorbed"
        );
        assert_eq!(events[1].1.kind, "turn_started");
        assert_eq!(events[1].0, 1);
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
        assert_eq!(chunk_text(&events[0].1), "answer");
        assert_eq!(chunk_text(&events[1].1), "thinking");
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
            state_dir: std::path::PathBuf::from("./buzz-acp-state"),
            context_message_limit: 12,
            max_turns_per_session: 0,
            presence_enabled: true,
            typing_enabled: true,
            memory_enabled: false,
            model: None,
            effort_level: None,
            session_title: None,
            permission_mode: config::PermissionMode::DontAsk,
            respond_to: config::RespondTo::Anyone,
            respond_to_allowlist: std::collections::HashSet::new(),
            allowed_respond_to: vec![],
            persona_env_vars: vec![],
            has_generated_codex_config: false,
            relay_observer: false,
            exit_after_inactivity_secs: 0,
            lazy_pool: false,
            project_routing_enabled: false,
            peer_agents: HashSet::new(),
            idle_pool_sleep_secs: 0,
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
    //! - Channel silence for the *error* outcomes is asserted by passing no
    //!   relay handle: `handle_prompt_result` posts only through the
    //!   `rest_client` it is given, so a `None` here is a channel that cannot
    //!   be written to at all. (The structural version of this claim — that
    //!   the function took no relay handle — stopped being true when
    //!   dead-letter and auth-required notices were added; the notices those
    //!   paths *do* post are pinned in their own sections below.)
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
            state_dir: std::path::PathBuf::from("./buzz-acp-state"),
            context_message_limit: 12,
            max_turns_per_session: 0,
            presence_enabled: true,
            typing_enabled: true,
            memory_enabled: false,
            model: None,
            effort_level: None,
            session_title: None,
            permission_mode: config::PermissionMode::DontAsk,
            respond_to: config::RespondTo::Anyone,
            respond_to_allowlist: HashSet::new(),
            allowed_respond_to: vec![],
            persona_env_vars: vec![],
            has_generated_codex_config: false,
            relay_observer: false,
            exit_after_inactivity_secs: 0,
            lazy_pool: false,
            project_routing_enabled: false,
            peer_agents: HashSet::new(),
            idle_pool_sleep_secs: 0,
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
            desired_model_request_id: None,
            desired_model_pending_ack: false,
            startup_effort: None,
            agent_name: "unknown".into(),
            goose_system_prompt_supported: None,
            // Error branches under test never read this; 1 is the legacy
            // non-systemPrompt path, the simplest valid value.
            protocol_version: 1,
        }
    }

    #[tokio::test]
    async fn successful_native_steer_is_transferred_to_live_session_delivery_state() {
        let channel_id = Uuid::new_v4();
        let steer_event_id = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let mut agent = dummy_agent(0).await;
        agent
            .state
            .sessions
            .insert(channel_id, "live-session".into());
        agent
            .state
            .deliveries
            .insert(channel_id, Default::default());

        let mut pool = AgentPool::from_slots(vec![None]);
        let task_id = pool.join_set.spawn(async {}).id();
        pool.task_map_mut().insert(
            task_id,
            crate::pool::TaskMeta {
                agent_index: 0,
                channel_id: Some(channel_id),
                turn_id: "test-turn-id".into(),
                recoverable_batch: None,
                control_tx: None,
                steer_tx: None,
                successful_steer_deliveries: HashSet::from([
                    crate::pool::SuccessfulSteerDelivery {
                        event_id: steer_event_id.into(),
                        session_id: "live-session".into(),
                    },
                ]),
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
            turn_id: "test-turn-id".into(),
            outcome: PromptOutcome::Ok(crate::acp::StopReason::EndTurn),
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
            None,
            None,
        );

        let returned = pool.agents_mut()[0].as_ref().expect("returned agent");
        assert!(returned.state.deliveries[&channel_id]
            .delivered_event_ids
            .contains(steer_event_id));
    }

    #[tokio::test]
    async fn in_flight_stale_native_steer_ack_cannot_update_replacement_session() {
        let channel_id = Uuid::new_v4();
        let mut agent = dummy_agent(0).await;
        agent
            .state
            .sessions
            .insert(channel_id, "replacement-session".into());
        agent
            .state
            .deliveries
            .insert(channel_id, Default::default());

        let mut pool = AgentPool::from_slots(vec![None]);
        let task_id = pool.join_set.spawn(async {}).id();
        pool.task_map_mut().insert(
            task_id,
            crate::pool::TaskMeta {
                agent_index: 0,
                channel_id: Some(channel_id),
                turn_id: "test-turn-id".into(),
                recoverable_batch: None,
                control_tx: None,
                steer_tx: None,
                successful_steer_deliveries: HashSet::from([
                    crate::pool::SuccessfulSteerDelivery {
                        event_id: "stale-event".into(),
                        session_id: "old-session".into(),
                    },
                ]),
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
            turn_id: "test-turn-id".into(),
            outcome: PromptOutcome::Ok(crate::acp::StopReason::EndTurn),
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
            None,
            None,
        );

        let returned = pool.agents_mut()[0].as_ref().expect("returned agent");
        assert!(returned.state.deliveries[&channel_id]
            .delivered_event_ids
            .is_empty());
    }

    #[tokio::test]
    async fn successful_native_steer_ack_after_task_return_updates_matching_live_session() {
        let channel_id = Uuid::new_v4();
        let steer_event_id = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let mut agent = dummy_agent(0).await;
        agent
            .state
            .sessions
            .insert(channel_id, "live-session".into());
        agent
            .state
            .deliveries
            .insert(channel_id, Default::default());
        let mut pool = AgentPool::from_slots(vec![Some(agent)]);

        assert!(pool.record_successful_steer(
            channel_id,
            steer_event_id.into(),
            "live-session".into(),
        ));
        let returned = pool.agents_mut()[0].as_ref().expect("idle returned agent");
        assert!(returned.state.deliveries[&channel_id]
            .delivered_event_ids
            .contains(steer_event_id));
    }

    #[tokio::test]
    async fn late_native_steer_ack_cannot_update_replacement_session() {
        let channel_id = Uuid::new_v4();
        let mut agent = dummy_agent(0).await;
        agent
            .state
            .sessions
            .insert(channel_id, "replacement-session".into());
        agent
            .state
            .deliveries
            .insert(channel_id, Default::default());
        let mut pool = AgentPool::from_slots(vec![Some(agent)]);

        assert!(!pool.record_successful_steer(
            channel_id,
            "stale-event".into(),
            "old-session".into(),
        ));
        let returned = pool.agents_mut()[0].as_ref().expect("replacement agent");
        assert!(returned.state.deliveries[&channel_id]
            .delivered_event_ids
            .is_empty());
    }

    #[tokio::test]
    async fn invalidated_session_does_not_resurrect_successful_steer_delivery_state() {
        let channel_id = Uuid::new_v4();
        let agent = dummy_agent(0).await;
        // No live session: simulates the prompt task invalidating before return.
        let mut pool = AgentPool::from_slots(vec![None]);
        let task_id = pool.join_set.spawn(async {}).id();
        pool.task_map_mut().insert(
            task_id,
            crate::pool::TaskMeta {
                agent_index: 0,
                channel_id: Some(channel_id),
                turn_id: "test-turn-id".into(),
                recoverable_batch: None,
                control_tx: None,
                steer_tx: None,
                successful_steer_deliveries: HashSet::from([
                    crate::pool::SuccessfulSteerDelivery {
                        event_id: "stale-event".into(),
                        session_id: "invalidated-session".into(),
                    },
                ]),
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
            turn_id: "test-turn-id".into(),
            outcome: PromptOutcome::Ok(crate::acp::StopReason::EndTurn),
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
            None,
            None,
        );

        let returned = pool.agents_mut()[0].as_ref().expect("returned agent");
        assert!(!returned.state.deliveries.contains_key(&channel_id));
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
                successful_steer_deliveries: HashSet::new(),
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
    async fn cancel_all_cutoff_suppresses_consumed_control_result_requeue() {
        let owner = Keys::generate();
        let channel_id = Uuid::new_v4();
        let mut queue = EventQueue::new(config::DedupMode::Queue);
        assert!(queue.push(QueuedEvent {
            channel_id,
            event: EventBuilder::new(Kind::Custom(9), "accepted before cutoff")
                .sign_with_keys(&owner)
                .unwrap(),
            received_at: std::time::Instant::now(),
            prompt_tag: "test".into(),
            project: None,
        }));
        let batch = queue.flush_next().expect("in-flight batch");

        let agent = dummy_agent(0).await;
        let mut pool = AgentPool::from_slots(vec![None]);
        let task_id = pool.join_set.spawn(async {}).id();
        pool.task_map_mut().insert(
            task_id,
            crate::pool::TaskMeta {
                agent_index: 0,
                channel_id: Some(channel_id),
                turn_id: "cutoff-turn".into(),
                recoverable_batch: Some(batch.clone()),
                control_tx: None,
                steer_tx: None,
                successful_steer_deliveries: HashSet::new(),
            },
        );

        let outcome = handle_cancel_all_control(&mut pool, &mut queue, None);
        assert_eq!(outcome.status(), "accepted");
        assert_eq!(outcome.active_turns, 1);
        assert_eq!(outcome.signalled_turns, 0);
        assert!(
            pool.task_map()
                .get(&task_id)
                .expect("cutoff task metadata")
                .recoverable_batch
                .is_none(),
            "panic recovery must not retain a pre-cutoff batch"
        );

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
            turn_id: "cutoff-turn".into(),
            outcome: PromptOutcome::Cancelled,
            batch: Some(batch),
        };
        assert!(matches!(
            handle_prompt_result(
                &mut pool,
                &mut queue,
                &test_config(),
                result,
                &mut heartbeat_in_flight,
                &removed_channels,
                &mut crash_history,
                &respawn_tx,
                &mut respawn_tasks,
                None,
                None,
            ),
            LoopAction::Continue
        ));
        assert!(!queue.has_undrained_work());
        assert_eq!(queue.queued_event_count(&channel_id), 0);

        assert!(queue.push(QueuedEvent {
            channel_id,
            event: EventBuilder::new(Kind::Custom(9), "new after cutoff")
                .sign_with_keys(&owner)
                .unwrap(),
            received_at: std::time::Instant::now(),
            prompt_tag: "test".into(),
            project: None,
        }));
        assert_eq!(queue.queued_event_count(&channel_id), 1);
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
                successful_steer_deliveries: HashSet::new(),
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
                    successful_steer_deliveries: HashSet::new(),
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
                    successful_steer_deliveries: HashSet::new(),
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
                    successful_steer_deliveries: HashSet::new(),
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
                successful_steer_deliveries: HashSet::new(),
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
                successful_steer_deliveries: HashSet::new(),
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
                successful_steer_deliveries: HashSet::new(),
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
                successful_steer_deliveries: HashSet::new(),
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

    // ── typed terminal-auth classification ─────────────────────────────────
    //
    // Prose recognition itself is tested where it lives (`terminal_auth`).
    // These cover the boundary this module owns: which outcomes the queue and
    // lifecycle treat as terminal.

    #[test]
    fn terminal_auth_outcome_is_recognised_and_carried_intact() {
        let terminal = terminal_auth::TerminalAuth {
            adapter: terminal_auth::AdapterFamily::Claude,
            stage: terminal_auth::AuthStage::Prompt,
            signal: terminal_auth::AuthSignal::ClaudeOauthUnrefreshable,
        };
        let outcome = PromptOutcome::Error(acp::AcpError::TerminalAuth(terminal));
        assert_eq!(terminal_auth_of(&outcome), Some(terminal));
    }

    #[test]
    fn ordinary_agent_errors_are_not_terminal_however_they_read() {
        // The queue must not re-derive terminal-ness from prose. Even the
        // exact legacy Claude wording, arriving as an untyped `AgentError`,
        // stays retryable here — classification happens once, at the ACP seam.
        for message in [
            "API Error: 401 OAuth access token has expired. Re-authenticate to continue.",
            "Usage credits required for 1M context — turn on usage credits",
        ] {
            let outcome = PromptOutcome::Error(acp::AcpError::AgentError {
                code: -32000,
                message: message.to_string(),
            });
            assert_eq!(terminal_auth_of(&outcome), None, "{message}");
        }
    }

    #[test]
    fn transport_and_non_error_outcomes_are_not_terminal() {
        let cases = [
            PromptOutcome::Error(acp::AcpError::Io(std::io::Error::other("pipe broke"))),
            PromptOutcome::Error(acp::AcpError::WriteTimeout(std::time::Duration::from_secs(
                5,
            ))),
            PromptOutcome::AgentExited,
            PromptOutcome::Cancelled,
            PromptOutcome::Ok(acp::StopReason::EndTurn),
        ];
        for outcome in cases {
            assert_eq!(terminal_auth_of(&outcome), None);
        }
    }

    /// A store rooted in a fresh temp dir, ready to attach to a test queue.
    fn test_terminal_auth_store(
        temp: &tempfile::TempDir,
    ) -> crate::terminal_auth_store::TerminalAuthStore {
        crate::terminal_auth_store::TerminalAuthStore::load(
            temp.path(),
            "aa11bb22cc33dd44ee55ff66aa77bb88cc99dd00ee11ff22aa33bb44cc55dd66",
        )
        .expect("fresh store loads empty")
    }

    fn terminal_auth_error() -> acp::AcpError {
        acp::AcpError::TerminalAuth(terminal_auth::TerminalAuth {
            adapter: terminal_auth::AdapterFamily::Claude,
            stage: terminal_auth::AuthStage::Prompt,
            signal: terminal_auth::AuthSignal::ClaudeApiUnauthorized,
        })
    }

    // ── terminal-auth disposition behavior ─────────────────────────────────

    /// A terminal-auth `PromptOutcome::Error` must dispose of its batch
    /// durably and immediately: never requeued, never retried, and never
    /// revivable.
    #[tokio::test]
    async fn auth_error_dead_letters_immediately_without_requeueing() {
        let keys = nostr::Keys::generate();
        let event = nostr::EventBuilder::new(nostr::Kind::Custom(9), "test")
            .sign_with_keys(&keys)
            .unwrap();
        let event_id = event.id.to_hex();
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

        let auth_error = terminal_auth_error();

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
                successful_steer_deliveries: HashSet::new(),
            },
        );
        let temp = tempfile::tempdir().expect("temp dir");
        let mut queue = EventQueue::new(config::DedupMode::Queue);
        queue.attach_terminal_auth_store(test_terminal_auth_store(&temp));
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
        let action = handle_prompt_result(
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
        assert!(matches!(action, LoopAction::Continue));

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
        assert!(
            !queue.is_channel_in_flight(channel_id),
            "in-flight ownership must be released after the disposition commits"
        );
        assert!(
            queue.is_terminally_disposed(&event_id),
            "the event must carry a durable disposition"
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
                successful_steer_deliveries: HashSet::new(),
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

    /// Everything a genuine in-flight batch needs to reach
    /// `handle_prompt_result` on a specific attempt.
    struct DispositionHarness {
        pool: AgentPool,
        queue: EventQueue,
        config: Config,
        crash_history: Vec<SlotCircuit>,
        respawn_tx: mpsc::Sender<RespawnResult>,
        _respawn_rx: mpsc::Receiver<RespawnResult>,
        respawn_tasks: tokio::task::JoinSet<()>,
        channel_id: Uuid,
        event_ids: Vec<String>,
    }

    impl DispositionHarness {
        /// Seed a batch that has already failed `prior_attempts` times and is
        /// currently in flight, exactly as the dispatch path leaves it.
        async fn seeded(
            temp: Option<&tempfile::TempDir>,
            dedup: config::DedupMode,
            prior_attempts: u32,
        ) -> (Self, FlushBatch) {
            let channel_id = Uuid::new_v4();
            let mut queue = EventQueue::new(dedup);
            if let Some(temp) = temp {
                queue.attach_terminal_auth_store(test_terminal_auth_store(temp));
            }

            let keys = nostr::Keys::generate();
            let mut event_ids = Vec::new();
            for i in 0..2 {
                let event =
                    nostr::EventBuilder::new(nostr::Kind::Custom(9), format!("request {i}"))
                        .sign_with_keys(&keys)
                        .unwrap();
                event_ids.push(event.id.to_hex());
                assert!(queue.push(QueuedEvent {
                    channel_id,
                    event,
                    received_at: std::time::Instant::now(),
                    prompt_tag: "test".into(),
                    project: None,
                }));
            }

            // Dispatch it for real so the channel is genuinely in flight, then
            // stamp the attempt count the failure is supposed to happen on.
            let batch = queue.flush_next().expect("batch dispatches");
            queue.set_retry_count_for_test(channel_id, prior_attempts);
            assert!(queue.is_channel_in_flight(channel_id));

            let mut pool = AgentPool::from_slots(vec![None]);
            let task_id = pool.join_set.spawn(async {}).id();
            pool.task_map_mut().insert(
                task_id,
                crate::pool::TaskMeta {
                    agent_index: 0,
                    channel_id: Some(channel_id),
                    turn_id: "disposition-turn".to_string(),
                    recoverable_batch: None,
                    control_tx: None,
                    steer_tx: None,
                    successful_steer_deliveries: HashSet::new(),
                },
            );

            let (respawn_tx, _respawn_rx) = mpsc::channel(8);
            (
                Self {
                    pool,
                    queue,
                    config: test_config(),
                    crash_history: vec![SlotCircuit {
                        crash_times: Vec::new(),
                        open_until: None,
                        respawn_in_flight: false,
                    }],
                    respawn_tx,
                    _respawn_rx,
                    respawn_tasks: tokio::task::JoinSet::new(),
                    channel_id,
                    event_ids,
                },
                batch,
            )
        }

        async fn run(&mut self, batch: FlushBatch, outcome: PromptOutcome) -> LoopAction {
            let agent = dummy_agent(0).await;
            let mut heartbeat_in_flight = false;
            let removed_channels = std::collections::HashSet::new();
            handle_prompt_result(
                &mut self.pool,
                &mut self.queue,
                &self.config,
                PromptResult {
                    agent,
                    source: PromptSource::Channel(self.channel_id),
                    turn_id: "disposition-turn".to_string(),
                    outcome,
                    batch: Some(batch),
                },
                &mut heartbeat_in_flight,
                &removed_channels,
                &mut self.crash_history,
                &self.respawn_tx,
                &mut self.respawn_tasks,
                None,
                None,
            )
        }
    }

    /// A batch that has already burned six attempts and fails on the seventh
    /// with terminal auth must finish there: nothing queued, nothing in
    /// flight, no retry metadata, and a durable disposition for every event.
    #[tokio::test]
    async fn a_genuine_in_flight_batch_is_terminally_disposed_on_attempt_seven() {
        for dedup in [config::DedupMode::Queue, config::DedupMode::Drop] {
            let temp = tempfile::tempdir().expect("temp dir");
            let (mut harness, batch) = DispositionHarness::seeded(Some(&temp), dedup, 6).await;
            let channel_id = harness.channel_id;
            let event_ids = harness.event_ids.clone();

            let action = harness
                .run(batch, PromptOutcome::Error(terminal_auth_error()))
                .await;

            assert!(matches!(action, LoopAction::Continue), "{dedup:?}");
            assert_eq!(harness.queue.pending_channels(), 0, "{dedup:?}");
            assert_eq!(
                harness.queue.queued_event_count(&channel_id),
                0,
                "{dedup:?}"
            );
            assert!(
                !harness.queue.is_channel_in_flight(channel_id),
                "{dedup:?}: in-flight ownership must be released"
            );
            assert_eq!(
                harness.queue.retry_count_for_test(channel_id),
                None,
                "{dedup:?}: no retry metadata may survive a terminal disposition"
            );
            for id in &event_ids {
                assert!(
                    harness.queue.is_terminally_disposed(id),
                    "{dedup:?}: {id} must be durably disposed"
                );
            }
            assert!(
                harness.pool.task_map().is_empty(),
                "{dedup:?}: the task map entry must be cleared"
            );

            // Simulated auth recovery: a fresh runtime over the same durable
            // state cannot redispatch any of it.
            let mut recovered = EventQueue::new(dedup);
            recovered.attach_terminal_auth_store(test_terminal_auth_store(&temp));
            for id in &event_ids {
                assert!(recovered.is_terminally_disposed(id), "{dedup:?}");
            }
            assert!(recovered.flush_next().is_none(), "{dedup:?}");
        }
    }

    /// The transient mirror of the test above: the same batch, same attempt,
    /// an ordinary provider error. It must requeue once, advance to attempt
    /// eight, and keep its backoff.
    #[tokio::test]
    async fn the_transient_mirror_requeues_once_and_advances_the_attempt() {
        let temp = tempfile::tempdir().expect("temp dir");
        let (mut harness, batch) =
            DispositionHarness::seeded(Some(&temp), config::DedupMode::Queue, 6).await;
        let channel_id = harness.channel_id;
        let event_ids = harness.event_ids.clone();

        let action = harness
            .run(
                batch,
                PromptOutcome::Error(acp::AcpError::AgentError {
                    code: -32000,
                    message: "API Error: 500 internal server error".to_string(),
                }),
            )
            .await;

        assert!(matches!(action, LoopAction::Continue));
        assert_eq!(
            harness.queue.queued_event_count(&channel_id),
            2,
            "a transient failure must preserve the events for retry"
        );
        assert_eq!(
            harness.queue.retry_count_for_test(channel_id),
            Some(7),
            "the attempt counter must advance from six to seven"
        );
        assert!(
            harness.queue.retry_deadline_for_test(channel_id).is_some(),
            "the backoff deadline must be preserved"
        );
        for id in &event_ids {
            assert!(
                !harness.queue.is_terminally_disposed(id),
                "a transient failure must not dispose of anything"
            );
        }
    }

    /// When the durable commit cannot be made, the harness must say nothing
    /// and stop. A notice or a completion here would claim a promise we do
    /// not hold.
    #[tokio::test]
    async fn a_failed_persistence_exits_without_notice_or_completion() {
        // No store attached — the same observable situation as a failed write.
        let (mut harness, batch) =
            DispositionHarness::seeded(None, config::DedupMode::Queue, 0).await;
        let channel_id = harness.channel_id;

        let action = harness
            .run(batch, PromptOutcome::Error(terminal_auth_error()))
            .await;

        assert!(
            matches!(action, LoopAction::Exit),
            "an un-promisable disposition must stop the harness"
        );
        assert!(
            harness.queue.is_channel_in_flight(channel_id),
            "the channel must NOT be marked complete when the disposition failed"
        );
    }

    // ── auth-required: saying so instead of going quiet ────────────────────
    //
    // An expired provider credential used to be answered with silence: the
    // turn failed, the batch was disposed of, and the person who mentioned the
    // agent never learned why nothing happened. These pin the two things that
    // now happen instead, and — just as importantly — how few times they
    // happen.

    /// Detection is the already-typed ACP classification, walked end to end.
    ///
    /// The seam matters more than either half: a shape that classifies but
    /// does not survive into [`terminal_auth_of`] notifies nobody, and a shape
    /// that reaches `terminal_auth_of` without having been classified would
    /// mean the harness was re-deriving auth-ness from prose.
    #[test]
    fn an_auth_failure_is_recognised_end_to_end_and_nothing_else_is() {
        let claude = terminal_auth::AdapterIdentity::from_command("claude-agent-acp");
        let classified = |error: serde_json::Value| {
            terminal_auth::classify_jsonrpc_error(&error, &claude, terminal_auth::AuthStage::Prompt)
                .map(|terminal| PromptOutcome::Error(acp::AcpError::TerminalAuth(terminal)))
                .unwrap_or_else(|| {
                    PromptOutcome::Error(acp::AcpError::AgentError {
                        code: error.get("code").and_then(|c| c.as_i64()).unwrap_or(-32000),
                        message: error
                            .get("message")
                            .and_then(|m| m.as_str())
                            .unwrap_or_default()
                            .to_string(),
                    })
                })
        };

        // Positive: the structured ACP signal in either accepted shape, and
        // the Claude prose the deployed adapter actually emits.
        for error in [
            serde_json::json!({ "code": -32000, "message": "Authentication required" }),
            serde_json::json!({ "code": -32603, "data": { "type": "auth_required" } }),
            serde_json::json!({ "code": 1, "data": { "authRequired": true } }),
            serde_json::json!({
                "code": -32603,
                "message": "API Error: 401 {\"type\":\"error\"}",
            }),
        ] {
            assert!(
                terminal_auth_of(&classified(error.clone())).is_some(),
                "an auth failure went unrecognised: {error}"
            );
        }

        // Negative: a bare -32000, another service's expired token relayed
        // through the same channel, a rate limit, and an ordinary tool
        // failure. Announcing a lockout for any of these would tell the root
        // its agent is broken when it is merely busy or unlucky.
        for error in [
            serde_json::json!({ "code": -32000, "message": "something went wrong" }),
            serde_json::json!({
                "code": -32603,
                "message": "GitHub OAuth access token has expired. Re-authenticate to continue.",
            }),
            serde_json::json!({ "code": -32000, "message": "rate limit exceeded (429)" }),
            serde_json::json!({ "code": -32602, "message": "unknown tool" }),
            serde_json::json!({ "code": -32000, "data": { "type": "tool_error" } }),
        ] {
            assert!(
                terminal_auth_of(&classified(error.clone())).is_none(),
                "an ordinary failure was misread as a lockout: {error}"
            );
        }
    }

    fn a_terminal_auth() -> terminal_auth::TerminalAuth {
        terminal_auth::TerminalAuth {
            adapter: terminal_auth::AdapterFamily::Claude,
            stage: terminal_auth::AuthStage::Prompt,
            signal: terminal_auth::AuthSignal::ClaudeOauthUnrefreshable,
        }
    }

    /// An episode speaks once and then holds its peace.
    #[test]
    fn an_episode_claims_each_notification_exactly_once() {
        let mut episode = AuthEpisode::default();
        episode.observe_failure(a_terminal_auth());
        assert!(
            episode.claim_public_notice(),
            "the first failure must speak"
        );
        assert!(episode.claim_owner_frame(), "the owner must be told");

        for _ in 0..9 {
            episode.observe_failure(a_terminal_auth());
            assert!(
                !episode.claim_public_notice(),
                "a second mention re-announced the same outage"
            );
            assert!(
                !episode.claim_owner_frame(),
                "the owner was told twice about one outage"
            );
        }
    }

    /// A good turn ends the episode, and a later expiry is a new one.
    ///
    /// Success is the reset rather than a timer because it is the only direct
    /// evidence the credential works. An agent re-authenticated on Monday and
    /// expired again on Friday must be able to say so again.
    #[test]
    fn a_successful_turn_closes_an_episode_and_re_arms_the_notices() {
        let mut episode = AuthEpisode::default();
        episode.observe_failure(a_terminal_auth());
        assert!(episode.claim_public_notice());
        assert!(episode.claim_owner_frame());

        assert!(
            episode.resolve(),
            "an open episode must report that it closed"
        );
        assert!(
            !episode.resolve(),
            "a second success must not report a second recovery"
        );

        episode.observe_failure(a_terminal_auth());
        assert!(
            episode.claim_public_notice(),
            "a fresh outage after a recovery must be announced"
        );
        assert!(episode.claim_owner_frame());
    }

    /// A batch that never had a public surface still has to be counted.
    fn auth_batch(origin: Option<crate::project::ProjectOrigin>) -> FlushBatch {
        let event = nostr::EventBuilder::new(nostr::Kind::Custom(9), "please look at this")
            .sign_with_keys(&Keys::generate())
            .expect("sign");
        FlushBatch {
            channel_id: Uuid::new_v4(),
            events: vec![BatchEvent {
                event,
                prompt_tag: "test".into(),
                received_at: std::time::Instant::now(),
                project: origin,
            }],
            cancelled_events: vec![],
            cancel_reason: None,
        }
    }

    /// Fails the same pool over and over, which is exactly what an expired
    /// credential does. The interesting assertion is always about the second
    /// failure, so the harness has to survive the first.
    struct AuthEpisodeHarness {
        pool: AgentPool,
        queue: EventQueue,
        config: Config,
        crash_history: Vec<SlotCircuit>,
        respawn_tx: mpsc::Sender<RespawnResult>,
        _respawn_rx: mpsc::Receiver<RespawnResult>,
        respawn_tasks: tokio::task::JoinSet<()>,
        observer: ObserverHandle,
        _temp: tempfile::TempDir,
    }

    impl AuthEpisodeHarness {
        fn new() -> Self {
            let temp = tempfile::tempdir().expect("temp dir");
            let mut queue = EventQueue::new(config::DedupMode::Queue);
            queue.attach_terminal_auth_store(test_terminal_auth_store(&temp));
            let (respawn_tx, _respawn_rx) = mpsc::channel(8);
            Self {
                pool: AgentPool::from_slots(vec![None]),
                queue,
                config: test_config(),
                crash_history: vec![SlotCircuit {
                    crash_times: Vec::new(),
                    open_until: None,
                    respawn_in_flight: false,
                }],
                respawn_tx,
                _respawn_rx,
                respawn_tasks: tokio::task::JoinSet::new(),
                observer: ObserverHandle::in_process(),
                _temp: temp,
            }
        }

        async fn run(&mut self, batch: Option<FlushBatch>, outcome: PromptOutcome) {
            let agent = dummy_agent(0).await;
            let channel_id = batch
                .as_ref()
                .map(|b| b.channel_id)
                .unwrap_or_else(Uuid::new_v4);
            // `handle_prompt_result` accounts for exactly one in-flight task
            // per completing agent, so every turn re-registers one.
            let task_id = self.pool.join_set.spawn(async {}).id();
            self.pool.task_map_mut().insert(
                task_id,
                crate::pool::TaskMeta {
                    agent_index: 0,
                    channel_id: Some(channel_id),
                    turn_id: "auth-turn".to_string(),
                    recoverable_batch: None,
                    control_tx: None,
                    steer_tx: None,
                    successful_steer_deliveries: HashSet::new(),
                },
            );
            let mut heartbeat_in_flight = false;
            let removed_channels = std::collections::HashSet::new();
            handle_prompt_result(
                &mut self.pool,
                &mut self.queue,
                &self.config,
                PromptResult {
                    agent,
                    source: PromptSource::Channel(channel_id),
                    turn_id: "auth-turn".to_string(),
                    outcome,
                    batch,
                },
                &mut heartbeat_in_flight,
                &removed_channels,
                &mut self.crash_history,
                &self.respawn_tx,
                &mut self.respawn_tasks,
                Some(self.observer.clone()),
                None,
            );
        }

        fn owner_frames(&self) -> Vec<observer::ObserverEvent> {
            self.observer
                .snapshot()
                .into_iter()
                .filter(|event| event.kind == OBSERVER_AUTH_REQUIRED)
                .collect()
        }
    }

    /// Ten mentions to an expired agent produce one notification, not ten.
    #[tokio::test]
    async fn a_repeatedly_failing_credential_notifies_the_owner_once() {
        let mut harness = AuthEpisodeHarness::new();
        for _ in 0..10 {
            harness
                .run(
                    Some(auth_batch(None)),
                    PromptOutcome::Error(terminal_auth_error()),
                )
                .await;
        }
        assert_eq!(
            harness.owner_frames().len(),
            1,
            "an expired agent spammed its owner once per failed turn"
        );
    }

    /// The episode is bounded by recovery, not by process lifetime.
    #[tokio::test]
    async fn a_successful_turn_lets_the_next_outage_be_announced_again() {
        let mut harness = AuthEpisodeHarness::new();
        harness
            .run(
                Some(auth_batch(None)),
                PromptOutcome::Error(terminal_auth_error()),
            )
            .await;
        assert_eq!(harness.owner_frames().len(), 1);

        harness
            .run(None, PromptOutcome::Ok(acp::StopReason::EndTurn))
            .await;
        harness
            .run(
                Some(auth_batch(None)),
                PromptOutcome::Error(terminal_auth_error()),
            )
            .await;
        assert_eq!(
            harness.owner_frames().len(),
            2,
            "a credential that expired again after a recovery stayed silent"
        );
    }

    /// An ordinary failure opens no episode and notifies nobody.
    #[tokio::test]
    async fn an_ordinary_failure_never_claims_an_auth_notification() {
        let mut harness = AuthEpisodeHarness::new();
        harness
            .run(
                Some(auth_batch(None)),
                PromptOutcome::Error(acp::AcpError::AgentError {
                    code: -32000,
                    message: "OAuth access token has expired. Re-authenticate to continue."
                        .to_string(),
                }),
            )
            .await;
        assert!(
            harness.owner_frames().is_empty(),
            "an untyped agent error was announced as a credential lockout"
        );
    }

    /// The owner's frame is the only one of the two that names the method.
    #[test]
    fn the_owner_frame_carries_the_advertised_auth_method() {
        let payload = auth_required_owner_payload(
            a_terminal_auth(),
            &[acp::AuthMethod {
                id: "claude-ai-login".into(),
                label: "Log in with Claude Code".into(),
            }],
            Some(serde_json::json!({ "surface": "channel", "channelId": "c" })),
        );
        assert_eq!(payload["signal"], "claude_oauth_unrefreshable");
        assert_eq!(payload["authMethods"][0]["id"], "claude-ai-login");
        assert_eq!(
            payload["authMethods"][0]["label"],
            "Log in with Claude Code"
        );
        assert_eq!(payload["publicNotice"]["surface"], "channel");

        // No public surface is a fact the owner needs: it distinguishes "I told
        // them and you" from "I could only tell you".
        let heartbeat = auth_required_owner_payload(a_terminal_auth(), &[], None);
        assert!(heartbeat["publicNotice"].is_null());
        assert_eq!(heartbeat["authMethods"].as_array().map(Vec::len), Some(0));
    }

    /// A project turn answers on its root, addressed so the asker is told.
    #[test]
    fn a_project_turns_notice_is_addressed_to_the_root_it_came_from() {
        let coordinate = format!("30617:{}:buzz", "a".repeat(64));
        let root = "48be1cc2000000000000000000000000000000000000000000000000000000ab";
        let batch = auth_batch(Some(crate::project::ProjectOrigin::for_test(
            &coordinate,
            root,
            false,
        )));
        let asker = batch.events[0].event.pubkey.to_hex();
        let comment_id = batch.events[0].event.id.to_hex();

        let AuthNoticeTarget::ProjectRoot { coordinate, meta } = auth_notice_target_for(&batch)
        else {
            panic!("a project batch was routed to a channel that does not exist");
        };
        assert_eq!(meta.root_event, root);
        assert_eq!(meta.parent_event.as_deref(), Some(comment_id.as_str()));
        assert_eq!(
            meta.recipients,
            vec![asker],
            "an untagged comment is one the asker is never told about"
        );

        let event = build_auth_required_comment(&coordinate, &meta)
            .expect("the comment must build")
            .sign_with_keys(&Keys::generate())
            .expect("sign");
        let tag_values = |key: &str| -> Vec<String> {
            event
                .tags
                .iter()
                .filter_map(|t| {
                    let s = t.as_slice();
                    (s.first().map(String::as_str) == Some(key))
                        .then(|| s.get(1).cloned())
                        .flatten()
                })
                .collect()
        };
        assert_eq!(tag_values("a"), vec![coordinate]);
        assert!(tag_values("e").contains(&root.to_string()));
    }

    /// A channel turn keeps the channel path, because a project root's
    /// addressing would name a repository the channel does not have.
    #[test]
    fn a_channel_turns_notice_stays_on_the_channel() {
        let batch = auth_batch(None);
        let channel_id = batch.channel_id;
        let AuthNoticeTarget::Channel {
            channel_id: got, ..
        } = auth_notice_target_for(&batch)
        else {
            panic!("a channel batch was addressed as a project root");
        };
        assert_eq!(got, channel_id);
    }

    /// The public notice says what happened and nothing about how to fix it.
    ///
    /// This is the trust boundary of the whole change: the frame that names the
    /// authentication method is encrypted to the owner, and this one — readable
    /// by everyone who can read the issue — must not leak the method id, the
    /// provider, a login URL or a command to run.
    #[test]
    fn the_public_notice_discloses_no_method_url_or_command() {
        let notice = AUTH_REQUIRED_PUBLIC_NOTICE.to_ascii_lowercase();
        for forbidden in [
            "http",
            "://",
            "www.",
            ".com",
            "login",
            "claude",
            "anthropic",
            "codex",
            "goose",
            "oauth",
            "token",
            "api key",
            "device",
            "`",
            "--",
            "/",
        ] {
            assert!(
                !notice.contains(forbidden),
                "the public notice leaks {forbidden:?}: {AUTH_REQUIRED_PUBLIC_NOTICE}"
            );
        }
        // And it still says the two things the asker needs.
        assert!(notice.contains("re-authentication"));
        assert!(notice.contains("owner has been notified"));
        assert!(
            AUTH_REQUIRED_PUBLIC_NOTICE.chars().count() < 200,
            "a notice nobody reads is the same as no notice"
        );
    }
}

#[cfg(test)]
mod observer_payload_trim_tests {
    use super::*;

    fn event_with_payload(kind: &str, payload: serde_json::Value) -> observer::ObserverEvent {
        observer::ObserverEvent {
            project: None,
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
            "[Agent Instructions]\npersona text".to_string(),
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
            "[Agent Instructions]",
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

/// NIP-PA: what a project turn announces on its root, and when it stops.
///
/// [`ProjectActivityPublisher::ingest`] is the whole decision — refresh, dedup,
/// stage carry-over and the terminal rule — so these drive it directly with the
/// observer events the pool really emits, and read the *signed events* it
/// produces rather than its internal map.
#[cfg(test)]
mod project_activity_tests {
    use super::*;
    use nostr::Keys;

    const ROOT: &str = "48be1cc2000000000000000000000000000000000000000000000000000000ab";
    const OTHER_ROOT: &str = "48be1cc2000000000000000000000000000000000000000000000000000000cd";
    /// The comment event id a queued announcement is named after.
    const COMMENT: &str = "9f3a0000000000000000000000000000000000000000000000000000000000ef";
    const OTHER_COMMENT: &str = "9f3a000000000000000000000000000000000000000000000000000000000012";

    fn coordinate() -> String {
        format!("30617:{}:buzz", "a".repeat(64))
    }

    /// An observer frame exactly as the pool emits one for a project turn.
    fn project_frame(kind: &str, root: &str, turn: &str) -> observer::ObserverEvent {
        observer::ObserverEvent {
            seq: 1,
            timestamp: "2026-08-02T00:00:00Z".to_string(),
            kind: kind.to_string(),
            agent_index: Some(0),
            channel_id: None,
            project: Some(observer::ProjectRouteRef {
                coordinate: coordinate(),
                root: root.to_string(),
            }),
            session_id: Some("sess".to_string()),
            turn_id: Some(turn.to_string()),
            started_at: Some("2026-08-02T00:00:00Z".to_string()),
            payload: serde_json::json!({}),
        }
    }

    /// An `acp_read` frame carrying one ACP `session/update`, in the exact
    /// envelope [`AcpClient::publish_inbound`] puts on the bus: the whole
    /// JSON-RPC notification, unwrapped by nothing.
    fn session_update_frame(
        root: &str,
        turn: &str,
        update: serde_json::Value,
    ) -> observer::ObserverEvent {
        observer::ObserverEvent {
            payload: serde_json::json!({
                "jsonrpc": "2.0",
                "method": "session/update",
                "params": { "sessionId": "sess", "update": update },
            }),
            ..project_frame("acp_read", root, turn)
        }
    }

    /// The frame an agent produces when it starts a tool call.
    fn tool_call_frame(root: &str, turn: &str, title: &str, kind: &str) -> observer::ObserverEvent {
        session_update_frame(
            root,
            turn,
            serde_json::json!({
                "sessionUpdate": "tool_call",
                "toolCallId": "call-1",
                "title": title,
                "kind": kind,
                "status": "in_progress",
            }),
        )
    }

    /// The frame the dispatch gate emits when it queues a comment.
    ///
    /// Built through [`observe_project_event_queued`] rather than by hand, so
    /// these cases are driven by the production emitter: a queued frame that
    /// stopped carrying its route, or its `queued:` turn id, would fail here
    /// rather than pass against a fixture that still described the old shape.
    fn queued_frame(root: &str, event_id: &str) -> observer::ObserverEvent {
        let bus = observer::ObserverHandle::in_process();
        let mut rx = bus.subscribe();
        observe_project_event_queued(
            Some(&bus),
            &crate::project::ProjectOrigin::for_test(&coordinate(), root, false),
            event_id,
        );
        rx.try_recv().expect("the gate emitted nothing")
    }

    /// A channel turn's frame: the control for every project assertion below.
    fn channel_frame(kind: &str) -> observer::ObserverEvent {
        observer::ObserverEvent {
            channel_id: Some("11111111-1111-1111-1111-111111111111".to_string()),
            project: None,
            ..project_frame(kind, ROOT, "turn-1")
        }
    }

    fn publisher() -> (ProjectActivityPublisher, Keys) {
        let keys = Keys::generate();
        let hex = keys.public_key().to_hex().to_ascii_lowercase();
        (ProjectActivityPublisher::new(keys.clone(), hex), keys)
    }

    fn signed(keys: &Keys, builders: Vec<nostr::EventBuilder>) -> Vec<nostr::Event> {
        builders
            .into_iter()
            .map(|b| b.sign_with_keys(keys).expect("sign"))
            .collect()
    }

    fn tag_of(event: &nostr::Event, key: &str) -> Option<String> {
        event.tags.iter().find_map(|t| {
            let s = t.as_slice();
            (s.first().map(String::as_str) == Some(key))
                .then(|| s.get(1).cloned())
                .flatten()
        })
    }

    /// A project turn announces `working` on its own root, with no `h`.
    #[test]
    fn a_project_turn_announces_working_on_its_root() {
        let (mut state, keys) = publisher();
        let now = tokio::time::Instant::now();

        let events = signed(
            &keys,
            state.ingest(&project_frame("turn_started", ROOT, "t1"), now),
        );
        assert_eq!(events.len(), 1, "a started project turn must announce once");
        let event = &events[0];

        assert_eq!(
            event.kind.as_u16(),
            buzz_core::kind::KIND_PROJECT_ACTIVITY as u16
        );
        assert_eq!(tag_of(event, "a").as_deref(), Some(coordinate().as_str()));
        assert_eq!(tag_of(event, "e").as_deref(), Some(ROOT));
        assert_eq!(tag_of(event, "state").as_deref(), Some("working"));
        assert_eq!(tag_of(event, "turn").as_deref(), Some("t1"));
        assert_eq!(
            tag_of(event, "agent").as_deref(),
            Some(keys.public_key().to_hex().to_ascii_lowercase().as_str())
        );
        assert_eq!(
            tag_of(event, "h"),
            None,
            "an issue is not a channel: an `h` here routes the signal where nobody is looking"
        );
        // The root `e` is marked, so a client that groups by root sees it.
        assert!(event.tags.iter().any(|t| {
            let s = t.as_slice();
            s.first().map(String::as_str) == Some("e")
                && s.get(3).map(String::as_str) == Some("root")
        }));
    }

    /// A channel turn announces nothing. This is the regression control: the
    /// existing channel activity route is untouched by this phase, and a
    /// publisher that fired on channel frames would put every ordinary turn on
    /// a project wire.
    #[test]
    fn a_channel_turn_announces_no_project_activity() {
        let (mut state, _keys) = publisher();
        let now = tokio::time::Instant::now();
        for kind in ["turn_started", "acp_read", "turn_completed"] {
            assert!(
                state.ingest(&channel_frame(kind), now).is_empty(),
                "{kind} on a channel turn must not reach the project wire"
            );
        }
    }

    /// The indicator clears when the turn ends.
    #[test]
    fn a_finished_turn_clears_its_own_root() {
        let (mut state, keys) = publisher();
        let now = tokio::time::Instant::now();
        state.ingest(&project_frame("turn_started", ROOT, "t1"), now);

        let events = signed(
            &keys,
            state.ingest(&project_frame("turn_completed", ROOT, "t1"), now),
        );
        assert_eq!(events.len(), 1);
        assert_eq!(tag_of(&events[0], "state").as_deref(), Some("idle"));
        assert_eq!(tag_of(&events[0], "e").as_deref(), Some(ROOT));

        // Nothing is left to refresh: a cleared root must not come back to life
        // on the next tick.
        assert!(state.refresh(now + PROJECT_ACTIVITY_REFRESH * 2).is_empty());
    }

    /// A late terminal frame from a turn that already ended must not clear the
    /// turn running now. Without the `turn` check the agent goes dark on an
    /// issue it is actively working.
    #[test]
    fn a_stale_terminal_frame_does_not_clear_the_current_turn() {
        let (mut state, keys) = publisher();
        let now = tokio::time::Instant::now();
        state.ingest(&project_frame("turn_started", ROOT, "t1"), now);
        state.ingest(&project_frame("turn_completed", ROOT, "t1"), now);
        state.ingest(&project_frame("turn_started", ROOT, "t2"), now);

        assert!(
            state
                .ingest(&project_frame("turn_completed", ROOT, "t1"), now)
                .is_empty(),
            "the previous turn's completion cleared the turn running now"
        );
        let refreshed = signed(&keys, state.refresh(now + PROJECT_ACTIVITY_REFRESH));
        assert_eq!(refreshed.len(), 1, "t2 is still live and still refreshing");
        assert_eq!(tag_of(&refreshed[0], "turn").as_deref(), Some("t2"));
    }

    /// One root's activity does not appear on another.
    #[test]
    fn two_roots_keep_their_own_activity() {
        let (mut state, keys) = publisher();
        let now = tokio::time::Instant::now();
        state.ingest(&project_frame("turn_started", ROOT, "t1"), now);
        state.ingest(&project_frame("turn_started", OTHER_ROOT, "t2"), now);

        let cleared = signed(
            &keys,
            state.ingest(&project_frame("turn_completed", ROOT, "t1"), now),
        );
        assert_eq!(cleared.len(), 1);
        assert_eq!(tag_of(&cleared[0], "e").as_deref(), Some(ROOT));

        let refreshed = signed(&keys, state.refresh(now + PROJECT_ACTIVITY_REFRESH));
        assert_eq!(
            refreshed.len(),
            1,
            "finishing one root must leave the other announcing"
        );
        assert_eq!(tag_of(&refreshed[0], "e").as_deref(), Some(OTHER_ROOT));
    }

    /// The gap this phase closes: a comment is accepted, and the issue says so
    /// before any agent process exists.
    ///
    /// Until this frame, "an agent picked this up and is waiting for a slot"
    /// and "nobody was addressed" produced the identical wire — nothing — so a
    /// person could not tell them apart until a turn started, which on a busy
    /// pool is minutes.
    #[test]
    fn a_queued_event_announces_queued_before_any_turn_exists() {
        let (mut state, keys) = publisher();
        let now = tokio::time::Instant::now();

        let events = signed(&keys, state.ingest(&queued_frame(ROOT, COMMENT), now));
        assert_eq!(events.len(), 1, "queueing must reach the root promptly");
        let event = &events[0];

        assert_eq!(
            event.kind.as_u16(),
            buzz_core::kind::KIND_PROJECT_ACTIVITY as u16
        );
        assert_eq!(tag_of(event, "state").as_deref(), Some("queued"));
        assert_eq!(tag_of(event, "e").as_deref(), Some(ROOT));
        assert_eq!(tag_of(event, "a").as_deref(), Some(coordinate().as_str()));
        assert_eq!(
            tag_of(event, "turn").as_deref(),
            Some(queued_turn_id(COMMENT).as_str()),
            "a queued frame names the comment that caused it: no turn exists yet"
        );
        assert_eq!(
            tag_of(event, "stage"),
            None,
            "nothing is happening yet — a caption here would describe work that has not begun"
        );
        assert_eq!(tag_of(event, "h"), None, "an issue is still not a channel");
    }

    /// The turn that starts takes the root over, under its own turn id.
    #[test]
    fn a_started_turn_replaces_the_queued_announcement() {
        let (mut state, keys) = publisher();
        let now = tokio::time::Instant::now();
        state.ingest(&queued_frame(ROOT, COMMENT), now);

        let started = signed(
            &keys,
            state.ingest(&project_frame("turn_started", ROOT, "t1"), now),
        );
        assert_eq!(
            started.len(),
            1,
            "the turn must supersede the queued frame at once, not on the next tick"
        );
        assert_eq!(tag_of(&started[0], "state").as_deref(), Some("working"));
        assert_eq!(tag_of(&started[0], "turn").as_deref(), Some("t1"));

        let refreshed = signed(&keys, state.refresh(now + PROJECT_ACTIVITY_REFRESH));
        assert_eq!(
            refreshed.len(),
            1,
            "one root announces one thing: the queued entry outlived the turn that replaced it"
        );
        assert_eq!(tag_of(&refreshed[0], "state").as_deref(), Some("working"));
    }

    /// A comment that arrives while the agent is already working says nothing.
    ///
    /// The root is already announcing something stronger and truer about the
    /// same agent. Announcing `queued` over it would walk the indicator
    /// backwards from "working — Edit lib.rs" while it was demonstrably
    /// editing that file, and the comment is going into the same root's queue,
    /// which the running turn's own completion will flush.
    #[test]
    fn a_comment_arriving_mid_turn_does_not_walk_the_indicator_back() {
        let (mut state, _keys) = publisher();
        let now = tokio::time::Instant::now();
        state.ingest(&project_frame("turn_started", ROOT, "t1"), now);
        state.ingest(&tool_call_frame(ROOT, "t1", "Edit lib.rs", "edit"), now);

        assert!(
            state.ingest(&queued_frame(ROOT, COMMENT), now).is_empty(),
            "a second comment demoted a live turn to queued"
        );
    }

    /// `queued` does not outlive the queue it describes.
    ///
    /// Two shapes, one rule. The ordinary one is queued → working → terminal,
    /// where the turn's own id clears it. The other is a terminal for a turn
    /// this publisher never saw start — a dropped frame on a lagged bus — where
    /// the queued announcement holds no turn id the terminal could match. It
    /// must still go: a turn ending on this root drained that root's queue, so
    /// there is nothing left for the announcement to be about, and the refresh
    /// tick would otherwise keep saying `queued` forever.
    #[test]
    fn a_queued_announcement_does_not_survive_a_terminal_frame() {
        let (mut state, keys) = publisher();
        let now = tokio::time::Instant::now();

        state.ingest(&queued_frame(ROOT, COMMENT), now);
        state.ingest(&project_frame("turn_started", ROOT, "t1"), now);
        let cleared = signed(
            &keys,
            state.ingest(&project_frame("turn_completed", ROOT, "t1"), now),
        );
        assert_eq!(tag_of(&cleared[0], "state").as_deref(), Some("idle"));
        assert!(state.refresh(now + PROJECT_ACTIVITY_REFRESH * 2).is_empty());

        // The lagged-bus shape: queued, then a terminal for a turn whose start
        // never reached this publisher.
        let (mut state, keys) = publisher();
        state.ingest(&queued_frame(ROOT, COMMENT), now);
        let cleared = signed(
            &keys,
            state.ingest(&project_frame("turn_completed", ROOT, "t9"), now),
        );
        assert_eq!(cleared.len(), 1, "the queued announcement was stranded");
        assert_eq!(tag_of(&cleared[0], "state").as_deref(), Some("idle"));
        assert_eq!(
            tag_of(&cleared[0], "turn").as_deref(),
            Some(queued_turn_id(COMMENT).as_str()),
            "an idle naming the terminal's turn is ignored by every consumer — \
             it must name the announcement it is retiring"
        );
        assert!(
            state.refresh(now + PROJECT_ACTIVITY_REFRESH * 2).is_empty(),
            "a retired queued root came back on the next tick"
        );
    }

    /// A comment still waiting for a slot keeps saying so.
    ///
    /// The alternative — announce once and let the consumer's 45-second expiry
    /// cap it — fails the only case `queued` exists for: a comment waits
    /// exactly when the pool is busy, which is routinely longer than that, and
    /// the issue would fall back into the silence this state was added to break.
    #[test]
    fn a_waiting_root_keeps_announcing_queued() {
        let (mut state, keys) = publisher();
        let now = tokio::time::Instant::now();
        state.ingest(&queued_frame(ROOT, COMMENT), now);

        let refreshed = signed(&keys, state.refresh(now + PROJECT_ACTIVITY_REFRESH));
        assert_eq!(refreshed.len(), 1);
        assert_eq!(tag_of(&refreshed[0], "state").as_deref(), Some("queued"));
        assert_eq!(
            tag_of(&refreshed[0], "turn").as_deref(),
            Some(queued_turn_id(COMMENT).as_str())
        );
    }

    /// A second comment on a root already announcing `queued` is not a second
    /// announcement. The rendered state is identical, so re-announcing would
    /// buy a different `turn` tag and nothing else — at one relay publish per
    /// comment on a root that is, by definition, backlogged.
    #[test]
    fn a_second_comment_on_a_queued_root_announces_nothing_new() {
        let (mut state, _keys) = publisher();
        let now = tokio::time::Instant::now();
        assert_eq!(state.ingest(&queued_frame(ROOT, COMMENT), now).len(), 1);
        assert!(state
            .ingest(&queued_frame(ROOT, OTHER_COMMENT), now)
            .is_empty());
    }

    /// Chatter does not become a stream of announcements, but a live turn does
    /// keep re-announcing — the event is ephemeral, so silence is invisibility.
    #[test]
    fn a_live_turn_is_re_announced_periodically_and_not_on_every_frame() {
        let (mut state, _keys) = publisher();
        let now = tokio::time::Instant::now();
        assert_eq!(
            state
                .ingest(&project_frame("turn_started", ROOT, "t1"), now)
                .len(),
            1
        );
        // An unrecognised frame at the same instant says nothing new.
        assert!(state
            .ingest(&project_frame("acp_notification", ROOT, "t1"), now)
            .is_empty());

        let later = now + PROJECT_ACTIVITY_REFRESH;
        assert_eq!(
            state
                .ingest(&project_frame("acp_notification", ROOT, "t1"), later)
                .len(),
            1,
            "a turn that goes quiet must still be re-announced before it looks stale"
        );
    }

    /// A recognised stage is published, and a frame with no stage of its own
    /// does not blank the caption the agent is still under.
    #[test]
    fn a_stage_change_is_announced_and_then_carried() {
        let (mut state, keys) = publisher();
        let now = tokio::time::Instant::now();
        state.ingest(&project_frame("turn_started", ROOT, "t1"), now);

        let reading = signed(
            &keys,
            state.ingest(&tool_call_frame(ROOT, "t1", "Read AGENTS.md", "read"), now),
        );
        assert_eq!(reading.len(), 1, "a new stage is worth an announcement");
        assert_eq!(
            tag_of(&reading[0], "stage").as_deref(),
            Some("Read AGENTS.md")
        );

        let refreshed = signed(&keys, state.refresh(now + PROJECT_ACTIVITY_REFRESH));
        assert_eq!(
            tag_of(&refreshed[0], "stage").as_deref(),
            Some("Read AGENTS.md"),
            "the refresh blanked a caption the agent is still working under"
        );
    }

    /// The caption is the agent's own account of the tool it is running.
    ///
    /// This is the whole point of the change: an ACP agent already says what it
    /// is doing, in `session/update`, and every compliant agent says it the
    /// same way. Nothing here reads the agent's identity, so an agent that
    /// used to narrate its work by posting a comment per tool call gets the
    /// same live caption from the protocol it already speaks.
    #[test]
    fn the_caption_is_the_agents_own_tool_title() {
        let (mut state, keys) = publisher();
        let now = tokio::time::Instant::now();
        state.ingest(&project_frame("turn_started", ROOT, "t1"), now);

        for (title, kind, expected) in [
            ("Read AGENTS.md", "read", "Read AGENTS.md"),
            (
                "Searching files for *buzz*",
                "search",
                "Searching files for *buzz*",
            ),
        ] {
            let events = signed(
                &keys,
                state.ingest(&tool_call_frame(ROOT, "t1", title, kind), now),
            );
            assert_eq!(events.len(), 1, "a new tool is a new caption");
            assert_eq!(tag_of(&events[0], "stage").as_deref(), Some(expected));
        }
    }

    /// A titleless tool call is captioned from its ACP `kind`, and an unknown
    /// kind degrades to the honest generic rather than to silence.
    #[test]
    fn a_titleless_tool_call_is_captioned_from_its_acp_kind() {
        for (kind, expected) in [
            ("read", "reading files"),
            ("edit", "editing files"),
            ("execute", "running a command"),
            ("search", "searching"),
            ("a_kind_this_build_has_never_heard_of", "running a tool"),
        ] {
            let (mut state, keys) = publisher();
            let now = tokio::time::Instant::now();
            state.ingest(&project_frame("turn_started", ROOT, "t1"), now);

            let frame = session_update_frame(
                ROOT,
                "t1",
                serde_json::json!({ "sessionUpdate": "tool_call", "kind": kind }),
            );
            let events = signed(&keys, state.ingest(&frame, now));
            assert_eq!(events.len(), 1, "{kind} announced nothing");
            assert_eq!(tag_of(&events[0], "stage").as_deref(), Some(expected));
        }
    }

    /// Transport is not work.
    ///
    /// `acp_read` fires for every line the agent writes and `acp_write` for
    /// every line the harness writes back — neither says anything about files.
    /// Captioning them "reading files" and "editing files" put a claim about
    /// the work on the issue that was true only by coincidence: an agent that
    /// had opened no file all turn still announced `working — reading files`,
    /// and the caption flapped to `editing files` whenever the harness answered
    /// a permission request.
    #[test]
    fn transport_frames_do_not_caption_the_turn() {
        let (mut state, keys) = publisher();
        let now = tokio::time::Instant::now();
        let started = signed(
            &keys,
            state.ingest(&project_frame("turn_started", ROOT, "t1"), now),
        );
        assert_eq!(tag_of(&started[0], "stage").as_deref(), Some("starting"));

        // A JSON-RPC message that is not a session/update, and a write back to
        // the agent: both are traffic, neither is a caption.
        let request = observer::ObserverEvent {
            payload: serde_json::json!({
                "jsonrpc": "2.0",
                "id": 7,
                "method": "session/request_permission",
                "params": {},
            }),
            ..project_frame("acp_read", ROOT, "t1")
        };
        assert!(
            state.ingest(&request, now).is_empty(),
            "a permission request re-captioned the turn"
        );
        assert!(
            state
                .ingest(&project_frame("acp_write", ROOT, "t1"), now)
                .is_empty(),
            "writing to the agent's stdin re-captioned the turn"
        );

        // And the caption the turn does have was not replaced by either.
        let refreshed = signed(&keys, state.refresh(now + PROJECT_ACTIVITY_REFRESH));
        assert_eq!(tag_of(&refreshed[0], "stage").as_deref(), Some("starting"));
    }

    /// A completed tool call does not blank the caption.
    ///
    /// `tool_call_update` carries a `status`, and "completed" is not something
    /// the agent is doing. It is folded in only for a `title` an agent supplied
    /// late; a status-only update leaves the caption alone.
    #[test]
    fn a_tool_call_update_recaptions_only_when_it_names_the_tool() {
        let (mut state, keys) = publisher();
        let now = tokio::time::Instant::now();
        state.ingest(&project_frame("turn_started", ROOT, "t1"), now);
        state.ingest(&tool_call_frame(ROOT, "t1", "Read AGENTS.md", "read"), now);

        let status_only = session_update_frame(
            ROOT,
            "t1",
            serde_json::json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": "call-1",
                "status": "completed",
            }),
        );
        assert!(
            state.ingest(&status_only, now).is_empty(),
            "a finished tool call announced a caption of its own"
        );

        let named_late = session_update_frame(
            ROOT,
            "t1",
            serde_json::json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": "call-2",
                "title": "Edit lib.rs",
            }),
        );
        let events = signed(&keys, state.ingest(&named_late, now));
        assert_eq!(events.len(), 1, "a late title never reached the wire");
        assert_eq!(tag_of(&events[0], "stage").as_deref(), Some("Edit lib.rs"));
    }

    /// A title is agent-supplied free text, and it is published to everyone who
    /// can read the issue. It reaches the wire as one bounded line.
    ///
    /// Driven through a non-`execute` kind on purpose: command executions no
    /// longer publish their title at all, so the 80-character bound would go
    /// untested if this case were captioned before it reached the builder.
    #[test]
    fn an_agent_supplied_title_reaches_the_wire_as_one_bounded_line() {
        let (mut state, keys) = publisher();
        let now = tokio::time::Instant::now();
        state.ingest(&project_frame("turn_started", ROOT, "t1"), now);

        let title = format!("Searching\n\tfiles for\u{7} '{}'", "x".repeat(200));
        let events = signed(
            &keys,
            state.ingest(&tool_call_frame(ROOT, "t1", &title, "search"), now),
        );
        let stage = tag_of(&events[0], "stage").expect("no stage on the wire");
        assert!(
            !stage.chars().any(char::is_control),
            "a control character reached the issue: {stage:?}"
        );
        assert_eq!(stage.chars().count(), 80, "the caption was not bounded");
        assert!(stage.starts_with("Searching files for 'xxx"), "{stage:?}");
    }

    /// A command execution is captioned, never quoted.
    ///
    /// The `stage` rides a public kind:20003. An `execute` title is the command
    /// line, so publishing it put absolute paths, unset environment variable
    /// names and machine layout onto a root that anyone can read — and did it in
    /// a one-line indicator that truncated the whole thing to noise anyway.
    #[test]
    fn a_command_execution_is_captioned_rather_than_quoted() {
        for (title, expected) in [
            // The reported case verbatim: a wrapper, some unset variables, an
            // assignment and an absolute path. None of it reaches the root.
            (
                "env -u BUZZ_RELAY_URL -u BUZZ_PRIVATE_KEY PYTHONPATH=. \
                 /home/hermes/.local/bin/pytest -q",
                "running a command",
            ),
            // A bare program name is worth saying: it is the one token that
            // tells a reader what is happening and discloses nothing.
            ("cargo test -p buzz-acp", "running a command (cargo)"),
            ("cargo", "running a command (cargo)"),
            // A path names the machine, not the work.
            ("/usr/bin/foo --flag", "running a command"),
            ("./scripts/deploy.sh", "running a command"),
            // An assignment is environment detail with a value attached.
            ("PYTHONPATH=. pytest -q", "running a command"),
            // A wrapper says only that the real command is wrapped.
            ("sudo systemctl restart buzz-relay", "running a command"),
            // A token this long is not a program name.
            (
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa --flag",
                "running a command",
            ),
            // Shell metacharacters never survive into the caption.
            (
                "bash -lc 'echo $BUZZ_PRIVATE_KEY'",
                "running a command (bash)",
            ),
            ("$EDITOR notes.md", "running a command"),
        ] {
            let (mut state, keys) = publisher();
            let now = tokio::time::Instant::now();
            state.ingest(&project_frame("turn_started", ROOT, "t1"), now);

            let events = signed(
                &keys,
                state.ingest(&tool_call_frame(ROOT, "t1", title, "execute"), now),
            );
            assert_eq!(events.len(), 1, "{title:?} announced nothing");
            let stage = tag_of(&events[0], "stage");
            assert_eq!(stage.as_deref(), Some(expected), "for title {title:?}");
        }
    }

    /// The caption rule is scoped to command execution and nothing else.
    ///
    /// A `read` or `edit` title is the agent's own description of the work —
    /// short, already safe, and better than anything this file could synthesise
    /// — so it must keep reaching the wire unchanged.
    #[test]
    fn a_non_command_tool_keeps_its_own_title() {
        for (title, kind) in [
            ("Read /home/hermes/notes.md", "read"),
            ("Edit crates/buzz-acp/src/lib.rs", "edit"),
            ("Fetch https://example.invalid/spec", "fetch"),
        ] {
            let (mut state, keys) = publisher();
            let now = tokio::time::Instant::now();
            state.ingest(&project_frame("turn_started", ROOT, "t1"), now);

            let events = signed(
                &keys,
                state.ingest(&tool_call_frame(ROOT, "t1", title, kind), now),
            );
            assert_eq!(events.len(), 1, "{kind} announced nothing");
            assert_eq!(tag_of(&events[0], "stage").as_deref(), Some(title));
        }
    }

    /// A late-named command execution is captioned too.
    ///
    /// `tool_call_update` is the other door a title comes through, and an
    /// adapter that opens a call with a placeholder and names it on the first
    /// update would otherwise put the raw command line on the root by the back
    /// way in.
    #[test]
    fn a_command_named_by_an_update_is_captioned_too() {
        let (mut state, keys) = publisher();
        let now = tokio::time::Instant::now();
        state.ingest(&project_frame("turn_started", ROOT, "t1"), now);

        let named_late = session_update_frame(
            ROOT,
            "t1",
            serde_json::json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": "call-1",
                "kind": "execute",
                "title": "env FOO=bar /opt/buzz/bin/buzz relay status",
            }),
        );
        let events = signed(&keys, state.ingest(&named_late, now));
        assert_eq!(events.len(), 1, "the update named no caption");
        assert_eq!(
            tag_of(&events[0], "stage").as_deref(),
            Some("running a command")
        );
    }

    /// A frame with no turn id, or an unreadable coordinate, announces nothing
    /// rather than publishing an event no consumer can place.
    #[test]
    fn an_unplaceable_frame_announces_nothing() {
        let (mut state, _keys) = publisher();
        let now = tokio::time::Instant::now();

        let mut no_turn = project_frame("turn_started", ROOT, "t1");
        no_turn.turn_id = None;
        assert!(state.ingest(&no_turn, now).is_empty());

        let mut bad_coordinate = project_frame("turn_started", ROOT, "t1");
        bad_coordinate.project = Some(observer::ProjectRouteRef {
            coordinate: "not-a-coordinate".to_string(),
            root: ROOT.to_string(),
        });
        assert!(state.ingest(&bad_coordinate, now).is_empty());
    }
    /// The route a real flushed batch produces, through the production mapping.
    ///
    /// This is the join between Phase 1's routing and this phase's activity: if
    /// a project batch stopped resolving to a project route, every assertion
    /// above would still pass while the issue went dark again.
    #[test]
    fn a_project_batch_routes_its_turn_to_the_root_and_a_channel_batch_does_not() {
        let root = ROOT.to_string();
        let origin = crate::project::ProjectOrigin::for_test(&coordinate(), &root, false);
        let key = crate::project::project_route_key(&root).expect("the root keys");
        let event = nostr::EventBuilder::new(nostr::Kind::TextNote, "look at this")
            .sign_with_keys(&Keys::generate())
            .expect("sign");

        let project_batch = queue::FlushBatch {
            channel_id: key,
            events: vec![queue::BatchEvent {
                event: event.clone(),
                prompt_tag: "@mention".into(),
                received_at: std::time::Instant::now(),
                project: Some(origin),
            }],
            cancelled_events: vec![],
            cancel_reason: None,
        };
        assert_eq!(
            pool::observer_route_for(&pool::PromptSource::Channel(key), Some(&project_batch)),
            observer::TurnRoute::Project(observer::ProjectRouteRef {
                coordinate: coordinate(),
                root,
            }),
            "a project turn must not report a route key as a channel id"
        );

        let channel_id = uuid::Uuid::new_v4();
        let channel_batch = queue::FlushBatch {
            channel_id,
            events: vec![queue::BatchEvent {
                event,
                prompt_tag: "@mention".into(),
                received_at: std::time::Instant::now(),
                project: None,
            }],
            cancelled_events: vec![],
            cancel_reason: None,
        };
        assert_eq!(
            pool::observer_route_for(
                &pool::PromptSource::Channel(channel_id),
                Some(&channel_batch)
            ),
            observer::TurnRoute::Channel(channel_id),
            "an ordinary channel turn must keep reporting its channel"
        );
    }

    /// A channel turn's observer payload is byte-for-byte what it was.
    ///
    /// The desktop's existing stores read `channelId` and know nothing about
    /// `project`. Serialising a null `project` key onto every channel frame
    /// would be a silent format change on a wire that is already deployed.
    #[test]
    fn a_channel_frame_carries_no_project_key_at_all() {
        let json = serde_json::to_value(channel_frame("turn_started")).expect("serialise");
        assert!(
            json.get("project").is_none(),
            "a channel frame gained a project key: {json}"
        );
        assert_eq!(
            json.get("channelId").and_then(serde_json::Value::as_str),
            Some("11111111-1111-1111-1111-111111111111")
        );

        let json =
            serde_json::to_value(project_frame("turn_started", ROOT, "t1")).expect("serialise");
        assert_eq!(
            json.pointer("/project/root")
                .and_then(serde_json::Value::as_str),
            Some(ROOT)
        );
        assert!(
            json.get("channelId")
                .is_some_and(serde_json::Value::is_null),
            "a project turn must not claim a channel: {json}"
        );
    }
}

/// Startup wiring: which configuration allocates the observer bus, and which
/// one publishes what.
///
/// The candidate this corrects claimed project activity was "deliberately not
/// behind `--relay-observer`". The publisher was not. Its only input was: the
/// bus itself was allocated from `relay_observer` alone, so the supported
/// default — project routing on, telemetry off — emitted no `20003` at all.
///
/// These drive the production predicates with real [`Config`] values rather
/// than starting a harness, and the first case carries a real turn frame
/// through the real publisher task so "the bus exists" is not the whole claim.
#[cfg(test)]
mod observer_bus_startup_tests {
    use super::*;
    use crate::config::{test_config, SubscribeMode};

    const ROOT: &str = "48be1cc2000000000000000000000000000000000000000000000000000000ab";

    fn config_with(relay_observer: bool, project_routing: bool) -> Config {
        let mut config = test_config(SubscribeMode::All);
        config.relay_observer = relay_observer;
        config.project_routing_enabled = project_routing;
        config
    }

    /// The reported failure, as a test: project routing on and telemetry off
    /// must still produce a bus, and a real project turn frame on that bus must
    /// still reach the wire as a signed `20003`.
    #[tokio::test]
    async fn project_routing_alone_publishes_activity_from_a_real_turn_frame() {
        let config = config_with(false, true);
        let observer = observer_bus_for(&config)
            .expect("project routing needs the observer bus: it is the publisher's only input");
        assert!(
            !encrypted_telemetry_enabled(&config),
            "sharing the bus must not switch on owner telemetry"
        );

        let keys = nostr::Keys::generate();
        let agent_hex = keys.public_key().to_hex().to_ascii_lowercase();
        let (publisher, mut published) = relay::RelayEventPublisher::test_pair();
        let rx = observer.subscribe();
        let task = tokio::spawn(run_project_activity_publisher(
            rx,
            publisher,
            keys,
            agent_hex.clone(),
        ));

        // A turn frame exactly as the pool emits one, through the production
        // context builder.
        observer.emit(
            "turn_started",
            Some(0),
            &observer::context_for_turn(
                observer::TurnRoute::Project(observer::ProjectRouteRef {
                    coordinate: format!("30617:{}:buzz", "a".repeat(64)),
                    root: ROOT.to_string(),
                }),
                None,
                "turn-1".to_string(),
                "2026-08-02T00:00:00Z".to_string(),
            ),
            serde_json::json!({}),
        );

        let event = tokio::time::timeout(std::time::Duration::from_secs(5), published.recv())
            .await
            .expect("the activity publisher produced nothing within 5s")
            .expect("publisher channel closed");
        task.abort();

        assert_eq!(
            event.kind.as_u16(),
            buzz_core::kind::KIND_PROJECT_ACTIVITY as u16
        );
        let tag = |key: &str| {
            event.tags.iter().find_map(|t| {
                let s = t.as_slice();
                (s.first().map(String::as_str) == Some(key))
                    .then(|| s.get(1).cloned())
                    .flatten()
            })
        };
        assert_eq!(tag("e").as_deref(), Some(ROOT));
        assert_eq!(tag("state").as_deref(), Some("working"));
        assert_eq!(tag("agent").as_deref(), Some(agent_hex.as_str()));
    }

    /// The other half of the separation: telemetry alone keeps its bus and its
    /// publisher, and does not switch on project activity.
    #[test]
    fn telemetry_alone_keeps_its_own_path() {
        let config = config_with(true, false);
        assert!(
            observer_bus_for(&config).is_some(),
            "owner telemetry lost the bus it has always had"
        );
        assert!(encrypted_telemetry_enabled(&config));
        assert!(
            !config.project_routing_enabled,
            "sharing the bus must not switch on project routing"
        );
    }

    /// Both features on is one bus, not two.
    #[test]
    fn both_features_share_one_bus() {
        let config = config_with(true, true);
        assert!(observer_bus_for(&config).is_some());
        assert!(encrypted_telemetry_enabled(&config));
    }

    /// Neither feature: no bus at all. The bus is cheap but not free, and a
    /// deployment that asked for neither should carry neither.
    #[test]
    fn neither_feature_allocates_nothing() {
        let config = config_with(false, false);
        assert!(
            observer_bus_for(&config).is_none(),
            "an unconfigured harness allocated an observer bus nobody reads"
        );
        assert!(!encrypted_telemetry_enabled(&config));
    }
}

/// The project arm as a running system: an enrolled root, a real pool, and a
/// child that records what it was asked to do.
///
/// Everything else about project routing is proved against `handle_project_event`,
/// which is the right level for an authority decision and the wrong level for
/// these two defects. Both were failures of *composition*: dispatch worked, the
/// queue worked, the pool worked, and a project-only runtime still ran nothing —
/// because the arm never flushed — and still ran a root twice — because two
/// subscriptions each delivered it. Neither is visible from inside any of the
/// parts.
#[cfg(test)]
mod project_runtime_tests {
    use std::collections::{BTreeSet, HashMap};
    use std::sync::Arc;
    use std::time::Duration;

    use nostr::{EventBuilder, Keys};

    use super::*;
    use crate::config::{DedupMode, PermissionMode};
    use crate::pool::{ChannelInfoResolver, OwnedAgent, PromptContext};
    use crate::queue::EventQueue;
    use crate::relay::RestClient;

    /// A child that speaks just enough ACP to complete a turn, and writes every
    /// `session/prompt` it receives to a file.
    ///
    /// The file is the assertion. Counting prompts *inside* the harness would
    /// count what the harness decided to send; counting them here counts what
    /// crossed the process boundary, which is the thing the live run observed
    /// happening twice — and, before the flush existed, never.
    const RECORDING_AGENT: &str = r#"
import json, os, sys
log = os.environ["BUZZ_TEST_PROMPT_LOG"]
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    try:
        msg = json.loads(line)
    except ValueError:
        continue
    mid = msg.get("id")
    if mid is None:
        continue
    method = msg.get("method")
    if method == "session/prompt":
        with open(log, "a") as f:
            f.write(line + "\n")
        result = {"stopReason": "end_turn"}
    elif method == "session/new":
        result = {"sessionId": "test-session"}
    elif method == "initialize":
        result = {"protocolVersion": 1}
    else:
        result = {}
    sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": mid, "result": result}) + "\n")
    sys.stdout.flush()
"#;

    /// Accepts every replacement. What a REQ replacement contains is proved
    /// against `RecordingSubscriber` in the dispatch tests above; these
    /// scenarios are about what reaches the child, and a subscriber that
    /// refused would stop them before that.
    struct NoopSubscriber;

    impl ProjectSubscriber for NoopSubscriber {
        async fn submit_project_replacement(
            &self,
            _replacement: project::ProjectReplacement,
            _filters: Vec<serde_json::Value>,
        ) -> Result<(), relay::RelayError> {
            Ok(())
        }

        async fn submit_enrolment_history(
            &self,
            _coordinates: Vec<String>,
            _agent: String,
        ) -> Result<(), relay::RelayError> {
            Ok(())
        }

        async fn submit_root_catch_up(
            &self,
            _root: project::VerifiedBoundRoot,
        ) -> Result<(), relay::RelayError> {
            Ok(())
        }
    }

    /// A temp path plus the agent wired to write to it.
    struct PromptRecorder {
        path: std::path::PathBuf,
    }

    impl PromptRecorder {
        fn new() -> Self {
            let path = std::env::temp_dir()
                .join(format!("buzz-acp-prompts-{}.jsonl", uuid::Uuid::new_v4()));
            Self { path }
        }

        async fn agent(&self, index: usize) -> OwnedAgent {
            let acp = crate::acp::AcpClient::spawn(
                "python3",
                &[
                    "-u".to_string(),
                    "-c".to_string(),
                    RECORDING_AGENT.to_string(),
                ],
                &[(
                    "BUZZ_TEST_PROMPT_LOG".to_string(),
                    self.path.to_string_lossy().to_string(),
                )],
                false,
            )
            .await
            .expect("spawn recording agent");
            OwnedAgent {
                index,
                acp,
                state: Default::default(),
                model_capabilities: None,
                desired_model: None,
                model_overridden: false,
                desired_model_request_id: None,
                desired_model_pending_ack: false,
                startup_effort: None,
                agent_name: "unknown".into(),
                goose_system_prompt_supported: None,
                protocol_version: 1,
            }
        }

        /// Every `session/prompt` the child has received so far.
        ///
        /// Polled rather than read once: dispatch spawns the turn on the pool's
        /// join set, so the prompt crosses the pipe after the call returns.
        async fn prompts(&self, at_least: usize) -> Vec<String> {
            for _ in 0..200 {
                let seen = self.read();
                if seen.len() >= at_least {
                    return seen;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            self.read()
        }

        fn read(&self) -> Vec<String> {
            std::fs::read_to_string(&self.path)
                .unwrap_or_default()
                .lines()
                .filter(|l| !l.trim().is_empty())
                .map(str::to_string)
                .collect()
        }
    }

    impl Drop for PromptRecorder {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    /// A relay that answers everything with an empty array, immediately.
    ///
    /// Not decoration. A project turn's first pass still runs the channel-info
    /// resolve and canvas fetch that every channel turn runs — keyed on a route
    /// key that names no channel, so both are always empty. Pointed at a closed
    /// port those two doomed requests spend their full connect timeout, which
    /// is ten seconds of a test proving nothing about connect timeouts.
    async fn empty_relay() -> String {
        use axum::{Json, Router};
        let app = Router::new().fallback(|| async { Json(serde_json::json!([])) });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });
        format!("http://{addr}")
    }

    fn test_ctx(agent_keys: &Keys, base_url: String) -> Arc<PromptContext> {
        test_ctx_with(agent_keys, base_url, 0, Vec::new())
    }

    /// A prompt context with the two knobs a project turn's context reads.
    ///
    /// `context_message_limit` is the window on the root's conversation and
    /// `peer_agents` is the roster; both default to off in [`test_ctx`], which
    /// is the configuration every pre-existing scenario was written against.
    fn test_ctx_with(
        agent_keys: &Keys,
        base_url: String,
        context_message_limit: u32,
        peer_agents: Vec<String>,
    ) -> Arc<PromptContext> {
        let rest = || RestClient {
            http: reqwest::Client::new(),
            base_url: base_url.clone(),
            keys: agent_keys.clone(),
            auth_tag_json: None,
        };
        Arc::new(PromptContext {
            mcp_servers: vec![],
            initial_message: None,
            idle_timeout: Duration::from_secs(30),
            max_turn_duration: Duration::from_secs(60),
            turn_liveness_interval: Duration::ZERO,
            dedup_mode: DedupMode::Queue,
            system_prompt: None,
            session_title: None,
            team_instructions: None,
            heartbeat_prompt: None,
            base_prompt: None,
            cwd: ".".to_string(),
            rest_client: rest(),
            channel_info: ChannelInfoResolver::new(HashMap::new(), rest()),
            context_message_limit,
            peer_agents,
            max_turns_per_session: 0,
            permission_mode: PermissionMode::Default,
            agent_keys: agent_keys.clone(),
            agent_owner_pubkey: None,
            memory_enabled: false,
            harness_name: "goose".to_string(),
            relay_url: "ws://127.0.0.1:1".to_string(),
        })
    }

    /// An issue root that names the agent, with the agent's `p` behind the
    /// name. The `p` on its own is structural and addresses nobody — see
    /// [`desktop_root_on_an_owned_repo`] for that shape.
    fn issue_root(owner: &Keys, agent: &Keys, repo_id: &str) -> nostr::Event {
        use nostr::ToBech32;
        let coord = format!("30617:{}:{repo_id}", owner.public_key().to_hex());
        EventBuilder::new(
            nostr::Kind::Custom(buzz_core::kind::KIND_GIT_ISSUE as u16),
            format!(
                "nostr:{} the test suite is flaky on the second fixture",
                agent.public_key().to_bech32().expect("npub"),
            ),
        )
        .tags([
            nostr::Tag::parse(["a", &coord]).unwrap(),
            nostr::Tag::parse(["p", &agent.public_key().to_hex()]).unwrap(),
        ])
        .sign_with_keys(owner)
        .expect("sign")
    }

    async fn routed(
        event: nostr::Event,
        source: project::ProjectSubscription,
    ) -> project::ProjectEvent {
        routed_mode(event, source, project::ProcessingMode::Live).await
    }

    async fn routed_mode(
        event: nostr::Event,
        source: project::ProjectSubscription,
        mode: project::ProcessingMode,
    ) -> project::ProjectEvent {
        let verified = project::VerifiedProjectEvent::verify(event)
            .await
            .expect("valid");
        let route = project::ProjectRoute::derive(&verified).expect("routes");
        project::ProjectEvent::Routed {
            source,
            route,
            event: verified,
            mode,
        }
    }

    fn unaddressed_root(owner: &Keys, repo_id: &str, kind: u32) -> nostr::Event {
        let coord = format!("30617:{}:{repo_id}", owner.public_key().to_hex());
        EventBuilder::new(nostr::Kind::Custom(kind as u16), "binding root")
            .tags([nostr::Tag::parse(["a", &coord]).unwrap()])
            .sign_with_keys(owner)
            .expect("sign")
    }

    /// A comment on `root` that names the agent in key syntax.
    ///
    /// Key syntax rather than a display name because the runtimes that use this
    /// are deliberately nameless — what they are about is the comment-first
    /// *binding*, and an agent with no configured name can still be addressed
    /// by `@hex`. It used to name `@desktop-agent`, which addressed nobody and
    /// was carried by the comment-first promotion; that promotion is gone, so
    /// the fixture now says what it always meant.
    fn addressed_comment(
        owner: &Keys,
        agent: &Keys,
        repo_id: &str,
        root: &nostr::Event,
    ) -> nostr::Event {
        let coord = format!("30617:{}:{repo_id}", owner.public_key().to_hex());
        EventBuilder::new(
            nostr::Kind::TextNote,
            format!("@{} please take this", agent.public_key().to_hex()),
        )
        .tags([
            nostr::Tag::parse(["a", &coord]).unwrap(),
            nostr::Tag::parse(["e", &root.id.to_hex(), "", "root"]).unwrap(),
            nostr::Tag::parse(["p", &agent.public_key().to_hex()]).unwrap(),
        ])
        .sign_with_keys(owner)
        .expect("sign")
    }

    /// The whole runtime for one scenario, with **no channels at all**.
    struct Runtime {
        owner: Keys,
        agent: Keys,
        identity: project::AgentIdentity,
        humans: BTreeSet<String>,
        externals: BTreeSet<String>,
        discovered: project::DiscoveredRepositories,
        enrolments: project::ProjectEnrolments,
        ledger: peer_call::CallLedger,
        seen: ProjectSeenIds,
        queue: EventQueue,
        pool: AgentPool,
        ctx: Arc<PromptContext>,
        typing: HashMap<uuid::Uuid, crate::queue::ThreadTags>,
        subscriber: NoopSubscriber,
        /// The observer bus, when a scenario wants to read what the arm said.
        ///
        /// `None` by default so every pre-existing case keeps driving the arm
        /// with the bus absent — the configuration where neither project
        /// routing nor telemetry is on — and nothing here starts depending on a
        /// bus it never asked for.
        observer: Option<observer::ObserverHandle>,
        /// Whether this runtime is still admitting work.
        ///
        /// `Open` by default, so every pre-existing scenario drives the arm
        /// exactly as it did before drain existed.
        drain: drain::DrainState,
    }

    impl Runtime {
        async fn new(recorder: &PromptRecorder) -> Self {
            Self::known_as(recorder, "").await
        }

        /// A runtime whose agent knows the name people call it by — what
        /// `BUZZ_ACP_DISPLAY_NAME` configures, and the only thing that makes
        /// Desktop's `@Name` mention syntax rather than prose.
        ///
        /// Separate from [`Runtime::new`] so the scenarios that are about the
        /// comment-first *binding* keep asserting against a nameless agent:
        /// they address it in key syntax, which needs no configured name, and
        /// keeping them nameless is what stops a display-name reading standing
        /// in for the binding they were written to prove.
        async fn known_as(recorder: &PromptRecorder, display_name: &str) -> Self {
            let owner = Keys::generate();
            let agent = Keys::generate();
            let identity = project::AgentIdentity::new(&agent.public_key())
                .unwrap()
                .with_display_name(display_name);
            let mut humans = BTreeSet::new();
            humans.insert(owner.public_key().to_hex());
            let ctx = test_ctx(&agent, empty_relay().await);
            Self {
                owner,
                agent,
                identity,
                humans,
                externals: BTreeSet::new(),
                discovered: project::DiscoveredRepositories::new(),
                enrolments: project::ProjectEnrolments::new(),
                ledger: peer_call::CallLedger::new(),
                seen: ProjectSeenIds::new(),
                queue: EventQueue::new(DedupMode::Queue),
                // One available child, and zero channel subscriptions: this is
                // the project-only shape the defect hid in.
                pool: AgentPool::from_slots(vec![Some(recorder.agent(0).await)]),
                ctx,
                typing: HashMap::new(),
                subscriber: NoopSubscriber,
                observer: None,
                drain: drain::DrainState::open(),
            }
        }

        async fn drive_with_candidate(
            &mut self,
            event: &project::ProjectEvent,
            candidate: Option<project::EnrolmentCandidate>,
        ) -> ProjectDispatched {
            let owner_hex = self.owner.public_key().to_hex();
            let agent_hex = self.agent.public_key().to_hex();
            // These scenarios assert on what was dispatched, not on the
            // inactivity clock, so the harness owns a local rather than
            // pretending the test runtime tracks one.
            let mut last_activity = tokio::time::Instant::now();
            dispatch_and_flush_project_event(
                &mut ProjectArm {
                    identity: &self.identity,
                    owner: Some(&owner_hex),
                    approved_humans: &self.humans,
                    approved_external_agents: &self.externals,
                    discovered: &mut self.discovered,
                    enrolments: &mut self.enrolments,
                    ledger: &mut self.ledger,
                    seen: &mut self.seen,
                    agent_pubkey_hex: &agent_hex,
                    startup_watermark: 0,
                    observer: self.observer.as_ref(),
                    drain: &self.drain,
                },
                None,
                candidate,
                &self.subscriber,
                event,
                &mut self.pool,
                &mut self.queue,
                &self.ctx,
                &mut last_activity,
                &mut self.typing,
                true,
            )
            .await
        }

        async fn drive(&mut self, event: &project::ProjectEvent) -> ProjectDispatched {
            self.drive_with_candidate(event, None).await
        }

        /// Honour a drain, as the control handler does.
        ///
        /// Goes through [`handle_drain_control`] rather than mutating the state
        /// directly, so these scenarios inherit the idempotence and the
        /// acknowledgement rather than testing a shortcut the runtime does not
        /// take. The frame's own verification is proved in
        /// [`drain_control_tests`]; what is only visible here is what a drained
        /// runtime then *does*.
        fn begin_drain(&mut self, bound: Duration) -> drain::DrainOnset {
            handle_drain_control(
                &serde_json::json!({"type": "drain"}),
                self.observer.as_ref(),
                &mut self.drain,
                bound,
                tokio::time::Instant::now(),
            )
        }

        /// Whether the run loop would now leave, and why.
        ///
        /// The exact expression the run loop evaluates at the top of every
        /// iteration, minus the heartbeat flag these project-only scenarios
        /// never set. Reproduced rather than reached because the loop it lives
        /// in parses CLI arguments and connects a relay before it is entered;
        /// this is as directly as the branch can be driven, and the pieces it
        /// is made of — `has_undrained_work` and `should_exit` — are each
        /// proved on their own.
        fn drain_exit(&self) -> Option<drain::DrainExit> {
            self.drain
                .should_exit(self.queue.has_undrained_work(), tokio::time::Instant::now())
        }

        /// Reap one finished turn, the way the run loop's result arm does.
        ///
        /// Exactly the two effects that arm has which this scenario depends on:
        /// the in-flight hold is released and the child goes back in its slot.
        /// Standing up the real `handle_prompt_result` would need a `Config`, a
        /// crash-history vector and a respawn channel, none of which say
        /// anything about drain — and the claim under test is about the queue's
        /// answer, not about result handling.
        async fn reap_one_turn(&mut self) {
            let result = {
                let (result_rx, _join_set) = self.pool.rx_and_join_set();
                tokio::time::timeout(Duration::from_secs(20), result_rx.recv())
                    .await
                    .expect("a turn must return within 20s")
                    .expect("pool result channel closed")
            };
            if let PromptSource::Channel(key) = result.source {
                self.queue.mark_complete(key);
            }
            self.pool.return_agent(result.agent);
        }

        async fn discover(&mut self, repo_id: &str) {
            let announcer = self.owner.clone();
            self.discover_announced_by(&announcer, repo_id).await;
        }

        /// Discover a repository somebody other than the human owner announced
        /// — including this agent, which is the live shape the root-addressing
        /// failure needed: the agent owns the coordinate, so Desktop's
        /// repository-owner `p` on every root is *this agent's* key.
        async fn discover_announced_by(&mut self, announcer: &Keys, repo_id: &str) {
            let announcement = proven_announcement_for(announcer, repo_id).await;
            self.drive(&project::ProjectEvent::Discovery { announcement })
                .await;
        }
    }

    async fn proven_announcement_for(
        keys: &Keys,
        identifier: &str,
    ) -> project::VerifiedAnnouncement {
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

    /// A project-only runtime runs the turn it queues.
    ///
    /// The reported failure: three live runtimes authenticated, discovered the
    /// repository, enrolled the root, logged `queued=true` — and every child
    /// received only `initialize`. Nothing else ever arrives for a runtime with
    /// no channels, so the queue was never flushed by anyone.
    #[tokio::test]
    async fn an_addressed_root_reaches_the_child_with_no_channels_configured() {
        let recorder = PromptRecorder::new();
        let mut rt = Runtime::new(&recorder).await;
        rt.discover("demo").await;

        let root = issue_root(&rt.owner, &rt.agent, "demo");
        let dispatched = rt
            .drive(&routed(root, project::ProjectSubscription::Enrolment).await)
            .await;

        assert!(
            matches!(dispatched, ProjectDispatched::Queued { queued: true, .. }),
            "precondition: the root must be admitted — got {dispatched:?}"
        );
        let prompts = recorder.prompts(1).await;
        assert_eq!(
            prompts.len(),
            1,
            "the child received no session/prompt: queued work was never dispatched"
        );
        assert!(
            prompts[0].contains("flaky on the second fixture"),
            "the prompt did not carry the issue: {}",
            prompts[0]
        );
        assert!(
            rt.typing.is_empty(),
            "a project route key names no channel — a typing frame would be h-tagged to nothing"
        );
    }

    /// The issue hears about the comment before the agent does.
    ///
    /// End-to-end through every joint of the new path: the dispatch gate queues
    /// the event and says so on the observer bus, the activity publisher folds
    /// that into NIP-PA, and a signed `20003` carrying `state=queued` reaches
    /// the relay — all before any turn has started. Each part is proved on its
    /// own elsewhere; what is only visible here is that they are *connected*,
    /// which is exactly the class of defect this module exists for.
    ///
    /// The child is deliberately observer-less, so nothing but the gate can put
    /// a frame on this bus: a `queued` that only appeared because a turn had
    /// already begun would be no signal at all.
    #[tokio::test]
    async fn a_queued_project_event_is_announced_on_its_root_before_any_turn() {
        let recorder = PromptRecorder::new();
        let mut rt = Runtime::new(&recorder).await;
        rt.discover("demo").await;

        let bus = observer::ObserverHandle::in_process();
        let rx = bus.subscribe();
        rt.observer = Some(bus);
        let agent_hex = rt.agent.public_key().to_hex().to_ascii_lowercase();
        let (publisher, mut published) = relay::RelayEventPublisher::test_pair();
        let task = tokio::spawn(run_project_activity_publisher(
            rx,
            publisher,
            rt.agent.clone(),
            agent_hex.clone(),
        ));

        let root = issue_root(&rt.owner, &rt.agent, "demo");
        let root_id = root.id.to_hex();
        let dispatched = rt
            .drive(&routed(root, project::ProjectSubscription::Enrolment).await)
            .await;
        assert!(
            matches!(dispatched, ProjectDispatched::Queued { queued: true, .. }),
            "precondition: the event must be admitted — got {dispatched:?}"
        );

        let event = tokio::time::timeout(Duration::from_secs(5), published.recv())
            .await
            .expect("nothing reached the relay within 5s: the gap is still silent")
            .expect("publisher channel closed");
        task.abort();

        let tag = |key: &str| {
            event.tags.iter().find_map(|t| {
                let s = t.as_slice();
                (s.first().map(String::as_str) == Some(key))
                    .then(|| s.get(1).cloned())
                    .flatten()
            })
        };
        assert_eq!(
            event.kind.as_u16(),
            buzz_core::kind::KIND_PROJECT_ACTIVITY as u16
        );
        assert_eq!(tag("state").as_deref(), Some("queued"));
        assert_eq!(
            tag("e").as_deref(),
            Some(root_id.as_str()),
            "the announcement must land on the root the comment is on"
        );
        assert_eq!(
            tag("a").as_deref(),
            Some(format!("30617:{}:demo", rt.owner.public_key().to_hex()).as_str()),
            "the repository coordinate must travel with it"
        );
        assert_eq!(
            tag("turn").as_deref(),
            Some(queued_turn_id(&root_id).as_str()),
            "no turn exists yet, so the frame is named after the event that queued"
        );
        assert_eq!(tag("agent").as_deref(), Some(agent_hex.as_str()));
        assert_eq!(tag("h"), None, "a root is not a channel");
    }

    /// One root delivered on two subscriptions is one turn.
    ///
    /// The live run logged the identical root twice, during the
    /// enrolment-to-watched replacement window. The relay task's own dedup does
    /// not cover it: its live domain deliberately excludes catch-up rows, so the
    /// same event can legitimately arrive by two routes that each believe they
    /// are first.
    #[tokio::test]
    async fn the_same_root_on_two_subscriptions_runs_one_turn() {
        let recorder = PromptRecorder::new();
        let mut rt = Runtime::new(&recorder).await;
        rt.discover("demo").await;

        let root = issue_root(&rt.owner, &rt.agent, "demo");
        let first = rt
            .drive(&routed(root.clone(), project::ProjectSubscription::Enrolment).await)
            .await;
        assert!(
            matches!(first, ProjectDispatched::Queued { queued: true, .. }),
            "the first delivery must be admitted — got {first:?}"
        );

        // The same signed event, arriving on the successor subscription.
        let second = rt
            .drive(
                &routed(
                    root.clone(),
                    project::ProjectSubscription::Watched { generation: 1 },
                )
                .await,
            )
            .await;
        assert!(
            matches!(second, ProjectDispatched::Ignored),
            "the second delivery of one event must not be admitted — got {second:?}"
        );

        let prompts = recorder.prompts(1).await;
        assert_eq!(
            prompts.len(),
            1,
            "one root, two subscriptions, {} prompts — the issue would get two replies",
            prompts.len()
        );
    }

    /// Dedup is by event id, not by root: a second, distinct comment on the same
    /// root in the same second is different work and is still admitted.
    ///
    /// Without this the "fix" for a double delivery is indistinguishable from
    /// dropping the conversation after its first message — and a same-second
    /// event is exactly what a timestamp-based dedup would lose.
    ///
    /// Admission is the assertion, not a second prompt: the root's turn is
    /// still in flight under this route key, so the comment waits for it. That
    /// queue-behind is ordinary per-channel serialisation and is proved in the
    /// queue's own tests; what is in question here is only whether the event
    /// survives the dedup.
    #[tokio::test]
    async fn a_distinct_same_second_event_on_one_root_is_not_deduped_away() {
        let recorder = PromptRecorder::new();
        let mut rt = Runtime::new(&recorder).await;
        rt.discover("demo").await;

        let root = issue_root(&rt.owner, &rt.agent, "demo");
        let root_id = root.id.to_hex();
        let created = root.created_at;
        let route_key = {
            let verified = project::VerifiedProjectEvent::verify(root.clone())
                .await
                .expect("valid");
            project::ProjectRoute::derive(&verified)
                .expect("routes")
                .key()
        };
        rt.drive(&routed(root.clone(), project::ProjectSubscription::Enrolment).await)
            .await;
        rt.drive(
            &routed(
                root,
                project::ProjectSubscription::Watched { generation: 1 },
            )
            .await,
        )
        .await;
        // The root's own turn is running; the queue is empty behind it.
        recorder.prompts(1).await;
        assert_eq!(rt.queue.queued_event_count(&route_key), 0);

        // Same root, same timestamp, different event. Addressed to the agent
        // because this test is about *dedup*, not addressing: an unaddressed
        // comment is now correctly ignored, which would hide what it asserts.
        let comment = EventBuilder::new(
            nostr::Kind::TextNote,
            format!("@{} and it fails on CI too", rt.agent.public_key().to_hex()),
        )
        .tags([
            nostr::Tag::parse([
                "a",
                &format!("30617:{}:demo", rt.owner.public_key().to_hex()),
            ])
            .unwrap(),
            nostr::Tag::parse(["e", &root_id, "", "root"]).unwrap(),
            nostr::Tag::parse(["p", &rt.agent.public_key().to_hex()]).unwrap(),
        ])
        .custom_created_at(created)
        .sign_with_keys(&rt.owner)
        .expect("sign");
        let dispatched = rt
            .drive(
                &routed(
                    comment,
                    project::ProjectSubscription::Watched { generation: 1 },
                )
                .await,
            )
            .await;

        assert!(
            matches!(dispatched, ProjectDispatched::Queued { queued: true, .. }),
            "a distinct event on a watched root is work — got {dispatched:?}"
        );
        assert_eq!(
            rt.queue.queued_event_count(&route_key),
            1,
            "deduping by root or by timestamp rather than by event id loses the conversation"
        );
        assert_eq!(
            recorder.read().len(),
            1,
            "it waits for the turn in flight rather than starting a second one"
        );
    }

    #[tokio::test]
    async fn approved_human_comment_first_uses_supplied_root_binding_and_queues_comment() {
        for kind in [
            buzz_core::kind::KIND_GIT_ISSUE,
            buzz_core::kind::KIND_GIT_PULL_REQUEST,
        ] {
            let recorder = PromptRecorder::new();
            let mut rt = Runtime::new(&recorder).await;
            rt.discover("comment-first").await;
            let root = unaddressed_root(&rt.owner, "comment-first", kind);
            let verified_root = project::VerifiedProjectEvent::verify(root.clone())
                .await
                .expect("root verifies");
            let candidate = project::validate_enrolment_candidate(&verified_root, &rt.discovered)
                .expect("known root is a candidate");
            let comment = addressed_comment(&rt.owner, &rt.agent, "comment-first", &root);

            let dispatched = rt
                .drive_with_candidate(
                    &routed(comment, project::ProjectSubscription::Enrolment).await,
                    Some(candidate),
                )
                .await;

            assert!(
                matches!(dispatched, ProjectDispatched::Queued { queued: true, .. }),
                "comment-first dispatch was {dispatched:?}"
            );
            assert!(rt.enrolments.get(&root.id.to_hex()).is_some());
            let prompts = recorder.prompts(1).await;
            assert!(prompts[0].contains("please take this"));
            assert!(!prompts[0].contains("binding root\n\nbinding root"));
        }
    }

    #[tokio::test]
    async fn replayed_comment_first_enrols_without_queueing_historical_work() {
        let recorder = PromptRecorder::new();
        let mut rt = Runtime::new(&recorder).await;
        rt.discover("replay-comment-first").await;
        let root = unaddressed_root(
            &rt.owner,
            "replay-comment-first",
            buzz_core::kind::KIND_GIT_ISSUE,
        );
        let verified_root = project::VerifiedProjectEvent::verify(root.clone())
            .await
            .expect("root verifies");
        let candidate = project::validate_enrolment_candidate(&verified_root, &rt.discovered)
            .expect("known root is a candidate");
        let comment = addressed_comment(&rt.owner, &rt.agent, "replay-comment-first", &root);

        let dispatched = rt
            .drive_with_candidate(
                &routed_mode(
                    comment,
                    project::ProjectSubscription::Enrolment,
                    project::ProcessingMode::Replay,
                )
                .await,
                Some(candidate),
            )
            .await;

        assert_eq!(dispatched, ProjectDispatched::Enrolled);
        assert!(rt.enrolments.get(&root.id.to_hex()).is_some());
        assert!(recorder.read().is_empty());
    }

    #[tokio::test]
    async fn mismatched_comment_first_candidate_is_ignored() {
        let recorder = PromptRecorder::new();
        let mut rt = Runtime::new(&recorder).await;
        rt.discover("expected").await;
        rt.discover("other").await;
        let referenced = unaddressed_root(&rt.owner, "expected", buzz_core::kind::KIND_GIT_ISSUE);
        let other = unaddressed_root(&rt.owner, "other", buzz_core::kind::KIND_GIT_ISSUE);
        let verified_other = project::VerifiedProjectEvent::verify(other)
            .await
            .expect("root verifies");
        let wrong_candidate =
            project::validate_enrolment_candidate(&verified_other, &rt.discovered)
                .expect("other known root is a candidate");
        let comment = addressed_comment(&rt.owner, &rt.agent, "expected", &referenced);

        let dispatched = rt
            .drive_with_candidate(
                &routed(comment, project::ProjectSubscription::Enrolment).await,
                Some(wrong_candidate),
            )
            .await;

        assert_eq!(dispatched, ProjectDispatched::Ignored);
        assert!(rt.enrolments.get(&referenced.id.to_hex()).is_none());
        assert!(recorder.read().is_empty());
    }

    /// The display name this agent is configured with, as a live one is.
    const DISPLAY_NAME: &str = "Claude";

    /// The event Buzz Desktop publishes when a person opens an issue on a
    /// repository **this agent owns**.
    ///
    /// One `p` tag, and it is the repository owner's — so it is the agent's own
    /// key, on every root, whoever opened it and whatever it says. That is the
    /// whole of the reported failure: nothing here is an address, and the body
    /// is the only place an address could have been written.
    fn desktop_root_on_an_owned_repo(
        author: &Keys,
        agent: &Keys,
        repo_id: &str,
        body: &str,
    ) -> nostr::Event {
        let agent_hex = agent.public_key().to_hex();
        EventBuilder::new(
            nostr::Kind::Custom(buzz_core::kind::KIND_GIT_ISSUE as u16),
            body,
        )
        .tags([
            nostr::Tag::parse(["a", &format!("30617:{agent_hex}:{repo_id}")]).unwrap(),
            nostr::Tag::parse(["p", &agent_hex]).unwrap(),
        ])
        .sign_with_keys(author)
        .expect("sign")
    }

    /// A comment on `root`, carrying the same structural `p` set Desktop copies
    /// forward, plus a body that names somebody.
    fn desktop_comment(
        author: &Keys,
        agent: &Keys,
        repo_id: &str,
        root: &nostr::Event,
        body: &str,
    ) -> nostr::Event {
        let agent_hex = agent.public_key().to_hex();
        EventBuilder::new(nostr::Kind::TextNote, body)
            .tags([
                nostr::Tag::parse(["a", &format!("30617:{agent_hex}:{repo_id}")]).unwrap(),
                nostr::Tag::parse(["e", &root.id.to_hex(), "", "root"]).unwrap(),
                nostr::Tag::parse(["p", &agent_hex]).unwrap(),
            ])
            .sign_with_keys(author)
            .expect("sign")
    }

    /// **The reported failure, as the sequence that produced it.**
    ///
    /// On `30617:…:comment-e2e`, roots `b1261034…` (queued 10:07:13, answered
    /// 10:07:27) and `eb1803a2…` (queued 10:09:56, answered 10:10:10) each had
    /// content `test`, named nobody, and carried the automatic repository-owner
    /// `p` — this agent's, because this agent owns the coordinate. Each woke a
    /// turn and produced a canned reply, seventeen and fifteen seconds *before*
    /// the comment that was actually addressed to somebody arrived. The
    /// comments were never what woke the agent. The roots were.
    ///
    /// Both halves are here because either alone is passable by a wrong build:
    /// an agent that ignored everything would satisfy the first, and the old
    /// build satisfied the second. The prompt count is what joins them — under
    /// the old behaviour the root's turn is in flight and the comment queues
    /// behind it, so the one prompt the child ever sees carries `test` rather
    /// than the request.
    #[tokio::test]
    async fn a_structurally_tagged_root_wakes_nobody_and_a_later_named_comment_still_enrols() {
        let recorder = PromptRecorder::new();
        let mut rt = Runtime::known_as(&recorder, DISPLAY_NAME).await;
        let agent = rt.agent.clone();
        rt.discover_announced_by(&agent, "comment-e2e").await;

        let root = desktop_root_on_an_owned_repo(&rt.owner, &agent, "comment-e2e", "test");
        let dispatched = rt
            .drive(&routed(root.clone(), project::ProjectSubscription::Enrolment).await)
            .await;

        assert_eq!(
            dispatched,
            ProjectDispatched::Ignored,
            "an issue saying `test` and naming nobody is not this agent's turn"
        );
        assert!(
            rt.enrolments.get(&root.id.to_hex()).is_none(),
            "and it must not have quietly enrolled the root either"
        );

        // The root is separately fetched and verified, which is what
        // `resolve_comment_first_candidate` does in the run loop for a comment
        // on a root this process is not watching.
        let verified_root = project::VerifiedProjectEvent::verify(root.clone())
            .await
            .expect("root verifies");
        let candidate = project::validate_enrolment_candidate(&verified_root, &rt.discovered)
            .expect("a root on a discovered coordinate is a candidate");
        let comment = desktop_comment(
            &rt.owner,
            &agent,
            "comment-e2e",
            &root,
            &format!("@{DISPLAY_NAME} could you pick this one up"),
        );
        let dispatched = rt
            .drive_with_candidate(
                &routed(comment, project::ProjectSubscription::Enrolment).await,
                Some(candidate),
            )
            .await;

        assert!(
            matches!(dispatched, ProjectDispatched::Queued { queued: true, .. }),
            "ignoring the root must not cost the agent the comment that named \
             it — got {dispatched:?}"
        );
        assert!(
            rt.enrolments.get(&root.id.to_hex()).is_some(),
            "comment-first enrolment binds the root it was fetched for"
        );

        let prompts = recorder.prompts(1).await;
        assert_eq!(
            prompts.len(),
            1,
            "the root ran a turn of its own: {prompts:?}"
        );
        assert!(
            prompts[0].contains("could you pick this one up"),
            "the one turn must be the comment's, not the root's: {}",
            prompts[0]
        );
    }

    /// …and the root the same person meant to send.
    ///
    /// Same repository, same structural `p`, one difference: the body says who
    /// it is for. `@Claude` is mention syntax only for an agent that knows its
    /// own name, which is why this runtime is configured with one.
    #[tokio::test]
    async fn a_root_that_names_this_agent_wakes_it_exactly_once() {
        let recorder = PromptRecorder::new();
        let mut rt = Runtime::known_as(&recorder, DISPLAY_NAME).await;
        let agent = rt.agent.clone();
        rt.discover_announced_by(&agent, "comment-e2e").await;

        let root = desktop_root_on_an_owned_repo(
            &rt.owner,
            &agent,
            "comment-e2e",
            &format!("@{DISPLAY_NAME} the pipeline drops frames after reconnect"),
        );
        let dispatched = rt
            .drive(&routed(root.clone(), project::ProjectSubscription::Enrolment).await)
            .await;

        assert!(
            matches!(dispatched, ProjectDispatched::Queued { queued: true, .. }),
            "a named root is exactly what enrols and wakes — got {dispatched:?}"
        );
        assert!(rt.enrolments.get(&root.id.to_hex()).is_some());

        let prompts = recorder.prompts(1).await;
        assert_eq!(prompts.len(), 1, "exactly one turn: {prompts:?}");
        assert!(
            prompts[0].contains("the pipeline drops frames after reconnect"),
            "the turn must carry the issue: {}",
            prompts[0]
        );
    }

    /// A root carrying the `subject` tag every Buzz writer puts a title in.
    fn titled_root(
        author: &Keys,
        agent: &Keys,
        repo_id: &str,
        subject: &str,
        body: &str,
    ) -> nostr::Event {
        let agent_hex = agent.public_key().to_hex();
        EventBuilder::new(
            nostr::Kind::Custom(buzz_core::kind::KIND_GIT_ISSUE as u16),
            body,
        )
        .tags([
            nostr::Tag::parse(["a", &format!("30617:{agent_hex}:{repo_id}")]).unwrap(),
            nostr::Tag::parse(["p", &agent_hex]).unwrap(),
            nostr::Tag::parse(["subject", subject]).unwrap(),
        ])
        .sign_with_keys(author)
        .expect("sign")
    }

    /// A relay that answers the project-situation read with `rows`.
    ///
    /// Profile lookups are answered empty and separately: they go to the same
    /// endpoint, and handing a kind:0 query a pile of `1621`s would let a
    /// profile parser's tolerance stand in for a situation fetch that never
    /// happened.
    async fn situation_relay(rows: Vec<serde_json::Value>) -> String {
        use axum::{routing::post, Json, Router};

        let app = Router::new()
            .route(
                "/query",
                post(move |body: String| {
                    let rows = rows.clone();
                    async move {
                        let wants_profiles = serde_json::from_str::<serde_json::Value>(&body)
                            .ok()
                            .and_then(|filters| {
                                let filters = filters.as_array()?.clone();
                                Some(filters.iter().any(|f| {
                                    f["kinds"].as_array().is_some_and(|kinds| {
                                        kinds.iter().any(|k| k.as_u64() == Some(0))
                                    })
                                }))
                            })
                            .unwrap_or(false);
                        Json(if wants_profiles {
                            serde_json::json!([])
                        } else {
                            serde_json::Value::Array(rows)
                        })
                    }
                }),
            )
            .fallback(|| async { Json(serde_json::json!([])) });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });
        format!("http://{addr}")
    }

    /// **What the child is actually told about the issue it was named on.**
    ///
    /// The fetch, the parse and the render each have their own tests, and each
    /// of them would stay green against a `run_prompt_task` that built the
    /// situation and dropped it — leaving a `[Project]` section correct about
    /// the coordinate and silent about everything the turn is for. This is the
    /// one place all three run together, driven by the real dispatch path and
    /// read back off the child's own stdin.
    #[tokio::test]
    async fn a_project_turn_carries_the_root_its_conversation_and_the_roster() {
        let recorder = PromptRecorder::new();
        let mut rt = Runtime::known_as(&recorder, DISPLAY_NAME).await;
        let agent = rt.agent.clone();
        rt.discover_announced_by(&agent, "situation").await;

        let root = titled_root(
            &rt.owner,
            &agent,
            "situation",
            "Reconnect drops the project subscription",
            &format!(
                "@{DISPLAY_NAME} the agent stops receiving comments after the second reconnect."
            ),
        );
        let earlier = desktop_comment(
            &rt.owner,
            &agent,
            "situation",
            &root,
            "I can reproduce it on main.",
        );
        let peer = Keys::generate();
        let peer_hex = peer.public_key().to_hex();

        // Everything the turn is about to read, and the two knobs that decide
        // how much of it is rendered.
        rt.ctx = test_ctx_with(
            &agent,
            situation_relay(vec![
                serde_json::to_value(&root).expect("serialise the root"),
                serde_json::to_value(&earlier).expect("serialise the comment"),
            ])
            .await,
            8,
            vec![peer_hex.clone()],
        );

        let dispatched = rt
            .drive(&routed(root.clone(), project::ProjectSubscription::Enrolment).await)
            .await;
        assert!(
            matches!(dispatched, ProjectDispatched::Queued { queued: true, .. }),
            "precondition: a root naming this agent must wake it — got {dispatched:?}"
        );

        let prompts = recorder.prompts(1).await;
        assert_eq!(prompts.len(), 1, "exactly one turn: {prompts:?}");
        let prompt = &prompts[0];

        assert!(
            prompt.contains("Title: Reconnect drops the project subscription"),
            "the turn does not know what the issue is called: {prompt}"
        );
        assert!(
            prompt.contains("I can reproduce it on main."),
            "the turn does not carry the conversation on the root: {prompt}"
        );
        assert!(
            !prompt.contains("history unavailable"),
            "the situation fetch did not land: {prompt}"
        );
        assert!(
            prompt.contains(&format!("--to {peer_hex}")),
            "the configured peer is not on the roster: {prompt}"
        );
        assert!(
            prompt.contains("This conversation is the issue itself"),
            "the turn was not told this conversation is durable: {prompt}"
        );
    }

    /// The handle of the agent the work was actually handed to.
    ///
    /// Hyphenated on purpose: this is the shape a Desktop display handle takes,
    /// and the shape the live comment carried. It is a fixture value and
    /// nothing in the crate knows it — the grammar is about the hyphen, not
    /// about this gateway.
    const OTHER_HANDLE: &str = "hermes-gateway";

    /// The live comment shape: Desktop's copied-forward `p` set with **both**
    /// parties in it, and a body that hands the work to one of them.
    ///
    /// Both tags are the point. The self `p` is what the enrolment
    /// subscription matched on and what every earlier build read as an address;
    /// the other party's `p` is what makes the visible handle an address rather
    /// than prose.
    fn desktop_comment_to_both(
        author: &Keys,
        agent: &Keys,
        other: &Keys,
        repo_id: &str,
        root: &nostr::Event,
        body: &str,
    ) -> nostr::Event {
        let agent_hex = agent.public_key().to_hex();
        EventBuilder::new(nostr::Kind::TextNote, body)
            .tags([
                nostr::Tag::parse(["a", &format!("30617:{agent_hex}:{repo_id}")]).unwrap(),
                nostr::Tag::parse(["e", &root.id.to_hex(), "", "root"]).unwrap(),
                nostr::Tag::parse(["p", &agent_hex]).unwrap(),
                nostr::Tag::parse(["p", &other.public_key().to_hex()]).unwrap(),
            ])
            .sign_with_keys(author)
            .expect("sign")
    }

    /// The root, separately fetched and verified — what
    /// `resolve_comment_first_candidate` does in the run loop for a comment on
    /// a root this process is not watching.
    async fn fetched_root_candidate(
        rt: &Runtime,
        root: &nostr::Event,
    ) -> project::EnrolmentCandidate {
        let verified = project::VerifiedProjectEvent::verify(root.clone())
            .await
            .expect("root verifies");
        project::validate_enrolment_candidate(&verified, &rt.discovered)
            .expect("a root on a discovered coordinate is a candidate")
    }

    /// The queue key a root's work would be parked under, derived the way
    /// dispatch derives it.
    fn route_key_of(root: &nostr::Event) -> uuid::Uuid {
        project::project_route_key(&root.id.to_hex()).expect("a root id is a route key")
    }

    /// A well-formed NIP-PC invocation from `caller` to `agent` on `root`,
    /// exactly the envelope the CLI publishes: derived call id, hop from path.
    fn project_call_from(
        caller: &Keys,
        agent: &Keys,
        repo_id: &str,
        root: &nostr::Event,
        task: &str,
    ) -> nostr::Event {
        let caller_hex = caller.public_key().to_hex();
        buzz_sdk::builders::build_peer_call(
            &caller_hex,
            task,
            &buzz_sdk::builders::PeerCallMeta {
                callee: agent.public_key().to_hex(),
                route: buzz_core::peer_call::PeerCallRoute::Project {
                    coordinate: format!("30617:{}:{repo_id}", agent.public_key().to_hex()),
                    root: root.id.to_hex(),
                },
                nonce: "00112233445566778899aabbccddeeff".into(),
                hop: 1,
                visited: vec![caller_hex.clone()],
            },
        )
        .expect("well-formed call")
        .sign_with_keys(caller)
        .expect("sign")
    }

    /// **The live #0a81a1ca failure, through the production dispatch path.**
    ///
    /// hermes-gateway's first call on a fresh issue was cryptographically
    /// exact — recomputed call id, route, hop, visited — and this agent
    /// refused it with a debug line. The resolver only supplied enrolment
    /// candidates for kind-1 comments with a visible mention, so the decision
    /// matrix's `TrustedAgent + Invocation => EnrolAndWake` arm was
    /// unreachable on any root nobody had commented `@agent` on first: the
    /// matrix permitted what the resolver never fed it. A trusted peer's
    /// first call must enrol the root and run the task.
    #[tokio::test]
    async fn a_trusted_agents_first_call_enrols_an_unknown_root() {
        let recorder = PromptRecorder::new();
        let mut rt = Runtime::new(&recorder).await;
        let agent = rt.agent.clone();
        let caller = Keys::generate();
        rt.externals.insert(caller.public_key().to_hex());
        rt.discover_announced_by(&agent, "call-first").await;

        let root =
            desktop_root_on_an_owned_repo(&rt.owner, &agent, "call-first", "no address here");
        let candidate = fetched_root_candidate(&rt, &root).await;
        let call = project_call_from(&caller, &agent, "call-first", &root, "first-contact task");

        // The resolver change under test: a peer-call envelope naming this
        // agent is a first-contact shape, so the run loop would have resolved
        // exactly this candidate for exactly this event.
        let verified = project::VerifiedProjectEvent::verify(call.clone())
            .await
            .expect("call verifies");
        assert!(
            first_contact_shape(&verified, &agent.public_key().to_hex()),
            "an invocation naming this agent is first contact"
        );

        let dispatched = rt
            .drive_with_candidate(
                &routed(call, project::ProjectSubscription::PeerCall).await,
                Some(candidate),
            )
            .await;
        assert!(
            matches!(dispatched, ProjectDispatched::Queued { queued: true, .. }),
            "a trusted peer's first call must queue a turn, got {dispatched:?}"
        );
        assert!(
            rt.enrolments.get(&root.id.to_hex()).is_some(),
            "the call must enrol the root it rode in on"
        );
        let prompts = recorder.prompts(1).await;
        assert!(
            prompts[0].contains("first-contact task"),
            "the turn must carry the call's task:\n{}",
            prompts[0]
        );
    }

    /// The caller not being approved keeps the same envelope out: first
    /// contact is a shape, and TrustedAgent is a grant — the resolver may
    /// fetch the root, but the author gate still refuses a stranger.
    #[tokio::test]
    async fn a_strangers_first_call_still_enrols_nothing() {
        let recorder = PromptRecorder::new();
        let mut rt = Runtime::new(&recorder).await;
        let agent = rt.agent.clone();
        let stranger = Keys::generate();
        // Deliberately NOT inserted into rt.externals.
        rt.discover_announced_by(&agent, "call-first").await;

        let root =
            desktop_root_on_an_owned_repo(&rt.owner, &agent, "call-first", "no address here");
        let candidate = fetched_root_candidate(&rt, &root).await;
        let call = project_call_from(&stranger, &agent, "call-first", &root, "should never run");

        let dispatched = rt
            .drive_with_candidate(
                &routed(call, project::ProjectSubscription::PeerCall).await,
                Some(candidate),
            )
            .await;
        assert!(
            !matches!(dispatched, ProjectDispatched::Queued { queued: true, .. }),
            "a stranger's call must not queue, got {dispatched:?}"
        );
        assert!(
            rt.enrolments.get(&root.id.to_hex()).is_none(),
            "a stranger's call must not enrol"
        );
    }

    /// The predicate's negatives, pinned: a result is not first contact, a
    /// call for somebody else is not first contact, and a comment without a
    /// visible mention still is not — the kind-1 arm is unchanged.
    #[tokio::test]
    async fn first_contact_shape_refuses_what_it_must() {
        let recorder = PromptRecorder::new();
        let rt = Runtime::new(&recorder).await;
        let agent = rt.agent.clone();
        let caller = Keys::generate();
        let other = Keys::generate();
        let root =
            desktop_root_on_an_owned_repo(&rt.owner, &agent, "call-first", "no address here");
        let agent_hex = agent.public_key().to_hex();

        let for_other = project_call_from(&caller, &other, "call-first", &root, "not for us");
        let verified = project::VerifiedProjectEvent::verify(for_other)
            .await
            .expect("verifies");
        assert!(
            !first_contact_shape(&verified, &agent_hex),
            "a call naming somebody else is not our first contact"
        );

        let bare_root = project::VerifiedProjectEvent::verify(root)
            .await
            .expect("verifies");
        assert!(
            !first_contact_shape(&bare_root, &agent_hex),
            "a root event is not a first-contact comment or call"
        );
    }

    /// **The live Phase 3e failure, through the production dispatch path.**
    ///
    /// Root `d2986fa7…` was correctly ignored for twenty-five seconds. Then
    /// comment `74f92354…` arrived, `p`-tagging both this agent and the agent
    /// it was for, and beginning `@hermes-gateway …` — and this agent woke,
    /// enrolled, and answered a comment that had told it, in its first word,
    /// who it was for.
    ///
    /// The root is `Unknown` here, which was the whole hole: the separately
    /// fetched root was being read as *addressing* as well as binding, so an
    /// unknown root's first comment was explicit by construction whoever it
    /// named.
    ///
    /// The addressed follow-up is in the same test rather than beside it. It
    /// bounds the negative — if the hyphenated comment had queued, the first
    /// prompt the child ever saw would carry it — and it holds the fix to
    /// target-only rather than deaf.
    #[tokio::test]
    async fn a_comment_handed_to_another_agent_never_enrols_an_unknown_root() {
        let recorder = PromptRecorder::new();
        let mut rt = Runtime::known_as(&recorder, DISPLAY_NAME).await;
        let agent = rt.agent.clone();
        let other = Keys::generate();
        rt.discover_announced_by(&agent, "comment-e2e").await;

        let root = desktop_root_on_an_owned_repo(&rt.owner, &agent, "comment-e2e", "test");
        assert_eq!(
            rt.drive(&routed(root.clone(), project::ProjectSubscription::Enrolment).await)
                .await,
            ProjectDispatched::Ignored,
            "precondition: the structural root is not an address"
        );

        let handed_off = desktop_comment_to_both(
            &rt.owner,
            &agent,
            &other,
            "comment-e2e",
            &root,
            &format!("@{OTHER_HANDLE} could you take the deploy check on this one"),
        );
        let dispatched = rt
            .drive_with_candidate(
                &routed(handed_off, project::ProjectSubscription::Enrolment).await,
                Some(fetched_root_candidate(&rt, &root).await),
            )
            .await;

        assert_eq!(
            dispatched,
            ProjectDispatched::Ignored,
            "a comment addressed to another agent is not this agent's turn"
        );
        assert!(
            rt.enrolments.get(&root.id.to_hex()).is_none(),
            "and it must not start a watch on the conversation either"
        );
        assert_eq!(
            rt.queue.queued_event_count(&route_key_of(&root)),
            0,
            "nothing may be queued for a turn that is not ours"
        );

        // The same person, on the same root, addressing this agent.
        let for_us = desktop_comment_to_both(
            &rt.owner,
            &agent,
            &other,
            "comment-e2e",
            &root,
            &format!("@{DISPLAY_NAME} and could you review the migration after that"),
        );
        let dispatched = rt
            .drive_with_candidate(
                &routed(for_us, project::ProjectSubscription::Enrolment).await,
                Some(fetched_root_candidate(&rt, &root).await),
            )
            .await;

        assert!(
            matches!(dispatched, ProjectDispatched::Queued { queued: true, .. }),
            "target-only is not deaf — got {dispatched:?}"
        );
        assert!(
            rt.enrolments.get(&root.id.to_hex()).is_some(),
            "the comment that named us enrols the root it was fetched for"
        );

        let prompts = recorder.prompts(1).await;
        assert_eq!(
            prompts.len(),
            1,
            "exactly one turn, and only from the comment that named us: {prompts:?}"
        );
        assert!(
            prompts[0].contains("review the migration"),
            "the turn carried the wrong comment: {}",
            prompts[0]
        );
        assert!(
            !prompts[0].contains("deploy check"),
            "the handed-off comment reached the child as work: {}",
            prompts[0]
        );
    }

    /// The same comment on a root this agent is already watching.
    ///
    /// Once enrolled, this agent is in every later comment's `p` set for good —
    /// Desktop copies prior recipients forward — so the inherited tag says
    /// nothing at all about who the next comment is for. Only the body does.
    #[tokio::test]
    async fn an_active_root_ignores_a_comment_handed_to_another_agent() {
        let recorder = PromptRecorder::new();
        let mut rt = Runtime::known_as(&recorder, DISPLAY_NAME).await;
        let agent = rt.agent.clone();
        let other = Keys::generate();
        rt.discover_announced_by(&agent, "comment-e2e").await;

        let root = desktop_root_on_an_owned_repo(
            &rt.owner,
            &agent,
            "comment-e2e",
            &format!("@{DISPLAY_NAME} the deploy check is flaky after a reconnect"),
        );
        assert!(
            matches!(
                rt.drive(&routed(root.clone(), project::ProjectSubscription::Enrolment).await)
                    .await,
                ProjectDispatched::Queued { queued: true, .. }
            ),
            "precondition: a named root enrols and wakes"
        );
        let opening = recorder.prompts(1).await;
        assert_eq!(opening.len(), 1, "precondition: one opening turn");
        assert!(matches!(
            rt.enrolments.state_of(&root.id.to_hex()),
            project::RootState::Active
        ));

        // …and the follow-up is for somebody else, on the watched subscription
        // the enrolment moved this root to.
        let handed_off = desktop_comment_to_both(
            &rt.owner,
            &agent,
            &other,
            "comment-e2e",
            &root,
            &format!("@{OTHER_HANDLE} could you take it from here"),
        );
        let dispatched = rt
            .drive(
                &routed(
                    handed_off,
                    project::ProjectSubscription::Watched { generation: 1 },
                )
                .await,
            )
            .await;

        assert_eq!(
            dispatched,
            ProjectDispatched::Ignored,
            "an inherited `p` is propagation, not this agent's next turn"
        );
        assert_eq!(
            rt.queue.queued_event_count(&route_key_of(&root)),
            0,
            "and nothing may be waiting behind the turn in flight either"
        );
        assert_eq!(
            recorder.read().len(),
            1,
            "the watched root answered a comment addressed to another agent"
        );
    }

    /// A copied-forward `p` and nothing else, with the root in hand.
    ///
    /// This is the residual claim the comment-first promotion rested on: that
    /// the enrolment subscription's `p` transport, plus a separately verified
    /// root, was proof of address by itself. It is not — Desktop stamps the
    /// repository owner onto every root and copies it into every comment
    /// below, so on an agent-owned project that evidence is present on comments
    /// nobody addressed to the agent, including the very first one. Both the
    /// root and the comment are driven with the fetched candidate supplied, so
    /// "with a separately resolved root" is asserted rather than assumed.
    #[tokio::test]
    async fn a_bare_structural_p_does_not_wake_an_unknown_root() {
        let recorder = PromptRecorder::new();
        let mut rt = Runtime::known_as(&recorder, DISPLAY_NAME).await;
        let agent = rt.agent.clone();
        rt.discover_announced_by(&agent, "comment-e2e").await;

        let root = desktop_root_on_an_owned_repo(&rt.owner, &agent, "comment-e2e", "test");
        let candidate = fetched_root_candidate(&rt, &root).await;
        assert_eq!(
            rt.drive_with_candidate(
                &routed(root.clone(), project::ProjectSubscription::Enrolment).await,
                Some(candidate.clone()),
            )
            .await,
            ProjectDispatched::Ignored,
            "a root whose only claim is the tag Desktop wrote is not an address"
        );

        let follow_up = desktop_comment(
            &rt.owner,
            &agent,
            "comment-e2e",
            &root,
            "could someone look at this today",
        );
        let dispatched = rt
            .drive_with_candidate(
                &routed(follow_up, project::ProjectSubscription::Enrolment).await,
                Some(candidate),
            )
            .await;

        assert_eq!(
            dispatched,
            ProjectDispatched::Ignored,
            "a comment naming nobody addresses nobody, root in hand or not"
        );
        assert!(rt.enrolments.get(&root.id.to_hex()).is_none());
        assert_eq!(rt.queue.queued_event_count(&route_key_of(&root)), 0);

        // Bounded by the addressed comment, as above: if either of the two
        // events had queued, this would not be the first prompt.
        let for_us = desktop_comment(
            &rt.owner,
            &agent,
            "comment-e2e",
            &root,
            &format!("@{DISPLAY_NAME} could you look at this today"),
        );
        rt.drive_with_candidate(
            &routed(for_us, project::ProjectSubscription::Enrolment).await,
            Some(fetched_root_candidate(&rt, &root).await),
        )
        .await;

        let prompts = recorder.prompts(1).await;
        assert_eq!(prompts.len(), 1, "exactly one turn: {prompts:?}");
        assert!(
            prompts[0].contains("could you look at this today"),
            "the turn carried the wrong event: {}",
            prompts[0]
        );
    }

    // ── Drain ─────────────────────────────────────────────────────────────
    //
    // Same harness, same production entry points. What each scenario is about
    // is what a *drained* runtime does with the very thing the scenarios above
    // prove it does when open, so the contrast is the assertion.

    /// **Take nothing new — and take nothing away either.**
    ///
    /// A project event arriving after the drain is refused, and the refusal
    /// costs the event nothing: no dedup id spent, no `state=queued` announced
    /// on the root, no turn. The proof that nothing was spent is the second
    /// half — the *same signed event*, offered to a runtime that is admitting
    /// again, is accepted in full. Had the refusal consumed the id, announced
    /// the root, or half-enrolled it, this second delivery would have been
    /// refused as a duplicate and the loss would be invisible.
    ///
    /// That is what makes the relay-side promise real: the successor process
    /// starts with an empty `ProjectSeenIds`, its enrolment filter reaches back
    /// from its own startup watermark, and the event is still relay history —
    /// so the comment declined here is delivered to the next binary.
    #[tokio::test]
    async fn a_project_event_refused_while_draining_is_unspent_and_re_admittable() {
        let recorder = PromptRecorder::new();
        let mut rt = Runtime::new(&recorder).await;
        rt.discover("demo").await;

        let bus = observer::ObserverHandle::in_process();
        let mut rx = bus.subscribe();
        rt.observer = Some(bus);

        rt.begin_drain(Duration::from_secs(600));
        let root = issue_root(&rt.owner, &rt.agent, "demo");
        let routed_root = routed(root.clone(), project::ProjectSubscription::Enrolment).await;

        assert_eq!(
            rt.drive(&routed_root).await,
            ProjectDispatched::Ignored,
            "a draining runtime must not admit a project event"
        );
        assert_eq!(rt.queue.queued_event_count(&route_key_of(&root)), 0);
        assert!(
            rt.enrolments.get(&root.id.to_hex()).is_none(),
            "a refused event must not enrol its root either"
        );
        // The acknowledgement of the drain itself is on the bus; nothing after
        // it may be a queued announcement for work that will not run.
        while let Ok(event) = rx.try_recv() {
            assert_ne!(
                event.kind, OBSERVER_PROJECT_QUEUED,
                "a refused event must promise nothing on its root"
            );
        }

        // …and the same event, to a runtime admitting again.
        rt.drain = drain::DrainState::open();
        assert!(
            matches!(
                rt.drive(&routed_root).await,
                ProjectDispatched::Queued { queued: true, .. }
            ),
            "the refusal spent nothing: the identical event is still admittable"
        );
    }

    /// A channel event meets the same refusal, at the one gate both channel
    /// admission sites go through.
    ///
    /// The `false` is not incidental. It is `queue::push`'s own "not accepted"
    /// answer, which is what both call sites already branch on to decide
    /// whether to add the 👀 reaction — so a drained runtime makes no visible
    /// promise about an event it declined.
    #[tokio::test]
    async fn a_channel_event_is_refused_once_the_runtime_is_draining() {
        let recorder = PromptRecorder::new();
        let mut rt = Runtime::new(&recorder).await;
        let channel_id = uuid::Uuid::new_v4();
        let author = rt.owner.clone();
        let queued = move |content: &str| QueuedEvent {
            channel_id,
            event: nostr::EventBuilder::new(nostr::Kind::Custom(9), content)
                .sign_with_keys(&author)
                .expect("sign"),
            received_at: std::time::Instant::now(),
            prompt_tag: "@mention".into(),
            project: None,
        };

        assert!(
            admit_channel_event(&rt.drain, &mut rt.queue, queued("before")),
            "precondition: an open runtime admits"
        );

        rt.begin_drain(Duration::from_secs(600));
        assert!(
            !admit_channel_event(&rt.drain, &mut rt.queue, queued("after")),
            "a draining runtime must refuse a new channel event"
        );
        assert_eq!(
            rt.queue.queued_event_count(&channel_id),
            1,
            "only the pre-drain event is held"
        );
    }

    /// A heartbeat is a turn nobody asked for, so a draining runtime starts
    /// none — otherwise the drain would keep manufacturing the very work it is
    /// waiting to finish, and never converge.
    #[tokio::test]
    async fn a_draining_runtime_starts_no_heartbeat() {
        let recorder = PromptRecorder::new();
        let mut rt = Runtime::new(&recorder).await;
        let mut heartbeat_in_flight = false;

        rt.begin_drain(Duration::from_secs(600));
        dispatch_heartbeat(&mut rt.pool, &rt.ctx, &mut heartbeat_in_flight, &rt.drain);

        assert!(!heartbeat_in_flight, "no heartbeat turn may begin");
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert!(
            recorder.read().is_empty(),
            "the child received a prompt a draining runtime never asked for"
        );

        // The contrast: the same call on an admitting runtime does start one,
        // so the assertion above is about the drain and not about the harness.
        rt.drain = drain::DrainState::open();
        dispatch_heartbeat(&mut rt.pool, &rt.ctx, &mut heartbeat_in_flight, &rt.drain);
        assert!(heartbeat_in_flight);
        assert_eq!(recorder.prompts(1).await.len(), 1);
    }

    /// **Finish what you have.** The whole arc a deployer is buying, end to
    /// end: a queued root, a drain landing while its turn is in flight, a
    /// second comment declined, the turn running to completion, and only then
    /// the run loop's exit condition becoming true.
    ///
    /// The middle assertion is the load-bearing one. Between dispatch and the
    /// result returning, the queue's buffers are empty — the events are inside
    /// a running prompt — so a drain that asked "is anything queued" would have
    /// concluded it was done and exited on top of a live turn. `should_exit`
    /// says no, which is exactly the promise.
    #[tokio::test]
    async fn queued_work_runs_dry_before_a_drain_reaches_its_exit() {
        let recorder = PromptRecorder::new();
        let mut rt = Runtime::new(&recorder).await;
        rt.discover("demo").await;

        let root = issue_root(&rt.owner, &rt.agent, "demo");
        assert!(
            matches!(
                rt.drive(&routed(root.clone(), project::ProjectSubscription::Enrolment).await)
                    .await,
                ProjectDispatched::Queued { queued: true, .. }
            ),
            "precondition: the root queues and dispatches"
        );

        rt.begin_drain(Duration::from_secs(600));
        assert_eq!(
            rt.drain_exit(),
            None,
            "a turn is in flight — the drain must wait for it"
        );

        // Nothing new, even for the root already being worked on.
        let follow_up = addressed_comment(&rt.owner, &rt.agent, "demo", &root);
        assert_eq!(
            rt.drive(&routed(follow_up, project::ProjectSubscription::Enrolment).await)
                .await,
            ProjectDispatched::Ignored,
        );

        // The in-flight turn completes on its own terms.
        assert_eq!(recorder.prompts(1).await.len(), 1);
        rt.reap_one_turn().await;

        assert_eq!(
            rt.drain_exit(),
            Some(drain::DrainExit::Complete),
            "hands empty — the run loop leaves, and `tokio_main` returns Ok(())"
        );
        assert_eq!(
            recorder.read().len(),
            1,
            "the refused follow-up must never have reached the child"
        );
    }

    /// Work queued but not yet dispatched is run dry too, not abandoned.
    ///
    /// The pool is empty at admission time, so the root queues without
    /// dispatching — the shape a busy runtime is always in. The drain then has
    /// to actually flush it rather than exiting on a queue it can see is
    /// non-empty, which is why the run loop flushes on drain onset instead of
    /// waiting for an inbound event that a drain has just stopped accepting.
    #[tokio::test]
    async fn a_drain_flushes_work_that_was_queued_but_never_dispatched() {
        let recorder = PromptRecorder::new();
        let mut rt = Runtime::new(&recorder).await;
        rt.discover("demo").await;

        // Take the only child out of its slot, so admission queues and stops.
        let held = rt.pool.try_claim(None).expect("one idle agent");
        let root = issue_root(&rt.owner, &rt.agent, "demo");
        assert!(matches!(
            rt.drive(&routed(root.clone(), project::ProjectSubscription::Enrolment).await)
                .await,
            ProjectDispatched::Queued { queued: true, .. }
        ));
        assert_eq!(rt.queue.queued_event_count(&route_key_of(&root)), 1);

        rt.begin_drain(Duration::from_secs(600));
        assert_eq!(
            rt.drain_exit(),
            None,
            "queued work is work — the drain must not exit over it"
        );

        // The child comes back, and the drain's flush finds the backlog.
        rt.pool.return_agent(held);
        let mut last_activity = tokio::time::Instant::now();
        dispatch_pending(&mut rt.pool, &mut rt.queue, &rt.ctx, &mut last_activity);

        assert_eq!(recorder.prompts(1).await.len(), 1, "the backlog ran");
        rt.reap_one_turn().await;
        assert_eq!(rt.drain_exit(), Some(drain::DrainExit::Complete));
    }

    /// **A drain cannot hang.** With work still in hand at the bound, the run
    /// loop leaves anyway — and says so as an error, because abandoning work is
    /// a fact an operator has to be told rather than a quiet success.
    #[tokio::test(start_paused = true)]
    async fn a_drain_with_stuck_work_still_exits_at_its_bound() {
        let recorder = PromptRecorder::new();
        let mut rt = Runtime::new(&recorder).await;
        let channel_id = uuid::Uuid::new_v4();
        rt.queue.push(QueuedEvent {
            channel_id,
            event: nostr::EventBuilder::new(nostr::Kind::Custom(9), "never runs")
                .sign_with_keys(&rt.owner)
                .expect("sign"),
            received_at: std::time::Instant::now(),
            prompt_tag: "@mention".into(),
            project: None,
        });

        let bound = Duration::from_secs(600);
        rt.begin_drain(bound);
        assert_eq!(rt.drain_exit(), None);

        tokio::time::advance(bound).await;
        assert_eq!(rt.drain_exit(), Some(drain::DrainExit::BoundExpired));
    }

    /// The promise the queued announcement made is taken back before the
    /// process goes.
    ///
    /// A queued project event lit `state=queued` on its issue. If the drain
    /// bounds out on top of it, the indicator would otherwise stay lit until
    /// the consumer's staleness window closed it — an agent that has already
    /// exited, still shown as about to start. The abandoned batch produces a
    /// terminal frame on the same root, and the activity publisher turns that
    /// into `state=idle`.
    #[tokio::test]
    async fn work_abandoned_at_the_bound_clears_the_indicator_it_lit() {
        let recorder = PromptRecorder::new();
        let mut rt = Runtime::new(&recorder).await;
        rt.discover("demo").await;

        let bus = observer::ObserverHandle::in_process();
        let rx = bus.subscribe();
        rt.observer = Some(bus);
        let agent_hex = rt.agent.public_key().to_hex().to_ascii_lowercase();
        let (publisher, mut published) = relay::RelayEventPublisher::test_pair();
        let task = tokio::spawn(run_project_activity_publisher(
            rx,
            publisher,
            rt.agent.clone(),
            agent_hex,
        ));

        // Queue a root with no child free, so it is announced and then stuck.
        let _held = rt.pool.try_claim(None).expect("one idle agent");
        let root = issue_root(&rt.owner, &rt.agent, "demo");
        let root_id = root.id.to_hex();
        assert!(matches!(
            rt.drive(&routed(root, project::ProjectSubscription::Enrolment).await)
                .await,
            ProjectDispatched::Queued { queued: true, .. }
        ));

        let queued_frame = tokio::time::timeout(Duration::from_secs(5), published.recv())
            .await
            .expect("the queued announcement must reach the relay")
            .expect("publisher channel closed");
        assert_eq!(tag_value(&queued_frame, "state").as_deref(), Some("queued"));

        // The drain gives up on it, and takes the announcement back.
        clear_queued_project_announcements(&mut rt.queue, rt.observer.as_ref());

        let cleared = tokio::time::timeout(Duration::from_secs(5), published.recv())
            .await
            .expect("the indicator must be cleared, not left to expire")
            .expect("publisher channel closed");
        task.abort();

        assert_eq!(tag_value(&cleared, "state").as_deref(), Some("idle"));
        assert_eq!(
            tag_value(&cleared, "e").as_deref(),
            Some(root_id.as_str()),
            "the clearing frame must land on the root that was promised"
        );
        assert!(
            !rt.queue.has_undrained_work(),
            "the batch is disposed of, so the queue and the wire agree"
        );
    }

    /// First tag value for `key`, or `None`.
    fn tag_value(event: &nostr::Event, key: &str) -> Option<String> {
        event.tags.iter().find_map(|t| {
            let s = t.as_slice();
            (s.first().map(String::as_str) == Some(key))
                .then(|| s.get(1).cloned())
                .flatten()
        })
    }
}
