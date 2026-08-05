//! Drain: finish what is running, take nothing new, exit 0.
//!
//! ## What this is for
//!
//! A deployer that is about to replace the `buzz-acp` binary has exactly two
//! levers today, and both of them are visible to open work. `SIGTERM` gives
//! in-flight prompts a 30-second grace and then aborts them, so a model turn
//! that is three minutes into a refactor dies mid-sentence. Killing the unit
//! outright is worse. Either way a queued project event — already announced on
//! its issue as `state=queued` by NIP-PA — is dropped on the floor, and the
//! indicator the announcement lit stays lit until the consumer's staleness
//! window closes it. The restart is therefore never invisible: somebody's turn
//! is cut, or somebody's issue promises work that no process is holding.
//!
//! Drain is the third lever. It is an owner-signed control frame that says
//! *stop admitting, finish the batch, then stop the process cleanly* — so the
//! swap is bounded by the work already in hand rather than by a fixed grace
//! period chosen without knowing what is running.
//!
//! ## Why exit 0 is the whole point
//!
//! The systemd units run `Restart=on-failure`. A non-zero exit is therefore an
//! instruction to systemd to bring the *old* binary straight back up, which is
//! precisely the thing a deployer draining for a swap must not have happen. A
//! drained runtime leaves through [`crate::tokio_main`]'s ordinary `Ok(())`
//! tail — the same teardown the inactivity bound and `!shutdown` already use —
//! so the unit stays down until the deployer starts the new binary. Nothing in
//! this module calls `std::process::exit`: the exit code is a consequence of
//! the run loop returning, and keeping it that way is what guarantees the
//! teardown (agent reaping, presence `offline`, relay close frame) actually
//! runs.
//!
//! ## The wire contract
//!
//! A drain frame is an ordinary **observer control frame** — the same envelope
//! `cancel_turn` and `switch_model` already travel in, verified by the same
//! code at [`crate::handle_relay_observer_control_event`]. Nothing about the
//! transport is new; only the payload `type` is.
//!
//! | field | value |
//! |---|---|
//! | kind | `24200` (`buzz_core::kind::KIND_AGENT_OBSERVER_FRAME`) |
//! | tags | `["p", <agent-pubkey-hex>]`, `["agent", <agent-pubkey-hex>]`, `["frame", "control"]` |
//! | content | NIP-44 **v2** ciphertext, owner secret → agent pubkey |
//! | plaintext | `{"type":"drain"}`, optionally `{"type":"drain","reason":"<free text>"}` |
//! | `created_at` | within ±300 s of the agent's clock ([`crate::OBSERVER_CONTROL_FRESHNESS_SECS`]) |
//! | signature | the **owner's** key — the agent's resolved owner, not the agent's own |
//!
//! Both `p` and `agent` carry the *agent's* pubkey: `p` is what the relay
//! routes on and what the agent's control REQ filters for, `agent` names whose
//! observer stream the frame belongs to. `buzz_sdk::build_agent_observer_frame`
//! builds exactly this envelope and is the reference implementation for a
//! sender; no CLI subcommand is required, because a correctly built and signed
//! `24200` from any Nostr tool is indistinguishable from one.
//!
//! Two senders in this repo build it today, and neither is privileged over the
//! other — the check below is the same for both:
//!
//! - `buzz-cli`'s `agent_drain.rs`, for a deployer swapping a binary;
//! - Buzz Desktop's Agents tab, where the owner of a relay-hosted agent drains
//!   it from the `Managed elsewhere` card (`desktop/src/shared/api/agentControl.ts`,
//!   signed in the `build_observer_control_event` Tauri command).
//!
//! The Desktop sender is why the `control_result` acknowledgement below is
//! load-bearing rather than a convenience: a deployer polls `systemctl` for
//! its answer, but a desktop half a world away from the host has only the
//! agent's own reply. What that reply may be read to mean is bounded — see
//! "What the acknowledgement does not say" below.
//!
//! ## Skew safety
//!
//! A runtime that predates this module reaches the `_ =>` arm of the payload
//! match, debug-logs the frame and ignores it. So a fleet may be drained
//! mid-rollout with no coordination: old binaries decline the instruction and
//! keep serving, new binaries honour it. That tolerance is why the drain
//! payload is a new `type` on the existing channel rather than a new kind, a
//! new tag, or a new subscription — each of which an old binary would have had
//! to be taught to ignore.
//!
//! ## Replay, and why idempotence is the whole answer
//!
//! The freshness window rejects any frame more than five minutes from the
//! agent's clock, which disposes of the archival replay: a drain frame captured
//! today cannot drain a process started tomorrow. It does **not** dispose of
//! replay *inside* the window — a relay redelivering on reconnect, a
//! subscription generation replacement handing the same event to a second REQ,
//! or a deployer that simply sent it twice.
//!
//! No nonce or seen-set is needed for that, because [`DrainState::begin`] is
//! idempotent by construction:
//!
//! - a repeat frame finds the state already `Draining` and returns
//!   [`DrainOnset::AlreadyDraining`];
//! - it **does not move the deadline**, so a repeating replay cannot hold the
//!   process open past its bound — which is the one way an idempotent-looking
//!   operation could still be abused;
//! - admission is already closed, so there is nothing for it to close again.
//!
//! The observable difference between one drain frame and ten is a second
//! `control_result` acknowledgement per frame. That is the intended answer to
//! "did it arrive", not a state change.
//!
//! ## What the acknowledgement does not say
//!
//! [`handle_drain_control`](crate::handle_drain_control) emits `control_result`
//! the moment admission closes — before the in-flight turn finishes and long
//! before the run loop leaves. So the ack means *the instruction was accepted*,
//! and a sender that renders it as "stopped" is asserting an exit that has not
//! happened and that this frame could not have reported. The strongest true
//! reading is "draining"; Desktop's `describeDrainRequest` is held to exactly
//! that by its own tests.
//!
//! The exit does have a signal — the `drained` runtime lifecycle frame emitted
//! just before the run loop breaks — but it is not the ack, it arrives an
//! unbounded turn later, and a consumer correlating it to a specific drain
//! request needs the `startNonce` to tell it from a previous process's exit.
//!
//! Silence is not a refusal either: `observer` is an `Option` at the emit site,
//! so an agent with owner telemetry disabled honours a drain and acknowledges
//! nothing.

