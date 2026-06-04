//! Liveness timeout for downstream language servers.

use std::time::Duration;

/// Tier-2 zombie-server timeout (ls-bridge-timeout-hierarchy, 30–120s).
///
/// Active only in Ready with `pending > 0`: starts on 0→1, resets on any stdout
/// activity, stops on return to 0, and fires Ready→Failed if it elapses (ls-bridge-async-connection).
/// Construction does not validate the range — a future user-configurable knob
/// (e.g. `bridge.livenessTimeoutSecs`) should land alongside a checked constructor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LivenessTimeout(Duration);

impl LivenessTimeout {
    /// Default timeout: 60 seconds (middle of ls-bridge-timeout-hierarchy recommended 30-120s range)
    const DEFAULT_SECS: u64 = 60;

    /// Get the inner Duration value.
    pub(crate) fn as_duration(&self) -> Duration {
        self.0
    }
}

impl Default for LivenessTimeout {
    fn default() -> Self {
        Self(Duration::from_secs(Self::DEFAULT_SECS))
    }
}
