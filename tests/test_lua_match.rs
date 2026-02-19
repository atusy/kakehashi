use streaming_iterator::StreamingIterator;
use tree_sitter::{Parser, Query, QueryCursor};

/// Helper to collect capture texts from a query using filter_captures
fn collect_filtered_captures(source_code: &str, query_str: &str) -> Vec<String> {
    let mut parser = Parser::new();
    let language = tree_sitter_rust::LANGUAGE.into();
    parser.set_language(&language).unwrap();

    let tree = parser.parse(source_code, None).unwrap();
    let root_node = tree.root_node();

    let query = Query::new(&language, query_str).unwrap();
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(&query, root_node, source_code.as_bytes());

    let mut results = Vec::new();
    while let Some(match_) = matches.next() {
        let filtered = kakehashi::language::filter_captures(&query, match_, source_code);
        for capture in filtered {
            let text = &source_code[capture.node.start_byte()..capture.node.end_byte()];
            results.push(text.to_string());
        }
    }
    results
}

#[test]
fn test_lua_match_predicates() {
    let source_code = r#"
        fn main() {
            let number = 123;
            let CONSTANT = 456;
            let mixed_Case = 789;
            let snake_case = 0;
        }
    "#;

    // Test lua-match with digit pattern
    let found_numbers = collect_filtered_captures(
        source_code,
        r#"((integer_literal) @number (#lua-match? @number "^%d+$"))"#,
    );
    assert!(found_numbers.contains(&"123".to_string()));
    assert!(found_numbers.contains(&"456".to_string()));
    assert!(found_numbers.contains(&"789".to_string()));

    // Test lua-match with uppercase pattern
    let found_constants = collect_filtered_captures(
        source_code,
        r#"((identifier) @constant (#lua-match? @constant "^[A-Z][A-Z_0-9]*$"))"#,
    );
    assert!(found_constants.contains(&"CONSTANT".to_string()));
    assert!(!found_constants.contains(&"mixed_Case".to_string()));
    assert!(!found_constants.contains(&"snake_case".to_string()));

    // Test lua-match with pattern classes
    // Note: %w in lua-pattern crate converts to [a-zA-Z0-9] without underscore
    // So we'll use a pattern that explicitly includes underscore
    let found_words = collect_filtered_captures(
        source_code,
        r#"((identifier) @word (#lua-match? @word "^[%w_]+$"))"#,
    );
    assert!(found_words.contains(&"main".to_string()));
    assert!(found_words.contains(&"number".to_string()));
    assert!(found_words.contains(&"CONSTANT".to_string()));
    assert!(found_words.contains(&"mixed_Case".to_string()));
    assert!(found_words.contains(&"snake_case".to_string()));
}

#[test]
fn test_lua_match_with_anchors() {
    let source_code = r#"
        fn test_function() {
            let x = 1;
        }
    "#;

    // Test pattern with start anchor
    let found_funcs = collect_filtered_captures(
        source_code,
        r#"((identifier) @func (#lua-match? @func "^test"))"#,
    );
    assert!(found_funcs.contains(&"test_function".to_string()));
    assert!(!found_funcs.contains(&"x".to_string()));

    // Test pattern with end anchor
    let found_suffix = collect_filtered_captures(
        source_code,
        r#"((identifier) @func_suffix (#lua-match? @func_suffix "function$"))"#,
    );
    assert!(found_suffix.contains(&"test_function".to_string()));
}

#[test]
fn test_lua_match_quantifiers() {
    let source_code = r#"
        fn main() {
            let a = 1;
            let ab = 2;
            let abc = 3;
            let abcd = 4;
        }
    "#;

    // Test with + quantifier (one or more)
    let found = collect_filtered_captures(
        source_code,
        r#"((identifier) @multi (#lua-match? @multi "^a%l+$"))"#,
    );
    assert!(!found.contains(&"a".to_string())); // 'a' alone doesn't match a%l+
    assert!(found.contains(&"ab".to_string()));
    assert!(found.contains(&"abc".to_string()));
    assert!(found.contains(&"abcd".to_string()));

    // Test with * quantifier (zero or more)
    let found_any = collect_filtered_captures(
        source_code,
        r#"((identifier) @any (#lua-match? @any "^a%l*$"))"#,
    );
    assert!(found_any.contains(&"a".to_string())); // Now 'a' matches
    assert!(found_any.contains(&"ab".to_string()));
    assert!(found_any.contains(&"abc".to_string()));
    assert!(found_any.contains(&"abcd".to_string()));
}

