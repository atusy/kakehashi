//! Bounded proactive recovery of crashed downstream connections (#977).
//!
//! A crash evicts the dead connection's pushed diagnostics (#469), and until
//! something acquires the connection again nothing respawns it — on a document
//! nobody edits, its diagnostics stay gone. Recovery acquires the connection
//! proactively; the respawn's re-open (respawn-reopen-derives-its-targets) then
//! brings the replacement up to date, and its pushes restore the diagnostics.
//!
//! This registry only decides WHEN, per connection key. A server that dies on
//! every start would otherwise be respawned in a hot loop, so attempts back off
//! exponentially and stop after [`MAX_CONSECUTIVE_ATTEMPTS`]. The count resets
//! once a connection outlives [`HEALTHY_PERIOD`] after the last attempt, so an
//! occasional crash in a long session is always recovered. Giving up only stops
//! the proactive path: the next edit or request still respawns the server the
//! ordinary way.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use tokio::time::Instant;

use super::ConnectionKey;
use crate::error::LockResultExt;

/// Delay before the first recovery attempt; each further consecutive attempt
/// doubles it.
///
/// Not shorter than the 1 s per-host wire quiet window: the eviction's cleared
/// publish can be withheld into that window's trailing flush, and a replacement
/// re-pushing inside it would merge into the same flush — the editor would
/// never see the crash, and neither would a test asserting it.
pub(crate) const FIRST_RETRY_DELAY: Duration = Duration::from_secs(2);

/// Consecutive attempts before recovery gives up on a key: 2+4+8+16+32 s, about
/// a minute of a server dying on every start.
pub(crate) const MAX_CONSECUTIVE_ATTEMPTS: u32 = 5;

/// How long after the last attempt a crash counts as unrelated to it, starting
/// the backoff over.
pub(crate) const HEALTHY_PERIOD: Duration = Duration::from_secs(60);

/// What to do about a connection that just crashed.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum RecoveryDecision {
    /// Attempt recovery after `delay`; `attempt` counts from 1.
    Retry { attempt: u32, delay: Duration },
    /// A recovery for this key is already waiting out its delay.
    AlreadyScheduled,
    /// This crash exhausted the consecutive attempts. Returned once per
    /// exhaustion, so the caller can report it once.
    GiveUp { attempts: u32 },
    /// Still exhausted; already reported.
    Exhausted,
}

#[derive(Default)]
struct KeyState {
    attempts: u32,
    last_attempt_at: Option<Instant>,
    scheduled: bool,
    gave_up: bool,
}

#[derive(Default)]
pub(crate) struct CrashRecoveryRegistry {
    keys: Mutex<HashMap<ConnectionKey, KeyState>>,
}

impl CrashRecoveryRegistry {
    /// Decide how to recover `key`'s connection, which crashed at `now`.
    pub(crate) fn schedule(&self, key: &ConnectionKey, now: Instant) -> RecoveryDecision {
        let mut keys = self
            .keys
            .lock()
            .recover_poison("CrashRecoveryRegistry::schedule");
        let state = keys.entry(key.clone()).or_default();
        if state.scheduled {
            return RecoveryDecision::AlreadyScheduled;
        }
        if state
            .last_attempt_at
            .is_some_and(|at| now.saturating_duration_since(at) >= HEALTHY_PERIOD)
        {
            *state = KeyState::default();
        }
        if state.attempts >= MAX_CONSECUTIVE_ATTEMPTS {
            if state.gave_up {
                return RecoveryDecision::Exhausted;
            }
            state.gave_up = true;
            return RecoveryDecision::GiveUp {
                attempts: state.attempts,
            };
        }
        state.attempts += 1;
        state.scheduled = true;
        RecoveryDecision::Retry {
            attempt: state.attempts,
            delay: FIRST_RETRY_DELAY * 2u32.pow(state.attempts - 1),
        }
    }

