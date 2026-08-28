//! §5 `subscribe` — aggregation, quota and the public-subscription lifecycle.
//!
//! A private child of `extensions::query`, so it inherits that module's seal:
//! `construct_filters`, `matches_any` and `verify_event` are reachable here and
//! nowhere else. The seal is **not** widened for 5b — that would undo the whole
//! point of Boundary 1's blocker.
//!
//! This module is the authority half. `relay::subscribe` owns bytes and frames;
//! nothing here talks to a socket.
//!
//! # What the aggregate is for
//!
//! One extension-facing `sub` spans `N` physical relay branches — one per
//! granted channel, because a multi-`#h` filter collapses to global at the
//! relay and delivers nothing. The aggregate is what makes those `N` look like
//! one ordered stream: it holds the public `eose` until **every** branch has
//! EOSE'd on a **raw** relay frame, dedups across branches, and closes as a
//! whole rather than silently narrowing when one branch dies.

// Consumed by the bridge handler and the reader task, which land next. Remove
// this when they do — a permanent allow here would hide an orphaned module.
#![allow(dead_code)]

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use super::super::dispatch::{code, BridgeReply};

/// Most physical branches one public subscription may own.
///
/// A subscription spanning more granted channels than this is refused rather
/// than partially served: a silently narrowed stream is the failure mode the
/// aggregate exists to prevent.
pub(super) const MAX_BRANCHES_PER_SUB: usize = 32;

/// Most physical branches one `(identity, extension)` may hold at once.
///
/// Keyed on the pair rather than the port, so closing and re-opening a port
/// cannot multiply an extension's footprint.
pub(super) const MAX_BRANCHES_PER_EXTENSION: usize = 128;

/// Most stored events one aggregate may hold before its public `eose`.
///
/// The pre-EOSE window is the only place the host buffers on the extension's
/// behalf, and it is bounded because the relay decides how much arrives.
pub(super) const MAX_PRE_EOSE_EVENTS: usize = 2048;

/// Most bytes of stored events one aggregate may buffer before its `eose`.
pub(super) const MAX_PRE_EOSE_BYTES: usize = 4 * 1024 * 1024;

/// Why an aggregate ended. Normalised — no relay text reaches an extension.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum CloseReason {
    /// The extension asked, or its port/lease went away.
    Unsubscribed,
    /// Authority changed under the subscription: revoked grant, lost lease,
    /// identity switch, witness mismatch.
    AuthorityLost,
    /// A branch was CLOSED by the relay, or the transport ended. Terminal in
    /// v1: no silent re-REQ.
    RelayClosed,
    /// A named bound was reached. Closing beats evicting, because what would be
    /// evicted is authority-relevant state.
    BoundExceeded,
    /// No public `eose` arrived inside the initial-EOSE deadline.
    EoseDeadline,
}

impl CloseReason {
    /// The bounded string an extension sees.
    pub(super) fn as_wire(&self) -> &'static str {
        match self {
            CloseReason::Unsubscribed => "unsubscribed",
            CloseReason::AuthorityLost => "authority_lost",
            CloseReason::RelayClosed => "relay_closed",
            CloseReason::BoundExceeded => "bound_exceeded",
            CloseReason::EoseDeadline => "eose_deadline",
        }
    }
}

/// What the aggregate wants delivered, in response to one input.
///
/// Returned rather than emitted, so ordering is the caller's serial queue and
/// this type stays testable without a port.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Emit {
    /// Nothing to deliver — a duplicate, or a branch that is not the last.
    Nothing,
    /// Deliver this event.
    Event(Box<nostr::Event>),
    /// Every branch has EOSE'd; deliver the single public `eose`.
    Eose,
    /// Terminal. Nothing may follow.
    Closed(CloseReason),
}

