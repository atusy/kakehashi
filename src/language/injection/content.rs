//! Reusable content primitives shared by the semantic-tokens and
//! selection-range paths: clean-content extraction, per-line column offsets,
//! parsing with included ranges, and byte→`Point` coordinate conversion.

use crate::text::clamped_slice;
use crate::text::terminal::escape_terminal_controls;

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
    // `clamped_slice` guards against byte offsets from a stale tree that no
    // longer matches `host_text` (out of bounds or mid-codepoint).
    let content = clamped_slice(host_text, byte_range);
    match included_ranges {
        None => content.to_string(),
        Some(ranges) => {
            // The output is a concatenation of sub-slices of `content`, so it
            // can't exceed `content.len()`. Cap the preallocation there (and sum
            // saturating) so stale, oversized ranges can't request a huge buffer.
            let capacity: usize = ranges
                .iter()
                .map(|r| r.end_byte.saturating_sub(r.start_byte))
                .fold(0usize, usize::saturating_add)
                .min(content.len());
            let mut result = String::with_capacity(capacity);
            for range in ranges {
                result.push_str(clamped_slice(content, range.start_byte..range.end_byte));
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
#[cfg(test)]
pub(super) fn compute_line_column_offsets(
    host_text: &str,
    byte_range: std::ops::Range<usize>,
    start_column: u32,
    included_ranges: Option<&[tree_sitter::Range]>,
) -> Vec<u32> {
    compute_line_prefixes(
        host_text,
        byte_range,
        start_column,
        included_ranges,
        |_, _| LinePrefix::Other,
    )
    .0
}

/// What the host text before a virtual line's content is made of: the text
/// a column offset skips. Only a markdown container's continuation (the
/// `> ` of a blockquote, a list item's indentation, as tree-sitter-md parses
/// them) repeats on every line of a block; anything else (the code before a
/// string that opens mid-line, an excluded escape sequence) is no container
/// prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LinePrefix {
    /// No text, or a container continuation of spaces and tabs only: list-item
    /// indentation.
    Whitespace,
    /// A container continuation holding markers (`>`).
    Container,
    /// Any other text.
    Other,
}

impl LinePrefix {
    /// The class of a container continuation spanning `text`.
    pub(super) fn continuation(text: &str) -> Self {
        if text.bytes().all(|byte| matches!(byte, b' ' | b'\t')) {
            Self::Whitespace
        } else {
            Self::Container
        }
    }
}

/// [`compute_line_column_offsets`], with what each line's prefix is made of,
/// as `classify` reads the host span before the line's gap: from the LSP
/// line start (`None` on the region's first line, whose start lies before
/// the region) to the gap's start byte. Gaps arrive in document order. Lines
/// without a recorded offset have an empty prefix.
pub(super) fn compute_line_prefixes(
    host_text: &str,
    byte_range: std::ops::Range<usize>,
    start_column: u32,
    included_ranges: Option<&[tree_sitter::Range]>,
    mut classify: impl FnMut(Option<usize>, usize) -> LinePrefix,
) -> (Vec<u32>, Vec<LinePrefix>) {
    match included_ranges {
        None => (vec![start_column], Vec::new()),
        Some(ranges) => {
            // Build a map from virtual line → column offset.
            // Each gap range starts at a known point with a column value.
            // The column value represents how many columns of prefix exist
            // before the actual code on that line.
            //
            // Lines are LSP lines, as the virtual document is read: a lone CR
            // ends one too (the markdown grammar ends a blockquote line there),
            // while tree-sitter rows split only at LF. So the lines and line
            // starts come from scanning the host bytes, not from the ranges'
            // Points. Gaps arrive in document order, so one forward scan serves
            // them all.
            let bytes = host_text.as_bytes();
            let mut scanned = byte_range.start;
            let mut line = 0usize;
            let mut line_start = byte_range.start;
            let mut advance_to = |to: usize, line: &mut usize, line_start: &mut usize| {
                let to = to.min(bytes.len());
                while scanned < to {
                    let byte = bytes[scanned];
                    scanned += 1;
                    if byte == b'\n' || (byte == b'\r' && bytes.get(scanned) != Some(&b'\n')) {
                        *line += 1;
                        *line_start = scanned;
                    }
                }
            };
            let mut offsets = Vec::new();
            let mut prefixes = Vec::new();
            for range in ranges {
                let gap_abs_byte = byte_range.start.saturating_add(range.start_byte);
                advance_to(gap_abs_byte, &mut line, &mut line_start);
                let gap_start_line = line;
                let prefix = classify((gap_start_line > 0).then_some(line_start), gap_abs_byte);
                // The column offset is the gap's start column within the host line.
                // For the first line of the content node, use start_column
                // (which accounts for the full prefix from the line start).
                // For subsequent lines, the prefix runs from the line start to
                // the gap — convert its bytes to UTF-16.
                let column = if gap_start_line == 0 {
                    start_column
                } else {
                    let prefix = clamped_slice(host_text, line_start..gap_abs_byte);
                    prefix.encode_utf16().count() as u32
                };
                advance_to(
                    byte_range.start.saturating_add(range.end_byte),
                    &mut line,
                    &mut line_start,
                );
                // Only the gap's start line carries a column offset; the rest of
                // its spanned lines stay at the default 0.
                if offsets.len() <= line {
                    offsets.resize(line + 1, 0);
                    prefixes.resize(line + 1, LinePrefix::Whitespace);
                }
                if let Some(slot) = offsets.get_mut(gap_start_line) {
                    *slot = column;
                    prefixes[gap_start_line] = prefix;
                }
            }
            if offsets.is_empty() {
                (vec![start_column], Vec::new())
            } else {
                (offsets, prefixes)
            }
        }
    }
}

/// The virtual lines of a region that are blank rows of a markdown
/// container block, which the edit checks protect.
///
/// A list item's fence leaves its blank rows unindented (or indented less
/// than its content), so virtual column 0 maps there to a host column short
/// of the item's indentation. Such a row is blank in `virtual_content`
/// (spaces and tabs only, as markdown counts it), and its nearest row that
/// holds text (before it, or after it when none is before) records a
/// container prefix wider than the row's own: none, or whitespace only and
/// shorter (a blank row keeping one space of a two-space item). A prefix of
/// other text (a string opening mid-line) is no
/// container, and a row recording no prefix that holds text (a lazy
/// continuation line) anchors nothing. The row after the content's final
/// line break is the closing fence's, which the edit checks treat apart.
/// Two linear passes over the rows.
pub(crate) fn protected_blank_rows(
    columns: &[u32],
    prefixes: &[LinePrefix],
    virtual_content: &str,
) -> Vec<u32> {
    if columns.len() <= 1 || !columns.iter().any(|&column| column > 0) {
        return Vec::new();
    }
    // Whether each LSP line (a lone CR ends one too, as the offsets count
    // them) is blank as markdown reads it.
    let mut blank = Vec::with_capacity(columns.len());
    let bytes = virtual_content.as_bytes();
    let mut line_blank = true;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\n' => {
                blank.push(line_blank);
                line_blank = true;
            }
            b'\r' => {
                blank.push(line_blank);
                line_blank = true;
                if bytes.get(i + 1) == Some(&b'\n') {
                    i += 1;
                }
            }
            b' ' | b'\t' => {}
            _ => line_blank = false,
        }
        i += 1;
    }
    // The row after the final line break is the closing fence's.
    if !virtual_content.ends_with(['\n', '\r']) {
        blank.push(line_blank);
    }
    let rows = columns.len().min(blank.len());
    let prefix = |row: usize| prefixes.get(row).copied().unwrap_or(LinePrefix::Whitespace);
    // A row holding text anchors the blank rows near it with the width of
    // its container prefix (0 when it has none).
    let anchor = |row: usize| {
        (!blank[row]).then(|| match prefix(row) {
            LinePrefix::Whitespace | LinePrefix::Container => columns[row],
            LinePrefix::Other => 0,
        })
    };
    let mut next_anchor = vec![None; rows];
    let mut next = None;
    for row in (0..rows).rev() {
        next_anchor[row] = next;
        if let Some(width) = anchor(row) {
            next = Some(width);
        }
    }
    let mut previous = None;
    let mut protected = Vec::new();
    for row in 0..rows {
        if blank[row] {
            let width = previous.or(next_anchor[row]).unwrap_or(0);
            // No prefix, or whitespace short of the container's: virtual
            // column 0 lands short of the block's content there.
            let short = columns[row] == 0
                || (prefix(row) == LinePrefix::Whitespace && columns[row] < width);
            if width > 0 && short {
                protected.push(row as u32);
            }
        }
        if let Some(width) = anchor(row) {
            previous = Some(width);
        }
    }
    protected
}

