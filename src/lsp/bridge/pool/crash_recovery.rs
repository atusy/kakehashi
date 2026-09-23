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
//! exponentially and stop after [`MAX_CONSECUTIVE_ATTEMPTS`]. The count starts
//! over only when a connection that ran for at least [`HEALTHY_PERIOD`]
//! crashes: a crash that ends a healthy run is a new incident, while one that
//! ends a short run is the same one continuing — however long ago the last
//! attempt was, so an edit that restarts a server recovery gave up on does not
//! buy that server another round. Giving up only stops the proactive path: the
//! next edit or request still respawns the server the ordinary way.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use super::ConnectionKey;
use crate::error::LockResultExt;

/// Delay before the first recovery attempt; each further consecutive attempt
/// doubles it.
///
/// Longer than the default `maxWaitMs` (1 s) of the `publishDiagnostics` quiet
/// window, which bounds how long the eviction's cleared publish can be
/// withheld: a replacement re-pushing inside that window would merge into the
/// same flush, and the editor would never see the crash. A user who raises
/// `maxWaitMs` past this delay can see exactly that — the clear and the
/// re-push collapsed into one publish.
pub(super) const FIRST_RETRY_DELAY: Duration = Duration::from_secs(2);

/// Consecutive attempts before recovery gives up on a key: 2+4+8+16+32 s, about
/// a minute of a server dying on every start.
pub(super) const MAX_CONSECUTIVE_ATTEMPTS: u32 = 5;

// The delay doubles per attempt from a `u32` factor; keep the largest one well
// inside `u32` so raising the cap cannot silently overflow it.
const _: () = assert!(MAX_CONSECUTIVE_ATTEMPTS <= 16);

/// How long a connection must have run for its crash to start the backoff
/// over.
pub(super) const HEALTHY_PERIOD: Duration = Duration::from_secs(60);

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
    scheduled: bool,
    gave_up: bool,
}

#[derive(Default)]
pub(super) struct CrashRecoveryRegistry {
    keys: Mutex<HashMap<ConnectionKey, KeyState>>,
}

impl CrashRecoveryRegistry {
    /// Decide how to recover `key`'s connection, which crashed after running
    /// for `uptime`.
    pub(super) fn schedule(&self, key: &ConnectionKey, uptime: Duration) -> RecoveryDecision {
        let mut keys = self
            .keys
            .lock()
            .recover_poison("CrashRecoveryRegistry::schedule");
        let state = keys.entry(key.clone()).or_default();
        if state.scheduled {
            return RecoveryDecision::AlreadyScheduled;
        }
        if uptime >= HEALTHY_PERIOD {
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

    /// Mark `key`'s scheduled recovery as starting.
    ///
    /// Clears the schedule BEFORE the respawn, not after it: a replacement that
    /// crashes during its own handshake reports that crash while the attempt is
    /// still awaiting the handshake, and that crash must schedule the next
    /// attempt rather than read as already scheduled.
    pub(super) fn begin_attempt(&self, key: &ConnectionKey) {
        let mut keys = self
            .keys
            .lock()
            .recover_poison("CrashRecoveryRegistry::begin_attempt");
        keys.entry(key.clone()).or_default().scheduled = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> ConnectionKey {
        ConnectionKey::for_server("crashy")
    }

    /// A replacement that died shortly after starting.
    const SHORT_RUN: Duration = Duration::from_millis(100);

    #[test]
    fn first_crash_retries_after_the_first_delay() {
        let registry = CrashRecoveryRegistry::default();
        assert_eq!(
            registry.schedule(&key(), SHORT_RUN),
            RecoveryDecision::Retry {
                attempt: 1,
                delay: FIRST_RETRY_DELAY
            }
        );
    }

    #[test]
    fn a_second_crash_report_while_scheduled_is_absorbed() {
        let registry = CrashRecoveryRegistry::default();
        let _ = registry.schedule(&key(), SHORT_RUN);
        assert_eq!(
            registry.schedule(&key(), SHORT_RUN),
            RecoveryDecision::AlreadyScheduled
        );
    }

    #[test]
    fn consecutive_crashes_back_off_then_give_up_once() {
        let registry = CrashRecoveryRegistry::default();
        for attempt in 1..=MAX_CONSECUTIVE_ATTEMPTS {
            assert_eq!(
                registry.schedule(&key(), SHORT_RUN),
                RecoveryDecision::Retry {
                    attempt,
                    delay: FIRST_RETRY_DELAY * 2u32.pow(attempt - 1)
                }
            );
            registry.begin_attempt(&key());
        }
        assert_eq!(
            registry.schedule(&key(), SHORT_RUN),
            RecoveryDecision::GiveUp {
                attempts: MAX_CONSECUTIVE_ATTEMPTS
            }
        );
        assert_eq!(
            registry.schedule(&key(), SHORT_RUN),
            RecoveryDecision::Exhausted
        );
    }

    #[test]
    fn a_crash_during_the_attempt_schedules_the_next_one() {
        let registry = CrashRecoveryRegistry::default();
        let _ = registry.schedule(&key(), SHORT_RUN);
        registry.begin_attempt(&key());
        assert!(matches!(
            registry.schedule(&key(), SHORT_RUN),
            RecoveryDecision::Retry { attempt: 2, .. }
        ));
    }

    #[test]
    fn a_crash_ending_a_healthy_run_starts_over_even_after_giving_up() {
        let registry = CrashRecoveryRegistry::default();
        for _ in 0..MAX_CONSECUTIVE_ATTEMPTS {
            let _ = registry.schedule(&key(), SHORT_RUN);
            registry.begin_attempt(&key());
        }
        assert!(matches!(
            registry.schedule(&key(), SHORT_RUN),
            RecoveryDecision::GiveUp { .. }
        ));
        assert_eq!(
            registry.schedule(&key(), HEALTHY_PERIOD),
            RecoveryDecision::Retry {
                attempt: 1,
                delay: FIRST_RETRY_DELAY
            }
        );
    }

    /// Time alone does not heal: after giving up, a server an edit restarts
    /// that dies right away again is the same failure, not a new round.
    #[test]
    fn a_short_run_after_giving_up_stays_exhausted() {
        let registry = CrashRecoveryRegistry::default();
        for _ in 0..MAX_CONSECUTIVE_ATTEMPTS {
            let _ = registry.schedule(&key(), SHORT_RUN);
            registry.begin_attempt(&key());
        }
        let _ = registry.schedule(&key(), SHORT_RUN);
        assert_eq!(
            registry.schedule(&key(), SHORT_RUN),
            RecoveryDecision::Exhausted
        );
    }

    #[test]
    fn keys_back_off_independently() {
        let registry = CrashRecoveryRegistry::default();
        let _ = registry.schedule(&key(), SHORT_RUN);
        registry.begin_attempt(&key());
        let _ = registry.schedule(&key(), SHORT_RUN);
        assert!(matches!(
            registry.schedule(&ConnectionKey::for_server("other"), SHORT_RUN),
            RecoveryDecision::Retry { attempt: 1, .. }
        ));
    }
}
