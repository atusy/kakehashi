//! Stable substitutes for `str::ceil_char_boundary` / `str::floor_char_boundary`.
//!
//! The std methods are only stable since Rust 1.95; these hand-rolled versions
//! keep the crate's MSRV from being raised. They snap an arbitrary byte index
//! to a UTF-8 char boundary so slicing at it never panics on a mid-codepoint or
//! out-of-range offset from a buggy downstream server.

/// Snap `index` forward to the nearest char boundary. Hand-rolled substitute
/// for the MSRV-gated `str::ceil_char_boundary` (see module docs).
pub(crate) fn ceil_char_boundary(text: &str, mut index: usize) -> usize {
    while index < text.len() && !text.is_char_boundary(index) {
        index += 1;
    }
    index.min(text.len())
}

/// Snap `index` backward to the nearest char boundary. Hand-rolled substitute
/// for the MSRV-gated `str::floor_char_boundary` (see module docs).
pub(crate) fn floor_char_boundary(text: &str, mut index: usize) -> usize {
    index = index.min(text.len());
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

/// Slice `text` by a byte range without ever panicking.
///
/// Byte offsets that come from a tree-sitter node are only guaranteed valid for
/// the exact text the tree was parsed from; if the tree and text desync (a stale
/// tree after a concurrent edit), an offset can be out of bounds or land
/// mid-codepoint, and `&text[range]` would panic and crash the LSP server. The
/// start is ceiled and the end is floored to UTF-8 char boundaries (both clamped
/// to `text.len()`) — so the result is always a subset of the requested bytes,
/// never spilling into an adjacent codepoint — and a degenerate or inverted
/// range (after snapping, `start >= end`) yields `""`. For a range that is
/// already valid this returns exactly `&text[range]`.
pub(crate) fn clamped_slice(text: &str, range: std::ops::Range<usize>) -> &str {
    let start = ceil_char_boundary(text, range.start);
    let end = floor_char_boundary(text, range.end);
    if start >= end {
        return "";
    }
    text.get(start..end).unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamped_slice_valid_range_is_unchanged() {
        assert_eq!(clamped_slice("hello world", 0..5), "hello");
        assert_eq!(clamped_slice("hello world", 6..11), "world");
    }

    #[test]
    fn clamped_slice_out_of_bounds_does_not_panic() {
        // A stale tree can report offsets past the end of the current text.
        assert_eq!(clamped_slice("hi", 0..100), "hi");
        assert_eq!(clamped_slice("hi", 50..100), "");
    }

    #[test]
    fn clamped_slice_mid_codepoint_snaps_inward() {
        // "あ"/"い" are 3 bytes each (0..3, 3..6); slicing at 1, 2, 4, 5 would
        // panic with `&text[..]`. Snapping inward (ceil start / floor end) keeps
        // the result a subset of the requested bytes.
        let s = "あい";
        assert_eq!(clamped_slice(s, 0..2), ""); // ceil 0->0, floor 2->0 => empty
        assert_eq!(clamped_slice(s, 1..6), "い"); // ceil 1->3, floor 6->6
        assert_eq!(clamped_slice(s, 0..4), "あ"); // ceil 0->0, floor 4->3
    }

    #[test]
    fn clamped_slice_inverted_range_yields_empty() {
        // Build the range from values so it isn't a literal reversed range.
        let (start, end) = (4usize, 2usize);
        assert_eq!(clamped_slice("hello", start..end), "");
        // An inverted multibyte range must not snap into a non-empty slice
        // (e.g. 2..1 -> 0..3 would wrongly yield "あ").
        let (ms, me) = (2usize, 1usize);
        assert_eq!(clamped_slice("あい", ms..me), "");
    }
}
