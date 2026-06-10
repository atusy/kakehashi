//! End-to-end tests for `kakehashi/captures/{full, full/delta, range}`
//! (captures-protocol).
//!
//! Exercises the full LSP round-trip a treesitter-context-style client makes:
//! install a `context.scm` for markdown into a search path, open a document,
//! and drive the semanticTokens-style triple — `full` for the first paint,
//! `full/delta` on subsequent cursor/edit ticks, `range` for a viewport.
//!
//! Covered:
//! - `full` returning match-grouped captures with inline ranges, trackable
//!   `NodeInfo`s, and a `resultId`
//! - `full/delta` with an up-to-date `previousResultId` → empty `edits`
//! - `full/delta` after an edit → a single positional edit with the new match
//! - `full/delta` with an unknown `previousResultId` → full result fallback
//! - `range` returning only matches intersecting the range (no `resultId`)
//! - a kind with no query file → `null`
//! - a malformed kind (path traversal) → JSON-RPC error
//!
//! Run with: `cargo test --test e2e_kakehashi_captures --features e2e`

#![cfg(feature = "e2e")]

mod helpers;

use helpers::lsp_client::LspClient;
use serde_json::{Value, json};

/// Create a search-path root holding `queries/markdown/context.scm`.
fn context_query_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("create tempdir");
    let qdir = dir.path().join("queries").join("markdown");
    std::fs::create_dir_all(&qdir).expect("create queries/markdown");
    std::fs::write(qdir.join("context.scm"), "(atx_heading) @context\n")
        .expect("write context.scm");
    dir
}

