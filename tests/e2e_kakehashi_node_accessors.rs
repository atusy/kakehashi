//! End-to-end tests for the scalar / navigation / field accessor methods of the
//! Node Reference Protocol (node-reference-protocol) — the tree-sitter `Node`
//! API surface beyond the original entry / text / parent / children methods.
//!
//! Covered: `kind`, `grammarName`, `isNamed`, `isExtra`, `hasError`, `isError`,
//! `isMissing`, `startByte`, `endByte`, `byteRange`, `childCount`,
//! `namedChildCount`, `descendantCount`, `toSexp`, `child`, `namedChild`,
//! `namedChildren`, `nextSibling`, `prevSibling`, `nextNamedSibling`,
//! `prevNamedSibling`, `firstChildForByte`, `descendantForByteRange`,
//! `namedDescendantForByteRange`, `childByFieldName`, `childrenByFieldName`,
//! `fieldNameForChild`, `fieldNameForNamedChild`, `range`, `startPosition`,
//! `endPosition`, `descendantForPointRange`, `namedDescendantForPointRange`.
//!
//! Run with: `cargo test --test e2e_kakehashi_node_accessors --features e2e`

#![cfg(feature = "e2e")]

mod helpers;

use helpers::lsp_client::LspClient;
use serde_json::{Value, json};

/// Initialize + `initialized` handshake.
fn initialize(client: &mut LspClient) {
    client.send_request(
        "initialize",
        json!({ "processId": std::process::id(), "rootUri": null, "capabilities": {} }),
    );
    client.send_notification("initialized", json!({}));
}

/// Open a markdown document via `textDocument/didOpen`.
fn open_markdown(client: &mut LspClient, uri: &str, text: &str) {
    client.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": { "uri": uri, "languageId": "markdown", "version": 1, "text": text }
        }),
    );
}

/// Send a custom `kakehashi/node*` request and unwrap its `result` field.
fn call(client: &mut LspClient, method: &str, params: Value) -> Value {
    let response = client.send_request(method, params);
    assert!(
        response.get("error").is_none(),
        "{} returned an error: {:?}",
        method,
        response.get("error")
    );
    response
        .get("result")
        .cloned()
        .unwrap_or_else(|| panic!("{} response must contain a result field", method))
}

/// `{ textDocument, id }` request body shared by the id-only accessors.
fn id_params(uri: &str, id: &str) -> Value {
    json!({ "textDocument": { "uri": uri }, "id": id })
}

/// Resolve a position to a node id via `kakehashi/node`.
fn node_at(client: &mut LspClient, uri: &str, line: u32, character: u32) -> Value {
    call(
        client,
        "kakehashi/node",
        json!({ "textDocument": { "uri": uri }, "position": { "line": line, "character": character } }),
    )
}

/// Read the `id` field of a NodeInfo as a string.
fn id_of(node: &Value) -> String {
    node.get("id")
        .and_then(Value::as_str)
        .expect("NodeInfo must have a string id")
        .to_string()
}

/// Walk to the root of the host tree starting from any in-document node, so we
/// have a node with a stable, non-trivial child set to exercise navigation.
fn root_id(client: &mut LspClient, uri: &str) -> String {
    let start = node_at(client, uri, 0, 0);
    assert!(!start.is_null(), "expected a node at document start");
    let mut current = id_of(&start);
    for _ in 0..64 {
        let parent = call(client, "kakehashi/node/parent", id_params(uri, &current));
        if parent.is_null() {
            return current;
        }
        current = id_of(&parent);
    }
    panic!("did not reach the tree root within 64 hops");
}

const DOC: &str = "# Heading\n\nplain **bold** more text.\n";

/// Descend from the root until we reach a node with at least two children, and
/// return that node's id. tree-sitter-md wraps the document in a single
/// `section`, so the root itself has only one child — siblings need a deeper,
/// genuinely multi-child node (e.g. the inline content of the paragraph).
fn multi_child_node(client: &mut LspClient, uri: &str) -> String {
    let mut current = root_id(client, uri);
    for _ in 0..32 {
        let children = call(client, "kakehashi/node/children", id_params(uri, &current));
        let arr = children.as_array().expect("children must be an array");
        if arr.len() >= 2 {
            return current;
        }
        assert!(
            !arr.is_empty(),
            "ran out of descendants before a multi-child node"
        );
        current = id_of(&arr[0]);
    }
    panic!("did not find a multi-child node within 32 levels");
}

