# ADR-0022: Replace tree-sitter-cli with tree-sitter-loader

| | |
|---|---|
| **Status** | accepted |
| **Date** | 2026-02-26 |
| **Decision-makers** | atusy |
| **Consulted** | Claude Code |
| **Informed** | kakehashi users |
| **Supersedes** | [ADR-0004](0004-keep-tree-sitter-cli-dependency.md) |

## Context and Problem Statement

kakehashi required users to install `tree-sitter-cli` (via `cargo install tree-sitter-cli`) as a runtime dependency for parser compilation. This in turn required the entire Rust toolchain — a significant barrier for users who only want to use kakehashi as an LSP server.

ADR-0004 evaluated "direct C compilation with manual header management" as an alternative and rejected it due to header management complexity. However, `tree-sitter-loader` is a different proposition: it's the official library crate from the tree-sitter team that handles headers, scanner detection, platform flags, and file locking automatically via the `cc` crate.

## Decision Drivers

* **Lower barrier to entry**: Users shouldn't need the Rust toolchain just to use kakehashi
* **Maintenance burden**: Library dependency is easier to maintain than CLI process spawning
* **Reliability**: Library calls are more reliable than shelling out to external processes
* **Ecosystem alignment**: `tree-sitter-loader` is the official tree-sitter library for this purpose

## Considered Options

1. **Keep tree-sitter-cli** (status quo from ADR-0004)
2. **Use tree-sitter-loader library**

## Decision Outcome

**Chosen option**: "Use tree-sitter-loader library", because:

1. **Eliminates Rust toolchain requirement**: Users only need a C compiler and git
2. **Simpler compilation flow**: `compile_parser_at_path()` replaces the build → find → copy pattern with a single library call
3. **Better error handling**: Library errors are structured, not stderr parsing
4. **Header management is handled**: The `cc` crate and tree-sitter-loader manage include paths automatically

### Consequences

**Positive:**

* Users no longer need `cargo install tree-sitter-cli`
* Users no longer need the Rust toolchain installed
* Simpler installation flow: compile directly to target path
* No process spawning for compilation

**Negative:**

* `tree-sitter-loader` pulls in `tree-sitter-highlight` and `tree-sitter-tags` as dependencies (required due to a bug in the crate when `default-features = false`)
* `compile_parser_at_path` internally loads the compiled library and leaks it via `mem::forget` — acceptable since the leaked copy is small and happens at most once per language per process lifetime

**Neutral:**

* C compiler and git are still required
* The compilation output is identical (same shared library format)

## More Information

* [tree-sitter-loader crate](https://crates.io/crates/tree-sitter-loader)
* [ADR-0004](0004-keep-tree-sitter-cli-dependency.md) — Superseded decision
