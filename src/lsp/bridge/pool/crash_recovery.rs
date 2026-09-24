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
///
/// Well above the liveness timeout: a server that hangs on some document is
/// failed by the liveness timer only after at least that long, so a period at
/// or below it would count every hang as the end of a healthy run and respawn
/// such a server forever while the document stays open.
pub(super) const HEALTHY_PERIOD: Duration = Duration::from_secs(300);

const _: () =
    assert!(HEALTHY_PERIOD.as_secs() >= 2 * super::liveness_timeout::LivenessTimeout::DEFAULT_SECS);

/// What to do about a connection that just crashed.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum RecoveryDecision {
    /// Attempt recovery after `delay`; `attempt` counts from 1. The caller
    /// owns `reservation` until it commits to the respawn or stands down.
    Retry {
        attempt: u32,
        delay: Duration,
        reservation: Reservation,
    },
    /// A recovery for this key is already waiting out its delay.
    AlreadyScheduled,
    /// This crash exhausted the consecutive attempts. Returned once per
    /// exhaustion, so the caller can report it once.
    GiveUp { attempts: u32 },
    /// Still exhausted; already reported.
    Exhausted,
}

/// One scheduled recovery's claim on its key, so that only the recovery that
/// scheduled can release the schedule: an older recovery finishing late must
/// not release a newer one's, or a third could be scheduled beside it and
/// skip the backoff.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Reservation(u64);

#[derive(Default)]
struct KeyState {
    attempts: u32,
    /// The reservation of the recovery currently waiting out its delay.
    scheduled: Option<Reservation>,
    next_reservation: u64,
    gave_up: bool,
    /// A crash reported while a recovery was already scheduled: that recovery
    /// owes it, and must hand it on if it stands down instead of respawning.
    missed: bool,
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
        if state.scheduled.is_some() {
            state.missed = true;
            return RecoveryDecision::AlreadyScheduled;
        }
        if uptime >= HEALTHY_PERIOD {
            state.attempts = 0;
            state.gave_up = false;
            state.missed = false;
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
        state.next_reservation += 1;
        let reservation = Reservation(state.next_reservation);
        state.scheduled = Some(reservation);
        RecoveryDecision::Retry {
            attempt: state.attempts,
            delay: FIRST_RETRY_DELAY * 2u32.pow(state.attempts - 1),
            reservation,
        }
    }

    /// Commit `key`'s scheduled recovery to respawning: the respawn serves
    /// whatever crashed under the key, including a crash reported while it was
    /// scheduled.
    ///
    /// Clears the schedule BEFORE the respawn, not after it: a replacement that
    /// crashes during its own handshake reports that crash while the attempt is
    /// still awaiting the handshake, and that crash must schedule the next
    /// attempt rather than read as already scheduled.
    pub(super) fn begin_attempt(&self, key: &ConnectionKey, reservation: Reservation) {
        let mut keys = self
            .keys
            .lock()
            .recover_poison("CrashRecoveryRegistry::begin_attempt");
        if let Some(state) = keys.get_mut(key)
            && state.scheduled == Some(reservation)
        {
            state.scheduled = None;
            state.missed = false;
        }
    }