/// What admission decided about one arriving event.
///
/// Three outcomes, not two, and that is the whole point: `verify_event` returns
/// a `bool` and so cannot distinguish "this event is bad" from "this extension
/// is no longer allowed to see anything". Collapsing them means a revoked grant
/// looks like a malformed event and the stream carries on.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Admission {
    /// Authority changed under the subscription. The **whole aggregate** ends.
    CloseAggregate(CloseReason),
    /// Authority holds, but this event is not one the extension may see —
    /// bad signature, misdelivered, matching no constructed filter. Dropped;
    /// the stream continues.
    DropEvent,
    /// Deliver it.
    Deliver,
}

/// Two-stage admission, in the order the contract fixes.
///
/// **Authority first, and `verify` is not called if authority fails.** That
/// ordering is the safety property, not a style preference: if a per-event
/// check ran first, a revoked grant arriving alongside a malformed event would
/// be reported as a dropped event and the subscription would keep streaming
/// under authority it no longer has. Taking the closures rather than the
/// collaborators keeps the ordering testable without a port, a socket or an
/// app handle — and lets a probe observe that `verify` really was not reached.
pub(super) fn admit(
    authority: impl FnOnce() -> Result<(), CloseReason>,
    verify: impl FnOnce() -> bool,
) -> Admission {
    if let Err(reason) = authority() {
        return Admission::CloseAggregate(reason);
    }
    if verify() {
        Admission::Deliver
    } else {
        Admission::DropEvent
    }
}

/// A reservation against the `(identity, extension)` branch budget.
///
/// **Exactly-once release is structural.** The contract requires rollback on
/// every failed creation path — branch-open failure, witness mismatch, teardown
/// mid-open, EOSE-deadline expiry — and enumerating those paths in prose is how
/// one gets missed. Releasing in `Drop` instead means the only way to keep a
/// reservation is to keep the value alive: an early return releases it, a panic
/// releases it, and [`Reservation::commit`] is what a caller uses to say the
/// subscription is live and the budget should stay spent.
pub(super) struct Reservation {
    quota: Arc<SubscriptionQuota>,
    key: (String, String),
    branches: usize,
    /// Cleared by `commit` or by an explicit `release`, so `Drop` is a no-op
    /// afterwards — the "exactly once" half.
    outstanding: bool,
}

impl Reservation {
    pub(super) fn branches(&self) -> usize {
        self.branches
    }

    /// The subscription is live; the budget stays spent until teardown.
    pub(super) fn commit(mut self) -> CommittedReservation {
        self.outstanding = false;
        CommittedReservation {
            quota: self.quota.clone(),
            key: self.key.clone(),
            branches: self.branches,
            released: false,
        }
    }

    /// Give the budget back now. Idempotent.
    pub(super) fn release(&mut self) {
        if self.outstanding {
            self.quota.give_back(&self.key, self.branches);
            self.outstanding = false;
        }
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        self.release();
    }
}

/// A live subscription's hold on the budget, released at teardown.
pub(super) struct CommittedReservation {
    quota: Arc<SubscriptionQuota>,
    key: (String, String),
    branches: usize,
    released: bool,
}

impl CommittedReservation {
    pub(super) fn release(&mut self) {
        if !self.released {
            self.quota.give_back(&self.key, self.branches);
            self.released = true;
        }
    }
}

impl Drop for CommittedReservation {
    fn drop(&mut self) {
        self.release();
    }
}

/// The host-side branch budget, keyed `(identity_pubkey, extension_id)`.
#[derive(Default)]
pub(super) struct SubscriptionQuota {
    held: Mutex<HashMap<(String, String), usize>>,
}

impl SubscriptionQuota {
    pub(super) fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Reserve `branches` atomically, or refuse.
    ///
    /// All-or-nothing on purpose: a partial reservation would leave a
    /// subscription half-funded and the rollback path ambiguous. The check and
    /// the increment happen under one lock, so two concurrent `subscribe` calls
    /// cannot both see room for the last branch.
    pub(super) fn reserve(
        self: &Arc<Self>,
        identity_pubkey: &str,
        extension_id: &str,
        branches: usize,
    ) -> Option<Reservation> {
        if branches == 0 || branches > MAX_BRANCHES_PER_SUB {
            return None;
        }
        let key = (identity_pubkey.to_string(), extension_id.to_string());
        let mut held = self.held.lock().ok()?;
        let current = held.get(&key).copied().unwrap_or(0);
        let next = current.checked_add(branches)?;
        if next > MAX_BRANCHES_PER_EXTENSION {
            return None;
        }
        held.insert(key.clone(), next);
        Some(Reservation {
            quota: self.clone(),
            key,
            branches,
            outstanding: true,
        })
    }

