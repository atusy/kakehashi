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
    inner: Mutex<HashMap<NumberOrString, std::sync::Arc<Mutex<ClientProgressAggregator>>>>,
    /// Monotonic source of unique per-server bridge tokens.
    counter: AtomicU64,
}

impl ClientProgressRegistry {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Mint a unique bridge token for one downstream of `aggregator`'s request,
    /// register the route, and return the token to hand to that downstream as its
    /// `workDoneToken`.
    pub(crate) fn register(
        &self,
        aggregator: std::sync::Arc<Mutex<ClientProgressAggregator>>,
    ) -> NumberOrString {
        let suffix = self.counter.fetch_add(1, Ordering::Relaxed);
        let token = NumberOrString::String(format!("kakehashi/bridge/cprog/{suffix}"));
        self.inner
            .lock()
            .recover_poison("ClientProgressRegistry::register")
            .insert(token.clone(), aggregator);
        token
    }

    /// Resolve an incoming `$/progress` token to its aggregator. `None` for tokens
    /// this registry never minted (e.g. a server-declared token, handled
    /// elsewhere).
    pub(crate) fn route(
        &self,
        token: &NumberOrString,
    ) -> Option<std::sync::Arc<Mutex<ClientProgressAggregator>>> {
        self.inner
            .lock()
            .recover_poison("ClientProgressRegistry::route")
            .get(token)
            .cloned()
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

/// Tears down a request's client-progress routing when dropped — i.e. when the
/// dispatch call returns. It first deregisters the bridge tokens (so a late
/// downstream `$/progress` no longer routes), then **synthesizes the terminal
/// `End`** if the aggregator relayed a `Begin` but never an `End` — guaranteeing
/// the editor's indicator is closed even when the downstream's own `End` raced
/// the response or never came (ls-bridge-client-progress pairing invariant).
pub(crate) struct ClientProgressDeregisterGuard {
    registry: std::sync::Arc<ClientProgressRegistry>,
    tokens: Vec<NumberOrString>,
    aggregator: std::sync::Arc<Mutex<ClientProgressAggregator>>,
    upstream_tx:
        tokio::sync::mpsc::UnboundedSender<crate::lsp::bridge::actor::UpstreamNotification>,
}

impl ClientProgressDeregisterGuard {
    pub(crate) fn new(
        registry: std::sync::Arc<ClientProgressRegistry>,
        tokens: Vec<NumberOrString>,
        aggregator: std::sync::Arc<Mutex<ClientProgressAggregator>>,
        upstream_tx: tokio::sync::mpsc::UnboundedSender<
            crate::lsp::bridge::actor::UpstreamNotification,
        >,
    ) -> Self {
        Self {
            registry,
            tokens,
            aggregator,
            upstream_tx,
        }
    }
}

impl Drop for ClientProgressDeregisterGuard {
    fn drop(&mut self) {
        // Deregister first so a late *real* `End` cannot flip `ended` after the
        // decision below (no double-`End`). The `Begin`-before-synthetic-`End`
        // ordering rests on the reader loop processing the downstream's `Begin`
        // notification before its response — and the response is what completes
        // fan-in and drops this guard (ls-bridge-message-ordering).
        self.registry.deregister(&self.tokens);
        let terminal = self
            .aggregator
            .lock()
            .recover_poison("ClientProgressAggregator teardown")
            .take_terminal_end();
        if let Some(params) = terminal {
            let _ = self
                .upstream_tx
                .send(crate::lsp::bridge::actor::UpstreamNotification::ClientProgress { params });
        }
    }
}

/// Translates a fanned-out request's downstream `$/progress` into a single
/// lifecycle on the client's `workDoneToken`.
///
/// Stage 1 implements the single-downstream (`N = 1`) relay, including a
/// synthetic terminal `End` on teardown (so a relayed `Begin` is always closed
/// even if the downstream's `End` races the response or never arrives).
/// Multi-server aggregation (`preferred`/`concatenated`), which reintroduces the
/// per-strategy anchor, arrives in later stages.
#[derive(Debug)]
pub(crate) struct ClientProgressAggregator {
    /// The editor-minted token every emitted `$/progress` is reported against.
    client_token: NumberOrString,
    /// Number of downstream servers the request fanned out to.
    server_count: usize,
    /// Whether a `Begin` has been relayed upstream (pairing invariant).
    begun: bool,
    /// Whether the terminal `End` has been emitted (drop anything after it; emit
    /// the synthetic teardown `End` at most once).
    ended: bool,
}

impl ClientProgressAggregator {
    pub(crate) fn new(client_token: NumberOrString, server_count: usize) -> Self {
        Self {
            client_token,
            server_count,
            begun: false,
            ended: false,
        }
    }

    /// Feed one downstream `$/progress` value. Returns the upstream `$/progress`
    /// to forward against the client token, or `None` to drop it.
    ///
    /// Stage 1 handles only the single-downstream case; with more than one
    /// server the progress is dropped (never forwarded raw) until the
    /// `preferred`/`concatenated` stages land.
    pub(crate) fn on_downstream_progress(
        &mut self,
        value: ProgressParamsValue,
    ) -> Option<ProgressParams> {
        let is_begin = matches!(
            &value,
            ProgressParamsValue::WorkDone(WorkDoneProgress::Begin(_))
        );
        if self.ended || self.server_count != 1 || (!self.begun && !is_begin) {
            // Drop: N > 1 is not yet aggregated (never forward raw); anything after
            // the terminal End; and any out-of-order `report`/`End` before a
            // `Begin` — the editor must see a coherent Begin → … → End lifecycle.
            return None;
        }
        // N = 1: relay verbatim under the client token, tracking the lifecycle so
        // a terminal End is guaranteed even if the downstream's own End never
        // arrives (or arrives after teardown).
        if is_begin {
            self.begun = true;
        } else if matches!(
            &value,
            ProgressParamsValue::WorkDone(WorkDoneProgress::End(_))
        ) {
            self.ended = true;
        }
        Some(ProgressParams {
            token: self.client_token.clone(),
            value,
        })
    }

    /// On request teardown, return a synthetic terminal `End` for the client token
    /// **iff** a `Begin` was relayed but no `End` — so the editor's indicator is
    /// always closed even when the downstream's `End` never arrives or races the
    /// response (ls-bridge-client-progress pairing invariant). Marks the lifecycle
    /// ended so it fires at most once and a later real `End` is ignored.
    pub(crate) fn take_terminal_end(&mut self) -> Option<ProgressParams> {
        if self.begun && !self.ended {
            self.ended = true;
            Some(ProgressParams {
                token: self.client_token.clone(),
                value: ProgressParamsValue::WorkDone(WorkDoneProgress::End(
                    tower_lsp_server::ls_types::WorkDoneProgressEnd { message: None },
                )),
            })
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower_lsp_server::ls_types::{WorkDoneProgressBegin, WorkDoneProgressEnd};

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
    fn end() -> ProgressParamsValue {
        ProgressParamsValue::WorkDone(WorkDoneProgress::End(WorkDoneProgressEnd { message: None }))
    }

    #[test]
    fn n1_relays_begin_then_end_under_the_client_token_then_drops() {
        let mut agg = ClientProgressAggregator::new(client_token(), 1);

        for value in [begin(), end()] {
            let out = agg
                .on_downstream_progress(value.clone())
                .expect("N=1 relays the downstream progress");
            assert_eq!(out.token, client_token(), "retargeted to the client token");
            assert_eq!(out.value, value, "value forwarded verbatim");
        }
        // After the terminal End, further progress is dropped.
        assert!(
            agg.on_downstream_progress(begin()).is_none(),
            "no progress is relayed after the End"
        );
    }

    fn agg() -> std::sync::Arc<Mutex<ClientProgressAggregator>> {
        std::sync::Arc::new(Mutex::new(ClientProgressAggregator::new(client_token(), 1)))
    }

    #[test]
    fn registry_routes_minted_tokens_and_forgets_deregistered_ones() {
        let reg = ClientProgressRegistry::new();
        let a = agg();
        let t1 = reg.register(a.clone());
        let t2 = reg.register(a.clone());
        assert_ne!(t1, t2, "each (request, server) gets a unique bridge token");

        let routed = reg.route(&t1).expect("registered token routes");
        assert!(
            std::sync::Arc::ptr_eq(&routed, &a),
            "routes to the exact aggregator registered"
        );
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
        let mut agg = ClientProgressAggregator::new(client_token(), 2);
        assert!(
            agg.on_downstream_progress(begin()).is_none(),
            "N>1 must not forward raw downstream progress (would duplicate Begins)"
        );
    }

    #[test]
    fn out_of_order_end_before_begin_is_dropped() {
        let mut agg = ClientProgressAggregator::new(client_token(), 1);
        // An `End` before any `Begin` must be dropped — the editor must never see
        // a terminal for a lifecycle that never began.
        assert!(
            agg.on_downstream_progress(end()).is_none(),
            "End before Begin is dropped"
        );
        // A subsequent normal Begin → End still relays.
        assert!(
            agg.on_downstream_progress(begin()).is_some(),
            "Begin relays"
        );
        assert!(
            agg.on_downstream_progress(end()).is_some(),
            "then End relays"
        );
    }

    use crate::lsp::bridge::actor::UpstreamNotification;

    /// Teardown after a relayed `Begin` with no `End` (the downstream's `End`
    /// raced the response or never came) must synthesize a terminal `End` so the
    /// editor's indicator does not hang.
    #[test]
    fn teardown_synthesizes_end_when_begun_but_not_ended() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let registry = std::sync::Arc::new(ClientProgressRegistry::new());
        let aggregator =
            std::sync::Arc::new(Mutex::new(ClientProgressAggregator::new(client_token(), 1)));
        let token = registry.register(aggregator.clone());
        // Downstream begins (relayed) but never ends.
        aggregator.lock().unwrap().on_downstream_progress(begin());

        drop(ClientProgressDeregisterGuard::new(
            registry.clone(),
            vec![token.clone()],
            aggregator,
            tx,
        ));

        let msg = rx.try_recv().expect("teardown synthesizes a terminal End");
        match msg {
            UpstreamNotification::ClientProgress { params } => {
                assert_eq!(params.token, client_token(), "End on the client token");
                assert!(matches!(
                    params.value,
                    ProgressParamsValue::WorkDone(WorkDoneProgress::End(_))
                ));
            }
            _ => panic!("expected a ClientProgress End"),
        }
        assert!(rx.try_recv().is_err(), "exactly one terminal End");
        assert!(
            registry.route(&token).is_none(),
            "tokens are deregistered on teardown"
        );
    }

    /// Teardown after a normal `Begin`→`End` must synthesize nothing (the editor
    /// already saw the real `End`); no double-`End`.
    #[test]
    fn teardown_emits_nothing_when_already_ended() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let registry = std::sync::Arc::new(ClientProgressRegistry::new());
        let aggregator =
            std::sync::Arc::new(Mutex::new(ClientProgressAggregator::new(client_token(), 1)));
        let token = registry.register(aggregator.clone());
        {
            let mut guard = aggregator.lock().unwrap();
            guard.on_downstream_progress(begin());
            guard.on_downstream_progress(end());
        }

        drop(ClientProgressDeregisterGuard::new(
            registry,
            vec![token],
            aggregator,
            tx,
        ));

        assert!(
            rx.try_recv().is_err(),
            "an already-ended lifecycle synthesizes no second End"
        );
    }

    /// Teardown after no progress at all (the downstream never began) synthesizes
    /// nothing — no spurious `End` for a token the editor never saw begin.
    #[test]
    fn teardown_emits_nothing_when_never_begun() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let registry = std::sync::Arc::new(ClientProgressRegistry::new());
        let aggregator =
            std::sync::Arc::new(Mutex::new(ClientProgressAggregator::new(client_token(), 1)));
        let token = registry.register(aggregator.clone());

        drop(ClientProgressDeregisterGuard::new(
            registry,
            vec![token],
            aggregator,
            tx,
        ));

        assert!(
            rx.try_recv().is_err(),
            "no Begin was relayed, so no terminal End is synthesized"
        );
    }
}
