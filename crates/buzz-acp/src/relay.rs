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

use std::collections::{HashMap, HashSet, VecDeque};
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
const SEEN_ID_LIMIT: usize = 12_000;

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
/// (or the socket is down). The upstream pacer feeds at most ~6 frames/s, so
/// this covers ~40 s of gating; beyond that the oldest frames are dropped with
/// visible accounting (`gated_observer_dropped`).
const GATED_OBSERVER_QUEUE_CAP: usize = 256;

use std::time::Instant;

use buzz_core::kind::{
    KIND_AGENT_OBSERVER_FRAME, KIND_MEMBER_ADDED_NOTIFICATION, KIND_MEMBER_REMOVED_NOTIFICATION,
    KIND_TYPING_INDICATOR,
};
use futures_util::{SinkExt, StreamExt};
use nostr::{Event, EventBuilder, Keys, Kind, RelayUrl, Tag};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio::time::timeout;
use tokio_tungstenite::{connect_async, tungstenite::Message, MaybeTlsStream, WebSocketStream};
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use crate::config::ChannelFilter;

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
    /// Publish a signed event to the relay (for typing indicators, etc.).
    PublishEvent { event: Box<Event> },
    /// Floor `since` for membership notification replay; events before startup are never re-delivered.
    SetStartupWatermark { ts: u64 },
    /// Open a project REQ under `sub_id`, registering it in lockstep.
    ///
    /// `filters` is the REQ's whole filter list, ORed, in wire order. Empty
    /// opens nothing — see [`HarnessRelay::subscribe_project`].
    SubscribeProject {
        sub_id: String,
        subscription: crate::project::ProjectSubscription,
        filters: Vec<Value>,
    },
    /// Replace a live project subscription, transactionally.
    ///
    /// Distinct from [`RelayCommand::SubscribeProject`] because the registry
    /// distinguishes them: opening refuses to change the identity held under an
    /// id, and replacement is the operation permitted to. Folding them into one
    /// command would put that decision at the call site rather than in the
    /// registry that owns it.
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

    /// Open a project REQ under `sub_id` carrying `filters`, ORed.
    ///
    /// The class recorded here is what every inbound frame on this id will be
    /// classified as — the id's spelling carries no authority. Registration
    /// happens in lockstep with the write, so a failed send leaves nothing
    /// answerable.
    ///
    /// A `Vec` because a NIP-01 REQ carries one *or more* filters and this
    /// crate's own watched-root builder returns two — a lowercase `#e` branch
    /// for comments and an uppercase `#E` branch for pull-request revisions.
    /// An empty vector opens nothing: `["REQ", id]` is an unbounded request,
    /// not an empty one, so a builder that produced no filters must produce no
    /// REQ.
    pub async fn subscribe_project(
        &self,
        sub_id: &str,
        subscription: crate::project::ProjectSubscription,
        filters: Vec<Value>,
    ) -> Result<(), RelayError> {
        self.cmd_tx
            .send(RelayCommand::SubscribeProject {
                sub_id: sub_id.to_string(),
                subscription,
                filters,
            })
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
    /// [`Self::subscribe_project`] with different arguments.
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

/// What [`send_project_subscribe`] did.
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
struct TwoGenDedup {
    current: HashSet<String>,
    previous: HashSet<String>,
    limit: usize,
}

impl TwoGenDedup {
    fn new(limit: usize) -> Self {
        Self {
            current: HashSet::new(),
            previous: HashSet::new(),
            limit,
        }
    }

    fn contains(&self, id: &str) -> bool {
        self.current.contains(id) || self.previous.contains(id)
    }

    /// Insert `id`. Returns `true` if it was new (not a duplicate).
    fn insert(&mut self, id: String) -> bool {
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
    /// Observer telemetry frames (kind 24200) parked while the rate-limit gate
    /// is armed. Unlike typing indicators, these frames are durable telemetry:
    /// dropping them silently loses turn history in the Desktop observer.
    /// Bounded at `GATED_OBSERVER_QUEUE_CAP` (drop-oldest); drained by the
    /// main loop one frame per pacing tick once the gate clears.
    gated_observer_pending: VecDeque<Box<Event>>,
    /// Observer frames written to the socket but not yet acknowledged. The
    /// relay's rate-limit NOTICE does not carry an event ID, so all unresolved
    /// observer writes are moved back ahead of the parked FIFO when one arrives.
    observer_in_flight: VecDeque<Box<Event>>,
    /// Frames evicted from the bounded pending/in-flight observer buffers since
    /// summary log. Makes overflow loss visible instead of silent.
    gated_observer_dropped: u64,
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
            channel_dropped_since: HashMap::new(),
            proactive_resubscribe_needed: false,
            startup_watermark: None,
            subscribe_since: HashMap::new(),
            rate_limit_gate: None,
            rate_limited_pending: HashMap::new(),
            membership_resub_needed: false,
            observer_resub_needed: false,
            gated_observer_pending: VecDeque::new(),
            observer_in_flight: VecDeque::new(),
            gated_observer_dropped: 0,
            resubscribe_retry: HashSet::new(),
            project_dropped_since: None,
            project_seen_ids: TwoGenDedup::new(SEEN_ID_LIMIT),
            project_requests: crate::project::ProjectRequests::new(),
            reconstructions: crate::project::ProjectReconstructions::new(),
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

    /// Park an observer telemetry frame while the rate-limit gate is armed.
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
        RelayCommand::SubscribeProject {
            sub_id,
            subscription,
            filters,
        } => {
            // Offline: record the intent only — nothing becomes answerable
            // until a REQ is actually written for it. Fail-closed all the same,
            // because a conflicting command accepted while disconnected would
            // be opened verbatim by the next connection's replay.
            let Some(identity) =
                crate::project::ProjectRequestIdentity::from_filters(subscription, filters)
            else {
                // Recording this as intent would replay a filterless REQ onto
                // the next connection, which asks the relay for everything.
                warn!(sub_id, "refusing a project subscription with no filters");
                return;
            };
            if let crate::project::IntentAdmission::Conflict { held } =
                state.project_requests.record_intent(&sub_id, identity)
            {
                warn!(
                    sub_id,
                    ?held,
                    "refusing conflicting project intent while disconnected — keeping the original"
                );
            }
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
                crate::project::ReplaceOutcome::Refused => {
                    warn!(?replacement, "refusing an unbounded project replacement");
                }
                crate::project::ReplaceOutcome::Exhausted => {
                    error!(
                        ?replacement,
                        "project generations exhausted while disconnected — no further \
                         replacement can be recorded"
                    );
                }
                _ => {}
            }
        }
        RelayCommand::SubscribeMembership => {
            state.membership_sub_active = true;
        }
        RelayCommand::SubscribeObserverControls => {
            state.observer_control_sub_active = true;
        }
        RelayCommand::SetStartupWatermark { ts } => {
            state.startup_watermark = Some(ts);
            if state.membership_last_seen.is_none() {
                state.membership_last_seen = Some(ts);
            }
        }
        // Observer telemetry frames are durable: park them (bounded, visible
        // overflow) so they are delivered by the post-reconnect drain. Other
        // ephemeral publishes (typing indicators) are meaningless while
        // disconnected and are dropped.
        RelayCommand::PublishEvent { event } => {
            if event.kind.as_u16() as u32 == KIND_AGENT_OBSERVER_FRAME {
                state.park_gated_observer_frame(event);
            }
        }
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
/// Subscription state must survive reconnect. Observer telemetry publishes are
/// parked for post-reconnect drain; other ephemeral publishes are deliberately
/// discarded because replaying a typing indicator after reconnect is meaningless.
/// `Shutdown` and `Reconnect` are handled by the caller.
fn retain_failed_command_intent(state: &mut BgState, cmd: RelayCommand) {
    match cmd {
        RelayCommand::PublishEvent { event }
            if event.kind.as_u16() as u32 == KIND_AGENT_OBSERVER_FRAME =>
        {
            state.park_gated_observer_frame(event);
        }
        RelayCommand::PublishEvent { .. } => {}
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
        RelayCommand::SubscribeProject {
            sub_id,
            subscription,
            filters,
        } => {
            // Intent and registration are decided together, in one operation
            // that either fully succeeds or records nothing. Admitting intent
            // first and consulting the registry second left a refused identity
            // sitting in intent, and the next reconnect installed it.
            let Some(identity) =
                crate::project::ProjectRequestIdentity::from_filters(subscription, filters)
            else {
                // Nothing written, nothing registered, and the socket is fine —
                // a filterless REQ asks the relay for everything.
                warn!(sub_id, "refusing a project subscription with no filters");
                return true;
            };
            match send_project_subscribe(ws, state, &sub_id, identity).await {
                ProjectSendOutcome::Sent | ProjectSendOutcome::AlreadyOpen => true,
                // The socket is fine; our own bookkeeping disagreed. Tearing
                // the connection down here is what let a refusal be replayed
                // into effect on the next one.
                ProjectSendOutcome::MetadataConflict => true,
                // Terminal, but not a transport failure — the socket stays.
                ProjectSendOutcome::Exhausted => true,
                ProjectSendOutcome::WriteFailed => false,
            }
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
                crate::project::ReplaceOutcome::Refused => {
                    warn!(?replacement, "refusing an unbounded project replacement");
                    true
                }
                crate::project::ReplaceOutcome::Exhausted => {
                    error!(
                        ?replacement,
                        "project generations exhausted — no further project subscription \
                         can be opened or replaced"
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
        RelayCommand::PublishEvent { event } => {
            // Observer telemetry frames (kind 24200) are durable telemetry, not
            // droppable ephemera: park them while the rate-limit gate is armed —
            // and while earlier parked frames are still draining, so relative
            // order is preserved — then let the main-loop drain deliver them
            // one per pacing tick once the gate clears.
            if event.kind.as_u16() as u32 == KIND_AGENT_OBSERVER_FRAME
                && (state.check_rate_gate().is_some() || !state.gated_observer_pending.is_empty())
            {
                debug!(
                    pending = state.gated_observer_pending.len(),
                    "rate-gated: parking observer frame for paced drain"
                );
                state.park_gated_observer_frame(event);
                return true;
            }
            // Drop remaining ephemeral publishes while rate-gated. Stale typing
            // indicators are worthless and sending them would consume admission
            // budget the relay already rejected us on.
            //
            // INVARIANT: apart from observer frames (parked above), the WS publish
            // path carries only ephemeral kinds (typing indicators). The silent
            // drop-while-gated relies on that invariant. If a future caller
            // publishes durable events through this path, it must extend the
            // kind guard above to avoid silently discarding user data.
            if state.check_rate_gate().is_some() {
                debug!("rate-gated: dropping ephemeral PublishEvent (typing indicator)");
                return true;
            }
            // Best-effort: log a send failure but don't trigger reconnect — the
            // next ping or read will detect the dead socket. A failed observer
            // frame is parked so the post-reconnect drain redelivers it.
            let is_observer = event.kind.as_u16() as u32 == KIND_AGENT_OBSERVER_FRAME;
            if send_publish_event_frame(ws, &event).await {
                if is_observer {
                    state.track_observer_in_flight(event);
                }
            } else if is_observer {
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
    initial_handshake_buffer: std::collections::VecDeque<RelayMessage>,
    event_tx: mpsc::Sender<Option<BuzzEvent>>,
    observer_control_tx: mpsc::Sender<Event>,
    mut cmd_rx: mpsc::Receiver<RelayCommand>,
    keys: Keys,
    relay_url: String,
    agent_pubkey_hex: String,
    auth_tag: Option<nostr::Tag>,
) {
    let mut state = BgState::new();

    let handshake_ok = process_handshake_buffer(
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

    loop {
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

            if budget > 0 && !state.gated_observer_pending.is_empty() {
                let sent = drain_gated_observer_pending(&mut ws, &mut state, budget).await;
                if sent > 0 {
                    any_sent = true;
                }
            }

            if any_sent {
                drain_pacing_next = Some(tokio::time::Instant::now() + REQ_PACING_INTERVAL);
            } else if !state.gated_observer_pending.is_empty() {
                // Nothing sent because the gate is still armed. Arm the pacing
                // timer to the gate deadline so parked observer frames drain
                // promptly even when no other traffic wakes the select loop.
                drain_pacing_next = state
                    .check_rate_gate()
                    .or_else(|| Some(tokio::time::Instant::now() + REQ_PACING_INTERVAL));
            }
        }

        tokio::select! {
                   raw = ws.next() => {
                       // Determine if the socket is lost.
                       let socket_lost = match raw {
                           Some(Ok(msg)) => {
                               if matches!(msg, Message::Pong(_)) {
                                   last_pong = Instant::now();
                                   ping_sent = false;
                                   false // pong is healthy — not a socket loss
                               } else {
                                   !handle_ws_message(
                                       msg,
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
                               }
                           }
                           Some(Err(e)) => {
                               warn!("WebSocket error in background task: {e}");
                               true
                           }
                           None => {
                               debug!("WebSocket stream ended");
                               true
                           }
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

/// Handle a single WebSocket message in the background task.
///
/// Returns `false` if the connection has been lost (Close frame or unrecoverable
/// error), `true` otherwise.
#[allow(clippy::too_many_arguments)]
async fn handle_ws_message(
    msg: Message,
    ws: &mut WsStream,
    event_tx: &mpsc::Sender<Option<BuzzEvent>>,
    observer_control_tx: &mpsc::Sender<Event>,
    state: &mut BgState,
    keys: &Keys,
    relay_url: &str,
    agent_pubkey_hex: &str,
    auth_tag: Option<&nostr::Tag>,
) -> bool {
    match msg {
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
                        let source = admission.subscription().clone();

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

                        // `None` is the ordinary case for discovery, enrolment
                        // and watched requests: they keep delivering after their
                        // backlog drains, so their boundary retires no page.
                        if let Some(advance) = state.reconstructions.complete(&witness) {
                            debug!(sub_id = %subscription_id, ?advance, "history page completed");
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
                            )
                        });
                    if state
                        .project_requests
                        .refuse_live(&subscription_id, &message)
                        .is_some()
                    {
                        if let Some(lost) = lost {
                            let routing = state.reconstructions.observe(lost.catch_up(
                                crate::project::CatchUpOutcome::RequestLost(
                                    "relay closed the request",
                                ),
                            ));
                            debug!(
                                sub_id = %subscription_id,
                                ?routing,
                                "history page released by a closed request"
                            );
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

/// Process messages buffered during the NIP-42 auth handshake.
///
/// `do_connect` buffers any non-AUTH/non-OK messages it receives while waiting
/// for the challenge and OK. Those messages would otherwise be silently
/// discarded. We replay them through the normal handler here.
#[allow(clippy::too_many_arguments)]
/// Returns `false` if any buffered message signals the connection should be dropped.
async fn process_handshake_buffer(
    ws: &mut WsStream,
    buffer: std::collections::VecDeque<RelayMessage>,
    event_tx: &mpsc::Sender<Option<BuzzEvent>>,
    observer_control_tx: &mpsc::Sender<Event>,
    state: &mut BgState,
    keys: &Keys,
    relay_url: &str,
    agent_pubkey_hex: &str,
    auth_tag: Option<&nostr::Tag>,
) -> bool {
    if buffer.is_empty() {
        return true;
    }
    debug!("processing {} buffered handshake message(s)", buffer.len());
    for relay_msg in buffer {
        // Re-encode to text so we can reuse handle_ws_message.
        // This is slightly wasteful but keeps the handler as the single
        // source of truth for message dispatch.
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
            let should_continue = handle_ws_message(
                Message::Text(text.into()),
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
            if !should_continue {
                return false;
            }
        }
    }
    true
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
    handshake_buffer: std::collections::VecDeque<RelayMessage>,
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
    process_handshake_buffer(
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
    for (sub_id, identity) in state.project_requests.replayable() {
        match send_project_subscribe(ws, state, &sub_id, identity).await {
            ProjectSendOutcome::Sent | ProjectSendOutcome::AlreadyOpen => {}
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

/// Drain parked observer telemetry frames once the rate-limit gate clears.
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
        state.track_observer_in_flight(event);
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
            | RelayCommand::SubscribeObserverControls => {
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
/// `identity` carries the filter rather than this function building one: the
/// registry serialises the REQ from it, so the bytes on the wire cannot differ
/// from the question that was registered.
async fn send_project_subscribe(
    ws: &mut WsStream,
    state: &mut BgState,
    sub_id: &str,
    identity: crate::project::ProjectRequestIdentity,
) -> ProjectSendOutcome {
    // The registry performs the write, against the socket itself. It is handed
    // the live `WsStream` rather than a closure: a closure returning
    // `Result<(), E>` could be `|_| async { Ok(()) }`, which manufactures send
    // authority with no socket in sight. The registry also serialises the REQ
    // from the registration's own filter, so this function no longer chooses
    // the bytes that go on the wire.
    let outcome = state
        .project_requests
        .open_request(ws, sub_id, identity)
        .await;

    match outcome {
        crate::project::OpenOutcome::Sent => {
            debug!(sub_id, "project REQ sent and registered");
            ProjectSendOutcome::Sent
        }
        crate::project::OpenOutcome::AlreadyLive => {
            debug!(
                sub_id,
                "project request already live — not re-sending its REQ"
            );
            ProjectSendOutcome::AlreadyOpen
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
            ProjectSendOutcome::Exhausted
        }
        crate::project::OpenOutcome::Conflict { held } => {
            warn!(
                sub_id,
                ?held,
                "refusing project request: this id is owned by a different request — \
                 nothing recorded"
            );
            ProjectSendOutcome::MetadataConflict
        }
        crate::project::OpenOutcome::WriteFailed(e) => {
            // Nothing was registered — installation happens only after a
            // successful write, so there is nothing to undo. Other project
            // requests are untouched and still answerable.
            warn!(
                sub_id,
                "failed to send project REQ — nothing registered: {e}"
            );
            ProjectSendOutcome::WriteFailed
        }
        crate::project::OpenOutcome::NotOpenableHere => {
            // A catch-up reached the generic sender. Its wire id has to name
            // one transport attempt, and only `open_history_page` mints those,
            // so this is a caller mistake rather than a relay condition —
            // reported as a conflict because that is what it is: an id this
            // path may not claim.
            error!(
                sub_id,
                "a root catch-up cannot be opened through the generic sender"
            );
            ProjectSendOutcome::MetadataConflict
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
) -> Result<(WsStream, VecDeque<RelayMessage>), RelayError> {
    let parsed = relay_url
        .parse::<url::Url>()
        .map_err(|e| RelayError::Http(format!("invalid relay URL: {e}")))?;

    let (ws, _response) = tokio::time::timeout(CONNECT_TIMEOUT, connect_async(parsed.as_str()))
        .await
        .map_err(|_| RelayError::ConnectionClosed)? // timeout → treat as connection failure
        .map_err(|e| RelayError::WebSocket(Box::new(e)))?;
    debug!("connected to relay at {relay_url}");

    let mut ws = ws;
    let mut buffer: VecDeque<RelayMessage> = VecDeque::new();

    let challenge = wait_for_auth_challenge(&mut ws, &mut buffer, AUTH_TIMEOUT).await?;

    send_auth_response(&mut ws, &challenge, relay_url, keys, auth_tag).await?;

    let event_id = {
        // We need the event_id that was just sent. Re-derive it by signing again
        // just to get the ID — but that's wasteful. Instead, parse the last sent
        // message. Simpler: wait_for_ok accepts any OK (we just sent one event).
        // The event_id in the OK will match whatever we sent.
        // We'll accept the first OK we receive.
        let ok = wait_for_any_ok(&mut ws, &mut buffer, AUTH_TIMEOUT).await?;
        if !ok.accepted {
            return Err(RelayError::AuthFailed(ok.message));
        }
        ok.event_id
    };

    debug!("NIP-42 authentication successful (event {event_id})");
    Ok((ws, buffer))
}

/// Wait for an `AUTH` challenge from the relay, buffering any other messages.
async fn wait_for_auth_challenge(
    ws: &mut WsStream,
    buffer: &mut VecDeque<RelayMessage>,
    timeout_dur: Duration,
) -> Result<String, RelayError> {
    // Check if there's already one buffered.
    if let Some(idx) = buffer
        .iter()
        .position(|m| matches!(m, RelayMessage::Auth { .. }))
    {
        if let Some(RelayMessage::Auth { challenge }) = buffer.remove(idx) {
            return Ok(challenge);
        }
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
                    other => buffer.push_back(other),
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
struct OkResponse {
    event_id: String,
    accepted: bool,
    message: String,
}

/// Wait for the first `OK` message from the relay (used after sending AUTH).
async fn wait_for_any_ok(
    ws: &mut WsStream,
    buffer: &mut VecDeque<RelayMessage>,
    timeout_dur: Duration,
) -> Result<OkResponse, RelayError> {
    // Check if there's already one buffered.
    if let Some(idx) = buffer
        .iter()
        .position(|m| matches!(m, RelayMessage::Ok { .. }))
    {
        if let Some(RelayMessage::Ok {
            event_id,
            accepted,
            message,
        }) = buffer.remove(idx)
        {
            return Ok(OkResponse {
                event_id,
                accepted,
                message,
            });
        }
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
                    other => buffer.push_back(other),
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

    /// Push one EVENT frame through the production dispatch path.
    ///
    /// These tests are about *which dedup set the real code spends*, so poking
    /// `BgState` by hand would assert nothing — the simulation would be the
    /// thing under test. This goes through `handle_ws_message`.
    async fn deliver_frame(
        state: &mut BgState,
        sub_id: &str,
        event: &Event,
        event_tx: &mpsc::Sender<Option<BuzzEvent>>,
    ) {
        let (mut ws, _server) = test_ws_pair().await;
        let (observer_tx, _observer_rx) = mpsc::channel(8);
        let keys = nostr::Keys::generate();
        let text = serde_json::to_string(&json!(["EVENT", sub_id, event])).expect("encode frame");
        let keep_going = handle_ws_message(
            Message::Text(text.into()),
            &mut ws,
            event_tx,
            &observer_tx,
            state,
            &keys,
            "ws://test",
            &keys.public_key().to_hex(),
            None,
        )
        .await;
        assert!(keep_going, "dispatch must not signal connection loss");
    }

    /// Deliver a signed EVENT the way a relay does: the retained peer writes
    /// the raw frame, and the **same registered connection** reads it back off
    /// the wire and hands it to production frame handling.
    ///
    /// Deliberately not [`deliver_frame`]. That builds a fresh socket per
    /// event and passes a constructed `Message::Text` straight into
    /// `handle_ws_message`, so the bytes never cross a connection and the
    /// socket a request was registered on is not the socket its answer arrives
    /// on. Every boundary between "a relay sent this" and "the gate saw it" was
    /// therefore assumed rather than crossed — which is what let a prepared
    /// midpoint survive three rounds of review.
    async fn deliver_over_connection(
        server: &mut WebSocketStream<tokio::net::TcpStream>,
        state: &mut BgState,
        ws: &mut WsStream,
        sub_id: &str,
        event: &Event,
        event_tx: &mpsc::Sender<Option<BuzzEvent>>,
        keys: &nostr::Keys,
    ) {
        use futures_util::SinkExt;
        let text = serde_json::to_string(&json!(["EVENT", sub_id, event])).expect("encode frame");
        server
            .send(Message::Text(text.into()))
            .await
            .expect("the relay peer writes the EVENT");

        let message = timeout(Duration::from_secs(2), ws.next())
            .await
            .expect("timed out reading the EVENT off the connection")
            .expect("the connection closed before the EVENT arrived")
            .expect("read the EVENT frame");

        let (observer_tx, _observer_rx) = mpsc::channel(8);
        let keep_going = handle_ws_message(
            message,
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
        assert!(keep_going, "dispatch must not signal connection loss");
    }

    /// Open a project request the way production must: recorded before any
    /// frame for it is accepted. Returns the subscription id to deliver on.
    ///
    /// Every project test now goes through one of these. That is not
    /// ceremony — before the registry, these tests delivered on ids nobody had
    /// asked for and the dispatch accepted them, which was precisely the
    /// defect.
    /// Open a request the way production does — through `send_project_subscribe`
    /// against a real socket.
    ///
    /// These helpers used to call `reserve()` directly, which left the
    /// registration in a state production never produces: recorded but never
    /// written. Inbound frames were then delivered against it as though the
    /// relay had been asked. Going through the transport is the point — a
    /// helper that can fabricate "sent" is a helper that proves nothing about
    /// what the relay was actually told.
    async fn open_sent(
        state: &mut BgState,
        id: &str,
        identity: crate::project::ProjectRequestIdentity,
    ) -> String {
        let (mut ws, _server) = test_ws_pair().await;
        assert_eq!(
            send_project_subscribe(&mut ws, state, id, identity).await,
            ProjectSendOutcome::Sent
        );
        id.to_string()
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
                    *is_pull_request,
                ))
                .expect("a fresh root enrols");
        }
        enrolments
    }

    /// The watched identity for `roots`, **from the production builder**.
    ///
    /// It used to hand-rebuild one comments/`#e` filter. That looked equivalent
    /// — same kinds accessor, same root-tag accessor — and it was not: the real
    /// builder returns *two* filters, because a comment points at its root with
    /// lowercase `e` and a pull-request revision with uppercase `E`. The
    /// approximation therefore tested a single-filter request nobody sends, and
    /// hid that the identity could not represent a two-filter one at all.
    ///
    /// `since` is a parameter for the same reason the rest of it is derived: a
    /// window the fixture invents is a window production never asked for.
    fn watched_identity(
        generation: u64,
        roots: &[(&str, bool)],
        since: u64,
    ) -> crate::project::ProjectRequestIdentity {
        crate::project::ProjectRequestIdentity::from_filters(
            crate::project::ProjectSubscription::Watched { generation },
            crate::project::watched_roots_filters(&watched_enrolments(roots), since),
        )
        .expect("an enrolled root yields at least one filter")
    }

    async fn open_watched(state: &mut BgState, generation: u64) -> String {
        open_watched_for(state, generation, &[&test_root_id()]).await
    }

    async fn open_watched_for(state: &mut BgState, generation: u64, roots: &[&str]) -> String {
        let issues: Vec<(&str, bool)> = roots.iter().map(|r| (*r, false)).collect();
        let id = crate::project::watched_sub_id(generation);
        open_sent(state, &id, watched_identity(generation, &issues, 0)).await
    }

    async fn open_discovery(state: &mut BgState) -> String {
        let id = crate::project::discovery_sub_id();
        open_sent(state, &id, discovery_identity()).await
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
            &root,
            HistoryStream::Comments,
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

        let watched = open_watched(&mut state, 0).await;
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
        let watched = open_watched(&mut state, 0).await;
        deliver_frame(&mut state, &watched, &event, &tx).await;

        let delivered = drain(&mut rx);
        assert_eq!(delivered.len(), 2, "both surfaces deliver: {delivered:?}");
        assert!(matches!(delivered[0], BuzzEvent::Channel { .. }));
        assert!(matches!(delivered[1], BuzzEvent::Project(_)));
    }

    #[tokio::test]
    async fn overlapping_watched_generations_share_one_project_dedup_set() {
        // A watched-root REQ replacement deliberately overlaps its
        // predecessor, so the same event arrives under two generations' ids.
        // One set across all project subscriptions folds that to one delivery;
        // a per-subscription set would call the second copy new and route the
        // event twice.
        let mut state = BgState::new();
        let (tx, mut rx) = mpsc::channel(16);
        let keys = nostr::Keys::generate();
        let event = mixed_surface_event(&keys, Uuid::new_v4(), &test_root_id(), 1_000);

        let gen1 = open_watched(&mut state, 1).await;
        let gen2 = open_watched(&mut state, 2).await;
        deliver_frame(&mut state, &gen1, &event, &tx).await;
        deliver_frame(&mut state, &gen2, &event, &tx).await;

        assert_eq!(
            drain(&mut rx).len(),
            1,
            "the generation overlap must fold to one delivery"
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
        let watched = open_watched(&mut state, 0).await;
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

    fn identity(
        subscription: crate::project::ProjectSubscription,
        filter: Value,
    ) -> crate::project::ProjectRequestIdentity {
        crate::project::ProjectRequestIdentity::new(subscription, filter)
            .expect("test filters constrain events")
    }

    fn discovery_identity() -> crate::project::ProjectRequestIdentity {
        identity(
            crate::project::ProjectSubscription::Discovery,
            test_filter(),
        )
    }

    /// Feed one non-EVENT frame through the production handler.
    async fn deliver_control_frame(state: &mut BgState, frame: Value) -> bool {
        let (mut ws, _server) = test_ws_pair().await;
        let (tx, _rx) = mpsc::channel(16);
        let (observer_tx, _observer_rx) = mpsc::channel(8);
        let keys = nostr::Keys::generate();
        let text = serde_json::to_string(&frame).expect("encode");
        handle_ws_message(
            Message::Text(text.into()),
            &mut ws,
            &tx,
            &observer_tx,
            state,
            &keys,
            "ws://test",
            &keys.public_key().to_hex(),
            None,
        )
        .await
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
            install_replacement_with(state, &mut dead, replacement, VecDeque::new()).await,
            "an empty handshake buffer carries no drop signal"
        );
        // `dead` now holds the replacement — production reassigns the same
        // variable for the same reason.
        resubscribe_after_reconnect(&mut dead, &mut cmd_rx, state, &agent, true).await
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
        buffer: VecDeque<RelayMessage>,
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
            send_project_subscribe(&mut ws, &mut state, &sub_id, discovery_identity()).await,
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

        let survivor = crate::project::watched_sub_id(0);
        assert_eq!(
            send_project_subscribe(
                &mut ws,
                &mut state,
                &survivor,
                identity(
                    crate::project::ProjectSubscription::Watched { generation: 0 },
                    test_filter(),
                ),
            )
            .await,
            ProjectSendOutcome::Sent
        );

        drop(server);
        let _ = ws.close(None).await;
        let doomed = crate::project::discovery_sub_id();
        assert_eq!(
            send_project_subscribe(&mut ws, &mut state, &doomed, discovery_identity()).await,
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
        assert_eq!(state.project_requests.watched_current(), Some(0));
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
            state.project_requests.watched_current(),
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
        assert_eq!(state.project_requests.watched_current(), Some(2));
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
        assert_eq!(state.project_requests.watched_current(), Some(0));
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
            state.project_requests.watched_current(),
            Some(0),
            "a refusal must leave the actual predecessor current"
        );

        // And the next valid replacement retires that predecessor, not the
        // generation the refusal burned.
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
            close_ids(&frames),
            vec![crate::project::watched_sub_id(0)],
            "the valid replacement must retire the real predecessor: {frames:?}"
        );
    }

    /// **Spent watched generations fail closed, in both build modes.**
    ///
    /// Asserts the refusal rather than a panic: the original `g + 1` panicked
    /// in debug and wrapped to generation zero in release, and release is the
    /// only mode where the reuse happened. A test that asserted the panic would
    /// have passed in debug and said nothing about the mode that mattered.
    #[tokio::test]
    async fn spent_watched_generations_refuse_rather_than_reuse() {
        let (mut ws, mut server) = test_ws_pair().await;
        let mut state = BgState::new();

        state
            .project_requests
            .seed_allocators_for_exhaustion(u64::MAX, 0);

        // The last generation this process can name installs normally.
        assert!(
            submit_replacement(
                &mut ws,
                &mut state,
                crate::project::ProjectReplacement::Watched,
                vec![watched_filter(1)],
            )
            .await
        );
        assert_eq!(state.project_requests.watched_current(), Some(u64::MAX));
        drain_test_frames(&mut server).await;

        // The space is now spent.
        assert!(
            submit_replacement(
                &mut ws,
                &mut state,
                crate::project::ProjectReplacement::Watched,
                vec![watched_filter(2)],
            )
            .await,
            "exhaustion is terminal, not a transport failure"
        );
        let frames = drain_test_frames(&mut server).await;
        assert!(
            frames.is_empty(),
            "an exhausted generation space must write nothing: {frames:?}"
        );
        assert!(
            !frames
                .iter()
                .any(|f| f[1] == serde_json::json!(crate::project::watched_sub_id(0))),
            "generation zero was reused: {frames:?}"
        );
        assert_eq!(
            state.project_requests.watched_current(),
            Some(u64::MAX),
            "the predecessor must be retained on refusal"
        );
        assert!(
            state
                .project_requests
                .match_frame(&crate::project::watched_sub_id(u64::MAX))
                .is_some(),
            "the agent keeps answering on the subscription it already had"
        );
    }

    /// **A spent incarnation space fails closed the same way.**
    ///
    /// A second, independent ceiling: the registry may have generations left
    /// and no authority to stamp them with. The run loop's own checked counter
    /// could not see this one at all, so an exhausted registry still read as a
    /// successful replacement upstream.
    #[tokio::test]
    async fn a_spent_incarnation_space_refuses_the_replacement() {
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
        assert_eq!(state.project_requests.watched_current(), Some(0));
        drain_test_frames(&mut server).await;

        // Burn the incarnation space, leaving generations available.
        state
            .project_requests
            .seed_allocators_for_exhaustion(1, u64::MAX);
        assert!(
            submit_replacement(
                &mut ws,
                &mut state,
                crate::project::ProjectReplacement::Watched,
                vec![watched_filter(2)],
            )
            .await
        );
        drain_test_frames(&mut server).await;

        let frames_before = drain_test_frames(&mut server).await;
        assert!(
            frames_before.is_empty(),
            "nothing outstanding before the exhausted attempt: {frames_before:?}"
        );
        assert!(
            submit_replacement(
                &mut ws,
                &mut state,
                crate::project::ProjectReplacement::Watched,
                vec![watched_filter(3)],
            )
            .await,
            "exhaustion is terminal, not a transport failure"
        );
        let frames = drain_test_frames(&mut server).await;
        assert!(
            frames.is_empty(),
            "a spent incarnation space must write nothing: {frames:?}"
        );
        assert_eq!(
            state.project_requests.watched_current(),
            Some(1),
            "the last genuinely installed generation stays current"
        );
        assert!(
            state
                .project_requests
                .intent(&crate::project::watched_sub_id(1))
                .is_some(),
            "and its durable intent is preserved"
        );
    }

    #[tokio::test]
    async fn an_already_open_request_does_not_emit_a_second_req() {
        // `AlreadyLive` is not permission to re-send. A second REQ under a live
        // id could replace the relay's subscription while leaving the old
        // request's EOSE indistinguishable from the new one's.
        let (mut ws, mut server) = test_ws_pair().await;
        let mut state = BgState::new();
        let sub_id = crate::project::discovery_sub_id();

        for expected in [ProjectSendOutcome::Sent, ProjectSendOutcome::AlreadyOpen] {
            assert_eq!(
                send_project_subscribe(&mut ws, &mut state, &sub_id, discovery_identity()).await,
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
            send_project_subscribe(&mut ws, &mut state, &sub_id, discovery_identity()).await,
            ProjectSendOutcome::Sent
        );
        assert_eq!(next_test_frame(&mut server).await[0], "REQ");

        let usurper = identity(
            crate::project::ProjectSubscription::Watched { generation: 9 },
            json!({ "kinds": [1] }),
        );
        assert_eq!(
            send_project_subscribe(&mut ws, &mut state, &sub_id, usurper.clone()).await,
            ProjectSendOutcome::MetadataConflict
        );
        assert!(
            timeout(Duration::from_millis(200), server.next())
                .await
                .is_err(),
            "no REQ is emitted"
        );
        assert_eq!(
            state.project_requests.intent(&sub_id),
            Some(&discovery_identity()),
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
        let sub_id = crate::project::discovery_sub_id();

        assert_eq!(
            send_project_subscribe(&mut ws, &mut state, &sub_id, discovery_identity()).await,
            ProjectSendOutcome::Sent
        );
        assert_eq!(next_test_frame(&mut server).await[0], "REQ");

        let other_filter = identity(
            crate::project::ProjectSubscription::Discovery,
            json!({ "kinds": [KIND_GIT_REPO_ANNOUNCEMENT], "authors": ["deadbeef"] }),
        );
        assert_eq!(
            send_project_subscribe(&mut ws, &mut state, &sub_id, other_filter).await,
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
            send_project_subscribe(&mut ws, &mut state, &sub_id, discovery_identity()).await,
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
                .record_intent(&sub_id, discovery_identity()),
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
        assert_eq!(
            state.project_requests.intent(&sub_id),
            Some(&discovery_identity()),
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
        let sub_id = crate::project::discovery_sub_id();
        state
            .project_requests
            .record_intent(&sub_id, discovery_identity());

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
        let sub_id = crate::project::watched_sub_id(0);
        send_project_subscribe(
            &mut ws,
            &mut state,
            &sub_id,
            identity(
                crate::project::ProjectSubscription::Watched { generation: 0 },
                test_filter(),
            ),
        )
        .await;
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
            page_finished: !state
                .reconstructions
                .get(root)
                .expect("the root is still tracked")
                .finished_streams()
                .is_empty(),
        }
    }

    /// Feed a control frame with a caller-supplied event channel.
    async fn deliver_control_frame_to(
        state: &mut BgState,
        frame: Value,
        tx: &mpsc::Sender<Option<BuzzEvent>>,
    ) -> bool {
        let (mut ws, _server) = test_ws_pair().await;
        let (observer_tx, _observer_rx) = mpsc::channel(8);
        let keys = nostr::Keys::generate();
        let text = serde_json::to_string(&frame).expect("encode");
        handle_ws_message(
            Message::Text(text.into()),
            &mut ws,
            tx,
            &observer_tx,
            state,
            &keys,
            "ws://test",
            &keys.public_key().to_hex(),
            None,
        )
        .await
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

    #[tokio::test]
    async fn an_exhausted_incarnation_space_writes_nothing_and_keeps_the_socket() {
        // Terminal, local, and none of the other three things it could be
        // mistaken for. Previously this surfaced as `MetadataConflict`, which
        // made the reconnect path report a request-ownership disagreement that
        // did not exist — a diagnostic pointing at the wrong subsystem.
        let (mut ws, mut server) = test_ws_pair().await;
        let mut state = BgState::new();
        let agent = nostr::Keys::generate().public_key().to_hex();
        let sub_id = crate::project::discovery_sub_id();

        // Spend the space.
        state.project_requests.force_next_incarnation(u64::MAX);
        let burner = crate::project::watched_sub_id(0);
        assert_eq!(
            send_project_subscribe(
                &mut ws,
                &mut state,
                &burner,
                identity(
                    crate::project::ProjectSubscription::Watched { generation: 0 },
                    test_filter(),
                ),
            )
            .await,
            ProjectSendOutcome::Sent
        );
        assert_eq!(next_test_frame(&mut server).await[0], "REQ");

        // Now nothing further can be opened.
        assert_eq!(
            send_project_subscribe(&mut ws, &mut state, &sub_id, discovery_identity()).await,
            ProjectSendOutcome::Exhausted,
            "reported as itself, not as a conflict"
        );
        assert!(
            timeout(Duration::from_millis(200), server.next())
                .await
                .is_err(),
            "no wire frame"
        );
        assert!(
            state.project_requests.match_frame(&sub_id).is_none(),
            "no live registration"
        );

        // The command path keeps the socket: this is not a transport failure.
        assert!(
            execute_connected_command(
                &mut ws,
                &mut state,
                &agent,
                RelayCommand::SubscribeProject {
                    sub_id: sub_id.clone(),
                    subscription: crate::project::ProjectSubscription::Discovery,
                    filters: vec![test_filter()],
                },
            )
            .await,
            "exhaustion must not be reported as a dead socket"
        );

        // And the request opened before exhaustion still works.
        assert!(state.project_requests.match_frame(&burner).is_some());
    }

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
        let watched = open_watched(&mut state, 0).await;

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
                RelayCommand::SubscribeProject {
                    sub_id: sub_id.clone(),
                    subscription: crate::project::ProjectSubscription::Discovery,
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
                RelayCommand::SubscribeProject {
                    sub_id: sub_id.clone(),
                    subscription: crate::project::ProjectSubscription::Watched { generation: 9 },
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
        assert_eq!(
            state.project_requests.intent(&sub_id),
            Some(&discovery_identity())
        );
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
        let sub_id = crate::project::watched_sub_id(0);
        let watched = crate::project::ProjectSubscription::Watched { generation: 0 };

        for filters in [Vec::new(), vec![json!({})], vec![json!({ "limit": 500 })]] {
            // ---- Disconnected: no durable intent, so no later replay. ------
            let mut disconnected = BgState::new();
            apply_command_to_state(
                &mut disconnected,
                RelayCommand::SubscribeProject {
                    sub_id: sub_id.clone(),
                    subscription: watched.clone(),
                    filters: filters.clone(),
                },
            );
            assert_eq!(
                disconnected.project_requests.intent(&sub_id),
                None,
                "{filters:?}: no intent"
            );
            assert!(
                disconnected.project_requests.replayable().is_empty(),
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
                    RelayCommand::SubscribeProject {
                        sub_id: sub_id.clone(),
                        subscription: watched.clone(),
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
                state.project_requests.replayable().is_empty(),
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
        for cmd in [
            RelayCommand::SubscribeProject {
                sub_id: sub_id.clone(),
                subscription: crate::project::ProjectSubscription::Discovery,
                filters: vec![test_filter()],
            },
            RelayCommand::SubscribeProject {
                sub_id: sub_id.clone(),
                subscription: crate::project::ProjectSubscription::Watched { generation: 4 },
                filters: vec![json!({ "kinds": [1] })],
            },
        ] {
            apply_command_to_state(&mut state, cmd);
        }

        assert_eq!(
            state.project_requests.intent(&sub_id),
            Some(&discovery_identity())
        );
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
    async fn a_closed_project_request_stops_being_answerable() {
        // The half a parser could never express. A subscription id does not
        // stop being well-formed when we stop listening, so late frames for a
        // request we have finished with used to keep working.
        let mut state = BgState::new();
        let (tx, mut rx) = mpsc::channel(16);
        let keys = nostr::Keys::generate();
        let event_a = mixed_surface_event(&keys, Uuid::new_v4(), &test_root_id(), 1_000);
        let event_b = mixed_surface_event(&keys, Uuid::new_v4(), &test_root_id(), 1_001);

        let watched = open_watched(&mut state, 0).await;
        deliver_frame(&mut state, &watched, &event_a, &tx).await;
        assert_eq!(drain(&mut rx).len(), 1, "open: delivered");

        state.project_requests.close_active(&watched);
        deliver_frame(&mut state, &watched, &event_b, &tx).await;

        assert!(drain(&mut rx).is_empty(), "closed: not delivered");
        assert!(
            !state.project_seen_ids.contains(&event_b.id.to_hex()),
            "a closed request spends nothing"
        );
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

    #[tokio::test]
    async fn a_refused_reopen_leaves_the_original_class_in_force_on_the_wire() {
        // The registry's unit tests prove the record survives a conflicting
        // reopen. This proves the *dispatch* still behaves under it, which is
        // the thing that actually matters — a retained record that nothing
        // consults would be bookkeeping theatre.
        //
        // The classes are chosen so they disagree observably: under
        // `RootCatchUp { root_a }` an event for root B is not one of the page's
        // rows, and under `Watched` the same event is delivered to the consumer
        // as a routed event. So if the reopen had taken effect, the assertions
        // below would see a delivery and an intact page.
        /// A catch-up for `bound` under `id`, holding a page, after a
        /// conflicting reopen to `Watched` has been refused.
        async fn after_a_refused_reopen(
            state: &mut BgState,
            bound: &crate::project::VerifiedBoundRoot,
        ) -> String {
            let id = bind_page_under(state, bound).await;
            let (mut ws, _server) = test_ws_pair().await;
            assert!(
                matches!(
                    state
                        .project_requests
                        .open_request(
                            &mut ws,
                            &id,
                            identity(
                                crate::project::ProjectSubscription::Watched { generation: 0 },
                                test_filter(),
                            ),
                        )
                        .await,
                    crate::project::OpenOutcome::Conflict { .. }
                ),
                "re-pointing a live id must be refused"
            );
            id
        }

        let mut state = BgState::new();
        let (tx, mut rx) = mpsc::channel(16);
        let (bound_a, keys) = proven_issue_root().await;
        let root_a = bound_a.binding().root().to_string();
        let root_b = "b".repeat(64);

        let id = after_a_refused_reopen(&mut state, &bound_a).await;

        let for_b = comment_on_root(&keys, &root_b, 900, "another root's event");
        deliver_frame(&mut state, &id, &for_b, &tx).await;
        assert!(
            drain(&mut rx).is_empty(),
            "under `Watched` this would have been delivered as a routed event"
        );
        assert!(
            !state.project_seen_ids.contains(&for_b.id.to_hex()),
            "and it spends nothing"
        );
        assert!(
            page_verdict(&mut state, &root_a, &id, &tx).await.is_err(),
            "still bound to root A's reconstruction, so root B is not one of its rows"
        );

        // Positive control, on its own connection because the refusal above is
        // terminal for that reconstruction: the same refused reopen leaves root
        // A's own events admissible.
        let mut state = BgState::new();
        let id = after_a_refused_reopen(&mut state, &bound_a).await;
        let for_a = comment_on_root(&keys, &root_a, 900, "its own root's event");
        deliver_frame(&mut state, &id, &for_a, &tx).await;
        assert_eq!(
            page_verdict(&mut state, &root_a, &id, &tx).await,
            Ok(1),
            "the original request is unharmed by the refused reopen"
        );
    }

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

        let watched = open_watched(&mut state, 0).await;
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

        let watched = open_watched(&mut state, 0).await;
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
                };
                crate::handle_project_event(&mut d, $ev)
            }};
        }

        // ── 1. Discovery REQ, written the way production writes it ──────────
        let (mut ws, mut server) = test_ws_pair().await;
        let discovery_id = crate::project::discovery_sub_id();
        assert_eq!(
            send_project_subscribe(&mut ws, &mut state, &discovery_id, discovery_identity()).await,
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
        let identity = crate::project::ProjectRequestIdentity::from_filters(
            crate::project::ProjectSubscription::Enrolment,
            vec![filter],
        )
        .expect("the enrolment filter is bounded");
        let enrol_id = crate::project::PROJECT_ENROL_SUB_ID.to_string();
        assert_eq!(
            send_project_subscribe(&mut ws, &mut state, &enrol_id, identity).await,
            ProjectSendOutcome::Sent
        );
        let frame = next_test_frame(&mut server).await;
        assert_eq!(frame[0], "REQ");
        assert_eq!(frame[1], enrol_id);
        let filter_text = frame[2].to_string();
        assert!(
            filter_text.contains(&agent_hex),
            "the enrolment REQ must be scoped to this agent"
        );

        // ── 4. The owner opens an issue naming the agent ────────────────────
        let coordinate = format!("30617:{owner_hex}:connected-repo");
        let root = EventBuilder::new(
            nostr::Kind::Custom(buzz_core::kind::KIND_GIT_ISSUE as u16),
            "please take a look",
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
        let identity = crate::project::ProjectRequestIdentity::from_filters(
            crate::project::ProjectSubscription::Watched { generation: 1 },
            watched,
        )
        .expect("watched filters are bounded");
        let watched_id = crate::project::watched_sub_id(1);
        assert_eq!(
            send_project_subscribe(&mut ws, &mut state, &watched_id, identity).await,
            ProjectSendOutcome::Sent
        );
        let frame = next_test_frame(&mut server).await;
        assert_eq!(
            frame[1], watched_id,
            "the replacement uses a fresh generation"
        );
        assert!(
            frame.to_string().contains(&root.id.to_hex()),
            "the watched REQ must name the root just enrolled"
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
        let identity = crate::project::ProjectRequestIdentity::from_filters(
            crate::project::ProjectSubscription::Enrolment,
            vec![filter],
        )
        .expect("bounded");
        let enrol_id = crate::project::PROJECT_ENROL_SUB_ID.to_string();
        let (mut ws, _server) = test_ws_pair().await;
        assert_eq!(
            send_project_subscribe(&mut ws, &mut state, &enrol_id, identity).await,
            ProjectSendOutcome::Sent
        );

        let root_named = |signer: &nostr::Keys, coord: &str, p: &str| {
            EventBuilder::new(
                nostr::Kind::Custom(buzz_core::kind::KIND_GIT_ISSUE as u16),
                "look at this",
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
    async fn readable_frames(
        server: &mut WebSocketStream<tokio::net::TcpStream>,
    ) -> Vec<serde_json::Value> {
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
    #[tokio::test]
    async fn phase_a_end_to_end_relay_bytes_reach_the_agents_stdin() {
        // A protocol stub hanging forever would be a tedious way to end the
        // night, so the whole scenario is bounded.
        timeout(Duration::from_secs(60), async {
            let owner = nostr::Keys::generate();
            let agent = nostr::Keys::generate();
            let owner_hex = owner.public_key().to_hex();
            let agent_hex = agent.public_key().to_hex();
            let agent_identity =
                crate::project::AgentIdentity::new(&agent.public_key()).expect("identity");

            let (tx, mut rx) = mpsc::channel(16);
            let mut discovered = crate::project::DiscoveredRepositories::new();
            let mut enrolments = crate::project::ProjectEnrolments::new();
            let mut queue = crate::queue::EventQueue::new(crate::config::DedupMode::Queue);
            let humans = std::collections::BTreeSet::new();
            let externals = std::collections::BTreeSet::new();

            // One socket and one registry for the whole scenario. Every REQ and
            // CLOSE below is read off the server half, so what is asserted is
            // what the relay received rather than what a helper returned.
            let (client, mut server) = test_ws_pair().await;
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
                                },
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
            // writes it, and the socket the requests were registered on reads
            // it. No fresh socket, and no `Message::Text` handed directly to
            // `handle_ws_message`.
            macro_rules! deliver {
                ($sub_id:expr, $event:expr) => {{
                    let mut guard = subscriber.inner.lock().await;
                    let (state, ws) = &mut *guard;
                    deliver_over_connection(&mut server, state, ws, $sub_id, $event, &tx, &agent)
                        .await;
                }};
            }

            // ── 1. discovery REQ, written the production way ─────────────────
            let discovery_id = crate::project::discovery_sub_id();
            {
                let mut guard = subscriber.inner.lock().await;
                let (state, ws) = &mut *guard;
                assert_eq!(
                    send_project_subscribe(ws, state, &discovery_id, discovery_identity()).await,
                    ProjectSendOutcome::Sent
                );
            }
            let seen = readable_frames(&mut server).await;
            assert_eq!(
                seen.len(),
                1,
                "discovery writes exactly one frame: {seen:?}"
            );
            assert_eq!(seen[0][0], "REQ");
            assert_eq!(seen[0][1], discovery_id);

            // ── 2. the announcement drives an enrolment replacement ──────────
            let announcement = EventBuilder::new(
                nostr::Kind::Custom(buzz_core::kind::KIND_GIT_REPO_ANNOUNCEMENT as u16),
                "",
            )
            .tags([nostr::Tag::parse(["d", "e2e-repo"]).expect("d tag")])
            .sign_with_keys(&owner)
            .expect("sign");
            deliver!(&discovery_id, &announcement);
            assert_eq!(drive_all!(), crate::ProjectDispatched::DiscoveryChanged);

            let seen = readable_frames(&mut server).await;
            assert_eq!(seen.len(), 1, "one enrolment REQ: {seen:?}");
            assert_eq!(seen[0][0], "REQ");
            assert_eq!(seen[0][1], crate::project::PROJECT_ENROL_SUB_ID);
            assert!(
                seen[0].to_string().contains(&agent_hex),
                "the enrolment REQ must be scoped to this agent"
            );
            let first_enrolment = seen[0].to_string();

            // ── 2b. a second repository must WIDEN that enrolment ────────────
            //
            // Without this the scenario proves only that an enrolment REQ is
            // issued, which the shipped defect also did. Widening is the thing
            // that was broken: the id is fixed, so the second identity has to
            // replace the first rather than be refused as a conflict.
            let other_owner = nostr::Keys::generate();
            let other_announcement = EventBuilder::new(
                nostr::Kind::Custom(buzz_core::kind::KIND_GIT_REPO_ANNOUNCEMENT as u16),
                "",
            )
            .tags([nostr::Tag::parse(["d", "second-repo"]).expect("d tag")])
            .sign_with_keys(&other_owner)
            .expect("sign");
            deliver!(&discovery_id, &other_announcement);
            assert_eq!(drive_all!(), crate::ProjectDispatched::DiscoveryChanged);

            let seen = readable_frames(&mut server).await;
            assert_eq!(seen.len(), 1, "the widened enrolment REQ: {seen:?}");
            assert_eq!(seen[0][0], "REQ");
            assert_eq!(
                seen[0][1],
                crate::project::PROJECT_ENROL_SUB_ID,
                "widening reuses the enrolment id"
            );
            let widened = seen[0].to_string();
            assert_ne!(
                widened, first_enrolment,
                "the second discovery did not change the filter — this is the \
                 shipped defect: the enrolment can never widen past the first \
                 repository"
            );
            assert!(
                widened.contains(&other_owner.public_key().to_hex()),
                "the second repository is absent from the widened filter"
            );

            // ── 3. the owner opens an issue naming the agent ─────────────────
            let coordinate = format!("30617:{owner_hex}:e2e-repo");
            let body = "the pipeline drops frames after reconnect";
            let enrol_id = crate::project::PROJECT_ENROL_SUB_ID.to_string();
            let root = EventBuilder::new(
                nostr::Kind::Custom(buzz_core::kind::KIND_GIT_ISSUE as u16),
                body,
            )
            .tags([
                nostr::Tag::parse(["a", &coordinate]).expect("a tag"),
                nostr::Tag::parse(["p", &agent_hex]).expect("p tag"),
            ])
            .sign_with_keys(&owner)
            .expect("sign");
            deliver!(&enrol_id, &root);

            let route_key = match drive_all!() {
                crate::ProjectDispatched::Queued { key, queued, .. } => {
                    assert!(queued, "the root must enter the queue");
                    key
                }
                other => panic!("expected a queued turn, got {other:?}"),
            };

            let seen = readable_frames(&mut server).await;
            assert_eq!(seen.len(), 1, "the first watched REQ, no CLOSE: {seen:?}");
            assert_eq!(seen[0][0], "REQ");
            assert_eq!(seen[0][1], crate::project::watched_sub_id(0));
            assert!(
                seen[0].to_string().contains(&root.id.to_hex()),
                "the watched REQ must name the enrolled root"
            );

            // ── 4. a second root replaces the watch and retires generation 0 ─
            let second = EventBuilder::new(
                nostr::Kind::Custom(buzz_core::kind::KIND_GIT_ISSUE as u16),
                "a second issue on the same repository",
            )
            .tags([
                nostr::Tag::parse(["a", &coordinate]).expect("a tag"),
                nostr::Tag::parse(["p", &agent_hex]).expect("p tag"),
            ])
            .sign_with_keys(&owner)
            .expect("sign");
            deliver!(&enrol_id, &second);
            drive_all!();

            let seen = readable_frames(&mut server).await;
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
            assert!(
                seen[0].to_string().contains(&second.id.to_hex()),
                "the successor must carry the newly enrolled root"
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
                    install_replacement_with(state, ws, replacement_client, VecDeque::new()).await,
                    "the replacement connection installs"
                );
                let (_reconnect_tx, mut reconnect_rx) = mpsc::channel(1);
                assert!(matches!(
                    resubscribe_after_reconnect(ws, &mut reconnect_rx, state, &agent_hex, true)
                        .await,
                    ResubscribeResult::Ok
                ));
            }
            // The dead socket's peer goes with it; everything after this reads
            // the replacement.
            server = replacement_server;
            let replayed = readable_frames(&mut server).await;
            let replayed_ids: Vec<String> = replayed
                .iter()
                .filter(|f| f[0] == "REQ")
                .filter_map(|f| f[1].as_str().map(str::to_string))
                .collect();

            // Exactly the three current intents, and generation 0 is not among
            // them. A retired generation coming back here is the defect this
            // whole iteration exists for, arriving by a different door.
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
            assert!(
                replayed.iter().any(|f| f[1]
                    == serde_json::json!(crate::project::watched_sub_id(1))
                    && f.to_string().contains(&second.id.to_hex())),
                "the replayed watch must carry the roots the scenario enrolled: {replayed:?}"
            );

            // ── the batch the pool claims is the batch the queue produced ────
            let batch = queue.flush_next().expect("the queued turn flushes");
            assert_eq!(batch.channel_id, route_key, "flushed under the root key");
            assert!(
                batch.project_origin().is_some(),
                "the flushed batch carries its project origin"
            );

            // ── the endpoint the agent's reply is submitted to ───────────────
            //
            // Scope, stated so it is not overread: this receives the signed
            // event and returns acceptance. It is the transport boundary, not
            // independent relay validation — nothing here checks the event
            // against relay policy. What it buys is that the reply must be
            // really built, really signed and really sent to be observed.
            let submissions: std::sync::Arc<std::sync::Mutex<Vec<serde_json::Value>>> =
                std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let events_url = {
                let sink = submissions.clone();
                let app = axum::Router::new().route(
                    "/events",
                    axum::routing::post(move |body: String| {
                        let sink = sink.clone();
                        async move {
                            let event: serde_json::Value =
                                serde_json::from_str(&body).unwrap_or(serde_json::Value::Null);
                            let id = event["id"].as_str().unwrap_or_default().to_string();
                            sink.lock().expect("submission sink").push(event);
                            axum::Json(serde_json::json!({
                                "event_id": id,
                                "accepted": true,
                                "message": "",
                            }))
                        }
                    }),
                );
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                    .await
                    .expect("bind the capture endpoint");
                let addr = listener.local_addr().expect("capture endpoint address");
                tokio::spawn(async move {
                    let _ = axum::serve(listener, app).await;
                });
                format!("http://{addr}")
            };

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
        env = dict(os.environ)
        env["BUZZ_ACP_TEST_CLI_ARGV"] = json.dumps(argv)
        # Set here, not inherited: see the note on `child_env` below.
        env["BUZZ_RELAY_URL"] = os.environ["BUZZ_ACP_TEST_RELAY_URL"]
        env["BUZZ_PRIVATE_KEY"] = os.environ["BUZZ_ACP_TEST_KEY"]
        proc = subprocess.run(
            [os.environ["BUZZ_ACP_TEST_EXE"], "--exact",
             os.environ["BUZZ_ACP_TEST_CLI_HELPER"], "--nocapture"],
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
            );

            // The child is given only what a real agent process is given: where
            // the relay is and which key is its own. The argv comes from the
            // prompt.
            //
            // **These names are harness-specific on purpose — do not "simplify"
            // them to `BUZZ_RELAY_URL` and `BUZZ_PRIVATE_KEY`.** `AcpClient::spawn`
            // injects `extra_env` only for keys absent from the parent
            // environment (operator precedence, `acp.rs`). A developer or agent
            // runtime with those two set therefore has this scenario silently
            // ignore the capture endpoint below, sign with the operator's real
            // key, and publish to their real relay. That is not a hypothetical:
            // it happened on the first run of this step. The stub copies these
            // into the CLI's own names for the helper process, where nothing
            // overrides them.
            let child_env = vec![
                (
                    "BUZZ_ACP_TEST_EXE".to_string(),
                    std::env::current_exe()
                        .expect("this test executable's own path")
                        .to_string_lossy()
                        .into_owned(),
                ),
                (
                    "BUZZ_ACP_TEST_CLI_HELPER".to_string(),
                    CLI_HELPER_TEST.to_string(),
                ),
                ("BUZZ_ACP_TEST_RELAY_URL".to_string(), events_url.clone()),
                (
                    "BUZZ_ACP_TEST_KEY".to_string(),
                    agent.secret_key().to_secret_hex(),
                ),
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

            // Guaranteed reaping. `Drop` only best-efforts a SIGKILL and a
            // non-blocking `try_wait` (`acp.rs:2168`); `shutdown` kills the
            // process group and waits, which is what the API documents as the
            // path a caller "SHOULD" take. Leaving the child to `Drop` would
            // leave the test's own cleanup weaker than production's.
            //
            // The enclosing 60s timeout is what makes this an assertion: a
            // child that never exits wedges here and fails the scenario rather
            // than leaking quietly past a green result.
            reclaimed.acp.shutdown().await;

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
            assert!(text.contains(&root.id.to_hex()), "no root event id");
            assert!(text.contains("issue"), "no issue/PR classification");
            assert!(text.contains("buzz issues comment"), "no reply command");
            assert!(
                text.contains(body),
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
            assert_eq!(
                argv.first().map(String::as_str),
                Some("buzz"),
                "the child did not invoke the buzz CLI: {argv:?}"
            );

            // ── the submission, as the endpoint received it ──────────────────
            let submitted = submissions.lock().expect("submission sink").clone();
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
            let event = &submitted[0];

            assert_eq!(
                event["kind"].as_u64(),
                Some(1),
                "a project comment is a kind:1 text note"
            );
            assert_eq!(
                event["pubkey"].as_str(),
                Some(agent_hex.as_str()),
                "the comment is not signed by the woken agent"
            );
            assert_eq!(
                event["content"].as_str(),
                Some(reply_body),
                "the published body is not what the agent wrote"
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
                vec![root.id.to_hex()],
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

    /// The flag-off control, through the harness the scenario uses.
    ///
    /// The same socket and the same production write path as
    /// [`phase_a_end_to_end_relay_bytes_reach_the_agents_stdin`], driven from
    /// the decision production actually makes. With the flag off nothing is
    /// written, and because every other project REQ derives its filter from
    /// discovery state, nothing downstream has anything to fire on — no
    /// announcement arrives, so no repository is discovered, so no root is
    /// enrolled or watched.
    ///
    /// The asymmetry is the point: a control that writes no bytes because the
    /// harness never asked it to would pass against a flag that gates nothing.
    #[tokio::test]
    async fn the_flag_off_control_writes_no_bytes_where_the_flag_on_control_writes_a_req() {
        let (mut client, mut server) = test_ws_pair().await;
        let mut state = BgState::new();

        assert!(
            crate::project::discovery_subscription(false).is_none(),
            "the flag-off decision must be to open nothing"
        );
        assert!(
            readable_frames(&mut server).await.is_empty(),
            "a disabled harness put bytes on the socket before anything asked it to"
        );

        let (sub_id, class, filters) = crate::project::discovery_subscription(true)
            .expect("the flag-on decision must be to open discovery");
        // `from_filters` refuses a filter that constrains nothing, so this also
        // says the production discovery filter is a bounded request rather than
        // a subscription to the whole relay.
        let identity = crate::project::ProjectRequestIdentity::from_filters(class, filters)
            .expect("the production discovery filter must constrain events");
        assert_eq!(
            send_project_subscribe(&mut client, &mut state, &sub_id, identity).await,
            ProjectSendOutcome::Sent
        );

        let seen = readable_frames(&mut server).await;
        assert_eq!(
            seen.len(),
            1,
            "the flag on writes exactly one REQ: {seen:?}"
        );
        assert_eq!(seen[0][0], "REQ");
        assert_eq!(
            seen[0][1],
            crate::project::discovery_sub_id(),
            "the flag-on control opens something other than discovery"
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
        let watched = open_watched(&mut state, 0).await;
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
        let id = crate::project::watched_sub_id(0);
        open_sent(
            &mut state,
            &id,
            watched_identity(0, &[(&watched_root, false)], 1_000),
        )
        .await;

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
        let identity = watched_identity(0, &roots, 0);

        // ---- The bytes, through a concrete paired socket. ------------------
        //
        // Compared against `project_req_frames`, which is what the driver will
        // actually send. Equality of the whole frame, not a spot check on one
        // key: this is the assertion that would have caught `["REQ", id, [a, b]]`.
        let mut state = BgState::new();
        let (mut ws, mut server) = test_ws_pair().await;
        let id = crate::project::watched_sub_id(0);
        assert_eq!(
            send_project_subscribe(&mut ws, &mut state, &id, identity.clone()).await,
            ProjectSendOutcome::Sent
        );
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
        let replayable = state.project_requests.replayable();
        assert_eq!(replayable.len(), 1, "one request to re-ask: {replayable:?}");
        for (sub_id, identity) in replayable {
            assert_eq!(
                send_project_subscribe(&mut replacement, &mut state, &sub_id, identity).await,
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
        let id = "proj-enrolment-test".to_string();
        open_sent(
            &mut state,
            &id,
            identity(crate::project::ProjectSubscription::Enrolment, filter),
        )
        .await;

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
        let recon = state
            .reconstructions
            .get(root)
            .expect("the root is still tracked");
        match recon.abandoned_reason() {
            Some(reason) => Err(reason.to_string()),
            None => Ok(recon.finished_streams().iter().map(|s| s.len()).sum()),
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
        assert!(
            drain(&mut rx).iter().all(|e| !matches!(
                e,
                BuzzEvent::Project(crate::project::ProjectEvent::Routed { .. })
            )),
            "and it never escaped as an ordinary routed event"
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
        let (tx, _rx) = mpsc::channel(16);
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
        let watched = open_watched_for(&mut state, 0, &[&root]).await;
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
        assert_eq!(
            recon.pages_wanted(),
            vec![(crate::project::HistoryStream::Comments, 1_000, 4)],
            "the stream asks again, from the bound it already had"
        );
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
        assert_eq!(
            recon.pages_wanted(),
            vec![(crate::project::HistoryStream::Comments, 1_000, 4)],
            "the stream asks again, from the bound it already had"
        );
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

        // A late row on the same id.
        let late = comment_on_root(&keys, &root, 899, "after the end");
        deliver_frame(&mut state, &sub_id, &late, &tx).await;
        let recon = state
            .reconstructions
            .get(&root)
            .expect("the root is still tracked");
        assert!(
            recon.abandoned_reason().is_none(),
            "an unadmitted frame is not a contradiction — it never reached the owner"
        );
        assert_eq!(
            recon
                .finished_streams()
                .iter()
                .map(|s| s.len())
                .sum::<usize>(),
            1,
            "and it is not in the history the completed page retained"
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

        // Page B. Under a name of its own — but that is asserted at the *end*,
        // deliberately. Two attempts sharing one name is the mechanism of the
        // defect, so checking it here would make this test report "the ids are
        // equal" and stop, in place of the outcome the ids exist to prevent.
        let page_b = open_page_under(&mut state, &root).await;
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

        let wanted = state
            .reconstructions
            .get(&root)
            .expect("tracked")
            .pages_wanted();
        assert_eq!(wanted.len(), 1, "the stream wants another page: {wanted:?}");
        let (_, next_until, _) = wanted[0];
        assert!(
            next_until < 1_000,
            "and from an advanced bound: {next_until}"
        );

        // No `close_active` anywhere in here, and page two gets a name of its
        // own rather than inheriting page one's.
        let second = open_page_under(&mut state, &root).await;
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
        let buffered = VecDeque::from(vec![
            RelayMessage::Event {
                subscription_id: sub_id.clone(),
                event: Box::new(row.clone()),
            },
            RelayMessage::Eose {
                subscription_id: sub_id.clone(),
            },
            RelayMessage::Closed {
                subscription_id: sub_id.clone(),
                message: "error: whatever".to_string(),
            },
        ]);

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

    /// While the rate-limit gate is armed, an observer frame (kind 24200) is
    /// parked — not silently dropped — and delivered by the drain once the
    /// gate clears. A typing indicator in the same window stays dropped.
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

        // Typing indicator while gated: still dropped, not parked.
        let typing = EventBuilder::new(Kind::Custom(KIND_TYPING_INDICATOR as u16), "")
            .tags([Tag::parse(["h", &Uuid::new_v4().to_string()]).unwrap()])
            .sign_with_keys(&keys)
            .expect("sign typing indicator");
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
            "typing indicators must not be parked"
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