    fn give_back(&self, key: &(String, String), branches: usize) {
        let Ok(mut held) = self.held.lock() else {
            return;
        };
        let Some(current) = held.get(key).copied() else {
            return;
        };
        let next = current.saturating_sub(branches);
        if next == 0 {
            held.remove(key);
        } else {
            held.insert(key.clone(), next);
        }
    }

    /// Branches currently held by this pair. For assertions and teardown.
    pub(super) fn held_by(&self, identity_pubkey: &str, extension_id: &str) -> usize {
        self.held
            .lock()
            .ok()
            .and_then(|held| {
                held.get(&(identity_pubkey.to_string(), extension_id.to_string()))
                    .copied()
            })
            .unwrap_or(0)
    }
}

/// The `N`-branch aggregate behind one extension-facing `sub`.
///
/// Holds no filters and no grants: admission is the caller's two-stage check.
/// This owns only what makes `N` branches read as one ordered stream.
pub(super) struct Aggregate {
    /// Every branch this sub opened. Fixed at construction — v1 never adds a
    /// branch to a live sub, which is what keeps late-join EOSE out of scope.
    branches: HashSet<String>,
    /// Branches that have delivered a **raw** relay EOSE.
    eosed: HashSet<String>,
    /// Ids seen before the public `eose`, for cross-branch dedup. Cleared once
    /// the aggregate EOSEs, because after that the window it guards is over.
    pre_eose_ids: HashSet<nostr::EventId>,
    pre_eose_bytes: usize,
    /// Every branch has EOSE'd — the stored phase is over.
    eose_reached: bool,
    /// The single public `eose` has actually been handed out.
    ///
    /// Distinct from [`Self::eose_reached`] because the two can be separated by
    /// the reply: the phase changes when the last branch reports, but nothing
    /// may be emitted until the `{sub}` reply is out.
    eose_emitted: bool,
    closed: Option<CloseReason>,
    /// Has the correlated `{sub}` reply been written to the port yet?
    ///
    /// The relay can answer before the host has finished replying to the
    /// `subscribe` call that caused it. Delivering those events first would
    /// hand an extension frames for a `sub` it has not been told the id of —
    /// unroutable at best, and at worst attributed to whatever sub it *has*
    /// seen. So they are held until the reply is out.
    reply_written: bool,
    /// Events that arrived before the reply, in arrival order.
    awaiting_reply: Vec<nostr::Event>,
}

impl Aggregate {
    /// Build an aggregate over exactly these branch ids.
    ///
    /// Refuses an empty set: a subscription with no branches would EOSE
    /// immediately and look like an empty channel, when in fact nothing was
    /// ever asked of the relay.
    pub(super) fn new(branch_ids: Vec<String>) -> Option<Self> {
        if branch_ids.is_empty() || branch_ids.len() > MAX_BRANCHES_PER_SUB {
            return None;
        }
        let branches: HashSet<String> = branch_ids.into_iter().collect();
        Some(Self {
            branches,
            eosed: HashSet::new(),
            pre_eose_ids: HashSet::new(),
            pre_eose_bytes: 0,
            eose_reached: false,
            eose_emitted: false,
            closed: None,
            reply_written: false,
            awaiting_reply: Vec::new(),
        })
    }

