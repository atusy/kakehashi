//! Generic lexical name resolution over `bindings.scm` queries.
//!
//! Implements the engine specified in
//! `docs/architecture-decisions/lexical-name-resolution.md`: the engine knows
//! only the capture vocabulary and property semantics — everything
//! language-specific is declared in per-language query assets.

pub(crate) mod collect;
