mod context;
mod hierarchy_chain;
mod injection_aware;
mod range_builder;

// Internal re-exports for tests
#[cfg(test)]
use injection_aware::is_cursor_within_effective_range;
#[cfg(test)]
use range_builder::build_from_node;

use crate::document::DocumentHandle;
use crate::language::{DocumentParserPool, LanguageCoordinator};
use context::{DocumentContext, InjectionContext};
use tower_lsp_server::ls_types::{Position, Range, SelectionRange};

/// Handle textDocument/selectionRange request with full injection parsing support.
///
/// Parses injected content and builds selection hierarchies from the injected
/// language's AST. Returns one SelectionRange per position (LSP Spec 3.17 alignment).
pub fn handle_selection_range(
    document: &DocumentHandle,
    positions: &[Position],
    coordinator: &LanguageCoordinator,
    parser_pool: &mut DocumentParserPool,
) -> Vec<SelectionRange> {
    let text = document.text();
    let mapper = document.position_mapper();
    let root = document.tree().map(|t| t.root_node());
    let lang = document.language_id();

    positions
        .iter()
        .map(|pos| {
            if let Some(root) = root
                && let Some(cursor_byte_offset) = mapper.position_to_byte(*pos)
                && let Some(node) =
                    root.descendant_for_byte_range(cursor_byte_offset, cursor_byte_offset)
                && let Some(lang) = lang
            {
                let doc_ctx = DocumentContext::new(text, &mapper, root, lang);
                let mut inj_ctx = InjectionContext::new(coordinator, parser_pool);
                range_builder::build(node, &doc_ctx, &mut inj_ctx, cursor_byte_offset)
            } else {
                SelectionRange {
                    range: Range::new(*pos, *pos),
                    parent: None,
                }
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::language::injection::parse_offset_directive_for_pattern;
    use crate::text::PositionMapper;
    use tree_sitter::Point;

    /// Calculate the start position for nested injection relative to host document
    ///
    /// This function handles signed offsets from injection directives like
    /// `(#offset! @injection.content -1 0 0 0)` used in markdown frontmatter.
    /// Negative offsets are handled with saturating arithmetic to prevent underflow.
    ///
    /// Column calculation logic:
    /// - If the *effective* row (content_start.row + offset_rows) is 0, we're on the
    ///   same row as the parent, so we add parent's column offset.
    /// - If the effective row is > 0, we've moved to a later row (e.g., after skipping
    ///   a fence line), so the column is absolute within the content.
    ///
    /// Note: This function was used for Point-based calculation before Sprint 9.
    /// It's now kept for test coverage but production code uses byte-based offsets.
    fn calculate_nested_start_position(
        parent_start: tree_sitter::Point,
        content_start: tree_sitter::Point,
        offset_rows: i32,
        offset_cols: i32,
    ) -> tree_sitter::Point {
        let col_parent = if (content_start.row as i32 + offset_rows).max(0) == 0 {
            parent_start.column as i64
        } else {
            0_i64
        };
        tree_sitter::Point::new(
            ((parent_start.row + content_start.row) as i64 + offset_rows as i64).max(0) as usize,
            (col_parent + content_start.column as i64 + offset_cols as i64).max(0) as usize,
        )
    }
    /// Verifies offset directive parsing and effective range boundary checking.
    #[test]
    fn test_selection_range_respects_offset_directive() {
        use crate::text::PositionMapper;
        use tree_sitter::{Parser, Query};

        let mut parser = Parser::new();
        let language = tree_sitter_rust::LANGUAGE.into();
        parser.set_language(&language).expect("load rust grammar");

        // string_content "_^\d+$" is at bytes 43-49
        let text = r#"fn main() {
    let pattern = Regex::new(r"_^\d+$").unwrap();
}"#;
        let tree = parser.parse(text, None).expect("parse rust");
        let root = tree.root_node();

        // offset (0, 2, 0, 0) means effective range starts 2 bytes after capture → bytes 45-49
        let injection_query_str = r#"
(call_expression
  function: (scoped_identifier
    path: (identifier) @_regex
    (#eq? @_regex "Regex")
    name: (identifier) @_new
    (#eq? @_new "new"))
  arguments: (arguments
    (raw_string_literal
      (string_content) @injection.content))
  (#set! injection.language "regex")
  (#offset! @injection.content 0 2 0 0))
        "#;

        let injection_query = Query::new(&language, injection_query_str).expect("valid query");
        let mapper = PositionMapper::new(text);

        let underscore_pos = Position::new(1, 31);
        let underscore_point = Point::new(
            underscore_pos.line as usize,
            underscore_pos.character as usize,
        );
        let underscore_byte = mapper.position_to_byte(underscore_pos).unwrap();

        let string_content_node = root
            .descendant_for_point_range(underscore_point, underscore_point)
            .expect("should find node");

        assert_eq!(string_content_node.kind(), "string_content");
        assert_eq!(string_content_node.start_byte(), 43);
        assert_eq!(string_content_node.end_byte(), 49);

        let offset = parse_offset_directive_for_pattern(&injection_query, 0);
        assert!(offset.is_some(), "Offset directive should be found");
        let offset = offset.unwrap();
        assert_eq!(offset.start_row, 0);
        assert_eq!(offset.start_column, 2);
        assert_eq!(offset.end_row, 0);
        assert_eq!(offset.end_column, 0);

        // byte 43 (underscore) is OUTSIDE effective range 45-49
        assert!(
            !is_cursor_within_effective_range(text, &string_content_node, underscore_byte, offset),
            "Cursor at byte 43 (underscore) should be OUTSIDE effective range 45-49"
        );

        // byte 45 (caret) is INSIDE effective range
        let caret_pos = Position::new(1, 33);
        let caret_byte = mapper.position_to_byte(caret_pos).unwrap();
        assert_eq!(caret_byte, 45, "Caret should be at byte 45");

        assert!(
            is_cursor_within_effective_range(text, &string_content_node, caret_byte, offset),
            "Cursor at byte 45 (caret ^) should be INSIDE effective range 45-49"
        );

        // byte 44 is OUTSIDE effective range
        assert!(
            !is_cursor_within_effective_range(text, &string_content_node, 44, offset),
            "Cursor at byte 44 should be OUTSIDE effective range 45-49"
        );
    }

    /// Nested injection (Rust → YAML → Rust) includes content node boundary in hierarchy.
    #[test]
    fn test_nested_injection_includes_content_node_boundary() {
        use crate::language::LanguageCoordinator;
        use tree_sitter::{Parser, Query};

        let coordinator = LanguageCoordinator::new();
        let registry = coordinator.language_registry_for_parallel();
        registry.register("yaml".to_string(), tree_sitter_yaml::LANGUAGE.into());
        registry.register("rust".to_string(), tree_sitter_rust::LANGUAGE.into());

        let yaml_injection_query_str = r#"
((double_quote_scalar) @injection.content
 (#set! injection.language "rust"))
        "#;
        let yaml_lang: tree_sitter::Language = tree_sitter_yaml::LANGUAGE.into();
        let yaml_injection_query =
            Query::new(&yaml_lang, yaml_injection_query_str).expect("valid yaml injection query");
        coordinator.query_store().insert_injection_query(
            "yaml".to_string(),
            std::sync::Arc::new(yaml_injection_query),
        );

        let mut parser_pool = coordinator.create_document_parser_pool();

        let mut parser = Parser::new();
        let rust_language = tree_sitter_rust::LANGUAGE.into();
        parser
            .set_language(&rust_language)
            .expect("load rust grammar");

        // Rust → YAML → Rust: double_quote_scalar contains "fn nested() {}"
        let text = r##"fn main() {
    let yaml = r#"title: "fn nested() {}""#;
}"##;
        let tree = parser.parse(text, None).expect("parse rust");
        let root = tree.root_node();

        let injection_query_str = r#"
(raw_string_literal
  (string_content) @injection.content
  (#set! injection.language "yaml"))
        "#;
        let injection_query =
            Query::new(&rust_language, injection_query_str).expect("valid injection query");
        coordinator
            .query_store()
            .insert_injection_query("rust".to_string(), std::sync::Arc::new(injection_query));

        let cursor_pos = Position::new(1, 33);
        let mapper = crate::text::PositionMapper::new(text);
        let cursor_byte = mapper.position_to_byte(cursor_pos).unwrap();
        let point = Point::new(cursor_pos.line as usize, cursor_pos.character as usize);
        let node = root
            .descendant_for_point_range(point, point)
            .expect("find node");

        let doc_ctx = DocumentContext::new(text, &mapper, root, "rust");
        let mut inj_ctx = InjectionContext::new(&coordinator, &mut parser_pool);
        let selection = range_builder::build(node, &doc_ctx, &mut inj_ctx, cursor_byte);

        let mut ranges: Vec<Range> = Vec::new();
        let mut curr = Some(&selection);
        while let Some(sel) = curr {
            ranges.push(sel.range);
            curr = sel.parent.as_ref().map(|p| p.as_ref());
        }

        assert!(
            ranges.len() >= 7,
            "Expected at least 7 selection levels with nested injection, got {}",
            ranges.len()
        );

        // double_quote_scalar boundary should be around (1:25-26 to 1:40-42)
        let nested_content_found = ranges.iter().any(|r| {
            r.start.line == 1
                && r.start.character >= 25
                && r.start.character <= 26
                && r.end.line == 1
                && r.end.character >= 40
                && r.end.character <= 42
        });

        assert!(
            nested_content_found,
            "Selection hierarchy should include nested injection content node boundary.\n\
             Ranges in hierarchy: {:?}\n\
             Expected a range around (1:25-26 to 1:40-42) for the nested content node.",
            ranges
        );
    }

    /// Injected content (YAML in Rust string) produces selection hierarchy from injected AST.
    #[test]
    fn test_selection_range_parses_injected_content() {
        use crate::language::LanguageCoordinator;
        use tree_sitter::{Parser, Query};

        let coordinator = LanguageCoordinator::new();
        coordinator
            .language_registry_for_parallel()
            .register("yaml".to_string(), tree_sitter_yaml::LANGUAGE.into());

        let mut parser_pool = coordinator.create_document_parser_pool();

        let mut parser = Parser::new();
        let rust_language = tree_sitter_rust::LANGUAGE.into();
        parser
            .set_language(&rust_language)
            .expect("load rust grammar");

        let text = r##"fn main() {
    let yaml = r#"title: "awesome"
array: ["xxxx"]"#;
}"##;
        let tree = parser.parse(text, None).expect("parse rust");
        let root = tree.root_node();

        let injection_query_str = r#"
(raw_string_literal
  (string_content) @injection.content
  (#set! injection.language "yaml"))
        "#;
        let injection_query =
            Query::new(&rust_language, injection_query_str).expect("valid injection query");
        coordinator
            .query_store()
            .insert_injection_query("rust".to_string(), std::sync::Arc::new(injection_query));

        let cursor_pos = Position::new(1, 32); // 'a' in "awesome"
        let point = Point::new(cursor_pos.line as usize, cursor_pos.character as usize);

        let node = root
            .descendant_for_point_range(point, point)
            .expect("should find node");

        let mapper = crate::text::PositionMapper::new(text);
        let cursor_byte = mapper.position_to_byte(cursor_pos).unwrap();

        let doc_ctx = DocumentContext::new(text, &mapper, root, "rust");
        let mut inj_ctx = InjectionContext::new(&coordinator, &mut parser_pool);
        let selection = range_builder::build(node, &doc_ctx, &mut inj_ctx, cursor_byte);

        let mut level_count = 0;
        let mut curr = Some(&selection);
        while let Some(sel) = curr {
            level_count += 1;
            curr = sel.parent.as_ref().map(|p| p.as_ref());
        }

        // With injection: YAML AST nodes + host nodes = at least 7 levels
        assert!(
            level_count >= 7,
            "Expected at least 7 selection levels with injected YAML AST, got {}. \
             This indicates the injected content was not parsed.",
            level_count
        );
    }

    /// Nested start position handles negative offsets and column alignment.
    #[test]
    fn test_calculate_nested_start_position() {
        // Negative row offset saturates to 0
        let result = calculate_nested_start_position(
            tree_sitter::Point::new(2, 0),
            tree_sitter::Point::new(1, 0),
            -5,
            0,
        );
        assert_eq!(result.row, 0);

        // Negative column offset saturates to 0
        let result = calculate_nested_start_position(
            tree_sitter::Point::new(0, 10),
            tree_sitter::Point::new(0, 5),
            0,
            -20,
        );
        assert_eq!(result.column, 0);

        // Effective row 0: add parent's column
        let result = calculate_nested_start_position(
            tree_sitter::Point::new(5, 4),
            tree_sitter::Point::new(0, 0),
            0,
            0,
        );
        assert_eq!(result.row, 5);
        assert_eq!(result.column, 4);

        // Row offset > 0: column is absolute
        let result = calculate_nested_start_position(
            tree_sitter::Point::new(5, 4),
            tree_sitter::Point::new(0, 0),
            1,
            0,
        );
        assert_eq!(result.row, 6);
        assert_eq!(result.column, 0);

        // Positive offset: column is absolute
        let result = calculate_nested_start_position(
            tree_sitter::Point::new(10, 5),
            tree_sitter::Point::new(0, 3),
            2,
            1,
        );
        assert_eq!(result.row, 12);
        assert_eq!(result.column, 4);

        // No row offset: add parent column
        let result = calculate_nested_start_position(
            tree_sitter::Point::new(10, 5),
            tree_sitter::Point::new(0, 3),
            0,
            1,
        );
        assert_eq!(result.row, 10);
        assert_eq!(result.column, 9);

        // Negative offset brings effective row to 0: add parent column
        let result = calculate_nested_start_position(
            tree_sitter::Point::new(5, 4),
            tree_sitter::Point::new(1, 2),
            -1,
            0,
        );
        assert_eq!(result.row, 5);
        assert_eq!(result.column, 6);
    }

    /// Deduplicates nodes with identical ranges (LSP requires strictly expanding ranges).
    #[test]
    fn test_selection_range_deduplicates_same_range_nodes() {
        use tree_sitter::Parser;

        let mut parser = Parser::new();
        let language = tree_sitter_rust::LANGUAGE.into();
        parser.set_language(&language).expect("load rust grammar");

        let text = "fn f() { x }";
        let tree = parser.parse(text, None).expect("parse rust");
        let root = tree.root_node();

        let cursor_byte = 9; // position of "x"
        let node = root
            .descendant_for_byte_range(cursor_byte, cursor_byte)
            .expect("should find node");

        assert_eq!(node.kind(), "identifier");

        let mapper = PositionMapper::new(text);
        let selection = build_from_node(node, &mapper);

        let mut ranges: Vec<(u32, u32, u32, u32)> = Vec::new();
        let mut curr = Some(&selection);
        while let Some(sel) = curr {
            ranges.push((
                sel.range.start.line,
                sel.range.start.character,
                sel.range.end.line,
                sel.range.end.character,
            ));
            curr = sel.parent.as_ref().map(|p| p.as_ref());
        }

        // No consecutive duplicates allowed
        for i in 1..ranges.len() {
            assert_ne!(
                ranges[i - 1],
                ranges[i],
                "Found duplicate ranges at positions {} and {}: {:?}",
                i - 1,
                i,
                ranges[i]
            );
        }

        assert!(
            ranges.len() <= 8,
            "Expected at most 8 levels (with deduplication), got {}. Ranges: {:?}",
            ranges.len(),
            ranges
        );
    }

    /// Selection ranges use UTF-16 columns, not byte offsets.
    /// "あ" = 3 bytes UTF-8, 1 UTF-16 code unit. 'x' at byte 17 → UTF-16 column 15.
    #[test]
    fn test_selection_range_output_uses_utf16_columns() {
        use tree_sitter::Parser;

        let mut parser = Parser::new();
        let language = tree_sitter_rust::LANGUAGE.into();
        parser.set_language(&language).expect("load rust grammar");

        let text = "let あ = 1; let x = 2;";
        let tree = parser.parse(text, None).expect("parse rust");
        let root = tree.root_node();

        let mapper = PositionMapper::new(text);

        let byte_offset = 17; // 'x' at byte 17
        let node = root
            .descendant_for_byte_range(byte_offset, byte_offset)
            .expect("should find node");

        assert_eq!(node.kind(), "identifier");
        assert_eq!(&text[node.byte_range()], "x");

        let selection = build_from_node(node, &mapper);

        // UTF-16 column 15, not byte 17
        assert_eq!(selection.range.start.character, 15);
        assert_eq!(selection.range.end.character, 16);
    }

    /// Injected content uses UTF-16 columns. "0" in `r#"あ: 0"#` is at UTF-16 col 17, not byte 19.
    #[test]
    fn test_injected_selection_range_uses_utf16_columns() {
        use crate::language::LanguageCoordinator;
        use tree_sitter::{Parser, Query};

        let coordinator = LanguageCoordinator::new();
        coordinator
            .language_registry_for_parallel()
            .register("yaml".to_string(), tree_sitter_yaml::LANGUAGE.into());

        let yaml_injection_query_str = r#"
((double_quote_scalar) @injection.content
 (#set! injection.language "yaml"))
        "#;
        let yaml_lang: tree_sitter::Language = tree_sitter_yaml::LANGUAGE.into();
        let yaml_injection_query =
            Query::new(&yaml_lang, yaml_injection_query_str).expect("valid yaml injection query");
        coordinator.query_store().insert_injection_query(
            "yaml".to_string(),
            std::sync::Arc::new(yaml_injection_query),
        );

        let mut parser_pool = coordinator.create_document_parser_pool();

        let mut parser = Parser::new();
        let rust_language = tree_sitter_rust::LANGUAGE.into();
        parser
            .set_language(&rust_language)
            .expect("load rust grammar");

        // "あ" = 3 bytes UTF-8, 1 UTF-16 code unit
        // "0" at UTF-16 col 17, byte 19
        let text = "let yaml = r#\"あ: 0\"#;";
        let tree = parser.parse(text, None).expect("parse");
        let root = tree.root_node();

        let injection_query_str = r#"
(raw_string_literal
  (string_content) @injection.content
  (#set! injection.language "yaml"))
        "#;
        let injection_query = Query::new(&rust_language, injection_query_str).expect("valid query");
        coordinator
            .query_store()
            .insert_injection_query("rust".to_string(), std::sync::Arc::new(injection_query));

        let mapper = crate::text::PositionMapper::new(text);

        let mut cursor = root.walk();
        let mut content_node = None;
        loop {
            let node = cursor.node();
            if node.kind() == "string_content" {
                content_node = Some(node);
                break;
            }
            if cursor.goto_first_child() {
                continue;
            }
            while !cursor.goto_next_sibling() {
                if !cursor.goto_parent() {
                    break;
                }
            }
            if cursor.node().id() == root.id() {
                break;
            }
        }
        let content_node = content_node.expect("Should find string_content node");

        let content_text = &text[content_node.byte_range()];
        assert_eq!(content_text, "あ: 0");

        let zero_byte_in_host = content_node.start_byte() + 5; // "あ"=3 + ": "=2

        let doc_ctx = DocumentContext::new(text, &mapper, root, "rust");
        let mut inj_ctx = InjectionContext::new(&coordinator, &mut parser_pool);
        let selection =
            range_builder::build(content_node, &doc_ctx, &mut inj_ctx, zero_byte_in_host);

        // Find small range in injected content, verify UTF-16 column < 19 (byte offset)
        let mut found_small_range = false;
        let mut current = &selection;
        loop {
            if current.range.start.line == 0
                && current.range.end.line == 0
                && current.range.end.character - current.range.start.character <= 5
                && current.range.start.character >= 17
                && current.range.start.character <= 20
            {
                found_small_range = true;
                assert!(
                    current.range.start.character < 19,
                    "Expected UTF-16 column (17 or 18), got byte-based column {}",
                    current.range.start.character
                );
                break;
            }
            if let Some(parent) = &current.parent {
                current = parent.as_ref();
            } else {
                break;
            }
        }

        assert!(
            found_small_range,
            "Should find a small range in the injected content. Selection ranges: {:?}",
            collect_ranges(&selection)
        );
    }

    /// Invalid positions get fallback empty ranges (LSP Spec 3.17: 1:1 position-result alignment).
    #[test]
    fn test_selection_range_maintains_position_alignment() {
        use crate::document::store::DocumentStore;
        use crate::language::LanguageCoordinator;
        use tree_sitter::Parser;
        use url::Url;

        let mut parser = Parser::new();
        let language = tree_sitter_rust::LANGUAGE.into();
        parser.set_language(&language).expect("load rust grammar");

        let text = "let x = 1;\nlet y = 2;";
        let tree = parser.parse(text, None).expect("parse rust");

        let url = Url::parse("file:///test.rs").unwrap();
        let store = DocumentStore::new();
        store.insert(
            url.clone(),
            text.to_string(),
            Some("rust".to_string()),
            Some(tree),
        );

        let positions = vec![
            Position::new(0, 4),   // valid: 'x'
            Position::new(100, 0), // invalid: line 100 doesn't exist
            Position::new(1, 4),   // valid: 'y'
        ];

        let coordinator = LanguageCoordinator::new();
        let mut parser_pool = coordinator.create_document_parser_pool();
        let document = store.get(&url).expect("document should exist");
        let ranges = handle_selection_range(&document, &positions, &coordinator, &mut parser_pool);

        assert_eq!(ranges.len(), positions.len());
        assert!(ranges[0].range.start.line == 0);
        assert_eq!(ranges[1].range.start, ranges[1].range.end); // invalid → empty
        assert!(ranges[1].parent.is_none());
        assert!(ranges[2].range.start.line == 1);
    }

    /// Helper to collect all ranges in a selection hierarchy for debugging
    fn collect_ranges(selection: &SelectionRange) -> Vec<Range> {
        let mut ranges = vec![selection.range];
        let mut current = &selection.parent;
        while let Some(parent) = current {
            ranges.push(parent.range);
            current = &parent.parent;
        }
        ranges
    }

    /// Selection ranges in blockquote code blocks use correct positions.
    ///
    /// Blockquote code blocks have `block_continuation` named children (the `> ` prefixes).
    /// The injection parser must use `set_included_ranges()` to exclude these prefixes,
    /// otherwise the injected language parser sees `> key1: val1` instead of `key1: val1`.
    ///
    /// Uses YAML as the injected language because `> ` is a YAML folded scalar indicator,
    /// causing a dramatically different parse tree when prefixes leak into the content.
    /// Compares blockquote vs non-blockquote to verify identical selection structure.
    #[test]
    fn test_selection_range_in_blockquote_code_block() {
        use crate::language::LanguageCoordinator;
        use tree_sitter::{Parser, Query};

        let coordinator = LanguageCoordinator::new();
        coordinator
            .language_registry_for_parallel()
            .register("yaml".to_string(), tree_sitter_yaml::LANGUAGE.into());

        let mut parser_pool = coordinator.create_document_parser_pool();

        let mut parser = Parser::new();
        let md_language: tree_sitter::Language = tree_sitter_md::LANGUAGE.into();
        parser
            .set_language(&md_language)
            .expect("load markdown grammar");

        // Note: injection.include-children is intentionally NOT set.
        // Its absence causes has_include_children_for_pattern() to return false,
        // which triggers compute_included_ranges() to exclude block_continuation
        // children — the code path under test.
        let injection_query_str = r#"
            (fenced_code_block
              (info_string (language) @_lang)
              (code_fence_content) @injection.content
              (#set-lang-from-info-string! @_lang))
        "#;
        let injection_query =
            Query::new(&md_language, injection_query_str).expect("valid injection query");
        coordinator
            .query_store()
            .insert_injection_query("markdown".to_string(), std::sync::Arc::new(injection_query));

        // --- Blockquote case ---
        // Line 0: "> ```yaml"
        // Line 1: "> key1: val1"
        // Line 2: "> key2: val2"
        // Line 3: "> ```"
        let text = "> ```yaml\n> key1: val1\n> key2: val2\n> ```\n";
        let tree = parser.parse(text, None).expect("parse markdown");
        let root = tree.root_node();
        let mapper = crate::text::PositionMapper::new(text);

        // Cursor on 'k' of 'key2' at line 2, character 2 (after "> ")
        let cursor_pos = Position::new(2, 2);
        let cursor_byte = mapper.position_to_byte(cursor_pos).unwrap();
        let point =
            tree_sitter::Point::new(cursor_pos.line as usize, cursor_pos.character as usize);
        let node = root
            .descendant_for_point_range(point, point)
            .expect("find node");

        let doc_ctx = DocumentContext::new(text, &mapper, root, "markdown");
        let mut inj_ctx = InjectionContext::new(&coordinator, &mut parser_pool);
        let selection = range_builder::build(node, &doc_ctx, &mut inj_ctx, cursor_byte);
        let ranges = collect_ranges(&selection);

        // --- Reference case: same YAML without blockquote ---
        let ref_text = "```yaml\nkey1: val1\nkey2: val2\n```\n";
        let ref_tree = parser.parse(ref_text, None).expect("parse ref markdown");
        let ref_root = ref_tree.root_node();
        let ref_mapper = crate::text::PositionMapper::new(ref_text);

        let ref_cursor_pos = Position::new(2, 0);
        let ref_cursor_byte = ref_mapper.position_to_byte(ref_cursor_pos).unwrap();
        let ref_point = tree_sitter::Point::new(
            ref_cursor_pos.line as usize,
            ref_cursor_pos.character as usize,
        );
        let ref_node = ref_root
            .descendant_for_point_range(ref_point, ref_point)
            .expect("find ref node");

        let ref_doc_ctx = DocumentContext::new(ref_text, &ref_mapper, ref_root, "markdown");
        let mut ref_inj_ctx = InjectionContext::new(&coordinator, &mut parser_pool);
        let ref_selection =
            range_builder::build(ref_node, &ref_doc_ctx, &mut ref_inj_ctx, ref_cursor_byte);
        let ref_ranges = collect_ranges(&ref_selection);

        // The innermost range must be on the same line as the cursor (line 2).
        // Without the fix, the YAML parser sees `key1: val1\n> key2: val2\n> `
        // and the `> ` prefix corrupts the parse tree, causing the innermost
        // range to land on line 1 instead of line 2.
        assert_eq!(
            selection.range.start.line, 2,
            "Innermost selection range should be on cursor line 2.\n\
             Blockquote ranges: {:?}\n\
             Reference ranges: {:?}",
            ranges, ref_ranges
        );

        // The injected ranges on the cursor line should have the same widths —
        // same YAML AST structure, just column-shifted by 2 for the `> ` prefix.
        // (Total depth may differ because blockquote adds extra host levels.)
        let bq_widths: Vec<_> = ranges
            .iter()
            .filter(|r| r.start.line == 2 && r.end.line == 2)
            .map(|r| r.end.character - r.start.character)
            .collect();
        let ref_widths: Vec<_> = ref_ranges
            .iter()
            .filter(|r| r.start.line == 2 && r.end.line == 2)
            .map(|r| r.end.character - r.start.character)
            .collect();
        assert_eq!(
            bq_widths, ref_widths,
            "Injected ranges on the cursor line should have identical widths.\n\
             Blockquote ranges: {:?}\n\
             Reference ranges: {:?}",
            ranges, ref_ranges
        );
    }

    /// Empty documents return valid fallback range at (0, 0).
    #[test]
    fn test_selection_range_handles_empty_document() {
        use crate::document::store::DocumentStore;
        use crate::language::LanguageCoordinator;
        use tree_sitter::Parser;
        use url::Url;

        let mut parser = Parser::new();
        let language = tree_sitter_rust::LANGUAGE.into();
        parser.set_language(&language).expect("load rust grammar");

        let text = "";
        let tree = parser.parse(text, None).expect("parse empty document");

        let url = Url::parse("file:///empty.rs").unwrap();
        let store = DocumentStore::new();
        store.insert(
            url.clone(),
            text.to_string(),
            Some("rust".to_string()),
            Some(tree),
        );

        let positions = vec![Position::new(0, 0)];

        let coordinator = LanguageCoordinator::new();
        let mut parser_pool = coordinator.create_document_parser_pool();
        let document = store.get(&url).expect("document should exist");
        let ranges = handle_selection_range(&document, &positions, &coordinator, &mut parser_pool);

        assert_eq!(ranges.len(), 1);
        assert_eq!(ranges[0].range.start, Position::new(0, 0));
        assert_eq!(ranges[0].range.end, Position::new(0, 0));
    }
}
