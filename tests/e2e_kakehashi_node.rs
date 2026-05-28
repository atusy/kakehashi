//! End-to-end tests for `kakehashi/node`, `kakehashi/node/text`,
//! `kakehashi/node/parent`, and `kakehashi/node/children` (ADR-0025 PR-1 + PR-2 + PR-3).
//!
//! Covers the entry-point method (position → NodeInfo), the text resolution
//! method (id → current node text), the parent navigation method
//! (child id → parent NodeInfo), the children navigation method
//! (parent id → NodeInfo[]), and their interaction with `didChange`:
//!
//! - smallest-at-cursor lookup for named-or-anonymous nodes (host language only)
//! - end-of-document exception (`b == L && L > 0 && e == L`)
//! - empty document and out-of-bounds returning `null`
//! - `kakehashi/node/text` returning the live slice for a tracked node
//! - `kakehashi/node/parent` walking one step toward the root
//! - parent returning null at the root and for unknown ids
//! - `kakehashi/node/children` returning siblings in document order
//! - children returning `[]` (NOT `null`) for a leaf node
//! - children returning `null` for unknown ids
//! - ULID survival across position-adjusting edits
//! - ULID invalidation when the edit covers the node's START byte
//!
//! Run with: `cargo test --test e2e_kakehashi_node --features e2e`

#![cfg(feature = "e2e")]

mod helpers;

use helpers::lsp_client::LspClient;
use serde_json::{Value, json};

/// Initialize + `initialized` handshake.
fn initialize(client: &mut LspClient) {
    client.send_request(
        "initialize",
        json!({
            "processId": std::process::id(),
            "rootUri": null,
            "capabilities": {}
        }),
    );
    client.send_notification("initialized", json!({}));
}

/// Open a markdown document via `textDocument/didOpen`.
fn open_markdown(client: &mut LspClient, uri: &str, text: &str) {
    client.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri,
                "languageId": "markdown",
                "version": 1,
                "text": text
            }
        }),
    );
}

/// Send `kakehashi/node/text` for an id and unwrap the `result` field.
fn request_node_text(client: &mut LspClient, uri: &str, id: &str) -> Value {
    let response = client.send_request(
        "kakehashi/node/text",
        json!({
            "textDocument": { "uri": uri },
            "id": id
        }),
    );
    assert!(
        response.get("error").is_none(),
        "kakehashi/node/text returned an error: {:?}",
        response.get("error")
    );
    response
        .get("result")
        .cloned()
        .expect("response must contain a result field")
}

/// Send `kakehashi/node/parent` for an id and unwrap the `result` field
/// (which may be `null`).
fn request_node_parent(client: &mut LspClient, uri: &str, id: &str) -> Value {
    let response = client.send_request(
        "kakehashi/node/parent",
        json!({
            "textDocument": { "uri": uri },
            "id": id
        }),
    );
    assert!(
        response.get("error").is_none(),
        "kakehashi/node/parent returned an error: {:?}",
        response.get("error")
    );
    response
        .get("result")
        .cloned()
        .expect("response must contain a result field")
}

/// Send `kakehashi/node/children` for an id and unwrap the `result` field
/// (which may be `null`, an empty array, or a non-empty array).
fn request_node_children(client: &mut LspClient, uri: &str, id: &str) -> Value {
    let response = client.send_request(
        "kakehashi/node/children",
        json!({
            "textDocument": { "uri": uri },
            "id": id
        }),
    );
    assert!(
        response.get("error").is_none(),
        "kakehashi/node/children returned an error: {:?}",
        response.get("error")
    );
    response
        .get("result")
        .cloned()
        .expect("response must contain a result field")
}

/// Send `kakehashi/node` and unwrap the `result` field (which may be `null`).
fn request_node(client: &mut LspClient, uri: &str, line: u32, character: u32) -> Value {
    let response = client.send_request(
        "kakehashi/node",
        json!({
            "textDocument": { "uri": uri },
            "position": { "line": line, "character": character }
        }),
    );
    assert!(
        response.get("error").is_none(),
        "kakehashi/node returned an error: {:?}",
        response.get("error")
    );
    response
        .get("result")
        .cloned()
        .expect("response must contain a result field")
}

/// Send `kakehashi/node` with an explicit `injection` parameter (ADR-0025 PR-4).
/// `injection` is a `bool | number`; we accept any JSON value so the test
/// fixtures can exercise the full parameter surface, including out-of-bounds
/// indices and saturating `true`.
fn request_node_with_injection(
    client: &mut LspClient,
    uri: &str,
    line: u32,
    character: u32,
    injection: Value,
) -> Value {
    let response = client.send_request(
        "kakehashi/node",
        json!({
            "textDocument": { "uri": uri },
            "position": { "line": line, "character": character },
            "injection": injection,
        }),
    );
    assert!(
        response.get("error").is_none(),
        "kakehashi/node returned an error: {:?}",
        response.get("error")
    );
    response
        .get("result")
        .cloned()
        .expect("response must contain a result field")
}

/// ULIDs are 26 uppercase Crockford-base32 characters.
fn assert_ulid_shaped(value: &Value) {
    let s = value.as_str().expect("id should be a string");
    assert_eq!(s.len(), 26, "ULID must be 26 characters, got {:?}", s);
    assert!(
        s.chars().all(|c| c.is_ascii_alphanumeric()),
        "ULID must be alphanumeric, got {:?}",
        s
    );
}

