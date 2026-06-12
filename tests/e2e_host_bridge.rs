//! E2E tests for the host-document bridge (host-document-bridge): with
//! `bridge._self.enabled = true`, requests on the host document itself are
//! forwarded to host-capable servers with the **real client URI** and the
//! response returned **verbatim** (no coordinate translation).
//!
//! The `mock-lsp-formatter` binary's `definition` mode answers definition
//! with a Location echoing the requested URI — but only for documents it
//! received via `didOpen` — so a successful response proves three things at
//! once: the host document was synced, the request carried the real URI, and
//! the response came back untranslated.

#![cfg(feature = "e2e")]

mod helpers;

use helpers::lsp_client::LspClient;
use serde_json::{Value, json};

fn mock_bin() -> &'static str {
    env!("CARGO_BIN_EXE_mock-lsp-formatter")
}

/// Markdown host document. The definition request targets the prose link on
/// LSP line 2 — outside any injection, so only the host layer can answer.
const MARKDOWN: &str = "# Title\n\nSee [reference].\n\n[reference]: https://example.com\n";
const MARKDOWN_URI: &str = "file:///test_host_bridge.md";

fn init_client(config_toml: &str) -> (LspClient, tempfile::TempDir) {
    let config_dir = tempfile::TempDir::new().expect("Failed to create config temp dir");
    let config_path = config_dir.path().join("host_bridge.toml");
    std::fs::write(&config_path, config_toml).expect("Failed to write config");

    let mut client = LspClient::builder()
        .arg("--config-file")
        .arg(config_path.to_str().expect("temp path should be UTF-8"))
        .build();

    let _init_response = client.send_request(
        "initialize",
        json!({
            "processId": std::process::id(),
            "rootUri": null,
            "capabilities": {},
            "workspaceFolders": null,
            "initializationOptions": {
                "languageServers": {
                    "mock-host": {
                        "cmd": [mock_bin(), "definition"],
                        "languages": ["markdown"]
                    }
                }
            }
        }),
    );
    client.send_notification("initialized", json!({}));

    client.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": MARKDOWN_URI,
                "languageId": "markdown",
                "version": 1,
                "text": MARKDOWN
            }
        }),
    );
    (client, config_dir)
}

fn send_definition(client: &mut LspClient) -> Value {
    let response = client.send_request(
        "textDocument/definition",
        json!({
            "textDocument": { "uri": MARKDOWN_URI },
            "position": { "line": 2, "character": 6 },
        }),
    );
    assert!(
        response.get("error").is_none(),
        "definition must not surface a top-level error; got: {:?}",
        response.get("error")
    );
    response["result"].clone()
}

fn shutdown(client: &mut LspClient) {
    let _ = client.send_request("shutdown", json!(null));
    client.send_notification("exit", json!(null));
}