use std::time::Duration;

use tokio::time::Instant;

/// The payload `type` that names a drain in an observer control frame.
///
/// A constant rather than a literal at the match site because the sender
/// contract in this module's docs and the string the runtime actually matches
/// have to be the same thing; a deployer reading the docs is reading this.
pub(crate) const CONTROL_TYPE_DRAIN: &str = "drain";

/// How much of an operator-supplied `reason` reaches the log line.
///
/// The payload as a whole is already capped by `OBSERVER_MAX_PLAINTEXT_LEN`
/// (64 KiB), which is a sane bound for a *frame* and an absurd one for a field
/// interpolated into every drain log line. Truncated rather than rejected: a
/// long reason is still a reason, and refusing the drain over it would let a
/// cosmetic field block an operational instruction.
const REASON_LOG_CAP: usize = 200;

/// Trim an operator-supplied drain reason to something loggable.
pub(crate) fn trim_reason(reason: &str) -> String {
    let trimmed = reason.trim();
    match trimmed.char_indices().nth(REASON_LOG_CAP) {
        Some((cut, _)) => format!("{}…", &trimmed[..cut]),
        None => trimmed.to_string(),
    }
}

/// Whether the runtime is still admitting work.
///
/// Deliberately a two-state enum rather than a `bool` plus a separate deadline
/// field. The deadline only exists while draining, and a `bool` would have let
/// a caller ask for the deadline of a runtime that is not draining — a question
/// with no answer that would have had to be invented as `None` at every call
/// site, or worse, as "now".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DrainState {
    /// Ordinary operation: new work is admitted.
    Open,
    /// A drain frame has been honoured. Admission is closed; the runtime is
    /// finishing what it already held and will then leave the run loop.
    Draining {
        /// When the runtime stops waiting for outstanding work.
        ///
        /// Fixed at onset and never moved — see the module docs on replay.
        deadline: Instant,
    },
}

/// What a drain frame did.
///
/// The distinction is returned rather than logged inside [`DrainState::begin`]
/// because the two callers want different things from it: the run loop emits a
/// runtime lifecycle frame only on a real transition, and the control handler
/// acknowledges *every* frame so a deployer's retry is not met with silence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DrainOnset {
    /// This frame closed admission. The runtime was open before it.
    Started,
    /// The runtime was already draining. Nothing changed, including the
    /// deadline.
    AlreadyDraining,
}

impl DrainOnset {
    /// The `status` an observer `control_result` reports for this onset.
    pub(crate) fn status(self) -> &'static str {
        match self {
            Self::Started => "draining",
            Self::AlreadyDraining => "already_draining",
        }
    }
}

