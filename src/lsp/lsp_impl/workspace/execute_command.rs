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
        self.sync_origin_documents_before_execute(&params, &settings)
            .await;

        let pool = self.bridge.pool_arc();
        Ok(pool
            .dispatch_execute_command(params, &settings, upstream_id)
            .await)
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
        // quickly, skip it and dispatch anyway — the happy path (doc already
        // open) returns immediately, and dispatch uses the fail-fast connection
        // variant, so a slow downstream can't block command execution here.
        const SYNC_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
        if tokio::time::timeout(
            SYNC_TIMEOUT,
            self.bridge.ensure_server_documents_open(
                settings,
                &host_language,
                &host_url,
                injections,
                route.origin,
            ),
        )
        .await
        .is_err()
        {
            // Timed out waiting for the downstream to open its documents; dispatch
            // anyway (fail-fast connection variant). Logged so an intermittent
            // "downstream never saw didOpen before the command" is diagnosable.
            log::debug!(
                target: "kakehashi::bridge",
                "executeCommand: pre-sync of origin '{}' documents timed out after {SYNC_TIMEOUT:?}; dispatching anyway",
                route.origin
            );
        }
    }
}
