//! Harness-side Buzz relay client.
//!
//! Connects to the Buzz relay via NIP-01 WebSocket, authenticates via NIP-42,
//! discovers channels via REST API, and streams events back to the harness main
//! loop. Also publishes ephemeral events (typing indicators) via the same
//! WebSocket connection.
//!
//! ## Architecture
//!
//! `HarnessRelay::connect()` retries a transient initial connect/auth failure
//! (e.g. a dropped handshake on a spotty link) with bounded jittered backoff
//! before giving up; a terminal configuration/auth error fails immediately.
//!
//! A background tokio task owns the WebSocket stream. It:
//! - Responds to Ping frames with Pong (preventing relay disconnect on long turns)
//! - Forwards `BuzzEvent`s through an `mpsc` channel
//! - Handles reconnection with `since` filters to avoid event loss
//! - Responds to mid-session AUTH challenges
//! - Publishes ephemeral events (typing indicators) via `PublishEvent` commands
//!
//! `HarnessRelay` communicates with the background task via a `RelayCommand`
//! channel. `next_event()` reads from the event receiver.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::time::Duration;

/// Default capacity of the event channel from background task to harness.
/// Override with `BUZZ_ACP_EVENT_BUFFER` env var at startup.
const EVENT_CHANNEL_CAPACITY_DEFAULT: usize = 256;
/// Capacity of the command channel from harness to background task.
const CMD_CHANNEL_CAPACITY: usize = 64;

/// Read the event channel capacity from the environment, falling back to the
/// compiled-in default. Parsed once at call-site (connect time).
fn event_channel_capacity() -> usize {
    std::env::var("BUZZ_ACP_EVENT_BUFFER")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .map(|v| v.max(1)) // mpsc::channel panics on capacity 0
        .unwrap_or(EVENT_CHANNEL_CAPACITY_DEFAULT)
}
/// Maximum number of seen event IDs before the dedup set is rotated.
/// Two-generation dedup: each generation holds up to SEEN_ID_LIMIT/2 entries.
pub(crate) const SEEN_ID_LIMIT: usize = 12_000;

/// Interval between client-initiated WebSocket pings.
const PING_INTERVAL: Duration = Duration::from_secs(30);
/// If no pong is received within this duration after a ping, the connection is
/// considered dead and the background task triggers a reconnect.
const PONG_TIMEOUT: Duration = Duration::from_secs(10);
/// Timeout for individual ws.send() calls. Prevents a stalled socket from
/// wedging the background task indefinitely.
const WS_SEND_TIMEOUT_SECS: u64 = 10;
/// Diagnostic threshold: log when a connection has been stable for this long.
/// The stability block resets `BgState::backoff_step` to 0 here so the next
/// drop after a long healthy run retries at the short end of the ladder again.
const STABLE_CONNECTION_SECS: u64 = 60;
/// Seconds subtracted from `since` on resubscribe to tolerate clock skew.
const SINCE_SKEW_SECS: u64 = 5;
/// Timeout for the NIP-42 auth handshake steps.
///
/// Raised from 5s to 20s (≈2 RTTs at the observed 10s max round-trip on degraded
/// links) so auth doesn't time out before the first WS frame arrives.
const AUTH_TIMEOUT: Duration = Duration::from_secs(20);
/// Timeout for the TCP + WebSocket handshake in `do_connect`.
///
/// Raised from 10s to 30s so the OS TCP connect attempt (SYN→SYN-ACK) has time
/// to succeed at 3.4s average / 10s max observed RTT.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// Backoff delay values shared by the initial-connect retry in
/// `HarnessRelay::connect()` and `try_autonomous_reconnect`'s post-start
/// reconnect loop — a spotty link should get consistent retry pacing whether
/// the failure happens at agent startup or later. Bounded so a dead relay
/// can't hang either path forever.
///
/// The two callers consume this differently: `retry_initial_connect` sleeps
/// before every entry (1 immediate attempt + up to 5 delayed retries, all 5
/// values used), while `try_autonomous_reconnect` skips the sleep after its
/// final attempt (5 attempts total, only the first 4 values used) — so
/// "shared values," not "identical schedule."
const STARTUP_CONNECT_BACKOFFS: [Duration; 5] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(4),
    Duration::from_secs(8),
    Duration::from_secs(16),
];
/// Flat retry interval for DNS failures — no backoff ladder rung consumed.
/// 2s gives name servers a short window to recover from a brownout without driving
/// a tight storm; jitter (±20%) staggers concurrent agent instances.
///
/// DNS flat retries are capped at 10 in the bounded startup/reconnect path
/// (`try_autonomous_reconnect`) so a full brownout cannot hang agent startup
/// indefinitely. In `wait_for_reconnect` the DNS path is unbounded — a
/// reconnecting agent should keep trying across extended outages rather than
/// give up.
const DNS_RETRY_INTERVAL: Duration = Duration::from_secs(2);
/// Minimum inter-REQ spacing during resubscribe bursts.
/// 125 ms ≈ 8 frames/s — safely below the relay's 50-frames-per-5s admission
/// window (10 frames/s at the limit). A 48-channel reconnect spreads over ≈6 s
/// instead of arriving as a single burst that consumes the entire budget at once.
const REQ_PACING_INTERVAL: Duration = Duration::from_millis(125);
/// Maximum REQ frames sent per drain iteration (shared across rate_limited_pending,
/// resubscribe_retry, and control-sub recovery). Keeps any single main-loop tick
/// below the relay's 50-frames/5s budget, and ensures the select! loop is never
/// blocked for more than one REQ's worth of I/O between drain ticks.
const DRAIN_BUDGET_PER_ITER: usize = 1;
/// Maximum observer telemetry frames parked while the rate-limit gate is armed
/// (or the socket is down). The upstream publisher ships at most ONE batched
/// frame per second GLOBALLY (one publish slot per tick, regardless of how
/// many channels are active), so this covers ~4 minutes of gating; beyond that
/// the oldest frames are dropped with visible accounting
/// (`gated_observer_dropped`). Note each dropped frame may carry a whole batch
/// of events, so event-level loss is larger than the frame count.
const GATED_OBSERVER_QUEUE_CAP: usize = 256;
/// Maximum distinct **scopes** parked in the superseding-ephemeral map while
/// the gate is armed.
///
/// Bounds scopes, not frames: a scope holds exactly one frame — its latest —
/// however long the gate lasts and however fast its publisher re-announces, so
/// a 50 s gate over 3 s typing costs one entry per channel rather than sixteen.
/// The live scope count is "channels this agent is typing in" plus "project
/// roots it is working on" plus one for presence, which is single digits in
/// practice; 64 is roughly an order of magnitude of headroom at ~1 KB an entry.
/// Overflow evicts the least-recently-refreshed scope and counts it in
/// `gated_ephemeral_dropped` so the loss is visible, never silent.
const GATED_EPHEMERAL_SCOPE_CAP: usize = 64;

use std::time::Instant;

use buzz_core::kind::{
    KIND_AGENT_OBSERVER_FRAME, KIND_MEMBER_ADDED_NOTIFICATION, KIND_MEMBER_REMOVED_NOTIFICATION,
    KIND_PRESENCE_UPDATE, KIND_PROJECT_ACTIVITY, KIND_STREAM_MESSAGE, KIND_TYPING_INDICATOR,
};
use buzz_core::peer_call::{KIND_PEER_CALL, KIND_PEER_CALL_RESULT};
use futures_util::{SinkExt, StreamExt};
use nostr::{Event, EventBuilder, Keys, Kind, RelayUrl, Tag};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio::time::timeout;
use tokio_tungstenite::{connect_async, tungstenite::Message, MaybeTlsStream, WebSocketStream};
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use crate::config::ChannelFilter;
use crate::project::VerifiedProjectEvent;

/// Metadata about a channel, populated at discovery time.
#[derive(Debug, Clone)]
pub struct ChannelInfo {
    pub name: String,
    pub channel_type: String,
}

pub(crate) fn channel_type_from_tags(tags: &[serde_json::Value]) -> String {
    let mut is_hidden = false;
    let mut is_private = false;
    let mut declared_type = None;
    for tag in tags {
        if let Some(arr) = tag.as_array() {
            match arr.first().and_then(|v| v.as_str()) {
                Some("hidden") => is_hidden = true,
                Some("private") => is_private = true,
                Some("t") => declared_type = arr.get(1).and_then(|v| v.as_str()),
                _ => {}
            }
        }
    }
    if declared_type == Some("dm") || is_hidden {
        "dm".to_string()
    } else if declared_type == Some("private") || is_private {
        "private".to_string()
    } else {
        "stream".to_string()
    }
}

/// Build the discovered-channel subscribe set from the membership UUIDs and the
/// kind:39000 metadata events, **skipping any channel flagged `archived=true`**.
///
/// Archived channels (e.g. auto-archived by the ephemeral-channel reaper) are
/// unusable: re-offering one on reconnect draws a `CLOSED restricted` and would
/// re-form the reconnect loop. Dropping them here is the defense-in-depth
/// backstop to the relay-side live-subscription eviction — it covers a client
/// that was offline when the channel was reaped and so missed the CLOSED.
/// A channel with no metadata event is preserved as `unknown`; security
/// consumers must lazy-resolve it or fail closed rather than assuming stream.
pub(crate) fn merge_discovered_channels(
    channel_uuids: Vec<Uuid>,
    meta_events: &serde_json::Value,
) -> HashMap<Uuid, ChannelInfo> {
    let mut meta_map: HashMap<Uuid, (String, String)> = HashMap::new();
    let mut archived: std::collections::HashSet<Uuid> = std::collections::HashSet::new();
    if let Some(arr) = meta_events.as_array() {
        for ev in arr {
            let tags = match ev.get("tags").and_then(|t| t.as_array()) {
                Some(t) => t,
                None => continue,
            };
            let mut d_val = None;
            let mut name = None;
            let mut is_archived = false;
            for tag in tags {
                if let Some(arr) = tag.as_array() {
                    match arr.first().and_then(|v| v.as_str()) {
                        Some("d") => d_val = arr.get(1).and_then(|v| v.as_str()),
                        Some("name") => name = arr.get(1).and_then(|v| v.as_str()),
                        Some("archived") => {
                            is_archived = arr.get(1).and_then(|v| v.as_str()) == Some("true")
                        }
                        _ => {}
                    }
                }
            }
            if let Some(d) = d_val {
                if let Ok(uuid) = d.parse::<Uuid>() {
                    if is_archived {
                        archived.insert(uuid);
                        continue;
                    }
                    let ch_name = name.unwrap_or("unknown").to_string();
                    let ch_type = channel_type_from_tags(tags);
                    meta_map.insert(uuid, (ch_name, ch_type));
                }
            }
        }
    }

    let mut map = HashMap::with_capacity(channel_uuids.len());
    for uuid in channel_uuids {
        if archived.contains(&uuid) {
            continue;
        }
        let (name, channel_type) = meta_map
            .remove(&uuid)
            .unwrap_or_else(|| ("unknown".to_string(), "unknown".to_string()));
        map.insert(uuid, ChannelInfo { name, channel_type });
    }
    map
}

/// Lightweight HTTP client for pre-prompt context fetches via the Nostr HTTP bridge.
///
/// Extracted from `HarnessRelay` fields so it can be shared (via `Arc`) with
/// spawned prompt tasks without giving them access to the WebSocket.
///
/// All reads go through `POST /query` with NIP-98 auth. Event submission goes
/// through `POST /events` with NIP-98 auth.
#[derive(Debug, Clone)]
pub struct RestClient {
    pub http: reqwest::Client,
    pub base_url: String,
    pub keys: Keys,
    /// Optional NIP-OA auth tag JSON for `x-auth-tag` header (relay membership delegation).
    pub auth_tag_json: Option<String>,
}

/// Whether an HTTP status code is retriable (transient server/rate-limit errors).
fn is_retriable_status(status: reqwest::StatusCode) -> bool {
    matches!(status.as_u16(), 429 | 502 | 503 | 504)
}

/// Base retry delays for transient HTTP failures: 500ms, 1s, 2s.
/// Jitter (±20%) is applied at call time via `jittered_duration`.
const REST_RETRY_BASE_DELAYS: [Duration; 3] = [
    Duration::from_millis(500),
    Duration::from_millis(1000),
    Duration::from_millis(2000),
];

fn unix_now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

impl RestClient {
    /// Sign a NIP-98 HTTP Auth event (kind:27235) for the given method/URL/body.
    ///
    /// Returns the `Authorization: Nostr <base64>` header value (without the
    /// `Nostr ` prefix — caller must prepend it or use `nip98_header`).
    fn sign_nip98(
        &self,
        method: &str,
        url: &str,
        body: Option<&[u8]>,
    ) -> Result<String, RelayError> {
        use base64::Engine;
        use sha2::{Digest, Sha256};

        let u_tag = Tag::parse(["u", url])
            .map_err(|e| RelayError::Http(format!("NIP-98 tag error: {e}")))?;
        let method_tag = Tag::parse(["method", method])
            .map_err(|e| RelayError::Http(format!("NIP-98 tag error: {e}")))?;
        // Nonce prevents replay rejection for rapid-fire requests with identical bodies.
        let nonce_tag = Tag::parse(["nonce", &uuid::Uuid::new_v4().to_string()])
            .map_err(|e| RelayError::Http(format!("NIP-98 tag error: {e}")))?;
        let mut tags = vec![u_tag, method_tag, nonce_tag];

        if let Some(b) = body {
            let hash = hex::encode(Sha256::digest(b));
            let payload_tag = Tag::parse(["payload", &hash])
                .map_err(|e| RelayError::Http(format!("NIP-98 tag error: {e}")))?;
            tags.push(payload_tag);
        }

        let event = EventBuilder::new(Kind::HttpAuth, "")
            .tags(tags)
            .sign_with_keys(&self.keys)
            .map_err(|e| RelayError::Http(format!("NIP-98 sign error: {e}")))?;
        let event_json = serde_json::to_string(&event)
            .map_err(|e| RelayError::Http(format!("NIP-98 serialize error: {e}")))?;
        Ok(base64::engine::general_purpose::STANDARD.encode(event_json))
    }

    /// Build the full `Authorization` header value: `Nostr <base64>`.
    fn nip98_header(
        &self,
        method: &str,
        url: &str,
        body: Option<&[u8]>,
    ) -> Result<String, RelayError> {
        Ok(format!("Nostr {}", self.sign_nip98(method, url, body)?))
    }

    /// Retry helper: executes `build_request` up to 4 times (1 attempt + 3 retries)
    /// on transient failures (429, 502, 503, 504, timeout, connect errors).
    ///
    /// NIP-98 auth events are re-signed on each attempt (they have a ±60s window).
    async fn request_with_retry<F, Fut>(
        &self,
        method: &str,
        path: &str,
        build_request: F,
    ) -> Result<reqwest::Response, RelayError>
    where
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = Result<reqwest::Response, reqwest::Error>>,
    {
        let mut last_err = None;

        for (attempt, delay) in std::iter::once(None)
            .chain(REST_RETRY_BASE_DELAYS.iter().map(|d| Some(*d)))
            .enumerate()
        {
            if let Some(base) = delay {
                let jittered = jittered_duration(base);
                tracing::debug!(
                    "retrying {method} {path} (attempt {attempt}) in {:.1}s",
                    jittered.as_secs_f64()
                );
                tokio::time::sleep(jittered).await;
            }

            match build_request().await {
                Ok(resp) if resp.status().is_success() => return Ok(resp),
                Ok(resp) if is_retriable_status(resp.status()) => {
                    let status = resp.status();
                    tracing::warn!("{method} {path} returned retriable HTTP {status}");
                    last_err = Some(RelayError::Http(format!(
                        "{method} {path} returned HTTP {status}"
                    )));
                }
                Ok(resp) => {
                    return Err(RelayError::Http(format!(
                        "{method} {} returned HTTP {}",
                        path,
                        resp.status()
                    )));
                }
                Err(e) if e.is_timeout() || e.is_connect() => {
                    tracing::warn!("{method} {path} network error: {e}");
                    last_err = Some(RelayError::Http(e.to_string()));
                }
                Err(e) => return Err(RelayError::Http(e.to_string())),
            }
        }

        Err(last_err
            .unwrap_or_else(|| RelayError::Http(format!("{method} {path} failed after retries"))))
    }

    /// POST with NIP-98 auth and retry. Re-signs on each attempt.
    async fn bridge_post(
        &self,
        path: &str,
        body_bytes: &[u8],
    ) -> Result<reqwest::Response, RelayError> {
        let url = format!("{}{}", self.base_url, path);
        let body_owned = body_bytes.to_vec();
        let auth_tag_header = self.auth_tag_json.clone();
        self.request_with_retry("POST", path, || {
            // NIP-98 is re-signed each attempt (fresh created_at).
            // sign_nip98 is infallible in practice (key is always valid).
            let auth = self
                .nip98_header("POST", &url, Some(&body_owned))
                .unwrap_or_default();
            let mut req = self
                .http
                .post(&url)
                .header("Authorization", auth)
                .header("Content-Type", "application/json");
            if let Some(ref tag) = auth_tag_header {
                req = req.header("x-auth-tag", tag);
            }
            req.body(body_owned.clone()).send()
        })
        .await
    }

    /// Query events via the HTTP bridge: `POST /query` with NIP-98 auth.
    ///
    /// Accepts a slice of `nostr::Filter` (serialized as JSON array).
    /// Returns the events as a `serde_json::Value` (JSON array of event objects).
    pub async fn query(&self, filters: &[nostr::Filter]) -> Result<Value, RelayError> {
        let body_bytes = serde_json::to_vec(filters)
            .map_err(|e| RelayError::Http(format!("filter serialize error: {e}")))?;
        let resp = self.bridge_post("/query", &body_bytes).await?;
        resp.json()
            .await
            .map_err(|e| RelayError::Http(e.to_string()))
    }

    /// Count events via the HTTP bridge: `POST /count` with NIP-98 auth.
    ///
    /// Accepts a slice of `nostr::Filter` (serialized as JSON array).
    /// Returns the bridge response as a `serde_json::Value` (usually `{ "count": n }`).
    pub async fn count(&self, filters: &[nostr::Filter]) -> Result<Value, RelayError> {
        let body_bytes = serde_json::to_vec(filters)
            .map_err(|e| RelayError::Http(format!("filter serialize error: {e}")))?;
        let resp = self.bridge_post("/count", &body_bytes).await?;
        resp.json()
            .await
            .map_err(|e| RelayError::Http(e.to_string()))
    }

    /// Submit a signed event via the HTTP bridge: `POST /events` with NIP-98 auth.
    ///
    /// The event must already be signed. Returns the relay response JSON.
    pub async fn submit_event(&self, event: &Event) -> Result<Value, RelayError> {
        let body_bytes = serde_json::to_vec(event)
            .map_err(|e| RelayError::Http(format!("event serialize error: {e}")))?;
        let resp = self.bridge_post("/events", &body_bytes).await?;
        let text = resp
            .text()
            .await
            .map_err(|e| RelayError::Http(e.to_string()))?;
        if text.is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_str(&text).map_err(|e| RelayError::Http(e.to_string()))
    }
}

/// Events the harness cares about.
#[derive(Debug, Clone)]
pub enum BuzzEvent {
    /// An event delivered on a channel subscription.
    Channel {
        /// Which channel this event belongs to.
        channel_id: Uuid,
        /// The underlying Nostr event.
        event: Event,
    },
    /// An event delivered on a project subscription.
    ///
    /// The witness is carried across the runtime boundary rather than unwrapped
    /// back into a raw `Event` on the way out of the relay task. Verifying here
    /// and handing on a bare event would put the project trust boundary back on
    /// convention one function later — the whole point of the witness is that
    /// `lib.rs` cannot classify authority for something unverified, because it
    /// has nothing unverified to classify.
    Project(crate::project::ProjectEvent),
}

/// Errors from relay operations.
#[derive(Debug, thiserror::Error)]
pub enum RelayError {
    #[error("WebSocket error: {0}")]
    WebSocket(Box<tokio_tungstenite::tungstenite::Error>),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("Auth failed: {0}")]
    AuthFailed(String),

    #[error("No auth challenge received")]
    NoAuthChallenge,

    #[error("Connection closed")]
    ConnectionClosed,

    #[error("Timeout")]
    Timeout,

    #[error("HTTP error: {0}")]
    Http(String),

    #[error("Unexpected message: {0}")]
    UnexpectedMessage(String),
}

impl From<nostr::event::builder::Error> for RelayError {
    fn from(e: nostr::event::builder::Error) -> Self {
        RelayError::AuthFailed(e.to_string())
    }
}

/// A parsed NIP-01 relay message.
#[derive(Debug, Clone)]
enum RelayMessage {
    Event {
        subscription_id: String,
        event: Box<Event>,
    },
    Ok {
        event_id: String,
        accepted: bool,
        message: String,
    },
    Eose {
        subscription_id: String,
    },
    Closed {
        subscription_id: String,
        message: String,
    },
    Notice {
        message: String,
    },
    Auth {
        challenge: String,
    },
}

/// Subscription ID for the global membership notification subscription.
const MEMBERSHIP_NOTIF_SUB_ID: &str = "membership-notif";
/// Subscription ID for encrypted owner-to-agent observer control frames.
const OBSERVER_CONTROL_SUB_ID: &str = "agent-observer-control";
/// Subscription ID for NIP-PC peer agent calls and their results.
///
/// Global rather than per-channel because a call's delivery must not depend on
/// how the operator configured channel subscriptions. `--subscribe-mode` can set
/// `kinds` to a list that omits `43001`, and `require_mention` puts `#p:[agent]`
/// on the whole filter — under which this agent's *own* outgoing calls (whose
/// `p` names the callee) are never echoed back, and the ledger that correlates
/// results would never learn the call was made.
const PEER_CALL_SUB_ID: &str = "peer-call";

/// Commands sent from `HarnessRelay` to the background WebSocket task.
enum RelayCommand {
    /// Subscribe to a channel (sends a NIP-01 REQ) with the given filter.
    Subscribe {
        channel_id: Uuid,
        filter: ChannelFilter,
        replay_since: Option<u64>,
    },
    /// Unsubscribe from a channel (sends a NIP-01 CLOSE).
    Unsubscribe { channel_id: Uuid },
    /// Reconnect to the relay (re-authenticate and resubscribe).
    Reconnect,
    /// Shut down the background task.
    Shutdown,
    /// Subscribe to global membership notifications.
    SubscribeMembership,
    /// Subscribe to encrypted observer control frames addressed to this agent.
    SubscribeObserverControls,
    /// Subscribe to NIP-PC peer calls addressed to this agent, and to the
    /// calls this agent itself publishes.
    SubscribePeerCalls,
    /// Publish a signed event to the relay (for typing indicators, etc.).
    PublishEvent { event: Box<Event> },
    /// Floor `since` for membership notification replay; events before startup are never re-delivered.
    SetStartupWatermark { ts: u64 },
    /// Open the repository-discovery REQ, registering it in lockstep.
    ///
    /// `filters` is the REQ's whole filter list, ORed, in wire order. Empty
    /// opens nothing — see [`HarnessRelay::submit_project_discovery`].
    ///
    /// **Discovery only, and it carries no id and no class.** This was
    /// `SubscribeProject { sub_id, subscription, filters }`, which let any
    /// crate caller submit `ProjectSubscription::Watched { generation: 99 }`
    /// under an id of its choosing. That installed durable watched intent
    /// outside the semantic replacement owner, so the owner's next replacement
    /// derived a predecessor that did not account for it and left the
    /// manufactured generation durable beside the successor — the stale
    /// predecessor defect, reached through a neighbouring command rather than
    /// through the run loop.
    ///
    /// Removing the generic capability is what closes it. A promise not to
    /// pass `Watched` would not have: the defect was that the argument existed.
    /// Discovery is the one project subscription with no prior state to derive
    /// from and a fixed id, so it is the only one that can be opened rather
    /// than replaced — and the handler, not the sender, supplies both.
    SubscribeProjectDiscovery { filters: Vec<Value> },
    /// Replace a live project subscription, transactionally.
    ///
    /// Distinct from [`RelayCommand::SubscribeProjectDiscovery`] because the
    /// registry distinguishes them: opening refuses to change the identity held
    /// under an id, and replacement is the operation permitted to. Folding them
    /// into one command would put that decision at the call site rather than in
    /// the registry that owns it.
    /// **Semantic, not addressed.** The command names the class it wants
    /// replaced and the filters it wants asked. It carries no id, no
    /// generation and no predecessor, because the component that knows what is
    /// installed is the registry on the far side of this channel — and a
    /// sender that could name a predecessor could name one that was never
    /// installed.
    ReplaceProject {
        replacement: crate::project::ProjectReplacement,
        filters: Vec<Value>,
    },
    /// Begin — or widen and restart — the walk back through the roots this
    /// agent is already addressed on.
    ///
    /// Separate from the enrolment REQ it accompanies, because the two are
    /// different questions with different lifetimes: the enrolment REQ is one
    /// standing tail under a fixed id, and history is a sequence of
    /// generation-distinct pages that ends. Folding them into one command is
    /// what produced the request whose fixed identity could not paginate, and
    /// so could only ever sample thirty days of history and call it complete.
    ///
    /// Carries the coordinate set and the agent rather than a filter: the
    /// cursor derives every page's filter from its own bound, and a
    /// caller-supplied one could describe a different question from the page it
    /// is bound to.
    BeginEnrolmentHistory {
        coordinates: Vec<String>,
        agent: String,
    },
    /// Rebuild one restored root's own history — its comments, its revisions
    /// and, the reason this exists, its lifecycle.
    ///
    /// Carries a [`crate::project::VerifiedBoundRoot`] rather than a root id
    /// because the proof can only be minted where the discovered set lives,
    /// which is the run loop. A command carrying a bare id would let this task
    /// start rebuilding a root nothing had validated, and the merge's own check
    /// — that the streams present are the ones this root's *class* requires —
    /// would then be answered from a class the relay task guessed.
    BeginRootCatchUp {
        root: Box<crate::project::VerifiedBoundRoot>,
    },
}

type WsStream = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

/// Harness-side relay client.
///
/// Connects to the Buzz relay, authenticates via NIP-42, and streams
/// matching events for subscribed channels.
///
/// A background tokio task owns the WebSocket connection and responds to
/// Ping frames, preventing disconnection during long agent turns.
pub struct HarnessRelay {
    /// Receiver for events forwarded by the background task.
    event_rx: mpsc::Receiver<Option<BuzzEvent>>,
    /// Receiver for encrypted observer control events addressed to this agent.
    observer_control_rx: Option<mpsc::Receiver<Event>>,
    /// Sender for commands to the background task.
    cmd_tx: mpsc::Sender<RelayCommand>,
    /// HTTP client for HTTP bridge calls.
    http: reqwest::Client,
    /// WebSocket URL of the relay.
    relay_url: String,
    /// Keys used for NIP-42 signing and NIP-98 HTTP auth.
    keys: Keys,
    /// Optional NIP-OA auth tag for relay membership delegation.
    auth_tag: Option<nostr::Tag>,
    /// Handle to the background task (for clean shutdown).
    /// Wrapped in `Option` so `shutdown()` can take ownership without conflicting
    /// with `Drop` (which only has `&mut self`).
    bg_handle: Option<tokio::task::JoinHandle<()>>,
}

/// Cloneable publisher handle for signed events on the relay background socket.
#[derive(Clone)]
pub struct RelayEventPublisher {
    cmd_tx: mpsc::Sender<RelayCommand>,
}

impl RelayEventPublisher {
    /// Publish a signed event through the relay background task.
    pub async fn publish_event(&self, event: Event) -> Result<(), RelayError> {
        self.cmd_tx
            .send(RelayCommand::PublishEvent {
                event: Box::new(event),
            })
            .await
            .map_err(|_| RelayError::ConnectionClosed)
    }

    /// Test-only publisher pair: published events are forwarded to the
    /// returned receiver instead of a live relay socket.
    #[cfg(test)]
    pub(crate) fn test_pair() -> (Self, mpsc::Receiver<Event>) {
        let (cmd_tx, mut cmd_rx) = mpsc::channel::<RelayCommand>(64);
        let (event_tx, event_rx) = mpsc::channel(64);
        tokio::spawn(async move {
            while let Some(cmd) = cmd_rx.recv().await {
                if let RelayCommand::PublishEvent { event } = cmd {
                    if event_tx.send(*event).await.is_err() {
                        break;
                    }
                }
            }
        });
        (Self { cmd_tx }, event_rx)
    }
}

impl HarnessRelay {
    /// Connect to relay and authenticate via NIP-42.
    ///
    /// `auth_tag` is an optional NIP-OA owner attestation included in the AUTH
    /// event for relay membership delegation.
    pub async fn connect(
        relay_url: &str,
        keys: &Keys,
        agent_pubkey_hex: &str,
        auth_tag: Option<nostr::Tag>,
    ) -> Result<Self, RelayError> {
        // Perform the initial connection and auth handshake, retrying
        // transient failures (dropped handshake, timeout) with bounded
        // jittered backoff. A terminal error (bad URL, bad auth tag,
        // rejected/invalid signing key) fails immediately — see
        // `is_terminal_connect_error`.
        let (ws, handshake_buffer) =
            retry_initial_connect(|| do_connect(relay_url, keys, auth_tag.as_ref())).await?;

        let (event_tx, event_rx) = mpsc::channel::<Option<BuzzEvent>>(event_channel_capacity());
        let (observer_control_tx, observer_control_rx) =
            mpsc::channel::<Event>(event_channel_capacity());
        let (cmd_tx, cmd_rx) = mpsc::channel::<RelayCommand>(CMD_CHANNEL_CAPACITY);

        let bg_keys = keys.clone();
        let bg_relay_url = relay_url.to_string();
        let bg_agent_pubkey_hex = agent_pubkey_hex.to_string();
        let bg_auth_tag = auth_tag.clone();

        let bg_handle = tokio::spawn(async move {
            run_background_task(
                ws,
                handshake_buffer,
                event_tx,
                observer_control_tx,
                cmd_rx,
                bg_keys,
                bg_relay_url,
                bg_agent_pubkey_hex,
                bg_auth_tag,
            )
            .await;
        });

        Ok(Self {
            event_rx,
            observer_control_rx: Some(observer_control_rx),
            cmd_tx,
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .connect_timeout(std::time::Duration::from_secs(5))
                .build()
                .map_err(|e| RelayError::Http(format!("failed to build HTTP client: {e}")))?,
            relay_url: relay_url.to_string(),
            keys: keys.clone(),
            auth_tag,
            bg_handle: Some(bg_handle),
        })
    }

    /// Discover channels the agent is a member of.
    ///
    /// Queries kind:39002 (NIP-29 group members) events where `#p` includes
    /// the agent pubkey to find channel memberships, then queries kind:39000
    /// (group metadata) for channel names and types.
    pub async fn discover_channels(&self) -> Result<HashMap<Uuid, ChannelInfo>, RelayError> {
        use nostr::{Alphabet, SingleLetterTag};

        let rest = self.rest_client();
        let pk_hex = self.keys.public_key().to_hex();

        // Step 1: Find all channels where agent is a member (kind:39002 with #p tag).
        let p_tag = SingleLetterTag::lowercase(Alphabet::P);
        let member_filter = nostr::Filter::new()
            .kind(Kind::Custom(
                buzz_core::kind::KIND_NIP29_GROUP_MEMBERS as u16,
            ))
            .custom_tags(p_tag, [pk_hex.as_str()]);
        let member_events = rest.query(&[member_filter]).await?;

        let member_arr = member_events
            .as_array()
            .ok_or_else(|| RelayError::Http("expected JSON array from /query (members)".into()))?;

        // Extract channel UUIDs from #d tags.
        let mut channel_uuids: Vec<Uuid> = Vec::new();
        for ev in member_arr {
            if let Some(tags) = ev.get("tags").and_then(|t| t.as_array()) {
                for tag in tags {
                    if let Some(arr) = tag.as_array() {
                        if arr.first().and_then(|v| v.as_str()) == Some("d") {
                            if let Some(d_val) = arr.get(1).and_then(|v| v.as_str()) {
                                if let Ok(uuid) = d_val.parse::<Uuid>() {
                                    channel_uuids.push(uuid);
                                }
                            }
                        }
                    }
                }
            }
        }

        if channel_uuids.is_empty() {
            debug!("discovered 0 channel(s)");
            return Ok(HashMap::new());
        }

        // Step 2: Fetch metadata (kind:39000) for discovered channels.
        let d_tag = SingleLetterTag::lowercase(Alphabet::D);
        let d_values: Vec<String> = channel_uuids.iter().map(|u| u.to_string()).collect();
        let meta_filter = nostr::Filter::new()
            .kind(Kind::Custom(
                buzz_core::kind::KIND_NIP29_GROUP_METADATA as u16,
            ))
            .custom_tags(d_tag, d_values);
        let meta_events = rest.query(&[meta_filter]).await?;

        // Step 3: Build the final subscribe set, skipping archived channels.
        let map = merge_discovered_channels(channel_uuids, &meta_events);

        debug!("discovered {} channel(s)", map.len());
        Ok(map)
    }

    /// Build a [`RestClient`] that shares this relay's HTTP credentials.
    ///
    /// The returned client is cheap to clone (wraps `reqwest::Client` which is
    /// internally `Arc`-ed) and safe to share across spawned tasks via `Arc`.
    pub fn rest_client(&self) -> RestClient {
        RestClient {
            http: self.http.clone(),
            base_url: relay_ws_to_http(&self.relay_url),
            keys: self.keys.clone(),
            auth_tag_json: self
                .auth_tag
                .as_ref()
                .and_then(|t| serde_json::to_string(t.as_slice()).ok()),
        }
    }

    /// Subscribe to events in a channel using the given filter.
    ///
    /// Sends a `Subscribe` command to the background task, which issues the
    /// NIP-01 `REQ` built from `filter`. Subscription ID is `ch-<uuid>`.
    pub async fn subscribe_channel(
        &mut self,
        channel_id: Uuid,
        filter: ChannelFilter,
    ) -> Result<(), RelayError> {
        self.subscribe_channel_from(channel_id, filter, None).await
    }

    /// Subscribe to events in a channel, replaying from a known timestamp.
    ///
    /// Used for channels discovered from membership notifications: the mention
    /// that invited an agent can be published immediately after the membership
    /// event, before this subscription is active. Replaying from the membership
    /// event timestamp closes that race.
    pub async fn subscribe_channel_from(
        &mut self,
        channel_id: Uuid,
        filter: ChannelFilter,
        replay_since: Option<u64>,
    ) -> Result<(), RelayError> {
        self.cmd_tx
            .send(RelayCommand::Subscribe {
                channel_id,
                filter,
                replay_since,
            })
            .await
            .map_err(|_| RelayError::ConnectionClosed)?;
        debug!("queued subscribe for channel {channel_id}");
        Ok(())
    }

    /// Subscribe to membership notifications for this agent.
    pub async fn subscribe_membership_notifications(&mut self) -> Result<(), RelayError> {
        self.cmd_tx
            .send(RelayCommand::SubscribeMembership)
            .await
            .map_err(|_| RelayError::ConnectionClosed)?;
        Ok(())
    }

    /// Subscribe to encrypted observer control frames addressed to this agent.
    pub async fn subscribe_observer_controls(&mut self) -> Result<(), RelayError> {
        self.cmd_tx
            .send(RelayCommand::SubscribeObserverControls)
            .await
            .map_err(|_| RelayError::ConnectionClosed)?;
        Ok(())
    }

    /// Subscribe to NIP-PC peer calls (kind 43001) and results (kind 43004).
    pub async fn subscribe_peer_calls(&mut self) -> Result<(), RelayError> {
        self.cmd_tx
            .send(RelayCommand::SubscribePeerCalls)
            .await
            .map_err(|_| RelayError::ConnectionClosed)?;
        Ok(())
    }

    /// Take the observer-control receiver for polling outside this relay object.
    pub fn take_observer_control_rx(&mut self) -> Option<mpsc::Receiver<Event>> {
        self.observer_control_rx.take()
    }

    /// Return a cloneable publisher handle for signed relay events.
    pub fn event_publisher(&self) -> RelayEventPublisher {
        RelayEventPublisher {
            cmd_tx: self.cmd_tx.clone(),
        }
    }

    /// Unsubscribe from a channel.
    pub async fn unsubscribe_channel(&mut self, channel_id: Uuid) -> Result<(), RelayError> {
        self.cmd_tx
            .send(RelayCommand::Unsubscribe { channel_id })
            .await
            .map_err(|_| RelayError::ConnectionClosed)?;
        debug!("queued unsubscribe for channel {channel_id}");
        Ok(())
    }

    /// Wait for the next event from any subscribed channel.
    ///
    /// Reads from the background task's event channel. Returns `None` on
    /// connection loss — the caller should call [`reconnect`](Self::reconnect).
    pub async fn next_event(&mut self) -> Option<BuzzEvent> {
        // The background task sends `None` to signal connection loss.
        self.event_rx.recv().await.flatten()
    }

    /// Publish a signed event to the relay via the background WebSocket task.
    ///
    /// Blocks until the command channel has capacity. For ephemeral events
    /// (typing indicators) prefer [`try_publish_event`] which never blocks.
    #[allow(dead_code)] // Public API — callers outside the harness may use this
    pub async fn publish_event(&self, event: Event) -> Result<(), RelayError> {
        self.cmd_tx
            .send(RelayCommand::PublishEvent {
                event: Box::new(event),
            })
            .await
            .map_err(|_| RelayError::ConnectionClosed)
    }

    /// Fire-and-forget publish — uses `try_send` so it never blocks the caller.
    ///
    /// Suitable for ephemeral commands like typing indicators where dropping
    /// the event on a full command channel is acceptable.
    pub fn try_publish_event(&self, event: Event) -> Result<(), RelayError> {
        self.cmd_tx
            .try_send(RelayCommand::PublishEvent {
                event: Box::new(event),
            })
            .map_err(|_| RelayError::ConnectionClosed)
    }

    /// Build a typing indicator event (kind:20002) for a channel.
    pub fn build_typing_event(
        &self,
        channel_id: Uuid,
        root_event_id: Option<&str>,
        parent_event_id: Option<&str>,
    ) -> Result<Event, RelayError> {
        let h_tag = Tag::parse(["h", &channel_id.to_string()])
            .map_err(|e| RelayError::AuthFailed(e.to_string()))?;
        let mut tags = vec![h_tag];
        if let Some(parent) = parent_event_id {
            if let Some(root) = root_event_id {
                if root != parent {
                    tags.push(
                        Tag::parse(["e", root, "", "root"])
                            .map_err(|e| RelayError::AuthFailed(e.to_string()))?,
                    );
                }
            }
            tags.push(
                Tag::parse(["e", parent, "", "reply"])
                    .map_err(|e| RelayError::AuthFailed(e.to_string()))?,
            );
        }
        let event = EventBuilder::new(Kind::Custom(KIND_TYPING_INDICATOR as u16), "")
            .tags(tags)
            .sign_with_keys(&self.keys)?;
        Ok(event)
    }

    /// Pins the floor `since` for membership notification replay.
    ///
    /// Call once after `connect()` with the Unix timestamp captured just before
    /// the relay connection was established. The background task uses this so
    /// events predating this session are never re-delivered after reconnect.
    pub async fn set_startup_watermark(&self, ts: u64) -> Result<(), RelayError> {
        self.cmd_tx
            .send(RelayCommand::SetStartupWatermark { ts })
            .await
            .map_err(|_| RelayError::ConnectionClosed)
    }

    /// **Submit** the repository-discovery REQ carrying `filters`, ORed.
    ///
    /// The class every inbound frame on this subscription is classified as is
    /// stamped by the background task, not named here — the id's spelling
    /// carries no authority, and neither does the sender's opinion of what it
    /// is opening. Registration happens in lockstep with the write, so a failed
    /// send leaves nothing answerable.
    ///
    /// This took a `sub_id` and a `ProjectSubscription` until it was found to
    /// be a second, unowned producer of watched generations; see
    /// [`RelayCommand::SubscribeProjectDiscovery`] for what that cost.
    ///
    /// A `Vec` because a NIP-01 REQ carries one *or more* filters and this
    /// crate's own watched-root builder returns two — a lowercase `#e` branch
    /// for comments and an uppercase `#E` branch for pull-request revisions.
    /// An empty vector opens nothing: `["REQ", id]` is an unbounded request,
    /// not an empty one, so a builder that produced no filters must produce no
    /// REQ.
    pub async fn submit_project_discovery(&self, filters: Vec<Value>) -> Result<(), RelayError> {
        self.cmd_tx
            .send(RelayCommand::SubscribeProjectDiscovery { filters })
            .await
            .map_err(|_| RelayError::ConnectionClosed)
    }

    /// **Submit** a project-subscription replacement to the background task.
    ///
    /// `Ok(())` means the command was accepted by the channel — *enqueued*, not
    /// written and not installed. The only reachable error is a closed channel.
    /// Whether a REQ reaches the relay, whether the registry installs it, and
    /// which generation it becomes are decided later and elsewhere.
    ///
    /// The name says `submit` for that reason. It was
    /// `replace_project_subscription`, and a caller read its `Ok` as proof the
    /// replacement had happened — then advanced its own generation counter on
    /// the strength of it. There is no counter on this side any more, and the
    /// name no longer invites one.
    ///
    /// See [`RelayCommand::ReplaceProject`] for why this is not
    /// [`Self::submit_project_discovery`] with different arguments.
    pub async fn submit_project_replacement(
        &self,
        replacement: crate::project::ProjectReplacement,
        filters: Vec<Value>,
    ) -> Result<(), RelayError> {
        self.cmd_tx
            .send(RelayCommand::ReplaceProject {
                replacement,
                filters,
            })
            .await
            .map_err(|_| RelayError::ConnectionClosed)
    }

    /// Ask the background task to walk back through the roots this agent is
    /// already addressed on.
    ///
    /// Sent alongside the enrolment replacement rather than derived from it,
    /// because the two are different questions: the enrolment REQ is a standing
    /// tail under a fixed id, and history is a finite sequence of
    /// generation-distinct pages. Nothing about the pages is decided here —
    /// not the bound, not the limit, not the id — for the same reason nothing
    /// about a replacement's generation is.
    pub async fn submit_enrolment_history(
        &self,
        coordinates: Vec<String>,
        agent: String,
    ) -> Result<(), RelayError> {
        self.cmd_tx
            .send(RelayCommand::BeginEnrolmentHistory { coordinates, agent })
            .await
            .map_err(|_| RelayError::ConnectionClosed)
    }

    /// Ask the background task to rebuild one restored root's own history.
    ///
    /// Sent per restored root rather than derived from the enrolment walk,
    /// because the walk asks for roots and this asks about one root's later
    /// events — different filters, different bounds, and a walk that fetched
    /// both would let a busy repository's chatter crowd out the roots the page
    /// budget exists to find.
    pub async fn submit_root_catch_up(
        &self,
        root: crate::project::VerifiedBoundRoot,
    ) -> Result<(), RelayError> {
        self.cmd_tx
            .send(RelayCommand::BeginRootCatchUp {
                root: Box::new(root),
            })
            .await
            .map_err(|_| RelayError::ConnectionClosed)
    }

    /// Reconnect after connection loss. Instructs the background task to
    /// re-authenticate and resubscribe to all previously active channels.
    pub async fn reconnect(&mut self) -> Result<(), RelayError> {
        warn!("relay connection lost — reconnecting…");
        self.cmd_tx
            .send(RelayCommand::Reconnect)
            .await
            .map_err(|_| RelayError::ConnectionClosed)?;
        Ok(())
    }
}

impl HarnessRelay {
    /// Graceful async shutdown — sends Shutdown command and waits up to 5s for
    /// the background task to finish. Use this from async contexts instead of
    /// relying on `Drop` (which aborts immediately).
    pub async fn shutdown(mut self) {
        let _ = self.cmd_tx.send(RelayCommand::Shutdown).await;
        if let Some(handle) = self.bg_handle.take() {
            let abort_handle = handle.abort_handle();
            if tokio::time::timeout(Duration::from_secs(5), handle)
                .await
                .is_err()
            {
                tracing::warn!("relay background task did not finish in 5s — aborting");
                abort_handle.abort();
            }
        }
    }
}

impl Drop for HarnessRelay {
    fn drop(&mut self) {
        // Best-effort shutdown signal; ignore errors (task may already be done).
        let _ = self.cmd_tx.try_send(RelayCommand::Shutdown);
        if let Some(handle) = self.bg_handle.take() {
            handle.abort();
        }
    }
}

/// What [`send_project_discovery`] or [`send_project_replay`] did.
///
/// Replaces a `bool` that conflated a locally refused command with a dead
/// socket. The caller treated every `false` as "reconnect", so a metadata
/// conflict tore down the connection — and the fresh-connection path then
/// replayed intent, opening on a clean registry the very metadata the live
/// registry had just refused. Authority substitution had moved from the map to
/// the reconnect boundary rather than being closed.
///
/// **Only `WriteFailed` may trigger a reconnect.**
#[derive(Debug, PartialEq)]
enum ProjectSendOutcome {
    /// Registered and written.
    Sent,
    /// Already live under identical metadata; no second REQ was emitted.
    AlreadyOpen,
    /// The live registry refused. The socket is fine; our bookkeeping is not.
    MetadataConflict,
    /// The registry refused the filters as unbounded. Nothing written, nothing
    /// recorded, socket fine.
    ///
    /// Separate from [`Self::MetadataConflict`] because they are different
    /// faults with different owners: a conflict says this id belongs to another
    /// request, and this says the question itself asks the relay for
    /// everything.
    UnboundedFilters,
    /// The incarnation space is spent. No REQ was written and none ever will
    /// be — distinct from a conflict, which is about *this* id and could in
    /// principle clear, and from a write failure, which is about the socket.
    ///
    /// Separate so diagnostics do not report a terminal, process-wide state as
    /// a per-request ownership disagreement.
    Exhausted,
    /// The write failed, so nothing was registered — installation happens only
    /// after a successful write, leaving nothing to undo. Durable intent
    /// survives.
    WriteFailed,
    /// The registry refused because the durable record as a whole does not
    /// resolve. Nothing written, nothing recorded, no incarnation burned, and
    /// the socket is fine.
    ///
    /// Separate from [`Self::MetadataConflict`], which is about *this* id
    /// belonging to another request: this one is about the record the id would
    /// have joined, and no other project request can be opened either until it
    /// is resolved.
    InvariantViolation,
}

/// Two-generation dedup set with bounded memory.
///
/// Mitigates the "amnesia window" caused by clearing the entire set at once.
/// When `current` reaches `limit/2` entries it is rotated into `previous`.
/// At any point we remember between `limit/2` and `limit` recent IDs.
/// The oldest `limit/2` IDs are forgotten on each rotation — this is the
/// inherent tradeoff of bounded-memory dedup. For the default limit of
/// 12,000, the worst case is that an ID seen 6,001+ inserts ago may be
/// replayed as new. This is acceptable for Nostr event dedup where the
/// `since` filter provides the primary replay protection.
pub(crate) struct TwoGenDedup {
    current: HashSet<String>,
    previous: HashSet<String>,
    limit: usize,
}

impl TwoGenDedup {
    pub(crate) fn new(limit: usize) -> Self {
        Self {
            current: HashSet::new(),
            previous: HashSet::new(),
            limit,
        }
    }

    pub(crate) fn contains(&self, id: &str) -> bool {
        self.current.contains(id) || self.previous.contains(id)
    }

    /// Insert `id`. Returns `true` if it was new (not a duplicate).
    pub(crate) fn insert(&mut self, id: String) -> bool {
        if self.contains(&id) {
            return false;
        }
        self.current.insert(id);
        if self.current.len() >= self.limit / 2 {
            // Rotate: current → previous, start fresh current.
            self.previous = std::mem::take(&mut self.current);
        }
        true
    }

    /// Remove an ID (used to un-deduplicate a dropped event so it can be
    /// replayed after reconnect).
    fn remove(&mut self, id: &str) {
        self.current.remove(id);
        self.previous.remove(id);
    }
}

/// State maintained by the background WebSocket task.
struct BgState {
    /// Active subscriptions: channel_id → subscription_id string.
    active_subscriptions: HashMap<Uuid, String>,
    /// Most recent `created_at` timestamp seen per channel (for `since` filter).
    last_seen: HashMap<Uuid, u64>,
    /// Two-generation dedup set of event IDs seen on the **channel** surface —
    /// ordinary channel events and membership notifications.
    ///
    /// Deliberately does not cover project events; see [`BgState::project_seen_ids`].
    seen_ids: TwoGenDedup,
    /// Per-channel filter used on subscribe (for resubscribe after reconnect).
    active_filters: HashMap<Uuid, ChannelFilter>,
    /// Oldest timestamp of a membership notification that was dropped due to
    /// backpressure. If set, reconnect replay must start from this timestamp
    /// (minus skew) to re-deliver the lost event. Reset on successful reconnect.
    membership_dropped_since: Option<u64>,
    /// Newest successfully-enqueued membership notification timestamp.
    /// Used as the `since` for reconnect replay when no events were dropped.
    membership_last_seen: Option<u64>,
    /// Whether the membership notification subscription is active.
    membership_sub_active: bool,
    /// Whether the observer control subscription is active.
    observer_control_sub_active: bool,
    /// Whether the NIP-PC peer-call subscription is active.
    peer_call_sub_active: bool,
    /// Oldest dropped channel-event timestamp per channel, keyed by channel_id.
    /// Mirrors `membership_dropped_since` but for ordinary channel events.
    /// On reconnect resubscribe, `since` = min(last_seen, channel_dropped_since).
    /// Cleared per-channel after a successful resubscribe.
    channel_dropped_since: HashMap<Uuid, u64>,
    /// Set by the backpressure handler when the event channel is full.
    /// The main loop checks this flag and triggers a proactive resubscribe
    /// (without waiting for a disconnect) so dropped events are replayed.
    proactive_resubscribe_needed: bool,
    /// Unix timestamp captured just before the relay connection was established.
    /// Used as the floor `since` for membership notification replay so events
    /// predating this session are never re-delivered.
    startup_watermark: Option<u64>,
    /// Replay floor captured when each channel was first subscribed.
    /// Used as the `since` fallback on reconnect for channels that have no
    /// `last_seen` or `channel_dropped_since`. This prevents channels joined
    /// after startup from replaying from an hours-old `startup_watermark`.
    /// Startup-era channels use the startup watermark; dynamic channels use
    /// the membership notification timestamp that caused the subscription.
    subscribe_since: HashMap<Uuid, u64>,
    /// Relay rate-limit gate deadline.
    ///
    /// While `Some(deadline)` and `Instant::now() < deadline`, outbound
    /// admission-counted frames (REQ, EVENT) are deferred or dropped.
    /// `check_rate_gate` lazily clears this to `None` once it expires.
    rate_limit_gate: Option<tokio::time::Instant>,
    /// Channels parked because a CLOSED "rate-limited:" was received.
    ///
    /// Drained by the main loop when the gate clears, one REQ per
    /// `REQ_PACING_INTERVAL` tick via the select-integrated pacing timer.
    /// Value is the `Instant` before which the channel must not be retried.
    rate_limited_pending: HashMap<Uuid, tokio::time::Instant>,
    /// Set when a rate-limited CLOSED arrives for the membership notification
    /// subscription. The main-loop drain re-sends the REQ once the gate clears,
    /// even when `rate_limited_pending` is empty.
    membership_resub_needed: bool,
    /// Set when a rate-limited CLOSED arrives for the observer control
    /// subscription. The main-loop drain re-sends the REQ once the gate clears,
    /// even when `rate_limited_pending` is empty.
    observer_resub_needed: bool,
    /// Set when a rate-limited CLOSED arrives for the peer-call subscription,
    /// or when its REQ could not be written. The main-loop drain re-sends it
    /// once the gate clears.
    peer_call_resub_needed: bool,
    /// Frames classified [`GatedPublish::Durable`] and parked while the
    /// rate-limit gate is armed or the socket is down.
    ///
    /// Named for its only high-volume tenant — kind 24200 observer telemetry —
    /// but it holds every durable publish, because the defining property is
    /// "nobody will say this again": dropping one loses turn history in the
    /// Desktop observer, or a message a person was waiting for. Order is the
    /// point, so this is a FIFO and nothing in it is ever superseded.
    /// Bounded at `GATED_OBSERVER_QUEUE_CAP` (drop-oldest); drained by the
    /// main loop one frame per pacing tick once the gate clears.
    gated_observer_pending: VecDeque<Box<Event>>,
    /// The latest frame each scope is announcing, parked while the rate-limit
    /// gate is armed — see [`GatedPublish::Superseding`].
    ///
    /// One entry per scope, newest wins: a scope's publisher re-announces on a
    /// cadence (3 s typing, 15 s project activity, 60 s presence), so the frame
    /// held here is at most one cadence old when the gate clears, and the older
    /// frame it replaced would have said the same thing later. Ordered by last
    /// refresh (superseding moves a scope to the back) so the drop-oldest bound
    /// evicts the scope that has gone quietest, not the one that is busiest.
    ///
    /// **Never survives a connection loss** — see
    /// [`BgState::discard_gated_ephemera`] for why that differs from the
    /// durable queue above.
    gated_ephemeral_pending: VecDeque<(EphemeralScope, Box<Event>)>,
    /// Observer frames written to the socket but not yet acknowledged. The
    /// relay's rate-limit NOTICE does not carry an event ID, so all unresolved
    /// observer writes are moved back ahead of the parked FIFO when one arrives.
    observer_in_flight: VecDeque<Box<Event>>,
    /// Frames evicted from the bounded pending/in-flight observer buffers since
    /// summary log. Makes overflow loss visible instead of silent.
    gated_observer_dropped: u64,
    /// Scopes evicted from `gated_ephemeral_pending` by the scope cap since the
    /// last summary log. Superseding is not counted here — replacing a scope's
    /// frame with a newer one about the same scope is the design, not a loss.
    gated_ephemeral_dropped: u64,
    /// Channels whose REQ failed during `resubscribe_after_reconnect`.
    ///
    /// A single failed channel REQ is parked here instead of aborting the whole
    /// reconnect. Drained by the main loop. Flushed on each reconnect attempt.
    resubscribe_retry: HashSet<Uuid>,
    /// Oldest dropped project-event timestamp, subscription-scoped.
    ///
    /// Deliberately not per-root: the watched-root REQ is a single
    /// subscription covering many roots, so the replay floor belongs to the
    /// subscription. Root-specific historical reconstruction is a separate
    /// mechanism with its own floor.
    project_dropped_since: Option<u64>,
    /// Two-generation dedup set for the **project** surface, separate from
    /// [`BgState::seen_ids`].
    ///
    /// One event can legitimately be deliverable on both surfaces: an event
    /// carrying an `h` tag and a root `e` tag matches both a channel REQ and
    /// the watched-root REQ. Sharing one set made whichever surface arrived
    /// first spend the id, and the other then saw a duplicate and delivered
    /// nothing.
    ///
    /// That was a suppression primitive, not merely a lost copy. Project
    /// classification is by subscription id, so anything that can name a
    /// project sub-id — the relay itself, first of all — could push a
    /// *genuine, correctly signed* channel event under `wr:` and burn its
    /// channel slot before the channel REQ delivered it. Verifying before
    /// deduping stops a **forgery** from spending a real id; it does nothing
    /// about a real event replayed on the wrong surface. Splitting the sets
    /// does, because the two surfaces no longer share the resource being
    /// spent.
    ///
    /// Deliberately one set for all project subscriptions rather than one per
    /// subscription. A watched-root REQ replacement overlaps its predecessor
    /// on purpose, so the same event arrives under two generations' sub-ids,
    /// and a per-subscription set would call the second copy new.
    project_seen_ids: TwoGenDedup,
    /// Project REQs this agent has opened and not closed.
    ///
    /// The admission gate for every inbound project frame. An id that is not
    /// in here has no class, and an unclassified frame is not verified,
    /// deduplicated or delivered.
    /// The single owner of project request state: durable intent, live
    /// registrations, and this connection's relay refusals.
    ///
    /// One owner rather than three fields updated in sequence — every gap
    /// between those updates was a way for them to disagree.
    project_requests: crate::project::ProjectRequests,
    /// The root reconstructions in progress, and the destination of every
    /// admitted catch-up frame and boundary.
    ///
    /// **Beside the registry, not across a channel from it.** A page is bound
    /// by `open_history_page` the moment its REQ reaches the socket, so whatever
    /// holds pages has to be able to call the registry directly; an owner in
    /// the run loop could only issue a collector, send it here, and wait for
    /// the bound page to come back — a response channel, for a decision that
    /// has already been made by the time it returns.
    ///
    /// Routing here also means an admitted frame never crosses the event
    /// channel. It cannot be dropped under backpressure, so a page cannot be
    /// left one row short of what the relay sent — and a short page is how a
    /// reconstruction concludes it has reached the end of history.
    ///
    /// Empty in production: nothing enrols a root yet.
    reconstructions: crate::project::ProjectReconstructions,
    /// The walk back through this agent's own roots, if one has begun.
    ///
    /// Lives beside the registry for the same reason the per-root
    /// reconstructions do: a page is bound the moment its REQ reaches the
    /// socket, so an owner anywhere else could only issue a collector and wait
    /// for the bound page to be handed back.
    enrolment_history: Option<crate::project::EnrolmentReconstruction>,
    /// **The visible fail-closed state.** Set when the walk could not prove it
    /// reached the end of history, and never cleared by anything short of a
    /// fresh walk.
    ///
    /// Held rather than merely logged because "we do not know which
    /// conversations we are responsible for" is a state the agent is *in*, not
    /// an event that happened to it — and the plan requires it be visible
    /// rather than inferred from a silence.
    enrolment_history_degraded: Option<String>,
    /// Ordered batches of reconstructed rows this task has not yet proven it
    /// handed to the run loop.
    ///
    /// Non-empty is the state "some reconstruction is not finished yet", and it
    /// is the reason a completion line has one producer rather than being a
    /// summary the caller assembles. See [`PendingReplay`].
    replay_deliveries: VecDeque<PendingReplay>,
    /// Roots whose history could not be proven complete, and why.
    ///
    /// **The per-root fail-closed state**, separate from
    /// [`BgState::enrolment_history_degraded`] because the two claims are
    /// different: that one says "I may not know every conversation I am
    /// responsible for", this one says "I know about this conversation and
    /// cannot prove what state it is in". An agent can be healthy on one and
    /// degraded on the other.
    root_catch_up_degraded: BTreeMap<String, String>,
    /// Roots whose history has been rebuilt and handed on, and how many rows
    /// each one accounted for.
    ///
    /// **Kept after the reconstruction retires**, for two reasons. A root is
    /// re-restored on every reconnect the enrolment walk survives, and starting
    /// its whole history again each time would re-page the same events for no
    /// new fact; and "this root's history is rebuilt" is the state a restart is
    /// trying to reach, so an agent that holds it should be able to say so.
    ///
    /// The rows themselves are **not** kept. They have been folded into the
    /// enrolment sets by the run loop, and a copy here would be a second model
    /// of the same lifecycle — the shape that produced the two-producers defect
    /// this phase already had to remove once.
    root_catch_up_done: BTreeMap<String, usize>,
    /// Current position in the exponential backoff ladder.
    ///
    /// Persisted across calls to `wait_for_reconnect` so a flapping link stays at
    /// the elevated rung it earned. Reset to 0 by the stability block once the
    /// connection has been up for `STABLE_CONNECTION_SECS`.
    backoff_step: usize,
}

impl BgState {
    fn new() -> Self {
        Self {
            active_subscriptions: HashMap::new(),
            last_seen: HashMap::new(),
            seen_ids: TwoGenDedup::new(SEEN_ID_LIMIT),
            active_filters: HashMap::new(),
            membership_dropped_since: None,
            membership_last_seen: None,
            membership_sub_active: false,
            observer_control_sub_active: false,
            peer_call_sub_active: false,
            channel_dropped_since: HashMap::new(),
            proactive_resubscribe_needed: false,
            startup_watermark: None,
            subscribe_since: HashMap::new(),
            rate_limit_gate: None,
            rate_limited_pending: HashMap::new(),
            membership_resub_needed: false,
            observer_resub_needed: false,
            peer_call_resub_needed: false,
            gated_observer_pending: VecDeque::new(),
            gated_ephemeral_pending: VecDeque::new(),
            observer_in_flight: VecDeque::new(),
            gated_observer_dropped: 0,
            gated_ephemeral_dropped: 0,
            resubscribe_retry: HashSet::new(),
            project_dropped_since: None,
            project_seen_ids: TwoGenDedup::new(SEEN_ID_LIMIT),
            project_requests: crate::project::ProjectRequests::new(),
            reconstructions: crate::project::ProjectReconstructions::new(),
            enrolment_history: None,
            enrolment_history_degraded: None,
            replay_deliveries: VecDeque::new(),
            root_catch_up_degraded: BTreeMap::new(),
            root_catch_up_done: BTreeMap::new(),
            backoff_step: 0,
        }
    }

    /// Record a received event for dedup and `since` tracking.
    /// Returns `true` if the event is new (not a duplicate).
    fn record_event(&mut self, channel_id: Uuid, event: &Event) -> bool {
        let id_hex = event.id.to_hex();

        // Two-generation dedup: no amnesia window on rotation.
        if !self.seen_ids.insert(id_hex) {
            return false;
        }

        // Update last_seen timestamp.
        let ts = event.created_at.as_secs();
        self.last_seen
            .entry(channel_id)
            .and_modify(|t| *t = (*t).max(ts))
            .or_insert(ts);

        true
    }

    /// Compute the `since` timestamp for a channel (re)subscribe.
    ///
    /// Picks the earliest of `last_seen` and `channel_dropped_since` so
    /// the replay window covers both successfully processed events and any
    /// that were dropped due to queue pressure. Falls back to the per-channel
    /// `subscribe_since` (set at first subscribe) or `startup_watermark`.
    fn channel_since(&self, channel_id: &Uuid) -> Option<u64> {
        let last_seen = self.last_seen.get(channel_id).copied();
        let dropped = self.channel_dropped_since.get(channel_id).copied();
        match (last_seen, dropped) {
            (Some(l), Some(d)) => Some(l.min(d)),
            (Some(l), None) => Some(l),
            (None, Some(d)) => Some(d),
            (None, None) => self
                .subscribe_since
                .get(channel_id)
                .copied()
                .or(self.startup_watermark),
        }
    }

    /// Clear all per-channel state for a channel that is being unsubscribed.
    /// Prevents stale replay on re-subscribe and avoids unbounded state growth
    /// for channels that are removed and never re-added.
    fn clear_channel_state(&mut self, channel_id: &Uuid) {
        self.last_seen.remove(channel_id);
        self.subscribe_since.remove(channel_id);
        self.channel_dropped_since.remove(channel_id);
        self.active_filters.remove(channel_id);
        self.rate_limited_pending.remove(channel_id);
        self.resubscribe_retry.remove(channel_id);
    }

    /// Arm or extend the rate-limit gate.
    ///
    /// `retry_secs` is the relay's `retry in {N}s` hint; hints below 2s (including
    /// the no-hint case of 0) floor to 5s. The floor prevents a burst of
    /// low-quality hints from dropping the gate so short that re-queued REQs
    /// immediately re-trigger rate limiting. Note the deliberate asymmetry with
    /// the desktop TypeScript client, which uses a 10s no-hint default — both
    /// values are conservative enough; the relay hint wins when present.
    ///
    /// The gate takes the **maximum** of any existing deadline and the newly
    /// computed one so overlapping CLOSED/NOTICE messages can't shorten a gate
    /// that is already set further out.
    ///
    /// Returns the gate deadline that was set.
    fn set_rate_limit_gate(&mut self, retry_secs: u64) -> tokio::time::Instant {
        let secs = if retry_secs < 2 { 5 } else { retry_secs };
        let base = Duration::from_secs(secs);
        let deadline = tokio::time::Instant::now() + jittered_duration(base);
        let gate = match self.rate_limit_gate {
            Some(existing) if existing > deadline => existing,
            _ => deadline,
        };
        self.rate_limit_gate = Some(gate);
        gate
    }

    /// Check whether the rate-limit gate is currently active.
    ///
    /// Returns `Some(deadline)` when gated, `None` when the gate has expired or
    /// was never set. Lazily clears `rate_limit_gate` to `None` on expiry so
    /// subsequent calls are cheap (no `Instant::now()` except when `Some`).
    fn check_rate_gate(&mut self) -> Option<tokio::time::Instant> {
        if let Some(deadline) = self.rate_limit_gate {
            if tokio::time::Instant::now() < deadline {
                return Some(deadline);
            }
            self.rate_limit_gate = None;
        }
        None
    }

    /// Park a durable frame while the rate-limit gate is armed.
    ///
    /// Bounded drop-oldest queue: overflow evicts the oldest frame and counts
    /// it in `gated_observer_dropped` so the loss is visible, never silent.
    fn park_gated_observer_frame(&mut self, event: Box<Event>) {
        if self.gated_observer_pending.len() >= GATED_OBSERVER_QUEUE_CAP {
            self.gated_observer_pending.pop_front();
            self.gated_observer_dropped += 1;
            warn!(
                dropped_total = self.gated_observer_dropped,
                "gated observer queue full — dropped oldest frame"
            );
        }
        self.gated_observer_pending.push_back(event);
    }

    /// Park the latest frame for one scope while the rate-limit gate is armed.
    ///
    /// **Latest wins.** A newer frame about the same scope replaces the parked
    /// one instead of queueing behind it: the two say the same kind of thing
    /// about the same subject, so delivering both after the gate clears would
    /// put a stale claim on the wire ahead of the true one. That replacement is
    /// not a loss and is not counted — it is what keeps the parked frame at
    /// most one publisher cadence old, which is the only reason parking
    /// ephemera is sound at all.
    ///
    /// Refreshing a scope moves it to the back, so the drop-oldest bound sheds
    /// the scope whose publisher has gone quietest — the one whose frame is
    /// closest to being a lie — rather than whichever happened to park first.
    fn park_gated_ephemeral_frame(&mut self, scope: EphemeralScope, event: Box<Event>) {
        if let Some(index) = self
            .gated_ephemeral_pending
            .iter()
            .position(|(parked, _)| *parked == scope)
        {
            self.gated_ephemeral_pending.remove(index);
        }
        if self.gated_ephemeral_pending.len() >= GATED_EPHEMERAL_SCOPE_CAP {
            if let Some((evicted, _)) = self.gated_ephemeral_pending.pop_front() {
                self.gated_ephemeral_dropped += 1;
                warn!(
                    kind = evicted.kind,
                    scope = %evicted.id,
                    dropped_total = self.gated_ephemeral_dropped,
                    "gated ephemeral map full — dropped least-recently-refreshed scope"
                );
            }
        }
        self.gated_ephemeral_pending.push_back((scope, event));
    }

    /// Whether this scope already has a frame waiting to be drained.
    ///
    /// The reason a publish can be parked even with the gate clear: a live send
    /// would overtake the frame parked behind it, and for a superseding kind
    /// that is worse than a delay. An `idle` overtaking a parked `working`
    /// leaves the root announcing work that has finished until the consumer's
    /// staleness window expires.
    fn ephemeral_scope_parked(&self, scope: &EphemeralScope) -> bool {
        self.gated_ephemeral_pending
            .iter()
            .any(|(parked, _)| parked == scope)
    }

    /// Drop every parked ephemeral frame because the socket they were parked on
    /// is gone.
    ///
    /// **Deliberately unlike the durable queue, which survives reconnect.** The
    /// freshness guarantee for a parked ephemeral frame is "its publisher will
    /// have re-announced within one cadence", and that holds only while the
    /// park is live: reconnect has no bounded duration (`wait_for_reconnect`
    /// retries DNS failures forever), and nothing re-announces during it. A
    /// frame carried across would say "this agent is typing" or "this root is
    /// working" as of a socket that died an unknown number of minutes ago.
    ///
    /// The cost of dropping is bounded and self-healing, and it is paid at an
    /// edge where the consumer has already gone blank: no frames flowed during
    /// the outage, so every client TTL (8 s typing, 45 s project activity) has
    /// long expired, and the publishers re-announce within one cadence (3 s,
    /// 15 s, 60 s) of the socket coming back. Logged with a count rather than
    /// warned because it happens on every reconnect and is the intended path.
    fn discard_gated_ephemera(&mut self, reason: &str) {
        if self.gated_ephemeral_pending.is_empty() {
            return;
        }
        debug!(
            scopes = self.gated_ephemeral_pending.len(),
            reason, "discarding parked ephemeral frames — they cannot outlive their socket"
        );
        self.gated_ephemeral_pending.clear();
    }

    /// Restore unresolved observer writes ahead of frames parked after the
    /// gate armed. NOTICE has no event ID, so conservatively retry every frame
    /// without an OK; duplicate IDs are harmless at the relay.
    fn requeue_observer_in_flight(&mut self) {
        while let Some(event) = self.observer_in_flight.pop_back() {
            self.gated_observer_pending.push_front(event);
        }
        while self.gated_observer_pending.len() > GATED_OBSERVER_QUEUE_CAP {
            self.gated_observer_pending.pop_front();
            self.gated_observer_dropped += 1;
        }
    }

    /// The socket that owned every project registration is gone.
    ///
    /// One operation, not two calls a caller has to remember to pair, because
    /// they are one fact: a registration and the page it opened belong to the
    /// same dead connection. Retiring the registry alone leaves a page nothing
    /// can ever complete — no boundary can be minted for it — while
    /// `pages_wanted` skips a stream that holds one, so that root stops asking
    /// for history in silence. Releasing the pages alone leaves ids the
    /// replacement connection would answer without having asked.
    ///
    /// Durable intent is untouched; it is what re-opens these requests.
    fn retire_project_connection(&mut self) {
        self.project_requests.clear_connection();
        self.reconstructions.disconnected();
        if let Some(history) = self.enrolment_history.as_mut() {
            history.disconnected();
        }
    }

    fn track_observer_in_flight(&mut self, event: Box<Event>) {
        if self.observer_in_flight.len() >= GATED_OBSERVER_QUEUE_CAP {
            self.observer_in_flight.pop_front();
            self.gated_observer_dropped += 1;
            warn!(
                dropped_total = self.gated_observer_dropped,
                "observer acknowledgment window full — dropped oldest frame"
            );
        }
        self.observer_in_flight.push_back(event);
    }

    fn acknowledge_observer_frame(&mut self, event_id: &str) {
        if let Some(index) = self
            .observer_in_flight
            .iter()
            .position(|event| event.id.to_hex() == event_id)
        {
            self.observer_in_flight.remove(index);
        }
    }
}

/// The subject a superseding ephemeral frame is *about*.
///
/// Two frames share a scope exactly when the later one makes the earlier one
/// untrue, which is the whole condition for replacing rather than queueing.
/// The kind is part of the key so a scope can never be shared across kinds by
/// coincidence — and so an entry names, in a log line, which wire it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
struct EphemeralScope {
    kind: u32,
    /// The scope value: the channel id for a typing indicator, the root event
    /// id for project activity, empty for a kind scoped to the agent itself.
    id: String,
}

/// What the WS publish path does with one outbound EVENT while the relay's
/// rate-limit gate is armed.
///
/// Both variants park; the difference is what a *second* frame does to the
/// first. Neither drops.
#[derive(Debug)]
enum GatedPublish {
    /// Nobody will say this again: park it in FIFO order and deliver all of it.
    ///
    /// Ordering is load-bearing (NIP-AO turn history is read as a sequence), so
    /// frames queue behind each other and a frame published after the gate
    /// clears still queues behind an undrained backlog rather than overtaking
    /// it.
    Durable,
    /// The publisher will say it again on a cadence: park only the latest frame
    /// per scope.
    ///
    /// Sound *because* of that cadence and for no other reason — typing
    /// re-announces every 3 s, project activity every 15 s, presence every
    /// 60 s — so the parked frame is at most one interval old when the gate
    /// clears, and every frame it superseded was a weaker statement of the same
    /// fact. Take the continuous re-announcement away and this becomes a stale
    /// claim delivered late; see [`BgState::discard_gated_ephemera`], which is
    /// the one place that assumption stops holding.
    Superseding(EphemeralScope),
}

/// Classify one outbound event for the gated publish path.
///
/// **This is the policy, and every kind that can reach
/// [`RelayCommand::PublishEvent`] must have a row here.** It replaces an
/// INVARIANT comment that asserted this path carried only observer telemetry
/// and typing indicators, so everything that was not telemetry could be dropped
/// while gated. That assertion was true when it was written and false within
/// two features, silently, because nothing enforced it:
///
/// - **kind 20003** (NIP-PA project activity) became a tenant and inherited the
///   typing indicator's drop by accident. A relay NOTICE throttle lasts 45–53 s
///   and the consumer's staleness window is 45 s, so every project indicator
///   blanked for the whole window and flapped back after it — the "status goes
///   in and out" bug this table exists to prevent recurring.
/// - **kind 9** (the setup-mode nudge) became a tenant too, and that one is a
///   real message to a real person: the exact "durable events through this
///   path" case the old comment warned a future caller about, discarded with no
///   trace for the length of a gate window.
///
/// | kind  | what it is                        | while gated                            |
/// |-------|-----------------------------------|----------------------------------------|
/// | 24200 | NIP-AO encrypted owner telemetry  | `Durable` — FIFO, order preserved      |
/// | 9     | setup-mode nudge (a real message) | `Durable` — FIFO                       |
/// | 20003 | NIP-PA project activity           | `Superseding` by root (`e` root tag)   |
/// | 20002 | typing indicator                  | `Superseding` by channel (`h` tag)     |
/// | 20001 | presence heartbeat                | `Superseding`, one agent-wide scope    |
/// | *any other* | not yet classified          | `Durable` + `error!` — add a row       |
///
/// The fallback is `Durable` rather than a drop because the two failure modes
/// are not symmetric: parking an ephemeral kind by mistake costs one late frame
/// that its own publisher will supersede, while dropping a durable one loses
/// data nothing will ever resend. A new kind on this path therefore arrives
/// safe and loud instead of quietly wrong, which is the property the comment
/// this replaced did not have.
fn gated_publish_policy(event: &Event) -> GatedPublish {
    let kind = event.kind.as_u16() as u32;
    match kind {
        KIND_AGENT_OBSERVER_FRAME | KIND_STREAM_MESSAGE => GatedPublish::Durable,
        // Channel-scoped: one "someone is typing" per channel.
        KIND_TYPING_INDICATOR => GatedPublish::Superseding(EphemeralScope {
            kind,
            id: tag_value(event, "h").unwrap_or_default(),
        }),
        // Root-scoped, and deliberately not channel-scoped: NIP-PA carries no
        // `h` at all — an issue is not a channel — so the root `e` tag
        // (`["e", <root>, "", "root"]`, per `build_project_activity`) is the
        // only identity the frame has, and it is the same key the consumer
        // subscribes by.
        KIND_PROJECT_ACTIVITY => GatedPublish::Superseding(EphemeralScope {
            kind,
            id: root_e_tag_value(event).unwrap_or_default(),
        }),
        // Agent-wide: presence is a property of the pubkey signing it, so every
        // presence frame shares one scope and the newest is the only true one.
        KIND_PRESENCE_UPDATE => GatedPublish::Superseding(EphemeralScope {
            kind,
            id: String::new(),
        }),
        _ => {
            error!(
                kind,
                "unclassified kind on the WS publish path — parking it as durable; \
                 add a row to gated_publish_policy so its gated behaviour is chosen, not inherited"
            );
            GatedPublish::Durable
        }
    }
}

/// The first value of the first tag with this name, if the event carries one.
///
/// A superseding frame with no scope tag is not dropped and does not get a
/// scope of its own: it collapses into its kind's empty-string scope, where the
/// newest such frame supersedes the last. That keeps a malformed publisher
/// bounded to one map entry instead of one per frame, and every builder in this
/// workspace emits the tag, so the case is a defect elsewhere rather than a
/// shape to design around.
fn tag_value(event: &Event, name: &str) -> Option<String> {
    event.tags.iter().find_map(|tag| {
        let parts = tag.as_slice();
        (parts.len() >= 2 && parts[0] == name).then(|| parts[1].clone())
    })
}

/// The NIP-10 root-marked `e` tag value, falling back to the first `e` tag.
///
/// The marker is checked first because a frame may carry several `e` tags and
/// only the root one identifies the conversation; the fallback covers a
/// single-`e` frame written without markers.
fn root_e_tag_value(event: &Event) -> Option<String> {
    let marked = event.tags.iter().find_map(|tag| {
        let parts = tag.as_slice();
        (parts.len() >= 4 && parts[0] == "e" && parts[3] == "root").then(|| parts[1].clone())
    });
    marked.or_else(|| tag_value(event, "e"))
}

/// Record a command's intent in state while disconnected (no WebSocket).
///
/// Subscribe/Unsubscribe/SubscribeMembership record intent so reconnect
/// restores the right subscriptions. SetStartupWatermark floors the replay
/// window. Observer telemetry publishes are parked for post-reconnect drain;
/// other PublishEvent and Reconnect are no-ops while disconnected.
///
/// Callers MUST handle `Shutdown` before calling — reaching the Shutdown
/// arm here is a logic error.
fn apply_command_to_state(state: &mut BgState, cmd: RelayCommand) {
    match cmd {
        RelayCommand::Subscribe {
            channel_id,
            filter,
            replay_since,
        } => {
            state
                .active_subscriptions
                .insert(channel_id, channel_sub_id(channel_id));
            state.active_filters.insert(channel_id, filter);
            state.subscribe_since.entry(channel_id).or_insert_with(|| {
                // Use an explicit replay floor when available (dynamic
                // membership), otherwise startup_watermark closes the startup
                // blind spot between watermark capture and first REQ.
                replay_since
                    .or(state.startup_watermark)
                    .unwrap_or_else(unix_now_secs)
            });
        }
        RelayCommand::Unsubscribe { channel_id } => {
            state.active_subscriptions.remove(&channel_id);
            state.clear_channel_state(&channel_id);
        }
        RelayCommand::SubscribeProjectDiscovery { filters } => {
            // Offline: record the intent only — nothing becomes answerable
            // until a REQ is actually written for it. Fail-closed all the same,
            // because a conflicting command accepted while disconnected would
            // be opened verbatim by the next connection's replay.
            //
            // The command carries filters and nothing else. The id, the class
            // and the refusal of an unbounded filter all belong to the registry
            // — a command that carried them was a command that could name a
            // watched generation, and a caller that decided boundedness was a
            // second place that had to agree with the owner.
            match state.project_requests.record_discovery_intent(filters) {
                crate::project::IntentAdmission::Conflict { held } => {
                    warn!(
                        ?held,
                        "refusing conflicting project discovery intent while disconnected — \
                         keeping the original"
                    );
                }
                crate::project::IntentAdmission::UnboundedFilters => {
                    // Recording this as intent would replay a filterless REQ
                    // onto the next connection, which asks the relay for
                    // everything.
                    warn!("refusing a project discovery subscription with no filters");
                }
                crate::project::IntentAdmission::InvariantViolation(violation) => {
                    // Durable intent as a whole does not resolve, so there is
                    // no record to add a canonical entry to. Reported at the
                    // same level as the replacements' violation: it is a local
                    // inconsistency, not a transport failure, and the next
                    // connection will replay nothing until it is gone.
                    warn!(
                        %violation,
                        "refusing project discovery intent — the durable record does not resolve"
                    );
                }
                crate::project::IntentAdmission::Recorded
                | crate::project::IntentAdmission::AlreadyIntended => {}
            }
        }
        RelayCommand::BeginEnrolmentHistory { coordinates, agent } => {
            // Offline: remember the question. No page can be written with no
            // socket, and the walk must not be marked degraded for that — the
            // reconnect path drives it as soon as there is one.
            begin_enrolment_history(state, coordinates, agent);
        }
        RelayCommand::BeginRootCatchUp { root } => {
            // Same offline rule: the reconstruction is created and wants its
            // first page, which the reconnect drive opens.
            begin_root_catch_up(state, *root);
        }
        RelayCommand::ReplaceProject {
            replacement,
            filters,
        } => {
            // Offline: move the durable intent only. No REQ can be written with
            // no socket, but the intent must still move or the next connection
            // replays the predecessor and the replacement is silently lost.
            //
            // The generation is allocated here as it is when connected, by the
            // same owner. Durable intent is what reconnect installs, so there
            // is no later moment at which this becomes true.
            let outcome = match replacement {
                crate::project::ProjectReplacement::Enrolment => {
                    state.project_requests.replace_enrolment_intent(filters)
                }
                crate::project::ProjectReplacement::Watched => {
                    state.project_requests.replace_watched_intent(filters)
                }
            };
            match outcome {
                crate::project::ReplaceOutcome::InvalidFilters => {
                    warn!(?replacement, "refusing an unbounded project replacement");
                }
                crate::project::ReplaceOutcome::WatchedGenerationExhausted => {
                    error!(
                        ?replacement,
                        "watched generations exhausted while disconnected — no further \
                         replacement can be recorded"
                    );
                }
                crate::project::ReplaceOutcome::RequestIncarnationExhausted => {
                    error!(
                        ?replacement,
                        "request incarnations exhausted while disconnected — no further \
                         replacement can be recorded"
                    );
                }
                crate::project::ReplaceOutcome::InvariantViolation(violation) => {
                    error!(
                        ?replacement,
                        violation,
                        "project subscription invariant violated while disconnected — \
                         no intent recorded"
                    );
                }
                crate::project::ReplaceOutcome::Replaced { .. }
                | crate::project::ReplaceOutcome::Unchanged
                | crate::project::ReplaceOutcome::WriteFailed(_) => {}
            }
        }
        RelayCommand::SubscribeMembership => {
            state.membership_sub_active = true;
        }
        RelayCommand::SubscribeObserverControls => {
            state.observer_control_sub_active = true;
        }
        RelayCommand::SubscribePeerCalls => {
            state.peer_call_sub_active = true;
        }
        RelayCommand::SetStartupWatermark { ts } => {
            state.startup_watermark = Some(ts);
            if state.membership_last_seen.is_none() {
                state.membership_last_seen = Some(ts);
            }
        }
        // Durable publishes are parked (bounded, visible overflow) so the
        // post-reconnect drain delivers them. Superseding ephemera are dropped:
        // a reconnect is unbounded, so nothing parked here could still be true
        // when it ends, and each one's publisher re-announces within a cadence
        // of the socket returning. See [`BgState::discard_gated_ephemera`],
        // which makes the same call for frames parked before the socket died.
        RelayCommand::PublishEvent { event } => match gated_publish_policy(&event) {
            GatedPublish::Durable => state.park_gated_observer_frame(event),
            GatedPublish::Superseding(_) => {}
        },
        // Already reconnecting — redundant.
        RelayCommand::Reconnect => {}
        // Callers MUST handle Shutdown before calling this function.
        RelayCommand::Shutdown => {
            debug_assert!(
                false,
                "Shutdown must be handled by caller, not apply_command_to_state"
            );
        }
    }
}

/// Retain command intent after a live send failure.
///
/// Subscription state must survive reconnect. Durable publishes are parked for
/// the post-reconnect drain; superseding ephemera are deliberately discarded,
/// because a send failure means the socket is probably already dead and
/// replaying a typing indicator across a reconnect states something about a
/// moment that has passed. `Shutdown` and `Reconnect` are handled by the caller.
fn retain_failed_command_intent(state: &mut BgState, cmd: RelayCommand) {
    match cmd {
        RelayCommand::PublishEvent { event } => match gated_publish_policy(&event) {
            GatedPublish::Durable => state.park_gated_observer_frame(event),
            GatedPublish::Superseding(_) => {}
        },
        cmd => apply_command_to_state(state, cmd),
    }
}

/// Preserve stateful commands already consumed during replay when that replay
/// loses its live socket before the deferred queue can be executed.
///
/// Commands are applied in arrival order. Ephemeral publishes are discarded by
/// [`retain_failed_command_intent`], and pacing never queues `Shutdown`.
fn retain_deferred_command_intent(
    state: &mut BgState,
    deferred_commands: &mut VecDeque<RelayCommand>,
) {
    while let Some(cmd) = deferred_commands.pop_front() {
        match cmd {
            RelayCommand::Shutdown | RelayCommand::Reconnect => {}
            cmd => retain_failed_command_intent(state, cmd),
        }
    }
}

/// Execute a command on a live WebSocket connection.
///
/// Handles the five data commands: Subscribe, Unsubscribe,
/// SubscribeMembership, PublishEvent, SetStartupWatermark. Callers handle
/// Shutdown and Reconnect for control flow before dispatching here.
///
/// Returns `true` if the command succeeded (or was a no-op). Returns `false`
/// if a WebSocket send failed — the caller should treat this as a dead socket
/// and trigger reconnect. On failure, subscription intent is preserved in
/// state via [`apply_command_to_state`] so reconnect will restore it.
async fn execute_connected_command(
    ws: &mut WsStream,
    state: &mut BgState,
    agent_pubkey_hex: &str,
    cmd: RelayCommand,
) -> bool {
    match cmd {
        RelayCommand::Subscribe {
            channel_id,
            filter,
            replay_since,
        } => {
            // Rate-gated: defer this REQ to prevent flooding a saturated relay.
            // The gate holds until the relay's retry hint expires.
            if let Some(retry_after) = state.check_rate_gate() {
                debug!(
                    "rate-gated: deferring REQ for channel {channel_id} to rate_limited_pending"
                );
                apply_command_to_state(
                    state,
                    RelayCommand::Subscribe {
                        channel_id,
                        filter,
                        replay_since,
                    },
                );
                state.rate_limited_pending.insert(channel_id, retry_after);
                return true; // connection is fine — just rate-limited
            }

            // Seed subscribe_since BEFORE computing since — on first
            // subscribe, this provides the fallback timestamp that
            // closes the startup/dynamic-membership blind spot.
            state.subscribe_since.entry(channel_id).or_insert_with(|| {
                replay_since
                    .or(state.startup_watermark)
                    .unwrap_or_else(unix_now_secs)
            });
            let since = state
                .last_seen
                .get(&channel_id)
                .copied()
                .or_else(|| state.subscribe_since.get(&channel_id).copied());
            let sent =
                send_subscribe(ws, state, channel_id, agent_pubkey_hex, since, &filter).await;
            if sent {
                state
                    .active_subscriptions
                    .insert(channel_id, channel_sub_id(channel_id));
                state.active_filters.insert(channel_id, filter);
                // Evict stale drain entries so the drain loop can't send a
                // duplicate REQ for this now-live subscription.
                state.rate_limited_pending.remove(&channel_id);
                state.resubscribe_retry.remove(&channel_id);
                true
            } else {
                // Send failed — record intent so reconnect restores it.
                warn!("subscribe REQ failed for channel {channel_id} — recording intent for reconnect");
                apply_command_to_state(
                    state,
                    RelayCommand::Subscribe {
                        channel_id,
                        filter,
                        replay_since,
                    },
                );
                false
            }
        }
        RelayCommand::Unsubscribe { channel_id } => {
            if let Some(sub_id) = state.active_subscriptions.remove(&channel_id) {
                let msg = json!(["CLOSE", sub_id]);
                if let Ok(text) = serde_json::to_string(&msg) {
                    // Best-effort CLOSE — don't fail the command if send fails,
                    // because the intent (unsubscribe) is already applied to state.
                    let _ =
                        ws_send_timeout(ws, Message::Text(text.into()), WS_SEND_TIMEOUT_SECS).await;
                }
                debug!("unsubscribed from channel {channel_id}");
            }
            state.clear_channel_state(&channel_id);
            true
        }
        RelayCommand::SubscribeProjectDiscovery { filters } => {
            // Intent and registration are decided together, in one operation
            // that either fully succeeds or records nothing. Admitting intent
            // first and consulting the registry second left a refused identity
            // sitting in intent, and the next reconnect installed it.
            match send_project_discovery(ws, state, filters).await {
                ProjectSendOutcome::Sent | ProjectSendOutcome::AlreadyOpen => true,
                // The socket is fine; our own bookkeeping disagreed. Tearing
                // the connection down here is what let a refusal be replayed
                // into effect on the next one.
                ProjectSendOutcome::MetadataConflict => true,
                // Our own filters were unusable. Nothing written, nothing
                // recorded, and the connection is unaffected.
                ProjectSendOutcome::UnboundedFilters => true,
                // Terminal, but not a transport failure — the socket stays.
                ProjectSendOutcome::Exhausted => true,
                // A local inconsistency in durable intent. The socket is fine
                // and nothing was written; reconnecting would replay the same
                // record and refuse again.
                ProjectSendOutcome::InvariantViolation => true,
                ProjectSendOutcome::WriteFailed => false,
            }
        }
        RelayCommand::BeginRootCatchUp { root } => {
            begin_root_catch_up(state, *root);
            drive_root_reconstructions(ws, state).await;
            // Same reasoning as the walk below: a root whose history cannot be
            // paged degrades on its own and takes nothing else with it.
            true
        }
        RelayCommand::BeginEnrolmentHistory { coordinates, agent } => {
            begin_enrolment_history(state, coordinates, agent);
            drive_enrolment_history(ws, state).await;
            // The socket is untouched by any outcome here. A walk that cannot
            // be opened degrades visibly and every other subscription keeps
            // working; tearing the connection down would take them with it.
            true
        }
        RelayCommand::ReplaceProject {
            replacement,
            filters,
        } => {
            // The registry derives the id, the generation and the predecessor.
            // Nothing here chooses any of them, so nothing here can choose a
            // stale one.
            let outcome = match replacement {
                crate::project::ProjectReplacement::Enrolment => {
                    state.project_requests.replace_enrolment(ws, filters).await
                }
                crate::project::ProjectReplacement::Watched => {
                    state.project_requests.replace_watched(ws, filters).await
                }
            };
            match outcome {
                crate::project::ReplaceOutcome::Replaced { retired } => {
                    if matches!(replacement, crate::project::ProjectReplacement::Enrolment) {
                        claim_enrolment_backlog(state);
                    }
                    debug!(?replacement, ?retired, "project subscription replaced");
                    true
                }
                crate::project::ReplaceOutcome::Unchanged => {
                    debug!(
                        ?replacement,
                        "project subscription already current — no REQ written"
                    );
                    true
                }
                // Nothing was written and nothing installed, so the predecessor
                // is still current and a later valid replacement will retire
                // it. The connection is fine; our own filters were not.
                crate::project::ReplaceOutcome::InvalidFilters => {
                    warn!(?replacement, "refusing an unbounded project replacement");
                    true
                }
                // Named separately from the incarnation ceiling below. One
                // message covered both, so an operator reading "generations
                // exhausted" would have gone looking at the wrong counter.
                crate::project::ReplaceOutcome::WatchedGenerationExhausted => {
                    error!(
                        ?replacement,
                        "watched generations exhausted — no further watched subscription \
                         can be replaced"
                    );
                    true
                }
                crate::project::ReplaceOutcome::RequestIncarnationExhausted => {
                    error!(
                        ?replacement,
                        "request incarnations exhausted — no further project subscription \
                         can be opened or replaced"
                    );
                    true
                }
                // Something installed watched intent outside this owner. The
                // registry refused to guess which one to retire, so nothing was
                // written and the connection is unharmed — but the state is not
                // reachable through any command, so it is a defect rather than
                // a condition.
                crate::project::ReplaceOutcome::InvariantViolation(violation) => {
                    error!(
                        ?replacement,
                        violation, "project subscription invariant violated — nothing replaced"
                    );
                    true
                }
                // The predecessor is intact, so the agent keeps answering on the
                // subscription it already had. Only a write failure may take the
                // connection down, and this is one.
                crate::project::ReplaceOutcome::WriteFailed(e) => {
                    warn!(?replacement, "project replacement write failed: {e}");
                    false
                }
            }
        }
        RelayCommand::SubscribeMembership => {
            state.membership_sub_active = true;
            if state.check_rate_gate().is_some() {
                debug!("rate-gated: deferring membership subscription");
                state.membership_resub_needed = true;
                return true;
            }
            let since = state.membership_last_seen.or(state.startup_watermark);
            let sent = send_membership_subscribe(ws, agent_pubkey_hex, since).await;
            if sent {
                state.membership_resub_needed = false;
                if state.membership_last_seen.is_none() {
                    state.membership_last_seen = since;
                }
                true
            } else {
                // Send failed — record intent so reconnect restores it.
                warn!("membership subscribe REQ failed — recording intent for reconnect");
                state.membership_resub_needed = true;
                false
            }
        }
        RelayCommand::SubscribeObserverControls => {
            state.observer_control_sub_active = true;
            if state.check_rate_gate().is_some() {
                debug!("rate-gated: deferring observer control subscription");
                state.observer_resub_needed = true;
                return true;
            }
            let sent = send_observer_control_subscribe(ws, agent_pubkey_hex).await;
            if sent {
                state.observer_resub_needed = false;
                true
            } else {
                warn!("observer control subscribe REQ failed — recording intent for reconnect");
                state.observer_resub_needed = true;
                false
            }
        }
        RelayCommand::SubscribePeerCalls => {
            state.peer_call_sub_active = true;
            if state.check_rate_gate().is_some() {
                debug!("rate-gated: deferring peer-call subscription");
                state.peer_call_resub_needed = true;
                return true;
            }
            let sent = send_peer_call_subscribe(ws, agent_pubkey_hex).await;
            if sent {
                state.peer_call_resub_needed = false;
                true
            } else {
                warn!("peer-call subscribe REQ failed — recording intent for reconnect");
                state.peer_call_resub_needed = true;
                false
            }
        }
        RelayCommand::PublishEvent { event } => {
            // One policy for everything on this path, parameterised by what the
            // kind means. [`gated_publish_policy`] carries the table and the
            // rationale; nothing here may special-case a kind.
            let policy = gated_publish_policy(&event);
            let durable = matches!(policy, GatedPublish::Durable);
            match policy {
                // Park while the gate is armed *and* while an earlier backlog is
                // still draining, so relative order is preserved: a frame that
                // overtook the queue would reorder turn history.
                GatedPublish::Durable => {
                    if state.check_rate_gate().is_some() || !state.gated_observer_pending.is_empty()
                    {
                        debug!(
                            kind = event.kind.as_u16(),
                            pending = state.gated_observer_pending.len(),
                            "rate-gated: parking durable frame for paced drain"
                        );
                        state.park_gated_observer_frame(event);
                        return true;
                    }
                }
                // Park the latest frame for this scope. The second condition is
                // the same order rule as above, narrowed to the only ordering
                // that exists for these kinds — within a scope. A live `idle`
                // overtaking a parked `working` for the same root would leave
                // the root announcing finished work for a whole staleness
                // window; superseding the parked frame is both the fix and the
                // wire semantics NIP-PA already specifies.
                GatedPublish::Superseding(scope) => {
                    if state.check_rate_gate().is_some() || state.ephemeral_scope_parked(&scope) {
                        debug!(
                            kind = scope.kind,
                            scope = %scope.id,
                            parked_scopes = state.gated_ephemeral_pending.len(),
                            "rate-gated: parking latest frame for this scope"
                        );
                        state.park_gated_ephemeral_frame(scope, event);
                        return true;
                    }
                }
            }
            // Best-effort: log a send failure but don't trigger reconnect — the
            // next ping or read will detect the dead socket. A failed durable
            // frame is parked so the post-reconnect drain redelivers it; a
            // failed ephemeral one is not, because its publisher re-announces.
            let is_observer = event.kind.as_u16() as u32 == KIND_AGENT_OBSERVER_FRAME;
            if send_publish_event_frame(ws, &event).await {
                if is_observer {
                    state.track_observer_in_flight(event);
                }
            } else if durable {
                state.park_gated_observer_frame(event);
            }
            true
        }
        RelayCommand::SetStartupWatermark { ts } => {
            state.startup_watermark = Some(ts);
            if state.membership_last_seen.is_none() {
                state.membership_last_seen = Some(ts);
            }
            debug!("startup watermark set to {ts}");
            true
        }
        // Control-flow commands — callers handle these before dispatching.
        RelayCommand::Shutdown | RelayCommand::Reconnect => {
            debug_assert!(
                false,
                "Shutdown/Reconnect must be handled by caller, not execute_connected_command"
            );
            true
        }
    }
}

/// The main background task loop.
///
/// Owns the WebSocket stream, responds to Pings, forwards events, and handles
/// reconnection.
#[allow(clippy::too_many_arguments)]
async fn run_background_task(
    mut ws: WsStream,
    initial_handshake_buffer: ingress::HandshakeBuffer,
    event_tx: mpsc::Sender<Option<BuzzEvent>>,
    observer_control_tx: mpsc::Sender<Event>,
    mut cmd_rx: mpsc::Receiver<RelayCommand>,
    keys: Keys,
    relay_url: String,
    agent_pubkey_hex: String,
    auth_tag: Option<nostr::Tag>,
) {
    let mut state = BgState::new();

    let handshake_ok = ingress::process_handshake_buffer(
        &mut ws,
        initial_handshake_buffer,
        &event_tx,
        &observer_control_tx,
        &mut state,
        &keys,
        &relay_url,
        &agent_pubkey_hex,
        auth_tag.as_ref(),
    )
    .await;
    if !handshake_ok {
        warn!("handshake buffer contained a drop signal — attempting autonomous reconnect");
        // Don't wait for a caller-driven Reconnect command — the caller was
        // never notified (no sentinel sent). Go straight to reconnect loop.
        let _ = event_tx.try_send(None);
        match try_autonomous_reconnect(
            &mut ws,
            &mut cmd_rx,
            &mut state,
            &keys,
            &relay_url,
            &agent_pubkey_hex,
            &event_tx,
            &observer_control_tx,
            auth_tag.as_ref(),
        )
        .await
        {
            ReconnectOutcome::Ok => {
                if matches!(
                    drain_post_reconnect(&mut ws, &mut cmd_rx, &mut state, &agent_pubkey_hex).await,
                    ReconnectOutcome::Shutdown
                ) {
                    return;
                }
            }
            ReconnectOutcome::Shutdown => return,
            ReconnectOutcome::Failed => {
                if matches!(
                    wait_for_reconnect(
                        &mut ws,
                        &mut cmd_rx,
                        &mut state,
                        &keys,
                        &relay_url,
                        &agent_pubkey_hex,
                        &event_tx,
                        &observer_control_tx,
                        true,
                        auth_tag.as_ref(),
                    )
                    .await,
                    ReconnectOutcome::Shutdown
                ) {
                    return;
                }
            }
        }
        // ping_sent, last_pong, connected_since are initialized below —
        // no reset needed here since they haven't been declared yet.
    }

    // Client-initiated ping to detect silent connection death.
    let mut ping_interval = tokio::time::interval(PING_INTERVAL);
    ping_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last_pong = Instant::now();
    let mut ping_sent = false;

    // Track connection stability for backoff reset.
    let mut connected_since = Instant::now();
    let mut stable_logged = false;

    // Pacing timer for select-integrated rate-limit drain.
    // `None` = no pending drain or budget window is open; `Some(t)` = next
    // allowed drain tick. The select! arm below fires when `t` elapses and
    // resets this to `None`, allowing the pre-select drain to run again.
    let mut drain_pacing_next: Option<tokio::time::Instant> = None;

    // Retry timer for reconstructed rows the run loop was too busy to take.
    // Armed from the state rather than from the delivery site: whichever
    // attempt leaves rows retained leaves a batch in `replay_deliveries`, and
    // this loop owns when the next attempt happens.
    let mut restore_retry_next: Option<tokio::time::Instant> = None;

    loop {
        restore_retry_next = match (!state.replay_deliveries.is_empty(), restore_retry_next) {
            (true, Some(t)) => Some(t),
            (true, None) => Some(tokio::time::Instant::now() + ENROLMENT_RESTORE_RETRY_INTERVAL),
            // Nothing retained: disarm, so a later retention starts a fresh
            // interval instead of firing against a deadline already past.
            (false, _) => None,
        };

        if state.proactive_resubscribe_needed {
            state.proactive_resubscribe_needed = false;
            info!("proactive resubscribe triggered by backpressure event loss");
            // Proactive resubscribe runs on the EXISTING socket — do NOT clear the
            // rate-limit gate or pending queues.
            match resubscribe_after_reconnect(
                &mut ws,
                &mut cmd_rx,
                &mut state,
                &agent_pubkey_hex,
                false, // existing socket — preserve gate state
            )
            .await
            {
                ResubscribeResult::Ok => {}
                ResubscribeResult::Shutdown => return,
                ResubscribeResult::RetryConnection => {
                    warn!("proactive resubscribe had failures — triggering reconnect");
                    let _ = event_tx.try_send(None);
                    match try_autonomous_reconnect(
                        &mut ws,
                        &mut cmd_rx,
                        &mut state,
                        &keys,
                        &relay_url,
                        &agent_pubkey_hex,
                        &event_tx,
                        &observer_control_tx,
                        auth_tag.as_ref(),
                    )
                    .await
                    {
                        ReconnectOutcome::Ok => {
                            if matches!(
                                drain_post_reconnect(
                                    &mut ws,
                                    &mut cmd_rx,
                                    &mut state,
                                    &agent_pubkey_hex
                                )
                                .await,
                                ReconnectOutcome::Shutdown
                            ) {
                                return;
                            }
                        }
                        ReconnectOutcome::Shutdown => return,
                        ReconnectOutcome::Failed => {
                            if matches!(
                                wait_for_reconnect(
                                    &mut ws,
                                    &mut cmd_rx,
                                    &mut state,
                                    &keys,
                                    &relay_url,
                                    &agent_pubkey_hex,
                                    &event_tx,
                                    &observer_control_tx,
                                    true,
                                    auth_tag.as_ref(),
                                )
                                .await,
                                ReconnectOutcome::Shutdown
                            ) {
                                return;
                            }
                        }
                    }
                    ping_sent = false;
                    last_pong = Instant::now();
                    connected_since = Instant::now();
                    stable_logged = false;
                }
            }
        }

        // Drain pending subs, one REQ per pacing tick within the relay's
        // admission window.
        let drain_window_open = drain_pacing_next.is_none_or(|t| tokio::time::Instant::now() >= t);
        if drain_window_open {
            let mut budget = DRAIN_BUDGET_PER_ITER;
            let mut any_sent = false;

            // Control subs use a flag rather than a per-channel pending entry, so
            // recovery fires even when rate_limited_pending is empty.
            if state.check_rate_gate().is_none() {
                if state.membership_resub_needed && budget > 0 {
                    let replay_since =
                        match (state.membership_dropped_since, state.membership_last_seen) {
                            (Some(d), Some(l)) => Some(d.min(l)),
                            (Some(d), None) => Some(d),
                            (None, Some(l)) => Some(l),
                            (None, None) => state.startup_watermark,
                        };
                    if send_membership_subscribe(&mut ws, &agent_pubkey_hex, replay_since).await {
                        state.membership_resub_needed = false;
                        state.membership_dropped_since = None;
                        budget = budget.saturating_sub(1);
                        any_sent = true;
                    } else {
                        warn!(
                            "membership control resub after rate-limit failed — will retry next drain"
                        );
                    }
                }
                if state.observer_resub_needed && budget > 0 {
                    if send_observer_control_subscribe(&mut ws, &agent_pubkey_hex).await {
                        state.observer_resub_needed = false;
                        budget = budget.saturating_sub(1);
                        any_sent = true;
                    } else {
                        warn!(
                            "observer control resub after rate-limit failed — will retry next drain"
                        );
                    }
                }
                if state.peer_call_resub_needed && budget > 0 {
                    if send_peer_call_subscribe(&mut ws, &agent_pubkey_hex).await {
                        state.peer_call_resub_needed = false;
                        budget = budget.saturating_sub(1);
                        any_sent = true;
                    } else {
                        warn!("peer-call resub after rate-limit failed — will retry next drain");
                    }
                }
            }

            if budget > 0 && !state.rate_limited_pending.is_empty() {
                let sent =
                    drain_rate_limited_pending(&mut ws, &mut state, &agent_pubkey_hex, budget)
                        .await;
                budget = budget.saturating_sub(sent);
                if sent > 0 {
                    any_sent = true;
                }
            }

            if budget > 0 && !state.resubscribe_retry.is_empty() {
                let sent =
                    drain_resubscribe_retry(&mut ws, &mut state, &agent_pubkey_hex, budget).await;
                budget = budget.saturating_sub(sent);
                if sent > 0 {
                    any_sent = true;
                }
            }

            // Durable before ephemeral, and both behind the REQ drains above.
            // The order is a priority, not a preference: the durable queue is
            // the only one holding frames nobody will send again, and a frame
            // superseded while it waits its turn costs nothing — the ephemeral
            // map keeps only the latest per scope, so a delayed drain drains
            // fresher frames, never more of them.
            if budget > 0 && !state.gated_observer_pending.is_empty() {
                let sent = drain_gated_observer_pending(&mut ws, &mut state, budget).await;
                budget = budget.saturating_sub(sent);
                if sent > 0 {
                    any_sent = true;
                }
            }

            if budget > 0 && !state.gated_ephemeral_pending.is_empty() {
                let sent = drain_gated_ephemeral_pending(&mut ws, &mut state, budget).await;
                if sent > 0 {
                    any_sent = true;
                }
            }

            if any_sent {
                drain_pacing_next = Some(tokio::time::Instant::now() + REQ_PACING_INTERVAL);
            } else if !state.gated_observer_pending.is_empty()
                || !state.gated_ephemeral_pending.is_empty()
            {
                // Nothing sent because the gate is still armed. Arm the pacing
                // timer to the gate deadline so parked frames drain promptly
                // even when no other traffic wakes the select loop — the whole
                // value of parking a status frame is that it lands at the gate
                // edge rather than one publisher cadence later.
                drain_pacing_next = state
                    .check_rate_gate()
                    .or_else(|| Some(tokio::time::Instant::now() + REQ_PACING_INTERVAL));
            }
        }

        tokio::select! {
                   // The read is the branch future because it is cancel-safe;
                   // the dispatch runs in the body, where `select!` cannot drop
                   // it part way through. Both belong to `ingress`, and so does
                   // the frame that passes between them.
                   read = ingress::read_frame(&mut ws) => {
                       // Determine if the socket is lost.
                       let socket_lost = match read {
                           ingress::FrameRead::Frame(frame) => {
                               match ingress::dispatch_frame(
                                   frame,
                                   &mut ws,
                                   &event_tx,
                                   &observer_control_tx,
                                   &mut state,
                                   &keys,
                                   &relay_url,
                                   &agent_pubkey_hex,
                                   auth_tag.as_ref(),
                               )
                               .await
                               {
                                   ingress::FrameDispatch::Pong => {
                                       last_pong = Instant::now();
                                       ping_sent = false;
                                       false // pong is healthy — not a socket loss
                                   }
                                   ingress::FrameDispatch::Handled => false,
                                   ingress::FrameDispatch::Lost => true,
                               }
                           }
                           ingress::FrameRead::Lost => true,
                       };

                       if socket_lost {
                           // Signal the caller, then attempt autonomous reconnect.
                           // Use try_send to avoid blocking on backpressure — recovery
                           // must not stall when the event channel is full.
                           let _ = event_tx.try_send(None);
                           let outcome = try_autonomous_reconnect(
                               &mut ws,
                               &mut cmd_rx,
                               &mut state,
                               &keys,
                               &relay_url,
                               &agent_pubkey_hex,
                               &event_tx,
                           &observer_control_tx,
            auth_tag.as_ref(),
                           )
                           .await;
                           match outcome {
                           ReconnectOutcome::Shutdown => return,
                           ReconnectOutcome::Ok => {
                               if matches!(
                                   drain_post_reconnect(&mut ws, &mut cmd_rx, &mut state, &agent_pubkey_hex).await,
                                   ReconnectOutcome::Shutdown
                               ) { return; }
                               // Reset ping state after reconnect.
                               ping_sent = false;
                               last_pong = Instant::now();
                               connected_since = Instant::now();
                               stable_logged = false;
                           }
                           ReconnectOutcome::Failed => {
                               if matches!(
                                   wait_for_reconnect(
                                       &mut ws, &mut cmd_rx, &mut state, &keys, &relay_url,
        &agent_pubkey_hex, &event_tx, &observer_control_tx, true,
                        auth_tag.as_ref(),
                                   ).await,
                                   ReconnectOutcome::Shutdown
                               ) { return; }
                               ping_sent = false;
                               last_pong = Instant::now();
                               connected_since = Instant::now();
                               stable_logged = false;
                           }
                           } // end match outcome
                       }
                   }

                   cmd = cmd_rx.recv() => {
                       match cmd {
                           Some(RelayCommand::Reconnect) => {
                               if matches!(
                                   wait_for_reconnect(
                                       &mut ws, &mut cmd_rx, &mut state, &keys, &relay_url,
        &agent_pubkey_hex, &event_tx, &observer_control_tx, true,
                        auth_tag.as_ref(),
                                   ).await,
                                   ReconnectOutcome::Shutdown
                               ) { return; }
                               ping_sent = false;
                               last_pong = Instant::now();
                               connected_since = Instant::now();
                               stable_logged = false;
                           }
                           Some(RelayCommand::Shutdown) | None => {
                               debug!("background task shutting down — sending close frame");
                               let _ = ws_send_timeout(
                                   &mut ws,
                                   Message::Close(None),
                                   WS_SEND_TIMEOUT_SECS,
                               )
                               .await;
                               return;
                           }
                           Some(cmd) => {
                               let ok = execute_connected_command(
                                   &mut ws,
                                   &mut state,
                                   &agent_pubkey_hex,
                                   cmd,
                               )
                               .await;
                               if !ok {
                                   // Send failed — socket is likely dead. Trigger reconnect.
                                   warn!("command send failed — triggering reconnect");
                                   let _ = event_tx.try_send(None);
                                   match try_autonomous_reconnect(
                                       &mut ws, &mut cmd_rx, &mut state, &keys, &relay_url,
        &agent_pubkey_hex, &event_tx,
                                   &observer_control_tx,
            auth_tag.as_ref(),
                                   ).await {
                                       ReconnectOutcome::Shutdown => return,
                                       ReconnectOutcome::Ok => {
                                           if matches!(
                                               drain_post_reconnect(&mut ws, &mut cmd_rx, &mut state, &agent_pubkey_hex).await,
                                               ReconnectOutcome::Shutdown
                                           ) { return; }
                                       }
                                       ReconnectOutcome::Failed => {
                                           if matches!(
                                               wait_for_reconnect(
                                                   &mut ws, &mut cmd_rx, &mut state, &keys, &relay_url,
        &agent_pubkey_hex, &event_tx, &observer_control_tx, true,
                        auth_tag.as_ref(),
                                               ).await,
                                               ReconnectOutcome::Shutdown
                                           ) { return; }
                                       }
                                   }
                                   ping_sent = false;
                                   last_pong = Instant::now();
                                   connected_since = Instant::now();
                                   stable_logged = false;
                               }
                           }
                       }
                   }

                   _ = ping_interval.tick() => {
                       if ping_sent && last_pong.elapsed() > PONG_TIMEOUT {
                           // No pong received after our last ping — connection is dead.
                           warn!("no pong received within {:?} — connection dead, reconnecting", PONG_TIMEOUT);
                           // Use try_send to avoid blocking on backpressure during recovery.
                           let _ = event_tx.try_send(None);
                           match try_autonomous_reconnect(
                               &mut ws, &mut cmd_rx, &mut state, &keys, &relay_url,
        &agent_pubkey_hex, &event_tx,
                           &observer_control_tx,
            auth_tag.as_ref(),
                           ).await {
                               ReconnectOutcome::Shutdown => return,
                               ReconnectOutcome::Ok => {
                                   if matches!(
                                       drain_post_reconnect(&mut ws, &mut cmd_rx, &mut state, &agent_pubkey_hex).await,
                                       ReconnectOutcome::Shutdown
                                   ) { return; }
                               }
                               ReconnectOutcome::Failed => {
                                   if matches!(
                                       wait_for_reconnect(
                                           &mut ws, &mut cmd_rx, &mut state, &keys, &relay_url,
        &agent_pubkey_hex, &event_tx, &observer_control_tx, true,
                        auth_tag.as_ref(),
                                       ).await,
                                       ReconnectOutcome::Shutdown
                                   ) { return; }
                               }
                           }
                           ping_sent = false;
                           last_pong = Instant::now();
                           connected_since = Instant::now();
                           stable_logged = false;
                       } else if !ping_sent {
                           if let Err(e) = ws_send_timeout(&mut ws, Message::Ping(vec![].into()), WS_SEND_TIMEOUT_SECS).await {
                               warn!("failed to send ping: {e} — triggering reconnect");
                               // Use try_send to avoid blocking on backpressure during recovery.
                               let _ = event_tx.try_send(None);
                               match try_autonomous_reconnect(
                                   &mut ws, &mut cmd_rx, &mut state, &keys, &relay_url,
        &agent_pubkey_hex, &event_tx,
                               &observer_control_tx,
            auth_tag.as_ref(),
                               ).await {
                                   ReconnectOutcome::Shutdown => return,
                                   ReconnectOutcome::Ok => {
                                       if matches!(
                                           drain_post_reconnect(&mut ws, &mut cmd_rx, &mut state, &agent_pubkey_hex).await,
                                           ReconnectOutcome::Shutdown
                                       ) { return; }
                                   }
                                   ReconnectOutcome::Failed => {
                                       if matches!(
                                           wait_for_reconnect(
                                               &mut ws, &mut cmd_rx, &mut state, &keys, &relay_url,
        &agent_pubkey_hex, &event_tx, &observer_control_tx, true,
                        auth_tag.as_ref(),
                                           ).await,
                                           ReconnectOutcome::Shutdown
                                       ) { return; }
                                   }
                               }
                               ping_sent = false;
                               last_pong = Instant::now();
                               connected_since = Instant::now();
                               stable_logged = false;
                           } else {
                               ping_sent = true;
                               debug!("sent ping to relay");
                           }
                       }
                   }

                   // Pacing timer arm — wakes the loop for the next drain batch.
                   // `pending()` when no drain is in progress so this arm never
                   // fires spuriously and never blocks the other select! arms.
                   _ = async {
                       match drain_pacing_next {
                           Some(t) => tokio::time::sleep_until(t).await,
                           None => std::future::pending::<()>().await,
                       }
                   } => {
                       drain_pacing_next = None;
                   }

                   // Retained reconstructed roots. `pending()` while nothing is
                   // retained, so this arm never fires spuriously and never
                   // blocks the others.
                   _ = async {
                       match restore_retry_next {
                           Some(t) => tokio::time::sleep_until(t).await,
                           None => std::future::pending::<()>().await,
                       }
                   } => {
                       restore_retry_next = None;
                       drive_pending_restorations(&mut state, &event_tx);
                   }
               }

        // Reset backoff_step on a long healthy run so a subsequent brief drop
        // retries at the short end of the backoff ladder.
        if !stable_logged && connected_since.elapsed() > Duration::from_secs(STABLE_CONNECTION_SECS)
        {
            stable_logged = true;
            state.backoff_step = 0;
            debug!(
                "connection stable for >{}s — backoff ladder reset",
                STABLE_CONNECTION_SECS
            );
        }
    }
}

/// The connected inbound path.
///
/// **Why this is a module and not two loose functions.** The phase contract
/// requires the canonical scenario's inbound events to cross the same reader
/// that installed the requests they answer, and names "direct midpoint
/// injection replaces the connected path" as a mutant the suite must catch.
/// While the scenario owned the step between `ws.next()` and
/// [`super::handle_ws_message`], it could not: an edit that read the real frame
/// off the socket, discarded it and passed a locally rebuilt `Message::Text` to
/// the handler produced byte-identical input, and no assertion separates a
/// transported value from a reconstruction of itself.
///
/// The composition moves here instead. [`InboundFrame`]'s field is private to
/// this module and [`read_frame`] is its only producer, so nothing outside —
/// the test module included, since it is a sibling of this module rather than a
/// descendant — can present bytes to the handler that did not come off a
/// connection. The substitution has nowhere to happen rather than being
/// forbidden by convention.
///
/// **Why the read and the dispatch stay separable.** The runtime races the read
/// against commands and the ping timer in a `select!`, where every branch
/// future is dropped as soon as another branch completes. `ws.next()` is
/// cancel-safe; a fused read-and-dispatch is not, and a frame cancelled part
/// way through `handle_ws_message` would be lost. So the read is the branch
/// future and the dispatch runs in the branch body, which cannot be cancelled.
/// Both halves live here and both are used by the runtime and by the canonical
/// scenario alike; what a caller cannot do is put anything of its own between
/// them.
mod ingress {
    use super::*;

    /// A frame that came off a live relay connection.
    ///
    /// Opaque on purpose — see the module comment.
    pub(super) struct InboundFrame(Message);

    impl InboundFrame {
        /// The message this frame carries.
        ///
        /// Consuming, and deliberately without a counterpart: a frame can be
        /// unwrapped but not built, so unwrapping one is not a route back to
        /// presenting bytes of a caller's own choosing to the handler.
        pub(super) fn into_message(self) -> Message {
            self.0
        }
    }

    /// The frames a connection delivered before its handshake finished.
    ///
    /// Opaque for the same reason [`InboundFrame`] is. The NIP-42 exchange has
    /// to read the socket to find its `AUTH` and its `OK`, and everything else
    /// that arrives in the meantime is a frame the relay sent on that
    /// connection — it must reach the handler, so this is the one production
    /// path that hands the dispatch a message it did not read a moment earlier.
    ///
    /// That made it the remaining way to put chosen bytes in front of
    /// [`super::handle_ws_message`]: `process_handshake_buffer` is reachable
    /// from the sibling test module, and a `VecDeque<RelayMessage>` is a
    /// caller's to fill. So the collection is a type whose only constructor is
    /// empty and whose only writers are the two handshake readers below, each
    /// of which pushes exactly what it took off `ws`. A caller can hold one; it
    /// cannot put anything in it.
    #[derive(Debug)]
    pub(super) struct HandshakeBuffer(std::collections::VecDeque<RelayMessage>);

    impl HandshakeBuffer {
        /// A buffer holding nothing.
        ///
        /// The only way to make one, and it carries no frames on purpose:
        /// [`wait_for_auth_challenge`] and [`wait_for_any_ok`] are what fill
        /// it, out of a socket.
        pub(super) fn empty() -> Self {
            Self(std::collections::VecDeque::new())
        }

        /// How many frames arrived during the handshake.
        pub(super) fn len(&self) -> usize {
            self.0.len()
        }

        /// Take the first buffered message satisfying `wanted`.
        ///
        /// The handshake readers check here first: a challenge or an OK may
        /// already have been buffered by the previous reader.
        fn take_first(&mut self, wanted: impl Fn(&RelayMessage) -> bool) -> Option<RelayMessage> {
            let idx = self.0.iter().position(wanted)?;
            self.0.remove(idx)
        }

        /// File a message that came off the socket.
        ///
        /// Private to this module: the pushers are the two readers below, and
        /// nothing else may add to what the dispatch will later be handed.
        fn push(&mut self, msg: RelayMessage) {
            self.0.push_back(msg);
        }
    }

    /// What [`read_frame`] took off the connection.
    pub(super) enum FrameRead {
        /// A frame, ready for [`dispatch_frame`].
        Frame(InboundFrame),
        /// The connection is gone; nothing was read.
        Lost,
    }

    /// What dispatching a frame did to the connection.
    #[derive(Debug, PartialEq, Eq)]
    pub(super) enum FrameDispatch {
        /// The frame was handled and the connection is healthy.
        Handled,
        /// The frame was the relay's pong — the peer is alive.
        Pong,
        /// The frame signals the connection should be dropped.
        Lost,
    }

    /// Take the next frame off `ws`.
    ///
    /// Cancel-safe: it awaits `ws.next()` and nothing else, so a `select!` that
    /// drops it loses no frame.
    pub(super) async fn read_frame(ws: &mut WsStream) -> FrameRead {
        match ws.next().await {
            Some(Ok(msg)) => FrameRead::Frame(InboundFrame(msg)),
            Some(Err(e)) => {
                warn!("WebSocket error in background task: {e}");
                FrameRead::Lost
            }
            None => {
                debug!("WebSocket stream ended");
                FrameRead::Lost
            }
        }
    }

    /// Hand a frame that came off the connection to the production handler.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn dispatch_frame(
        frame: InboundFrame,
        ws: &mut WsStream,
        event_tx: &mpsc::Sender<Option<BuzzEvent>>,
        observer_control_tx: &mpsc::Sender<Event>,
        state: &mut BgState,
        keys: &Keys,
        relay_url: &str,
        agent_pubkey_hex: &str,
        auth_tag: Option<&nostr::Tag>,
    ) -> FrameDispatch {
        if matches!(frame.0, Message::Pong(_)) {
            return FrameDispatch::Pong;
        }
        if handle_ws_message(
            frame,
            ws,
            event_tx,
            observer_control_tx,
            state,
            keys,
            relay_url,
            agent_pubkey_hex,
            auth_tag,
        )
        .await
        {
            FrameDispatch::Handled
        } else {
            FrameDispatch::Lost
        }
    }

    /// Replay messages buffered during the NIP-42 handshake.
    ///
    /// The connection may have delivered EVENTs and EOSEs while we were waiting
    /// for the challenge and OK. Those messages would otherwise be silently
    /// discarded, so they are re-encoded and pushed through the same handler
    /// every connected frame crosses.
    ///
    /// This lives inside `ingress` because it is the one production path that
    /// legitimately builds a frame rather than reading one, and the constructor
    /// that lets it do so must not be reachable from anywhere else. Its
    /// argument is a [`HandshakeBuffer`] for the other half of the same
    /// reason: the function is reachable from outside, so what it may be given
    /// must not be.
    ///
    /// Returns `false` if any buffered message signals the connection should be
    /// dropped.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn process_handshake_buffer(
        ws: &mut WsStream,
        buffer: HandshakeBuffer,
        event_tx: &mpsc::Sender<Option<BuzzEvent>>,
        observer_control_tx: &mpsc::Sender<Event>,
        state: &mut BgState,
        keys: &Keys,
        relay_url: &str,
        agent_pubkey_hex: &str,
        auth_tag: Option<&nostr::Tag>,
    ) -> bool {
        if buffer.0.is_empty() {
            return true;
        }
        debug!("processing {} buffered handshake message(s)", buffer.len());
        for relay_msg in buffer.0 {
            // Re-encode to text so we can reuse the handler. This is slightly
            // wasteful but keeps it the single source of truth for dispatch.
            let text = match &relay_msg {
                RelayMessage::Event {
                    subscription_id,
                    event,
                } => serde_json::to_string(&json!(["EVENT", subscription_id, event])).ok(),
                RelayMessage::Eose { subscription_id } => {
                    serde_json::to_string(&json!(["EOSE", subscription_id])).ok()
                }
                RelayMessage::Notice { message } => {
                    serde_json::to_string(&json!(["NOTICE", message])).ok()
                }
                RelayMessage::Closed {
                    subscription_id,
                    message,
                } => serde_json::to_string(&json!(["CLOSED", subscription_id, message])).ok(),
                RelayMessage::Ok {
                    event_id,
                    accepted,
                    message,
                } => serde_json::to_string(&json!(["OK", event_id, accepted, message])).ok(),
                // AUTH in the buffer is stale — skip it.
                RelayMessage::Auth { .. } => None,
            };
            if let Some(text) = text {
                let outcome = dispatch_frame(
                    InboundFrame(Message::Text(text.into())),
                    ws,
                    event_tx,
                    observer_control_tx,
                    state,
                    keys,
                    relay_url,
                    agent_pubkey_hex,
                    auth_tag,
                )
                .await;
                if outcome == FrameDispatch::Lost {
                    debug!("buffered message signalled connection loss");
                    return false;
                }
            }
        }
        true
    }

    /// Wait for an `AUTH` challenge from the relay, buffering any other messages.
    pub(super) async fn wait_for_auth_challenge(
        ws: &mut WsStream,
        buffer: &mut HandshakeBuffer,
        timeout_dur: Duration,
    ) -> Result<String, RelayError> {
        // Check if there's already one buffered.
        if let Some(RelayMessage::Auth { challenge }) =
            buffer.take_first(|m| matches!(m, RelayMessage::Auth { .. }))
        {
            return Ok(challenge);
        }

        let deadline = tokio::time::Instant::now() + timeout_dur;

        loop {
            let remaining = deadline
                .checked_duration_since(tokio::time::Instant::now())
                .unwrap_or(Duration::ZERO);

            if remaining.is_zero() {
                return Err(RelayError::NoAuthChallenge);
            }

            let raw = timeout(remaining, ws.next())
                .await
                .map_err(|_| RelayError::NoAuthChallenge)?
                .ok_or(RelayError::ConnectionClosed)?
                .map_err(|e| RelayError::WebSocket(Box::new(e)))?;

            match raw {
                Message::Text(text) => {
                    let msg = parse_relay_message(&text)?;
                    match msg {
                        RelayMessage::Auth { challenge } => return Ok(challenge),
                        other => buffer.push(other),
                    }
                }
                Message::Ping(data) => {
                    ws_send_timeout(ws, Message::Pong(data), WS_SEND_TIMEOUT_SECS)
                        .await
                        .map_err(|_| RelayError::Timeout)?;
                }
                Message::Close(_) => return Err(RelayError::ConnectionClosed),
                _ => {}
            }
        }
    }

    /// Response from an `OK` relay message.
    pub(super) struct OkResponse {
        pub(super) event_id: String,
        pub(super) accepted: bool,
        pub(super) message: String,
    }

    /// Wait for the first `OK` message from the relay (used after sending AUTH).
    pub(super) async fn wait_for_any_ok(
        ws: &mut WsStream,
        buffer: &mut HandshakeBuffer,
        timeout_dur: Duration,
    ) -> Result<OkResponse, RelayError> {
        // Check if there's already one buffered.
        if let Some(RelayMessage::Ok {
            event_id,
            accepted,
            message,
        }) = buffer.take_first(|m| matches!(m, RelayMessage::Ok { .. }))
        {
            return Ok(OkResponse {
                event_id,
                accepted,
                message,
            });
        }

        let deadline = tokio::time::Instant::now() + timeout_dur;

        loop {
            let remaining = deadline
                .checked_duration_since(tokio::time::Instant::now())
                .unwrap_or(Duration::ZERO);

            if remaining.is_zero() {
                return Err(RelayError::Timeout);
            }

            let raw = timeout(remaining, ws.next())
                .await
                .map_err(|_| RelayError::Timeout)?
                .ok_or(RelayError::ConnectionClosed)?
                .map_err(|e| RelayError::WebSocket(Box::new(e)))?;

            match raw {
                Message::Text(text) => {
                    let msg = parse_relay_message(&text)?;
                    match msg {
                        RelayMessage::Ok {
                            event_id,
                            accepted,
                            message,
                        } => {
                            return Ok(OkResponse {
                                event_id,
                                accepted,
                                message,
                            });
                        }
                        other => buffer.push(other),
                    }
                }
                Message::Ping(data) => {
                    ws_send_timeout(ws, Message::Pong(data), WS_SEND_TIMEOUT_SECS)
                        .await
                        .map_err(|_| RelayError::Timeout)?;
                }
                Message::Close(_) => return Err(RelayError::ConnectionClosed),
                _ => {}
            }
        }
    }
}

/// Handle a single WebSocket message in the background task.
///
/// Returns `false` if the connection has been lost (Close frame or unrecoverable
/// error), `true` otherwise.
///
/// Reached only through [`ingress::dispatch_frame`], which is the only holder of
/// a frame to give it.
#[allow(clippy::too_many_arguments)]
async fn handle_ws_message(
    frame: ingress::InboundFrame,
    ws: &mut WsStream,
    event_tx: &mpsc::Sender<Option<BuzzEvent>>,
    observer_control_tx: &mpsc::Sender<Event>,
    state: &mut BgState,
    keys: &Keys,
    relay_url: &str,
    agent_pubkey_hex: &str,
    auth_tag: Option<&nostr::Tag>,
) -> bool {
    match frame.into_message() {
        Message::Text(text) => {
            let relay_msg = match parse_relay_message(&text) {
                Ok(m) => m,
                Err(e) => {
                    warn!("failed to parse relay message: {e} — raw: {text}");
                    return true;
                }
            };

            match relay_msg {
                RelayMessage::Event {
                    subscription_id,
                    event,
                } => {
                    if subscription_id == OBSERVER_CONTROL_SUB_ID {
                        match observer_control_tx.try_send(*event) {
                            Ok(()) => {}
                            Err(mpsc::error::TrySendError::Full(_)) => {
                                warn!("observer control event dropped because control channel is full");
                            }
                            Err(mpsc::error::TrySendError::Closed(_)) => return false,
                        }
                    } else if subscription_id == MEMBERSHIP_NOTIF_SUB_ID {
                        // Shape gate first, before anything with a side effect.
                        //
                        // `send_membership_subscribe` asks for exactly these two
                        // kinds, so anything else arriving here is a relay off
                        // its own contract. Accepting it was a watermark
                        // poisoning path, not a cosmetic one: a wrong-kind event
                        // advanced `membership_last_seen`, and reconnect uses
                        // that value directly as the membership REQ's `since`
                        // (`resubscribe_after_reconnect`, and the rate-limit
                        // drain). One signed event with a far-future timestamp
                        // therefore moved the watermark past legitimate
                        // membership notifications and they were never replayed
                        // — the agent silently keeps acting on stale membership.
                        //
                        // Refusing here costs the frame nothing it is entitled
                        // to: the event is not deduped, so its own channel
                        // subscription still delivers it normally.
                        let kind_u32 = event.kind.as_u16() as u32;
                        if !matches!(
                            kind_u32,
                            KIND_MEMBER_ADDED_NOTIFICATION | KIND_MEMBER_REMOVED_NOTIFICATION
                        ) {
                            warn!(
                                kind = kind_u32,
                                event_id = %event.id.to_hex(),
                                "non-membership kind on the membership subscription — refusing without \
                                 spending dedup or watermarks"
                            );
                            return true;
                        }
                        // Membership notification — extract channel UUID from h tag.
                        let channel_uuid = match extract_h_tag_uuid(&event) {
                            Some(uuid) => uuid,
                            None => {
                                warn!("membership notification missing h tag — dropping");
                                return true;
                            }
                        };
                        // Dedup membership notifications through TwoGenDedup.
                        // We use seen_ids directly instead of record_event()
                        // because record_event() also updates last_seen, which
                        // would contaminate per-channel replay watermarks with
                        // membership-event timestamps and cause channel event
                        // loss on reconnect.
                        let event_id_hex = event.id.to_hex();
                        if !state.seen_ids.insert(event_id_hex.clone()) {
                            debug!(
                                channel_id = %channel_uuid,
                                event_id = %event_id_hex,
                                "duplicate membership notification — skipping"
                            );
                            return true;
                        }
                        let ts = event.created_at.as_secs();
                        let buzz_event = BuzzEvent::Channel {
                            channel_id: channel_uuid,
                            event: *event,
                        };
                        let cap = event_tx.max_capacity();
                        let used = cap - event_tx.capacity();
                        if used >= (cap * 4 / 5) {
                            warn!(
                                used,
                                capacity = cap,
                                "event channel at ≥80% capacity — backpressure imminent"
                            );
                        }
                        match event_tx.try_send(Some(buzz_event)) {
                            Ok(()) => {
                                state.membership_last_seen =
                                    Some(state.membership_last_seen.unwrap_or(0).max(ts));
                            }
                            Err(mpsc::error::TrySendError::Full(_)) => {
                                // Remove from dedup so reconnect replay can
                                // re-deliver this event (it was never forwarded
                                // to the harness).
                                state.seen_ids.remove(&event_id_hex);
                                // Track the oldest dropped timestamp so reconnect
                                // replay starts early enough to re-deliver it.
                                state.membership_dropped_since =
                                    Some(state.membership_dropped_since.map_or(ts, |d| d.min(ts)));
                                // Proactively trigger resubscribe without waiting for a disconnect.
                                state.proactive_resubscribe_needed = true;
                                warn!(
                                    channel_id = %channel_uuid,
                                    ts,
                                    "membership notification dropped (backpressure) — proactive resubscribe queued"
                                );
                            }
                            Err(mpsc::error::TrySendError::Closed(_)) => return false,
                        }
                    } else if subscription_id == PEER_CALL_SUB_ID {
                        // Shape gate first, as on the membership subscription:
                        // this REQ asks for exactly two kinds, so anything else
                        // is the relay off its own contract and is refused
                        // before it can spend a dedup slot.
                        let kind_u32 = event.kind.as_u16() as u32;
                        if !matches!(kind_u32, KIND_PEER_CALL | KIND_PEER_CALL_RESULT) {
                            warn!(
                                kind = kind_u32,
                                event_id = %event.id.to_hex(),
                                "non-peer-call kind on the peer-call subscription — refusing"
                            );
                            return true;
                        }

                        // Only the channel route travels this way. A
                        // project-routed envelope carries `a` + `e` and no `h`,
                        // and it is delivered by the watched-root REQ that
                        // already owns that root — enrolment state, session key
                        // and lifecycle all live there. Splitting on the route
                        // the envelope itself declares is what keeps a project
                        // call from arriving twice, once down each path, and
                        // having the second delivery refused as a replay of the
                        // first.
                        let Some(channel_uuid) = extract_h_tag_uuid(&event) else {
                            // A project-routed envelope: `a` + rooted `e`, no
                            // `h`. It used to be dropped here on the assumption
                            // that the watched-root REQ owned it. That holds
                            // only while the root is already enrolled *and* a
                            // watched generation is live — so a call arriving
                            // before enrolment, or inside a REQ replacement
                            // window, was discarded by both paths and the pair
                            // never began.
                            //
                            // The peer subscription is a **transport source and
                            // nothing more**. It carries the event to the same
                            // project entry the watched stream uses, where
                            // enrolment, lifecycle, trusted-peer, caller/callee,
                            // ledger, replay, hop and visited rules all still
                            // decide. Arriving here grants no authority; it only
                            // means the event arrived.
                            route_project_peer_call(state, event_tx, *event).await;
                            return true;
                        };

                        // Dedup through `seen_ids` directly rather than
                        // `record_event`, for the same reason membership does:
                        // this subscription's `since` is its own, and letting a
                        // peer call advance a channel's replay watermark would
                        // lose ordinary channel events across a reconnect.
                        let event_id_hex = event.id.to_hex();
                        if !state.seen_ids.insert(event_id_hex.clone()) {
                            debug!(
                                event_id = %event_id_hex,
                                "duplicate peer-call event — skipping"
                            );
                            return true;
                        }

                        let buzz_event = BuzzEvent::Channel {
                            channel_id: channel_uuid,
                            event: *event,
                        };
                        match event_tx.try_send(Some(buzz_event)) {
                            Ok(()) => {}
                            Err(mpsc::error::TrySendError::Full(_)) => {
                                // Release the id so a replay can re-deliver it:
                                // it never reached the harness.
                                state.seen_ids.remove(&event_id_hex);
                                state.proactive_resubscribe_needed = true;
                                warn!(
                                    channel_id = %channel_uuid,
                                    "peer-call event dropped (backpressure) — proactive resubscribe queued"
                                );
                            }
                            Err(mpsc::error::TrySendError::Closed(_)) => return false,
                        }
                    } else if let Some(admission) =
                        state.project_requests.admit_frame(&subscription_id)
                    {
                        // A frame on a catch-up leaves here first, and by a
                        // different route: to the page its own request opened,
                        // without crossing the event channel at all. Its page
                        // counts what the relay returned in order to tell a
                        // saturated page from an exhausted one, so the steps
                        // below — which drop a frame at every failed check —
                        // would silently shorten it. Every admitted frame
                        // reaches the page; what varies is whether it arrives
                        // as a row or as a reason the page cannot be trusted.
                        if let crate::project::ProjectSubscription::RootCatchUp { root, .. } =
                            admission.subscription()
                        {
                            let expected = root.clone();
                            route_catch_up_frame(state, admission, expected, *event).await;
                            return true;
                        }
                        // The enrolment walk's pages take the same path and for
                        // the same reason: they count what the relay returned.
                        if matches!(
                            admission.subscription(),
                            crate::project::ProjectSubscription::EnrolmentHistory { .. }
                        ) {
                            route_enrolment_history_frame(state, admission, *event).await;
                            return true;
                        }
                        // Replay or live is **the class of the request that
                        // delivered this frame**, read from what this agent
                        // recorded when it sent the REQ. Nothing here decides
                        // it, so there is nothing here to get wrong.
                        //
                        // Two earlier rules lived here and both were defects
                        // of the same shape — a second producer of an answer
                        // that already had one.
                        //
                        // The first read the enrolment REQ's drain flag. The
                        // enrolment id is fixed, so a *predecessor's* EOSE
                        // arriving after its successor was registered certified
                        // a backlog it knew nothing about; the successor's
                        // remaining stored frames were stamped live and four
                        // historical roots were re-answered on a real relay.
                        //
                        // The second read `created_at < startup_watermark`.
                        // That is the author's clock, which the relay bounds at
                        // ±15 minutes on ingest: a root published live by a
                        // slightly slow clock looked historical, so it enrolled
                        // silently and was never answered. It also could not
                        // tell a genuinely historical root from one this agent
                        // had already handled.
                        //
                        // Now the two questions have two requests, and neither
                        // answer is recomputed anywhere downstream.
                        //
                        // `EnrolmentHistory` pages walk backwards on their own
                        // generation-distinct identities and are always replay.
                        // The enrolment REQ is a live tail — floored a full
                        // accepted-skew interval below startup, because a relay
                        // filters `since` on the author's signed `created_at`
                        // and would otherwise never hand over an accepted
                        // slow-clock root at all. That floor pulls a stored
                        // prefix in behind it, so the tail is asked which
                        // registration admitted this frame and whether that
                        // registration's own backlog is still draining. A
                        // predecessor's boundary cannot answer for it, and no
                        // timestamp is consulted: a skewed root arriving after
                        // the boundary is live because *this* request was past
                        // its stored events when it delivered it.
                        let source = admission.subscription().clone();
                        let mode =
                            if matches!(source, crate::project::ProjectSubscription::Enrolment) {
                                state.project_requests.enrolment_frame_mode(&admission)
                            } else if matches!(
                                source,
                                crate::project::ProjectSubscription::EnrolmentHistory { .. }
                            ) {
                                crate::project::ProcessingMode::Replay
                            } else {
                                crate::project::ProcessingMode::Live
                            };

                        // Project dispatch. The step order below is the whole
                        // security property, so it is written out rather than
                        // left to reading order:
                        //
                        //   1. matched against a request we actually opened,
                        //      and classified from *our* record of it;
                        //   2. id + signature verified;
                        //   3. source-specific admissibility;
                        //   4. only then dedup.
                        //
                        // Step 1 used to be `classify_subscription`, which read
                        // the class out of the relay's own string. `proj-roots-7`
                        // *was* generation 7 because it said so. The registry
                        // makes the id a lookup key: an id we never opened has
                        // no class, and the class a frame is handled under now
                        // comes from what we wrote down rather than from what
                        // arrived.
                        //
                        // Verification precedes dedup because an invalid event
                        // must not spend the id of a genuine one that has not
                        // arrived yet — that would let a malicious relay
                        // suppress a real event by pre-sending a forgery
                        // claiming its id.
                        //
                        // Verification is awaited inline rather than spawned.
                        // The Schnorr check hands off to `spawn_blocking`
                        // internally, so the runtime is not blocked, and
                        // awaiting here means hostile project traffic can slow
                        // this loop but cannot fan out unbounded blocking jobs.
                        let verified =
                            match crate::project::VerifiedProjectEvent::verify(*event).await {
                                Ok(v) => v,
                                Err(e) => {
                                    warn!(
                                        sub_id = %subscription_id,
                                        "project event failed verification — dropping: {e}"
                                    );
                                    return true;
                                }
                            };

                        // Does it match the question this request asked?
                        //
                        // Before the route is derived and before the dedup slot
                        // is spent, because both of those treat the event as
                        // ours. A relay decides what to put under a
                        // subscription id; the filter is the only record of
                        // what was *requested*, and until this check existed a
                        // correctly signed event for a root this agent never
                        // watched was delivered as a routed event on the
                        // watched subscription — and spent the shared project
                        // dedup slot doing it, which would then suppress the
                        // same event's legitimate delivery elsewhere.
                        //
                        // Catch-up frames do not come through here: their page
                        // must *count* what the relay returned, so a frame it
                        // will not accept is delivered as a reason the page is
                        // untrustworthy rather than dropped.
                        if !admission.admits(verified.event()) {
                            warn!(
                                sub_id = %subscription_id,
                                kind = verified.kind(),
                                "project event does not match the filter this request sent — dropping"
                            );
                            return true;
                        }

                        let event_id_hex = verified.id();
                        let ts = verified.event().created_at.as_secs();

                        // Source-specific admissibility **before** the dedup
                        // slot is spent.
                        //
                        // The earlier order — verify, dedup, then decide
                        // admissibility — left a suppression primitive inside
                        // the project namespace, one narrower than the
                        // cross-surface one but the same shape: an event that
                        // this source was never entitled to carry still spent
                        // the id, and the delivery that *was* entitled then saw
                        // a duplicate. A genuine announcement pushed under a
                        // watched id burned discovery; a genuine root pushed
                        // under discovery burned enrolment; an event for root B
                        // under root-A's catch-up burned its real rooted
                        // delivery.
                        //
                        // The comment that used to sit here claimed deduping
                        // later would force a fresh Schnorr check on every
                        // replay. That was wrong: verification already runs
                        // unconditionally above, so dedup never gated it and
                        // moving dedup down costs no signature work at all.
                        let project_event = match &source {
                            // An announcement has no root. Sending it through
                            // route derivation would drop it via a path that
                            // looks like it handled it.
                            crate::project::ProjectSubscription::Discovery => {
                                // The full announcement shape, not just the
                                // kind. A kind check alone let a malformed
                                // `30617` — no `d`, an empty `d`, two `d`s —
                                // spend a dedup slot here and be rejected much
                                // later, inside the state it was trying to
                                // enter. `prove` is the single place that
                                // parses `d`, and its failure is this frame's
                                // failure.
                                let Some(announcement) =
                                    crate::project::VerifiedAnnouncement::prove(verified)
                                else {
                                    debug!(
                                        sub_id = %subscription_id,
                                        "not a well-formed repository announcement — dropping"
                                    );
                                    return true;
                                };
                                crate::project::ProjectEvent::Discovery { announcement }
                            }
                            other => {
                                let Some(route) = crate::project::ProjectRoute::derive(&verified)
                                else {
                                    debug!(
                                        sub_id = %subscription_id,
                                        kind = verified.kind(),
                                        "project event resolves to no root — dropping"
                                    );
                                    return true;
                                };

                                // The catch-up root check that used to sit here
                                // has moved to `route_catch_up_frame`, which
                                // is the only path a catch-up frame now takes.
                                // It could not stay: the answer to "the relay
                                // returned a different root" is not `return
                                // true`. Dropping the frame leaves the page one
                                // row short of what the relay actually sent, and
                                // a short page is how a reconstruction decides
                                // it has reached the end of history.
                                crate::project::ProjectEvent::Routed {
                                    source: other.clone(),
                                    route,
                                    event: verified,
                                    mode,
                                }
                            }
                        };

                        // Only now is the id spent, and against
                        // `project_seen_ids` rather than the channel set —
                        // spending a channel id from here would let a project
                        // sub-id suppress a legitimate channel delivery.
                        //
                        // One set for every *live* project source, and no longer
                        // for catch-up replay, which never reaches here. Sharing
                        // a dedup domain across the two was not merely untidy:
                        // an event already delivered on the watched-root REQ
                        // would be suppressed as a duplicate on a history page,
                        // and the page — which counts rows to decide whether it
                        // is saturated — would read short by exactly the number
                        // of events the agent had already seen live.
                        if !state.project_seen_ids.insert(event_id_hex.clone()) {
                            debug!(event_id = %event_id_hex, "duplicate project event — skipping");
                            return true;
                        }

                        match event_tx.try_send(Some(BuzzEvent::Project(project_event))) {
                            Ok(()) => {}
                            Err(mpsc::error::TrySendError::Full(_)) => {
                                // Same contract as the channel path: release the
                                // dedup slot so replay can re-deliver, and record
                                // the drop so the replay window covers it.
                                //
                                // The dropped timestamp is subscription-scoped,
                                // not per-root. Per-root dropped state was the
                                // arrangement already rejected for REQ
                                // replacement: the watched-root REQ is one
                                // subscription over many roots, so a per-root
                                // floor cannot express when *this subscription*
                                // must replay from.
                                // Only the project slot is released, and only
                                // the project replay floor moves. Touching
                                // `seen_ids` or `channel_dropped_since` here
                                // would let project pressure rewind a channel's
                                // replay window and re-deliver channel events
                                // the agent already handled.
                                state.project_seen_ids.remove(&event_id_hex);
                                state.project_dropped_since =
                                    Some(state.project_dropped_since.map_or(ts, |d| d.min(ts)));
                                state.proactive_resubscribe_needed = true;
                                warn!(
                                    ts,
                                    "project event dropped (backpressure) — proactive resubscribe queued"
                                );
                            }
                            Err(mpsc::error::TrySendError::Closed(_)) => return false,
                        }
                    } else if subscription_id.starts_with(crate::project::PROJECT_SUB_ID_PREFIX) {
                        // A project-shaped id that matched no open request.
                        //
                        // The prefix test decides only what to *say* about the
                        // frame and stops it falling through to channel
                        // parsing; it never grants admission. Admission comes
                        // from the registry above and nowhere else, which is
                        // why an id we closed reaches this arm rather than the
                        // one it used to work in.
                        warn!(
                            sub_id = %subscription_id,
                            "unsolicited frame on a project subscription this agent did not open — dropping"
                        );
                    } else if let Some(channel_id) = channel_id_from_sub_id(&subscription_id) {
                        let ts = event.created_at.as_secs();
                        let event_id_hex = event.id.to_hex();
                        if state.record_event(channel_id, &event) {
                            let buzz_event = BuzzEvent::Channel {
                                channel_id,
                                event: *event,
                            };
                            // Warn at 80% capacity.
                            let cap = event_tx.max_capacity();
                            let used = cap - event_tx.capacity();
                            if used >= (cap * 4 / 5) {
                                warn!(
                                    used,
                                    capacity = cap,
                                    "event channel at ≥80% capacity — backpressure imminent"
                                );
                            }
                            match event_tx.try_send(Some(buzz_event)) {
                                Ok(()) => {}
                                Err(mpsc::error::TrySendError::Full(_)) => {
                                    // Remove from dedup set so the replayed event
                                    // won't be rejected as a duplicate after reconnect.
                                    state.seen_ids.remove(&event_id_hex);
                                    // Track the oldest dropped timestamp so reconnect
                                    // replay starts early enough to re-deliver it.
                                    state
                                        .channel_dropped_since
                                        .entry(channel_id)
                                        .and_modify(|d| *d = (*d).min(ts))
                                        .or_insert(ts);
                                    // Proactively trigger resubscribe without waiting for a disconnect.
                                    state.proactive_resubscribe_needed = true;
                                    warn!(
                                        channel_id = %channel_id,
                                        ts,
                                        "event channel full — dropping event for channel {channel_id} — proactive resubscribe queued"
                                    );
                                }
                                Err(mpsc::error::TrySendError::Closed(_)) => {
                                    // Receiver dropped — shut down.
                                    return false;
                                }
                            }
                        } else {
                            debug!("dropping duplicate event for channel {channel_id}");
                        }
                    } else {
                        warn!("received EVENT for unknown subscription {subscription_id}");
                    }
                }
                RelayMessage::Eose { subscription_id } => {
                    // A project EOSE becomes a witness only if we hold a live
                    // registration for that exact id. An EOSE for a request we
                    // never sent, or have already closed, is a relay assertion
                    // rather than evidence about our own backlog — and a
                    // completion claim resting on it would be resting on the
                    // relay's word.
                    //
                    // Nothing else here mints one. A timeout, `CLOSED`,
                    // `NOTICE` or reconnect never reaches this arm, so none of
                    // them can produce an end-of-backlog boundary.
                    //
                    // The boundary goes nowhere near the event channel. It is
                    // consumed where it is minted, in one uninterrupted step:
                    // minting retires a one-shot catch-up registration, and the
                    // page it bounds is completed immediately after, with no
                    // await between. Nothing else can observe the interval, so
                    // there is no interval in which the registry and the page
                    // owner disagree about whether that request is current.
                    if let Some(witness) = state
                        .project_requests
                        .witness_end_of_stored_events(&subscription_id)
                    {
                        debug!(sub_id = %subscription_id, "project EOSE");

                        // The enrolment tail's own boundary, and only its own.
                        //
                        // The tail keeps delivering after this, so nothing is
                        // retired — what ends is the claim that its remaining
                        // frames are the window the history walk covers. The
                        // check is by allocation, so a boundary from the
                        // predecessor that wore this same fixed id, or from a
                        // registration on a connection that has since died,
                        // closes nothing. That precise substitution is what
                        // stamped a successor's stored frames live and
                        // re-answered four historical roots on a real relay.
                        if state.project_requests.close_enrolment_backlog(&witness) {
                            debug!(
                                sub_id = %subscription_id,
                                "enrolment tail backlog drained — later frames are live"
                            );
                        }

                        // `None` is the ordinary case for discovery, enrolment
                        // and watched requests: they keep delivering after their
                        // backlog drains, so their boundary retires no page.
                        if let Some(advance) = state.reconstructions.complete(&witness) {
                            debug!(sub_id = %subscription_id, ?advance, "history page completed");
                            match advance {
                                // Another page, on the bound the cursor reached.
                                crate::project::StreamAdvance::Continue { .. } => {
                                    drive_root_reconstructions(ws, state).await;
                                }
                                // One stream is exhausted. The root's history is
                                // only *finished* when every stream its class
                                // requires is, and `take_completed` is the one
                                // thing that answers that — the driver is still
                                // called because the other stream may want a
                                // page it has not been given.
                                crate::project::StreamAdvance::Finished { stream } => {
                                    let root = match witness.subscription() {
                                        crate::project::ProjectSubscription::RootCatchUp {
                                            root,
                                            ..
                                        } => root.clone(),
                                        // Unreachable: `complete` returns `None`
                                        // for any other class.
                                        _ => String::new(),
                                    };
                                    debug!(root = %root, ?stream, "root history stream exhausted");
                                    finish_root_catch_up(state, event_tx, &root);
                                    drive_root_reconstructions(ws, state).await;
                                }
                                // Ordinary reconnect traffic. The page in flight
                                // is untouched and still completable by its own
                                // boundary, so nothing is asked for here.
                                crate::project::StreamAdvance::Stale { .. } => {}
                                crate::project::StreamAdvance::Degraded {
                                    stream, reason, ..
                                } => {
                                    let root = match witness.subscription() {
                                        crate::project::ProjectSubscription::RootCatchUp {
                                            root,
                                            ..
                                        } => root.clone(),
                                        _ => String::new(),
                                    };
                                    degrade_root_catch_up(
                                        state,
                                        &root,
                                        format!("{stream:?}: {reason}"),
                                    );
                                }
                            }
                        }

                        // The enrolment walk's own boundary, and **only** its
                        // own: `complete` compares the page in flight against
                        // this witness's registration, so a boundary from the
                        // live enrolment tail, from a predecessor page, or from
                        // a request opened on a connection that has since died
                        // certifies nothing here. That is the defect that
                        // re-answered four historical roots on a real relay,
                        // closed at the structure rather than by a flag.
                        let generation = match witness.subscription() {
                            crate::project::ProjectSubscription::EnrolmentHistory {
                                generation,
                            } => *generation,
                            _ => 0,
                        };
                        let advance = state
                            .enrolment_history
                            .as_mut()
                            .and_then(|walk| walk.complete(&witness));
                        match advance {
                            Some(crate::project::EnrolmentAdvance::Continue { until, limit }) => {
                                debug!(
                                    sub_id = %subscription_id, until, limit,
                                    "enrolment history page saturated — asking further back"
                                );
                                drive_enrolment_history(ws, state).await;
                            }
                            Some(crate::project::EnrolmentAdvance::Finished { roots }) => {
                                // An exhausted walk is not a finished one. The
                                // roots still have to reach the run loop, and
                                // the completion line belongs to whichever
                                // attempt empties the queue — here, or a later
                                // retry, or never, in which case the walk
                                // degrades. Nothing is reported from this site.
                                let outcome =
                                    begin_restoring_roots(state, event_tx, generation, roots);
                                debug!(?outcome, "enrolment history walk exhausted");
                            }
                            Some(crate::project::EnrolmentAdvance::Stale) => {
                                debug!(
                                    sub_id = %subscription_id,
                                    "boundary from a superseded enrolment history page — ignored"
                                );
                            }
                            Some(crate::project::EnrolmentAdvance::Degraded { reason }) => {
                                degrade_enrolment_history(state, reason);
                            }
                            None => {}
                        }
                    } else {
                        debug!("EOSE for subscription {subscription_id}");
                    }
                }
                RelayMessage::Notice { message } => {
                    // Fix 4: NOTICE at warn level.
                    tracing::warn!("relay NOTICE: {message}");
                    // The relay sends NOTICE for rate-limited EVENT/COUNT frames.
                    if message.starts_with("rate-limited:") {
                        let secs = parse_rate_limit_retry_secs(&message).unwrap_or(0);
                        let deadline = state.set_rate_limit_gate(secs);
                        state.requeue_observer_in_flight();
                        warn!(
                            "rate-limit gate armed via NOTICE until ~{:.1}s from now",
                            deadline
                                .checked_duration_since(tokio::time::Instant::now())
                                .unwrap_or_default()
                                .as_secs_f64()
                        );
                    }
                }
                RelayMessage::Closed {
                    subscription_id,
                    message,
                } => {
                    // A CLOSED is evidence about a request we actually sent on
                    // *this* connection, and it is authenticated the same way
                    // an EVENT is: by an exact live registration. Durable
                    // intent is not evidence — it says what we want, not what
                    // we asked. Gating on intent let relay text mutate
                    // suspension state for an id that had never been sent, so
                    // an unsolicited CLOSED could suppress a request before it
                    // was ever made.
                    //
                    // Minted *before* the refusal, because a refusal removes the
                    // registration and the proof is what names it. A page in
                    // flight under this request has to learn its request is
                    // gone: no boundary can ever follow a CLOSED, so a page left
                    // attached keeps its stream out of `pages_wanted` forever —
                    // a reconstruction stalled in silence by one relay message.
                    let lost = state
                        .project_requests
                        .admit_frame(&subscription_id)
                        .filter(|a| {
                            matches!(
                                a.subscription(),
                                crate::project::ProjectSubscription::RootCatchUp { .. }
                                    | crate::project::ProjectSubscription::EnrolmentHistory { .. }
                            )
                        });
                    if state
                        .project_requests
                        .refuse_live(&subscription_id, &message)
                        .is_some()
                    {
                        if let Some(lost) = lost {
                            let is_enrolment_history = matches!(
                                lost.subscription(),
                                crate::project::ProjectSubscription::EnrolmentHistory { .. }
                            );
                            let frame = lost.catch_up(crate::project::CatchUpOutcome::RequestLost(
                                "relay closed the request",
                            ));
                            if is_enrolment_history {
                                // Released, not completed. A CLOSED is never a
                                // boundary, so the walk keeps its cursor and
                                // re-asks the same bound under a fresh
                                // registration — a page left attached would
                                // stall the walk in silence, since no boundary
                                // can ever follow a CLOSED.
                                if let Some(walk) = state.enrolment_history.as_mut() {
                                    let routing = walk.observe(frame);
                                    debug!(
                                        sub_id = %subscription_id,
                                        ?routing,
                                        "enrolment history page released by a closed request"
                                    );
                                }
                                drive_enrolment_history(ws, state).await;
                            } else {
                                let routing = state.reconstructions.observe(frame);
                                debug!(
                                    sub_id = %subscription_id,
                                    ?routing,
                                    "history page released by a closed request"
                                );
                                // Released, not completed — the same rule as
                                // the walk above. The stream re-asks its own
                                // bound under a fresh registration, because no
                                // boundary can ever follow a CLOSED and a page
                                // left attached would stall this root in
                                // silence.
                                drive_root_reconstructions(ws, state).await;
                            }
                        }
                        // Registration closed; durable intent kept. An earlier
                        // version dropped the intent too, on the reasoning that
                        // re-asking a refused question would loop. That was
                        // wrong in the direction that matters: discovery intent
                        // derives from `project_routing_enabled`, so one CLOSED
                        // would have let the relay revoke a local configuration
                        // decision and keep it revoked across every later
                        // healthy connection.
                        //
                        // The loop is avoided by where the retry lives instead:
                        // the suspension excludes it from replay on this
                        // connection, and is cleared only when the connection
                        // is replaced.
                        warn!(
                            sub_id = %subscription_id,
                            "project subscription refused by relay — unanswerable until the next \
                             connection, intent retained: {message}"
                        );
                        return true;
                    }

                    // Project-shaped, but not a request that is live here. It
                    // may be stale, unsolicited, or invented. Ignore it rather
                    // than letting it fall through to the generic CLOSED
                    // handling below, which reads "restricted" as an auth
                    // failure and drops the socket — that would hand any peer
                    // able to name a `proj-` id a way to disconnect us.
                    if subscription_id.starts_with(crate::project::PROJECT_SUB_ID_PREFIX) {
                        debug!(
                            sub_id = %subscription_id,
                            "CLOSED for a project id with no live request — ignoring: {message}"
                        );
                        return true;
                    }

                    // A per-channel membership denial means THIS channel is
                    // forbidden, not the whole connection. Drop just this
                    // channel's subscription and keep the socket — otherwise the
                    // socket is torn down, the forbidden channel is resubscribed,
                    // and the same CLOSED arrives again: a tight reconnect loop.
                    if drop_channel_on_access_denied(state, &subscription_id, &message) {
                        return true;
                    }

                    // Rate-limited CLOSED — park and keep the socket. The relay's
                    // "retry in {N}s" hint arms the gate; the channel or control sub
                    // is resubscribed by the main-loop drain once the gate clears.
                    if message.starts_with("rate-limited:") {
                        let secs = parse_rate_limit_retry_secs(&message).unwrap_or(0);
                        let deadline = state.set_rate_limit_gate(secs);
                        warn!(
                            "subscription {subscription_id} rate-limited — parking until ~{:.1}s, gate armed",
                            deadline
                                .checked_duration_since(tokio::time::Instant::now())
                                .unwrap_or_default()
                                .as_secs_f64()
                        );
                        if let Some(channel_id) = channel_id_from_sub_id(&subscription_id) {
                            state.rate_limited_pending.insert(channel_id, deadline);
                        } else if subscription_id == MEMBERSHIP_NOTIF_SUB_ID {
                            // Mark membership sub for drain recovery. The relay rejected
                            // this REQ before registering it, so the sub does not exist
                            // server-side — the drain must re-send it.
                            state.membership_resub_needed = true;
                        } else if subscription_id == OBSERVER_CONTROL_SUB_ID {
                            state.observer_resub_needed = true;
                        } else if subscription_id == PEER_CALL_SUB_ID {
                            state.peer_call_resub_needed = true;
                        }
                        return true; // keep the socket
                    }

                    // CLOSED needs cleanup and resubscribe, not just logging.
                    let is_auth_error = message.starts_with("auth-required")
                        || message.starts_with("restricted")
                        || message.contains("auth");
                    warn!(
                        "subscription {subscription_id} closed by relay: {message}{}",
                        if is_auth_error {
                            " [auth error — reconnect required]"
                        } else {
                            ""
                        }
                    );

                    if is_auth_error {
                        // Auth errors require a full reconnect (re-handshake).
                        return false;
                    }

                    // Attempt targeted resubscribe. State is NOT cleared before
                    // the attempt — if the send fails and triggers reconnect,
                    // resubscribe_after_reconnect() needs the subscription to
                    // still be in state so it can restore it.
                    if subscription_id == OBSERVER_CONTROL_SUB_ID {
                        let sent = send_observer_control_subscribe(ws, agent_pubkey_hex).await;
                        if sent {
                            state.observer_control_sub_active = true;
                        } else {
                            warn!("observer control resubscribe failed after CLOSED — triggering reconnect");
                            return false;
                        }
                    } else if subscription_id == PEER_CALL_SUB_ID {
                        if send_peer_call_subscribe(ws, agent_pubkey_hex).await {
                            state.peer_call_resub_needed = false;
                        } else {
                            warn!(
                                "peer-call resubscribe failed after CLOSED — triggering reconnect"
                            );
                            return false;
                        }
                    } else if subscription_id == MEMBERSHIP_NOTIF_SUB_ID {
                        let since =
                            match (state.membership_dropped_since, state.membership_last_seen) {
                                (Some(d), Some(l)) => Some(d.min(l)),
                                (Some(d), None) => Some(d),
                                (None, Some(l)) => Some(l),
                                (None, None) => state.startup_watermark,
                            };
                        let sent = send_membership_subscribe(ws, agent_pubkey_hex, since).await;
                        if sent {
                            // Success — subscription is live again.
                            state.membership_dropped_since = None;
                        } else {
                            // Resubscribe failed — likely half-dead socket.
                            // Keep membership_sub_active = true so reconnect restores it.
                            warn!(
                                "membership resubscribe failed after CLOSED — triggering reconnect"
                            );
                            return false;
                        }
                    } else if let Some(channel_id) = channel_id_from_sub_id(&subscription_id) {
                        // Guard: only resubscribe if the channel is still active.
                        // A delayed CLOSED for an already-unsubscribed channel must
                        // NOT resurrect the subscription (especially with a default
                        // permissive filter, which would be a fail-open regression).
                        if !state.active_subscriptions.contains_key(&channel_id) {
                            debug!("ignoring CLOSED for already-unsubscribed channel {channel_id}");
                        } else {
                            let since = state.channel_since(&channel_id);
                            let filter = match state.active_filters.get(&channel_id).cloned() {
                                Some(f) => f,
                                None => {
                                    // Fail closed: missing filter state means the subscription
                                    // intent is inconsistent. Trigger reconnect rather than
                                    // resubscribing with a permissive wildcard.
                                    warn!("missing filter for channel {channel_id} after CLOSED — triggering reconnect (fail-closed)");
                                    return false;
                                }
                            };
                            let sent = send_subscribe(
                                ws,
                                state,
                                channel_id,
                                agent_pubkey_hex,
                                since,
                                &filter,
                            )
                            .await;
                            if sent {
                                // Success — update subscription ID (relay may assign new one).
                                state
                                    .active_subscriptions
                                    .insert(channel_id, channel_sub_id(channel_id));
                                state.channel_dropped_since.remove(&channel_id);
                            } else {
                                // Resubscribe failed — likely half-dead socket.
                                // Keep channel in active_subscriptions so reconnect restores it.
                                warn!("channel {channel_id} resubscribe failed after CLOSED — triggering reconnect");
                                return false;
                            }
                        } // end: channel is still active
                    } else {
                        warn!("CLOSED for unknown subscription {subscription_id} — ignoring");
                    }
                }
                RelayMessage::Auth { challenge } => {
                    // AUTH send failure must trigger reconnect.
                    debug!("received mid-session AUTH challenge — re-authenticating");
                    if let Err(e) =
                        send_auth_response(ws, &challenge, relay_url, keys, auth_tag).await
                    {
                        warn!("failed to respond to mid-session AUTH challenge: {e} — triggering reconnect");
                        return false;
                    }
                }
                RelayMessage::Ok {
                    event_id,
                    accepted,
                    message,
                } => {
                    if !accepted && message.starts_with("auth") {
                        // AUTH OK with accepted=false means auth was rejected.
                        warn!("mid-session AUTH rejected (event {event_id}): {message} — triggering reconnect");
                        return false;
                    }
                    state.acknowledge_observer_frame(&event_id);
                    debug!("OK for event {event_id}: accepted={accepted} message={message}");
                }
            }
            true
        }
        Message::Ping(data) => {
            if let Err(e) = ws_send_timeout(ws, Message::Pong(data), WS_SEND_TIMEOUT_SECS).await {
                warn!("failed to send pong: {e}");
                return false;
            }
            true
        }
        Message::Close(_) => {
            debug!("relay sent Close frame");
            false
        }
        // Binary, Pong, Frame — ignore
        _ => true,
    }
}

/// Install a replacement socket and hand it the frames it already buffered.
///
/// **The order is the point.** `do_connect` returns a live socket plus whatever
/// arrived on it during the handshake, and those frames are handled by the
/// ordinary dispatch — which authenticates a project frame against whatever is
/// live in the registry. Until the dead connection's registrations are gone,
/// "live" still means *its* registrations, so an `EOSE` buffered by the
/// replacement could mint a boundary for a request the replacement never sent
/// and complete a page opened on a socket that no longer exists. The same
/// window admits an `EVENT` into that page and lets a `CLOSED` record a refusal
/// of a request nobody asked this connection.
///
/// Retiring inside this function rather than at its two call sites is
/// deliberate: a rule that says "clear before you process" is only true while
/// every caller remembers, and there is no way to observe from outside that one
/// of them did not.
#[allow(clippy::too_many_arguments)]
async fn install_replacement_connection(
    ws: &mut WsStream,
    replacement: WsStream,
    handshake_buffer: ingress::HandshakeBuffer,
    event_tx: &mpsc::Sender<Option<BuzzEvent>>,
    observer_control_tx: &mpsc::Sender<Event>,
    state: &mut BgState,
    keys: &Keys,
    relay_url: &str,
    agent_pubkey_hex: &str,
    auth_tag: Option<&nostr::Tag>,
) -> bool {
    state.retire_project_connection();
    *ws = replacement;
    ingress::process_handshake_buffer(
        ws,
        handshake_buffer,
        event_tx,
        observer_control_tx,
        state,
        keys,
        relay_url,
        agent_pubkey_hex,
        auth_tag,
    )
    .await
}

/// Outcome of [`resubscribe_after_reconnect`].
enum ResubscribeResult {
    /// All subscriptions restored (or parked for drain recovery).
    Ok,
    /// A control subscription or deferred live command failed to send.
    /// Caller should retry the connection.
    RetryConnection,
    /// A `Shutdown` command arrived during a pacing sleep.
    /// Caller must return immediately (background task is exiting).
    Shutdown,
}

/// Resubscribe all active channels and membership notifications after a
/// successful reconnect. Computes `since = min(last_seen, channel_dropped_since)`
/// per channel, and only clears the drop tracker when the REQ is confirmed sent.
///
/// Paces REQs at `REQ_PACING_INTERVAL` (125 ms) via a shutdown-aware sleep so
/// a 48-channel reconnect burst spreads over ≈6 s. Commands received during a
/// pacing sleep are deferred in arrival order and executed on the live socket
/// after replay. If the gate is active mid-burst, remaining channels are parked
/// in `rate_limited_pending` instead of sent.
///
/// A failed CHANNEL REQ is parked in `resubscribe_retry` rather than failing
/// the whole reconnect. Only membership, observer-control, or deferred-command
/// failures return `RetryConnection` — their silent loss would leave live state
/// inconsistent with command intent.
///
/// A relay quota gate is keyed by community and pubkey, so replacing the socket
/// does not reset it. Fresh connections may clear derived pending/retry queues
/// before rebuilding them from `active_subscriptions`, but the gate itself is
/// always preserved until its deadline expires.
///
/// Returns [`ResubscribeResult`] signalling success, retry, or shutdown.
async fn resubscribe_after_reconnect(
    ws: &mut WsStream,
    cmd_rx: &mut mpsc::Receiver<RelayCommand>,
    state: &mut BgState,
    agent_pubkey_hex: &str,
    is_fresh_connection: bool,
) -> ResubscribeResult {
    if is_fresh_connection {
        // These queues are derived from active subscription intent and rebuilt
        // below. The rate-limit gate is deliberately preserved: the relay's
        // shared admission counter survives socket replacement.
        state.rate_limited_pending.clear();
        state.resubscribe_retry.clear();

        // The project registrations and pages that belonged to the dead socket
        // are **not** retired here. They are retired by
        // `install_replacement_connection`, before the replacement's own
        // handshake buffer is handled — which is strictly earlier than this
        // function runs. Doing it here as well would be harmless and would also
        // be the second place that has to agree with the first.
    }

    // Re-ask for what local policy wants, registering only as each REQ is
    // successfully written.
    //
    // `replayable()` excludes requests this connection has already refused, so
    // a *proactive* resubscribe on the existing socket does not quietly retry
    // something the relay just said no to. A fresh connection has had its
    // suspensions cleared, so everything intended is offered once.
    //
    // Discovery, enrolment and watched intent all replay through here, and
    // catch-up pages will too. This loop still lacks the pacing and
    // shutdown-awareness the channel burst above has — that gap is real and
    // unaddressed, and it grew when the second and third request classes
    // arrived. An earlier revision of this comment said only discovery existed,
    // which stopped being true when enrolment landed.
    // A record the registry cannot validate replays nothing at all. This is
    // where durable intent becomes bytes, so a non-canonical entry admitted
    // here would be a REQ this agent never wrote — and dropping only the bad
    // entry would install the rest against a record already known to be
    // inconsistent. The connection is healthy either way; what is broken is
    // local, and a reconnect cannot repair it.
    let replayable = match state.project_requests.replayable() {
        Ok(replayable) => replayable,
        Err(violation) => {
            error!(
                violation,
                "project durable intent is inconsistent — replaying no project request on this \
                 connection"
            );
            Vec::new()
        }
    };
    for request in replayable {
        let sub_id = request.sub_id().to_string();
        match send_project_replay(ws, state, request).await {
            ProjectSendOutcome::Sent | ProjectSendOutcome::AlreadyOpen => {}
            ProjectSendOutcome::UnboundedFilters => {
                // Unreachable: an identity only exists because its filters were
                // bounded when it was minted. Reported rather than ignored,
                // because a silent arm here would hide the two rules drifting.
                error!(
                    sub_id,
                    "project resubscribe refused its own recorded filters as unbounded — \
                     internal invariant failure"
                );
            }
            ProjectSendOutcome::WriteFailed => {
                // Do not carry on and report a healthy connection. Intent is
                // retained but inactive, and there may be no later reconnect to
                // notice — project routing would be silently dead on a
                // connection everything else considers fine.
                //
                // No deferred commands have been collected at this point in the
                // sequence, so there is no command intent to retain.
                warn!(
                    sub_id,
                    "project resubscribe write failed — retrying the connection"
                );
                return ResubscribeResult::RetryConnection;
            }
            ProjectSendOutcome::MetadataConflict => {
                warn!(
                    sub_id,
                    "project resubscribe hit a request-ownership conflict — internal invariant \
                     failure, original authority retained"
                );
            }
            ProjectSendOutcome::InvariantViolation => {
                // Unreachable from here: `replayable` validated the whole
                // record to mint these tokens, and nothing between that call
                // and this one writes to it. Reported rather than ignored,
                // because a silent arm would hide the two walks disagreeing.
                error!(
                    sub_id,
                    "project resubscribe refused a token it had just validated — internal \
                     invariant failure"
                );
            }
            ProjectSendOutcome::Exhausted => {
                // Not a conflict and not a dead socket. Reporting it as either
                // would send someone looking for a disagreement or a network
                // fault that does not exist. The connection is healthy; this
                // process simply cannot open project requests any more.
                error!(
                    sub_id,
                    "project resubscribe refused — incarnation space exhausted; project routing \
                     is permanently degraded for this process"
                );
            }
        }
    }

    // The walk survives the connection: its cursor and cutoff are ours, and
    // only the page that was in flight belonged to the socket that died. It
    // resumes from the bound it had reached, under a fresh registration.
    //
    // It is deliberately *not* replayed from durable intent. A history page's
    // filter carries a bound that moves, so a recorded one would re-ask for a
    // page the cursor has already walked past — which is why the registry
    // refuses to hold this class as intent at all.
    drive_enrolment_history(ws, state).await;

    // Every root reconstruction survives for the same reason and resumes the
    // same way: `disconnected` dropped only the pages the dead socket owned,
    // and the cursors say where each stream had got to. A root left unresumed
    // here would sit finished-looking forever with a history it never proved.
    drive_root_reconstructions(ws, state).await;

    let mut deferred_commands = VecDeque::new();
    let channels: Vec<Uuid> = state.active_subscriptions.keys().copied().collect();
    if !channels.is_empty() {
        info!(
            "resubscribing to {} channel(s) after reconnect",
            channels.len()
        );
        for channel_id in channels {
            // Gate re-armed mid-burst — park remaining channels.
            if let Some(retry_after) = state.check_rate_gate() {
                debug!(
                    "rate-gated mid-resubscribe: parking channel {channel_id} in rate_limited_pending"
                );
                state.rate_limited_pending.insert(channel_id, retry_after);
                continue;
            }

            let since = state.channel_since(&channel_id);
            let filter = match state.active_filters.get(&channel_id).cloned() {
                Some(f) => f,
                None => {
                    // Fail closed: missing filter state means the subscription
                    // intent is inconsistent. Skip rather than resubscribe with
                    // a permissive wildcard that would widen the subscription.
                    warn!("missing filter for channel {channel_id} — skipping resubscribe (fail-closed)");
                    state.resubscribe_retry.insert(channel_id);
                    continue;
                }
            };
            let this_sent =
                send_subscribe(ws, state, channel_id, agent_pubkey_hex, since, &filter).await;
            if this_sent {
                state.channel_dropped_since.remove(&channel_id);
                // Shutdown-aware pacing sleep before any next replay/deferred REQ.
                if !pacing_sleep(cmd_rx, &mut deferred_commands, REQ_PACING_INTERVAL).await {
                    return ResubscribeResult::Shutdown;
                }
            } else {
                // Partial failure — park the channel for main-loop retry instead
                // of aborting the entire reconnect.
                warn!(
                    "failed to resubscribe channel {channel_id} after reconnect — parking for retry"
                );
                state.resubscribe_retry.insert(channel_id);
            }
        }
    }

    // Membership and observer-control are control-plane subscriptions: a silent
    // failure breaks join notifications and agent pause/resume. A shared quota
    // gate parks their intent for the main-loop drain just like channel REQs.
    if state.membership_sub_active {
        if state.check_rate_gate().is_some() {
            debug!("rate-gated: parking membership resubscribe after reconnect");
            state.membership_resub_needed = true;
        } else {
            if !state.active_subscriptions.is_empty()
                && !pacing_sleep(cmd_rx, &mut deferred_commands, REQ_PACING_INTERVAL).await
            {
                return ResubscribeResult::Shutdown;
            }
            let replay_since = match (state.membership_dropped_since, state.membership_last_seen) {
                (Some(d), Some(l)) => Some(d.min(l)),
                (Some(d), None) => Some(d),
                (None, Some(l)) => Some(l),
                (None, None) => state.startup_watermark,
            };
            let sent = send_membership_subscribe(ws, agent_pubkey_hex, replay_since).await;
            if sent {
                state.membership_dropped_since = None;
                state.membership_resub_needed = false;
            } else {
                warn!("failed to resubscribe membership after reconnect");
                retain_deferred_command_intent(state, &mut deferred_commands);
                return ResubscribeResult::RetryConnection;
            }
        }
    }

    if state.observer_control_sub_active {
        if state.check_rate_gate().is_some() {
            debug!("rate-gated: parking observer control resubscribe after reconnect");
            state.observer_resub_needed = true;
        } else {
            if !pacing_sleep(cmd_rx, &mut deferred_commands, REQ_PACING_INTERVAL).await {
                return ResubscribeResult::Shutdown;
            }
            if !send_observer_control_subscribe(ws, agent_pubkey_hex).await {
                warn!("failed to resubscribe observer controls after reconnect");
                retain_deferred_command_intent(state, &mut deferred_commands);
                return ResubscribeResult::RetryConnection;
            }
            state.observer_resub_needed = false;
        }
    }

    if state.peer_call_sub_active {
        if state.check_rate_gate().is_some() {
            debug!("rate-gated: parking peer-call resubscribe after reconnect");
            state.peer_call_resub_needed = true;
        } else {
            if !pacing_sleep(cmd_rx, &mut deferred_commands, REQ_PACING_INTERVAL).await {
                return ResubscribeResult::Shutdown;
            }
            if !send_peer_call_subscribe(ws, agent_pubkey_hex).await {
                warn!("failed to resubscribe peer calls after reconnect");
                retain_deferred_command_intent(state, &mut deferred_commands);
                return ResubscribeResult::RetryConnection;
            }
            state.peer_call_resub_needed = false;
        }
    }

    match drain_commands(ws, cmd_rx, &mut deferred_commands, state, agent_pubkey_hex).await {
        ReconnectOutcome::Ok => ResubscribeResult::Ok,
        ReconnectOutcome::Failed => ResubscribeResult::RetryConnection,
        ReconnectOutcome::Shutdown => ResubscribeResult::Shutdown,
    }
}

/// Send a signed EVENT frame on the live socket. Returns `false` on send failure.
///
/// Best-effort at the socket level: a failure is logged but does not trigger
/// reconnect — the next ping or read will detect the dead socket.
async fn send_publish_event_frame(ws: &mut WsStream, event: &Event) -> bool {
    let msg = json!(["EVENT", event]);
    if let Ok(text) = serde_json::to_string(&msg) {
        if let Err(e) = ws_send_timeout(ws, Message::Text(text.into()), WS_SEND_TIMEOUT_SECS).await
        {
            warn!("failed to publish event: {e}");
            return false;
        }
    }
    true
}

/// Drain parked durable frames once the rate-limit gate clears.
///
/// Called by the main loop pacing timer. Sends at most `budget` frames without
/// sleeping — pacing is enforced by the caller via `drain_pacing_next`. Stops
/// immediately if the gate re-arms mid-drain. When the queue empties, any
/// overflow loss is summarized in one warning. Returns the number of frames sent.
async fn drain_gated_observer_pending(
    ws: &mut WsStream,
    state: &mut BgState,
    budget: usize,
) -> usize {
    let mut sent = 0;
    while sent < budget {
        if state.check_rate_gate().is_some() {
            break;
        }
        let Some(event) = state.gated_observer_pending.pop_front() else {
            break;
        };
        if !send_publish_event_frame(ws, &event).await {
            // Socket may be dead — re-park at the front so the frame survives
            // reconnect (the post-reconnect drain will retry it in order).
            state.gated_observer_pending.push_front(event);
            break;
        }
        // Only observer frames enter the acknowledgment window. It exists
        // because a rate-limit NOTICE names no event id, so every unacknowledged
        // *telemetry* write is re-sent conservatively — and a duplicate 24200 is
        // free, since the observer stream is deduplicated by event id. A
        // non-telemetry durable frame is left alone rather than given a retry
        // policy nothing has asked for.
        if event.kind.as_u16() as u32 == KIND_AGENT_OBSERVER_FRAME {
            state.track_observer_in_flight(event);
        }
        sent += 1;
    }
    if state.gated_observer_pending.is_empty() && state.gated_observer_dropped > 0 {
        warn!(
            observer_frames_dropped = state.gated_observer_dropped,
            "observer frames lost to gated-queue overflow"
        );
        state.gated_observer_dropped = 0;
    }
    sent
}

/// Drain the parked latest-per-scope ephemeral frames once the gate clears.
///
/// Runs after [`drain_gated_observer_pending`] on the same pacing tick and
/// shares its budget, so a status frame costs the durable queue nothing and
/// both stay inside the relay's admission window.
///
/// A send failure drops the frame instead of re-parking it: the socket is
/// probably gone, and the publisher behind every kind here re-announces within
/// one cadence, so retrying would only risk delivering something older than
/// what is about to be published anyway. Returns the number of frames sent.
async fn drain_gated_ephemeral_pending(
    ws: &mut WsStream,
    state: &mut BgState,
    budget: usize,
) -> usize {
    let mut sent = 0;
    while sent < budget {
        if state.check_rate_gate().is_some() {
            break;
        }
        let Some((scope, event)) = state.gated_ephemeral_pending.pop_front() else {
            break;
        };
        if !send_publish_event_frame(ws, &event).await {
            warn!(
                kind = scope.kind,
                scope = %scope.id,
                "parked ephemeral frame dropped: send failed — its publisher re-announces"
            );
            break;
        }
        sent += 1;
    }
    if state.gated_ephemeral_pending.is_empty() && state.gated_ephemeral_dropped > 0 {
        warn!(
            ephemeral_scopes_dropped = state.gated_ephemeral_dropped,
            "status frames lost to gated ephemeral scope-cap overflow"
        );
        state.gated_ephemeral_dropped = 0;
    }
    sent
}

/// Drain `rate_limited_pending` channels whose retry deadline has passed.
///
/// Called by the main loop pacing timer. Sends at most `budget` REQs without
/// sleeping — pacing is enforced by the caller via `drain_pacing_next`. A
/// failed send re-queues the channel with a +5 s penalty. Returns the number
/// of REQs successfully sent.
async fn drain_rate_limited_pending(
    ws: &mut WsStream,
    state: &mut BgState,
    agent_pubkey_hex: &str,
    budget: usize,
) -> usize {
    let now = tokio::time::Instant::now();
    let ready: Vec<Uuid> = state
        .rate_limited_pending
        .iter()
        .filter(|(_, &deadline)| now >= deadline)
        .map(|(&ch, _)| ch)
        .take(budget)
        .collect();

    if ready.is_empty() {
        return 0;
    }
    debug!("draining {} rate_limited_pending channel(s)", ready.len());

    let mut sent_count = 0;
    for channel_id in ready {
        // Re-check gate each iteration — a new CLOSED may have re-armed it mid-drain.
        if let Some(retry_after) = state.check_rate_gate() {
            state.rate_limited_pending.insert(channel_id, retry_after);
            continue;
        }

        let since = state.channel_since(&channel_id);
        let filter = match state.active_filters.get(&channel_id).cloned() {
            Some(f) => f,
            None => {
                warn!("missing filter for channel {channel_id} in rate_limited_pending — dropping");
                state.rate_limited_pending.remove(&channel_id);
                continue;
            }
        };
        let sent = send_subscribe(ws, state, channel_id, agent_pubkey_hex, since, &filter).await;
        if sent {
            state.rate_limited_pending.remove(&channel_id);
            state.channel_dropped_since.remove(&channel_id);
            sent_count += 1;
            // Pacing is enforced by the main-loop timer; no inline sleep here.
        } else {
            // Socket may be dead — re-queue with +5s penalty; the next ws event
            // will detect the dead socket and trigger a full reconnect.
            let penalty = tokio::time::Instant::now() + Duration::from_secs(5);
            state.rate_limited_pending.insert(channel_id, penalty);
            warn!("drain_rate_limited_pending: REQ failed for channel {channel_id} — re-queued with +5s penalty");
        }
    }
    sent_count
}

/// Drain `resubscribe_retry` channels that were parked by partial reconnect failure.
///
/// Called by the main loop pacing timer. Sends at most `budget` REQs without
/// sleeping — pacing is enforced by the caller. A failed send leaves the
/// channel in the retry set; a gate re-armed mid-drain moves it to
/// `rate_limited_pending`. Returns the number of REQs successfully sent.
async fn drain_resubscribe_retry(
    ws: &mut WsStream,
    state: &mut BgState,
    agent_pubkey_hex: &str,
    budget: usize,
) -> usize {
    if state.resubscribe_retry.is_empty() {
        return 0;
    }
    // Budget-bounded take avoids cloning the full set.
    let channels: Vec<Uuid> = state
        .resubscribe_retry
        .iter()
        .copied()
        .take(budget)
        .collect();
    debug!("draining {} resubscribe_retry channel(s)", channels.len());
    let mut sent_count = 0;
    for channel_id in channels {
        if let Some(retry_after) = state.check_rate_gate() {
            // Gate re-armed mid-drain — move to rate_limited_pending.
            state.rate_limited_pending.insert(channel_id, retry_after);
            state.resubscribe_retry.remove(&channel_id);
            continue;
        }
        let since = state.channel_since(&channel_id);
        let filter = match state.active_filters.get(&channel_id).cloned() {
            Some(f) => f,
            None => {
                warn!("missing filter for channel {channel_id} in resubscribe_retry — dropping");
                state.resubscribe_retry.remove(&channel_id);
                continue;
            }
        };
        let sent = send_subscribe(ws, state, channel_id, agent_pubkey_hex, since, &filter).await;
        if sent {
            state.resubscribe_retry.remove(&channel_id);
            state.channel_dropped_since.remove(&channel_id);
            sent_count += 1;
            // Pacing is enforced by the main-loop timer; no inline sleep here.
        } else {
            warn!(
                "drain_resubscribe_retry: REQ still failing for channel {channel_id} — will retry"
            );
            // Leave in resubscribe_retry; next main-loop tick will try again.
        }
    }
    sent_count
}

/// Outcome of an autonomous reconnect attempt.
enum ReconnectOutcome {
    /// Reconnected and resubscribed successfully.
    Ok,
    /// Reconnect or resubscription attempts failed; caller should retry or fall
    /// back to `wait_for_reconnect`. Live command intent is retained.
    Failed,
    /// A Shutdown command was received during backoff — caller must return immediately.
    Shutdown,
}

/// Execute commands deferred during paced replay, then commands that arrived
/// while the deferred queue was draining. FIFO order is preserved across both
/// sources. Subscription REQs are paced; CLOSE, ephemeral EVENT, and local-state
/// commands execute immediately. A failed live send records remaining command
/// intent and returns `Failed`; Shutdown closes the socket immediately.
async fn drain_commands(
    ws: &mut WsStream,
    cmd_rx: &mut mpsc::Receiver<RelayCommand>,
    deferred_commands: &mut VecDeque<RelayCommand>,
    state: &mut BgState,
    agent_pubkey_hex: &str,
) -> ReconnectOutcome {
    let mut send_failed = false;
    loop {
        let cmd = match deferred_commands.pop_front() {
            Some(cmd) => cmd,
            None => match cmd_rx.try_recv() {
                Ok(cmd) => cmd,
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    return ReconnectOutcome::Shutdown;
                }
            },
        };

        if send_failed {
            match cmd {
                RelayCommand::Shutdown => {
                    let _ = ws_send_timeout(ws, Message::Close(None), WS_SEND_TIMEOUT_SECS).await;
                    return ReconnectOutcome::Shutdown;
                }
                RelayCommand::Reconnect => {}
                cmd => retain_failed_command_intent(state, cmd),
            }
            continue;
        }

        match cmd {
            RelayCommand::Reconnect => {
                debug!("drained stale Reconnect after reconnect");
            }
            RelayCommand::Shutdown => {
                debug!("shutdown received during post-reconnect drain");
                let _ = ws_send_timeout(ws, Message::Close(None), WS_SEND_TIMEOUT_SECS).await;
                return ReconnectOutcome::Shutdown;
            }
            RelayCommand::Subscribe { .. }
            | RelayCommand::SubscribeMembership
            | RelayCommand::SubscribeObserverControls
            | RelayCommand::SubscribePeerCalls => {
                // A gated subscription is only parked in state; pace only an
                // actual live send attempt.
                let pace_after = state.check_rate_gate().is_none();
                if !execute_connected_command(ws, state, agent_pubkey_hex, cmd).await {
                    warn!("send failed during post-reconnect drain — recording remaining commands as intent");
                    send_failed = true;
                }
                if !send_failed
                    && pace_after
                    && !pacing_sleep(cmd_rx, deferred_commands, REQ_PACING_INTERVAL).await
                {
                    return ReconnectOutcome::Shutdown;
                }
            }
            cmd => {
                if !execute_connected_command(ws, state, agent_pubkey_hex, cmd).await {
                    warn!("send failed during post-reconnect drain — recording remaining commands as intent");
                    send_failed = true;
                }
            }
        }
    }

    if send_failed {
        ReconnectOutcome::Failed
    } else {
        ReconnectOutcome::Ok
    }
}

/// Drain all pending commands after a successful reconnect.
///
/// Processes queued commands that arrived while reconnecting. Reconnect
/// commands are silently dropped (already reconnected). Shutdown causes an
/// immediate close-frame + return of `ReconnectOutcome::Shutdown`. All other
/// commands are executed on the live socket via [`execute_connected_command`].
/// If any subscription send fails, remaining commands are recorded as intent
/// and `Failed` is returned so the caller can reconnect.
async fn drain_post_reconnect(
    ws: &mut WsStream,
    cmd_rx: &mut mpsc::Receiver<RelayCommand>,
    state: &mut BgState,
    agent_pubkey_hex: &str,
) -> ReconnectOutcome {
    drain_commands(ws, cmd_rx, &mut VecDeque::new(), state, agent_pubkey_hex).await
}

/// Attempt autonomous reconnect on socket loss.
///
/// Returns [`ReconnectOutcome::Ok`] on success, [`ReconnectOutcome::Failed`]
/// if all attempts are exhausted, or [`ReconnectOutcome::Shutdown`] if a
/// Shutdown command was received during backoff sleep. Callers MUST check
/// for `Shutdown` and return immediately — do NOT fall through to
/// `wait_for_reconnect`, which would loop forever since the Shutdown command
/// was already consumed.
#[allow(clippy::too_many_arguments)]
async fn try_autonomous_reconnect(
    ws: &mut WsStream,
    cmd_rx: &mut mpsc::Receiver<RelayCommand>,
    state: &mut BgState,
    keys: &Keys,
    relay_url: &str,
    agent_pubkey_hex: &str,
    event_tx: &mpsc::Sender<Option<BuzzEvent>>,
    observer_control_tx: &mpsc::Sender<Event>,
    auth_tag: Option<&nostr::Tag>,
) -> ReconnectOutcome {
    state.requeue_observer_in_flight();
    state.discard_gated_ephemera("autonomous reconnect");
    // 5 attempts, up to 16s base backoff. Shares delay values with the
    // initial-connect retry in `HarnessRelay::connect()` (STARTUP_CONNECT_BACKOFFS) —
    // see its doc comment for how the two loops consume the array differently.
    // DNS failures sleep flat (DNS_RETRY_INTERVAL) without consuming a ladder
    // rung. Capped at 10 DNS-only retries in this bounded startup path so a
    // total brownout cannot hang agent startup indefinitely. By contrast,
    // `wait_for_reconnect` (the post-startup loop) retries DNS failures without
    // a cap — a reconnecting agent should keep trying across extended outages.
    let backoffs = STARTUP_CONNECT_BACKOFFS;
    const MAX_DNS_FLAT_RETRIES: usize = 10;
    let mut dns_retry_count = 0usize;

    let mut attempt = 0usize;
    while attempt < backoffs.len() {
        info!(
            "autonomous reconnect attempt {}/{} to {relay_url}…",
            attempt + 1,
            backoffs.len()
        );
        match do_connect(relay_url, keys, auth_tag).await {
            Ok((new_ws, handshake_buffer)) => {
                info!("autonomous reconnect succeeded (attempt {})", attempt + 1);
                let handshake_ok = install_replacement_connection(
                    ws,
                    new_ws,
                    handshake_buffer,
                    event_tx,
                    observer_control_tx,
                    state,
                    keys,
                    relay_url,
                    agent_pubkey_hex,
                    auth_tag,
                )
                .await;
                if !handshake_ok {
                    warn!(
                        "handshake buffer drop signal after autonomous reconnect (attempt {})",
                        attempt + 1
                    );
                    // Fall through to backoff sleep instead of returning immediately.
                    // Returning false here would skip remaining attempts; continuing
                    // without sleep would drive a tight reconnect storm.
                } else {
                    match resubscribe_after_reconnect(ws, cmd_rx, state, agent_pubkey_hex, true)
                        .await
                    {
                        ResubscribeResult::Ok => return ReconnectOutcome::Ok,
                        ResubscribeResult::Shutdown => return ReconnectOutcome::Shutdown,
                        ResubscribeResult::RetryConnection => {
                            warn!("resubscribe failed after autonomous reconnect — treating as failed attempt");
                            // Fall through to backoff sleep and retry.
                        }
                    }
                }
            }
            // DNS failures retry flat without consuming a ladder rung.
            // Cap at MAX_DNS_FLAT_RETRIES so a total brownout doesn't hang startup.
            Err(e) if is_dns_error(&e) && dns_retry_count < MAX_DNS_FLAT_RETRIES => {
                dns_retry_count += 1;
                warn!(
                    "autonomous reconnect DNS failure ({}/{}), flat retry in {:.1}s: {e}",
                    dns_retry_count,
                    MAX_DNS_FLAT_RETRIES,
                    DNS_RETRY_INTERVAL.as_secs_f64()
                );
                if !dns_flat_sleep(cmd_rx, state, DNS_RETRY_INTERVAL).await {
                    return ReconnectOutcome::Shutdown;
                }
                continue; // retry WITHOUT incrementing attempt
            }
            Err(e) => {
                warn!("autonomous reconnect attempt {} failed: {e}", attempt + 1);
            }
        }

        // Backoff sleep between ladder attempts (shared by handshake-drop and connect-error).
        // Skip sleep on the final attempt — we'll fall through to the caller.
        // Use select! so Shutdown commands are honoured during sleep.
        if attempt + 1 < backoffs.len() {
            let jittered = jittered_duration(backoffs[attempt]);
            tracing::info!(
                "retrying autonomous reconnect in {:.1}s",
                jittered.as_secs_f64()
            );
            // Deadline-based sleep: commands processed during the wait don't
            // reset the timer (prevents PublishEvent traffic from collapsing backoff).
            let deadline = tokio::time::Instant::now() + jittered;
            let sleep = tokio::time::sleep_until(deadline);
            tokio::pin!(sleep);
            loop {
                tokio::select! {
                    _ = &mut sleep => break,
                    cmd = cmd_rx.recv() => {
                        match cmd {
                            Some(RelayCommand::Shutdown) | None => return ReconnectOutcome::Shutdown,
                            Some(cmd) => apply_command_to_state(state, cmd),
                        }
                    }
                }
            }
        }
        attempt += 1;
    }

    ReconnectOutcome::Failed
}

/// Attempt reconnection with exponential backoff. Resubscribes all active
/// channels with `since` filters on success.
///
/// If `skip_drain` is `false`, drains the command channel until a `Reconnect`
/// command arrives (used when called from the WS-error path where the caller
/// hasn't sent Reconnect yet). If `true`, skips the drain and reconnects
/// immediately (used when called from the `RelayCommand::Reconnect` arm where
/// the command was already consumed).
#[allow(clippy::too_many_arguments)]
async fn wait_for_reconnect(
    ws: &mut WsStream,
    cmd_rx: &mut mpsc::Receiver<RelayCommand>,
    state: &mut BgState,
    keys: &Keys,
    relay_url: &str,
    agent_pubkey_hex: &str,
    event_tx: &mpsc::Sender<Option<BuzzEvent>>,
    observer_control_tx: &mpsc::Sender<Event>,
    skip_drain: bool,
    auth_tag: Option<&nostr::Tag>,
) -> ReconnectOutcome {
    state.requeue_observer_in_flight();
    state.discard_gated_ephemera("waiting for reconnect");
    if !skip_drain {
        // Drain commands until we get Reconnect (or Shutdown).
        // Other commands update state so reconnect reflects latest intent.
        loop {
            match cmd_rx.recv().await {
                Some(RelayCommand::Reconnect) => break,
                Some(RelayCommand::Shutdown) | None => return ReconnectOutcome::Shutdown,
                Some(cmd) => apply_command_to_state(state, cmd),
            }
        }
    }

    // 6 attempts with backoff up to 32s + jitter; uses tokio::select! so shutdown is
    // honoured during sleep. Resumes from state.backoff_step so a flapping link
    // keeps its elevated position; the stability block resets it to 0 after 60s.
    // DNS failures retry flat without consuming a ladder rung.
    let backoffs = [
        Duration::from_secs(1),
        Duration::from_secs(2),
        Duration::from_secs(4),
        Duration::from_secs(8),
        Duration::from_secs(16),
        Duration::from_secs(32),
    ];
    let mut attempt = state.backoff_step;
    loop {
        info!("attempting relay reconnect to {relay_url}…");
        match do_connect(relay_url, keys, auth_tag).await {
            Ok((new_ws, handshake_buffer)) => {
                info!("relay reconnected to {relay_url}");
                let handshake_ok = install_replacement_connection(
                    ws,
                    new_ws,
                    handshake_buffer,
                    event_tx,
                    observer_control_tx,
                    state,
                    keys,
                    relay_url,
                    agent_pubkey_hex,
                    auth_tag,
                )
                .await;
                if !handshake_ok {
                    warn!("handshake buffer contained a drop signal after reconnect — will retry with backoff");
                    // Fall through to the backoff sleep below instead of
                    // tight-looping. A relay that consistently fails the
                    // handshake would otherwise drive a reconnect storm.
                } else {
                    match resubscribe_after_reconnect(ws, cmd_rx, state, agent_pubkey_hex, true)
                        .await
                    {
                        ResubscribeResult::Ok => {
                            // Drain any commands that arrived during do_connect() +
                            // resubscribe (which don't poll cmd_rx).
                            return drain_post_reconnect(ws, cmd_rx, state, agent_pubkey_hex).await;
                        }
                        ResubscribeResult::Shutdown => return ReconnectOutcome::Shutdown,
                        ResubscribeResult::RetryConnection => {
                            warn!("resubscribe failed after reconnect — will retry with backoff");
                            // Fall through to backoff sleep.
                        }
                    }
                }
            }
            // DNS failures retry on a flat interval without consuming a backoff
            // ladder rung — the host is temporarily unresolvable, not persistently
            // rejecting us, so exponential back-off is counter-productive.
            // This loop is unbounded (unlike the 10-retry cap in `try_autonomous_reconnect`)
            // so a reconnecting agent keeps trying across extended DNS brownouts.
            Err(e) if is_dns_error(&e) => {
                warn!("relay reconnect DNS failure (not consuming ladder rung): {e}");
                if !dns_flat_sleep(cmd_rx, state, DNS_RETRY_INTERVAL).await {
                    return ReconnectOutcome::Shutdown;
                }
                continue; // retry without incrementing attempt
            }
            Err(e) => {
                warn!("relay reconnect failed: {e}");
            }
        }

        // Persist ladder position before sleeping — if shutdown arrives mid-sleep,
        // the next session resumes from here rather than restarting at 0.
        state.backoff_step = attempt;

        // Backoff sleep — shared by both handshake-drop and connect-error paths.
        // Uses a deadline so commands processed during the wait don't reset
        // the timer. Without this, periodic PublishEvent traffic (typing
        // refresh every 3s) would collapse the jittered backoff into a
        // reconnect storm.
        let delay = if attempt < backoffs.len() {
            backoffs[attempt]
        } else {
            Duration::from_secs(60)
        };
        let jittered = jittered_duration(delay);
        warn!("retrying reconnect in {:.1}s", jittered.as_secs_f64());
        let deadline = tokio::time::Instant::now() + jittered;
        let sleep = tokio::time::sleep_until(deadline);
        tokio::pin!(sleep);
        loop {
            tokio::select! {
                _ = &mut sleep => break,
                cmd = cmd_rx.recv() => {
                    match cmd {
                        Some(RelayCommand::Shutdown) | None => return ReconnectOutcome::Shutdown,
                        Some(cmd) => apply_command_to_state(state, cmd),
                    }
                }
            }
        }
        attempt += 1;
    }
}

/// Send a NIP-01 REQ for a channel, built from a [`ChannelFilter`].
///
/// - `kinds` is included only when `filter.kinds` is `Some`; `None` = wildcard.
/// - `#p` is included only when `filter.require_mention` is `true`.
/// - `#h` is always included (channel-scoped subscription).
/// - On first subscribe (`since` is `None`) adds `since=now` to avoid replaying
///   history. On reconnect (`since` is `Some`) subtracts [`SINCE_SKEW_SECS`].
///
/// Returns `true` if the REQ was successfully written to the WebSocket.
async fn send_subscribe(
    ws: &mut WsStream,
    _state: &BgState,
    channel_id: Uuid,
    agent_pubkey_hex: &str,
    since: Option<u64>,
    filter: &ChannelFilter,
) -> bool {
    let sub_id = channel_sub_id(channel_id);

    let mut req_filter = serde_json::Map::new();

    // kinds — omit entirely for wildcard subscriptions.
    if let Some(ref kinds) = filter.kinds {
        req_filter.insert("kinds".into(), json!(kinds));
    }

    // #h — always present (channel scope).
    req_filter.insert("#h".into(), json!([channel_id.to_string()]));

    // #p — only when require_mention is true.
    if filter.require_mention {
        req_filter.insert("#p".into(), json!([agent_pubkey_hex]));
    }

    // since — on first subscribe use current time to skip history; on reconnect
    // subtract skew buffer to catch events missed during the disconnect window.
    let since_ts = match since {
        Some(ts) => ts.saturating_sub(SINCE_SKEW_SECS),
        None => std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
    };
    req_filter.insert("since".into(), json!(since_ts));

    let req = json!(["REQ", sub_id, Value::Object(req_filter)]);

    match serde_json::to_string(&req) {
        Ok(text) => {
            match ws_send_timeout(ws, Message::Text(text.into()), WS_SEND_TIMEOUT_SECS).await {
                Ok(()) => {
                    debug!(
                        "subscribed to channel {channel_id}{}",
                        if since.is_some() {
                            " (with since filter)"
                        } else {
                            " (since=now)"
                        }
                    );
                    true
                }
                Err(e) => {
                    warn!("failed to send REQ for channel {channel_id}: {e}");
                    false
                }
            }
        }
        Err(e) => {
            warn!("failed to serialize REQ for channel {channel_id}: {e}");
            false
        }
    }
}

/// Send a project REQ and register it in the same step.
///
/// **Preflight, write, then install.** The registry is what makes an inbound
/// project frame admissible, so nothing may be registered until the REQ has
/// actually reached the socket. `open_request` decides admissibility first,
/// performs the concrete write, and only then installs an already-live
/// registration.
///
/// An earlier version reserved first and rolled back on failure, arguing that
/// send-then-register could drop the first frames of a subscription we really
/// did open. That argument was wrong in a way worth recording, because it is
/// the tempting one: an async write has a third exit besides `Ok` and `Err` —
/// it can be **dropped while suspended**. A pre-await reservation therefore
/// survives cancellation with nothing able to promote or remove it, holding
/// the subscription id hostage so that root silently never reconstructs.
///
/// The frame-loss risk it was guarding against is not real here: this task
/// owns the socket and the registry together and holds `&mut` across both, so
/// no inbound frame is processed between the write returning and the
/// registration being installed.
///
/// Outcomes are obeyed, not just logged:
///
/// - `Sent` — the REQ is on the wire and the registration is live;
/// - `AlreadyLive` — this exact request is live; **do not** send another REQ
///   under its id. A second REQ could replace the relay's subscription while
///   leaving the old request's EOSE indistinguishable from the new one's;
/// - `Conflict` — refuse, having recorded nothing at all, and say what is
///   actually live;
/// - `Exhausted` — refuse permanently. The incarnation space is spent, so no
///   project request can be opened again by this process;
/// - `WriteFailed` — nothing was registered. Durable intent survives, because
///   the intent is still what we want; the write is what failed.
///
/// Neither sender builds an identity or names an id. Discovery submits its
/// filters and the registry stamps the rest; replay hands back a token the
/// registry minted from its own validated record. The registry serialises the
/// REQ from the registration it installs, so the bytes on the wire cannot
/// differ from the question that was registered.
async fn send_project_discovery(
    ws: &mut WsStream,
    state: &mut BgState,
    filters: Vec<serde_json::Value>,
) -> ProjectSendOutcome {
    // The registry performs the write, against the socket itself. It is handed
    // the live `WsStream` rather than a closure: a closure returning
    // `Result<(), E>` could be `|_| async { Ok(()) }`, which manufactures send
    // authority with no socket in sight.
    let outcome = state.project_requests.open_discovery(ws, filters).await;
    // For the log line only. The id is the registry's, read back from the one
    // function that names it rather than chosen here; nothing downstream of
    // this call takes it as authority.
    report_project_open(&crate::project::discovery_sub_id(), outcome)
}

/// Re-open one request the registry itself intends. See
/// [`crate::project::ReplayableRequest`] — the token is the whole argument, so
/// this path cannot re-ask a question of its own.
async fn send_project_replay(
    ws: &mut WsStream,
    state: &mut BgState,
    request: crate::project::ReplayableRequest,
) -> ProjectSendOutcome {
    let sub_id = request.sub_id().to_string();
    let is_enrolment = sub_id == crate::project::PROJECT_ENROL_SUB_ID;
    let outcome = state.project_requests.open_replayed(ws, request).await;
    let reported = report_project_open(&sub_id, outcome);
    // A replayed tail is a *new* registration asking the same question, so it
    // has its own stored-events prefix and its own boundary. Claiming it here
    // is what makes reconnect symmetric with the first open; without it the
    // replacement tail would inherit no backlog, and every frame the relay
    // handed back on reconnect would count as live.
    if is_enrolment && matches!(reported, ProjectSendOutcome::Sent) {
        claim_enrolment_backlog(state);
    }
    reported
}

/// The registry's outcome as this module's outcome. Nothing else.
///
/// **Extracted so the mapping itself can be proved.** Every arm here is a
/// refusal the registry can make and this module has to report faithfully, and
/// three of them — exhaustion, an invariant violation, an unbounded filter —
/// describe states no honest fixture can reach, so the arms had no proof at all
/// while they were embedded in the logging below. Reporting terminal exhaustion
/// as a per-request ownership conflict is a diagnostic that sends someone
/// looking for a disagreement that does not exist, which is the failure this
/// separation makes visible.
fn project_send_outcome(outcome: &crate::project::OpenOutcome) -> ProjectSendOutcome {
    match outcome {
        crate::project::OpenOutcome::Sent => ProjectSendOutcome::Sent,
        crate::project::OpenOutcome::AlreadyLive => ProjectSendOutcome::AlreadyOpen,
        crate::project::OpenOutcome::Exhausted => ProjectSendOutcome::Exhausted,
        crate::project::OpenOutcome::Conflict { .. } => ProjectSendOutcome::MetadataConflict,
        crate::project::OpenOutcome::WriteFailed(_) => ProjectSendOutcome::WriteFailed,
        crate::project::OpenOutcome::UnboundedFilters => ProjectSendOutcome::UnboundedFilters,
        crate::project::OpenOutcome::InvariantViolation(_) => {
            ProjectSendOutcome::InvariantViolation
        }
    }
}

/// Turn one registry outcome into the caller-facing outcome, with its log.
///
/// The outcome is [`project_send_outcome`]'s, decided before the logging below
/// and returned unchanged afterwards — so what this reports and what that
/// function is proved to map cannot drift apart.
fn report_project_open(sub_id: &str, outcome: crate::project::OpenOutcome) -> ProjectSendOutcome {
    let reported = project_send_outcome(&outcome);
    match outcome {
        crate::project::OpenOutcome::Sent => {
            debug!(sub_id, "project REQ sent and registered");
            reported
        }
        crate::project::OpenOutcome::AlreadyLive => {
            debug!(
                sub_id,
                "project request already live — not re-sending its REQ"
            );
            reported
        }
        crate::project::OpenOutcome::Exhausted => {
            // Terminal and local: no REQ is written, the socket is fine, and
            // no further project request can ever be opened by this process.
            // Logged at error because unlike a conflict it will not resolve —
            // every later attempt returns here.
            error!(
                sub_id,
                "project request incarnations exhausted — refusing to reuse one; no further \
                 project subscription can be opened"
            );
            reported
        }
        crate::project::OpenOutcome::Conflict { held } => {
            warn!(
                sub_id,
                ?held,
                "refusing project request: this id is owned by a different request — \
                 nothing recorded"
            );
            reported
        }
        crate::project::OpenOutcome::WriteFailed(e) => {
            // Nothing was registered — installation happens only after a
            // successful write, so there is nothing to undo. Other project
            // requests are untouched and still answerable.
            warn!(
                sub_id,
                "failed to send project REQ — nothing registered: {e}"
            );
            reported
        }
        crate::project::OpenOutcome::UnboundedFilters => {
            // Nothing written, nothing registered, and the socket is fine — a
            // filterless REQ asks the relay for everything.
            warn!(
                sub_id,
                "refusing a project subscription whose filters constrain nothing"
            );
            reported
        }
        crate::project::OpenOutcome::InvariantViolation(violation) => {
            // The record this request would have joined does not resolve, so
            // the registry acted on none of it. Logged at error for the same
            // reason `replayable`'s refusal is: no project request can be
            // opened at all until durable intent is consistent again.
            error!(
                sub_id,
                %violation,
                "refusing a project subscription — the durable record does not resolve"
            );
            reported
        }
    }
}

async fn send_membership_subscribe(
    ws: &mut WsStream,
    agent_pubkey_hex: &str,
    since: Option<u64>,
) -> bool {
    let mut req_filter = serde_json::Map::new();
    req_filter.insert(
        "kinds".into(),
        json!([
            KIND_MEMBER_ADDED_NOTIFICATION,
            KIND_MEMBER_REMOVED_NOTIFICATION
        ]),
    );
    req_filter.insert("#p".into(), json!([agent_pubkey_hex]));

    let since_ts = match since {
        Some(ts) => ts.saturating_sub(SINCE_SKEW_SECS),
        None => std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
    };
    req_filter.insert("since".into(), json!(since_ts));

    let req = json!(["REQ", MEMBERSHIP_NOTIF_SUB_ID, Value::Object(req_filter)]);
    match serde_json::to_string(&req) {
        Ok(text) => {
            match ws_send_timeout(ws, Message::Text(text.into()), WS_SEND_TIMEOUT_SECS).await {
                Ok(()) => {
                    debug!("subscribed to membership notifications (since={since_ts})");
                    true
                }
                Err(e) => {
                    warn!("failed to send membership notification REQ: {e}");
                    false
                }
            }
        }
        Err(e) => {
            warn!("failed to serialize membership notification REQ: {e}");
            false
        }
    }
}

/// Build the NIP-PC peer-call REQ's filter list, in wire order.
///
/// Two filters, ORed by the relay, because the subscription answers two
/// different questions and neither one covers the other:
///
/// 1. `#p` — calls and results addressed to this agent. This is the inbound
///    half: what another agent asked us to do, and what a callee returned.
/// 2. `authors` — calls **this agent published**. This is what makes the
///    outstanding-call ledger real. The harness does not publish calls itself;
///    the agent subprocess runs `buzz agents call`, so the only place this
///    process can learn that a call exists is the wire. Without this filter a
///    result would arrive correlating to nothing and be refused as `Unknown`,
///    and the fan-out ceiling would bound a set that was always empty.
///
/// Results this agent publishes are deliberately not requested: a callee's own
/// result closes nothing on its side, and echoing it back would only spend
/// dedup slots.
fn peer_call_filters(agent_pubkey_hex: &str, since_ts: u64) -> Vec<Value> {
    vec![
        json!({
            "kinds": [KIND_PEER_CALL, KIND_PEER_CALL_RESULT],
            "#p": [agent_pubkey_hex],
            "since": since_ts,
        }),
        json!({
            "kinds": [KIND_PEER_CALL],
            "authors": [agent_pubkey_hex],
            "since": since_ts,
        }),
    ]
}

/// Send the NIP-01 REQ for NIP-PC peer calls and results.
async fn send_peer_call_subscribe(ws: &mut WsStream, agent_pubkey_hex: &str) -> bool {
    let since_ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let mut req = vec![json!("REQ"), json!(PEER_CALL_SUB_ID)];
    req.extend(peer_call_filters(agent_pubkey_hex, since_ts));

    match serde_json::to_string(&Value::Array(req)) {
        Ok(text) => {
            match ws_send_timeout(ws, Message::Text(text.into()), WS_SEND_TIMEOUT_SECS).await {
                Ok(()) => {
                    debug!("subscribed to peer calls (since={since_ts})");
                    true
                }
                Err(e) => {
                    warn!("failed to send peer-call REQ: {e}");
                    false
                }
            }
        }
        Err(e) => {
            warn!("failed to serialize peer-call REQ: {e}");
            false
        }
    }
}

/// Send a NIP-01 REQ for owner-to-agent observer control frames.
async fn send_observer_control_subscribe(ws: &mut WsStream, agent_pubkey_hex: &str) -> bool {
    let req = json!([
        "REQ",
        OBSERVER_CONTROL_SUB_ID,
        {
            "kinds": [KIND_AGENT_OBSERVER_FRAME],
            "#p": [agent_pubkey_hex],
            "since": std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        }
    ]);

    match serde_json::to_string(&req) {
        Ok(text) => {
            match ws_send_timeout(ws, Message::Text(text.into()), WS_SEND_TIMEOUT_SECS).await {
                Ok(()) => {
                    debug!("subscribed to observer control frames");
                    true
                }
                Err(e) => {
                    warn!("failed to send observer control REQ: {e}");
                    false
                }
            }
        }
        Err(e) => {
            warn!("failed to serialize observer control REQ: {e}");
            false
        }
    }
}

/// Send a WebSocket message with a hard timeout.
///
/// All `ws.send()` calls go through here so a stalled TCP socket can't wedge
/// the background task. On timeout the caller should break out of the loop to
/// trigger reconnect.
async fn ws_send_timeout(
    ws: &mut WsStream,
    msg: Message,
    timeout_secs: u64,
) -> Result<(), RelayError> {
    tokio::time::timeout(Duration::from_secs(timeout_secs), ws.send(msg))
        .await
        .map_err(|_| RelayError::Timeout)?
        .map_err(|e| RelayError::WebSocket(Box::new(e)))
}

/// Parse the relay's `retry in {N}s` hint from a rate-limit message.
///
/// Accepts any string containing `"retry in "` followed by decimal digits then `'s'`.
/// Returns `None` if the hint is absent; returns `Some(0)` for a literal zero (caller
/// defaults to 5 s). No regex dependency — a simple split is sufficient.
pub(crate) fn parse_rate_limit_retry_secs(msg: &str) -> Option<u64> {
    let after = msg.split("retry in ").nth(1)?;
    // All hint digits are ASCII, so char count == byte count — subslice is valid.
    let len = after.chars().take_while(|c| c.is_ascii_digit()).count();
    after[..len].parse::<u64>().ok()
}

/// Add ±20% jitter to a backoff duration using the nanosecond sub-second
/// component of the system clock as a cheap entropy source (no `rand` dep).
fn jittered_duration(base: Duration) -> Duration {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos();
    // factor ∈ [0.8, 1.2)
    let factor = 0.8 + (nanos as f64 / u32::MAX as f64) * 0.4;
    base.mul_f64(factor)
}

/// Classify a `RelayError` as a DNS resolution failure.
///
/// Matches the OS-level "name not found" strings surfaced by the platform's
/// resolver, covering macOS (`nodename nor servname`), Linux (`Name or service not
/// known`), and common BSD/Windows variants (`No such host`,
/// `failed to lookup address`). These are transient on brownouts and must NOT
/// consume a backoff ladder rung — they retry on a flat `DNS_RETRY_INTERVAL`.
pub(crate) fn is_dns_error(err: &RelayError) -> bool {
    let msg = err.to_string();
    msg.contains("nodename nor servname")
        || msg.contains("Name or service not known")
        || msg.contains("No such host")
        || msg.contains("failed to lookup address")
}

/// Shutdown-aware fixed-duration sleep for REQ pacing in `resubscribe_after_reconnect`.
///
/// Unlike `dns_flat_sleep`, no jitter is applied — exact `duration` is required
/// to maintain the ≤8 REQ/s pacing invariant. Non-Shutdown commands received
/// during the sleep are deferred in arrival order for live execution after
/// replay. Returns `true` if sleep completed normally, `false` if shutdown was
/// received.
async fn pacing_sleep(
    cmd_rx: &mut mpsc::Receiver<RelayCommand>,
    deferred_commands: &mut VecDeque<RelayCommand>,
    duration: Duration,
) -> bool {
    let deadline = tokio::time::Instant::now() + duration;
    let sleep = tokio::time::sleep_until(deadline);
    tokio::pin!(sleep);
    loop {
        tokio::select! {
            _ = &mut sleep => return true,
            cmd = cmd_rx.recv() => {
                match cmd {
                    Some(RelayCommand::Shutdown) | None => return false,
                    Some(cmd) => deferred_commands.push_back(cmd),
                }
            }
        }
    }
}

/// Shutdown-aware sleep used for DNS flat retries.
///
/// Selects between `duration` elapsing and a `Shutdown`/channel-closed signal on
/// `cmd_rx`. Returns `true` if the sleep completed normally, `false` if the task
/// should shut down.
async fn dns_flat_sleep(
    cmd_rx: &mut mpsc::Receiver<RelayCommand>,
    state: &mut BgState,
    duration: Duration,
) -> bool {
    let jittered = jittered_duration(duration);
    let deadline = tokio::time::Instant::now() + jittered;
    let sleep = tokio::time::sleep_until(deadline);
    tokio::pin!(sleep);
    loop {
        tokio::select! {
            _ = &mut sleep => return true,
            cmd = cmd_rx.recv() => {
                match cmd {
                    Some(RelayCommand::Shutdown) | None => return false,
                    Some(cmd) => apply_command_to_state(state, cmd),
                }
            }
        }
    }
}

/// Extract a channel UUID from the h tag of a Nostr event.
fn extract_h_tag_uuid(event: &nostr::Event) -> Option<Uuid> {
    event.tags.iter().find_map(|tag| {
        let tag_vec = tag.as_slice();
        if tag_vec.len() >= 2 && tag_vec[0] == "h" {
            tag_vec[1].parse::<Uuid>().ok()
        } else {
            None
        }
    })
}

/// Build and send a NIP-42 AUTH response event.
///
/// If `auth_tag` is provided (NIP-OA owner attestation), it is included in the
/// AUTH event so the relay can use it for membership delegation fallback.
async fn send_auth_response(
    ws: &mut WsStream,
    challenge: &str,
    relay_url: &str,
    keys: &Keys,
    auth_tag: Option<&nostr::Tag>,
) -> Result<(), RelayError> {
    let relay_nostr_url = RelayUrl::parse(relay_url)
        .map_err(|e| RelayError::Http(format!("invalid relay URL: {e}")))?;

    let auth_event = if let Some(tag) = auth_tag {
        // Cannot use EventBuilder::auth() shortcut — it doesn't accept extra tags.
        let tags = vec![
            nostr::Tag::parse(["relay", relay_url])
                .map_err(|e| RelayError::Http(format!("tag parse error: {e}")))?,
            nostr::Tag::parse(["challenge", challenge])
                .map_err(|e| RelayError::Http(format!("tag parse error: {e}")))?,
            tag.clone(),
        ];
        EventBuilder::new(nostr::Kind::Authentication, "")
            .tags(tags)
            .sign_with_keys(keys)?
    } else {
        EventBuilder::auth(challenge, relay_nostr_url).sign_with_keys(keys)?
    };

    let auth_msg = serde_json::to_string(&json!(["AUTH", auth_event]))?;
    ws_send_timeout(ws, Message::Text(auth_msg.into()), WS_SEND_TIMEOUT_SECS).await?;
    debug!("sent AUTH response for challenge");
    Ok(())
}

/// Convert a WebSocket URL to its HTTP equivalent.
///
/// `ws://host:port` → `http://host:port`
/// `wss://host:port` → `https://host:port`
/// Trailing slashes are stripped.
pub(crate) fn relay_ws_to_http(url: &str) -> String {
    url.replace("wss://", "https://")
        .replace("ws://", "http://")
        .trim_end_matches('/')
        .to_string()
}

/// Build the subscription ID for a channel: `ch-<uuid>`.
pub(crate) fn channel_sub_id(channel_id: Uuid) -> String {
    format!("ch-{channel_id}")
}

/// Extract a channel UUID from a subscription ID of the form `ch-<uuid>`.
/// Returns `None` if the format doesn't match or the UUID is invalid.
fn channel_id_from_sub_id(sub_id: &str) -> Option<Uuid> {
    sub_id
        .strip_prefix("ch-")
        .and_then(|s| s.parse::<Uuid>().ok())
}

/// Deliver a project-routed peer-call envelope that arrived on the peer-call
/// subscription.
///
/// The peer REQ and the watched-root REQ can both carry the same signed call —
/// the first by `#p`/`authors`, the second by `#e` — and which one wins is a
/// race. Both therefore end in the same place and spend the same
/// `project_seen_ids` slot, so whichever arrives first acts and the other is an
/// exact no-op rather than a second turn on one issue.
///
/// Order is the security property, and it is the same order the watched path
/// uses: verify id and signature, derive one unambiguous route, *then* spend
/// the dedup slot. Verifying before dedup is what stops a forged event claiming
/// a genuine event's id and suppressing it; deriving before dedup is what stops
/// an envelope with two roots — which belongs to neither — spending a slot at
/// all.
///
/// What this function deliberately does not do is decide anything. Enrolment,
/// lifecycle, trusted-peer classification, caller/callee binding, the call
/// ledger, replay, hop and visited limits are all downstream in
/// `dispatch_project_event`, unchanged and unaware of which REQ carried the
/// event here.
async fn route_project_peer_call(
    state: &mut BgState,
    event_tx: &mpsc::Sender<Option<BuzzEvent>>,
    event: Event,
) {
    let verified = match crate::project::VerifiedProjectEvent::verify(event).await {
        Ok(v) => v,
        Err(e) => {
            warn!("peer-call project envelope failed verification — dropping: {e}");
            return;
        }
    };

    let Some(route) = crate::project::ProjectRoute::derive(&verified) else {
        debug!(
            kind = verified.kind(),
            event_id = %verified.id(),
            "peer-call envelope resolves to no project root — dropping"
        );
        return;
    };

    let event_id_hex = verified.id();
    let ts = verified.event().created_at.as_secs();

    // The shared slot. A call delivered here and again on the watched stream is
    // one call.
    if !state.project_seen_ids.insert(event_id_hex.clone()) {
        debug!(
            event_id = %event_id_hex,
            "peer-call envelope already delivered on another project subscription — skipping"
        );
        return;
    }

    let project_event = crate::project::ProjectEvent::Routed {
        source: crate::project::ProjectSubscription::PeerCall,
        route,
        event: verified,
        // A peer call is delivered on the standing peer REQ, which carries no
        // reconstruction of its own. There is no backlog here to be the history
        // of, so the only honest answer is live.
        mode: crate::project::ProcessingMode::Live,
    };

    match event_tx.try_send(Some(BuzzEvent::Project(project_event))) {
        Ok(()) => {}
        Err(mpsc::error::TrySendError::Full(_)) => {
            // Same contract as every other project delivery: release the slot
            // so a replay can re-deliver an event that never reached the
            // harness, and move the project replay floor — not the channel one.
            state.project_seen_ids.remove(&event_id_hex);
            state.project_dropped_since =
                Some(state.project_dropped_since.map_or(ts, |d| d.min(ts)));
            state.proactive_resubscribe_needed = true;
            warn!(
                ts,
                "peer-call project envelope dropped (backpressure) — proactive resubscribe queued"
            );
        }
        Err(mpsc::error::TrySendError::Closed(_)) => {}
    }
}

/// Route one frame admitted by a live root catch-up request to its page.
///
/// **Every admitted frame reaches the page**, and that is the whole difference
/// from the live-surface path above. A history page counts what the relay
/// returned under its `limit` in order to distinguish a saturated page — there
/// is more history, ask again from further back — from an exhausted one. A
/// frame this agent refuses and discards here does not cost one event: it makes
/// the page read short, and a short page is how a reconstruction concludes it
/// has reached the end of history. So a frame that cannot be a row arrives as a
/// reason the page is untrustworthy instead.
///
/// Nothing crosses the event channel, so nothing here can be lost to
/// backpressure. That is not a convenience: `try_send` has a `Full` arm, and a
/// page that never learns a frame arrived is short by exactly that many rows,
/// with no way to tell afterwards.
///
/// Nothing here is deduplicated either. The live surfaces share one
/// `project_seen_ids` set, and putting page rows through it would suppress
/// exactly those events the agent had already been delivered live — shortening
/// the page for the second time, by the same mechanism, for a different reason.
async fn route_catch_up_frame(
    state: &mut BgState,
    admission: crate::project::FrameAdmission,
    expected_root: String,
    event: Event,
) {
    use crate::project::CatchUpOutcome;

    let sub_id = admission.sub_id().to_string();

    // Verification stays ahead of everything, exactly as on the live path. What
    // differs is the consequence: an unverifiable frame is not silently gone,
    // it is a hole the page must account for.
    let outcome = match crate::project::VerifiedProjectEvent::verify(event).await {
        Err(e) => {
            warn!(sub_id = %sub_id, "history page frame failed verification: {e}");
            CatchUpOutcome::Unusable("frame failed verification")
        }
        Ok(verified) => match crate::project::ProjectRoute::derive(&verified) {
            None => {
                debug!(sub_id = %sub_id, kind = verified.kind(), "history page frame resolves to no root");
                CatchUpOutcome::Unusable("frame resolves to no root")
            }
            // A relay filter is candidate selection, not authority: a catch-up
            // that answers with a different root is not answering the question
            // that was asked, and the root compared against is the one recorded
            // when the REQ was sent.
            Some(route) if route.root() != expected_root => {
                warn!(
                    expected = %expected_root,
                    got = %route.root(),
                    "history page frame names another root"
                );
                CatchUpOutcome::Unusable("frame names another root")
            }
            Some(_) => CatchUpOutcome::Row(Box::new(verified)),
        },
    };

    // `NotOurs` is the ordinary case in production today: nothing enrols a root,
    // so no reconstruction holds a page under any id. It stops being routine
    // exactly when enrolment lands, which is why it is logged rather than
    // counted as an anomaly now.
    let routing = state.reconstructions.observe(admission.catch_up(outcome));
    debug!(sub_id = %sub_id, ?routing, "catch-up frame routed");
}

/// Open as many enrolment-history pages as the walk currently wants.
///
/// A loop rather than a single open, because `page_wanted` is the cursor's own
/// answer and it stays true across a reconnect that dropped a page in flight.
/// At most one page is in flight at a time, so this issues one page per call in
/// the steady state — the loop exists so that resuming after a disconnect does
/// not need a second entry point.
///
/// **The bound is never a caller's choice.** The collector comes from the
/// walk's own cursor, the filter and class come from the collector, and the id
/// comes from the registry. Nothing between here and the socket can describe a
/// different question from the page it binds.
async fn drive_enrolment_history(ws: &mut WsStream, state: &mut BgState) {
    loop {
        let Some(history) = state.enrolment_history.as_mut() else {
            return;
        };
        let Some(collector) = history.begin_page() else {
            // Either nothing is wanted, or the walk just abandoned itself
            // because it could no longer distinguish a page from a superseded
            // one. The second is a fail-closed state and must be visible.
            if let Some(reason) = state
                .enrolment_history
                .as_ref()
                .and_then(|h| h.abandoned_reason())
            {
                let reason = reason.to_string();
                degrade_enrolment_history(state, reason);
            }
            return;
        };
        match state
            .project_requests
            .open_history_page(ws, collector)
            .await
        {
            crate::project::PageOpen::Opened(page) => {
                let sub_id = page.sub_id().to_string();
                let Some(history) = state.enrolment_history.as_mut() else {
                    return;
                };
                if let Err(rejected) = history.attach(page) {
                    // The page is bound and reachable, so its registration is
                    // closed rather than left behind: an abandoned registration
                    // would keep absorbing frames with nobody able to complete
                    // it.
                    let orphan = rejected.page.sub_id().to_string();
                    state
                        .project_requests
                        .refuse_live(&orphan, "enrolment history page could not attach");
                    let error = rejected.error;
                    degrade_enrolment_history(
                        state,
                        format!("enrolment history page could not attach: {error:?}"),
                    );
                    return;
                }
                debug!(sub_id = %sub_id, "enrolment history page opened");
            }
            // Not a completion. A page that could not be sent proves nothing
            // about how much history exists, so the walk must not be allowed to
            // look finished — it degrades, visibly.
            other => {
                degrade_enrolment_history(
                    state,
                    format!("enrolment history page could not be opened: {other:?}"),
                );
                return;
            }
        }
    }
}

/// Claim the stored-events prefix of the enrolment tail that was just opened.
///
/// Everything the relay had already stored when it answered this REQ is
/// context: it restores authority and lifecycle and creates no turn. Everything
/// after this registration's own boundary is work.
///
/// **The boundary is the discriminator, and the walk's progress is not.** An
/// earlier version asked whether the enrolment history walk had proven
/// exhaustion yet, reasoning that the walk owns the overlap window. It is the
/// wrong question about the wrong thing: a frame that arrives while the walk
/// happens to be mid-page is not thereby history — a real issue opened by a
/// real owner one second after startup arrives exactly then, and that version
/// enrolled it silently instead of answering it. What the walk covers is a fact
/// about the walk. What is stored versus what is new is a fact about this
/// request, and only its own EOSE states it.
///
/// **Nor is a timestamp the discriminator.** Comparing a backlog row's
/// `created_at` against the startup watermark would separate the overlap the
/// walk covers from rows published after startup — and would misfile exactly
/// the accepted slow-clock root this whole correction exists for, in the narrow
/// window where one can still land in a backlog. The rule that survives is the
/// one that never reads the author's clock.
///
/// The cost is stated rather than hidden: an issue published while this agent
/// is disconnected arrives in the reconnect backlog, so it enrols and refreshes
/// context without creating a turn, and is answered on its next comment.
/// Comments on already-enrolled roots are unaffected — they arrive on the
/// watched REQ, which has no such prefix. That is the conservative direction,
/// and it is the direction three iterations of re-answered history argue for.
fn claim_enrolment_backlog(state: &mut BgState) {
    if state.project_requests.bind_enrolment_backlog() {
        debug!("enrolment tail backlog claimed — its stored rows are context");
    }
}

/// Begin — or restart, on a widened coordinate set — the walk back through the
/// roots this agent is addressed on.
///
/// A widened set restarts from the current snapshot boundary rather than
/// widening the walk in place. The bound a cursor has already walked past was
/// proven exhausted *for the set it was asking about*; carrying it onto a
/// larger set would assert exhaustion over repositories no page had ever
/// mentioned. Restarting re-reads rows already seen, which the shared dedup
/// makes an exact no-op, and that is the cheap direction of the trade.
///
/// An identical set is left alone, so a discovery that changed nothing does not
/// restart a walk in progress.
fn begin_enrolment_history(state: &mut BgState, coordinates: Vec<String>, agent: String) {
    let cutoff = state
        .startup_watermark
        .unwrap_or_else(|| nostr::Timestamp::now().as_secs());
    let Some(walk) = crate::project::EnrolmentReconstruction::begin(
        coordinates,
        &agent,
        cutoff,
        ENROLMENT_HISTORY_PAGE_LIMIT,
        RELAY_MAX_LIMIT,
    ) else {
        // Nothing discovered. There is no question to ask, which is not the
        // same as an unanswered one — the walk stays absent rather than
        // degraded.
        return;
    };
    if state
        .enrolment_history
        .as_ref()
        .is_some_and(|live| live.scope() == walk.scope() && live.abandoned_reason().is_none())
    {
        return;
    }
    // A restart clears the degraded state: it is a claim about a walk, and this
    // is a different walk.
    state.enrolment_history_degraded = None;
    state.enrolment_history = Some(walk);
}

/// Rows per enrolment history page.
///
/// Not a completeness bound — the cursor escalates it on a saturated page and
/// keeps walking, so this is only how much is asked for at a time. It is small
/// enough that the common case (an agent with a handful of open issues) costs
/// one page, and the escalation covers the rest.
const ENROLMENT_HISTORY_PAGE_LIMIT: usize = 64;

/// The largest `limit` this relay will honour on a REQ.
///
/// The escalation ceiling, and the point at which a page that is still
/// saturated with every row sharing one timestamp can no longer be widened —
/// which is a degraded walk, not a complete one.
const RELAY_MAX_LIMIT: usize = 5_000;

/// Roots the walk discovered and has not yet proven it handed to the run loop.
///
/// **Why a queue and not a count.** The relay task may only ever `try_send`:
/// a blocking send would stall the reader that the run loop's own commands
/// come back through, which is a deadlock rather than a delay. So a refused
/// send is routine — and it is a fact about how full the queue was for one
/// instant, not a fact about how much history exists. The version before this
/// one let that instant end the walk: it dropped the root, logged
/// `reconstruction complete` with `dropped = 1`, and left the agent holding
/// authority over a strict subset of the conversations it had just proven it
/// was addressed on. The proactive resubscribe it queued could not repair
/// that either — the walk asks for no further page, an identical scope does
/// not restart it, and the live tail reaches back only as far as the relay's
/// accepted skew.
///
/// So an undeliverable root is *retained*, and the walk stays unfinished
/// until every root it found is either restored or the retention bound is
/// spent — at which point the reconstruction is degraded, visibly. Those are
/// the only three exits, which is what makes the completion line honest.
/// Which reconstruction a batch of retained rows belongs to.
///
/// Carried rather than inferred, because the two batches make *different*
/// claims when they finish — "this agent has found every root it is addressed
/// on" and "this root's history is complete through the cutoff" — and one
/// completion line covering both would be true of neither.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ReplayBatch {
    /// The roots the enrolment walk found.
    EnrolmentHistory,
    /// One root's comments, revisions and lifecycle, merged and ordered.
    RootCatchUp { root: String },
}

struct PendingReplay {
    kind: ReplayBatch,
    /// How many rows the batch found. The denominator of every claim made
    /// about it.
    discovered: usize,
    /// How many have reached the run loop, across every attempt.
    restored: usize,
    /// Still owed, **in the order the batch established**.
    ///
    /// For a catch-up that order is load-bearing rather than incidental: the
    /// merge sorts ascending by `created_at` with the root first, and a close
    /// delivered after the reopen that followed it would leave the watch in
    /// the state the history says it left three events ago.
    queue: VecDeque<(crate::project::ProjectSubscription, VerifiedProjectEvent)>,
    /// Consecutive attempts that may deliver nothing before the reconstruction
    /// is declared degraded.
    ///
    /// Reset by any delivery, so what this bounds is a *stalled* run loop
    /// rather than a merely slow one: a run loop that keeps draining keeps
    /// earning attempts, and one that has stopped draining altogether reaches
    /// the fail-closed state in [`ENROLMENT_RESTORE_STALL_LIMIT`] ticks.
    stalls_left: u32,
}

/// What one pass over [`PendingReplay`] settled.
///
/// **The single producer of "complete".** `Complete` is returned from exactly
/// one place — the branch where the queue is empty — so there is no path on
/// which a partial restore can be reported as a whole one. A caller cannot
/// assemble this verdict from counts it holds itself, which is precisely how
/// the previous version came to log completion over a dropped root.
#[derive(Debug, PartialEq, Eq)]
enum RestoreOutcome {
    /// Nothing was pending.
    Idle,
    /// Every discovered root reached the run loop.
    Complete,
    /// Some roots are retained for a later attempt. Carries how many.
    Retained(usize),
    /// The walk cannot restore what it found. Fail-closed and visible.
    Degraded,
}

/// How long between attempts to hand retained roots on.
const ENROLMENT_RESTORE_RETRY_INTERVAL: Duration = Duration::from_millis(250);

/// Consecutive fruitless attempts before a retained set degrades the walk.
///
/// At [`ENROLMENT_RESTORE_RETRY_INTERVAL`] that is ten seconds of a run loop
/// that has drained nothing at all. Long enough that an ordinary burst of
/// turns is absorbed; short enough that an operator learns the agent cannot
/// prove its authority while it still matters.
const ENROLMENT_RESTORE_STALL_LIMIT: u32 = 40;

/// Take ownership of the roots a finished walk found, and try to hand them on.
///
/// Separate from [`drive_pending_restorations`] only so the retry path and the
/// first attempt are the same code. Nothing here reports completion.
fn begin_restoring_roots(
    state: &mut BgState,
    event_tx: &mpsc::Sender<Option<BuzzEvent>>,
    generation: u64,
    roots: Vec<VerifiedProjectEvent>,
) -> RestoreOutcome {
    let superseded = state
        .replay_deliveries
        .iter()
        .filter(|batch| batch.kind == ReplayBatch::EnrolmentHistory)
        .map(|batch| batch.queue.len())
        .sum::<usize>();
    if superseded > 0 {
        // A restarted walk covers everything the previous one did — it asks
        // again from the current snapshot boundary — so its own roots subsume
        // these, and delivery is idempotent through `project_seen_ids` either
        // way.
        debug!(
            retained = superseded,
            "a fresh reconstruction supersedes an earlier one's retained roots"
        );
        state
            .replay_deliveries
            .retain(|batch| batch.kind != ReplayBatch::EnrolmentHistory);
    }
    let source = crate::project::ProjectSubscription::EnrolmentHistory { generation };
    enqueue_replay(
        state,
        event_tx,
        ReplayBatch::EnrolmentHistory,
        roots.into_iter().map(|row| (source.clone(), row)).collect(),
    )
}

/// Queue one ordered batch of replay rows and try to hand it on.
fn enqueue_replay(
    state: &mut BgState,
    event_tx: &mpsc::Sender<Option<BuzzEvent>>,
    kind: ReplayBatch,
    rows: Vec<(crate::project::ProjectSubscription, VerifiedProjectEvent)>,
) -> RestoreOutcome {
    state.replay_deliveries.push_back(PendingReplay {
        kind,
        discovered: rows.len(),
        restored: 0,
        queue: rows.into(),
        stalls_left: ENROLMENT_RESTORE_STALL_LIMIT,
    });
    drive_pending_restorations(state, event_tx)
}

/// Hand as many retained rows to the run loop as it will take right now.
///
/// Every row goes out under the class of the page that produced it —
/// `EnrolmentHistory` or `RootCatchUp` — and both fold every effect through
/// `ProcessingMode::Replay`, so these restore authority, context and lifecycle
/// and **never create a turn**. Nothing here decides that; the class the page
/// was registered under does.
///
/// **Batches drain in order, and rows within a batch drain in the order the
/// batch established.** For a catch-up that order is the merge's — ascending
/// `created_at`, root first — and it is what makes lifecycle replay mean
/// anything: a close applied after the reopen that followed it would leave the
/// watch in a state the history left three events ago.
///
/// Rows spend the shared `project_seen_ids` slot, so one the live surfaces have
/// already delivered is not replayed a second time — and, in the other order, a
/// row replayed here cannot be re-answered by a later live delivery of the same
/// event. A row already holding a slot is counted restored: its effect is
/// already held, which is the whole point of replaying it.
fn drive_pending_restorations(
    state: &mut BgState,
    event_tx: &mpsc::Sender<Option<BuzzEvent>>,
) -> RestoreOutcome {
    let mut outcome = RestoreOutcome::Idle;
    // Batches, front to back. A batch that completes is retired and the next
    // one is attempted in the same pass, so a run loop with room takes the
    // whole backlog rather than one batch per retry tick.
    loop {
        let Some(mut pending) = state.replay_deliveries.pop_front() else {
            return outcome;
        };

        let mut delivered = 0usize;
        // A refusal the retry cannot repair. Held rather than acted on inside
        // the loop so that the rows already handed on are still counted.
        let mut unrestorable: Option<String> = None;

        while let Some((source, verified)) = pending.queue.front().cloned() {
            let event_id_hex = verified.id();
            let Some(route) = crate::project::ProjectRoute::derive(&verified) else {
                // The page's own scope already refused rows that resolve to no
                // root, so this is unreachable rather than routine. If it
                // happens anyway, no number of retries changes it: it is a row
                // this agent cannot replay, and the batch must say so rather
                // than wait.
                unrestorable = Some(format!(
                    "replayed row {event_id_hex} resolves to no project route"
                ));
                break;
            };
            if !state.project_seen_ids.insert(event_id_hex.clone()) {
                pending.queue.pop_front();
                pending.restored += 1;
                delivered += 1;
                continue;
            }
            let project_event = crate::project::ProjectEvent::Routed {
                source,
                route,
                event: verified,
                // Reconstructed history, by the only path that produces it.
                mode: crate::project::ProcessingMode::Replay,
            };
            match event_tx.try_send(Some(BuzzEvent::Project(project_event))) {
                Ok(()) => {
                    pending.queue.pop_front();
                    pending.restored += 1;
                    delivered += 1;
                }
                Err(mpsc::error::TrySendError::Full(_)) => {
                    // The slot is released so that a live delivery of the same
                    // event in the meantime is not suppressed by a claim
                    // standing for one that never arrived. The row stays at the
                    // front of the queue; the retry re-claims it, or finds it
                    // already held and counts it restored.
                    state.project_seen_ids.remove(&event_id_hex);
                    break;
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    state.project_seen_ids.remove(&event_id_hex);
                    unrestorable =
                        Some("the run loop is gone — no replayed row can reach it".to_string());
                    break;
                }
            }
        }

        let retained = pending.queue.len();
        let discovered = pending.discovered;
        let restored = pending.restored;

        if let Some(reason) = unrestorable {
            degrade_replay_batch(
                state,
                &pending.kind,
                format!("{reason}; {restored} of {discovered} rows replayed, {retained} retained"),
            );
            return RestoreOutcome::Degraded;
        }

        if retained == 0 {
            // The one place a completion line is produced, and it is reachable
            // only with an empty queue — so `dropped` is zero by construction
            // rather than by an arithmetic that could disagree with it.
            match &pending.kind {
                ReplayBatch::EnrolmentHistory => info!(
                    discovered,
                    restored,
                    dropped = 0,
                    "enrolment history reconstruction complete"
                ),
                ReplayBatch::RootCatchUp { root } => info!(
                    root = %root,
                    discovered,
                    restored,
                    dropped = 0,
                    "root history reconstruction complete"
                ),
            }
            outcome = RestoreOutcome::Complete;
            continue;
        }

        if delivered > 0 {
            pending.stalls_left = ENROLMENT_RESTORE_STALL_LIMIT;
        } else {
            pending.stalls_left = pending.stalls_left.saturating_sub(1);
        }

        if pending.stalls_left == 0 {
            degrade_replay_batch(
                state,
                &pending.kind,
                format!(
                    "{retained} of {discovered} reconstructed rows could not be handed to the \
                     run loop within {ENROLMENT_RESTORE_STALL_LIMIT} attempts"
                ),
            );
            return RestoreOutcome::Degraded;
        }

        debug!(
            ?pending.kind,
            discovered, restored, retained,
            "reconstructed rows retained for retry — reconstruction unfinished"
        );
        // Back at the front: a batch that could not drain blocks the ones
        // behind it on purpose. They are later history, and delivering them
        // past a stalled predecessor would reorder the replay.
        state.replay_deliveries.push_front(pending);
        return RestoreOutcome::Retained(retained);
    }
}

/// Route a batch's failure to the degraded state that owns the claim it breaks.
fn degrade_replay_batch(state: &mut BgState, kind: &ReplayBatch, reason: String) {
    match kind {
        ReplayBatch::EnrolmentHistory => degrade_enrolment_history(state, reason),
        ReplayBatch::RootCatchUp { root } => {
            let root = root.clone();
            degrade_root_catch_up(state, &root, reason);
        }
    }
}

/// Hand one root's merged history to the run loop, if it is finished.
///
/// Does nothing while any required stream is still paging: `take_completed` is
/// the only thing that answers "finished", and it answers by consuming the
/// reconstruction, so there is no window in which a root can be both finished
/// and still asking for pages.
///
/// A merge that refuses — a stream missing, duplicated, or paginated from
/// another cutoff — degrades the root. It is not a partial history to deliver
/// anyway: rows that do not compose into one ordered snapshot cannot be folded
/// into a lifecycle state, and folding them regardless is how an agent ends up
/// confidently wrong about whether a conversation is open.
fn finish_root_catch_up(
    state: &mut BgState,
    event_tx: &mpsc::Sender<Option<BuzzEvent>>,
    root: &str,
) {
    let Some(merged) = state.reconstructions.take_completed(root) else {
        return;
    };
    let rows = match merged {
        Ok(rows) => rows,
        Err(reason) => {
            degrade_root_catch_up(state, root, format!("history merge refused: {reason}"));
            return;
        }
    };
    // Each row is stamped with the class of the stream that carries its kind,
    // derived rather than chosen: the merge folded two streams into one order,
    // so a single class for the batch would file a PR revision under comments.
    // `RootCatchUp` is also the only history class `resolve_addressing` lets
    // through without a `p` tag — which a status event routinely has none of,
    // and which is exactly the row this whole reconstruction exists to replay.
    let root_id = rows.root().to_string();
    // The root event leads the merge, so what this reconstruction *found* is
    // the rows behind it. Recorded before delivery, because it is a fact about
    // the history that was rebuilt rather than about how fast the run loop
    // drained it.
    state
        .root_catch_up_done
        .insert(root_id.clone(), rows.len().saturating_sub(1));
    let mut batch = Vec::with_capacity(rows.len());
    for row in rows.rows() {
        // The merge leads with the root itself, and the root is not a row of
        // either stream — it is the object they hang off. It is also already
        // enrolled: a catch-up starts *because* the run loop bound this root,
        // so replaying it would be a dedup no-op at best. Skipped by identity
        // rather than by kind, so a history row that happens to carry a root
        // kind is still refused loudly below.
        if row.id() == root_id {
            continue;
        }
        let Some(stream) = crate::project::HistoryStream::carrying(row.kind()) else {
            // The page's own filter admitted it, so a kind neither stream
            // carries means the filter and this mapping disagree. Refuse the
            // whole root rather than replay a row under a class that does not
            // describe it.
            degrade_root_catch_up(
                state,
                root,
                format!("merged row of kind {} belongs to no stream", row.kind()),
            );
            return;
        };
        batch.push((
            crate::project::ProjectSubscription::RootCatchUp {
                root: root_id.clone(),
                stream,
            },
            row.clone(),
        ));
    }
    let rows = batch;
    let outcome = enqueue_replay(
        state,
        event_tx,
        ReplayBatch::RootCatchUp {
            root: root.to_string(),
        },
        rows,
    );
    debug!(root = %root, ?outcome, "root history merged and queued for replay");
}

/// Begin rebuilding one restored root's history.
///
/// **Only a root the run loop has already bound reaches here.** The proof is
/// minted where the discovered set lives and travels as a proof, so this cannot
/// start a reconstruction for a root that was never validated against a
/// discovered repository — and the binding the merge checks its streams against
/// is the same one the enrolment set holds.
///
/// The cutoff is the startup watermark, the same snapshot boundary the
/// enrolment walk uses. Every stream of one root must end at the same moment or
/// the merge is comparing histories that stop at different times, and the two
/// walks agreeing on it is what makes "complete through the cutoff, live after
/// it" one statement rather than two overlapping guesses.
///
/// A second request for a root already being rebuilt is a no-op: `insert`
/// refuses it, which is what makes a rediscovered root idempotent rather than a
/// doubled page.
fn begin_root_catch_up(state: &mut BgState, root: crate::project::VerifiedBoundRoot) {
    let cutoff = state
        .startup_watermark
        .unwrap_or_else(|| nostr::Timestamp::now().as_secs());
    let root_id = root.binding().root().to_string();
    if state.root_catch_up_degraded.contains_key(&root_id) {
        // Already fail-closed on this root. Starting a fresh reconstruction
        // would let it re-enter the healthy path and quietly withdraw a claim
        // that was never repaired.
        debug!(root = %root_id, "root history already degraded — not restarting");
        return;
    }
    if state.root_catch_up_done.contains_key(&root_id) {
        // Already rebuilt on this connection's process. The enrolment walk
        // re-restores every root it finds on each reconnect, and re-paging a
        // history that has already been folded in would cost the same events
        // again for no new fact.
        debug!(root = %root_id, "root history already rebuilt — not repeating");
        return;
    }
    let reconstruction = crate::project::RootReconstruction::begin(
        &root,
        cutoff,
        ENROLMENT_HISTORY_PAGE_LIMIT,
        RELAY_MAX_LIMIT,
    );
    if state.reconstructions.insert(reconstruction) {
        debug!(root = %root_id, cutoff, "root history reconstruction begun");
    }
}

/// Open as many root-history pages as the live reconstructions currently want.
///
/// The same shape as [`drive_enrolment_history`] and for the same reasons: the
/// collector comes from the reconstruction's own cursor, the filter and class
/// come from the collector, and the id comes from the registry, so nothing
/// between here and the socket can describe a different question from the page
/// it binds. A page that cannot be opened or attached degrades **that root**
/// rather than being retried into a silence — a reconstruction that stops
/// asking without saying so is exactly the false completeness this whole phase
/// exists to remove.
///
/// A loop over `pages_wanted` rather than one page per call, because a root
/// requires up to two streams and a reconnect drops every page in flight at
/// once.
async fn drive_root_reconstructions(ws: &mut WsStream, state: &mut BgState) {
    loop {
        let Some((root, stream)) = state.reconstructions.pages_wanted().into_iter().next() else {
            return;
        };
        let Some(collector) = state.reconstructions.begin_page(&root, stream) else {
            // Either the stream stopped wanting a page between the two calls,
            // or `begin_page` abandoned the reconstruction because its
            // generation space is spent. The second is fail-closed and must be
            // visible; the first would spin, so both end the drive.
            report_abandoned_reconstructions(state);
            return;
        };
        match state
            .project_requests
            .open_history_page(ws, collector)
            .await
        {
            crate::project::PageOpen::Opened(page) => {
                let sub_id = page.sub_id().to_string();
                if let Err(rejected) = state.reconstructions.attach(page) {
                    // The page is bound and reachable, so its registration is
                    // closed rather than left behind: an abandoned registration
                    // would keep absorbing frames with nobody able to complete
                    // it.
                    let orphan = rejected.page.sub_id().to_string();
                    state
                        .project_requests
                        .refuse_live(&orphan, "root history page could not attach");
                    let error = rejected.error;
                    degrade_root_catch_up(
                        state,
                        &root,
                        format!("root history page could not attach: {error:?}"),
                    );
                    // One root's bookkeeping, not the socket's. The others
                    // still want pages, and returning here would stall them
                    // until something unrelated drove the loop again.
                    continue;
                }
                debug!(root = %root, ?stream, sub_id = %sub_id, "root history page opened");
            }
            // Not a completion. A page that could not be sent proves nothing
            // about how much history exists, so this root must not be allowed
            // to look finished — it degrades, visibly.
            other => {
                degrade_root_catch_up(
                    state,
                    &root,
                    format!("root history page could not be opened: {other:?}"),
                );
                return;
            }
        }
    }
}

/// Surface any reconstruction that abandoned itself without a caller asking.
///
/// `begin_page` can abandon on its own — generation space exhausted — and a
/// reconstruction that has given up while nothing reported it is the silent
/// half of the failure this phase forbids.
fn report_abandoned_reconstructions(state: &mut BgState) {
    for (root, reason) in state.reconstructions.abandoned() {
        if !state.root_catch_up_degraded.contains_key(&root) {
            degrade_root_catch_up(state, &root, reason);
        }
    }
}

/// Enter the per-root fail-closed state, once.
///
/// The root keeps its watch and keeps answering live traffic — what is lost is
/// the claim to know what state its history left it in, which is exactly the
/// claim a restart needs and cannot bluff.
fn degrade_root_catch_up(state: &mut BgState, root: &str, reason: String) {
    state.reconstructions.abandon(root, reason.clone());
    state.replay_deliveries.retain(|batch| {
        batch.kind
            != ReplayBatch::RootCatchUp {
                root: root.to_string(),
            }
    });
    if !state.root_catch_up_degraded.contains_key(root) {
        warn!(
            root = %root,
            reason = %reason,
            "root history reconstruction DEGRADED — this agent cannot prove what state this \
             conversation was left in, and is fail-closed on that claim"
        );
        state
            .root_catch_up_degraded
            .insert(root.to_string(), reason);
    }
}

/// Enter the fail-closed degraded state, once.
///
/// Recorded on the state and logged at `warn`. It is deliberately not an error
/// returned upwards: the connection is fine and every other subscription keeps
/// working — what is not fine is the claim that this agent knows the full set
/// of conversations it is responsible for.
fn degrade_enrolment_history(state: &mut BgState, reason: String) {
    if let Some(history) = state.enrolment_history.as_mut() {
        history.abandon(reason.clone());
    }
    if state.enrolment_history_degraded.is_none() {
        warn!(
            reason = %reason,
            "enrolment history reconstruction DEGRADED — this agent cannot prove it has \
             found every root it is addressed on, and is fail-closed on that claim"
        );
        state.enrolment_history_degraded = Some(reason);
    }
}

/// Route one frame admitted by a live enrolment-history request to its page.
///
/// The same contract as [`route_catch_up_frame`]: every admitted frame reaches
/// the page, because the page counts what the relay returned in order to tell a
/// saturated page from an exhausted one, and a frame silently dropped here
/// shortens the page by exactly one row — which is how the walk decides it has
/// reached the end of history.
///
/// Which rows *belong* is not decided here. The collector asks its own scope,
/// so a root naming an undiscovered repository, or not addressing this agent,
/// is counted and refused rather than counted as a row.
async fn route_enrolment_history_frame(
    state: &mut BgState,
    admission: crate::project::FrameAdmission,
    event: Event,
) {
    use crate::project::CatchUpOutcome;

    let sub_id = admission.sub_id().to_string();
    let outcome = match crate::project::VerifiedProjectEvent::verify(event).await {
        Err(e) => {
            warn!(sub_id = %sub_id, "enrolment history frame failed verification: {e}");
            CatchUpOutcome::Unusable("frame failed verification")
        }
        Ok(verified) => CatchUpOutcome::Row(Box::new(verified)),
    };

    let Some(history) = state.enrolment_history.as_mut() else {
        debug!(sub_id = %sub_id, "enrolment history frame with no walk in progress");
        return;
    };
    let routing = history.observe(admission.catch_up(outcome));
    if let crate::project::EnrolmentRouting::Contradiction { reason } = &routing {
        let reason = reason.clone();
        degrade_enrolment_history(state, reason);
        return;
    }
    debug!(sub_id = %sub_id, ?routing, "enrolment history frame routed");
}

/// Per-channel CLOSED denials: the channel is forbidden but the connection is
/// fine. Match these EXACT strings, never a `starts_with("restricted")` prefix —
/// a prefix would also swallow connection-level `restricted: insufficient scope`,
/// dropping a channel instead of reconnecting. The only CLOSED senders of these
/// strings are `req.rs:153` (not a channel member) and `side_effects.rs:71`
/// (channel access revoked, via member eviction / open→private flip).
/// `ingest.rs` returns these as EVENT-publish `OK(false)`, never as a
/// subscription CLOSED, so it is not a source here.
const CHANNEL_ACCESS_DENIED_REASONS: &[&str] = &[
    "restricted: not a channel member",
    "restricted: channel access revoked",
];

/// Handle a CLOSED that denies access to a single channel: drop just that
/// channel's subscription (the proven Unsubscribe cleanup) and keep the socket.
///
/// Returns `true` when the CLOSED was an exact per-channel denial on a `ch-`
/// subscription and the channel was dropped — the caller keeps the connection
/// with no reconnect. Returns `false` for everything else (connection-level
/// `restricted: insufficient scope`, `auth-required`, non-channel subs), which
/// falls through to the existing reconnect path.
///
/// An already-removed channel is a harmless no-op: the remove/clear simply
/// affect nothing, and the dropped channel is never re-subscribed, so the loop
/// cannot re-form.
fn drop_channel_on_access_denied(state: &mut BgState, sub_id: &str, message: &str) -> bool {
    if !CHANNEL_ACCESS_DENIED_REASONS.contains(&message) {
        return false;
    }
    let Some(channel_id) = channel_id_from_sub_id(sub_id) else {
        return false;
    };
    warn!(
        "channel {channel_id} access denied by relay: {message} — dropping subscription, keeping connection"
    );
    state.active_subscriptions.remove(&channel_id);
    state.clear_channel_state(&channel_id);
    true
}

/// Apply the appropriate auth header to a reqwest request builder.
/// Parse a raw relay text frame into a typed [`RelayMessage`].
#[allow(private_interfaces)]
pub(crate) fn parse_relay_message(text: &str) -> Result<RelayMessage, RelayError> {
    let arr: Vec<Value> = serde_json::from_str(text)?;

    let msg_type = arr
        .first()
        .and_then(|v| v.as_str())
        .ok_or_else(|| RelayError::UnexpectedMessage(text.to_string()))?;

    match msg_type {
        "EVENT" => {
            let sub_id = arr
                .get(1)
                .and_then(|v| v.as_str())
                .ok_or_else(|| RelayError::UnexpectedMessage(text.to_string()))?
                .to_string();
            let event: Event = serde_json::from_value(
                arr.get(2)
                    .cloned()
                    .ok_or_else(|| RelayError::UnexpectedMessage(text.to_string()))?,
            )?;
            Ok(RelayMessage::Event {
                subscription_id: sub_id,
                event: Box::new(event),
            })
        }
        "OK" => {
            let event_id = arr
                .get(1)
                .and_then(|v| v.as_str())
                .ok_or_else(|| RelayError::UnexpectedMessage(text.to_string()))?
                .to_string();
            let accepted = arr.get(2).and_then(|v| v.as_bool()).unwrap_or(false);
            let message = arr
                .get(3)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            Ok(RelayMessage::Ok {
                event_id,
                accepted,
                message,
            })
        }
        "EOSE" => {
            let sub_id = arr
                .get(1)
                .and_then(|v| v.as_str())
                .ok_or_else(|| RelayError::UnexpectedMessage(text.to_string()))?
                .to_string();
            Ok(RelayMessage::Eose {
                subscription_id: sub_id,
            })
        }
        "CLOSED" => {
            let sub_id = arr
                .get(1)
                .and_then(|v| v.as_str())
                .ok_or_else(|| RelayError::UnexpectedMessage(text.to_string()))?
                .to_string();
            let message = arr
                .get(2)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            Ok(RelayMessage::Closed {
                subscription_id: sub_id,
                message,
            })
        }
        "NOTICE" => {
            let message = arr
                .get(1)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            Ok(RelayMessage::Notice { message })
        }
        "AUTH" => {
            let challenge = arr
                .get(1)
                .and_then(|v| v.as_str())
                .ok_or_else(|| RelayError::UnexpectedMessage(text.to_string()))?
                .to_string();
            Ok(RelayMessage::Auth { challenge })
        }
        other => Err(RelayError::UnexpectedMessage(format!(
            "unknown message type: {other}"
        ))),
    }
}

/// Whether an initial connect/auth-handshake error is terminal — retrying
/// with the same `relay_url`/`keys`/`auth_tag` would reproduce it — rather
/// than transient (the network dropping bytes on a spotty link).
///
/// **Terminal (fail fast):**
/// - `Http`/`Json`/`UnexpectedMessage` — local parsing or relay protocol
///   mismatch; deterministic given the same relay.
/// - `WebSocket` inner variants `Url`, `Capacity`, `Utf8`, `HttpFormat`,
///   `AttackAttempt` — deterministic pre-connect or handshake-shape failures.
/// - `WebSocket(Protocol(…))` — most variants indicate a stable HTTP/WS
///   upgrade mismatch (wrong method, missing headers, accept-key mismatch).
///   Two exceptions are transient: `HandshakeIncomplete` (connection dropped
///   mid-handshake) and `ResetWithoutClosingHandshake` (abrupt reset).
/// - `WebSocket(Http(resp))` — non-101 HTTP response; terminal unless the
///   status is `408`, `429`, or `5xx` (server-side transient conditions).
/// - `WebSocket(Tls)` — deterministic TLS config failures. On our rustls
///   build the only connect-time `Tls` is `InvalidDnsName`.
/// - `WebSocket(Io)` with a deterministic `rustls::Error` in the source
///   chain — terminal. `tokio-rustls` wraps all rustls handshake failures
///   as `io::Error` with the `rustls::Error` as source; `tokio-tungstenite`
///   then surfaces them as `Error::Io`. Only deterministic cert/config/
///   incompatibility variants (allowlist) are terminal; ambiguous protocol,
///   decrypt, and server-alert shapes stay transient under the bounded budget.
/// - `AuthFailed` — split by [`is_terminal_auth_failure`].
///
/// **Transient (retry):**
/// - `WebSocket(Io)` without a rustls source, or with an ambiguous rustls
///   error (alerts, protocol, decrypt) — plain transport failures (reset,
///   EOF, timeout, refused) and ambiguous TLS errors stay retryable.
/// - `WebSocket(ConnectionClosed)` — link-level closure.
/// - `WebSocket(AlreadyClosed)`, `WebSocket(WriteBufferFull)` — unreachable
///   during `connect_async`; kept fail-safe transient.
/// - `NoAuthChallenge`, `ConnectionClosed`, `Timeout` — timing/link noise.
fn is_terminal_connect_error(err: &RelayError) -> bool {
    match err {
        RelayError::Http(_) | RelayError::Json(_) | RelayError::UnexpectedMessage(_) => true,
        RelayError::WebSocket(e) => is_terminal_ws_error(e.as_ref()),
        RelayError::AuthFailed(message) => is_terminal_auth_failure(message),
        RelayError::NoAuthChallenge | RelayError::ConnectionClosed | RelayError::Timeout => false,
    }
}

/// Exhaustive classification of `tungstenite::Error` inner variants for
/// startup connect retry. No wildcard — a tungstenite upgrade forces
/// reclassification at compile time.
fn is_terminal_ws_error(err: &tokio_tungstenite::tungstenite::Error) -> bool {
    use tokio_tungstenite::tungstenite::error::ProtocolError;
    use tokio_tungstenite::tungstenite::Error as WsError;

    match err {
        // Deterministic pre-connect / handshake-shape failures.
        WsError::Url(_)
        | WsError::Capacity(_)
        | WsError::Utf8(_)
        | WsError::HttpFormat(_)
        | WsError::AttackAttempt => true,

        // Non-101 HTTP: terminal unless 408/429/5xx.
        WsError::Http(resp) => {
            let status = resp.status().as_u16();
            !(status == 408 || status == 429 || (500..600).contains(&status))
        }

        // Protocol errors: most are deterministic upgrade mismatches.
        WsError::Protocol(p) => !matches!(
            p,
            ProtocolError::HandshakeIncomplete | ProtocolError::ResetWithoutClosingHandshake
        ),

        // Io: split by error source and rustls variant. tokio-rustls wraps
        // rustls errors as io::Error(InvalidData, rustls_err). Deterministic
        // cert/config/incompatibility failures (allowlist) are terminal;
        // ambiguous protocol, decrypt, and server-alert shapes stay transient
        // under the bounded retry budget. Plain transport Io (reset, EOF,
        // timeout, refused) also stays transient.
        // Relies on a single rustls version in the dep tree (0.23.40);
        // a version split would break the downcast.
        WsError::Io(e) => is_terminal_rustls_io_error(e),

        WsError::ConnectionClosed => false,

        // Deterministic TLS config failures. On our rustls build the only
        // connect-time Tls variant is InvalidDnsName; certificate validation
        // failures arrive wrapped inside Io (terminal via source-chain
        // downcast above).
        WsError::Tls(_) => true,

        // Unreachable during connect_async; kept fail-safe transient.
        WsError::AlreadyClosed | WsError::WriteBufferFull(_) => false,
    }
}

/// Walks an `io::Error` for a `rustls::Error` and inspects its variant.
/// Returns `true` (terminal) only for deterministic cert/config/incompatibility
/// failures that retry cannot fix. Ambiguous protocol, decrypt, and server-alert
/// shapes return `false` (transient) — retries are bounded and the feature's
/// purpose is resilience.
///
/// Relies on a single rustls version in the dep tree (0.23.40); a version split
/// would break the downcast.
fn is_terminal_rustls_io_error(err: &std::io::Error) -> bool {
    use std::error::Error as _;

    fn find_rustls_error(err: &std::io::Error) -> Option<&rustls::Error> {
        // First check the direct inner payload (io::Error stores it via
        // get_ref — source() skips to *its* source).
        if let Some(inner) = err.get_ref() {
            if let Some(re) = inner.downcast_ref::<rustls::Error>() {
                return Some(re);
            }
        }
        // Walk the source chain for deeper wrapping.
        let mut source = err.source();
        while let Some(e) = source {
            if let Some(re) = e.downcast_ref::<rustls::Error>() {
                return Some(re);
            }
            source = e.source();
        }
        None
    }

    let Some(rustls_err) = find_rustls_error(err) else {
        return false;
    };

    matches!(
        rustls_err,
        rustls::Error::InvalidCertificate(_)
            | rustls::Error::InvalidCertRevocationList(_)
            | rustls::Error::NoCertificatesPresented
            | rustls::Error::UnsupportedNameType
            | rustls::Error::PeerIncompatible(_)
    )
}

/// Whether a relay's `OK false <message>` denial during NIP-42 auth is
/// terminal, per the NIP-01 machine-readable prefixes the relay actually
/// sends (`crates/buzz-relay/src/handlers/auth.rs`).
///
/// `error:` marks the relay's own dependency failures (e.g. a ban-state DB
/// lookup that couldn't run) — the relay is failing closed on itself, not
/// rejecting the caller, and a later attempt can succeed once the
/// dependency recovers. `invalid:`, `auth-required:`, `restricted:`, and
/// `blocked:` are explicit rejections of this identity/config (bad
/// signature, ban, non-member, allowlist denial) that retrying without
/// changing anything cannot fix. An unrecognized prefix is treated as
/// terminal — failing fast on an unknown denial is safer than retrying one
/// that might be a real rejection.
fn is_terminal_auth_failure(message: &str) -> bool {
    !message.trim_start().starts_with("error:")
}

/// Retry `op` with bounded jittered backoff, stopping immediately on a
/// terminal error (see [`is_terminal_connect_error`]). Used by
/// `HarnessRelay::connect()` so a transient failure during the initial
/// WebSocket/NIP-42 handshake — e.g. a dropped connection on a spotty link —
/// doesn't fail agent startup outright.
///
/// Generic over the success type so the backoff/classification logic can be
/// exercised in tests without a real socket. Returns the last transient
/// error if all attempts are exhausted.
async fn retry_initial_connect<F, Fut, T>(mut op: F) -> Result<T, RelayError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, RelayError>>,
{
    let mut last_err = None;

    for (attempt, delay) in std::iter::once(None)
        .chain(STARTUP_CONNECT_BACKOFFS.iter().map(|d| Some(*d)))
        .enumerate()
    {
        if let Some(base) = delay {
            let jittered = jittered_duration(base);
            info!(
                "retrying initial relay connect (attempt {attempt}) in {:.1}s",
                jittered.as_secs_f64()
            );
            tokio::time::sleep(jittered).await;
        }

        match op().await {
            Ok(v) => return Ok(v),
            Err(e) if is_terminal_connect_error(&e) => {
                warn!("initial relay connect failed with terminal error: {e}");
                return Err(e);
            }
            Err(e) => {
                warn!("initial relay connect attempt {attempt} failed: {e}");
                last_err = Some(e);
            }
        }
    }

    Err(last_err.unwrap_or(RelayError::ConnectionClosed))
}

/// Perform a single WebSocket connect + NIP-42 auth handshake.
///
/// Returns `(ws, buffer)` on success.
async fn do_connect(
    relay_url: &str,
    keys: &Keys,
    auth_tag: Option<&nostr::Tag>,
) -> Result<(WsStream, ingress::HandshakeBuffer), RelayError> {
    let parsed = relay_url
        .parse::<url::Url>()
        .map_err(|e| RelayError::Http(format!("invalid relay URL: {e}")))?;

    let (ws, _response) = tokio::time::timeout(CONNECT_TIMEOUT, connect_async(parsed.as_str()))
        .await
        .map_err(|_| RelayError::ConnectionClosed)? // timeout → treat as connection failure
        .map_err(|e| RelayError::WebSocket(Box::new(e)))?;
    debug!("connected to relay at {relay_url}");

    let mut ws = ws;
    let mut buffer = ingress::HandshakeBuffer::empty();

    let challenge = ingress::wait_for_auth_challenge(&mut ws, &mut buffer, AUTH_TIMEOUT).await?;

    send_auth_response(&mut ws, &challenge, relay_url, keys, auth_tag).await?;

    let event_id = {
        // We need the event_id that was just sent. Re-derive it by signing again
        // just to get the ID — but that's wasteful. Instead, parse the last sent
        // message. Simpler: wait_for_ok accepts any OK (we just sent one event).
        // The event_id in the OK will match whatever we sent.
        // We'll accept the first OK we receive.
        let ok = ingress::wait_for_any_ok(&mut ws, &mut buffer, AUTH_TIMEOUT).await?;
        if !ok.accepted {
            return Err(RelayError::AuthFailed(ok.message));
        }
        ok.event_id
    };

    debug!("NIP-42 authentication successful (event {event_id})");
    Ok((ws, buffer))
}

#[cfg(test)]
mod tests {
    use super::*;
    use buzz_core::kind::KIND_GIT_REPO_ANNOUNCEMENT;

    #[test]
    fn relay_ws_to_http_plain() {
        assert_eq!(
            relay_ws_to_http("ws://localhost:3000"),
            "http://localhost:3000"
        );
    }

    #[test]
    fn relay_ws_to_http_secure() {
        assert_eq!(
            relay_ws_to_http("wss://relay.example.com"),
            "https://relay.example.com"
        );
    }

    #[test]
    fn relay_ws_to_http_strips_trailing_slash() {
        assert_eq!(
            relay_ws_to_http("ws://localhost:3000/"),
            "http://localhost:3000"
        );
    }

    #[test]
    fn relay_ws_to_http_with_path() {
        assert_eq!(
            relay_ws_to_http("wss://relay.example.com/nostr"),
            "https://relay.example.com/nostr"
        );
    }

    #[test]
    fn relay_ws_to_http_with_port_and_path() {
        assert_eq!(
            relay_ws_to_http("wss://relay.example.com:4000/ws"),
            "https://relay.example.com:4000/ws"
        );
    }

    #[test]
    fn channel_sub_id_format() {
        let uuid = Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap();
        assert_eq!(
            channel_sub_id(uuid),
            "ch-550e8400-e29b-41d4-a716-446655440000"
        );
    }

    #[test]
    fn channel_id_from_sub_id_roundtrip() {
        let uuid = Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap();
        let sub_id = channel_sub_id(uuid);
        let recovered = channel_id_from_sub_id(&sub_id).unwrap();
        assert_eq!(recovered, uuid);
    }

    #[test]
    fn channel_id_from_sub_id_invalid_prefix() {
        assert!(channel_id_from_sub_id("sub-550e8400-e29b-41d4-a716-446655440000").is_none());
    }

    #[test]
    fn channel_id_from_sub_id_invalid_uuid() {
        assert!(channel_id_from_sub_id("ch-not-a-uuid").is_none());
    }

    #[test]
    fn channel_id_from_sub_id_empty() {
        assert!(channel_id_from_sub_id("").is_none());
    }

    fn meta_event(uuid: Uuid, name: &str, extra: &[&str]) -> serde_json::Value {
        let mut tags = vec![
            serde_json::json!(["d", uuid.to_string()]),
            serde_json::json!(["name", name]),
        ];
        // `extra` is a flat list of single-value tag names (e.g. archived=true).
        for pair in extra.chunks(2) {
            match pair {
                [k, v] => tags.push(serde_json::json!([k, v])),
                [k] => tags.push(serde_json::json!([k])),
                _ => {}
            }
        }
        serde_json::json!({ "tags": tags })
    }

    #[test]
    fn merge_discovered_channels_preserves_missing_metadata_as_unknown() {
        let channel = Uuid::new_v4();
        let map = merge_discovered_channels(vec![channel], &serde_json::json!([]));
        assert_eq!(map[&channel].channel_type, "unknown");
    }

    #[test]
    fn merge_discovered_channels_uses_declared_dm_type_without_hidden_hint() {
        let channel = Uuid::new_v4();
        let meta = serde_json::json!([meta_event(channel, "dm", &["t", "dm"])]);
        let map = merge_discovered_channels(vec![channel], &meta);
        assert_eq!(map[&channel].channel_type, "dm");
    }

    #[test]
    fn merge_discovered_channels_skips_archived_metadata() {
        let live = Uuid::new_v4();
        let archived = Uuid::new_v4();
        let meta = serde_json::json!([
            meta_event(live, "live", &[]),
            meta_event(archived, "dead", &["archived", "true"]),
        ]);

        let map = merge_discovered_channels(vec![live, archived], &meta);

        assert!(map.contains_key(&live), "non-archived channel is kept");
        assert!(
            !map.contains_key(&archived),
            "archived=true channel is skipped from the subscribe set"
        );
        assert_eq!(map.len(), 1);
    }

    #[test]
    fn merge_discovered_channels_skips_archived_even_when_still_a_member() {
        // The offline feeder: the agent is still listed as a member
        // (uuid present in channel_uuids, the kind:39002 membership set), but the
        // channel was reaped while the agent was offline. Even though the agent
        // missed the eviction CLOSED, the archived=true kind:39000 makes the
        // client skip re-subscribing on reconnect — proving (b) closes the loop
        // independently of the relay-side eviction.
        let reaped = Uuid::new_v4();
        let meta = serde_json::json!([meta_event(reaped, "reaped", &["archived", "true"])]);

        let map = merge_discovered_channels(vec![reaped], &meta);

        assert!(
            map.is_empty(),
            "a still-member but archived channel is not re-subscribed"
        );
    }

    #[test]
    fn merge_discovered_channels_archived_false_is_kept() {
        // An explicit archived=false (e.g. after unarchive) must NOT be skipped.
        let ch = Uuid::new_v4();
        let meta = serde_json::json!([meta_event(ch, "back", &["archived", "false"])]);

        let map = merge_discovered_channels(vec![ch], &meta);

        assert!(map.contains_key(&ch), "archived=false is treated as live");
    }

    #[test]
    fn parse_ok_accepted() {
        let text = r#"["OK","abc123",true,""]"#;
        let msg = parse_relay_message(text).unwrap();
        match msg {
            RelayMessage::Ok {
                event_id,
                accepted,
                message,
            } => {
                assert_eq!(event_id, "abc123");
                assert!(accepted);
                assert_eq!(message, "");
            }
            _ => panic!("expected Ok"),
        }
    }

    #[test]
    fn parse_ok_rejected() {
        let text = r#"["OK","abc123",false,"blocked: spam"]"#;
        let msg = parse_relay_message(text).unwrap();
        match msg {
            RelayMessage::Ok {
                event_id,
                accepted,
                message,
            } => {
                assert_eq!(event_id, "abc123");
                assert!(!accepted);
                assert_eq!(message, "blocked: spam");
            }
            _ => panic!("expected Ok"),
        }
    }

    #[test]
    fn parse_eose() {
        let text = r#"["EOSE","sub-1"]"#;
        let msg = parse_relay_message(text).unwrap();
        match msg {
            RelayMessage::Eose { subscription_id } => {
                assert_eq!(subscription_id, "sub-1");
            }
            _ => panic!("expected Eose"),
        }
    }

    #[test]
    fn parse_notice() {
        let text = r#"["NOTICE","hello from relay"]"#;
        let msg = parse_relay_message(text).unwrap();
        match msg {
            RelayMessage::Notice { message } => {
                assert_eq!(message, "hello from relay");
            }
            _ => panic!("expected Notice"),
        }
    }

    #[test]
    fn parse_notice_empty() {
        let text = r#"["NOTICE"]"#;
        let msg = parse_relay_message(text).unwrap();
        match msg {
            RelayMessage::Notice { message } => {
                assert_eq!(message, "");
            }
            _ => panic!("expected Notice"),
        }
    }

    #[test]
    fn parse_auth() {
        let text = r#"["AUTH","some-challenge-string"]"#;
        let msg = parse_relay_message(text).unwrap();
        match msg {
            RelayMessage::Auth { challenge } => {
                assert_eq!(challenge, "some-challenge-string");
            }
            _ => panic!("expected Auth"),
        }
    }

    #[test]
    fn parse_closed() {
        let text = r#"["CLOSED","sub-2","error: rate-limited"]"#;
        let msg = parse_relay_message(text).unwrap();
        match msg {
            RelayMessage::Closed {
                subscription_id,
                message,
            } => {
                assert_eq!(subscription_id, "sub-2");
                assert_eq!(message, "error: rate-limited");
            }
            _ => panic!("expected Closed"),
        }
    }

    #[test]
    fn parse_closed_no_message() {
        let text = r#"["CLOSED","sub-3"]"#;
        let msg = parse_relay_message(text).unwrap();
        match msg {
            RelayMessage::Closed {
                subscription_id,
                message,
            } => {
                assert_eq!(subscription_id, "sub-3");
                assert_eq!(message, "");
            }
            _ => panic!("expected Closed"),
        }
    }

    #[test]
    fn parse_unknown_type_returns_error() {
        let text = r#"["UNKNOWN","data"]"#;
        let result = parse_relay_message(text);
        assert!(result.is_err());
        match result.unwrap_err() {
            RelayError::UnexpectedMessage(msg) => {
                assert!(msg.contains("unknown message type"));
            }
            e => panic!("expected UnexpectedMessage, got {e:?}"),
        }
    }

    #[test]
    fn parse_invalid_json_returns_error() {
        let text = "not json at all";
        let result = parse_relay_message(text);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), RelayError::Json(_)));
    }

    #[test]
    fn parse_empty_array_returns_error() {
        let text = "[]";
        let result = parse_relay_message(text);
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            RelayError::UnexpectedMessage(_)
        ));
    }

    #[test]
    fn parse_auth_missing_challenge_returns_error() {
        let text = r#"["AUTH"]"#;
        let result = parse_relay_message(text);
        assert!(result.is_err());
    }

    #[test]
    fn parse_eose_missing_sub_id_returns_error() {
        let text = r#"["EOSE"]"#;
        let result = parse_relay_message(text);
        assert!(result.is_err());
    }

    #[test]
    fn subscription_id_starts_with_ch_prefix() {
        let uuid = Uuid::new_v4();
        let sub_id = channel_sub_id(uuid);
        assert!(sub_id.starts_with("ch-"));
    }

    #[test]
    fn subscription_id_contains_full_uuid() {
        let uuid = Uuid::parse_str("12345678-1234-5678-1234-567812345678").unwrap();
        let sub_id = channel_sub_id(uuid);
        assert_eq!(sub_id, "ch-12345678-1234-5678-1234-567812345678");
    }

    /// Build a real signed Nostr event for testing BgState.
    ///
    /// Uses `custom_created_at` so tests can control the timestamp.
    /// The event ID is determined by the nostr signing process — we don't
    /// control it, but we return it so callers can use it for dedup tests.
    fn make_test_event(keys: &nostr::Keys, created_at_secs: u64) -> Event {
        let ts = nostr::Timestamp::from(created_at_secs);
        EventBuilder::new(nostr::Kind::TextNote, "test")
            .tags([])
            .custom_created_at(ts)
            .sign_with_keys(keys)
            .expect("signing should succeed")
    }

    async fn test_ws_pair() -> (WsStream, WebSocketStream<tokio::net::TcpStream>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test websocket");
        let address = listener.local_addr().expect("read test address");
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept test websocket");
            tokio_tungstenite::accept_async(stream)
                .await
                .expect("complete server websocket handshake")
        });
        let (client, _) = connect_async(format!("ws://{address}"))
            .await
            .expect("connect test websocket");
        (client, server.await.expect("join test websocket server"))
    }

    async fn next_test_frame(
        server: &mut WebSocketStream<tokio::net::TcpStream>,
    ) -> serde_json::Value {
        let message = timeout(Duration::from_secs(1), server.next())
            .await
            .expect("timed out waiting for websocket frame")
            .expect("test websocket closed")
            .expect("read test websocket frame");
        serde_json::from_str(message.to_text().expect("expected text frame"))
            .expect("parse test websocket frame")
    }

    fn test_channel_filter() -> ChannelFilter {
        ChannelFilter {
            kinds: Some(vec![9]),
            require_mention: false,
        }
    }

    fn seed_test_subscription(state: &mut BgState, channel_id: Uuid) {
        apply_command_to_state(
            state,
            RelayCommand::Subscribe {
                channel_id,
                filter: test_channel_filter(),
                replay_since: Some(1_000),
            },
        );
    }

    #[tokio::test]
    async fn fresh_reconnect_preserves_gate_until_pending_replay_resumes() {
        let (mut client, mut server) = test_ws_pair().await;
        let (_cmd_tx, mut cmd_rx) = mpsc::channel(1);
        let mut state = BgState::new();
        let channel_id = Uuid::new_v4();
        seed_test_subscription(&mut state, channel_id);
        state.rate_limit_gate = Some(tokio::time::Instant::now() + Duration::from_millis(150));

        let result =
            resubscribe_after_reconnect(&mut client, &mut cmd_rx, &mut state, "agent-pubkey", true)
                .await;

        assert!(matches!(result, ResubscribeResult::Ok));
        assert!(state.rate_limit_gate.is_some());
        assert!(state.rate_limited_pending.contains_key(&channel_id));
        assert!(
            timeout(Duration::from_millis(50), server.next())
                .await
                .is_err(),
            "fresh reconnect must not send REQ while the shared quota gate is active"
        );

        tokio::time::sleep(Duration::from_millis(125)).await;
        assert_eq!(
            drain_rate_limited_pending(&mut client, &mut state, "agent-pubkey", 1).await,
            1
        );
        let frame = next_test_frame(&mut server).await;
        assert_eq!(frame[0], "REQ");
        assert_eq!(frame[1], channel_sub_id(channel_id));
    }

    #[tokio::test]
    async fn subscribe_during_replay_pacing_is_sent_on_live_socket() {
        let (client, mut server) = test_ws_pair().await;
        let (cmd_tx, mut cmd_rx) = mpsc::channel(4);
        let mut state = BgState::new();
        let replayed_channel = Uuid::new_v4();
        let deferred_channel = Uuid::new_v4();
        seed_test_subscription(&mut state, replayed_channel);

        let task = tokio::spawn(async move {
            let mut client = client;
            let result = resubscribe_after_reconnect(
                &mut client,
                &mut cmd_rx,
                &mut state,
                "agent-pubkey",
                true,
            )
            .await;
            (result, state)
        });

        let replay = next_test_frame(&mut server).await;
        assert_eq!(replay[1], channel_sub_id(replayed_channel));
        cmd_tx
            .send(RelayCommand::Subscribe {
                channel_id: deferred_channel,
                filter: test_channel_filter(),
                replay_since: Some(2_000),
            })
            .await
            .expect("queue subscribe during pacing");

        let deferred = next_test_frame(&mut server).await;
        assert_eq!(deferred[0], "REQ");
        assert_eq!(deferred[1], channel_sub_id(deferred_channel));
        let (result, state) = task.await.expect("join resubscribe task");
        assert!(matches!(result, ResubscribeResult::Ok));
        assert!(state.active_subscriptions.contains_key(&deferred_channel));
    }

    #[tokio::test]
    async fn unsubscribe_during_replay_pacing_sends_close_on_live_socket() {
        let (client, mut server) = test_ws_pair().await;
        let (cmd_tx, mut cmd_rx) = mpsc::channel(4);
        let mut state = BgState::new();
        let channel_id = Uuid::new_v4();
        seed_test_subscription(&mut state, channel_id);

        let task = tokio::spawn(async move {
            let mut client = client;
            let result = resubscribe_after_reconnect(
                &mut client,
                &mut cmd_rx,
                &mut state,
                "agent-pubkey",
                true,
            )
            .await;
            (result, state)
        });

        let replay = next_test_frame(&mut server).await;
        assert_eq!(replay[1], channel_sub_id(channel_id));
        cmd_tx
            .send(RelayCommand::Unsubscribe { channel_id })
            .await
            .expect("queue unsubscribe during pacing");

        let close = next_test_frame(&mut server).await;
        assert_eq!(close, json!(["CLOSE", channel_sub_id(channel_id)]));
        let (result, state) = task.await.expect("join resubscribe task");
        assert!(matches!(result, ResubscribeResult::Ok));
        assert!(!state.active_subscriptions.contains_key(&channel_id));
    }

    #[tokio::test]
    async fn publish_during_replay_pacing_is_sent_on_live_socket() {
        let (client, mut server) = test_ws_pair().await;
        let (cmd_tx, mut cmd_rx) = mpsc::channel(4);
        let mut state = BgState::new();
        let channel_id = Uuid::new_v4();
        seed_test_subscription(&mut state, channel_id);
        let event = make_test_event(&nostr::Keys::generate(), 2_000);
        let event_id = event.id.to_hex();

        let task = tokio::spawn(async move {
            let mut client = client;
            let result = resubscribe_after_reconnect(
                &mut client,
                &mut cmd_rx,
                &mut state,
                "agent-pubkey",
                true,
            )
            .await;
            result
        });

        let replay = next_test_frame(&mut server).await;
        assert_eq!(replay[1], channel_sub_id(channel_id));
        cmd_tx
            .send(RelayCommand::PublishEvent {
                event: Box::new(event),
            })
            .await
            .expect("queue publish during pacing");

        let publish = next_test_frame(&mut server).await;
        assert_eq!(publish[0], "EVENT");
        assert_eq!(publish[1]["id"], event_id);
        assert!(matches!(
            task.await.expect("join resubscribe task"),
            ResubscribeResult::Ok
        ));
    }

    #[test]
    fn failed_replay_retains_deferred_subscription_intent_in_fifo_order() {
        let mut state = BgState::new();
        let kept_channel = Uuid::new_v4();
        let removed_channel = Uuid::new_v4();
        seed_test_subscription(&mut state, removed_channel);
        let event = make_test_event(&nostr::Keys::generate(), 2_000);
        let mut deferred = VecDeque::from([
            RelayCommand::Subscribe {
                channel_id: kept_channel,
                filter: test_channel_filter(),
                replay_since: Some(2_000),
            },
            RelayCommand::Unsubscribe {
                channel_id: removed_channel,
            },
            RelayCommand::PublishEvent {
                event: Box::new(event),
            },
        ]);

        retain_deferred_command_intent(&mut state, &mut deferred);

        assert!(deferred.is_empty());
        assert!(state.active_subscriptions.contains_key(&kept_channel));
        assert!(!state.active_subscriptions.contains_key(&removed_channel));
    }

    #[test]
    fn bg_state_dedup_first_event_accepted() {
        let mut state = BgState::new();
        let channel_id = Uuid::new_v4();
        let keys = nostr::Keys::generate();
        let event = make_test_event(&keys, 1_000_000);
        assert!(
            state.record_event(channel_id, &event),
            "first event should be accepted"
        );
    }

    #[test]
    fn bg_state_dedup_duplicate_rejected() {
        let mut state = BgState::new();
        let channel_id = Uuid::new_v4();
        let keys = nostr::Keys::generate();
        let event = make_test_event(&keys, 1_000_000);
        assert!(
            state.record_event(channel_id, &event),
            "first should be accepted"
        );
        assert!(
            !state.record_event(channel_id, &event),
            "duplicate should be rejected"
        );
    }

    #[test]
    fn bg_state_dedup_different_ids_both_accepted() {
        let mut state = BgState::new();
        let channel_id = Uuid::new_v4();
        // Two different keys → two different event IDs.
        let keys1 = nostr::Keys::generate();
        let keys2 = nostr::Keys::generate();
        let event1 = make_test_event(&keys1, 1_000_000);
        let event2 = make_test_event(&keys2, 1_000_001);
        assert!(state.record_event(channel_id, &event1));
        assert!(state.record_event(channel_id, &event2));
    }

    #[test]
    fn bg_state_last_seen_set_on_first_event() {
        let mut state = BgState::new();
        let channel_id = Uuid::new_v4();
        let keys = nostr::Keys::generate();
        let event = make_test_event(&keys, 1_700_000);
        state.record_event(channel_id, &event);
        assert_eq!(state.last_seen.get(&channel_id).copied(), Some(1_700_000));
    }

    #[test]
    fn bg_state_last_seen_advances_on_newer_event() {
        let mut state = BgState::new();
        let channel_id = Uuid::new_v4();
        let keys1 = nostr::Keys::generate();
        let keys2 = nostr::Keys::generate();
        let event1 = make_test_event(&keys1, 1_700_000);
        let event2 = make_test_event(&keys2, 1_800_000);
        state.record_event(channel_id, &event1);
        state.record_event(channel_id, &event2);
        assert_eq!(state.last_seen.get(&channel_id).copied(), Some(1_800_000));
    }

    #[test]
    fn bg_state_last_seen_does_not_regress_on_older_event() {
        let mut state = BgState::new();
        let channel_id = Uuid::new_v4();
        let keys1 = nostr::Keys::generate();
        let keys2 = nostr::Keys::generate();
        let event_new = make_test_event(&keys1, 1_800_000);
        let event_old = make_test_event(&keys2, 1_700_000);
        state.record_event(channel_id, &event_new);
        state.record_event(channel_id, &event_old);
        // last_seen should remain at the higher timestamp
        assert_eq!(state.last_seen.get(&channel_id).copied(), Some(1_800_000));
    }

    #[test]
    fn bg_state_last_seen_independent_per_channel() {
        let mut state = BgState::new();
        let ch1 = Uuid::new_v4();
        let ch2 = Uuid::new_v4();
        let keys1 = nostr::Keys::generate();
        let keys2 = nostr::Keys::generate();
        let event1 = make_test_event(&keys1, 1_000_000);
        let event2 = make_test_event(&keys2, 2_000_000);
        state.record_event(ch1, &event1);
        state.record_event(ch2, &event2);
        assert_eq!(state.last_seen.get(&ch1).copied(), Some(1_000_000));
        assert_eq!(state.last_seen.get(&ch2).copied(), Some(2_000_000));
    }

    /// Two-generation dedup: no amnesia window on rotation.
    ///
    /// The old implementation cleared the entire set at 12_001, creating a gap
    /// where all previously-seen IDs became eligible again. The new TwoGenDedup
    /// rotates at SEEN_ID_LIMIT/2 = 6_000, keeping the previous generation so
    /// IDs from both generations are still recognised as duplicates.
    #[test]
    fn bg_state_two_gen_dedup_no_amnesia_on_rotation() {
        let mut dedup = TwoGenDedup::new(SEEN_ID_LIMIT);

        // Fill current generation to the rotation threshold (limit/2 = 6_000).
        // After inserting the 6_000th item, current rotates into previous.
        let mut ids: Vec<String> = Vec::new();
        for i in 0u64..6_000 {
            let id = format!("{:0>64x}", i);
            ids.push(id.clone());
            dedup.insert(id);
        }

        // All 6_000 IDs were rotated into `previous`. `current` is now empty.
        // They must still be recognised as duplicates.
        for id in &ids {
            assert!(
                dedup.contains(id),
                "rotated ID {id} should still be a duplicate"
            );
        }

        // New IDs after rotation must be accepted.
        let new_id = format!("{:0>64x}", 99_999u64);
        assert!(
            dedup.insert(new_id.clone()),
            "new ID after rotation should be accepted"
        );
        assert!(
            dedup.contains(&new_id),
            "new ID should be found after insert"
        );
    }

    #[test]
    fn bg_state_two_gen_dedup_duplicate_rejected_across_generations() {
        let mut dedup = TwoGenDedup::new(12);
        // limit/2 = 6, so rotation happens at 6 inserts.
        for i in 0u64..6 {
            dedup.insert(format!("id-{i}"));
        }
        // id-0 is now in `previous` (rotated). Inserting it again must return false.
        assert!(
            !dedup.insert("id-0".to_string()),
            "cross-generation duplicate must be rejected"
        );
    }

    #[test]
    fn bg_state_seen_ids_cleared_at_limit() {
        // Compatibility test: BgState.record_event still deduplicates correctly
        // after the TwoGenDedup rotation threshold is crossed.
        let mut state = BgState::new();
        let channel_id = Uuid::new_v4();

        // Insert SEEN_ID_LIMIT/2 synthetic IDs to trigger the first rotation.
        for i in 0u64..(SEEN_ID_LIMIT as u64 / 2) {
            state.seen_ids.insert(format!("{:0>64x}", i));
        }

        // The first generation has been rotated into `previous`. All IDs are
        // still present across the two generations — no amnesia window.
        assert!(
            state
                .seen_ids
                .contains("0000000000000000000000000000000000000000000000000000000000000000"),
            "first ID should still be recognised after rotation"
        );

        // A new real event should be accepted (not a duplicate).
        let keys = nostr::Keys::generate();
        let event = make_test_event(&keys, 1_000_000);
        assert!(
            state.record_event(channel_id, &event),
            "new event after rotation should be accepted"
        );

        // The same event must be rejected as a duplicate.
        assert!(
            !state.record_event(channel_id, &event),
            "duplicate event after rotation should be rejected"
        );
    }

    /// Test 8: channel_dropped_since records the OLDEST dropped timestamp.
    ///
    /// Simulates the backpressure path directly on BgState:
    /// - First drop at ts=1000 → entry is 1000
    /// - Second drop at ts=2000 (later) → entry stays 1000 (min)
    /// - Third drop at ts=500 (earlier) → entry updates to 500 (min)
    #[test]
    fn acp_records_channel_dropped_since_on_backpressure() {
        let mut state = BgState::new();
        let channel_id = Uuid::new_v4();

        // Simulate the backpressure path: record ts=1000.
        let ts1: u64 = 1_000;
        state
            .channel_dropped_since
            .entry(channel_id)
            .and_modify(|d| *d = (*d).min(ts1))
            .or_insert(ts1);
        assert_eq!(
            state.channel_dropped_since.get(&channel_id).copied(),
            Some(1_000),
            "first drop should record ts=1000"
        );

        // Later timestamp (2000) — entry should stay at 1000.
        let ts2: u64 = 2_000;
        state
            .channel_dropped_since
            .entry(channel_id)
            .and_modify(|d| *d = (*d).min(ts2))
            .or_insert(ts2);
        assert_eq!(
            state.channel_dropped_since.get(&channel_id).copied(),
            Some(1_000),
            "later drop should not overwrite earlier timestamp"
        );

        // Earlier timestamp (500) — entry should update to 500.
        let ts3: u64 = 500;
        state
            .channel_dropped_since
            .entry(channel_id)
            .and_modify(|d| *d = (*d).min(ts3))
            .or_insert(ts3);
        assert_eq!(
            state.channel_dropped_since.get(&channel_id).copied(),
            Some(500),
            "earlier drop should update entry to 500"
        );
    }

    /// Test 9: reconnect since filter = min(last_seen, channel_dropped_since) - SINCE_SKEW_SECS.
    ///
    /// With last_seen=1000 and channel_dropped_since=900, the effective since
    /// passed to send_subscribe should be min(1000, 900) - SINCE_SKEW_SECS = 895.
    #[test]
    fn acp_reconnect_uses_dropped_since_for_replay() {
        let mut state = BgState::new();
        let channel_id = Uuid::new_v4();

        // Set up state: last_seen=1000, channel_dropped_since=900.
        state.last_seen.insert(channel_id, 1_000);
        state.channel_dropped_since.insert(channel_id, 900);

        // Compute the since value the reconnect path would use.
        let since = state.channel_since(&channel_id);

        // The since passed to send_subscribe (which subtracts SINCE_SKEW_SECS internally).
        assert_eq!(since, Some(900), "since should be min(1000, 900) = 900");

        // After subtracting skew (as send_subscribe does), the REQ filter value is:
        let req_since = since.unwrap().saturating_sub(SINCE_SKEW_SECS);
        assert_eq!(
            req_since, 895,
            "REQ since filter should be 900 - {} = 895",
            SINCE_SKEW_SECS
        );

        // Simulate clearing after resubscribe.
        state.channel_dropped_since.remove(&channel_id);
        assert!(
            !state.channel_dropped_since.contains_key(&channel_id),
            "channel_dropped_since should be cleared after resubscribe"
        );
    }

    #[test]
    fn dynamic_subscribe_records_membership_replay_floor() {
        let mut state = BgState::new();
        state.startup_watermark = Some(2_000);
        let channel_id = Uuid::new_v4();
        let membership_ts = 10_000;
        let filter = ChannelFilter {
            kinds: Some(vec![9]),
            require_mention: true,
        };

        apply_command_to_state(
            &mut state,
            RelayCommand::Subscribe {
                channel_id,
                filter,
                replay_since: Some(membership_ts),
            },
        );

        assert_eq!(
            state.subscribe_since.get(&channel_id).copied(),
            Some(membership_ts),
            "dynamic channel subscriptions should replay from the membership notification, not startup"
        );
        assert_eq!(
            state.channel_since(&channel_id),
            Some(membership_ts),
            "channel_since should use the dynamic replay floor until an event is seen"
        );
    }

    /// Membership dedup must NOT contaminate per-channel `last_seen`.
    /// Using `record_event()` for membership notifications would update
    /// `last_seen[channel_uuid]`, causing channel resubscribe to use a
    /// membership timestamp as the `since` filter — skipping channel events.
    /// The fix uses `seen_ids.insert()` directly.
    #[test]
    fn membership_dedup_does_not_touch_last_seen() {
        let mut state = BgState::new();
        let channel_id = Uuid::new_v4();
        let keys = nostr::Keys::generate();

        // Simulate: a channel event sets last_seen to 1000.
        let event1 = make_test_event(&keys, 1_000);
        assert!(state.record_event(channel_id, &event1));
        assert_eq!(state.last_seen.get(&channel_id).copied(), Some(1_000));

        // Simulate: a membership notification for the same channel at ts=2000.
        // This should go through seen_ids only, NOT update last_seen.
        let membership_event = make_test_event(&keys, 2_000);
        let membership_id = membership_event.id.to_hex();
        assert!(
            state.seen_ids.insert(membership_id),
            "membership event should be accepted by dedup"
        );
        // last_seen must still be 1000, not 2000.
        assert_eq!(
            state.last_seen.get(&channel_id).copied(),
            Some(1_000),
            "membership dedup must not contaminate last_seen"
        );
    }

    /// On membership backpressure (TrySendError::Full), the dedup ID must
    /// be removed from seen_ids so reconnect replay can re-deliver the event.
    /// Without this, a dropped membership notification would be permanently
    /// rejected as a duplicate on replay.
    #[test]
    fn membership_backpressure_removes_dedup_id() {
        let mut state = BgState::new();
        let keys = nostr::Keys::generate();

        let event = make_test_event(&keys, 1_000);
        let event_id_hex = event.id.to_hex();

        // Insert into dedup (simulating the pre-try_send path).
        assert!(state.seen_ids.insert(event_id_hex.clone()));
        assert!(state.seen_ids.contains(&event_id_hex));

        // Simulate backpressure: remove the ID (matching the production code).
        state.seen_ids.remove(&event_id_hex);

        // The ID should now be accepted again on replay.
        assert!(
            state.seen_ids.insert(event_id_hex),
            "after backpressure removal, replay must be accepted"
        );
    }

    // ── Project dedup is separate from channel dedup ─────────────────────────

    /// One event that is legitimately deliverable on both surfaces: an `h` tag
    /// puts it on a channel REQ, and an `e`-root tag routes it on the
    /// watched-root REQ.
    /// An event both the enrolment and the watched filter admit.
    ///
    /// A kind-1 comment on `root`, on the discovered repository, tagging this
    /// agent — which is exactly the shape the relay may deliver under both
    /// subscription ids at once.
    fn enrolled_and_watched_event(keys: &nostr::Keys, root: &str, ts: u64) -> Event {
        EventBuilder::new(nostr::Kind::TextNote, "on both project subscriptions")
            .custom_created_at(nostr::Timestamp::from(ts))
            .tags([
                nostr::Tag::parse(vec![
                    "e".to_string(),
                    root.to_string(),
                    String::new(),
                    "root".to_string(),
                ])
                .expect("e tag"),
                nostr::Tag::parse(vec!["a".to_string(), test_coordinate()]).expect("a tag"),
                nostr::Tag::parse(vec!["p".to_string(), test_agent_hex()]).expect("p tag"),
            ])
            .sign_with_keys(keys)
            .expect("signing should succeed")
    }

    fn mixed_surface_event(keys: &nostr::Keys, channel_id: Uuid, root: &str, ts: u64) -> Event {
        EventBuilder::new(nostr::Kind::TextNote, "on both surfaces")
            .custom_created_at(nostr::Timestamp::from(ts))
            .tags([
                nostr::Tag::parse(vec!["h".to_string(), channel_id.to_string()]).expect("h tag"),
                nostr::Tag::parse(vec![
                    "e".to_string(),
                    root.to_string(),
                    String::new(),
                    "root".to_string(),
                ])
                .expect("e tag"),
            ])
            .sign_with_keys(keys)
            .expect("signing should succeed")
    }

    fn test_root_id() -> String {
        "a".repeat(64)
    }

    /// The agent pubkey the enrolment filter scopes to in these tests.
    fn test_agent_hex() -> String {
        "b".repeat(64)
    }

    /// The repository coordinate `watched_enrolments` enrols under.
    fn test_coordinate() -> String {
        format!("30617:{}:repo", "1".repeat(64))
    }

    /// Write `text` onto a fresh connection and let the production reader take
    /// it off again.
    ///
    /// Every narrow frame-level test goes through here rather than calling the
    /// handler with a `Message` of its own, because it cannot do the latter:
    /// only `ingress` can produce the frame the handler accepts. The frame
    /// these helpers deliver is one they wrote themselves, which is why they
    /// are not the canonical connected proof — but the step between the socket
    /// and the handler is production's here as it is there.
    async fn dispatch_over_fresh_connection(
        state: &mut BgState,
        text: String,
        event_tx: &mpsc::Sender<Option<BuzzEvent>>,
    ) -> bool {
        use futures_util::SinkExt;
        let (mut ws, mut server) = test_ws_pair().await;
        let (observer_tx, _observer_rx) = mpsc::channel(8);
        let keys = nostr::Keys::generate();
        server
            .send(Message::Text(text.into()))
            .await
            .expect("the peer writes the frame");
        let frame = match ingress::read_frame(&mut ws).await {
            ingress::FrameRead::Frame(frame) => frame,
            ingress::FrameRead::Lost => panic!("the connection dropped the frame"),
        };
        ingress::dispatch_frame(
            frame,
            &mut ws,
            event_tx,
            &observer_tx,
            state,
            &keys,
            "ws://test",
            &keys.public_key().to_hex(),
            None,
        )
        .await
            != ingress::FrameDispatch::Lost
    }

    /// Push one EVENT frame through the production dispatch path.
    ///
    /// These tests are about *which dedup set the real code spends*, so poking
    /// `BgState` by hand would assert nothing — the simulation would be the
    /// thing under test.
    async fn deliver_frame(
        state: &mut BgState,
        sub_id: &str,
        event: &Event,
        event_tx: &mpsc::Sender<Option<BuzzEvent>>,
    ) {
        let text = serde_json::to_string(&json!(["EVENT", sub_id, event])).expect("encode frame");
        let keep_going = dispatch_over_fresh_connection(state, text, event_tx).await;
        assert!(keep_going, "dispatch must not signal connection loss");
    }

    /// The write half of a scenario's relay peer.
    type PeerSink = futures_util::stream::SplitSink<
        WebSocketStream<tokio::net::TcpStream>,
        tokio_tungstenite::tungstenite::Message,
    >;

    /// Which of the peer's identities signs an inbound event.
    #[derive(Clone, Copy, Debug)]
    enum PeerSigner {
        /// The repository owner: discoverer, issue author, commenter.
        Owner,
        /// A second, unrelated owner, used to prove enrolment widening.
        SecondOwner,
    }

    /// What a scenario asks the relay peer to publish.
    ///
    /// It carries a kind, content and tags — no signature, no key, and no
    /// `Event`. Producing the signed object is the peer's job, and the peer's
    /// alone.
    struct InboundSpec {
        signer: PeerSigner,
        kind: u16,
        content: String,
        tags: Vec<Vec<String>>,
    }

    enum PeerCommand {
        Publish {
            sub_id: String,
            spec: InboundSpec,
            reply: tokio::sync::oneshot::Sender<nostr::EventId>,
        },
        /// End of stored events for one subscription — the frame a relay sends
        /// when it has finished answering a REQ from its store and everything
        /// after it is new. The peer writes it because the peer *is* the relay
        /// here; a scenario that synthesised it locally would be asserting
        /// about its own fixture.
        EndOfStoredEvents {
            sub_id: String,
            reply: tokio::sync::oneshot::Sender<()>,
        },
        /// After a reconnect, the peer writes to the replacement socket.
        Rebind {
            sink: PeerSink,
            reply: tokio::sync::oneshot::Sender<()>,
        },
    }

    /// The relay peer: the only holder of the keys inbound events are signed
    /// with, and the only writer of inbound frames.
    ///
    /// **Why a task rather than a helper the scenario calls with an `Event`.**
    /// The phase contract names "direct midpoint injection replaces the
    /// connected path" as a mutant this suite must catch, and the previous
    /// shape could not catch it. That helper took the signed `Event` as an
    /// argument, so an edit which read the frame off the socket, discarded it
    /// and passed a locally rebuilt `Message::Text` to `handle_ws_message`
    /// produced byte-identical input. No assertion separates a transported
    /// value from a reconstruction of itself, and the helper's own comment said
    /// so — which is an admission that the falsifier was open, not a proof that
    /// it was closed.
    ///
    /// What changes here is not the assertions but what the scenario *has*. The
    /// signing keys and the serialised frames are moved into this task; what
    /// crosses back is an [`nostr::EventId`] and nothing else. A midpoint
    /// injection now has nothing faithful to inject. Its best available forgery
    /// is an event signed by a key the scenario still holds — the agent's — and
    /// production refuses that twice over: `VerifiedProjectEvent::verify`
    /// rejects a signature that does not match the id, and an event genuinely
    /// signed by the agent classifies as self-authored and never queues. The
    /// mutant is caught by the production gate rather than by a test asserting
    /// about its own fixture.
    struct RelayPeer {
        tx: mpsc::Sender<PeerCommand>,
    }

    impl RelayPeer {
        /// Sign `spec` and write it, followed by a sentinel, to the socket the
        /// peer currently holds. Returns the id of the event it wrote.
        async fn publish(&self, sub_id: &str, spec: InboundSpec) -> nostr::EventId {
            let (reply, wait) = tokio::sync::oneshot::channel();
            self.tx
                .send(PeerCommand::Publish {
                    sub_id: sub_id.to_string(),
                    spec,
                    reply,
                })
                .await
                .expect("the relay peer accepts the publish");
            timeout(Duration::from_secs(5), wait)
                .await
                .expect("timed out waiting for the relay peer to publish")
                .expect("the relay peer answered")
        }

        /// Finish the stored-events prefix of `sub_id`.
        async fn end_of_stored_events(&self, sub_id: &str) {
            let (reply, wait) = tokio::sync::oneshot::channel();
            self.tx
                .send(PeerCommand::EndOfStoredEvents {
                    sub_id: sub_id.to_string(),
                    reply,
                })
                .await
                .expect("the relay peer accepts the boundary");
            timeout(Duration::from_secs(5), wait)
                .await
                .expect("timed out waiting for the relay peer's EOSE")
                .expect("the relay peer answered");
        }

        /// Point the peer at the replacement connection after a reconnect.
        async fn rebind(&self, sink: PeerSink) {
            let (reply, wait) = tokio::sync::oneshot::channel();
            self.tx
                .send(PeerCommand::Rebind { sink, reply })
                .await
                .expect("the relay peer accepts the rebind");
            timeout(Duration::from_secs(5), wait)
                .await
                .expect("timed out waiting for the relay peer to rebind")
                .expect("the relay peer answered");
        }
    }

    /// Move the peer's sockets and signing keys out of the scenario.
    ///
    /// `owner` and `second_owner` are consumed here on purpose: after this call
    /// no caller can sign as either identity, which is what makes a prepared
    /// midpoint unbuildable rather than merely discouraged.
    fn spawn_relay_peer(
        mut sink: PeerSink,
        owner: nostr::Keys,
        second_owner: nostr::Keys,
    ) -> RelayPeer {
        let (tx, mut rx) = mpsc::channel::<PeerCommand>(8);
        tokio::spawn(async move {
            use futures_util::SinkExt;
            while let Some(command) = rx.recv().await {
                match command {
                    PeerCommand::Publish {
                        sub_id,
                        spec,
                        reply,
                    } => {
                        let keys = match spec.signer {
                            PeerSigner::Owner => &owner,
                            PeerSigner::SecondOwner => &second_owner,
                        };
                        let tags: Vec<nostr::Tag> = spec
                            .tags
                            .iter()
                            .map(|tag| nostr::Tag::parse(tag.clone()).expect("peer tag parses"))
                            .collect();
                        let event = EventBuilder::new(nostr::Kind::Custom(spec.kind), spec.content)
                            .tags(tags)
                            .sign_with_keys(keys)
                            .expect("the relay peer signs");
                        let text = serde_json::to_string(&json!(["EVENT", sub_id, event]))
                            .expect("encode frame");
                        // The EVENT, and immediately behind it a sentinel. Both
                        // are read back in order by `deliver_over_connection`;
                        // see the assertion there for what the sentinel proves.
                        let sentinel =
                            serde_json::to_string(&json!(["NOTICE", sentinel_text(event.id)]))
                                .expect("encode sentinel");
                        sink.send(Message::Text(text.into()))
                            .await
                            .expect("the relay peer writes the EVENT");
                        sink.send(Message::Text(sentinel.into()))
                            .await
                            .expect("the relay peer writes the sentinel");
                        let _ = reply.send(event.id);
                    }
                    PeerCommand::EndOfStoredEvents { sub_id, reply } => {
                        let text = serde_json::to_string(&json!(["EOSE", sub_id]))
                            .expect("encode boundary");
                        let sentinel =
                            serde_json::to_string(&json!(["NOTICE", boundary_sentinel(&sub_id)]))
                                .expect("encode sentinel");
                        sink.send(Message::Text(text.into()))
                            .await
                            .expect("the relay peer writes the EOSE");
                        sink.send(Message::Text(sentinel.into()))
                            .await
                            .expect("the relay peer writes the sentinel");
                        let _ = reply.send(());
                    }
                    PeerCommand::Rebind {
                        sink: replacement,
                        reply,
                    } => {
                        sink = replacement;
                        let _ = reply.send(());
                    }
                }
            }
        });
        RelayPeer { tx }
    }

    /// The sentinel the peer writes behind every EVENT.
    ///
    /// An id is all this needs to be: it is an ordering marker, not evidence
    /// about the event's contents.
    fn sentinel_text(id: nostr::EventId) -> String {
        format!("delivered:{id}")
    }

    /// The sentinel behind an EOSE, for the same ordering reason.
    fn boundary_sentinel(sub_id: &str) -> String {
        format!("drained:{sub_id}")
    }

    /// Let the production reader take the next EVENT off the retained
    /// connection and dispatch it.
    ///
    /// Nothing here touches the frame. The bytes are whatever [`RelayPeer`]
    /// wrote, the **same registered connection** they were written to is the
    /// one they are read back from — so the socket a request was registered on
    /// is the socket its answer arrives on — and the step between the read and
    /// the handler belongs to [`ingress`], not to this function.
    ///
    /// That last part is what closes the "direct midpoint injection replaces
    /// the connected path" falsifier. This helper used to own the step: it read
    /// the frame, asserted about it, and called `handle_ws_message` itself, so
    /// an edit that discarded the transported value and passed a locally
    /// rebuilt `Message::Text` was byte-identical and went unnoticed. There is
    /// now nowhere to put that edit — `ingress::InboundFrame` cannot be
    /// constructed here and the handler takes nothing else. The envelope
    /// assertions this function used to make went with it; what a frame
    /// delivered on a registration nobody opened does is production's answer to
    /// give, and the scenario reads it downstream in the dispatch outcome.
    ///
    /// Deliberately not [`deliver_frame`]. That writes a frame of its own onto
    /// a fresh socket, so it proves nothing about the connection the requests
    /// were installed on.
    async fn deliver_over_connection(
        state: &mut BgState,
        ws: &mut WsStream,
        event_id: nostr::EventId,
        event_tx: &mpsc::Sender<Option<BuzzEvent>>,
        keys: &nostr::Keys,
    ) {
        let (observer_tx, _observer_rx) = mpsc::channel(8);
        let read = timeout(Duration::from_secs(2), ingress::read_frame(ws))
            .await
            .expect("timed out reading the EVENT off the connection");
        let frame = match read {
            ingress::FrameRead::Frame(frame) => frame,
            ingress::FrameRead::Lost => {
                panic!("the connection closed before the EVENT arrived")
            }
        };
        let outcome = ingress::dispatch_frame(
            frame,
            ws,
            event_tx,
            &observer_tx,
            state,
            keys,
            "ws://test",
            &keys.public_key().to_hex(),
            None,
        )
        .await;
        assert_eq!(
            outcome,
            ingress::FrameDispatch::Handled,
            "dispatch must not signal connection loss"
        );

        // And the sentinel is what follows it, which is what proves the read
        // above consumed the EVENT rather than peeking past it.
        let echoed = timeout(Duration::from_secs(2), ws.next())
            .await
            .expect("timed out reading the sentinel")
            .expect("the connection closed before the sentinel arrived")
            .expect("read the sentinel frame");
        let echoed: serde_json::Value =
            serde_json::from_str(echoed.to_text().expect("sentinel is text"))
                .expect("the sentinel is JSON");
        assert_eq!(
            echoed,
            json!(["NOTICE", sentinel_text(event_id)]),
            "the EVENT was not consumed from the connection — it was still \
             queued when the sentinel was read, so the frame handled above did \
             not come off this socket"
        );
    }

    /// Let the production reader take the next EOSE off the retained connection
    /// and dispatch it.
    ///
    /// The same contract as [`deliver_over_connection`], for the frame that
    /// ends a request's stored-events prefix. It matters that this goes through
    /// [`ingress::dispatch_frame`] rather than calling the registry directly:
    /// the boundary's whole job is to be attributed to the exact registration
    /// that received it, and a scenario that reached past the reader could
    /// attribute it to whatever it liked.
    async fn drain_backlog_over_connection(
        state: &mut BgState,
        ws: &mut WsStream,
        sub_id: &str,
        event_tx: &mpsc::Sender<Option<BuzzEvent>>,
        keys: &nostr::Keys,
    ) {
        let (observer_tx, _observer_rx) = mpsc::channel(8);
        let read = timeout(Duration::from_secs(2), ingress::read_frame(ws))
            .await
            .expect("timed out reading the EOSE off the connection");
        let frame = match read {
            ingress::FrameRead::Frame(frame) => frame,
            ingress::FrameRead::Lost => {
                panic!("the connection closed before the EOSE arrived")
            }
        };
        let outcome = ingress::dispatch_frame(
            frame,
            ws,
            event_tx,
            &observer_tx,
            state,
            keys,
            "ws://test",
            &keys.public_key().to_hex(),
            None,
        )
        .await;
        assert_eq!(
            outcome,
            ingress::FrameDispatch::Handled,
            "dispatch must not signal connection loss"
        );

        let echoed = timeout(Duration::from_secs(2), ws.next())
            .await
            .expect("timed out reading the sentinel")
            .expect("the connection closed before the sentinel arrived")
            .expect("read the sentinel frame");
        let echoed: serde_json::Value =
            serde_json::from_str(echoed.to_text().expect("sentinel is text"))
                .expect("the sentinel is JSON");
        assert_eq!(
            echoed,
            json!(["NOTICE", boundary_sentinel(sub_id)]),
            "the EOSE was not consumed from the connection"
        );
    }

    /// Open a project request the way production must: recorded before any
    /// frame for it is accepted. Returns the subscription id to deliver on.
    ///
    /// Every project test now goes through one of these. That is not
    /// ceremony — before the registry, these tests delivered on ids nobody had
    /// asked for and the dispatch accepted them, which was precisely the
    /// defect.
    /// Open discovery the way production does — through
    /// `send_project_discovery` against a real socket.
    ///
    /// These helpers used to call `reserve()` directly, which left the
    /// registration in a state production never produces: recorded but never
    /// written. Inbound frames were then delivered against it as though the
    /// relay had been asked. Going through the transport is the point — a
    /// helper that can fabricate "sent" is a helper that proves nothing about
    /// what the relay was actually told.
    ///
    /// They now take *filters*, because that is all the production senders
    /// take. The id and the class are the registry's, and the returned string
    /// is what it stamped — an assertion, not an argument.
    async fn send_discovery(state: &mut BgState, filters: Vec<Value>) -> ProjectSendOutcome {
        let (mut ws, _server) = test_ws_pair().await;
        send_project_discovery(&mut ws, state, filters).await
    }

    /// The enrolment state behind a watched-root REQ: `(root, is_pull_request)`.
    fn watched_enrolments(roots: &[(&str, bool)]) -> crate::project::ProjectEnrolments {
        let mut enrolments = crate::project::ProjectEnrolments::new();
        for (root, is_pull_request) in roots {
            let coordinate = format!("30617:{}:repo", "1".repeat(64));
            enrolments
                .enrol(&crate::project::EnrolmentCandidate::for_test(
                    root,
                    &coordinate,
                    &"1".repeat(64),
                    &"2".repeat(64),
                    *is_pull_request,
                ))
                .expect("a fresh root enrols");
        }
        enrolments
    }

    /// The watched filters for `roots`, **from the production builder**.
    ///
    /// It used to hand-rebuild one comments/`#e` filter. That looked equivalent
    /// — same kinds accessor, same root-tag accessor — and it was not: the real
    /// builder returns *two* filters, because a comment points at its root with
    /// lowercase `e` and a pull-request revision with uppercase `E`. The
    /// approximation therefore tested a single-filter request nobody sends, and
    /// hid that the identity could not represent a two-filter one at all.
    ///
    /// `since` is a parameter for the same reason the rest of it is derived: a
    /// window the fixture invents is a window production never asked for. The
    /// generation is *not* a parameter any more: it is allocated by the
    /// registry when the replacement installs.
    fn watched_filters(roots: &[(&str, bool)], since: u64) -> Vec<Value> {
        crate::project::watched_roots_filters(&watched_enrolments(roots), since)
    }

    /// Install the watched subscription over `roots`, through the registry's
    /// own replacement — the only route production has — and return the id it
    /// stamped.
    async fn open_watched(state: &mut BgState) -> String {
        open_watched_for(state, &[&test_root_id()]).await
    }

    async fn open_watched_for(state: &mut BgState, roots: &[&str]) -> String {
        let issues: Vec<(&str, bool)> = roots.iter().map(|r| (*r, false)).collect();
        open_watched_since(state, &issues, 0).await
    }

    async fn open_watched_since(state: &mut BgState, roots: &[(&str, bool)], since: u64) -> String {
        let (mut ws, _server) = test_ws_pair().await;
        let outcome = state
            .project_requests
            .replace_watched(&mut ws, watched_filters(roots, since))
            .await;
        assert!(
            matches!(outcome, crate::project::ReplaceOutcome::Replaced { .. }),
            "the watched replacement must install, got {outcome:?}"
        );
        installed_watched_id(state)
    }

    /// The watched id the registry currently holds as current.
    fn installed_watched_id(state: &BgState) -> String {
        state
            .project_requests
            .current_watched()
            .expect("a canonical durable record")
            .map(crate::project::watched_sub_id)
            .expect("a watched generation is installed")
    }

    // ── Enrolment history reconstruction ─────────────────────────────────────

    /// A relay peer whose socket survives across several frames.
    ///
    /// `dispatch_over_fresh_connection` builds a new pair per frame, which is
    /// right for frame-level tests and useless here: the page the driver opens
    /// in response to a boundary is written to the socket, and a test that
    /// cannot read that socket cannot see whether the walk continued.
    struct WalkHarness {
        state: BgState,
        ws: WsStream,
        server: WebSocketStream<tokio::net::TcpStream>,
        owner: nostr::Keys,
        coordinate: String,
        tx: mpsc::Sender<Option<BuzzEvent>>,
        rx: mpsc::Receiver<Option<BuzzEvent>>,
    }

    impl WalkHarness {
        async fn new() -> Self {
            Self::with_capacity(64).await
        }

        /// A harness whose run loop can only hold `capacity` events.
        ///
        /// The reconstruction's delivery is a `try_send`, so the capacity is
        /// what decides whether a root is handed on or refused — a test that
        /// wants to reason about backpressure has to own it rather than hope
        /// for it.
        async fn with_capacity(capacity: usize) -> Self {
            let (ws, server) = test_ws_pair().await;
            let owner = nostr::Keys::generate();
            let coordinate = format!("30617:{}:demo", owner.public_key().to_hex());
            let (tx, rx) = mpsc::channel(capacity);
            Self {
                state: BgState::new(),
                ws,
                server,
                owner,
                coordinate,
                tx,
                rx,
            }
        }

        /// Begin the walk through the production command path.
        async fn begin(&mut self) {
            self.state.startup_watermark = Some(10_000);
            execute_connected_command(
                &mut self.ws,
                &mut self.state,
                &test_agent_hex(),
                RelayCommand::BeginEnrolmentHistory {
                    coordinates: vec![self.coordinate.clone()],
                    agent: test_agent_hex(),
                },
            )
            .await;
        }

        /// The next REQ the driver wrote, or `None` if it wrote nothing.
        async fn next_req(&mut self) -> Option<Value> {
            let message = timeout(Duration::from_millis(250), self.server.next())
                .await
                .ok()??
                .ok()?;
            serde_json::from_str(message.to_text().ok()?).ok()
        }

        async fn req(&mut self) -> Value {
            self.next_req().await.expect("a REQ must reach the socket")
        }

        /// Feed one frame through the production ingress seam, on this socket.
        async fn feed(&mut self, frame: Value) {
            use futures_util::SinkExt;
            let (observer_tx, _observer_rx) = mpsc::channel(8);
            let keys = nostr::Keys::generate();
            let text = serde_json::to_string(&frame).expect("encode");
            self.server
                .send(Message::Text(text.into()))
                .await
                .expect("the peer writes the frame");
            let frame = match ingress::read_frame(&mut self.ws).await {
                ingress::FrameRead::Frame(frame) => frame,
                ingress::FrameRead::Lost => panic!("the connection dropped the frame"),
            };
            assert_ne!(
                ingress::dispatch_frame(
                    frame,
                    &mut self.ws,
                    &self.tx,
                    &observer_tx,
                    &mut self.state,
                    &keys,
                    "ws://test",
                    &test_agent_hex(),
                    None,
                )
                .await,
                ingress::FrameDispatch::Lost,
                "dispatch must not signal connection loss"
            );
        }

        /// Fill a page with `count` roots, then close it with its own boundary.
        async fn fill_and_close(&mut self, sub_id: &str, stamps: &[u64]) {
            for created_at in stamps {
                let root = project_root_frame(&self.owner, &self.coordinate, *created_at);
                self.feed(json!(["EVENT", sub_id, root])).await;
            }
            self.feed(json!(["EOSE", sub_id])).await;
        }

        /// Fill `req`'s page to exactly its own limit, with orderable
        /// timestamps, and close it with its own boundary.
        ///
        /// Saturation is relative to the limit the page actually asked for, not
        /// to a number the test picked: a page is saturated when the relay
        /// returns as many rows as it was allowed to, and hard-coding a count
        /// would pass or fail on the page limit rather than on the rule.
        async fn saturate(&mut self, req: &Value) {
            let limit = req[2]["limit"].as_u64().expect("a page limit");
            let id = req[1].as_str().expect("an id").to_string();
            let stamps: Vec<u64> = (0..limit).map(|i| 9_000 - i * 10).collect();
            self.fill_and_close(&id, &stamps).await;
        }

        fn degraded(&self) -> Option<&str> {
            self.state.enrolment_history_degraded.as_deref()
        }

        /// How many discovered roots are still owed to the run loop.
        ///
        /// `None` means nothing is pending, which is the only state in which
        /// the reconstruction may be spoken of as finished.
        fn retained(&self) -> Option<usize> {
            let owed: usize = self
                .state
                .replay_deliveries
                .iter()
                .map(|batch| batch.queue.len())
                .sum();
            (owed > 0).then_some(owed)
        }

        /// One more attempt to hand the retained roots on.
        fn retry_restore(&mut self) -> RestoreOutcome {
            drive_pending_restorations(&mut self.state, &self.tx)
        }
    }

    /// The walk opens its first page, and the live tail is untouched by it.
    ///
    /// The two requests are the whole shape of this correction. The tail is one
    /// standing REQ under a fixed id; the walk is a sequence of pages. Paging
    /// through the tail's identity is what made the restart case unfixable,
    /// because a fixed identity cannot carry a bound that moves.
    #[tokio::test]
    async fn the_walk_pages_without_disturbing_the_live_tail() {
        let mut h = WalkHarness::new().await;
        let tail_id = open_enrolment_for(&mut h.state, std::slice::from_ref(&h.coordinate)).await;
        // Drain the tail's own REQ, written by the replacement above onto its
        // own throwaway socket — nothing of it reaches this one.
        assert!(h.next_req().await.is_none());

        h.begin().await;
        let first = h.req().await;
        let first_id = first[1].as_str().expect("an id").to_string();
        assert_ne!(
            first_id, tail_id,
            "a history page must not wear the tail's identity"
        );
        assert!(
            h.state.project_requests.match_frame(&tail_id).is_some(),
            "the tail must still be live after a page is opened"
        );

        // A second page, after the first proves saturated.
        h.saturate(&first).await;
        let second = h.req().await;
        let second_id = second[1].as_str().expect("an id").to_string();
        assert_ne!(
            second_id, first_id,
            "each page takes a fresh identity, so a delayed frame from page one \
             cannot be handed page two's authority"
        );
        assert_ne!(second_id, tail_id);
        assert!(
            h.state.project_requests.match_frame(&tail_id).is_some(),
            "and the tail is still live after paging — it is never replaced by \
             a history request"
        );
        assert!(h.degraded().is_none(), "a healthy walk is not degraded");
    }

    /// A history page is never durable intent.
    ///
    /// Its filter carries a bound that moves, so a recorded one would re-ask,
    /// after a reconnect, for a page the cursor has already walked past — and
    /// the walk would never terminate. The registry refuses the class outright
    /// rather than checking its key.
    #[tokio::test]
    async fn a_history_page_is_never_recorded_as_durable_intent() {
        let mut h = WalkHarness::new().await;
        open_enrolment_for(&mut h.state, std::slice::from_ref(&h.coordinate)).await;
        let _ = h.next_req().await;
        h.begin().await;
        let page_id = h.req().await[1].as_str().expect("an id").to_string();

        let replayable = h
            .state
            .project_requests
            .replayable()
            .expect("durable intent resolves");
        assert!(
            !replayable.iter().any(|r| r.sub_id() == page_id),
            "a page bound to a moving cursor must not be replayed from a record"
        );
        assert!(
            !replayable
                .iter()
                .any(|r| r.sub_id().contains("enrol-history")),
            "no history identity at all belongs in durable intent"
        );
    }

    /// A predecessor page's boundary cannot certify its successor.
    ///
    /// This is the defect that re-answered four historical roots on a real
    /// relay, reproduced at the page level: an EOSE from a request that is no
    /// longer the one in flight must leave the walk exactly as it found it.
    #[tokio::test]
    async fn a_predecessors_boundary_cannot_certify_its_successor() {
        let mut h = WalkHarness::new().await;
        open_enrolment_for(&mut h.state, std::slice::from_ref(&h.coordinate)).await;
        let _ = h.next_req().await;
        h.begin().await;
        let first = h.req().await;
        let first_id = first[1].as_str().expect("an id").to_string();

        h.saturate(&first).await;
        let second_id = h.req().await[1].as_str().expect("an id").to_string();

        // The predecessor speaks again, after its successor is in flight.
        h.feed(json!(["EOSE", first_id])).await;

        assert!(
            h.next_req().await.is_none(),
            "a stale boundary must not advance the walk"
        );
        assert!(
            h.state.project_requests.match_frame(&second_id).is_some(),
            "and the page actually in flight is untouched"
        );
        assert!(
            h.state
                .enrolment_history
                .as_ref()
                .is_some_and(|w| !w.has_proven_exhaustion()),
            "nor may it certify that history is exhausted"
        );
    }

    /// A saturated page asks further back; it does not conclude.
    ///
    /// The removed 500-row ceiling made exactly this mistake: a page that came
    /// back full is evidence there is *more*, and treating it as the end is how
    /// an agent reports complete authority over a truncated set.
    #[tokio::test]
    async fn a_saturated_page_asks_further_back_rather_than_finishing() {
        let mut h = WalkHarness::new().await;
        open_enrolment_for(&mut h.state, std::slice::from_ref(&h.coordinate)).await;
        let _ = h.next_req().await;
        h.begin().await;
        let first = h.req().await;
        let limit = first[2]["limit"].as_u64().expect("a page limit") as usize;
        let first_id = first[1].as_str().expect("an id").to_string();

        // Exactly `limit` rows, distinct timestamps: saturated, and orderable.
        let stamps: Vec<u64> = (0..limit as u64).map(|i| 9_000 - i * 10).collect();
        h.fill_and_close(&first_id, &stamps).await;

        let second = h.req().await;
        assert_eq!(
            second[2]["until"],
            json!(stamps.last().copied().expect("a stamp")),
            "the next page resumes at the oldest row seen, so no root between \
             the two bounds can be skipped: {second:?}"
        );
        assert_eq!(
            second[2]["limit"],
            json!(limit),
            "an orderable saturated page does not need a wider one: {second:?}"
        );
    }

    /// A page filled by one timestamp widens instead of stepping past it.
    ///
    /// Walking backwards by `until` cannot separate rows that share a second.
    /// Stepping past them would skip roots silently; asking again from the same
    /// bound would spin. The page widens until the cohort fits.
    #[tokio::test]
    async fn a_same_timestamp_cohort_widens_the_page_instead_of_skipping_roots() {
        let mut h = WalkHarness::new().await;
        open_enrolment_for(&mut h.state, std::slice::from_ref(&h.coordinate)).await;
        let _ = h.next_req().await;
        h.begin().await;
        let first = h.req().await;
        let limit = first[2]["limit"].as_u64().expect("a page limit") as usize;
        let until = first[2]["until"].as_u64().expect("a bound");
        let first_id = first[1].as_str().expect("an id").to_string();

        let stamps: Vec<u64> = std::iter::repeat_n(9_000u64, limit).collect();
        h.fill_and_close(&first_id, &stamps).await;

        let second = h.req().await;
        assert!(
            second[2]["limit"].as_u64().expect("a limit") > limit as u64,
            "the page must widen to fit the cohort: {second:?}"
        );
        assert!(
            second[2]["until"].as_u64().expect("a bound") <= until,
            "and must not step past a bound it cannot order within: {second:?}"
        );
        assert!(
            h.degraded().is_none(),
            "widening is progress, not a failure: {:?}",
            h.degraded()
        );
    }

    /// An unsaturated page proves exhaustion, and the roots restore silently.
    ///
    /// Both halves matter. The walk ends because the relay returned fewer rows
    /// than it was allowed to — nothing else may end it — and the roots it
    /// found arrive as `EnrolmentHistory`, which folds every effect through
    /// `ProcessingMode::Replay`. A restart that answered them would re-reply to
    /// every issue the agent had ever been addressed on.
    #[tokio::test]
    async fn a_short_page_completes_the_walk_and_restores_without_a_turn() {
        let mut h = WalkHarness::new().await;
        open_enrolment_for(&mut h.state, std::slice::from_ref(&h.coordinate)).await;
        let _ = h.next_req().await;
        h.begin().await;
        let first_id = h.req().await[1].as_str().expect("an id").to_string();

        // Two rows against a page that asked for many: unsaturated, and the
        // only thing that may end a walk.
        h.fill_and_close(&first_id, &[9_000, 8_500]).await;

        assert!(
            h.next_req().await.is_none(),
            "an unsaturated page is the end of history — nothing more is asked"
        );
        assert!(
            h.state
                .enrolment_history
                .as_ref()
                .is_some_and(|w| w.has_proven_exhaustion()),
            "and the walk says so"
        );
        assert!(h.degraded().is_none());

        let restored: Vec<_> = drain(&mut h.rx)
            .into_iter()
            .filter_map(|e| match e {
                BuzzEvent::Project(crate::project::ProjectEvent::Routed {
                    source, mode, ..
                }) => Some((source, mode)),
                _ => None,
            })
            .collect();
        assert_eq!(restored.len(), 2, "both roots restore: {restored:?}");
        for (source, mode) in &restored {
            assert!(
                matches!(
                    source,
                    crate::project::ProjectSubscription::EnrolmentHistory { .. }
                ),
                "a reconstructed root must carry its page's class: {source:?}"
            );
            assert_eq!(
                *mode,
                crate::project::ProcessingMode::Replay,
                "history restores authority and lifecycle; it never runs a turn"
            );
        }
    }

    /// A root the run loop could not take is **retained**, never dropped.
    ///
    /// The counterexample, exactly as reported: an event channel of capacity
    /// one, an exhausted page carrying two distinct roots, and a receiver that
    /// does not drain. The version before this one handed the first root on,
    /// was refused the second, released its dedup claim, queued a proactive
    /// resubscribe that could not reach back far enough to recover it, and
    /// logged `enrolment history reconstruction complete` over the hole.
    ///
    /// A walk with a root still owed is not finished. That is the whole rule,
    /// and `retained()` is where the agent says so.
    #[tokio::test]
    async fn a_root_the_run_loop_cannot_take_is_retained_not_dropped() {
        let mut h = WalkHarness::with_capacity(1).await;
        open_enrolment_for(&mut h.state, std::slice::from_ref(&h.coordinate)).await;
        let _ = h.next_req().await;
        h.begin().await;
        let first_id = h.req().await[1].as_str().expect("an id").to_string();

        // Unsaturated, so the walk proves exhaustion — and two roots against
        // one slot, so the second cannot be delivered now.
        h.fill_and_close(&first_id, &[9_000, 8_500]).await;

        assert!(
            h.state
                .enrolment_history
                .as_ref()
                .is_some_and(|w| w.has_proven_exhaustion()),
            "the walk did reach the end of history — the question is what it does next"
        );
        assert_eq!(
            h.retained(),
            Some(1),
            "the root the channel refused is still owed, so reconstruction is unfinished"
        );
        assert!(
            h.degraded().is_none(),
            "a full queue for one instant is not yet a failure: {:?}",
            h.degraded()
        );

        // The run loop catches up.
        let first = drain(&mut h.rx);
        assert_eq!(first.len(), 1, "one slot, one delivery: {first:?}");

        assert_eq!(
            h.retry_restore(),
            RestoreOutcome::Complete,
            "with the queue drained, the retained root reaches the run loop"
        );
        assert_eq!(
            h.retained(),
            None,
            "and only then is there nothing left owed"
        );
        assert!(h.degraded().is_none());

        let second: Vec<_> = drain(&mut h.rx)
            .into_iter()
            .filter_map(|e| match e {
                BuzzEvent::Project(crate::project::ProjectEvent::Routed {
                    source, mode, ..
                }) => Some((source, mode)),
                _ => None,
            })
            .collect();
        assert_eq!(second.len(), 1, "the retained root, restored: {second:?}");
        assert!(
            matches!(
                second[0].0,
                crate::project::ProjectSubscription::EnrolmentHistory { .. }
            ),
            "a retry is the same delivery arriving late — it carries the page's own \
             class, not a new one: {:?}",
            second[0].0
        );
        assert_eq!(
            second[0].1,
            crate::project::ProcessingMode::Replay,
            "and restores authority without running a turn"
        );
    }

    /// A run loop that never drains degrades the reconstruction.
    ///
    /// Retention is bounded, or it is just a slower way to lose the root: an
    /// agent that waits forever for a queue that has stopped moving reports
    /// nothing at all, which is the same false health as reporting completion.
    /// The bound is on *consecutive* fruitless attempts, so this counts them
    /// exactly rather than trusting a wall clock.
    #[tokio::test]
    async fn retention_is_bounded_and_ends_in_visible_degradation() {
        let mut h = WalkHarness::with_capacity(1).await;
        open_enrolment_for(&mut h.state, std::slice::from_ref(&h.coordinate)).await;
        let _ = h.next_req().await;
        h.begin().await;
        let first_id = h.req().await[1].as_str().expect("an id").to_string();

        h.fill_and_close(&first_id, &[9_000, 8_500]).await;
        assert_eq!(h.retained(), Some(1));

        // The receiver is never drained. Every attempt from here delivers
        // nothing, and the first attempt that delivered something reset the
        // budget — so the walk survives exactly the limit and no further.
        for attempt in 1..ENROLMENT_RESTORE_STALL_LIMIT {
            assert_eq!(
                h.retry_restore(),
                RestoreOutcome::Retained(1),
                "attempt {attempt} is still inside the budget"
            );
            assert!(
                h.degraded().is_none(),
                "and must not degrade early: {:?}",
                h.degraded()
            );
        }
        assert_eq!(
            h.retry_restore(),
            RestoreOutcome::Degraded,
            "the last attempt of the budget is the one that gives up"
        );

        let reason = h
            .degraded()
            .expect("an agent that cannot restore what it found must say so");
        assert!(
            reason.contains('1') && reason.contains('2'),
            "the degraded state must name how much of what it found is missing: {reason}"
        );
        assert_eq!(
            h.retained(),
            None,
            "a degraded reconstruction stops retrying rather than growing a queue forever"
        );
        assert_eq!(
            h.retry_restore(),
            RestoreOutcome::Idle,
            "and nothing further is owed by a walk that has already failed"
        );
    }

    /// The retry belongs to the running loop, not to a test.
    ///
    /// The two tests above drive `drive_pending_restorations` themselves,
    /// which proves the rule and says nothing about its reachability — and a
    /// retained root that nothing ever retries is a lost root wearing a
    /// queue. The only thing that retries one in production is an arm of
    /// `run_background_task`'s `select!`.
    ///
    /// So this starts the real background task over a real socket, gives it an
    /// event channel of exactly one slot, and then does nothing except drain
    /// that slot: no further command, no further frame, and no call into the
    /// restore path. The second root arrives because the loop woke itself up.
    #[tokio::test]
    async fn the_background_loop_retries_a_retained_root_on_its_own() {
        use futures_util::SinkExt;

        let (client, mut server) = test_ws_pair().await;
        let (event_tx, mut event_rx) = mpsc::channel(1);
        let (observer_tx, _observer_rx) = mpsc::channel(4);
        let (cmd_tx, cmd_rx) = mpsc::channel(4);
        let owner = nostr::Keys::generate();
        let coordinate = format!("30617:{}:demo", owner.public_key().to_hex());

        let task = tokio::spawn(run_background_task(
            client,
            ingress::HandshakeBuffer::empty(),
            event_tx,
            observer_tx,
            cmd_rx,
            nostr::Keys::generate(),
            "ws://test".to_string(),
            test_agent_hex(),
            None,
        ));

        cmd_tx
            .send(RelayCommand::BeginEnrolmentHistory {
                coordinates: vec![coordinate.clone()],
                agent: test_agent_hex(),
            })
            .await
            .expect("the loop takes the command");

        // The page the walk opened, read off the wire it wrote it to.
        let page_id = loop {
            let frame = next_test_frame(&mut server).await;
            if frame[0] == "REQ" {
                break frame[1].as_str().expect("an id").to_string();
            }
        };

        // Two roots against one slot, and few enough that the page is
        // unsaturated — so the walk proves exhaustion and hands them on.
        let now = nostr::Timestamp::now().as_secs();
        for offset in [600u64, 1_200] {
            let root = project_root_frame(&owner, &coordinate, now - offset);
            server
                .send(Message::Text(
                    json!(["EVENT", page_id, root]).to_string().into(),
                ))
                .await
                .expect("the peer writes the row");
        }
        server
            .send(Message::Text(json!(["EOSE", page_id]).to_string().into()))
            .await
            .expect("the peer writes the boundary");

        let restored_root = |event: Option<BuzzEvent>| match event {
            Some(BuzzEvent::Project(crate::project::ProjectEvent::Routed {
                event, mode, ..
            })) => {
                assert_eq!(
                    mode,
                    crate::project::ProcessingMode::Replay,
                    "a reconstructed root restores authority without a turn"
                );
                event.id()
            }
            other => panic!("expected a restored project root, got {other:?}"),
        };

        let first = timeout(Duration::from_secs(5), event_rx.recv())
            .await
            .expect("the first root fits in the one slot there is")
            .expect("the loop is running");
        let first = restored_root(first);

        // From here the test writes nothing and calls nothing. Draining the
        // slot is the only thing that changed.
        let second = timeout(Duration::from_secs(5), event_rx.recv())
            .await
            .expect(
                "the retained root must be retried by the loop itself — nothing else \
                 can deliver it, and the walk will ask for no further page",
            )
            .expect("the loop is running");
        let second = restored_root(second);

        assert_ne!(
            first, second,
            "the retry must deliver the root that was refused, not repeat the one that fit"
        );

        drop(cmd_tx);
        let _ = timeout(Duration::from_secs(2), task).await;
    }

    /// A root the live tail already delivered counts as restored.
    ///
    /// The other half of the same rule. Retention only terminates if a root
    /// whose authority is *already held* resolves — otherwise the ordinary
    /// race between the live tail and the walk (they overlap deliberately)
    /// would leave a permanent debt, and the bound above would turn every such
    /// overlap into a degraded agent. It must also not deliver twice: the
    /// dedup slot is spent, and a second copy would be a second effect.
    #[tokio::test]
    async fn a_root_already_delivered_live_is_restored_rather_than_owed() {
        let mut h = WalkHarness::with_capacity(4).await;
        let enrol_id = open_enrolment_for(&mut h.state, std::slice::from_ref(&h.coordinate)).await;
        let _ = h.next_req().await;

        // The tail delivers the root first, as live work.
        drain_enrolment_backlog(&mut h.state, &enrol_id).await;
        let root = project_root_frame(&h.owner, &h.coordinate, 9_000);
        deliver_frame(&mut h.state, &enrol_id, &root, &h.tx).await;
        assert_eq!(drain(&mut h.rx).len(), 1, "the live delivery happened");

        // …and the walk then finds the same root in history.
        h.begin().await;
        let page_id = h.req().await[1].as_str().expect("an id").to_string();
        h.feed(json!(["EVENT", page_id, root])).await;
        h.feed(json!(["EOSE", page_id])).await;

        assert_eq!(
            h.retained(),
            None,
            "a root whose authority is already held is restored, not owed forever"
        );
        assert!(h.degraded().is_none());
        assert!(
            drain(&mut h.rx).is_empty(),
            "and it is not delivered a second time — the slot is already spent"
        );
    }

    /// A walk that cannot prove completeness is **visibly** degraded.
    ///
    /// The fail-closed state the plan requires. A cohort that still fills the
    /// page at the relay's ceiling cannot be walked past without skipping
    /// roots, so there is no honest answer left except "I do not know how much
    /// history there is" — and that has to be a state the agent is in, not a
    /// silence an operator has to infer.
    #[tokio::test]
    async fn a_walk_that_cannot_prove_completeness_degrades_visibly() {
        let mut h = WalkHarness::new().await;
        open_enrolment_for(&mut h.state, std::slice::from_ref(&h.coordinate)).await;
        let _ = h.next_req().await;
        h.begin().await;

        // Widen until the ceiling, always with one indivisible cohort.
        let mut asked = h.req().await;
        for _ in 0..8 {
            let limit = asked[2]["limit"].as_u64().expect("a limit") as usize;
            let id = asked[1].as_str().expect("an id").to_string();
            let stamps: Vec<u64> = std::iter::repeat_n(9_000u64, limit).collect();
            h.fill_and_close(&id, &stamps).await;
            match h.next_req().await {
                Some(next) => asked = next,
                None => break,
            }
        }

        let reason = h
            .degraded()
            .expect("a walk that cannot prove exhaustion must say so");
        assert!(
            reason.contains("enrolment history"),
            "the degraded state must name what is unproven: {reason}"
        );
        assert!(
            h.state
                .enrolment_history
                .as_ref()
                .is_some_and(|w| !w.has_proven_exhaustion()),
            "and must never read as complete"
        );
        assert!(
            h.next_req().await.is_none(),
            "a degraded walk stops asking rather than spinning"
        );
    }

    /// Replay or live is the **request path**, never the author's clock.
    ///
    /// Three rules have stood on this line and all three re-answered old work
    /// or lost new work. The first required that the enrolment backlog had not
    /// drained, keyed by subscription id: the enrolment id is fixed, so a
    /// predecessor's EOSE certified a successor's backlog it knew nothing
    /// about, and four historical roots were replied to on a real relay. The
    /// second read `created_at < startup_watermark`, the author's clock, which
    /// fails in both directions and is invisible from the event alone.
    ///
    /// The third was "the tail is live, always". It is the one this test now
    /// pins the replacement for: the tail has to reach behind startup to be
    /// gapless at all (see `the_enrolment_tail_covers_the_relays_accepted_skew`),
    /// and the rows that reach-back pulls in are stored, not new.
    ///
    /// So the same signed root, on the same request, means different things on
    /// either side of *that request's own* boundary — and nothing about the
    /// event decides which.
    #[tokio::test]
    async fn the_tails_own_boundary_separates_its_backlog_from_live_work() {
        let owner = nostr::Keys::generate();
        let coord = format!("30617:{}:demo", owner.public_key().to_hex());
        let root = project_root_frame(&owner, &coord, 900);

        // Before the boundary: stored rows, restoring context.
        let mut state = BgState::new();
        state.startup_watermark = Some(1_000);
        let enrol_id = open_enrolment_for(&mut state, std::slice::from_ref(&coord)).await;
        let (tx, mut rx) = mpsc::channel(16);
        deliver_frame(&mut state, &enrol_id, &root, &tx).await;
        assert_eq!(
            routed_mode(&mut rx),
            crate::project::ProcessingMode::Replay,
            "the tail's stored prefix is context — answering it re-answers history"
        );

        // After it: live work, and the timestamp never changed.
        let mut state = BgState::new();
        state.startup_watermark = Some(1_000);
        let enrol_id = open_enrolment_for(&mut state, std::slice::from_ref(&coord)).await;
        drain_enrolment_backlog(&mut state, &enrol_id).await;
        let (tx, mut rx) = mpsc::channel(16);
        deliver_frame(&mut state, &enrol_id, &root, &tx).await;
        assert_eq!(
            routed_mode(&mut rx),
            crate::project::ProcessingMode::Live,
            "a root published after the tail finished answering from store is news, \
             however old its author's clock claims it is"
        );
    }

    /// A predecessor's boundary cannot drain its successor's backlog.
    ///
    /// This is the original reported defect, in the shape the fix has to keep
    /// closed. The enrolment id is *fixed*, so a stale EOSE is spelled exactly
    /// like the current one — id comparison cannot tell them apart, and the
    /// version that compared ids stamped a successor's stored frames live and
    /// replied to four historical roots.
    ///
    /// The replacement is opened first and the predecessor's boundary arrives
    /// afterwards, which is the real interleaving: an EOSE already in flight
    /// when the tail widens.
    #[tokio::test]
    async fn a_predecessors_boundary_cannot_drain_the_successors_backlog() {
        let owner = nostr::Keys::generate();
        let coord = format!("30617:{}:demo", owner.public_key().to_hex());
        let second = format!("30617:{}:second", owner.public_key().to_hex());
        let mut state = BgState::new();
        state.startup_watermark = Some(1_000);

        let enrol_id = open_enrolment_for(&mut state, std::slice::from_ref(&coord)).await;
        // The tail widens onto a second repository: a new registration, the
        // same wire id, and the predecessor's boundary still unaccounted for.
        let widened = open_enrolment_for(&mut state, &[coord.clone(), second]).await;
        assert_eq!(
            widened, enrol_id,
            "the enrolment id is fixed — that is what makes this substitution possible"
        );

        // The EOSE the *predecessor* earned, arriving now.
        drain_enrolment_backlog(&mut state, &enrol_id).await;

        let root = project_root_frame(&owner, &coord, 900);
        let (tx, mut rx) = mpsc::channel(16);
        deliver_frame(&mut state, &enrol_id, &root, &tx).await;
        assert_eq!(
            routed_mode(&mut rx),
            crate::project::ProcessingMode::Replay,
            "a boundary belonging to a request that no longer exists must certify \
             nothing about the one that replaced it"
        );
    }

    /// Two replacements before any boundary owe two boundaries.
    ///
    /// The debt is a count, not a flag. A flag would let the second stale
    /// boundary through — and the successor's stored rows behind it would be
    /// answered as live work, which is the whole defect wearing one more
    /// replacement.
    #[tokio::test]
    async fn every_undrained_predecessor_owes_its_own_boundary() {
        let owner = nostr::Keys::generate();
        let a = format!("30617:{}:a", owner.public_key().to_hex());
        let b = format!("30617:{}:b", owner.public_key().to_hex());
        let c = format!("30617:{}:c", owner.public_key().to_hex());
        let mut state = BgState::new();
        state.startup_watermark = Some(1_000);

        // Three registrations, no boundary consumed between them.
        let enrol_id = open_enrolment_for(&mut state, std::slice::from_ref(&a)).await;
        let _ = open_enrolment_for(&mut state, &[a.clone(), b.clone()]).await;
        let _ = open_enrolment_for(&mut state, &[a.clone(), b, c]).await;

        let root = project_root_frame(&owner, &a, 900);
        for owed in 1..=2 {
            drain_enrolment_backlog(&mut state, &enrol_id).await;
            let (tx, mut rx) = mpsc::channel(16);
            deliver_frame(&mut state, &enrol_id, &root, &tx).await;
            assert_eq!(
                routed_mode(&mut rx),
                crate::project::ProcessingMode::Replay,
                "boundary {owed} of 2 is owed to a predecessor and settles nothing"
            );
            // Release the slot so the next round delivers rather than dedups.
            state.project_seen_ids.remove(&root.id.to_hex());
        }

        // The third boundary is the live registration's own.
        drain_enrolment_backlog(&mut state, &enrol_id).await;
        let (tx, mut rx) = mpsc::channel(16);
        deliver_frame(&mut state, &enrol_id, &root, &tx).await;
        assert_eq!(
            routed_mode(&mut rx),
            crate::project::ProcessingMode::Live,
            "the tail must eventually go live, or the agent is deaf by construction"
        );
    }

    /// A boundary from a dead connection cannot drain the replacement's
    /// backlog.
    ///
    /// The same substitution across a reconnect rather than a replacement.
    /// `clear_connection` drops the claim with the connection that earned it,
    /// so the replayed tail starts its own prefix from scratch.
    #[tokio::test]
    async fn a_reconnect_starts_the_tails_backlog_again() {
        let owner = nostr::Keys::generate();
        let coord = format!("30617:{}:demo", owner.public_key().to_hex());
        let mut state = BgState::new();
        state.startup_watermark = Some(1_000);

        let enrol_id = open_enrolment_for(&mut state, std::slice::from_ref(&coord)).await;
        drain_enrolment_backlog(&mut state, &enrol_id).await;

        // The connection dies and the tail is re-opened on its replacement.
        state.retire_project_connection();
        let reopened = open_enrolment_for(&mut state, std::slice::from_ref(&coord)).await;

        let root = project_root_frame(&owner, &coord, 900);
        let (tx, mut rx) = mpsc::channel(16);
        deliver_frame(&mut state, &reopened, &root, &tx).await;
        assert_eq!(
            routed_mode(&mut rx),
            crate::project::ProcessingMode::Replay,
            "the dead connection's boundary drained a request that no longer exists; \
             the replacement is answering from store again"
        );
    }

    /// A dead connection's unsettled debt does not follow the replacement.
    ///
    /// The reconnect case that `a_reconnect_starts_the_tails_backlog_again`
    /// does not reach: the tail is retired *before* its boundary ever arrives,
    /// so a registry that kept the claim would carry a debt for an EOSE the
    /// dead socket will now never send. The replacement's own boundary would be
    /// spent settling that phantom, and the tail would stay in its stored
    /// prefix forever — deaf to live work, silently, with no error anywhere.
    ///
    /// Fail-deaf is the mirror of the defect this correction fixes, and it is
    /// harder to notice, so it gets its own test rather than riding on another.
    #[tokio::test]
    async fn a_dead_connections_unsettled_boundary_is_not_owed_by_its_replacement() {
        let owner = nostr::Keys::generate();
        let coord = format!("30617:{}:demo", owner.public_key().to_hex());
        let mut state = BgState::new();
        state.startup_watermark = Some(1_000);

        // Opened and never drained — the boundary was still in flight.
        let _ = open_enrolment_for(&mut state, std::slice::from_ref(&coord)).await;
        state.retire_project_connection();
        let reopened = open_enrolment_for(&mut state, std::slice::from_ref(&coord)).await;

        // One boundary, from the replacement's own registration.
        drain_enrolment_backlog(&mut state, &reopened).await;

        let root = project_root_frame(&owner, &coord, 900);
        let (tx, mut rx) = mpsc::channel(16);
        deliver_frame(&mut state, &reopened, &root, &tx).await;
        assert_eq!(
            routed_mode(&mut rx),
            crate::project::ProcessingMode::Live,
            "the replacement answered its own boundary; a debt owed by a socket that \
             no longer exists can never be settled, so carrying it makes the tail deaf"
        );
    }

    /// **The real-filter regression.**
    ///
    /// The reported miss did not happen inside this agent. A `1621` addressed
    /// to two agents was accepted by the relay with `200 OK`, stored, and
    /// readable by `buzz issues get` — and neither agent ever saw it, because
    /// the standing tail's `since` was the agent's own startup and the relay
    /// evaluates `since` against the event's **signed** `created_at`. The event
    /// was filtered out before delivery, so every classification rule
    /// downstream was arguing about a frame that never arrived.
    ///
    /// A test that hands a frame to `deliver_frame` cannot catch that: it
    /// starts one step past the boundary that failed. So this one asks the
    /// relay's own matcher — `buzz_core::filter`, the code the relay runs — to
    /// judge the REQ this agent actually wrote to the socket against a real
    /// signed root at the reported skew.
    ///
    /// The negative control is what makes it a regression rather than a
    /// restatement: the identical root, against a tail built at the caller's
    /// floor with no reach-back, is **rejected** by that same matcher. That is
    /// the shipped behaviour, reproduced, failing.
    #[tokio::test]
    async fn the_tails_wire_filter_admits_an_accepted_slow_clock_root() {
        use buzz_core::event::StoredEvent;

        // A plausible live watermark rather than a small number: `since`
        // arithmetic that only works near zero is arithmetic that has not been
        // tested.
        const STARTUP: u64 = 1_785_743_469;
        // The reported event's own skew: 387 seconds early, well inside the
        // ±900 the relay's ingest gate accepts.
        const SKEW: u64 = 387;

        let owner = nostr::Keys::generate();
        let coord = format!("30617:{}:phase6-e2e", owner.public_key().to_hex());
        let discovered = crate::project::DiscoveredRepositories::for_test(vec![coord.clone()]);

        // The REQ this agent writes, read back off the wire it wrote it to.
        let mut state = BgState::new();
        state.startup_watermark = Some(STARTUP);
        let (mut ws, mut server) = test_ws_pair().await;
        let filter = crate::project::enrolment_filter(&discovered, &test_agent_hex(), STARTUP)
            .expect("a discovered repository yields a filter");
        let outcome = state
            .project_requests
            .replace_enrolment(&mut ws, vec![filter])
            .await;
        assert!(matches!(
            outcome,
            crate::project::ReplaceOutcome::Replaced { .. }
        ));
        claim_enrolment_backlog(&mut state);

        let req = readable_frames(&mut server)
            .await
            .into_iter()
            .find(|f| f[0] == "REQ")
            .expect("the tail's REQ reached the socket");
        assert_eq!(
            req[1],
            json!(crate::project::PROJECT_ENROL_SUB_ID),
            "the frame judged below must be the enrolment tail's own"
        );
        let on_the_wire: Vec<nostr::Filter> = req.as_array().expect("a REQ is an array")[2..]
            .iter()
            .map(|f| serde_json::from_value(f.clone()).expect("a NIP-01 filter"))
            .collect();

        // A real signed root, addressed to this agent, on the discovered
        // repository, stamped the way the reported one was.
        let root = project_root_frame(&owner, &coord, STARTUP - SKEW);
        let stored = StoredEvent::new(root.clone(), None);

        assert!(
            buzz_core::filter::filters_match(&on_the_wire, &stored),
            "the relay stored this root and would deliver it to anyone whose filter \
             matched; a tail that does not match it is deaf to an event the relay \
             accepted, and no downstream classification can recover it: {req:?}"
        );

        // The negative control: the shipped filter, on the same root.
        let shipped: Vec<nostr::Filter> = {
            let mut f = crate::project::enrolment_filter(
                &discovered,
                &test_agent_hex(),
                STARTUP + crate::project::ACCEPTED_CLOCK_SKEW_SECS,
            )
            .expect("a filter");
            f["since"] = json!(STARTUP);
            vec![serde_json::from_value(f).expect("a NIP-01 filter")]
        };
        assert!(
            !buzz_core::filter::filters_match(&shipped, &stored),
            "the control must reproduce the miss, or the assertion above proves nothing"
        );

        // And once it is delivered, past the tail's own boundary, it is one
        // live invocation rather than restored context.
        let enrol_id = crate::project::PROJECT_ENROL_SUB_ID.to_string();
        drain_enrolment_backlog(&mut state, &enrol_id).await;
        let (tx, mut rx) = mpsc::channel(16);
        deliver_frame(&mut state, &enrol_id, &root, &tx).await;
        deliver_frame(&mut state, &enrol_id, &root, &tx).await;

        let deliveries: Vec<_> = drain(&mut rx)
            .into_iter()
            .filter_map(|e| match e {
                BuzzEvent::Project(crate::project::ProjectEvent::Routed { mode, .. }) => Some(mode),
                _ => None,
            })
            .collect();
        assert_eq!(
            deliveries,
            vec![crate::project::ProcessingMode::Live],
            "exactly once, as live work: {deliveries:?}"
        );
    }

    /// …and a skewed root delivered twice is still answered once.
    ///
    /// Being live is only correct if the second delivery — from the watched REQ
    /// this very enrolment installs, or from a peer call naming the same root —
    /// is an exact no-op. The shared `project_seen_ids` slot is what makes it
    /// one.
    #[tokio::test]
    async fn a_clock_skewed_root_on_the_live_tail_invokes_exactly_once() {
        let owner = nostr::Keys::generate();
        let coord = format!("30617:{}:demo", owner.public_key().to_hex());
        let mut state = BgState::new();
        state.startup_watermark = Some(1_000);
        let enrol_id = open_enrolment_for(&mut state, std::slice::from_ref(&coord)).await;
        drain_enrolment_backlog(&mut state, &enrol_id).await;

        let skewed = project_root_frame(&owner, &coord, 900);
        let (tx, mut rx) = mpsc::channel(16);
        deliver_frame(&mut state, &enrol_id, &skewed, &tx).await;
        // The same signed event again, on the same path.
        deliver_frame(&mut state, &enrol_id, &skewed, &tx).await;

        let deliveries: Vec<_> = drain(&mut rx)
            .into_iter()
            .filter(|e| matches!(e, BuzzEvent::Project(_)))
            .collect();
        assert_eq!(
            deliveries.len(),
            1,
            "one root, one delivery — got {deliveries:?}"
        );
    }

    /// The mode carried by the one routed project delivery in `rx`.
    fn routed_mode(rx: &mut mpsc::Receiver<Option<BuzzEvent>>) -> crate::project::ProcessingMode {
        let modes: Vec<_> = drain(rx)
            .into_iter()
            .filter_map(|e| match e {
                BuzzEvent::Project(crate::project::ProjectEvent::Routed { mode, .. }) => Some(mode),
                _ => None,
            })
            .collect();
        assert_eq!(modes.len(), 1, "expected one routed delivery: {modes:?}");
        modes[0]
    }

    /// A signed, addressed project root at a chosen timestamp.
    ///
    /// It names the agent as well as `p`-tagging it. A root's `p` alone is
    /// structural — Desktop stamps the repository owner onto every root it
    /// creates — so a fixture that carried only the tag would be admitted by
    /// the relay and then correctly refused by the gate, which is not what any
    /// test here is about.
    fn project_root_frame(owner: &nostr::Keys, coord: &str, created_at: u64) -> Event {
        nostr::EventBuilder::new(
            nostr::Kind::Custom(buzz_core::kind::KIND_GIT_ISSUE as u16),
            format!("@{} please look", test_agent_hex()),
        )
        .tags([
            nostr::Tag::parse(["a", coord]).unwrap(),
            nostr::Tag::parse(["p", &test_agent_hex()]).unwrap(),
        ])
        .custom_created_at(nostr::Timestamp::from(created_at))
        .sign_with_keys(owner)
        .expect("sign")
    }

    /// Install the enrolment subscription over `coordinates`, through the
    /// registry's own replacement, and return its fixed id.
    async fn open_enrolment_for(state: &mut BgState, coordinates: &[String]) -> String {
        let discovered = crate::project::DiscoveredRepositories::for_test(coordinates.to_vec());
        let filter = crate::project::enrolment_filter(&discovered, &test_agent_hex(), 0)
            .expect("a discovered repository yields a filter");
        open_enrolment_with(state, vec![filter]).await
    }

    /// Drain the enrolment tail's stored-events prefix through the production
    /// EOSE path.
    ///
    /// A tail that has just been opened is still answering from the relay's
    /// store, so its frames are context. Tests that are about *live* traffic
    /// have to get past that the way production does — by the boundary — and
    /// saying so at each call site is the point: a helper that opened a tail
    /// already drained would hide the very state this correction adds.
    async fn drain_enrolment_backlog(state: &mut BgState, enrol_id: &str) {
        let (tx, _rx) = mpsc::channel(16);
        deliver_control_frame_to(state, json!(["EOSE", enrol_id]), &tx).await;
    }

    /// Install the enrolment subscription over `filters`, through the
    /// registry's own replacement, and return the id it stamped.
    async fn open_enrolment_with(state: &mut BgState, filters: Vec<Value>) -> String {
        let (mut ws, _server) = test_ws_pair().await;
        let outcome = state
            .project_requests
            .replace_enrolment(&mut ws, filters)
            .await;
        // Exactly what the `ReplaceProject` command handler does next. Without
        // it a fixture would open a tail that had never claimed a backlog, and
        // every frame on it would read as live for the wrong reason.
        claim_enrolment_backlog(state);
        assert!(
            matches!(outcome, crate::project::ReplaceOutcome::Replaced { .. }),
            "the enrolment replacement must install, got {outcome:?}"
        );
        crate::project::PROJECT_ENROL_SUB_ID.to_string()
    }

    async fn open_discovery(state: &mut BgState) -> String {
        assert_eq!(
            send_discovery(state, discovery_filters()).await,
            ProjectSendOutcome::Sent
        );
        crate::project::discovery_sub_id()
    }

    /// open → REQ on the wire → observe → EOSE → completion.
    ///
    /// Every link composed at once, over a real socket, because each of them
    /// being individually correct is exactly the property that kept holding
    /// while the whole was wrong. It also checks the bytes actually written:
    /// the REQ the relay receives must carry the id the registry minted and the
    /// filter the page's own parameters imply, or the request and the page are
    /// talking about different subscriptions.
    ///
    /// The dispatch-side link — a raw `["EVENT", id, e]` frame reaching this
    /// page — is composed separately in
    /// `a_page_fills_from_the_wire_and_completes_at_its_own_boundary`.
    #[tokio::test]
    async fn a_catch_up_page_completes_end_to_end_over_a_real_socket() {
        use crate::project::{HistoryStream, PageOutcome};

        let mut state = BgState::new();
        let (mut ws, mut server) = test_ws_pair().await;

        let root = "c".repeat(64);
        let cutoff = 1_000u64;
        let limit = 4usize;
        let filter = crate::project::catch_up_filter(&root, HistoryStream::Comments, cutoff, limit);

        let mut cursor = crate::project::HistoryCursor::new(
            crate::project::HistoryScope::Root {
                root: root.clone(),
                stream: HistoryStream::Comments,
            },
            cutoff,
            limit,
            1_000,
        );
        let mut page = match state
            .project_requests
            .open_history_page(&mut ws, cursor.begin_request())
            .await
        {
            crate::project::PageOpen::Opened(page) => page,
            other => panic!("a real socket must open a page: {other:?}"),
        };
        let sub_id = page.sub_id().to_string();
        assert!(
            sub_id.contains(&root),
            "the id names the root it asks about: {sub_id}"
        );
        assert!(
            sub_id.len() <= 256,
            "and fits this relay's advertised limit: {}",
            sub_id.len()
        );

        // The relay's view of what we asked.
        //
        // Bounded: without the timeout, an implementation that returned
        // `Opened` *without* writing would hang here rather than fail, which is
        // a worse outcome than a red test and hides the defect behind a stuck
        // suite. Found by mutating the open to skip its own write.
        let frame = tokio::time::timeout(std::time::Duration::from_secs(5), server.next())
            .await
            .expect("a REQ must reach the socket — an opened page without a write is the defect")
            .expect("a REQ frame must arrive")
            .expect("frame is readable");
        let parsed: Value =
            serde_json::from_str(frame.to_text().expect("REQ is text")).expect("REQ is JSON");
        assert_eq!(parsed[0], json!("REQ"));
        assert_eq!(
            parsed[1],
            json!(sub_id),
            "the id on the wire is the one the page was installed under"
        );
        assert_eq!(
            parsed[2], filter,
            "and the filter is the one this page's parameters imply"
        );

        let keys = nostr::Keys::generate();
        let comment = EventBuilder::new(nostr::Kind::TextNote, "a comment")
            .custom_created_at(nostr::Timestamp::from(900))
            .tags([nostr::Tag::parse(vec![
                "e".to_string(),
                root.clone(),
                String::new(),
                "root".to_string(),
            ])
            .expect("e tag")])
            .sign_with_keys(&keys)
            .expect("sign");
        page.observe(
            crate::project::VerifiedProjectEvent::verify(comment)
                .await
                .expect("test rows are signed"),
        );

        let witness = state
            .project_requests
            .witness_end_of_stored_events(&sub_id)
            .expect("a sent registration mints its boundary");

        match cursor.complete(&witness, page) {
            PageOutcome::Complete(stream) => {
                assert_eq!(stream.len(), 1, "the observed row survives to completion");
                assert_eq!(stream.root(), root);
                assert_eq!(stream.cutoff(), cutoff);
            }
            other => panic!("expected Complete, got {other:?}"),
        }
    }

    fn drain(rx: &mut mpsc::Receiver<Option<BuzzEvent>>) -> Vec<BuzzEvent> {
        let mut out = Vec::new();
        while let Ok(Some(event)) = rx.try_recv() {
            out.push(event);
        }
        out
    }

    #[tokio::test]
    async fn a_project_event_does_not_spend_the_channel_dedup_slot() {
        // The suppression primitive this closes: project classification is by
        // subscription id, so anything that can name `proj-roots-N` — the
        // relay first of all — could push a genuine channel event on the
        // project surface and burn its channel dedup slot. The channel REQ
        // would then deliver nothing, and the agent would never see a message
        // addressed to it. Verifying before deduping does not help here; the
        // event is real, it is just being replayed on the wrong surface.
        let mut state = BgState::new();
        let (tx, mut rx) = mpsc::channel(16);
        let keys = nostr::Keys::generate();
        let channel_id = Uuid::new_v4();
        let event = mixed_surface_event(&keys, channel_id, &test_root_id(), 1_000);

        let watched = open_watched(&mut state).await;
        deliver_frame(&mut state, &watched, &event, &tx).await;
        deliver_frame(&mut state, &channel_sub_id(channel_id), &event, &tx).await;

        let delivered = drain(&mut rx);
        assert_eq!(delivered.len(), 2, "both surfaces deliver: {delivered:?}");
        assert!(
            matches!(delivered[0], BuzzEvent::Project(_)),
            "project surface first: {:?}",
            delivered[0]
        );
        assert!(
            matches!(delivered[1], BuzzEvent::Channel { .. }),
            "the channel delivery must survive the project one: {:?}",
            delivered[1]
        );
    }

    #[tokio::test]
    async fn a_channel_event_does_not_spend_the_project_dedup_slot() {
        // The same separation in the other direction. A channel event that
        // also names a root must still reach project routing, or enrolment
        // would silently miss whatever the channel REQ happened to see first.
        let mut state = BgState::new();
        let (tx, mut rx) = mpsc::channel(16);
        let keys = nostr::Keys::generate();
        let channel_id = Uuid::new_v4();
        let event = mixed_surface_event(&keys, channel_id, &test_root_id(), 1_000);

        deliver_frame(&mut state, &channel_sub_id(channel_id), &event, &tx).await;
        let watched = open_watched(&mut state).await;
        deliver_frame(&mut state, &watched, &event, &tx).await;

        let delivered = drain(&mut rx);
        assert_eq!(delivered.len(), 2, "both surfaces deliver: {delivered:?}");
        assert!(matches!(delivered[0], BuzzEvent::Channel { .. }));
        assert!(matches!(delivered[1], BuzzEvent::Project(_)));
    }

    #[tokio::test]
    async fn overlapping_project_subscriptions_share_one_dedup_set() {
        // A comment on a watched root that also tags this agent on a known
        // repository satisfies the enrolment filter *and* the watched filter,
        // so the relay is entitled to send it under both ids. One set across
        // all project subscriptions folds that to one delivery; a
        // per-subscription set would call the second copy new and route the
        // event twice.
        //
        // This used to be posed as two watched *generations* answering at
        // once, opened by naming their ids and classes directly. That state is
        // not production-reachable: the registry retires the predecessor's
        // live registration the instant the successor installs, so a frame
        // under the retired generation is not admitted at all. The overlap
        // that does exist is this one, across classes.
        let mut state = BgState::new();
        let (tx, mut rx) = mpsc::channel(16);
        let keys = nostr::Keys::generate();
        let event = enrolled_and_watched_event(&keys, &test_root_id(), 1_000);

        let enrolment = open_enrolment_for(&mut state, &[test_coordinate()]).await;
        let watched = open_watched(&mut state).await;
        deliver_frame(&mut state, &enrolment, &event, &tx).await;
        deliver_frame(&mut state, &watched, &event, &tx).await;

        assert_eq!(
            drain(&mut rx).len(),
            1,
            "the subscription overlap must fold to one delivery"
        );
    }

    #[tokio::test]
    async fn project_backpressure_releases_only_the_project_slot() {
        // The order is the point. The channel surface delivers the event
        // first and legitimately spends its channel slot. Only then does the
        // project surface drop a copy under backpressure.
        //
        // With one shared set, that drop's `remove` released the *channel's*
        // slot — so the next reconnect replay re-delivered, as new, a message
        // the agent had already answered. Releasing a slot is only safe when
        // the surface releasing it is the surface that spent it.
        let mut state = BgState::new();
        let (tx, mut rx) = mpsc::channel(4);
        let keys = nostr::Keys::generate();
        let channel_id = Uuid::new_v4();
        let event = mixed_surface_event(&keys, channel_id, &test_root_id(), 1_000);
        let id = event.id.to_hex();

        deliver_frame(&mut state, &channel_sub_id(channel_id), &event, &tx).await;
        assert_eq!(drain(&mut rx).len(), 1, "the channel delivery lands");
        assert!(state.seen_ids.contains(&id), "and spends the channel slot");

        // Now wedge the queue and drop a project copy of the same event.
        while tx.try_send(None).is_ok() {}
        let watched = open_watched(&mut state).await;
        deliver_frame(&mut state, &watched, &event, &tx).await;

        assert!(
            state.seen_ids.contains(&id),
            "the channel slot survives a project drop — otherwise reconnect \
             replays an already-handled message as new"
        );
        assert_eq!(
            state.project_dropped_since,
            Some(1_000),
            "the project replay floor covers the drop"
        );
        assert!(
            !state.project_seen_ids.contains(&id),
            "and the project slot is released so project replay re-delivers"
        );
        assert!(
            state.channel_dropped_since.is_empty(),
            "project pressure must not rewind a channel's replay window"
        );

        // The channel replay is still deduped: the agent does not see it twice.
        while rx.try_recv().is_ok() {}
        deliver_frame(&mut state, &channel_sub_id(channel_id), &event, &tx).await;
        assert!(
            drain(&mut rx).is_empty(),
            "a channel replay after a project drop must stay deduplicated"
        );
    }

    // ── The request lifecycle is owned by the transport ──────────────────────

    fn test_filter() -> Value {
        json!({ "kinds": [KIND_GIT_REPO_ANNOUNCEMENT] })
    }

    /// The filters production's discovery subscription carries, from the
    /// production builder.
    fn discovery_filters() -> Vec<Value> {
        crate::project::discovery_subscription(true).expect("enabled")
    }

    /// A second, narrower discovery question. The class is no longer something
    /// a caller can vary, so a conflicting submission differs in its filter —
    /// which is the whole of what the registry's comparison has left to catch.
    fn other_discovery_filters() -> Vec<Value> {
        vec![json!({ "kinds": [KIND_GIT_REPO_ANNOUNCEMENT], "authors": [test_agent_hex()] })]
    }

    /// Does durable intent under `sub_id` ask exactly `filters`?
    ///
    /// The identity cannot be built out here to compare against — that is the
    /// point of the owner-stamped operations — so what a test reads is what
    /// the recorded request asks.
    fn intent_asks(state: &BgState, sub_id: &str, filters: &[Value]) -> bool {
        state
            .project_requests
            .intent(sub_id)
            .is_some_and(|held| held.filters().eq(filters.iter()))
    }

    /// Feed one non-EVENT frame through the production handler.
    async fn deliver_control_frame(state: &mut BgState, frame: Value) -> bool {
        let (tx, _rx) = mpsc::channel(16);
        deliver_control_frame_to(state, frame, &tx).await
    }

    /// Replace the connection onto `replacement`, the way production does.
    ///
    /// `install_replacement_connection` first, then the resubscribe. Tests used
    /// to call the resubscribe directly, which started them one step *after*
    /// the transition most of them depend on — and that gap is where a
    /// replacement connection answering the dead one's requests went unnoticed
    /// through a whole review round.
    ///
    /// The dead socket is created here rather than taken from the caller: what
    /// it is does not matter, only that there was one and that what belonged to
    /// it is retired before anything the replacement carries is handled.
    async fn reconnect_onto(state: &mut BgState, replacement: WsStream) -> ResubscribeResult {
        let (mut dead, _dead_server) = test_ws_pair().await;
        let (_cmd_tx, mut cmd_rx) = mpsc::channel(4);
        let agent = nostr::Keys::generate().public_key().to_hex();
        assert!(
            install_replacement_with(
                state,
                &mut dead,
                replacement,
                ingress::HandshakeBuffer::empty()
            )
            .await,
            "an empty handshake buffer carries no drop signal"
        );
        // `dead` now holds the replacement — production reassigns the same
        // variable for the same reason.
        resubscribe_after_reconnect(&mut dead, &mut cmd_rx, state, &agent, true).await
    }

    /// A handshake buffer holding `frames`, filled the way production fills
    /// one: by a NIP-42 reader taking them off a socket while it waits for the
    /// `AUTH` challenge.
    ///
    /// There is no other way to fill one — see [`ingress::HandshakeBuffer`].
    /// That is deliberate, and this helper is the reason it is affordable: the
    /// buffered-frame proofs below need a buffer with something in it, and a
    /// test that could simply construct one would be a test that can hand the
    /// production dispatch messages of its own choosing, which is the midpoint
    /// injection the phase forbids. Here the frames are relay JSON written onto
    /// a wire and read back by `wait_for_auth_challenge`, exactly as a relay
    /// that sent them before its challenge would have produced.
    async fn handshake_buffer_from_wire(
        frames: Vec<serde_json::Value>,
    ) -> ingress::HandshakeBuffer {
        use futures_util::SinkExt;
        let expected = frames.len();
        let (mut client, mut server) = test_ws_pair().await;
        for frame in frames {
            server
                .send(Message::Text(frame.to_string().into()))
                .await
                .expect("the peer writes the buffered frame");
        }
        server
            .send(Message::Text(
                json!(["AUTH", "challenge-after-the-buffer"])
                    .to_string()
                    .into(),
            ))
            .await
            .expect("the peer writes the challenge");

        let mut buffer = ingress::HandshakeBuffer::empty();
        let challenge =
            ingress::wait_for_auth_challenge(&mut client, &mut buffer, Duration::from_secs(2))
                .await
                .expect("the challenge arrives after the frames ahead of it");
        assert_eq!(challenge, "challenge-after-the-buffer");
        assert_eq!(
            buffer.len(),
            expected,
            "every frame ahead of the challenge must have been buffered"
        );
        buffer
    }

    /// Install `replacement` over `ws`, handing it `buffer` as the frames it
    /// received during its own handshake.
    ///
    /// The production entry point, called with what `do_connect` would have
    /// returned. Nothing here reorders or pre-filters the buffer: the whole
    /// question is what the ordinary dispatch does with those frames, and when.
    async fn install_replacement_with(
        state: &mut BgState,
        ws: &mut WsStream,
        replacement: WsStream,
        buffer: ingress::HandshakeBuffer,
    ) -> bool {
        let (event_tx, _event_rx) = mpsc::channel(16);
        let (observer_tx, _observer_rx) = mpsc::channel(8);
        let keys = nostr::Keys::generate();
        let agent = keys.public_key().to_hex();
        install_replacement_connection(
            ws,
            replacement,
            buffer,
            &event_tx,
            &observer_tx,
            state,
            &keys,
            "ws://test",
            &agent,
            None,
        )
        .await
    }

    /// A proactive resubscribe on the socket that is already connected.
    async fn resubscribe_on_same_socket(
        state: &mut BgState,
        ws: &mut WsStream,
    ) -> ResubscribeResult {
        let (_cmd_tx, mut cmd_rx) = mpsc::channel(4);
        let agent = nostr::Keys::generate().public_key().to_hex();
        resubscribe_after_reconnect(ws, &mut cmd_rx, state, &agent, false).await
    }

    #[tokio::test]
    async fn a_sent_project_req_is_registered_in_the_same_step() {
        let (mut ws, mut server) = test_ws_pair().await;
        let mut state = BgState::new();
        let sub_id = crate::project::discovery_sub_id();

        assert_eq!(
            send_project_discovery(&mut ws, &mut state, discovery_filters()).await,
            ProjectSendOutcome::Sent
        );

        let frame = next_test_frame(&mut server).await;
        assert_eq!(frame[0], "REQ");
        assert_eq!(frame[1], sub_id.as_str());
        assert!(state.project_requests.match_frame(&sub_id).is_some());
    }

    #[tokio::test]
    async fn a_failed_write_registers_nothing_and_disturbs_no_other_request() {
        // The window this closes: registered-but-never-asked. An id we hold a
        // record for, but never actually put on the wire, would answer frames
        // for a question we never posed.
        //
        // Named for what happens rather than for how: there is no reservation
        // and no rollback. A failed write installs nothing, because
        // installation only happens after the write returns.
        let (mut ws, server) = test_ws_pair().await;
        let mut state = BgState::new();

        let survivor = open_watched(&mut state).await;

        drop(server);
        let _ = ws.close(None).await;
        let doomed = crate::project::discovery_sub_id();
        assert_eq!(
            send_project_discovery(&mut ws, &mut state, discovery_filters()).await,
            ProjectSendOutcome::WriteFailed
        );

        assert!(
            state.project_requests.match_frame(&doomed).is_none(),
            "a failed send must leave nothing answerable"
        );
        assert!(
            state.project_requests.intent(&doomed).is_some(),
            "but the intent survives — the write failed, not the wish"
        );
        assert!(
            state.project_requests.match_frame(&survivor).is_some(),
            "and another live request is undisturbed"
        );
    }

    // ── the composed replacement path ───────────────────────────────────────
    //
    // These four drive `execute_connected_command` — the production background
    // owner — rather than `ProjectRequests::replace_request`. The defect they
    // exist for lived *between* those two: the run loop advanced its own
    // generation when the command was enqueued, so a generation the registry
    // never installed became the next named predecessor. A test that called
    // `replace_request` directly could not see that seam at all, because it
    // supplied the predecessor itself.

    /// Every frame the relay has received and not yet been read for.
    async fn drain_test_frames(
        server: &mut WebSocketStream<tokio::net::TcpStream>,
    ) -> Vec<serde_json::Value> {
        let mut frames = Vec::new();
        while let Ok(Some(Ok(message))) = timeout(Duration::from_millis(80), server.next()).await {
            if let Ok(text) = message.to_text() {
                if let Ok(value) = serde_json::from_str(text) {
                    frames.push(value);
                }
            }
        }
        frames
    }

    /// Subscription ids named by `REQ` frames, in wire order.
    fn req_ids(frames: &[serde_json::Value]) -> Vec<String> {
        frames
            .iter()
            .filter(|f| f[0] == "REQ")
            .filter_map(|f| f[1].as_str().map(str::to_string))
            .collect()
    }

    /// Every value a project REQ frame's filters carry under `tags`,
    /// deduplicated and sorted.
    ///
    /// Reads the named branches out of the frame's own filters rather than
    /// substring-matching the serialised frame. `contains(x)` is satisfied by a
    /// frame that carries `x` *and* by one that carries it alongside anything
    /// else, or that dropped a sibling — so it cannot express "the complete
    /// intended set", which is the property both the watched successor and the
    /// widened enrolment filter have to hold.
    fn req_tag_set(frame: &serde_json::Value, tags: &[&str]) -> Vec<String> {
        let mut values: Vec<String> = frame
            .as_array()
            .map(|f| &f[2..])
            .unwrap_or_default()
            .iter()
            .flat_map(|filter| {
                tags.iter().flat_map(move |tag| {
                    filter[*tag]
                        .as_array()
                        .map(|v| {
                            v.iter()
                                .filter_map(|r| r.as_str().map(str::to_string))
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default()
                })
            })
            .collect();
        values.sort();
        values.dedup();
        values
    }

    /// Every root id a project REQ frame asks about.
    ///
    /// Both reference styles, because they are not interchangeable: comments
    /// and status events point at the root with lowercase `e`, a PR update with
    /// uppercase `E`.
    fn req_root_set(frame: &serde_json::Value) -> Vec<String> {
        req_tag_set(frame, &["#e", "#E"])
    }

    /// Every repository coordinate a project REQ frame asks about.
    fn req_coordinate_set(frame: &serde_json::Value) -> Vec<String> {
        req_tag_set(frame, &["#a"])
    }

    /// Subscription ids named by `CLOSE` frames, in wire order.
    fn close_ids(frames: &[serde_json::Value]) -> Vec<String> {
        frames
            .iter()
            .filter(|f| f[0] == "CLOSE")
            .filter_map(|f| f[1].as_str().map(str::to_string))
            .collect()
    }

    /// Submit a replacement the way the run loop does: as a command, through
    /// the production background owner. Returns "keep the connection".
    async fn submit_replacement(
        ws: &mut WsStream,
        state: &mut BgState,
        replacement: crate::project::ProjectReplacement,
        filters: Vec<Value>,
    ) -> bool {
        execute_connected_command(
            ws,
            state,
            &"0".repeat(64),
            RelayCommand::ReplaceProject {
                replacement,
                filters,
            },
        )
        .await
    }

    /// A bounded watched filter that differs per call, so successive
    /// replacements are genuinely different questions.
    fn watched_filter(root_byte: u8) -> Value {
        serde_json::json!({ "#e": [format!("{:064x}", root_byte)] })
    }

    // `state_over`, `restored` and `watched_entry` stood here. They composed a
    // durable record — ids, classes, filters and both allocator positions — and
    // installed it into a real `ProjectRequests` inside a real `BgState`. That
    // is durable authority chosen by a proof rather than produced by an
    // operation, whatever the composing function is called, and it is how a
    // generation this process never issued came to be a predecessor that
    // reached a successor `REQ` and a predecessor `CLOSE`. The registry now has
    // one constructor and it takes nothing.
    //
    // What needed those helpers moved, and the destinations are named at each
    // deletion below: whole-record refusals are now proved against
    // `crate::project::validate_persisted_document`, which judges serialised
    // bytes by the owner's own rule and returns a description rather than a
    // record; allocator ceilings are proved against the allocator itself.
    // Predecessors here are installed by the operation that installs them in
    // production.

    /// **A failed generation must never become the retired predecessor.**
    ///
    /// The shipped chain: submit acknowledged the mpsc, the run loop advanced
    /// its copy, the socket write then failed, and the *next* replacement named
    /// the failed generation as its predecessor. `replace_request` dutifully
    /// retired that nonexistent id, so the genuinely installed predecessor
    /// stayed durable beside the successor — two live watched subscriptions,
    /// one of them retired as far as anything local was concerned.
    #[tokio::test]
    async fn a_failed_watched_generation_is_burned_but_never_retires_anything() {
        let (mut ws, mut server) = test_ws_pair().await;
        let mut state = BgState::new();

        // ── generation 0 installs ────────────────────────────────────────────
        assert!(
            submit_replacement(
                &mut ws,
                &mut state,
                crate::project::ProjectReplacement::Watched,
                vec![watched_filter(1)],
            )
            .await,
            "a successful replacement keeps the connection"
        );
        assert_eq!(
            state
                .project_requests
                .current_watched()
                .expect("exactly one watched intent"),
            Some(0)
        );
        let frames = drain_test_frames(&mut server).await;
        assert_eq!(
            req_ids(&frames),
            vec![crate::project::watched_sub_id(0)],
            "the first watched REQ, with nothing behind it: {frames:?}"
        );
        assert!(close_ids(&frames).is_empty(), "nothing to retire yet");

        // ── the socket dies; generation 1 burns and writes nothing ───────────
        drop(server);
        let _ = ws.close(None).await;
        assert!(
            !submit_replacement(
                &mut ws,
                &mut state,
                crate::project::ProjectReplacement::Watched,
                vec![watched_filter(2)],
            )
            .await,
            "a write failure must take the connection down"
        );
        assert_eq!(
            state
                .project_requests
                .current_watched()
                .expect("exactly one watched intent"),
            Some(0),
            "a failed attempt must not move the installed predecessor"
        );

        // ── reconnect, then succeed ──────────────────────────────────────────
        let (mut ws2, mut server2) = test_ws_pair().await;
        state.project_requests.clear_connection();
        assert!(
            submit_replacement(
                &mut ws2,
                &mut state,
                crate::project::ProjectReplacement::Watched,
                vec![watched_filter(3)],
            )
            .await
        );

        // Generation 1 was burned and is gone. The successor is 2, and what it
        // retires is 0 — the generation that was genuinely installed.
        assert_eq!(
            state
                .project_requests
                .current_watched()
                .expect("exactly one watched intent"),
            Some(2)
        );
        let frames = drain_test_frames(&mut server2).await;
        assert_eq!(
            req_ids(&frames),
            vec![crate::project::watched_sub_id(2)],
            "the successor must take a fresh generation, not reuse the failed one: {frames:?}"
        );
        assert_eq!(
            close_ids(&frames),
            vec![crate::project::watched_sub_id(0)],
            "the successor must retire the generation that was actually installed: {frames:?}"
        );

        // ── reconnect replays only the successful successor ──────────────────
        let (mut ws3, mut server3) = test_ws_pair().await;
        let (_tx, mut cmd_rx) = mpsc::channel(1);
        state.project_requests.clear_connection();
        assert!(matches!(
            resubscribe_after_reconnect(&mut ws3, &mut cmd_rx, &mut state, "agent", true).await,
            ResubscribeResult::Ok
        ));
        let replayed = req_ids(&drain_test_frames(&mut server3).await);
        assert_eq!(
            replayed,
            vec![crate::project::watched_sub_id(2)],
            "reconnect must replay only the current durable intent — a retired or \
             failed generation returning here is the defect coming back: {replayed:?}"
        );
    }

    /// **A refused filter installs nothing and retires nothing.**
    ///
    /// This decision used to be made in the command handler *before* the
    /// registry was reached — it warned and returned, while the run loop had
    /// already advanced its generation. So the next replacement named a
    /// generation that had never existed.
    #[tokio::test]
    async fn a_refused_replacement_leaves_the_real_predecessor_current() {
        let (mut ws, mut server) = test_ws_pair().await;
        let mut state = BgState::new();

        assert!(
            submit_replacement(
                &mut ws,
                &mut state,
                crate::project::ProjectReplacement::Watched,
                vec![watched_filter(1)],
            )
            .await
        );
        assert_eq!(
            state
                .project_requests
                .current_watched()
                .expect("exactly one watched intent"),
            Some(0)
        );
        drain_test_frames(&mut server).await;

        // `{"limit": ..}` constrains nothing about *which* events are wanted,
        // so it would ask the relay for everything. The registry refuses to
        // mint an identity from it.
        assert!(
            submit_replacement(
                &mut ws,
                &mut state,
                crate::project::ProjectReplacement::Watched,
                vec![serde_json::json!({ "limit": 500 })],
            )
            .await,
            "a refusal is not a transport failure — the connection is fine"
        );
        let frames = drain_test_frames(&mut server).await;
        assert!(
            frames.is_empty(),
            "a refused replacement must write nothing at all: {frames:?}"
        );
        assert_eq!(
            state
                .project_requests
                .current_watched()
                .expect("exactly one watched intent"),
            Some(0),
            "a refusal must leave the actual predecessor current"
        );

        // And the next valid replacement retires that predecessor — using the
        // generation the refusal did *not* spend.
        //
        // This is where the ordering shows. Validation used to happen after the
        // burn, so the refusal consumed generation 1 and the successor here
        // came out as `proj-roots-2`. A number was retired from the wire
        // identity space by a request that was never going to be written, and
        // the gap it left was indistinguishable from a failed write's.
        assert!(
            submit_replacement(
                &mut ws,
                &mut state,
                crate::project::ProjectReplacement::Watched,
                vec![watched_filter(2)],
            )
            .await
        );
        let frames = drain_test_frames(&mut server).await;
        assert_eq!(
            req_ids(&frames),
            vec![crate::project::watched_sub_id(1)],
            "the refusal must have burned no generation, so the successor is 1: {frames:?}"
        );
        assert_eq!(
            close_ids(&frames),
            vec![crate::project::watched_sub_id(0)],
            "the valid replacement must retire the real predecessor: {frames:?}"
        );
        assert_eq!(
            state
                .project_requests
                .current_watched()
                .expect("exactly one watched intent"),
            Some(1),
        );
    }

    /// **A replacement that changes nothing burns nothing.**
    ///
    /// The other half of validate-before-burn. A genuine no-op used to consume
    /// a generation and write a REQ asking exactly what the relay was already
    /// answering — churning the relay's admission budget to arrive where it
    /// already was, and spending a wire identity to do it.
    ///
    /// Asserted through the command path, and asserted on the *bytes*: a test
    /// that only read `current_watched()` would pass against an implementation
    /// that re-sent the REQ under a fresh id and then reported the same
    /// generation back.
    #[tokio::test]
    async fn an_unchanged_replacement_writes_nothing_and_spends_no_generation() {
        let (mut ws, mut server) = test_ws_pair().await;
        let mut state = BgState::new();

        assert!(
            submit_replacement(
                &mut ws,
                &mut state,
                crate::project::ProjectReplacement::Watched,
                vec![watched_filter(1)],
            )
            .await
        );
        assert_eq!(
            req_ids(&drain_test_frames(&mut server).await),
            vec![crate::project::watched_sub_id(0)]
        );

        // The same question, submitted again.
        assert!(
            submit_replacement(
                &mut ws,
                &mut state,
                crate::project::ProjectReplacement::Watched,
                vec![watched_filter(1)],
            )
            .await
        );
        let frames = drain_test_frames(&mut server).await;
        assert!(
            frames.is_empty(),
            "an unchanged replacement must write no REQ and no CLOSE: {frames:?}"
        );

        // Now a genuinely different question. It must be generation 1 — if the
        // no-op had burned one, this would be 2.
        assert!(
            submit_replacement(
                &mut ws,
                &mut state,
                crate::project::ProjectReplacement::Watched,
                vec![watched_filter(2)],
            )
            .await
        );
        let frames = drain_test_frames(&mut server).await;
        assert_eq!(
            req_ids(&frames),
            vec![crate::project::watched_sub_id(1)],
            "the no-op must have spent no generation: {frames:?}"
        );
        assert_eq!(close_ids(&frames), vec![crate::project::watched_sub_id(0)]);
    }

    // Fifteen proofs stood between here and the end of this block. Each needed
    // a registry born over a composed record, and that route is gone. Where
    // each went:
    //
    // Whole-record refusals — a discovery class under a foreign id, an
    // enrolment class under a foreign id and a foreign class under the
    // enrolment id, a durable root catch-up under every id, a watched id that
    // disagrees with its own generation, a generation the allocator never
    // issued, two watched generations at once, and a poisoned record replaying
    // nothing — are now proved in `crate::project`'s own tests against
    // `validate_persisted_document`, which judges serialised bytes by the same
    // walk the owner makes. They are stronger there in one respect and weaker
    // in another: stronger because the document keeps its member order and
    // cardinality all the way to the rule, so duplicates and
    // malformed-before-canonical orderings are now covered too; weaker because
    // the refusal is no longer observed as "and the socket stayed silent". The
    // silence followed from the refusal, and the refusal is what a rule can
    // own.
    //
    // Allocator ceilings — `spent_watched_generations_refuse_rather_than_reuse`
    // and `a_spent_incarnation_space_refuses_the_replacement` — are proved
    // against `CheckedCounter` in `crate::project`, which nothing can install.
    // Reaching `u64::MAX` honestly costs 2^64 operations, so what is no longer
    // proved anywhere is the registry's *handling* of a spent allocator.
    //
    // The three proofs added in the previous iteration —
    // `discovery_intent_is_refused_into_a_record_that_does_not_resolve`,
    // `a_record_that_does_not_resolve_opens_no_project_request` and
    // `a_page_over_a_record_that_does_not_resolve_burns_no_incarnation` — were
    // about the gate refusing over a corrupt record. No registry can hold one
    // now, so the gate remains as the rule's last line and its refusal arm is
    // unreachable from any proof. The two properties underneath them that are
    // still reachable are kept below.

    /// **The registry's outcome reaches this module unchanged.**
    ///
    /// Each arm is a refusal the registry can make and this module has to
    /// report faithfully. Three of them describe states no honest fixture can
    /// reach — exhaustion is 2^64 operations away and a record that does not
    /// resolve cannot be handed to an owner — so before the mapping was
    /// extracted they were unreachable code with no proof, and reporting
    /// terminal exhaustion as a per-request ownership conflict would have sent
    /// a reader looking for a disagreement that does not exist.
    ///
    /// `Conflict` is the one arm not asserted here: its payload is a
    /// `ProjectRequestIdentity`, which nothing outside the registry can build —
    /// which is the property that closed the earlier findings, and the reason
    /// this list is seven long and six deep.
    #[test]
    fn every_registry_outcome_maps_to_exactly_one_send_outcome() {
        for (outcome, expected) in [
            (crate::project::OpenOutcome::Sent, ProjectSendOutcome::Sent),
            (
                crate::project::OpenOutcome::AlreadyLive,
                ProjectSendOutcome::AlreadyOpen,
            ),
            (
                crate::project::OpenOutcome::Exhausted,
                ProjectSendOutcome::Exhausted,
            ),
            (
                crate::project::OpenOutcome::WriteFailed("socket closed".to_string()),
                ProjectSendOutcome::WriteFailed,
            ),
            (
                crate::project::OpenOutcome::UnboundedFilters,
                ProjectSendOutcome::UnboundedFilters,
            ),
            (
                crate::project::OpenOutcome::InvariantViolation("two watched".to_string()),
                ProjectSendOutcome::InvariantViolation,
            ),
        ] {
            assert_eq!(
                project_send_outcome(&outcome),
                expected,
                "{outcome:?} must report as {expected:?}"
            );
        }
    }

    /// **A replacement retires the predecessor it installed, and only that.**
    ///
    /// The predecessor is established the way production establishes one: by
    /// performing the replacement that installs it. Nothing here chooses a
    /// generation — the registry stamps both — so the `CLOSE` this asserts is a
    /// `CLOSE` for an id this process actually opened, which is the whole
    /// property. A composed predecessor could prove the frame was written and
    /// could not prove that.
    #[tokio::test]
    async fn a_watched_replacement_retires_the_predecessor_it_installed() {
        let (mut ws, mut server) = test_ws_pair().await;
        let mut state = BgState::new();

        // First replacement: nothing to retire.
        assert!(
            submit_replacement(
                &mut ws,
                &mut state,
                crate::project::ProjectReplacement::Watched,
                vec![watched_filter(1)],
            )
            .await
        );
        let frames = drain_test_frames(&mut server).await;
        assert_eq!(
            req_ids(&frames),
            vec![crate::project::watched_sub_id(0)],
            "the first watched generation is 0: {frames:?}"
        );
        assert!(
            close_ids(&frames).is_empty(),
            "a first install has no predecessor to retire: {frames:?}"
        );

        // Second: the successor is installed, then generation 0 is retired.
        assert!(
            submit_replacement(
                &mut ws,
                &mut state,
                crate::project::ProjectReplacement::Watched,
                vec![watched_filter(2)],
            )
            .await
        );
        let frames = drain_test_frames(&mut server).await;
        assert_eq!(
            req_ids(&frames),
            vec![crate::project::watched_sub_id(1)],
            "the successor takes the next generation: {frames:?}"
        );
        assert_eq!(
            close_ids(&frames),
            vec![crate::project::watched_sub_id(0)],
            "and the predecessor it installed is the one retired: {frames:?}"
        );
        assert!(
            state
                .project_requests
                .match_frame(&crate::project::watched_sub_id(0))
                .is_none(),
            "the retired generation is no longer live"
        );
        assert!(
            state
                .project_requests
                .match_frame(&crate::project::watched_sub_id(1))
                .is_some(),
            "and the successor is"
        );
        assert_eq!(
            state
                .project_requests
                .current_watched()
                .expect("one watched intent resolves"),
            Some(1),
            "durable intent moved with it"
        );
    }

    /// **The outcomes that decide before allocation spend nothing.**
    ///
    /// Proved off the wire rather than at the ceiling: a generation's number is
    /// in the id the relay receives, so "this refusal burned nothing" is the
    /// assertion that the next legitimate replacement is still generation 0.
    /// The ceiling made the same point with a spent allocator, which cost a
    /// composed one.
    ///
    /// Every refusal here is one production can reach. `InvalidFilters` and the
    /// unchanged no-op are the two that decide before `burn_watched_generation`
    /// and they are the two that must not consume one — a burned generation is
    /// gone for good, because the number may already have been on the wire.
    #[tokio::test]
    async fn no_outcome_that_decides_before_allocation_spends_a_generation() {
        let (mut ws, mut server) = test_ws_pair().await;
        let mut state = BgState::new();

        // Filters that constrain nothing: refused before allocation.
        for unbounded in [Vec::new(), vec![serde_json::json!({})]] {
            assert!(
                submit_replacement(
                    &mut ws,
                    &mut state,
                    crate::project::ProjectReplacement::Watched,
                    unbounded.clone(),
                )
                .await,
                "an unusable filter is not a transport failure: {unbounded:?}"
            );
            let frames = drain_test_frames(&mut server).await;
            assert!(frames.is_empty(), "nothing may be written: {frames:?}");
        }

        // So the first generation this process names is still 0.
        assert!(
            submit_replacement(
                &mut ws,
                &mut state,
                crate::project::ProjectReplacement::Watched,
                vec![watched_filter(1)],
            )
            .await
        );
        assert_eq!(
            req_ids(&drain_test_frames(&mut server).await),
            vec![crate::project::watched_sub_id(0)],
            "two refusals must have burned no generation"
        );

        // A genuine no-op — same filters, still live — decides before
        // allocation too.
        assert!(
            submit_replacement(
                &mut ws,
                &mut state,
                crate::project::ProjectReplacement::Watched,
                vec![watched_filter(1)],
            )
            .await
        );
        let frames = drain_test_frames(&mut server).await;
        assert!(
            frames.is_empty(),
            "an unchanged replacement writes nothing: {frames:?}"
        );

        // And the next real one is 1, not 2.
        assert!(
            submit_replacement(
                &mut ws,
                &mut state,
                crate::project::ProjectReplacement::Watched,
                vec![watched_filter(2)],
            )
            .await
        );
        let frames = drain_test_frames(&mut server).await;
        assert_eq!(
            req_ids(&frames),
            vec![crate::project::watched_sub_id(1)],
            "the no-op must have spent no generation: {frames:?}"
        );
        assert_eq!(close_ids(&frames), vec![crate::project::watched_sub_id(0)]);
    }

    /// **A refused page burns no incarnation.**
    ///
    /// A catch-up's wire id carries the incarnation it was minted under, so the
    /// allocator's position is readable off the socket. The refusal used here
    /// is one production can reach — a collector that has already observed
    /// something cannot be laundered into a fresh registration — and the page
    /// that follows it still takes incarnation zero.
    #[tokio::test]
    async fn a_refused_page_burns_no_incarnation() {
        use crate::project::HistoryStream;

        let root = test_root_id();
        let (mut ws, mut server) = test_ws_pair().await;
        let mut state = BgState::new();

        let mut cursor = crate::project::HistoryCursor::new(
            crate::project::HistoryScope::Root {
                root: root.clone(),
                stream: HistoryStream::Comments,
            },
            1_000,
            4,
            1_000,
        );
        let mut used = cursor.begin_request();
        used.observe_malformed("a row that arrived before any registration existed");
        assert!(
            matches!(
                state
                    .project_requests
                    .open_history_page(&mut ws, used)
                    .await,
                crate::project::PageOpen::NotPristine
            ),
            "a collector that has already observed something opens no page"
        );
        let frames = drain_test_frames(&mut server).await;
        assert!(frames.is_empty(), "nothing may be written: {frames:?}");

        let page = match state
            .project_requests
            .open_history_page(&mut ws, cursor.begin_request())
            .await
        {
            crate::project::PageOpen::Opened(page) => page,
            other => panic!("a pristine collector must open a page: {other:?}"),
        };
        assert!(
            page.sub_id().ends_with("-0"),
            "the first page must still be minted under incarnation zero: {}",
            page.sub_id()
        );
        let _ = drain_test_frames(&mut server).await;
    }

    #[tokio::test]
    async fn an_already_open_request_does_not_emit_a_second_req() {
        // `AlreadyLive` is not permission to re-send. A second REQ under a live
        // id could replace the relay's subscription while leaving the old
        // request's EOSE indistinguishable from the new one's.
        let (mut ws, mut server) = test_ws_pair().await;
        let mut state = BgState::new();

        for expected in [ProjectSendOutcome::Sent, ProjectSendOutcome::AlreadyOpen] {
            assert_eq!(
                send_project_discovery(&mut ws, &mut state, discovery_filters()).await,
                expected
            );
        }

        assert_eq!(next_test_frame(&mut server).await[0], "REQ");
        assert!(
            timeout(Duration::from_millis(200), server.next())
                .await
                .is_err(),
            "exactly one REQ reached the wire"
        );
    }

    #[tokio::test]
    async fn a_conflicting_send_records_nothing_and_cannot_be_installed_by_a_reconnect() {
        // The trapdoor: an earlier arrangement admitted intent first and asked
        // the registry second, so a refused identity stayed in intent and the
        // next reconnect installed it. Asserting "the socket stayed up" and
        // "the live class is unchanged" did not catch that — the residue was in
        // the map neither assertion looked at.
        let (mut ws, mut server) = test_ws_pair().await;
        let mut state = BgState::new();
        let sub_id = crate::project::discovery_sub_id();

        assert_eq!(
            send_project_discovery(&mut ws, &mut state, discovery_filters()).await,
            ProjectSendOutcome::Sent
        );
        assert_eq!(next_test_frame(&mut server).await[0], "REQ");

        assert_eq!(
            send_project_discovery(&mut ws, &mut state, other_discovery_filters()).await,
            ProjectSendOutcome::MetadataConflict
        );
        assert!(
            timeout(Duration::from_millis(200), server.next())
                .await
                .is_err(),
            "no REQ is emitted"
        );
        assert!(
            intent_asks(&state, &sub_id, &discovery_filters()),
            "the attempted identity is not retained as intent"
        );

        // The decisive part: a fresh connection must reopen the ORIGINAL.
        let (ws2, mut server2) = test_ws_pair().await;
        assert!(matches!(
            reconnect_onto(&mut state, ws2).await,
            ResubscribeResult::Ok
        ));
        assert_eq!(next_test_frame(&mut server2).await[1], sub_id.as_str());
        assert_eq!(
            state.project_requests.match_frame(&sub_id),
            Some(&crate::project::ProjectSubscription::Discovery),
            "the refused identity must not arrive via the reconnect path"
        );
    }

    #[tokio::test]
    async fn a_send_differing_only_in_filter_conflicts_and_the_original_filter_survives() {
        // When the live registry stored only the class it answered "already
        // live" here: no REQ, and the other filter left behind as what the next
        // connection would ask for. Same class, different question.
        let (mut ws, mut server) = test_ws_pair().await;
        let mut state = BgState::new();

        assert_eq!(
            send_project_discovery(&mut ws, &mut state, discovery_filters()).await,
            ProjectSendOutcome::Sent
        );
        assert_eq!(next_test_frame(&mut server).await[0], "REQ");

        assert_eq!(
            send_project_discovery(&mut ws, &mut state, other_discovery_filters()).await,
            ProjectSendOutcome::MetadataConflict,
            "a different filter is a different request, not an idempotent repeat"
        );

        let (ws2, mut server2) = test_ws_pair().await;
        assert!(matches!(
            reconnect_onto(&mut state, ws2).await,
            ResubscribeResult::Ok
        ));
        let replayed = next_test_frame(&mut server2).await;
        assert_eq!(
            replayed[2],
            test_filter(),
            "filter A survives the reconnect"
        );
    }

    #[tokio::test]
    async fn a_relay_closed_suspends_the_request_without_deleting_local_policy() {
        // A relay refusal is transport evidence, not authority over local
        // configuration. Discovery intent derives from
        // `project_routing_enabled`, so letting one CLOSED delete it would let
        // the relay revoke an operator's decision and keep it revoked across
        // every later healthy connection.
        let mut state = BgState::new();
        let (tx, mut rx) = mpsc::channel(16);
        let keys = nostr::Keys::generate();

        // 1. open and send discovery.
        let (mut ws, mut server) = test_ws_pair().await;
        let sub_id = crate::project::discovery_sub_id();
        assert_eq!(
            send_project_discovery(&mut ws, &mut state, discovery_filters()).await,
            ProjectSendOutcome::Sent
        );
        assert_eq!(next_test_frame(&mut server).await[0], "REQ");

        // 2. receive CLOSED.
        assert!(
            deliver_control_frame(
                &mut state,
                json!(["CLOSED", sub_id, "restricted: not permitted"]),
            )
            .await,
            "a project CLOSED must not tear down the socket"
        );

        // 3. the id is immediately unanswerable.
        assert!(state.project_requests.match_frame(&sub_id).is_none());
        let event = announcement(&keys, 1_000);
        deliver_frame(&mut state, &sub_id, &event, &tx).await;
        assert!(drain(&mut rx).is_empty());

        // 4. intent remains, and the refusal is recorded rather than silent.
        assert!(state.project_requests.intent(&sub_id).is_some());
        assert_eq!(
            state.project_requests.suspension(&sub_id),
            Some("restricted: not permitted")
        );

        // 7. no retry on the connection that refused — including through a
        //    *proactive* resubscribe on the existing socket, which is the path
        //    that made "no immediate retry" an insufficient assertion.
        assert!(matches!(
            resubscribe_on_same_socket(&mut state, &mut ws).await,
            ResubscribeResult::Ok
        ));
        assert!(
            timeout(Duration::from_millis(200), server.next())
                .await
                .is_err(),
            "a proactive resubscribe must not re-send what this connection refused"
        );
        assert!(state.project_requests.match_frame(&sub_id).is_none());
        assert_eq!(
            state.project_requests.suspension(&sub_id),
            Some("restricted: not permitted"),
            "and the suspension is still in force"
        );

        // 5 & 6. a fresh connection asks once more and registers it again.
        let (ws2, mut server2) = test_ws_pair().await;
        assert!(matches!(
            reconnect_onto(&mut state, ws2).await,
            ResubscribeResult::Ok
        ));
        let frame = next_test_frame(&mut server2).await;
        assert_eq!(frame[0], "REQ");
        assert_eq!(frame[1], sub_id.as_str());
        assert!(state.project_requests.match_frame(&sub_id).is_some());
        assert_eq!(state.project_requests.suspension(&sub_id), None);
    }

    #[tokio::test]
    async fn an_unsolicited_closed_cannot_suspend_a_request_that_was_never_sent() {
        // `CLOSED` must be authenticated by an exact live registration, exactly
        // as EVENT provenance is. Gating on durable intent instead let relay
        // text mutate suspension state for an id that had never been asked —
        // the relay could suppress a request before it was ever made.
        let mut state = BgState::new();
        let sub_id = crate::project::discovery_sub_id();
        assert_eq!(
            state
                .project_requests
                .record_discovery_intent(discovery_filters()),
            crate::project::IntentAdmission::Recorded
        );

        assert!(
            deliver_control_frame(&mut state, json!(["CLOSED", sub_id, "restricted: nope"])).await
        );

        assert_eq!(
            state.project_requests.suspension(&sub_id),
            None,
            "an id that was never live cannot be suspended by relay text"
        );
        assert!(
            intent_asks(&state, &sub_id, &discovery_filters()),
            "and local policy is untouched"
        );

        // The proof it matters: the next connection still asks.
        let (ws, mut server) = test_ws_pair().await;
        assert!(matches!(
            reconnect_onto(&mut state, ws).await,
            ResubscribeResult::Ok
        ));
        assert_eq!(next_test_frame(&mut server).await[1], sub_id.as_str());
    }

    #[tokio::test]
    async fn a_failed_project_replay_retries_the_connection_instead_of_stranding_intent() {
        // Continuing past a failed replay would report a healthy connection on
        // which project routing is silently dead, with intent retained but
        // inactive and possibly no later reconnect to notice.
        let mut state = BgState::new();
        state
            .project_requests
            .record_discovery_intent(discovery_filters());

        let (mut ws, server) = test_ws_pair().await;
        drop(server);
        let _ = ws.close(None).await;

        assert!(
            matches!(
                reconnect_onto(&mut state, ws).await,
                ResubscribeResult::RetryConnection
            ),
            "a failed project replay must not be reported as a healthy connection"
        );
    }

    #[tokio::test]
    async fn a_fresh_connection_clears_registrations_but_keeps_intent() {
        // `BgState` outlives the socket. Without clearing, a new connection
        // would answer ids registered against the dead one before their
        // replacement REQs had been sent — and the ids are deterministic, so
        // that is easy to hit rather than exotic.
        let (mut ws, mut server) = test_ws_pair().await;
        let mut state = BgState::new();
        let sub_id = crate::project::discovery_sub_id();
        send_project_discovery(&mut ws, &mut state, discovery_filters()).await;
        assert_eq!(next_test_frame(&mut server).await[0], "REQ");

        state.project_requests.clear_connection();

        assert!(
            state.project_requests.match_frame(&sub_id).is_none(),
            "the dead connection's registration is gone"
        );
        assert!(
            state.project_requests.intent(&sub_id).is_some(),
            "but what we want subscribed survives, to be re-asked"
        );
    }

    /// Did an EOSE for `sub_id` mint a boundary?
    ///
    /// A boundary no longer leaves the relay task, so there is no delivery to
    /// count. What it does instead is exactly two things — retire a one-shot
    /// catch-up registration and complete the page that registration opened —
    /// and both are asserted here. There is nothing weaker available: a
    /// persistent request's boundary changes no state at all, which is why the
    /// tests below reconstruct a root instead of opening discovery.
    ///
    /// Both facts are returned rather than reduced to one, because the
    /// disagreement between them is itself a defect: a registration retired
    /// without its page completing, or a page completed while its registration
    /// stayed answerable, is precisely the state where the registry and the
    /// page owner no longer describe the same request.
    #[derive(Debug, PartialEq, Eq)]
    struct EoseOutcome {
        /// Is a registration still live under that id?
        still_live: bool,
        /// Does the reconstruction now hold a finished stream?
        page_finished: bool,
    }

    async fn eose_outcome(
        state: &mut BgState,
        root: &str,
        sub_id: &str,
        tx: &mpsc::Sender<Option<BuzzEvent>>,
    ) -> EoseOutcome {
        assert!(
            deliver_control_frame_to(state, json!(["EOSE", sub_id]), tx).await,
            "dispatch must not signal connection loss"
        );
        EoseOutcome {
            still_live: state.project_requests.match_frame(sub_id).is_some(),
            // A stream that finished is either still held by a reconstruction
            // with another stream outstanding, or — for a root whose last
            // required stream this was — merged, handed on, and recorded as it
            // retired. Both are "the page finished"; only one of them leaves a
            // reconstruction to ask.
            page_finished: match state.reconstructions.get(root) {
                Some(recon) => !recon.finished_streams().is_empty(),
                None => state.root_catch_up_done.contains_key(root),
            },
        }
    }

    /// Feed a control frame with a caller-supplied event channel.
    async fn deliver_control_frame_to(
        state: &mut BgState,
        frame: Value,
        tx: &mpsc::Sender<Option<BuzzEvent>>,
    ) -> bool {
        let text = serde_json::to_string(&frame).expect("encode");
        dispatch_over_fresh_connection(state, text, tx).await
    }

    #[tokio::test]
    async fn an_eose_on_a_live_project_request_produces_a_witness() {
        let mut state = BgState::new();
        let (tx, _rx) = mpsc::channel(16);
        let (bound, _keys) = proven_issue_root().await;
        let root = bound.binding().root().to_string();
        let sub_id = bind_page_under(&mut state, &bound).await;

        assert_eq!(
            eose_outcome(&mut state, &root, &sub_id, &tx).await,
            EoseOutcome {
                still_live: false,
                page_finished: true
            },
            "a live request's backlog boundary is evidence, and answering the \
             one question a catch-up asked ends it"
        );
    }

    #[tokio::test]
    async fn an_eose_for_a_request_this_agent_never_sent_produces_no_witness() {
        // The property that makes the witness worth anything. Without it, any
        // peer able to name a `proj-` id could assert that our backlog was
        // complete — and a completion claim resting on that is resting on the
        // relay's word.
        let mut state = BgState::new();
        let (tx, _rx) = mpsc::channel(16);
        let (bound, _keys) = proven_issue_root().await;
        let root = bound.binding().root().to_string();
        let sub_id = bind_page_under(&mut state, &bound).await;

        for id in [
            crate::project::discovery_sub_id(),
            crate::project::watched_sub_id(0),
            // Catch-up shaped, and nobody minted it: the registry is the
            // only thing that names a page, so this is exactly what an
            // invented one looks like.
            format!("proj-catchup-c-{}-1", test_root_id()),
            "proj-unknown".to_string(),
        ] {
            assert_eq!(
                eose_outcome(&mut state, &root, &id, &tx).await,
                EoseOutcome {
                    still_live: false,
                    page_finished: false
                },
                "{id} was never opened, so it is nothing and completes nothing"
            );
        }

        // The page it could not complete is still there to be completed.
        assert_eq!(
            eose_outcome(&mut state, &root, &sub_id, &tx).await,
            EoseOutcome {
                still_live: false,
                page_finished: true
            }
        );
    }

    #[tokio::test]
    async fn an_eose_witness_names_the_request_it_was_asked_about() {
        // Found by mutation: the "never sent" test above once had *nothing*
        // live, so an implementation that fell back to whichever request
        // happened to be open would pass it. Here a catch-up is live and the
        // EOSE names a watched id that was never opened — a witness of any kind
        // is wrong, and one naming the catch-up would complete a page on the
        // strength of a boundary that belongs to no request at all.
        let mut state = BgState::new();
        let (tx, _rx) = mpsc::channel(16);
        let (bound, _keys) = proven_issue_root().await;
        let root = bound.binding().root().to_string();
        let live = bind_page_under(&mut state, &bound).await;
        let never_opened = crate::project::watched_sub_id(0);
        assert_ne!(live, never_opened);

        assert_eq!(
            eose_outcome(&mut state, &root, &never_opened, &tx).await,
            EoseOutcome {
                still_live: false,
                page_finished: false
            },
            "an unopened id must not borrow a live request's boundary"
        );
        assert!(
            state.project_requests.match_frame(&live).is_some(),
            "and must not retire it either"
        );

        // And the live request's own EOSE still names itself.
        assert_eq!(
            eose_outcome(&mut state, &root, &live, &tx).await,
            EoseOutcome {
                still_live: false,
                page_finished: true
            }
        );
    }

    #[tokio::test]
    async fn a_closed_request_stops_being_able_to_witness_an_eose() {
        // A relay that refuses a request and then announces its backlog is
        // complete must not be believed about the request it just declined.
        let mut state = BgState::new();
        let (tx, _rx) = mpsc::channel(16);
        let (bound, _keys) = proven_issue_root().await;
        let root = bound.binding().root().to_string();
        let sub_id = bind_page_under(&mut state, &bound).await;

        assert!(
            deliver_control_frame_to(
                &mut state,
                json!(["CLOSED", sub_id, "restricted: nope"]),
                &tx,
            )
            .await
        );
        assert_eq!(
            eose_outcome(&mut state, &root, &sub_id, &tx).await,
            EoseOutcome {
                still_live: false,
                page_finished: false
            },
            "the refused request has no backlog boundary left to give"
        );
    }

    #[tokio::test]
    async fn a_fresh_connection_invalidates_an_earlier_requests_eose() {
        // Witnesses are connection-scoped because registrations are. An EOSE
        // arriving for the dead connection's id, before its replacement REQ has
        // been written, is not evidence about the new connection's backlog.
        let mut state = BgState::new();
        let (tx, _rx) = mpsc::channel(16);
        let (bound, _keys) = proven_issue_root().await;
        let root = bound.binding().root().to_string();
        let sub_id = bind_page_under(&mut state, &bound).await;

        state.retire_project_connection();
        assert_eq!(
            eose_outcome(&mut state, &root, &sub_id, &tx).await,
            EoseOutcome {
                still_live: false,
                page_finished: false
            },
            "the request that EOSE refers to no longer exists"
        );

        // Once a replacement page is genuinely opened, EOSE means something
        // again — under the replacement's own name, and with nobody closing
        // anything by hand.
        let sub_id = open_page_under(&mut state, &root).await;
        assert_eq!(
            eose_outcome(&mut state, &root, &sub_id, &tx).await,
            EoseOutcome {
                still_live: false,
                page_finished: true
            }
        );
    }

    #[tokio::test]
    async fn notices_and_closed_frames_never_mint_a_witness() {
        // Only an EOSE frame on a live request is a boundary. Anything else
        // arriving about that request — a NOTICE, a CLOSED — must produce
        // none, or "the backlog drained" becomes inferrable from noise.
        let mut state = BgState::new();
        let (tx, _rx) = mpsc::channel(16);
        let (bound, _keys) = proven_issue_root().await;
        let root = bound.binding().root().to_string();
        let sub_id = bind_page_under(&mut state, &bound).await;

        assert!(
            deliver_control_frame_to(&mut state, json!(["NOTICE", "something happened"]), &tx)
                .await
        );
        assert!(
            state.project_requests.match_frame(&sub_id).is_some()
                && state
                    .reconstructions
                    .get(&root)
                    .expect("tracked")
                    .finished_streams()
                    .is_empty(),
            "a NOTICE is not a boundary"
        );

        assert!(deliver_control_frame_to(&mut state, json!(["CLOSED", sub_id, "done"]), &tx).await);
        assert!(
            state
                .reconstructions
                .get(&root)
                .expect("tracked")
                .finished_streams()
                .is_empty(),
            "a CLOSED is not a boundary"
        );
    }

    // `an_exhausted_incarnation_space_writes_nothing_and_keeps_the_socket`
    // stood here. It proved that a spent incarnation space surfaces as
    // `ProjectSendOutcome::Exhausted` rather than `MetadataConflict` — a
    // diagnostic pointing at the right subsystem — and it needed a registry
    // whose allocator was composed at `u64::MAX`. The arithmetic is proved
    // against `CheckedCounter` in `crate::project`; the mapping from a spent
    // allocator to that outcome is not proved anywhere now, because reaching
    // the state honestly costs 2^64 registrations.

    // Deleted 2026-08-01: `a_consumer_can_name_and_store_the_incarnation_it_opened_under`.
    //
    // Its premise was that a reconstruction driver stores the incarnation it
    // opened under and compares a later boundary against it. That is the
    // caller-holds-authority design, rejected twice in review; the test existed
    // to prove the rejected shape was expressible. Completion now decides via
    // `OpenedHistoryPage::verdict_for`, and the property that matters — a
    // predecessor's boundary cannot complete the replacement — is covered by
    // the `predecessor_witness_*` falsifiers in `project.rs`.

    // Deleted 2026-08-01: `a_dropped_eose_leaves_no_boundary_and_no_synthetic_replay_floor`.
    //
    // It covered the full-channel branch of the boundary's delivery to the run
    // loop. That delivery is gone — a boundary is a capability the run loop
    // could not use, and it is now consumed where it is minted — so there is no
    // queue to wedge and no drop to reason about. What survives of its subject
    // is `a_persistent_requests_boundary_retires_nothing`, which holds the
    // "still live afterwards" half against the class it is actually true for.

    #[tokio::test]
    async fn a_persistent_requests_boundary_retires_nothing() {
        // The other half of the one-shot rule. Discovery, enrolment and watched
        // subscriptions keep delivering after their stored backlog drains, so
        // retiring them on EOSE would silently stop routing at the moment a
        // backlog ended — the failure being avoided is the mirror image of the
        // one that made catch-up retirement necessary.
        let mut state = BgState::new();
        let (tx, _rx) = mpsc::channel(16);
        let discovery = open_discovery(&mut state).await;
        let watched = open_watched(&mut state).await;

        for id in [&discovery, &watched] {
            assert!(deliver_control_frame_to(&mut state, json!(["EOSE", id]), &tx).await);
            assert!(
                state.project_requests.match_frame(id).is_some(),
                "{id} still has live traffic to deliver"
            );
        }

        assert_eq!(
            state.project_dropped_since, None,
            "and an EOSE contributes nothing to a replay floor measured in event timestamps"
        );
    }

    #[tokio::test]
    async fn an_eose_leaves_a_persistent_project_request_answerable() {
        // EOSE means end of *stored* events, not end of subscription. Removing
        // discovery/enrolment/watched on EOSE would silently stop live routing
        // the moment the backlog drained.
        let mut state = BgState::new();
        let (tx, mut rx) = mpsc::channel(16);
        let keys = nostr::Keys::generate();
        let sub_id = open_discovery(&mut state).await;

        assert!(deliver_control_frame(&mut state, json!(["EOSE", sub_id])).await);

        assert!(state.project_requests.match_frame(&sub_id).is_some());
        let event = announcement(&keys, 1_000);
        deliver_frame(&mut state, &sub_id, &event, &tx).await;
        assert_eq!(
            drain(&mut rx).len(),
            1,
            "still delivering after end of stored events"
        );
    }

    #[tokio::test]
    async fn a_conflicting_subscribe_command_keeps_the_socket_and_sends_nothing() {
        // `true` here means "the connection is fine". Returning `false` for a
        // metadata conflict let a locally refused command tear down a healthy
        // connection — and the reconnect then replayed the refusal into effect.
        let (mut ws, mut server) = test_ws_pair().await;
        let mut state = BgState::new();
        let agent = nostr::Keys::generate().public_key().to_hex();
        let sub_id = crate::project::discovery_sub_id();

        assert!(
            execute_connected_command(
                &mut ws,
                &mut state,
                &agent,
                RelayCommand::SubscribeProjectDiscovery {
                    filters: vec![test_filter()],
                },
            )
            .await
        );
        assert_eq!(next_test_frame(&mut server).await[1], sub_id.as_str());

        assert!(
            execute_connected_command(
                &mut ws,
                &mut state,
                &agent,
                RelayCommand::SubscribeProjectDiscovery {
                    filters: vec![json!({ "kinds": [1] })],
                },
            )
            .await,
            "a locally refused command must not be reported as a dead socket"
        );
        assert!(
            timeout(Duration::from_millis(200), server.next())
                .await
                .is_err(),
            "and must emit no REQ"
        );
        assert!(intent_asks(&state, &sub_id, &discovery_filters()));
    }

    /// A subscribe command that constrains nothing opens nothing, on either
    /// side of the connection.
    ///
    /// Three shapes, one failure. `["REQ", id]`, `["REQ", id, {}]` and
    /// `["REQ", id, {"limit": 500}]` are not narrower requests than a filtered
    /// one — they are unbounded, and each would install a registration that
    /// admitted every event the relay chose to send under that id. The empty
    /// *vector* was refused when the filter list arrived; the other two were
    /// not, because "the collection is non-empty" is not the same claim as
    /// "the request selects something".
    ///
    /// `watched_roots_filters` returns an empty vector when nothing is enrolled,
    /// so the first is reachable from the builder rather than only from a
    /// mistake.
    ///
    /// The disconnected half matters just as much: intent recorded there is
    /// replayed verbatim by the next connection, so an unbounded intent sends
    /// the unbounded REQ later rather than now. That is the leg the reconnect
    /// assertion below covers.
    #[tokio::test]
    async fn a_subscribe_command_that_constrains_nothing_opens_nothing() {
        // Discovery, because that is the only subscription a command can now
        // open. The refusal being proved is the filter list's, not the class's:
        // `watched_roots_filters` returning an empty vector is where an
        // unbounded list comes from in practice, and it reaches the registry
        // through the same `from_filters` gate this exercises.
        let sub_id = crate::project::discovery_sub_id();

        for filters in [Vec::new(), vec![json!({})], vec![json!({ "limit": 500 })]] {
            // ---- Disconnected: no durable intent, so no later replay. ------
            let mut disconnected = BgState::new();
            apply_command_to_state(
                &mut disconnected,
                RelayCommand::SubscribeProjectDiscovery {
                    filters: filters.clone(),
                },
            );
            assert_eq!(
                disconnected.project_requests.intent(&sub_id),
                None,
                "{filters:?}: no intent"
            );
            assert!(
                disconnected
                    .project_requests
                    .replayable()
                    .expect("a canonical record")
                    .is_empty(),
                "{filters:?}: and nothing for a reconnect to re-ask"
            );

            // ---- Connected: no bytes, no registration, no authority. -------
            let (mut ws, mut server) = test_ws_pair().await;
            let mut state = BgState::new();
            let agent = nostr::Keys::generate().public_key().to_hex();
            assert!(
                execute_connected_command(
                    &mut ws,
                    &mut state,
                    &agent,
                    RelayCommand::SubscribeProjectDiscovery {
                        filters: filters.clone(),
                    },
                )
                .await,
                "{filters:?}: refusing to ask for everything is not a transport failure"
            );
            assert!(
                timeout(Duration::from_millis(200), server.next())
                    .await
                    .is_err(),
                "{filters:?}: writes no REQ"
            );
            assert!(
                state.project_requests.match_frame(&sub_id).is_none(),
                "{filters:?}: registers nothing"
            );
            assert!(
                state.project_requests.admit_frame(&sub_id).is_none(),
                "{filters:?}: and admits no frame under that id"
            );
            assert_eq!(
                state.project_requests.intent(&sub_id),
                None,
                "{filters:?}: no intent"
            );
            assert!(
                state
                    .project_requests
                    .replayable()
                    .expect("a canonical record")
                    .is_empty(),
                "{filters:?}: and a reconnect re-asks nothing"
            );
        }
    }

    #[test]
    fn a_disconnected_conflicting_command_keeps_the_first_intent() {
        // Intent recorded while disconnected is replayed verbatim by the next
        // connection, so admitting a conflict here is admitting it everywhere.
        let mut state = BgState::new();
        let sub_id = crate::project::discovery_sub_id();
        for filters in [discovery_filters(), other_discovery_filters()] {
            apply_command_to_state(
                &mut state,
                RelayCommand::SubscribeProjectDiscovery { filters },
            );
        }

        assert!(intent_asks(&state, &sub_id, &discovery_filters()));
    }

    // ── Only requests this agent opened are answerable ───────────────────────

    #[tokio::test]
    async fn a_frame_on_a_project_id_this_agent_never_opened_is_refused() {
        // Every one of these ids used to work. `classify_subscription` parsed
        // the class out of the relay's own string, so a relay could deliver
        // under `proj-roots-7` and be believed — no REQ of ours required.
        //
        // Refusal must be total: no delivery, no dedup spend, no verification
        // side effects. Nothing here is registered, so nothing is answerable.
        //
        // Both event shapes, deliberately. A rooted event is what a `Watched`
        // or `RootCatchUp` misclassification would admit; an announcement is
        // what a `Discovery` misclassification would admit. Testing only one
        // shape lets the *other* source's admissibility gate catch the frame
        // and look like the registry did it — I found that by mutating the
        // registry to fall back to `Discovery` and watching this test pass
        // anyway.
        let keys = nostr::Keys::generate();
        let rooted = mixed_surface_event(&keys, Uuid::new_v4(), &test_root_id(), 1_000);
        let announced = announcement(&keys, 1_000);

        for id in [
            crate::project::watched_sub_id(0),
            crate::project::watched_sub_id(7),
            crate::project::discovery_sub_id(),
            // Catch-up shaped, and nobody minted it: the registry is the
            // only thing that names a page, so this is exactly what an
            // invented one looks like.
            format!("proj-catchup-c-{}-1", test_root_id()),
            crate::project::PROJECT_ENROL_SUB_ID.to_string(),
            "proj-unknown".to_string(),
        ] {
            for event in [&rooted, &announced] {
                let mut state = BgState::new();
                let (tx, mut rx) = mpsc::channel(16);

                deliver_frame(&mut state, &id, event, &tx).await;

                let eid = event.id.to_hex();
                assert!(drain(&mut rx).is_empty(), "{id}/{eid}: no delivery");
                assert!(
                    !state.project_seen_ids.contains(&eid),
                    "{id}/{eid}: no project dedup slot spent"
                );
                assert!(
                    !state.seen_ids.contains(&eid),
                    "{id}/{eid}: no channel dedup slot spent either"
                );
            }
        }
    }

    #[tokio::test]
    async fn a_retired_project_request_stops_being_answerable() {
        // The half a parser could never express. A subscription id does not
        // stop being well-formed when we stop listening, so late frames for a
        // request we have finished with used to keep working.
        let mut state = BgState::new();
        let (tx, mut rx) = mpsc::channel(16);
        let keys = nostr::Keys::generate();
        let event_a = mixed_surface_event(&keys, Uuid::new_v4(), &test_root_id(), 1_000);
        let event_b = mixed_surface_event(&keys, Uuid::new_v4(), &test_root_id(), 1_001);

        let retired = open_watched_for(&mut state, &[&test_root_id()]).await;
        deliver_frame(&mut state, &retired, &event_a, &tx).await;
        assert_eq!(drain(&mut rx).len(), 1, "open: delivered");

        // The production route that stops a request being answerable: a
        // watched replacement retires its predecessor's live registration the
        // instant the successor installs. It used to be `close_active`, a
        // registry method with no production caller at all.
        let successor = open_watched_for(&mut state, &[&test_root_id(), &"c".repeat(64)]).await;
        assert_ne!(retired, successor, "the successor takes a new generation");

        deliver_frame(&mut state, &retired, &event_b, &tx).await;
        assert!(drain(&mut rx).is_empty(), "retired: not delivered");
        assert!(
            !state.project_seen_ids.contains(&event_b.id.to_hex()),
            "a retired request spends nothing"
        );

        // The positive control: the successor still answers, so the refusal
        // above is not the whole surface being dead.
        deliver_frame(&mut state, &successor, &event_b, &tx).await;
        assert_eq!(drain(&mut rx).len(), 1, "the successor delivers");
    }

    // Deleted 2026-08-01: `a_catch_up_is_bound_to_the_root_we_recorded_not_the_one_the_id_spells`.
    //
    // It registered a catch-up under an id spelling one root while recording
    // another, and proved the recorded class decided. That state is no longer
    // constructible: a catch-up wire id is minted by
    // `ProjectRequests::open_history_page` from the collector's own root and
    // stream, and no caller — production or test — can supply one. What the
    // test guarded against is now unrepresentable rather than merely refused.
    //
    // The surviving half of its subject, that a frame naming another root is
    // not one of this page's rows, is
    // `a_catch_up_root_mismatch_does_not_burn_the_correct_rooted_delivery`.

    // Deleted 2026-08-02: `a_refused_reopen_leaves_the_original_class_in_force_on_the_wire`.
    //
    // It re-pointed a live catch-up id at a `Watched` identity through
    // `ProjectRequests::open_request`, and proved the refusal held all the way
    // to dispatch. That call no longer exists for any caller: `open_request` is
    // private to the registry, and its two entry points are `open_discovery`,
    // which stamps `Discovery` under the discovery id, and `open_replayed`,
    // whose argument the registry mints from its own validated record. Nothing
    // -- production or test -- can aim an identity of its choosing at an id of
    // its choosing, so the reopen this guarded against is unrepresentable
    // rather than refused.
    //
    // The surviving half of its subject, that opening refuses to change what an
    // id holds, is asserted on the one class a caller can still open twice, in
    // `a_conflicting_send_records_nothing_and_cannot_be_installed_by_a_reconnect`.

    // ── The membership subscription accepts only membership kinds ────────────

    fn membership_notification(keys: &nostr::Keys, channel_id: Uuid, kind: u32, ts: u64) -> Event {
        EventBuilder::new(nostr::Kind::Custom(kind as u16), "membership")
            .custom_created_at(nostr::Timestamp::from(ts))
            .tags([
                nostr::Tag::parse(vec!["h".to_string(), channel_id.to_string()]).expect("h tag"),
                nostr::Tag::parse(vec!["p".to_string(), keys.public_key().to_hex()])
                    .expect("p tag"),
            ])
            .sign_with_keys(keys)
            .expect("signing should succeed")
    }

    #[tokio::test]
    async fn a_wrong_kind_on_the_membership_subscription_changes_nothing() {
        // The watermark poisoning path. `membership_last_seen` is used
        // directly as the membership REQ's `since` on reconnect, so a
        // wrong-kind event with a far-future timestamp used to push that
        // watermark past legitimate membership notifications — which were then
        // never replayed. That is loss, not a widened duplicate window: it
        // *narrows* the replay window.
        let mut state = BgState::new();
        let (tx, mut rx) = mpsc::channel(16);
        let keys = nostr::Keys::generate();
        let channel_id = Uuid::new_v4();
        // A perfectly valid channel message, delivered on the wrong sub.
        let event = mixed_surface_event(&keys, channel_id, &test_root_id(), 9_999_999);
        let id = event.id.to_hex();

        deliver_frame(&mut state, MEMBERSHIP_NOTIF_SUB_ID, &event, &tx).await;

        assert!(drain(&mut rx).is_empty(), "no delivery");
        assert!(
            !state.seen_ids.contains(&id),
            "no channel dedup slot consumed"
        );
        assert_eq!(
            state.membership_last_seen, None,
            "the membership watermark is not poisoned"
        );
        assert!(state.last_seen.is_empty(), "no channel watermark moved");
        assert_eq!(state.membership_dropped_since, None);

        // And the event is still deliverable through its legitimate channel
        // subscription — refusing it here costs it nothing it is entitled to.
        deliver_frame(&mut state, &channel_sub_id(channel_id), &event, &tx).await;
        let delivered = drain(&mut rx);
        assert_eq!(delivered.len(), 1);
        assert!(matches!(delivered[0], BuzzEvent::Channel { .. }));
    }

    #[tokio::test]
    async fn the_membership_subscription_still_accepts_both_membership_kinds() {
        // Positive controls, so the gate cannot be "fixed" by refusing
        // everything.
        for kind in [
            KIND_MEMBER_ADDED_NOTIFICATION,
            KIND_MEMBER_REMOVED_NOTIFICATION,
        ] {
            let mut state = BgState::new();
            let (tx, mut rx) = mpsc::channel(16);
            let keys = nostr::Keys::generate();
            let channel_id = Uuid::new_v4();
            let event = membership_notification(&keys, channel_id, kind, 1_000);

            deliver_frame(&mut state, MEMBERSHIP_NOTIF_SUB_ID, &event, &tx).await;

            let delivered = drain(&mut rx);
            assert_eq!(delivered.len(), 1, "kind {kind} must still be delivered");
            match &delivered[0] {
                BuzzEvent::Channel { channel_id: ch, .. } => assert_eq!(*ch, channel_id),
                other => panic!("expected a channel delivery for kind {kind}: {other:?}"),
            }
            assert_eq!(
                state.membership_last_seen,
                Some(1_000),
                "a genuine membership notification does advance the watermark"
            );
        }
    }

    // ── NIP-PC peer calls ────────────────────────────────────────────────────

    /// The REQ that has to exist for any of this to be reachable.
    ///
    /// Asserted on the filters the subscription actually sends, not on observed
    /// behaviour: a subscription that asks the wrong question fails by delivering
    /// nothing, which is indistinguishable from a quiet relay and passes every
    /// test written downstream of it.
    #[test]
    fn the_peer_call_request_asks_for_calls_to_us_and_the_calls_we_made() {
        let agent = "ab".repeat(32);
        let filters = peer_call_filters(&agent, 1_000);
        assert_eq!(
            filters.len(),
            2,
            "inbound and own-authored are two questions"
        );

        // Inbound: a call for us, and a result addressed to us.
        assert_eq!(
            filters[0]["kinds"],
            json!([KIND_PEER_CALL, KIND_PEER_CALL_RESULT])
        );
        assert_eq!(filters[0]["#p"], json!([agent]));
        assert!(filters[0].get("authors").is_none());

        // Our own calls. Without this the ledger never learns a call was made,
        // because the harness does not publish calls — the agent subprocess
        // does — and every returned result would correlate to nothing.
        assert_eq!(filters[1]["kinds"], json!([KIND_PEER_CALL]));
        assert_eq!(filters[1]["authors"], json!([agent]));
        assert!(filters[1].get("#p").is_none());

        for f in &filters {
            assert_eq!(f["since"], json!(1_000));
        }
    }

    fn peer_call_frame(keys: &nostr::Keys, tags: &[&[&str]], kind: u32) -> Event {
        EventBuilder::new(nostr::Kind::Custom(kind as u16), "do the thing")
            .tags(
                tags.iter()
                    .map(|t| nostr::Tag::parse(t.iter().copied()).expect("tag")),
            )
            .sign_with_keys(keys)
            .expect("sign")
    }

    /// A channel-routed envelope takes the channel path; a project-routed one
    /// now takes the project path instead of being dropped.
    ///
    /// It used to be dropped here, on the assumption that the watched-root REQ
    /// owned it. That holds only while the root is already enrolled and a
    /// watched generation is live, so a call arriving before enrolment or
    /// inside a REQ replacement window was discarded by both paths — which is
    /// exactly what the Phase 6 runtime observed: the call ingested, both
    /// agents received it on this subscription, and both logged it away.
    #[tokio::test]
    async fn the_peer_call_subscription_routes_the_channel_and_the_project_envelope() {
        let keys = nostr::Keys::generate();
        let channel_id = Uuid::new_v4();
        let root = "48be1cc2000000000000000000000000000000000000000000000000000000ab";

        let channel_routed = peer_call_frame(
            &keys,
            &[&["p", &"cd".repeat(32)], &["h", &channel_id.to_string()]],
            KIND_PEER_CALL,
        );
        let mut state = BgState::new();
        let (tx, mut rx) = mpsc::channel(16);
        deliver_frame(&mut state, PEER_CALL_SUB_ID, &channel_routed, &tx).await;
        let delivered = drain(&mut rx);
        assert_eq!(delivered.len(), 1);
        match &delivered[0] {
            BuzzEvent::Channel { channel_id: ch, .. } => assert_eq!(*ch, channel_id),
            other => panic!("expected a channel delivery: {other:?}"),
        }
        assert!(
            !state.project_seen_ids.contains(&channel_routed.id.to_hex()),
            "a channel call must not spend the project dedup slot"
        );

        // The same subscription, a project route: no `h`, so it reaches the
        // project gate — which is where every authority question is asked.
        let project_routed = peer_call_frame(
            &keys,
            &[
                &["p", &"cd".repeat(32)],
                &["a", &format!("30617:{}:buzz", "ef".repeat(32))],
                &["e", root, "", "root"],
            ],
            KIND_PEER_CALL,
        );
        let mut state = BgState::new();
        let (tx, mut rx) = mpsc::channel(16);
        deliver_frame(&mut state, PEER_CALL_SUB_ID, &project_routed, &tx).await;
        let delivered = drain(&mut rx);
        assert_eq!(delivered.len(), 1, "the project envelope was dropped again");
        match &delivered[0] {
            BuzzEvent::Project(crate::project::ProjectEvent::Routed { source, route, .. }) => {
                assert_eq!(
                    *source,
                    crate::project::ProjectSubscription::PeerCall,
                    "the source must name the transport it arrived on, not a watched generation"
                );
                assert_eq!(route.root(), root);
            }
            other => panic!("expected a project delivery: {other:?}"),
        }
        assert!(
            state.project_seen_ids.contains(&project_routed.id.to_hex()),
            "the project slot must be spent so the watched REQ's copy is a no-op"
        );
        assert!(
            !state.seen_ids.contains(&project_routed.id.to_hex()),
            "a project call must not spend a channel slot"
        );
    }

    /// One call, two subscriptions, one delivery.
    ///
    /// The peer REQ matches by `#p`/`authors` and the watched REQ by `#e`, so
    /// both can carry the same signed call and which arrives first is a race.
    /// They share `project_seen_ids` precisely so the loser is an exact no-op
    /// rather than a second turn on one issue.
    #[tokio::test]
    async fn the_same_call_on_the_peer_and_watched_subscriptions_is_delivered_once() {
        let keys = nostr::Keys::generate();
        let root = "48be1cc2000000000000000000000000000000000000000000000000000000ab";
        let call = peer_call_frame(
            &keys,
            &[
                &["p", &"cd".repeat(32)],
                &["a", &format!("30617:{}:buzz", "ef".repeat(32))],
                &["e", root, "", "root"],
            ],
            KIND_PEER_CALL,
        );

        let mut state = BgState::new();
        let (tx, mut rx) = mpsc::channel(16);
        deliver_frame(&mut state, PEER_CALL_SUB_ID, &call, &tx).await;
        assert_eq!(drain(&mut rx).len(), 1, "the first delivery must act");

        // The watched REQ's copy of the identical event.
        deliver_frame(&mut state, PEER_CALL_SUB_ID, &call, &tx).await;
        assert!(
            drain(&mut rx).is_empty(),
            "the second delivery of one call must be an exact no-op"
        );
    }

    /// An envelope that names no single root spends nothing.
    ///
    /// Two `e` tags marked root belong to neither, and a dedup slot spent by
    /// one would suppress the genuine event that owns the id.
    #[tokio::test]
    async fn a_peer_call_with_no_unambiguous_root_is_dropped_without_spending_a_slot() {
        let keys = nostr::Keys::generate();
        let a = format!("30617:{}:buzz", "ef".repeat(32));
        let ambiguous = peer_call_frame(
            &keys,
            &[
                &["p", &"cd".repeat(32)],
                &["a", &a],
                &["e", &"11".repeat(32), "", "root"],
                &["e", &"22".repeat(32), "", "root"],
            ],
            KIND_PEER_CALL,
        );

        let mut state = BgState::new();
        let (tx, mut rx) = mpsc::channel(16);
        deliver_frame(&mut state, PEER_CALL_SUB_ID, &ambiguous, &tx).await;

        assert!(drain(&mut rx).is_empty());
        assert!(!state.project_seen_ids.contains(&ambiguous.id.to_hex()));
    }

    /// A kind this REQ never asked for is refused before it can spend a dedup
    /// slot — the same shape gate the membership subscription has, and for the
    /// same reason: a slot spent here suppresses the delivery that was entitled
    /// to it.
    #[tokio::test]
    async fn a_kind_the_peer_call_request_never_asked_for_is_refused() {
        let keys = nostr::Keys::generate();
        let channel_id = Uuid::new_v4();
        let intruder = peer_call_frame(&keys, &[&["h", &channel_id.to_string()]], 9);

        let mut state = BgState::new();
        let (tx, mut rx) = mpsc::channel(16);
        deliver_frame(&mut state, PEER_CALL_SUB_ID, &intruder, &tx).await;

        assert!(drain(&mut rx).is_empty());
        assert!(!state.seen_ids.contains(&intruder.id.to_hex()));
    }

    /// Replay across a reconnect delivers the call once.
    #[tokio::test]
    async fn a_peer_call_redelivered_after_a_reconnect_is_deduplicated() {
        let keys = nostr::Keys::generate();
        let channel_id = Uuid::new_v4();
        let event = peer_call_frame(
            &keys,
            &[&["p", &"cd".repeat(32)], &["h", &channel_id.to_string()]],
            KIND_PEER_CALL_RESULT,
        );

        let mut state = BgState::new();
        let (tx, mut rx) = mpsc::channel(16);
        deliver_frame(&mut state, PEER_CALL_SUB_ID, &event, &tx).await;
        deliver_frame(&mut state, PEER_CALL_SUB_ID, &event, &tx).await;

        assert_eq!(drain(&mut rx).len(), 1);
    }

    // ── Project sources validate before spending the dedup slot ──────────────

    fn announcement_with(keys: &nostr::Keys, ts: u64, tags: &[&[&str]]) -> Event {
        EventBuilder::new(
            nostr::Kind::Custom(KIND_GIT_REPO_ANNOUNCEMENT as u16),
            "announcement",
        )
        .custom_created_at(nostr::Timestamp::from(ts))
        .tags(tags.iter().map(|t| {
            nostr::Tag::parse(t.iter().map(|s| s.to_string()).collect::<Vec<_>>()).expect("tag")
        }))
        .sign_with_keys(keys)
        .expect("signing should succeed")
    }

    fn announcement(keys: &nostr::Keys, ts: u64) -> Event {
        announcement_with(keys, ts, &[&["d", "repo"]])
    }

    #[tokio::test]
    async fn a_malformed_announcement_is_refused_before_it_spends_anything() {
        // A kind check alone let these through the discovery boundary and into
        // `ProjectEvent::Discovery`, where the variant's name claimed more than
        // its payload could prove. They were rejected much later, inside the
        // state they were trying to enter, having already spent a dedup slot to
        // get there.
        //
        // Each case must produce no delivery, spend no project dedup slot, and
        // mutate no repository state. The last of those is structural rather
        // than asserted here: `DiscoveredRepositories::ingest` now takes a
        // `VerifiedAnnouncement`, so a frame that never proves one has no route
        // to the set at all.
        let keys = nostr::Keys::generate();
        for (label, tags) in [
            ("no `d`", vec![vec!["a", "30617:x:y"]]),
            ("empty `d`", vec![vec!["d", ""]]),
            ("conflicting `d`", vec![vec!["d", "one"], vec!["d", "two"]]),
            (
                "duplicate equal `d`",
                vec![vec!["d", "same"], vec!["d", "same"]],
            ),
        ] {
            let mut state = BgState::new();
            let (tx, mut rx) = mpsc::channel(16);
            let borrowed: Vec<&[&str]> = tags.iter().map(|t| t.as_slice()).collect();
            let event = announcement_with(&keys, 1_000, &borrowed);

            let discovery = open_discovery(&mut state).await;
            deliver_frame(&mut state, &discovery, &event, &tx).await;

            assert!(drain(&mut rx).is_empty(), "{label}: no discovery delivery");
            assert!(
                !state.project_seen_ids.contains(&event.id.to_hex()),
                "{label}: no project dedup slot spent"
            );
            assert_eq!(state.project_dropped_since, None, "{label}");
        }
    }

    #[tokio::test]
    async fn a_well_formed_announcement_still_reaches_discovery_with_its_coordinate() {
        // Positive control: the gate refuses malformed shapes, not everything.
        // Ownership comes from the verified signer, so an attacker-supplied `a`
        // naming someone else's repository changes nothing.
        let keys = nostr::Keys::generate();
        let signer = keys.public_key().to_hex();
        let mut state = BgState::new();
        let (tx, mut rx) = mpsc::channel(16);
        let event = announcement_with(
            &keys,
            1_000,
            &[
                &["d", "my-repo"],
                &["a", "30617:1111111111111111111111111111111111111111111111111111111111111111:not-mine"],
            ],
        );

        let discovery = open_discovery(&mut state).await;
        deliver_frame(&mut state, &discovery, &event, &tx).await;

        let delivered = drain(&mut rx);
        assert_eq!(delivered.len(), 1);
        match &delivered[0] {
            BuzzEvent::Project(crate::project::ProjectEvent::Discovery { announcement }) => {
                assert_eq!(
                    announcement.coordinate(),
                    format!("30617:{signer}:my-repo"),
                    "the coordinate comes from the signer, not the announcement's `a`"
                );
            }
            other => panic!("expected a discovery delivery: {other:?}"),
        }
        assert!(state.project_seen_ids.contains(&event.id.to_hex()));
    }

    #[tokio::test]
    async fn an_announcement_on_a_watched_id_does_not_burn_its_discovery_delivery() {
        // Suppression inside the project namespace. An announcement has no
        // root, so route derivation drops it — but the id had already been
        // spent, and the genuine discovery delivery then saw a duplicate.
        let mut state = BgState::new();
        let (tx, mut rx) = mpsc::channel(16);
        let keys = nostr::Keys::generate();
        let event = announcement(&keys, 1_000);

        let watched = open_watched(&mut state).await;
        deliver_frame(&mut state, &watched, &event, &tx).await;
        assert!(
            drain(&mut rx).is_empty(),
            "an announcement is not admissible on a watched subscription"
        );

        let discovery = open_discovery(&mut state).await;
        deliver_frame(&mut state, &discovery, &event, &tx).await;
        let delivered = drain(&mut rx);
        assert_eq!(delivered.len(), 1, "discovery still receives it");
        assert!(matches!(
            delivered[0],
            BuzzEvent::Project(crate::project::ProjectEvent::Discovery { .. })
        ));
    }

    #[tokio::test]
    async fn a_rooted_event_on_the_discovery_id_does_not_burn_its_routed_delivery() {
        // The mirror case. Discovery carries `30617` and nothing else; without
        // that gate a rooted event delivered under the discovery id was
        // accepted *as discovery state* and spent its id doing it.
        let mut state = BgState::new();
        let (tx, mut rx) = mpsc::channel(16);
        let keys = nostr::Keys::generate();
        let event = mixed_surface_event(&keys, Uuid::new_v4(), &test_root_id(), 1_000);

        let discovery = open_discovery(&mut state).await;
        deliver_frame(&mut state, &discovery, &event, &tx).await;
        assert!(
            drain(&mut rx).is_empty(),
            "a rooted event is not an announcement"
        );

        let watched = open_watched(&mut state).await;
        deliver_frame(&mut state, &watched, &event, &tx).await;
        let delivered = drain(&mut rx);
        assert_eq!(delivered.len(), 1, "the routed delivery survives");
        assert!(matches!(
            delivered[0],
            BuzzEvent::Project(crate::project::ProjectEvent::Routed { .. })
        ));
    }

    /// **The connected inbound slice, entered as relay bytes.**
    ///
    /// Nothing here constructs a `ProjectEvent`, a `ProjectEffect`, an author
    /// classification or a queue insertion. The test writes REQs through
    /// `send_project_subscribe` against a real socket, reads the bytes the
    /// relay actually received, feeds EVENT frames back on the exact ids those
    /// REQs registered, and lets `handle_ws_message` and `handle_project_event`
    /// do the rest. What it inspects is the resulting queue.
    ///
    /// The `watch_changed` near-miss is why this observes wire bytes rather
    /// than trusting what dispatch reports: a guard that never fired would have
    /// left every state assertion passing.
    #[tokio::test]
    async fn relay_bytes_from_an_authorised_root_mention_queue_exactly_one_project_turn() {
        let owner = nostr::Keys::generate();
        let agent = nostr::Keys::generate();
        let owner_hex = owner.public_key().to_hex();
        let agent_hex = agent.public_key().to_hex();
        let agent_identity =
            crate::project::AgentIdentity::new(&agent.public_key()).expect("identity");

        let mut state = BgState::new();
        let (tx, mut rx) = mpsc::channel(16);

        let mut discovered = crate::project::DiscoveredRepositories::new();
        let mut enrolments = crate::project::ProjectEnrolments::new();
        let mut queue = crate::queue::EventQueue::new(crate::config::DedupMode::Queue);
        let mut ledger = crate::peer_call::CallLedger::new();
        let humans = std::collections::BTreeSet::new();
        let externals = std::collections::BTreeSet::new();

        macro_rules! dispatch {
            ($ev:expr) => {{
                let mut d = crate::ProjectDispatch {
                    identity: crate::project::ProjectIdentity {
                        agent: &agent_identity,
                        agent_owner: Some(&owner_hex),
                        approved_humans: &humans,
                        approved_external_agents: &externals,
                    },
                    discovered: &mut discovered,
                    enrolments: &mut enrolments,
                    queue: &mut queue,
                    // These harnesses exercise routing and enrolment, not peer
                    // trust: no attestation means an agent author classifies as
                    // untrusted, which is the conservative reading and the one
                    // these cases were written against.
                    sibling: None,
                    ledger: &mut ledger,
                    resolved_candidate: None,
                    // Likewise no bus: what is under test is the routing
                    // decision, not what it announces on the issue.
                    observer: None,
                };
                crate::handle_project_event(&mut d, $ev)
            }};
        }

        // ── 1. Discovery REQ, written the way production writes it ──────────
        let (mut ws, mut server) = test_ws_pair().await;
        let discovery_id = crate::project::discovery_sub_id();
        assert_eq!(
            send_project_discovery(&mut ws, &mut state, discovery_filters()).await,
            ProjectSendOutcome::Sent
        );
        let frame = next_test_frame(&mut server).await;
        assert_eq!(frame[0], "REQ", "discovery must open with a REQ");
        assert_eq!(frame[1], discovery_id, "REQ carries the registered id");

        // ── 2. The announcement arrives on that exact registration ──────────
        let announcement = EventBuilder::new(
            nostr::Kind::Custom(buzz_core::kind::KIND_GIT_REPO_ANNOUNCEMENT as u16),
            "",
        )
        .tags([nostr::Tag::parse(["d", "connected-repo"]).expect("d tag")])
        .sign_with_keys(&owner)
        .expect("sign announcement");
        deliver_frame(&mut state, &discovery_id, &announcement, &tx).await;

        let delivered = drain(&mut rx);
        assert_eq!(delivered.len(), 1, "the announcement was admitted");
        let outcome = match &delivered[0] {
            BuzzEvent::Project(ev) => dispatch!(ev),
            other => panic!("expected a project event, got {other:?}"),
        };
        assert_eq!(
            outcome,
            crate::ProjectDispatched::DiscoveryChanged,
            "a new coordinate must widen the enrolment filter"
        );

        // ── 3. Enrolment REQ, derived from what discovery just admitted ─────
        let filter = crate::project::enrolment_filter(&discovered, &agent_hex, 0)
            .expect("a discovered repository yields an enrolment filter");
        let enrol_id = crate::project::PROJECT_ENROL_SUB_ID.to_string();
        assert!(
            matches!(
                state
                    .project_requests
                    .replace_enrolment(&mut ws, vec![filter])
                    .await,
                crate::project::ReplaceOutcome::Replaced { .. }
            ),
            "the enrolment replacement must install"
        );
        let frame = next_test_frame(&mut server).await;
        assert_eq!(frame[0], "REQ");
        assert_eq!(frame[1], enrol_id);
        assert_eq!(
            req_tag_set(&frame, &["#p"]),
            vec![agent_hex.clone()],
            "the enrolment REQ must be scoped to this agent and no other: \
             {frame:?}"
        );
        assert_eq!(
            req_coordinate_set(&frame),
            vec![format!("30617:{owner_hex}:connected-repo")],
            "the enrolment REQ must name exactly the one discovered \
             repository: {frame:?}"
        );

        // ── 4. The owner opens an issue naming the agent ────────────────────
        let coordinate = format!("30617:{owner_hex}:connected-repo");
        let root = EventBuilder::new(
            nostr::Kind::Custom(buzz_core::kind::KIND_GIT_ISSUE as u16),
            format!("@{agent_hex} please take a look"),
        )
        .tags([
            nostr::Tag::parse(["a", &coordinate]).expect("a tag"),
            nostr::Tag::parse(["p", &agent_hex]).expect("p tag"),
        ])
        .sign_with_keys(&owner)
        .expect("sign root");
        deliver_frame(&mut state, &enrol_id, &root, &tx).await;

        let delivered = drain(&mut rx);
        assert_eq!(
            delivered.len(),
            1,
            "the root was admitted on the enrolment id"
        );
        let outcome = match &delivered[0] {
            BuzzEvent::Project(ev) => dispatch!(ev),
            other => panic!("expected a project event, got {other:?}"),
        };

        // ── 5. Exactly one queued turn, under the root's UUIDv5 ─────────────
        let expected_key = {
            let verified = crate::project::VerifiedProjectEvent::verify(root.clone())
                .await
                .expect("the same event the relay admitted");
            crate::project::ProjectRoute::derive(&verified)
                .expect("routes")
                .key()
        };
        match outcome {
            crate::ProjectDispatched::Queued {
                key,
                queued,
                watch_changed,
            } => {
                assert!(queued, "the turn must actually enter the queue");
                assert_eq!(key, expected_key, "queued under the root's UUIDv5");
                assert!(
                    watch_changed,
                    "a newly enrolled root must replace the watched REQ"
                );
            }
            other => panic!("expected a queued turn, got {other:?}"),
        }
        assert_eq!(
            queue.queued_event_count(&expected_key),
            1,
            "exactly one turn queued under the root key, not zero and not two"
        );
        assert_eq!(
            queue.pending_channels(),
            1,
            "nothing was queued anywhere else"
        );

        // ── 6. The watched REQ replacement carries the root ─────────────────
        let watched = crate::project::watched_roots_filters(&enrolments, 0);
        assert!(
            !watched.is_empty(),
            "an enrolled root must produce watched-root filters"
        );
        assert!(
            matches!(
                state
                    .project_requests
                    .replace_watched(&mut ws, watched)
                    .await,
                crate::project::ReplaceOutcome::Replaced { .. }
            ),
            "the watched replacement must install"
        );
        let watched_id = installed_watched_id(&state);
        assert_eq!(
            watched_id,
            crate::project::watched_sub_id(0),
            "the registry stamps the first watched generation, and it is 0"
        );
        let frame = next_test_frame(&mut server).await;
        assert_eq!(
            frame[1], watched_id,
            "the REQ carries the generation the registry allocated"
        );
        assert_eq!(
            req_root_set(&frame),
            vec![root.id.to_hex()],
            "the watched REQ must name exactly the root just enrolled: \
             {frame:?}"
        );
    }

    /// Every refusal, entered as relay bytes on a real registration.
    ///
    /// The refusals were already asserted at dispatch level. What this adds is
    /// the production entry: each event is admitted by the relay's own frame
    /// handling on the exact id an enrolment REQ registered, so nothing here
    /// can pass because a test built a convenient classification.
    ///
    /// Each case asserts the queue is untouched — a refusal that still spent a
    /// queue slot would be a refusal in name only.
    #[tokio::test]
    async fn refused_project_events_reach_the_queue_as_nothing() {
        let owner = nostr::Keys::generate();
        let stranger = nostr::Keys::generate();
        let agent = nostr::Keys::generate();
        let owner_hex = owner.public_key().to_hex();
        let agent_hex = agent.public_key().to_hex();
        let agent_identity =
            crate::project::AgentIdentity::new(&agent.public_key()).expect("identity");

        let mut state = BgState::new();
        let (tx, mut rx) = mpsc::channel(32);
        let mut discovered = crate::project::DiscoveredRepositories::new();
        let mut enrolments = crate::project::ProjectEnrolments::new();
        let mut queue = crate::queue::EventQueue::new(crate::config::DedupMode::Queue);
        let mut ledger = crate::peer_call::CallLedger::new();
        let humans = std::collections::BTreeSet::new();
        let externals = std::collections::BTreeSet::new();

        macro_rules! dispatch {
            ($ev:expr) => {{
                let mut d = crate::ProjectDispatch {
                    identity: crate::project::ProjectIdentity {
                        agent: &agent_identity,
                        agent_owner: Some(&owner_hex),
                        approved_humans: &humans,
                        approved_external_agents: &externals,
                    },
                    discovered: &mut discovered,
                    enrolments: &mut enrolments,
                    queue: &mut queue,
                    // These harnesses exercise routing and enrolment, not peer
                    // trust: no attestation means an agent author classifies as
                    // untrusted, which is the conservative reading and the one
                    // these cases were written against.
                    sibling: None,
                    ledger: &mut ledger,
                    resolved_candidate: None,
                    // Likewise no bus: what is under test is the routing
                    // decision, not what it announces on the issue.
                    observer: None,
                };
                crate::handle_project_event(&mut d, $ev)
            }};
        }

        // Two repositories are discovered: the owner's, and a stranger's.
        // Announcing a repository is open to anyone — it grants no authority
        // over this agent, which is the first refusal below.
        let discovery_id = open_discovery(&mut state).await;
        for (keys, d) in [(&owner, "ours"), (&stranger, "theirs")] {
            let ann = EventBuilder::new(
                nostr::Kind::Custom(buzz_core::kind::KIND_GIT_REPO_ANNOUNCEMENT as u16),
                "",
            )
            .tags([nostr::Tag::parse(["d", d]).expect("d tag")])
            .sign_with_keys(keys)
            .expect("sign announcement");
            deliver_frame(&mut state, &discovery_id, &ann, &tx).await;
            for ev in drain(&mut rx) {
                if let BuzzEvent::Project(p) = ev {
                    dispatch!(&p);
                }
            }
        }

        let filter =
            crate::project::enrolment_filter(&discovered, &agent_hex, 0).expect("enrolment filter");
        let enrol_id = open_enrolment_with(&mut state, vec![filter]).await;
        // This scenario is about the authority gate, not the tail's prefix, so
        // it starts where live traffic starts.
        drain_enrolment_backlog(&mut state, &enrol_id).await;

        let root_named = |signer: &nostr::Keys, coord: &str, p: &str| {
            EventBuilder::new(
                nostr::Kind::Custom(buzz_core::kind::KIND_GIT_ISSUE as u16),
                format!("@{p} look at this"),
            )
            .tags([
                nostr::Tag::parse(["a", coord]).expect("a tag"),
                nostr::Tag::parse(["p", p]).expect("p tag"),
            ])
            .sign_with_keys(signer)
            .expect("sign")
        };

        let stranger_coord = format!("30617:{}:theirs", stranger.public_key().to_hex());
        let owner_coord = format!("30617:{owner_hex}:ours");

        let cases: Vec<(&str, Event)> = vec![
            // Announcing a repository is not authority over this agent. A
            // stranger who owns a repo and names the agent still cannot invoke
            // it — invocation authority comes from the agent's owner.
            (
                "an untrusted repository owner cannot invoke the agent",
                root_named(&stranger, &stranger_coord, &agent_hex),
            ),
            // The `a` tag names a coordinate the signer does not own. The
            // binding is validated against the announcement's signer, not the
            // claim.
            (
                "a forged coordinate binding cannot enrol",
                root_named(&stranger, &owner_coord, &agent_hex),
            ),
            // The agent's own root. Suppressed by author classification
            // regardless of how it is addressed.
            (
                "a self-authored root cannot wake",
                root_named(&agent, &owner_coord, &agent_hex),
            ),
        ];

        // (event id, whether it reached dispatch rather than being refused at
        // admission)
        let mut refused_ids: Vec<(String, bool)> = Vec::new();
        for (why, event) in cases {
            deliver_frame(&mut state, &enrol_id, &event, &tx).await;
            let delivered = drain(&mut rx);
            let reached_dispatch = !delivered.is_empty();
            // Some refusals happen at admission and never reach dispatch at
            // all; those that do must refuse there. Either way the queue is
            // the thing that must stay empty.
            for ev in delivered {
                if let BuzzEvent::Project(p) = ev {
                    let outcome = dispatch!(&p);
                    assert_eq!(outcome, crate::ProjectDispatched::Ignored, "{why}");
                }
            }
            assert_eq!(
                queue.pending_channels(),
                0,
                "{why}: a refusal spent a queue slot"
            );
            assert!(
                enrolments.all_roots().is_empty(),
                "{why}: a refusal enrolled a root"
            );
            refused_ids.push((event.id.to_hex(), reached_dispatch));
        }

        // **Where a refused event's dedup slot ends up, measured rather than
        // assumed.**
        //
        // Refusals happen at two different depths and the dedup consequence
        // differs, which is worth stating exactly because "a refusal consumes
        // no dedup slot" is true of only one of them:
        //
        // - refused at *admission* (the relay could not verify, route or bind
        //   it) — `project_seen_ids` was never reached, so no slot is spent;
        // - refused at *dispatch* (admitted and delivered, then declined by the
        //   authority gate) — the slot was taken on delivery, because dedup is
        //   about not delivering the same bytes twice and is decided before
        //   permission is.
        //
        // Neither leaks authority. The second means the identical event id
        // cannot be re-delivered, which is correct: it is the same event.
        for (id, reached_dispatch) in &refused_ids {
            assert_eq!(
                state.project_seen_ids.contains(id),
                *reached_dispatch,
                "a refused event's dedup slot must be spent exactly when it was \
                 delivered — never merely because it was refused"
            );
        }

        // What must still hold: a different, legitimate event arriving
        // afterwards on the same registration is unaffected.
        let legitimate = root_named(&owner, &owner_coord, &agent_hex);
        deliver_frame(&mut state, &enrol_id, &legitimate, &tx).await;
        let mut queued = false;
        for ev in drain(&mut rx) {
            if let BuzzEvent::Project(p) = ev {
                if let crate::ProjectDispatched::Queued { queued: q, .. } = dispatch!(&p) {
                    queued = q;
                }
            }
        }
        assert!(
            queued,
            "three refusals left the path unable to admit a legitimate mention"
        );
        assert_eq!(
            enrolments.all_roots().len(),
            1,
            "exactly the legitimate root is watched"
        );
    }

    /// Frames the relay can read right now, as JSON.
    ///
    /// Bounded, so a frame that never arrives fails as an assertion on an empty
    /// vector rather than hanging the scenario.
    async fn readable_frames<S>(server: &mut S) -> Vec<serde_json::Value>
    where
        S: futures_util::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>>
            + Unpin,
    {
        let mut out = Vec::new();
        while let Ok(Some(Ok(msg))) = timeout(Duration::from_millis(300), server.next()).await {
            if let Ok(text) = msg.to_text() {
                if let Ok(value) = serde_json::from_str(text) {
                    out.push(value);
                }
            }
        }
        out
    }

    /// Drives production replacement onto the scenario's own socket.
    ///
    /// Owns the `BgState` and the client half together, because the registry's
    /// write and the state it installs into are one operation. Interior
    /// mutability is required by the trait taking `&self`, which is right:
    /// dispatch holds a capability, not a mutable resource.
    struct SocketSubscriber {
        inner: tokio::sync::Mutex<(BgState, WsStream)>,
        /// What the last executed command returned for "keep the connection".
        kept: tokio::sync::Mutex<bool>,
    }

    impl SocketSubscriber {
        /// Whether the last command this subscriber ran left the connection up.
        ///
        /// `execute_connected_command` answers exactly that, and it is the
        /// production answer — a write failure returns `false` and is what
        /// takes the socket down.
        async fn last_connection_kept(&self) -> bool {
            *self.kept.lock().await
        }
    }

    impl crate::ProjectSubscriber for SocketSubscriber {
        /// Same seam: the proof is turned into the production command and run
        /// by the production handler, which is where the first page's REQ is
        /// written.
        async fn submit_root_catch_up(
            &self,
            root: crate::project::VerifiedBoundRoot,
        ) -> Result<(), RelayError> {
            let mut guard = self.inner.lock().await;
            let (state, ws) = &mut *guard;
            let kept = execute_connected_command(
                ws,
                state,
                "0".repeat(64).as_str(),
                RelayCommand::BeginRootCatchUp {
                    root: Box::new(root),
                },
            )
            .await;
            *self.kept.lock().await = kept;
            Ok(())
        }

        /// **Drives the production command path**, not the registry directly.
        ///
        /// The submission is turned into the same [`RelayCommand`] the run loop
        /// sends and handed to [`execute_connected_command`], so the seam under
        /// test is the one production composes: submit → command → background
        /// owner → registry → socket. Calling `replace_request` here instead
        /// would skip the two boundaries where the defect actually lived.
        async fn submit_project_replacement(
            &self,
            replacement: crate::project::ProjectReplacement,
            filters: Vec<serde_json::Value>,
        ) -> Result<(), RelayError> {
            let mut guard = self.inner.lock().await;
            let (state, ws) = &mut *guard;
            let kept = execute_connected_command(
                ws,
                state,
                "0".repeat(64).as_str(),
                RelayCommand::ReplaceProject {
                    replacement,
                    filters,
                },
            )
            .await;
            *self.kept.lock().await = kept;
            // Submission succeeded: the command was accepted and run. Whether
            // it installed anything is a separate question, answered by the
            // registry and readable through `last_connection_kept` and the
            // frames on the wire.
            Ok(())
        }

        /// Same seam, same reason: the walk is begun by the production command
        /// handler, which is where the first page's REQ is written.
        async fn submit_enrolment_history(
            &self,
            coordinates: Vec<String>,
            agent: String,
        ) -> Result<(), RelayError> {
            let mut guard = self.inner.lock().await;
            let (state, ws) = &mut *guard;
            let kept = execute_connected_command(
                ws,
                state,
                "0".repeat(64).as_str(),
                RelayCommand::BeginEnrolmentHistory { coordinates, agent },
            )
            .await;
            *self.kept.lock().await = kept;
            Ok(())
        }
    }

    /// The libtest filter that selects [`project_comment_cli_helper`].
    ///
    /// A typo here does not silently pass: libtest exits 0 when a filter
    /// matches nothing, but the scenario asserts a captured submission, which
    /// only a helper that actually ran can produce.
    const CLI_HELPER_TEST: &str = "relay::tests::project_comment_cli_helper";

    /// Helper mode — this test *is* the `buzz` CLI when the harness asks.
    ///
    /// The Phase A scenario has the agent's child process re-invoke this test
    /// executable with `BUZZ_ACP_TEST_CLI_ARGV` set, so the argv the agent read
    /// out of its prompt runs through [`buzz_cli::run_from_args`] — the real
    /// parser and the real dispatch. Calling `issues::dispatch` from the
    /// harness instead would prove only that a function the harness picked does
    /// what the harness expects, which is the prepared-midpoint mistake moved
    /// to the outbound side.
    ///
    /// Unset — every ordinary run — this returns immediately.
    #[tokio::test]
    async fn project_comment_cli_helper() {
        let Ok(argv) = std::env::var("BUZZ_ACP_TEST_CLI_ARGV") else {
            return;
        };
        let argv: Vec<String> =
            serde_json::from_str(&argv).expect("helper argv must be a JSON array of strings");
        let code = buzz_cli::run_from_args(argv).await;
        // The exit code is what the parent reads. Returning normally would
        // report libtest's verdict in place of the CLI's.
        std::process::exit(code);
    }

    /// **Phase A end to end: a mention on a root becomes a comment on it.**
    ///
    /// One scenario, not green halves. The batch the pool claims is the batch
    /// the relay-byte path produced, and the command the child runs is the one
    /// the prompt gave it — nothing here builds a `FlushBatch`, a
    /// `ProjectOrigin`, a classification, or an argv.
    ///
    /// ```text
    /// discovery REQ on a real socket → announcement EVENT on that id
    /// → enrolment REQ → p-tagged root EVENT on that id
    /// → authority gate → enrol → queue under the root's UUIDv5
    /// → queue.flush_next() → pool.try_claim(root key) → run_prompt_task
    /// → AcpClient drives initialize / session/new / session/prompt
    /// → the child reads the reply command out of that prompt
    /// → buzz_cli::run_from_args signs and POSTs the comment
    /// → the capture endpoint accepts it
    /// ```
    /// One submission the acceptance endpoint took: the exact bytes it
    /// received, and the event those bytes deserialised and verified as.
    struct Accepted {
        body: String,
        event: Event,
    }

    /// The local endpoint an agent's reply is submitted to.
    ///
    /// Scope, stated so it is not overread: this receives the signed event and
    /// answers acceptance. It is the transport boundary, not independent relay
    /// validation — nothing here checks the event against relay policy. What it
    /// buys is that the reply must be really built, really signed and really
    /// sent to be observed.
    struct AcceptanceEndpoint {
        url: String,
        accepted: std::sync::Arc<std::sync::Mutex<Vec<Accepted>>>,
        refused: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    }

    /// Serve `/events`, accepting only bodies that verify as signed events.
    ///
    /// **Acceptance is conditional on the signature, and conditional here.**
    /// The handler deserialises the exact body it was given as an
    /// [`nostr::Event`] and calls `verify()` — which checks that the id is the
    /// hash of the serialised event *and* that the signature is the claimed
    /// author's over that id — before the submission is recorded or acceptance
    /// is returned. Anything else is refused, kept out of `accepted`, and
    /// answered `accepted: false`.
    ///
    /// This used to be a `serde_json::Value` projection that recorded every
    /// body and always answered `accepted: true`, with the deserialisation and
    /// `verify()` done afterwards by the scenario's own assertions. That
    /// ordering made the check decorative: the child had already been told
    /// "accepted" and the collection already held the body, so deleting the
    /// later `verify()` left the scenario green. Verification now precedes both
    /// effects it is supposed to guard, and
    /// [`the_acceptance_endpoint_refuses_what_does_not_verify`] is what fails
    /// if it is dropped.
    ///
    /// The exact bytes are retained beside the parsed event, because the claim
    /// is about what the child sent — re-encoding a parsed value and asserting
    /// on that would check this harness's own round-trip.
    async fn spawn_acceptance_endpoint() -> AcceptanceEndpoint {
        let accepted: std::sync::Arc<std::sync::Mutex<Vec<Accepted>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let refused: std::sync::Arc<std::sync::Mutex<Vec<String>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let app = {
            let accepted = accepted.clone();
            let refused = refused.clone();
            axum::Router::new().route(
                "/events",
                axum::routing::post(move |body: String| {
                    let accepted = accepted.clone();
                    let refused = refused.clone();
                    async move {
                        let verified = serde_json::from_str::<Event>(&body)
                            .ok()
                            .filter(|event| event.verify().is_ok());
                        match verified {
                            Some(event) => {
                                let id = event.id.to_hex();
                                accepted
                                    .lock()
                                    .expect("acceptance sink")
                                    .push(Accepted { body, event });
                                axum::Json(serde_json::json!({
                                    "event_id": id,
                                    "accepted": true,
                                    "message": "",
                                }))
                            }
                            None => {
                                refused.lock().expect("refusal sink").push(body);
                                axum::Json(serde_json::json!({
                                    "event_id": "",
                                    "accepted": false,
                                    "message": "submission does not verify as a signed event",
                                }))
                            }
                        }
                    }
                }),
            )
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind the capture endpoint");
        let addr = listener.local_addr().expect("capture endpoint address");
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        AcceptanceEndpoint {
            url: format!("http://{addr}"),
            accepted,
            refused,
        }
    }

    /// **A pull-request turn's reply lands on the pull request.**
    ///
    /// The issue path already has its end-to-end scenario. This is the half that
    /// did not: the class flows from a real signed `kind:1618` root through the
    /// real candidate validation into the origin, the prompt is rendered by
    /// production code, the command is read *out of that prompt text* the way
    /// the stub agent reads it, and the argv runs through
    /// [`buzz_cli::run_from_args`] — the real clap parser and the real dispatch.
    ///
    /// ```text
    /// signed kind:1618 root → validate_enrolment_candidate → ProjectOrigin
    /// → queue::format_prompt → the command parsed out of the prompt
    /// → buzz_cli::run_from_args signs and POSTs
    /// → the acceptance endpoint verifies the signature
    /// → a / marked-root e / p / no h asserted on the PR root
    /// ```
    ///
    /// Nothing here builds an argv. If `format_project_context` emitted a
    /// command that does not parse — or one that parses and names the wrong
    /// root — the CLI exits non-zero or the captured event points elsewhere, and
    /// this fails.
    ///
    /// Destination and identity are passed as **explicit flags**, not left to
    /// the environment. `--relay` and `--private-key` override their
    /// `BUZZ_RELAY_URL` / `BUZZ_PRIVATE_KEY` fallbacks, so an operator shell
    /// that has those set cannot make this test publish anywhere real. That is
    /// not hypothetical: an earlier step of this project did exactly that.
    #[tokio::test]
    async fn a_pull_request_turn_replies_on_the_pull_request_through_the_real_cli() {
        let owner = nostr::Keys::generate();
        let asker = nostr::Keys::generate();
        let agent = nostr::Keys::generate();
        let coordinate = format!("30617:{}:pr-repo", owner.public_key().to_hex());

        // The class comes from a real root through the real validation — not
        // from a boolean a test chose. A `for_test` origin here would prove that
        // the prompt renders whatever flag it is handed, which is not the claim.
        let pr_root = EventBuilder::new(
            nostr::Kind::Custom(buzz_core::kind::KIND_GIT_PULL_REQUEST as u16),
            "please review the reconnect fix",
        )
        .tags([nostr::Tag::parse(["a", &coordinate]).expect("a tag")])
        .sign_with_keys(&owner)
        .expect("sign the pull request root");
        let verified = crate::project::VerifiedProjectEvent::verify(pr_root.clone())
            .await
            .expect("a freshly signed root verifies");
        let discovered = crate::project::DiscoveredRepositories::for_test([coordinate.clone()]);
        let candidate = crate::project::validate_enrolment_candidate(&verified, &discovered)
            .expect("a signed PR root naming a discovered coordinate is a candidate");
        assert!(
            candidate.is_pull_request(),
            "kind 1618 must classify as a pull request, or this proves nothing"
        );
        let origin = crate::project::ProjectOrigin::from_candidate(&candidate);

        // The turn: somebody comments on the PR and the agent is woken.
        let triggering = EventBuilder::new(nostr::Kind::TextNote, "any thoughts?")
            .tags([
                nostr::Tag::parse(["a", &coordinate]).expect("a tag"),
                nostr::Tag::parse(["e", &pr_root.id.to_hex(), "", "root"]).expect("e tag"),
                nostr::Tag::parse(["p", &agent.public_key().to_hex()]).expect("p tag"),
            ])
            .sign_with_keys(&asker)
            .expect("sign the comment");

        let batch = crate::queue::FlushBatch {
            channel_id: crate::project::project_route_key(&pr_root.id.to_hex())
                .expect("the root keys"),
            events: vec![crate::queue::BatchEvent {
                event: triggering,
                prompt_tag: "@mention".into(),
                received_at: std::time::Instant::now(),
                project: Some(origin),
            }],
            cancelled_events: vec![],
            cancel_reason: None,
        };
        let prompt = crate::queue::format_prompt(
            &batch,
            &crate::queue::FormatPromptArgs {
                project: batch.project_origin(),
                ..Default::default()
            },
        )
        .join("\n\n");

        // Read the command the way the stub agent does: the first line that
        // starts a `buzz ` invocation, following backslash continuations.
        let lines: Vec<&str> = prompt.lines().collect();
        let start = lines
            .iter()
            .position(|l| l.trim_start().starts_with("buzz "))
            .expect("the prompt must contain a runnable reply command");
        let mut command = String::new();
        for line in &lines[start..] {
            let trimmed = line.trim();
            if let Some(head) = trimmed.strip_suffix('\\') {
                command.push_str(head);
                command.push(' ');
            } else {
                command.push_str(trimmed);
                break;
            }
        }
        let argv: Vec<String> = command.split_whitespace().map(str::to_owned).collect();
        assert_eq!(
            argv.first().map(String::as_str),
            Some("buzz"),
            "the parsed command is not an invocation: {command:?}"
        );
        assert_eq!(
            &argv[1..3],
            ["pr", "comment"],
            "a pull-request turn was handed {:?}",
            &argv[1..3]
        );

        let endpoint = spawn_acceptance_endpoint().await;
        let mut invocation = vec![
            "buzz".to_string(),
            "--relay".to_string(),
            endpoint.url.clone(),
            "--private-key".to_string(),
            agent.secret_key().to_secret_hex(),
        ];
        invocation.extend(argv[1..].iter().cloned());
        invocation.push("--content".to_string());
        // The prompt's own `--content -` reads stdin; supply the body directly
        // so the in-process run needs no stdin of its own. Every other argument
        // is the prompt's.
        let body = "Reviewed — the reconnect path is right.";
        let content_flag = invocation
            .iter()
            .position(|a| a == "--content")
            .expect("the command must carry --content");
        invocation.truncate(content_flag);
        invocation.push("--content".to_string());
        invocation.push(body.to_string());

        let code = buzz_cli::run_from_args(invocation).await;
        assert_eq!(
            code, 0,
            "the command in the prompt did not run: {command:?}"
        );

        let accepted = endpoint.accepted.lock().expect("acceptance sink");
        assert_eq!(
            accepted.len(),
            1,
            "exactly one reply reached the acceptance endpoint"
        );
        let event = &accepted[0].event;
        assert_eq!(event.kind.as_u16(), 1, "a project comment is a kind:1");
        assert_eq!(event.content, body);
        assert_eq!(
            event.pubkey.to_hex(),
            agent.public_key().to_hex(),
            "the reply is signed by the agent whose key the invocation named"
        );

        let values = |key: &str| -> Vec<String> {
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
        assert_eq!(
            values("a"),
            vec![coordinate.clone()],
            "the reply left the repository it belongs to"
        );
        let marked_root = event.tags.iter().any(|t| {
            let s = t.as_slice();
            s.first().map(String::as_str) == Some("e")
                && s.get(1).map(String::as_str) == Some(pr_root.id.to_hex().as_str())
                && s.get(3).map(String::as_str) == Some("root")
        });
        assert!(
            marked_root,
            "the reply is not attached to the pull request that woke it"
        );
        assert!(
            values("p").contains(&asker.public_key().to_hex()),
            "the reply notifies nobody, so the person who asked never sees it"
        );
        assert!(
            values("h").is_empty(),
            "an `h` would scope the reply to a channel and take it out of the project"
        );
        assert!(
            endpoint.refused.lock().expect("refusal sink").is_empty(),
            "the endpoint refused a submission"
        );
    }

    /// The negative control for the acceptance endpoint.
    ///
    /// Three bodies that a `Value`-projecting endpoint would have taken: one
    /// that is not JSON at all, one that is a well-formed object carrying every
    /// field name an event has, and one that is a genuinely signed event whose
    /// content was edited afterwards — the last being the shape that matters,
    /// because every field an assertion reads is present and correct-looking
    /// and only the signature disagrees.
    ///
    /// None of them may be recorded or accepted. Delete the `verify()` from the
    /// handler and the tampered body sails through, which is what makes that
    /// call load-bearing rather than ornamental.
    #[tokio::test]
    async fn the_acceptance_endpoint_refuses_what_does_not_verify() {
        let endpoint = spawn_acceptance_endpoint().await;
        let author = nostr::Keys::generate();
        let genuine = EventBuilder::new(nostr::Kind::Custom(1), "as signed")
            .sign_with_keys(&author)
            .expect("sign the genuine event");

        let tampered = {
            let mut body: serde_json::Value =
                serde_json::to_value(&genuine).expect("encode the genuine event");
            body["content"] = json!("not what was signed");
            serde_json::to_string(&body).expect("encode the tampered body")
        };
        let impostor = serde_json::to_string(&json!({
            "id": "b".repeat(64),
            "pubkey": author.public_key().to_hex(),
            "created_at": 1,
            "kind": 1,
            "tags": [],
            "content": "never signed",
            "sig": "c".repeat(128),
        }))
        .expect("encode the impostor body");

        let post = |body: String| {
            let url = format!("{}/events", endpoint.url);
            async move {
                reqwest::Client::new()
                    .post(url)
                    .body(body)
                    .send()
                    .await
                    .expect("the endpoint answered")
                    .json::<serde_json::Value>()
                    .await
                    .expect("the answer is JSON")
            }
        };

        for (label, body) in [
            ("not JSON", "{{{ not an event".to_string()),
            ("an unsigned impostor", impostor),
            ("a tampered signed event", tampered),
        ] {
            let answer = post(body).await;
            assert_eq!(
                answer["accepted"],
                json!(false),
                "the endpoint accepted {label}: {answer}"
            );
        }
        assert!(
            endpoint
                .accepted
                .lock()
                .expect("acceptance sink")
                .is_empty(),
            "a submission that does not verify was recorded as accepted"
        );
        assert_eq!(
            endpoint.refused.lock().expect("refusal sink").len(),
            3,
            "the endpoint did not see all three submissions"
        );

        // And the genuine article, so the refusals above are not simply an
        // endpoint that refuses everything.
        let answer = post(serde_json::to_string(&genuine).expect("encode the genuine body")).await;
        assert_eq!(
            answer["accepted"],
            json!(true),
            "the endpoint refused a genuinely signed event: {answer}"
        );
        let accepted = endpoint.accepted.lock().expect("acceptance sink");
        assert_eq!(accepted.len(), 1, "exactly the genuine event is accepted");
        assert_eq!(
            accepted[0].event.id, genuine.id,
            "the accepted event is not the one submitted"
        );
    }

    #[tokio::test]
    async fn phase_a_end_to_end_relay_bytes_reach_the_agents_stdin() {
        // A protocol stub hanging forever would be a tedious way to end the
        // night, so the whole scenario is bounded.
        timeout(Duration::from_secs(60), async {
            let owner = nostr::Keys::generate();
            let other_owner = nostr::Keys::generate();
            let agent = nostr::Keys::generate();
            let owner_hex = owner.public_key().to_hex();
            let other_owner_hex = other_owner.public_key().to_hex();
            let agent_hex = agent.public_key().to_hex();
            let agent_identity =
                crate::project::AgentIdentity::new(&agent.public_key()).expect("identity");

            let (tx, mut rx) = mpsc::channel(16);
            let mut discovered = crate::project::DiscoveredRepositories::new();
            let mut enrolments = crate::project::ProjectEnrolments::new();
            let mut queue = crate::queue::EventQueue::new(crate::config::DedupMode::Queue);
            let mut ledger = crate::peer_call::CallLedger::new();
            let mut seen = crate::ProjectSeenIds::new();
            let humans = std::collections::BTreeSet::new();
            let externals = std::collections::BTreeSet::new();

            // One socket and one registry for the whole scenario. Every REQ and
            // CLOSE below is read off the server half, so what is asserted is
            // what the relay received rather than what a helper returned.
            //
            // The server half is split: the scenario keeps the reader, and the
            // writer goes to the relay peer together with both owner
            // identities. After this line the scenario cannot sign as an owner
            // and cannot write to the wire — which is what makes a prepared
            // midpoint unbuildable here. See [`RelayPeer`].
            let (client, server) = test_ws_pair().await;
            let (server_sink, mut server_rx) = server.split();
            let peer = spawn_relay_peer(server_sink, owner, other_owner);
            let subscriber = SocketSubscriber {
                inner: tokio::sync::Mutex::new((BgState::new(), client)),
                kept: tokio::sync::Mutex::new(true),
            };

            macro_rules! drive_all {
                () => {{
                    let mut last = crate::ProjectDispatched::Ignored;
                    for ev in drain(&mut rx) {
                        if let BuzzEvent::Project(p) = ev {
                            last = crate::dispatch_project_event(
                                &mut crate::ProjectDispatch {
                                    identity: crate::project::ProjectIdentity {
                                        agent: &agent_identity,
                                        agent_owner: Some(&owner_hex),
                                        approved_humans: &humans,
                                        approved_external_agents: &externals,
                                    },
                                    discovered: &mut discovered,
                                    enrolments: &mut enrolments,
                                    queue: &mut queue,
                                    sibling: None,
                                    ledger: &mut ledger,
                                    resolved_candidate: None,
                                    // These scenarios are about subscription
                                    // upkeep; nothing here reads the activity
                                    // wire, so the bus is absent as it is in a
                                    // runtime with neither feature on.
                                    observer: None,
                                },
                                &mut seen,
                                &subscriber,
                                &agent_hex,
                                0,
                                &p,
                            )
                            .await;
                            // Every replacement this scenario causes must have
                            // been executed *and* kept the connection. Without
                            // this the scenario would read identically whether
                            // the command installed anything or failed its
                            // write, because submission returns `Ok` either way.
                            assert!(
                                subscriber.last_connection_kept().await,
                                "a project replacement failed its write mid-scenario"
                            );
                        }
                    }
                    last
                }};
            }

            // Every EVENT below crosses the retained connection: the relay peer
            // signs and writes it on the subscription it was told to, and
            // `ingress` reads it back off the socket the requests were
            // registered on. No fresh socket, and no `Message::Text` this
            // scenario could hand to the handler even if it wanted to — the id
            // is all it ever holds of an inbound event.
            macro_rules! deliver {
                ($event_id:expr) => {{
                    let mut guard = subscriber.inner.lock().await;
                    let (state, ws) = &mut *guard;
                    deliver_over_connection(state, ws, $event_id, &tx, &agent).await;
                }};
            }

            /// Finish a request's stored-events prefix, the way a relay does.
            macro_rules! drain_backlog {
                ($sub_id:expr) => {{
                    peer.end_of_stored_events($sub_id).await;
                    let mut guard = subscriber.inner.lock().await;
                    let (state, ws) = &mut *guard;
                    drain_backlog_over_connection(state, ws, $sub_id, &tx, &agent).await;
                }};
            }

            // ── 1. discovery REQ, written the production way ─────────────────
            let discovery_id = crate::project::discovery_sub_id();
            {
                let mut guard = subscriber.inner.lock().await;
                let (state, ws) = &mut *guard;
                assert_eq!(
                    send_project_discovery(ws, state, discovery_filters()).await,
                    ProjectSendOutcome::Sent
                );
            }
            let seen = readable_frames(&mut server_rx).await;
            assert_eq!(
                seen.len(),
                1,
                "discovery writes exactly one frame: {seen:?}"
            );
            assert_eq!(seen[0][0], "REQ");
            assert_eq!(seen[0][1], discovery_id);

            // ── 2. the announcement drives an enrolment replacement ──────────
            let announcement = peer
                .publish(
                    &discovery_id,
                    InboundSpec {
                        signer: PeerSigner::Owner,
                        kind: buzz_core::kind::KIND_GIT_REPO_ANNOUNCEMENT as u16,
                        content: String::new(),
                        tags: vec![vec!["d".to_string(), "e2e-repo".to_string()]],
                    },
                )
                .await;
            deliver!(announcement);
            assert_eq!(drive_all!(), crate::ProjectDispatched::DiscoveryChanged);

            // Two REQs, because discovery raises two different questions.
            //
            // The tail under the fixed id — floored, unlimited, open-ended
            // forwards — and the first history page on its own
            // generation-distinct id, walking backwards from a snapshot bound.
            // A single REQ answering both is what made the restart case
            // unfixable: a fixed identity cannot paginate, so any reach-back it
            // carried could only sample.
            let seen = readable_frames(&mut server_rx).await;
            assert_eq!(
                seen.len(),
                2,
                "the enrolment tail and its first history page: {seen:?}"
            );
            let history = seen
                .iter()
                .find(|f| f[1].as_str().is_some_and(|id| id.contains("enrol-history")))
                .expect("a history page REQ");
            assert!(
                history[2]["until"].as_u64().is_some(),
                "a history page walks backwards from a bound: {history:?}"
            );
            assert!(
                history[2]["since"].is_null(),
                "a floor would make exhaustion a statement about the floor: {history:?}"
            );
            assert_eq!(
                history[2]["kinds"],
                json!([1621, 1618]),
                "roots only — comments reach an enrolled root through its watched \
                 REQ: {history:?}"
            );
            let seen: Vec<Value> = seen
                .into_iter()
                .filter(|f| f[1].as_str() == Some(crate::project::PROJECT_ENROL_SUB_ID))
                .collect();
            assert_eq!(seen.len(), 1, "one enrolment tail: {seen:?}");
            assert_eq!(seen[0][0], "REQ");
            assert_eq!(
                req_tag_set(&seen[0], &["#p"]),
                vec![agent_hex.clone()],
                "the enrolment REQ must be scoped to this agent and no other: \
                 {seen:?}"
            );
            let first_coordinate = format!("30617:{owner_hex}:e2e-repo");
            assert_eq!(
                req_coordinate_set(&seen[0]),
                vec![first_coordinate.clone()],
                "the enrolment REQ must name exactly the discovered \
                 repository: {seen:?}"
            );
            let first_enrolment = seen[0].to_string();

            // ── 2b. a second repository must WIDEN that enrolment ────────────
            //
            // Without this the scenario proves only that an enrolment REQ is
            // issued, which the shipped defect also did. Widening is the thing
            // that was broken: the id is fixed, so the second identity has to
            // replace the first rather than be refused as a conflict.
            let other_announcement = peer
                .publish(
                    &discovery_id,
                    InboundSpec {
                        signer: PeerSigner::SecondOwner,
                        kind: buzz_core::kind::KIND_GIT_REPO_ANNOUNCEMENT as u16,
                        content: String::new(),
                        tags: vec![vec!["d".to_string(), "second-repo".to_string()]],
                    },
                )
                .await;
            deliver!(other_announcement);
            assert_eq!(drive_all!(), crate::ProjectDispatched::DiscoveryChanged);

            // The tail widens in place; the walk restarts.
            //
            // Two different correct answers to the same discovery, because the
            // two requests mean different things. The tail's id is fixed, so
            // widening it is a replacement. The walk's proven-exhausted bound
            // was proven *for the narrower set*: carrying it onto a wider one
            // would assert exhaustion over a repository no page had mentioned,
            // so the walk starts again from the snapshot boundary under a fresh
            // identity — which is what `enrol-history-4` is.
            let seen = readable_frames(&mut server_rx).await;
            assert_eq!(
                seen.len(),
                2,
                "the widened tail and a restarted history walk: {seen:?}"
            );
            let restarted = seen
                .iter()
                .find(|f| f[1].as_str().is_some_and(|id| id.contains("enrol-history")))
                .expect("a restarted history page REQ");
            assert_eq!(
                req_coordinate_set(restarted).len(),
                2,
                "the restarted walk must cover both repositories: {restarted:?}"
            );
            let seen: Vec<Value> = seen
                .into_iter()
                .filter(|f| f[1].as_str() == Some(crate::project::PROJECT_ENROL_SUB_ID))
                .collect();
            assert_eq!(seen.len(), 1, "one widened tail: {seen:?}");
            assert_eq!(seen[0][0], "REQ");
            let widened = seen[0].to_string();
            assert_ne!(
                widened, first_enrolment,
                "the second discovery did not change the filter — this is the \
                 shipped defect: the enrolment can never widen past the first \
                 repository"
            );
            //
            // **Widened, not replaced.** `contains(second_owner)` passes for a
            // filter that gained the second repository *and* for one that
            // swapped the first out for it — and a swap is the same shipped
            // defect wearing the opposite sign: the agent stops hearing about
            // the repository it was already enrolled on. Only the complete
            // coordinate set tells the two apart.
            let mut expected_coordinates = vec![
                first_coordinate.clone(),
                format!("30617:{other_owner_hex}:second-repo"),
            ];
            expected_coordinates.sort();
            assert_eq!(
                req_coordinate_set(&seen[0]),
                expected_coordinates,
                "the widened enrolment must carry both discovered \
                 repositories, not just the newest: {seen:?}"
            );
            assert_eq!(
                req_tag_set(&seen[0], &["#p"]),
                vec![agent_hex.clone()],
                "widening must not widen the agent scope: {seen:?}"
            );

            // ── 3. the owner opens an issue naming the agent ─────────────────
            //
            // The tail's stored-events prefix is drained first, because that is
            // what a relay does and because everything after it is what "live"
            // means. Skipping it would leave the scenario asserting about a
            // request that had never finished answering — and the issue below
            // is published *after* this point precisely so that it is news
            // rather than backlog, which is the shape of the real miss this
            // fixture now covers.
            //
            // Two enrolment REQs have been written — the first discovery's and
            // the widening — so the relay owes two boundaries, and it sends one
            // per REQ. Draining only one would leave the tail still answering
            // from store, which is exactly the state the count exists to track.
            drain_backlog!(crate::project::PROJECT_ENROL_SUB_ID);
            drain_backlog!(crate::project::PROJECT_ENROL_SUB_ID);
            let coordinate = format!("30617:{owner_hex}:e2e-repo");
            // Named as well as `p`-tagged. The tag alone is what Desktop puts
            // on every root it creates; it is not an address, and a fixture
            // that relied on it would be asserting the relay path against an
            // event the gate is right to refuse.
            let body = format!("@{agent_hex} the pipeline drops frames after reconnect");
            let enrol_id = crate::project::PROJECT_ENROL_SUB_ID.to_string();
            let root = peer
                .publish(
                    &enrol_id,
                    InboundSpec {
                        signer: PeerSigner::Owner,
                        kind: buzz_core::kind::KIND_GIT_ISSUE as u16,
                        content: body.clone(),
                        tags: vec![
                            vec!["a".to_string(), coordinate.clone()],
                            vec!["p".to_string(), agent_hex.clone()],
                        ],
                    },
                )
                .await;
            deliver!(root);

            let route_key = match drive_all!() {
                crate::ProjectDispatched::Queued { key, queued, .. } => {
                    assert!(queued, "the root must enter the queue");
                    key
                }
                other => panic!("expected a queued turn, got {other:?}"),
            };

            let seen = readable_frames(&mut server_rx).await;
            assert_eq!(seen.len(), 1, "the first watched REQ, no CLOSE: {seen:?}");
            assert_eq!(seen[0][0], "REQ");
            assert_eq!(seen[0][1], crate::project::watched_sub_id(0));
            assert_eq!(
                req_root_set(&seen[0]),
                vec![root.to_hex()],
                "generation 0 must watch exactly the one enrolled root: \
                 {seen:?}"
            );

            // ── 4. a second root replaces the watch and retires generation 0 ─
            let second = peer
                .publish(
                    &enrol_id,
                    InboundSpec {
                        signer: PeerSigner::Owner,
                        kind: buzz_core::kind::KIND_GIT_ISSUE as u16,
                        content: format!("@{agent_hex} a second issue on the same repository"),
                        tags: vec![
                            vec!["a".to_string(), coordinate.clone()],
                            vec!["p".to_string(), agent_hex.clone()],
                        ],
                    },
                )
                .await;
            deliver!(second);
            drive_all!();

            let seen = readable_frames(&mut server_rx).await;
            assert_eq!(
                seen.len(),
                2,
                "successor REQ then predecessor CLOSE: {seen:?}"
            );
            assert_eq!(seen[0][0], "REQ");
            assert_eq!(
                seen[0][1],
                crate::project::watched_sub_id(1),
                "successor first"
            );
            //
            // **The complete set, not merely the new one.** A successor that
            // carried only the newly enrolled root would pass a `contains`
            // check while silently dropping the watch on everything enrolled
            // before it — the agent would stop hearing about the first root
            // and nothing on the wire would say so.
            let mut expected_roots = vec![root.to_hex(), second.to_hex()];
            expected_roots.sort();
            assert_eq!(
                req_root_set(&seen[0]),
                expected_roots,
                "the successor must watch every enrolled root, not just the \
                 newest: {seen:?}"
            );
            assert_eq!(seen[1][0], "CLOSE", "the predecessor is closed second");
            assert_eq!(seen[1][1], crate::project::watched_sub_id(0));

            // ── 5. reconnect, and read what the new connection re-asks ───────
            //
            // Inside this scenario, on the state these steps actually built.
            // The `clear_connection()`/`replayable()` unit test remains useful,
            // but it works on a registry a test assembled — it cannot show that
            // the intent left behind by a real replacement sequence is the
            // intent a real reconnect replays.
            let (replacement_client, replacement_server) = test_ws_pair().await;
            {
                let mut guard = subscriber.inner.lock().await;
                let (state, ws) = &mut *guard;
                assert!(
                    install_replacement_with(
                        state,
                        ws,
                        replacement_client,
                        ingress::HandshakeBuffer::empty()
                    )
                    .await,
                    "the replacement connection installs"
                );
                let (_reconnect_tx, mut reconnect_rx) = mpsc::channel(1);
                assert!(matches!(
                    resubscribe_after_reconnect(ws, &mut reconnect_rx, state, &agent_hex, true)
                        .await,
                    ResubscribeResult::Ok
                ));
            }
            // The dead socket's write half goes with it; everything after this
            // reads the replacement, and the peer writes to it.
            let (replacement_sink, replacement_rx) = replacement_server.split();
            peer.rebind(replacement_sink).await;
            server_rx = replacement_rx;
            let replayed = readable_frames(&mut server_rx).await;
            let replayed_ids: Vec<String> = replayed
                .iter()
                .filter(|f| f[0] == "REQ")
                .filter_map(|f| f[1].as_str().map(str::to_string))
                .collect();

            // Exactly the three current intents, and generation 0 is not among
            // them. A retired generation coming back here is the defect this
            // whole iteration exists for, arriving by a different door.
            //
            // Plus the history walk resuming — which is *not* a fourth intent.
            // The registry refuses to hold a history page as durable intent at
            // all, because its filter carries a bound that moves and a recorded
            // one would re-ask for a page the cursor has already walked past.
            // It comes back because the walk re-derives it from its own cursor.
            let history_ids: Vec<&String> = replayed_ids
                .iter()
                .filter(|id| id.contains("enrol-history"))
                .collect();
            assert_eq!(
                history_ids.len(),
                1,
                "the walk resumes under exactly one fresh identity: {replayed:?}"
            );
            let replayed_ids: Vec<String> = replayed_ids
                .into_iter()
                .filter(|id| !id.contains("enrol-history"))
                .collect();
            assert_eq!(
                replayed_ids.len(),
                3,
                "reconnect must re-ask discovery, enrolment and the current watch: {replayed:?}"
            );
            for expected in [
                discovery_id.as_str(),
                crate::project::PROJECT_ENROL_SUB_ID,
                &crate::project::watched_sub_id(1),
            ] {
                assert!(
                    replayed_ids.iter().any(|id| id == expected),
                    "reconnect did not re-ask {expected}: {replayed_ids:?}"
                );
            }
            assert!(
                !replayed_ids.contains(&crate::project::watched_sub_id(0)),
                "a retired watched generation was replayed: {replayed_ids:?}"
            );
            let replayed_watch = replayed
                .iter()
                .find(|f| f[1] == serde_json::json!(crate::project::watched_sub_id(1)))
                .expect("the current watched generation is replayed");
            assert_eq!(
                req_root_set(replayed_watch),
                expected_roots,
                "the replayed watch must carry every root the scenario \
                 enrolled: {replayed_watch:?}"
            );

            // ── a comment on the watched root, over the live connection ──────
            //
            // Discovery and root events both arrive above; the third inbound
            // class does not, and the watched generation's own admission path
            // was therefore never crossed by real bytes in this scenario. A
            // comment is what the watch exists to receive — the enrolment REQ
            // never matches one, so nothing else here proves a `#e` reference
            // to an enrolled root is admitted on the generation that asked
            // for it.
            //
            // Delivered on the replacement connection, after the batch below is
            // taken, so it queues a turn of its own rather than merging into
            // the one the prompt assertions read.
            let batch = queue.flush_next().expect("the queued turn flushes");

            // Addressed to the agent. The watched REQ matches on `#e` and
            // carries no `p` requirement, so a comment can arrive on it naming
            // nobody — and under the target-only rule that comment is context,
            // not a turn. What this scenario proves is that a `#e` reference to
            // an enrolled root is *admitted* on the generation that asked for
            // it, so the comment has to be one that would wake.
            let comment = peer
                .publish(
                    &crate::project::watched_sub_id(1),
                    InboundSpec {
                        signer: PeerSigner::Owner,
                        kind: buzz_core::kind::KIND_TEXT_NOTE as u16,
                        content: format!("@{agent_hex} any progress on this?"),
                        tags: vec![
                            vec!["a".to_string(), coordinate.clone()],
                            vec!["e".to_string(), root.to_hex()],
                            vec!["p".to_string(), agent_hex.clone()],
                        ],
                    },
                )
                .await;
            deliver!(comment);
            match drive_all!() {
                crate::ProjectDispatched::Queued { key, queued, .. } => {
                    assert!(queued, "the comment must enter the queue");
                    assert_eq!(
                        key, route_key,
                        "a comment routes to the root's session, not one of its own"
                    );
                }
                other => panic!("the watched generation did not admit the comment: {other:?}"),
            }
            assert_eq!(batch.channel_id, route_key, "flushed under the root key");
            //
            // `is_some()` would pass for an origin bound to the wrong
            // repository or the wrong root — a typed context that is present
            // and wrong is worse than one that is absent, because everything
            // downstream trusts it. The author is deliberately not checked
            // here: `ProjectOrigin` does not carry one, and the triggering
            // author's own path to the wire is asserted where it is
            // load-bearing, in the `--to` argument of the argv below.
            let origin = batch
                .project_origin()
                .expect("the flushed batch carries its project origin");
            assert_eq!(
                (origin.coordinate(), origin.root(), origin.is_pull_request()),
                (coordinate.as_str(), root.to_hex().as_str(), false),
                "the origin must name the repository and root that produced it"
            );

            // ── the endpoint the agent's reply is submitted to ───────────────
            let endpoint = spawn_acceptance_endpoint().await;
            let events_url = endpoint.url.clone();

            // ── a stub agent, entered through the production spawn path ──────
            //
            // Every method it receives is journalled in arrival order, so the
            // harness can assert the protocol sequence rather than assume it.
            //
            // On `session/prompt` it does what an agent does: reads the command
            // out of the prompt it was given and runs it. It does not receive
            // the argv from the harness — it parses the same text the model
            // would read, so a prompt that describes an unrunnable command
            // fails here rather than passing on a technicality.
            let capture =
                std::env::temp_dir().join(format!("buzz-acp-e2e-{}.json", Uuid::new_v4()));
            let _ = std::fs::remove_file(&capture);
            let reply_body = "Looked at it — the enrolment path is the culprit.";
            let stub = format!(
                r#"
import sys, json, os, shlex, subprocess

cap = open({path:?}, "w")

def reply_argv(text):
    lines = text.splitlines()
    start = next(i for i, l in enumerate(lines)
                 if l.strip().startswith("buzz issues comment"))
    parts = []
    for l in lines[start:]:
        s = l.strip()
        if s.endswith("\\"):
            parts.append(s[:-1])
            continue
        parts.append(s)
        break
    return shlex.split(" ".join(parts))

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    msg = json.loads(line)
    method = msg.get("method")
    if method:
        cap.write(json.dumps({{"method": method, "params": msg.get("params")}}) + "\n")
        cap.flush()
    if "id" not in msg:
        continue
    if method == "initialize":
        result = {{"protocolVersion": 2, "agentCapabilities": {{}},
                   "agentInfo": {{"name": "buzz-acp-test-stub", "version": "1"}}}}
    elif method == "session/new":
        result = {{"sessionId": "project-test-session"}}
    elif method == "session/prompt":
        text = "".join(b.get("text", "")
                       for b in (msg["params"].get("prompt") or []))
        argv = reply_argv(text)
        # What the environment handed this process, recorded before anything is
        # decided from it. The harness reads this back to confirm the
        # counterprobe was armed: a test proving hostile variables are ignored
        # proves nothing if no hostile variable ever arrived.
        cap.write(json.dumps({{"env_seen": {{
            k: os.environ.get(k)
            for k in ("BUZZ_RELAY_URL", "BUZZ_PRIVATE_KEY", "BUZZ_AUTH_TAG",
                      "BUZZ_ACP_TEST_RELAY_URL", "BUZZ_ACP_TEST_KEY")
        }}}}) + "\n")
        cap.flush()
        env = dict(os.environ)
        env["BUZZ_ACP_TEST_CLI_ARGV"] = json.dumps(argv)
        # **Destination and identity come from this generated source, never
        # from the environment.** They used to be read out of `os.environ`
        # under harness-specific names, which left them overrideable by
        # anything exporting those names — and `AcpClient::spawn` gives the
        # parent environment precedence, so an ambient value would silently
        # win. The literals below were written into this program by the same
        # harness that bound the capture endpoint, so there is no name left to
        # collide with.
        env["BUZZ_RELAY_URL"] = {relay_url:?}
        env["BUZZ_PRIVATE_KEY"] = {agent_key:?}
        # An ambient auth tag would present delegation this harness never
        # granted. Removed rather than overwritten: no test value belongs here.
        env.pop("BUZZ_AUTH_TAG", None)
        # What the CLI process is actually given, as opposed to what this one
        # was. Journalled so the harness can assert the removal rather than
        # trust the line above.
        cap.write(json.dumps({{"helper_env": {{
            k: env.get(k)
            for k in ("BUZZ_RELAY_URL", "BUZZ_PRIVATE_KEY", "BUZZ_AUTH_TAG")
        }}}}) + "\n")
        cap.flush()
        proc = subprocess.run(
            [{test_exe:?}, "--exact", {cli_helper:?}, "--nocapture"],
            input={reply:?}.encode(),
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=env)
        cap.write(json.dumps({{"cli": {{
            "argv": argv,
            "code": proc.returncode,
            "stdout": proc.stdout.decode("utf-8", "replace")[-4000:],
            "stderr": proc.stderr.decode("utf-8", "replace")[-4000:],
        }}}}) + "\n")
        cap.flush()
        result = {{"stopReason": "end_turn"}}
    else:
        result = {{}}
    sys.stdout.write(json.dumps({{"jsonrpc": "2.0", "id": msg["id"], "result": result}}) + "\n")
    sys.stdout.flush()
"#,
                path = capture.to_string_lossy(),
                reply = reply_body,
                relay_url = events_url,
                agent_key = agent.secret_key().to_secret_hex(),
                test_exe = std::env::current_exe()
                    .expect("this test executable's own path")
                    .to_string_lossy(),
                cli_helper = CLI_HELPER_TEST,
            );

            // **The counterprobe, not the configuration.**
            //
            // Nothing the child needs is passed here any more — the endpoint,
            // the key and the helper's own path are literals in the generated
            // stub above. What this vector carries is hostile: production names
            // and the former harness-specific names, all pointing somewhere
            // that is not the capture endpoint and naming a key that is not the
            // agent's.
            //
            // The scenario then requires the submission to arrive at the
            // capture endpoint signed by the agent anyway. Without this the
            // isolation claim would be untested — an environment that happened
            // to be clean passes identically to one that is genuinely ignored.
            //
            // This is not hypothetical. When these names were the real
            // configuration, `AcpClient::spawn`'s operator precedence
            // (`extra_env` is injected only for keys absent from the parent)
            // meant a developer or agent runtime with `BUZZ_RELAY_URL` and
            // `BUZZ_PRIVATE_KEY` set had this scenario ignore its own capture
            // endpoint, sign with the operator's real key and publish to the
            // operator's real relay. It happened on the first run of this step.
            let hostile_relay = "http://127.0.0.1:9/hostile-relay".to_string();
            let hostile_key = nostr::Keys::generate().secret_key().to_secret_hex();
            let child_env = vec![
                ("BUZZ_RELAY_URL".to_string(), hostile_relay.clone()),
                ("BUZZ_PRIVATE_KEY".to_string(), hostile_key.clone()),
                (
                    "BUZZ_AUTH_TAG".to_string(),
                    "hostile-delegation".to_string(),
                ),
                ("BUZZ_ACP_TEST_RELAY_URL".to_string(), hostile_relay.clone()),
                ("BUZZ_ACP_TEST_KEY".to_string(), hostile_key.clone()),
            ];

            // **The production spawn and initialise path, not a stamped state.**
            // `protocol_version` and `agent_name` come from the child's own
            // `initialize` response here. Constructing an `OwnedAgent` with them
            // hard-coded — as this harness previously did — skipped the
            // handshake entirely and left the stub's `initialize` branch dead.
            let (acp, protocol_version, agent_name) = crate::spawn_and_init(
                "python3",
                &["-c".to_string(), stub],
                &child_env,
                false,
                0,
                None,
            )
            .await
            .expect("spawn and initialise the stub agent");
            assert_eq!(
                protocol_version, 2,
                "protocol version must come from the child's initialize response"
            );
            assert_eq!(agent_name, "buzz-acp-test-stub");

            let mut pool =
                crate::pool::AgentPool::from_slots(vec![Some(crate::pool::OwnedAgent {
                    index: 0,
                    acp,
                    state: crate::pool::SessionState::default(),
                    model_capabilities: None,
                    desired_model: None,
                    model_overridden: false,
                    agent_name,
                    goose_system_prompt_supported: None,
                    protocol_version,
                })]);

            // ── the ordinary claim, by root key ──────────────────────────────
            let claimed = pool
                .try_claim(Some(route_key))
                .expect("the pool claims the root-key batch");
            let (result_tx, mut result_rx) = tokio::sync::mpsc::unbounded_channel();
            crate::pool::run_prompt_task(
                claimed,
                Some(batch),
                None,
                std::sync::Arc::new(crate::pool::PromptContext::for_test()),
                result_tx,
                None,
                "e2e-turn".to_string(),
            )
            .await;

            let result = result_rx.try_recv().expect("the turn produced a result");
            assert!(
                matches!(result.outcome, crate::pool::PromptOutcome::Ok(_)),
                "the turn must succeed"
            );
            assert!(
                matches!(result.source, crate::pool::PromptSource::Channel(k) if k == route_key),
                "the turn is attributed to the root's UUIDv5"
            );
            pool.return_agent(result.agent);
            let mut reclaimed = pool
                .try_claim(None)
                .expect("the agent slot is returned cleanly and claimable again");

            // Observed reaping. `Drop` only best-efforts a SIGKILL and a
            // non-blocking `try_wait`; `shutdown` kills the process group,
            // waits, and reports which of the three outcomes occurred. Leaving
            // the child to `Drop` would leave the test's own cleanup weaker
            // than production's *and* unobservable — the reason the return
            // type exists is that "it returned" was never the same claim as
            // "the child is gone".
            //
            // The enclosing 60s timeout is what makes this an assertion: a
            // child that never exits wedges here and fails the scenario rather
            // than leaking quietly past a green result.
            let reap = reclaimed.acp.shutdown().await;
            assert!(
                matches!(reap, crate::acp::ChildReap::Reaped(_)),
                "the child was not observably reaped: {reap:?}"
            );

            // ── what the child was actually told ─────────────────────────────
            let captured = std::fs::read_to_string(&capture).expect("the stub captured a journal");
            let _ = std::fs::remove_file(&capture);

            let journal: Vec<serde_json::Value> = captured
                .lines()
                .filter(|l| !l.trim().is_empty())
                .map(|l| serde_json::from_str(l).expect("valid journal line"))
                .collect();
            let methods: Vec<&str> = journal
                .iter()
                .filter_map(|e| e["method"].as_str())
                .collect();

            // **The protocol sequence, asserted rather than assumed.**
            // Each exactly once and in this order. Anything else — a second
            // `session/new`, a prompt before initialisation — is a different
            // conversation with the agent than the one production has.
            assert_eq!(
                methods,
                vec!["initialize", "session/new", "session/prompt"],
                "unexpected ACP sequence: {methods:?}"
            );

            let params = journal
                .iter()
                .find(|e| e["method"] == "session/prompt")
                .expect("a session/prompt was journalled")["params"]
                .clone();
            let text: String = params["prompt"]
                .as_array()
                .expect("prompt blocks")
                .iter()
                .filter_map(|b| b["text"].as_str())
                .collect();

            assert!(text.contains(&coordinate), "no repository coordinate");
            assert!(text.contains(&root.to_hex()), "no root event id");
            assert!(text.contains("issue"), "no issue/PR classification");
            assert!(text.contains("buzz issues comment"), "no reply command");
            assert!(
                text.contains(&body),
                "the triggering event's content is missing"
            );

            assert!(
                !text.contains(&format!("Channel: {route_key}")),
                "the route key is presented as a channel"
            );
            assert!(
                !text.contains("messages send"),
                "the prompt offers the channel reply command"
            );
            assert!(
                !text.contains("[Context]"),
                "the prompt carries synthetic channel metadata"
            );

            // ── the command the child actually ran ───────────────────────────
            let cli = journal
                .iter()
                .find_map(|e| e.get("cli"))
                .expect("the child journalled no CLI invocation");
            let argv: Vec<String> = cli["argv"]
                .as_array()
                .expect("argv array")
                .iter()
                .filter_map(|a| a.as_str().map(str::to_string))
                .collect();
            assert_eq!(
                cli["code"].as_i64(),
                Some(0),
                "the real CLI rejected the command the prompt gave it\n\
                 argv:   {argv:?}\n\
                 stdout: {}\n\
                 stderr: {}",
                cli["stdout"].as_str().unwrap_or_default(),
                cli["stderr"].as_str().unwrap_or_default()
            );

            // **The whole vector.** A prefix assertion passes against an argv
            // that names the right command and then addresses the wrong root,
            // notifies the wrong participant, or drops `--to` entirely — which
            // is the defect that made the non-author `p` assertion unreachable
            // in the first place.
            assert_eq!(
                argv,
                vec![
                    "buzz".to_string(),
                    "issues".to_string(),
                    "comment".to_string(),
                    "--repo-owner".to_string(),
                    owner_hex.clone(),
                    "--repo-id".to_string(),
                    "e2e-repo".to_string(),
                    "--root".to_string(),
                    root.to_hex(),
                    "--to".to_string(),
                    owner_hex.clone(),
                    "--content".to_string(),
                    "-".to_string(),
                ],
                "the child ran a different command than the prompt specifies"
            );

            // ── the counterprobe was armed ───────────────────────────────────
            //
            // Read before the submission is examined, because it is what makes
            // the submission assertion mean anything: the child must have been
            // holding a relay URL and a key that were not the ones it used.
            let env_seen = journal
                .iter()
                .find_map(|e| e.get("env_seen"))
                .expect("the child journalled no environment");
            let saw = |k: &str| env_seen[k].as_str().unwrap_or_default().to_string();
            assert!(
                !saw("BUZZ_RELAY_URL").is_empty() && saw("BUZZ_RELAY_URL") != events_url,
                "the isolation counterprobe was not armed — the child held no \
                 competing BUZZ_RELAY_URL, so this scenario would pass against \
                 a harness with no isolation at all: {env_seen:?}"
            );
            assert!(
                !saw("BUZZ_PRIVATE_KEY").is_empty()
                    && saw("BUZZ_PRIVATE_KEY") != agent.secret_key().to_secret_hex(),
                "the counterprobe held no competing key: {env_seen:?}"
            );
            assert_eq!(
                saw("BUZZ_ACP_TEST_RELAY_URL"),
                hostile_relay,
                "the former test-looking name must also be hostile: {env_seen:?}"
            );
            assert_eq!(
                saw("BUZZ_AUTH_TAG"),
                "hostile-delegation",
                "the counterprobe held no competing auth tag: {env_seen:?}"
            );

            // And what the CLI was actually handed: the capture endpoint, the
            // agent's own key, and no delegation at all.
            let helper_env = journal
                .iter()
                .find_map(|e| e.get("helper_env"))
                .expect("the child journalled no helper environment");
            assert_eq!(
                helper_env["BUZZ_RELAY_URL"].as_str(),
                Some(events_url.as_str()),
                "the CLI was pointed somewhere other than the capture endpoint: {helper_env:?}"
            );
            assert_eq!(
                helper_env["BUZZ_PRIVATE_KEY"].as_str(),
                Some(agent.secret_key().to_secret_hex().as_str()),
                "the CLI was given a key that is not the agent's: {helper_env:?}"
            );
            assert!(
                helper_env["BUZZ_AUTH_TAG"].is_null(),
                "an ambient auth tag reached the CLI as delegation this harness \
                 never granted: {helper_env:?}"
            );

            // ── the submission, as the endpoint received it ──────────────────
            //
            // Everything in here has already been deserialised from the exact
            // body and cryptographically verified — that is the condition of
            // being in this collection at all, and
            // `the_acceptance_endpoint_refuses_what_does_not_verify` is what
            // holds the endpoint to it. So `verify()` is not repeated below:
            // repeating it here is what made it deletable last time.
            let refused = endpoint.refused.lock().expect("refusal sink").clone();
            assert!(
                refused.is_empty(),
                "the endpoint refused a submission from this child: {refused:?}"
            );
            let submitted = endpoint.accepted.lock().expect("acceptance sink");
            assert_eq!(
                submitted.len(),
                1,
                "expected exactly one comment; a wake must not fan out into \
                 several submissions, and zero means the child never reached \
                 the relay — note that libtest exits 0 when the filter \
                 {CLI_HELPER_TEST} matches nothing, so the child's own output \
                 is the thing to read here\n\
                 argv:   {argv:?}\n\
                 stdout: {}\n\
                 stderr: {}",
                cli["stdout"].as_str().unwrap_or_default(),
                cli["stderr"].as_str().unwrap_or_default()
            );

            // ── what the endpoint verified, projected ────────────────────────
            //
            // `signed` is the event the endpoint deserialised out of the exact
            // body and verified before accepting it, so every projection below
            // is a projection of something the agent actually signed rather
            // than of something that merely said so.
            let signed = &submitted[0].event;

            assert_eq!(
                signed.kind.as_u16(),
                1,
                "a project comment is a kind:1 text note"
            );
            assert_eq!(
                signed.pubkey.to_hex(),
                agent_hex,
                "the comment is not signed by the woken agent"
            );
            assert_eq!(
                signed.content, reply_body,
                "the published body is not what the agent wrote"
            );

            // Tags are read off the exact accepted body, which is the same
            // bytes `signed` was verified from — the endpoint keeps both, and
            // the assertion below ties them together before any tag is read.
            let event: serde_json::Value =
                serde_json::from_str(&submitted[0].body).expect("the captured body is JSON");
            assert_eq!(
                event["id"],
                json!(signed.id.to_hex()),
                "the retained body is not the event that was verified"
            );

            let tags: Vec<Vec<String>> = event["tags"]
                .as_array()
                .expect("tags array")
                .iter()
                .map(|t| {
                    t.as_array()
                        .expect("tag array")
                        .iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .collect();
            let values_of = |name: &str| -> Vec<String> {
                tags.iter()
                    .filter(|t| t.first().map(String::as_str) == Some(name))
                    .filter_map(|t| t.get(1).cloned())
                    .collect()
            };

            // The addressing, exactly. These four are the whole claim: the
            // comment lands on *this* repository and *this* root, notifies the
            // person who asked rather than the agent itself, and carries no
            // channel scope — the route key is a UUIDv5 of a root and names no
            // channel, so an `h` tag here would be a fabricated one.
            assert_eq!(
                values_of("a"),
                vec![coordinate.clone()],
                "wrong repository coordinate"
            );
            assert_eq!(
                values_of("e"),
                vec![root.to_hex()],
                "the comment is not attached to the root that woke the agent"
            );
            assert_eq!(
                values_of("p"),
                vec![owner_hex.clone()],
                "the comment does not notify the human who asked"
            );
            assert_ne!(
                values_of("p"),
                vec![agent_hex.clone()],
                "the agent notified itself"
            );
            assert!(
                values_of("h").is_empty(),
                "the comment carries a channel scope: {tags:?}"
            );
        })
        .await
        .expect("the end-to-end scenario must not hang");
    }

    /// Opens through the production command path, so the control ends at bytes.
    struct SocketOpener {
        inner: tokio::sync::Mutex<(BgState, WsStream)>,
    }

    impl crate::ProjectOpener for SocketOpener {
        async fn submit_project_discovery(
            &self,
            filters: Vec<serde_json::Value>,
        ) -> Result<(), RelayError> {
            let mut guard = self.inner.lock().await;
            let (state, ws) = &mut *guard;
            execute_connected_command(
                ws,
                state,
                &"0".repeat(64),
                RelayCommand::SubscribeProjectDiscovery { filters },
            )
            .await;
            Ok(())
        }
    }

    /// **The startup gate, from configuration through to bytes.**
    ///
    /// Both halves run the whole chain production runs:
    ///
    /// ```text
    /// config.project_routing_enabled
    ///   → project::discovery_subscription
    ///   → open_startup_project_subscriptions
    ///   → RelayCommand::SubscribeProjectDiscovery
    ///   → the socket
    /// ```
    ///
    /// The previous control asserted on `project_req_frames`, which no
    /// production code calls, and then on `discovery_subscription(false)`
    /// directly — one boundary short in each case. Neither could fail if the
    /// startup path stopped consulting the flag, because neither ran it.
    ///
    /// The asymmetry is the point: a control that writes no bytes because
    /// nothing asked it to would pass against a flag that gates nothing. So the
    /// flag-on half must write exactly one REQ over the same opener.
    #[tokio::test]
    async fn the_startup_gate_runs_from_config_to_the_socket() {
        for (enabled, expected) in [(false, 0usize), (true, 1usize)] {
            let (client, mut server) = test_ws_pair().await;
            let opener = SocketOpener {
                inner: tokio::sync::Mutex::new((BgState::new(), client)),
            };

            let mut config = crate::config::test_config(crate::config::SubscribeMode::All);
            config.project_routing_enabled = enabled;

            crate::open_startup_project_subscriptions(&config, &opener).await;

            let seen = readable_frames(&mut server).await;
            assert_eq!(
                seen.len(),
                expected,
                "project_routing_enabled={enabled} must write {expected} frame(s): {seen:?}"
            );
            if enabled {
                assert_eq!(seen[0][0], "REQ");
                assert_eq!(
                    seen[0][1],
                    crate::project::discovery_sub_id(),
                    "the flag-on gate opened something other than discovery"
                );
                // The registration exists on the connection, so this is an
                // opened subscription rather than bytes that happened to leave.
                let guard = opener.inner.lock().await;
                assert!(
                    guard
                        .0
                        .project_requests
                        .match_frame(&crate::project::discovery_sub_id())
                        .is_some(),
                    "the discovery REQ was written but never registered"
                );
            }
        }
    }

    /// The registry refuses filters that constrain nothing, so opening
    /// discovery with the production filter says the startup REQ is a bounded
    /// request rather than a subscription to the whole relay.
    ///
    /// Asserted through the operation rather than through a constructor: the
    /// constructor is private to the registry now, and it was never the thing
    /// at risk — what matters is that the production filter survives the route
    /// production takes.
    #[tokio::test]
    async fn the_production_discovery_filter_is_a_bounded_request() {
        let filters = crate::project::discovery_subscription(true)
            .expect("the flag-on decision must be to open discovery");
        let mut state = BgState::new();
        assert_eq!(
            send_discovery(&mut state, filters).await,
            ProjectSendOutcome::Sent,
            "the production discovery filter must constrain events"
        );
    }

    /// The frame-level flag check.
    #[test]
    fn the_flag_off_control_writes_no_project_req_bytes() {
        let discovered = crate::project::DiscoveredRepositories::for_test(std::iter::once(
            format!("30617:{}:repo", "a".repeat(64)),
        ));
        let mut enrolments = crate::project::ProjectEnrolments::new();
        enrolments
            .enrol(&crate::project::EnrolmentCandidate::for_test(
                &"b".repeat(64),
                &format!("30617:{}:repo", "a".repeat(64)),
                &"a".repeat(64),
                &"c".repeat(64),
                false,
            ))
            .expect("enrol");

        // State that would produce every project REQ if the flag were on.
        let off =
            crate::project::project_req_frames(false, &discovered, &enrolments, &"c".repeat(64), 0);
        assert!(off.is_empty(), "flag off must write no project REQ bytes");

        let on =
            crate::project::project_req_frames(true, &discovered, &enrolments, &"c".repeat(64), 0);
        assert!(
            !on.is_empty(),
            "the same state with the flag on must produce REQs — otherwise the \
             control proves nothing"
        );
    }

    #[tokio::test]
    async fn a_catch_up_root_mismatch_does_not_burn_the_correct_rooted_delivery() {
        // The catch-up root check already existed, but it ran *after*
        // insertion, so a mismatched frame still spent the id and the correct
        // delivery for that event was then suppressed.
        //
        // A catch-up now spends no dedup slot at all, which subsumes that: the
        // shared set is for the live surfaces, and running page rows through it
        // would suppress exactly the events already delivered live — leaving the
        // page short by that number and reading it as end-of-history.
        let mut state = BgState::new();
        let (tx, mut rx) = mpsc::channel(16);
        let (bound_a, _) = proven_issue_root().await;
        let root_a = bound_a.binding().root().to_string();
        let keys = nostr::Keys::generate();
        let root_b = test_root_id();
        let event = mixed_surface_event(&keys, Uuid::new_v4(), &root_b, 1_000);

        let catchup_a = bind_page_under(&mut state, &bound_a).await;
        deliver_frame(&mut state, &catchup_a, &event, &tx).await;
        assert!(
            drain(&mut rx).is_empty(),
            "the mismatched frame delivers nothing to the consumer"
        );
        assert!(
            page_verdict(&mut state, &root_a, &catchup_a, &tx)
                .await
                .is_err(),
            "root B is not admissible on root A's catch-up"
        );
        assert!(
            !state.project_seen_ids.contains(&event.id.to_hex()),
            "no catch-up frame spends a live-surface dedup slot"
        );

        // The correct rooted delivery is still there to be made: the mismatch
        // burned nothing on the way past. (The boundary above delivers a
        // `StoredEventsComplete`, which is not what is being counted here.)
        drain(&mut rx);
        let watched = open_watched(&mut state).await;
        deliver_frame(&mut state, &watched, &event, &tx).await;
        let delivered = drain(&mut rx);
        assert_eq!(delivered.len(), 1, "the routed delivery survives");
        assert!(matches!(
            delivered[0],
            BuzzEvent::Project(crate::project::ProjectEvent::Routed { .. })
        ));
    }

    // ── Piece 3: frames reach the page their own request opened ──────────────

    /// A proven issue root, and the keys that signed it.
    ///
    /// Through the real proof: a coordinate in the discovered set, a signed root
    /// naming it, `VerifiedBoundRoot::prove`. There is no test constructor for a
    /// bound root and there should not be — it is exactly the thing a
    /// reconstruction may not start without.
    async fn proven_issue_root() -> (crate::project::VerifiedBoundRoot, nostr::Keys) {
        let keys = nostr::Keys::generate();
        let coordinate = format!("30617:{}:repo", keys.public_key().to_hex());
        let root = EventBuilder::new(
            nostr::Kind::Custom(buzz_core::kind::KIND_GIT_ISSUE as u16),
            "an issue",
        )
        .tags([nostr::Tag::parse(vec!["a".to_string(), coordinate.clone()]).expect("a tag")])
        .sign_with_keys(&keys)
        .expect("sign");
        let verified = crate::project::VerifiedProjectEvent::verify(root)
            .await
            .expect("a freshly signed root verifies");
        let known = crate::project::DiscoveredRepositories::for_test([coordinate]);
        let bound =
            crate::project::VerifiedBoundRoot::prove(std::slice::from_ref(&verified), &known)
                .expect("a signed root naming a discovered coordinate proves");
        (bound, keys)
    }

    /// **The restart proof, composed end to end.**
    ///
    /// The reported failure: an owner closed a watched root, the agent recorded
    /// it, the process restarted, reconstruction reported `6 discovered / 6
    /// restored / 0 dropped`, and the root came back **active** — because the
    /// enrolment walk fetches root kinds and nothing ever fetched the close.
    /// The next comment ran a turn on a closed issue. The classifier could read
    /// a replayed lifecycle event correctly and no production path supplied one.
    ///
    /// So this crosses both halves of that gap in one run, on real signed
    /// events:
    ///
    /// 1. the relay task rebuilds the root's history through the production
    ///    command path, on a real socket, and its REQ is read back off the wire;
    /// 2. what it hands to the event channel is fed to the production run-loop
    ///    dispatch, on one enrolment set, in the order it arrived.
    ///
    /// **The close is signed an hour before startup** — four times the tail's
    /// 900-second reach-back. Nothing but a real history walk can find it, so a
    /// pass here cannot be the live tail happening to be close enough.
    #[tokio::test]
    async fn a_close_older_than_the_tails_reach_survives_a_restart() {
        const STARTUP: u64 = 1_785_743_469;
        const ROOT_AT: u64 = STARTUP - 7_200;
        const CLOSE_AT: u64 = STARTUP - 3_600;

        let owner = nostr::Keys::generate();
        let agent = nostr::Keys::generate();
        let agent_hex = agent.public_key().to_hex();
        let coordinate = format!("30617:{}:demo", owner.public_key().to_hex());
        let discovered = crate::project::DiscoveredRepositories::for_test([coordinate.clone()]);

        let root_event = addressed_root(&owner, &agent, &coordinate, ROOT_AT);
        let root_id = root_event.id.to_hex();
        let verified_root = crate::project::VerifiedProjectEvent::verify(root_event.clone())
            .await
            .expect("a freshly signed root verifies");
        let bound = crate::project::VerifiedBoundRoot::prove(
            std::slice::from_ref(&verified_root),
            &discovered,
        )
        .expect("the root names a discovered coordinate");

        // The owner's close, long before anything live could reach.
        let close = root_status(
            &owner,
            &coordinate,
            &root_id,
            buzz_core::kind::KIND_GIT_STATUS_CLOSED,
            CLOSE_AT,
        );

        // ── the relay task rebuilds this root's history ──────────────────────
        let mut state = BgState::new();
        state.startup_watermark = Some(STARTUP);
        let (mut ws, mut server) = test_ws_pair().await;
        let (tx, mut rx) = mpsc::channel(64);

        assert!(
            execute_connected_command(
                &mut ws,
                &mut state,
                &agent_hex,
                RelayCommand::BeginRootCatchUp {
                    root: Box::new(bound)
                },
            )
            .await,
            "a catch-up command must not take the socket down"
        );

        let req = readable_frames(&mut server)
            .await
            .into_iter()
            .find(|f| f[0] == "REQ")
            .expect("the catch-up's REQ reached the socket");
        let page_id = req[1].as_str().expect("an id").to_string();
        let filter = &req[2];
        assert!(
            filter.get("since").is_none(),
            "a history walk with a floor is a walk that cannot reach an old close: {req:?}"
        );
        assert!(
            filter["kinds"]
                .as_array()
                .expect("kinds")
                .contains(&json!(buzz_core::kind::KIND_GIT_STATUS_CLOSED)),
            "and it must actually ask for lifecycle: {req:?}"
        );

        // The relay answers with the close, then its boundary. One row against
        // a page that asked for many: unsaturated, so the stream is exhausted
        // and the reconstruction completes.
        deliver_frame(&mut state, &page_id, &close, &tx).await;
        assert!(
            deliver_control_frame_to(&mut state, json!(["EOSE", page_id]), &tx).await,
            "dispatch must not signal connection loss"
        );

        let replayed: Vec<_> = drain(&mut rx)
            .into_iter()
            .filter_map(|e| match e {
                BuzzEvent::Project(crate::project::ProjectEvent::Routed {
                    source,
                    event,
                    mode,
                    ..
                }) => Some((source, event, mode)),
                _ => None,
            })
            .collect();
        assert_eq!(
            replayed.len(),
            1,
            "the rebuilt history is handed on: {replayed:?}"
        );
        assert_eq!(
            replayed[0].1.id(),
            close.id.to_hex(),
            "and it is the close the walk fetched"
        );
        assert_eq!(
            replayed[0].2,
            crate::project::ProcessingMode::Replay,
            "as history, which never runs a turn"
        );

        // ── the run loop folds it into the watch ─────────────────────────────
        //
        // Restart order, on one enrolment set: the enrolment walk restores the
        // root first — that is what establishes the binding the close is
        // authorised against — and the catch-up's rows follow it.
        let mut run_loop = RunLoopState::new(&agent, &owner, discovered);
        assert!(
            matches!(
                run_loop
                    .deliver(
                        crate::project::ProjectSubscription::EnrolmentHistory { generation: 0 },
                        crate::project::ProcessingMode::Replay,
                        verified_root,
                    )
                    .await,
                crate::ProjectDispatched::Enrolled
            ),
            "precondition: the restored root is watched again, with no turn"
        );
        assert_eq!(
            run_loop.state_of(&root_id),
            crate::project::RootState::Active,
            "precondition: and a root restored from its own event comes back active — \
             which is exactly why its lifecycle has to be replayed after it"
        );

        for (source, event, mode) in replayed {
            run_loop.deliver(source, mode, event).await;
        }
        assert_eq!(
            run_loop.state_of(&root_id),
            crate::project::RootState::Dormant,
            "the close the walk fetched must suspend the watch — this is the \
             restart the reported failure survived"
        );

        // ── and the watch behaves ────────────────────────────────────────────
        let while_closed = run_loop
            .deliver_live(participant_comment(
                &owner,
                &agent,
                &coordinate,
                &root_id,
                STARTUP + 10,
                "ITER3-AFTER-RESTART-CLOSED-MUST-NOT-WAKE",
            ))
            .await;
        assert!(
            !matches!(
                while_closed,
                crate::ProjectDispatched::Queued { queued: true, .. }
            ),
            "a comment on a root closed before the restart must not run a turn — \
             got {while_closed:?}"
        );

        let reopened = run_loop
            .deliver_live(root_status(
                &owner,
                &coordinate,
                &root_id,
                buzz_core::kind::KIND_GIT_STATUS_OPEN,
                STARTUP + 20,
            ))
            .await;
        assert_eq!(
            reopened,
            crate::ProjectDispatched::LifecycleApplied {
                root_state: crate::project::RootState::Active
            },
            "an authorised reopen restores the watch"
        );

        // Addressed: a reopened watch admits the comment, and the target-only
        // rule decides whether it becomes a turn. Both have to hold for the
        // reopen to mean anything, so the comment names the agent.
        let after_reopen = run_loop
            .deliver_live(participant_comment(
                &owner,
                &agent,
                &coordinate,
                &root_id,
                STARTUP + 30,
                &format!("@{} and now?", agent.public_key().to_hex()),
            ))
            .await;
        assert!(
            matches!(
                after_reopen,
                crate::ProjectDispatched::Queued { queued: true, .. }
            ),
            "and the next addressed comment invokes exactly once — got {after_reopen:?}"
        );
    }

    /// Replayed lifecycle applies in the order it happened, not the order it
    /// arrived.
    ///
    /// A history walk pages **backwards**, so the relay hands a close and the
    /// reopen that followed it back newest-first. Folding them in arrival order
    /// would leave the watch in the state the conversation left two events ago
    /// — closed, silently, on a root its owner reopened. The merge sorts
    /// ascending by `created_at` and the delivery queue preserves that order;
    /// this is the test that would notice if either stopped.
    ///
    /// Both events predate the tail's reach, so neither could have arrived any
    /// other way.
    #[tokio::test]
    async fn a_replayed_close_and_reopen_leave_the_watch_where_history_left_it() {
        const STARTUP: u64 = 1_785_743_469;
        const ROOT_AT: u64 = STARTUP - 9_000;
        const CLOSE_AT: u64 = STARTUP - 7_200;
        const REOPEN_AT: u64 = STARTUP - 3_600;

        let owner = nostr::Keys::generate();
        let agent = nostr::Keys::generate();
        let agent_hex = agent.public_key().to_hex();
        let coordinate = format!("30617:{}:demo", owner.public_key().to_hex());
        let discovered = crate::project::DiscoveredRepositories::for_test([coordinate.clone()]);

        let root_event = addressed_root(&owner, &agent, &coordinate, ROOT_AT);
        let root_id = root_event.id.to_hex();
        let verified_root = crate::project::VerifiedProjectEvent::verify(root_event)
            .await
            .expect("valid");
        let bound = crate::project::VerifiedBoundRoot::prove(
            std::slice::from_ref(&verified_root),
            &discovered,
        )
        .expect("proves");

        let close = root_status(
            &owner,
            &coordinate,
            &root_id,
            buzz_core::kind::KIND_GIT_STATUS_CLOSED,
            CLOSE_AT,
        );
        let reopen = root_status(
            &owner,
            &coordinate,
            &root_id,
            buzz_core::kind::KIND_GIT_STATUS_OPEN,
            REOPEN_AT,
        );

        let mut state = BgState::new();
        state.startup_watermark = Some(STARTUP);
        let (mut ws, mut server) = test_ws_pair().await;
        let (tx, mut rx) = mpsc::channel(64);
        assert!(
            execute_connected_command(
                &mut ws,
                &mut state,
                &agent_hex,
                RelayCommand::BeginRootCatchUp {
                    root: Box::new(bound)
                },
            )
            .await
        );
        let page_id = readable_frames(&mut server)
            .await
            .into_iter()
            .find(|f| f[0] == "REQ")
            .expect("a REQ")[1]
            .as_str()
            .expect("an id")
            .to_string();

        // Newest first, the way a backwards walk returns them.
        deliver_frame(&mut state, &page_id, &reopen, &tx).await;
        deliver_frame(&mut state, &page_id, &close, &tx).await;
        assert!(deliver_control_frame_to(&mut state, json!(["EOSE", page_id]), &tx).await);

        let replayed: Vec<_> = drain(&mut rx)
            .into_iter()
            .filter_map(|e| match e {
                BuzzEvent::Project(crate::project::ProjectEvent::Routed {
                    source,
                    event,
                    mode,
                    ..
                }) => Some((source, event, mode)),
                _ => None,
            })
            .collect();
        assert_eq!(
            replayed
                .iter()
                .map(|(_, e, _)| e.event().created_at.as_secs())
                .collect::<Vec<_>>(),
            vec![CLOSE_AT, REOPEN_AT],
            "the walk read them newest-first; the replay must hand them on oldest-first"
        );

        let mut run_loop = RunLoopState::new(&agent, &owner, discovered);
        run_loop
            .deliver(
                crate::project::ProjectSubscription::EnrolmentHistory { generation: 0 },
                crate::project::ProcessingMode::Replay,
                verified_root,
            )
            .await;
        for (source, event, mode) in replayed {
            run_loop.deliver(source, mode, event).await;
        }

        assert_eq!(
            run_loop.state_of(&root_id),
            crate::project::RootState::Active,
            "the owner reopened this root after closing it; a watch left dormant is \
             an agent silent on a conversation that is open"
        );
    }

    /// A root whose history carries peer-call traffic rebuilds — and rebuilding
    /// it invokes nothing.
    ///
    /// The reported failure: every real root that had ever carried a NIP-PC
    /// event degraded on catch-up with `Comments: kind 43004 does not belong to
    /// Comments`. [`crate::project::HistoryStream::kinds`] asked the relay for
    /// `43001` and `43004` and `admits` refused them, so the relay returned
    /// exactly what had been requested and the reconstruction objected to
    /// receiving it. The root then lost the claim to know its own lifecycle on
    /// the strength of rows the agent had asked for itself.
    ///
    /// Three facts in one run, on one root, over a single page whose rows all
    /// predate startup:
    ///
    /// 1. the catch-up **completes without degrading**, with every row it was
    ///    sent accounted for;
    /// 2. nothing it replays **invokes a turn or correlates a result**;
    /// 3. the **lifecycle and comment ordering** the same page carried still
    ///    lands, so admitting the call did not cost the close.
    ///
    /// The second is a contrast rather than an absence. The identical call
    /// event, delivered live to the *same* run loop afterwards, queues exactly
    /// one turn — which also proves the replay did not quietly spend the call
    /// id in the ledger, because a call id already recorded as admitted is
    /// refused as a replay and would queue nothing.
    #[tokio::test]
    async fn historical_peer_call_traffic_rebuilds_without_degrading_or_invoking() {
        const STARTUP: u64 = 1_785_743_469;
        const ROOT_AT: u64 = STARTUP - 9_000;
        const COMMENT_AT: u64 = STARTUP - 8_000;
        const CALL_AT: u64 = STARTUP - 7_000;
        const RESULT_AT: u64 = STARTUP - 6_000;
        const CLOSE_AT: u64 = STARTUP - 5_000;

        let owner = nostr::Keys::generate();
        let agent = nostr::Keys::generate();
        // A second agent the owner has approved. Its calls are ones this agent
        // answers, which is what makes "replay ran none of them" a claim.
        let peer = nostr::Keys::generate();
        let agent_hex = agent.public_key().to_hex();
        let peer_hex = peer.public_key().to_hex().to_ascii_lowercase();
        let coordinate = format!("30617:{}:demo", owner.public_key().to_hex());
        let discovered = crate::project::DiscoveredRepositories::for_test([coordinate.clone()]);

        let root_event = addressed_root(&owner, &agent, &coordinate, ROOT_AT);
        let root_id = root_event.id.to_hex();
        let verified_root = crate::project::VerifiedProjectEvent::verify(root_event)
            .await
            .expect("valid");
        let bound = crate::project::VerifiedBoundRoot::prove(
            std::slice::from_ref(&verified_root),
            &discovered,
        )
        .expect("proves");

        let route = buzz_core::peer_call::PeerCallRoute::Project {
            coordinate: coordinate.clone(),
            root: root_id.clone(),
        };
        let comment = participant_comment(
            &owner,
            &agent,
            &coordinate,
            &root_id,
            COMMENT_AT,
            "before any of this",
        );
        // Inbound: the peer asks this agent to do something, on this root.
        let call = project_call(&peer, &agent_hex, &route, CALL_AT, "take a look");
        // Outbound answered: a result for a call *this* agent made, so its `p`
        // names the agent and `call` derives from the agent as caller.
        let result = project_call_result(
            &peer,
            &agent_hex,
            &buzz_core::peer_call::derive_call_id(&agent_hex, &peer_hex, &route, CALL_NONCE),
            &route,
            RESULT_AT,
            "done",
        );
        let close = root_status(
            &owner,
            &coordinate,
            &root_id,
            buzz_core::kind::KIND_GIT_STATUS_CLOSED,
            CLOSE_AT,
        );

        // ── the relay task rebuilds this root's history ──────────────────────
        let mut state = BgState::new();
        state.startup_watermark = Some(STARTUP);
        let (mut ws, mut server) = test_ws_pair().await;
        let (tx, mut rx) = mpsc::channel(64);
        assert!(
            execute_connected_command(
                &mut ws,
                &mut state,
                &agent_hex,
                RelayCommand::BeginRootCatchUp {
                    root: Box::new(bound)
                },
            )
            .await
        );
        let req = readable_frames(&mut server)
            .await
            .into_iter()
            .find(|f| f[0] == "REQ")
            .expect("the catch-up's REQ reached the socket");
        let page_id = req[1].as_str().expect("an id").to_string();
        let kinds = req[2]["kinds"].as_array().expect("kinds");
        assert!(
            kinds.contains(&json!(KIND_PEER_CALL)) && kinds.contains(&json!(KIND_PEER_CALL_RESULT)),
            "the walk asks for peer-call traffic, which is the whole reason it \
             must also admit it: {req:?}"
        );

        // Newest first, the way a backwards walk returns them.
        for row in [&close, &result, &call, &comment] {
            deliver_frame(&mut state, &page_id, row, &tx).await;
        }
        assert!(deliver_control_frame_to(&mut state, json!(["EOSE", page_id]), &tx).await);

        assert!(
            !state.root_catch_up_degraded.contains_key(&root_id),
            "a root must not lose the claim to its own history over rows it \
             asked for: {:?}",
            state.root_catch_up_degraded.get(&root_id)
        );
        assert_eq!(
            state.root_catch_up_done.get(&root_id),
            Some(&4),
            "and every row the page carried is in the rebuilt history"
        );

        let replayed: Vec<_> = drain(&mut rx)
            .into_iter()
            .filter_map(|e| match e {
                BuzzEvent::Project(crate::project::ProjectEvent::Routed {
                    source,
                    event,
                    mode,
                    ..
                }) => Some((source, event, mode)),
                _ => None,
            })
            .collect();
        assert_eq!(
            replayed
                .iter()
                .map(|(_, e, _)| e.event().created_at.as_secs())
                .collect::<Vec<_>>(),
            vec![COMMENT_AT, CALL_AT, RESULT_AT, CLOSE_AT],
            "the walk read them newest-first; the replay hands them on oldest-first"
        );
        assert!(
            replayed.iter().all(|(source, _, mode)| matches!(
                source,
                crate::project::ProjectSubscription::RootCatchUp { .. }
            ) && *mode
                == crate::project::ProcessingMode::Replay),
            "each under the class of the page that fetched it, as history"
        );

        // ── the run loop folds it in ─────────────────────────────────────────
        let mut run_loop = RunLoopState::new(&agent, &owner, discovered).approving(&peer_hex);
        assert!(
            matches!(
                run_loop
                    .deliver(
                        crate::project::ProjectSubscription::EnrolmentHistory { generation: 0 },
                        crate::project::ProcessingMode::Replay,
                        verified_root,
                    )
                    .await,
                crate::ProjectDispatched::Enrolled
            ),
            "precondition: the restored root is watched again"
        );

        for (source, event, mode) in replayed {
            let kind = event.kind();
            let dispatched = run_loop.deliver(source, mode, event).await;
            assert!(
                !matches!(
                    dispatched,
                    crate::ProjectDispatched::Queued { queued: true, .. }
                ),
                "history restores state and answers nobody — kind {kind} queued a \
                 turn: {dispatched:?}"
            );
        }
        assert_eq!(
            run_loop.state_of(&root_id),
            crate::project::RootState::Dormant,
            "and the close still lands: admitting the call must not cost the lifecycle"
        );

        // ── the contrast ─────────────────────────────────────────────────────
        //
        // The owner reopens, and then the *same* call event arrives live. It
        // must run exactly one turn: if it does not, "replay invoked nothing"
        // was a fact about this call rather than about replay — and if replay
        // had spent the call id, the ledger would refuse this as a duplicate.
        assert_eq!(
            run_loop
                .deliver_live(root_status(
                    &owner,
                    &coordinate,
                    &root_id,
                    buzz_core::kind::KIND_GIT_STATUS_OPEN,
                    STARTUP + 10,
                ))
                .await,
            crate::ProjectDispatched::LifecycleApplied {
                root_state: crate::project::RootState::Active
            },
        );
        assert!(
            matches!(
                run_loop.deliver_live(call).await,
                crate::ProjectDispatched::Queued { queued: true, .. }
            ),
            "the identical call invokes when it is live, so the silence above \
             belongs to replay and the ledger was left unspent"
        );
    }

    /// The run loop's project state, kept across a sequence of events.
    ///
    /// Holds what `tokio_main` holds and hands every event to
    /// [`crate::handle_project_event`], the production entry. It classifies
    /// nothing: "the root is dormant" read from here is a fact the production
    /// dispatch produced.
    struct RunLoopState {
        agent: crate::project::AgentIdentity,
        owner_hex: String,
        humans: std::collections::BTreeSet<String>,
        externals: std::collections::BTreeSet<String>,
        discovered: crate::project::DiscoveredRepositories,
        enrolments: crate::project::ProjectEnrolments,
        queue: crate::queue::EventQueue,
        ledger: crate::peer_call::CallLedger,
    }

    impl RunLoopState {
        fn new(
            agent: &nostr::Keys,
            owner: &nostr::Keys,
            discovered: crate::project::DiscoveredRepositories,
        ) -> Self {
            Self {
                agent: crate::project::AgentIdentity::new(&agent.public_key()).unwrap(),
                owner_hex: owner.public_key().to_hex(),
                humans: std::collections::BTreeSet::new(),
                externals: std::collections::BTreeSet::new(),
                discovered,
                enrolments: crate::project::ProjectEnrolments::new(),
                queue: crate::queue::EventQueue::new(crate::config::DedupMode::Queue),
                ledger: crate::peer_call::CallLedger::new(),
            }
        }

        /// Approve one external agent, the way the owner's config does.
        ///
        /// A peer call from an unapproved key classifies `Untrusted` and would
        /// never run a turn anyway, so a test that wants "replay invoked
        /// nothing" to mean something has to make the caller one whose calls
        /// this agent *does* answer.
        fn approving(mut self, agent_hex: &str) -> Self {
            self.externals.insert(agent_hex.to_ascii_lowercase());
            self
        }

        async fn deliver(
            &mut self,
            source: crate::project::ProjectSubscription,
            mode: crate::project::ProcessingMode,
            verified: crate::project::VerifiedProjectEvent,
        ) -> crate::ProjectDispatched {
            let route = crate::project::ProjectRoute::derive(&verified).expect("routes");
            crate::handle_project_event(
                &mut crate::ProjectDispatch {
                    identity: crate::project::ProjectIdentity {
                        agent: &self.agent,
                        agent_owner: Some(&self.owner_hex),
                        approved_humans: &self.humans,
                        approved_external_agents: &self.externals,
                    },
                    discovered: &mut self.discovered,
                    enrolments: &mut self.enrolments,
                    queue: &mut self.queue,
                    sibling: None,
                    ledger: &mut self.ledger,
                    resolved_candidate: None,
                    observer: None,
                },
                &crate::project::ProjectEvent::Routed {
                    source,
                    route,
                    event: verified,
                    mode,
                },
            )
        }

        /// One live event on the watched-root REQ this agent's enrolment
        /// installed.
        async fn deliver_live(&mut self, event: Event) -> crate::ProjectDispatched {
            let verified = crate::project::VerifiedProjectEvent::verify(event)
                .await
                .expect("valid");
            self.deliver(
                crate::project::ProjectSubscription::Watched { generation: 0 },
                crate::project::ProcessingMode::Live,
                verified,
            )
            .await
        }

        fn state_of(&self, root: &str) -> crate::project::RootState {
            self.enrolments.state_of(root)
        }
    }

    /// A `1621` root on `coordinate`, addressed to `agent`, signed by `owner`.
    ///
    /// Addressed means named *and* `p`-tagged: the tag alone is structure the
    /// client writes for itself and does not wake anybody.
    fn addressed_root(
        owner: &nostr::Keys,
        agent: &nostr::Keys,
        coordinate: &str,
        ts: u64,
    ) -> Event {
        EventBuilder::new(
            nostr::Kind::Custom(buzz_core::kind::KIND_GIT_ISSUE as u16),
            format!("@{} please look", agent.public_key().to_hex()),
        )
        .custom_created_at(nostr::Timestamp::from(ts))
        .tags([
            nostr::Tag::parse(["a", coordinate]).unwrap(),
            nostr::Tag::parse(["p", &agent.public_key().to_hex()]).unwrap(),
        ])
        .sign_with_keys(owner)
        .expect("sign")
    }

    /// The nonce every peer-call fixture here derives its call id from.
    const CALL_NONCE: &str = "0123456789abcdef0123456789abcdef";

    /// A NIP-PC invocation on a project root, in the shape the wire specifies.
    ///
    /// The call id is derived rather than invented, because
    /// `CallEnvelope::parse` recomputes it — a hand-written id would be refused
    /// as a mismatch and the event would classify as "not a call to us", which
    /// is precisely the reading that would make an invocation test pass by
    /// never having an invocation in it.
    fn project_call(
        caller: &nostr::Keys,
        callee_hex: &str,
        route: &buzz_core::peer_call::PeerCallRoute,
        ts: u64,
        task: &str,
    ) -> Event {
        let caller_hex = caller.public_key().to_hex().to_ascii_lowercase();
        let call_id = buzz_core::peer_call::derive_call_id(
            &caller_hex,
            &callee_hex.to_ascii_lowercase(),
            route,
            CALL_NONCE,
        );
        EventBuilder::new(nostr::Kind::Custom(KIND_PEER_CALL as u16), task)
            .custom_created_at(nostr::Timestamp::from(ts))
            .tags(
                [
                    nostr::Tag::parse(["p", callee_hex]).unwrap(),
                    nostr::Tag::parse(["call", &call_id]).unwrap(),
                    nostr::Tag::parse(["nonce", CALL_NONCE]).unwrap(),
                    nostr::Tag::parse(["hop", "1"]).unwrap(),
                    nostr::Tag::parse(["visited", &caller_hex]).unwrap(),
                ]
                .into_iter()
                .chain(peer_route_tags(route)),
            )
            .sign_with_keys(caller)
            .expect("sign")
    }

    /// A NIP-PC result on a project root, answering `call_id`.
    fn project_call_result(
        callee: &nostr::Keys,
        caller_hex: &str,
        call_id: &str,
        route: &buzz_core::peer_call::PeerCallRoute,
        ts: u64,
        body: &str,
    ) -> Event {
        EventBuilder::new(nostr::Kind::Custom(KIND_PEER_CALL_RESULT as u16), body)
            .custom_created_at(nostr::Timestamp::from(ts))
            .tags(
                [
                    nostr::Tag::parse(["p", caller_hex]).unwrap(),
                    nostr::Tag::parse(["call", call_id]).unwrap(),
                ]
                .into_iter()
                .chain(peer_route_tags(route)),
            )
            .sign_with_keys(callee)
            .expect("sign")
    }

    /// The route tags a project-routed NIP-PC event carries.
    fn peer_route_tags(route: &buzz_core::peer_call::PeerCallRoute) -> Vec<nostr::Tag> {
        match route {
            buzz_core::peer_call::PeerCallRoute::Project { coordinate, root } => vec![
                nostr::Tag::parse(["a", coordinate]).unwrap(),
                nostr::Tag::parse(["e", root, "", "root"]).unwrap(),
            ],
            buzz_core::peer_call::PeerCallRoute::Channel { .. } => {
                unreachable!("these fixtures are project-routed")
            }
        }
    }

    /// A signed status event on `root`.
    fn root_status(actor: &nostr::Keys, coordinate: &str, root: &str, kind: u32, ts: u64) -> Event {
        EventBuilder::new(nostr::Kind::Custom(kind as u16), "")
            .custom_created_at(nostr::Timestamp::from(ts))
            .tags([
                nostr::Tag::parse(["a", coordinate]).unwrap(),
                nostr::Tag::parse(["e", root, "", "root"]).unwrap(),
            ])
            .sign_with_keys(actor)
            .expect("sign")
    }

    /// A follow-up comment carrying the agent's inherited `p` tag.
    ///
    /// Not a fresh explicit mention: Desktop copies prior participants into
    /// every later comment, so this is the shape an ordinary reply has, and the
    /// shape a closed root must not answer.
    fn participant_comment(
        author: &nostr::Keys,
        agent: &nostr::Keys,
        coordinate: &str,
        root: &str,
        ts: u64,
        body: &str,
    ) -> Event {
        EventBuilder::new(nostr::Kind::TextNote, body)
            .custom_created_at(nostr::Timestamp::from(ts))
            .tags([
                nostr::Tag::parse(["a", coordinate]).unwrap(),
                nostr::Tag::parse(["e", root, "", "root"]).unwrap(),
                nostr::Tag::parse(["p", &agent.public_key().to_hex()]).unwrap(),
            ])
            .sign_with_keys(author)
            .expect("sign")
    }

    /// A comment on `root`, signed for real.
    fn comment_on_root(keys: &nostr::Keys, root: &str, ts: u64, body: &str) -> Event {
        EventBuilder::new(nostr::Kind::TextNote, body)
            .custom_created_at(nostr::Timestamp::from(ts))
            .tags([nostr::Tag::parse(vec![
                "e".to_string(),
                root.to_string(),
                String::new(),
                "root".to_string(),
            ])
            .expect("e tag")])
            .sign_with_keys(keys)
            .expect("sign")
    }

    /// An event with arbitrary tags, signed for real.
    fn tagged_event(keys: &nostr::Keys, kind: u16, ts: u64, tags: &[&[&str]]) -> Event {
        EventBuilder::new(nostr::Kind::Custom(kind), "body")
            .custom_created_at(nostr::Timestamp::from(ts))
            .tags(tags.iter().map(|t| {
                nostr::Tag::parse(t.iter().map(|s| s.to_string()).collect::<Vec<_>>())
                    .expect("tag parses")
            }))
            .sign_with_keys(keys)
            .expect("sign")
    }

    /// Deliver one frame and report whether it reached the consumer — and,
    /// separately, whether it spent the shared project dedup slot.
    ///
    /// Both, because a refusal that still spends the slot is not a refusal: the
    /// event's legitimate delivery on another surface would then see a
    /// duplicate and deliver nothing.
    async fn delivery_and_dedup(
        state: &mut BgState,
        sub_id: &str,
        event: &Event,
        tx: &mpsc::Sender<Option<BuzzEvent>>,
        rx: &mut mpsc::Receiver<Option<BuzzEvent>>,
    ) -> (usize, bool) {
        deliver_frame(state, sub_id, event, tx).await;
        (
            drain(rx).len(),
            state.project_seen_ids.contains(&event.id.to_hex()),
        )
    }

    /// A watched request admits only the roots, kinds and window it asked for.
    ///
    /// The relay chooses what to send under a subscription id. Until this
    /// check, "we opened *something* under this id" was the whole admission
    /// test for the live surfaces: a correctly signed event for a root this
    /// agent never watched arrived as a routed event and spent the project
    /// dedup slot on the way, which would then suppress that same event's
    /// legitimate delivery on the surface entitled to it.
    #[tokio::test]
    async fn a_watched_request_admits_only_what_it_asked_for() {
        let mut state = BgState::new();
        let (tx, mut rx) = mpsc::channel(16);
        let keys = nostr::Keys::generate();
        let watched_root = test_root_id();
        let other_root = "b".repeat(64);

        // The real watched request for one issue root, from the production
        // builder, with the window it would be sent with.
        let id = open_watched_since(&mut state, &[(&watched_root, false)], 1_000).await;

        let comment_kind = crate::project::HistoryStream::Comments.kinds()[0] as u16;
        let pr_update_kind = crate::project::HistoryStream::PullRequestUpdates.kinds()[0] as u16;

        let refused: [(&str, Event); 4] = [
            (
                "a root this agent never watched",
                tagged_event(
                    &keys,
                    comment_kind,
                    1_000,
                    &[&["e", &other_root, "", "root"]],
                ),
            ),
            (
                "a kind this request did not ask for",
                tagged_event(
                    &keys,
                    pr_update_kind,
                    1_000,
                    &[&["e", &watched_root, "", "root"]],
                ),
            ),
            (
                // `#e` and `#E` are different questions: a comment points at
                // its root with lowercase `e`, a pull-request revision with
                // uppercase `E`. A matcher that case-folded the tag name would
                // admit each on the other's filter.
                "the right root under the wrong reference style",
                tagged_event(&keys, comment_kind, 1_000, &[&["E", &watched_root]]),
            ),
            (
                "older than the window this request asked for",
                tagged_event(
                    &keys,
                    comment_kind,
                    999,
                    &[&["e", &watched_root, "", "root"]],
                ),
            ),
        ];

        for (why, event) in &refused {
            assert_eq!(
                delivery_and_dedup(&mut state, &id, event, &tx, &mut rx).await,
                (0, false),
                "{why}: must deliver nothing and spend nothing"
            );
        }

        // The positive control, so none of the above is passing because the
        // whole surface is broken.
        let admissible = tagged_event(
            &keys,
            comment_kind,
            1_000,
            &[&["e", &watched_root, "", "root"]],
        );
        assert_eq!(
            delivery_and_dedup(&mut state, &id, &admissible, &tx, &mut rx).await,
            (1, true),
            "what the request actually asked for is delivered"
        );
    }

    /// The watched REQ production builds — two filters — goes on the wire whole
    /// and admits either branch.
    ///
    /// A NIP-01 REQ carries one *or more* filters, ORed, and this is the request
    /// that uses that: comments and status events point at their root with
    /// lowercase `e`, a pull-request revision with **uppercase `E`**, and a
    /// single lowercase filter silently drops every PR revision. The registry
    /// held one `Value`, so it could not represent this request at all — the
    /// pair would have serialised as `["REQ", id, [a, b]]` and would have made
    /// `admits` refuse everything, since the stored value was no longer an
    /// object. The only thing hiding that was a fixture rebuilding one filter by
    /// hand.
    ///
    /// So this test starts from `watched_roots_filters`, not from JSON: an
    /// approximation cannot demonstrate an equivalence with the thing it
    /// approximates.
    #[tokio::test]
    async fn the_watched_req_carries_both_reference_styles_and_admits_either() {
        let issue_root = "a".repeat(64);
        let pr_root = "b".repeat(64);
        let roots = [(issue_root.as_str(), false), (pr_root.as_str(), true)];

        // ---- The bytes, through a concrete paired socket. ------------------
        //
        // Compared against `project_req_frames`, which is what the driver will
        // actually send. Equality of the whole frame, not a spot check on one
        // key: this is the assertion that would have caught `["REQ", id, [a, b]]`.
        let mut state = BgState::new();
        let (mut ws, mut server) = test_ws_pair().await;
        assert!(
            matches!(
                state
                    .project_requests
                    .replace_watched(&mut ws, watched_filters(&roots, 0))
                    .await,
                crate::project::ReplaceOutcome::Replaced { .. }
            ),
            "the watched replacement must install"
        );
        let id = installed_watched_id(&state);
        let written = next_test_frame(&mut server).await;

        let produced = crate::project::project_req_frames(
            true,
            &crate::project::DiscoveredRepositories::new(),
            &watched_enrolments(&roots),
            &"1".repeat(64),
            0,
        );
        assert_eq!(
            produced.len(),
            1,
            "nothing is discovered, so the watched REQ is the only frame: {produced:?}"
        );
        assert_eq!(
            written, produced[0],
            "the REQ on the wire must be the frame the builder produces, filter \
             for filter and in order"
        );
        assert_eq!(
            written.as_array().map(Vec::len),
            Some(4),
            "and the two filters ride as separate REQ elements, not as one array"
        );

        // ---- What it admits, on the same registration. ---------------------
        let (tx, mut rx) = mpsc::channel(16);
        let keys = nostr::Keys::generate();
        let comment_kind = crate::project::HistoryStream::Comments.kinds()[0] as u16;
        let update_kind = crate::project::HistoryStream::PullRequestUpdates.kinds()[0] as u16;

        // Each reference style on its own branch. Neither would be admitted by
        // the other's filter, so both are needed to show the OR is real.
        for (why, event) in [
            (
                "a comment on the issue root, lowercase `e`",
                tagged_event(&keys, comment_kind, 10, &[&["e", &issue_root, "", "root"]]),
            ),
            (
                "a revision on the pull-request root, uppercase `E`",
                tagged_event(&keys, update_kind, 10, &[&["E", &pr_root]]),
            ),
        ] {
            assert_eq!(
                delivery_and_dedup(&mut state, &id, &event, &tx, &mut rx).await,
                (1, true),
                "{why}: the request asked for this"
            );
        }

        // The branches must not lend each other their root list either.
        //
        // Chosen so the **filter** is what refuses it: a revision resolves its
        // root through its uppercase `E`, so this event derives a route and
        // would be delivered by any earlier step. The comments branch does not
        // ask for this kind and the revisions branch does not ask for this root,
        // so no single filter is satisfied — which is what a two-filter REQ
        // means and what merging the branches into one constraint set would
        // lose.
        //
        // The tag-style crossings hermes-gateway asked for — a comment carrying
        // only `E`, a revision carrying only `e` — are asserted in
        // `project::tests::a_request_admits_an_event_matching_any_one_of_its_filters_entirely`
        // instead. On this path they never reach the filter: route derivation
        // refuses them first, so asserting them here would pass with the filter
        // check deleted entirely.
        let wrong_branch_root = tagged_event(&keys, update_kind, 10, &[&["E", &issue_root]]);
        assert_eq!(
            delivery_and_dedup(&mut state, &id, &wrong_branch_root, &tx, &mut rx).await,
            (0, false),
            "a revision on a root watched only for comments must deliver nothing \
             and spend nothing"
        );

        // ---- And a reconnect re-asks the same question. --------------------
        //
        // Byte-for-byte against the frame recorded above, because a replay that
        // dropped a branch would leave the agent silently blind to one
        // reference style on every connection after the first — and the suite
        // would stay green, since the first connection asked correctly.
        state.project_requests.clear_connection();
        let (mut replacement, mut replacement_server) = test_ws_pair().await;
        let replayable = state
            .project_requests
            .replayable()
            .expect("a canonical durable record");
        assert_eq!(replayable.len(), 1, "one request to re-ask: {replayable:?}");
        for request in replayable {
            assert_eq!(
                send_project_replay(&mut replacement, &mut state, request).await,
                ProjectSendOutcome::Sent
            );
        }
        assert_eq!(
            next_test_frame(&mut replacement_server).await,
            written,
            "the reconnect must re-ask the complete filter set"
        );
    }

    /// An enrolment request admits only events naming its project *and* its
    /// agent.
    ///
    /// Built with the production filter, not a hand-written one: this is the
    /// request that decides which mentions become project work, and a fixture
    /// that invented its own `#a`/`#p` shape would be checking a question
    /// nobody asks.
    #[tokio::test]
    async fn an_enrolment_request_admits_only_its_project_and_its_agent() {
        let mut state = BgState::new();
        let (tx, mut rx) = mpsc::channel(16);
        let author = nostr::Keys::generate();
        let agent = nostr::Keys::generate().public_key().to_hex();
        let stranger = nostr::Keys::generate().public_key().to_hex();
        let coordinate = format!("30617:{}:repo", author.public_key().to_hex());
        let other_coordinate = format!("30617:{}:elsewhere", author.public_key().to_hex());

        let discovered = crate::project::DiscoveredRepositories::for_test([coordinate.clone()]);
        let filter = crate::project::enrolment_filter(&discovered, &agent, 0)
            .expect("a known coordinate yields a filter");
        // The id is the registry's, not a name this test invents: an enrolment
        // class recorded under `proj-enrolment-test` is exactly the
        // caller-selected identity this tranche removed.
        let id = open_enrolment_with(&mut state, vec![filter]).await;

        let kind = buzz_core::kind::KIND_TEXT_NOTE as u16;
        let root = test_root_id();
        let refused: [(&str, Event); 4] = [
            (
                "no agent p-tag at all",
                tagged_event(
                    &author,
                    kind,
                    10,
                    &[&["a", &coordinate], &["e", &root, "", "root"]],
                ),
            ),
            (
                "another agent's p-tag",
                tagged_event(
                    &author,
                    kind,
                    10,
                    &[
                        &["a", &coordinate],
                        &["p", &stranger],
                        &["e", &root, "", "root"],
                    ],
                ),
            ),
            (
                "no project a-tag at all",
                tagged_event(
                    &author,
                    kind,
                    10,
                    &[&["p", &agent], &["e", &root, "", "root"]],
                ),
            ),
            (
                "another project's a-tag",
                tagged_event(
                    &author,
                    kind,
                    10,
                    &[
                        &["a", &other_coordinate],
                        &["p", &agent],
                        &["e", &root, "", "root"],
                    ],
                ),
            ),
        ];

        for (why, event) in &refused {
            assert_eq!(
                delivery_and_dedup(&mut state, &id, event, &tx, &mut rx).await,
                (0, false),
                "{why}: must deliver nothing and spend nothing"
            );
        }

        let admissible = tagged_event(
            &author,
            kind,
            10,
            &[
                &["a", &coordinate],
                &["p", &agent],
                &["e", &root, "", "root"],
            ],
        );
        assert_eq!(
            delivery_and_dedup(&mut state, &id, &admissible, &tx, &mut rx).await,
            (1, true),
            "an event naming this project and this agent is what was asked for"
        );
    }

    /// A comments page for `bound`, opened the way a driver must and held by
    /// this connection's reconstructions.
    ///
    /// The owner issues the collector; the registry mints the wire id, writes
    /// the REQ to a real socket, installs the registration and binds the page;
    /// the owner attaches what comes back. Returns that minted id, because
    /// nothing else knows it — which is the property blocker 4 bought.
    async fn bind_page_under(
        state: &mut BgState,
        bound: &crate::project::VerifiedBoundRoot,
    ) -> String {
        let root = bound.binding().root().to_string();
        assert!(
            state
                .reconstructions
                .insert(crate::project::RootReconstruction::begin(
                    bound, 1_000, 4, 1_000
                )),
            "one reconstruction per root"
        );
        open_page_under(state, &root).await
    }

    /// Open the next comments page for a root already being reconstructed, and
    /// return the wire id the registry minted for it.
    ///
    /// **The caller cannot choose the id, here or in production.** It used to
    /// pass one in, which is how a fixture could hand two successive pages the
    /// same name — the thing that let a delayed frame from the first be stamped
    /// with the second's authority.
    ///
    /// **No `close_active` either.** An earlier version retired whatever was
    /// live before every page, performing the transition the production EOSE
    /// path was missing and therefore hiding that it was missing.
    async fn open_page_under(state: &mut BgState, root: &str) -> String {
        let collector = state
            .reconstructions
            .get(root)
            .expect("the root is being reconstructed")
            .begin_page(crate::project::HistoryStream::Comments)
            .expect("that stream wants a page");
        let (mut ws, _server) = test_ws_pair().await;
        let page = match state
            .project_requests
            .open_history_page(&mut ws, collector)
            .await
        {
            crate::project::PageOpen::Opened(page) => page,
            other => panic!("a real socket must open a page: {other:?}"),
        };
        let sub_id = page.sub_id().to_string();
        state
            .reconstructions
            .get(root)
            .expect("still tracked")
            .attach(page)
            .map_err(|r| r.error)
            .expect("attaches");
        sub_id
    }

    /// Close the page under `sub_id` with its own genuine boundary and report
    /// what the reconstruction kept.
    ///
    /// `Ok(rows)` when the page completed and retained that many events;
    /// `Err(reason)` when it could not account for what arrived and the
    /// reconstruction gave up. The distinction is the whole point: a page that
    /// silently drops a frame reports `Ok` one row short, and one row short of
    /// the limit is how a reconstruction decides history is exhausted.
    async fn page_verdict(
        state: &mut BgState,
        root: &str,
        sub_id: &str,
        tx: &mpsc::Sender<Option<BuzzEvent>>,
    ) -> Result<usize, String> {
        assert!(
            deliver_control_frame_to(state, json!(["EOSE", sub_id]), tx).await,
            "dispatch must not signal connection loss"
        );
        if let Some(reason) = state.root_catch_up_degraded.get(root) {
            return Err(reason.clone());
        }
        match state.reconstructions.get(root) {
            Some(recon) => match recon.abandoned_reason() {
                Some(reason) => Err(reason.to_string()),
                None => Ok(recon.finished_streams().iter().map(|s| s.len()).sum()),
            },
            // Not tracked means *finished*: the last stream this root requires
            // completed on this boundary, its streams were merged and handed to
            // the run loop, and the reconstruction retired. The count comes from
            // the record that retirement leaves, and it is the same number — the
            // merge carries every retained row plus the root that leads it.
            None => Ok(*state
                .root_catch_up_done
                .get(root)
                .expect("a retired reconstruction records what it rebuilt")),
        }
    }

    /// The composed Piece 3 path, end to end on real frames.
    ///
    /// REQ on a real socket → an `["EVENT", id, e]` frame through
    /// `handle_ws_message` → routed to the page that exact request opened →
    /// `["EOSE", id]` finishing that same page.
    ///
    /// Before this, the last two steps did not exist: a catch-up frame reached
    /// the routed arm of `handle_project_event`, which logged and dropped it. A
    /// page could be opened, bound and attached, and no row ever reached it.
    #[tokio::test]
    async fn a_page_fills_from_the_wire_and_completes_at_its_own_boundary() {
        let mut state = BgState::new();
        let (tx, mut rx) = mpsc::channel(16);
        let (bound, keys) = proven_issue_root().await;
        let root = bound.binding().root().to_string();
        let sub_id = bind_page_under(&mut state, &bound).await;

        let comment = comment_on_root(&keys, &root, 900, "a comment");
        deliver_frame(&mut state, &sub_id, &comment, &tx).await;

        assert_eq!(
            page_verdict(&mut state, &root, &sub_id, &tx).await,
            Ok(1),
            "the row that arrived on the wire is the row it retained"
        );
        // The row does escape now — that is the point of rebuilding a history —
        // but only as replay. The claim this replaces ("it never escaped") was
        // true of a phase where completion had no consumer; the claim that
        // survives is that nothing a page retained can wake a turn.
        let escaped: Vec<_> = drain(&mut rx)
            .into_iter()
            .filter_map(|e| match e {
                BuzzEvent::Project(crate::project::ProjectEvent::Routed {
                    source, mode, ..
                }) => Some((source, mode)),
                _ => None,
            })
            .collect();
        assert_eq!(
            escaped.len(),
            1,
            "the retained row is handed on: {escaped:?}"
        );
        assert!(
            matches!(
                escaped[0].0,
                crate::project::ProjectSubscription::RootCatchUp { .. }
            ),
            "under the class of the page that fetched it: {:?}",
            escaped[0].0
        );
        assert_eq!(
            escaped[0].1,
            crate::project::ProcessingMode::Replay,
            "and as history, which never runs a turn"
        );
    }

    /// A frame the agent refuses poisons the page instead of shortening it.
    ///
    /// The defect this closes is quiet: a page counts what the relay returned
    /// under its `limit` to tell a saturated page from an exhausted one. The
    /// dispatch used to drop an unverifiable frame with `return true`, so the
    /// page never learned one had arrived, read one row short of the limit, and
    /// declared the history exhausted. One forged frame ended a reconstruction
    /// early and the result claimed to be complete.
    #[tokio::test]
    async fn an_unverifiable_frame_poisons_the_page_instead_of_shortening_it() {
        let mut state = BgState::new();
        let (tx, mut rx) = mpsc::channel(16);
        let (bound, keys) = proven_issue_root().await;
        let root = bound.binding().root().to_string();
        let sub_id = bind_page_under(&mut state, &bound).await;

        // Re-serialised with mutated content: the id and signature no longer
        // match, which is what a malicious relay sends.
        let genuine = comment_on_root(&keys, &root, 900, "a comment");
        let mut json = serde_json::to_value(&genuine).expect("encode");
        json["content"] = serde_json::Value::String("tampered".to_string());
        let forged: Event = serde_json::from_value(json).expect("decode");

        deliver_frame(&mut state, &sub_id, &forged, &tx).await;

        let verdict = page_verdict(&mut state, &root, &sub_id, &tx).await;
        assert!(
            verdict.is_err(),
            "a page that received a frame it cannot account for must not complete: {verdict:?}"
        );
        assert!(
            state
                .reconstructions
                .get(&root)
                .expect("still tracked")
                .finished_streams()
                .is_empty(),
            "and must claim no exhausted history"
        );
        // **And it hands nothing on.** Now that a completed reconstruction has
        // a consumer, "claims no exhausted history" and "replays no history"
        // are separate facts, and only the second one protects the watch: a
        // poisoned page that delivered the rows it did receive would fold a
        // *partial* lifecycle into the enrolment set — the close without the
        // reopen that followed it is a permanently silent conversation, and it
        // would look exactly like a healthy replay.
        assert!(
            drain(&mut rx).iter().all(|e| !matches!(
                e,
                BuzzEvent::Project(crate::project::ProjectEvent::Routed { .. })
            )),
            "a degraded root replays nothing at all"
        );
    }

    /// A row already delivered on a live surface still counts on the page.
    ///
    /// Catch-up rows used to share `project_seen_ids` with discovery, enrolment
    /// and watched traffic. An event the agent had already been handed live was
    /// therefore suppressed as a duplicate on the history page — shortening the
    /// page by exactly the number of events it had already seen, which is the
    /// same false end-of-history by a different route.
    #[tokio::test]
    async fn a_row_already_delivered_live_still_reaches_the_page() {
        let mut state = BgState::new();
        let (tx, mut rx) = mpsc::channel(16);
        let (bound, keys) = proven_issue_root().await;
        let root = bound.binding().root().to_string();
        let comment = comment_on_root(&keys, &root, 900, "seen live first");

        // Live first: this spends the shared dedup slot. The watched request
        // has to actually be watching this root, or the frame is refused before
        // it can spend anything — which is the point of the check, not a way
        // around setting the fixture up properly.
        let watched = open_watched_for(&mut state, &[&root]).await;
        deliver_frame(&mut state, &watched, &comment, &tx).await;
        assert_eq!(drain(&mut rx).len(), 1, "the live delivery happens");
        assert!(state.project_seen_ids.contains(&comment.id.to_hex()));

        let sub_id = bind_page_under(&mut state, &bound).await;

        deliver_frame(&mut state, &sub_id, &comment, &tx).await;
        assert_eq!(
            page_verdict(&mut state, &root, &sub_id, &tx).await,
            Ok(1),
            "the page keeps the row the live surface had already spent"
        );
    }

    /// A `CLOSED` releases the page instead of stalling its stream forever.
    ///
    /// No boundary can follow a `CLOSED` — the registration is gone, and
    /// `witness_end_of_stored_events` needs a live one — so a page left attached
    /// can never complete. `pages_wanted` skips a stream holding a page, so the
    /// stream would never ask again either: one relay message, and that root
    /// stops reconstructing in silence.
    #[tokio::test]
    async fn a_closed_request_releases_its_page_rather_than_stalling_the_stream() {
        let mut state = BgState::new();
        let (tx, _rx) = mpsc::channel(16);
        let (bound, _keys) = proven_issue_root().await;
        let root = bound.binding().root().to_string();
        let sub_id = bind_page_under(&mut state, &bound).await;
        assert!(
            state
                .reconstructions
                .get(&root)
                .expect("tracked")
                .pages_wanted()
                .is_empty(),
            "the stream holds a page, so it wants none"
        );

        assert!(
            deliver_control_frame_to(
                &mut state,
                json!(["CLOSED", sub_id, "error: rate-limited"]),
                &tx
            )
            .await,
            "a project CLOSED must not drop the socket"
        );

        let recon = state
            .reconstructions
            .get(&root)
            .expect("the root is still tracked");
        assert!(
            recon.abandoned_reason().is_none(),
            "a closed request is not a corrupt page — nothing is wrong with what it received"
        );
        // Released *and re-asked*. The earlier version of this test stopped at
        // `pages_wanted`, which was as far as production went; the driver now
        // opens the replacement itself, so the observable is the page in
        // flight rather than the appetite for one.
        let (reopened, until) = recon
            .in_flight_page(crate::project::HistoryStream::Comments)
            .expect("the stream asks again rather than stalling");
        assert_ne!(reopened, sub_id, "under a fresh registration");
        assert_eq!(until, 1_000, "from the bound it already had");
    }

    /// A reconnect releases the pages the dead connection opened.
    ///
    /// The same silence as a `CLOSED`, from the routine event rather than the
    /// rare one. `clear_connection` retires every registration the old socket
    /// held, so no boundary can ever be minted for a page opened under one; a
    /// reconstruction that was not told keeps the page, and `pages_wanted` skips
    /// a stream that holds one. That root would stop asking for history on an
    /// otherwise healthy connection, with nothing logged and nothing failed.
    #[tokio::test]
    async fn a_reconnect_releases_the_pages_the_dead_connection_opened() {
        let mut state = BgState::new();
        let (tx, _rx) = mpsc::channel(16);
        let (bound, _keys) = proven_issue_root().await;
        let root = bound.binding().root().to_string();
        let sub_id = bind_page_under(&mut state, &bound).await;

        // The production reconnect path, on a fresh connection.
        let (ws, _server) = test_ws_pair().await;
        assert!(matches!(
            reconnect_onto(&mut state, ws).await,
            ResubscribeResult::Ok
        ));

        let recon = state
            .reconstructions
            .get(&root)
            .expect("the root is still tracked");
        assert!(
            recon.abandoned_reason().is_none(),
            "a reconnect is not a corrupt page"
        );
        // Released *and re-asked*, by the reconnect itself. Stopping at
        // `pages_wanted` was as far as production went when this was written;
        // the resubscribe now opens the replacement, so a root that stayed
        // silent here would be a root that stopped reconstructing.
        let (reopened, until) = recon
            .in_flight_page(crate::project::HistoryStream::Comments)
            .expect("the stream asks again rather than stalling");
        assert_ne!(reopened, sub_id, "under a fresh registration");
        assert_eq!(until, 1_000, "from the bound it already had");
        assert!(
            state.project_requests.match_frame(&sub_id).is_none(),
            "and the registration that opened it is gone, so nothing can complete it"
        );

        // The page really is unusable now: its own boundary cannot even be
        // minted, so releasing it is the only thing that keeps the stream alive.
        assert!(
            deliver_control_frame_to(&mut state, json!(["EOSE", sub_id]), &tx).await,
            "dispatch must not signal connection loss"
        );
        assert!(state
            .reconstructions
            .get(&root)
            .expect("tracked")
            .finished_streams()
            .is_empty());
    }

    /// A completed catch-up answers nothing further.
    ///
    /// Its boundary retired it, so the id it used stops being a way in. Each of
    /// these was reachable while the registration outlived its own answer: a
    /// second `EOSE` minting a second boundary, an `EVENT` still admitted into a
    /// page that had already been completed, and a `CLOSED` recording a refusal
    /// of a request that was over — which would then suspend the *next* page's
    /// replay for a reason belonging to the previous one.
    #[tokio::test]
    async fn a_completed_catch_up_answers_nothing_further() {
        let mut state = BgState::new();
        let (tx, mut rx) = mpsc::channel(16);
        let (bound, keys) = proven_issue_root().await;
        let root = bound.binding().root().to_string();
        let sub_id = bind_page_under(&mut state, &bound).await;

        let first = comment_on_root(&keys, &root, 900, "the one row");
        deliver_frame(&mut state, &sub_id, &first, &tx).await;
        assert_eq!(
            page_verdict(&mut state, &root, &sub_id, &tx).await,
            Ok(1),
            "a short page is an exhausted stream"
        );
        assert!(
            state.project_requests.match_frame(&sub_id).is_none(),
            "and the request that asked is retired by its own answer"
        );

        // A second boundary for the same id.
        assert_eq!(
            eose_outcome(&mut state, &root, &sub_id, &tx).await,
            EoseOutcome {
                still_live: false,
                page_finished: true
            },
            "a duplicate EOSE changes nothing — there is no request left to answer"
        );

        // The completed reconstruction handed its history on; that is a
        // separate fact, asserted above. Drained here so the emptiness checked
        // below is about the late frame and nothing else.
        let _ = drain(&mut rx);

        // A late row on the same id.
        let late = comment_on_root(&keys, &root, 899, "after the end");
        deliver_frame(&mut state, &sub_id, &late, &tx).await;
        assert!(
            !state.root_catch_up_degraded.contains_key(&root),
            "an unadmitted frame is not a contradiction — it never reached the owner"
        );
        assert_eq!(
            state.root_catch_up_done.get(&root),
            Some(&1),
            "and it is not in the history the completed page rebuilt"
        );
        assert!(
            !state.project_seen_ids.contains(&late.id.to_hex()),
            "nor was it laundered onto a live surface"
        );
        assert!(drain(&mut rx).is_empty());

        // A `CLOSED` for the same id.
        assert!(
            deliver_control_frame_to(
                &mut state,
                json!(["CLOSED", sub_id, "error: rate-limited"]),
                &tx
            )
            .await
        );
        assert_eq!(
            state.project_requests.suspension(&sub_id),
            None,
            "a request that already ended cannot be refused, and a refusal \
             recorded here would suspend the next page for the previous one's sake"
        );
    }

    /// A retired page's raw frames cannot act on the page that replaced it.
    ///
    /// The reviewer's sequence, at the wire: page A opens, is answered and
    /// retired, page B opens; then A's `EVENT`, `EOSE` and `CLOSED` arrive
    /// late. Under a deterministic catch-up id all three named whatever was
    /// live — B — and were stamped with B's authority before any comparison
    /// could tell them apart. The `EOSE` was the dangerous one: it finished B
    /// as an empty page, which reads as "this history is exhausted".
    ///
    /// Raw frames, not pre-minted proofs: the defect was in the minting, so a
    /// test that started from an admission or a witness would have started
    /// after it.
    #[tokio::test]
    async fn a_retired_pages_raw_frames_cannot_act_on_its_successor() {
        let mut state = BgState::new();
        let (tx, mut rx) = mpsc::channel(16);
        let (bound, keys) = proven_issue_root().await;
        let root = bound.binding().root().to_string();

        // Page A: saturated, so the stream continues rather than finishing.
        let page_a = bind_page_under(&mut state, &bound).await;
        for (i, ts) in [900u64, 899, 898, 897].iter().enumerate() {
            let row = comment_on_root(&keys, &root, *ts, &format!("page one row {i}"));
            deliver_frame(&mut state, &page_a, &row, &tx).await;
        }
        assert!(deliver_control_frame_to(&mut state, json!(["EOSE", page_a]), &tx).await);

        // Page B, opened by A's own boundary. Under a name of its own — but
        // that is asserted at the *end*, deliberately. Two attempts sharing one
        // name is the mechanism of the defect, so checking it here would make
        // this test report "the ids are equal" and stop, in place of the
        // outcome the ids exist to prevent.
        let (page_b, _) = state
            .reconstructions
            .get(&root)
            .expect("tracked")
            .in_flight_page(crate::project::HistoryStream::Comments)
            .expect("the saturated stream asks again");
        let _ = drain(&mut rx);

        // Now A's stragglers, all three kinds, all naming A.
        let straggler = comment_on_root(&keys, &root, 896, "late from page one");
        deliver_frame(&mut state, &page_a, &straggler, &tx).await;
        assert!(deliver_control_frame_to(&mut state, json!(["EOSE", page_a]), &tx).await);
        assert!(
            deliver_control_frame_to(
                &mut state,
                json!(["CLOSED", page_a, "error: rate-limited"]),
                &tx
            )
            .await
        );

        let recon = state
            .reconstructions
            .get(&root)
            .expect("the root is still tracked");
        assert!(
            recon.finished_streams().is_empty(),
            "the predecessor's boundary must not finish the page that replaced it"
        );
        assert!(
            recon.abandoned_reason().is_none(),
            "and a late straggler is not a reason to abandon a root"
        );
        assert!(
            recon.pages_wanted().is_empty(),
            "page B is still in flight, so the stream is not asking for another"
        );
        assert_eq!(
            state.project_requests.suspension(&page_a),
            None,
            "and a CLOSED for a request that already ended records no refusal"
        );
        assert!(
            !state.project_seen_ids.contains(&straggler.id.to_hex()),
            "nor does the straggler spend a live-surface dedup slot"
        );

        // B's own frames still work, so none of the above passed by breaking
        // the page it was protecting.
        let wanted_until = 897;
        let row = comment_on_root(&keys, &root, wanted_until, "page two row");
        deliver_frame(&mut state, &page_b, &row, &tx).await;
        assert_eq!(
            page_verdict(&mut state, &root, &page_b, &tx).await,
            Ok(5),
            "page two takes its own rows and completes on its own boundary"
        );

        // And the mechanism, last: everything above holds because the two
        // attempts never shared a name.
        assert_ne!(page_a, page_b, "one page, one wire id");
    }

    /// The next page opens with no out-of-band cleanup, under a name of its own.
    ///
    /// Two transitions in one sequence, and each used to be missing. Page one's
    /// boundary must retire page one's registration: while the old entry
    /// survived, opening page two had to be preceded by a hand-written
    /// `close_active`, which every fixture here performed — which is exactly how
    /// the missing transition stayed invisible. And page two must not re-register
    /// page one's name, because a page's id is what admits its frames.
    #[tokio::test]
    async fn the_next_page_opens_clean_and_under_its_own_name() {
        let mut state = BgState::new();
        let (tx, _rx) = mpsc::channel(16);
        let (bound, keys) = proven_issue_root().await;
        let root = bound.binding().root().to_string();
        let sub_id = bind_page_under(&mut state, &bound).await;

        // Saturate page one: four rows against a limit of four, so the stream
        // continues rather than finishing.
        for (i, ts) in [900u64, 899, 898, 897].iter().enumerate() {
            let row = comment_on_root(&keys, &root, *ts, &format!("page one row {i}"));
            deliver_frame(&mut state, &sub_id, &row, &tx).await;
        }
        assert!(
            deliver_control_frame_to(&mut state, json!(["EOSE", sub_id]), &tx).await,
            "dispatch must not signal connection loss"
        );
        assert!(
            state.project_requests.match_frame(&sub_id).is_none(),
            "the boundary retired page one's registration"
        );

        // Page two is already in flight: the boundary that retired page one
        // drove it, which is the step this test used to perform for production.
        let (second, next_until) = state
            .reconstructions
            .get(&root)
            .expect("tracked")
            .in_flight_page(crate::project::HistoryStream::Comments)
            .expect("the saturated stream asks again");
        assert!(
            next_until < 1_000,
            "and from an advanced bound: {next_until}"
        );

        // No `close_active` anywhere in here, and page two got a name of its
        // own rather than inheriting page one's.
        assert_ne!(second, sub_id, "one page, one wire id");
        let row = comment_on_root(&keys, &root, next_until, "page two row");
        deliver_frame(&mut state, &second, &row, &tx).await;
        assert_eq!(
            page_verdict(&mut state, &root, &second, &tx).await,
            Ok(5),
            "page two completes the stream, and the history is both pages'"
        );
    }

    /// Frames the replacement connection buffered cannot act on the dead
    /// connection's page.
    ///
    /// The window this closes: `do_connect` returns a socket *plus* whatever
    /// arrived on it during the handshake, and those frames go through the
    /// ordinary dispatch, which authenticates a project frame against whatever
    /// the registry says is live. While the dead connection's registrations were
    /// still there, "live" meant *its* registrations — so a `["EOSE", id]` the
    /// replacement happened to carry could mint a boundary for a request the
    /// replacement never sent and complete a page opened on a socket that no
    /// longer exists. An `EVENT` in the same buffer would have been filed into
    /// that page as a row, and a `CLOSED` recorded as its refusal.
    ///
    /// All three are in one buffer here because the fix is not three fixes: it
    /// is the order of two lines, and any frame the dispatch can authenticate
    /// is in scope.
    #[tokio::test]
    async fn a_replacement_connections_buffered_frames_cannot_act_on_the_dead_page() {
        let mut state = BgState::new();
        let (bound, keys) = proven_issue_root().await;
        let root = bound.binding().root().to_string();
        let sub_id = bind_page_under(&mut state, &bound).await;

        let row = comment_on_root(&keys, &root, 900, "buffered by the new socket");
        let buffered = handshake_buffer_from_wire(vec![
            json!(["EVENT", sub_id, row]),
            json!(["EOSE", sub_id]),
            json!(["CLOSED", sub_id, "error: whatever"]),
        ])
        .await;

        let (mut dead, _dead_server) = test_ws_pair().await;
        let (replacement, _server) = test_ws_pair().await;
        assert!(
            install_replacement_with(&mut state, &mut dead, replacement, buffered).await,
            "none of these frames is a reason to drop the new connection"
        );

        let recon = state
            .reconstructions
            .get(&root)
            .expect("the root is still tracked");
        assert!(
            recon.finished_streams().is_empty(),
            "the dead connection's page must not have been completed by a boundary \
             the replacement carried"
        );
        assert!(
            recon.abandoned_reason().is_none(),
            "and nothing about a routine reconnect is a contradiction"
        );
        assert_eq!(
            recon.pages_wanted(),
            vec![(crate::project::HistoryStream::Comments, 1_000, 4)],
            "the page was released, so the stream asks again from its own bound"
        );
        assert!(
            state.project_requests.match_frame(&sub_id).is_none(),
            "and the registration those frames named is gone before any of them ran"
        );
        assert_eq!(
            state.project_requests.suspension(&sub_id),
            None,
            "a CLOSED for a request this connection never sent records no refusal"
        );
        assert!(
            !state.project_seen_ids.contains(&row.id.to_hex()),
            "and the buffered EVENT was not admitted on any surface"
        );
    }

    /// Subscribe a channel via the production command path so the test exercises
    /// real subscription state (active_subscriptions + active_filters + since).
    fn subscribe_channel(state: &mut BgState, channel_id: Uuid) {
        apply_command_to_state(
            state,
            RelayCommand::Subscribe {
                channel_id,
                filter: ChannelFilter {
                    kinds: Some(vec![9]),
                    require_mention: false,
                },
                replay_since: Some(1_000),
            },
        );
    }

    #[test]
    fn not_a_channel_member_drops_channel_without_reconnect() {
        let mut state = BgState::new();
        let channel_id = Uuid::new_v4();
        subscribe_channel(&mut state, channel_id);

        let handled = drop_channel_on_access_denied(
            &mut state,
            &channel_sub_id(channel_id),
            "restricted: not a channel member",
        );

        assert!(handled, "per-channel denial must be handled (no reconnect)");
        assert!(
            !state.active_subscriptions.contains_key(&channel_id),
            "the forbidden channel's subscription must be dropped"
        );
        assert!(
            !state.active_filters.contains_key(&channel_id),
            "channel state must be cleared (Unsubscribe cleanup)"
        );
    }

    #[test]
    fn channel_access_revoked_drops_channel_without_reconnect() {
        let mut state = BgState::new();
        let channel_id = Uuid::new_v4();
        subscribe_channel(&mut state, channel_id);

        let handled = drop_channel_on_access_denied(
            &mut state,
            &channel_sub_id(channel_id),
            "restricted: channel access revoked",
        );

        assert!(handled, "per-channel denial must be handled (no reconnect)");
        assert!(!state.active_subscriptions.contains_key(&channel_id));
        assert!(!state.active_filters.contains_key(&channel_id));
    }

    #[test]
    fn insufficient_scope_is_not_dropped_and_reconnects() {
        let mut state = BgState::new();
        let channel_id = Uuid::new_v4();
        subscribe_channel(&mut state, channel_id);

        let handled = drop_channel_on_access_denied(
            &mut state,
            &channel_sub_id(channel_id),
            "restricted: insufficient scope",
        );

        assert!(
            !handled,
            "connection-level insufficient-scope must fall through to reconnect, not drop the channel"
        );
        assert!(
            state.active_subscriptions.contains_key(&channel_id),
            "the channel must survive so reconnect can restore it"
        );
    }

    #[test]
    fn auth_required_is_not_dropped_and_reconnects() {
        let mut state = BgState::new();
        let channel_id = Uuid::new_v4();
        subscribe_channel(&mut state, channel_id);

        let handled = drop_channel_on_access_denied(
            &mut state,
            &channel_sub_id(channel_id),
            "auth-required: not authenticated",
        );

        assert!(
            !handled,
            "auth-required must fall through to reconnect, not drop the channel"
        );
        assert!(state.active_subscriptions.contains_key(&channel_id));
    }

    #[test]
    fn already_removed_channel_is_a_no_op() {
        let mut state = BgState::new();
        let channel_id = Uuid::new_v4();
        // Channel was never subscribed (or already dropped) — a delayed CLOSED.

        let handled = drop_channel_on_access_denied(
            &mut state,
            &channel_sub_id(channel_id),
            "restricted: not a channel member",
        );

        assert!(
            handled,
            "an exact per-channel denial is still handled (keep socket) even if the channel is gone"
        );
        assert!(
            !state.active_subscriptions.contains_key(&channel_id),
            "no-op: nothing to remove and nothing resurrected"
        );
    }

    #[test]
    fn dropped_channel_is_not_resubscribed_so_loop_cannot_re_form() {
        let mut state = BgState::new();
        let channel_id = Uuid::new_v4();
        subscribe_channel(&mut state, channel_id);

        drop_channel_on_access_denied(
            &mut state,
            &channel_sub_id(channel_id),
            "restricted: not a channel member",
        );

        // Simulate a reconnect: only channels still in active_subscriptions are
        // restored. The dropped channel must not be among them — otherwise the
        // forbidden channel would be resubscribed and earn the same CLOSED again.
        let resubscribed: Vec<Uuid> = state.active_subscriptions.keys().copied().collect();
        assert!(
            !resubscribed.contains(&channel_id),
            "the dropped channel must not be resubscribed — the loop cannot re-form"
        );
    }

    // ── startup connect retry ────────────────────────────────────────────

    /// Table-driven coverage of every `RelayError` variant and every
    /// `tungstenite::Error` inner variant. Exhaustive — adding a new
    /// tungstenite variant without updating this table is a compile error
    /// in `is_terminal_ws_error` (no wildcard), and a missing row here
    /// is a code-review gap, not a silent misclassification.
    #[test]
    fn connect_error_classification_matches_every_relay_error_variant() {
        use tokio_tungstenite::tungstenite::error::{
            CapacityError, Error as WsError, ProtocolError, SubProtocolError, TlsError, UrlError,
        };
        use tokio_tungstenite::tungstenite::http;

        fn ws(e: WsError) -> RelayError {
            RelayError::WebSocket(Box::new(e))
        }

        let cases: Vec<(&str, RelayError, bool)> = vec![
            // ── outer RelayError variants ──
            ("Http: bad URL", RelayError::Http("bad url".into()), true),
            (
                "Json: malformed relay frame",
                RelayError::Json(serde_json::from_str::<()>("not json").unwrap_err()),
                true,
            ),
            (
                "UnexpectedMessage: unknown frame type",
                RelayError::UnexpectedMessage("unknown message type: WAT".into()),
                true,
            ),
            (
                "AuthFailed: relay dependency fault (NIP-01 `error:` prefix)",
                RelayError::AuthFailed("error: internal error checking restriction state".into()),
                false,
            ),
            (
                "AuthFailed: bad signature (`invalid:` prefix)",
                RelayError::AuthFailed("invalid: bad signature".into()),
                true,
            ),
            (
                "AuthFailed: banned (`blocked:` prefix)",
                RelayError::AuthFailed("blocked: you are banned from this community".into()),
                true,
            ),
            (
                "AuthFailed: not a member (`restricted:` prefix)",
                RelayError::AuthFailed("restricted: not a relay member".into()),
                true,
            ),
            (
                "AuthFailed: allowlist denial (`auth-required:` prefix)",
                RelayError::AuthFailed("auth-required: verification failed".into()),
                true,
            ),
            (
                "AuthFailed: unrecognized prefix fails safe as terminal",
                RelayError::AuthFailed("some new denial reason".into()),
                true,
            ),
            (
                "NoAuthChallenge: relay silence is link/relay-timing noise",
                RelayError::NoAuthChallenge,
                false,
            ),
            ("ConnectionClosed", RelayError::ConnectionClosed, false),
            ("Timeout", RelayError::Timeout, false),
            // ── WebSocket inner: terminal ──
            (
                "WebSocket(Url): unsupported scheme",
                ws(WsError::Url(UrlError::UnsupportedUrlScheme)),
                true,
            ),
            (
                "WebSocket(Url): missing host",
                ws(WsError::Url(UrlError::NoHostName)),
                true,
            ),
            (
                "WebSocket(Url): empty host",
                ws(WsError::Url(UrlError::EmptyHostName)),
                true,
            ),
            (
                "WebSocket(Url): TLS feature not enabled",
                ws(WsError::Url(UrlError::TlsFeatureNotEnabled)),
                true,
            ),
            (
                "WebSocket(Url): unable to connect",
                ws(WsError::Url(UrlError::UnableToConnect("addr".into()))),
                true,
            ),
            (
                "WebSocket(Url): no path or query",
                ws(WsError::Url(UrlError::NoPathOrQuery)),
                true,
            ),
            (
                "WebSocket(Capacity): message too long",
                ws(WsError::Capacity(CapacityError::MessageTooLong {
                    size: 100,
                    max_size: 50,
                })),
                true,
            ),
            (
                "WebSocket(Capacity): too many headers",
                ws(WsError::Capacity(CapacityError::TooManyHeaders)),
                true,
            ),
            (
                "WebSocket(Utf8): encoding error",
                ws(WsError::Utf8("invalid utf-8".into())),
                true,
            ),
            (
                "WebSocket(HttpFormat): malformed HTTP",
                ws(WsError::HttpFormat(
                    http::Response::builder().status(9999).body(()).unwrap_err(),
                )),
                true,
            ),
            ("WebSocket(AttackAttempt)", ws(WsError::AttackAttempt), true),
            // ── WebSocket inner: Http status split ──
            (
                "WebSocket(Http): 200 = plain HTTPS endpoint → terminal",
                ws(WsError::Http(Box::new(
                    http::Response::builder().status(200).body(None).unwrap(),
                ))),
                true,
            ),
            (
                "WebSocket(Http): 301 redirect → terminal",
                ws(WsError::Http(Box::new(
                    http::Response::builder().status(301).body(None).unwrap(),
                ))),
                true,
            ),
            (
                "WebSocket(Http): 404 not found → terminal",
                ws(WsError::Http(Box::new(
                    http::Response::builder().status(404).body(None).unwrap(),
                ))),
                true,
            ),
            (
                "WebSocket(Http): 403 forbidden → terminal",
                ws(WsError::Http(Box::new(
                    http::Response::builder().status(403).body(None).unwrap(),
                ))),
                true,
            ),
            (
                "WebSocket(Http): 408 request timeout → transient",
                ws(WsError::Http(Box::new(
                    http::Response::builder().status(408).body(None).unwrap(),
                ))),
                false,
            ),
            (
                "WebSocket(Http): 429 too many requests → transient",
                ws(WsError::Http(Box::new(
                    http::Response::builder().status(429).body(None).unwrap(),
                ))),
                false,
            ),
            (
                "WebSocket(Http): 500 internal server error → transient",
                ws(WsError::Http(Box::new(
                    http::Response::builder().status(500).body(None).unwrap(),
                ))),
                false,
            ),
            (
                "WebSocket(Http): 502 bad gateway → transient",
                ws(WsError::Http(Box::new(
                    http::Response::builder().status(502).body(None).unwrap(),
                ))),
                false,
            ),
            (
                "WebSocket(Http): 503 service unavailable → transient",
                ws(WsError::Http(Box::new(
                    http::Response::builder().status(503).body(None).unwrap(),
                ))),
                false,
            ),
            // ── WebSocket inner: Protocol variants ──
            (
                "Protocol(WrongHttpMethod): deterministic upgrade mismatch",
                ws(WsError::Protocol(ProtocolError::WrongHttpMethod)),
                true,
            ),
            (
                "Protocol(WrongHttpVersion): deterministic upgrade mismatch",
                ws(WsError::Protocol(ProtocolError::WrongHttpVersion)),
                true,
            ),
            (
                "Protocol(MissingConnectionUpgradeHeader)",
                ws(WsError::Protocol(
                    ProtocolError::MissingConnectionUpgradeHeader,
                )),
                true,
            ),
            (
                "Protocol(MissingUpgradeWebSocketHeader)",
                ws(WsError::Protocol(
                    ProtocolError::MissingUpgradeWebSocketHeader,
                )),
                true,
            ),
            (
                "Protocol(MissingSecWebSocketVersionHeader)",
                ws(WsError::Protocol(
                    ProtocolError::MissingSecWebSocketVersionHeader,
                )),
                true,
            ),
            (
                "Protocol(MissingSecWebSocketKey)",
                ws(WsError::Protocol(ProtocolError::MissingSecWebSocketKey)),
                true,
            ),
            (
                "Protocol(SecWebSocketAcceptKeyMismatch)",
                ws(WsError::Protocol(
                    ProtocolError::SecWebSocketAcceptKeyMismatch,
                )),
                true,
            ),
            (
                "Protocol(SecWebSocketSubProtocolError)",
                ws(WsError::Protocol(
                    ProtocolError::SecWebSocketSubProtocolError(
                        SubProtocolError::ServerSentSubProtocolNoneRequested,
                    ),
                )),
                true,
            ),
            (
                "Protocol(JunkAfterRequest)",
                ws(WsError::Protocol(ProtocolError::JunkAfterRequest)),
                true,
            ),
            (
                "Protocol(CustomResponseSuccessful)",
                ws(WsError::Protocol(ProtocolError::CustomResponseSuccessful)),
                true,
            ),
            (
                "Protocol(InvalidHeader)",
                ws(WsError::Protocol(ProtocolError::InvalidHeader(Box::new(
                    http::header::UPGRADE,
                )))),
                true,
            ),
            (
                "Protocol(HttparseError)",
                ws(WsError::Protocol(ProtocolError::HttparseError(
                    httparse::Error::TooManyHeaders,
                ))),
                true,
            ),
            (
                "Protocol(SendAfterClosing)",
                ws(WsError::Protocol(ProtocolError::SendAfterClosing)),
                true,
            ),
            (
                "Protocol(ReceivedAfterClosing)",
                ws(WsError::Protocol(ProtocolError::ReceivedAfterClosing)),
                true,
            ),
            (
                "Protocol(NonZeroReservedBits)",
                ws(WsError::Protocol(ProtocolError::NonZeroReservedBits)),
                true,
            ),
            (
                "Protocol(UnmaskedFrameFromClient)",
                ws(WsError::Protocol(ProtocolError::UnmaskedFrameFromClient)),
                true,
            ),
            (
                "Protocol(MaskedFrameFromServer)",
                ws(WsError::Protocol(ProtocolError::MaskedFrameFromServer)),
                true,
            ),
            (
                "Protocol(FragmentedControlFrame)",
                ws(WsError::Protocol(ProtocolError::FragmentedControlFrame)),
                true,
            ),
            (
                "Protocol(ControlFrameTooBig)",
                ws(WsError::Protocol(ProtocolError::ControlFrameTooBig)),
                true,
            ),
            (
                "Protocol(UnknownControlFrameType)",
                ws(WsError::Protocol(ProtocolError::UnknownControlFrameType(
                    0xF,
                ))),
                true,
            ),
            (
                "Protocol(UnknownDataFrameType)",
                ws(WsError::Protocol(ProtocolError::UnknownDataFrameType(0xF))),
                true,
            ),
            (
                "Protocol(UnexpectedContinueFrame)",
                ws(WsError::Protocol(ProtocolError::UnexpectedContinueFrame)),
                true,
            ),
            (
                "Protocol(ExpectedFragment)",
                ws(WsError::Protocol(ProtocolError::ExpectedFragment(
                    tokio_tungstenite::tungstenite::protocol::frame::coding::Data::Text,
                ))),
                true,
            ),
            (
                "Protocol(InvalidOpcode)",
                ws(WsError::Protocol(ProtocolError::InvalidOpcode(0xF))),
                true,
            ),
            (
                "Protocol(InvalidCloseSequence)",
                ws(WsError::Protocol(ProtocolError::InvalidCloseSequence)),
                true,
            ),
            // ── Protocol: transient exceptions ──
            (
                "Protocol(HandshakeIncomplete): connection dropped mid-handshake",
                ws(WsError::Protocol(ProtocolError::HandshakeIncomplete)),
                false,
            ),
            (
                "Protocol(ResetWithoutClosingHandshake): abrupt reset",
                ws(WsError::Protocol(
                    ProtocolError::ResetWithoutClosingHandshake,
                )),
                false,
            ),
            // ── WebSocket(Io): transport (transient) ──
            (
                "Io(other): plain transport failure is transient",
                ws(WsError::Io(std::io::Error::other("reset"))),
                false,
            ),
            (
                "Io(ConnectionReset): transport reset is transient",
                ws(WsError::Io(std::io::ErrorKind::ConnectionReset.into())),
                false,
            ),
            (
                "Io(UnexpectedEof): transport EOF is transient",
                ws(WsError::Io(std::io::ErrorKind::UnexpectedEof.into())),
                false,
            ),
            (
                "Io(TimedOut): transport timeout is transient",
                ws(WsError::Io(std::io::ErrorKind::TimedOut.into())),
                false,
            ),
            // ── WebSocket(Io): rustls-sourced, variant-inspected ──
            // Production shape: tokio-rustls wraps rustls errors as
            // io::Error(InvalidData, rustls::Error). Only deterministic
            // cert/config/incompatibility variants are terminal.
            (
                "Io(rustls InvalidCertificate(Expired)): production-shaped expired cert is terminal",
                ws(WsError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    rustls::Error::InvalidCertificate(rustls::CertificateError::Expired),
                ))),
                true,
            ),
            (
                "Io(rustls InvalidCertificate(NotValidForName)): hostname mismatch is terminal",
                ws(WsError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    rustls::Error::InvalidCertificate(
                        rustls::CertificateError::NotValidForName,
                    ),
                ))),
                true,
            ),
            (
                "Io(rustls NoCertificatesPresented): missing cert is terminal",
                ws(WsError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    rustls::Error::NoCertificatesPresented,
                ))),
                true,
            ),
            // ── WebSocket(Io): rustls-sourced, ambiguous (transient) ──
            // Protocol, decrypt, alert, and general errors may be caused by
            // network conditions or transient server failures — retryable
            // under the bounded budget.
            (
                "Io(rustls General): ambiguous general error is transient",
                ws(WsError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    rustls::Error::General("protocol error".into()),
                ))),
                false,
            ),
            (
                "Io(rustls AlertReceived(InternalError)): server alert is transient",
                ws(WsError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    rustls::Error::AlertReceived(rustls::AlertDescription::InternalError),
                ))),
                false,
            ),
            (
                "Io(rustls DecryptError): corrupted record is transient",
                ws(WsError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    rustls::Error::DecryptError,
                ))),
                false,
            ),
            (
                "WebSocket(ConnectionClosed): link-level closure",
                ws(WsError::ConnectionClosed),
                false,
            ),
            // ── WebSocket(Tls): deterministic config (terminal, pins the arm) ──
            // These shapes are constructible but not reachable through our
            // rustls production connector — cert failures arrive as Io above.
            // Kept to pin the Tls(_) => true arm.
            (
                "Tls(Rustls(General)): pins Tls arm terminal",
                ws(WsError::Tls(
                    rustls::Error::General("tls handshake failed".into()).into(),
                )),
                true,
            ),
            (
                "Tls(InvalidDnsName): only reachable connect-time Tls variant",
                ws(WsError::Tls(TlsError::InvalidDnsName)),
                true,
            ),
            (
                "Tls(Rustls(InvalidCertificate(Expired))): pins Tls arm terminal",
                ws(WsError::Tls(
                    rustls::Error::InvalidCertificate(rustls::CertificateError::Expired).into(),
                )),
                true,
            ),
            (
                "WebSocket(AlreadyClosed): unreachable at connect, fail-safe transient",
                ws(WsError::AlreadyClosed),
                false,
            ),
            (
                "WebSocket(WriteBufferFull): unreachable at connect, fail-safe transient",
                ws(WsError::WriteBufferFull(Box::new(
                    tokio_tungstenite::tungstenite::Message::Text("x".into()),
                ))),
                false,
            ),
        ];

        for (label, err, want_terminal) in cases {
            assert_eq!(
                is_terminal_connect_error(&err),
                want_terminal,
                "{label}: expected terminal={want_terminal}"
            );
        }
    }

    /// A literal `https://…` URL through production `do_connect()` must fail
    /// fast as terminal — the relay endpoint is a plain HTTPS server, not a
    /// WebSocket endpoint, and tungstenite returns `Error::Http` (non-101
    /// response) or `Error::Url(UnsupportedUrlScheme)` depending on how far
    /// the handshake gets. Either way it must not be retried.
    #[tokio::test]
    async fn do_connect_wrong_scheme_is_terminal() {
        let keys = nostr::Keys::generate();
        let err = do_connect("https://example.com", &keys, None)
            .await
            .unwrap_err();
        assert!(
            is_terminal_connect_error(&err),
            "wrong-scheme URL should be terminal, got: {err}"
        );
    }

    /// A transient failure (e.g. connection dropped mid-handshake on a spotty
    /// link) must be retried and can still succeed once the link recovers.
    #[tokio::test(start_paused = true)]
    async fn retry_initial_connect_retries_transient_failure_then_succeeds() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let attempts = AtomicUsize::new(0);
        let result: Result<&'static str, RelayError> = retry_initial_connect(|| {
            let n = attempts.fetch_add(1, Ordering::SeqCst);
            async move {
                if n < 2 {
                    Err(RelayError::ConnectionClosed)
                } else {
                    Ok("connected")
                }
            }
        })
        .await;

        assert_eq!(result.unwrap(), "connected");
        assert_eq!(
            attempts.load(Ordering::SeqCst),
            3,
            "should succeed on the 3rd attempt (2 transient failures + 1 success)"
        );
    }

    /// A terminal error (bad auth, bad config) must not be retried — the
    /// same call would fail identically every time, so retrying just delays
    /// surfacing a real problem to the caller.
    #[tokio::test(start_paused = true)]
    async fn retry_initial_connect_does_not_retry_terminal_error() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let attempts = AtomicUsize::new(0);
        let result: Result<(), RelayError> = retry_initial_connect(|| {
            attempts.fetch_add(1, Ordering::SeqCst);
            async { Err(RelayError::AuthFailed("invalid: bad signature".into())) }
        })
        .await;

        assert!(matches!(result, Err(RelayError::AuthFailed(_))));
        assert_eq!(
            attempts.load(Ordering::SeqCst),
            1,
            "a terminal error must fail on the first attempt with no retries"
        );
    }

    /// A relay-side dependency fault (NIP-01 `error:` prefix) is transient —
    /// the relay is failing closed on itself, not rejecting this identity —
    /// so it must be retried rather than surfaced immediately like a real
    /// auth rejection.
    #[tokio::test(start_paused = true)]
    async fn retry_initial_connect_retries_relay_dependency_fault() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let attempts = AtomicUsize::new(0);
        let result: Result<&'static str, RelayError> = retry_initial_connect(|| {
            let n = attempts.fetch_add(1, Ordering::SeqCst);
            async move {
                if n < 1 {
                    Err(RelayError::AuthFailed(
                        "error: internal error checking restriction state".into(),
                    ))
                } else {
                    Ok("connected")
                }
            }
        })
        .await;

        assert_eq!(result.unwrap(), "connected");
        assert_eq!(
            attempts.load(Ordering::SeqCst),
            2,
            "a relay dependency fault must be retried, not surfaced immediately"
        );
    }

    /// Once every attempt (1 initial + N backoff retries) is exhausted, the
    /// last transient error is returned rather than retrying forever — a
    /// dead relay must not hang agent startup indefinitely.
    #[tokio::test(start_paused = true)]
    async fn retry_initial_connect_exhausts_and_returns_last_error() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let attempts = AtomicUsize::new(0);
        let result: Result<(), RelayError> = retry_initial_connect(|| {
            attempts.fetch_add(1, Ordering::SeqCst);
            async { Err(RelayError::Timeout) }
        })
        .await;

        assert!(
            matches!(result, Err(RelayError::Timeout)),
            "must surface the last attempt's error, not a generic one"
        );
        assert_eq!(
            attempts.load(Ordering::SeqCst),
            STARTUP_CONNECT_BACKOFFS.len() + 1,
            "must attempt exactly once plus one retry per backoff entry"
        );
    }

    /// Backoff sleeps must actually elapse (not be skipped) — this pins the
    /// bounded-but-real-delay contract using `tokio::time::pause` so the
    /// test itself stays fast (virtual time, not wall-clock sleeps).
    #[tokio::test(start_paused = true)]
    async fn retry_initial_connect_sleeps_between_attempts() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let attempts = AtomicUsize::new(0);
        let call = retry_initial_connect(|| {
            let n = attempts.fetch_add(1, Ordering::SeqCst);
            async move {
                if n < 1 {
                    Err(RelayError::ConnectionClosed)
                } else {
                    Ok(())
                }
            }
        });
        tokio::pin!(call);

        // Before the first backoff elapses, the retry must still be pending
        // (i.e. it actually slept rather than immediately retrying).
        tokio::select! {
            biased;
            _ = tokio::time::sleep(Duration::from_millis(1)) => {}
            _ = &mut call => panic!("must not resolve before the backoff sleep elapses"),
        }
        assert_eq!(attempts.load(Ordering::SeqCst), 1);

        // Advancing past the (jittered, ≤1.2x) first backoff lets it proceed.
        let result = call.await;
        assert!(result.is_ok());
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
    }

    // ── Rate-limit gate, pacing, backoff reset, DNS ──────────────────────────

    /// parse_rate_limit_retry_secs: full hint extracts the N from "retry in Ns".
    #[test]
    fn parse_rate_limit_retry_secs_with_hint() {
        assert_eq!(
            parse_rate_limit_retry_secs("rate-limited: quota exceeded; retry in 12s"),
            Some(12)
        );
    }

    /// parse_rate_limit_retry_secs: message without a hint returns None.
    #[test]
    fn parse_rate_limit_retry_secs_missing_hint() {
        assert_eq!(
            parse_rate_limit_retry_secs("rate-limited: too many concurrent requests"),
            None
        );
    }

    /// parse_rate_limit_retry_secs: explicit zero value is returned as Some(0).
    #[test]
    fn parse_rate_limit_retry_secs_zero() {
        assert_eq!(
            parse_rate_limit_retry_secs("rate-limited: quota exceeded; retry in 0s"),
            Some(0)
        );
    }

    /// parse_rate_limit_retry_secs: garbage input returns None.
    #[test]
    fn parse_rate_limit_retry_secs_garbage() {
        assert_eq!(
            parse_rate_limit_retry_secs("not a rate limit message"),
            None
        );
    }

    /// set_rate_limit_gate arms the gate with jittered expiry from the hint.
    /// check_rate_gate returns Some while active and lazily clears on expiry.
    #[tokio::test(start_paused = true)]
    async fn rate_limit_gate_set_and_expiry() {
        let mut state = BgState::new();
        assert!(
            state.check_rate_gate().is_none(),
            "gate must start inactive"
        );

        // Arm with a 5 s hint.
        state.set_rate_limit_gate(5);
        assert!(
            state.check_rate_gate().is_some(),
            "gate must be active immediately after arming"
        );

        // Advance virtual time past the max jitter (1.2 × 5 s = 6 s).
        tokio::time::advance(Duration::from_secs(7)).await;

        assert!(
            state.check_rate_gate().is_none(),
            "gate must have expired after 7s"
        );
        assert!(
            state.rate_limit_gate.is_none(),
            "check_rate_gate must lazily clear the field on expiry"
        );
    }

    /// set_rate_limit_gate takes the max of overlapping deadlines.
    #[tokio::test(start_paused = true)]
    async fn rate_limit_gate_extends_to_max() {
        let mut state = BgState::new();

        // Arm with a long hint first.
        state.set_rate_limit_gate(30);
        let first_deadline = state.rate_limit_gate.unwrap();

        // A shorter subsequent hint must NOT shorten the existing gate.
        state.set_rate_limit_gate(1);
        let second_deadline = state.rate_limit_gate.unwrap();

        assert_eq!(
            first_deadline, second_deadline,
            "shorter hint must not overwrite a later existing deadline"
        );
    }

    /// Build a signed observer telemetry frame (kind 24200) for gate tests.
    fn make_observer_frame(keys: &Keys) -> Event {
        let recipient = Keys::generate();
        let encrypted = buzz_core::observer::encrypt_observer_payload(
            keys,
            &recipient.public_key(),
            &json!({"type": "test"}),
        )
        .expect("encrypt test observer payload");
        buzz_sdk::build_agent_observer_frame(
            &recipient.public_key().to_hex(),
            &keys.public_key().to_hex(),
            "telemetry",
            &encrypted,
        )
        .expect("build test observer frame")
        .sign_with_keys(keys)
        .expect("sign test observer frame")
    }

    /// Build a signed typing indicator (kind 20002) scoped to a channel.
    fn make_typing_frame(keys: &Keys, channel_id: Uuid) -> Event {
        make_typing_frame_at(keys, channel_id, nostr::Timestamp::now())
    }

    /// A typing indicator carries no content, so two frames for one channel are
    /// the same event unless their timestamps differ — which in production they
    /// always do, one refresh cadence apart.
    fn make_typing_frame_at(keys: &Keys, channel_id: Uuid, created_at: nostr::Timestamp) -> Event {
        EventBuilder::new(Kind::Custom(KIND_TYPING_INDICATOR as u16), "")
            .tags([Tag::parse(["h", &channel_id.to_string()]).expect("h tag")])
            .custom_created_at(created_at)
            .sign_with_keys(keys)
            .expect("sign typing indicator")
    }

    /// Build a signed NIP-PA project activity frame (kind 20003) for a root.
    fn make_project_activity_frame(
        keys: &Keys,
        root: &str,
        activity: buzz_sdk::builders::ProjectActivityState,
        turn_id: &str,
    ) -> Event {
        let agent = keys.public_key().to_hex();
        let repo = buzz_sdk::GitRepoCoord {
            owner: agent.clone(),
            id: "repo".to_string(),
        };
        buzz_sdk::builders::build_project_activity(&repo, root, &agent, activity, turn_id, None)
            .expect("build project activity")
            .sign_with_keys(keys)
            .expect("sign project activity")
    }

    /// Publish one event through the connected command path.
    async fn publish_through_gate(
        client: &mut WsStream,
        state: &mut BgState,
        event: &Event,
    ) -> bool {
        execute_connected_command(
            client,
            state,
            "agent-pubkey",
            RelayCommand::PublishEvent {
                event: Box::new(event.clone()),
            },
        )
        .await
    }

    /// While the rate-limit gate is armed, an observer frame (kind 24200) is
    /// parked in the durable FIFO — not silently dropped — and delivered by the
    /// drain once the gate clears. A typing indicator in the same window is
    /// parked too, in the ephemeral map rather than the durable queue: the two
    /// buffers stay separate because their drain rules are different.
    #[tokio::test]
    async fn gated_observer_frame_is_parked_then_drained_not_dropped() {
        let (mut client, mut server) = test_ws_pair().await;
        let mut state = BgState::new();
        let keys = Keys::generate();
        state.rate_limit_gate = Some(tokio::time::Instant::now() + Duration::from_millis(150));

        // Observer frame while gated: parked, nothing on the wire.
        let observer_frame = make_observer_frame(&keys);
        let ok = execute_connected_command(
            &mut client,
            &mut state,
            "agent-pubkey",
            RelayCommand::PublishEvent {
                event: Box::new(observer_frame.clone()),
            },
        )
        .await;
        assert!(ok);
        assert_eq!(
            state.gated_observer_pending.len(),
            1,
            "observer frame must be parked while gated"
        );

        // Typing indicator while gated: parked as superseding ephemera, in its
        // own map — never in the durable queue.
        let typing = make_typing_frame(&keys, Uuid::new_v4());
        let ok = execute_connected_command(
            &mut client,
            &mut state,
            "agent-pubkey",
            RelayCommand::PublishEvent {
                event: Box::new(typing),
            },
        )
        .await;
        assert!(ok);
        assert_eq!(
            state.gated_observer_pending.len(),
            1,
            "typing indicators must not enter the durable queue"
        );
        assert_eq!(
            state.gated_ephemeral_pending.len(),
            1,
            "typing indicators must be parked, not dropped"
        );
        assert!(
            timeout(Duration::from_millis(50), server.next())
                .await
                .is_err(),
            "nothing may reach the wire while the gate is armed"
        );

        // Gate expires — the drain delivers the parked frame.
        tokio::time::sleep(Duration::from_millis(160)).await;
        assert_eq!(
            drain_gated_observer_pending(&mut client, &mut state, 1).await,
            1
        );
        assert!(state.gated_observer_pending.is_empty());
        let frame = next_test_frame(&mut server).await;
        assert_eq!(frame[0], "EVENT");
        assert_eq!(frame[1]["id"], observer_frame.id.to_hex());
        assert_eq!(
            frame[1]["kind"],
            u64::from(KIND_AGENT_OBSERVER_FRAME),
            "delivered frame must be the parked observer frame"
        );
    }

    /// Observer frames arriving while earlier parked frames are still queued
    /// are appended behind them (order preserved), even if the gate has
    /// already expired.
    #[tokio::test]
    async fn observer_frames_queue_behind_parked_backlog_in_order() {
        let (mut client, mut server) = test_ws_pair().await;
        let mut state = BgState::new();
        let keys = Keys::generate();
        state.rate_limit_gate = Some(tokio::time::Instant::now() + Duration::from_millis(50));

        let first = make_observer_frame(&keys);
        let second = make_observer_frame(&keys);
        for event in [&first, &second] {
            let ok = execute_connected_command(
                &mut client,
                &mut state,
                "agent-pubkey",
                RelayCommand::PublishEvent {
                    event: Box::new(event.clone()),
                },
            )
            .await;
            assert!(ok);
        }
        assert_eq!(state.gated_observer_pending.len(), 2);

        // Gate expires but the backlog is not drained yet — a third frame must
        // queue behind it rather than jumping ahead on the wire.
        tokio::time::sleep(Duration::from_millis(60)).await;
        let third = make_observer_frame(&keys);
        let ok = execute_connected_command(
            &mut client,
            &mut state,
            "agent-pubkey",
            RelayCommand::PublishEvent {
                event: Box::new(third.clone()),
            },
        )
        .await;
        assert!(ok);
        assert_eq!(
            state.gated_observer_pending.len(),
            3,
            "frame must queue behind undrained backlog to preserve order"
        );

        for expected in [&first, &second, &third] {
            assert_eq!(
                drain_gated_observer_pending(&mut client, &mut state, 1).await,
                1
            );
            let frame = next_test_frame(&mut server).await;
            assert_eq!(frame[1]["id"], expected.id.to_hex(), "order preserved");
        }
        assert!(state.gated_observer_pending.is_empty());
    }

    #[test]
    fn observer_notice_requeues_unacknowledged_frames_and_ok_retires_them() {
        let mut state = BgState::new();
        let keys = Keys::generate();
        let accepted = make_observer_frame(&keys);
        let rejected = make_observer_frame(&keys);
        let later = make_observer_frame(&keys);

        state.track_observer_in_flight(Box::new(accepted.clone()));
        state.track_observer_in_flight(Box::new(rejected.clone()));
        state.acknowledge_observer_frame(&accepted.id.to_hex());
        state.park_gated_observer_frame(Box::new(later.clone()));
        state.requeue_observer_in_flight();

        let ids: Vec<_> = state
            .gated_observer_pending
            .iter()
            .map(|event| event.id)
            .collect();
        assert_eq!(ids, [rejected.id, later.id]);
        assert!(state.observer_in_flight.is_empty());
    }

    /// The parked-frame queue is bounded: overflow evicts the oldest frame and
    /// counts it; the drain resets the counter after logging the summary.
    #[tokio::test]
    async fn gated_observer_queue_drops_oldest_on_overflow() {
        let mut state = BgState::new();
        let keys = Keys::generate();
        let first = make_observer_frame(&keys);
        state.park_gated_observer_frame(Box::new(first.clone()));
        for _ in 1..GATED_OBSERVER_QUEUE_CAP {
            state.park_gated_observer_frame(Box::new(make_observer_frame(&keys)));
        }
        assert_eq!(state.gated_observer_pending.len(), GATED_OBSERVER_QUEUE_CAP);
        assert_eq!(state.gated_observer_dropped, 0);

        let overflow = make_observer_frame(&keys);
        state.park_gated_observer_frame(Box::new(overflow.clone()));
        assert_eq!(
            state.gated_observer_pending.len(),
            GATED_OBSERVER_QUEUE_CAP,
            "queue must stay bounded"
        );
        assert_eq!(state.gated_observer_dropped, 1, "loss must be counted");
        assert!(
            !state
                .gated_observer_pending
                .iter()
                .any(|e| e.id == first.id),
            "oldest frame must be the one evicted"
        );
        assert_eq!(
            state.gated_observer_pending.back().map(|e| e.id),
            Some(overflow.id),
            "newest frame must be retained"
        );
    }

    /// A superseding kind parks one frame per scope: the newer typing frame for
    /// a channel replaces the parked one instead of queueing behind it, so what
    /// drains at the gate edge is at most one cadence old.
    #[tokio::test]
    async fn gated_ephemera_park_latest_per_scope() {
        let (mut client, mut server) = test_ws_pair().await;
        let mut state = BgState::new();
        let keys = Keys::generate();
        let channel = Uuid::new_v4();
        state.rate_limit_gate = Some(tokio::time::Instant::now() + Duration::from_millis(150));

        // One typing cadence apart, as the publisher emits them.
        let now = nostr::Timestamp::now();
        let stale = make_typing_frame_at(&keys, channel, now);
        let fresh = make_typing_frame_at(&keys, channel, now + 3u64);
        assert_ne!(stale.id, fresh.id, "test needs two distinct frames");
        assert!(publish_through_gate(&mut client, &mut state, &stale).await);
        assert!(publish_through_gate(&mut client, &mut state, &fresh).await);

        assert_eq!(
            state.gated_ephemeral_pending.len(),
            1,
            "one channel is one scope however many frames it publishes"
        );
        assert_eq!(
            state.gated_ephemeral_pending[0].1.id, fresh.id,
            "the newer frame must supersede the parked one"
        );
        assert_eq!(
            state.gated_ephemeral_dropped, 0,
            "superseding is the design, not a counted loss"
        );

        tokio::time::sleep(Duration::from_millis(160)).await;
        assert_eq!(
            drain_gated_ephemeral_pending(&mut client, &mut state, 4).await,
            1
        );
        let frame = next_test_frame(&mut server).await;
        assert_eq!(frame[1]["id"], fresh.id.to_hex());
        assert!(
            timeout(Duration::from_millis(50), server.next())
                .await
                .is_err(),
            "the superseded frame must never reach the wire"
        );
    }

    /// Distinct scopes are independent: two channels and two project roots park
    /// four frames, and a frame for one scope never displaces another's.
    #[tokio::test]
    async fn gated_ephemera_park_distinct_scopes_independently() {
        let (mut client, _server) = test_ws_pair().await;
        let mut state = BgState::new();
        let keys = Keys::generate();
        state.rate_limit_gate = Some(tokio::time::Instant::now() + Duration::from_secs(5));

        let channel_a = make_typing_frame(&keys, Uuid::new_v4());
        let channel_b = make_typing_frame(&keys, Uuid::new_v4());
        let root_a = make_project_activity_frame(
            &keys,
            &"a".repeat(64),
            buzz_sdk::builders::ProjectActivityState::Working,
            "turn-a",
        );
        let root_b = make_project_activity_frame(
            &keys,
            &"b".repeat(64),
            buzz_sdk::builders::ProjectActivityState::Working,
            "turn-b",
        );
        for event in [&channel_a, &channel_b, &root_a, &root_b] {
            assert!(publish_through_gate(&mut client, &mut state, event).await);
        }

        assert_eq!(
            state.gated_ephemeral_pending.len(),
            4,
            "two channels and two roots are four scopes"
        );
        let parked: Vec<_> = state
            .gated_ephemeral_pending
            .iter()
            .map(|(_, event)| event.id)
            .collect();
        for event in [&channel_a, &channel_b, &root_a, &root_b] {
            assert!(
                parked.contains(&event.id),
                "every scope keeps its own frame"
            );
        }
    }

    /// The bug this whole policy exists for: a NIP-PA `queued` frame published
    /// during a gate window reaches the wire when the gate clears, instead of
    /// being dropped as if it were a typing indicator.
    #[tokio::test]
    async fn gated_project_activity_reaches_the_wire_after_the_gate_clears() {
        let (mut client, mut server) = test_ws_pair().await;
        let mut state = BgState::new();
        let keys = Keys::generate();
        let root = "c".repeat(64);
        state.rate_limit_gate = Some(tokio::time::Instant::now() + Duration::from_millis(150));

        let queued = make_project_activity_frame(
            &keys,
            &root,
            buzz_sdk::builders::ProjectActivityState::Queued,
            "queued:abc",
        );
        assert!(publish_through_gate(&mut client, &mut state, &queued).await);
        assert_eq!(
            state.gated_ephemeral_pending.len(),
            1,
            "project activity must be parked, not dropped"
        );
        assert_eq!(
            state.gated_ephemeral_pending[0].0,
            EphemeralScope {
                kind: KIND_PROJECT_ACTIVITY,
                id: root.clone(),
            },
            "a project activity frame is scoped by its root `e` tag"
        );
        assert!(
            timeout(Duration::from_millis(50), server.next())
                .await
                .is_err(),
            "nothing may reach the wire while the gate is armed"
        );

        tokio::time::sleep(Duration::from_millis(160)).await;
        assert_eq!(
            drain_gated_ephemeral_pending(&mut client, &mut state, 1).await,
            1
        );
        let frame = next_test_frame(&mut server).await;
        assert_eq!(frame[0], "EVENT");
        assert_eq!(frame[1]["id"], queued.id.to_hex());
        assert_eq!(frame[1]["kind"], u64::from(KIND_PROJECT_ACTIVITY));
    }

    /// One pacing tick sends one frame, durable first. Mirrors the main-loop
    /// drain block's budget arithmetic so the priority is asserted where it is
    /// decided rather than by whichever drain the test calls first.
    #[tokio::test]
    async fn gated_drain_sends_durable_before_ephemera_one_per_tick() {
        let (mut client, mut server) = test_ws_pair().await;
        let mut state = BgState::new();
        let keys = Keys::generate();

        let telemetry = make_observer_frame(&keys);
        let typing = make_typing_frame(&keys, Uuid::new_v4());
        state.park_gated_observer_frame(Box::new(telemetry.clone()));
        state.park_gated_ephemeral_frame(
            EphemeralScope {
                kind: KIND_TYPING_INDICATOR,
                id: "channel".to_string(),
            },
            Box::new(typing.clone()),
        );

        for expected in [&telemetry, &typing] {
            let mut budget = DRAIN_BUDGET_PER_ITER;
            if budget > 0 && !state.gated_observer_pending.is_empty() {
                let sent = drain_gated_observer_pending(&mut client, &mut state, budget).await;
                budget = budget.saturating_sub(sent);
            }
            if budget > 0 && !state.gated_ephemeral_pending.is_empty() {
                drain_gated_ephemeral_pending(&mut client, &mut state, budget).await;
            }
            let frame = next_test_frame(&mut server).await;
            assert_eq!(
                frame[1]["id"],
                expected.id.to_hex(),
                "durable telemetry drains ahead of status frames, one per tick"
            );
        }
        assert!(state.gated_observer_pending.is_empty());
        assert!(state.gated_ephemeral_pending.is_empty());
    }

    /// The ephemeral map is bounded by scopes: overflow evicts the
    /// least-recently-refreshed scope and counts it, and the drain reports the
    /// total once the map empties.
    #[tokio::test]
    async fn gated_ephemeral_map_drops_least_recently_refreshed_scope() {
        let (mut client, mut server) = test_ws_pair().await;
        let mut state = BgState::new();
        let keys = Keys::generate();

        let first_channel = Uuid::new_v4();
        let first = make_typing_frame(&keys, first_channel);
        state.park_gated_ephemeral_frame(
            EphemeralScope {
                kind: KIND_TYPING_INDICATOR,
                id: first_channel.to_string(),
            },
            Box::new(first.clone()),
        );
        for _ in 1..GATED_EPHEMERAL_SCOPE_CAP {
            let channel = Uuid::new_v4();
            state.park_gated_ephemeral_frame(
                EphemeralScope {
                    kind: KIND_TYPING_INDICATOR,
                    id: channel.to_string(),
                },
                Box::new(make_typing_frame(&keys, channel)),
            );
        }
        assert_eq!(
            state.gated_ephemeral_pending.len(),
            GATED_EPHEMERAL_SCOPE_CAP
        );
        assert_eq!(state.gated_ephemeral_dropped, 0);

        // Refreshing the oldest scope moves it out of the eviction slot: the
        // scope that has gone quiet is the one that should be shed.
        let refreshed = make_typing_frame(&keys, first_channel);
        state.park_gated_ephemeral_frame(
            EphemeralScope {
                kind: KIND_TYPING_INDICATOR,
                id: first_channel.to_string(),
            },
            Box::new(refreshed.clone()),
        );
        assert_eq!(
            state.gated_ephemeral_pending.len(),
            GATED_EPHEMERAL_SCOPE_CAP,
            "a refresh replaces in place, it does not grow the map"
        );
        assert_eq!(state.gated_ephemeral_dropped, 0, "a refresh is not a loss");

        let overflow_channel = Uuid::new_v4();
        state.park_gated_ephemeral_frame(
            EphemeralScope {
                kind: KIND_TYPING_INDICATOR,
                id: overflow_channel.to_string(),
            },
            Box::new(make_typing_frame(&keys, overflow_channel)),
        );
        assert_eq!(
            state.gated_ephemeral_pending.len(),
            GATED_EPHEMERAL_SCOPE_CAP,
            "map must stay bounded"
        );
        assert_eq!(state.gated_ephemeral_dropped, 1, "loss must be counted");
        assert!(
            state
                .gated_ephemeral_pending
                .iter()
                .any(|(_, event)| event.id == refreshed.id),
            "the refreshed scope must survive: eviction takes the quietest"
        );

        // Draining the map empties the accounting after reporting it.
        let mut drained = 0;
        while !state.gated_ephemeral_pending.is_empty() {
            drained += drain_gated_ephemeral_pending(&mut client, &mut state, 8).await;
            while timeout(Duration::from_millis(20), server.next())
                .await
                .is_ok()
            {}
        }
        assert_eq!(drained, GATED_EPHEMERAL_SCOPE_CAP);
        assert_eq!(
            state.gated_ephemeral_dropped, 0,
            "the counter resets after the summary is logged"
        );
    }

    /// Gate expiry mid-park does not let a live publish overtake what is still
    /// parked for the same scope — but a scope with nothing parked publishes
    /// straight to the wire.
    #[tokio::test]
    async fn ephemeral_publish_after_gate_expiry_supersedes_its_own_parked_scope() {
        let (mut client, mut server) = test_ws_pair().await;
        let mut state = BgState::new();
        let keys = Keys::generate();
        let root = "d".repeat(64);
        state.rate_limit_gate = Some(tokio::time::Instant::now() + Duration::from_millis(50));

        let working = make_project_activity_frame(
            &keys,
            &root,
            buzz_sdk::builders::ProjectActivityState::Working,
            "turn-1",
        );
        assert!(publish_through_gate(&mut client, &mut state, &working).await);
        assert_eq!(state.gated_ephemeral_pending.len(), 1);

        // Gate expires; the parked backlog has not drained yet.
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert!(state.check_rate_gate().is_none(), "gate must have expired");

        let idle = make_project_activity_frame(
            &keys,
            &root,
            buzz_sdk::builders::ProjectActivityState::Idle,
            "turn-1",
        );
        assert!(publish_through_gate(&mut client, &mut state, &idle).await);
        assert_eq!(
            state.gated_ephemeral_pending.len(),
            1,
            "the live frame must supersede the parked one, not overtake it"
        );
        assert_eq!(
            state.gated_ephemeral_pending[0].1.id, idle.id,
            "the root must end up announcing idle, never working again"
        );

        // A different scope has nothing parked, so it goes live immediately.
        let other = make_typing_frame(&keys, Uuid::new_v4());
        assert!(publish_through_gate(&mut client, &mut state, &other).await);
        let frame = next_test_frame(&mut server).await;
        assert_eq!(frame[1]["id"], other.id.to_hex());

        assert_eq!(
            drain_gated_ephemeral_pending(&mut client, &mut state, 4).await,
            1
        );
        let frame = next_test_frame(&mut server).await;
        assert_eq!(frame[1]["id"], idle.id.to_hex());
    }

    /// Parked status frames do not outlive their socket; parked durable frames
    /// do. Both reconnect entry points call this before backing off.
    #[test]
    fn parked_ephemera_are_discarded_on_socket_loss_and_durables_are_not() {
        let mut state = BgState::new();
        let keys = Keys::generate();
        let telemetry = make_observer_frame(&keys);
        let channel = Uuid::new_v4();

        state.park_gated_observer_frame(Box::new(telemetry.clone()));
        state.park_gated_ephemeral_frame(
            EphemeralScope {
                kind: KIND_TYPING_INDICATOR,
                id: channel.to_string(),
            },
            Box::new(make_typing_frame(&keys, channel)),
        );

        state.discard_gated_ephemera("test socket loss");

        assert!(
            state.gated_ephemeral_pending.is_empty(),
            "a status frame cannot be re-stated on a socket that did not carry it"
        );
        assert_eq!(
            state.gated_observer_pending.len(),
            1,
            "durable telemetry must survive the reconnect"
        );

        // While disconnected the same split holds: durable parks, status drops.
        apply_command_to_state(
            &mut state,
            RelayCommand::PublishEvent {
                event: Box::new(make_typing_frame(&keys, channel)),
            },
        );
        apply_command_to_state(
            &mut state,
            RelayCommand::PublishEvent {
                event: Box::new(make_observer_frame(&keys)),
            },
        );
        assert!(state.gated_ephemeral_pending.is_empty());
        assert_eq!(state.gated_observer_pending.len(), 2);
    }

    /// Every tenant of the publish path gets its policy from the table, not
    /// from a kind check at the call site — including the two that arrived
    /// after the invariant this replaced was written.
    #[test]
    fn gated_publish_policy_covers_every_tenant_of_the_publish_path() {
        let keys = Keys::generate();
        let channel = Uuid::new_v4();
        let root = "e".repeat(64);

        assert!(matches!(
            gated_publish_policy(&make_observer_frame(&keys)),
            GatedPublish::Durable
        ));
        assert!(
            matches!(
                gated_publish_policy(&make_typing_frame(&keys, channel)),
                GatedPublish::Superseding(scope) if scope == EphemeralScope {
                    kind: KIND_TYPING_INDICATOR,
                    id: channel.to_string(),
                }
            ),
            "a typing indicator is scoped by its channel"
        );
        assert!(
            matches!(
                gated_publish_policy(&make_project_activity_frame(
                    &keys,
                    &root,
                    buzz_sdk::builders::ProjectActivityState::Working,
                    "turn-1",
                )),
                GatedPublish::Superseding(scope) if scope == EphemeralScope {
                    kind: KIND_PROJECT_ACTIVITY,
                    id: root.clone(),
                }
            ),
            "project activity is scoped by its root, not by a channel it does not have"
        );

        // Presence (20001): one agent-wide scope, so the newest wins outright.
        let presence = EventBuilder::new(Kind::Custom(KIND_PRESENCE_UPDATE as u16), "online")
            .tags([])
            .sign_with_keys(&keys)
            .expect("sign presence");
        assert!(matches!(
            gated_publish_policy(&presence),
            GatedPublish::Superseding(scope) if scope == EphemeralScope {
                kind: KIND_PRESENCE_UPDATE,
                id: String::new(),
            }
        ));

        // The setup-mode nudge (kind 9) is a real message to a person: durable,
        // and the case the replaced INVARIANT comment warned about.
        let nudge = buzz_sdk::build_message(channel, "setup nudge", None, &[], false, &[])
            .expect("build nudge")
            .sign_with_keys(&keys)
            .expect("sign nudge");
        assert!(matches!(
            gated_publish_policy(&nudge),
            GatedPublish::Durable
        ));

        // An unclassified kind is parked, loudly, rather than dropped.
        let unknown = EventBuilder::new(Kind::Custom(31337), "")
            .tags([])
            .sign_with_keys(&keys)
            .expect("sign unknown kind");
        assert!(matches!(
            gated_publish_policy(&unknown),
            GatedPublish::Durable
        ));
    }

    /// A durable non-telemetry frame published while gated is parked and
    /// delivered — but it never joins the observer acknowledgment window, which
    /// exists only to re-send unacknowledged telemetry after a NOTICE.
    #[tokio::test]
    async fn gated_durable_message_is_parked_and_drained_without_in_flight_tracking() {
        let (mut client, mut server) = test_ws_pair().await;
        let mut state = BgState::new();
        let keys = Keys::generate();
        state.rate_limit_gate = Some(tokio::time::Instant::now() + Duration::from_millis(120));

        let nudge = buzz_sdk::build_message(Uuid::new_v4(), "setup nudge", None, &[], false, &[])
            .expect("build nudge")
            .sign_with_keys(&keys)
            .expect("sign nudge");
        assert!(publish_through_gate(&mut client, &mut state, &nudge).await);
        assert_eq!(
            state.gated_observer_pending.len(),
            1,
            "a message must never be dropped by the gate"
        );

        tokio::time::sleep(Duration::from_millis(130)).await;
        assert_eq!(
            drain_gated_observer_pending(&mut client, &mut state, 1).await,
            1
        );
        let frame = next_test_frame(&mut server).await;
        assert_eq!(frame[1]["id"], nudge.id.to_hex());
        assert!(
            state.observer_in_flight.is_empty(),
            "only telemetry enters the acknowledgment window"
        );
    }

    /// is_dns_error correctly classifies platform resolver strings, including
    /// the production shape: a WebSocket I/O error wrapping the OS message.
    #[test]
    fn is_dns_error_classification() {
        use tokio_tungstenite::tungstenite;

        // macOS resolver (Http-wrapped, used in many existing tests)
        assert!(is_dns_error(&RelayError::Http(
            "nodename nor servname provided, or not known".into()
        )));
        // Linux resolver
        assert!(is_dns_error(&RelayError::Http(
            "Name or service not known".into()
        )));
        // BSD/Windows
        assert!(is_dns_error(&RelayError::Http("No such host".into())));
        // Another common variant
        assert!(is_dns_error(&RelayError::Http(
            "failed to lookup address information".into()
        )));
        // F15: production-shaped error — RelayError::WebSocket wrapping a
        // tungstenite I/O error (the shape emitted by connect_async on macOS).
        let ws_io_err = RelayError::WebSocket(Box::new(tungstenite::Error::Io(
            std::io::Error::other("nodename nor servname provided, or not known"),
        )));
        assert!(
            is_dns_error(&ws_io_err),
            "WebSocket-wrapped I/O DNS error must be classified as DNS"
        );
        // Normal connection errors are NOT DNS errors.
        assert!(!is_dns_error(&RelayError::Timeout));
        assert!(!is_dns_error(&RelayError::ConnectionClosed));
        assert!(!is_dns_error(&RelayError::Http(
            "connection refused".into()
        )));
    }

    /// resubscribe_retry is populated when a channel REQ fails during partial reconnect.
    ///
    /// This exercises BgState directly since we have no live socket in unit tests.
    #[test]
    fn resubscribe_retry_populated_on_failure() {
        let mut state = BgState::new();
        let channel_id = Uuid::new_v4();

        // Subscribe the channel so it ends up in active_subscriptions.
        apply_command_to_state(
            &mut state,
            RelayCommand::Subscribe {
                channel_id,
                filter: ChannelFilter {
                    kinds: Some(vec![9]),
                    require_mention: false,
                },
                replay_since: Some(1_000),
            },
        );
        assert!(state.active_subscriptions.contains_key(&channel_id));

        // Simulate a partial-reconnect failure: insert into resubscribe_retry.
        state.resubscribe_retry.insert(channel_id);

        assert!(
            state.resubscribe_retry.contains(&channel_id),
            "failed channel must be in resubscribe_retry"
        );
        assert!(
            state.active_subscriptions.contains_key(&channel_id),
            "channel must stay in active_subscriptions so reconnect can restore it"
        );
    }

    // ── Control-sub recovery from rate-limited CLOSED ────────────────────────

    /// A rate-limited CLOSED for the membership sub sets membership_resub_needed.
    /// After the gate expires the drain re-arms the sub and clears the flag.
    #[tokio::test(start_paused = true)]
    async fn membership_resub_flag_set_on_rate_limited_closed() {
        let mut state = BgState::new();
        state.membership_sub_active = true;

        // Simulate a rate-limited CLOSED arriving for the membership sub.
        let secs = parse_rate_limit_retry_secs("rate-limited: retry in 5s").unwrap_or(0);
        state.set_rate_limit_gate(secs);
        state.membership_resub_needed = true;

        assert!(
            state.membership_resub_needed,
            "flag must be set after rate-limited CLOSED"
        );
        assert!(
            state.check_rate_gate().is_some(),
            "gate must be active while membership sub is pending"
        );

        // Advance past the gate (max jitter: 1.2 × 5s = 6s).
        tokio::time::advance(Duration::from_secs(7)).await;

        assert!(
            state.check_rate_gate().is_none(),
            "gate must expire so drain can fire"
        );
        // The drain clears membership_resub_needed after re-sending the REQ.
        // Simulate successful re-send:
        state.membership_resub_needed = false;
        assert!(
            !state.membership_resub_needed,
            "flag must clear after drain re-sends the membership REQ"
        );
    }

    /// A rate-limited CLOSED for the observer control sub sets observer_resub_needed.
    #[test]
    fn observer_resub_flag_set_on_rate_limited_closed() {
        let mut state = BgState::new();
        state.observer_control_sub_active = true;

        // Simulate rate-limited CLOSED on observer control sub.
        state.set_rate_limit_gate(5);
        state.observer_resub_needed = true;

        assert!(
            state.observer_resub_needed,
            "flag must be set after rate-limited CLOSED on observer sub"
        );
    }

    // ── Drain state transitions ───────────────────────────────────────────────

    /// drain_rate_limited_pending: a channel re-queued with +5s penalty on send
    /// failure stays in pending and is not immediately retried.
    #[tokio::test(start_paused = true)]
    async fn rate_limited_pending_failure_requeues_with_penalty() {
        let mut state = BgState::new();
        let channel_id = Uuid::new_v4();

        // Seed the channel's subscription intent.
        apply_command_to_state(
            &mut state,
            RelayCommand::Subscribe {
                channel_id,
                filter: ChannelFilter {
                    kinds: Some(vec![9]),
                    require_mention: false,
                },
                replay_since: None,
            },
        );

        // Park the channel as rate-limited with a deadline in the past.
        let past = tokio::time::Instant::now();
        state.rate_limited_pending.insert(channel_id, past);

        // Simulate a send failure by re-queuing with +5s (what the drain does).
        let penalty = tokio::time::Instant::now() + Duration::from_secs(5);
        state.rate_limited_pending.insert(channel_id, penalty);

        assert!(
            state.rate_limited_pending.contains_key(&channel_id),
            "channel must stay in rate_limited_pending after send failure"
        );
        // Deadline should be in the future.
        let deadline = state.rate_limited_pending[&channel_id];
        assert!(
            deadline > tokio::time::Instant::now(),
            "penalty deadline must be in the future"
        );
    }

    /// drain_resubscribe_retry: a gate re-armed mid-drain moves the channel to
    /// rate_limited_pending and removes it from resubscribe_retry.
    #[tokio::test(start_paused = true)]
    async fn resubscribe_retry_gate_rearm_moves_to_pending() {
        let mut state = BgState::new();
        let channel_id = Uuid::new_v4();

        apply_command_to_state(
            &mut state,
            RelayCommand::Subscribe {
                channel_id,
                filter: ChannelFilter {
                    kinds: Some(vec![9]),
                    require_mention: false,
                },
                replay_since: None,
            },
        );
        state.resubscribe_retry.insert(channel_id);

        // Simulate gate re-arming mid-drain (what the drain does on check_rate_gate hit).
        let retry_after = state.set_rate_limit_gate(5);
        state.rate_limited_pending.insert(channel_id, retry_after);
        state.resubscribe_retry.remove(&channel_id);

        assert!(
            !state.resubscribe_retry.contains(&channel_id),
            "channel must be removed from resubscribe_retry when gate re-arms"
        );
        assert!(
            state.rate_limited_pending.contains_key(&channel_id),
            "channel must be moved to rate_limited_pending on gate re-arm"
        );
    }

    /// drain_resubscribe_retry: a successful drain removes the channel and
    /// clears channel_dropped_since.
    #[test]
    fn resubscribe_retry_success_clears_state() {
        let mut state = BgState::new();
        let channel_id = Uuid::new_v4();

        apply_command_to_state(
            &mut state,
            RelayCommand::Subscribe {
                channel_id,
                filter: ChannelFilter {
                    kinds: Some(vec![9]),
                    require_mention: false,
                },
                replay_since: None,
            },
        );
        state.resubscribe_retry.insert(channel_id);
        state.channel_dropped_since.insert(channel_id, 1_000_000);

        // Simulate successful re-send (what the drain does on success).
        state.resubscribe_retry.remove(&channel_id);
        state.channel_dropped_since.remove(&channel_id);

        assert!(
            !state.resubscribe_retry.contains(&channel_id),
            "channel must leave resubscribe_retry on successful drain"
        );
        assert!(
            !state.channel_dropped_since.contains_key(&channel_id),
            "channel_dropped_since must be cleared on successful drain"
        );
    }
}
