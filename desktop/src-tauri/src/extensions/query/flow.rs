//! Literal end-to-end stream backpressure for the browser MessagePort.
//!
//! Rust retains a bounded queue and emits bounded batches only inside an exact
//! per-subscription/per-port in-flight window. A batch keeps its credit until
//! the extension-side bridge returns its unguessable token and exact monotonic
//! sequence after dequeue/adoption. Host queues alone are not enough: without
//! this window an immediate `MessagePort.postMessage` merely moves an unbounded
//! backlog into the browser.

use std::collections::VecDeque;

use super::subscription::{CloseReason, StreamFrame};

pub(super) const MAX_STREAM_BATCH_FRAMES: usize = 16;
pub(super) const MAX_STREAM_BATCH_BYTES: usize = 640 * 1024;
pub(super) const MAX_QUEUED_FRAMES_PER_SUB: usize = 128;
pub(super) const MAX_QUEUED_BYTES_PER_SUB: usize = 2 * 1024 * 1024;
pub(super) const MAX_QUEUED_FRAMES_PER_PORT: usize = 512;
pub(super) const MAX_QUEUED_BYTES_PER_PORT: usize = 8 * 1024 * 1024;
pub(super) const MAX_IN_FLIGHT_BATCHES_PER_SUB: usize = 4;
pub(super) const MAX_IN_FLIGHT_BYTES_PER_SUB: usize = 1024 * 1024;
pub(super) const MAX_IN_FLIGHT_BATCHES_PER_PORT: usize = 8;
pub(super) const MAX_IN_FLIGHT_BYTES_PER_PORT: usize = 3 * 1024 * 1024;
pub(super) const STREAM_ACK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

#[derive(Debug, Clone, serde::Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(super) struct StreamBatch {
    pub(super) generation: String,
    pub(super) sub: String,
    pub(super) seq: u64,
    pub(super) token: String,
    pub(super) frames: Vec<serde_json::Value>,
    pub(super) frame_count: usize,
    pub(super) encoded_bytes: usize,
    pub(super) terminal: bool,
}

