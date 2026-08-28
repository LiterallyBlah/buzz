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
    public_eose_sent: bool,
    closed: Option<CloseReason>,
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
            public_eose_sent: false,
            closed: None,
        })
    }

    pub(super) fn is_closed(&self) -> bool {
        self.closed.is_some()
    }

    pub(super) fn has_eosed(&self) -> bool {
        self.public_eose_sent
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
        if self.closed.is_some() || self.public_eose_sent {
            return Emit::Nothing;
        }
        if !self.branches.contains(branch_id) {
            return Emit::Nothing;
        }
        self.eosed.insert(branch_id.to_string());
        if self.eosed.len() < self.branches.len() {
            return Emit::Nothing;
        }
        self.public_eose_sent = true;
        // The dedup window guarded the stored phase; that phase is over.
        self.pre_eose_ids.clear();
        self.pre_eose_bytes = 0;
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
        if self.public_eose_sent {
            return Emit::Event(Box::new(event));
        }

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
        Emit::Event(Box::new(event))
    }
}

#[cfg(test)]
#[path = "subscription_tests.rs"]
mod subscription_tests;
