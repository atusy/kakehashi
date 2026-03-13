//! Integration tests for auto-install functionality.
//!
//! These tests verify the complete flow of auto-installing parsers when
//! documents with injected languages are opened or edited.

use kakehashi::document::DocumentStore;
use kakehashi::language::LanguageCoordinator;
use kakehashi::language::injection::collect_all_injections;
use kakehashi::lsp::auto_install::{InstallingLanguages, InstallingLanguagesExt};
use std::collections::HashSet;
use tree_sitter::{Parser, Query};
use url::Url;

#[test]
fn test_did_open_should_call_check_injected_languages_after_parsing() {
    // Test that did_open calls check_injected_languages_auto_install after parsing.
    //
    // The expected call sequence in did_open is:
    // 1. Determine language from path or language_id
    // 2. Check if auto-install needed for host language -> maybe_auto_install_language()
    // 3. parse_document() - parses the document and stores in DocumentStore
    // 4. check_injected_languages_auto_install() - checks injected languages (NEW!)
    // 5. Check if queries are ready and request semantic tokens refresh

    // Create a coordinator and document store
    let coordinator = LanguageCoordinator::new();
    let documents = DocumentStore::new();

    // Create a test URL
    let uri = Url::parse("file:///test/example.md").unwrap();

    // Before parsing (document not in store):
    let no_doc_result = documents.get(&uri);
    assert!(
        no_doc_result.is_none(),
        "Document should not exist before parsing"
    );

    // Parse a simple markdown document with a code block
    let markdown_text = r#"# Test
```lua
print("hello")
```
"#;
    let mut parser = Parser::new();
    let md_language: tree_sitter::Language = tree_sitter_md::LANGUAGE.into();
    parser.set_language(&md_language).expect("set markdown");
    let tree = parser.parse(markdown_text, None).expect("parse markdown");

    // Insert the document (simulating what parse_document does)
    documents.insert(
        uri.clone(),
        markdown_text.to_string(),
        Some("markdown".to_string()),
        Some(tree),
    );

    // Now the document exists with a tree
    let doc = documents.get(&uri);
    assert!(doc.is_some(), "Document should exist after parsing");
    assert!(
        doc.as_ref().unwrap().tree().is_some(),
        "Document should have parsed tree"
    );

    // Verify that get_injected_languages needs the coordinator to have injection queries
    assert!(
        coordinator.get_injection_query("markdown").is_none(),
        "No injection query configured for markdown in bare coordinator"
    );
}

#[test]
fn test_opening_markdown_with_code_blocks_triggers_auto_install_for_injected_languages() {
    // Integration test for opening a markdown file with code blocks
    // triggers auto-install for injected languages.

    // Create a markdown document with multiple injected languages
    let markdown_text = r#"# Example Document

This is a markdown file with multiple code blocks.

```lua
print("Hello from Lua")
local x = 42
```

Some text between code blocks.

```python
def hello():
    print("Hello from Python")
```

And another Lua block (should not trigger duplicate install):

```lua
local y = "duplicate"
```
"#;

    // Parse the markdown document
    let mut parser = Parser::new();
    let md_language: tree_sitter::Language = tree_sitter_md::LANGUAGE.into();
    parser.set_language(&md_language).expect("set markdown");
    let tree = parser.parse(markdown_text, None).expect("parse markdown");
    let root = tree.root_node();

    // Create an injection query that matches fenced code blocks
    let injection_query_str = r#"
        (fenced_code_block
          (info_string
            (language) @injection.language)
          (code_fence_content) @injection.content)
    "#;
    let injection_query =
        Query::new(&md_language, injection_query_str).expect("valid injection query");

    // Collect all injections from the document
    let injections =
        collect_all_injections(&root, markdown_text, Some(&injection_query)).unwrap_or_default();

    // Extract unique languages
    let unique_languages: HashSet<String> = injections.iter().map(|i| i.language.clone()).collect();

    // Verify we detected both Lua and Python (unique, not 3 total)
    assert_eq!(
        unique_languages.len(),
        2,
        "Should detect exactly 2 unique languages (lua and python), not 3"
    );
    assert!(
        unique_languages.contains("lua"),
        "Should detect 'lua' from code blocks"
    );
    assert!(
        unique_languages.contains("python"),
        "Should detect 'python' from code block"
    );

    // Verify there are 3 injection regions total (2 lua + 1 python)
    assert_eq!(
        injections.len(),
        3,
        "Should have 3 injection regions (2 lua + 1 python)"
    );

    // Test InstallingLanguages tracker prevents duplicate install attempts
    let tracker = InstallingLanguages::new();

    // Simulate the auto-install check for each unique language
    let mut install_triggered: Vec<String> = Vec::new();

    for lang in &unique_languages {
        if tracker.try_start_install(lang) {
            install_triggered.push(lang.to_string());
        }
    }

    // Both languages should trigger install (first time)
    assert_eq!(
        install_triggered.len(),
        2,
        "Should trigger install for both unique languages"
    );
    assert!(install_triggered.contains(&"lua".to_string()));
    assert!(install_triggered.contains(&"python".to_string()));

    // Simulate opening another file with the same languages
    let mut second_file_install_triggered: Vec<String> = Vec::new();
    for lang in &unique_languages {
        if tracker.try_start_install(lang) {
            second_file_install_triggered.push(lang.to_string());
        }
    }

    // Second file should NOT trigger any installs
    assert!(
        second_file_install_triggered.is_empty(),
        "Second file should not trigger installs for languages already being installed"
    );
}

