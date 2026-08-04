//! Lazy agent-pool lifecycle state.
//!
//! Relay connection, subscription, and event buffering live outside this
//! module. This state machine owns only whether a deferred pool has not started,
//! is waking, is ready, or is waiting to retry after a failed wake.

use std::time::Duration;
use tokio::time::Instant;

use crate::terminal_auth::TerminalAuth;

const INITIAL_RETRY_DELAY: Duration = Duration::from_secs(5);
const MAX_RETRY_DELAY: Duration = Duration::from_secs(300);

/// Why a lazy pool wake failed.
///
/// The distinction is typed rather than inferred from an error string:
/// `Transient` keeps the existing bounded backoff, `TerminalAuth` has no
/// deadline at all and never wakes again on its own. Erasing the difference
/// into `anyhow` would put an expired credential on the same five-second
/// retry ladder as a busy provider.
#[derive(Debug, Clone)]
pub(crate) enum PoolStartError {
    /// A non-retryable authentication failure classified at the ACP seam.
    TerminalAuth(TerminalAuth),
    /// Anything else — spawn failure, protocol error, provider outage.
    Transient(String),
}

impl PoolStartError {
    /// A safe, categorical summary for logs and lifecycle events. Carries no
    /// provider text for the terminal-auth case.
    pub(crate) fn summary(&self) -> String {
        match self {
            Self::TerminalAuth(terminal) => {
                format!("terminal authentication failure ({terminal})")
            }
            Self::Transient(message) => message.clone(),
        }
    }
}

#[derive(Debug)]
pub(crate) enum PoolLifecycle<P> {
    Listening,
    Waking {
        attempt: u32,
    },
    Ready(P),
    Failed {
        attempt: u32,
        retry_at: Instant,
        error: String,
    },
    /// The provider rejected our credentials. There is no retry deadline and
    /// no automatic wake: advancing the clock changes nothing, because the
    /// credential can only be repaired outside this process. Only an explicit
    /// fresh lifecycle (a restart after re-authentication) leaves this state.
    BlockedAuth {
        terminal: TerminalAuth,
    },
}

impl<P> PoolLifecycle<P> {
    pub(crate) fn listening() -> Self {
        Self::Listening
    }

    /// Start the first wake, or a due retry, when buffered work exists.
    ///
    /// Returns the attempt token exactly once per transition into `Waking`;
    /// callers attach it to the single pool-initialization task and return it
    /// with the result.
    pub(crate) fn start_wake_if_due(
        &mut self,
        has_pending_work: bool,
        now: Instant,
    ) -> Option<u32> {
        if !has_pending_work {
            return None;
        }

        let next_attempt = match self {
            Self::Listening => Some(1),
            Self::Failed {
                attempt, retry_at, ..
            } if now >= *retry_at => Some(attempt.saturating_add(1)),
            // BlockedAuth is absorbing: no amount of pending work or elapsed
            // time makes an expired credential valid.
            Self::Waking { .. }
            | Self::Ready(_)
            | Self::Failed { .. }
            | Self::BlockedAuth { .. } => None,
        };

        if let Some(attempt) = next_attempt {
            *self = Self::Waking { attempt };
        }
        next_attempt
    }

    pub(crate) fn take_ready(&mut self) -> Option<P> {
        match std::mem::replace(self, Self::Listening) {
            Self::Ready(pool) => Some(pool),
            other => {
                *self = other;
                None
            }
        }
    }

    pub(crate) fn waking_attempt(&self) -> Option<u32> {
        match self {
            Self::Waking { attempt } => Some(*attempt),
            _ => None,
        }
    }

    pub(crate) fn retry_at(&self) -> Option<Instant> {
        match self {
            Self::Failed { retry_at, .. } => Some(*retry_at),
            _ => None,
        }
    }

    pub(crate) fn failed_error(&self) -> Option<&str> {
        match self {
            Self::Failed { error, .. } => Some(error),
            _ => None,
        }
    }

    /// The terminal-auth disposition, when the lifecycle is blocked on one.
    pub(crate) fn blocked_auth(&self) -> Option<TerminalAuth> {
        match self {
            Self::BlockedAuth { terminal } => Some(*terminal),
            _ => None,
        }
    }

