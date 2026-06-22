//! Text manipulation utilities.
//!
//! This module provides utilities for working with text content:
//! - Position mapping between LSP (UTF-16) and byte offsets
//! - Content hashing for caching

mod char_boundary;
pub(crate) mod edit;
mod hash;
pub(crate) mod position;

pub(crate) use char_boundary::{ceil_char_boundary, clamped_slice, floor_char_boundary};
pub(crate) use hash::fnv1a_hash;
pub(crate) use position::PositionMapper;
