//! Text manipulation utilities.
//!
//! This module provides utilities for working with text content:
//! - Position mapping between LSP (UTF-16) and byte offsets
//! - Content hashing for caching

mod char_boundary;
pub(crate) mod edge_layout;
pub(crate) mod edit;
mod hash;
mod lone_cr;
pub(crate) mod position;
pub(crate) mod terminal;

pub(crate) use char_boundary::clamped_slice;
pub(crate) use hash::{Fnv1aWriter, fnv1a_hash};
pub(crate) use lone_cr::LoneCarriageReturns;
pub(crate) use position::PositionMapper;

/// Whether `text` is blank as markdown reads a line: nothing but spaces,
/// tabs and line breaks. Other Unicode whitespace (a no-break space, a
/// vertical tab, a form feed) is text there, unlike `char::is_whitespace`.
pub(crate) fn is_markdown_blank(text: &str) -> bool {
    text.bytes()
        .all(|byte| matches!(byte, b' ' | b'\t' | b'\n' | b'\r'))
}
