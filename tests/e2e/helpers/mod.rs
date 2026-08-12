//! Shared test helpers for E2E tests, declared once by `tests/e2e/main.rs`
//! and reached from the test modules as `crate::helpers::*`.

pub mod lsp_client;
pub mod lsp_polling;
pub mod lua_bridge;
pub mod sanitization;
pub mod test_fixtures;
pub mod text;
