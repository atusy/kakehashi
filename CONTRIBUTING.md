# Contributing to kakehashi

Thank you for your interest in contributing to kakehashi! This document provides guidelines and information for contributors.

## Table of Contents

- [Development Setup](#development-setup)
- [Architecture Overview](#architecture-overview)
- [Directory Structure](#directory-structure)
- [Development Workflow](#development-workflow)
- [Testing Guidelines](#testing-guidelines)
- [Code Style](#code-style)
- [Commit Guidelines](#commit-guidelines)
- [Adding New Features](#adding-new-features)

## Development Setup

### Prerequisites

- Rust (latest stable version)
- Cargo
- Tree-sitter CLI (optional, for grammar development)

### Building the Project

```bash
# Build in debug mode
cargo build

# Build in release mode
cargo build --release
# or
make build
```

### Building from Source

```bash
# Clone the repository
git clone https://github.com/atusy/kakehashi.git
cd kakehashi

# Build release binary
cargo build --release
# Binary location: target/release/kakehashi

# Build debug binary
cargo build
# or
make debug
```

### Running Tests

#### Rust Tests

```bash
# Setup test dependencies (recommended for integration tests)
make deps  # Creates deps/tree-sitter with parsers and queries

# Run all Rust tests
cargo test
# or
make test

# Run specific test by name
cargo test test_lua_match

# Run only library tests
cargo test --lib

# Run specific integration test file
cargo test --test test_lua_match

# Run tests with output visible
cargo test -- --nocapture

# Run tests sequentially (useful for debugging)
cargo test -- --test-threads=1

# Run with formatting and linting
make check  # runs cargo check, clippy, and fmt --check
```

The `deps/tree-sitter` directory created by `make deps` contains pre-built Tree-sitter parsers and queries that can be used in Rust integration tests by setting appropriate search paths in test configuration.

#### Neovim E2E Tests

The project includes end-to-end testing using Neovim with the `mini.test` framework:

```bash
# Setup test dependencies (required once)
make deps

# Run all Neovim tests
make test_nvim

# Run a specific test file
make test_nvim_file FILE=tests/test_lsp_select.lua

# Clean test dependencies
rm -rf deps/
```

The test infrastructure includes:
- **deps/nvim/mini.nvim**: Testing framework for Neovim E2E tests
- **deps/nvim/nvim-treesitter**: Neovim Tree-sitter integration for E2E testing
- **deps/tree-sitter**: Pre-built Tree-sitter parsers and queries used by both Rust integration tests and Neovim E2E tests

### Code Quality Commands

```bash
# Run linter with warnings as errors
cargo clippy -- -D warnings
# or
make lint

# Format code
cargo fmt
# or
make format

# Check formatting without modifying
cargo fmt --check

# Run all checks (check, clippy, fmt)
make check
```

## Architecture Overview

kakehashi follows a **vertical slice architecture** where each module is responsible for a complete feature area. This design was chosen to avoid circular dependencies and maintain clear separation of concerns.

### Design Principles

1. **Single Responsibility**: Each module has one clear purpose
2. **Vertical Slices**: Features are self-contained within their modules
3. **Unidirectional Dependencies**: Higher-level modules depend on lower-level ones, never the reverse
4. **No Circular Dependencies**: Strict dependency hierarchy is maintained

### Key Architectural Decisions

#### Why Vertical Slices?

The initial codebase had several problems:
- Circular dependencies between `state/` and `tree_sitter/` modules
- Upward dependencies (e.g., `layers/` depending on `state/`)
- Scattered responsibilities across multiple modules
- "God objects" with too many responsibilities

The vertical slice architecture solves these by:
- Grouping related functionality together
- Making dependencies explicit and unidirectional
- Making it easy to find and modify features
- Simplifying testing by isolating concerns

## Directory Structure

```
.
├── src/                    # Source code (see below for detailed structure)
├── tests/                  # Integration and E2E tests
│   ├── *.rs               # Rust integration tests
│   ├── *.lua              # Neovim E2E tests
│   └── assets/            # Test fixtures and sample files
├── scripts/                # Helper scripts
│   └── minimal_init.lua   # Neovim configuration for testing
├── deps/                   # Test dependencies (created by 'make deps')
│   ├── nvim/              # Neovim plugins for E2E tests
│   │   ├── mini.nvim/     # Testing framework
│   │   └── nvim-treesitter/ # Tree-sitter integration
│   └── tree-sitter/       # Pre-built parsers and queries (used by both Rust and Neovim tests)
├── Cargo.toml             # Rust dependencies
├── Makefile               # Build and test automation
└── CONTRIBUTING.md        # This file

src/
├── analysis/       # LSP feature implementations (vertical slice)
│   ├── definition.rs   # Go-to-definition functionality
│   ├── refactor.rs     # Code actions (parameter reordering)
│   ├── selection.rs    # Selection range expansion
│   └── semantic.rs     # Semantic token highlighting
│
├── bin/            # Binary entry point
│   └── main.rs         # Application startup and initialization
│
├── config/         # Configuration management
│   └── settings.rs     # LSP initialization options parsing
│
├── document/       # Document management (vertical slice)
│   ├── model.rs        # Unified Document struct
│   ├── store.rs        # Thread-safe document storage with DashMap
│   └── view.rs         # DocumentView trait exposing read-only access for analysis
│
├── language/       # Language services (parsers, queries, config)
│   ├── config_store.rs     # Language configuration storage
│   ├── coordinator.rs      # Stateless orchestration of language components
│   ├── events.rs           # LanguageEvent and LanguageLoadResult types
│   ├── filetypes.rs        # File extension to language mapping
│   ├── injection.rs        # Language injection support
│   ├── loader.rs           # Dynamic parser loading (.so/.dylib files)
│   ├── parser_pool.rs      # DocumentParserPool for efficient parser reuse
│   ├── query_loader.rs     # Query file loading from disk
│   ├── query_predicates.rs # Shared Tree-sitter predicate filtering
│   ├── query_store.rs      # Query storage and retrieval
│   └── registry.rs         # Language registry and configuration
│
├── lsp/            # LSP server implementation
│   ├── lsp_impl.rs     # LSP protocol handler and orchestration
│   └── settings.rs     # LSP-specific configuration
│
├── text/           # Text manipulation utilities
│   └── position.rs     # Byte ↔ Position conversions
│
└── error.rs        # Error handling types (LspError, LspResult)
```

### Module Responsibilities

Quick reference summary:

- `analysis/`: LSP feature implementations (semantic tokens, go-to-definition, etc.)
- `document/`: Document lifecycle management with thread-safe storage
- `language/`: Parser loading, query management, and language configuration
- `text/`: Text manipulation and coordinate conversion utilities
- `config/`: Configuration parsing and settings management
- `lsp/`: LSP protocol handling and module orchestration
- `error.rs`: Centralized error types for the entire codebase

#### `analysis/` - LSP Features
Each file implements a complete LSP feature:
- **definition.rs**: Handles `textDocument/definition` requests using Tree-sitter locals queries
- **semantic.rs**: Provides syntax highlighting via `textDocument/semanticTokens`
- **selection.rs**: Implements `textDocument/selectionRange` for smart selection
- **refactor.rs**: Code actions like parameter reordering

#### `document/` - Document Management
Manages document lifecycle:
- **model.rs**: Unified `Document` struct
- **store.rs**: Thread-safe document storage with `DashMap`
- **view.rs**: `DocumentView` trait providing read-only access for analysis code

#### `language/` - Language Services
Coordinates parser loading, queries, and language configuration:
- **coordinator.rs**: Stateless orchestration returning structured events for the LSP layer
- **events.rs**: LanguageEvent and result types consumed by the LSP layer
- **config_store.rs**: Language configuration storage
- **filetypes.rs**: File extension to language mapping
- **injection.rs**: Language injection support for embedded languages
- **loader.rs**: Dynamic parser library loading (.so/.dylib files)
- **parser_pool.rs**: DocumentParserPool for efficient parser reuse
- **query_loader.rs**: Query file loading from disk
- **query_predicates.rs**: Shared Tree-sitter predicate filtering
- **query_store.rs**: Query storage and retrieval
- **registry.rs**: Central language registry

#### `text/` - Text Utilities
Provides text manipulation helpers:
- **position.rs**: Coordinate conversions between byte offsets and line-column positions

#### `config/` - Configuration Management
- **settings.rs**: LSP initialization options parsing and validation

### Design Rationale

#### Clean Architecture Implementation
The architecture implements clean architecture principles:
- **Vertical Slices**: Each module owns a complete feature area from top to bottom
- **Module Boundaries**: Clear separation between document management, language services, and text operations
- **No Circular Dependencies**: Strict dependency hierarchy with unidirectional flow
- **Coordinator Pattern**: `LanguageCoordinator` provides stateless coordination between language modules and returns events consumed by the LSP layer
- **Parser Pooling**: `DocumentParserPool` manages parser instances efficiently across documents


#### Parser Pool Unification
We unified parser management into a single `DocumentParserPool` managed by the LSP layer to:
- Avoid duplicate parser instances across documents
- Reduce parser creation overhead during incremental parsing
- Keep parsing concerns isolated from document management

## Development Workflow

### TDD and Development Principles

This project follows Test-Driven Development (TDD) and Kent Beck's "Tidy First" approach:

#### Core TDD Cycle
1. **Red**: Write a failing test that defines a small increment of functionality
2. **Green**: Implement the minimum code needed to make the test pass
3. **Refactor**: Improve the code structure while keeping tests passing

#### Tidy First Approach
Separate all changes into two distinct types:
- **Structural Changes**: Rearranging code without changing behavior (renaming, extracting methods, moving code)
- **Behavioral Changes**: Adding or modifying actual functionality

Never mix structural and behavioral changes in the same commit. Always make structural changes first when both are needed.

#### Commit Discipline
Only commit when:
1. ALL tests are passing
2. ALL compiler/linter warnings have been resolved (`cargo clippy -- -D warnings`)
3. The change represents a single logical unit of work
4. Commit messages clearly state whether structural or behavioral changes

### Pre-commit Checklist
- Run `cargo test` to ensure all tests pass
- Run `cargo clippy -- -D warnings` to check for linting issues
- Run `cargo fmt` to format code
- Or simply run `make check` to do all of the above

### Adding a New LSP Feature

1. Create a new file in `src/analysis/`
2. Implement the feature handler
3. Add the handler to `src/lsp/lsp_impl.rs`
4. Write tests in the same file or in `tests/`

Example structure for a new feature:
```rust
// src/analysis/my_feature.rs
use crate::document::store::DocumentStore;
use crate::error::LspResult;

pub fn handle_my_feature(
    store: &DocumentStore,
    uri: &str,
    params: MyFeatureParams,
) -> LspResult<MyFeatureResponse> {
    let doc = store.get_document(uri)?;
    // Implementation
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_my_feature() {
        // Test implementation
    }
}
```

### Adding Language Support

1. Obtain or build the Tree-sitter parser library (.so/.dylib)
2. Create query files:
   - `queries/<language>/highlights.scm` for syntax highlighting
   - `queries/<language>/locals.scm` for go-to-definition
3. Configure the language in initialization options:
```json
{
  "languages": {
    "my_lang": {
      "aliases": ["ml", "mli"],
      "parser": "/path/to/my_lang.so"
    }
  }
}
```

## Testing Guidelines

### Test Organization

- **Unit tests**: Colocated with implementation in `#[cfg(test)]` modules
- **Rust Integration tests**: In the `tests/` directory:
  - `test_lua_match.rs` - Tests for lua-match predicate functionality
  - `test_multiple_file_opening.rs` - Tests for handling multiple file operations
  - `test_file_reopen.rs` - File reopening scenarios
  - `test_poison_recovery.rs` - Lock poison recovery tests
  - `test_runtime_coordinator_api.rs` - Runtime coordinator tests
- **Neovim E2E tests**: In the `tests/` directory (Lua files):
  - `test_lsp_attach.lua` - LSP attachment and initialization tests
  - `test_lsp_select.lua` - Selection range functionality tests
- **Test utilities**:
  - Rust: Shared test helpers in test modules
  - Neovim: Helper functions in `scripts/minimal_init.lua`

### Writing Tests

Follow the TDD approach:
1. Write a failing test that defines the desired behavior
2. Implement the minimum code to make it pass
3. Refactor while keeping tests green

Example test structure:
```rust
#[test]
fn test_semantic_tokens() {
    // Arrange
    let text = "fn main() { /* comment */ }";
    let document = create_test_document(text);

    // Act
    let tokens = handle_semantic_tokens(&document);

    // Assert
    assert!(tokens.is_some());
    assert_eq!(tokens.unwrap().data.len(), expected_count);
}

// Example: Integration test using deps/tree-sitter parsers
#[test]
fn test_with_real_parser() {
    // Use parsers from deps/tree-sitter directory
    let search_paths = vec!["deps/tree-sitter".to_string()];
    let settings = RawWorkspaceSettings {
        search_paths: Some(search_paths),
        languages: /* language config */,
        ..Default::default()
    };

    // Test with actual Tree-sitter parsers
    let coordinator = LanguageCoordinator::new();
    // ... test implementation
}
```

### Writing Neovim E2E Tests

E2E tests use the `mini.test` framework. For detailed documentation on writing tests with mini.test, see `deps/nvim/mini.nvim/TESTING.md`.

Key points for LSP testing:
- Tests are written in Lua files in the `tests/` directory
- Use `MiniTest.new_child_neovim()` to create isolated Neovim instances
- The `helper.wait()` function in `scripts/minimal_init.lua` helps with async LSP operations
- See existing test files like `tests/test_lsp_select.lua` for examples

### Running Specific Tests

```bash
# Rust tests
# Run tests for a specific module
cargo test analysis::

# Run a single test by name
cargo test test_semantic_tokens

# Run a specific integration test file
cargo test --test test_lua_match

# Run tests with debug output
cargo test -- --nocapture --test-threads=1

# Run only unit tests (no integration tests)
cargo test --lib

# Neovim E2E tests
# Run a specific test file
make test_nvim_file FILE=tests/test_lsp_select.lua

# Run tests interactively in Neovim (for debugging)
nvim -u scripts/minimal_init.lua
# Then in Neovim: :lua MiniTest.run_file('tests/test_lsp_select.lua')
```

## Code Style

### Rust Guidelines

- Follow standard Rust naming conventions
- Use `rustfmt` for formatting (`cargo fmt`)
- Use `clippy` for linting (`cargo clippy -- -D warnings`)
- Prefer explicit types for public APIs
- Document public items with doc comments

### Error Handling Guidelines

This project follows strict error handling practices to ensure robustness:

#### Core Principles
1. **No unwrap() on lock operations**: All mutex/RwLock operations use proper error handling with poison recovery
2. **No panic! in production code**: Use Result types and proper error propagation instead
3. **Custom error types**: Use the `LspError` enum from `src/error.rs` for type-safe error handling
4. **Logging on recovery**: When recovering from poisoned locks, log warnings using the `log` crate

#### Lock Handling Patterns

```rust
use log::warn;

// ✅ Good - with poison recovery
match self.data.lock() {
    Ok(guard) => guard.get(key).cloned(),
    Err(poisoned) => {
        warn!(
            target: "kakehashi::lock_recovery",
            "Recovered from poisoned lock in module::function"
        );
        poisoned.into_inner().get(key).cloned()
    }
}

// ❌ Bad - will panic on poisoned lock
self.data.lock().unwrap().get(key).cloned()
```

#### Error Type Usage

```rust
use crate::error::{LspError, LspResult};

// Function returning a Result
pub fn process_document(uri: &str) -> LspResult<Document> {
    let doc = store.get(uri)
        .ok_or_else(|| LspError::document_not_found(uri))?;

    // Process document...
    Ok(doc)
}
```

#### Testing Poison Recovery

All lock-based modules include tests for poison recovery. See `tests/test_poison_recovery.rs` for examples.

### Code Organization

- Keep modules focused on a single responsibility
- Prefer composition over inheritance
- Use the type system to make illegal states unrepresentable
- Minimize mutable state

### Error Handling

- Use `Option` for operations that might not produce a value
- Use `Result` for operations that might fail with an error
- Provide context in error messages
- Avoid `unwrap()` except in tests or when impossible to fail

## Commit Guidelines

### Commit Message Format

```
<type>(<scope>): <brief description>

<detailed description>

<footer>
```

Types:
- `feat`: New feature
- `fix`: Bug fix
- `refactor`: Code restructuring without behavior change
- `test`: Test additions or modifications
- `docs`: Documentation changes
- `perf`: Performance improvements
- `chore`: Maintenance tasks

Example:
```
refactor(document): consolidate coordinate mapping

Move all coordinate conversion logic to document/coordinates.rs
to maintain single responsibility and avoid duplication.

All tests passing.
```

### Commit Best Practices

1. Make atomic commits (one logical change per commit)
2. Ensure all tests pass before committing
3. Run `make test format lint` before committing
4. Write clear, descriptive commit messages
5. Reference issues when applicable

## Adding New Features

### Checklist for New Features

- [ ] Design follows vertical slice architecture
- [ ] No circular dependencies introduced
- [ ] Unit tests written and passing
- [ ] Integration tests added if applicable
- [ ] Documentation updated
- [ ] Code formatted with `rustfmt`
- [ ] No `clippy` warnings
- [ ] Commit message follows guidelines

### Feature Development Process

1. **Design Phase**
   - Identify which vertical slice the feature belongs to
   - Define clear interfaces and dependencies
   - Consider language injection and multi-language support

2. **Test-First Implementation (TDD)**
   - Write a failing test that defines the desired behavior
   - Implement the minimum code to make the test pass
   - Refactor while keeping tests green
   - Repeat for each small increment of functionality

3. **Code Quality Phase**
   - Run `cargo clippy -- -D warnings` to check for issues
   - Run `cargo fmt` to format code
   - Ensure no `unwrap()` on locks or potential panics
   - Add error recovery for poisoned locks where needed

4. **Review Phase**
   - Self-review for code quality and clarity
   - Ensure tests cover edge cases
   - Update documentation if APIs changed

### Performance Considerations

- **Parser Pooling**: Use `DocumentParserPool` to avoid recreating parsers
- **Query Caching**: Queries are compiled once and stored in `QueryStore`
- **Incremental Parsing**: Use Tree-sitter's incremental parsing for edits
- **Concurrent Access**: `DashMap` enables lock-free concurrent document access
- **Large Documents**: Be mindful of performance with very large files
- **Profile First**: Always profile before optimizing to identify actual bottlenecks

## Obtaining Parser Libraries

To add support for a language, you need its Tree-sitter parser as a shared library:

### Building Parsers

Example: Building the Rust parser
```bash
git clone https://github.com/tree-sitter/tree-sitter-rust.git
cd tree-sitter-rust
npm install
npm run build
# Creates rust.so (Linux) or rust.dylib (macOS)
```

### Parser Library Formats
- **Linux**: `.so` files
- **macOS**: `.dylib` files
- **Windows**: `.dll` files (experimental)

### Query Files

Tree-sitter queries power the language features. Place them in:
- `<searchPath>/queries/<language>/highlights.scm` - Syntax highlighting
- `<searchPath>/queries/<language>/locals.scm` - Go-to-definition support

Queries use Tree-sitter's S-expression syntax:
```scheme
; highlights.scm
(function_item name: (identifier) @function)
(string_literal) @string

; locals.scm
(function_item name: (identifier) @local.definition.function)
(call_expression function: (identifier) @local.reference.function)
```

## Questions and Support

If you have questions about contributing:

1. Check existing issues and discussions
2. Look at similar features for examples
3. Open an issue for design discussions
4. Ask in pull request comments

Thank you for contributing to kakehashi!
