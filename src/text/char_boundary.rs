//! Stable substitutes for `str::ceil_char_boundary` / `str::floor_char_boundary`.
//!
//! The std methods are only stable since Rust 1.95; these hand-rolled versions
//! keep the crate's MSRV from being raised. They snap an arbitrary byte index
//! to a UTF-8 char boundary so slicing at it never panics on a mid-codepoint or
//! out-of-range offset from a buggy downstream server.

/// Snap `index` forward to the nearest char boundary (stable alternative to
/// the unstable `str::ceil_char_boundary`).
pub(crate) fn ceil_char_boundary(text: &str, mut index: usize) -> usize {
    while index < text.len() && !text.is_char_boundary(index) {
        index += 1;
    }
    index.min(text.len())
}

/// Snap `index` backward to the nearest char boundary (stable alternative to
/// the unstable `str::floor_char_boundary`).
pub(crate) fn floor_char_boundary(text: &str, mut index: usize) -> usize {
    index = index.min(text.len());
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}