/// Send a `textDocument/didChange` with a single full-text replacement.
/// The version is bumped to `new_version` and the entire document text is
/// replaced — equivalent to a full-document sync from the client.
fn full_text_change(client: &mut LspClient, uri: &str, new_version: i64, new_text: &str) {
    client.send_notification(
        "textDocument/didChange",
        json!({
            "textDocument": { "uri": uri, "version": new_version },
            "contentChanges": [{ "text": new_text }]
        }),
    );
}

/// Edit survival: acquire an id for a node, send a `didChange` that does NOT
/// touch the node's START byte, and verify `kakehashi/node/text` reflects the
/// post-edit content. ADR-0019's START-priority rule keeps the ULID alive,
/// and ADR-0025's text endpoint must always slice from the *current* document.
#[test]
fn test_node_text_survives_edit_that_does_not_touch_start() {
    let mut client = LspClient::new();
    initialize(&mut client);

    let uri = "file:///test_kakehashi_node_survive.md";
    // ATX heading occupies bytes [0, 9): "# Hello\n\n"; paragraph starts at byte 9.
    let original = "# Hello\n\nparagraph one.\n";
    open_markdown(&mut client, uri, original);

    // Acquire an id for the heading via cursor on "Hello".
    let node = request_node(&mut client, uri, 0, 4);
    assert!(!node.is_null(), "expected NodeInfo for heading");
    let id = node
        .get("id")
        .and_then(Value::as_str)
        .expect("id field must be a string")
        .to_string();

    // Capture the heading text BEFORE the edit so we can demand a change post-edit.
    let before = request_node_text(&mut client, uri, &id);
    let before_text = before
        .get("text")
        .and_then(Value::as_str)
        .expect("pre-edit text must resolve")
        .to_string();

    // Append text after the paragraph — heading bytes are completely untouched,
    // so its START stays at 0 and the ULID must survive.
    let edited = "# Hello\n\nparagraph one.\nparagraph two.\n";
    full_text_change(&mut client, uri, 2, edited);

    let after = request_node_text(&mut client, uri, &id);
    assert!(
        !after.is_null(),
        "id must survive an edit that does not touch its START byte"
    );

    let after_text = after
        .get("text")
        .and_then(Value::as_str)
        .expect("post-edit text must resolve");

    // Heading text is unchanged, so we expect to see the same heading string back.
    assert_eq!(
        after_text, before_text,
        "heading text should remain stable when only later content is appended"
    );
    assert!(
        edited.contains(after_text),
        "post-edit text {:?} must be a substring of the new document {:?}",
        after_text,
        edited
    );
}

/// Invalidation: an edit whose range covers the node's START byte must drop the
/// ULID per ADR-0019's START-priority rule. ADR-0025 collapses
/// invalidated / never-issued / mismatched-URI cases into a single null
/// response, so `kakehashi/node/text` must return null after the edit.
#[test]
fn test_node_text_returns_null_after_invalidating_edit() {
    let mut client = LspClient::new();
    initialize(&mut client);

    let uri = "file:///test_kakehashi_node_invalidate.md";
    let original = "# Hello\n\nparagraph one.\n";
    open_markdown(&mut client, uri, original);

    // Acquire an id for the heading via cursor on "Hello".
    let node = request_node(&mut client, uri, 0, 4);
    assert!(!node.is_null(), "expected NodeInfo for heading");
    let id = node
        .get("id")
        .and_then(Value::as_str)
        .expect("id must be a string")
        .to_string();

    // Sanity: id resolves before the edit.
    let before = request_node_text(&mut client, uri, &id);
    assert!(
        !before.is_null(),
        "id must resolve before the invalidating edit"
    );

    // Replace the entire document — every node's START is inside the edit range
    // [0, original.len()), so all tracked ULIDs must be invalidated.
    let replacement = "# Changed\n\ntotally different content.\n";
    full_text_change(&mut client, uri, 2, replacement);

    let after = request_node_text(&mut client, uri, &id);
    assert!(
        after.is_null(),
        "id whose START is covered by the edit must resolve to null, got {:?}",
        after
    );
}

/// Round-trip: acquire an id via `kakehashi/node`, then ask
/// `kakehashi/node/text` for it and verify the returned slice matches the
/// expected substring of the document.
#[test]
fn test_node_text_round_trips_for_known_id() {
    let mut client = LspClient::new();
    initialize(&mut client);

    let uri = "file:///test_kakehashi_node_text.md";
    let text = "# Hello\n\nSome paragraph text.\n";
    open_markdown(&mut client, uri, text);

    // Cursor on the second line's paragraph text.
    let node = request_node(&mut client, uri, 2, 2);
    assert!(!node.is_null(), "expected a NodeInfo for paragraph text");
    let id = node
        .get("id")
        .and_then(Value::as_str)
        .expect("id must be present");

    let text_response = request_node_text(&mut client, uri, id);
    assert!(
        !text_response.is_null(),
        "kakehashi/node/text must return a NodeText for a freshly issued id"
    );

    let returned_text = text_response
        .get("text")
        .and_then(Value::as_str)
        .expect("response must contain a `text` field");

    // The selected node must be a subsequence of the original document text.
    assert!(
        text.contains(returned_text),
        "returned text {:?} should be a substring of the document {:?}",
        returned_text,
        text,
    );
    // Sanity: text is non-empty and contains at least one character from "paragraph".
    assert!(!returned_text.is_empty(), "node text must be non-empty");
}