    pub(crate) fn cancel_wake(&mut self, attempt: u32, error: String, now: Instant) -> bool {
        self.complete_wake(attempt, Err(PoolStartError::Transient(error)), now)
            .is_ok()
    }

    /// Complete the matching in-flight wake attempt.
    ///
    /// A transient failure remains retryable. A terminal authentication
    /// failure does not: it enters `BlockedAuth`, which has no deadline. A
    /// result returned outside `Waking`, or from an older attempt, is
    /// rejected: accepting it could replace a newer pool.
    pub(crate) fn complete_wake(
        &mut self,
        completed_attempt: u32,
        result: Result<P, PoolStartError>,
        now: Instant,
    ) -> Result<(), &'static str> {
        let attempt = match self {
            Self::Waking { attempt } if *attempt == completed_attempt => *attempt,
            Self::Waking { .. } => return Err("wake result attempt did not match Waking attempt"),
            _ => return Err("wake completed while lifecycle was not Waking"),
        };

        *self = match result {
            Ok(pool) => Self::Ready(pool),
            Err(PoolStartError::TerminalAuth(terminal)) => Self::BlockedAuth { terminal },
            Err(PoolStartError::Transient(error)) => Self::Failed {
                attempt,
                retry_at: now + retry_delay(attempt),
                error,
            },
        };
        Ok(())
    }
}

