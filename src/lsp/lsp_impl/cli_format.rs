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

/// Result of one CLI formatting attempt ([`Kakehashi::cli_format_text`]).
pub(crate) struct CliFormatOutcome {
    /// The full formatted text when formatting produced edits; `None` when
    /// nothing applied (unknown language, no regions, no servers, no edits).
    pub(crate) formatted: Option<String>,
    /// Human-readable descriptions of configured downstream servers that
    /// failed to spawn or initialize within the ready timeout. Non-empty
    /// means the document's formatting is incomplete or unverifiable — the
    /// CLI maps this to its error exit code instead of reporting the file
    /// as "unchanged".
    pub(crate) server_failures: Vec<String>,
}

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

    /// Format `text` as if it were the document at absolute `path`.
    ///
    /// `formatted` is `None` when no formatting applies (unknown language,
    /// no injection regions, no configured servers, or the formatters
    /// returned no edits); `server_failures` lists configured downstream
    /// servers that failed to come up, so the caller can distinguish
    /// "nothing to do" from "the formatter is broken".
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
    ) -> CliFormatOutcome {
        // A path we cannot express as a URI cannot be opened as an LSP
        // document at all — report it as a failure (the CLI maps it to its
        // error exit code) instead of silently skipping the file.
        let uri_failure = |what: &str| CliFormatOutcome {
            formatted: None,
            server_failures: vec![format!(
                "cannot convert path '{}' to {what}",
                path.display()
            )],
        };
        let Ok(url) = Url::from_file_path(path) else {
            return uri_failure("a file URL");
        };
        let Ok(lsp_uri) = url_to_uri(&url) else {
            return uri_failure("an LSP URI");
        };

        // Path-based detection first, then first-line content detection —
        // both via the loading chain, because `detect_language`'s own stages
        // only accept parsers that are already loaded, and nothing is loaded
        // before the first didOpen in CLI mode. Detect on the raw path, not
        // `url.path()`: URL paths percent-encode special characters, which
        // would break extension/filename matching for such files.
        let language_id = self
            .language
            .loadable_language_for_document(&path.to_string_lossy(), text)
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

        let mut server_failures = self.wait_bridge_servers_ready(&url, ready_timeout).await;
        self.wait_eager_open_finished(&url, ready_timeout).await;

        // Counts requests that fail AFTER the server came up (crash, error
        // response, per-step timeout) — the ready-wait above cannot see
        // those, and without the counter they collapse into "no edits".
        let request_errors = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let result = self
            .formatting_impl_with_error_sink(
                DocumentFormattingParams {
                    text_document: TextDocumentIdentifier {
                        uri: lsp_uri.clone(),
                    },
                    options: formatting_options,
                    work_done_progress_params: Default::default(),
                },
                Some(std::sync::Arc::clone(&request_errors)),
            )
            .await;

        self.did_close_impl(DidCloseTextDocumentParams {
            text_document: TextDocumentIdentifier { uri: lsp_uri },
        })
        .await;

        // An Err from formatting_impl is currently unreachable in CLI mode
        // (it only signals upstream cancellation, and there is no upstream
        // request id here) — but if a future code path introduces one, a
        // silent "no change" would hide it, so surface it in the log.
        let formatted = result
            .inspect_err(|e| {
                log::warn!(target: "kakehashi::cli", "formatting failed for {url}: {e}");
            })
            .ok()
            .flatten()
            .filter(|edits| !edits.is_empty())
            .map(|edits| crate::text::edit::apply_text_edits(text, &edits));

        let request_errors = request_errors.load(std::sync::atomic::Ordering::Relaxed);
        if request_errors > 0 {
            server_failures.push(format!(
                "{request_errors} downstream formatting request(s) failed \
                 (run with RUST_LOG=kakehashi=warn for details)"
            ));
        }
        CliFormatOutcome {
            formatted,
            server_failures,
        }
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
    /// configured for the document — injection-region (virt) servers and,
    /// when `bridge._self.enabled`, host-layer servers — is Ready, returning
    /// a description of each server that failed to come up.
    ///
    /// `formatting_impl` treats a not-yet-ready server as a failed step and
    /// skips it — correct for an editor where the next request retries, but a
    /// one-shot CLI run would silently produce no edits. Host requests do
    /// run their own handshake wait inside `get_or_create_connection`, but
    /// that time is spent inside the request's own budget — pre-warming here
    /// keeps a slow cold host server from surfacing as a spurious request
    /// failure. Spawning is idempotent
    /// (`get_or_create_connection_wait_ready`), so this overlaps with the
    /// eager-open `didOpen` triggered.
    async fn wait_bridge_servers_ready(&self, uri: &Url, timeout: Duration) -> Vec<String> {
        let mut configs = Vec::new();

        // Virt layer: servers configured for the document's injection regions.
        if let Some(language_name) = self.document_language(uri)
            && let Some(injection_query) = self.language.injection_query(&language_name)
            && let Some(snapshot) = self.documents.get(uri).and_then(|doc| doc.snapshot())
        {
            let regions = InjectionResolver::resolve_all(
                &self.language,
                self.bridge.node_tracker(),
                uri,
                snapshot.tree(),
                snapshot.text(),
                injection_query.as_ref(),
            );
            for resolved in regions {
                configs.extend(self.bridge_configs_for_injection_language(
                    &language_name,
                    &resolved.injection_language,
                ));
            }
        }

        // Host layer (bridge._self): resolves to nothing unless opted in.
        if let Ok(lsp_uri) = url_to_uri(uri)
            && let Some(ctx) = self.resolve_host_bridge_context(&lsp_uri, "textDocument/formatting")
        {
            configs.extend(ctx.configs);
        }

        let pool = self.bridge.pool_arc();
        let mut waited: HashSet<String> = HashSet::new();
        let mut ready_waits = Vec::new();
        for config in configs {
            if waited.insert(config.server_name.clone()) {
                let pool = std::sync::Arc::clone(&pool);
                ready_waits.push(async move {
                    let result = pool
                        .get_or_create_connection_wait_ready(
                            &config.server_name,
                            &config.config,
                            timeout,
                        )
                        .await;
                    (config.server_name, result)
                });
            }
        }

        // Wait concurrently so several broken servers cost one `timeout`
        // total, not one each.
        let mut failures = Vec::new();
        for (server_name, result) in futures::future::join_all(ready_waits).await {
            if let Err(e) = result {
                log::warn!(
                    target: "kakehashi::cli",
                    "downstream server {server_name} not ready: {e}"
                );
                failures.push(format!(
                    "downstream server '{server_name}' failed to start: {e}"
                ));
            }
        }
        failures
    }
}
