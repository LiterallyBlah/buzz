//! Live-subscription ownership, exact connection generations, teardown and
//! ACK/window flow control.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use super::super::dispatch::{code, BridgeReply};
use super::flow::{FlowError, FlowState, StreamBatch};
use super::subscription::{
    on_initial_eose_deadline, route_frame, Aggregate, CloseReason, CommittedReservation, Emit,
    StreamFrame, SubscriptionQuota,
};

pub(super) type ConnectionKey = (String, String);

/// One authenticated socket. The key can be reused; the generation cannot.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) struct ConnectionInstance {
    pub(super) key: ConnectionKey,
    pub(super) generation: u64,
}

pub(super) struct SubAdmission {
    pub(super) authority: Box<dyn Fn() -> Result<(), CloseReason> + Send + Sync>,
    pub(super) verify: Box<dyn Fn(&nostr::Event) -> bool + Send + Sync>,
}

pub(super) type RelayCloser = Box<dyn Fn(&[String]) + Send + Sync>;

struct LiveSub {
    aggregate: Aggregate,
    admission: SubAdmission,
    close_at_relay: Option<RelayCloser>,
    reservation: Option<CommittedReservation>,
    connection: ConnectionInstance,
    flow: FlowState,
    terminated: bool,
}

impl LiveSub {
    fn terminate(&mut self) {
        if self.terminated {
            return;
        }
        self.terminated = true;
        let branches: Vec<String> = self.aggregate.branch_ids().map(str::to_string).collect();
        if let Some(closer) = self.close_at_relay.take() {
            closer(&branches);
        }
        // Drop is the exactly-once quota return.
        self.reservation.take();
        self.flow.clear();
    }
}

#[derive(Debug, PartialEq)]
pub(super) struct Delivery {
    pub(super) lease: String,
    pub(super) batches: Vec<StreamBatch>,
    pub(super) arm_gate: bool,
}

#[derive(Default)]
pub(super) struct SubscriptionRegistry {
    subs: Mutex<HashMap<(String, String), LiveSub>>,
}

