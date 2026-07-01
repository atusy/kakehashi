//! Cooperative cancellation for long-running semantic-token computation.
//!
//! A full-document semantic-token compute runs on tokio's blocking pool and
//! fans out across Rayon workers. Dropping its future does **not** stop it — the
//! blocking task detaches and runs to completion, burning CPU whose result is
//! then discarded (see `analysis::handle_semantic_tokens_full`). Under rapid
//! typing on a large document that turns into a pile-up: every keystroke
//! supersedes the previous request, but the superseded compute keeps running.
//!
//! [`CancelToken`] is the escape hatch. A compute polls it at coarse boundaries
//! (between the host pass and the injection pass, and once per injection region
//! during discovery/tokenization) and bails early once it is set. The token is
//! flipped when the request is superseded by a newer one for the same document,
//! when the client sends `$/cancelRequest`, or when the document closes — so an
//! obsolete compute stops instead of running to completion (see
//! `lsp::semantic_request_tracker::SemanticRequestTracker`).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// A shared, cheap-to-poll cancellation signal. Clones share the same flag, so
/// one side can cancel a compute another side is running.
///
/// The flag is monotonic (`false` → `true`, never reset) and carries no other
/// state, so [`Ordering::Relaxed`] is sufficient — we only need eventual
/// visibility of the flip, not to publish other memory through it.
#[derive(Debug, Clone, Default)]
pub(crate) struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    /// Signal cancellation to every holder of this token. Idempotent.
    pub(crate) fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    /// Returns `true` once [`cancel`](Self::cancel) has been called on this
    /// token or any clone of it.
    pub(crate) fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

/// Checkpoint helper for the pervasive optional-token case: `true` only when a
/// token is present *and* cancelled. A `None` token (cancellation disabled, e.g.
/// range requests and tests) is never cancelled, so callers collapse to their
/// pre-cancellation behavior.
pub(crate) fn is_cancelled(token: Option<&CancelToken>) -> bool {
    token.is_some_and(|t| t.is_cancelled())
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