#[test]
fn test_scalar_accessors_report_intrinsic_properties() {
    let mut client = LspClient::new();
    initialize(&mut client);
    let uri = "file:///accessors_scalar.md";
    open_markdown(&mut client, uri, DOC);

    let root = root_id(&mut client, uri);

    // kind / grammarName are non-empty strings.
    let kind = call(&mut client, "kakehashi/node/kind", id_params(uri, &root));
    assert!(
        kind.get("kind")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.is_empty()),
        "kind must be a non-empty string, got {:?}",
        kind
    );
    let grammar = call(
        &mut client,
        "kakehashi/node/grammarName",
        id_params(uri, &root),
    );
    assert!(
        grammar.get("grammarName").and_then(Value::as_str).is_some(),
        "grammarName must be a string, got {:?}",
        grammar
    );

    // The document root is a named node spanning the whole document.
    let is_named = call(&mut client, "kakehashi/node/isNamed", id_params(uri, &root));
    assert_eq!(is_named.get("isNamed"), Some(&Value::Bool(true)));

    for (method, field) in [
        ("kakehashi/node/isExtra", "isExtra"),
        ("kakehashi/node/hasError", "hasError"),
        ("kakehashi/node/isError", "isError"),
        ("kakehashi/node/isMissing", "isMissing"),
    ] {
        let v = call(&mut client, method, id_params(uri, &root));
        assert!(
            v.get(field).and_then(Value::as_bool).is_some(),
            "{} must report a boolean {}, got {:?}",
            method,
            field,
            v
        );
    }

    // Byte span: root starts at 0 and ends at the document length.
    let start_byte = call(
        &mut client,
        "kakehashi/node/startByte",
        id_params(uri, &root),
    );
    assert_eq!(start_byte.get("startByte"), Some(&json!(0)));
    let end_byte = call(&mut client, "kakehashi/node/endByte", id_params(uri, &root));
    assert_eq!(
        end_byte.get("endByte").and_then(Value::as_u64),
        Some(DOC.len() as u64),
        "root endByte must equal the document length"
    );
    let byte_range = call(
        &mut client,
        "kakehashi/node/byteRange",
        id_params(uri, &root),
    );
    assert_eq!(byte_range.get("startByte"), Some(&json!(0)));
    assert_eq!(
        byte_range.get("endByte").and_then(Value::as_u64),
        Some(DOC.len() as u64)
    );

    // Counts: the root has children, and named ≤ total.
    let child_count = call(
        &mut client,
        "kakehashi/node/childCount",
        id_params(uri, &root),
    )
    .get("childCount")
    .and_then(Value::as_u64)
    .expect("childCount");
    let named_count = call(
        &mut client,
        "kakehashi/node/namedChildCount",
        id_params(uri, &root),
    )
    .get("namedChildCount")
    .and_then(Value::as_u64)
    .expect("namedChildCount");
    assert!(child_count >= 1, "root must have children");
    assert!(
        named_count <= child_count,
        "named children cannot exceed total"
    );

    let descendant_count = call(
        &mut client,
        "kakehashi/node/descendantCount",
        id_params(uri, &root),
    )
    .get("descendantCount")
    .and_then(Value::as_u64)
    .expect("descendantCount");
    assert!(
        descendant_count > child_count,
        "descendant count must exceed immediate child count for a non-trivial tree"
    );

    // s-expression is a non-empty parenthesized form.
    let sexp = call(&mut client, "kakehashi/node/toSexp", id_params(uri, &root));
    assert!(
        sexp.get("sexp")
            .and_then(Value::as_str)
            .is_some_and(|s| s.starts_with('(')),
        "toSexp must return a parenthesized s-expression, got {:?}",
        sexp
    );
}

#[test]
fn test_child_and_named_child_indexing() {
    let mut client = LspClient::new();
    initialize(&mut client);
    let uri = "file:///accessors_child.md";
    open_markdown(&mut client, uri, DOC);
    let root = root_id(&mut client, uri);

    let child0 = call(
        &mut client,
        "kakehashi/node/child",
        json!({ "textDocument": { "uri": uri }, "id": root, "index": 0 }),
    );
    assert!(!child0.is_null(), "child(0) of a non-leaf must resolve");
    assert!(child0.get("id").and_then(Value::as_str).is_some());

    // Out-of-range and negative indices collapse to null.
    let out = call(
        &mut client,
        "kakehashi/node/child",
        json!({ "textDocument": { "uri": uri }, "id": root, "index": 100000 }),
    );
    assert!(out.is_null(), "out-of-range child index must be null");
    let neg = call(
        &mut client,
        "kakehashi/node/child",
        json!({ "textDocument": { "uri": uri }, "id": root, "index": -1 }),
    );
    assert!(neg.is_null(), "negative child index must be null");

    let named0 = call(
        &mut client,
        "kakehashi/node/namedChild",
        json!({ "textDocument": { "uri": uri }, "id": root, "index": 0 }),
    );
    assert!(
        !named0.is_null(),
        "namedChild(0) of a non-leaf must resolve"
    );
}

