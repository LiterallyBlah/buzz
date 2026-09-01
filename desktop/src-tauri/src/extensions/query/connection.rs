//! The shared relay socket behind extension subscriptions, and the §5
//! `subscribe` handler that opens one on it.
//!
//! A private child of `extensions::query`, so it sits inside the same seal as
//! [`super::construction`]: it can build constrained filters and it can count
//! them, and it still cannot hand them to the generic relay helper.
//!
//! # One socket per `(relay, authenticated identity)`
//!
//! Not one per subscription. A relay authenticates the *connection*, so a
//! second socket means a second NIP-42 round trip for authority the host
//! already holds, and N sockets for N subscriptions makes an extension's
//! footprint a function of how often it calls `subscribe`. Sharing also gives
//! the authenticated pubkey a single owner: [`IdentityWitness`] is minted once,
//! by the code that completed the handshake, and every branch opened on the
//! socket inherits it.
//!
//! The key includes the identity precisely so a switch cannot silently reuse a
//! socket: a different identity is a different key, hence a different socket
//! and a fresh handshake.
//!
//! # The reader owns nothing it decides
//!
//! It parses bytes into [`RelayFrame`]s and asks the registry which
//! subscription owns each branch. Admission travels with the subscription (see
//! [`SubAdmission`]), because a multiplexing reader that supplied the check
//! itself would be choosing whose authority to apply to an arriving event.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::sync::{mpsc, watch};
use tokio_tungstenite::tungstenite::{protocol::WebSocketConfig, Message};

use crate::relay::subscribe::{
    authenticate, parse_frame, IdentityWitness, RelayFrame, TransportError, MAX_WS_FRAME_BYTES,
    MAX_WS_MESSAGE_BYTES,
};

use super::super::dispatch::{code, BridgeReply};
use super::construction::construct_filters;
use super::flow::{StreamBatch, STREAM_ACK_TIMEOUT};
use super::registry::{
    quota, registry, ConnectionInstance, ConnectionKey, Delivery, StreamSink, SubAdmission,
};
use super::subscription::{
    on_notice, open_subscription, Aggregate, CloseReason, OpenFailure, INITIAL_EOSE_DEADLINE,
    MAX_BRANCHES_PER_SUB,
};
use super::{validate_request, QueryError, QueryRevalidation};

/// The Tauri event the host frontend forwards to the owning port.
///
/// One event name for every stream frame, carrying the lease. The frontend
/// delivers only to the port whose lease matches, which is the second of the
/// two independent walls — the registry enforces the first by keying on the
/// lease, and a released lease matches nothing on either side.
pub(super) const STREAM_EVENT: &str = "extension-stream";

/// Queued outbound relay frames before a `subscribe` is refused.
///
/// Bounded, and a full queue fails the open rather than waiting: an unbounded
/// queue in front of a wedged socket is how a dead relay turns into unbounded
/// host memory.
const OUTBOUND_QUEUE: usize = 256;

/// Priority-bounded control bursts. They are separate from data so a full REQ
/// queue cannot starve teardown; if this queue itself is full the connection is
/// aborted, which reliably drops every relay-side subscription on the socket.
const CONTROL_QUEUE: usize = 64;

enum OutboundCommand {
    Burst(Vec<String>),
}

/// A live, authenticated socket to one relay, shared by every subscription
/// opened under one identity.
pub(super) struct Connection {
    outbound: mpsc::Sender<OutboundCommand>,
    control: mpsc::Sender<OutboundCommand>,
    cancel: watch::Sender<bool>,
    witness: IdentityWitness,
    instance: ConnectionInstance,
    alive: Arc<AtomicBool>,
}

impl Connection {
    pub(super) fn witness(&self) -> &IdentityWitness {
        &self.witness
    }

    pub(super) fn instance(&self) -> &ConnectionInstance {
        &self.instance
    }