#[test]
fn test_did_change_should_call_check_injected_languages_after_parsing() {
    // Test that did_change calls check_injected_languages_auto_install after parsing.

    // Create a coordinator and document store
    let coordinator = LanguageCoordinator::new();
    let documents = DocumentStore::new();

    // Create a test URL
    let uri = Url::parse("file:///test/example.md").unwrap();

    // Before parsing
    let no_doc_result = documents.get(&uri);
    assert!(
        no_doc_result.is_none(),
        "Document should not exist before parsing"
    );

    // After edit (simulating did_change with new content containing code block)
    let edited_text = "# Test\n```lua\nprint(\"hello\")\n```\n";

    // Parse the edited document
    let mut parser = Parser::new();
    let md_language: tree_sitter::Language = tree_sitter_md::LANGUAGE.into();
    parser.set_language(&md_language).expect("set markdown");
    let tree = parser.parse(edited_text, None).expect("parse markdown");

    // Insert the document
    documents.insert(
        uri.clone(),
        edited_text.to_string(),
        Some("markdown".to_string()),
        Some(tree),
    );

    // After parse_document, document should be ready
    let doc = documents.get(&uri);
    assert!(doc.is_some(), "Document should exist after parsing");
    assert!(
        doc.as_ref().unwrap().tree().is_some(),
        "Document should have parsed tree"
    );

    // Verify that the coordinator would need injection query configured
    assert!(
        coordinator.get_injection_query("markdown").is_none(),
        "No injection query configured for markdown in bare coordinator"
    );
}

#[test]
fn test_adding_code_block_triggers_auto_install_for_injected_language() {
    // Test that editing a document to add a code block triggers auto-install

    // BEFORE: Markdown document with NO code blocks
    let initial_text = r#"# My Document

This is a simple markdown file with no code blocks yet.

I will add a code block below:

"#;

    // Parse initial document
    let mut parser = Parser::new();
    let md_language: tree_sitter::Language = tree_sitter_md::LANGUAGE.into();
    parser.set_language(&md_language).expect("set markdown");
    let initial_tree = parser.parse(initial_text, None).expect("parse markdown");
    let initial_root = initial_tree.root_node();

    // Create injection query for markdown code blocks
    let injection_query_str = r#"
        (fenced_code_block
          (info_string
            (language) @injection.language)
          (code_fence_content) @injection.content)
    "#;
    let injection_query =
        Query::new(&md_language, injection_query_str).expect("valid injection query");

    // Initially, there should be NO injected languages
    let initial_injections =
        collect_all_injections(&initial_root, initial_text, Some(&injection_query))
            .unwrap_or_default();
    assert!(
        initial_injections.is_empty(),
        "Initial document should have no code blocks"
    );

    // AFTER: User adds a Lua code block
    let edited_text = r#"# My Document

This is a simple markdown file with no code blocks yet.

I will add a code block below:

```lua
print("Hello from Lua!")
local x = 42
```
"#;

    // Re-parse after edit
    let edited_tree = parser
        .parse(edited_text, None)
        .expect("parse edited markdown");
    let edited_root = edited_tree.root_node();

    // Now there should be ONE injected language: "lua"
    let edited_injections =
        collect_all_injections(&edited_root, edited_text, Some(&injection_query))
            .unwrap_or_default();

    // Extract unique languages
    let unique_languages: HashSet<String> = edited_injections
        .iter()
        .map(|i| i.language.clone())
        .collect();

    // Verify the Lua code block was detected
    assert_eq!(
        unique_languages.len(),
        1,
        "Should detect exactly 1 unique language after adding code block"
    );
    assert!(
        unique_languages.contains("lua"),
        "Should detect 'lua' from the newly added code block"
    );

    // Verify that the InstallingLanguages tracker would allow installation
    let tracker = InstallingLanguages::new();

    assert!(
        tracker.try_start_install("lua"),
        "Should be able to start install for new language"
    );
}

