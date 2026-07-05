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
//! - `#set!` directives → `metadata` on the match (`(#set! k v)`) or on the
//!   capture (`(#set! @cap k v)`), absent on unannotated patterns/captures
//! - a kind with no query file → `null`
//! - a malformed kind (path traversal) → JSON-RPC error
//!
//! Run with: `cargo test --test e2e_kakehashi_captures --features e2e`

#![cfg(feature = "e2e")]

mod helpers;

use helpers::lsp_client::LspClient;
use serde_json::{Value, json};

/// Create a search-path root holding `queries/markdown/context.scm` and
/// `queries/python/context.scm` (the latter exercises injection-aware
/// collection: each layer resolves its own language's kind file).
fn context_query_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("create tempdir");
    let md = dir.path().join("queries").join("markdown");
    std::fs::create_dir_all(&md).expect("create queries/markdown");
    std::fs::write(md.join("context.scm"), "(atx_heading) @context\n")
        .expect("write markdown context.scm");
    let py = dir.path().join("queries").join("python");
    std::fs::create_dir_all(&py).expect("create queries/python");
    std::fs::write(py.join("context.scm"), "(function_definition) @context\n")
        .expect("write python context.scm");
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

fn full_with_injection(client: &mut LspClient, uri: &str, kind: &str) -> Value {
    request(
        client,
        "kakehashi/captures/full",
        json!({ "textDocument": { "uri": uri }, "kind": kind, "injection": true }),
    )
}

