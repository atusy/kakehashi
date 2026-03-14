//! Shared test helpers for E2E tests.
//!
//! Note: We use `helpers/mod.rs` instead of the modern `helpers.rs` + `helpers/` pattern
//! because Cargo auto-discovers top-level `.rs` files in `tests/` as integration tests.
//! A `tests/helpers.rs` file would be compiled as a standalone test, which we don't want.

pub mod lsp_client;
pub mod lsp_polling;
pub mod lua_bridge;
pub mod sanitization;
pub mod test_fixtures;
pub mod text;
