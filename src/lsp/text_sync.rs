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
use tree_sitter::{InputEdit, Point};

use crate::text::PositionMapper;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SequentialByteEdit {
    pub(crate) start_byte: usize,
    pub(crate) old_end_byte: usize,
    pub(crate) new_text: String,
}

pub(crate) struct AppliedContentChanges {
    pub(crate) text: String,
    pub(crate) input_edits: Vec<InputEdit>,
    /// `Some` preserves the ordered ranged-edit stream. `None` means at least
    /// one full replacement occurred, so an incremental consumer must resync.
    pub(crate) sequential_byte_edits: Option<Vec<SequentialByteEdit>>,
}

/// Apply LSP content changes to `old_text`, returning the updated text and the
/// matching tree-sitter `InputEdit`s. Changes with `range` build an `InputEdit`;
/// rangeless full-sync changes replace the whole text and emit no edit.
///
/// An empty returned edits vec is the caller's signal to do a full re-parse
/// (`apply_text_change`); a non-empty vec means incremental (`apply_edits`).
///
/// **If any rangeless (full-replacement) change occurs anywhere in the batch the
/// returned edits are empty**, even if ranged changes follow it. The returned
/// `InputEdit`s describe a transform of `old_text`, but a full replacement severs
/// that relationship: edits emitted *after* it are relative to the replacement
/// text, not `old_text`, so they can neither diff against `old_text` nor seed a
/// tree parsed from `old_text` (the #348 incremental-contract hazard). The whole
/// batch is therefore a full sync.
pub(crate) fn apply_content_changes_detailed(
    old_text: &str,
    content_changes: Vec<TextDocumentContentChangeEvent>,
) -> AppliedContentChanges {
    let mut text = old_text.to_string();
    let mut edits = Vec::new();
    let mut sequential_byte_edits = Vec::new();
    // Once a full replacement lands, no `InputEdit` in this batch can describe
    // `old_text → final` incrementally, so the batch is a full sync regardless of
    // any ranged changes that follow.
    let mut saw_full_replacement = false;

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
            // inverts. Clamping to line content also keeps an overlong column
            // from consuming that line's terminator (#707).
            let mapper = PositionMapper::new(&text);
            let start_offset = mapper.position_to_byte_clamped(range.start);
            let end_offset = mapper.position_to_byte_clamped(range.end).max(start_offset);

            sequential_byte_edits.push(SequentialByteEdit {
                start_byte: start_offset,
                old_end_byte: end_offset,
                new_text: change.text.clone(),
            });

            // Once a full replacement has landed in this batch the edits will be
            // discarded at the end (the batch is a full sync), so skip building the
            // `InputEdit` — its `split('\n')` allocation and `byte_to_point` searches
            // would be pure waste. Still apply the splice so `text` ends correct.
            if !saw_full_replacement {
                let new_end_offset = start_offset + change.text.len();

                // Calculate the new end position for tree-sitter (using byte columns).
                // Iterate the split directly rather than `.collect()`-ing a `Vec` — this
                // runs on every keystroke's ranged edit, so the allocation is pure
                // overhead. `split('\n')` always yields at least one segment, so
                // `line_count >= 1` and `last_line_len` ends as the last segment's BYTE
                // length (bytes, not UTF-16, since `str::len` is byte count).
                let mut line_count = 0usize;
                let mut last_line_len = 0usize;
                for line in change.text.split('\n') {
                    line_count += 1;
                    last_line_len = line.len();
                }

                // Points are derived from the (clamped) byte offsets via the same
                // `LineIndex` (O(log n)), not the raw LSP positions, so they stay
                // consistent with start_byte/old_end_byte for incremental parsing.
                let start_point = mapper.byte_to_point(start_offset);
                let old_end_point = mapper.byte_to_point(end_offset);

                // Calculate new end Point (tree-sitter uses byte columns)
                let new_end_point = if line_count > 1 {
                    // New content spans multiple lines
                    Point::new(start_point.row + line_count - 1, last_line_len)
                } else {
                    // New content is on same line as start
                    Point::new(start_point.row, start_point.column + last_line_len)
                };

                // Create InputEdit for incremental parsing
                let edit = InputEdit {
                    start_byte: start_offset,
                    old_end_byte: end_offset,
                    new_end_byte: new_end_offset,
                    start_position: start_point,
                    old_end_position: old_end_point,
                    new_end_position: new_end_point,
                };
                edits.push(edit);
            }

            // Replace the range with new text
            text.replace_range(start_offset..end_offset, &change.text);
        } else {
            // Full document change - no incremental parsing
            text = change.text;
            edits.clear(); // Clear any previous edits since it's a full replacement
            saw_full_replacement = true;
        }
    }

    // Authoritative guarantee: any batch containing a full replacement returns
    // empty edits (the full-reparse path). The skip above already prevents pushing
    // post-replacement edits, so this is normally a no-op — but keeping it as the
    // correctness mechanism means that optimization is *only* an optimization: a
    // future change to the loop can't silently leak a `replacement`-relative edit
    // (which would be unusable for both the `old_text` diff and the incremental
    // seed) into the returned vec.
    if saw_full_replacement {
        edits.clear();
    }

    AppliedContentChanges {
        text,
        input_edits: edits,
        sequential_byte_edits: (!saw_full_replacement).then_some(sequential_byte_edits),
    }
}

