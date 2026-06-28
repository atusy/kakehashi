//! `kakehashi format` per-file wrapper (CLI mode).
//!
//! Packages the formatting lifecycle — `didOpen` → wait for downstream servers
//! → `textDocument/formatting` → `didClose` — so `crate::cli::format` never
//! touches server internals. The shared CLI lifecycle (`cli_initialize`,
//! readiness waits, …) lives in the parent [`super`] module.

use std::path::Path;
use std::time::Duration;

use tower_lsp_server::ls_types::{
    DidCloseTextDocumentParams, DidOpenTextDocumentParams, DocumentFormattingParams,
    FormattingOptions, TextDocumentIdentifier, TextDocumentItem,
};
use url::Url;

use super::super::{Kakehashi, url_to_uri};

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

        let mut server_failures = self
            .wait_bridge_servers_ready(&url, "textDocument/formatting", ready_timeout)
            .await;
        if !self.wait_eager_open_finished(&url, ready_timeout).await {
            // Formatting would race the still-pending didOpens (the
            // downstream server may see an unknown URI and answer null =
            // "no edits") — report instead of silently mis-reporting
            // "unchanged".
            server_failures.push(format!(
                "eager-open tasks did not finish within {ready_timeout:?}; \
                 formatting results would be unreliable"
            ));
        }

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
            // "operation(s)": the counter also covers capability-probe
            // failures and pipeline step timeouts, not just request errors.
            server_failures.push(format!(
                "{request_errors} downstream formatting operation(s) failed \
                 (run with RUST_LOG=kakehashi=warn for details)"
            ));
        }
        CliFormatOutcome {
            formatted,
            server_failures,
        }
    }
}
