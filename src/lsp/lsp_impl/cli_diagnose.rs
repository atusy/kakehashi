//! In-process driving of the LSP handlers for `kakehashi diagnose` (CLI mode).
//!
//! Mirrors [`super::cli_format`]: the CLI runs the same `Kakehashi` instance
//! an editor would talk to, but calls the handler implementations directly
//! instead of going through JSON-RPC framing. This wrapper packages the
//! per-file lifecycle — `didOpen` → wait for downstream servers →
//! `textDocument/diagnostic` → `didClose` — so `crate::cli::diagnose` never
//! touches server internals.
//!
//! Lives under `lsp_impl` (not `crate::cli`) because it relies on the
//! `pub(crate)` lifecycle helpers (`wait_bridge_servers_ready`,
//! `wait_eager_open_finished`) that need the private `language` / `documents`
//! / `bridge` fields.

use std::path::Path;
use std::time::Duration;

use tower_lsp_server::ls_types::{
    Diagnostic, DidCloseTextDocumentParams, DidOpenTextDocumentParams, DocumentDiagnosticParams,
    DocumentDiagnosticReport, DocumentDiagnosticReportResult, TextDocumentIdentifier,
    TextDocumentItem,
};
use url::Url;

use super::{Kakehashi, url_to_uri};

/// Result of one CLI diagnostic attempt ([`Kakehashi::cli_diagnose_text`]).
pub(crate) struct CliDiagnoseOutcome {
    /// The aggregated diagnostics for the document (possibly empty).
    pub(crate) diagnostics: Vec<Diagnostic>,
    /// Human-readable descriptions of configured downstream servers that
    /// failed to spawn or initialize within the ready timeout. Non-empty
    /// means the document's diagnostics are incomplete or unverifiable — the
    /// CLI maps this to its error exit code instead of reporting the file as
    /// "clean".
    pub(crate) server_failures: Vec<String>,
}

impl Kakehashi {
    /// Collect diagnostics for `text` as if it were the document at absolute
    /// `path`, driving the same `didOpen` → wait-ready → pull → `didClose`
    /// lifecycle the editor uses.
    ///
    /// `server_failures` lists configured downstream servers that failed to
    /// come up (or a request that errored after they did), so the caller can
    /// distinguish "no diagnostics" from "the diagnostics are unreliable".
    /// Downstream servers are spawned cold on the first file; the explicit
    /// ready-wait (bounded by `ready_timeout` per server) keeps a cold start
    /// from being misread as "no diagnostics".
    pub(crate) async fn cli_diagnose_text(
        &self,
        path: &Path,
        text: &str,
        ready_timeout: Duration,
    ) -> CliDiagnoseOutcome {
        // A path we cannot express as a URI cannot be opened as an LSP
        // document at all — report it as a failure (the CLI maps it to its
        // error exit code) instead of silently skipping the file.
        let uri_failure = |what: &str| CliDiagnoseOutcome {
            diagnostics: Vec::new(),
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

        // Path-based detection first, then first-line content detection, via
        // the loading chain — nothing is loaded before the first didOpen in
        // CLI mode. Detect on the raw path, not `url.path()`, whose
        // percent-encoding would break extension/filename matching.
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
            .wait_bridge_servers_ready(&url, "textDocument/diagnostic", ready_timeout)
            .await;
        if !self.wait_eager_open_finished(&url, ready_timeout).await {
            // A diagnostic pull would race the still-pending didOpens (the
            // downstream server may see an unknown URI and answer an empty
            // report) — report instead of silently mis-reporting "clean".
            server_failures.push(format!(
                "eager-open tasks did not finish within {ready_timeout:?}; \
                 diagnostic results would be unreliable"
            ));
        }

        let report = self
            .diagnostic_impl(DocumentDiagnosticParams {
                text_document: TextDocumentIdentifier {
                    uri: lsp_uri.clone(),
                },
                identifier: None,
                previous_result_id: None,
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            })
            .await;

        self.did_close_impl(DidCloseTextDocumentParams {
            text_document: TextDocumentIdentifier { uri: lsp_uri },
        })
        .await;

        // An Err is only returned on upstream cancellation, which cannot
        // happen in CLI mode (no upstream request id) — but a future code
        // path might introduce one, and a silent empty report would hide it.
        let diagnostics = match report {
            Ok(result) => diagnostics_from_report(result),
            Err(e) => {
                log::warn!(target: "kakehashi::cli", "diagnostic failed for {url}: {e}");
                server_failures.push(format!("diagnostic request failed: {e}"));
                Vec::new()
            }
        };

        CliDiagnoseOutcome {
            diagnostics,
            server_failures,
        }
    }
}

/// Extract the diagnostic items from a pull-diagnostic report.
///
/// CLI mode never sends `previousResultId`, so `diagnostic_impl` always
/// answers with a full report; an `unchanged` report (which cannot
/// legitimately occur here) yields nothing.
fn diagnostics_from_report(result: DocumentDiagnosticReportResult) -> Vec<Diagnostic> {
    match result {
        DocumentDiagnosticReportResult::Report(DocumentDiagnosticReport::Full(report)) => {
            report.full_document_diagnostic_report.items
        }
        DocumentDiagnosticReportResult::Report(DocumentDiagnosticReport::Unchanged(_))
        | DocumentDiagnosticReportResult::Partial(_) => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower_lsp_server::ls_types::{
        FullDocumentDiagnosticReport, Position, Range, RelatedFullDocumentDiagnosticReport,
        RelatedUnchangedDocumentDiagnosticReport, UnchangedDocumentDiagnosticReport,
    };

    fn diag(message: &str) -> Diagnostic {
        Diagnostic {
            range: Range::new(Position::new(0, 0), Position::new(0, 1)),
            message: message.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn full_report_yields_its_items() {
        let result = DocumentDiagnosticReportResult::Report(DocumentDiagnosticReport::Full(
            RelatedFullDocumentDiagnosticReport {
                full_document_diagnostic_report: FullDocumentDiagnosticReport {
                    result_id: None,
                    items: vec![diag("boom")],
                },
                related_documents: None,
            },
        ));
        let items = diagnostics_from_report(result);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].message, "boom");
    }

    #[test]
    fn unchanged_report_yields_nothing() {
        let result = DocumentDiagnosticReportResult::Report(DocumentDiagnosticReport::Unchanged(
            RelatedUnchangedDocumentDiagnosticReport {
                unchanged_document_diagnostic_report: UnchangedDocumentDiagnosticReport {
                    result_id: "x".to_string(),
                },
                related_documents: None,
            },
        ));
        assert!(diagnostics_from_report(result).is_empty());
    }
}