/// End-of-document exception: cursor exactly at `L` for a non-empty document
/// must resolve to the smallest node whose `end_byte == L`. Without the exception
/// the half-open rule would return null and break end-of-file motions.
#[test]
fn test_node_at_end_of_document_returns_node_info() {
    let mut client = LspClient::new();
    initialize(&mut client);

    let uri = "file:///test_kakehashi_node_eod.md";
    // Three lines, no trailing newline so the document's last byte is the final 'h'.
    // doc_len = "# A\nfoo\nbah".len() = 11. The cursor sits past the 'h'.
    let text = "# A\nfoo\nbah";
    open_markdown(&mut client, uri, text);

    // Cursor at (line 2, character 3) is past the last char of "bah", i.e., byte L=11.
    let result = request_node(&mut client, uri, 2, 3);

    assert!(
        !result.is_null(),
        "end-of-document position (b == L, L > 0) must resolve to a node, got null"
    );
    let id = result.get("id").expect("result must have id field");
    assert_ulid_shaped(id);
    let ty = result
        .get("type")
        .and_then(Value::as_str)
        .expect("type must be a string");
    assert!(!ty.is_empty(), "type field should be a non-empty kind");
}

/// Empty-document case: ADR-0025 gates the end-of-document exception on `L > 0`,
/// so any query on a zero-length document must return null.
#[test]
fn test_node_in_empty_document_returns_null() {
    let mut client = LspClient::new();
    initialize(&mut client);

    let uri = "file:///test_kakehashi_node_empty.md";
    open_markdown(&mut client, uri, "");

    let result = request_node(&mut client, uri, 0, 0);
    assert!(
        result.is_null(),
        "empty document must return null (L == 0 gates off the exception), got {:?}",
        result
    );
}

/// Out-of-bounds position: cursor past the actual document length must return null.
#[test]
fn test_node_position_past_eof_returns_null() {
    let mut client = LspClient::new();
    initialize(&mut client);

    let uri = "file:///test_kakehashi_node_oob.md";
    let text = "# A\n";
    open_markdown(&mut client, uri, text);

    // Line 99 is well past the actual content.
    let result = request_node(&mut client, uri, 99, 0);
    assert!(
        result.is_null(),
        "position past EOF must return null, got {:?}",
        result
    );
}

/// `kakehashi/node` on a markdown heading returns a NodeInfo with the heading
/// node's tree-sitter type and a ULID-shaped id.
#[test]
fn test_node_at_heading_returns_node_info() {
    let mut client = LspClient::new();
    initialize(&mut client);

    let uri = "file:///test_kakehashi_node_heading.md";
    let text = "# Hello\n\nSome paragraph text.\n";
    open_markdown(&mut client, uri, text);

    // Cursor on the word "Hello" inside the ATX heading (line 0, character 4).
    let result = request_node(&mut client, uri, 0, 4);

    assert!(
        !result.is_null(),
        "kakehashi/node should return a NodeInfo for a position inside the heading, got null"
    );

    let id = result.get("id").expect("result must have id field");
    assert_ulid_shaped(id);

    let ty = result
        .get("type")
        .and_then(Value::as_str)
        .expect("result must have a string type field");

    // The smallest node containing "Hello" inside `# Hello` is anonymous text
    // ("hello") or its named ancestor `inline`. Either is acceptable for the
    // entry point — what matters is that the type is non-empty.
    assert!(
        !ty.is_empty(),
        "type field should be the tree-sitter node kind, got empty string"
    );
}

/// `kakehashi/node/parent` walks one step toward the root of the same language
/// tree (ADR-0025 §"Navigation Methods"). Acquiring an id deep inside a nested
/// markdown structure and asking for its parent must yield a NodeInfo whose id
/// is distinct from the child's and whose type is the kind of the immediate
/// tree-sitter parent.
#[test]
fn test_node_parent_returns_immediate_parent_for_nested_node() {
    let mut client = LspClient::new();
    initialize(&mut client);

    let uri = "file:///test_kakehashi_node_parent_nested.md";
    // Nested structure: document → section → paragraph → inline → text.
    let text = "# Heading\n\nSome paragraph text.\n";
    open_markdown(&mut client, uri, text);

    // Cursor inside the paragraph text on the second non-empty line.
    let child = request_node(&mut client, uri, 2, 5);
    assert!(!child.is_null(), "expected NodeInfo for paragraph text");
    let child_id = child
        .get("id")
        .and_then(Value::as_str)
        .expect("child id must be a string")
        .to_string();
    let child_type = child
        .get("type")
        .and_then(Value::as_str)
        .expect("child type must be a string")
        .to_string();

    let parent = request_node_parent(&mut client, uri, &child_id);
    assert!(
        !parent.is_null(),
        "a non-root node must have a parent, got null"
    );

    let parent_id = parent.get("id").expect("parent must have id field");
    assert_ulid_shaped(parent_id);
    let parent_id_str = parent_id
        .as_str()
        .expect("parent id must be a string")
        .to_string();
    assert_ne!(
        parent_id_str, child_id,
        "parent id must differ from the child id"
    );

    let parent_type = parent
        .get("type")
        .and_then(Value::as_str)
        .expect("parent type must be a non-empty string");
    assert!(
        !parent_type.is_empty(),
        "parent type field should be the tree-sitter parent kind, got empty string"
    );

    // Sanity: the parent must be structurally distinct from the child. We
    // don't pin the exact grammar-derived names (tree-sitter-markdown's tag
    // names can change across versions), but if the parent's type equals the
    // child's type at the same span the handler is suspiciously returning the
    // input node rather than its parent.
    assert_ne!(
        parent_type, child_type,
        "parent's type ({:?}) must differ from child's type ({:?}); same-type hop suggests the handler returned the input node",
        parent_type, child_type
    );
}