    /// Queue one relay frame.
    ///
    /// `try_send` rather than `send`: a full queue or a dead writer must fail
    /// the caller now. Awaiting here would block a `subscribe` on a relay that
    /// has stopped reading, and the caller is holding a committed reservation.
    pub(super) fn send_reqs(&self, frames: Vec<String>) -> Result<(), ()> {
        self.outbound
            .try_send(OutboundCommand::Burst(frames))
            .map_err(|_| ())
    }

    pub(super) fn send_closes(&self, frames: Vec<String>) -> Result<(), ()> {
        if self
            .control
            .try_send(OutboundCommand::Burst(frames))
            .is_err()
        {
            self.abort();
            return Err(());
        }
        Ok(())
    }

    fn abort(&self) {
        self.alive.store(false, Ordering::Release);
        let _ = self.cancel.send(true);
    }

    fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Acquire)
    }
}

fn reusable(connection: &Connection) -> bool {
    connection.is_alive()
}

/// Every open extension-subscription socket, by `(relay url, identity)`.
#[derive(Default)]
pub(super) struct ConnectionManager {
    conns: tokio::sync::Mutex<HashMap<ConnectionKey, Arc<Connection>>>,
}

impl ConnectionManager {
    /// Reuse this identity's socket to this relay, or open and authenticate one.
    ///
    /// The lock is held across the connect so two concurrent `subscribe` calls
    /// cannot each open a socket and race to install it — the loser's would be
    /// dropped with its subscriptions' branches already on it.
    pub(super) async fn get_or_open(
        &self,
        relay_url: &str,
        keys: &nostr::Keys,
        sink: &StreamSink,
    ) -> Result<Arc<Connection>, TransportError> {
        let key: ConnectionKey = (relay_url.to_string(), keys.public_key().to_hex());
        let mut conns = self.conns.lock().await;

        if let Some(existing) = conns.get(&key) {
            if reusable(existing) {
                return Ok(Arc::clone(existing));
            }
            // The writer task ended, so the socket is gone. Reusing the handle
            // would queue REQs nobody will ever send.
            conns.remove(&key);
        }

        let opened = open(relay_url, keys, key.clone(), Arc::clone(sink)).await?;
        conns.insert(key, Arc::clone(&opened));
        Ok(opened)
    }

    async fn remove_if_current(&self, instance: &ConnectionInstance) {
        let mut conns = self.conns.lock().await;
        let is_current = conns
            .get(&instance.key)
            .is_some_and(|connection| connection.instance() == instance);
        if is_current {
            conns.remove(&instance.key);
        }
    }
}

pub(super) fn connections() -> &'static ConnectionManager {
    static MANAGER: std::sync::OnceLock<ConnectionManager> = std::sync::OnceLock::new();
    MANAGER.get_or_init(ConnectionManager::default)
}

/// Monotonic connection generation, so a witness names a specific socket.
fn next_generation() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// Connect, authenticate, and spawn the reader and writer.
///
/// **No `REQ` is possible before the witness exists**, because the witness is
/// what this returns: [`authenticate`] is fail-closed at every step, and its
/// error short-circuits before either task is spawned.
fn websocket_config() -> WebSocketConfig {
    WebSocketConfig::default()
        .max_message_size(Some(MAX_WS_MESSAGE_BYTES))
        .max_frame_size(Some(MAX_WS_FRAME_BYTES))
}

async fn send_burst<W>(write: &mut W, frames: Vec<String>) -> Result<(), ()>
where
    W: SinkExt<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
{
    for text in frames {
        write
            .send(Message::Text(text.into()))
            .await
            .map_err(|_| ())?;
    }
    Ok(())
}

