//! In-process driving of the LSP handlers for `kakehashi format` (CLI mode).
//!
//! The CLI runs the same `Kakehashi` instance the editor would talk to, but
//! calls the handler implementations directly instead of going through
//! JSON-RPC framing. These wrappers package the per-file lifecycle —
//! `didOpen` → wait for downstream servers → `textDocument/formatting` →
//! `didClose` — so `crate::cli::format` never touches server internals.
//!
//! Lives under `lsp_impl` (not `crate::cli`) because the ready-wait needs the
//! private `language` / `documents` / `bridge` fields.

use std::collections::HashSet;
use std::path::Path;
use std::time::Duration;

use tower_lsp_server::ls_types::{
    DidCloseTextDocumentParams, DidOpenTextDocumentParams, DocumentFormattingParams,
    FormattingOptions, InitializeParams, InitializedParams, TextDocumentIdentifier,
    TextDocumentItem, WorkspaceFolder,
};
use url::Url;

use crate::language::InjectionResolver;

use super::{Kakehashi, url_to_uri};

impl Kakehashi {
    /// Run the LSP initialize/initialized lifecycle for CLI mode, loading
    /// configuration from `root` (typically the current directory) exactly
    /// like an editor session rooted there would.
    pub(crate) async fn cli_initialize(&self, root: &Path) {
        let workspace_folders = Url::from_file_path(root)
            .ok()
            .and_then(|url| url_to_uri(&url).ok())
            .map(|uri| {
                vec![WorkspaceFolder {
                    uri,
                    name: root
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "workspace".to_string()),
                }]
            });
        let params = InitializeParams {
            workspace_folders,
            ..Default::default()
        };
        if let Err(e) = self.initialize_impl(params).await {
            log::warn!(target: "kakehashi::cli", "initialize failed: {e}");
        }
        self.initialized_impl(InitializedParams {}).await;
    }

    /// Whether the path alone identifies a formattable language (loading the
    /// parser if installed but not yet loaded). Used to filter directory
    /// walks; explicit files skip this (content-based detection still
    /// applies when they are opened).
    pub(crate) fn cli_can_format_path(&self, path: &Path) -> bool {
        path.to_str()
            .is_some_and(|p| self.language.loadable_language_for_path(p).is_some())
    }

    /// Format `text` as if it were the document at absolute `path`, returning
    /// the formatted text — or `None` when no formatting applies (unknown
    /// language, no injection regions, no configured servers, or the
    /// formatters returned no edits).
    ///
    /// Downstream servers are spawned cold on the first file; the explicit
    /// ready-wait (bounded by `ready_timeout` per server) keeps a cold start
    /// from being misread as "nothing to format" — the race editor clients
    /// paper over by retrying.
    pub(crate) async fn cli_format_text(
        &self,
        path: &Path,
        text: &str,
        formatting_options: FormattingOptions,
        ready_timeout: Duration,
    ) -> Option<String> {
        let url = Url::from_file_path(path).ok()?;
        let lsp_uri = url_to_uri(&url).ok()?;

        // Path-based detection first: it loads an installed-but-unloaded
        // parser, which the content-based chain (used second, for shebangs
        // and mode lines) requires to already be loaded.
        let language_id = self
            .language
            .loadable_language_for_path(url.path())
            .or_else(|| self.language.detect_language(url.path(), text, None, None))
            .unwrap_or_default();

        self.did_open_impl(DidOpenTextDocumentParams {
            text_document: TextDocumentItem {
                uri: lsp_uri.clone(),
                language_id,
                version: 1,
                text: text.to_string(),
            },
        })
        .await;

        self.wait_bridge_servers_ready(&url, ready_timeout).await;
        self.wait_eager_open_finished(&url, ready_timeout).await;

        let result = self
            .formatting_impl(DocumentFormattingParams {
                text_document: TextDocumentIdentifier {
                    uri: lsp_uri.clone(),
                },
                options: formatting_options,
                work_done_progress_params: Default::default(),
            })
            .await;

        self.did_close_impl(DidCloseTextDocumentParams {
            text_document: TextDocumentIdentifier { uri: lsp_uri },
        })
        .await;

        let edits = result.ok().flatten()?;
        if edits.is_empty() {
            return None;
        }
        Some(crate::text::edit::apply_text_edits(text, &edits))
    }

    /// Gracefully shut down downstream language servers (LSP shutdown/exit
    /// handshake, escalating for unresponsive ones).
    pub(crate) async fn cli_shutdown(&self) {
        let _ = self.shutdown_impl().await;
    }

    /// Wait (up to `timeout`) until the eager-open tasks `didOpen` spawned
    /// for this document have finished.
    ///
    /// An eager-open task claims each region's virtual document BEFORE its
    /// `didOpen` reaches the writer queue, so a formatting request issued in
    /// that window skips its own `didOpen` (already claimed) and overtakes
    /// the eager one on the wire — the downstream server then sees an
    /// unknown URI and answers `null`, which the preferred fan-in accepts as
    /// authoritative "no edits". Editors retry past this cold-start race; a
    /// one-shot CLI run must instead wait the tasks out: once finished,
    /// every `didOpen` is enqueued and the single-writer FIFO keeps the
    /// formatting request behind them.
    async fn wait_eager_open_finished(&self, uri: &Url, timeout: Duration) {
        let deadline = tokio::time::Instant::now() + timeout;
        while !self.bridge.eager_open_tasks_finished(uri) {
            if tokio::time::Instant::now() >= deadline {
                log::warn!(
                    target: "kakehashi::cli",
                    "eager-open tasks for {uri} did not finish within {timeout:?}; \
                     formatting may race their didOpen"
                );
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// Wait (up to `timeout` per server) until every downstream server
    /// configured for the document's injection regions is Ready.
    ///
    /// `formatting_impl` treats a not-yet-ready server as a failed step and
    /// skips it — correct for an editor where the next request retries, but a
    /// one-shot CLI run would silently produce no edits. Spawning is
    /// idempotent (`get_or_create_connection_wait_ready`), so this overlaps
    /// with the eager-open `didOpen` triggered.
    async fn wait_bridge_servers_ready(&self, uri: &Url, timeout: Duration) {
        let Some(language_name) = self.document_language(uri) else {
            return;
        };
        let Some(injection_query) = self.language.injection_query(&language_name) else {
            return;
        };
        let Some(snapshot) = self.documents.get(uri).and_then(|doc| doc.snapshot()) else {
            return;
        };

        let regions = InjectionResolver::resolve_all(
            &self.language,
            self.bridge.node_tracker(),
            uri,
            snapshot.tree(),
            snapshot.text(),
            injection_query.as_ref(),
        );

        let pool = self.bridge.pool_arc();
        let mut waited: HashSet<String> = HashSet::new();
        for resolved in regions {
            for config in self
                .bridge_configs_for_injection_language(&language_name, &resolved.injection_language)
            {
                if waited.insert(config.server_name.clone())
                    && let Err(e) = pool
                        .get_or_create_connection_wait_ready(
                            &config.server_name,
                            &config.config,
                            timeout,
                        )
                        .await
                {
                    log::warn!(
                        target: "kakehashi::cli",
                        "downstream server {} not ready: {e}",
                        config.server_name
                    );
                }
            }
        }
    }
}