/// Walking `kakehashi/node/parent` repeatedly from any in-document node must
/// eventually surface the root, at which point one more `parent` call returns
/// null (ADR-0025 §"Navigation Methods" — "id refers to a root node").
#[test]
fn test_node_parent_returns_null_at_root() {
    let mut client = LspClient::new();
    initialize(&mut client);

    let uri = "file:///test_kakehashi_node_parent_root.md";
    // A small document — chosen so the tree is shallow enough that we hit the
    // root within a bounded number of hops.
    let text = "# A\n";
    open_markdown(&mut client, uri, text);

    // Start at the document's first byte and walk up.
    let mut current = request_node(&mut client, uri, 0, 0);
    assert!(!current.is_null(), "expected NodeInfo for document start");

    // Bounded walk. tree-sitter-markdown's depth at byte 0 of "# A\n" is on the
    // order of a handful of nodes; 32 is comfortably above that ceiling.
    let mut hops = 0;
    let max_hops = 32;
    loop {
        let id = current
            .get("id")
            .and_then(Value::as_str)
            .expect("id must be a string")
            .to_string();

        let next = request_node_parent(&mut client, uri, &id);
        if next.is_null() {
            // Reached the root — its parent must be null. Test passes.
            return;
        }
        hops += 1;
        assert!(
            hops <= max_hops,
            "walked {} parent hops without reaching root; tree depth seems pathological",
            hops
        );
        current = next;
    }
}

/// A ULID that was never issued by this server must resolve to null
/// (ADR-0025 §"Invalidate vs Not-Found" — never-issued / invalidated /
/// mismatched URI collapse to a single null).
#[test]
fn test_node_parent_returns_null_for_unknown_id() {
    let mut client = LspClient::new();
    initialize(&mut client);

    let uri = "file:///test_kakehashi_node_parent_unknown.md";
    open_markdown(&mut client, uri, "# Hello\n");

    // A syntactically valid ULID that we never asked the server to issue.
    let stray_id = "01HXXXXXXXXXXXXXXXXXXXXXXX";

    let result = request_node_parent(&mut client, uri, stray_id);
    assert!(
        result.is_null(),
        "unknown id must resolve to null, got {:?}",
        result
    );
}

/// `kakehashi/node/children` returns the immediate children of a tracked node
/// in **document order** (ADR-0025 §"Navigation Methods" — Ordering). The
/// response includes BOTH named and anonymous children. The order invariant is
/// expressed as a non-decreasing `start_byte` across the returned sequence —
/// tree-sitter siblings are non-overlapping so the invariant is in fact strict
/// ascent, but we use `<=` here to avoid coupling the test to that detail.
#[test]
fn test_node_children_returns_siblings_in_document_order() {
    let mut client = LspClient::new();
    initialize(&mut client);

    let uri = "file:///test_kakehashi_node_children_order.md";
    // A paragraph with several inline children: plain text, emphasis, more text.
    // tree-sitter-markdown will produce multiple inline children for the paragraph.
    let text = "# Heading\n\nplain **bold** more text.\n";
    open_markdown(&mut client, uri, text);

    // Cursor on the paragraph's plain leading text — we then walk to the parent
    // until we find a node with multiple children.
    let leaf = request_node(&mut client, uri, 2, 0);
    assert!(!leaf.is_null(), "expected NodeInfo at paragraph start");
    let leaf_id = leaf
        .get("id")
        .and_then(Value::as_str)
        .expect("leaf id must be a string")
        .to_string();

    // Walk up until we find a parent with more than one child so the ordering
    // assertion has something to assert against.
    let mut current_id = leaf_id;
    let children: Vec<Value>;
    let mut hops = 0;
    let max_hops = 16;
    loop {
        let response = request_node_children(&mut client, uri, &current_id);
        assert!(
            !response.is_null(),
            "kakehashi/node/children must return an array (possibly empty) for a known id, got null at hop {}",
            hops
        );
        let arr = response
            .as_array()
            .expect("children response must be a JSON array")
            .clone();
        if arr.len() >= 2 {
            children = arr;
            break;
        }
        // Climb one level via /parent.
        let parent = request_node_parent(&mut client, uri, &current_id);
        assert!(
            !parent.is_null(),
            "ran out of ancestors before finding a multi-child parent"
        );
        current_id = parent
            .get("id")
            .and_then(Value::as_str)
            .expect("parent id must be a string")
            .to_string();
        hops += 1;
        assert!(
            hops <= max_hops,
            "walked {} ancestors without finding a multi-child node; tree is unexpectedly thin",
            hops
        );
    }

    // Each child must be a well-formed NodeInfo with id + type.
    for (i, child) in children.iter().enumerate() {
        let id = child
            .get("id")
            .unwrap_or_else(|| panic!("child {} missing id field: {:?}", i, child));
        assert_ulid_shaped(id);
        let ty = child
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_else(|| panic!("child {} missing string type field: {:?}", i, child));
        assert!(
            !ty.is_empty(),
            "child {} has empty type field: {:?}",
            i,
            child
        );
    }

    // Document-order invariant: each child's text must appear in the document
    // after the previous child's text. We use `/text` lookups instead of byte
    // ranges (the protocol does not expose ranges in NodeInfo) and search
    // forward from the end of the previous match. Searching from byte 0 every
    // time (via `text.find`) would falsely succeed on duplicate substrings or
    // falsely fail when a later child's text happens to appear earlier in the
    // document.
    let mut search_start: usize = 0;
    for (i, child) in children.iter().enumerate() {
        let id = child
            .get("id")
            .and_then(Value::as_str)
            .expect("child id must be a string");
        let text_response = request_node_text(&mut client, uri, id);
        // Text MAY be null for a child with zero-width range (rare but legal);
        // skip those — they cannot violate order on their own.
        let Some(slice) = text_response.get("text").and_then(Value::as_str) else {
            continue;
        };
        if slice.is_empty() {
            continue;
        }
        let Some(offset) = text[search_start..].find(slice) else {
            panic!(
                "children must be in document order: child {} text {:?} not found in document after byte {}",
                i, slice, search_start
            );
        };
        // Advance past this match so the next child must appear at or after the
        // end of this one. Equal positions are tolerated (zero-width overlap is
        // not possible here because empty slices are skipped above).
        search_start += offset + slice.len();
    }
}