#[cfg(test)]
fn apply_content_changes_with_edits(
    old_text: &str,
    content_changes: Vec<TextDocumentContentChangeEvent>,
) -> (String, Vec<InputEdit>) {
    let result = apply_content_changes_detailed(old_text, content_changes);
    (result.text, result.input_edits)
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
    fn detailed_changes_preserve_sequential_intermediate_coordinates() {
        let changes = vec![
            TextDocumentContentChangeEvent {
                range: Some(Range::new(Position::new(0, 1), Position::new(0, 1))),
                range_length: Some(0),
                text: "XX".to_string(),
            },
            TextDocumentContentChangeEvent {
                range: Some(Range::new(Position::new(0, 4), Position::new(0, 5))),
                range_length: Some(1),
                text: "Y".to_string(),
            },
        ];

        let result = apply_content_changes_detailed("abcde", changes);

        assert_eq!(result.text, "aXXbYde");
        assert_eq!(
            result.sequential_byte_edits,
            Some(vec![
                SequentialByteEdit {
                    start_byte: 1,
                    old_end_byte: 1,
                    new_text: "XX".to_string(),
                },
                SequentialByteEdit {
                    start_byte: 4,
                    old_end_byte: 5,
                    new_text: "Y".to_string(),
                },
            ])
        );
    }

    #[test]
    fn detailed_changes_require_resync_after_any_full_replacement() {
        let changes = vec![
            TextDocumentContentChangeEvent {
                range: Some(Range::new(Position::new(0, 0), Position::new(0, 1))),
                range_length: Some(1),
                text: "A".to_string(),
            },
            TextDocumentContentChangeEvent {
                range: None,
                range_length: None,
                text: "replacement".to_string(),
            },
        ];

        let result = apply_content_changes_detailed("abc", changes);

        assert_eq!(result.text, "replacement");
        assert!(result.input_edits.is_empty());
        assert_eq!(result.sequential_byte_edits, None);
    }

    #[test]
    fn test_incremental_change_resolves_line_after_lone_cr() {
        let changes = vec![TextDocumentContentChangeEvent {
            range: Some(Range::new(Position::new(1, 0), Position::new(1, 5))),
            range_length: Some(5),
            text: "rust".to_string(),
        }];

        let (new_text, edits) = apply_content_changes_with_edits("hello\rworld", changes);
        let edit = edits.first().expect("one edit");

        assert_eq!(new_text, "hello\rrust");
        assert_eq!((edit.start_byte, edit.old_end_byte), (6, 11));
        // tree-sitter still sees the lone CR as a column byte, not a row break.
        assert_eq!(edit.start_position, Point::new(0, 6));
        assert_eq!(edit.old_end_position, Point::new(0, 11));
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
    fn test_overlong_column_does_not_consume_line_terminator() {
        let old_text = "hello\nworld\n";
        let changes = vec![TextDocumentContentChangeEvent {
            range: Some(Range {
                start: Position::new(0, 5),
                end: Position::new(0, 999),
            }),
            range_length: None,
            text: String::new(),
        }];

        let (new_text, edits) = apply_content_changes_with_edits(old_text, changes);

        assert_eq!(new_text, old_text);
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].start_byte, 5);
        assert_eq!(edits[0].old_end_byte, 5);
    }

    #[test]
    fn test_overlong_column_does_not_consume_crlf_terminator() {
        let old_text = "hello\r\nworld\r\n";
        let changes = vec![TextDocumentContentChangeEvent {
            range: Some(Range {
                start: Position::new(0, 5),
                end: Position::new(0, 999),
            }),
            range_length: None,
            text: String::new(),
        }];

        let (new_text, edits) = apply_content_changes_with_edits(old_text, changes);

        assert_eq!(new_text, old_text);
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].start_byte, 5);
        assert_eq!(edits[0].old_end_byte, 5);
    }

    #[test]
    fn test_out_of_range_edit_keeps_inputedit_byte_and_point_consistent() {
        // When the range is clamped, the InputEdit's tree-sitter points must be
        // derived from the clamped byte offsets, not the original out-of-bounds
        // LSP positions — otherwise byte and point disagree and incremental
        // parsing corrupts. Base "local x = {\n}\n" (14 bytes): line 1 is "}"
        // (one char), so character 2/3 are past it. Both ends clamp to byte 13,
        // immediately before the newline — NOT the next line's byte 14.
        let old_text = "local x = {\n}\n";
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

        let (_new_text, edits) = apply_content_changes_with_edits(old_text, changes);
        let edit = edits.first().expect("one edit");

        // Each point must match the row/col of its byte offset in the old text.
        let point_of = |byte: usize| {
            let prefix = &old_text[..byte];
            let row = prefix.bytes().filter(|&c| c == b'\n').count();
            let col = byte - prefix.rfind('\n').map(|p| p + 1).unwrap_or(0);
            (row, col)
        };
        assert_eq!(
            (edit.start_position.row, edit.start_position.column),
            point_of(edit.start_byte),
            "start_position must correspond to start_byte"
        );
        assert_eq!(
            (edit.old_end_position.row, edit.old_end_position.column),
            point_of(edit.old_end_byte),
            "old_end_position must correspond to old_end_byte"
        );
        // Concretely: both ends clamp to byte 13 → row 1, col 1.
        assert_eq!(edit.old_end_byte, 13);
        assert_eq!(
            (edit.old_end_position.row, edit.old_end_position.column),
            (1, 1)
        );
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
    fn test_apply_content_changes_full_then_incremental_yields_empty_edits() {
        // Regression: a full replacement FOLLOWED by a ranged change must still
        // signal a full sync (empty edits). The ranged edit is relative to the
        // replacement text, not `old_text`; returning it as a non-empty edit would
        // make the incremental seed apply it to the (old_text) tree — the #348
        // contract violation. Order matters: unlike the [incremental, full] case,
        // the full replacement is NOT the last change here.
        let old_text = "hello world";
        let changes = vec![
            // First: full document replacement (clears relationship to old_text)
            TextDocumentContentChangeEvent {
                range: None,
                range_length: None,
                text: "brand new body".to_string(),
            },
            // Second: incremental edit on the replacement text ("brand" -> "BRAND")
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
                text: "BRAND".to_string(),
            },
        ];

        let (new_text, edits) = apply_content_changes_with_edits(old_text, changes);

        // Text reflects both changes applied in order.
        assert_eq!(new_text, "BRAND new body");
        // But edits MUST be empty so the caller does a full reparse — the edits
        // cannot incrementally describe old_text -> new_text.
        assert!(
            edits.is_empty(),
            "a full replacement anywhere in the batch must force the full-sync path"
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