    /// Mark `key`'s scheduled recovery as starting at `now`.
    ///
    /// Clears the schedule BEFORE the respawn, not after it: a replacement that
    /// crashes during its own handshake reports that crash while the attempt is
    /// still awaiting the handshake, and that crash must schedule the next
    /// attempt rather than read as already scheduled.
    pub(crate) fn begin_attempt(&self, key: &ConnectionKey, now: Instant) {
        let mut keys = self
            .keys
            .lock()
            .recover_poison("CrashRecoveryRegistry::begin_attempt");
        let state = keys.entry(key.clone()).or_default();
        state.scheduled = false;
        state.last_attempt_at = Some(now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> ConnectionKey {
        ConnectionKey::for_server("crashy")
    }

    #[tokio::test(start_paused = true)]
    async fn first_crash_retries_after_the_first_delay() {
        let registry = CrashRecoveryRegistry::default();
        assert_eq!(
            registry.schedule(&key(), Instant::now()),
            RecoveryDecision::Retry {
                attempt: 1,
                delay: FIRST_RETRY_DELAY
            }
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_second_crash_report_while_scheduled_is_absorbed() {
        let registry = CrashRecoveryRegistry::default();
        let now = Instant::now();
        let _ = registry.schedule(&key(), now);
        assert_eq!(
            registry.schedule(&key(), now),
            RecoveryDecision::AlreadyScheduled
        );
    }

    #[tokio::test(start_paused = true)]
    async fn consecutive_crashes_back_off_then_give_up_once() {
        let registry = CrashRecoveryRegistry::default();
        let mut now = Instant::now();
        for attempt in 1..=MAX_CONSECUTIVE_ATTEMPTS {
            assert_eq!(
                registry.schedule(&key(), now),
                RecoveryDecision::Retry {
                    attempt,
                    delay: FIRST_RETRY_DELAY * 2u32.pow(attempt - 1)
                }
            );
            now += FIRST_RETRY_DELAY * 2u32.pow(attempt - 1);
            registry.begin_attempt(&key(), now);
            // The replacement dies right away.
            now += Duration::from_millis(100);
        }
        assert_eq!(
            registry.schedule(&key(), now),
            RecoveryDecision::GiveUp {
                attempts: MAX_CONSECUTIVE_ATTEMPTS
            }
        );
        assert_eq!(registry.schedule(&key(), now), RecoveryDecision::Exhausted);
    }

    #[tokio::test(start_paused = true)]
    async fn a_crash_during_the_attempt_schedules_the_next_one() {
        let registry = CrashRecoveryRegistry::default();
        let now = Instant::now();
        let _ = registry.schedule(&key(), now);
        registry.begin_attempt(&key(), now);
        assert!(matches!(
            registry.schedule(&key(), now),
            RecoveryDecision::Retry { attempt: 2, .. }
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn a_healthy_period_resets_the_backoff_even_after_giving_up() {
        let registry = CrashRecoveryRegistry::default();
        let now = Instant::now();
        for _ in 0..MAX_CONSECUTIVE_ATTEMPTS {
            let _ = registry.schedule(&key(), now);
            registry.begin_attempt(&key(), now);
        }
        assert!(matches!(
            registry.schedule(&key(), now),
            RecoveryDecision::GiveUp { .. }
        ));
        assert_eq!(
            registry.schedule(&key(), now + HEALTHY_PERIOD),
            RecoveryDecision::Retry {
                attempt: 1,
                delay: FIRST_RETRY_DELAY
            }
        );
    }

    #[tokio::test(start_paused = true)]
    async fn keys_back_off_independently() {
        let registry = CrashRecoveryRegistry::default();
        let now = Instant::now();
        let _ = registry.schedule(&key(), now);
        registry.begin_attempt(&key(), now);
        let _ = registry.schedule(&key(), now);
        assert!(matches!(
            registry.schedule(&ConnectionKey::for_server("other"), now),
            RecoveryDecision::Retry { attempt: 1, .. }
        ));
    }
}