    /// The `{sub}` reply has been written; release anything held behind it.
    ///
    /// Returns the held events **in arrival order**, which is the whole point:
    /// the queue is serial, so what the relay sent first is what the extension
    /// sees first, even across the reply boundary.
    pub(super) fn mark_reply_written(&mut self) -> Vec<Emit> {
        if self.reply_written {
            return Vec::new();
        }
        self.reply_written = true;
        if self.closed.is_some() {
            // Closed before the reply landed: the held events are not
            // deliverable, and `closed` has already been emitted.
            self.awaiting_reply.clear();
            return Vec::new();
        }
        let mut out: Vec<Emit> = std::mem::take(&mut self.awaiting_reply)
            .into_iter()
            .map(|event| Emit::Event(Box::new(event)))
            .collect();
        // If every branch EOSE'd while the reply was still in flight, the
        // public `eose` was held too — it must land *after* the stored events
        // it terminates, not before them.
        if self.eose_reached && !self.eose_emitted {
            self.eose_emitted = true;
            out.push(Emit::Eose);
        }
        out
    }

    pub(super) fn reply_written(&self) -> bool {
        self.reply_written
    }

    pub(super) fn is_closed(&self) -> bool {
        self.closed.is_some()
    }

    /// Has the single public `eose` been handed out?
    pub(super) fn has_eosed(&self) -> bool {
        self.eose_emitted
    }

    pub(super) fn branch_count(&self) -> usize {
        self.branches.len()
    }

    /// Close terminally. Idempotent, and the **first** reason wins.
    ///
    /// A later cause must not relabel why a stream ended — the first thing that
    /// went wrong is the true one, and a teardown that overwrote it with
    /// "unsubscribed" would erase an authority failure.
    pub(super) fn close(&mut self, reason: CloseReason) -> Emit {
        if let Some(existing) = &self.closed {
            return Emit::Closed(existing.clone());
        }
        self.closed = Some(reason.clone());
        Emit::Closed(reason)
    }

    /// A raw relay EOSE for one branch.
    ///
    /// Emits the single public `eose` only when every branch has reported, and
    /// only once. An EOSE for an unknown branch is ignored rather than counted:
    /// counting it could complete the aggregate early, which is the same defect
    /// as inventing an EOSE on a timer.
    pub(super) fn on_branch_eose(&mut self, branch_id: &str) -> Emit {
        if self.closed.is_some() || self.eose_reached {
            return Emit::Nothing;
        }
        if !self.branches.contains(branch_id) {
            return Emit::Nothing;
        }
        self.eosed.insert(branch_id.to_string());
        if self.eosed.len() < self.branches.len() {
            return Emit::Nothing;
        }
        self.eose_reached = true;
        // The dedup window guarded the stored phase; that phase is over.
        self.pre_eose_ids.clear();
        self.pre_eose_bytes = 0;
        if !self.reply_written {
            // Held: the `eose` may not overtake the reply, nor the stored
            // events it terminates. `mark_reply_written` releases it.
            return Emit::Nothing;
        }
        self.eose_emitted = true;
        Emit::Eose
    }

    /// An event that has already passed two-stage admission.
    ///
    /// Before the public `eose` it is a *stored* event: deduped across branches
    /// and counted against the pre-EOSE bounds. After it, it is live and passes
    /// straight through — the relay is the deduplicating authority for the live
    /// phase in v1, and holding an unbounded seen-set for the life of a stream
    /// is the leak the bound exists to refuse.
    pub(super) fn on_event(&mut self, branch_id: &str, event: nostr::Event) -> Emit {
        if self.closed.is_some() {
            return Emit::Nothing;
        }
        if !self.branches.contains(branch_id) {
            return Emit::Nothing;
        }
        // Stored phase: dedup and bounds apply whether or not the reply has
        // landed, so holding events behind the reply cannot smuggle a
        // duplicate or an unbounded buffer past them.
        if !self.eose_reached {
            if !self.pre_eose_ids.insert(event.id) {
                return Emit::Nothing;
            }
            if self.pre_eose_ids.len() > MAX_PRE_EOSE_EVENTS {
                return self.close(CloseReason::BoundExceeded);
            }
            use nostr::JsonUtil as _;
            let size = event.as_json().len();
            self.pre_eose_bytes = self.pre_eose_bytes.saturating_add(size);
            if self.pre_eose_bytes > MAX_PRE_EOSE_BYTES {
                return self.close(CloseReason::BoundExceeded);
            }
        }

        if !self.reply_written {
            if self.awaiting_reply.len() >= MAX_PRE_EOSE_EVENTS {
                return self.close(CloseReason::BoundExceeded);
            }
            self.awaiting_reply.push(event);
            return Emit::Nothing;
        }
        Emit::Event(Box::new(event))
    }
}

