//! Text manipulation utilities.
//!
//! This module provides utilities for working with text content:
//! - Position mapping between LSP (UTF-16) and byte offsets
//! - Content hashing for caching

mod char_boundary;
pub(crate) mod edit;
mod hash;
pub(crate) mod position;
pub(crate) mod terminal;

pub(crate) use char_boundary::clamped_slice;
pub(crate) use hash::{Fnv1aWriter, fnv1a_hash};
pub(crate) use position::PositionMapper;