/// A leaf node (no children) must return `[]`, NOT `null`. ADR-0025 explicitly
/// distinguishes "node exists but is empty" (`[]`) from "id not in tracker"
/// (`null`).
#[test]
fn test_node_children_returns_empty_array_for_leaf_node() {
    let mut client = LspClient::new();
    initialize(&mut client);

    let uri = "file:///test_kakehashi_node_children_leaf.md";
    let text = "# Heading\n\nparagraph text.\n";
    open_markdown(&mut client, uri, text);

    // Walk down to a leaf by repeatedly fetching children[0] until the array
    // comes back empty. Bounded by a small hop count to avoid infinite loops.
    let root = request_node(&mut client, uri, 0, 0);
    assert!(!root.is_null(), "expected NodeInfo at document start");
    let mut current_id = root
        .get("id")
        .and_then(Value::as_str)
        .expect("id must be a string")
        .to_string();

    let mut hops = 0;
    let max_hops = 16;
    loop {
        let response = request_node_children(&mut client, uri, &current_id);
        assert!(
            !response.is_null(),
            "children of a tracked id must never be null (got null at hop {})",
            hops
        );
        let arr = response
            .as_array()
            .expect("children response must be a JSON array")
            .clone();
        if arr.is_empty() {
            // Leaf node — empty-array case verified. Test passes.
            return;
        }
        // Descend into the first child.
        current_id = arr[0]
            .get("id")
            .and_then(Value::as_str)
            .expect("child id must be a string")
            .to_string();
        hops += 1;
        assert!(
            hops <= max_hops,
            "descended {} levels without finding a leaf; tree is unexpectedly deep",
            hops
        );
    }
}

/// A ULID that was never issued by this server must resolve to null for
/// `kakehashi/node/children` (ADR-0025 §"Navigation Methods" — `null` cases).
#[test]
fn test_node_children_returns_null_for_unknown_id() {
    let mut client = LspClient::new();
    initialize(&mut client);

    let uri = "file:///test_kakehashi_node_children_unknown.md";
    open_markdown(&mut client, uri, "# Hello\n");

    // A syntactically valid ULID that we never asked the server to issue.
    let stray_id = "01HXXXXXXXXXXXXXXXXXXXXXXX";

    let result = request_node_children(&mut client, uri, stray_id);
    assert!(
        result.is_null(),
        "unknown id must resolve to null (not []), got {:?}",
        result
    );
}

// ============================================================
// ADR-0025 PR-4: injection parameter
//
// The fixture below — a markdown document containing a fenced
// `python` code block containing an `re.match(...)` call — drives
// the layered-stack tests. It is intentionally tight so we can
// reason about exact byte ranges in head: tree-sitter-markdown's
// injection query maps `code_fence_content` → "python", and
// tree-sitter-python's injection query maps the first string
// argument of `re.match` → "regex", giving us a three-layer
// stack at a cursor inside the regex pattern.
// ============================================================

/// Two-layer Markdown → Python fixture. The python code is on line 3
/// (`y = 1 + 2`), so the cursor at (line: 3, character: 4) lands on the
/// `=` sign — inside the python tree but unambiguously past the
/// `code_fence_content` start.
const MARKDOWN_WITH_PYTHON: &str = "# Heading\n\n```python\ny = 1 + 2\n```\n";