#[test]
fn test_named_children_are_a_subset_in_order() {
    let mut client = LspClient::new();
    initialize(&mut client);
    let uri = "file:///accessors_named_children.md";
    open_markdown(&mut client, uri, DOC);
    let root = root_id(&mut client, uri);

    let all = call(
        &mut client,
        "kakehashi/node/children",
        id_params(uri, &root),
    );
    let named = call(
        &mut client,
        "kakehashi/node/namedChildren",
        id_params(uri, &root),
    );

    let all_arr = all.as_array().expect("children must be an array");
    let named_arr = named.as_array().expect("namedChildren must be an array");
    assert!(
        named_arr.len() <= all_arr.len(),
        "named children cannot outnumber all children"
    );
    assert!(
        !named_arr.is_empty(),
        "root must have at least one named child"
    );
}

#[test]
fn test_sibling_navigation_round_trips() {
    let mut client = LspClient::new();
    initialize(&mut client);
    let uri = "file:///accessors_siblings.md";
    open_markdown(&mut client, uri, DOC);
    let container = multi_child_node(&mut client, uri);

    // child(0) of a multi-child node is followed by at least one more sibling.
    let first = call(
        &mut client,
        "kakehashi/node/child",
        json!({ "textDocument": { "uri": uri }, "id": container, "index": 0 }),
    );
    assert!(!first.is_null());
    let first_id = id_of(&first);

    let next = call(
        &mut client,
        "kakehashi/node/nextSibling",
        id_params(uri, &first_id),
    );
    assert!(!next.is_null(), "first child must have a next sibling");
    let next_id = id_of(&next);
    assert_ne!(next_id, first_id, "next sibling must differ from the node");

    // Walking back must return to the original node.
    let back = call(
        &mut client,
        "kakehashi/node/prevSibling",
        id_params(uri, &next_id),
    );
    assert_eq!(
        id_of(&back),
        first_id,
        "prevSibling of nextSibling must return the original node"
    );

    // The first child has no previous sibling.
    let none = call(
        &mut client,
        "kakehashi/node/prevSibling",
        id_params(uri, &first_id),
    );
    assert!(none.is_null(), "first child must have no previous sibling");

    // Named-sibling variants resolve (value depends on grammar; just exercise).
    let _ = call(
        &mut client,
        "kakehashi/node/nextNamedSibling",
        id_params(uri, &first_id),
    );
    let _ = call(
        &mut client,
        "kakehashi/node/prevNamedSibling",
        id_params(uri, &next_id),
    );
}