/// How long the host waits for every branch to EOSE before failing closed.
///
/// Reconciles "never invent an EOSE" with "never wait forever". It does not
/// synthesise the `eose` an absent branch owes — it ends the subscription.
pub(super) const INITIAL_EOSE_DEADLINE: std::time::Duration = std::time::Duration::from_secs(30);

/// What the host must do after one relay frame.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Routed {
    /// Deliver these to the port, in this order.
    pub(super) emits: Vec<Emit>,
    /// The aggregate ended: send a real relay `CLOSE` for **every** branch it
    /// opened, not just the one that failed. A half-closed aggregate would
    /// leave the relay streaming into a subscription nobody is reading.
    pub(super) close_branches: bool,
    /// The relay signalled rate limiting; arm the process-global admission gate
    /// before the next subscription tries again.
    pub(super) arm_gate: bool,
}

/// Does this relay text indicate rate limiting?
///
/// NIP-01 gives `CLOSED`/`OK` reasons a machine-readable `rate-limited:`
/// prefix; `NOTICE` is free-form, so that arm is a heuristic. Erring toward
/// arming is the safe direction — a spurious arm costs a short wait, a missed
/// one repeats the offence the relay just objected to.
fn indicates_rate_limit(text: &str) -> bool {
    let lowered = text.to_ascii_lowercase();
    lowered.starts_with("rate-limited") || lowered.contains("rate limit")
}

/// Drive the aggregate from one raw relay frame.
///
/// Multiplexes from the first frame, which is the half `pairing.rs`'s
/// `wait_for_eose` gets wrong: it reads until EOSE and discards every EVENT it
/// passes, so the stored events §5 requires be delivered would be silently
/// dropped. Here an `EVENT` before its branch's EOSE is stored, an `EVENT`
/// after it is live, and the branch's own EOSE is what separates them.
pub(super) fn route_frame(
    aggregate: &mut Aggregate,
    frame: crate::relay::subscribe::RelayFrame,
    authority: impl FnOnce() -> Result<(), CloseReason>,
    verify: impl FnOnce(&nostr::Event) -> bool,
) -> Routed {
    use crate::relay::subscribe::RelayFrame;
    match frame {
        RelayFrame::Event { sub_id, event } => {
            // Two-stage admission, in that order: authority first, and the
            // per-event check is not reached if it fails.
            match admit(authority, || verify(&event)) {
                Admission::CloseAggregate(reason) => Routed {
                    emits: vec![aggregate.close(reason)],
                    close_branches: true,
                    arm_gate: false,
                },
                Admission::DropEvent => Routed::default(),
                Admission::Deliver => Routed {
                    emits: match aggregate.on_event(&sub_id, *event) {
                        Emit::Nothing => Vec::new(),
                        emit => vec![emit],
                    },
                    // A bound reached inside `on_event` closes the aggregate,
                    // and that must still take the branches down with it.
                    close_branches: aggregate.is_closed(),
                    arm_gate: false,
                },
            }
        }
        RelayFrame::Eose { sub_id } => Routed {
            emits: match aggregate.on_branch_eose(&sub_id) {
                Emit::Nothing => Vec::new(),
                emit => vec![emit],
            },
            close_branches: false,
            arm_gate: false,
        },
        RelayFrame::Closed { sub_id, reason } => {
            // Terminal in v1: no silent re-REQ, and one branch dying ends the
            // whole aggregate rather than narrowing it.
            let _ = sub_id;
            Routed {
                emits: vec![aggregate.close(CloseReason::RelayClosed)],
                close_branches: true,
                arm_gate: indicates_rate_limit(&reason),
            }
        }
        RelayFrame::Notice { message } => Routed {
            arm_gate: indicates_rate_limit(&message),
            ..Routed::default()
        },
        RelayFrame::Other => Routed::default(),
    }
}

