//! Text document related LSP methods.

mod code_lens;
#[cfg(feature = "experimental")]
mod color_presentation;
mod completion;
mod completion_item;
mod declaration;
mod definition;
pub(crate) mod diagnostic;
mod did_change;
mod did_close;
mod did_open;
mod did_save;
#[cfg(feature = "experimental")]
mod document_color;
mod document_highlight;
mod document_link;
mod document_symbol;
mod folding_range;
mod formatting;
mod hover;
mod implementation;
mod inlay_hint;
mod linked_editing_range;
mod moniker;
mod on_type_formatting;
mod prepare_rename;
pub(crate) mod publish_diagnostic;
mod range_formatting;
mod references;
mod rename;
mod selection_range;
mod semantic_tokens;
mod signature_help;
mod type_definition;
mod will_save;
mod will_save_wait_until;

/// Optional counter for downstream bridge requests that failed at request time
/// (I/O error, error response, per-step timeout) — as opposed to startup
/// failures, which CLI mode detects separately via its ready-wait. `None` in
/// LSP mode, where failed requests are log-only because the editor retries;
/// `Some` in CLI mode (`format`, `diagnose`), where a one-shot run must map
/// them onto a non-zero exit instead of "nothing changed" / "no diagnostics".
///
/// Counts **observed** failures only, by design: a request abandoned because a
/// racing layer or higher-priority server already won (its future dropped
/// mid-flight) was *cancelled*, not failed — it never produced a verdict, and
/// counting it would trade the race's latency win for accounting of requests
/// whose outcome no longer matters.
pub(crate) type RequestErrorSink = Option<std::sync::Arc<std::sync::atomic::AtomicUsize>>;

/// Add `n` request failures to the sink, if one is installed.
pub(crate) fn count_request_errors(sink: &RequestErrorSink, n: usize) {
    if n > 0
        && let Some(sink) = sink
    {
        sink.fetch_add(n, std::sync::atomic::Ordering::Relaxed);
    }
}

// Re-export the methods (they are implemented as impl blocks on Kakehashi)