#[test]
fn test_byte_based_descendant_lookups() {
    let mut client = LspClient::new();
    initialize(&mut client);
    let uri = "file:///accessors_byte.md";
    open_markdown(&mut client, uri, DOC);
    let root = root_id(&mut client, uri);

    // descendantForByteRange at [0, 1) lands inside the first heading.
    let desc = call(
        &mut client,
        "kakehashi/node/descendantForByteRange",
        json!({ "textDocument": { "uri": uri }, "id": root, "startByte": 0, "endByte": 1 }),
    );
    assert!(
        !desc.is_null(),
        "descendantForByteRange must resolve a node"
    );
    assert!(desc.get("id").and_then(Value::as_str).is_some());

    let named_desc = call(
        &mut client,
        "kakehashi/node/namedDescendantForByteRange",
        json!({ "textDocument": { "uri": uri }, "id": root, "startByte": 0, "endByte": 1 }),
    );
    assert!(
        !named_desc.is_null(),
        "namedDescendantForByteRange must resolve"
    );

    // firstChildForByte(0) returns the root's first child past byte 0.
    let first_child = call(
        &mut client,
        "kakehashi/node/firstChildForByte",
        json!({ "textDocument": { "uri": uri }, "id": root, "byte": 0 }),
    );
    assert!(!first_child.is_null(), "firstChildForByte(0) must resolve");

    // Negative byte → null.
    let neg = call(
        &mut client,
        "kakehashi/node/firstChildForByte",
        json!({ "textDocument": { "uri": uri }, "id": root, "byte": -5 }),
    );
    assert!(neg.is_null(), "negative byte must collapse to null");

    // Inverted range (startByte > endByte) is invalid → null, mirroring the
    // defensive guard in lookup::find_node_at rather than handing tree-sitter an
    // unspecified range.
    let inverted = call(
        &mut client,
        "kakehashi/node/descendantForByteRange",
        json!({ "textDocument": { "uri": uri }, "id": root, "startByte": 5, "endByte": 1 }),
    );
    assert!(
        inverted.is_null(),
        "inverted byte range must collapse to null"
    );
    let inverted_named = call(
        &mut client,
        "kakehashi/node/namedDescendantForByteRange",
        json!({ "textDocument": { "uri": uri }, "id": root, "startByte": 5, "endByte": 1 }),
    );
    assert!(
        inverted_named.is_null(),
        "inverted byte range must collapse to null for the named variant too"
    );

    // Out-of-bounds bytes (past the document end) collapse to null rather than
    // being forwarded to tree-sitter, whose behaviour for ranges beyond the
    // parsed tree is version-dependent (mirrors the defensive bound in
    // lookup::find_node_at). `oob` is well past the end of `DOC`.
    let oob = (DOC.len() + 10) as i64;

    let oob_first_child = call(
        &mut client,
        "kakehashi/node/firstChildForByte",
        json!({ "textDocument": { "uri": uri }, "id": root, "byte": oob }),
    );
    assert!(
        oob_first_child.is_null(),
        "firstChildForByte past EOF must collapse to null, got {:?}",
        oob_first_child
    );

    let oob_desc = call(
        &mut client,
        "kakehashi/node/descendantForByteRange",
        json!({ "textDocument": { "uri": uri }, "id": root, "startByte": 0, "endByte": oob }),
    );
    assert!(
        oob_desc.is_null(),
        "descendantForByteRange past EOF must collapse to null, got {:?}",
        oob_desc
    );

    let oob_named_desc = call(
        &mut client,
        "kakehashi/node/namedDescendantForByteRange",
        json!({ "textDocument": { "uri": uri }, "id": root, "startByte": 0, "endByte": oob }),
    );
    assert!(
        oob_named_desc.is_null(),
        "namedDescendantForByteRange past EOF must collapse to null, got {:?}",
        oob_named_desc
    );
}

/// Byte/range arguments are scoped to the queried node: an offset *before* the
/// node's own `start_byte` is outside it and must collapse to `null`, just like
/// one past its end. Uses a node on line 2 (which starts well after byte 0).
#[test]
fn test_byte_lookups_before_node_start_are_null() {
    let mut client = LspClient::new();
    initialize(&mut client);
    let uri = "file:///accessors_byte_lowerbound.md";
    open_markdown(&mut client, uri, DOC);

    // A node inside the line-2 paragraph starts after "# Heading\n\n" (byte 11).
    let node = node_at(&mut client, uri, 2, 2);
    assert!(!node.is_null(), "expected a node on line 2");
    let id = id_of(&node);
    let start_byte = call(&mut client, "kakehashi/node/startByte", id_params(uri, &id))
        .get("startByte")
        .and_then(Value::as_u64)
        .expect("startByte");
    assert!(start_byte > 0, "fixture node must start after byte 0");

    // firstChildForByte at byte 0 is before this node → null.
    let before = call(
        &mut client,
        "kakehashi/node/firstChildForByte",
        json!({ "textDocument": { "uri": uri }, "id": id, "byte": 0 }),
    );
    assert!(
        before.is_null(),
        "firstChildForByte before the node's start must be null, got {:?}",
        before
    );

    // A range whose start is before this node → null (both variants).
    for method in [
        "kakehashi/node/descendantForByteRange",
        "kakehashi/node/namedDescendantForByteRange",
    ] {
        let v = call(
            &mut client,
            method,
            json!({ "textDocument": { "uri": uri }, "id": id, "startByte": 0, "endByte": start_byte }),
        );
        assert!(
            v.is_null(),
            "{} with a start before the node must be null, got {:?}",
            method,
            v
        );
    }
}