/// Per-parse wall-clock budget for every native parse that runs on the
/// bounded compute pool — the single source of truth: the parse
/// coordinator's `PARSE_TIMEOUT` is defined as this constant, so host and
/// injected-layer parses cannot silently drift to different budgets.
pub(crate) const NATIVE_PARSE_BUDGET: std::time::Duration = std::time::Duration::from_secs(10);

/// Parse with a wall-clock abort wired into tree-sitter's progress callback.
///
/// Every native parse in the server runs as (part of) a work-unit on the
/// bounded compute pool, where an unbounded `ts_parser_parse` call pins a
/// thread for as long as a pathological parse takes — and the pool is sized
/// as low as ONE thread on small hosts, where a single pin stalls every
/// document's tree-CPU server-wide. An async-side `tokio::time::timeout`
/// cannot help (it only abandons the awaiter); breaking out of the progress
/// callback makes the parse itself return shortly after the deadline,
/// reclaiming the thread. An aborted parser is `reset()` so it carries no
/// half-parse state back into a parser pool or thread-local cache.
pub(crate) fn parse_with_deadline(
    parser: &mut tree_sitter::Parser,
    text: &str,
    old_tree: Option<&tree_sitter::Tree>,
    deadline: std::time::Instant,
) -> Option<tree_sitter::Tree> {
    parse_with_deadline_cancellable(parser, text, old_tree, deadline, None)
}

