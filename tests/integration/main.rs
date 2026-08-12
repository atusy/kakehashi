//! Merged integration test binary (no `e2e` feature required): CLI-level
//! tests that drive the real `kakehashi` binary but spawn no language
//! servers. Merged for the same link-once reason as tests/e2e/main.rs.
//!
//! Run with: `cargo test --test integration`

mod test_cli;
mod test_compile_parser_subprocess;
