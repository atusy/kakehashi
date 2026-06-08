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
/// name and the **current** accumulated text *by value* and must hand the text
/// back (possibly unchanged) alongside its result. Handing the text in and out
/// by move — rather than passing a borrowed `&str` plus a per-step
/// `accumulated.clone()` — keeps the pipeline O(text_size) total instead of
/// O(steps × text_size): no-op and failed steps no longer clone the whole
/// accumulated text. The result is interpreted per
/// concatenated-formatting-pipeline Decision points 3 and 6:
///
/// - `Some(edits)` — apply `edits` (relative to the returned text) to produce
///   the next accumulated text. An empty list is the authoritative "already
///   formatted / no changes" signal and is a no-op that still advances the
///   pipeline.
/// - `None` — a failed or unsupported step: **skip and continue** with the
///   text the step handed back, never aborting the pipeline or discarding
///   earlier successful steps.
///
/// Returns `Some(final_text)` when at least one server applied a non-empty
/// edit, or `None` when nothing changed (every server was a no-op via empty
/// edits, was skipped, or all failed) so the caller can emit no edit. An empty
/// edit list — not byte-equality with `initial_text` — is the "no change"
/// signal, which lets the pipeline avoid cloning `initial_text` up front and
/// re-scanning the whole accumulated text at the end.
pub(crate) async fn run_sequential_format_pipeline<F, Fut>(
    initial_text: String,
    server_names: &[String],
    format_step: F,
) -> Option<String>
where
    F: Fn(String, String) -> Fut,
    Fut: Future<Output = (String, Option<Vec<TextEdit>>)>,
{
    // Start from the moved-in `initial_text` and only ever allocate a new
    // string when a step actually changes the text. `changed` tracks whether
    // any step produced a real edit, so we avoid both the up-front clone of
    // `initial_text` and the final whole-string `accumulated == initial_text`
    // comparison (which would re-scan potentially large region text on every
    // call). When nothing changed we return `None` so the caller emits no edit.
    let mut accumulated = initial_text;
    let mut changed = false;

    for server_name in server_names {
        // Move the accumulated text into the step and take it back out — no
        // per-step clone, even when the step is a no-op or fails.
        let (text, result) = format_step(server_name.clone(), accumulated).await;
        accumulated = match result {
            // A non-empty edit list is the only signal that the text changed.
            Some(edits) if !edits.is_empty() => {
                changed = true;
                apply_text_edits(&text, &edits)
            }
            // Empty edits ("already formatted") or `None` (failed/unsupported
            // step): skip-and-continue carrying the text the step handed back.
            _ => text,
        };
    }

    if changed { Some(accumulated) } else { None }
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
    async fn returns_some_when_a_step_applies_a_real_edit() {
        // A non-empty edit list means the step changed the text. Even if the
        // edit happens to reproduce the original bytes, the pipeline reports a
        // change: emptiness — not byte-equality — is the "no-op" signal, and
        // skipping the whole-string comparison is the point of the `changed`
        // flag. The previous byte-equality check would have returned `None`
        // here; this test pins the new contract.
        let servers = vec!["echo".to_string()];
        let result = run_sequential_format_pipeline("abc".to_string(), &servers, |_name, text| {
            // Replace the whole text with identical content via a real edit.
            let value = Some(vec![replace_all(&text)]);
            async move { (text, value) }
        })
        .await;

        assert_eq!(result, Some("abc".to_string()));
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