/// [`parse_with_deadline`] with a version-scoped cooperative cancellation hook.
pub(crate) fn parse_with_deadline_cancellable(
    parser: &mut tree_sitter::Parser,
    text: &str,
    old_tree: Option<&tree_sitter::Tree>,
    deadline: std::time::Instant,
    cancel: Option<&crate::cancel::CancelToken>,
) -> Option<tree_sitter::Tree> {
    if crate::cancel::is_cancelled(cancel) {
        parser.reset();
        return None;
    }
    let bytes = text.as_bytes();
    // The callback fires at tree-sitter's internal progress intervals, which
    // can be very frequent on large inputs; sample the clock every 128
    // invocations so the steady-state cost is a counter increment, not a
    // syscall. Worst-case abort delay stays far below the whole-parse
    // budget's scale.
    let mut calls: u32 = 0;
    let mut on_progress = |_: &tree_sitter::ParseState| {
        calls = calls.wrapping_add(1);
        let cancelled = calls.is_multiple_of(64) && crate::cancel::is_cancelled(cancel);
        let timed_out = calls.is_multiple_of(128) && std::time::Instant::now() >= deadline;
        if cancelled || timed_out {
            std::ops::ControlFlow::Break(())
        } else {
            std::ops::ControlFlow::Continue(())
        }
    };
    let result = parser.parse_with_options(
        &mut |i, _| bytes.get(i..).unwrap_or(&[]),
        old_tree,
        Some(tree_sitter::ParseOptions::new().progress_callback(&mut on_progress)),
    );
    if result.is_none() {
        parser.reset();
    }
    result
}

