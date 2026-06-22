//! The single proactive `textDocument/publishDiagnostics` publisher
//! (push-propagation-diagnostic-forwarding).
//!
//! Every proactive diagnostic feed writes slots into the [`DiagnosticAggregator`]
//! and then asks this publisher to **republish** the host: it snapshots the
//! cache, merges every slot, and emits one `publishDiagnostics`. Routing every
//! feed through one publisher is what keeps sibling regions intact against the
//! client's URI-level clobber.
//!
//! Staged: this commit handles the host-event pull blob
//! ([`DiagnosticSource::PullLayer`](crate::lsp::diagnostic_cache::DiagnosticSource)).
//! A follow-up adds downstream region pushes, which need the host document's live
//! injection offsets to transform virtual coordinates — the publisher gains
//! `documents`/`language`/`bridge` then.

use std::sync::Arc;

use url::Url;

use crate::lsp::diagnostic_cache::{DiagnosticAggregator, merge_cached_diagnostics};
use crate::lsp::lsp_impl::Kakehashi;
use tower_lsp_server::Client;

/// Logging target for proactive push diagnostics.
const LOG_TARGET: &str = "kakehashi::push_diag";

/// Bundles the state needed to merge the cache and publish for a host, so the
/// notification feeds can trigger a republish without each holding `Kakehashi`.
pub(crate) struct DiagnosticPublisher {
    client: Client,
    aggregator: Arc<DiagnosticAggregator>,
}

impl DiagnosticPublisher {
    pub(crate) fn new(server: &Kakehashi) -> Self {
        Self {
            client: server.client.clone(),
            aggregator: Arc::clone(&server.diagnostics),
        }
    }

    /// Feed the host-event pull's combined result into the cache and republish.
    ///
    /// The pull blob is already host-local; it replaces the
    /// [`DiagnosticSource::PullLayer`](crate::lsp::diagnostic_cache::DiagnosticSource)
    /// slot, then the merge folds in every other slot for the host.
    pub(crate) async fn publish_pull_layer(
        &self,
        host: &Url,
        diagnostics: Vec<tower_lsp_server::ls_types::Diagnostic>,
    ) {
        self.aggregator.set_pull_layer(host, diagnostics);
        self.republish(host).await;
    }

    /// Drop the host's cache entry and publish the now-empty set (host `didClose`).
    pub(crate) async fn clear_host(&self, host: &Url) {
        self.aggregator.evict_host(host);
        self.republish(host).await;
    }

    /// Merge the host's cached slots and publish the cumulative result. An empty
    /// merge clears the editor's diagnostics for the host.
    pub(crate) async fn republish(&self, host: &Url) {
        let snapshot = self.aggregator.snapshot(host);
        let diagnostics = merge_cached_diagnostics(&snapshot);

        let lsp_uri = match crate::lsp::lsp_impl::url_to_uri(host) {
            Ok(uri) => uri,
            Err(e) => {
                log::warn!(target: LOG_TARGET, "skip publish, bad host URI {host}: {e}");
                return;
            }
        };

        log::debug!(
            target: LOG_TARGET,
            "publishing {} merged diagnostics for {}",
            diagnostics.len(),
            host
        );
        self.client
            .publish_diagnostics(lsp_uri, diagnostics, None)
            .await;
    }
}