async fn open(
    relay_url: &str,
    keys: &nostr::Keys,
    key: ConnectionKey,
    sink: StreamSink,
) -> Result<Arc<Connection>, TransportError> {
    // Decoder ceilings are installed before tungstenite reads a frame. The
    // application-side check remains defence in depth, not the allocation wall.
    let (socket, _) =
        tokio_tungstenite::connect_async_with_config(relay_url, Some(websocket_config()), false)
            .await
            .map_err(|_| TransportError::Connect)?;
    let (mut write, mut read) = socket.split();

    let generation = next_generation();
    let witness = authenticate(&mut read, &mut write, keys, relay_url, generation).await?;
    let instance = ConnectionInstance {
        key: key.clone(),
        generation: witness.connection_generation(),
    };
    let alive = Arc::new(AtomicBool::new(true));
    let (cancel, cancel_rx) = watch::channel(false);
    let (outbound, mut queued) = mpsc::channel::<OutboundCommand>(OUTBOUND_QUEUE);
    let (control, mut controls) = mpsc::channel::<OutboundCommand>(CONTROL_QUEUE);

    let writer_alive = Arc::clone(&alive);
    let writer_cancel = cancel.clone();
    let mut writer_cancel_rx = cancel_rx.clone();
    tokio::spawn(async move {
        loop {
            let command = tokio::select! {
                biased;
                changed = writer_cancel_rx.changed() => {
                    if changed.is_err() || *writer_cancel_rx.borrow() { break; }
                    continue;
                }
                command = controls.recv() => command,
                command = queued.recv() => command,
            };
            let Some(OutboundCommand::Burst(frames)) = command else {
                break;
            };
            if send_burst(&mut write, frames).await.is_err() {
                break;
            }
        }
        writer_alive.store(false, Ordering::Release);
        let _ = writer_cancel.send(true);
        let _ = write.close().await;
    });

    let reader_alive = Arc::clone(&alive);
    let reader_cancel = cancel.clone();
    let reader_instance = instance.clone();
    tokio::spawn(async move {
        pump(read, sink, reader_instance.clone(), cancel_rx).await;
        reader_alive.store(false, Ordering::Release);
        let _ = reader_cancel.send(true);
        connections().remove_if_current(&reader_instance).await;
    });

    Ok(Arc::new(Connection {
        outbound,
        control,
        cancel,
        witness,
        instance,
        alive,
    }))
}

/// Which branch a frame belongs to, or `None` for connection-scoped frames.
fn branch_of(frame: &RelayFrame) -> Option<&str> {
    match frame {
        RelayFrame::Event { sub_id, .. }
        | RelayFrame::Eose { sub_id }
        | RelayFrame::Closed { sub_id, .. } => Some(sub_id),
        RelayFrame::Notice { .. } | RelayFrame::Other => None,
    }
}

/// Read frames until the socket ends, routing each to the subscription that
/// owns its branch.
///
/// Multiplexes from the first frame. Ending the loop is terminal for every
/// subscription this socket carried: v1 has no reconnect, and leaving them live
/// against a dead socket would hold their branch budget forever while
/// delivering nothing.
async fn pump<S>(
    mut read: S,
    sink: StreamSink,
    instance: ConnectionInstance,
    mut cancel: watch::Receiver<bool>,
) where
    S: StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    loop {
        let message = tokio::select! {
            biased;
            changed = cancel.changed() => {
                if changed.is_err() || *cancel.borrow() { break; }
                continue;
            }
            message = read.next() => message,
        };
        let Some(message) = message else {
            break;
        };
        let text = match message {
            Ok(Message::Text(text)) => text,
            // Ping/pong and binary are not this protocol; a close or an error
            // ends the socket.
            Ok(Message::Close(_)) | Err(_) => break,
            Ok(_) => continue,
        };
        if text.len() > MAX_WS_MESSAGE_BYTES {
            break;
        }
        let Ok(frame) = parse_frame(text.as_str()) else {
            // A frame this host cannot parse is the relay's problem, not a
            // reason to tear down subscriptions that are working.
            continue;
        };

        match branch_of(&frame) {
            None => {
                if let RelayFrame::Notice { message } = &frame {
                    if on_notice(message) {
                        crate::relay_admission::activate_rate_limit(None);
                    }
                }
            }
            Some(branch) => {
                let branch = branch.to_string();
                if let Some(delivery) = registry().route_by_branch(&instance, &branch, frame) {
                    deliver(&sink, delivery);
                }
            }
        }
    }

    for delivery in registry().close_for_connection(&instance) {
        deliver(&sink, delivery);
    }
}