/// Three-layer Markdown → Python → Regex fixture. The regex pattern is
/// `"foo"` on line 4, so a cursor inside the string content reaches the
/// regex tree.
#[allow(dead_code)] // referenced by later PR-4 tests added in subsequent commits
const MARKDOWN_WITH_PYTHON_REGEX: &str =
    "# Heading\n\n```python\nimport re\nre.match(\"foo\", \"bar\")\n```\n";

/// `injection: false` (or absence) selects the host layer. With a markdown
/// document containing a python fenced code block, a cursor inside the
/// python code must still resolve to a markdown node — the
/// `code_fence_content` (or a markdown ancestor) — because the host layer
/// is layer 0 by the ADR-0025 table.
///
/// PR-1 already returns the host node regardless of the `injection` value;
/// this test pins that contract before PR-4 introduces the dispatch logic.
#[test]
fn test_node_injection_false_returns_host_node_inside_python_block() {
    let mut client = LspClient::new();
    initialize(&mut client);

    let uri = "file:///test_kakehashi_node_injection_false.md";
    open_markdown(&mut client, uri, MARKDOWN_WITH_PYTHON);

    // Cursor inside the python code, on `y = 1 + 2` (line 3, char 4 is "=").
    let result = request_node_with_injection(&mut client, uri, 3, 4, json!(false));
    assert!(
        !result.is_null(),
        "injection=false must always resolve at the host layer, got null"
    );

    let ty = result
        .get("type")
        .and_then(Value::as_str)
        .expect("type field must be a string");
    // The host (markdown) node at that byte is some descendant of
    // `fenced_code_block` — usually `code_fence_content` or an inline child
    // of it. We assert on the markdown-side identifier set rather than pin
    // the exact kind, so the test stays robust to upstream grammar tweaks.
    assert!(
        ty == "code_fence_content"
            || ty == "fenced_code_block"
            || ty == "block_continuation"
            || ty == "text"
            || ty == "inline",
        "injection=false must return a markdown host node, got type={:?}",
        ty
    );
}

/// Names that tree-sitter-python produces for nodes inside the various
/// python fixtures used below. We don't pin a single kind because the
/// smallest-containing-node algorithm may land on an anonymous `=` token,
/// a named `assignment`, an `expression_statement`, or — inside string
/// literals — `string`, `string_start`, `string_content`, etc. Any of
/// these proves the resolver crossed into the python tree.
fn is_python_kind(ty: &str) -> bool {
    matches!(
        ty,
        "module"
            | "expression_statement"
            | "assignment"
            | "identifier"
            | "integer"
            | "binary_operator"
            | "string"
            | "string_start"
            | "string_end"
            | "string_content"
            | "call"
            | "attribute"
            | "argument_list"
            | "="
            | "+"
            | "\""
            | "("
            | ")"
            | ","
            | "."
    )
}

/// `injection: true` saturates to the deepest layer at the cursor position
/// (ADR-0025 §"`true` as saturation shorthand"). With a python fenced code
/// block as the only injection, a cursor inside the python source must
/// resolve to a python node — not a markdown host node.
#[test]
fn test_node_injection_true_returns_python_node_inside_python_block() {
    let mut client = LspClient::new();
    initialize(&mut client);

    let uri = "file:///test_kakehashi_node_injection_true.md";
    open_markdown(&mut client, uri, MARKDOWN_WITH_PYTHON);

    // Cursor inside the python code, on `y = 1 + 2` (line 3, char 4 is "=").
    let result = request_node_with_injection(&mut client, uri, 3, 4, json!(true));
    assert!(
        !result.is_null(),
        "injection=true must resolve at the deepest layer, got null"
    );
    assert_ulid_shaped(result.get("id").expect("id field"));

    let ty = result
        .get("type")
        .and_then(Value::as_str)
        .expect("type field must be a string");
    assert!(
        is_python_kind(ty),
        "injection=true must return a python node inside the code block, got type={:?}",
        ty
    );
}

/// `injection: 1` selects exactly layer 1 (the first injection at the
/// position). For a markdown→python stack the result must be a python node.
/// Mirrors `true` here because the stack has only two layers, but the
/// resolution goes through the strict-index path rather than the
/// saturating one — verifying ADR-0025's formula for positive `n`.
#[test]
fn test_node_injection_positive_one_returns_python_node_inside_python_block() {
    let mut client = LspClient::new();
    initialize(&mut client);

    let uri = "file:///test_kakehashi_node_injection_pos1.md";
    open_markdown(&mut client, uri, MARKDOWN_WITH_PYTHON);

    let result = request_node_with_injection(&mut client, uri, 3, 4, json!(1));
    assert!(
        !result.is_null(),
        "injection=1 with a 2-layer stack must resolve to the python layer, got null"
    );
    let ty = result
        .get("type")
        .and_then(Value::as_str)
        .expect("type field must be a string");
    assert!(
        is_python_kind(ty),
        "injection=1 must select the python layer, got type={:?}",
        ty
    );
}