/// Why the run loop is leaving.
///
/// Both variants exit 0 — the deployer asked for a clean stop and gets one
/// either way. They differ in what the operator is told, and that difference is
/// load-bearing: `BoundExpired` means work was abandoned and will be re-run by
/// the successor process, which is a fact worth a loud log rather than a silent
/// success.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DrainExit {
    /// Nothing was left outstanding. The promise was kept in full.
    Complete,
    /// The bound expired with work still in hand.
    BoundExpired,
}

impl DrainState {
    /// A runtime that admits work.
    pub(crate) fn open() -> Self {
        Self::Open
    }

    /// Honour a drain frame.
    ///
    /// Idempotent: a second frame reports [`DrainOnset::AlreadyDraining`] and
    /// leaves the deadline exactly where the first one put it. That is not a
    /// convenience — it is the property that makes replay-within-freshness
    /// harmless without a nonce. See the module docs.
    pub(crate) fn begin(&mut self, now: Instant, bound: Duration) -> DrainOnset {
        match self {
            Self::Open => {
                *self = Self::Draining {
                    deadline: now + bound,
                };
                DrainOnset::Started
            }
            Self::Draining { .. } => DrainOnset::AlreadyDraining,
        }
    }

    /// Whether the runtime is draining.
    pub(crate) fn is_draining(&self) -> bool {
        matches!(self, Self::Draining { .. })
    }

    /// Whether a newly arrived event may become work.
    ///
    /// The one predicate every admission point asks. Spelled as its own method
    /// rather than `!is_draining()` at each site so that the *question* each
    /// site is asking is visible in the call: an admission gate is not asking
    /// about the drain's state machine, it is asking whether it may accept.
    pub(crate) fn admits_new_work(&self) -> bool {
        !self.is_draining()
    }

    /// The instant the drain stops waiting, if it is draining.
    ///
    /// Handed to the run loop's `select!` as a sleep target so the loop wakes to
    /// notice its own bound. Copied out before the `select!` is built rather
    /// than borrowed into it: an arm holding `&self` for the lifetime of the
    /// poll would collide with the control arm's `&mut self`.
    pub(crate) fn deadline(&self) -> Option<Instant> {
        match self {
            Self::Open => None,
            Self::Draining { deadline } => Some(*deadline),
        }
    }

    /// Whether the run loop should now leave, and why.
    ///
    /// `work_outstanding` is the caller's whole answer to "is the runtime still
    /// holding anything" — in-flight turns, queued batches, a heartbeat turn
    /// that started before the drain. It is a parameter rather than something
    /// this module reaches for so the predicate stays pure and the queue stays
    /// the single authority on what it contains.
    ///
    /// `Complete` outranks `BoundExpired` when both are true, because "we
    /// finished" is the truer report of a runtime that emptied its hands on the
    /// last possible tick.
    pub(crate) fn should_exit(&self, work_outstanding: bool, now: Instant) -> Option<DrainExit> {
        let Self::Draining { deadline } = self else {
            return None;
        };
        if !work_outstanding {
            return Some(DrainExit::Complete);
        }
        if now >= *deadline {
            return Some(DrainExit::BoundExpired);
        }
        None
    }
}