/// Languages of the matches in a full/range result, in match order.
fn match_languages(result: &Value) -> Vec<String> {
    result
        .get("matches")
        .and_then(Value::as_array)
        .expect("result.matches must be an array")
        .iter()
        .map(|m| {
            m.get("language")
                .and_then(Value::as_str)
                .expect("every match must carry a language")
                .to_string()
        })
        .collect()
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

/// Follow the delta resultId chain until `pred` accepts a response (bounded).
///
/// Under the parse-snapshot model (serve-stale + heal, ADR §3) a delta issued
/// right after an edit may serve a snapshot that still trails the edit (a
/// delta without the edit's matches) or answer `null` when the edit raced the
/// request mid-flight (the lineage gate). A real client simply re-requests —
/// on `null` with the same id (the lineage is intact), otherwise following the
/// rotated id — and converges once the off-ingress reparse publishes.
fn delta_until(
    client: &mut LspClient,
    uri: &str,
    kind: &str,
    initial_id: &str,
    pred: impl Fn(&Value) -> bool,
) -> Value {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut id = initial_id.to_string();
    loop {
        let d = delta(client, uri, kind, &id);
        if pred(&d) {
            return d;
        }
        // Fail loudly at the bound: returning the non-matching response would
        // surface later as a less-informative assertion (e.g. missing edits)
        // instead of pointing at the eventual-consistency bound itself.
        assert!(
            std::time::Instant::now() <= deadline,
            "delta_until: predicate did not converge within 10s; last response: {d:?}"
        );
        if let Some(next) = d.get("resultId").and_then(Value::as_str) {
            id = next.to_string();
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
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
fn set_directive_yields_match_level_metadata() {
    // `(#set! key value)` (treesitter-directive-set!) sets match-level
    // metadata; patterns without `#set!` carry no metadata field at all.
    let dir = tempfile::tempdir().expect("create tempdir");
    let md = dir.path().join("queries").join("markdown");
    std::fs::create_dir_all(&md).expect("create queries/markdown");
    std::fs::write(
        md.join("context.scm"),
        "((atx_heading) @context (#set! kind \"heading\"))\n(thematic_break) @context\n",
    )
    .expect("write markdown context.scm");
    let mut client = LspClient::new();
    initialize(&mut client, dir.path());
    let uri = "file:///captures_set_match_metadata.md";
    open_markdown(&mut client, uri, "# Title\n\n---\n");

    let result = full(&mut client, uri, "context");

    let matches = result
        .get("matches")
        .and_then(Value::as_array)
        .expect("result.matches must be an array");
    assert_eq!(
        matches.len(),
        2,
        "one heading + one thematic break: {matches:?}"
    );
    assert_eq!(
        matches[0].get("metadata"),
        Some(&json!({ "kind": "heading" })),
        "#set! pattern carries match-level metadata: {matches:?}"
    );
    assert_eq!(
        matches[1].get("metadata"),
        None,
        "a pattern without #set! has no metadata field: {matches:?}"
    );
}

#[test]
fn set_directive_with_capture_yields_capture_level_metadata() {
    // `(#set! @capture key value)` scopes the metadata to that capture
    // (treesitter-directive-set!): it rides on the capture entry, not the
    // match envelope, and unannotated captures stay metadata-free.
    let dir = tempfile::tempdir().expect("create tempdir");
    let md = dir.path().join("queries").join("markdown");
    std::fs::create_dir_all(&md).expect("create queries/markdown");
    std::fs::write(
        md.join("context.scm"),
        "((atx_heading (inline) @text) @context (#set! @text role \"title\"))\n",
    )
    .expect("write markdown context.scm");
    let mut client = LspClient::new();
    initialize(&mut client, dir.path());
    let uri = "file:///captures_set_capture_metadata.md";
    open_markdown(&mut client, uri, "# Title\n");

    let result = full(&mut client, uri, "context");

    let matches = result
        .get("matches")
        .and_then(Value::as_array)
        .expect("result.matches must be an array");
    assert_eq!(matches.len(), 1, "one heading: {matches:?}");
    assert_eq!(
        matches[0].get("metadata"),
        None,
        "capture-scoped #set! does not appear match-level: {matches:?}"
    );
    let captures = matches[0]
        .get("captures")
        .and_then(Value::as_array)
        .expect("match must carry captures");
    let by_name = |name: &str| {
        captures
            .iter()
            .find(|c| c.get("name").and_then(Value::as_str) == Some(name))
            .unwrap_or_else(|| panic!("capture @{name} present: {captures:?}"))
    };
    assert_eq!(
        by_name("text").get("metadata"),
        Some(&json!({ "role": "title" })),
        "@text carries its #set! metadata: {captures:?}"
    );
    assert_eq!(
        by_name("context").get("metadata"),
        None,
        "unannotated @context has no metadata field: {captures:?}"
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

    // Serve-stale + heal (ADR §3): converge on the delta that carries the edit.
    let d = delta_until(&mut client, uri, "context", &id1, |d| {
        d.get("edits")
            .and_then(Value::as_array)
            .is_some_and(|e| !e.is_empty())
    });
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

/// Markdown with an embedded Python block — the cross-language sticky-context
/// case: `injection: true` should yield the markdown heading AND the python
/// function in one response.
const DOC_WITH_PYTHON: &str = "# Title\n\n```python\ndef f():\n    pass\n```\n";

#[test]
fn full_with_injection_collects_all_layers() {
    let dir = context_query_dir();
    let mut client = LspClient::new();
    initialize(&mut client, dir.path());
    let uri = "file:///captures_injection.md";
    open_markdown(&mut client, uri, DOC_WITH_PYTHON);

    let result = full_with_injection(&mut client, uri, "context");
    let langs = match_languages(&result);
    assert!(
        langs.contains(&"markdown".to_string()),
        "host heading match expected: {langs:?}"
    );
    assert!(
        langs.contains(&"python".to_string()),
        "injected function match expected: {langs:?}"
    );

    // Ordering: document-order DFS — the host heading precedes the python match.
    assert_eq!(langs.first().map(String::as_str), Some("markdown"));

    // The python match's node is minted in its layer and composes with
    // kakehashi/node/*: feeding the id back resolves to the python node kind.
    let matches = result.get("matches").and_then(Value::as_array).unwrap();
    let py = matches
        .iter()
        .find(|m| m.get("language").and_then(Value::as_str) == Some("python"))
        .expect("python match present");
    let id = py
        .pointer("/captures/0/node/id")
        .and_then(Value::as_str)
        .expect("python capture has a node id");
    let response = client.send_request(
        "kakehashi/node/kind",
        json!({ "textDocument": { "uri": uri }, "id": id }),
    );
    assert_eq!(
        response.pointer("/result/kind").and_then(Value::as_str),
        Some("function_definition"),
        "injected-layer node id must resolve in its minting layer"
    );
}

#[test]
fn full_without_injection_stays_host_only_with_language() {
    let dir = context_query_dir();
    let mut client = LspClient::new();
    initialize(&mut client, dir.path());
    let uri = "file:///captures_host_only.md";
    open_markdown(&mut client, uri, DOC_WITH_PYTHON);

    let result = full(&mut client, uri, "context");
    let langs = match_languages(&result);
    assert_eq!(
        langs,
        vec!["markdown".to_string()],
        "host-only mode must not surface injected layers, and still tags language"
    );
}

#[test]
fn delta_inherits_injection_mode_from_lineage() {
    let dir = context_query_dir();
    let mut client = LspClient::new();
    initialize(&mut client, dir.path());
    let uri = "file:///captures_delta_injection.md";
    open_markdown(&mut client, uri, DOC_WITH_PYTHON);

    let _ = full_with_injection(&mut client, uri, "context");

    // A stale previousResultId forces the full-fallback path, which recomputes
    // under the lineage's STORED mode. The delta request itself carries no
    // injection parameter, so the python match appearing in the fallback
    // proves the mode was inherited from the initial full.
    let d = delta(&mut client, uri, "context", "stale-id");
    let langs = match_languages(&d);
    assert!(
        langs.contains(&"python".to_string()),
        "inherited injection mode must surface python matches: {langs:?}"
    );
}

/// The edit-driven variant of inheritance: full(injection) → didChange →
/// delta sees the new python match in its edits.
///
/// Also the regression test for issue #348: a full-text didChange used to
/// seed the reparse with the UNEDITED stored tree, which corrupts the heap in
/// the markdown external scanner and killed the server. The crash needed a
/// stored tree to exist at didChange time — exactly what the preceding
/// `full` request guarantees via ensure_parsed.
#[test]
fn delta_after_edit_carries_injected_matches() {
    let dir = context_query_dir();
    let mut client = LspClient::new();
    initialize(&mut client, dir.path());
    let uri = "file:///captures_delta_injection_edit.md";
    open_markdown(&mut client, uri, DOC_WITH_PYTHON);

    let id1 = result_id_of(&full_with_injection(&mut client, uri, "context"));

    let edited = format!("{DOC_WITH_PYTHON}\n```python\ndef g():\n    pass\n```\n");
    change_full_text(&mut client, uri, 2, &edited);

    // Serve-stale + heal (ADR §3): converge on the delta carrying the python match.
    let d = delta_until(&mut client, uri, "context", &id1, |d| {
        d.get("edits")
            .and_then(Value::as_array)
            .is_some_and(|edits| {
                edits
                    .iter()
                    .flat_map(|e| {
                        e.get("data")
                            .and_then(Value::as_array)
                            .into_iter()
                            .flatten()
                    })
                    .filter_map(|m| m.get("language").and_then(Value::as_str))
                    .any(|l| l == "python")
            })
    });
    let edits = d
        .get("edits")
        .and_then(Value::as_array)
        .expect("matching previousResultId -> delta with edits");
    let added_langs: Vec<&str> = edits
        .iter()
        .flat_map(|e| {
            e.get("data")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
        })
        .filter_map(|m| m.get("language").and_then(Value::as_str))
        .collect();
    assert!(
        added_langs.contains(&"python"),
        "inherited injection mode must surface the new python match: {d:?}"
    );
}

#[test]
fn delta_without_lineage_returns_null() {
    let dir = context_query_dir();
    let mut client = LspClient::new();
    initialize(&mut client, dir.path());
    let uri = "file:///captures_delta_no_lineage.md";
    open_markdown(&mut client, uri, DOC_WITH_PYTHON);

    // No prior full: the server cannot know the client's injection intent, so
    // a full fallback could silently serve the wrong layer set -> null.
    let result = delta(&mut client, uri, "context", "never-issued-id");
    assert_eq!(result, Value::Null, "lineage-less delta must be null");
}

#[test]
fn range_with_injection_prunes_to_intersecting_layers() {
    let dir = context_query_dir();
    let mut client = LspClient::new();
    initialize(&mut client, dir.path());
    let uri = "file:///captures_range_injection.md";
    open_markdown(&mut client, uri, DOC_WITH_PYTHON);

    // Lines 3-4 cover only the python function body, not the heading.
    let result = request(
        &mut client,
        "kakehashi/captures/range",
        json!({
            "textDocument": { "uri": uri },
            "kind": "context",
            "injection": true,
            "range": {
                "start": { "line": 3, "character": 0 },
                "end": { "line": 5, "character": 0 }
            }
        }),
    );
    let langs = match_languages(&result);
    assert_eq!(
        langs,
        vec!["python".to_string()],
        "only the python layer intersects the range: {result:?}"
    );
}

#[test]
fn per_mode_lineages_do_not_clobber_each_other() {
    let dir = context_query_dir();
    let mut client = LspClient::new();
    initialize(&mut client, dir.path());
    let uri = "file:///captures_mode_isolation.md";
    open_markdown(&mut client, uri, DOC_WITH_PYTHON);

    // Injection lineage first, then a host-only full for the SAME (uri, kind):
    // the host full must not clobber the injection lineage's mode.
    let id_injection = result_id_of(&full_with_injection(&mut client, uri, "context"));
    let id_host = result_id_of(&full(&mut client, uri, "context"));

    // Edit so the injection-mode delta has something to report.
    let edited = format!("{DOC_WITH_PYTHON}\n```python\ndef g():\n    pass\n```\n");
    change_full_text(&mut client, uri, 2, &edited);

    // Serve-stale + heal (ADR §3): converge on the delta carrying the python match.
    let d = delta_until(&mut client, uri, "context", &id_injection, |d| {
        d.get("edits")
            .and_then(Value::as_array)
            .is_some_and(|edits| {
                edits
                    .iter()
                    .flat_map(|e| {
                        e.get("data")
                            .and_then(Value::as_array)
                            .into_iter()
                            .flatten()
                    })
                    .filter_map(|m| m.get("language").and_then(Value::as_str))
                    .any(|l| l == "python")
            })
    });
    let edits = d
        .get("edits")
        .and_then(Value::as_array)
        .expect("injection lineage must still answer with a delta: {d:?}");
    let added_langs: Vec<&str> = edits
        .iter()
        .flat_map(|e| {
            e.get("data")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
        })
        .filter_map(|m| m.get("language").and_then(Value::as_str))
        .collect();
    assert!(
        added_langs.contains(&"python"),
        "the injection-mode lineage must surface the new python match \
         despite the interleaved host-only full: {d:?}"
    );

    // The host lineage answers under host mode: its edits must not contain
    // python matches (only the markdown side of the edit, if any).
    let dh = delta(&mut client, uri, "context", &id_host);
    let host_edits = dh
        .get("edits")
        .and_then(Value::as_array)
        .expect("host lineage must also answer with a delta: {dh:?}");
    let host_langs: Vec<&str> = host_edits
        .iter()
        .flat_map(|e| {
            e.get("data")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
        })
        .filter_map(|m| m.get("language").and_then(Value::as_str))
        .collect();
    assert!(
        !host_langs.contains(&"python"),
        "the host-only lineage must stay host-only: {dh:?}"
    );
}

#[test]
fn stale_id_with_both_modes_live_returns_null() {
    let dir = context_query_dir();
    let mut client = LspClient::new();
    initialize(&mut client, dir.path());
    let uri = "file:///captures_mode_ambiguous.md";
    open_markdown(&mut client, uri, DOC_WITH_PYTHON);

    let _ = full_with_injection(&mut client, uri, "context");
    let _ = full(&mut client, uri, "context");

    // With BOTH mode lineages live, a stale id cannot pick a mode without
    // guessing — the protocol answers null ("re-acquire via full").
    let d = delta(&mut client, uri, "context", "stale-id");
    assert_eq!(
        d,
        Value::Null,
        "ambiguous mode for a stale id must be null, not a guessed full"
    );
}

#[test]
fn broken_kind_file_still_reports_skipped_when_other_layers_match() {
    // markdown context.scm is valid; python's exists but is wholly invalid.
    let dir = tempfile::tempdir().expect("create tempdir");
    let md = dir.path().join("queries").join("markdown");
    std::fs::create_dir_all(&md).unwrap();
    std::fs::write(md.join("context.scm"), "(atx_heading) @context\n").unwrap();
    let py = dir.path().join("queries").join("python");
    std::fs::create_dir_all(&py).unwrap();
    std::fs::write(py.join("context.scm"), "(no_such_python_node) @broken\n").unwrap();

    let mut client = LspClient::new();
    initialize(&mut client, dir.path());
    let uri = "file:///captures_broken_kind.md";
    open_markdown(&mut client, uri, DOC_WITH_PYTHON);

    let result = full_with_injection(&mut client, uri, "context");

    // markdown still matches; python contributes nothing...
    let langs = match_languages(&result);
    assert!(langs.contains(&"markdown".to_string()));
    assert!(!langs.contains(&"python".to_string()));

    // ...but its broken patterns surface in `skipped` for debuggability.
    let skipped = result
        .get("skipped")
        .and_then(Value::as_array)
        .expect("result.skipped must be an array");
    assert!(
        skipped
            .iter()
            .any(|s| s.get("language").and_then(Value::as_str) == Some("python")),
        "the broken python kind file must appear in skipped: {skipped:?}"
    );
}
