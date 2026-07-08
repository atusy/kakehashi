//! Acceptance E2E for #568 PR 8: organize-imports on a python block inside
//! markdown, bridged to a real `ruff server`. Composes the whole codeAction
//! stack — request over the injection region, `source.organizeImports` matched,
//! `codeAction/resolve` round-trip, and the resolved edit re-keyed to the host
//! markdown document with host-translated ranges.
//!
//! Skips when `ruff` is not on PATH.

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

/// Markdown host: a python fence whose imports are out of order. Fence content
/// starts on host line 3 (`import sys`).
const MARKDOWN: &str = "# Doc\n\n```python\nimport sys\nimport os\n\nprint(os, sys)\n```\n";
const MARKDOWN_URI: &str = "file:///test_organize.md";

fn resolve_support_caps() -> Value {
    json!({ "textDocument": { "codeAction": {
        "codeActionLiteralSupport": { "codeActionKind": { "valueSet": [] } },
        "disabledSupport": true,
        "dataSupport": true,
        "resolveSupport": { "properties": ["edit"] },
        "isPreferredSupport": true
    }}})
}

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

fn open_markdown(client: &mut LspClient) {
    client.send_notification(
        "textDocument/didOpen",
        json!({ "textDocument": {
            "uri": MARKDOWN_URI, "languageId": "markdown", "version": 1, "text": MARKDOWN
        }}),
    );
    std::thread::sleep(Duration::from_millis(2000));
}

/// Request codeAction over the python fence (host lines 3-6), retrying while the
/// downstream is still cold.
fn code_action_over_block(client: &mut LspClient, only: Value) -> Vec<Value> {
    for _ in 0..400 {
        let response = client.send_request(
            "textDocument/codeAction",
            json!({
                "textDocument": { "uri": MARKDOWN_URI },
                "range": {
                    "start": { "line": 3, "character": 0 },
                    "end": { "line": 6, "character": 0 }
                },
                "context": { "diagnostics": [], "only": only }
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

#[test]
fn organize_imports_sorts_the_python_block_in_host_coordinates() {
    if !is_ruff_available() {
        eprintln!("SKIP: ruff is not available on PATH");
        return;
    }
    let (mut client, _ws, _cfg) = init_ruff_client();
    open_markdown(&mut client);

    let actions = code_action_over_block(&mut client, json!(["source.organizeImports"]));

    // Find the organize-imports action; the bridge suffixed its title "— ruff".
    let action = actions
        .iter()
        .find(|a| {
            a["kind"]
                .as_str()
                .is_some_and(|k| k.starts_with("source.organizeImports"))
        })
        .unwrap_or_else(|| panic!("an organize-imports action, got: {actions:#?}"));
    assert!(
        action["title"]
            .as_str()
            .is_some_and(|t| t.ends_with("— ruff")),
        "the bridged title carries the origin suffix, got: {:?}",
        action["title"]
    );

    // Resolve if the edit is lazy (ruff advertises resolveProvider).
    let resolved = if action["edit"].is_object() {
        (*action).clone()
    } else {
        client.send_request("codeAction/resolve", (*action).clone())["result"].clone()
    };

    let edits = resolved["edit"]["changes"][MARKDOWN_URI]
        .as_array()
        .unwrap_or_else(|| panic!("edit re-keyed to the host markdown URI, got: {resolved:#?}"));
    assert_eq!(edits.len(), 1, "one import-block replacement: {edits:#?}");
    let edit = &edits[0];
    // ruff sorts `import sys\nimport os` → `import os\nimport sys`; the bridge
    // re-keys the edit to the host doc and translates the virtual range
    // (fence content starts at host line 3), never touching the fence markers.
    assert_eq!(edit["newText"], json!("import os\nimport sys\n"));
    assert_eq!(edit["range"]["start"]["line"], json!(3));
    assert!(
        edit["range"]["end"]["line"].as_u64().unwrap() >= 4,
        "the replacement spans the two-line import block, got: {edit:#?}"
    );

    let _ = client.send_request("shutdown", json!(null));
    client.send_notification("exit", json!(null));
}

/// A python fence with an UNUSED import (`sys`), for the diag→quickfix round
/// trip: ruff diagnoses F401, and its quickfix — matched against that
/// diagnostic — removes the import, re-keyed to the host markdown document.
const MARKDOWN_UNUSED: &str = "# Doc\n\n```python\nimport os\nimport sys\n\nprint(os)\n```\n";
const MARKDOWN_UNUSED_URI: &str = "file:///test_unused.md";

fn pull_diagnostics(client: &mut LspClient) -> Vec<Value> {
    for _ in 0..400 {
        let response = client.send_request(
            "textDocument/diagnostic",
            json!({ "textDocument": { "uri": MARKDOWN_UNUSED_URI } }),
        );
        if let Some(items) = response["result"]["items"].as_array()
            && !items.is_empty()
        {
            return items.clone();
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("textDocument/diagnostic never returned items");
}

#[test]
fn diagnostic_drives_a_quickfix_on_the_python_block() {
    if !is_ruff_available() {
        eprintln!("SKIP: ruff is not available on PATH");
        return;
    }
    let (mut client, _ws, _cfg) = init_ruff_client();
    client.send_notification(
        "textDocument/didOpen",
        json!({ "textDocument": {
            "uri": MARKDOWN_UNUSED_URI, "languageId": "markdown", "version": 1, "text": MARKDOWN_UNUSED
        }}),
    );
    std::thread::sleep(Duration::from_millis(2000));

    // ruff's unused-import diagnostic (F401), in HOST coordinates (line >= 3).
    let diags = pull_diagnostics(&mut client);
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
    let actions = {
        let mut found = None;
        for _ in 0..400 {
            let response = client.send_request(
                "textDocument/codeAction",
                json!({
                    "textDocument": { "uri": MARKDOWN_UNUSED_URI },
                    "range": f401["range"],
                    "context": { "diagnostics": [f401], "only": ["quickfix"] }
                }),
            );
            if let Some(a) = response["result"].as_array()
                && !a.is_empty()
            {
                found = Some(a.clone());
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        found.unwrap_or_else(|| panic!("codeAction never offered a quickfix for the F401"))
    };

    // A quickfix that removes the unused import, re-keyed to the host doc.
    let fix = actions
        .iter()
        .find(|a| {
            a["kind"]
                .as_str()
                .is_some_and(|k| k.starts_with("quickfix"))
                && (a["edit"].is_object() || a["data"].is_object())
        })
        .unwrap_or_else(|| panic!("a quickfix action, got: {actions:#?}"));
    let resolved = if fix["edit"].is_object() {
        (*fix).clone()
    } else {
        client.send_request("codeAction/resolve", (*fix).clone())["result"].clone()
    };
    let edits = resolved["edit"]["changes"][MARKDOWN_UNUSED_URI]
        .as_array()
        .unwrap_or_else(|| panic!("quickfix edit re-keyed to the host URI, got: {resolved:#?}"));
    assert!(
        edits
            .iter()
            .all(|e| e["range"]["start"]["line"].as_u64().unwrap() >= 3),
        "every quickfix edit targets the fence content (host line >= 3), got: {edits:#?}"
    );

    let _ = client.send_request("shutdown", json!(null));
    client.send_notification("exit", json!(null));
}
