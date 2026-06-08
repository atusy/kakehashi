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
/// name and the **current** accumulated text. Its result is interpreted per
/// concatenated-formatting-pipeline Decision points 3 and 6:
///
/// - `Some(edits)` — apply `edits` (relative to the current text) to produce the
///   next accumulated text. An empty list is the authoritative "already
///   formatted / no changes" signal and is a no-op that still advances the
///   pipeline.
/// - `None` — a failed or unsupported step: **skip and continue** with the
///   current accumulated text, never aborting the pipeline or discarding earlier
///   successful steps.
///
/// Returns `Some(final_text)` when the final accumulated text differs from
/// `initial_text`, or `None` when nothing changed (every server was a no-op,
/// was skipped, or all failed) so the caller can emit no edit.
pub(crate) async fn run_sequential_format_pipeline<F, Fut>(
    initial_text: String,
    server_names: &[String],
    format_step: F,
) -> Option<String>
where
    F: Fn(String, String) -> Fut,
    Fut: Future<Output = Option<Vec<TextEdit>>>,
{
    let mut accumulated = initial_text.clone();

    for server_name in server_names {
        match format_step(server_name.clone(), accumulated.clone()).await {
            // Skip-and-continue: a failed or unsupported server contributes
            // nothing but must not discard the text produced so far.
            None => continue,
            Some(edits) => {
                // Empty edits = authoritative "already formatted" = no-op.
                if edits.is_empty() {
                    continue;
                }
                accumulated = apply_text_edits(&accumulated, &edits);
            }
        }
    }

    if accumulated == initial_text {
        None
    } else {
        Some(accumulated)
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
            async move { value }
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
            async move { value }
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
            async move { value }
        })
        .await;

        assert_eq!(result, Some("done".to_string()));
    }

    #[tokio::test]
    async fn returns_none_when_text_is_unchanged() {
        // Every server is a no-op (empty edits or skip) → final text equals the
        // original → no edit to emit.
        let servers = vec!["noop".to_string(), "broken".to_string()];
        let result = run_sequential_format_pipeline("abc".to_string(), &servers, |name, _text| {
            let value = match name.as_str() {
                "noop" => Some(vec![]),
                "broken" => None,
                other => panic!("unexpected server {other}"),
            };
            async move { value }
        })
        .await;

        assert_eq!(result, None, "unchanged text must contribute no edit");
    }

    #[tokio::test]
    async fn empty_server_list_returns_none() {
        let result = run_sequential_format_pipeline(
            "abc".to_string(),
            &[],
            |_name, _text| async move { None },
        )
        .await;
        assert_eq!(result, None);
    }
}
