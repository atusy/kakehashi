//! `workspace/executeCommand` method for Kakehashi (#568 PR 6).
//!
//! A `Command` the bridge surfaced in a code action is executed here: the
//! origin server + host document are encoded in the command NAME, so the pool
//! decodes them and routes the request back to that server (see
//! [`dispatch_execute_command`](crate::lsp::bridge::pool::LanguageServerPool)).
//! The server's result is relayed verbatim; a command the bridge didn't mint,
//! or any downstream failure, yields a null result (fail soft).

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

        // Self-heal document state before routing. A bridged command assumes the
        // origin server has the (virtual) document open, but a downstream respawn
        // between the codeAction that surfaced the command and this executeCommand
        // purges that connection's doc tracker — and, unlike the request path,
        // executeCommand has no `ensure_document_opened` step. Re-open the origin's
        // injected docs first (awaited, so that WHEN a didOpen is queued it
        // precedes the command on the shared connection; the open is best-effort
        // and may queue nothing). Fail-soft: any gap (foreign command, unparseable
        // host URI, undetectable host language, no matching injection) just skips
        // the sync and dispatches as before.
        // Subscribe for the client's $/cancelRequest BEFORE the first await:
        // the forwarder does not buffer cancels that arrive pre-subscribe, so
        // subscribing after the (up to SYNC_TIMEOUT) pre-sync would silently
        // drop a cancel fired while it runs. Subscribed here, such a cancel is
        // latched in the receiver and the select below sees it immediately.
        let (cancel_rx, _cancel_guard) = self.subscribe_cancel(upstream_id.as_ref());
        let sweep_id = upstream_id.clone();

        self.sync_origin_documents_before_execute(&params, &settings)
            .await;

        // Propagate a client $/cancelRequest as RequestCancelled instead of
        // masking it as a null success: the cancel IS forwarded downstream via
        // the registry, the downstream answers -32800, and fail-soft parsing
        // would otherwise collapse that to `Ok(None)` (the same masking the
        // multi-region codeAction walk already fixed).
        let pool = self.bridge.pool_arc();
        let dispatch = pool.dispatch_execute_command(params, &settings, upstream_id);
        let result = match cancel_rx {
            Some(rx) => tokio::select! {
                biased;
                _ = rx => Err(tower_lsp_server::jsonrpc::Error::request_cancelled()),
                outcome = dispatch => Ok(outcome),
            },
            None => Ok(dispatch.await),
        };
        // The cancel arm DROPS the in-flight dispatch, which then never
        // reaches its own refcounted unregister — sweep the id (idempotent
        // after normal completion, where the dispatch cleaned up itself).
        // The CAPTURED id, not a re-read of the task-local: the sweep must
        // target exactly the id the dispatch registered under.
        pool.unregister_all_for_upstream_id(sweep_id.as_ref());
        result
    }

    /// Best-effort re-open of the routed command's origin-server documents (see
    /// [`Self::execute_command_impl`]). Scoped to virt-layer injected documents;
    /// a host-layer command resolves to no injection and is left to dispatch.
    async fn sync_origin_documents_before_execute(
        &self,
        params: &ExecuteCommandParams,
        settings: &std::sync::Arc<crate::config::WorkspaceSettings>,
    ) {
        let Some(route) = crate::lsp::bridge::decode_command(&params.command) else {
            return;
        };
        let Ok(host_url) = url::Url::parse(route.host_uri) else {
            return;
        };
        // didChange clears the tree and reparses off-ingress: an executeCommand
        // landing right after an edit would otherwise find no injections and
        // silently skip the heal (the request paths await the fresh tree the
        // same way).
        self.ensure_document_parsed(&host_url).await;
        let Some((host_language, injections)) =
            self.injection_coordinator().bridge_injections(&host_url)
        else {
            return;
        };
        if injections.is_empty() {
            return;
        }
        // Bound the best-effort pre-sync: `eager_open_virtual_documents` can wait
        // up to the init timeout for a cold/stuck downstream to reach Ready, and
        // this sits on the user-facing executeCommand path. If it doesn't finish
        // quickly, stop waiting and dispatch anyway — the happy path (doc
        // already open) returns immediately. (Dispatch itself waits through
        // initialization with its own INIT_TIMEOUT bound.)
        const SYNC_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
        // Run the open as a SPAWNED task and bound only the wait: a plain
        // `timeout(fut)` DROPS the future on expiry, throwing away the
        // best-effort didOpen mid-flight (the open path itself is
        // cancellation-safe — `OpenClaimGuard` rolls back an orphaned claim on
        // drop). Spawning lets the open run to completion in the background
        // while the command dispatches, so a slow-but-healthy downstream still
        // ends up with the document open.
        let bridge = std::sync::Arc::clone(&self.bridge);
        let settings = std::sync::Arc::clone(settings);
        let host_language_owned = host_language.clone();
        let host_url_owned = host_url.clone();
        let origin = route.origin.to_string();
        let open_task = tokio::spawn(async move {
            bridge
                .ensure_server_documents_open(
                    &settings,
                    &host_language_owned,
                    &host_url_owned,
                    injections,
                    &origin,
                )
                .await;
        });
        // Bound only the WAIT: on timeout the task keeps running detached (see
        // above), but a watcher still awaits the handle so a background panic
        // surfaces instead of vanishing with the dropped JoinHandle.
        let mut open_task = open_task;
        match tokio::time::timeout(SYNC_TIMEOUT, &mut open_task).await {
            Ok(Ok(())) => return,
            // The spawned open PANICKED (or was aborted): surface it — a
            // swallowed JoinError would hide a real bug behind "the heal
            // just didn't help".
            Ok(Err(join_error)) => {
                log::warn!(
                    target: "kakehashi::bridge",
                    "executeCommand: pre-sync of origin '{}' documents failed: {join_error}",
                    route.origin
                );
                return;
            }
            Err(_) => {}
        }
        {
            // Timed out waiting for the downstream to open its documents;
            // dispatch anyway. Logged so an intermittent "downstream never saw
            // didOpen before the command" is diagnosable.
            log::debug!(
                target: "kakehashi::bridge",
                "executeCommand: pre-sync of origin '{}' documents timed out after {SYNC_TIMEOUT:?}; dispatching anyway",
                route.origin
            );
            let origin = route.origin.to_string();
            tokio::spawn(async move {
                if let Err(join_error) = open_task.await {
                    log::warn!(
                        target: "kakehashi::bridge",
                        "executeCommand: background pre-sync of origin {origin:?} documents failed: {join_error}"
                    );
                }
            });
        }
    }
}
