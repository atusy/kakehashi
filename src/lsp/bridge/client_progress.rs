//! Per-client-request work-done progress aggregation (ls-bridge-client-progress).
//!
//! When the editor attaches a `workDoneToken` to a request, the bridge fans the
//! request out to several downstream servers. Each downstream may report
//! `$/progress` against the bridge-minted token it was handed; forwarding those
//! verbatim would duplicate the lifecycle (N `Begin`, N `End`) on the editor's
//! single token. A [`ClientProgressAggregator`] translates the per-downstream
//! progress into **one** coherent lifecycle on the client's own token.
//!
//! This module is the pure state machine — it decides *what* upstream
//! `$/progress` to emit; the bridge wiring feeds it downstream events and
//! forwards the [`ProgressParams`] it returns (ungated, since the editor minted
//! the token and needs no `window/workDoneProgress/create`).
//!
//! # Staging
//!
//! Per the non-regressing rule, the aggregator **drops any progress it does not
//! yet handle — it never forwards raw downstream progress**. Currently only the
//! single-downstream (`N = 1`) relay is implemented; multi-server fan-out
//! (`preferred`/`concatenated`) is dropped until those stages land.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use tower_lsp_server::ls_types::{
    NumberOrString, ProgressParams, ProgressParamsValue, WorkDoneProgress,
};

use crate::error::LockResultExt;

/// Routing table from a bridge-minted per-server progress token to the
/// aggregator that owns it. The reader looks up an incoming `$/progress` token
/// here to feed the right aggregator; `dispatch_*` registers the tokens when it
/// fans out and deregisters them when the request completes.
///
/// Shared (`Arc`) between every reader task and the dispatch path.
#[derive(Default)]
pub(crate) struct ClientProgressRegistry {
    inner: Mutex<HashMap<NumberOrString, ClientProgressRoute>>,
    /// Monotonic source of unique per-server bridge tokens.
    counter: AtomicU64,
}

struct ClientProgressRoute {
    aggregator: std::sync::Arc<Mutex<ClientProgressAggregator>>,
    server: String,
}

impl ClientProgressRegistry {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Mint a unique bridge token for `(aggregator, server)`, register the route,
    /// and return the token to hand to that downstream as its `workDoneToken`.
    pub(crate) fn register(
        &self,
        aggregator: std::sync::Arc<Mutex<ClientProgressAggregator>>,
        server: &str,
    ) -> NumberOrString {
        let suffix = self.counter.fetch_add(1, Ordering::Relaxed);
        let token = NumberOrString::String(format!("kakehashi/bridge/cprog/{suffix}"));
        self.inner
            .lock()
            .recover_poison("ClientProgressRegistry::register")
            .insert(
                token.clone(),
                ClientProgressRoute {
                    aggregator,
                    server: server.to_string(),
                },
            );
        token
    }

    /// Resolve an incoming `$/progress` token to its aggregator and originating
    /// server name. `None` for tokens this registry never minted (e.g. a
    /// server-declared token, handled elsewhere).
    pub(crate) fn route(
        &self,
        token: &NumberOrString,
    ) -> Option<(std::sync::Arc<Mutex<ClientProgressAggregator>>, String)> {
        let guard = self
            .inner
            .lock()
            .recover_poison("ClientProgressRegistry::route");
        guard
            .get(token)
            .map(|r| (r.aggregator.clone(), r.server.clone()))
    }

    /// Drop the routes for a finished request's bridge tokens.
    pub(crate) fn deregister(&self, tokens: &[NumberOrString]) {
        let mut guard = self
            .inner
            .lock()
            .recover_poison("ClientProgressRegistry::deregister");
        for token in tokens {
            guard.remove(token);
        }
    }
}

/// Aggregation shape for the fanned-out request, mirroring the server-level
/// fan-in strategy (language-server-bridge-request-strategies).
#[derive(Debug, Clone)]
pub(crate) enum ClientProgressStrategy {
    /// One winner is delivered; progress is anchored on the highest-priority
    /// *named* candidate (`None` when only `Rest`/wildcard members participate).
    Preferred { anchor: Option<String> },
    /// All contributors are merged; the bridge composes a synthetic aggregate.
    Concatenated,
}

/// Translates a fanned-out request's downstream `$/progress` into a single
/// lifecycle on the client's `workDoneToken`.
#[derive(Debug)]
pub(crate) struct ClientProgressAggregator {
    /// The editor-minted token every emitted `$/progress` is reported against.
    client_token: NumberOrString,
    strategy: ClientProgressStrategy,
    /// Number of downstream servers the request fanned out to.
    server_count: usize,
    /// Whether a `Begin` has been emitted upstream (pairing invariant).
    begun: bool,
    /// Whether the terminal `End` has been emitted (idempotency guard).
    ended: bool,
}

impl ClientProgressAggregator {
    pub(crate) fn new(
        client_token: NumberOrString,
        strategy: ClientProgressStrategy,
        server_count: usize,
    ) -> Self {
        Self {
            client_token,
            strategy,
            server_count,
            begun: false,
            ended: false,
        }
    }

