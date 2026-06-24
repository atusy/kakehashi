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
//! *preferred* is implemented with a **single fixed anchor**: the dispatch path
//! mints a bridge token solely for the **tracked source** (the sole server at
//! `N = 1`, or the highest-priority *named* anchor at `N > 1`) and suppresses
//! every other candidate by handing it no token, so only the tracked source's
//! progress routes here. If that anchor returns empty and the request falls
//! through to a lower-priority candidate, the editor sees no *new* progress for
//! it; the open `Begin` is closed by the synthetic terminal `End` at request
//! completion (an ADR-sanctioned branch of the pairing invariant). Deferred to
//! later stages: **dynamic fall-through re-anchoring** (showing the *next* named
//! anchor's real progress/`End`), *concatenated* aggregation, and
//! `partialResultToken`. Until then a *concatenated* request shows no client
//! progress.

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

/// Translates one fanned-out request's downstream `$/progress` into a single
/// coherent lifecycle on the client's `workDoneToken`.
///
/// Several bridge-minted source tokens may route here — one per tracked source,
/// and for a multi-region request (e.g. `documentSymbol` over several injection
/// regions) one per region. The aggregator picks the **first source to relay a
/// `Begin`** as the winner and shows only its `Begin`/`report`/`End`; every other
/// source (and any duplicate `Begin`) is dropped, so the editor sees exactly one
/// `Begin → … → End`. It also guarantees a terminal `End` on teardown (even if the
/// winner's own `End` races the response or never arrives).
#[derive(Debug)]
pub(crate) struct ClientProgressAggregator {
    /// The editor-minted token every emitted `$/progress` is reported against.
    client_token: NumberOrString,
    /// The source token that won the lifecycle — the first to relay a `Begin`.
    /// Only its subsequent non-`Begin` progress is relayed; `None` until then.
    winner: Option<NumberOrString>,
    /// Whether the terminal `End` has been emitted (drop anything after it; emit
    /// the synthetic teardown `End` at most once).
    ended: bool,
}

impl ClientProgressAggregator {
    pub(crate) fn new(client_token: NumberOrString) -> Self {
        Self {
            client_token,
            winner: None,
            ended: false,
        }
    }

    /// Feed one downstream `$/progress` value from `source` (its bridge token).
    /// Returns the upstream `$/progress` to forward against the client token, or
    /// `None` to drop it.
    pub(crate) fn on_downstream_progress(
        &mut self,
        source: &NumberOrString,
        value: ProgressParamsValue,
    ) -> Option<ProgressParams> {
        if self.ended {
            return None;
        }
        let is_begin = matches!(
            &value,
            ProgressParamsValue::WorkDone(WorkDoneProgress::Begin(_))
        );
        match &self.winner {
            // No lifecycle yet: only a `Begin` starts one (drop out-of-order
            // `report`/`End` that arrive before any `Begin`).
            None => {
                if !is_begin {
                    return None;
                }
                self.winner = Some(source.clone());
            }
            // Lifecycle owned by the winner: relay only its non-`Begin` progress;
            // drop a losing source's progress and the winner's duplicate `Begin`.
            Some(winner) => {
                if winner != source || is_begin {
                    return None;
                }
            }
        }
        if matches!(
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
    /// always closed even when the winner's `End` never arrives or races the
    /// response (ls-bridge-client-progress pairing invariant). Marks the lifecycle
    /// ended so it fires at most once and a later real `End` is ignored.
    pub(crate) fn take_terminal_end(&mut self) -> Option<ProgressParams> {
        if self.winner.is_some() && !self.ended {
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

    /// A bridge-minted source token (the single-source tests use one consistent
    /// source; multi-source tests pass distinct ones).
    fn src() -> NumberOrString {
        NumberOrString::String("cprog-src".to_string())
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
        let mut agg = ClientProgressAggregator::new(client_token());

        for value in [begin(), end()] {
            let out = agg
                .on_downstream_progress(&src(), value.clone())
                .expect("N=1 relays the downstream progress");
            assert_eq!(out.token, client_token(), "retargeted to the client token");
            assert_eq!(out.value, value, "value forwarded verbatim");
        }
        // After the terminal End, further progress is dropped.
        assert!(
            agg.on_downstream_progress(&src(), begin()).is_none(),
            "no progress is relayed after the End"
        );
    }

    fn agg() -> std::sync::Arc<Mutex<ClientProgressAggregator>> {
        std::sync::Arc::new(Mutex::new(ClientProgressAggregator::new(client_token())))
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
    fn out_of_order_end_before_begin_is_dropped() {
        let mut agg = ClientProgressAggregator::new(client_token());
        // An `End` before any `Begin` must be dropped — the editor must never see
        // a terminal for a lifecycle that never began.
        assert!(
            agg.on_downstream_progress(&src(), end()).is_none(),
            "End before Begin is dropped"
        );
        // A subsequent normal Begin → End still relays.
        assert!(
            agg.on_downstream_progress(&src(), begin()).is_some(),
            "Begin relays"
        );
        assert!(
            agg.on_downstream_progress(&src(), end()).is_some(),
            "then End relays"
        );
    }

    #[test]
    fn duplicate_begin_is_dropped() {
        let mut agg = ClientProgressAggregator::new(client_token());
        assert!(
            agg.on_downstream_progress(&src(), begin()).is_some(),
            "first Begin relays"
        );
        // A second Begin must be dropped — the editor must see exactly one Begin.
        assert!(
            agg.on_downstream_progress(&src(), begin()).is_none(),
            "duplicate Begin is dropped"
        );
        assert!(
            agg.on_downstream_progress(&src(), end()).is_some(),
            "End still relays"
        );
    }

    #[test]
    fn first_source_to_begin_wins_others_are_dropped() {
        // A request fanned out over several sources (e.g. multiple injection
        // regions) routes each source's progress to one shared aggregator. The
        // first to `Begin` wins; every other source's progress is dropped, so the
        // editor sees a single coherent lifecycle.
        let mut agg = ClientProgressAggregator::new(client_token());
        let a = NumberOrString::String("src-a".to_string());
        let b = NumberOrString::String("src-b".to_string());

        assert!(
            agg.on_downstream_progress(&a, begin()).is_some(),
            "source A begins first → wins"
        );
        assert!(
            agg.on_downstream_progress(&b, begin()).is_none(),
            "loser B's Begin is dropped"
        );
        assert!(
            agg.on_downstream_progress(&b, end()).is_none(),
            "loser B's End is dropped"
        );
        assert!(
            agg.on_downstream_progress(&a, end()).is_some(),
            "winner A's End relays"
        );
        assert!(
            agg.on_downstream_progress(&a, begin()).is_none(),
            "nothing relays after the winner's End"
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
            std::sync::Arc::new(Mutex::new(ClientProgressAggregator::new(client_token())));
        let token = registry.register(aggregator.clone());
        // Downstream begins (relayed) but never ends.
        aggregator
            .lock()
            .unwrap()
            .on_downstream_progress(&src(), begin());

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
            std::sync::Arc::new(Mutex::new(ClientProgressAggregator::new(client_token())));
        let token = registry.register(aggregator.clone());
        {
            let mut guard = aggregator.lock().unwrap();
            guard.on_downstream_progress(&src(), begin());
            guard.on_downstream_progress(&src(), end());
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
            std::sync::Arc::new(Mutex::new(ClientProgressAggregator::new(client_token())));
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
