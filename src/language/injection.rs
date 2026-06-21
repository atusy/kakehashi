//! Tree-sitter language injection: discovering injected regions (e.g. Lua in
//! Markdown) and preparing them for highlighting and the LSP bridge.
//!
//! The surface is split by concern, mirroring `analysis/semantic/` and
//! `analysis/selection/`:
//! - [`offset`] — parsing the `#offset!` directive
//! - [`ranges`] — computing the included byte ranges fed to the injection parser
//! - [`content`] — clean-content extraction and byte→`Point` coordinate helpers
//! - [`language`] — resolving the injection language for a matched pattern
//! - [`discovery`] — finding regions and resolving them into cacheable form

mod content;
mod discovery;
mod language;
mod offset;
mod ranges;

/// Maximum recursion depth for nested injections to prevent stack overflow.
pub(crate) const MAX_INJECTION_DEPTH: usize = 10;

pub use discovery::{
    CacheableInjectionRegion, InjectionRegionInfo, InjectionResolver, ResolvedInjection,
    collect_all_injections,
};
pub use offset::InjectionOffset;

pub(crate) use content::{
    byte_to_point, byte_to_point_anchored, extract_clean_content, parse_with_ranges,
};
pub(crate) use discovery::detect_injection;
pub(crate) use offset::effective_offset_for_pattern;
pub(crate) use ranges::{
    compute_included_ranges, compute_included_ranges_clipped, has_combined_for_pattern,
    has_include_children_for_pattern, intersect_included_ranges, sub_select_included_ranges,
};