#[derive(Debug, Clone)]
struct QueuedFrame {
    wire: serde_json::Value,
    encoded_bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct InFlight {
    seq: u64,
    token: String,
    frame_count: usize,
    encoded_bytes: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FlowError {
    BoundExceeded,
    ActivationViolation,
    AckViolation,
}

#[derive(Default)]
pub(super) struct FlowState {
    activated: bool,
    queue: VecDeque<QueuedFrame>,
    queued_bytes: usize,
    in_flight: VecDeque<InFlight>,
    in_flight_bytes: usize,
    next_seq: u64,
}

impl FlowState {
    pub(super) fn activate(&mut self) -> Result<(), FlowError> {
        if self.activated {
            return Err(FlowError::ActivationViolation);
        }
        self.activated = true;
        if self.next_seq == 0 {
            self.next_seq = 1;
        }
        Ok(())
    }

    pub(super) fn is_activated(&self) -> bool {
        self.activated
    }

    pub(super) fn queued_totals(&self) -> (usize, usize) {
        (self.queue.len(), self.queued_bytes)
    }

    pub(super) fn in_flight_totals(&self) -> (usize, usize) {
        (self.in_flight.len(), self.in_flight_bytes)
    }

    pub(super) fn enqueue(
        &mut self,
        frames: Vec<StreamFrame>,
        port_queued_frames: usize,
        port_queued_bytes: usize,
    ) -> Result<(), FlowError> {
        let mut encoded = Vec::with_capacity(frames.len());
        let mut added_bytes = 0usize;
        for frame in frames {
            let wire = frame.to_wire();
            let size = serde_json::to_vec(&wire)
                .map_err(|_| FlowError::BoundExceeded)?
                .len();
            if size > MAX_STREAM_BATCH_BYTES {
                return Err(FlowError::BoundExceeded);
            }
            added_bytes = added_bytes
                .checked_add(size)
                .ok_or(FlowError::BoundExceeded)?;
            encoded.push(QueuedFrame {
                wire,
                encoded_bytes: size,
            });
        }
        let sub_frames = self
            .queue
            .len()
            .checked_add(encoded.len())
            .ok_or(FlowError::BoundExceeded)?;
        let sub_bytes = self
            .queued_bytes
            .checked_add(added_bytes)
            .ok_or(FlowError::BoundExceeded)?;
        let port_frames = port_queued_frames
            .checked_add(encoded.len())
            .ok_or(FlowError::BoundExceeded)?;
        let port_bytes = port_queued_bytes
            .checked_add(added_bytes)
            .ok_or(FlowError::BoundExceeded)?;
        if sub_frames > MAX_QUEUED_FRAMES_PER_SUB
            || sub_bytes > MAX_QUEUED_BYTES_PER_SUB
            || port_frames > MAX_QUEUED_FRAMES_PER_PORT
            || port_bytes > MAX_QUEUED_BYTES_PER_PORT
        {
            return Err(FlowError::BoundExceeded);
        }
        self.queued_bytes = sub_bytes;
        self.queue.extend(encoded);
        Ok(())
    }

    pub(super) fn ack(
        &mut self,
        seq: u64,
        token: &str,
        frame_count: usize,
        encoded_bytes: usize,
    ) -> Result<(), FlowError> {
        let Some(expected) = self.in_flight.front() else {
            return Err(FlowError::AckViolation);
        };
        if expected.seq != seq
            || expected.token != token
            || expected.frame_count != frame_count
            || expected.encoded_bytes != encoded_bytes
        {
            return Err(FlowError::AckViolation);
        }
        let released = self.in_flight.pop_front().expect("front exists");
        self.in_flight_bytes = self.in_flight_bytes.saturating_sub(released.encoded_bytes);
        Ok(())
    }

    pub(super) fn has_in_flight(&self, seq: u64, token: &str) -> bool {
        self.in_flight
            .iter()
            .any(|batch| batch.seq == seq && batch.token == token)
    }

    pub(super) fn drain(
        &mut self,
        generation: &str,
        sub: &str,
        port_in_flight_batches: usize,
        port_in_flight_bytes: usize,
    ) -> Vec<StreamBatch> {
        if !self.activated {
            return Vec::new();
        }
        let mut batches = Vec::new();
        let mut port_batches = port_in_flight_batches;
        let mut port_bytes = port_in_flight_bytes;
        while !self.queue.is_empty()
            && self.in_flight.len() < MAX_IN_FLIGHT_BATCHES_PER_SUB
            && port_batches < MAX_IN_FLIGHT_BATCHES_PER_PORT
        {
            let mut frames = Vec::new();
            let mut bytes = 0usize;
            while frames.len() < MAX_STREAM_BATCH_FRAMES {
                let Some(next) = self.queue.front() else {
                    break;
                };
                if !frames.is_empty()
                    && bytes.saturating_add(next.encoded_bytes) > MAX_STREAM_BATCH_BYTES
                {
                    break;
                }
                if self
                    .in_flight_bytes
                    .saturating_add(bytes)
                    .saturating_add(next.encoded_bytes)
                    > MAX_IN_FLIGHT_BYTES_PER_SUB
                    || port_bytes
                        .saturating_add(bytes)
                        .saturating_add(next.encoded_bytes)
                        > MAX_IN_FLIGHT_BYTES_PER_PORT
                {
                    break;
                }
                let next = self.queue.pop_front().expect("front exists");
                self.queued_bytes = self.queued_bytes.saturating_sub(next.encoded_bytes);
                bytes = bytes.saturating_add(next.encoded_bytes);
                frames.push(next.wire);
            }
            if frames.is_empty() {
                break;
            }
            let seq = self.next_seq;
            self.next_seq = self.next_seq.saturating_add(1);
            let token = uuid::Uuid::new_v4().to_string();
            let frame_count = frames.len();
            self.in_flight.push_back(InFlight {
                seq,
                token: token.clone(),
                frame_count,
                encoded_bytes: bytes,
            });
            self.in_flight_bytes = self.in_flight_bytes.saturating_add(bytes);
            port_batches += 1;
            port_bytes = port_bytes.saturating_add(bytes);
            batches.push(StreamBatch {
                generation: generation.to_string(),
                sub: sub.to_string(),
                seq,
                token,
                frames,
                frame_count,
                encoded_bytes: bytes,
                terminal: false,
            });
        }
        batches
    }

    pub(super) fn clear(&mut self) {
        self.queue.clear();
        self.queued_bytes = 0;
        self.in_flight.clear();
        self.in_flight_bytes = 0;
    }

    pub(super) fn terminal_batch(
        &mut self,
        generation: &str,
        sub: &str,
        reason: CloseReason,
    ) -> StreamBatch {
        self.clear();
        if self.next_seq == 0 {
            self.next_seq = 1;
        }
        let seq = self.next_seq;
        self.next_seq = self.next_seq.saturating_add(1);
        let wire = StreamFrame::Closed {
            sub: sub.to_string(),
            reason,
        }
        .to_wire();
        let encoded_bytes = serde_json::to_vec(&wire).map_or(0, |bytes| bytes.len());
        StreamBatch {
            generation: generation.to_string(),
            sub: sub.to_string(),
            seq,
            token: uuid::Uuid::new_v4().to_string(),
            frames: vec![wire],
            frame_count: 1,
            encoded_bytes,
            terminal: true,
        }
    }
}

#[cfg(test)]
#[path = "flow_tests.rs"]
mod flow_tests;
