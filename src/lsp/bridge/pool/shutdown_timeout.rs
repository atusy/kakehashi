//! Global shutdown timeout for language server pool.
//!
//! This module provides a validated timeout type for graceful shutdown
//! per ls-bridge-timeout-hierarchy Tier 3 (5-15s range).

use std::time::Duration;

/// Tier-3 global shutdown ceiling for all connections (ls-bridge-timeout-hierarchy, 5–15s,
/// default 10s). Parallel shutdown under one timeout (ls-bridge-graceful-shutdown); on expiry,
/// survivors get `force_kill_all` (SIGTERM→SIGKILL). The 5s floor lets fast
/// servers finish the LSP handshake; the 15s ceiling caps user wait time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct GlobalShutdownTimeout(Duration);

impl GlobalShutdownTimeout {
    /// Default timeout: 10 seconds
    const DEFAULT_SECS: u64 = 10;

    /// Minimum valid timeout: 5 seconds (used in validation)
    #[cfg(test)]
    const MIN_SECS: u64 = 5;

    /// Maximum valid timeout: 15 seconds (used in validation)
    #[cfg(test)]
    const MAX_SECS: u64 = 15;

    /// Construct with range check; production currently uses `default()` and
    /// this only runs in tests until the timeout is exposed in config.
    ///
    /// Asymmetric bounds: lower bound uses whole-second floor (accept ≥5.0s,
    /// reject 4.999s) so the LSP handshake always has at least 5s; upper bound
    /// is strict (reject 15.001s) so user wait time can't drift past 15s.
    #[cfg(test)]
    pub(crate) fn new(duration: Duration) -> std::io::Result<Self> {
        let secs = duration.as_secs();
        let has_subsec = duration.subsec_nanos() > 0;

        // Check minimum: must be at least 5 whole seconds
        // 4.999s has secs=4, so it's rejected. 5.001s has secs=5, so it's accepted.
        if secs < Self::MIN_SECS {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "Global shutdown timeout must be at least {}s, got {:?}",
                    Self::MIN_SECS,
                    duration
                ),
            ));
        }

        // Check maximum: must be at most exactly 15 seconds
        // 15.0s is accepted. 15.001s is rejected (has_subsec is true when secs=15).
        if secs > Self::MAX_SECS || (secs == Self::MAX_SECS && has_subsec) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "Global shutdown timeout must be at most {}s, got {:?}",
                    Self::MAX_SECS,
                    duration
                ),
            ));
        }

        Ok(Self(duration))
    }

    /// Get the inner Duration value.
    pub(crate) fn as_duration(&self) -> Duration {
        self.0
    }
}

impl Default for GlobalShutdownTimeout {
    fn default() -> Self {
        Self(Duration::from_secs(Self::DEFAULT_SECS))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test that GlobalShutdownTimeout type rejects values outside 5-15s range.
    ///
    /// ls-bridge-timeout-hierarchy specifies the global shutdown timeout should be 5-15s.
    /// This test verifies the newtype validation rejects out-of-range values.
    #[test]
    fn global_shutdown_timeout_rejects_out_of_range() {
        // Below minimum: 4 seconds
        let too_short = GlobalShutdownTimeout::new(Duration::from_secs(4));
        assert!(too_short.is_err(), "4s should be rejected as too short");

        // Above maximum: 16 seconds
        let too_long = GlobalShutdownTimeout::new(Duration::from_secs(16));
        assert!(too_long.is_err(), "16s should be rejected as too long");

        // Zero duration
        let zero = GlobalShutdownTimeout::new(Duration::ZERO);
        assert!(zero.is_err(), "0s should be rejected");
    }

    /// Test that GlobalShutdownTimeout provides access to inner Duration.
    #[test]
    fn global_shutdown_timeout_as_duration() {
        let timeout = GlobalShutdownTimeout::new(Duration::from_secs(10)).expect("10s is valid");

        assert_eq!(timeout.as_duration(), Duration::from_secs(10));
    }

    /// Test sub-second boundary validation as documented.
    ///
    /// Per the documented boundary behavior:
    /// - Minimum: floor at 5 whole seconds (4.999s rejected, 5.001s accepted)
    /// - Maximum: ceiling at exactly 15 seconds (15.001s rejected)
    #[test]
    fn global_shutdown_timeout_subsecond_boundaries() {
        // 4.999s has secs=4, rejected (floor is 5 whole seconds)
        let just_under_min = GlobalShutdownTimeout::new(Duration::from_millis(4999));
        assert!(
            just_under_min.is_err(),
            "4.999s should be rejected (secs=4, under minimum)"
        );

        // 5.001s has secs=5, accepted
        let just_over_min = GlobalShutdownTimeout::new(Duration::from_millis(5001));
        assert!(just_over_min.is_ok(), "5.001s should be accepted (secs=5)");

        // 5.5s accepted (mid-range subsecond)
        let mid_subsec = GlobalShutdownTimeout::new(Duration::from_millis(5500));
        assert!(mid_subsec.is_ok(), "5.5s should be accepted");

        // 10.123s accepted (arbitrary subsecond)
        let arbitrary_subsec = GlobalShutdownTimeout::new(Duration::from_millis(10123));
        assert!(arbitrary_subsec.is_ok(), "10.123s should be accepted");

        // 15.0s exactly accepted (maximum boundary)
        let exact_max = GlobalShutdownTimeout::new(Duration::from_secs(15));
        assert!(exact_max.is_ok(), "15.0s exactly should be accepted");

        // 15.001s rejected (ceiling is exactly 15s)
        let just_over_max = GlobalShutdownTimeout::new(Duration::from_millis(15001));
        assert!(
            just_over_max.is_err(),
            "15.001s should be rejected (over maximum)"
        );

        // 15s + 1 nanosecond rejected
        let one_nano_over =
            GlobalShutdownTimeout::new(Duration::from_secs(15) + Duration::from_nanos(1));
        assert!(
            one_nano_over.is_err(),
            "15s + 1ns should be rejected (ceiling is exactly 15s)"
        );
    }

    /// Test default GlobalShutdownTimeout value.
    ///
    /// Default should be exactly 10s per ls-bridge-timeout-hierarchy recommendation - a balance between
    /// allowing graceful shutdown for fast servers and bounding user wait time.
    ///
    /// This also documents the architectural property: GlobalShutdownTimeout is the
    /// only timeout configuration for shutdown. Individual connection shutdowns
    /// don't have their own timeouts - they're bounded by this global ceiling.
    /// (See `connection_handle.rs` for the implementation that relies on this.)
    #[test]
    fn global_shutdown_timeout_default() {
        let default_timeout = GlobalShutdownTimeout::default();

        // Assert exact default value, not just range - ensures intentional changes
        assert_eq!(
            default_timeout.as_duration(),
            Duration::from_secs(10),
            "Default should be exactly 10s per ls-bridge-timeout-hierarchy"
        );
    }
}