/// Parse text with optional included ranges, resetting parser state afterward.
///
/// Shared protocol for Tree-sitter's `set_included_ranges` API; both semantic
/// tokens and selection range paths delegate here to avoid divergence. The reset
/// to `&[]` is unconditional (even on the failure path) so parsers returned to
/// pools or caches never carry stale included-range state. The parse itself is
/// bounded by [`NATIVE_PARSE_BUDGET`] via [`parse_with_deadline`]: injected
/// re-parses run on the bounded compute pool too, where an unbounded parse
/// would pin a thread.
pub(crate) fn parse_with_ranges(
    parser: &mut tree_sitter::Parser,
    text: &str,
    included_ranges: Option<&[tree_sitter::Range]>,
    log_target: &str,
    lang_name: &str,
) -> Option<tree_sitter::Tree> {
    // Tree-sitter treats an empty included-range slice like a reset to the
    // default whole-document range.  Here it means child/parent exclusions
    // removed every byte, so there is intentionally nothing to parse.
    if included_ranges.is_some_and(<[tree_sitter::Range]>::is_empty) {
        let _ = parser.set_included_ranges(&[]);
        return None;
    }

    if let Some(ranges) = included_ranges
        && let Err(e) = parser.set_included_ranges(ranges)
    {
        log::warn!(
            target: log_target,
            "Failed to set included ranges for {}: {}. Skipping parse.",
            escape_terminal_controls(lang_name), e
        );
        let _ = parser.set_included_ranges(&[]);
        return None;
    }

    let deadline = std::time::Instant::now() + NATIVE_PARSE_BUDGET;
    let tree = parse_with_deadline(parser, text, None, deadline);
    if tree.is_none() {
        log::warn!(
            target: log_target,
            "Injected parse for {} yielded no tree (aborted at the {}s budget or failed)",
            escape_terminal_controls(lang_name),
            NATIVE_PARSE_BUDGET.as_secs()
        );
    }

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
    let clamped = text.floor_char_boundary(byte);
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
/// typically a tree-sitter node's cached `start_position()`. The anchored fast
/// path is taken only when `anchor_byte` is an in-bounds char boundary and
/// `anchor_point` is its Point in `text`; otherwise — `byte` before the anchor
/// (e.g. a negative `#offset!`), or a stale anchor that's out of bounds or
/// mid-codepoint — it falls back to the full scan, which trusts neither.
pub(crate) fn byte_to_point_anchored(
    text: &str,
    byte: usize,
    anchor_byte: usize,
    anchor_point: tree_sitter::Point,
) -> tree_sitter::Point {
    let clamped = text.floor_char_boundary(byte);
    // Reuse the cached `anchor_point` only when `anchor_byte` is a real position
    // in the current text: if it's past the target, out of bounds, or
    // mid-codepoint (a stale tree), `anchor_point` no longer matches it and the
    // `text[anchor_byte..clamped]` slice could panic — fall back to a full scan,
    // which depends on neither.
    if clamped < anchor_byte || !text.is_char_boundary(anchor_byte) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::language::injection::{collect_all_injections, compute_included_ranges};
    use tree_sitter::{Parser, Query, StreamingIterator};

    #[test]
    fn native_parse_stops_for_cancelled_version() {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        let cancel = crate::cancel::CancelToken::default();
        cancel.cancel();

        let parsed = parse_with_deadline_cancellable(
            &mut parser,
            "fn main() {}",
            None,
            std::time::Instant::now() + std::time::Duration::from_secs(60),
            Some(&cancel),
        );

        assert!(parsed.is_none(), "obsolete parse must stop without a tree");
    }

    /// Helper to set up a markdown parser and injection query, parse text,
    /// and return the first injection's content node byte range and included ranges.
    fn blockquote_injection_data(
        text: &str,
    ) -> (std::ops::Range<usize>, u32, Option<Vec<tree_sitter::Range>>) {
        let mut parser = Parser::new();
        let md_language: tree_sitter::Language = tree_sitter_md::LANGUAGE.into();
        parser
            .set_language(&md_language)
            .expect("load markdown grammar");

        let injection_query_str = r#"
            (fenced_code_block
              (info_string (language) @injection.language)
              (code_fence_content) @injection.content)
        "#;
        let injection_query =
            Query::new(&md_language, injection_query_str).expect("valid injection query");

        let tree = parser.parse(text, None).expect("parse markdown");
        let root = tree.root_node();

        let injections = collect_all_injections(&root, text, Some(&injection_query))
            .expect("Should find injections");
        assert!(!injections.is_empty(), "Should find at least one injection");

        let region = &injections[0];
        let byte_range = region.content_node.byte_range();
        let included_ranges =
            compute_included_ranges(&region.content_node, region.include_children);

        // Compute start_column (UTF-16)
        let byte_column = region.content_node.start_position().column;
        let line_start_byte = region.content_node.start_byte() - byte_column;
        let line_prefix = &text[line_start_byte..region.content_node.start_byte()];
        let start_column = line_prefix.encode_utf16().count() as u32;

        (byte_range, start_column, included_ranges)
    }

    #[test]
    fn test_byte_to_point_anchored_matches_full_scan() {
        // Exhaustive equivalence with the full-prefix scan, covering the
        // before-anchor fallback, same-row and cross-row targets, and
        // mid-codepoint clamping (bytes inside あ floor to its start).
        let text = "abc\ndefあ\nghi";
        let anchor_byte = 4; // start of "def"
        let anchor_point = byte_to_point(text, anchor_byte);
        for byte in 0..=text.len() {
            assert_eq!(
                byte_to_point_anchored(text, byte, anchor_byte, anchor_point),
                byte_to_point(text, byte),
                "anchored and full scans must agree at byte {byte}"
            );
        }
    }

    #[test]
    fn test_blockquote_bridge_content_strips_prefixes() {
        // Verify that extract_clean_content strips blockquote prefixes so that
        // downstream language servers receive parseable code.
        let text = "> ```lua\n> local x = 1\n> local y = 2\n> ```\n";
        let (byte_range, _start_column, included_ranges) = blockquote_injection_data(text);

        let clean = extract_clean_content(text, byte_range, included_ranges.as_deref());
        assert!(
            !clean.contains("> "),
            "Clean content should NOT contain blockquote prefixes: {:?}",
            clean
        );
        assert!(
            clean.contains("local x = 1"),
            "Clean content should contain the code: {:?}",
            clean
        );
        assert!(
            clean.contains("local y = 2"),
            "Clean content should contain the code: {:?}",
            clean
        );
    }

    #[test]
    fn extract_clean_content_without_ranges_returns_full_text() {
        // When called with None for included_ranges, returns the full content slice
        let text = "hello world";
        let clean = extract_clean_content(text, 0..text.len(), None);
        assert_eq!(clean, text);
    }

    #[test]
    fn extract_clean_content_with_ranges_strips_prefixes() {
        // Blockquote case: included_ranges present → only gap content
        let text = "> ```lua\n> local x = 1\n> local y = 2\n> ```\n";
        let (byte_range, _start_column, included_ranges) = blockquote_injection_data(text);
        assert!(
            included_ranges.is_some(),
            "Blockquote should have included ranges"
        );

        let clean = extract_clean_content(text, byte_range, included_ranges.as_deref());
        assert!(
            !clean.contains("> "),
            "Clean content should not contain blockquote prefixes: {:?}",
            clean
        );
        assert!(
            clean.contains("local x = 1"),
            "Clean content should contain the code: {:?}",
            clean
        );
        assert!(
            clean.contains("local y = 2"),
            "Clean content should contain the code: {:?}",
            clean
        );
    }

    #[test]
    fn compute_line_column_offsets_without_ranges_returns_start_column() {
        // When called with None for included_ranges, returns vec![start_column]
        let offsets = compute_line_column_offsets("hello", 0..5, 4, None);
        assert_eq!(offsets, vec![4]);
    }

    #[test]
    fn compute_line_column_offsets_with_ranges_returns_per_line() {
        // Blockquote case: "> " prefix on each line → 2 UTF-16 units per line
        let text = "> ```lua\n> local x = 1\n> local y = 2\n> ```\n";
        let (byte_range, start_column, included_ranges) = blockquote_injection_data(text);
        assert!(included_ranges.is_some());

        let offsets =
            compute_line_column_offsets(text, byte_range, start_column, included_ranges.as_deref());
        // Virtual lines 0 and 1 contain actual code after "> " prefixes.
        // Line 2 may be trailing content (the "> " before "```") — its offset
        // is 0 because there's no gap range starting on that line.
        assert!(
            offsets.len() >= 2,
            "Should have at least 2 line offsets: {:?}",
            offsets
        );
        assert_eq!(
            offsets[0], 2,
            "Line 0 should have offset 2 (for '> ' prefix)"
        );
        assert_eq!(
            offsets[1], 2,
            "Line 1 should have offset 2 (for '> ' prefix)"
        );
    }

    #[test]
    fn compute_line_column_offsets_counts_a_lone_cr_as_a_line_end() {
        // A lone CR ends an LSP line, and the markdown grammar ends a
        // blockquote line there too, but tree-sitter rows split only at LF.
        // Offsets are read per LSP line of the virtual document, so every
        // line after the lone CR must still carry its "> " prefix.
        let text = "> ```lua\n> local x = 1\r> local y = 2\n> local z = 3\n> ```\n";
        let (byte_range, start_column, included_ranges) = blockquote_injection_data(text);

        let offsets =
            compute_line_column_offsets(text, byte_range, start_column, included_ranges.as_deref());

        assert_eq!(offsets[..3], [2, 2, 2], "{offsets:?}");
    }

    #[test]
    fn compute_line_column_offsets_converts_non_ascii_prefix_to_utf16() {
        // The offsets contract is UTF-16 code units. A fullwidth prefix
        // "\u{ff1e} " is 4 UTF-8 bytes but 2 UTF-16 units — a byte-derived
        // width would mint 4 here and shift every translated column. Ranges
        // carry raw BYTE columns (tree_sitter::Point), so this pins the
        // byte→UTF-16 conversion at the mint site.
        //
        // Text: "\u{ff1e} let x = 1;\n\u{ff1e} let y = 2;\n"
        //   line 0: bytes  0..15 (prefix 0..4,  code 4..14,  \n at 14)
        //   line 1: bytes 15..30 (prefix 15..19, code 19..29, \n at 29)
        let text = "\u{ff1e} let x = 1;\n\u{ff1e} let y = 2;\n";
        assert_eq!(&text[15..19], "\u{ff1e} ");
        let included_ranges = vec![
            tree_sitter::Range {
                start_byte: 4,
                end_byte: 15,
                start_point: tree_sitter::Point { row: 0, column: 4 },
                end_point: tree_sitter::Point { row: 0, column: 15 },
            },
            tree_sitter::Range {
                start_byte: 19,
                end_byte: 30,
                start_point: tree_sitter::Point { row: 1, column: 4 },
                end_point: tree_sitter::Point { row: 1, column: 15 },
            },
        ];

        // start_column mirrors production (already UTF-16, content.rs mints it
        // via encode_utf16 from the line-0 prefix).
        let offsets = compute_line_column_offsets(text, 0..30, 2, Some(&included_ranges));

        // A third, zero slot covers the line the second range's newline
        // ends on.
        assert_eq!(
            offsets[..2],
            [2, 2],
            "line 1 must mint the UTF-16 width (2), not the 4-byte width"
        );
    }

    /// Tree-sitter assigns column positions from Range.start_point, not byte offset.
    ///
    /// When `set_included_ranges` is called with ranges whose `start_point.column = 2`,
    /// tree-sitter reports parsed nodes at column 2 — not column 0 — even though the
    /// bytes start at offset 2 within the raw text.
    ///
    /// This is the invariant that makes blockquote injection work correctly:
    /// `compute_included_ranges` builds ranges with `start_point.column = prefix_len`
    /// (e.g. 2 for `> `), so injected keywords appear at their true host column.
    #[test]
    fn test_parse_with_included_ranges_preserves_start_point_column() {
        // Simulate "> let x = 1;\n> let y = 2;\n" with included_ranges skipping `> `
        // Line 0: "> let x = 1;\n" = 13 bytes (0..13, \n at byte 12)
        // Line 1: "> let y = 2;\n" = 13 bytes (13..26, \n at byte 25)
        // Content ranges (skipping 2-byte `> ` prefix on each line):
        //   Range 0: bytes  2..13 = "let x = 1;\n", start_point {row:0, col:2}
        //   Range 1: bytes 15..26 = "let y = 2;\n", start_point {row:1, col:2}
        let content_text = "> let x = 1;\n> let y = 2;\n";
        let included_ranges = vec![
            tree_sitter::Range {
                start_byte: 2,
                end_byte: 13,
                start_point: tree_sitter::Point { row: 0, column: 2 },
                end_point: tree_sitter::Point { row: 1, column: 0 },
            },
            tree_sitter::Range {
                start_byte: 15,
                end_byte: 26,
                start_point: tree_sitter::Point { row: 1, column: 2 },
                end_point: tree_sitter::Point { row: 2, column: 0 },
            },
        ];

        let rust_language: tree_sitter::Language = tree_sitter_rust::LANGUAGE.into();
        let mut parser = Parser::new();
        parser.set_language(&rust_language).unwrap();
        let tree = parse_with_ranges(
            &mut parser,
            content_text,
            Some(&included_ranges),
            "test",
            "rust",
        )
        .expect("should parse");

        let query = Query::new(&rust_language, "(let_declaration) @decl").unwrap();
        let mut cursor = tree_sitter::QueryCursor::new();
        let mut matches = cursor.matches(&query, tree.root_node(), content_text.as_bytes());

        let mut let_columns: Vec<usize> = Vec::new();
        while let Some(m) = matches.next() {
            for c in m.captures() {
                let node = c.node;
                let mut walk = node.walk();
                for child in node.children(&mut walk) {
                    if child.kind() == "let" {
                        let_columns.push(child.start_position().column);
                    }
                }
            }
        }

        assert_eq!(
            let_columns,
            vec![2, 2],
            "Both 'let' keywords should be at column 2 (from Range.start_point), not 0"
        );
    }

    #[test]
    fn parse_with_empty_included_ranges_skips_instead_of_parsing_everything() {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        parser
            .set_included_ranges(&[tree_sitter::Range {
                start_byte: 0,
                end_byte: 1,
                start_point: tree_sitter::Point::new(0, 0),
                end_point: tree_sitter::Point::new(0, 1),
            }])
            .unwrap();

        assert!(
            parse_with_ranges(&mut parser, "fn main() {}", Some(&[]), "test", "rust").is_none()
        );
        let reparsed = parser
            .parse("fn main() {}", None)
            .expect("empty-range skip must leave the parser unrestricted");
        assert_eq!(reparsed.root_node().byte_range(), 0..12);
    }

    // --- stale-tree hardening: byte offsets that no longer match `text` must
    // degrade gracefully instead of panicking (#401). ---

    fn range_at(
        start_byte: usize,
        end_byte: usize,
        row: usize,
        column: usize,
    ) -> tree_sitter::Range {
        tree_sitter::Range {
            start_byte,
            end_byte,
            start_point: tree_sitter::Point { row, column },
            end_point: tree_sitter::Point { row, column },
        }
    }

    #[test]
    fn extract_clean_content_out_of_bounds_does_not_panic() {
        assert_eq!(extract_clean_content("hi", 0..100, None), "hi");
        assert_eq!(extract_clean_content("hi", 50..100, None), "");
        // Included sub-ranges past the end of the (clamped) content are skipped.
        let oversized = [range_at(0, 999, 0, 0)];
        assert_eq!(extract_clean_content("hi", 0..2, Some(&oversized)), "hi");
    }

    /// Splitting each region on one long host line into lines takes time
    /// linear in the regions, not in the line before each: nothing reads
    /// back to the line start. Scanning back for each of the 20k regions
    /// below would take seconds (about 2e9 byte checks); the bound leaves a
    /// wide margin for a loaded machine.
    #[test]
    fn regions_on_one_long_line_classify_their_prefixes_in_linear_time() {
        const REGIONS: usize = 20_000;
        let host = "f`ab`, ".repeat(REGIONS);
        let gap = tree_sitter::Range {
            start_byte: 0,
            end_byte: 2,
            start_point: tree_sitter::Point::new(0, 0),
            end_point: tree_sitter::Point::new(0, 2),
        };
        let started = std::time::Instant::now();
        for region in 0..REGIONS {
            let start = region * 7 + 2;
            let (offsets, prefixes) = compute_line_prefixes(
                &host,
                start..start + 2,
                start as u32,
                Some(std::slice::from_ref(&gap)),
                |_, _| LinePrefix::Other,
            );
            assert_eq!((offsets.len(), prefixes[0]), (1, LinePrefix::Other));
        }
        let elapsed = started.elapsed();
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "took {elapsed:?}"
        );
    }

    #[test]
    fn compute_line_column_offsets_oversized_column_does_not_panic() {
        // A stale range whose Point claims a later row and an oversized
        // column must not underflow or slice past the end. Lines and line
        // starts come from the bytes, so the range sits on line 0.
        let ranges = [range_at(2, 3, 1, 100)];
        let offsets = compute_line_column_offsets("hello", 0..5, 0, Some(&ranges));
        assert_eq!(offsets, vec![0]);
    }

    #[test]
    fn byte_to_point_anchored_stale_anchor_does_not_panic() {
        // "あ" is 3 bytes; an anchor landing mid-codepoint (byte 1) or past the
        // end must not panic the `text[anchor..clamped]` slice.
        let p = byte_to_point_anchored("あ", 3, 1, tree_sitter::Point { row: 0, column: 0 });
        assert_eq!(p.row, 0);
        let _ = byte_to_point_anchored("あ", 99, 50, tree_sitter::Point { row: 0, column: 0 });
    }
}
