//! Reusable content primitives shared by the semantic-tokens and
//! selection-range paths: clean-content extraction, per-line column offsets,
//! parsing with included ranges, and byte→`Point` coordinate conversion.

use crate::text::floor_char_boundary;

/// Extract clean injection content, stripping child node bytes when `included_ranges` present.
///
/// When `included_ranges` is `Some`, only the gap bytes (actual code content) are
/// concatenated, effectively stripping blockquote prefixes like `> `. When `None`,
/// returns the full content slice as-is.
pub(crate) fn extract_clean_content(
    host_text: &str,
    byte_range: std::ops::Range<usize>,
    included_ranges: Option<&[tree_sitter::Range]>,
) -> String {
    let content = &host_text[byte_range.clone()];
    match included_ranges {
        None => content.to_string(),
        Some(ranges) => {
            let capacity: usize = ranges.iter().map(|r| r.end_byte - r.start_byte).sum();
            let mut result = String::with_capacity(capacity);
            for range in ranges {
                result.push_str(&content[range.start_byte..range.end_byte]);
            }
            result
        }
    }
}

/// Compute per-virtual-line UTF-16 column offsets from included ranges.
///
/// Each offset is the width of the blockquote prefix (or other excluded content)
/// before the gap on that line. When `included_ranges` is `None`, returns
/// `vec![start_column]` — the non-blockquote fallback where only line 0 has an offset.
pub(crate) fn compute_line_column_offsets(
    host_text: &str,
    byte_range: std::ops::Range<usize>,
    start_column: u32,
    included_ranges: Option<&[tree_sitter::Range]>,
) -> Vec<u32> {
    match included_ranges {
        None => vec![start_column],
        Some(ranges) => {
            // Build a map from virtual line → column offset.
            // Each gap range starts at a known point with a column value.
            // The column value represents how many columns of prefix exist
            // before the actual code on that line.
            let mut offsets = Vec::new();
            for range in ranges {
                let gap_start_line = range.start_point.row;
                // How many virtual lines does this gap span?
                let gap_end_line = range.end_point.row;
                for line in gap_start_line..=gap_end_line {
                    offsets.resize(line + 1, 0);
                    if line == gap_start_line {
                        // The column offset is the gap's start column within the host line.
                        // For the first line (row 0) of the content node, use start_column
                        // (which accounts for the full prefix from the line start).
                        // For subsequent lines, the start_point.column from
                        // compute_included_ranges is already the raw byte column
                        // within the line — convert to UTF-16.
                        if line == 0 {
                            offsets[line] = start_column;
                        } else {
                            // Convert byte column to UTF-16 code units.
                            // The gap's start_byte is relative to content node start.
                            // The line starts at the byte after the previous newline.
                            let gap_abs_byte = byte_range.start + range.start_byte;
                            let line_start_byte = gap_abs_byte - range.start_point.column;
                            let prefix = &host_text[line_start_byte..gap_abs_byte];
                            offsets[line] = prefix.encode_utf16().count() as u32;
                        }
                    }
                }
            }
            if offsets.is_empty() {
                vec![start_column]
            } else {
                offsets
            }
        }
    }
}

/// Parse text with optional included ranges, resetting parser state afterward.
///
/// Shared protocol for Tree-sitter's `set_included_ranges` API; both semantic
/// tokens and selection range paths delegate here to avoid divergence. The reset
/// to `&[]` is unconditional (even on the failure path) so parsers returned to
/// pools or caches never carry stale included-range state.
pub(crate) fn parse_with_ranges(
    parser: &mut tree_sitter::Parser,
    text: &str,
    included_ranges: Option<&[tree_sitter::Range]>,
    log_target: &str,
    lang_name: &str,
) -> Option<tree_sitter::Tree> {
    if let Some(ranges) = included_ranges
        && let Err(e) = parser.set_included_ranges(ranges)
    {
        log::warn!(
            target: log_target,
            "Failed to set included ranges for {}: {}. Skipping parse.",
            lang_name, e
        );
        let _ = parser.set_included_ranges(&[]);
        return None;
    }

    let tree = parser.parse(text, None);

    // Always reset included ranges after parsing to prevent stale state
    let _ = parser.set_included_ranges(&[]);

    tree
}

/// Convert an absolute byte offset to a `tree_sitter::Point` (byte-based
/// column). Used when an offset directive shifts the injection boundary away
/// from a known node position, so we can't reuse the content node's
/// start/end points.
pub(crate) fn byte_to_point(text: &str, byte: usize) -> tree_sitter::Point {
    // Align first — slicing `&text[..clamped]` on a mid-character byte would
    // panic and crash the LSP server.
    let clamped = floor_char_boundary(text, byte);
    let prefix = &text[..clamped];
    let row = prefix.bytes().filter(|b| *b == b'\n').count();
    let last_nl = prefix.rfind('\n');
    let column = match last_nl {
        Some(idx) => clamped - idx - 1,
        None => clamped,
    };
    tree_sitter::Point { row, column }
}

/// Like [`byte_to_point`], but scans only the span between a known anchor
/// and `byte` instead of the whole prefix of `text` — the anchor is
/// typically a tree-sitter node's cached `start_position()`. Falls back to a
/// full scan when `byte` precedes the anchor (e.g. a negative `#offset!`
/// extending before the content node). `anchor_byte` must lie on a char
/// boundary, with `anchor_point` its Point in `text`.
pub(crate) fn byte_to_point_anchored(
    text: &str,
    byte: usize,
    anchor_byte: usize,
    anchor_point: tree_sitter::Point,
) -> tree_sitter::Point {
    let clamped = floor_char_boundary(text, byte);
    if clamped < anchor_byte {
        return byte_to_point(text, clamped);
    }
    let segment = &text[anchor_byte..clamped];
    match segment.rfind('\n') {
        Some(last_nl) => tree_sitter::Point {
            row: anchor_point.row + segment.bytes().filter(|b| *b == b'\n').count(),
            column: segment.len() - last_nl - 1,
        },
        None => tree_sitter::Point {
            row: anchor_point.row,
            column: anchor_point.column + segment.len(),
        },
    }
}