#[test]
fn test_field_name_for_child_reports_or_nulls() {
    let mut client = LspClient::new();
    initialize(&mut client);
    let uri = "file:///accessors_field_index.md";
    open_markdown(&mut client, uri, DOC);
    let root = root_id(&mut client, uri);

    // The node resolves, so the response is an object with a `fieldName` key —
    // possibly null when the child carries no field (markdown rarely labels
    // fields). The point is that a resolvable node never yields top-level null.
    let resp = call(
        &mut client,
        "kakehashi/node/fieldNameForChild",
        json!({ "textDocument": { "uri": uri }, "id": root, "index": 0 }),
    );
    assert!(
        resp.is_object() && resp.as_object().unwrap().contains_key("fieldName"),
        "fieldNameForChild on a resolvable node must return {{fieldName: ...}}, got {:?}",
        resp
    );

    let named = call(
        &mut client,
        "kakehashi/node/fieldNameForNamedChild",
        json!({ "textDocument": { "uri": uri }, "id": root, "index": 0 }),
    );
    assert!(
        named.is_object() && named.as_object().unwrap().contains_key("fieldName"),
        "fieldNameForNamedChild on a resolvable node must return {{fieldName: ...}}, got {:?}",
        named
    );
}

/// Markdown → Python fixture: the python `y = 1 + 2` assignment has named
/// fields (`left`, `right`), letting us exercise the field-name lookups
/// meaningfully. Skipped when the python grammar is unavailable.
const MARKDOWN_WITH_PYTHON: &str = "# Heading\n\n```python\ny = 1 + 2\n```\n";

#[test]
fn test_field_name_lookups_on_python_assignment() {
    let mut client = LspClient::new();
    initialize(&mut client);
    let uri = "file:///accessors_field_python.md";
    open_markdown(&mut client, uri, MARKDOWN_WITH_PYTHON);

    // Saturate into the python layer inside `y = 1 + 2` on line 3 (character 4
    // is the `1`, which is unambiguously within the python tree).
    let py = call(
        &mut client,
        "kakehashi/node",
        json!({ "textDocument": { "uri": uri }, "position": { "line": 3, "character": 4 }, "injection": true }),
    );
    if py.is_null() {
        eprintln!("SKIP: python layer did not resolve (python grammar missing?)");
        return;
    }

    // Walk up to the `assignment` node (its children carry `left` / `right`).
    let mut current = id_of(&py);
    let mut assignment: Option<String> = None;
    for _ in 0..16 {
        let kind = call(&mut client, "kakehashi/node/kind", id_params(uri, &current));
        if kind.get("kind").and_then(Value::as_str) == Some("assignment") {
            assignment = Some(current.clone());
            break;
        }
        let parent = call(
            &mut client,
            "kakehashi/node/parent",
            id_params(uri, &current),
        );
        if parent.is_null() {
            break;
        }
        current = id_of(&parent);
    }
    let Some(assignment) = assignment else {
        eprintln!("SKIP: could not locate a python `assignment` node");
        return;
    };

    // child_by_field_name("left") → the `y` identifier.
    let left = call(
        &mut client,
        "kakehashi/node/childByFieldName",
        json!({ "textDocument": { "uri": uri }, "id": assignment, "name": "left" }),
    );
    assert!(!left.is_null(), "assignment must have a `left` field");
    assert_eq!(
        left.get("kind").and_then(Value::as_str),
        Some("identifier"),
        "the `left` field of a python assignment is an identifier"
    );

    // childrenByFieldName("left") → exactly one node, same as the singular lookup.
    let lefts = call(
        &mut client,
        "kakehashi/node/childrenByFieldName",
        json!({ "textDocument": { "uri": uri }, "id": assignment, "name": "left" }),
    );
    let lefts_arr = lefts
        .as_array()
        .expect("childrenByFieldName must be an array");
    assert_eq!(lefts_arr.len(), 1, "exactly one `left` child expected");

    // An absent field name yields an empty array, not null.
    let nope = call(
        &mut client,
        "kakehashi/node/childrenByFieldName",
        json!({ "textDocument": { "uri": uri }, "id": assignment, "name": "no_such_field" }),
    );
    assert_eq!(
        nope.as_array().map(|a| a.len()),
        Some(0),
        "an unknown field name must return [] for a resolvable node"
    );
}