#[test]
fn test_not_lua_match_predicate() {
    let source_code = r#"
        fn test_fn() {}
        fn main() {}
    "#;

    // not-lua-match? should exclude identifiers matching "^test"
    let query_str = r#"((identifier) @name (#not-lua-match? @name "^test"))"#;
    let results = collect_filtered_captures(source_code, query_str);

    assert!(results.contains(&"main".to_string()));
    assert!(!results.contains(&"test_fn".to_string()));
}

#[test]
fn test_contains_predicate() {
    let source_code = r#"
        fn foobar() {}
        fn foo() {}
        fn bar() {}
    "#;

    // contains? with single arg: "oo" must be a substring
    let query_str = r#"((identifier) @name (#contains? @name "oo"))"#;
    let results = collect_filtered_captures(source_code, query_str);

    assert!(results.contains(&"foobar".to_string()));
    assert!(results.contains(&"foo".to_string()));
    assert!(!results.contains(&"bar".to_string()));
}

#[test]
fn test_contains_predicate_multiple_args() {
    let source_code = r#"
        fn foobar() {}
        fn foo() {}
        fn bar() {}
    "#;

    // contains? with multiple args: ALL must be substrings (AND semantics)
    let query_str = r#"((identifier) @name (#contains? @name "foo" "bar"))"#;
    let results = collect_filtered_captures(source_code, query_str);

    assert!(results.contains(&"foobar".to_string()));
    assert!(!results.contains(&"foo".to_string())); // "foo" doesn't contain "bar"
    assert!(!results.contains(&"bar".to_string())); // "bar" doesn't contain "foo"
}

#[test]
fn test_contains_predicate_no_args() {
    let source_code = r#"
        fn foobar() {}
    "#;

    // contains? with no string args beyond the capture: vacuous truth, all match
    let query_str = r#"((identifier) @name (#contains? @name))"#;
    let results = collect_filtered_captures(source_code, query_str);

    assert!(results.contains(&"foobar".to_string()));
}

#[test]
fn test_has_parent_predicate() {
    let source_code = r#"
        fn main() {
            let x = 1;
        }
    "#;

    // In Rust AST, the identifier "x" inside `let x = 1` has parent `let_declaration`
    // while "main" has parent `function_item`
    let query_str = r#"((identifier) @name (#has-parent? @name "let_declaration"))"#;
    let results = collect_filtered_captures(source_code, query_str);

    assert!(results.contains(&"x".to_string()));
    assert!(!results.contains(&"main".to_string()));
}

#[test]
fn test_not_has_parent_predicate() {
    let source_code = r#"
        fn main() {
            let x = 1;
        }
    "#;

    let query_str = r#"((identifier) @name (#not-has-parent? @name "let_declaration"))"#;
    let results = collect_filtered_captures(source_code, query_str);

    assert!(!results.contains(&"x".to_string()));
    assert!(results.contains(&"main".to_string()));
}

#[test]
fn test_has_ancestor_predicate() {
    let source_code = r#"
        fn main() {
            let x = 1;
        }
    "#;

    // "x" is inside a function_item (via function_item > block > let_declaration > identifier)
    let query_str = r#"((identifier) @name (#has-ancestor? @name "function_item"))"#;
    let results = collect_filtered_captures(source_code, query_str);

    // Both "main" and "x" are descendants of function_item
    // "main" is a direct child of function_item
    assert!(results.contains(&"main".to_string()));
    assert!(results.contains(&"x".to_string()));
}

#[test]
fn test_has_ancestor_predicate_specific() {
    let source_code = r#"
        fn main() {
            let x = 1;
        }
    "#;

    // Only "x" has a let_declaration ancestor (not "main")
    let query_str = r#"((identifier) @name (#has-ancestor? @name "let_declaration"))"#;
    let results = collect_filtered_captures(source_code, query_str);

    assert!(results.contains(&"x".to_string()));
    assert!(!results.contains(&"main".to_string()));
}

#[test]
fn test_not_has_ancestor_predicate() {
    let source_code = r#"
        fn main() {
            let x = 1;
        }
    "#;

    let query_str = r#"((identifier) @name (#not-has-ancestor? @name "let_declaration"))"#;
    let results = collect_filtered_captures(source_code, query_str);

    assert!(!results.contains(&"x".to_string()));
    assert!(results.contains(&"main".to_string()));
}
