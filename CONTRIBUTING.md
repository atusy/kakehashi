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