/// The transport ended without a `CLOSED` — still terminal in v1.
pub(super) fn on_transport_end(aggregate: &mut Aggregate) -> Routed {
    Routed {
        emits: vec![aggregate.close(CloseReason::RelayClosed)],
        close_branches: true,
        arm_gate: false,
    }
}

/// The initial-EOSE deadline expired.
///
/// Fail-closed and explicit: **no public `eose`** is emitted, every opened
/// branch gets a real relay `CLOSE`, the aggregate closes, and the caller
/// releases the whole reservation exactly once. Inventing the missing `eose`
/// would tell the extension it had seen all stored history when one channel
/// never answered.
pub(super) fn on_initial_eose_deadline(aggregate: &mut Aggregate) -> Routed {
    if aggregate.has_eosed() || aggregate.is_closed() {
        // Already resolved; the deadline is a no-op rather than a second close.
        return Routed::default();
    }
    Routed {
        emits: vec![aggregate.close(CloseReason::EoseDeadline)],
        close_branches: true,
        arm_gate: false,
    }
}

/// A host→extension stream frame.
///
/// Keyed by `sub`, never by `id`: a stream is many frames for one subscription
/// rather than one settle for one request, so these ride a parallel lifecycle
/// table and consume none of the port's request-id budget.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum StreamFrame {
    Event {
        sub: String,
        event: Box<nostr::Event>,
    },
    Eose {
        sub: String,
    },
    Closed {
        sub: String,
        reason: CloseReason,
    },
}

impl StreamFrame {
    /// Build the frame from what the aggregate emitted, or `None` when there
    /// was nothing to deliver.
    pub(super) fn from_emit(sub: &str, emit: Emit) -> Option<Self> {
        match emit {
            Emit::Nothing => None,
            Emit::Event(event) => Some(StreamFrame::Event {
                sub: sub.to_string(),
                event,
            }),
            Emit::Eose => Some(StreamFrame::Eose {
                sub: sub.to_string(),
            }),
            Emit::Closed(reason) => Some(StreamFrame::Closed {
                sub: sub.to_string(),
                reason,
            }),
        }
    }

    /// The §2 wire shape. Carries no `id`, so a stream frame can never be
    /// mistaken for — or settle — a correlated request.
    pub(super) fn to_wire(&self) -> serde_json::Value {
        use nostr::JsonUtil as _;
        match self {
            StreamFrame::Event { sub, event } => serde_json::json!({
                "sub": sub,
                "kind": "event",
                "event": serde_json::from_str::<serde_json::Value>(&event.as_json())
                    .unwrap_or(serde_json::Value::Null),
            }),
            StreamFrame::Eose { sub } => serde_json::json!({ "sub": sub, "kind": "eose" }),
            StreamFrame::Closed { sub, reason } => serde_json::json!({
                "sub": sub,
                "kind": "closed",
                "reason": reason.as_wire(),
            }),
        }
    }
}

/// Why a `subscribe` could not be opened.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum OpenFailure {
    /// The `(identity, extension)` branch budget had no room.
    QuotaExhausted,
    /// Authority changed between reservation and the REQ burst.
    AuthorityLost(CloseReason),
    /// The relay branches could not be opened.
    BranchOpenFailed,
}