fn retry_delay(attempt: u32) -> Duration {
    let exponent = attempt.saturating_sub(1).min(63);
    let multiplier = 1_u64.checked_shl(exponent).unwrap_or(u64::MAX);
    Duration::from_secs(
        INITIAL_RETRY_DELAY
            .as_secs()
            .saturating_mul(multiplier)
            .min(MAX_RETRY_DELAY.as_secs()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn first_pending_event_starts_exactly_one_wake() {
        let now = Instant::now();
        let mut lifecycle = PoolLifecycle::<()>::listening();

        assert_eq!(lifecycle.start_wake_if_due(false, now), None);
        assert_eq!(lifecycle.start_wake_if_due(true, now), Some(1));
        assert_eq!(lifecycle.start_wake_if_due(true, now), None);
        assert!(matches!(lifecycle, PoolLifecycle::Waking { attempt: 1 }));
    }

    #[tokio::test(start_paused = true)]
    async fn failure_retries_only_when_work_exists_and_deadline_is_due() {
        let now = Instant::now();
        let mut lifecycle = PoolLifecycle::<()>::listening();
        assert_eq!(lifecycle.start_wake_if_due(true, now), Some(1));
        lifecycle
            .complete_wake(
                1,
                Err(PoolStartError::Transient("provider unavailable".into())),
                now,
            )
            .unwrap();

        assert_eq!(
            lifecycle.start_wake_if_due(true, now + Duration::from_secs(4)),
            None
        );
        assert_eq!(
            lifecycle.start_wake_if_due(false, now + Duration::from_secs(5)),
            None
        );
        assert_eq!(
            lifecycle.start_wake_if_due(true, now + Duration::from_secs(5)),
            Some(2)
        );
        assert!(matches!(lifecycle, PoolLifecycle::Waking { attempt: 2 }));
    }

    #[tokio::test(start_paused = true)]
    async fn retry_backoff_doubles_and_caps_at_five_minutes() {
        let mut now = Instant::now();
        let mut lifecycle = PoolLifecycle::<()>::listening();

        for attempt in 1..=9 {
            assert_eq!(lifecycle.start_wake_if_due(true, now), Some(attempt));
            assert!(matches!(
                lifecycle,
                PoolLifecycle::Waking { attempt: actual } if actual == attempt
            ));
            lifecycle
                .complete_wake(
                    attempt,
                    Err(PoolStartError::Transient("no brain".into())),
                    now,
                )
                .unwrap();

            let expected = retry_delay(attempt);
            let retry_at = match &lifecycle {
                PoolLifecycle::Failed { retry_at, .. } => *retry_at,
                _ => panic!("failure must enter Failed"),
            };
            assert_eq!(retry_at, now + expected);
            assert!(expected <= MAX_RETRY_DELAY);
            now = retry_at;
        }

        assert_eq!(retry_delay(7), MAX_RETRY_DELAY);
        assert_eq!(retry_delay(u32::MAX), MAX_RETRY_DELAY);
    }

    #[tokio::test(start_paused = true)]
    async fn successful_retry_consumes_pool_and_stops_future_wakes() {
        let now = Instant::now();
        let mut lifecycle = PoolLifecycle::listening();
        assert_eq!(lifecycle.start_wake_if_due(true, now), Some(1));
        lifecycle
            .complete_wake(
                1,
                Err(PoolStartError::Transient("first attempt failed".into())),
                now,
            )
            .unwrap();

        let retry_at = match &lifecycle {
            PoolLifecycle::Failed { retry_at, .. } => *retry_at,
            _ => panic!("expected Failed"),
        };
        assert_eq!(lifecycle.start_wake_if_due(true, retry_at), Some(2));
        lifecycle.complete_wake(2, Ok("pool"), retry_at).unwrap();

        assert!(matches!(lifecycle, PoolLifecycle::Ready("pool")));
        assert_eq!(
            lifecycle.start_wake_if_due(true, retry_at + Duration::from_secs(600)),
            None
        );
    }

    #[tokio::test(start_paused = true)]
    async fn stale_or_duplicate_wake_result_is_rejected() {
        let now = Instant::now();
        let mut lifecycle = PoolLifecycle::<()>::listening();
        assert_eq!(
            lifecycle.complete_wake(1, Ok(()), now),
            Err("wake completed while lifecycle was not Waking")
        );

        assert_eq!(lifecycle.start_wake_if_due(true, now), Some(1));
        lifecycle.complete_wake(1, Ok(()), now).unwrap();
        assert_eq!(
            lifecycle.complete_wake(1, Ok(()), now),
            Err("wake completed while lifecycle was not Waking")
        );
        assert!(matches!(lifecycle, PoolLifecycle::Ready(())));
    }

    #[tokio::test(start_paused = true)]
    async fn stale_attempt_result_cannot_replace_current_wake() {
        let now = Instant::now();
        let mut lifecycle = PoolLifecycle::<&str>::listening();
        assert_eq!(lifecycle.start_wake_if_due(true, now), Some(1));
        lifecycle
            .complete_wake(
                1,
                Err(PoolStartError::Transient("attempt one failed".into())),
                now,
            )
            .unwrap();

        let retry_at = match &lifecycle {
            PoolLifecycle::Failed { retry_at, .. } => *retry_at,
            _ => panic!("expected Failed"),
        };
        assert_eq!(lifecycle.start_wake_if_due(true, retry_at), Some(2));
        assert_eq!(
            lifecycle.complete_wake(1, Ok("stale pool"), retry_at),
            Err("wake result attempt did not match Waking attempt")
        );
        assert!(matches!(lifecycle, PoolLifecycle::Waking { attempt: 2 }));
        lifecycle
            .complete_wake(2, Ok("current pool"), retry_at)
            .unwrap();
        assert!(matches!(lifecycle, PoolLifecycle::Ready("current pool")));
    }

    #[tokio::test(start_paused = true)]
    async fn cancelled_wake_enters_failed_and_can_retry() {
        let now = Instant::now();
        let mut lifecycle = PoolLifecycle::<()>::listening();
        assert_eq!(lifecycle.start_wake_if_due(true, now), Some(1));
        assert_eq!(lifecycle.waking_attempt(), Some(1));
        assert!(lifecycle.cancel_wake(1, "task panicked".into(), now));
        assert_eq!(lifecycle.failed_error(), Some("task panicked"));
        assert_eq!(
            lifecycle.start_wake_if_due(true, now + Duration::from_secs(5)),
            Some(2)
        );
    }

    #[test]
    fn take_ready_transfers_pool_exactly_once() {
        let now = Instant::now();
        let mut lifecycle = PoolLifecycle::listening();
        assert_eq!(lifecycle.start_wake_if_due(true, now), Some(1));
        lifecycle.complete_wake(1, Ok("pool"), now).unwrap();
        assert_eq!(lifecycle.take_ready(), Some("pool"));
        assert_eq!(lifecycle.take_ready(), None);
    }

    fn terminal() -> TerminalAuth {
        TerminalAuth {
            adapter: crate::terminal_auth::AdapterFamily::Claude,
            stage: crate::terminal_auth::AuthStage::SessionNew,
            signal: crate::terminal_auth::AuthSignal::Structured,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn terminal_auth_wake_blocks_and_never_wakes_again() {
        let now = Instant::now();
        let mut lifecycle = PoolLifecycle::<()>::listening();
        assert_eq!(lifecycle.start_wake_if_due(true, now), Some(1));
        lifecycle
            .complete_wake(1, Err(PoolStartError::TerminalAuth(terminal())), now)
            .unwrap();

        assert!(matches!(lifecycle, PoolLifecycle::BlockedAuth { .. }));
        assert_eq!(lifecycle.blocked_auth(), Some(terminal()));
        assert_eq!(
            lifecycle.retry_at(),
            None,
            "blocked-auth must expose no retry deadline"
        );
        assert_eq!(lifecycle.failed_error(), None);

        // Advancing time by an hour, with work pending the whole way, still
        // never schedules a wake.
        for minutes in [1_u64, 5, 30, 60] {
            assert_eq!(
                lifecycle.start_wake_if_due(true, now + Duration::from_secs(minutes * 60)),
                None,
                "blocked-auth must not wake after {minutes} minutes"
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn transient_wake_keeps_its_bounded_backoff_alongside_the_terminal_state() {
        let now = Instant::now();
        let mut lifecycle = PoolLifecycle::<()>::listening();
        assert_eq!(lifecycle.start_wake_if_due(true, now), Some(1));
        lifecycle
            .complete_wake(
                1,
                Err(PoolStartError::Transient("provider down".into())),
                now,
            )
            .unwrap();
        assert_eq!(lifecycle.retry_at(), Some(now + INITIAL_RETRY_DELAY));
        assert_eq!(lifecycle.failed_error(), Some("provider down"));
        assert_eq!(lifecycle.blocked_auth(), None);
        assert_eq!(
            lifecycle.start_wake_if_due(true, now + INITIAL_RETRY_DELAY),
            Some(2)
        );
    }

    #[test]
    fn a_fresh_lifecycle_after_reauthentication_accepts_work_again() {
        let now = Instant::now();
        let mut lifecycle = PoolLifecycle::<&str>::listening();
        assert_eq!(lifecycle.start_wake_if_due(true, now), Some(1));
        lifecycle
            .complete_wake(1, Err(PoolStartError::TerminalAuth(terminal())), now)
            .unwrap();
        assert_eq!(lifecycle.start_wake_if_due(true, now), None);

        // Only an explicit fresh lifecycle — what a restart builds — leaves
        // the blocked state.
        let mut restarted = PoolLifecycle::<&str>::listening();
        assert_eq!(restarted.start_wake_if_due(true, now), Some(1));
        restarted.complete_wake(1, Ok("pool"), now).unwrap();
        assert!(matches!(restarted, PoolLifecycle::Ready("pool")));
    }

    #[test]
    fn pool_start_error_summary_carries_no_provider_text() {
        let summary = PoolStartError::TerminalAuth(terminal()).summary();
        assert_eq!(
            summary,
            "terminal authentication failure (adapter=claude stage=session_new signal=structured)"
        );
        assert_eq!(
            PoolStartError::Transient("spawn failed".into()).summary(),
            "spawn failed"
        );
    }

    #[test]
    fn failed_state_preserves_attempt_deadline_and_error() {
        let now = Instant::now();
        let mut lifecycle = PoolLifecycle::<()>::listening();
        assert_eq!(lifecycle.start_wake_if_due(true, now), Some(1));
        lifecycle
            .complete_wake(1, Err(PoolStartError::Transient("boom".into())), now)
            .unwrap();

        match lifecycle {
            PoolLifecycle::Failed {
                attempt,
                retry_at,
                error,
            } => {
                assert_eq!(attempt, 1);
                assert_eq!(retry_at, now + Duration::from_secs(5));
                assert_eq!(error, "boom");
            }
            _ => panic!("expected Failed"),
        }
    }
}