    /// Give back the attempt a recovery took, because it stood down before
    /// committing to a respawn (the connection was already replaced, or
    /// settings or open documents no longer need it). Only respawns spend the
    /// budget: stand-downs after ordinary restarts or configuration changes
    /// must not exhaust it for a later crash that does need recovering.
    ///
    /// Returns the decision for a crash reported while this recovery was
    /// scheduled — absorbed as already scheduled, and otherwise unrecovered.
    ///
    /// A recovery that already committed gives nothing back: its attempt may
    /// be interleaved with newer ones by then, and refunding it could let the
    /// key restart more often than the cap. Erring toward fewer restarts is
    /// the safe direction for a bound.
    pub(super) fn stand_down(
        &self,
        key: &ConnectionKey,
        reservation: Reservation,
    ) -> Option<RecoveryDecision> {
        let mut keys = self
            .keys
            .lock()
            .recover_poison("CrashRecoveryRegistry::stand_down");
        let state = keys.get_mut(key)?;
        if state.scheduled != Some(reservation) {
            return None;
        }
        state.scheduled = None;
        state.attempts = state.attempts.saturating_sub(1);
        let owed = std::mem::take(&mut state.missed);
        drop(keys);
        owed.then(|| self.schedule(key, Duration::ZERO))
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

    /// `(attempt, delay, reservation)` of a `Retry`, panicking otherwise.
    fn retry(decision: RecoveryDecision) -> (u32, Duration, Reservation) {
        match decision {
            RecoveryDecision::Retry {
                attempt,
                delay,
                reservation,
            } => (attempt, delay, reservation),
            other => panic!("expected a retry, got {other:?}"),
        }
    }

    #[test]
    fn first_crash_retries_after_the_first_delay() {
        let registry = CrashRecoveryRegistry::default();
        let (attempt, delay, _) = retry(registry.schedule(&key(), SHORT_RUN));
        assert_eq!((attempt, delay), (1, FIRST_RETRY_DELAY));
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
        for expected in 1..=MAX_CONSECUTIVE_ATTEMPTS {
            let (attempt, delay, reservation) = retry(registry.schedule(&key(), SHORT_RUN));
            assert_eq!(
                (attempt, delay),
                (expected, FIRST_RETRY_DELAY * 2u32.pow(expected - 1))
            );
            registry.begin_attempt(&key(), reservation);
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
        let (_, _, reservation) = retry(registry.schedule(&key(), SHORT_RUN));
        registry.begin_attempt(&key(), reservation);
        assert_eq!(retry(registry.schedule(&key(), SHORT_RUN)).0, 2);
    }

    fn exhaust(registry: &CrashRecoveryRegistry) {
        for _ in 0..MAX_CONSECUTIVE_ATTEMPTS {
            let (_, _, reservation) = retry(registry.schedule(&key(), SHORT_RUN));
            registry.begin_attempt(&key(), reservation);
        }
    }

    #[test]
    fn a_crash_ending_a_healthy_run_starts_over_even_after_giving_up() {
        let registry = CrashRecoveryRegistry::default();
        exhaust(&registry);
        assert!(matches!(
            registry.schedule(&key(), SHORT_RUN),
            RecoveryDecision::GiveUp { .. }
        ));
        let (attempt, delay, _) = retry(registry.schedule(&key(), HEALTHY_PERIOD));
        assert_eq!((attempt, delay), (1, FIRST_RETRY_DELAY));
    }

    /// Time alone does not heal: after giving up, a server an edit restarts
    /// that dies right away again is the same failure, not a new round.
    #[test]
    fn a_short_run_after_giving_up_stays_exhausted() {
        let registry = CrashRecoveryRegistry::default();
        exhaust(&registry);
        let _ = registry.schedule(&key(), SHORT_RUN);
        assert_eq!(
            registry.schedule(&key(), SHORT_RUN),
            RecoveryDecision::Exhausted
        );
    }

    #[test]
    fn keys_back_off_independently() {
        let registry = CrashRecoveryRegistry::default();
        let (_, _, reservation) = retry(registry.schedule(&key(), SHORT_RUN));
        registry.begin_attempt(&key(), reservation);
        let _ = registry.schedule(&key(), SHORT_RUN);
        assert_eq!(
            retry(registry.schedule(&ConnectionKey::for_server("other"), SHORT_RUN)).0,
            1
        );
    }

    #[test]
    fn a_recovery_that_stands_down_spends_no_attempt() {
        let registry = CrashRecoveryRegistry::default();
        for _ in 0..MAX_CONSECUTIVE_ATTEMPTS + 1 {
            let (attempt, _, reservation) = retry(registry.schedule(&key(), SHORT_RUN));
            assert_eq!(attempt, 1);
            assert_eq!(registry.stand_down(&key(), reservation), None);
        }
    }

    #[test]
    fn a_crash_absorbed_while_scheduled_is_handed_on_by_a_stand_down() {
        let registry = CrashRecoveryRegistry::default();
        let (_, _, reservation) = retry(registry.schedule(&key(), SHORT_RUN));
        // A replacement crashed while the recovery waited.
        assert_eq!(
            registry.schedule(&key(), SHORT_RUN),
            RecoveryDecision::AlreadyScheduled
        );
        let handed_on = registry.stand_down(&key(), reservation);
        assert!(matches!(
            handed_on,
            Some(RecoveryDecision::Retry { attempt: 1, .. })
        ));
    }

    /// Once committed, a recovery gives nothing back: newer attempts may be
    /// interleaved with it by then, and a refund could lift the cap.
    #[test]
    fn a_committed_recovery_refunds_nothing() {
        let registry = CrashRecoveryRegistry::default();
        let (_, _, older) = retry(registry.schedule(&key(), SHORT_RUN));
        registry.begin_attempt(&key(), older);
        let (_, _, newer) = retry(registry.schedule(&key(), SHORT_RUN));
        registry.begin_attempt(&key(), newer);
        assert_eq!(registry.stand_down(&key(), older), None);
        assert_eq!(retry(registry.schedule(&key(), SHORT_RUN)).0, 3);
    }

    /// A recovery that committed and then stood down must not release the
    /// schedule of a newer recovery, or a third crash could be scheduled
    /// beside it and skip the backoff.
    #[test]
    fn a_late_stand_down_leaves_a_newer_recovery_scheduled() {
        let registry = CrashRecoveryRegistry::default();
        let (_, _, older) = retry(registry.schedule(&key(), SHORT_RUN));
        registry.begin_attempt(&key(), older);
        let _newer = retry(registry.schedule(&key(), SHORT_RUN));
        assert_eq!(registry.stand_down(&key(), older), None);
        assert_eq!(
            registry.schedule(&key(), SHORT_RUN),
            RecoveryDecision::AlreadyScheduled
        );
    }
}