/// Open a subscription in the fixed admission order.
///
/// ```text
/// reserve quota
///   → wait on the process-global relay admission gate
///   → revalidate lease + witness + every pair
///   → send the bounded REQ burst
/// ```
///
/// **The order is the contract, and each step is a closure so a probe can see
/// which ones ran.** Two properties depend on it:
///
/// - reserving *before* any network side effect means a subscription cannot
///   connect, authenticate or REQ against budget it does not hold;
/// - revalidating *after* the gate means the unbounded wait cannot leave a
///   stale authority behind — and if authority went away during it, **zero**
///   REQs go out.
///
/// Rollback is structural: the `Reservation` is released by `Drop` on every
/// early return here, so the four failure paths the contract enumerates do not
/// each need remembering.
pub(super) fn open_subscription(
    quota: &Arc<SubscriptionQuota>,
    identity_pubkey: &str,
    extension_id: &str,
    branches: usize,
    wait_gate: impl FnOnce(),
    revalidate: impl FnOnce() -> Result<(), CloseReason>,
    send_reqs: impl FnOnce() -> Result<Vec<String>, ()>,
) -> Result<(Vec<String>, CommittedReservation), OpenFailure> {
    let reservation = quota
        .reserve(identity_pubkey, extension_id, branches)
        .ok_or(OpenFailure::QuotaExhausted)?;

    wait_gate();

    if let Err(reason) = revalidate() {
        // `reservation` drops here, giving the branches back.
        return Err(OpenFailure::AuthorityLost(reason));
    }

    let branch_ids = send_reqs().map_err(|()| OpenFailure::BranchOpenFailed)?;
    Ok((branch_ids, reservation.commit()))
}

/// One live subscription and everything that must die with it.
struct LiveSub {
    aggregate: Aggregate,
    /// Dropping this releases the branch budget, so a sub cannot be removed
    /// from the registry without its quota coming back.
    reservation: CommittedReservation,
}

/// Every live subscription, keyed by `(lease, sub id)`.
///
/// **The lease is the generation, on this side.** `frame_host::acquire` mints a
/// fresh UUID per frame mount and `ExtensionFrame` releases it on unmount, so a
/// successor port necessarily carries a different lease. Keying on it is what
/// makes "no migration to a successor port" structural: a completion addressed
/// to a lease that has been released simply finds nothing, and there is no code
/// path that could hand it to the frame that replaced it. A sub id is
/// meaningless without the lease that minted it.
///
/// An earlier revision keyed on an invented `port_generation: u64`. No such
/// counter exists in this codebase — the lease already *is* that identifier, and
/// carrying a second one would have meant two things to keep in step.
///
/// The two walls stay independent because they fall separately: the lease is
/// released when the tab closes or the extension is disabled, while the TS port
/// registry is disposed on its own schedule, and the contract notes those
/// effects are unordered. This is the Rust wall; the forwarder enforces the
/// other by refusing to deliver to a disposed port.
#[derive(Default)]
pub(super) struct SubscriptionRegistry {
    subs: Mutex<HashMap<(String, String), LiveSub>>,
}

impl SubscriptionRegistry {
    pub(super) fn new() -> Self {
        Self::default()
    }

    pub(super) fn insert(
        &self,
        lease: &str,
        sub: &str,
        aggregate: Aggregate,
        reservation: CommittedReservation,
    ) {
        let Ok(mut subs) = self.subs.lock() else {
            return;
        };
        subs.insert(
            (lease.to_string(), sub.to_string()),
            LiveSub {
                aggregate,
                reservation,
            },
        );
    }

    pub(super) fn live_count(&self) -> usize {
        self.subs.lock().map(|subs| subs.len()).unwrap_or(0)
    }

    /// Act on one live subscription's aggregate.
    ///
    /// Returns `None` when the `(generation, sub)` pair is not live — which is
    /// exactly what a frame for a torn-down port hits. Dropping it here is the
    /// no-migration rule doing its work.
    pub(super) fn with_aggregate<T>(
        &self,
        lease: &str,
        sub: &str,
        act: impl FnOnce(&mut Aggregate) -> T,
    ) -> Option<T> {
        let mut subs = self.subs.lock().ok()?;
        let live = subs.get_mut(&(lease.to_string(), sub.to_string()))?;
        Some(act(&mut live.aggregate))
    }

