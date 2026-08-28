//! The live-subscription registry: who owns which stream, and what dies with
//! it.
//!
//! Split from `subscription.rs` only because the two together exceed the
//! repo's 1000-line ratchet. The visibility is unchanged — a sibling module
//! under `query`, with `pub(super)` items, reaches and is reached by exactly
//! the same code as before.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use super::super::dispatch::{code, BridgeReply};
use super::subscription::{
    on_initial_eose_deadline, on_transport_end, route_frame, Aggregate, CloseReason,
    CommittedReservation, Emit, Routed, StreamFrame, SubscriptionQuota,
};

/// Which shared socket a subscription's branches were opened on:
/// `(relay url, authenticated identity)`.
pub(super) type ConnectionKey = (String, String);

/// The two-stage admission check for one live subscription.
///
/// **Carried by the entry, not supplied by the reader.** The reader multiplexes
/// every subscription on a shared socket, so if it passed these in it would be
/// choosing which subscription's authority to apply to an arriving event — and
/// picking the wrong one is indistinguishable from picking the right one until
/// something leaks. Storing them beside the aggregate makes the pairing
/// structural: the only admission an event can be judged by is the one that
/// belongs to the subscription that asked for it.
pub(super) struct SubAdmission {
    /// Is the subscription still entitled to stream at all? Revoked grant, lost
    /// lease, identity switch. Failing this ends the whole aggregate.
    pub(super) authority: Box<dyn Fn() -> Result<(), CloseReason> + Send + Sync>,
    /// May the extension see this specific event? Failing this drops the event
    /// and leaves the stream running.
    pub(super) verify: Box<dyn Fn(&nostr::Event) -> bool + Send + Sync>,
}

/// One live subscription and everything that must die with it.
struct LiveSub {
    aggregate: Aggregate,
    admission: SubAdmission,
    /// Dropping this releases the branch budget, so a sub cannot be removed
    /// from the registry without its quota coming back.
    reservation: CommittedReservation,
    /// The socket its branches live on. Recorded so a dead transport can close
    /// exactly the subscriptions it was carrying — and no others, since one
    /// relay's socket says nothing about another's.
    connection: ConnectionKey,
}