/// Hand one routed frame to the port, and take its branches down at the relay.
pub(super) fn deliver(sink: &StreamSink, delivery: Delivery) {
    if delivery.arm_gate {
        crate::relay_admission::activate_rate_limit(None);
    }
    for batch in delivery.batches {
        if sink(&batch).is_err() {
            if let Some(terminal) =
                registry().close_for_flow_violation(&batch.generation, &batch.sub)
            {
                for terminal_batch in terminal.batches {
                    let _ = sink(&terminal_batch);
                }
            }
            continue;
        }
        if !batch.terminal {
            let lease = batch.generation.clone();
            let sub = batch.sub.clone();
            let seq = batch.seq;
            let token = batch.token.clone();
            let sink = Arc::clone(sink);
            tokio::spawn(async move {
                tokio::time::sleep(STREAM_ACK_TIMEOUT).await;
                if let Some(timeout) = registry().close_on_ack_timeout(&lease, &sub, seq, &token) {
                    deliver(&sink, timeout);
                }
            });
        }
    }
}

/// The `CLOSE` burst for one subscription's branches, bound to its socket.
///
/// Handed to the registry entry so that **every** removal path can stop the
/// relay, not just the one that happens to be holding the socket. `try_send`,
/// so a dead or wedged socket fails here rather than blocking a teardown.
fn relay_closer(connection: &Arc<Connection>) -> super::registry::RelayCloser {
    let connection = Arc::clone(connection);
    Box::new(move |branches: &[String]| {
        let frames = branches
            .iter()
            .map(|branch| format!("[\"CLOSE\",{}]", serde_json::json!(branch)))
            .collect();
        // A full priority queue aborts the socket; silently dropping CLOSE is
        // forbidden because it leaves ownerless relay subscriptions alive.
        let _ = connection.send_closes(frames);
    })
}

/// The production sink: a Tauri event carrying the lease and the §2 frame.
pub(super) fn app_sink<R: tauri::Runtime>(app: &tauri::AppHandle<R>) -> StreamSink {
    let app = app.clone();
    Arc::new(move |batch: &StreamBatch| {
        use tauri::Emitter as _;
        app.emit(STREAM_EVENT, batch).map_err(|_| ())
    })
}

pub(super) fn activate_prepared<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    lease: &str,
    sub: &str,
) {
    if let Some(delivery) = registry().activate(lease, sub) {
        deliver(&app_sink(app), delivery);
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn acknowledge_batch<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    lease: &str,
    sub: &str,
    seq: u64,
    token: &str,
    frame_count: usize,
    encoded_bytes: usize,
) {
    if let Some(delivery) =
        registry().acknowledge(lease, sub, seq, token, frame_count, encoded_bytes)
    {
        deliver(&app_sink(app), delivery);
    }
}

pub(super) fn report_flow_violation<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    lease: &str,
    sub: &str,
) {
    if let Some(delivery) = registry().close_for_flow_violation(lease, sub) {
        deliver(&app_sink(app), delivery);
    }
}

async fn reserve_before_network<T, NetworkFut>(
    quota: &Arc<super::subscription::SubscriptionQuota>,
    identity_pubkey: &str,
    extension_id: &str,
    branches: usize,
    network: impl FnOnce() -> NetworkFut,
) -> Result<(super::subscription::Reservation, T), OpenFailure>
where
    NetworkFut: std::future::Future<Output = Result<T, ()>>,
{
    let reservation = quota
        .reserve(identity_pubkey, extension_id, branches)
        .ok_or(OpenFailure::QuotaExhausted)?;
    let connected = network().await.map_err(|_| OpenFailure::BranchOpenFailed)?;
    Ok((reservation, connected))
}