    /// Close one subscription, releasing its quota.
    ///
    /// Returns the close emission when it was live. Removing the entry drops
    /// its `CommittedReservation`, so the budget comes back on this path and on
    /// every other one that removes an entry.
    pub(super) fn close_one(&self, lease: &str, sub: &str, reason: CloseReason) -> Option<Emit> {
        let mut subs = self.subs.lock().ok()?;
        let mut live = subs.remove(&(lease.to_string(), sub.to_string()))?;
        let emit = live.aggregate.close(reason);
        live.reservation.release();
        Some(emit)
    }

    /// The lease wall: close every subscription this lease owns.
    pub(super) fn close_for_lease(
        &self,
        lease: &str,
        reason: CloseReason,
    ) -> Vec<(String, String)> {
        self.close_matching(reason, |key| key.0 == lease)
    }

    fn close_matching(
        &self,
        reason: CloseReason,
        matches: impl Fn(&(String, String)) -> bool,
    ) -> Vec<(String, String)> {
        let Ok(mut subs) = self.subs.lock() else {
            return Vec::new();
        };
        let doomed: Vec<(String, String)> =
            subs.keys().filter(|key| matches(key)).cloned().collect();
        for key in &doomed {
            if let Some(mut live) = subs.remove(key) {
                live.aggregate.close(reason.clone());
                live.reservation.release();
            }
        }
        doomed
    }
}

#[cfg(test)]
#[path = "subscription_tests.rs"]
mod subscription_tests;

/// The process-wide subscription registry.
///
/// One per host, like the frame-host lease map: subscriptions outlive any one
/// bridge call, and the teardown walls have to reach them from wherever they
/// fire.
pub(super) fn registry() -> &'static SubscriptionRegistry {
    static REGISTRY: std::sync::OnceLock<SubscriptionRegistry> = std::sync::OnceLock::new();
    REGISTRY.get_or_init(SubscriptionRegistry::new)
}

/// The process-wide `(identity, extension)` branch budget.
pub(super) fn quota() -> &'static Arc<SubscriptionQuota> {
    static QUOTA: std::sync::OnceLock<Arc<SubscriptionQuota>> = std::sync::OnceLock::new();
    QUOTA.get_or_init(SubscriptionQuota::new)
}

/// §5 `unsubscribe({ sub }) → { ok }`.
///
/// **Idempotent ensure-not-live, scoped to the calling lease, and no existence
/// oracle.** A well-formed `sub` returns `{ok:true}` whether or not it was
/// live, consulting only this lease's own subscriptions. A foreign, stale or
/// invented well-formed id therefore touches nothing and produces an identical
/// reply — so the method cannot be used to discover whether an id exists on
/// somebody else's frame. Only a malformed `sub` is distinguishable, because
/// that is a statement about the caller's own request rather than about the
/// host's state.
pub(super) fn unsubscribe(lease: &str, params: Option<serde_json::Value>) -> BridgeReply {
    let Some(serde_json::Value::Object(map)) = params else {
        return BridgeReply::err(code::INVALID_PARAMS, "params must be an object");
    };
    let Some(sub) = map.get("sub").and_then(serde_json::Value::as_str) else {
        return BridgeReply::err(code::INVALID_PARAMS, "sub is required and must be a string");
    };
    if sub.is_empty() || sub.len() > MAX_SUB_ID_LEN {
        return BridgeReply::err(code::INVALID_PARAMS, "sub is not a subscription id");
    }

    // The outcome is deliberately discarded. Whether this lease held that sub
    // is exactly the fact an existence oracle would leak.
    let _ = registry().close_one(lease, sub, CloseReason::Unsubscribed);
    BridgeReply::ok(serde_json::json!({ "ok": true }))
}

/// Longest `sub` id the host will look up. Host-minted ids are UUIDs; the bound
/// keeps a hostile lookup key from being unbounded work.
pub(super) const MAX_SUB_ID_LEN: usize = 128;