#[test]
fn test_accessors_return_null_for_unknown_id() {
    let mut client = LspClient::new();
    initialize(&mut client);
    let uri = "file:///accessors_unknown.md";
    open_markdown(&mut client, uri, DOC);

    // A syntactically valid ULID that was never issued.
    let stray = "01HXXXXXXXXXXXXXXXXXXXXXXX";

    for method in [
        "kakehashi/node/kind",
        "kakehashi/node/grammarName",
        "kakehashi/node/isNamed",
        "kakehashi/node/startByte",
        "kakehashi/node/byteRange",
        "kakehashi/node/childCount",
        "kakehashi/node/descendantCount",
        "kakehashi/node/toSexp",
        "kakehashi/node/namedChildren",
        "kakehashi/node/nextSibling",
    ] {
        let v = call(&mut client, method, id_params(uri, stray));
        assert!(v.is_null(), "{} must return null for an unknown id", method);
    }

    // Index / byte / field variants likewise collapse to null.
    let v = call(
        &mut client,
        "kakehashi/node/child",
        json!({ "textDocument": { "uri": uri }, "id": stray, "index": 0 }),
    );
    assert!(v.is_null(), "child on unknown id must be null");
    let v = call(
        &mut client,
        "kakehashi/node/fieldNameForChild",
        json!({ "textDocument": { "uri": uri }, "id": stray, "index": 0 }),
    );
    assert!(v.is_null(), "fieldNameForChild on unknown id must be null");
    let v = call(
        &mut client,
        "kakehashi/node/childByFieldName",
        json!({ "textDocument": { "uri": uri }, "id": stray, "name": "left" }),
    );
    assert!(v.is_null(), "childByFieldName on unknown id must be null");
}

/// Read an LSP `Position` object as `(line, character)`.
fn lsp_pos(v: &Value) -> (u64, u64) {
    (
        v.get("line")
            .and_then(Value::as_u64)
            .expect("Position.line"),
        v.get("character")
            .and_then(Value::as_u64)
            .expect("Position.character"),
    )
}

#[test]
fn test_position_accessors_return_lsp_positions() {
    let mut client = LspClient::new();
    initialize(&mut client);
    let uri = "file:///accessors_position.md";
    open_markdown(&mut client, uri, DOC);
    let root = root_id(&mut client, uri);

    // The document root starts at the very beginning.
    let start_pos = call(
        &mut client,
        "kakehashi/node/startPosition",
        id_params(uri, &root),
    );
    assert_eq!(
        lsp_pos(start_pos.get("startPosition").expect("startPosition field")),
        (0, 0),
        "root startPosition must be line 0, character 0"
    );

    let end_pos = call(
        &mut client,
        "kakehashi/node/endPosition",
        id_params(uri, &root),
    );
    let end = lsp_pos(end_pos.get("endPosition").expect("endPosition field"));

    // range bundles the two into { start, end } and must agree with the singular
    // accessors.
    let range = call(&mut client, "kakehashi/node/range", id_params(uri, &root));
    assert_eq!(
        lsp_pos(range.get("start").expect("range.start")),
        (0, 0),
        "range.start must match startPosition"
    );
    assert_eq!(
        lsp_pos(range.get("end").expect("range.end")),
        end,
        "range.end must match endPosition"
    );
}

/// `character` is a UTF-16 code unit offset (LSP), NOT tree-sitter's UTF-8 byte
/// column. With an all-CJK line each character is 3 UTF-8 bytes but 1 UTF-16
/// code unit, so the reported column must be `endByte / 3`, strictly less than
/// the byte offset — proving the server converts rather than leaking byte points.
#[test]
fn test_positions_are_utf16_not_byte_columns() {
    let mut client = LspClient::new();
    initialize(&mut client);
    let uri = "file:///accessors_utf16.md";
    // Two CJK characters on line 0 (3 UTF-8 bytes each), then a newline.
    open_markdown(&mut client, uri, "あい\n");

    // Smallest node at the very start — some inline/text node spanning CJK bytes.
    let node = node_at(&mut client, uri, 0, 0);
    assert!(!node.is_null(), "expected a node at the CJK line start");
    let id = id_of(&node);

    let end_byte = call(&mut client, "kakehashi/node/endByte", id_params(uri, &id))
        .get("endByte")
        .and_then(Value::as_u64)
        .expect("endByte");
    let end_pos = call(
        &mut client,
        "kakehashi/node/endPosition",
        id_params(uri, &id),
    );
    let (line, character) = lsp_pos(end_pos.get("endPosition").expect("endPosition field"));

    assert_eq!(line, 0, "the node ends on line 0");
    assert!(
        end_byte >= 3 && end_byte.is_multiple_of(3),
        "CJK node end byte should fall on a 3-byte boundary, got {}",
        end_byte
    );
    assert_eq!(
        character,
        end_byte / 3,
        "endPosition.character must be the UTF-16 column ({}), not the byte column ({})",
        end_byte / 3,
        end_byte
    );
    assert!(
        character < end_byte,
        "UTF-16 column must be strictly less than the byte offset for CJK content"
    );
}

