//! `workspace/executeCommand` method for Kakehashi (#568 PR 6).
//!
//! A `Command` the bridge surfaced in a code action is executed here: the
//! CONNECTION it must run on is encoded in the command NAME, so the pool decodes
//! it and routes the request back to that exact `(server, root)` (see
//! [`dispatch_execute_command`](crate::lsp::bridge::pool::LanguageServerPool)).
//! The server's result is relayed verbatim; a command the bridge didn't mint,
//! or any downstream failure, yields a null result (fail soft).
//!
//! This handler does NOT repair document state before dispatching. It used to,
//! because a respawn purges the origin's virtual documents and — unlike the
//! request paths — this path has no `ensure_document_opened` step. That repair
//! now runs when the replacement connection reaches `Ready`, off the
//! user-facing path and covering every request path
//! (execute-command-routing-token); needing a host document for it was the only
//! reason the routing token ever named one.

use serde_json::Value;
use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::ExecuteCommandParams;

use super::super::Kakehashi;

impl Kakehashi {
    pub(crate) async fn execute_command_impl(
        &self,
        params: ExecuteCommandParams,
    ) -> Result<Option<Value>> {
        let settings = self.settings_manager.load_settings();
        let upstream_id = crate::lsp::current_upstream_id();

        // Subscribe for the client's $/cancelRequest BEFORE the first await:
        // the forwarder does not buffer cancels that arrive pre-subscribe, so
        // subscribing later would silently drop one fired in the meantime.
        // Subscribed here, such a cancel is latched in the receiver and the
        // select below sees it immediately.
        let (cancel_rx, _cancel_guard) = self.subscribe_cancel(upstream_id.as_ref());
        let sweep_id = upstream_id.clone();

        // Propagate a client $/cancelRequest as RequestCancelled instead of
        // masking it as a null success: the cancel IS forwarded downstream via
        // the registry, the downstream answers -32800, and fail-soft parsing
        // would otherwise collapse that to `Ok(None)` (the same masking the
        // multi-region codeAction walk already fixed).
        let pool = self.bridge.pool_arc();
        let dispatch = pool.dispatch_execute_command(params, &settings, upstream_id);
        // The cancel arm DROPS the in-flight dispatch, which then never
        // reaches its own refcounted unregister — an RAII sweep (dropped at
        // function exit) covers that, and unlike a trailing statement it also
        // runs when this whole handler future is dropped (client disconnect /
        // shutdown). Idempotent after normal completion, where the dispatch
        // cleaned up itself. The CAPTURED id, not a re-read of the task-local:
        // the sweep must target exactly the id the dispatch registered under.
        let _sweep = crate::lsp::lsp_impl::bridge_context::UpstreamRegistrySweepGuard::new(
            std::sync::Arc::clone(&pool),
            sweep_id,
        );
        match cancel_rx {
            Some(rx) => tokio::select! {
                biased;
                _ = rx => Err(tower_lsp_server::jsonrpc::Error::request_cancelled()),
                outcome = dispatch => Ok(outcome),
            },
            None => Ok(dispatch.await),
        }
    }
}