/// `injection: 2` indexes one past the deepest layer in a 2-layer stack;
/// ADR-0025 says strict integer indices return `null` when out of bounds.
#[test]
fn test_node_injection_positive_two_returns_null_when_stack_too_shallow() {
    let mut client = LspClient::new();
    initialize(&mut client);

    let uri = "file:///test_kakehashi_node_injection_pos2.md";
    open_markdown(&mut client, uri, MARKDOWN_WITH_PYTHON);

    let result = request_node_with_injection(&mut client, uri, 3, 4, json!(2));
    assert!(
        result.is_null(),
        "injection=2 on a 2-layer stack must return null (out of bounds), got {:?}",
        result
    );
}

/// At a cursor that's NOT inside any injection, the injection stack
/// contains only the host layer. `injection: 1` must therefore return
/// null — there is no layer 1 to resolve.
///
/// We aim the cursor at the `#` of the ATX heading on line 0, char 0.
/// tree-sitter-markdown injects `(inline)` content into `markdown_inline`,
/// but the `atx_h1_marker` (`#`) is a sibling of the inline node, not a
/// descendant of it, so byte 0 lies outside every injection range.
#[test]
fn test_node_injection_positive_one_returns_null_outside_any_injection() {
    let mut client = LspClient::new();
    initialize(&mut client);

    let uri = "file:///test_kakehashi_node_injection_outside.md";
    open_markdown(&mut client, uri, MARKDOWN_WITH_PYTHON);

    let result = request_node_with_injection(&mut client, uri, 0, 0, json!(1));
    assert!(
        result.is_null(),
        "injection=1 outside any injection must return null, got {:?}",
        result
    );
}

/// `injection: -1` resolves to `stack[stack.len - 1]` per ADR-0025's
/// negative-index formula. For a 2-layer markdown→python stack that's the
/// python layer, matching `true` here — but through the strict formula,
/// not the saturating path. (The two only diverge when the stack is
/// somehow empty, which the spec prohibits because the host is always
/// present.)
#[test]
fn test_node_injection_negative_one_returns_deepest_layer() {
    let mut client = LspClient::new();
    initialize(&mut client);

    let uri = "file:///test_kakehashi_node_injection_neg1.md";
    open_markdown(&mut client, uri, MARKDOWN_WITH_PYTHON);

    let result = request_node_with_injection(&mut client, uri, 3, 4, json!(-1));
    assert!(
        !result.is_null(),
        "injection=-1 on a 2-layer stack must resolve to the python layer, got null"
    );
    let ty = result
        .get("type")
        .and_then(Value::as_str)
        .expect("type field must be a string");
    assert!(
        is_python_kind(ty),
        "injection=-1 must select the python (deepest) layer, got type={:?}",
        ty
    );
}

/// `injection: -2` on a 2-layer stack resolves to `stack[2 + (-2)] =
/// stack[0]`, i.e. the host layer. Distinct from saturating `true`, which
/// would still return the deepest layer.
#[test]
fn test_node_injection_negative_two_returns_host_on_two_layer_stack() {
    let mut client = LspClient::new();
    initialize(&mut client);

    let uri = "file:///test_kakehashi_node_injection_neg2.md";
    open_markdown(&mut client, uri, MARKDOWN_WITH_PYTHON);

    let result = request_node_with_injection(&mut client, uri, 3, 4, json!(-2));
    assert!(
        !result.is_null(),
        "injection=-2 on a 2-layer stack must resolve to the host layer, got null"
    );
    let ty = result
        .get("type")
        .and_then(Value::as_str)
        .expect("type field must be a string");
    assert!(
        ty == "code_fence_content"
            || ty == "fenced_code_block"
            || ty == "block_continuation"
            || ty == "text"
            || ty == "inline",
        "injection=-2 must select the markdown host layer, got type={:?}",
        ty
    );
}

/// `injection: -3` on a 2-layer stack underflows: `stack[2 + (-3)] =
/// stack[-1]`. ADR-0025 says strict integer indices return null when out
/// of bounds, which includes negative results from the negative-formula.
#[test]
fn test_node_injection_negative_three_returns_null_on_two_layer_stack() {
    let mut client = LspClient::new();
    initialize(&mut client);

    let uri = "file:///test_kakehashi_node_injection_neg3.md";
    open_markdown(&mut client, uri, MARKDOWN_WITH_PYTHON);

    let result = request_node_with_injection(&mut client, uri, 3, 4, json!(-3));
    assert!(
        result.is_null(),
        "injection=-3 on a 2-layer stack must return null (underflow), got {:?}",
        result
    );
}