// Exposed only for the unit tests below and the selection-range tests, which
// exercise these helpers directly. Gating with cfg(test) keeps them out of the
// production surface so non-test builds don't see unused re-exports.
#[cfg(test)]
pub(crate) use content::compute_line_column_offsets;
#[cfg(test)]
pub(crate) use discovery::{find_injection_at_position, is_node_within};
#[cfg(test)]
pub(crate) use offset::parse_offset_directive_for_pattern;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::language::LanguageCoordinator;
    use rstest::rstest;
    use tree_sitter::{Node, Parser, Query, StreamingIterator};
    use url::Url;

    #[test]
    fn test_parse_offset_directive_for_pattern() {
        // Test that the pattern-aware function correctly returns
        // offsets only for the specific pattern

        // Create a query similar to markdown's injection.scm with multiple patterns
        let query_str = r#"
            ; Pattern 0: Raw string literals - NO OFFSET
            ((raw_string_literal) @injection.content
              (#set! injection.language "regex"))

            ; Pattern 1: Comments - HAS OFFSET
            ((line_comment) @injection.content
              (#set! injection.language "markdown")
              (#offset! @injection.content 1 0 -1 0))
        "#;

        let language = tree_sitter_rust::LANGUAGE.into();
        let query = Query::new(&language, query_str).expect("valid query");

        // Pattern 0 (raw_string_literal) has NO offset
        let offset_pattern_0 = parse_offset_directive_for_pattern(&query, 0);
        assert_eq!(offset_pattern_0, None, "Pattern 0 should have no offset");

        // Pattern 1 (line_comment) HAS offset
        let offset_pattern_1 = parse_offset_directive_for_pattern(&query, 1);
        assert_eq!(
            offset_pattern_1,
            Some(InjectionOffset {
                start_row: 1,
                start_column: 0,
                end_row: -1,
                end_column: 0
            }),
            "Pattern 1 should have offset (1, 0, -1, 0)"
        );
    }

    fn create_rust_parser() -> Parser {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .expect("load rust grammar");
        parser
    }

    fn parse_rust_code(parser: &mut Parser, code: &str) -> tree_sitter::Tree {
        parser.parse(code, None).expect("parse rust")
    }

    #[test]
    fn test_detect_nested_injections() {
        use tree_sitter::Parser;

        // Simulate a markdown file with a code block
        let mut parser = Parser::new();
        let language = tree_sitter_rust::LANGUAGE.into();
        parser.set_language(&language).expect("load rust grammar");

        let text = r#"let x = "markdown with ```lua code```";"#;
        let tree = parser.parse(text, None).expect("parse rust");
        let root = tree.root_node();

        // Create a mock injection query that simulates nested injections
        let query_str = r#"
        (string_literal
          (string_content) @injection.content
          (#set! injection.language "markdown"))
        "#;

        let query = Query::new(&language, query_str).expect("valid query");

        // Find a node within the string content
        let node_in_string = find_node_at_byte(&root, 20).expect("node at position");

        // Detect injection with content
        let result = detect_injection(&node_in_string, &root, text, Some(&query), "rust");

        assert!(result.is_some());
        let (hierarchy, _content_node, _pattern_index) = result.unwrap();

        // Should detect rust -> markdown hierarchy
        assert_eq!(hierarchy, vec!["rust", "markdown"]);
    }

    #[test]
    fn test_detect_injection_with_static_language() {
        let mut parser = create_rust_parser();
        let text = r#"fn main() { let re = Regex::new(r"^\d+$").unwrap(); }"#;
        let tree = parse_rust_code(&mut parser, text);
        let root = tree.root_node();

        // Create a query that matches Regex::new with static language
        let query_str = r#"
            (call_expression
              function: (scoped_identifier
                path: (identifier) @_regex
                (#eq? @_regex "Regex")
                name: (identifier) @_new
                (#eq? @_new "new"))
              arguments: (arguments
                (raw_string_literal
                  (string_content) @injection.content))
              (#set! injection.language "regex"))
        "#;

        let language = tree_sitter_rust::LANGUAGE.into();
        let query = Query::new(&language, query_str).expect("valid query");

        // Find a node inside the regex string
        let node = find_node_at_byte(&root, 35); // Position in regex string
        assert!(node.is_some());

        let result = detect_injection(&node.unwrap(), &root, text, Some(&query), "rust");
        assert_eq!(
            result.map(|(h, _, _)| h),
            Some(vec!["rust".to_string(), "regex".to_string()])
        );
    }

    #[test]
    fn test_detect_injection_with_no_injection() {
        let mut parser = create_rust_parser();
        let text = r#"fn main() { println!("hello"); }"#;
        let tree = parse_rust_code(&mut parser, text);
        let root = tree.root_node();

        // Query that won't match
        let query_str = r#"
            (call_expression
              function: (identifier) @_fn
              (#eq? @_fn "nonexistent")
              (arguments) @injection.content
              (#set! injection.language "test"))
        "#;

        let language = tree_sitter_rust::LANGUAGE.into();
        let query = Query::new(&language, query_str).expect("valid query");

        let node = find_node_at_byte(&root, 20); // Position in string
        assert!(node.is_some());

        let result = detect_injection(&node.unwrap(), &root, text, Some(&query), "rust");
        assert_eq!(result.map(|(h, _, _)| h), None);
    }

    #[test]
    fn test_detect_injection_without_query() {
        let mut parser = create_rust_parser();
        let text = r#"fn main() { }"#;
        let tree = parse_rust_code(&mut parser, text);
        let root = tree.root_node();

        let node = root.child(0).unwrap();
        let result = detect_injection(&node, &root, text, None, "rust");
        assert_eq!(result, None);
    }

    #[test]
    fn test_is_node_within() {
        let mut parser = create_rust_parser();
        let text = r#"fn main() { let x = 42; }"#;
        let tree = parse_rust_code(&mut parser, text);
        let root = tree.root_node();

        let outer = root.child(0).unwrap(); // function_item
        let inner = find_node_at_byte(&root, 20).unwrap(); // Some node inside

        assert!(is_node_within(&inner, &outer));
        assert!(!is_node_within(&outer, &inner));
    }

    #[test]
    fn test_recursive_injection_depth_limit() {
        // Test that we can handle multiple levels of injection
        // This is a simple test - real recursive injection happens in refactor.rs

        let mut parser = create_rust_parser();
        let text = r#"fn main() { let x = "nested"; }"#;
        let tree = parse_rust_code(&mut parser, text);
        let root = tree.root_node();

        // Create a query that would inject strings as another language
        let query_str = r#"
        ((string_literal
          (string_content) @injection.content)
         (#set! injection.language "nested_lang"))
        "#;

        let language = tree_sitter_rust::LANGUAGE.into();
        let query = Query::new(&language, query_str).expect("valid query");

        let node = find_node_at_byte(&root, 22).expect("node in string");
        let result = detect_injection(&node, &root, text, Some(&query), "rust");

        assert!(result.is_some());
        let (hierarchy, _, _) = result.unwrap();
        assert_eq!(hierarchy, vec!["rust", "nested_lang"]);

        // The actual deep recursion is tested through integration with refactor.rs
        // where handle_nested_injection recursively processes injections
    }

    #[test]
    fn test_duplicate_injections_same_node() {
        // Test that multiple injection patterns matching the same node
        // should only result in one injection (not nested)
        let mut parser = create_rust_parser();
        let text = r#"fn main() { /* comment */ }"#;
        let tree = parse_rust_code(&mut parser, text);
        let root = tree.root_node();

        // Create a mock query that would inject the same node twice
        // This simulates what happens with luadoc -> comment
        let query_str = r#"
        ((block_comment) @injection.content
         (#set! injection.language "doc"))

        ((block_comment) @injection.content
         (#set! injection.language "comment"))
        "#;

        let language = tree_sitter_rust::LANGUAGE.into();
        let query = Query::new(&language, query_str).expect("valid query");

        // Find a node inside the comment
        // The injection query matches on block_comment nodes, so we need to be inside one
        let mut cursor = tree_sitter::QueryCursor::new();
        let mut matches = cursor.matches(&query, root, text.as_bytes());

        let mut injection_count = 0;
        while let Some(_match) = matches.next() {
            injection_count += 1;
        }

        // This should find 2 matches (both patterns match the same comment)
        assert_eq!(injection_count, 2, "Expected 2 injection patterns to match");

        // Now test our detection from inside the comment
        let node_in_comment = find_node_at_byte(&root, 14).expect("node in comment");
        let result = detect_injection(&node_in_comment, &root, text, Some(&query), "rust");

        // Should detect only one injection (first pattern takes precedence)
        assert!(result.is_some(), "Should find injection");
        let (hierarchy, _, _) = result.unwrap();
        // Should only use the first matching pattern, not both
        assert_eq!(
            hierarchy,
            vec!["rust", "doc"],
            "Should only show first injection"
        );
    }

    // Helper function to find a node at a specific byte position
    fn find_node_at_byte<'a>(root: &Node<'a>, byte: usize) -> Option<Node<'a>> {
        root.descendant_for_byte_range(byte, byte)
    }

    #[rstest]
    #[case::non_numeric_values("foo bar baz qux", Some(InjectionOffset::default()))]
    #[case::missing_arguments("1 0", Some(InjectionOffset::default()))]
    #[case::extra_arguments("1 0 -1 0 5", Some(InjectionOffset { start_row: 1, start_column: 0, end_row: -1, end_column: 0 }))]
    #[case::mixed_valid_invalid("1 invalid -1 0", Some(InjectionOffset::default()))]
    #[case::empty_args("", Some(InjectionOffset::default()))]
    #[trace]
    fn test_offset_directive_edge_cases(
        #[case] offset_args: &str,
        #[case] expected: Option<InjectionOffset>,
    ) {
        let language = tree_sitter_rust::LANGUAGE.into();
        let query_str = if offset_args.is_empty() {
            r#"
            ((line_comment) @injection.content
              (#set! injection.language "test")
              (#offset! @injection.content))
        "#
            .to_string()
        } else {
            format!(
                r#"
            ((line_comment) @injection.content
              (#set! injection.language "test")
              (#offset! @injection.content {}))
        "#,
                offset_args
            )
        };

        let query = Query::new(&language, &query_str).expect("valid query");
        let offset = parse_offset_directive_for_pattern(&query, 0);

        assert_eq!(
            offset, expected,
            "offset_args={:?} should produce {:?}",
            offset_args, expected
        );
    }

    #[test]
    fn test_cacheable_injection_region_from_region_info() {
        // Create a parser and parse some code to get a real Node
        let mut parser = create_rust_parser();
        let text = r#"fn main() { let s = "hello"; }"#;
        let tree = parse_rust_code(&mut parser, text);
        let root = tree.root_node();

        // Create an injection query that matches the string
        let query_str = r#"
            ((string_literal) @injection.content
              (#set! injection.language "markdown"))
        "#;
        let language = tree_sitter_rust::LANGUAGE.into();
        let query = Query::new(&language, query_str).expect("valid query");

        // Get injection regions
        let regions = collect_all_injections(&root, text, Some(&query));
        let regions = regions.expect("Should find injections");
        assert!(!regions.is_empty(), "Should find at least one injection");

        let region_info = &regions[0];

        // Convert to CacheableInjectionRegion (owned, no lifetime)
        let cacheable =
            CacheableInjectionRegion::from_region_info(region_info, "test-result-id", text);

        // Verify all fields are captured correctly
        assert_eq!(cacheable.language, "markdown");
        assert_eq!(
            cacheable.byte_range.start,
            region_info.content_node.start_byte()
        );
        assert_eq!(
            cacheable.byte_range.end,
            region_info.content_node.end_byte()
        );
        assert_eq!(
            cacheable.line_range.start,
            region_info.content_node.start_position().row as u32
        );
        assert_eq!(
            cacheable.line_range.end,
            region_info.content_node.start_position().row as u32 + 1
        );
        assert_eq!(cacheable.region_id, "test-result-id");
    }

    #[test]
    fn test_from_region_info_applies_column_offset() {
        // #offset! with a column delta (e.g. regex content after a prefix)
        // must shift byte_range and start_column to the effective position,
        // so the bridge extracts the right content and translates coordinates
        // correctly (#183).
        let mut parser = create_rust_parser();
        let text = "// regex content";
        let tree = parse_rust_code(&mut parser, text);
        let root = tree.root_node();

        let query_str = r#"
            ((line_comment) @injection.content
              (#set! injection.language "regex")
              (#offset! @injection.content 0 3 0 0))
        "#;
        let language = tree_sitter_rust::LANGUAGE.into();
        let query = Query::new(&language, query_str).expect("valid query");

        let regions = collect_all_injections(&root, text, Some(&query)).expect("injections");
        assert_eq!(regions.len(), 1);
        let cacheable = CacheableInjectionRegion::from_region_info(&regions[0], "test-id", text);

        assert_eq!(&text[cacheable.byte_range.clone()], "regex content");
        assert_eq!(cacheable.byte_range, 3..text.len());
        assert_eq!(cacheable.start_column, 3);
        assert_eq!(cacheable.line_range, 0..1);
    }

    #[test]
    fn test_from_region_info_applies_row_offset_for_frontmatter() {
        // The vendored markdown query trims `---` frontmatter delimiters via
        // (#offset! @injection.content 1 0 -1 0); the cacheable region must
        // reflect the effective (delimiter-free) range (#183).
        let md_language: tree_sitter::Language = tree_sitter_md::LANGUAGE.into();
        let mut parser = Parser::new();
        parser.set_language(&md_language).expect("set md language");
        let text = "---\ntitle: x\n---\n\n# heading\n";
        let tree = parser.parse(text, None).expect("parse markdown");
        let root = tree.root_node();

        let query_str = r#"
            ((minus_metadata) @injection.content
              (#set! injection.language "yaml")
              (#set! injection.include-children)
              (#offset! @injection.content 1 0 -1 0))
        "#;
        let query = Query::new(&md_language, query_str).expect("valid query");

        let regions = collect_all_injections(&root, text, Some(&query)).expect("injections");
        assert_eq!(regions.len(), 1);
        let cacheable = CacheableInjectionRegion::from_region_info(&regions[0], "test-id", text);

        assert_eq!(&text[cacheable.byte_range.clone()], "title: x\n");
        assert_eq!(cacheable.line_range, 1..2);
        assert_eq!(cacheable.start_column, 0);
    }

    #[test]
    fn test_resolved_injection_virtual_content_honors_offset() {
        // End-to-end through the bridge resolution path: the virtual document
        // sent downstream must contain only the effective (post-offset)
        // content, not the raw node text with delimiters (#183).
        let md_language: tree_sitter::Language = tree_sitter_md::LANGUAGE.into();
        let mut parser = Parser::new();
        parser.set_language(&md_language).expect("set md language");
        let text = "---\ntitle: x\n---\n\n# heading\n";
        let tree = parser.parse(text, None).expect("parse markdown");

        let query_str = r#"
            ((minus_metadata) @injection.content
              (#set! injection.language "yaml")
              (#set! injection.include-children)
              (#offset! @injection.content 1 0 -1 0))
        "#;
        let query = Query::new(&md_language, query_str).expect("valid query");

        let coordinator = test_coordinator();
        let tracker = NodeTracker::new();
        let uri = test_uri("offset_frontmatter");

        let resolved =
            InjectionResolver::resolve_all(&coordinator, &tracker, &uri, &tree, text, &query);
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].virtual_content, "title: x\n");
        assert_eq!(resolved[0].region.line_range.start, 1);
        assert_eq!(resolved[0].line_column_offsets, vec![0]);
    }

    #[test]
    fn test_resolved_virtual_content_combines_offset_with_child_exclusion() {
        // #186: a query with #offset! but WITHOUT injection.include-children.
        // Blockquote `> ` prefixes (block_continuation children) must still be
        // stripped, restricted to the post-offset window — previously the
        // child-exclusion step was skipped entirely whenever an offset was
        // active, leaking prefixes into the virtual document.
        let md_language: tree_sitter::Language = tree_sitter_md::LANGUAGE.into();
        let mut parser = Parser::new();
        parser.set_language(&md_language).expect("set md language");
        let text = "> ```lua\n> local a = 1\n> local b = 2\n> ```\n";
        let tree = parser.parse(text, None).expect("parse markdown");

        let query_str = r#"
            ((fenced_code_block
               (info_string (language) @injection.language)
               (code_fence_content) @injection.content)
              (#offset! @injection.content 1 0 0 0))
        "#;
        let query = Query::new(&md_language, query_str).expect("valid query");

        let coordinator = test_coordinator();
        let tracker = NodeTracker::new();
        let uri = test_uri("offset_blockquote");

        let resolved =
            InjectionResolver::resolve_all(&coordinator, &tracker, &uri, &tree, text, &query);
        assert_eq!(resolved.len(), 1);
        // The row offset trims the first content line; child exclusion strips
        // the remaining `> ` prefixes.
        assert_eq!(resolved[0].virtual_content, "local b = 2\n");
    }

    // ============================================================
    // Tests for InjectionResolver region_id generation
    // ============================================================

    use crate::language::node_tracker::NodeTracker;

    fn test_uri(name: &str) -> Url {
        Url::parse(&format!("file:///test/{}.rs", name)).unwrap()
    }

    fn test_coordinator() -> LanguageCoordinator {
        LanguageCoordinator::new()
    }

    #[test]
    fn test_resolve_injection_returns_ulid_format() {
        // Test that resolved injection has region_id in ULID format (26 chars)
        let mut parser = create_rust_parser();
        let text = r#"fn main() { let s = "hello"; }"#;
        let tree = parse_rust_code(&mut parser, text);

        let query_str = r#"
            ((string_literal) @injection.content
              (#set! injection.language "lua"))
        "#;
        let language = tree_sitter_rust::LANGUAGE.into();
        let query = Query::new(&language, query_str).expect("valid query");

        let coordinator = test_coordinator();
        let tracker = NodeTracker::new();
        let uri = test_uri("ulid_format");

        // Resolve injection at byte offset inside the string literal
        let resolved = InjectionResolver::resolve_at_byte_offset(
            &coordinator,
            &tracker,
            &uri,
            &tree,
            text,
            &query,
            22,
        );
        assert!(resolved.is_some(), "Should resolve injection");
        let region_id = resolved.unwrap().region.region_id;
        assert_eq!(
            region_id.len(),
            26,
            "ULID should be 26 characters, got: {}",
            region_id
        );
    }

    #[test]
    fn test_resolve_injection_multiple_regions_stable_ulids() {
        // Test that multiple injection regions get stable ULIDs for same ordinal
        let mut parser = create_rust_parser();
        let text = r#"fn main() { let a = "hello"; let b = "world"; let c = "test"; }"#;
        let tree = parse_rust_code(&mut parser, text);

        let query_str = r#"
            ((string_literal) @injection.content
              (#set! injection.language "lua"))
        "#;
        let language = tree_sitter_rust::LANGUAGE.into();
        let query = Query::new(&language, query_str).expect("valid query");

        let coordinator = test_coordinator();
        let tracker = NodeTracker::new();
        let uri = test_uri("multiple");

        // Find byte offsets for each string
        let query_all = Query::new(&language, r#"(string_literal) @str"#).expect("valid query");
        let mut cursor = tree_sitter::QueryCursor::new();
        let mut matches_iter = cursor.matches(&query_all, tree.root_node(), text.as_bytes());
        let mut byte_offsets = Vec::new();
        while let Some(m) = matches_iter.next() {
            byte_offsets.push(m.captures[0].node.start_byte() + 1);
        }
        assert_eq!(byte_offsets.len(), 3, "Should find 3 strings");

        // Resolve each injection
        let r1 = InjectionResolver::resolve_at_byte_offset(
            &coordinator,
            &tracker,
            &uri,
            &tree,
            text,
            &query,
            byte_offsets[0],
        );
        let r2 = InjectionResolver::resolve_at_byte_offset(
            &coordinator,
            &tracker,
            &uri,
            &tree,
            text,
            &query,
            byte_offsets[1],
        );
        let r3 = InjectionResolver::resolve_at_byte_offset(
            &coordinator,
            &tracker,
            &uri,
            &tree,
            text,
            &query,
            byte_offsets[2],
        );

        // Each should have different ULIDs (different ordinals)
        let id1 = r1.unwrap().region.region_id;
        let id2 = r2.unwrap().region.region_id;
        let id3 = r3.unwrap().region.region_id;
        assert_ne!(id1, id2, "Different ordinals should have different ULIDs");
        assert_ne!(id2, id3, "Different ordinals should have different ULIDs");
        assert_ne!(id1, id3, "Different ordinals should have different ULIDs");
    }

    #[test]
    fn test_resolve_injection_same_position_returns_consistent_region_id() {
        // Test that resolving the same position returns consistent region_id
        let mut parser = create_rust_parser();
        let text = r#"fn main() { let s = "hello"; }"#;
        let tree = parse_rust_code(&mut parser, text);

        let query_str = r#"
            ((string_literal) @injection.content
              (#set! injection.language "lua"))
        "#;
        let language = tree_sitter_rust::LANGUAGE.into();
        let query = Query::new(&language, query_str).expect("valid query");

        let coordinator = test_coordinator();
        let tracker = NodeTracker::new();
        let uri = test_uri("consistent");

        // Resolve the same position multiple times
        let byte_offset = 22;
        let r1 = InjectionResolver::resolve_at_byte_offset(
            &coordinator,
            &tracker,
            &uri,
            &tree,
            text,
            &query,
            byte_offset,
        );
        let r2 = InjectionResolver::resolve_at_byte_offset(
            &coordinator,
            &tracker,
            &uri,
            &tree,
            text,
            &query,
            byte_offset,
        );

        assert_eq!(
            r1.unwrap().region.region_id,
            r2.unwrap().region.region_id,
            "Same position should return same region_id"
        );
    }

    #[test]
    fn test_calculate_region_id_different_positions_different_ulids() {
        // Test that different injection positions produce different ULIDs
        // Phase 2: Uses position-based keys (start_byte, end_byte, kind)
        let mut parser = create_rust_parser();
        let text = r#"fn main() { let a = "lua1"; let b = "python"; let c = "lua2"; }"#;
        let tree = parse_rust_code(&mut parser, text);
        let root = tree.root_node();

        let mut cursor = tree_sitter::QueryCursor::new();
        let query_str = r#"(string_literal) @str"#;
        let language = tree_sitter_rust::LANGUAGE.into();
        let query = Query::new(&language, query_str).expect("valid query");

        let mut matches_iter = cursor.matches(&query, root, text.as_bytes());
        let mut nodes = Vec::new();
        while let Some(m) = matches_iter.next() {
            nodes.push(m.captures[0].node);
        }
        assert_eq!(nodes.len(), 3, "Should find 3 strings");

        // Create injection regions manually: lua, python, lua
        let injections = [
            InjectionRegionInfo {
                language: "lua".to_string(),
                content_node: nodes[0],
                pattern_index: 0,
                include_children: false,
                offset: None,
            },
            InjectionRegionInfo {
                language: "python".to_string(),
                content_node: nodes[1],
                pattern_index: 0,
                include_children: false,
                offset: None,
            },
            InjectionRegionInfo {
                language: "lua".to_string(),
                content_node: nodes[2],
                pattern_index: 0,
                include_children: false,
                offset: None,
            },
        ];

        let tracker = NodeTracker::new();
        let uri = test_uri("mixed");

        // Phase 2: calculate_region_id uses position-based keys (not ordinals)
        // Different positions → different ULIDs regardless of language
        let ulid_0 = InjectionResolver::calculate_region_id(&tracker, &uri, &injections[0]);
        let ulid_1 = InjectionResolver::calculate_region_id(&tracker, &uri, &injections[1]);
        let ulid_2 = InjectionResolver::calculate_region_id(&tracker, &uri, &injections[2]);

        // All different because they have different byte positions
        assert_ne!(
            ulid_0, ulid_1,
            "Different positions should have different ULIDs"
        );
        assert_ne!(
            ulid_1, ulid_2,
            "Different positions should have different ULIDs"
        );
        assert_ne!(
            ulid_0, ulid_2,
            "Different positions should have different ULIDs"
        );

        // Same position returns same ULID (stability)
        let ulid_0_again = InjectionResolver::calculate_region_id(&tracker, &uri, &injections[0]);
        assert_eq!(
            ulid_0, ulid_0_again,
            "Same position key should return same ULID"
        );
    }

    #[test]
    fn test_find_injection_at_position_returns_correct_region_and_index() {
        // Test that find_injection_at_position returns the correct region and its index
        // for use with calculate_region_id

        let mut parser = create_rust_parser();
        let text = r#"fn main() { let a = "lua1"; let b = "py"; let c = "lua2"; }"#;
        let tree = parse_rust_code(&mut parser, text);
        let root = tree.root_node();

        // Find all string_literal nodes
        let mut cursor = tree_sitter::QueryCursor::new();
        let query_str = r#"(string_literal) @str"#;
        let language = tree_sitter_rust::LANGUAGE.into();
        let query = Query::new(&language, query_str).expect("valid query");

        let mut matches_iter = cursor.matches(&query, root, text.as_bytes());
        let mut nodes = Vec::new();
        while let Some(m) = matches_iter.next() {
            nodes.push(m.captures[0].node);
        }

        assert_eq!(nodes.len(), 3, "Should find 3 strings");

        // Create injection regions: lua, python, lua
        let injections = vec![
            InjectionRegionInfo {
                language: "lua".to_string(),
                content_node: nodes[0],
                pattern_index: 0,
                include_children: false,
                offset: None,
            },
            InjectionRegionInfo {
                language: "python".to_string(),
                content_node: nodes[1],
                pattern_index: 0,
                include_children: false,
                offset: None,
            },
            InjectionRegionInfo {
                language: "lua".to_string(),
                content_node: nodes[2],
                pattern_index: 0,
                include_children: false,
                offset: None,
            },
        ];

        // Test finding position inside first Lua block
        let lua1_byte = nodes[0].start_byte() + 1; // Inside first string
        let result = find_injection_at_position(&injections, lua1_byte);
        assert!(result.is_some(), "Should find injection at lua1 position");
        let (idx, region) = result.unwrap();
        assert_eq!(idx, 0, "Should be at index 0");
        assert_eq!(region.language, "lua", "Should be lua region");

        // Test finding position inside Python block
        let py_byte = nodes[1].start_byte() + 1;
        let result = find_injection_at_position(&injections, py_byte);
        assert!(result.is_some(), "Should find injection at python position");
        let (idx, region) = result.unwrap();
        assert_eq!(idx, 1, "Should be at index 1");
        assert_eq!(region.language, "python", "Should be python region");

        // Test finding position inside second Lua block
        let lua2_byte = nodes[2].start_byte() + 1;
        let result = find_injection_at_position(&injections, lua2_byte);
        assert!(result.is_some(), "Should find injection at lua2 position");
        let (idx, region) = result.unwrap();
        assert_eq!(idx, 2, "Should be at index 2");
        assert_eq!(region.language, "lua", "Should be lua region");

        // Test position outside all injections
        let outside_byte = 5; // Position before any string
        let result = find_injection_at_position(&injections, outside_byte);
        assert!(
            result.is_none(),
            "Should not find injection outside regions"
        );
    }

    #[test]
    fn test_collect_all_injections_respects_lua_match_predicate() {
        // Regression test: #lua-match? is a general predicate (not built-in to tree-sitter).
        // collect_all_injections must apply predicate filtering so that injection rules
        // guarded by #lua-match? only match when the predicate actually passes.
        //
        // Without filtering, a rule like:
        //   (string content: _ @injection.content (#lua-match? @injection.content "^;") (#set! injection.language "query"))
        // would match ALL strings, not just those starting with ";".
        let mut parser = create_rust_parser();
        let text = r#"fn main() { let a = "hello"; let b = "; query"; }"#;
        let tree = parse_rust_code(&mut parser, text);
        let root = tree.root_node();

        // Injection query with #lua-match? predicate:
        // Only strings starting with ";" should be injected as "query"
        let query_str = r#"
            ((string_literal
                (string_content) @injection.content)
              (#lua-match? @injection.content "^;")
              (#set! injection.language "query"))
        "#;

        let language = tree_sitter_rust::LANGUAGE.into();
        let query = Query::new(&language, query_str).expect("valid query");

        let injections =
            collect_all_injections(&root, text, Some(&query)).expect("Should return Some");

        // Only "; query" should match, not "hello"
        assert_eq!(
            injections.len(),
            1,
            "Only strings matching #lua-match? should be injected, got: {:?}",
            injections
                .iter()
                .map(|i| &text[i.content_node.start_byte()..i.content_node.end_byte()])
                .collect::<Vec<_>>()
        );
        let content =
            &text[injections[0].content_node.start_byte()..injections[0].content_node.end_byte()];
        assert!(
            content.starts_with(';'),
            "Injected content should start with ';', got: {:?}",
            content
        );
    }

    // ============================================================
    // Tests for line_range edge cases in CacheableInjectionRegion
    // ============================================================

    /// Helper: parse `text` with tree-sitter Rust, match `string_content` nodes
    /// via injection query, and return the `CacheableInjectionRegion` for the first match.
    fn cacheable_from_first_injection(text: &str) -> CacheableInjectionRegion {
        let mut parser = create_rust_parser();
        let tree = parse_rust_code(&mut parser, text);
        let root = tree.root_node();

        let query_str = r#"
            ((string_literal
              (string_content) @injection.content)
             (#set! injection.language "test"))
        "#;
        let language = tree_sitter_rust::LANGUAGE.into();
        let query = Query::new(&language, query_str).expect("valid query");

        let regions =
            collect_all_injections(&root, text, Some(&query)).expect("should find injections");
        assert!(!regions.is_empty(), "expected at least one injection");

        CacheableInjectionRegion::from_region_info(&regions[0], "test-id", text)
    }

    #[rstest]
    #[case::single_line_no_trailing_newline(
        // "hello" sits entirely on row 0; no trailing newline → exclusive end = 1
        r#"let s = "hello";"#,
        0..1,
    )]
    #[case::multi_line_trailing_newline(
        // string_content starts at the byte after `"` on row 0; the content
        // ends with `\n` so end_position().column == 0 at row 4 → exclusive end = 4.
        "let s = \"\nline1\nline2\nline3\n\";",
        0..4,
    )]
    #[case::multi_line_no_trailing_newline(
        // string_content starts on row 0; last line has content (no trailing \n),
        // so end_position().column > 0 at row 2 → exclusive end = 3.
        "let s = \"\nline1\nline2\";",
        0..3,
    )]
    #[trace]
    fn test_line_range_edge_cases(
        #[case] text: &str,
        #[case] expected_line_range: std::ops::Range<u32>,
    ) {
        let cacheable = cacheable_from_first_injection(text);
        assert_eq!(
            cacheable.line_range, expected_line_range,
            "line_range mismatch for text: {:?}",
            text
        );
    }

    #[test]
    fn test_has_include_children_for_pattern_without_property() {
        // Pattern 0 has NO #set! injection.include-children
        let query_str = r#"
            ((raw_string_literal) @injection.content
              (#set! injection.language "regex"))
        "#;
        let language = tree_sitter_rust::LANGUAGE.into();
        let query = Query::new(&language, query_str).expect("valid query");

        assert!(
            !has_include_children_for_pattern(&query, 0),
            "Pattern without #set! injection.include-children should return false"
        );
    }

    #[test]
    fn test_has_include_children_for_pattern_with_property() {
        // Pattern has #set! injection.include-children (value-less property)
        let query_str = r#"
            ((line_comment) @injection.content
              (#set! injection.language "html")
              (#set! injection.include-children))
        "#;
        let language = tree_sitter_rust::LANGUAGE.into();
        let query = Query::new(&language, query_str).expect("valid query");

        assert!(
            has_include_children_for_pattern(&query, 0),
            "Pattern with #set! injection.include-children should return true"
        );
    }

    #[test]
    fn test_has_include_children_multi_pattern() {
        // Two patterns: first without, second with include-children
        let query_str = r#"
            ; Pattern 0: NO include-children
            ((raw_string_literal) @injection.content
              (#set! injection.language "regex"))

            ; Pattern 1: HAS include-children
            ((line_comment) @injection.content
              (#set! injection.language "yaml")
              (#set! injection.include-children))
        "#;
        let language = tree_sitter_rust::LANGUAGE.into();
        let query = Query::new(&language, query_str).expect("valid query");

        assert!(
            !has_include_children_for_pattern(&query, 0),
            "Pattern 0 should not have include-children"
        );
        assert!(
            has_include_children_for_pattern(&query, 1),
            "Pattern 1 should have include-children"
        );
    }

    #[test]
    fn test_has_combined_for_pattern() {
        // Pattern 0 has no combined property; pattern 1 mirrors the vendored
        // markdown html_block pattern shape.
        let query_str = r#"
            ((raw_string_literal) @injection.content
              (#set! injection.language "regex"))

            ((line_comment) @injection.content
              (#set! injection.language "html")
              (#set! injection.combined)
              (#set! injection.include-children))
        "#;
        let language = tree_sitter_rust::LANGUAGE.into();
        let query = Query::new(&language, query_str).expect("valid query");

        assert!(
            !has_combined_for_pattern(&query, 0),
            "Pattern without #set! injection.combined should return false"
        );
        assert!(
            has_combined_for_pattern(&query, 1),
            "Pattern with #set! injection.combined should return true"
        );
    }

    #[test]
    fn test_compute_included_ranges_returns_none_when_include_children() {
        // When include_children is true, all content is included (no restriction)
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        let tree = parser.parse("fn main() {}", None).unwrap();
        let root = tree.root_node();

        assert!(
            compute_included_ranges(&root, true).is_none(),
            "include_children=true should return None"
        );
    }

    #[test]
    fn test_compute_included_ranges_returns_none_for_no_named_children() {
        // A leaf node (identifier) has no named children
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        let tree = parser.parse("x", None).unwrap();
        let root = tree.root_node();

        // Find identifier leaf node
        let ident = root
            .named_descendant_for_byte_range(0, 1)
            .expect("should find node");
        assert_eq!(ident.named_child_count(), 0);

        assert!(
            compute_included_ranges(&ident, false).is_none(),
            "Node with no named children should return None"
        );
    }

    #[test]
    fn test_compute_included_ranges_computes_gap_ranges() {
        // Use a node that has named children with gaps between them.
        // In Rust, a function_item has named children (name, parameters, body)
        // with gaps (the "fn" keyword, whitespace, etc.)
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        let tree = parser.parse("fn main() {}", None).unwrap();
        let root = tree.root_node();

        // function_item is the top-level node
        let func = root.named_child(0).expect("should have function_item");
        let func_start = func.start_byte();
        let func_size = func.end_byte() - func_start;

        let ranges = compute_included_ranges(&func, false);
        let ranges = ranges.expect("Node with named children should produce gap ranges");
        assert!(
            !ranges.is_empty(),
            "Should have at least one gap range between named children"
        );

        // Collect named children byte ranges (relative to func start)
        let mut tree_cursor = func.walk();
        let child_ranges: Vec<(usize, usize)> = func
            .named_children(&mut tree_cursor)
            .map(|c| (c.start_byte() - func_start, c.end_byte() - func_start))
            .collect();

        // Gap ranges must not overlap any named child
        for r in &ranges {
            assert!(
                r.start_byte < r.end_byte,
                "Range should be non-empty: {r:?}"
            );
            assert!(r.end_byte <= func_size, "Range exceeds node bounds: {r:?}");
            for (cs, ce) in &child_ranges {
                assert!(
                    r.end_byte <= *cs || r.start_byte >= *ce,
                    "Gap range {r:?} overlaps named child [{cs}, {ce})"
                );
            }
        }

        // Gap ranges + named child ranges should cover the entire node
        let mut covered = vec![false; func_size];
        for r in &ranges {
            for c in covered[r.start_byte..r.end_byte].iter_mut() {
                *c = true;
            }
        }
        for (cs, ce) in &child_ranges {
            for c in covered[*cs..*ce].iter_mut() {
                *c = true;
            }
        }
        assert!(
            covered.iter().all(|&b| b),
            "Gaps + children should cover the entire node"
        );
    }

    #[test]
    fn test_byte_to_point_anchored_matches_full_scan() {
        // Exhaustive equivalence with the full-prefix scan, covering the
        // before-anchor fallback, same-row and cross-row targets, and
        // mid-codepoint clamping (bytes inside あ floor to its start).
        let text = "abc\ndefあ\nghi";
        let anchor_byte = 4; // start of "def"
        let anchor_point = byte_to_point(text, anchor_byte);
        for byte in 0..=text.len() {
            assert_eq!(
                byte_to_point_anchored(text, byte, anchor_byte, anchor_point),
                byte_to_point(text, byte),
                "anchored and full scans must agree at byte {byte}"
            );
        }
    }

    #[test]
    fn test_compute_included_ranges_clipped_full_window_matches_unclipped() {
        let mut parser = create_rust_parser();
        let text = "fn main() {}";
        let tree = parser.parse(text, None).unwrap();
        let func = tree.root_node().named_child(0).expect("function_item");

        let unclipped = compute_included_ranges(&func, false).expect("named children produce gaps");
        let clipped =
            compute_included_ranges_clipped(&func, false, text, func.start_byte()..func.end_byte())
                .expect("full window should produce the same gaps");

        assert_eq!(
            clipped, unclipped,
            "window covering the whole node must reproduce compute_included_ranges"
        );
    }

    #[test]
    fn test_compute_included_ranges_clipped_drops_children_before_window() {
        // "fn main() {}": named children of function_item are
        // main(3..7), parameters(7..9), body(10..12); gaps are [0..3) and [9..10).
        let mut parser = create_rust_parser();
        let text = "fn main() {}";
        let tree = parser.parse(text, None).unwrap();
        let func = tree.root_node().named_child(0).expect("function_item");

        // Window starts inside `main` (3..7): the straddling child is clipped,
        // the "fn " gap disappears, and only the " " gap [9..10) remains,
        // relative to the window start.
        let ranges = compute_included_ranges_clipped(&func, false, text, 4..12)
            .expect("gap inside window should survive");

        assert_eq!(ranges.len(), 1);
        assert_eq!((ranges[0].start_byte, ranges[0].end_byte), (5, 6));
        assert_eq!(
            ranges[0].start_point,
            tree_sitter::Point { row: 0, column: 5 }
        );
        assert_eq!(
            ranges[0].end_point,
            tree_sitter::Point { row: 0, column: 6 }
        );
    }

    #[test]
    fn test_compute_included_ranges_clipped_truncates_at_window_end() {
        let mut parser = create_rust_parser();
        let text = "fn main() {}";
        let tree = parser.parse(text, None).unwrap();
        let func = tree.root_node().named_child(0).expect("function_item");

        // Window ends inside the body (10..12): both gaps survive, and no
        // spurious gap is emitted after the clipped body.
        let ranges = compute_included_ranges_clipped(&func, false, text, 0..11)
            .expect("gaps inside window should survive");

        let bytes: Vec<(usize, usize)> =
            ranges.iter().map(|r| (r.start_byte, r.end_byte)).collect();
        assert_eq!(bytes, vec![(0, 3), (9, 10)]);
    }

    #[test]
    fn test_compute_included_ranges_clipped_returns_none_when_children_cover_window() {
        let mut parser = create_rust_parser();
        let text = "fn main() {}";
        let tree = parser.parse(text, None).unwrap();
        let func = tree.root_node().named_child(0).expect("function_item");

        // Window [10..12) lies entirely inside the body child: no gaps.
        assert!(
            compute_included_ranges_clipped(&func, false, text, 10..12).is_none(),
            "window fully covered by a child has no gaps"
        );
    }

    #[test]
    fn test_compute_included_ranges_clipped_relativizes_points_to_window_start() {
        // Two statements on separate lines; gaps are the newlines [4..5) and [9..10).
        let mut parser = create_rust_parser();
        let text = "a();\nb();\n";
        let tree = parser.parse(text, None).unwrap();
        let root = tree.root_node();

        // Window starts at byte 6 = row 1, column 1 (inside `b();`).
        let ranges = compute_included_ranges_clipped(&root, false, text, 6..10)
            .expect("trailing newline gap should survive");

        assert_eq!(ranges.len(), 1);
        // Gap [9..10) relative to window start 6.
        assert_eq!((ranges[0].start_byte, ranges[0].end_byte), (3, 4));
        // Same row as window start: column is relative (4 - 1 = 3).
        assert_eq!(
            ranges[0].start_point,
            tree_sitter::Point { row: 0, column: 3 }
        );
        // Next row: column stays absolute.
        assert_eq!(
            ranges[0].end_point,
            tree_sitter::Point { row: 1, column: 0 }
        );
    }

    #[test]
    fn test_blockquote_bridge_content_strips_prefixes() {
        // Verify that extract_clean_content strips blockquote prefixes so that
        // downstream language servers receive parseable code.
        let text = "> ```lua\n> local x = 1\n> local y = 2\n> ```\n";
        let (byte_range, _start_column, included_ranges) = blockquote_injection_data(text);

        let clean = extract_clean_content(text, byte_range, included_ranges.as_deref());
        assert!(
            !clean.contains("> "),
            "Clean content should NOT contain blockquote prefixes: {:?}",
            clean
        );
        assert!(
            clean.contains("local x = 1"),
            "Clean content should contain the code: {:?}",
            clean
        );
        assert!(
            clean.contains("local y = 2"),
            "Clean content should contain the code: {:?}",
            clean
        );
    }

    // ============================================================
    // Tests for extract_clean_content and compute_line_column_offsets
    // ============================================================

    /// Helper to set up a markdown parser and injection query, parse text,
    /// and return the first injection's content node byte range and included ranges.
    fn blockquote_injection_data(
        text: &str,
    ) -> (std::ops::Range<usize>, u32, Option<Vec<tree_sitter::Range>>) {
        let mut parser = Parser::new();
        let md_language: tree_sitter::Language = tree_sitter_md::LANGUAGE.into();
        parser
            .set_language(&md_language)
            .expect("load markdown grammar");

        let injection_query_str = r#"
            (fenced_code_block
              (info_string (language) @injection.language)
              (code_fence_content) @injection.content)
        "#;
        let injection_query =
            Query::new(&md_language, injection_query_str).expect("valid injection query");

        let tree = parser.parse(text, None).expect("parse markdown");
        let root = tree.root_node();

        let injections = collect_all_injections(&root, text, Some(&injection_query))
            .expect("Should find injections");
        assert!(!injections.is_empty(), "Should find at least one injection");

        let region = &injections[0];
        let byte_range = region.content_node.byte_range();
        let included_ranges =
            compute_included_ranges(&region.content_node, region.include_children);

        // Compute start_column (UTF-16)
        let byte_column = region.content_node.start_position().column;
        let line_start_byte = region.content_node.start_byte() - byte_column;
        let line_prefix = &text[line_start_byte..region.content_node.start_byte()];
        let start_column = line_prefix.encode_utf16().count() as u32;

        (byte_range, start_column, included_ranges)
    }

    #[test]
    fn extract_clean_content_without_ranges_returns_full_text() {
        // When called with None for included_ranges, returns the full content slice
        let text = "hello world";
        let clean = extract_clean_content(text, 0..text.len(), None);
        assert_eq!(clean, text);
    }

    #[test]
    fn extract_clean_content_with_ranges_strips_prefixes() {
        // Blockquote case: included_ranges present → only gap content
        let text = "> ```lua\n> local x = 1\n> local y = 2\n> ```\n";
        let (byte_range, _start_column, included_ranges) = blockquote_injection_data(text);
        assert!(
            included_ranges.is_some(),
            "Blockquote should have included ranges"
        );

        let clean = extract_clean_content(text, byte_range, included_ranges.as_deref());
        assert!(
            !clean.contains("> "),
            "Clean content should not contain blockquote prefixes: {:?}",
            clean
        );
        assert!(
            clean.contains("local x = 1"),
            "Clean content should contain the code: {:?}",
            clean
        );
        assert!(
            clean.contains("local y = 2"),
            "Clean content should contain the code: {:?}",
            clean
        );
    }

    #[test]
    fn compute_line_column_offsets_without_ranges_returns_start_column() {
        // When called with None for included_ranges, returns vec![start_column]
        let offsets = compute_line_column_offsets("hello", 0..5, 4, None);
        assert_eq!(offsets, vec![4]);
    }

    #[test]
    fn compute_line_column_offsets_with_ranges_returns_per_line() {
        // Blockquote case: "> " prefix on each line → 2 UTF-16 units per line
        let text = "> ```lua\n> local x = 1\n> local y = 2\n> ```\n";
        let (byte_range, start_column, included_ranges) = blockquote_injection_data(text);
        assert!(included_ranges.is_some());

        let offsets =
            compute_line_column_offsets(text, byte_range, start_column, included_ranges.as_deref());
        // Virtual lines 0 and 1 contain actual code after "> " prefixes.
        // Line 2 may be trailing content (the "> " before "```") — its offset
        // is 0 because there's no gap range starting on that line.
        assert!(
            offsets.len() >= 2,
            "Should have at least 2 line offsets: {:?}",
            offsets
        );
        assert_eq!(
            offsets[0], 2,
            "Line 0 should have offset 2 (for '> ' prefix)"
        );
        assert_eq!(
            offsets[1], 2,
            "Line 1 should have offset 2 (for '> ' prefix)"
        );
    }

    /// Tree-sitter assigns column positions from Range.start_point, not byte offset.
    ///
    /// When `set_included_ranges` is called with ranges whose `start_point.column = 2`,
    /// tree-sitter reports parsed nodes at column 2 — not column 0 — even though the
    /// bytes start at offset 2 within the raw text.
    ///
    /// This is the invariant that makes blockquote injection work correctly:
    /// `compute_included_ranges` builds ranges with `start_point.column = prefix_len`
    /// (e.g. 2 for `> `), so injected keywords appear at their true host column.
    #[test]
    fn test_parse_with_included_ranges_preserves_start_point_column() {
        // Simulate "> let x = 1;\n> let y = 2;\n" with included_ranges skipping `> `
        // Line 0: "> let x = 1;\n" = 13 bytes (0..13, \n at byte 12)
        // Line 1: "> let y = 2;\n" = 13 bytes (13..26, \n at byte 25)
        // Content ranges (skipping 2-byte `> ` prefix on each line):
        //   Range 0: bytes  2..13 = "let x = 1;\n", start_point {row:0, col:2}
        //   Range 1: bytes 15..26 = "let y = 2;\n", start_point {row:1, col:2}
        let content_text = "> let x = 1;\n> let y = 2;\n";
        let included_ranges = vec![
            tree_sitter::Range {
                start_byte: 2,
                end_byte: 13,
                start_point: tree_sitter::Point { row: 0, column: 2 },
                end_point: tree_sitter::Point { row: 1, column: 0 },
            },
            tree_sitter::Range {
                start_byte: 15,
                end_byte: 26,
                start_point: tree_sitter::Point { row: 1, column: 2 },
                end_point: tree_sitter::Point { row: 2, column: 0 },
            },
        ];

        let rust_language: tree_sitter::Language = tree_sitter_rust::LANGUAGE.into();
        let mut parser = Parser::new();
        parser.set_language(&rust_language).unwrap();
        let tree = parse_with_ranges(
            &mut parser,
            content_text,
            Some(&included_ranges),
            "test",
            "rust",
        )
        .expect("should parse");

        let query = Query::new(&rust_language, "(let_declaration) @decl").unwrap();
        let mut cursor = tree_sitter::QueryCursor::new();
        let mut matches = cursor.matches(&query, tree.root_node(), content_text.as_bytes());

        let mut let_columns: Vec<usize> = Vec::new();
        while let Some(m) = matches.next() {
            for c in m.captures {
                let node = c.node;
                let mut walk = node.walk();
                for child in node.children(&mut walk) {
                    if child.kind() == "let" {
                        let_columns.push(child.start_position().column);
                    }
                }
            }
        }

        assert_eq!(
            let_columns,
            vec![2, 2],
            "Both 'let' keywords should be at column 2 (from Range.start_point), not 0"
        );
    }

    // ─── Tests for sub_select_included_ranges ────────────────────────────

    /// Helper to build a tree_sitter::Range for test fixtures.
    fn make_range(
        start_byte: usize,
        end_byte: usize,
        start_row: usize,
        start_col: usize,
        end_row: usize,
        end_col: usize,
    ) -> tree_sitter::Range {
        tree_sitter::Range {
            start_byte,
            end_byte,
            start_point: tree_sitter::Point {
                row: start_row,
                column: start_col,
            },
            end_point: tree_sitter::Point {
                row: end_row,
                column: end_col,
            },
        }
    }

    #[test]
    fn test_sub_select_included_ranges_basic() {
        // Parent has 3 ranges spanning 3 rows (simulating "> " prefix on each line).
        // Each line is ">" (1 byte) + " " (1 byte) + content.
        // Line 0: "> abc\n"  → bytes 0..6, gap at 2..6, row 0 col 2
        // Line 1: "> def\n"  → bytes 6..12, gap at 8..12, row 1 col 2
        // Line 2: "> ghi\n"  → bytes 12..18, gap at 14..18, row 2 col 2
        let parent_ranges = vec![
            make_range(2, 6, 0, 2, 0, 6),
            make_range(8, 12, 1, 2, 1, 6),
            make_range(14, 18, 2, 2, 2, 6),
        ];

        // Nested region covers rows 1-2 (bytes 8..18 in parent coordinates)
        let result = sub_select_included_ranges(&parent_ranges, 8, 18);

        assert!(
            result.is_some(),
            "Should return Some for overlapping ranges"
        );
        let ranges = result.unwrap();
        assert_eq!(ranges.len(), 2, "Should have 2 ranges for rows 1-2");

        // First range: re-relativized from parent byte 8..12 → nested byte 0..4
        // First range column is 0 (prefix bytes are outside nested content_text)
        assert_eq!(ranges[0].start_byte, 0);
        assert_eq!(ranges[0].end_byte, 4);
        assert_eq!(ranges[0].start_point.row, 0);
        assert_eq!(ranges[0].start_point.column, 0); // first range: no prefix in nested content
        assert_eq!(ranges[0].end_point.row, 0);
        assert_eq!(ranges[0].end_point.column, 6);

        // Second range: re-relativized from parent byte 14..18 → nested byte 6..10
        assert_eq!(ranges[1].start_byte, 6);
        assert_eq!(ranges[1].end_byte, 10);
        assert_eq!(ranges[1].start_point.row, 1);
        assert_eq!(ranges[1].start_point.column, 2);
        assert_eq!(ranges[1].end_point.row, 1);
        assert_eq!(ranges[1].end_point.column, 6);
    }

    #[test]
    fn test_sub_select_included_ranges_no_overlap() {
        // Parent ranges span bytes 0..6
        let parent_ranges = vec![make_range(2, 6, 0, 2, 0, 6)];

        // Nested region is completely outside parent ranges
        let result = sub_select_included_ranges(&parent_ranges, 10, 20);

        assert!(
            result.is_none(),
            "Should return None for non-overlapping ranges"
        );
    }

    #[test]
    fn test_sub_select_included_ranges_partial_overlap() {
        // Parent range: bytes 2..10 on row 0
        let parent_ranges = vec![make_range(2, 10, 0, 2, 0, 10)];

        // Nested region starts at byte 5 (mid-range) and ends at byte 15 (past range)
        let result = sub_select_included_ranges(&parent_ranges, 5, 15);

        assert!(
            result.is_some(),
            "Should return Some for partially overlapping range"
        );
        let ranges = result.unwrap();
        assert_eq!(ranges.len(), 1);

        // Clipped: start clamped to 5, end clamped to 10
        // Re-relativized: 5-5=0 start, 10-5=5 end
        assert_eq!(ranges[0].start_byte, 0);
        assert_eq!(ranges[0].end_byte, 5);
        assert_eq!(ranges[0].start_point.row, 0);
        assert_eq!(ranges[0].start_point.column, 0); // first range: column zeroed
    }

    // ─── Tests for intersect_included_ranges ─────────────────────────────

    #[test]
    fn test_intersect_included_ranges_identical() {
        let ranges = vec![make_range(0, 4, 0, 0, 0, 4), make_range(6, 10, 1, 2, 1, 6)];
        let result = intersect_included_ranges(&ranges, &ranges);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].start_byte, 0);
        assert_eq!(result[0].end_byte, 4);
        assert_eq!(result[1].start_byte, 6);
        assert_eq!(result[1].end_byte, 10);
    }

    #[test]
    fn test_intersect_included_ranges_no_overlap() {
        let a = vec![make_range(0, 4, 0, 0, 0, 4)];
        let b = vec![make_range(6, 10, 1, 2, 1, 6)];
        let result = intersect_included_ranges(&a, &b);
        assert!(result.is_empty());
    }

    #[test]
    fn test_intersect_included_ranges_partial_overlap() {
        // a: [2..8], b: [5..12]
        // intersection: [5..8]
        let a = vec![make_range(2, 8, 0, 2, 0, 8)];
        let b = vec![make_range(5, 12, 0, 5, 0, 12)];
        let result = intersect_included_ranges(&a, &b);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].start_byte, 5);
        assert_eq!(result[0].end_byte, 8);
        // start_byte == b's start → use b's start_point
        assert_eq!(result[0].start_point.column, 5);
        // end_byte == a's end → use a's end_point
        assert_eq!(result[0].end_point.column, 8);
    }

    #[test]
    fn test_intersect_included_ranges_one_contains_other() {
        // a: [0..20], b: [5..10]
        // intersection: [5..10]
        let a = vec![make_range(0, 20, 0, 0, 0, 20)];
        let b = vec![make_range(5, 10, 0, 5, 0, 10)];
        let result = intersect_included_ranges(&a, &b);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].start_byte, 5);
        assert_eq!(result[0].end_byte, 10);
    }

    #[test]
    fn test_intersect_included_ranges_multiple_overlaps() {
        // a: [0..10, 20..30]
        // b: [5..25]
        // intersections: [5..10, 20..25]
        let a = vec![
            make_range(0, 10, 0, 0, 0, 10),
            make_range(20, 30, 2, 0, 2, 10),
        ];
        let b = vec![make_range(5, 25, 0, 5, 2, 5)];
        let result = intersect_included_ranges(&a, &b);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].start_byte, 5);
        assert_eq!(result[0].end_byte, 10);
        assert_eq!(result[1].start_byte, 20);
        assert_eq!(result[1].end_byte, 25);
    }

    #[test]
    fn test_intersect_included_ranges_empty_inputs() {
        let ranges = vec![make_range(0, 4, 0, 0, 0, 4)];
        assert!(intersect_included_ranges(&[], &ranges).is_empty());
        assert!(intersect_included_ranges(&ranges, &[]).is_empty());
        assert!(intersect_included_ranges(&[], &[]).is_empty());
    }

    #[test]
    fn test_intersect_included_ranges_adjacent_non_overlapping() {
        // a: [0..5], b: [5..10] — touching but not overlapping
        let a = vec![make_range(0, 5, 0, 0, 0, 5)];
        let b = vec![make_range(5, 10, 0, 5, 0, 10)];
        let result = intersect_included_ranges(&a, &b);
        assert!(result.is_empty(), "Adjacent ranges should not intersect");
    }

    #[test]
    fn collects_injected_languages_from_markdown_code_blocks() {
        use std::collections::HashSet;
        use tree_sitter::Query;

        let markdown_text = r#"# Example

```lua
print("Hello from Lua")
local x = 42
```

Some text.

```python
def hello():
    print("Hello from Python")
```

```lua
local y = "duplicate"
```
"#;

        let mut parser = Parser::new();
        let md_language: tree_sitter::Language = tree_sitter_md::LANGUAGE.into();
        parser.set_language(&md_language).expect("set markdown");
        let tree = parser.parse(markdown_text, None).expect("parse markdown");
        let root = tree.root_node();

        let injection_query_str = r#"
            (fenced_code_block
              (info_string
                (language) @injection.language)
              (code_fence_content) @injection.content)
        "#;
        let injection_query =
            Query::new(&md_language, injection_query_str).expect("valid injection query");

        let injections = collect_all_injections(&root, markdown_text, Some(&injection_query))
            .unwrap_or_default();

        let unique_languages: HashSet<String> =
            injections.iter().map(|i| i.language.clone()).collect();

        assert_eq!(unique_languages.len(), 2);
        assert!(unique_languages.contains("lua"));
        assert!(unique_languages.contains("python"));
        assert_eq!(injections.len(), 3, "2 lua + 1 python");
    }
}