#[test]
fn test_descendant_for_point_range_takes_lsp_positions() {
    let mut client = LspClient::new();
    initialize(&mut client);
    let uri = "file:///accessors_point_range.md";
    open_markdown(&mut client, uri, DOC);
    let root = root_id(&mut client, uri);

    // A Position range covering the first byte resolves a descendant.
    let desc = call(
        &mut client,
        "kakehashi/node/descendantForPointRange",
        json!({ "textDocument": { "uri": uri }, "id": root,
                "start": { "line": 0, "character": 0 },
                "end":   { "line": 0, "character": 1 } }),
    );
    assert!(
        !desc.is_null(),
        "descendantForPointRange must resolve a node"
    );
    assert!(desc.get("id").and_then(Value::as_str).is_some());

    let named = call(
        &mut client,
        "kakehashi/node/namedDescendantForPointRange",
        json!({ "textDocument": { "uri": uri }, "id": root,
                "start": { "line": 0, "character": 0 },
                "end":   { "line": 0, "character": 1 } }),
    );
    assert!(
        !named.is_null(),
        "namedDescendantForPointRange must resolve"
    );

    // Inverted Position range collapses to null.
    let inverted = call(
        &mut client,
        "kakehashi/node/descendantForPointRange",
        json!({ "textDocument": { "uri": uri }, "id": root,
                "start": { "line": 0, "character": 5 },
                "end":   { "line": 0, "character": 1 } }),
    );
    assert!(inverted.is_null(), "inverted Position range must be null");

    // A `character` past line 0's end ("# Heading" = 9 cols) must NOT spill onto
    // a later line — an over-long column means "no such location" → null, not a
    // descendant resolved elsewhere in the document.
    let overlong = call(
        &mut client,
        "kakehashi/node/descendantForPointRange",
        json!({ "textDocument": { "uri": uri }, "id": root,
                "start": { "line": 0, "character": 0 },
                "end":   { "line": 0, "character": 999 } }),
    );
    assert!(
        overlong.is_null(),
        "an over-long character must collapse to null, not spill to a later line, got {:?}",
        overlong
    );
}

#[test]
fn test_position_accessors_return_null_for_unknown_id() {
    let mut client = LspClient::new();
    initialize(&mut client);
    let uri = "file:///accessors_position_unknown.md";
    open_markdown(&mut client, uri, DOC);
    let stray = "01HXXXXXXXXXXXXXXXXXXXXXXX";

    for method in [
        "kakehashi/node/range",
        "kakehashi/node/startPosition",
        "kakehashi/node/endPosition",
    ] {
        let v = call(&mut client, method, id_params(uri, stray));
        assert!(v.is_null(), "{} must return null for an unknown id", method);
    }
    let v = call(
        &mut client,
        "kakehashi/node/descendantForPointRange",
        json!({ "textDocument": { "uri": uri }, "id": stray,
                "start": { "line": 0, "character": 0 },
                "end":   { "line": 0, "character": 1 } }),
    );
    assert!(
        v.is_null(),
        "descendantForPointRange on unknown id must be null"
    );
}

// ============================================================
// Excluded-gap coordinates in injected layers (#341)
// ============================================================
//
// A blockquoted python fence parses the injected python layer with
// non-contiguous included ranges: each line's code is included, the `> `
// prefixes are excluded gaps. A coordinate landing in a gap is not real
// injected content, so the coordinate accessors must return null — matching
// the entry point (`kakehashi/node`), which never pushes an injection layer
// for a gap byte.
//
// Byte map of the fixture (every line starts with a 2-byte `> ` prefix):
//   line 0 `> ```python\n`  bytes  0..12   (prefix  0..2)
//   line 1 `> x = 1\n`      bytes 12..20   (prefix 12..14, code 14..20)
//   line 2 `> y = 2\n`      bytes 20..28   (prefix 20..22, code 22..28)
//   line 3 `> ```\n`        bytes 28..34
const BLOCKQUOTED_PYTHON: &str = "> ```python\n> x = 1\n> y = 2\n> ```\n";

/// Resolve the injected python layer's root by entering the layer at a cursor
/// at the `=` of `x = 1` (line 1, character 4 — past the `> ` prefix) and
/// climbing parents;
/// per-layer scope keeps the walk inside the python tree.
fn python_layer_root(client: &mut LspClient, uri: &str) -> String {
    let node = call(
        client,
        "kakehashi/node",
        json!({
            "textDocument": { "uri": uri },
            "position": { "line": 1, "character": 4 },
            "injection": true,
        }),
    );
    assert!(
        !node.is_null(),
        "expected an injected python node at the cursor"
    );
    let mut current = id_of(&node);
    for _ in 0..32 {
        let parent = call(client, "kakehashi/node/parent", id_params(uri, &current));
        if parent.is_null() {
            return current;
        }
        current = id_of(&parent);
    }
    panic!("did not reach the injected layer root within 32 hops");
}

