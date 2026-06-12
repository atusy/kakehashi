//! CLI-mode commands that reuse the LSP server's machinery in-process.
//!
//! Unlike the thin install/config subcommands in `src/bin/main.rs`, the
//! commands here need the full server stack (config loading, parser
//! registry, injection resolution, downstream language-server bridge), so
//! they live in the library crate where `pub(crate)` internals are reachable.

pub mod format;