/// Initialize + `initialized`, pointing `searchPaths` at `query_root`.
///
/// `${KAKEHASHI_DATA_DIR}` (the lone default search path) must be kept
/// alongside the temp dir: overriding `searchPaths` replaces the default, and
/// without the data dir the auto-installed markdown parser is unfindable —
/// every request then nulls out with "no parsed document".
fn initialize(client: &mut LspClient, query_root: &std::path::Path) {
    client.send_request(
        "initialize",
        json!({
            "processId": std::process::id(),
            "rootUri": null,
            "capabilities": {},
            "initializationOptions": {
                "searchPaths": [
                    query_root.to_str().expect("utf-8 tempdir path"),
                    "${KAKEHASHI_DATA_DIR}"
                ]
            }
        }),
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

/// Replace the whole document text via `textDocument/didChange`.
fn change_full_text(client: &mut LspClient, uri: &str, version: i64, text: &str) {
    client.send_notification(
        "textDocument/didChange",
        json!({
            "textDocument": { "uri": uri, "version": version },
            "contentChanges": [{ "text": text }]
        }),
    );
}

/// Send a captures request and unwrap a successful `result`.
fn request(client: &mut LspClient, method: &str, params: Value) -> Value {
    let response = client.send_request(method, params);
    assert!(
        response.get("error").is_none(),
        "{method} returned an error: {:?}",
        response.get("error")
    );
    response
        .get("result")
        .cloned()
        .expect("response must contain a result field")
}

fn full(client: &mut LspClient, uri: &str, kind: &str) -> Value {
    request(
        client,
        "kakehashi/captures/full",
        json!({ "textDocument": { "uri": uri }, "kind": kind }),
    )
}

fn delta(client: &mut LspClient, uri: &str, kind: &str, previous_result_id: &str) -> Value {
    request(
        client,
        "kakehashi/captures/full/delta",
        json!({
            "textDocument": { "uri": uri },
            "kind": kind,
            "previousResultId": previous_result_id
        }),
    )
}

fn result_id_of(result: &Value) -> String {
    result
        .get("resultId")
        .and_then(Value::as_str)
        .expect("result must carry a resultId")
        .to_string()
}

/// Two headings — the things a "sticky context" feature renders.
const DOC: &str = "# Title\n\nintro text\n\n## Section A\n\nbody text\n";

#[test]
fn full_returns_grouped_matches_with_ranges_and_result_id() {
    let dir = context_query_dir();
    let mut client = LspClient::new();
    initialize(&mut client, dir.path());
    let uri = "file:///captures_full.md";
    open_markdown(&mut client, uri, DOC);

    let result = full(&mut client, uri, "context");

    assert!(
        result.get("resultId").and_then(Value::as_str).is_some(),
        "full must hand out a resultId: {result:?}"
    );
    let matches = result
        .get("matches")
        .and_then(Value::as_array)
        .expect("result.matches must be an array");
    assert_eq!(matches.len(), 2, "two headings -> two matches: {matches:?}");

    let capture = matches[0]
        .pointer("/captures/0")
        .expect("match must carry captures");
    assert_eq!(capture.get("name").and_then(Value::as_str), Some("context"));
    assert_eq!(
        capture.pointer("/node/kind").and_then(Value::as_str),
        Some("atx_heading")
    );
    assert!(
        capture
            .pointer("/node/id")
            .and_then(Value::as_str)
            .is_some(),
        "NodeInfo must carry a ULID id"
    );
    assert_eq!(
        capture.pointer("/range/start/line").and_then(Value::as_u64),
        Some(0),
        "# Title starts on line 0"
    );
    assert_eq!(
        matches[1]
            .pointer("/captures/0/range/start/line")
            .and_then(Value::as_u64),
        Some(4),
        "## Section A starts on line 4"
    );
}

#[test]
fn delta_without_changes_returns_empty_edits() {
    let dir = context_query_dir();
    let mut client = LspClient::new();
    initialize(&mut client, dir.path());
    let uri = "file:///captures_delta_same.md";
    open_markdown(&mut client, uri, DOC);

    let id1 = result_id_of(&full(&mut client, uri, "context"));
    let d = delta(&mut client, uri, "context", &id1);

    assert_eq!(
        d.get("edits").and_then(Value::as_array).map(Vec::len),
        Some(0),
        "unchanged document -> empty edits: {d:?}"
    );
    assert_ne!(
        result_id_of(&d),
        id1,
        "every delta response advances the resultId lineage"
    );
}

#[test]
fn delta_after_edit_returns_single_positional_edit() {
    let dir = context_query_dir();
    let mut client = LspClient::new();
    initialize(&mut client, dir.path());
    let uri = "file:///captures_delta_edit.md";
    open_markdown(&mut client, uri, "# A\n\ntext\n");

    let id1 = result_id_of(&full(&mut client, uri, "context"));
    change_full_text(&mut client, uri, 2, "# A\n\ntext\n\n## B\n");

    let d = delta(&mut client, uri, "context", &id1);
    let edits = d
        .get("edits")
        .and_then(Value::as_array)
        .expect("matching previousResultId -> delta with edits");
    assert_eq!(edits.len(), 1, "single positional edit: {edits:?}");
    let edit = &edits[0];
    assert_eq!(edit.get("start").and_then(Value::as_u64), Some(1));
    assert_eq!(edit.get("deleteCount").and_then(Value::as_u64), Some(0));
    let data = edit.get("data").and_then(Value::as_array).unwrap();
    assert_eq!(data.len(), 1, "the appended heading arrives in data");
    assert_eq!(
        data[0]
            .pointer("/captures/0/range/start/line")
            .and_then(Value::as_u64),
        Some(4),
        "## B starts on line 4"
    );
}

#[test]
fn delta_with_unknown_result_id_falls_back_to_full() {
    let dir = context_query_dir();
    let mut client = LspClient::new();
    initialize(&mut client, dir.path());
    let uri = "file:///captures_delta_unknown.md";
    open_markdown(&mut client, uri, DOC);

    let _ = full(&mut client, uri, "context");
    let d = delta(&mut client, uri, "context", "bogus-result-id");

    assert!(
        d.get("matches").and_then(Value::as_array).is_some(),
        "unknown previousResultId -> full result, not edits: {d:?}"
    );
    assert!(d.get("edits").is_none());
}

#[test]
fn range_returns_only_intersecting_matches() {
    let dir = context_query_dir();
    let mut client = LspClient::new();
    initialize(&mut client, dir.path());
    let uri = "file:///captures_range.md";
    open_markdown(&mut client, uri, DOC);

    let result = request(
        &mut client,
        "kakehashi/captures/range",
        json!({
            "textDocument": { "uri": uri },
            "kind": "context",
            "range": {
                "start": { "line": 4, "character": 0 },
                "end": { "line": 5, "character": 0 }
            }
        }),
    );

    let matches = result.get("matches").and_then(Value::as_array).unwrap();
    assert_eq!(
        matches.len(),
        1,
        "only ## Section A intersects: {matches:?}"
    );
    assert_eq!(
        matches[0]
            .pointer("/captures/0/range/start/line")
            .and_then(Value::as_u64),
        Some(4)
    );
    assert!(
        result.get("resultId").is_none(),
        "range results carry no resultId (no delta lineage)"
    );
}

#[test]
fn unknown_kind_returns_null() {
    let dir = context_query_dir();
    let mut client = LspClient::new();
    initialize(&mut client, dir.path());
    let uri = "file:///captures_unknown_kind.md";
    open_markdown(&mut client, uri, DOC);

    let result = full(&mut client, uri, "nosuchkind");
    assert_eq!(
        result,
        Value::Null,
        "a kind with no query file for the language -> null"
    );
}

#[test]
fn malformed_kind_returns_error() {
    let dir = context_query_dir();
    let mut client = LspClient::new();
    initialize(&mut client, dir.path());
    let uri = "file:///captures_bad_kind.md";
    open_markdown(&mut client, uri, DOC);

    let response = client.send_request(
        "kakehashi/captures/full",
        json!({ "textDocument": { "uri": uri }, "kind": "../evil" }),
    );
    assert!(
        response.get("error").is_some(),
        "a path-traversal kind must surface a JSON-RPC error, got: {response:?}"
    );
}