/// The python root's contiguous span covers the line-2 `> ` prefix (an
/// excluded gap), so this asserts the test premise: the gap bytes pass the
/// node-span bound and only the included-ranges check can reject them.
fn assert_gap_inside_root_span(client: &mut LspClient, uri: &str, root: &str) {
    let range = call(client, "kakehashi/node/byteRange", id_params(uri, root));
    let start = range
        .get("startByte")
        .and_then(Value::as_u64)
        .expect("startByte");
    let end = range
        .get("endByte")
        .and_then(Value::as_u64)
        .expect("endByte");
    assert!(
        start <= 20 && 22 < end,
        "fixture premise: python root span [{start}, {end}) must cover the \
         line-2 `> ` prefix bytes 20..22"
    );
}

#[test]
fn test_byte_lookups_in_excluded_gap_are_null() {
    let mut client = LspClient::new();
    initialize(&mut client);
    let uri = "file:///accessors_gap_bytes.md";
    open_markdown(&mut client, uri, BLOCKQUOTED_PYTHON);

    let root = python_layer_root(&mut client, uri);
    assert_gap_inside_root_span(&mut client, uri, &root);

    // Control: a code byte (the `y` at byte 22) resolves normally.
    let on_code = call(
        &mut client,
        "kakehashi/node/descendantForByteRange",
        json!({ "textDocument": { "uri": uri }, "id": root,
                "startByte": 22, "endByte": 23 }),
    );
    assert!(
        !on_code.is_null(),
        "descendantForByteRange on included code bytes must resolve"
    );

    // A range crossing the gap with both endpoints on real content resolves
    // the spanning node: [14, 23) starts on `x` and its last queried byte is
    // the `y` at 22.
    let spanning = call(
        &mut client,
        "kakehashi/node/descendantForByteRange",
        json!({ "textDocument": { "uri": uri }, "id": root,
                "startByte": 14, "endByte": 23 }),
    );
    assert!(
        !spanning.is_null(),
        "a gap-spanning range with both bounds on code must still resolve"
    );

    // The `> ` prefix bytes (20..22) are an excluded gap → null. This covers
    // a range inside the gap (20..21) and a range whose exclusive end lands
    // on the next block's start (14..22): its last queried byte (21) is gap
    // content even though 22 itself starts an included range.
    for (start, end) in [(20, 21), (14, 22)] {
        for method in [
            "kakehashi/node/descendantForByteRange",
            "kakehashi/node/namedDescendantForByteRange",
        ] {
            let v = call(
                &mut client,
                method,
                json!({ "textDocument": { "uri": uri }, "id": root,
                        "startByte": start, "endByte": end }),
            );
            assert!(
                v.is_null(),
                "{} on [{start}, {end}) must return null (gap content), got {:?}",
                method,
                v
            );
        }
    }

    let v = call(
        &mut client,
        "kakehashi/node/firstChildForByte",
        json!({ "textDocument": { "uri": uri }, "id": root, "byte": 20 }),
    );
    assert!(
        v.is_null(),
        "firstChildForByte on an excluded-gap byte must return null, got {:?}",
        v
    );
}

#[test]
fn test_point_lookups_in_excluded_gap_are_null() {
    let mut client = LspClient::new();
    initialize(&mut client);
    let uri = "file:///accessors_gap_points.md";
    open_markdown(&mut client, uri, BLOCKQUOTED_PYTHON);

    let root = python_layer_root(&mut client, uri);
    assert_gap_inside_root_span(&mut client, uri, &root);

    // Control: positions on the code (`y` at line 2, characters 2..3) resolve.
    let on_code = call(
        &mut client,
        "kakehashi/node/descendantForPointRange",
        json!({ "textDocument": { "uri": uri }, "id": root,
                "start": { "line": 2, "character": 2 },
                "end":   { "line": 2, "character": 3 } }),
    );
    assert!(
        !on_code.is_null(),
        "descendantForPointRange on included code must resolve"
    );

    // Positions on the line-2 `> ` prefix are an excluded gap → null.
    for method in [
        "kakehashi/node/descendantForPointRange",
        "kakehashi/node/namedDescendantForPointRange",
    ] {
        let v = call(
            &mut client,
            method,
            json!({ "textDocument": { "uri": uri }, "id": root,
                    "start": { "line": 2, "character": 0 },
                    "end":   { "line": 2, "character": 1 } }),
        );
        assert!(
            v.is_null(),
            "{} on an excluded-gap position must return null, got {:?}",
            method,
            v
        );
    }
}