#[test]
fn e2e_host_bridge_definition_uses_real_uri_verbatim() {
    let (mut client, _config_dir) = init_client(
        r#"
[languages.markdown.bridge._self]
enabled = true
"#,
    );

    // Retry while the downstream server warms up.
    let mut hit = None;
    for _ in 0..300 {
        let result = send_definition(&mut client);
        if !result.is_null() {
            hit = Some(result);
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let result = hit.expect("host bridge definition must produce a result");

    // The mock echoes the URI it was asked about: the request must have
    // carried the REAL host URI (not a kakehashi-virtual-uri), and the
    // response must come back verbatim — same URI, untranslated range.
    let entry = result
        .as_array()
        .and_then(|a| a.first())
        .unwrap_or(&result)
        .clone();
    let uri = entry["uri"]
        .as_str()
        .or_else(|| entry["targetUri"].as_str())
        .expect("definition entry must carry a uri");
    assert_eq!(
        uri, MARKDOWN_URI,
        "host bridge must forward the real client URI and pass the response through"
    );
    let line = entry
        .pointer("/range/start/line")
        .or_else(|| entry.pointer("/targetRange/start/line"))
        .and_then(Value::as_u64)
        .expect("definition entry must carry a range");
    assert_eq!(line, 1, "host ranges must NOT be offset-translated");

    shutdown(&mut client);
}

#[test]
fn e2e_host_bridge_is_opt_in() {
    // Without bridge._self.enabled = true, a host-capable server alone does
    // nothing (host-document-bridge: capability declaration is not consent).
    // Warm-then-flip in reverse: prove the gate by enabling at runtime —
    // null while disabled, results after the flip.
    let (mut client, _config_dir) = init_client("");

    // While disabled, the request must stay null. A short stabilization loop
    // (rather than a single probe) guards against a slow first response.
    for _ in 0..10 {
        let result = send_definition(&mut client);
        assert!(
            result.is_null(),
            "host bridging is opt-in: no _self.enabled, no result; got {result}"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    client.send_notification(
        "workspace/didChangeConfiguration",
        json!({
            "settings": {
                "languages": {
                    "markdown": { "bridge": { "_self": { "enabled": true } } }
                }
            }
        }),
    );

    let mut enabled_result = None;
    for _ in 0..300 {
        let result = send_definition(&mut client);
        if !result.is_null() {
            enabled_result = Some(result);
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(
        enabled_result.is_some(),
        "after opting in via didChangeConfiguration, the host bridge must respond"
    );

    shutdown(&mut client);
}

#[test]
fn e2e_host_bridge_respects_layers_order() {
    // Omitting "host" from layers.order must gate the host layer off even
    // though _self is enabled.
    let (mut client, _config_dir) = init_client(
        r#"
[languages.markdown.bridge._self]
enabled = true
"#,
    );

    // Warm up: host layer answers.
    let mut warmed = false;
    for _ in 0..300 {
        if !send_definition(&mut client).is_null() {
            warmed = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(
        warmed,
        "precondition: host bridge must answer before the flip"
    );

    client.send_notification(
        "workspace/didChangeConfiguration",
        json!({
            "settings": {
                "languages": {
                    "markdown": {
                        "layers": {
                            "aggregation": {
                                "textDocument/definition": { "order": ["virt", "native"] }
                            }
                        }
                    }
                }
            }
        }),
    );

    let mut went_null = false;
    for _ in 0..300 {
        if send_definition(&mut client).is_null() {
            went_null = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(
        went_null,
        "layers.order without 'host' must gate the host layer off"
    );

    shutdown(&mut client);
}

// ==========================================================================
// Host formatting (host-document-bridge + cross-layer-aggregation phase 3)
// ==========================================================================

fn send_formatting(client: &mut LspClient, uri: &str) -> Value {
    let response = client.send_request(
        "textDocument/formatting",
        json!({
            "textDocument": { "uri": uri },
            "options": { "tabSize": 4, "insertSpaces": true },
        }),
    );
    assert!(
        response.get("error").is_none(),
        "formatting must not surface a top-level error; got: {:?}",
        response.get("error")
    );
    response["result"].clone()
}

/// Host-only formatting under the default `preferred` layer order: no virt
/// server is configured, so the host layer's whole-document edits win and
/// pass through verbatim.
#[test]
fn e2e_host_formatting_preferred_falls_through_to_host() {
    let config_dir = tempfile::TempDir::new().expect("config dir");
    let config_path = config_dir.path().join("host_fmt.toml");
    std::fs::write(
        &config_path,
        r#"
[languages.markdown.bridge._self]
enabled = true
"#,
    )
    .expect("write config");

    let mut client = LspClient::builder()
        .arg("--config-file")
        .arg(config_path.to_str().unwrap())
        .build();
    let _init = client.send_request(
        "initialize",
        json!({
            "processId": std::process::id(),
            "rootUri": null,
            "capabilities": {},
            "workspaceFolders": null,
            "initializationOptions": {
                "languageServers": {
                    "mock-host-upper": { "cmd": [mock_bin(), "upper"], "languages": ["markdown"] },
                }
            }
        }),
    );
    client.send_notification("initialized", json!({}));

    let uri = "file:///test_host_fmt_preferred.md";
    client.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": { "uri": uri, "languageId": "markdown", "version": 1,
                              "text": "# title\n\nbody text\n" }
        }),
    );

    let mut hit = None;
    for _ in 0..300 {
        let result = send_formatting(&mut client, uri);
        if result.as_array().is_some_and(|a| !a.is_empty()) {
            hit = Some(result);
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let result = hit.expect("host formatting must produce edits");
    let new_text = result[0]["newText"].as_str().expect("edit newText");
    assert!(
        new_text.contains("# TITLE"),
        "host formatter's whole-document edit must pass through verbatim; got: {new_text:?}"
    );

    shutdown(&mut client);
}

/// Cross-layer `concatenated` formatting: virt formats the lua fence first
/// (appending a lowercase marker), then the host formatter runs ON THE VIRT
/// OUTPUT (uppercasing everything). The marker arriving uppercased proves the
/// serial virt → host threading; the response collapses into one
/// whole-document replacement edit.
#[test]
fn e2e_host_formatting_concatenated_threads_virt_then_host() {
    let config_dir = tempfile::TempDir::new().expect("config dir");
    let config_path = config_dir.path().join("host_fmt_concat.toml");
    std::fs::write(
        &config_path,
        r#"
[languages.markdown.bridge._self]
enabled = true

[languages.markdown.layers.aggregation."textDocument/formatting"]
strategy = "concatenated"
"#,
    )
    .expect("write config");

    let mut client = LspClient::builder()
        .arg("--config-file")
        .arg(config_path.to_str().unwrap())
        .build();
    let _init = client.send_request(
        "initialize",
        json!({
            "processId": std::process::id(),
            "rootUri": null,
            "capabilities": {},
            "workspaceFolders": null,
            "initializationOptions": {
                "languageServers": {
                    "mock-host-upper": { "cmd": [mock_bin(), "upper"], "languages": ["markdown"] },
                    "mock-virt-append": { "cmd": [mock_bin(), "append"], "languages": ["lua"] },
                }
            }
        }),
    );
    client.send_notification("initialized", json!({}));

    let uri = "file:///test_host_fmt_concat.md";
    client.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": { "uri": uri, "languageId": "markdown", "version": 1,
                              "text": "# title\n\n```lua\nprint(1)\n```\n" }
        }),
    );

    // Retry until BOTH layers have produced: the uppercased marker can only
    // exist if the host formatter ran on the virt layer's output.
    let mut final_text = None;
    for _ in 0..300 {
        let result = send_formatting(&mut client, uri);
        if let Some(text) = result
            .as_array()
            .and_then(|a| a.first())
            .and_then(|e| e["newText"].as_str())
            && text.contains("MOCK-MARKER")
        {
            final_text = Some(text.to_string());
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let text = final_text
        .expect("concatenated cross-layer formatting must thread virt output into the host layer");

    assert!(
        text.contains("# TITLE"),
        "host layer must have formatted the whole document; got: {text:?}"
    );
    assert!(
        !text.contains("mock-marker"),
        "the marker must be uppercased — host ran AFTER virt, on virt's output; got: {text:?}"
    );

    shutdown(&mut client);
}

/// The generic raw-forward path serves the other methods too: hover on the
/// host document round-trips verbatim (the mock echoes the requested URI in
/// its hover contents — a virtual URI would betray a translation).
#[test]
fn e2e_host_bridge_hover_round_trips_verbatim() {
    let (mut client, _config_dir) = init_client(
        r#"
[languages.markdown.bridge._self]
enabled = true
"#,
    );

    let mut hover_contents = None;
    for _ in 0..300 {
        let response = client.send_request(
            "textDocument/hover",
            json!({
                "textDocument": { "uri": MARKDOWN_URI },
                "position": { "line": 2, "character": 6 },
            }),
        );
        assert!(response.get("error").is_none(), "hover must not error");
        if let Some(contents) = response.pointer("/result/contents").and_then(Value::as_str) {
            hover_contents = Some(contents.to_string());
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let contents = hover_contents.expect("host hover must produce a result");
    assert_eq!(
        contents,
        format!("mock-hover:{MARKDOWN_URI}"),
        "hover must carry the real URI to the server and return verbatim"
    );

    shutdown(&mut client);
}
