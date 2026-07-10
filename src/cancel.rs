//! Cooperative cancellation for long-running semantic-token computation.
//!
//! A full-document semantic-token compute runs on the bounded `ComputePool` and
//! fans out across its Rayon workers. Dropping its future does **not** preempt
//! an already-running work unit, which otherwise runs to completion and burns CPU
//! whose result is
//! then discarded (see `analysis::handle_semantic_tokens_full`). Under rapid
//! typing on a large document that turns into a pile-up: every keystroke
//! supersedes the previous request, but the superseded compute keeps running.
//!
//! [`CancelToken`] is the escape hatch. A compute polls it during the host-query
//! walk, once per injection region during discovery/tokenization, and throughout
//! final token shaping, then bails early once it is set. The token is flipped when
//! the request is superseded by a newer one for the same document, when the client
//! sends `$/cancelRequest`, or when the document closes — so an obsolete compute
//! stops instead of running to completion (see
//! `lsp::semantic_request_tracker::SemanticRequestTracker`).

/// A shared cancellation signal: cheap to poll from blocking compute AND
/// awaitable from async parks. Clones share the same state, so one side can
/// cancel a compute another side is running.
///
/// Backed by [`tokio_util::sync::CancellationToken`] (monotonic
/// `false` → `true`, never reset): `is_cancelled` stays a single atomic load
/// for the per-region compute checkpoints, while [`cancelled`](Self::cancelled)
/// lets the serve-current parked waits complete promptly when a newer request
/// supersedes this one — an `AtomicBool` alone made supersession invisible to
/// a parked future until its next wakeup.
#[derive(Debug, Clone)]
pub(crate) struct CancelToken {
    inner: tokio_util::sync::CancellationToken,
    #[cfg(test)]
    polls_before_cancel: std::sync::Arc<std::sync::atomic::AtomicIsize>,
}

impl Default for CancelToken {
    fn default() -> Self {
        Self {
            inner: tokio_util::sync::CancellationToken::new(),
            #[cfg(test)]
            polls_before_cancel: std::sync::Arc::new(std::sync::atomic::AtomicIsize::new(-1)),
        }
    }
}

impl CancelToken {
    /// Signal cancellation to every holder of this token. Idempotent.
    pub(crate) fn cancel(&self) {
        self.inner.cancel();
    }

    /// Returns `true` once [`cancel`](Self::cancel) has been called on this
    /// token or any clone of it.
    pub(crate) fn is_cancelled(&self) -> bool {
        #[cfg(test)]
        {
            use std::sync::atomic::Ordering;
            let remaining = self.polls_before_cancel.load(Ordering::Relaxed);
            if remaining > 0
                && self
                    .polls_before_cancel
                    .compare_exchange(
                        remaining,
                        remaining - 1,
                        Ordering::Relaxed,
                        Ordering::Relaxed,
                    )
                    .is_ok()
                && remaining == 1
            {
                self.inner.cancel();
            }
        }
        self.inner.is_cancelled()
    }

    /// Completes when [`cancel`](Self::cancel) is called on this token or any
    /// clone of it (immediately if it already was). Cancel-safe: dropping the
    /// future loses nothing.
    pub(crate) async fn cancelled(&self) {
        self.inner.cancelled().await;
    }

    /// Deterministically flip during a later cooperative checkpoint.
    #[cfg(test)]
    pub(crate) fn cancel_after_polls(&self, polls: isize) {
        self.polls_before_cancel
            .store(polls, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Checkpoint helper for the pervasive optional-token case: `true` only when a
/// token is present *and* cancelled. A `None` token (cancellation disabled, e.g.
/// range requests and tests) is never cancelled, so callers collapse to their
/// pre-cancellation behavior.
pub(crate) fn is_cancelled(token: Option<&CancelToken>) -> bool {
    token.is_some_and(|t| t.is_cancelled())
}

/// Check one out of every 64 units of linear work. Callers keep the counter
/// local to a pass and use [`is_cancelled`] for its entry/exit checkpoints.
#[inline]
pub(crate) fn is_cancelled_periodically(
    token: Option<&CancelToken>,
    work_items: &mut usize,
) -> bool {
    *work_items = work_items.wrapping_add(1);
    *work_items & 63 == 0 && is_cancelled(token)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_uncancelled() {
        assert!(!CancelToken::default().is_cancelled());
    }

    #[test]
    fn cancel_is_visible_through_clones() {
        let token = CancelToken::default();
        let clone = token.clone();
        assert!(!clone.is_cancelled());
        token.cancel();
        assert!(clone.is_cancelled(), "clone must observe the flip");
        assert!(token.is_cancelled());
    }

    #[test]
    fn cancel_is_idempotent() {
        let token = CancelToken::default();
        token.cancel();
        token.cancel();
        assert!(token.is_cancelled());
    }
}