/// What the reader must do with one routed relay frame.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Delivery {
    /// The port that owns the subscription. A frame is delivered to this lease
    /// or to nothing — never to whichever frame happens to be mounted now.
    pub(super) lease: String,
    /// Frames for the extension, in order.
    pub(super) frames: Vec<StreamFrame>,
    /// Branch ids to `CLOSE` at the relay. Non-empty exactly when the aggregate
    /// ended, and it carries **every** branch the aggregate opened rather than
    /// the one that failed — a half-closed aggregate leaves the relay streaming
    /// into a subscription nobody reads.
    pub(super) close_branches: Vec<String>,
    /// The relay signalled rate limiting on this frame.
    pub(super) arm_gate: bool,
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
        admission: SubAdmission,
        reservation: CommittedReservation,
        connection: ConnectionKey,
    ) {
        let Ok(mut subs) = self.subs.lock() else {
            return;
        };
        subs.insert(
            (lease.to_string(), sub.to_string()),
            LiveSub {
                aggregate,
                admission,
                reservation,
                connection,
            },
        );
    }

    /// Route one relay frame to the subscription that owns its branch.
    ///
    /// Ownership is **derived from the aggregate itself** on every frame rather
    /// than read from a branch index kept beside it. An index would be a second
    /// authority over which branches exist, and the two fall out of step in
    /// exactly the case that matters — a sub closing while a frame for it is in
    /// flight — leaving a live branch pointing at an aggregate that has gone.
    /// The cost is a scan over live subscriptions per frame; the bounds keep
    /// that small (32 branches a sub, 64 subs a port), and it can become an
    /// index later without changing this signature if it ever stops being.
    ///
    /// Returns `None` when no live subscription owns the branch. That is the
    /// no-migration rule doing its work: a frame for a torn-down port finds
    /// nothing and is dropped, rather than being handed to its successor.
    ///
    /// The subscription's **own** admission runs here, under the registry lock,
    /// which serialises admission across subscriptions. That is deliberate: the
    /// alternative is releasing the lock between finding the aggregate and
    /// mutating it, which reopens the window this method exists to close.
    pub(super) fn route_by_branch(
        &self,
        branch_id: &str,
        frame: crate::relay::subscribe::RelayFrame,
    ) -> Option<Delivery> {
        let mut subs = self.subs.lock().ok()?;
        let (key, live) = subs
            .iter_mut()
            .find(|(_, live)| live.aggregate.owns_branch(branch_id))?;
        let lease = key.0.clone();
        let sub = key.1.clone();

        let LiveSub {
            aggregate,
            admission,
            ..
        } = live;
        let routed = route_frame(
            aggregate,
            frame,
            || (admission.authority)(),
            |event| (admission.verify)(event),
        );
        let frames = routed
            .emits
            .into_iter()
            .filter_map(|emit| StreamFrame::from_emit(&sub, emit))
            .collect();
        let close_branches: Vec<String> = if routed.close_branches {
            aggregate.branch_ids().map(str::to_string).collect()
        } else {
            Vec::new()
        };

        // The aggregate is finished, so the entry goes — which drops its
        // `CommittedReservation` and returns the branch budget. Reading the
        // branch ids first is why this is not simply `close_one`: they are
        // needed for the relay `CLOSE` burst and they live in the entry being
        // removed.
        if !close_branches.is_empty() {
            subs.remove(&(lease.clone(), sub.clone()));
        }

        Some(Delivery {
            lease,
            frames,
            close_branches,
            arm_gate: routed.arm_gate,
        })
    }

    #[cfg(test)]
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
    ///
    /// **Not yet called from production.** §9 teardown fires from
    /// `frame_host::release`, which this increment does not touch; wiring it is
    /// remaining 5b work. Allowed by name rather than by a module-wide
    /// attribute so the gap is one item wide and stays visible.
    #[allow(dead_code)]
    pub(super) fn close_for_lease(
        &self,
        lease: &str,
        reason: CloseReason,
    ) -> Vec<(String, String)> {
        self.close_matching(reason, |key| key.0 == lease)
    }

    /// The transport wall: close every subscription carried by one dead socket.
    ///
    /// Scoped to the connection key rather than sweeping everything, because a
    /// socket dying says nothing about subscriptions on another relay or under
    /// another identity. No branch ids come back: there is no socket left to
    /// send a relay `CLOSE` on, which is the whole reason this path exists.
    pub(super) fn close_for_connection(&self, connection: &ConnectionKey) -> Vec<Delivery> {
        let Ok(mut subs) = self.subs.lock() else {
            return Vec::new();
        };
        let doomed: Vec<(String, String)> = subs
            .iter()
            .filter(|(_, live)| &live.connection == connection)
            .map(|(key, _)| key.clone())
            .collect();

        let mut closed = Vec::with_capacity(doomed.len());
        for key in doomed {
            let Some(mut live) = subs.remove(&key) else {
                continue;
            };
            // Through `on_transport_end`, not an inline `close`, so what a dead
            // transport means to an aggregate has one definition. A second copy
            // here would keep passing its own test while drifting from the one
            // the aggregate actually implements.
            let routed = on_transport_end(&mut live.aggregate);
            live.reservation.release();
            closed.push(delivery_from(key, routed));
        }
        closed
    }

    /// The initial-EOSE deadline expired for one subscription.
    ///
    /// Returns `None` when it has already EOSE'd or closed — the deadline is a
    /// no-op then, not a second close. The branches come back because unlike a
    /// dead transport there is still a socket to `CLOSE` them on.
    pub(super) fn close_on_eose_deadline(&self, lease: &str, sub: &str) -> Option<Delivery> {
        let mut subs = self.subs.lock().ok()?;
        let key = (lease.to_string(), sub.to_string());
        let routed = on_initial_eose_deadline(&mut subs.get_mut(&key)?.aggregate);
        if !routed.close_branches {
            return None;
        }
        let mut live = subs.remove(&key)?;
        let branches: Vec<String> = live.aggregate.branch_ids().map(str::to_string).collect();
        live.reservation.release();
        let mut delivery = delivery_from(key, routed);
        delivery.close_branches = branches;
        Some(delivery)
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

/// Turn a routed outcome for one subscription into a delivery for its port.
fn delivery_from((lease, sub): (String, String), routed: Routed) -> Delivery {
    Delivery {
        frames: routed
            .emits
            .into_iter()
            .filter_map(|emit| StreamFrame::from_emit(&sub, emit))
            .collect(),
        lease,
        close_branches: Vec::new(),
        arm_gate: routed.arm_gate,
    }
}

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

#[cfg(test)]
#[path = "registry_tests.rs"]
mod registry_tests;
