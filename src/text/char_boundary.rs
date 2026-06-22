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
/// start is floored and the end is ceiled to UTF-8 char boundaries (both clamped
/// to `text.len()`), and a degenerate (start > end) range yields `""`. For a
/// range that is already valid this returns exactly `&text[range]`.
pub(crate) fn clamped_slice(text: &str, range: std::ops::Range<usize>) -> &str {
    let start = floor_char_boundary(text, range.start);
    let end = ceil_char_boundary(text, range.end);
    text.get(start..end.max(start)).unwrap_or("")
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
    fn clamped_slice_mid_codepoint_floors_to_boundary() {
        // "あ" is 3 bytes (0..3); slicing at 1 or 2 would panic with `&text[..]`.
        let s = "あい";
        assert_eq!(clamped_slice(s, 0..2), "あ"); // end ceiled 2 -> 3
        assert_eq!(clamped_slice(s, 1..6), "あい"); // start floored 1 -> 0
    }

    #[test]
    fn clamped_slice_inverted_range_yields_empty() {
        // Build the range from values so it isn't a literal reversed range.
        let (start, end) = (4usize, 2usize);
        assert_eq!(clamped_slice("hello", start..end), "");
    }
}