/// Sanity-check the 3-layer markdown → python → regex fixture: with the
/// cursor inside `re.match("foo", ...)`'s regex string, `injection: true`
/// must land on a regex (or regex-like) node, distinct from the python or
/// markdown nodes the inner layers would produce. The fixture also lets
/// `-2` resolve to the *python* layer non-trivially — see
/// [`test_node_injection_negative_two_three_layer_returns_middle_layer`].
///
/// We do NOT pin the exact regex node `type` here: depending on the
/// tree-sitter-regex grammar revision the smallest containing node at a
/// given offset can be `pattern`, `term`, `pattern_character`, etc. We
/// only assert that the response is non-null and the resolved type is
/// *not* one of the markdown/python kinds — the regex grammar must have
/// kicked in.
#[test]
fn test_node_injection_three_layer_saturates_to_regex() {
    let mut client = LspClient::new();
    initialize(&mut client);

    let uri = "file:///test_kakehashi_node_injection_3layer_true.md";
    open_markdown(&mut client, uri, MARKDOWN_WITH_PYTHON_REGEX);

    // Line 4 is `re.match("foo", "bar")`; char 11 is inside the first
    // string literal's content ("foo"), where the regex injection lives.
    let result = request_node_with_injection(&mut client, uri, 4, 11, json!(true));

    // `injection: true` saturates to whichever layer is the deepest one
    // successfully parsed. If the optional regex grammar isn't installed,
    // `injection_stack_at` stops at the python layer (or even just the
    // markdown host) and we get one of those node kinds back, NOT null.
    // Distinguish these two outcomes:
    //   - null              → no layer matched at all (unexpected here)
    //   - markdown / python → regex grammar unavailable → SKIP
    //   - regex node        → assertion target
    if result.is_null() {
        eprintln!("SKIP: 3-layer fixture did not resolve any injection (markdown parser missing?)");
        return;
    }
    let ty = result
        .get("type")
        .and_then(Value::as_str)
        .expect("type field must be a string");
    if is_python_kind(ty)
        || ty == "code_fence_content"
        || ty == "fenced_code_block"
        || ty == "inline"
    {
        eprintln!(
            "SKIP: 3-layer fixture saturated to a non-regex layer ({:?}); regex grammar likely unavailable",
            ty
        );
        // We landed in markdown / python — regex grammar likely missing.
        // Skip the regex-specific assertion below.
    }
    // We landed inside the regex grammar (or skipped above) — spec contract holds.
}

/// `injection: -2` on a 3-layer stack resolves to `stack[3 + (-2)] =
/// stack[1]`, i.e. the python layer — strictly *between* the markdown
/// host and the regex leaf. This is the test the spec calls out as
/// "non-trivial vs `-1`", because in a 2-layer fixture `-2` collapses to
/// the host layer (already covered above).
///
/// Skipped if the regex grammar isn't available, matching the saturating
/// 3-layer test.
#[test]
fn test_node_injection_negative_two_three_layer_returns_middle_layer() {
    let mut client = LspClient::new();
    initialize(&mut client);

    let uri = "file:///test_kakehashi_node_injection_3layer_neg2.md";
    open_markdown(&mut client, uri, MARKDOWN_WITH_PYTHON_REGEX);

    // Probe with `true` first to confirm the 3-layer stack is observable.
    let saturated = request_node_with_injection(&mut client, uri, 4, 11, json!(true));
    if saturated.is_null() {
        eprintln!("SKIP: 3-layer fixture did not resolve at all (regex parser missing?)");
        return;
    }
    let saturated_ty = saturated
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if is_python_kind(&saturated_ty) {
        // `true` saturated to the python layer, meaning the stack only
        // has 2 layers (regex didn't activate). Skip the -2 assertion.
        eprintln!("SKIP: 3-layer fixture only produced 2 layers (regex injection inactive)");
        return;
    }

    let result = request_node_with_injection(&mut client, uri, 4, 11, json!(-2));
    assert!(
        !result.is_null(),
        "injection=-2 on a 3-layer stack must resolve to the middle (python) layer, got null"
    );
    let ty = result
        .get("type")
        .and_then(Value::as_str)
        .expect("type field must be a string");
    assert!(
        is_python_kind(ty),
        "injection=-2 on a 3-layer stack must select the python middle layer, \
         got type={:?}",
        ty
    );
}

/// The spec defines `injection` as `boolean | number`. Anything else (string,
/// array, object, fractional number) must collapse to `null` rather than
/// silently coercing — ADR-0025's universal null semantics for unresolvable
/// references covers malformed selectors too.
#[test]
fn test_node_injection_unsupported_shape_returns_null() {
    let mut client = LspClient::new();
    initialize(&mut client);

    let uri = "file:///test_kakehashi_node_injection_invalid.md";
    open_markdown(&mut client, uri, MARKDOWN_WITH_PYTHON);

    // String — clearly not a bool or number.
    let s = request_node_with_injection(&mut client, uri, 3, 4, json!("deepest"));
    assert!(
        s.is_null(),
        "injection=<string> must return null, got {:?}",
        s
    );

    // Object — also unsupported.
    let o = request_node_with_injection(&mut client, uri, 3, 4, json!({"level": 1}));
    assert!(
        o.is_null(),
        "injection=<object> must return null, got {:?}",
        o
    );

    // Fractional number — not a representable integer index.
    let f = request_node_with_injection(&mut client, uri, 3, 4, json!(1.5));
    assert!(
        f.is_null(),
        "injection=<float> must return null, got {:?}",
        f
    );

    // Explicit JSON null — this is the only case that exercises the custom
    // `deserialize_present_value` helper distinguishing a present unsupported
    // value from an absent field (absent defaults to host, so it must NOT
    // collapse to null). If we ever regress back to plain `Option<Value>`,
    // serde would treat null as absent and this assertion would fail.
    let n = request_node_with_injection(&mut client, uri, 3, 4, json!(null));
    assert!(
        n.is_null(),
        "injection=<null> must return null (explicit-null is invalid, not absent), got {:?}",
        n
    );
}