/// §5 `subscribe({ filter }) → { sub }`.
///
/// The same authority pipeline as `query.events` up to the point they diverge:
/// validate → identity → granted pairs → construct filters, which *is* the
/// initial authority check because it refuses unless granted pairs survive the
/// request. From there a subscription differs in that its authority must keep
/// holding, so the revalidation closure and the per-event verifier are stored
/// with the subscription rather than run once.
pub(super) async fn subscribe<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    extension_id: &str,
    lease: &str,
    params: Option<Value>,
) -> BridgeReply {
    use tauri::Manager as _;

    let Some(params) = params else {
        return BridgeReply::err(code::INVALID_PARAMS, "params must be an object");
    };
    let request = match validate_request(&params) {
        Ok(request) => request,
        Err(error) => return error.into_reply(),
    };

    let state = app.state::<crate::AppState>();
    let keys = match super::super::publish::signing_identity(&state) {
        Ok(keys) => keys,
        Err(_) => return BridgeReply::err(code::DENIED, "missing scope: read"),
    };
    let identity_pubkey = keys.public_key().to_hex();

    let Some(grant_db) = super::super::dispatch::grant_db_path(app).ok() else {
        return BridgeReply::err(code::DENIED, "missing scope: read");
    };
    let Ok(conn) = super::super::grants::open_grant_db(&grant_db) else {
        return BridgeReply::err(code::DENIED, "missing scope: read");
    };
    let granted = super::super::grants::list_read_pairs(&conn, &identity_pubkey, extension_id);

    let filters = match construct_filters(&granted, &request) {
        Ok(filters) => filters,
        Err(error) => return error.into_reply(),
    };

    // One relay branch per emitted filter. The count comes from inside the
    // seal; the filters themselves never leave it.
    let branches = filters.filter_count();
    if branches > MAX_BRANCHES_PER_SUB {
        return QueryError::QuotaExceeded(
            "this subscription would span more channels than one stream may carry",
        )
        .into_reply();
    }

    let pairs: Vec<(u32, String)> = filters.pairs().to_vec();
    let revalidation = QueryRevalidation {
        lease,
        extension_id,
        identity_at_entry: &identity_pubkey,
        pairs_at_entry: &pairs,
        state: &state,
        grant_db: Some(grant_db.clone()),
    };

    let sub = uuid::Uuid::new_v4().to_string();
    let branch_ids: Vec<String> = (0..branches)
        .map(|_| uuid::Uuid::new_v4().to_string())
        .collect();
    let Some(aggregate) = Aggregate::new(branch_ids.clone()) else {
        return BridgeReply::err(code::INTERNAL, "could not open a subscription");
    };
    // Built now, from inside the seal, so the burst and the aggregate span the
    // same branch ids by construction rather than by two callers agreeing.
    let Some(requests) = filters.req_frames(&branch_ids) else {
        return BridgeReply::err(code::INTERNAL, "could not open a subscription");
    };

    let relay_url = crate::relay::relay_ws_url_with_override(&state);
    let sink = app_sink(app);

    // The helper owns both steps, so a quota mutant cannot accidentally leave
    // connect/auth ahead of the reservation while a pure quota test stays green.
    let (reservation, connection) = match reserve_before_network(
        quota(),
        &identity_pubkey,
        extension_id,
        branches,
        || async {
            connections()
                .get_or_open(&relay_url, &keys, &sink)
                .await
                .map_err(|_| ())
        },
    )
    .await
    {
        Ok(opened) => opened,
        Err(OpenFailure::QuotaExhausted) => {
            return QueryError::QuotaExceeded(
                "this extension holds as many live subscriptions as it may",
            )
            .into_reply();
        }
        Err(_) => return BridgeReply::err(code::INTERNAL, "could not reach the relay"),
    };

    // The witness is the connection's own evidence of who authenticated. A
    // mismatch means this socket is not speaking for the identity the grant
    // admitted, and no branch may be opened on it.
    if connection.witness().authenticated_pubkey() != identity_pubkey {
        return BridgeReply::err(code::DENIED, "missing scope: read");
    }

    let admission = live_admission(
        app.clone(),
        lease.to_string(),
        extension_id.to_string(),
        identity_pubkey.clone(),
        grant_db.clone(),
        filters,
    );

    let outcome = open_subscription(
        reservation,
        crate::relay_admission::wait_for_rate_limit,
        || revalidation.check().map_err(|_| CloseReason::AuthorityLost),
        |reservation| {
            registry().insert(
                lease,
                &sub,
                aggregate,
                admission,
                Arc::clone(&sink),
                relay_closer(&connection),
                reservation,
                connection.instance().clone(),
            );
        },
        || {
            let connection = Arc::clone(&connection);
            async move { connection.send_reqs(requests) }
        },
        || {
            let _ = registry().close_one(lease, &sub, CloseReason::RelayClosed);
        },
    )
    .await;

    if let Err(failure) = outcome {
        return match failure {
            OpenFailure::QuotaExhausted => QueryError::QuotaExceeded(
                "this extension holds as many live subscriptions as it may",
            )
            .into_reply(),
            OpenFailure::AuthorityLost(_) => BridgeReply::err(code::DENIED, "missing scope: read"),
            OpenFailure::BranchOpenFailed => {
                BridgeReply::err(code::INTERNAL, "could not reach the relay")
            }
        };
    }

    // Prepared only. Rust retains every frame until the frontend writes this
    // correlated reply, adopts the exact sub, and returns the internal
    // exact-generation activation receipt.
    let reply = BridgeReply::ok(serde_json::json!({ "sub": sub }));

    // Arm the initial-EOSE deadline. Nothing else bounds the stored phase: a
    // relay that accepts every `REQ` and then says nothing leaves the aggregate
    // waiting forever, holding its branch budget and delivering a stream the
    // extension cannot tell from an empty channel. On expiry no public `eose`
    // is invented — the subscription closes with `eose_deadline`, which is a
    // fact the extension can act on.
    {
        let lease = lease.to_string();
        let sub = sub.clone();
        let sink = Arc::clone(&sink);
        tokio::spawn(async move {
            tokio::time::sleep(INITIAL_EOSE_DEADLINE).await;
            if let Some(delivery) = registry().close_on_eose_deadline(&lease, &sub) {
                deliver(&sink, delivery);
            }
        });
    }

    reply
}

