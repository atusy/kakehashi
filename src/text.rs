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

#[cfg(test)]
thread_local! {
    /// Rows inspected by per-row region scans on this thread, so tests can
    /// bound the work a scan does.
    pub(crate) static ROWS_SCANNED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Count one row inspected by a per-row region scan (tests only).
#[inline]
pub(crate) fn count_row_scanned() {
    #[cfg(test)]
    ROWS_SCANNED.with(|rows| rows.set(rows.get() + 1));
}