/// How long a drain waits for the work it inherited.
///
/// **Derived from the existing max-turn machinery, not chosen.** It is exactly
/// [`crate::queue::in_flight_deadline_secs`] — the same figure the queue
/// already uses to declare an in-flight channel orphaned. Three things follow,
/// and each is why the alternatives were rejected:
///
/// - A turn that began an instant before the drain gets its full configured
///   allowance. A shorter bound would make drain the thing that killed a turn,
///   which is the exact failure it exists to prevent.
/// - A turn that hangs is reaped by the queue's own backstop at roughly the
///   same moment the drain gives up on it, so the two cannot disagree about
///   whether the runtime is still busy for more than one loop iteration.
/// - Nothing is unbounded. A backlog deeper than one turn's worth of time is
///   abandoned *to the successor process*, not lost: the events are relay
///   history, and the replacement binary's subscriptions replay them.
///
/// The rejected alternative was "run the queue dry however long it takes". A
/// busy runtime's queue is refilled by its own turns (a completed turn requeues
/// on failure, and a cancelled batch re-flushes), so "however long it takes" is
/// not a bound at all — and a deployer whose drain never returns has no swap.
pub(crate) fn drain_bound(max_turn_duration_secs: u64) -> Duration {
    Duration::from_secs(crate::queue::in_flight_deadline_secs(
        max_turn_duration_secs,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bound is the queue's own in-flight backstop, not a second opinion
    /// about how long a turn may take.
    #[test]
    fn the_drain_bound_is_the_queue_s_in_flight_deadline() {
        assert_eq!(
            drain_bound(7200),
            Duration::from_secs(crate::queue::in_flight_deadline_secs(7200)),
        );
        assert!(
            drain_bound(60) > Duration::from_secs(60),
            "a turn running to its configured cap must not be cut short by the drain"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn an_open_runtime_admits_and_never_exits() {
        let drain = DrainState::open();
        assert!(drain.admits_new_work());
        assert_eq!(drain.deadline(), None);
        assert_eq!(drain.should_exit(false, Instant::now()), None);
        assert_eq!(drain.should_exit(true, Instant::now()), None);
    }

    #[tokio::test(start_paused = true)]
    async fn the_first_frame_closes_admission() {
        let mut drain = DrainState::open();
        assert_eq!(
            drain.begin(Instant::now(), Duration::from_secs(30)),
            DrainOnset::Started
        );
        assert!(drain.is_draining());
        assert!(
            !drain.admits_new_work(),
            "a draining runtime must refuse new work"
        );
    }

    /// **The replay guarantee.** A second frame inside the freshness window
    /// changes nothing — in particular it does not move the deadline, so a
    /// frame replayed on every reconnect cannot hold the process open.
    #[tokio::test(start_paused = true)]
    async fn a_second_frame_is_a_no_op_and_does_not_extend_the_bound() {
        let mut drain = DrainState::open();
        let bound = Duration::from_secs(30);
        drain.begin(Instant::now(), bound);
        let first_deadline = drain.deadline().expect("draining");

        tokio::time::advance(Duration::from_secs(20)).await;
        assert_eq!(
            drain.begin(Instant::now(), bound),
            DrainOnset::AlreadyDraining
        );
        assert_eq!(
            drain.deadline(),
            Some(first_deadline),
            "a replayed drain frame must not buy the runtime another bound"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn an_empty_draining_runtime_exits_at_once() {
        let mut drain = DrainState::open();
        drain.begin(Instant::now(), Duration::from_secs(30));
        assert_eq!(
            drain.should_exit(false, Instant::now()),
            Some(DrainExit::Complete)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn outstanding_work_holds_the_runtime_until_the_bound() {
        let mut drain = DrainState::open();
        let bound = Duration::from_secs(30);
        drain.begin(Instant::now(), bound);

        assert_eq!(
            drain.should_exit(true, Instant::now()),
            None,
            "in-flight work must finish — this is the whole promise"
        );

        tokio::time::advance(bound).await;
        assert_eq!(
            drain.should_exit(true, Instant::now()),
            Some(DrainExit::BoundExpired),
            "…but not forever"
        );
    }

    /// Emptying its hands on the last tick is a completed drain, not an
    /// expired one. The operator is told the promise was kept.
    #[tokio::test(start_paused = true)]
    async fn finishing_exactly_at_the_bound_reports_complete() {
        let mut drain = DrainState::open();
        let bound = Duration::from_secs(30);
        drain.begin(Instant::now(), bound);
        tokio::time::advance(bound).await;
        assert_eq!(
            drain.should_exit(false, Instant::now()),
            Some(DrainExit::Complete)
        );
    }

    #[test]
    fn an_onset_names_the_status_the_owner_is_acknowledged_with() {
        assert_eq!(DrainOnset::Started.status(), "draining");
        assert_eq!(DrainOnset::AlreadyDraining.status(), "already_draining");
    }

    #[test]
    fn a_long_reason_is_trimmed_rather_than_refused() {
        assert_eq!(trim_reason("  binary swap  "), "binary swap");
        let long = "x".repeat(REASON_LOG_CAP + 50);
        let trimmed = trim_reason(&long);
        assert!(trimmed.ends_with('…'));
        assert_eq!(trimmed.chars().count(), REASON_LOG_CAP + 1);
    }

    /// Multi-byte reasons are cut on a char boundary, not a byte one.
    #[test]
    fn trimming_a_reason_never_splits_a_char() {
        let long = "é".repeat(REASON_LOG_CAP + 10);
        let trimmed = trim_reason(&long);
        assert_eq!(trimmed.chars().count(), REASON_LOG_CAP + 1);
    }
}
