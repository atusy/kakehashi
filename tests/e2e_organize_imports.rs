//! Acceptance E2E for #568 PR 8: organize-imports and diagnostic→quickfix on a
//! python block inside markdown, bridged to a real `ruff server`. Composes the
//! whole codeAction stack — request over the injection region,
//! `source.organizeImports`/quickfix matched, `codeAction/resolve` round-trip,
//! and the resolved edit re-keyed to the host markdown document with
//! host-translated ranges.
//!
//! To stay robust across ruff versions the tests do NOT pin ruff's exact edit
//! shape; they APPLY the returned edits to the host markdown and assert the
//! resulting document — which still fails if the bridge mis-translates or
//! mis-keys the edit, but tolerates ruff changing how it slices the edit.
//!
//! Skips when `ruff` is not on PATH (same system-dependency pattern as the
//! lua-language-server bridge tests).

#![cfg(feature = "e2e")]

mod helpers;

use std::time::Duration;

use helpers::lsp_client::LspClient;
use serde_json::{Value, json};

fn is_ruff_available() -> bool {
    std::process::Command::new("ruff")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn resolve_support_caps() -> Value {
    json!({ "textDocument": { "codeAction": {
        "codeActionLiteralSupport": { "codeActionKind": { "valueSet": [] } },
        "disabledSupport": true,
        "dataSupport": true,
        "resolveSupport": { "properties": ["edit"] },
        "isPreferredSupport": true
    }}})
}

/// A kakehashi client with `ruff server` bridged for python, plus the temp
/// workspace whose root the document URIs live under (kept alive by the caller).
fn init_ruff_client() -> (LspClient, tempfile::TempDir, tempfile::TempDir) {
    let workspace = tempfile::TempDir::new().expect("workspace temp dir");
    let config_dir = tempfile::TempDir::new().expect("config temp dir");
    let config_path = config_dir.path().join("empty.toml");
    std::fs::write(&config_path, "").expect("write empty config");

    let mut client = LspClient::builder()
        .arg("--config-file")
        .arg(config_path.to_str().expect("utf-8 path"))
        .env_remove("KAKEHASHI_EXPERIMENTAL")
        .build();

    let root_uri = format!("file://{}", workspace.path().display());
    client.send_request(
        "initialize",
        json!({
            "processId": std::process::id(),
            "rootUri": root_uri,
            "capabilities": resolve_support_caps(),
            "workspaceFolders": json!([{ "uri": root_uri, "name": "test" }]),
            "initializationOptions": { "languageServers": {
                "ruff": { "cmd": ["ruff", "server"], "languages": ["python"] }
            }}
        }),
    );
    client.send_notification("initialized", json!({}));
    (client, workspace, config_dir)
}

/// A `file://` URI for `name` under the workspace root, so ruff sees the
/// document inside its workspace (config discovery, out-of-workspace policy).
fn doc_uri(workspace: &tempfile::TempDir, name: &str) -> String {
    format!("file://{}/{name}", workspace.path().display())
}

fn open(client: &mut LspClient, uri: &str, text: &str) {
    client.send_notification(
        "textDocument/didOpen",
        json!({ "textDocument": {
            "uri": uri, "languageId": "markdown", "version": 1, "text": text
        }}),
    );
    std::thread::sleep(Duration::from_millis(2000));
}

/// Apply LSP `TextEdit`s (whole-document `changes` entry) to `text`. The test
/// fixtures are ASCII, so a UTF-16 offset equals a byte offset; edits are
/// applied end-first so earlier splices don't shift later ranges.
fn apply_edits(text: &str, edits: &[Value]) -> String {
    let offset = |line: u64, character: u64| -> usize {
        let mut off = 0usize;
        for (i, l) in text.split_inclusive('\n').enumerate() {
            if i as u64 == line {
                // Don't clamp: an out-of-line character means the bridge
                // produced an invalid host range (the very thing under test), so
                // fail loudly instead of normalizing it into a plausible edit.
                let content = l.strip_suffix('\n').unwrap_or(l);
                assert!(
                    character as usize <= content.len(),
                    "edit character {character} past end of host line {line} ({:?})",
                    content
                );
                return off + character as usize;
            }
            off += l.len();
        }
        panic!("edit line {line} past end of host document");
    };
    let mut spans: Vec<(usize, usize, String)> = edits
        .iter()
        .map(|e| {
            let s = offset(
                e["range"]["start"]["line"].as_u64().unwrap(),
                e["range"]["start"]["character"].as_u64().unwrap(),
            );
            let en = offset(
                e["range"]["end"]["line"].as_u64().unwrap(),
                e["range"]["end"]["character"].as_u64().unwrap(),
            );
            (s, en, e["newText"].as_str().unwrap_or("").to_string())
        })
        .collect();
    spans.sort_by_key(|(s, _, _)| *s);
    let mut out = text.to_string();
    for (s, en, new) in spans.into_iter().rev() {
        out.replace_range(s..en.max(s), &new);
    }
    out
}

/// Request codeAction over the python fence (host lines 3-6), retrying while the
/// downstream is still cold.
fn code_action_over_block(client: &mut LspClient, uri: &str, context: Value) -> Vec<Value> {
    for _ in 0..400 {
        let response = client.send_request(
            "textDocument/codeAction",
            json!({
                "textDocument": { "uri": uri },
                "range": {
                    "start": { "line": 3, "character": 0 },
                    "end": { "line": 6, "character": 0 }
                },
                "context": context
            }),
        );
        if let Some(actions) = response["result"].as_array()
            && !actions.is_empty()
        {
            return actions.clone();
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("codeAction never returned actions");
}

/// Resolve `action` if its edit is lazy (ruff advertises resolveProvider),
/// returning the edits targeting `host_uri` from EITHER `WorkspaceEdit`
/// representation — `changes` or `documentChanges` — so the test isn't coupled
/// to ruff's current shape (the bridge translates both).
fn resolved_host_edits(client: &mut LspClient, action: &Value, host_uri: &str) -> Vec<Value> {
    let resolved = if action["edit"].is_object() {
        action.clone()
    } else {
        client.send_request("codeAction/resolve", action.clone())["result"].clone()
    };
    let edit = &resolved["edit"];
    if let Some(edits) = edit["changes"][host_uri].as_array() {
        return edits.clone();
    }
    if let Some(document_changes) = edit["documentChanges"].as_array() {
        let edits: Vec<Value> = document_changes
            .iter()
            .filter(|dc| dc["textDocument"]["uri"] == json!(host_uri))
            .filter_map(|dc| dc["edits"].as_array())
            .flatten()
            .cloned()
            .collect();
        if !edits.is_empty() {
            return edits;
        }
    }
    panic!(
        "edit re-keyed to the host URI {host_uri} (changes or documentChanges), got: {resolved:#?}"
    )
}

#[test]
fn organize_imports_sorts_the_python_block_in_host_coordinates() {
    if !is_ruff_available() {
        eprintln!("SKIP: ruff is not available on PATH");
        return;
    }
    let (mut client, ws, _cfg) = init_ruff_client();
    let uri = doc_uri(&ws, "organize.md");
    // Out-of-order imports; organize-imports sorts os before sys.
    let text = "# Doc\n\n```python\nimport sys\nimport os\n\nprint(os, sys)\n```\n";
    open(&mut client, &uri, text);

    let actions = code_action_over_block(
        &mut client,
        &uri,
        json!({ "diagnostics": [], "only": ["source.organizeImports"] }),
    );
    let action = actions
        .iter()
        .find(|a| {
            a["kind"]
                .as_str()
                .is_some_and(|k| k.starts_with("source.organizeImports"))
        })
        .unwrap_or_else(|| panic!("an organize-imports action, got: {actions:#?}"));
    // The bridge suffixed the origin server onto the title.
    assert!(
        action["title"]
            .as_str()
            .is_some_and(|t| t.ends_with("— ruff")),
        "the bridged title carries the origin suffix, got: {:?}",
        action["title"]
    );

    let edits = resolved_host_edits(&mut client, action, &uri);
    let applied = apply_edits(text, &edits);
    // Applying the (host-translated) edit sorts the fence imports and leaves the
    // markdown structure — fence markers, heading, the print line — intact.
    assert_eq!(
        applied, "# Doc\n\n```python\nimport os\nimport sys\n\nprint(os, sys)\n```\n",
        "organize-imports must sort the fenced python without disturbing the host markdown"
    );

    let _ = client.send_request("shutdown", json!(null));
    client.send_notification("exit", json!(null));
}

#[test]
fn diagnostic_drives_a_quickfix_on_the_python_block() {
    if !is_ruff_available() {
        eprintln!("SKIP: ruff is not available on PATH");
        return;
    }
    let (mut client, ws, _cfg) = init_ruff_client();
    let uri = doc_uri(&ws, "unused.md");
    // `sys` is imported but unused → ruff F401.
    let text = "# Doc\n\n```python\nimport os\nimport sys\n\nprint(os)\n```\n";
    open(&mut client, &uri, text);

    // ruff's unused-import diagnostic, in HOST coordinates (fence content).
    let mut diags = Vec::new();
    for _ in 0..400 {
        let response = client.send_request(
            "textDocument/diagnostic",
            json!({ "textDocument": { "uri": uri } }),
        );
        if let Some(items) = response["result"]["items"].as_array()
            && !items.is_empty()
        {
            diags = items.clone();
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let f401 = diags
        .iter()
        .find(|d| {
            d["code"] == json!("F401") || d["message"].as_str().is_some_and(|m| m.contains("sys"))
        })
        .unwrap_or_else(|| panic!("an unused-import (F401) diagnostic, got: {diags:#?}"));
    assert!(
        f401["range"]["start"]["line"].as_u64().unwrap() >= 3,
        "diagnostic is host-translated onto the fence content, got: {f401:#?}"
    );

    // Feed that diagnostic back as codeAction context — the bridge must
    // translate its range host→virtual so ruff matches it and offers the fix
    // (checklist §1: diagnostic round-trip identity).
    let actions = code_action_over_block(
        &mut client,
        &uri,
        json!({ "diagnostics": [f401], "only": ["quickfix"] }),
    );
    let fix = actions
        .iter()
        .find(|a| {
            a["kind"]
                .as_str()
                .is_some_and(|k| k.starts_with("quickfix"))
        })
        .unwrap_or_else(|| panic!("a quickfix action, got: {actions:#?}"));

    let edits = resolved_host_edits(&mut client, fix, &uri);
    let applied = apply_edits(text, &edits);
    // The quickfix removes the unused `import sys` and nothing else: `import os`
    // and `print(os)` survive, the fence markers survive, and `sys` is gone.
    assert!(
        !applied.contains("import sys"),
        "the unused import must be removed, got:\n{applied}"
    );
    assert!(
        applied.contains("import os") && applied.contains("print(os)"),
        "the used import and body must remain, got:\n{applied}"
    );
    // Both fence markers survive: the opening ```python and the CLOSING ``` (a
    // bad translation that also ate the closing fence would otherwise slip past
    // an opening-marker-only check).
    assert!(
        applied.contains("```python") && applied.contains("# Doc") && applied.ends_with("```\n"),
        "the host markdown structure (both fence markers) must remain, got:\n{applied}"
    );

    let _ = client.send_request("shutdown", json!(null));
    client.send_notification("exit", json!(null));
}
