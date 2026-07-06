//! Text document related LSP methods.

mod code_lens;
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
mod native_bindings;
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

/// Record a `preferred`-strategy fan-in's failures into `sink`, counting them
/// **only when no server won** (`NoResult`).
///
/// Under `preferred`, the winning server's result is authoritative, so a
/// non-winning server's failure is irrelevant — it must not surface as a CLI
/// exit-2 (the same intent [`RequestErrorSink`] documents: an abandoned
/// loser was *cancelled*, not failed). Counting losers in-task is also **racy**:
/// the preferred fan-in `abort_all`s the losers without joining them, so a
/// loser's in-task `fetch_add` can land after the CLI has read the counter
/// (#487). Counting from the fan-in result instead is deterministic: only
/// `NoResult` (no server won — every contender failed or returned empty)
/// carries a decisive, fully-drained `errors` count, and `errors` counts only
/// the actual failures (it is `0` when the non-winners merely returned empty),
/// so an all-empty run is not a failure. `Done`/`Cancelled` count nothing.
/// Shared so the preferred diagnose and (future) formatting dispatches stay
/// consistent.
pub(crate) fn count_no_winner_errors<T>(
    result: &crate::lsp::aggregation::server::FanInResult<T>,
    sink: &RequestErrorSink,
) {
    if let crate::lsp::aggregation::server::FanInResult::NoResult { errors } = result {
        count_request_errors(sink, *errors);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lsp::aggregation::server::FanInResult;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn sink_value(sink: &RequestErrorSink) -> usize {
        sink.as_ref().unwrap().load(Ordering::Relaxed)
    }

    #[test]
    fn count_no_winner_errors_counts_only_when_no_server_won() {
        let sink: RequestErrorSink = Some(Arc::new(AtomicUsize::new(0)));

        // No winner (every server failed) → the drained failure count is
        // decisive and is recorded, so an all-failed `preferred` run still
        // surfaces as exit 2.
        count_no_winner_errors(&FanInResult::<()>::NoResult { errors: 3 }, &sink);
        assert_eq!(sink_value(&sink), 3);

        // A winner emerged → non-winning failures are irrelevant and MUST NOT
        // be counted (the heart of #487).
        count_no_winner_errors(&FanInResult::Done(()), &sink);
        assert_eq!(
            sink_value(&sink),
            3,
            "Done must not count non-winning failures"
        );

        // Cancelled → nothing decisive happened; count nothing.
        count_no_winner_errors(&FanInResult::<()>::Cancelled, &sink);
        assert_eq!(sink_value(&sink), 3, "Cancelled must not count");
    }

    #[test]
    fn count_no_winner_errors_is_a_noop_without_a_sink() {
        // LSP mode installs no sink; the helper must not panic.
        let sink: RequestErrorSink = None;
        count_no_winner_errors(&FanInResult::<()>::NoResult { errors: 5 }, &sink);
    }
}

// Re-export the methods (they are implemented as impl blocks on Kakehashi)
