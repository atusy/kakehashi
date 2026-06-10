//! Text synchronization utilities for LSP didChange handling.
//!
//! This module provides functions for processing LSP TextDocumentContentChangeEvent
//! items and building tree-sitter InputEdit structures for incremental parsing.
//!
//! # Overview
//!
//! The LSP protocol supports two text synchronization modes:
//! - **Incremental**: Client sends only the changed ranges
//! - **Full**: Client sends the entire document content
//!
//! This module handles both modes and produces the appropriate data structures
//! for tree-sitter's incremental parsing API.

use tower_lsp_server::ls_types::TextDocumentContentChangeEvent;
use tree_sitter::InputEdit;

use crate::text::PositionMapper;

/// Apply LSP content changes to `old_text`, returning the updated text and the
/// matching tree-sitter `InputEdit`s. Changes with `range` build an `InputEdit`;
/// rangeless full-sync changes replace the whole text and emit no edit.
///
/// An empty returned edits vec is the caller's signal to do a full re-parse
/// (`apply_text_change`); a non-empty vec means incremental (`apply_edits`).
pub(crate) fn apply_content_changes_with_edits(
    old_text: &str,
    content_changes: Vec<TextDocumentContentChangeEvent>,
) -> (String, Vec<InputEdit>) {
    let mut text = old_text.to_string();
    let mut edits = Vec::new();

    for change in content_changes {
        if let Some(range) = change.range {
            // Incremental change - create InputEdit for tree editing.
            //
            // Use the *clamped* position→byte mapping: `position_to_byte` returns
            // `line_start + character` without bounding it to the line/document,
            // so a range end past the current text yields an offset > text.len().
            // That overflows `replace_range` below ("range end index N out of
            // range for slice of length M"). This is reachable when a stale,
            // shorter base text is edited with a range authored against a later
            // document state (concurrent `didChange` processing). Clamping keeps
            // the splice in bounds; `start <= end` is enforced so the range never
            // inverts.
            let mapper = PositionMapper::new(&text);
            let start_offset = mapper.position_to_byte_clamped(range.start);
            let end_offset = mapper.position_to_byte_clamped(range.end).max(start_offset);
            let new_end_offset = start_offset + change.text.len();

            // Calculate the new end position for tree-sitter (using byte columns)
            let lines: Vec<&str> = change.text.split('\n').collect();
            let line_count = lines.len();
            // last_line_len is in BYTES (not UTF-16) because .len() on &str returns byte count
            let last_line_len = lines.last().map(|l| l.len()).unwrap_or(0);

            // Get start position with proper byte column conversion
            let start_point =
                mapper
                    .position_to_point(range.start)
                    .unwrap_or(tree_sitter::Point::new(
                        range.start.line as usize,
                        start_offset,
                    ));

            // Calculate new end Point (tree-sitter uses byte columns)
            let new_end_point = if line_count > 1 {
                // New content spans multiple lines
                tree_sitter::Point::new(start_point.row + line_count - 1, last_line_len)
            } else {
                // New content is on same line as start
                tree_sitter::Point::new(start_point.row, start_point.column + last_line_len)
            };

            // Create InputEdit for incremental parsing
            let edit = InputEdit {
                start_byte: start_offset,
                old_end_byte: end_offset,
                new_end_byte: new_end_offset,
                start_position: start_point,
                old_end_position: mapper
                    .position_to_point(range.end)
                    .unwrap_or(tree_sitter::Point::new(range.end.line as usize, end_offset)),
                new_end_position: new_end_point,
            };
            edits.push(edit);

            // Replace the range with new text
            text.replace_range(start_offset..end_offset, &change.text);
        } else {
            // Full document change - no incremental parsing
            text = change.text;
            edits.clear(); // Clear any previous edits since it's a full replacement
        }
    }

    (text, edits)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower_lsp_server::ls_types::{Position, Range, TextDocumentContentChangeEvent};

    // ============================================================
    // Tests for apply_content_changes_with_edits branch decision
    // ============================================================
    // These tests verify that the function returns empty vs non-empty edits
    // correctly, which controls the branch in did_change between
    // apply_text_change (full sync) and apply_edits (incremental sync).

    #[test]
    fn test_apply_content_changes_incremental_produces_edits() {
        // Incremental change (with range) should produce InputEdits
        let old_text = "hello world";
        let changes = vec![TextDocumentContentChangeEvent {
            range: Some(Range {
                start: Position {
                    line: 0,
                    character: 6,
                },
                end: Position {
                    line: 0,
                    character: 11,
                },
            }),
            range_length: Some(5),
            text: "rust".to_string(),
        }];

        let (new_text, edits) = apply_content_changes_with_edits(old_text, changes);

        // Verify text was updated
        assert_eq!(new_text, "hello rust");

        // Verify edits is NON-EMPTY (incremental sync path will be taken)
        assert!(
            !edits.is_empty(),
            "Incremental change should produce non-empty edits for apply_edits path"
        );
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].start_byte, 6);
        assert_eq!(edits[0].old_end_byte, 11);
        assert_eq!(edits[0].new_end_byte, 10); // "rust" is 4 bytes
    }

    #[test]
    fn test_apply_content_changes_out_of_range_does_not_panic() {
        // Regression: a change whose range extends past the current text must not
        // panic in `replace_range`. This happens under concurrent `didChange`
        // processing, where a handler reads a stale (shorter) base text but the
        // client-authored range targets a later, longer document state. Here the
        // base text is "local x = {\n}\n" (14 bytes); the range end maps to byte
        // 15 (line 1, col 3 = line_start 12 + 3), one past the text — the exact
        // shape of the observed `range end index 15 out of range for slice of
        // length 14` crash. The change must be applied (clamped), not panic.
        let old_text = "local x = {\n}\n"; // 14 bytes
        let changes = vec![TextDocumentContentChangeEvent {
            range: Some(Range {
                start: Position {
                    line: 1,
                    character: 2,
                },
                end: Position {
                    line: 1,
                    character: 3,
                },
            }),
            range_length: Some(1),
            text: "\n  }".to_string(),
        }];

        // Must not panic.
        let (new_text, edits) = apply_content_changes_with_edits(old_text, changes);
        assert!(
            !edits.is_empty(),
            "incremental change should produce an edit"
        );
        // The offsets are clamped to the text length, so the edit stays in bounds
        // and the resulting text is well-formed (no slice panic).
        assert!(new_text.starts_with("local x = {"));
    }

    #[test]
    fn test_apply_content_changes_full_sync_produces_empty_edits() {
        // Full document change (without range) should produce EMPTY edits
        let old_text = "hello world";
        let changes = vec![TextDocumentContentChangeEvent {
            range: None, // No range = full document sync
            range_length: None,
            text: "completely new content".to_string(),
        }];

        let (new_text, edits) = apply_content_changes_with_edits(old_text, changes);

        // Verify text was replaced
        assert_eq!(new_text, "completely new content");

        // Verify edits is EMPTY (apply_text_change path will be taken)
        assert!(
            edits.is_empty(),
            "Full document sync should produce empty edits for apply_text_change path"
        );
    }

    #[test]
    fn test_apply_content_changes_mixed_clears_edits_on_full_sync() {
        // Mixed changes: incremental followed by full sync should clear edits
        let old_text = "hello world";
        let changes = vec![
            // First: incremental change
            TextDocumentContentChangeEvent {
                range: Some(Range {
                    start: Position {
                        line: 0,
                        character: 0,
                    },
                    end: Position {
                        line: 0,
                        character: 5,
                    },
                }),
                range_length: Some(5),
                text: "hi".to_string(),
            },
            // Second: full document sync (should clear previous edits)
            TextDocumentContentChangeEvent {
                range: None,
                range_length: None,
                text: "final content".to_string(),
            },
        ];

        let (new_text, edits) = apply_content_changes_with_edits(old_text, changes);

        // Verify final text
        assert_eq!(new_text, "final content");

        // Verify edits is EMPTY because full sync clears all previous edits
        assert!(
            edits.is_empty(),
            "Full document sync should clear previous incremental edits"
        );
    }

    #[test]
    fn test_apply_content_changes_multiple_incremental_accumulates_edits() {
        // Multiple incremental changes should accumulate edits
        let old_text = "aaa bbb ccc";
        let changes = vec![
            // First: replace "aaa" with "AAA"
            TextDocumentContentChangeEvent {
                range: Some(Range {
                    start: Position {
                        line: 0,
                        character: 0,
                    },
                    end: Position {
                        line: 0,
                        character: 3,
                    },
                }),
                range_length: Some(3),
                text: "AAA".to_string(),
            },
            // Second: replace "ccc" with "CCC" (position adjusted for running coords)
            TextDocumentContentChangeEvent {
                range: Some(Range {
                    start: Position {
                        line: 0,
                        character: 8,
                    },
                    end: Position {
                        line: 0,
                        character: 11,
                    },
                }),
                range_length: Some(3),
                text: "CCC".to_string(),
            },
        ];

        let (new_text, edits) = apply_content_changes_with_edits(old_text, changes);

        // Verify final text
        assert_eq!(new_text, "AAA bbb CCC");

        // Verify multiple edits accumulated (incremental sync path)
        assert_eq!(
            edits.len(),
            2,
            "Multiple incremental changes should produce multiple edits"
        );
    }
}
