//! Sequential formatter pipeline for the `concatenated` aggregation strategy
//! on `textDocument/formatting` (concatenated-formatting-pipeline).
//!
//! Unlike the list-concatenation mechanics diagnostics/references use, two
//! formatters over the same region produce *overlapping* `TextEdit`s. The
//! pipeline instead runs the priority-listed servers **serially**: each server
//! formats the previous server's output, so by construction there is nothing to
//! merge and the eventual region-replacement edit can never overlap.
//!
//! This module owns only the **pure** sequential core — feed each server the
//! accumulated text, apply its returned edits, carry the result forward — so it
//! is unit-testable with a fake per-server step and no real bridge. The host
//! wiring (scratch URIs, capability fallback, host-coordinate translation) lives
//! in the formatting handler.

use std::future::Future;

use tower_lsp_server::ls_types::TextEdit;

use crate::text::edit::apply_text_edits;

/// Run the priority-ordered servers serially over `initial_text`.
///
/// For each `server_name` in order, `format_step` is invoked with that server's
/// name and the **current** accumulated text *by value*, and must return that
/// **same text moved straight back, unchanged**, alongside its result. The move
/// in/out is purely to avoid cloning — rather than borrowing `&str` and cloning
/// per step — and keeps the pipeline O(text_size) total instead of
/// O(steps × text_size): no-op and failed steps no longer clone the whole
/// accumulated text. `format_step` must **not** mutate the text it returns: all
/// changes are conveyed via the returned edit list, and *this function* applies
/// them (returning mutated text would double-apply on top of the edits). The
/// result is interpreted per concatenated-formatting-pipeline Decision points 3
/// and 6:
///
/// - `Some(edits)` — apply `edits` (relative to the returned text) to produce
///   the next accumulated text. An empty list is the authoritative "already
///   formatted / no changes" signal and is a no-op that still advances the
///   pipeline.
/// - `None` — a failed or unsupported step: **skip and continue** with the
///   text the step handed back, never aborting the pipeline or discarding
///   earlier successful steps.
///
/// Returns `Some(final_text)` only when at least one server applied a non-empty
/// edit **and** the resulting text is not byte-identical to the original;
/// otherwise `None`, so the caller emits no edit. Byte-equality with the
/// original — not merely "some step returned edits" — is the "no change" signal,
/// so a formatter that returns edits which round-trip to identical content does
/// not produce a pointless whole-region replacement.
///
/// The original text is captured **lazily**: it is only cloned right before the
/// first non-empty edit is applied (at which point `accumulated` still equals the
/// original, since every prior step was a no-op that returned the text
/// unchanged). The common "already formatted" case — where no step ever applies a
/// non-empty edit — therefore never clones the text at all.
pub(crate) async fn run_sequential_format_pipeline<F, Fut>(
    initial_text: String,
    server_names: &[String],
    format_step: F,
) -> Option<String>
where
    F: Fn(String, String) -> Fut,
    Fut: Future<Output = (String, Option<Vec<TextEdit>>)>,
{
    // The working copy is moved through each step (no per-step clone). The
    // original is captured lazily into `original` only when the first non-empty
    // edit is about to be applied — at that moment `accumulated` still equals the
    // original (every prior step was a no-op that returned the text unchanged), so
    // cloning it then is equivalent to cloning `initial_text` up front but avoids
    // the clone entirely in the common "already formatted" case (no step ever
    // applies a non-empty edit). The comparison matters because a step can return
    // non-empty edits that nonetheless round-trip to byte-identical text (a
    // formatter reformatting already-conformant code); without it the caller would
    // emit a pointless whole-region replacement with identical content.
    let mut accumulated = initial_text;
    let mut original: Option<String> = None;

    for server_name in server_names {
        // Move the accumulated text into the step and take it back out — no
        // per-step clone, even when the step is a no-op or fails.
        let (text, result) = format_step(server_name.clone(), accumulated).await;
        accumulated = match result {
            // A non-empty edit list is the only thing that can change the text.
            Some(edits) if !edits.is_empty() => {
                // Capture the original just before the first applied edit, while
                // `text` still equals the original text.
                if original.is_none() {
                    original = Some(text.clone());
                }
                apply_text_edits(&text, &edits)
            }
            // Empty edits ("already formatted") or `None` (failed/unsupported
            // step): skip-and-continue carrying the text the step handed back.
            _ => text,
        };
    }

    // Emit `Some` only when an edit was captured-and-applied AND the bytes
    // actually differ from the captured original.
    match original {
        Some(original) if accumulated != original => Some(accumulated),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower_lsp_server::ls_types::{Position, Range};

    /// Whole-text replacement edit (range covers the entire input), the shape a
    /// whole-document formatter emits.
    fn replace_all(new_text: &str) -> TextEdit {
        TextEdit {
            range: Range {
                start: Position {
                    line: 0,
                    character: 0,
                },
                // u32::MAX end is clamped by `apply_text_edits` to the real EOF.
                end: Position {
                    line: u32::MAX,
                    character: u32::MAX,
                },
            },
            new_text: new_text.to_string(),
        }
    }

    #[tokio::test]
    async fn feeds_each_server_the_previous_servers_output_in_order() {
        // black uppercases, then isort appends a marker. The second server must
        // see the FIRST server's output, not the original text.
        let servers = vec!["black".to_string(), "isort".to_string()];
        let result = run_sequential_format_pipeline("abc".to_string(), &servers, |name, text| {
            let value = match name.as_str() {
                "black" => {
                    assert_eq!(text, "abc", "black must see the original text");
                    Some(vec![replace_all(&text.to_uppercase())])
                }
                "isort" => {
                    assert_eq!(text, "ABC", "isort must see black's output");
                    Some(vec![replace_all(&format!("{text}!"))])
                }
                other => panic!("unexpected server {other}"),
            };
            async move { (text, value) }
        })
        .await;

        assert_eq!(result, Some("ABC!".to_string()));
    }

    #[tokio::test]
    async fn skips_a_failing_server_and_continues_with_current_text() {
        // The first server fails (None); the pipeline must hand the SECOND
        // server the unchanged original text rather than aborting.
        let servers = vec!["broken".to_string(), "isort".to_string()];
        let result = run_sequential_format_pipeline("abc".to_string(), &servers, |name, text| {
            let value = match name.as_str() {
                "broken" => None,
                "isort" => {
                    assert_eq!(text, "abc", "skipped server must not change the text");
                    Some(vec![replace_all("formatted")])
                }
                other => panic!("unexpected server {other}"),
            };
            async move { (text, value) }
        })
        .await;

        assert_eq!(result, Some("formatted".to_string()));
    }

    #[tokio::test]
    async fn empty_edit_list_is_a_no_op_and_advances_the_pipeline() {
        // First server reports "already formatted" (empty edits); the text is
        // unchanged but the pipeline still proceeds to the second server.
        let servers = vec!["already_ok".to_string(), "isort".to_string()];
        let result = run_sequential_format_pipeline("abc".to_string(), &servers, |name, text| {
            let value = match name.as_str() {
                "already_ok" => Some(vec![]),
                "isort" => {
                    assert_eq!(text, "abc", "empty edits must leave the text untouched");
                    Some(vec![replace_all("done")])
                }
                other => panic!("unexpected server {other}"),
            };
            async move { (text, value) }
        })
        .await;

        assert_eq!(result, Some("done".to_string()));
    }

    #[tokio::test]
    async fn returns_none_when_text_is_unchanged() {
        // Every server is a no-op (empty edits or skip) → final text equals the
        // original → no edit to emit.
        let servers = vec!["noop".to_string(), "broken".to_string()];
        let result = run_sequential_format_pipeline("abc".to_string(), &servers, |name, text| {
            let value = match name.as_str() {
                "noop" => Some(vec![]),
                "broken" => None,
                other => panic!("unexpected server {other}"),
            };
            async move { (text, value) }
        })
        .await;

        assert_eq!(result, None, "unchanged text must contribute no edit");
    }

    #[tokio::test]
    async fn returns_some_when_a_step_changes_the_bytes() {
        // A non-empty edit that actually changes the text yields Some(new_text).
        let servers = vec!["upper".to_string()];
        let result = run_sequential_format_pipeline("abc".to_string(), &servers, |_name, text| {
            let value = Some(vec![replace_all(&text.to_uppercase())]);
            async move { (text, value) }
        })
        .await;

        assert_eq!(result, Some("ABC".to_string()));
    }

    #[tokio::test]
    async fn non_empty_edits_that_round_trip_to_identical_text_emit_nothing() {
        // A formatter may return a (non-empty) whole-document replacement whose
        // content is byte-identical to the input — e.g. re-formatting code that
        // already conforms. The pipeline must NOT report a change, so the caller
        // never emits a pointless whole-region replacement with identical text.
        let servers = vec!["noop_reformat".to_string()];
        let result = run_sequential_format_pipeline("abc".to_string(), &servers, |_name, text| {
            let value = Some(vec![replace_all(&text)]); // same bytes back
            async move { (text, value) }
        })
        .await;

        assert_eq!(result, None);
    }

    #[tokio::test]
    async fn empty_server_list_returns_none() {
        let result =
            run_sequential_format_pipeline("abc".to_string(), &[], |_name, text| async move {
                (text, None)
            })
            .await;
        assert_eq!(result, None);
    }
}