    /// Feed one downstream `$/progress` value (from `server`). Returns the
    /// upstream `$/progress` to forward against the client token, or `None` to
    /// drop it.
    ///
    /// Stage 1 handles only the single-downstream case; with more than one
    /// server the progress is dropped (never forwarded raw) until the
    /// `preferred`/`concatenated` stages land.
    pub(crate) fn on_downstream_progress(
        &mut self,
        _server: &str,
        value: ProgressParamsValue,
    ) -> Option<ProgressParams> {
        if self.ended || self.server_count != 1 {
            // N > 1 is not yet aggregated; never forward raw. Anything after the
            // terminal End is also dropped.
            let _ = &self.strategy;
            return None;
        }

        // N = 1: relay verbatim under the client token, tracking the lifecycle so
        // the pairing invariant holds.
        match &value {
            ProgressParamsValue::WorkDone(WorkDoneProgress::Begin(_)) => self.begun = true,
            ProgressParamsValue::WorkDone(WorkDoneProgress::End(_)) => self.ended = true,
            _ => {}
        }
        Some(ProgressParams {
            token: self.client_token.clone(),
            value,
        })
    }

    /// Whether a `Begin` was emitted upstream but no `End` yet — i.e. a terminal
    /// `End` must be synthesized if the request ends without one (failure /
    /// cancellation / disconnect; ls-bridge-progress-disconnect-cleanup).
    pub(crate) fn needs_terminal_end(&self) -> bool {
        self.begun && !self.ended
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower_lsp_server::ls_types::{
        WorkDoneProgressBegin, WorkDoneProgressEnd, WorkDoneProgressReport,
    };

    fn client_token() -> NumberOrString {
        NumberOrString::String("editor-wd-1".to_string())
    }

    fn begin() -> ProgressParamsValue {
        ProgressParamsValue::WorkDone(WorkDoneProgress::Begin(WorkDoneProgressBegin {
            title: "Indexing".to_string(),
            cancellable: None,
            message: None,
            percentage: None,
        }))
    }
    fn report() -> ProgressParamsValue {
        ProgressParamsValue::WorkDone(WorkDoneProgress::Report(WorkDoneProgressReport {
            cancellable: None,
            message: Some("50%".to_string()),
            percentage: Some(50),
        }))
    }
    fn end() -> ProgressParamsValue {
        ProgressParamsValue::WorkDone(WorkDoneProgress::End(WorkDoneProgressEnd { message: None }))
    }

    #[test]
    fn n1_relays_begin_report_end_under_the_client_token() {
        let mut agg = ClientProgressAggregator::new(
            client_token(),
            ClientProgressStrategy::Preferred { anchor: None },
            1,
        );

        for value in [begin(), report(), end()] {
            let out = agg
                .on_downstream_progress("rust-analyzer", value.clone())
                .expect("N=1 relays the downstream progress");
            assert_eq!(out.token, client_token(), "retargeted to the client token");
            assert!(
                matches!((&out.value, &value), (a, b) if std::mem::discriminant(a) == std::mem::discriminant(b)),
                "value forwarded verbatim"
            );
        }
    }

    #[test]
    fn n1_tracks_begin_then_end_for_the_pairing_invariant() {
        let mut agg = ClientProgressAggregator::new(
            client_token(),
            ClientProgressStrategy::Preferred { anchor: None },
            1,
        );
        assert!(!agg.needs_terminal_end(), "nothing begun yet");
        agg.on_downstream_progress("s", begin());
        assert!(agg.needs_terminal_end(), "begun, not ended");
        agg.on_downstream_progress("s", end());
        assert!(!agg.needs_terminal_end(), "ended normally");
    }

    fn agg() -> std::sync::Arc<Mutex<ClientProgressAggregator>> {
        std::sync::Arc::new(Mutex::new(ClientProgressAggregator::new(
            client_token(),
            ClientProgressStrategy::Preferred { anchor: None },
            1,
        )))
    }

    #[test]
    fn registry_routes_minted_tokens_and_forgets_deregistered_ones() {
        let reg = ClientProgressRegistry::new();
        let a = agg();
        let t1 = reg.register(a.clone(), "rust-analyzer");
        let t2 = reg.register(a.clone(), "pyright");
        assert_ne!(t1, t2, "each (request, server) gets a unique bridge token");

        let (_routed, server) = reg.route(&t1).expect("registered token routes");
        assert_eq!(server, "rust-analyzer");
        assert!(
            reg.route(&NumberOrString::Number(99)).is_none(),
            "unknown token does not route"
        );

        reg.deregister(&[t1.clone(), t2.clone()]);
        assert!(reg.route(&t1).is_none(), "deregistered token is forgotten");
        assert!(reg.route(&t2).is_none());
    }

    #[test]
    fn multi_server_progress_is_dropped_until_aggregation_lands() {
        let mut agg = ClientProgressAggregator::new(
            client_token(),
            ClientProgressStrategy::Preferred {
                anchor: Some("rust-analyzer".to_string()),
            },
            2,
        );
        assert!(
            agg.on_downstream_progress("rust-analyzer", begin())
                .is_none(),
            "N>1 must not forward raw downstream progress (would duplicate Begins)"
        );
        assert!(
            !agg.needs_terminal_end(),
            "nothing emitted, nothing to terminate"
        );
    }
}