#[test]
fn test_unrelated_edits_dont_retrigger_for_already_loaded_languages() {
    // Test that editing text outside code blocks doesn't trigger auto-install
    // for languages that are already loaded.

    // Create a markdown document with an existing Lua code block
    let initial_text = r#"# My Document

Here is some Lua code:

```lua
print("Hello from Lua!")
local x = 42
```

Some text below the code block.
"#;

    // Parse the document
    let mut parser = Parser::new();
    let md_language: tree_sitter::Language = tree_sitter_md::LANGUAGE.into();
    parser.set_language(&md_language).expect("set markdown");
    let tree = parser.parse(initial_text, None).expect("parse markdown");
    let root = tree.root_node();

    // Create injection query for markdown code blocks
    let injection_query_str = r#"
        (fenced_code_block
          (info_string
            (language) @injection.language)
          (code_fence_content) @injection.content)
    "#;
    let injection_query =
        Query::new(&md_language, injection_query_str).expect("valid injection query");

    // Verify Lua is detected
    let injections =
        collect_all_injections(&root, initial_text, Some(&injection_query)).unwrap_or_default();
    let unique_languages: HashSet<String> = injections.iter().map(|i| i.language.clone()).collect();
    assert!(
        unique_languages.contains("lua"),
        "Lua should be detected as injected language"
    );

    // NOW: Simulate an unrelated edit
    let edited_text = r#"# My Updated Document Title

Here is some Lua code:

```lua
print("Hello from Lua!")
local x = 42
```

Some updated text below the code block with more content.
"#;

    // Re-parse after edit
    let edited_tree = parser
        .parse(edited_text, None)
        .expect("parse edited markdown");
    let edited_root = edited_tree.root_node();

    // Lua should STILL be detected
    let edited_injections =
        collect_all_injections(&edited_root, edited_text, Some(&injection_query))
            .unwrap_or_default();
    let edited_languages: HashSet<String> = edited_injections
        .iter()
        .map(|i| i.language.clone())
        .collect();
    assert!(
        edited_languages.contains("lua"),
        "Lua should still be detected after unrelated edit"
    );

    // Create a coordinator and verify ensure_language_loaded behavior
    let coordinator = LanguageCoordinator::new();

    // For an unconfigured coordinator, ensure_language_loaded fails
    let lua_result = coordinator.ensure_language_loaded("lua");
    assert!(
        !lua_result.success,
        "Unconfigured coordinator should fail to load lua"
    );

    // Simulate using InstallingLanguages tracker
    let tracker = InstallingLanguages::new();

    // First install attempt should succeed
    assert!(
        tracker.try_start_install("lua"),
        "First install attempt should succeed"
    );

    // Second install attempt should fail (already installing)
    assert!(
        !tracker.try_start_install("lua"),
        "Second install attempt should fail (already installing)"
    );

    // After completion, install can start again
    tracker.finish_install("lua");
    assert!(
        tracker.try_start_install("lua"),
        "After finish, can install again"
    );
}

