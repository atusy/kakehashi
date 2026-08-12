//! Test fixtures for E2E tests.
//!
//! Provides reusable markdown file fixtures with Rust code blocks for testing
//! different LSP features (hover, completion, references, etc.).

// These functions are shared across multiple test binaries but not all tests use every function.
// Allow dead_code to suppress per-binary warnings.
#![allow(dead_code)]

/// Create a temporary markdown file with Rust code block for hover testing.
///
/// Content: fn main() { println!("Hello, world!"); }
/// Cursor target: 'main' at line 3, column 3 (0-indexed)
pub(crate) fn create_hover_fixture() -> (String, String, tempfile::NamedTempFile) {
    let content = r#"# Example

```rust
fn main() {
    println!("Hello, world!");
}
```
"#;

    create_markdown_file(content)
}

/// Create a temporary markdown file with Rust code block for completion testing.
///
/// Content: struct Point { x: i32, y: i32 } with instance p
/// Cursor target: after 'p.' at line 10, column 6 (0-indexed)
pub(crate) fn create_completion_fixture() -> (String, String, tempfile::NamedTempFile) {
    let content = r#"# Rust Example

```rust
struct Point {
    x: i32,
    y: i32,
}

fn main() {
    let p = Point { x: 1, y: 2 };
    p.
}
```
"#;

    create_markdown_file(content)
}

/// Create a temporary markdown file with Rust code block for references testing.
///
/// Content: variable 'x' defined and used multiple times
/// Cursor target: 'x' definition at line 4, column 8 (0-indexed)
pub(crate) fn create_references_fixture() -> (String, String, tempfile::NamedTempFile) {
    let content = r#"# Rust Example

```rust
fn main() {
    let x = 42;
    let y = x + 1;
    let z = x * 2;
}
```
"#;

    create_markdown_file(content)
}

/// Create a temporary markdown file with Rust code block for definition testing.
///
/// Content: fn example() { println!("Hello, world!"); }
/// Cursor target: 'example' at line 5 for go-to-definition
pub(crate) fn create_definition_fixture() -> (String, String, tempfile::NamedTempFile) {
    let content = r#"Here is a function definition:

```rust
fn example() {
    println!("Hello, world!");
}

fn main() {
    example();
}
```
"#;

    create_markdown_file(content)
}

/// Create a temporary markdown file with Rust code block for signature help testing.
///
/// Content: fn greet(name: &str, age: u32) { ... }
/// Cursor target: inside 'greet(' call at line 8, column 10 (after opening paren)
pub(crate) fn create_signature_help_fixture() -> (String, String, tempfile::NamedTempFile) {
    let content = r#"# Signature Help Example

```rust
fn greet(name: &str, age: u32) {
    println!("Hello, {}! You are {} years old.", name, age);
}

fn main() {
    greet(
}
```
"#;

    create_markdown_file(content)
}

/// Create a temporary Lua file for selection range testing (no injection).
///
/// Content: Simple Lua module with function definition
/// Test positions: line 0 col 0 for "local" keyword
pub(crate) fn create_selection_range_lua_fixture() -> (String, String, tempfile::NamedTempFile) {
    let content = r#"local M = {}

function M.f()
	return
end

return M
"#;

    create_lua_file(content)
}

/// Create a temporary markdown file for selection range testing (with injections).
///
/// Content: Markdown with YAML frontmatter, Lua code blocks, nested injections
/// Based on tests/assets/example.md
pub(crate) fn create_selection_range_md_fixture() -> (String, String, tempfile::NamedTempFile) {
    let content = r#"---
title: "awesome"
array: ["xxxx"]
---

```lua
local xyz = 12345
```

# nested injection

`````markdown
```lua
local injection = true
```
`````

# indented injection

* item

    ```lua
    local indent = true
    ```

# section

paragraph

```py
def f():
    return 1
```
"#;

    create_markdown_file(content)
}

/// Helper to create a temporary Lua file with given content.
fn create_lua_file(content: &str) -> (String, String, tempfile::NamedTempFile) {
    let temp_file = tempfile::Builder::new()
        .suffix(".lua")
        .tempfile()
        .expect("Failed to create temp file");

    std::fs::write(temp_file.path(), content).expect("Failed to write temp file");

    let uri = url::Url::from_file_path(temp_file.path())
        .expect("Failed to construct file URI from temp file path")
        .to_string();

    (uri, content.to_string(), temp_file)
}

/// Helper to create a temporary markdown file with given content.
fn create_markdown_file(content: &str) -> (String, String, tempfile::NamedTempFile) {
    let temp_file = tempfile::Builder::new()
        .suffix(".md")
        .tempfile()
        .expect("Failed to create temp file");

    std::fs::write(temp_file.path(), content).expect("Failed to write temp file");

    let uri = url::Url::from_file_path(temp_file.path())
        .expect("Failed to construct file URI from temp file path")
        .to_string();

    (uri, content.to_string(), temp_file)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hover_fixture_creates_file() {
        let (uri, content, _temp_file) = create_hover_fixture();
        assert!(uri.starts_with("file://"));
        assert!(content.contains("fn main()"));
    }

    #[test]
    fn test_completion_fixture_creates_file() {
        let (uri, content, _temp_file) = create_completion_fixture();
        assert!(uri.starts_with("file://"));
        assert!(content.contains("struct Point"));
        assert!(content.contains("p."));
    }

    #[test]
    fn test_references_fixture_creates_file() {
        let (uri, content, _temp_file) = create_references_fixture();
        assert!(uri.starts_with("file://"));
        assert!(content.contains("let x = 42"));
    }

    #[test]
    fn test_definition_fixture_creates_file() {
        let (uri, content, _temp_file) = create_definition_fixture();
        assert!(uri.starts_with("file://"));
        assert!(content.contains("fn example()"));
    }
}