/// Freeze this subscription's admission into two closures.
///
/// Everything they read is captured now, at the moment authority was granted —
/// except the grant store and the lease map, which are re-read on every use
/// because a revocation between then and now is exactly what must be caught.
fn live_admission<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    lease: String,
    extension_id: String,
    identity_pubkey: String,
    grant_db: std::path::PathBuf,
    filters: super::construction::ConstrainedFilters,
) -> SubAdmission {
    use tauri::Manager as _;

    let filters = Arc::new(filters);
    let pairs: Arc<Vec<(u32, String)>> = Arc::new(filters.pairs().to_vec());

    let authority = {
        let app = app.clone();
        let lease = lease.clone();
        let extension_id = extension_id.clone();
        let identity_pubkey = identity_pubkey.clone();
        let grant_db = grant_db.clone();
        let pairs = Arc::clone(&pairs);
        Box::new(move || {
            let state = app.state::<crate::AppState>();
            QueryRevalidation {
                lease: &lease,
                extension_id: &extension_id,
                identity_at_entry: &identity_pubkey,
                pairs_at_entry: &pairs,
                state: &state,
                grant_db: Some(grant_db.clone()),
            }
            .check()
            .map_err(|_| CloseReason::AuthorityLost)
        }) as Box<dyn Fn() -> Result<(), CloseReason> + Send + Sync>
    };

    let verify = {
        let filters = Arc::clone(&filters);
        Box::new(move |event: &nostr::Event| {
            let Ok(conn) = super::super::grants::open_grant_db(&grant_db) else {
                // Fail closed: a store we cannot open has granted nothing.
                return false;
            };
            super::verify_event(event, &filters, &conn, &identity_pubkey, &extension_id)
        }) as Box<dyn Fn(&nostr::Event) -> bool + Send + Sync>
    };

    SubAdmission { authority, verify }
}

#[cfg(test)]
#[path = "connection_tests.rs"]
mod connection_tests;