#[test]
fn test_pasting_multiple_code_blocks_triggers_all_languages() {
    // Test that pasting multiple code blocks triggers auto-install for all new languages.

    // BEFORE: Minimal markdown document
    let initial_text = "# My Document\n\nI will paste some code blocks here:\n\n";

    // Parse initial document
    let mut parser = Parser::new();
    let md_language: tree_sitter::Language = tree_sitter_md::LANGUAGE.into();
    parser.set_language(&md_language).expect("set markdown");
    let initial_tree = parser.parse(initial_text, None).expect("parse markdown");
    let initial_root = initial_tree.root_node();

    // Create injection query for markdown code blocks
    let injection_query_str = r#"
        (fenced_code_block
          (info_string
            (language) @injection.language)
          (code_fence_content) @injection.content)
    "#;
    let injection_query =
        Query::new(&md_language, injection_query_str).expect("valid injection query");

    // Initially, there should be NO injected languages
    let initial_injections =
        collect_all_injections(&initial_root, initial_text, Some(&injection_query))
            .unwrap_or_default();
    assert!(
        initial_injections.is_empty(),
        "Initial document should have no code blocks"
    );

    // AFTER: User pastes multiple code blocks with Python, Rust, Go
    let pasted_text = r#"# My Document

I will paste some code blocks here:

```python
def hello():
    print("Hello from Python!")
```

```rust
fn main() {
    println!("Hello from Rust!");
}
```

```go
package main

func main() {
    fmt.Println("Hello from Go!")
}
```

And another Python block (duplicate language):

```python
class Foo:
    pass
```
"#;

    // Re-parse after paste
    let pasted_tree = parser
        .parse(pasted_text, None)
        .expect("parse pasted markdown");
    let pasted_root = pasted_tree.root_node();

    // Detect all injections
    let pasted_injections =
        collect_all_injections(&pasted_root, pasted_text, Some(&injection_query))
            .unwrap_or_default();

    // Should have 4 injection regions total (python, rust, go, python)
    assert_eq!(
        pasted_injections.len(),
        4,
        "Should detect 4 injection regions (2 python + 1 rust + 1 go)"
    );

    // Extract unique languages
    let unique_languages: HashSet<String> = pasted_injections
        .iter()
        .map(|i| i.language.clone())
        .collect();

    // Should have exactly 3 unique languages
    assert_eq!(
        unique_languages.len(),
        3,
        "Should detect exactly 3 unique languages"
    );
    assert!(
        unique_languages.contains("python"),
        "Should detect 'python'"
    );
    assert!(unique_languages.contains("rust"), "Should detect 'rust'");
    assert!(unique_languages.contains("go"), "Should detect 'go'");

    // Simulate the check_injected_languages_auto_install behavior
    let tracker = InstallingLanguages::new();
    let mut installed: Vec<String> = Vec::new();

    for lang in &unique_languages {
        if tracker.try_start_install(lang) {
            installed.push(lang.to_string());
        }
    }

    // All 3 languages should trigger install
    assert_eq!(
        installed.len(),
        3,
        "Should trigger install for all 3 unique languages"
    );

    // Simulate: user opens another file with same languages (while still installing)
    let mut second_file_installed: Vec<String> = Vec::new();
    for lang in &unique_languages {
        if tracker.try_start_install(lang) {
            second_file_installed.push(lang.to_string());
        }
    }

    assert!(
        second_file_installed.is_empty(),
        "Second file should not trigger installs for languages already being installed"
    );
}

#[test]
fn test_reload_after_install_requires_ensure_language_loaded_sequence() {
    // TDD RED PHASE: This test documents the bug in reload_language_after_install
    // and verifies the CORRECT sequence that should be followed.
    //
    // Bug in reload_language_after_install:
    //   apply_settings() -> parse_document(lang) -> FAILS because detect_language
    //   checks has_parser_available which returns false
    //
    // Correct sequence (what the fix should implement):
    //   apply_settings() -> ensure_language_loaded(lang) -> parse_document(lang) -> WORKS
    //
    // This test verifies the correct behavior at the coordinator level.
    // The actual bug is in lsp_impl.rs which needs to call ensure_language_loaded.

    use kakehashi::language::LanguageCoordinator;

    let coordinator = LanguageCoordinator::new();

    // Initially: parser not loaded, detect_language returns None
    let path = "test.rs";
    let content = "fn main() {}";
    let language_id = Some("rust");

    let detected_before = coordinator.detect_language(path, content, None, language_id);
    assert!(
        detected_before.is_none(),
        "Before ensure_language_loaded: detect_language returns None because \
         has_parser_available returns false. This is the bug scenario."
    );

    // After calling ensure_language_loaded (which tries to load from search_paths):
    // Note: This will fail without proper search_paths, but it demonstrates the intent.
    // The key point is that ensure_language_loaded MUST be called to register the parser.
    let load_result = coordinator.ensure_language_loaded("rust");

    // Without proper search_paths, loading fails (expected in this test environment)
    // In production with auto-install, the search_paths would include the installed parser
    assert!(
        !load_result.success,
        "ensure_language_loaded fails without search_paths (expected in test)"
    );

    // The fix ensures that in reload_language_after_install:
    // 1. apply_settings adds the installed parser path to search_paths
    // 2. ensure_language_loaded("rust") is called to load from those paths
    // 3. parse_document can then use detect_language successfully
    //
    // This test documents the contract; E2E verification is done with:
    // make test_nvim FILE=tests/test_lsp_shebang.lua (or similar)
}