impl SubscriptionRegistry {
    pub(super) fn new() -> Self {
        Self::default()
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn insert(
        &self,
        lease: &str,
        sub: &str,
        aggregate: Aggregate,
        admission: SubAdmission,
        close_at_relay: RelayCloser,
        reservation: CommittedReservation,
        connection: ConnectionInstance,
    ) {
        let Ok(mut subs) = self.subs.lock() else {
            return;
        };
        subs.insert(
            (lease.to_string(), sub.to_string()),
            LiveSub {
                aggregate,
                admission,
                close_at_relay: Some(close_at_relay),
                reservation: Some(reservation),
                connection,
                flow: FlowState::default(),
                terminated: false,
            },
        );
    }

    fn port_queued_totals(
        subs: &HashMap<(String, String), LiveSub>,
        lease: &str,
    ) -> (usize, usize) {
        subs.iter()
            .filter(|(key, _)| key.0 == lease)
            .map(|(_, live)| live.flow.queued_totals())
            .fold((0usize, 0usize), |(fc, bc), (f, b)| {
                (fc.saturating_add(f), bc.saturating_add(b))
            })
    }

    fn port_in_flight_totals(
        subs: &HashMap<(String, String), LiveSub>,
        lease: &str,
    ) -> (usize, usize) {
        subs.iter()
            .filter(|(key, _)| key.0 == lease)
            .map(|(_, live)| live.flow.in_flight_totals())
            .fold((0usize, 0usize), |(fc, bc), (f, b)| {
                (fc.saturating_add(f), bc.saturating_add(b))
            })
    }

    fn terminal_delivery(
        lease: &str,
        sub: &str,
        live: &mut LiveSub,
        reason: CloseReason,
    ) -> Delivery {
        live.terminate();
        Delivery {
            lease: lease.to_string(),
            batches: vec![live.flow.terminal_batch(lease, sub, reason)],
            arm_gate: false,
        }
    }

    pub(super) fn route_by_branch(
        &self,
        connection: &ConnectionInstance,
        branch_id: &str,
        frame: crate::relay::subscribe::RelayFrame,
    ) -> Option<Delivery> {
        let mut subs = self.subs.lock().ok()?;
        let key = subs
            .iter()
            .find(|(_, live)| {
                &live.connection == connection && live.aggregate.owns_branch(branch_id)
            })
            .map(|(key, _)| key.clone())?;
        let lease = key.0.clone();
        let sub = key.1.clone();
        let all_port_queued = Self::port_queued_totals(&subs, &lease);
        let own_queued = subs.get(&key)?.flow.queued_totals();
        let port_queued = (
            all_port_queued.0.saturating_sub(own_queued.0),
            all_port_queued.1.saturating_sub(own_queued.1),
        );
        let port_in_flight = Self::port_in_flight_totals(&subs, &lease);

        let mut remove = false;
        let delivery = {
            let live = subs.get_mut(&key)?;
            let routed = route_frame(
                &mut live.aggregate,
                frame,
                || (live.admission.authority)(),
                |event| (live.admission.verify)(event),
            );
            if routed.close_branches {
                let reason = live
                    .aggregate
                    .close_reason()
                    .unwrap_or(CloseReason::RelayClosed);
                live.terminate();
                if live.flow.is_activated() {
                    remove = true;
                    let mut delivery = Self::terminal_delivery(&lease, &sub, live, reason);
                    delivery.arm_gate = routed.arm_gate;
                    delivery
                } else {
                    // Tombstone retained until the exact activation receipt, so
                    // a pre-reply close cannot overtake or disappear before the
                    // correlated reply.
                    Delivery {
                        lease: lease.clone(),
                        batches: Vec::new(),
                        arm_gate: routed.arm_gate,
                    }
                }
            } else {
                let frames: Vec<StreamFrame> = routed
                    .emits
                    .into_iter()
                    .filter_map(|emit| StreamFrame::from_emit(&sub, emit))
                    .collect();
                if live
                    .flow
                    .enqueue(frames, port_queued.0, port_queued.1)
                    .is_err()
                {
                    let reason = CloseReason::BoundExceeded;
                    live.aggregate.close(reason.clone());
                    live.terminate();
                    if live.flow.is_activated() {
                        remove = true;
                        Self::terminal_delivery(&lease, &sub, live, reason)
                    } else {
                        Delivery {
                            lease: lease.clone(),
                            batches: Vec::new(),
                            arm_gate: false,
                        }
                    }
                } else {
                    Delivery {
                        lease: lease.clone(),
                        batches: live
                            .flow
                            .drain(&lease, &sub, port_in_flight.0, port_in_flight.1),
                        arm_gate: routed.arm_gate,
                    }
                }
            }
        };
        if remove {
            subs.remove(&key);
        }
        Some(delivery)
    }

    pub(super) fn activate(&self, lease: &str, sub: &str) -> Option<Delivery> {
        let mut subs = self.subs.lock().ok()?;
        let key = (lease.to_string(), sub.to_string());
        let all_port_queued = Self::port_queued_totals(&subs, lease);
        let own_queued = subs.get(&key)?.flow.queued_totals();
        let port_queued = (
            all_port_queued.0.saturating_sub(own_queued.0),
            all_port_queued.1.saturating_sub(own_queued.1),
        );
        let port_in_flight = Self::port_in_flight_totals(&subs, lease);
        let mut remove = false;
        let delivery = {
            let live = subs.get_mut(&key)?;
            if live.flow.activate().is_err() {
                live.aggregate.close(CloseReason::BoundExceeded);
                remove = true;
                Self::terminal_delivery(lease, sub, live, CloseReason::BoundExceeded)
            } else {
                let emits = live.aggregate.mark_reply_written();
                if let Some(reason) = live.aggregate.close_reason() {
                    remove = true;
                    Self::terminal_delivery(lease, sub, live, reason)
                } else {
                    let frames = emits
                        .into_iter()
                        .filter_map(|emit| StreamFrame::from_emit(sub, emit))
                        .collect();
                    if live
                        .flow
                        .enqueue(frames, port_queued.0, port_queued.1)
                        .is_err()
                    {
                        live.aggregate.close(CloseReason::BoundExceeded);
                        remove = true;
                        Self::terminal_delivery(lease, sub, live, CloseReason::BoundExceeded)
                    } else {
                        Delivery {
                            lease: lease.to_string(),
                            batches: live.flow.drain(
                                lease,
                                sub,
                                port_in_flight.0,
                                port_in_flight.1,
                            ),
                            arm_gate: false,
                        }
                    }
                }
            }
        };
        if remove {
            subs.remove(&key);
        }
        Some(delivery)
    }

    pub(super) fn acknowledge(
        &self,
        lease: &str,
        sub: &str,
        seq: u64,
        token: &str,
        frame_count: usize,
        encoded_bytes: usize,
    ) -> Option<Delivery> {
        let mut subs = self.subs.lock().ok()?;
        let key = (lease.to_string(), sub.to_string());
        let acked = {
            let live = subs.get_mut(&key)?;
            live.flow.ack(seq, token, frame_count, encoded_bytes)
        };
        if acked == Err(FlowError::AckViolation) {
            let mut live = subs.remove(&key)?;
            live.aggregate.close(CloseReason::BoundExceeded);
            return Some(Self::terminal_delivery(
                lease,
                sub,
                &mut live,
                CloseReason::BoundExceeded,
            ));
        }
        let port_in_flight = Self::port_in_flight_totals(&subs, lease);
        let live = subs.get_mut(&key)?;
        Some(Delivery {
            lease: lease.to_string(),
            batches: live
                .flow
                .drain(lease, sub, port_in_flight.0, port_in_flight.1),
            arm_gate: false,
        })
    }

    pub(super) fn close_on_ack_timeout(
        &self,
        lease: &str,
        sub: &str,
        seq: u64,
        token: &str,
    ) -> Option<Delivery> {
        let mut subs = self.subs.lock().ok()?;
        let key = (lease.to_string(), sub.to_string());
        if !subs.get(&key)?.flow.has_in_flight(seq, token) {
            return None;
        }
        let mut live = subs.remove(&key)?;
        live.aggregate.close(CloseReason::BoundExceeded);
        Some(Self::terminal_delivery(
            lease,
            sub,
            &mut live,
            CloseReason::BoundExceeded,
        ))
    }

    pub(super) fn close_for_flow_violation(&self, lease: &str, sub: &str) -> Option<Delivery> {
        let mut subs = self.subs.lock().ok()?;
        let key = (lease.to_string(), sub.to_string());
        let mut live = subs.remove(&key)?;
        live.aggregate.close(CloseReason::BoundExceeded);
        Some(Self::terminal_delivery(
            lease,
            sub,
            &mut live,
            CloseReason::BoundExceeded,
        ))
    }

    #[cfg(test)]
    pub(super) fn live_count(&self) -> usize {
        self.subs.lock().map(|subs| subs.len()).unwrap_or(0)
    }

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

    pub(super) fn close_one(&self, lease: &str, sub: &str, reason: CloseReason) -> Option<Emit> {
        let mut subs = self.subs.lock().ok()?;
        let mut live = subs.remove(&(lease.to_string(), sub.to_string()))?;
        let emit = live.aggregate.close(reason);
        live.terminate();
        Some(emit)
    }

    pub(super) fn close_for_lease(
        &self,
        lease: &str,
        reason: CloseReason,
    ) -> Vec<(String, String)> {
        self.close_matching(reason, |key| key.0 == lease)
    }

    pub(super) fn close_for_connection(&self, connection: &ConnectionInstance) -> Vec<Delivery> {
        let Ok(mut subs) = self.subs.lock() else {
            return Vec::new();
        };
        let doomed: Vec<(String, String)> = subs
            .iter()
            .filter(|(_, live)| &live.connection == connection)
            .map(|(key, _)| key.clone())
            .collect();
        let mut closed = Vec::new();
        for key in doomed {
            let activated = subs.get(&key).is_some_and(|live| live.flow.is_activated());
            if activated {
                let Some(mut live) = subs.remove(&key) else {
                    continue;
                };
                live.aggregate.close(CloseReason::RelayClosed);
                closed.push(Self::terminal_delivery(
                    &key.0,
                    &key.1,
                    &mut live,
                    CloseReason::RelayClosed,
                ));
            } else if let Some(live) = subs.get_mut(&key) {
                live.aggregate.close(CloseReason::RelayClosed);
                live.terminate();
            }
        }
        closed
    }

    pub(super) fn close_on_eose_deadline(&self, lease: &str, sub: &str) -> Option<Delivery> {
        let mut subs = self.subs.lock().ok()?;
        let key = (lease.to_string(), sub.to_string());
        let live = subs.get_mut(&key)?;
        if live.aggregate.has_eosed() || live.aggregate.is_closed() {
            return None;
        }
        let _ = on_initial_eose_deadline(&mut live.aggregate);
        live.terminate();
        if live.flow.is_activated() {
            let mut live = subs.remove(&key)?;
            Some(Self::terminal_delivery(
                lease,
                sub,
                &mut live,
                CloseReason::EoseDeadline,
            ))
        } else {
            Some(Delivery {
                lease: lease.to_string(),
                batches: Vec::new(),
                arm_gate: false,
            })
        }
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
                live.terminate();
            }
        }
        doomed
    }
}

pub(super) fn registry() -> &'static SubscriptionRegistry {
    static REGISTRY: std::sync::OnceLock<SubscriptionRegistry> = std::sync::OnceLock::new();
    REGISTRY.get_or_init(SubscriptionRegistry::new)
}

pub(super) fn quota() -> &'static Arc<SubscriptionQuota> {
    static QUOTA: std::sync::OnceLock<Arc<SubscriptionQuota>> = std::sync::OnceLock::new();
    QUOTA.get_or_init(SubscriptionQuota::new)
}

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
    let _ = registry().close_one(lease, sub, CloseReason::Unsubscribed);
    BridgeReply::ok(serde_json::json!({ "ok": true }))
}

pub(super) const MAX_SUB_ID_LEN: usize = 128;

#[cfg(test)]
#[path = "registry_tests.rs"]
mod registry_tests;

#[cfg(test)]
#[path = "registry_successor_tests.rs"]
mod registry_successor_tests;
