//! In-process driving of the LSP handlers for the `kakehashi` CLI subcommands.
//!
//! Each subcommand runs the same `Kakehashi` instance an editor would talk to,
//! but calls the handler implementations directly instead of going through
//! JSON-RPC framing. The [`session`] submodule holds the lifecycle every
//! subcommand shares (initialize/shutdown, path filter, readiness waits); the
//! per-command `didOpen → … → didClose` wrappers live in [`format`] and
//! [`diagnose`] (mirroring `crate::cli`'s own `format`/`diagnose` split).

mod diagnose;
mod format;
mod session;
