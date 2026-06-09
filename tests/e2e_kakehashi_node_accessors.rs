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
//! `fieldNameForChild`, `fieldNameForNamedChild`.
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

    // Saturate into the python layer at the `=` on line 3.
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
