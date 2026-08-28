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

use crate::helpers::lsp_client::LspClient;
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
fn e2e_whole_document_links_concatenate_virt_and_host_layers() {
    let config_dir = tempfile::TempDir::new().expect("config dir");
    let config_path = config_dir.path().join("whole_doc_links.toml");
    std::fs::write(
        &config_path,
        r#"
[languages.markdown.bridge._self]
enabled = true

[languages.markdown.layers.aggregation."textDocument/documentLink"]
strategy = "concatenated"
priorities = ["virt", "host"]
"#,
    )
    .expect("write config");

    let mut client = LspClient::builder()
        .arg("--config-file")
        .arg(config_path.to_str().expect("temp path should be UTF-8"))
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
                    "mock-host-link": {
                        "cmd": [mock_bin(), "document-link"],
                        "languages": ["markdown"]
                    },
                    "mock-virt-link": {
                        "cmd": [mock_bin(), "document-link"],
                        "languages": ["lua"]
                    }
                }
            }
        }),
    );
    client.send_notification("initialized", json!({}));

    let uri = "file:///test_whole_doc_links.md";
    client.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri,
                "languageId": "markdown",
                "version": 1,
                "text": "# Title\n\n```lua\nprint(1)\n```\n"
            }
        }),
    );

    let links = (0..300)
        .find_map(|_| {
            let response = client.send_request(
                "textDocument/documentLink",
                json!({ "textDocument": { "uri": uri } }),
            );
            assert!(
                response.get("error").is_none(),
                "documentLink must not surface a top-level error; got: {:?}",
                response.get("error")
            );
            let links = response["result"].as_array().cloned().unwrap_or_default();
            if links.len() >= 2 {
                Some(links)
            } else {
                std::thread::sleep(std::time::Duration::from_millis(50));
                None
            }
        })
        .expect("concatenated whole-document links should include virt and host results");

    let lines = links
        .iter()
        .filter(|link| {
            link["tooltip"]
                .as_str()
                .is_some_and(|tooltip| tooltip.starts_with("mock-link:"))
        })
        .filter_map(|link| link.pointer("/range/start/line").and_then(Value::as_u64))
        .collect::<Vec<_>>();

    assert!(
        lines.contains(&0),
        "host-layer documentLink should keep the host range: {links:?}"
    );
    assert!(
        lines.contains(&3),
        "virt-layer documentLink should be translated to the lua code line (print(1)): {links:?}"
    );

    shutdown(&mut client);
}

#[test]
fn e2e_document_colors_concatenate_virt_and_host_layers() {
    let config_dir = tempfile::TempDir::new().expect("config dir");
    let config_path = config_dir.path().join("document_colors.toml");
    std::fs::write(
        &config_path,
        r#"
[languages.markdown.bridge._self]
enabled = true

[languages.markdown.layers.aggregation."textDocument/documentColor"]
strategy = "concatenated"
priorities = ["virt", "host"]

[languages.markdown.layers.aggregation."textDocument/colorPresentation"]
strategy = "concatenated"
priorities = ["virt", "host"]
"#,
    )
    .expect("write config");

    let mut client = LspClient::builder()
        .arg("--config-file")
        .arg(config_path.to_str().expect("temp path should be UTF-8"))
        .env("KAKEHASHI_EXPERIMENTAL", "true")
        .build();
    let init = client.send_request(
        "initialize",
        json!({
            "processId": std::process::id(),
            "rootUri": null,
            "capabilities": {},
            "workspaceFolders": null,
            "initializationOptions": {
                "languageServers": {
                    "mock-host-color": {
                        "cmd": [mock_bin(), "document-color-host"],
                        "languages": ["markdown"]
                    },
                    "mock-virt-color": {
                        "cmd": [mock_bin(), "document-color-virt"],
                        "languages": ["lua"]
                    }
                }
            }
        }),
    );
    assert_eq!(
        init.pointer("/result/capabilities/colorProvider"),
        Some(&json!(true))
    );
    client.send_notification("initialized", json!({}));

    let uri = "file:///test_document_colors.md";
    client.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri,
                "languageId": "markdown",
                "version": 1,
                "text": "# Title\n\n> ```lua\n> red!\n> ```\n"
            }
        }),
    );

    let colors = (0..300)
        .find_map(|_| {
            let response = client.send_request(
                "textDocument/documentColor",
                json!({ "textDocument": { "uri": uri } }),
            );
            assert!(
                response.get("error").is_none(),
                "documentColor must not surface a top-level error; got: {:?}",
                response.get("error")
            );
            let colors = response["result"].as_array().cloned().unwrap_or_default();
            if colors.len() >= 2 {
                Some(colors)
            } else {
                std::thread::sleep(std::time::Duration::from_millis(50));
                None
            }
        })
        .expect("concatenated document colors should include virt and host results");

    assert!(
        colors.iter().any(|color| {
            color["range"]
                == json!({
                    "start": { "line": 0, "character": 0 },
                    "end": { "line": 0, "character": 4 }
                })
        }),
        "host-layer documentColor should keep the host range: {colors:?}"
    );
    assert!(
        colors.iter().any(|color| {
            color["range"]
                == json!({
                    "start": { "line": 3, "character": 2 },
                    "end": { "line": 3, "character": 6 }
                })
        }),
        "virt-layer documentColor should translate the injected lua line and column: {colors:?}"
    );

    let presentation = client.send_request(
        "textDocument/colorPresentation",
        json!({
            "textDocument": { "uri": uri },
            "range": {
                "start": { "line": 0, "character": 0 },
                "end": { "line": 0, "character": 4 }
            },
            "color": { "red": 1.0, "green": 0.0, "blue": 0.0, "alpha": 1.0 }
        }),
    );
    assert_eq!(
        presentation.pointer("/result/0"),
        Some(&json!({
            "label": "host-color",
            "textEdit": {
                "range": {
                    "start": { "line": 0, "character": 0 },
                    "end": { "line": 0, "character": 4 }
                },
                "newText": "#ff0000"
            }
        })),
        "a host document color should present through its host server: {presentation:?}"
    );

    let virtual_presentation = client.send_request(
        "textDocument/colorPresentation",
        json!({
            "textDocument": { "uri": uri },
            "range": {
                "start": { "line": 3, "character": 2 },
                "end": { "line": 3, "character": 6 }
            },
            "color": { "red": 1.0, "green": 0.0, "blue": 0.0, "alpha": 1.0 }
        }),
    );
    assert_eq!(
        virtual_presentation.pointer("/result/0"),
        Some(&json!({
            "label": "virt-color",
            "textEdit": {
                "range": {
                    "start": { "line": 3, "character": 2 },
                    "end": { "line": 3, "character": 6 }
                },
                "newText": "#00ff00"
            }
        })),
        "an injected color should use the virtual server and translate its edit: {virtual_presentation:?}"
    );
    assert_eq!(
        virtual_presentation.pointer("/result/1"),
        Some(&json!({
            "label": "host-color",
            "textEdit": {
                "range": {
                    "start": { "line": 3, "character": 2 },
                    "end": { "line": 3, "character": 6 }
                },
                "newText": "#ff0000"
            }
        })),
        "concatenated colorPresentation should retain the host answer after virt: {virtual_presentation:?}"
    );

    shutdown(&mut client);
}

#[test]
fn e2e_inline_value_routes_host_and_virtual_ranges() {
    let config_dir = tempfile::TempDir::new().expect("config dir");
    let config_path = config_dir.path().join("inline_value.toml");
    std::fs::write(
        &config_path,
        r#"
[languages.markdown.bridge._self]
enabled = true

[languages.markdown.layers.aggregation."textDocument/inlineValue"]
strategy = "preferred"
priorities = ["virt", "host"]
"#,
    )
    .expect("write config");

    let mut client = LspClient::builder()
        .arg("--config-file")
        .arg(config_path.to_str().expect("temp path should be UTF-8"))
        .build();
    let init = client.send_request(
        "initialize",
        json!({
            "processId": std::process::id(),
            "rootUri": null,
            "capabilities": {},
            "workspaceFolders": null,
            "initializationOptions": {
                "languageServers": {
                    "mock-host-inline-value": {
                        "cmd": [mock_bin(), "inline-value-host"],
                        "languages": ["markdown"]
                    },
                    "mock-virt-inline-value": {
                        "cmd": [mock_bin(), "inline-value-virt"],
                        "languages": ["lua"]
                    }
                }
            }
        }),
    );
    assert_eq!(
        init.pointer("/result/capabilities/inlineValueProvider"),
        Some(&json!({ "workDoneProgress": true }))
    );
    client.send_notification("initialized", json!({}));

    let uri = "file:///test_inline_value.md";
    client.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri,
                "languageId": "markdown",
                "version": 1,
                "text": "host\n\n> ```lua\n> code\n> next\n> ```\n"
            }
        }),
    );

    let request = |client: &mut LspClient, range: Value, stopped_location: Value| {
        (0..300)
            .find_map(|_| {
                let response = client.send_request(
                    "textDocument/inlineValue",
                    json!({
                        "textDocument": { "uri": uri },
                        "range": range,
                        "context": {
                            "frameId": 7,
                            "stoppedLocation": stopped_location
                        }
                    }),
                );
                assert!(
                    response.get("error").is_none(),
                    "inlineValue must not surface a top-level error: {response:?}"
                );
                let values = response["result"].as_array().cloned().unwrap_or_default();
                if values.len() == 3 {
                    Some(values)
                } else {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                    None
                }
            })
            .expect("inline values should become available")
    };

    let host_range = json!({
        "start": { "line": 0, "character": 0 },
        "end": { "line": 0, "character": 4 }
    });
    let host_values = request(&mut client, host_range.clone(), host_range.clone());
    assert_eq!(host_values[0]["text"], json!("host:frame=7"));
    assert_eq!(host_values[0]["range"], host_range);

    let injection_range = json!({
        "start": { "line": 3, "character": 2 },
        "end": { "line": 3, "character": 6 }
    });
    let host_fallback = request(&mut client, injection_range.clone(), host_range.clone());
    assert_eq!(host_fallback[0]["text"], json!("host:frame=7"));
    assert_eq!(host_fallback[0]["range"], injection_range);
    assert_eq!(host_fallback[1]["range"], host_range);

    // A viewport normally begins outside the stopped injection. Routing must
    // use stoppedLocation and clamp this visible span to the Lua region.
    let virtual_range = json!({
        "start": { "line": 0, "character": 0 },
        "end": { "line": 3, "character": 6 }
    });
    let stopped_location = json!({
        "start": { "line": 3, "character": 3 },
        "end": { "line": 3, "character": 5 }
    });
    let virtual_values = request(&mut client, virtual_range.clone(), stopped_location.clone());
    assert_eq!(virtual_values[0]["text"], json!("virt:frame=7"));
    assert_eq!(
        virtual_values[0]["range"],
        json!({
            "start": { "line": 3, "character": 2 },
            "end": { "line": 3, "character": 6 }
        })
    );
    assert_eq!(virtual_values[1]["range"], stopped_location);
    assert_eq!(
        virtual_values[2]["range"],
        json!({
            "start": { "line": 3, "character": 3 },
            "end": { "line": 3, "character": 5 }
        })
    );

    // Range endpoints are bounds, not routing positions: normalize an
    // overlong intermediate-line column to that line's end and a past-EOF
    // endpoint to the document end before intersecting the stopped region.
    let defended_values = request(
        &mut client,
        json!({
            "start": { "line": 3, "character": 999 },
            "end": { "line": u32::MAX, "character": u32::MAX }
        }),
        stopped_location,
    );
    assert_eq!(
        defended_values[0]["range"],
        json!({
            "start": { "line": 3, "character": 6 },
            "end": { "line": 5, "character": 0 }
        })
    );

    shutdown(&mut client);
}

#[test]
fn e2e_semantic_tokens_range_routes_host_and_virtual_legends() {
    let config_dir = tempfile::TempDir::new().expect("config dir");
    let config_path = config_dir.path().join("semantic_tokens_range.toml");
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
        .arg(config_path.to_str().expect("temp path should be UTF-8"))
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
                    "mock-host-semantic-range": {
                        "cmd": [mock_bin(), "semantic-tokens-range-host"],
                        "languages": ["markdown"]
                    },
                    "mock-virt-semantic-range": {
                        "cmd": [mock_bin(), "semantic-tokens-range-virt"],
                        "languages": ["lua"]
                    }
                }
            }
        }),
    );
    client.send_notification("initialized", json!({}));
    let uri = "file:///test_semantic_tokens_range.md";
    open_inline_value_document(&mut client, uri, 1, "hostword\n\n> ```lua\n> code\n> ```\n");

    let request = |client: &mut LspClient, range: Value| {
        (0..300)
            .find_map(|_| {
                let response = client.send_request(
                    "textDocument/semanticTokens/range",
                    json!({ "textDocument": { "uri": uri }, "range": range }),
                );
                assert!(response.get("error").is_none(), "{response:?}");
                response
                    .pointer("/result/data")
                    .and_then(Value::as_array)
                    .filter(|data| !data.is_empty())
                    .cloned()
                    .or_else(|| {
                        std::thread::sleep(std::time::Duration::from_millis(50));
                        None
                    })
            })
            .expect("semantic range tokens should become available")
    };

    let host = request(
        &mut client,
        json!({
            "start": { "line": 0, "character": 0 },
            "end": { "line": 0, "character": 8 }
        }),
    );
    assert_eq!(host, json!([0, 0, 8, 17, 8]).as_array().unwrap().clone());

    let virtual_tokens = request(
        &mut client,
        json!({
            "start": { "line": 3, "character": 2 },
            "end": { "line": 3, "character": 6 }
        }),
    );
    assert_eq!(
        virtual_tokens,
        json!([3, 2, 4, 17, 8]).as_array().unwrap().clone()
    );
    shutdown(&mut client);
}

#[test]
fn e2e_semantic_tokens_full_merges_host_and_virtual_layers() {
    let config_dir = tempfile::TempDir::new().expect("config dir");
    let config_path = config_dir.path().join("semantic_tokens_full.toml");
    std::fs::write(
        &config_path,
        r#"
[languages.markdown.layers.aggregation."textDocument/semanticTokens/full"]
priorities = ["virt", "host", "native"]

[languages.markdown.bridge._self]
enabled = true
"#,
    )
    .expect("write config");
    let mut client = LspClient::builder()
        .arg("--config-file")
        .arg(config_path.to_str().expect("temp path should be UTF-8"))
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
                    "mock-host-semantic-full": {
                        "cmd": [mock_bin(), "semantic-tokens-full-host"],
                        "languages": ["markdown"]
                    },
                    "mock-virt-semantic-full": {
                        "cmd": [mock_bin(), "semantic-tokens-full-virt"],
                        "languages": ["lua", "python"]
                    }
                }
            }
        }),
    );
    client.send_notification("initialized", json!({}));
    let uri = "file:///test_semantic_tokens_full.md";
    open_inline_value_document(
        &mut client,
        uri,
        1,
        "hostword\n\n> ```lua\n> code\n> ```\n\n> ```python\n> next\n> ```\n",
    );

    let data = (0..300)
        .find_map(|_| {
            let response = client.send_request(
                "textDocument/semanticTokens/full",
                json!({ "textDocument": { "uri": uri } }),
            );
            assert!(response.get("error").is_none(), "{response:?}");
            response
                .pointer("/result/data")
                .and_then(Value::as_array)
                .filter(|data| data.len() == 45)
                .cloned()
                .or_else(|| {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                    None
                })
        })
        .expect("full semantic tokens from both layers should become available");

    assert_eq!(
        data,
        json!([
            0, 0, 8, 1, 8, 2, 2, 3, 2, 0, 0, 3, 3, 17, 0, 1, 2, 4, 17, 8, 1, 2, 3, 2, 0, 2, 2, 3,
            2, 0, 0, 3, 6, 17, 0, 1, 2, 4, 17, 8, 1, 2, 3, 2, 0
        ])
        .as_array()
        .unwrap()
        .clone()
    );
    shutdown(&mut client);
}

#[test]
fn e2e_semantic_tokens_delta_reenters_bridge_after_first_injection() {
    let config_dir = tempfile::TempDir::new().expect("config dir");
    let config_path = config_dir.path().join("semantic_delta_reentry.toml");
    std::fs::write(
        &config_path,
        r#"
[languages.markdown.layers.aggregation."textDocument/semanticTokens/full"]
priorities = ["virt", "native"]
"#,
    )
    .expect("write config");
    let mut client = LspClient::builder()
        .arg("--config-file")
        .arg(config_path.to_str().expect("temp path should be UTF-8"))
        .build();
    client.send_request(
        "initialize",
        json!({
            "processId": std::process::id(),
            "rootUri": null,
            "capabilities": {},
            "workspaceFolders": null,
            "initializationOptions": {
                "languageServers": {
                    "mock-virt-semantic-full": {
                        "cmd": [mock_bin(), "semantic-tokens-full-virt"],
                        "languages": ["lua"]
                    }
                }
            }
        }),
    );
    client.send_notification("initialized", json!({}));
    let uri = "file:///test_semantic_delta_reentry.md";
    open_inline_value_document(&mut client, uri, 1, "# heading\n");

    let (previous_result_id, previous_data) = (0..300)
        .find_map(|_| {
            let response = client.send_request(
                "textDocument/semanticTokens/full",
                json!({ "textDocument": { "uri": uri } }),
            );
            response
                .pointer("/result/resultId")
                .and_then(Value::as_str)
                .map(|id| {
                    (
                        id.to_string(),
                        response
                            .pointer("/result/data")
                            .and_then(Value::as_array)
                            .cloned()
                            .unwrap_or_default(),
                    )
                })
                .or_else(|| {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                    None
                })
        })
        .expect("the injection-free native response should advertise a delta lineage");

    client.send_notification(
        "textDocument/didChange",
        json!({
            "textDocument": { "uri": uri, "version": 2 },
            "contentChanges": [{ "text": "```lua\ncode\n```\n" }]
        }),
    );
    let data = (0..300)
        .find_map(|_| {
            let response = client.send_request(
                "textDocument/semanticTokens/full/delta",
                json!({
                    "textDocument": { "uri": uri },
                    "previousResultId": previous_result_id
                }),
            );
            assert!(
                response.get("error").is_none()
                    || response.pointer("/error/code") == Some(&json!(-32801)),
                "{response:?}"
            );
            let data = response
                .pointer("/result/data")
                .and_then(Value::as_array)
                .cloned()
                .or_else(|| {
                    let edits = response.pointer("/result/edits")?.as_array()?;
                    let mut data = previous_data.clone();
                    for edit in edits {
                        let start = edit.get("start")?.as_u64()? as usize;
                        let delete_count = edit.get("deleteCount")?.as_u64()? as usize;
                        let replacement = edit
                            .get("data")
                            .and_then(Value::as_array)
                            .cloned()
                            .unwrap_or_default();
                        data.splice(start..start + delete_count, replacement);
                    }
                    Some(data)
                });
            data.filter(|data| data.chunks_exact(5).any(|token| token == [1, 0, 4, 17, 8]))
                .or_else(|| {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                    None
                })
        })
        .expect("delta must return a full response containing the new virtual token");
    assert!(data.chunks_exact(5).any(|token| token == [1, 0, 4, 17, 8]));
    shutdown(&mut client);
}

#[test]
fn e2e_semantic_tokens_delta_reentry_keeps_cancellation() {
    let config_dir = tempfile::TempDir::new().expect("config dir");
    let config_path = config_dir.path().join("semantic_delta_reentry_cancel.toml");
    std::fs::write(
        &config_path,
        r#"
[languages.markdown.layers.aggregation."textDocument/semanticTokens/full"]
priorities = ["virt", "native"]
"#,
    )
    .expect("write config");
    let event_dir = tempfile::TempDir::new().expect("event dir");
    let mut client = LspClient::builder()
        .arg("--config-file")
        .arg(config_path.to_str().expect("temp path should be UTF-8"))
        .env("MOCK_LSP_CANCEL_DIR", event_dir.path().to_string_lossy())
        .build();
    client.send_request(
        "initialize",
        json!({
            "processId": std::process::id(),
            "rootUri": null,
            "capabilities": {},
            "workspaceFolders": null,
            "initializationOptions": {
                "languageServers": {
                    "mock-virt-semantic-full": {
                        "cmd": [mock_bin(), "semantic-tokens-full-delayed"],
                        "languages": ["lua"]
                    }
                }
            }
        }),
    );
    client.send_notification("initialized", json!({}));
    let uri = "file:///test_semantic_delta_reentry_cancel.md";
    open_inline_value_document(&mut client, uri, 1, "# heading\n");
    let previous_result_id = (0..300)
        .find_map(|_| {
            let response = client.send_request(
                "textDocument/semanticTokens/full",
                json!({ "textDocument": { "uri": uri } }),
            );
            response
                .pointer("/result/resultId")
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                    None
                })
        })
        .expect("native baseline resultId");
    client.send_notification(
        "textDocument/didChange",
        json!({
            "textDocument": { "uri": uri, "version": 2 },
            "contentChanges": [{ "text": "```lua\ncode\n```\n" }]
        }),
    );

    let request_id = client.send_request_async(
        "textDocument/semanticTokens/full/delta",
        json!({
            "textDocument": { "uri": uri },
            "previousResultId": previous_result_id
        }),
    );
    let marker = event_dir
        .path()
        .join("semantic-tokens-full-delayed.request.json");
    assert!(
        (0..60).any(|_| {
            if marker.exists() {
                true
            } else {
                std::thread::sleep(std::time::Duration::from_millis(100));
                false
            }
        }),
        "delta re-entry should reach the delayed bridge"
    );
    client.send_notification("$/cancelRequest", json!({ "id": request_id }));
    let response = client.receive_response_for_id_public(request_id);
    assert_eq!(
        response.pointer("/error/code").and_then(Value::as_i64),
        Some(-32800),
        "the outer delta subscription must cancel nested full aggregation: {response:?}"
    );

    std::fs::write(
        event_dir
            .path()
            .join("semantic-tokens-full-delayed.release"),
        b"release",
    )
    .expect("release delayed server before shutdown");
    shutdown(&mut client);
}

#[test]
fn e2e_semantic_tokens_full_nested_virtual_layer_overlays_outer_layer() {
    let config_dir = tempfile::TempDir::new().expect("config dir");
    let config_path = config_dir.path().join("semantic_tokens_full_nested.toml");
    std::fs::write(&config_path, "").expect("write config");
    let query_path = config_dir.path().join("nested-injections.scm");
    std::fs::write(
        &query_path,
        r#"
((function_item) @injection.content
  (#set! injection.language "rust")
  (#set! injection.include-children))

((identifier) @injection.content
  (#eq? @injection.content "code")
  (#set! injection.language "lua")
  (#set! injection.include-children))
"#,
    )
    .expect("write nested injection query");
    let mut client = LspClient::builder()
        .arg("--config-file")
        .arg(config_path.to_str().expect("temp path should be UTF-8"))
        .build();
    client.send_request(
        "initialize",
        json!({
            "processId": std::process::id(),
            "rootUri": null,
            "capabilities": {},
            "workspaceFolders": null,
            "initializationOptions": {
                "languageServers": {
                    "mock-nested-semantic-full": {
                        "cmd": [mock_bin(), "semantic-tokens-full-nested"],
                        "languages": ["rust", "lua"]
                    }
                },
                "languages": {
                    "rust": {
                        "queries": [{
                            "path": query_path.to_str().expect("UTF-8 query path"),
                            "kind": "injections"
                        }],
                        "layers": { "aggregation": {
                            "textDocument/semanticTokens/full": {
                                "priorities": ["virt"]
                            }
                        }}
                    }
                }
            }
        }),
    );
    client.send_notification("initialized", json!({}));
    let uri = "file:///test_semantic_tokens_full_nested.rs";
    client.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri,
                "languageId": "rust",
                "version": 1,
                "text": "fn code() {}\n"
            }
        }),
    );

    let data = (0..300)
        .find_map(|_| {
            let response = client.send_request(
                "textDocument/semanticTokens/full",
                json!({ "textDocument": { "uri": uri } }),
            );
            assert!(response.get("error").is_none(), "{response:?}");
            response
                .pointer("/result/data")
                .and_then(Value::as_array)
                .filter(|data| !data.is_empty())
                .cloned()
                .or_else(|| {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                    None
                })
        })
        .expect("nested virtual semantic tokens should become available");

    assert_eq!(
        data,
        json!([0, 3, 4, 17, 0]).as_array().unwrap().clone(),
        "the nested identifier token must replace the outer function token"
    );
    shutdown(&mut client);
}

#[test]
fn e2e_semantic_tokens_full_tries_next_host_after_invalid_transformation() {
    let config_dir = tempfile::TempDir::new().expect("config dir");
    let config_path = config_dir
        .path()
        .join("semantic_tokens_full_preferred.toml");
    std::fs::write(
        &config_path,
        r#"
[languages.markdown.layers.aggregation."textDocument/semanticTokens/full"]
priorities = ["host"]

[languages.markdown.bridge._self]
enabled = true
"#,
    )
    .expect("write config");
    let mut client = LspClient::builder()
        .arg("--config-file")
        .arg(config_path.to_str().expect("temp path should be UTF-8"))
        .build();
    client.send_request(
        "initialize",
        json!({
            "processId": std::process::id(),
            "rootUri": null,
            "capabilities": {},
            "workspaceFolders": null,
            "initializationOptions": {
                "languageServers": {
                    "a-invalid": {
                        "cmd": [mock_bin(), "semantic-tokens-full-host-invalid"],
                        "languages": ["markdown"]
                    },
                    "z-valid": {
                        "cmd": [mock_bin(), "semantic-tokens-full-host"],
                        "languages": ["markdown"]
                    }
                }
            }
        }),
    );
    client.send_notification("initialized", json!({}));
    let uri = "file:///test_semantic_tokens_full_preferred.md";
    open_inline_value_document(&mut client, uri, 1, "hostword\n");

    let data = (0..300)
        .find_map(|_| {
            let response = client.send_request(
                "textDocument/semanticTokens/full",
                json!({ "textDocument": { "uri": uri } }),
            );
            response
                .pointer("/result/data")
                .and_then(Value::as_array)
                .filter(|data| !data.is_empty())
                .cloned()
                .or_else(|| {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                    None
                })
        })
        .expect("the valid fallback host should produce tokens");
    assert_eq!(data, json!([0, 0, 8, 1, 8]).as_array().unwrap().clone());
    shutdown(&mut client);
}

#[test]
fn e2e_semantic_tokens_full_empty_priorities_disable_parserless_document() {
    let config_dir = tempfile::TempDir::new().expect("config dir");
    let config_path = config_dir.path().join("semantic_tokens_full_disabled.toml");
    std::fs::write(
        &config_path,
        r#"
[languages.hostonly.layers.aggregation."textDocument/semanticTokens/full"]
priorities = []

[languages.hostonly.bridge._self]
enabled = true
"#,
    )
    .expect("write config");
    let mut client = LspClient::builder()
        .arg("--config-file")
        .arg(config_path.to_str().expect("temp path should be UTF-8"))
        .build();
    client.send_request(
        "initialize",
        json!({
            "processId": std::process::id(),
            "rootUri": null,
            "capabilities": {},
            "workspaceFolders": null,
            "initializationOptions": {
                "languageServers": {
                    "host": {
                        "cmd": [mock_bin(), "semantic-tokens-full-host"],
                        "languages": ["hostonly"]
                    }
                }
            }
        }),
    );
    client.send_notification("initialized", json!({}));
    let uri = "file:///test_semantic_tokens_full_disabled.hostonly";
    client.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri,
                "languageId": "hostonly",
                "version": 1,
                "text": "hostword\n"
            }
        }),
    );

    let response = client.send_request(
        "textDocument/semanticTokens/full",
        json!({ "textDocument": { "uri": uri } }),
    );
    assert!(response["result"].is_null(), "{response:?}");
    shutdown(&mut client);
}

#[test]
fn e2e_semantic_tokens_full_preserves_result_id_for_incapable_host_server() {
    let config_dir = tempfile::TempDir::new().expect("config dir");
    let config_path = config_dir
        .path()
        .join("semantic_tokens_full_incapable_host.toml");
    std::fs::write(
        &config_path,
        r#"
[languages.markdown.layers.aggregation."textDocument/semanticTokens/full"]
priorities = ["host", "native"]

[languages.markdown.bridge._self]
enabled = true
"#,
    )
    .expect("write config");
    let mut client = LspClient::builder()
        .arg("--config-file")
        .arg(config_path.to_str().expect("temp path should be UTF-8"))
        .build();
    client.send_request(
        "initialize",
        json!({
            "processId": std::process::id(),
            "rootUri": null,
            "capabilities": {},
            "workspaceFolders": null,
            "initializationOptions": {
                "languageServers": {
                    "incapable-host": {
                        "cmd": [mock_bin(), "definition"],
                        "languages": ["markdown"]
                    }
                }
            }
        }),
    );
    client.send_notification("initialized", json!({}));
    let uri = "file:///test_semantic_tokens_full_incapable_host.md";
    open_inline_value_document(&mut client, uri, 1, "# heading\n");

    let result_id = (0..300)
        .find_map(|_| {
            let response = client.send_request(
                "textDocument/semanticTokens/full",
                json!({ "textDocument": { "uri": uri } }),
            );
            assert!(response.get("error").is_none(), "{response:?}");
            response
                .pointer("/result/resultId")
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                    None
                })
        })
        .expect("an incapable host must preserve the native delta lineage");
    assert!(!result_id.is_empty());
    shutdown(&mut client);
}

#[test]
fn e2e_semantic_tokens_full_preserves_result_id_for_incapable_virtual_server() {
    let config_dir = tempfile::TempDir::new().expect("config dir");
    let config_path = config_dir
        .path()
        .join("semantic_tokens_full_incapable_virtual.toml");
    std::fs::write(
        &config_path,
        r#"
[languages.markdown.layers.aggregation."textDocument/semanticTokens/full"]
priorities = ["virt", "native"]
"#,
    )
    .expect("write config");
    let mut client = LspClient::builder()
        .arg("--config-file")
        .arg(config_path.to_str().expect("temp path should be UTF-8"))
        .build();
    client.send_request(
        "initialize",
        json!({
            "processId": std::process::id(),
            "rootUri": null,
            "capabilities": {},
            "workspaceFolders": null,
            "initializationOptions": {
                "languageServers": {
                    "incapable-virtual": {
                        "cmd": [mock_bin(), "definition"],
                        "languages": ["lua"]
                    }
                }
            }
        }),
    );
    client.send_notification("initialized", json!({}));
    let uri = "file:///test_semantic_tokens_full_incapable_virtual.md";
    open_inline_value_document(&mut client, uri, 1, "```lua\ncode\n```\n");

    // Let the parser publish its first snapshot without touching the still-cold
    // virtual server. The first semantic request must discover its missing
    // capability without treating that discovery as a bridge attempt.
    std::thread::sleep(std::time::Duration::from_millis(500));
    let response = client.send_request(
        "textDocument/semanticTokens/full",
        json!({ "textDocument": { "uri": uri } }),
    );
    assert!(response.get("error").is_none(), "{response:?}");
    let result_id = response
        .pointer("/result/resultId")
        .and_then(Value::as_str)
        .expect("an incapable virtual server must preserve the native delta lineage");
    assert!(!result_id.is_empty());
    let delta = client.send_request(
        "textDocument/semanticTokens/full/delta",
        json!({
            "textDocument": { "uri": uri },
            "previousResultId": result_id
        }),
    );
    assert!(delta.get("error").is_none(), "{delta:?}");
    assert!(
        delta.pointer("/result/edits").is_some() && delta.pointer("/result/data").is_none(),
        "a known-incapable virtual server must not force the native lineage back through full aggregation: {delta:?}"
    );
    shutdown(&mut client);
}

#[test]
fn e2e_semantic_tokens_full_drops_a_virtual_response_after_content_changes() {
    let config_dir = tempfile::TempDir::new().expect("config dir");
    let config_path = config_dir.path().join("semantic_tokens_full_stale.toml");
    std::fs::write(
        &config_path,
        r#"
[languages.markdown.layers.aggregation."textDocument/semanticTokens/full"]
priorities = ["virt"]
"#,
    )
    .expect("write config");
    let event_dir = tempfile::TempDir::new().expect("event dir");
    let mut client = LspClient::builder()
        .arg("--config-file")
        .arg(config_path.to_str().expect("temp path should be UTF-8"))
        .env("MOCK_LSP_CANCEL_DIR", event_dir.path().to_string_lossy())
        .env(
            "KAKEHASHI_E2E_INLINE_VALUE_CHANGE_DIR",
            event_dir.path().to_string_lossy(),
        )
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
                    "mock-virt-semantic-full": {
                        "cmd": [mock_bin(), "semantic-tokens-full-delayed"],
                        "languages": ["lua"]
                    }
                }
            }
        }),
    );
    client.send_notification("initialized", json!({}));
    let uri = "file:///test_semantic_tokens_full_stale.md";
    open_inline_value_document(&mut client, uri, 1, "> ```lua\n> old!\n> ```\n");

    let request_id = client.send_request_async(
        "textDocument/semanticTokens/full",
        json!({ "textDocument": { "uri": uri } }),
    );
    let request_marker = event_dir
        .path()
        .join("semantic-tokens-full-delayed.request.json");
    assert!(
        (0..60).any(|_| {
            if request_marker.exists() {
                true
            } else {
                std::thread::sleep(std::time::Duration::from_millis(100));
                false
            }
        }),
        "the delayed full semantic token request should reach the virtual server"
    );

    client.send_notification(
        "textDocument/didChange",
        json!({
            "textDocument": { "uri": uri, "version": 2 },
            "contentChanges": [{ "text": "> ```lua\n> new!\n> ```\n" }]
        }),
    );
    let changed = event_dir.path().join("changed");
    assert!(
        (0..60).any(|_| {
            if changed.exists() {
                true
            } else {
                std::thread::sleep(std::time::Duration::from_millis(100));
                false
            }
        }),
        "Kakehashi should apply didChange before the delayed response is released"
    );
    std::fs::write(
        event_dir
            .path()
            .join("semantic-tokens-full-delayed.release"),
        b"release",
    )
    .expect("release delayed full semantic token response");
    let response = client.receive_response_for_id_public(request_id);
    assert_eq!(
        response["result"],
        Value::Null,
        "full semantic tokens authored against the old content must be dropped: {response:?}"
    );
    shutdown(&mut client);
}

#[test]
fn e2e_semantic_tokens_full_skips_non_contiguous_combined_injections() {
    let config_dir = tempfile::TempDir::new().expect("config dir");
    let config_path = config_dir.path().join("semantic_tokens_full_combined.toml");
    std::fs::write(&config_path, "").expect("write config");
    let query_path = config_dir.path().join("combined-injections.scm");
    std::fs::write(
        &query_path,
        r#"
        (fenced_code_block
          (info_string (language) @injection.language)
          (code_fence_content) @injection.content
          (#set! injection.combined)
          (#set! injection.include-children))
        "#,
    )
    .expect("write combined injection query");
    let event_dir = tempfile::TempDir::new().expect("event dir");
    let mut client = LspClient::builder()
        .arg("--config-file")
        .arg(config_path.to_str().expect("temp path should be UTF-8"))
        .env("MOCK_LSP_CANCEL_DIR", event_dir.path().to_string_lossy())
        .build();
    let init = client.send_request(
        "initialize",
        json!({
            "processId": std::process::id(),
            "rootUri": null,
            "capabilities": {},
            "workspaceFolders": null,
            "initializationOptions": {
                "languageServers": {
                    "mock-virt-semantic-full": {
                        "cmd": [mock_bin(), "semantic-tokens-full-marker"],
                        "languages": ["lua"]
                    }
                },
                "languages": {
                    "markdown": {
                        "queries": [{
                            "path": query_path.to_str().expect("UTF-8 query path"),
                            "kind": "injections"
                        }],
                        "layers": { "aggregation": {
                            "textDocument/semanticTokens/full": {
                                "priorities": ["virt", "native"]
                            }
                        }}
                    }
                }
            }
        }),
    );
    assert!(init.get("error").is_none(), "{init:?}");
    client.send_notification("initialized", json!({}));
    let uri = "file:///test_semantic_tokens_full_combined.md";
    open_inline_value_document(
        &mut client,
        uri,
        1,
        "```lua\nfirst\n```\ntext gap\n```lua\nsecond\n```\n",
    );

    let response = client.send_request(
        "textDocument/semanticTokens/full",
        json!({ "textDocument": { "uri": uri } }),
    );
    assert!(response.get("error").is_none(), "{response:?}");
    assert!(
        response.pointer("/result/data").is_some(),
        "the native layer should remain as the safe fallback: {response:?}"
    );
    assert!(
        !event_dir
            .path()
            .join("semantic-tokens-full-marker.request.json")
            .exists(),
        "a non-contiguous combined virtual document must not be dispatched"
    );
    shutdown(&mut client);
}

#[test]
fn e2e_semantic_tokens_full_supersedes_a_request_waiting_on_the_bridge() {
    let config_dir = tempfile::TempDir::new().expect("config dir");
    let config_path = config_dir
        .path()
        .join("semantic_tokens_full_supersede.toml");
    std::fs::write(
        &config_path,
        r#"
[languages.markdown.layers.aggregation."textDocument/semanticTokens/full"]
priorities = ["virt"]
"#,
    )
    .expect("write config");
    let event_dir = tempfile::TempDir::new().expect("event dir");
    let mut client = LspClient::builder()
        .arg("--config-file")
        .arg(config_path.to_str().expect("temp path should be UTF-8"))
        .env("MOCK_LSP_CANCEL_DIR", event_dir.path().to_string_lossy())
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
                    "mock-virt-semantic-full": {
                        "cmd": [mock_bin(), "semantic-tokens-full-delayed"],
                        "languages": ["lua"]
                    }
                }
            }
        }),
    );
    client.send_notification("initialized", json!({}));
    let uri = "file:///test_semantic_tokens_full_supersede.md";
    open_inline_value_document(&mut client, uri, 1, "> ```lua\n> code\n> ```\n");

    let first_id = client.send_request_async(
        "textDocument/semanticTokens/full",
        json!({ "textDocument": { "uri": uri } }),
    );
    let request_marker = event_dir
        .path()
        .join("semantic-tokens-full-delayed.request.json");
    assert!(
        (0..60).any(|_| {
            if request_marker.exists() {
                true
            } else {
                std::thread::sleep(std::time::Duration::from_millis(100));
                false
            }
        }),
        "the first request should wait inside the bridge"
    );
    let second_id = client.send_request_async(
        "textDocument/semanticTokens/full",
        json!({ "textDocument": { "uri": uri } }),
    );

    let first = client.receive_response_for_id_public(first_id);
    assert_eq!(
        first["result"],
        Value::Null,
        "the superseded bridge response must be dropped without waiting for downstream: {first:?}"
    );
    std::fs::write(
        event_dir
            .path()
            .join("semantic-tokens-full-delayed.release"),
        b"release",
    )
    .expect("release delayed full semantic token responses");
    let second = client.receive_response_for_id_public(second_id);
    assert!(
        second.pointer("/result/data").is_some(),
        "the newer request should own the final response: {second:?}"
    );
    shutdown(&mut client);
}

#[test]
fn e2e_semantic_tokens_full_cancels_while_waiting_on_the_bridge() {
    let config_dir = tempfile::TempDir::new().expect("config dir");
    let config_path = config_dir.path().join("semantic_tokens_full_cancel.toml");
    std::fs::write(
        &config_path,
        r#"
[languages.markdown.layers.aggregation."textDocument/semanticTokens/full"]
priorities = ["virt"]
"#,
    )
    .expect("write config");
    let event_dir = tempfile::TempDir::new().expect("event dir");
    let mut client = LspClient::builder()
        .arg("--config-file")
        .arg(config_path.to_str().expect("temp path should be UTF-8"))
        .env("MOCK_LSP_CANCEL_DIR", event_dir.path().to_string_lossy())
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
                    "mock-virt-semantic-full": {
                        "cmd": [mock_bin(), "semantic-tokens-full-delayed"],
                        "languages": ["lua"]
                    }
                }
            }
        }),
    );
    client.send_notification("initialized", json!({}));
    let uri = "file:///test_semantic_tokens_full_cancel.md";
    open_inline_value_document(&mut client, uri, 1, "> ```lua\n> code\n> ```\n");

    let request_id = client.send_request_async(
        "textDocument/semanticTokens/full",
        json!({ "textDocument": { "uri": uri } }),
    );
    let request_marker = event_dir
        .path()
        .join("semantic-tokens-full-delayed.request.json");
    assert!(
        (0..60).any(|_| {
            if request_marker.exists() {
                true
            } else {
                std::thread::sleep(std::time::Duration::from_millis(100));
                false
            }
        }),
        "the request should wait inside the bridge"
    );
    client.send_notification("$/cancelRequest", json!({ "id": request_id }));
    let response = client.receive_response_for_id_public(request_id);
    assert_eq!(
        response.pointer("/error/code").and_then(Value::as_i64),
        Some(-32800),
        "cancellation must cross the native-to-bridge handoff without waiting for downstream: {response:?}"
    );

    std::fs::write(
        event_dir
            .path()
            .join("semantic-tokens-full-delayed.release"),
        b"release",
    )
    .expect("release delayed server before shutdown");
    shutdown(&mut client);
}

#[test]
fn e2e_semantic_tokens_full_delta_tracks_the_merged_bridge_result() {
    let config_dir = tempfile::TempDir::new().expect("config dir");
    let config_path = config_dir.path().join("semantic_tokens_full_delta.toml");
    std::fs::write(
        &config_path,
        r#"
[languages.markdown.layers.aggregation."textDocument/semanticTokens/full"]
priorities = ["virt", "native"]
"#,
    )
    .expect("write config");
    let event_dir = tempfile::TempDir::new().expect("event dir");
    let mut client = LspClient::builder()
        .arg("--config-file")
        .arg(config_path.to_str().expect("temp path should be UTF-8"))
        .env(
            "KAKEHASHI_E2E_INLINE_VALUE_CHANGE_DIR",
            event_dir.path().to_string_lossy(),
        )
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
                    "mock-virt-semantic-full": {
                        "cmd": [mock_bin(), "semantic-tokens-full-changing"],
                        "languages": ["lua"]
                    }
                }
            }
        }),
    );
    client.send_notification("initialized", json!({}));
    let uri = "file:///test_semantic_tokens_full_delta.md";
    open_inline_value_document(&mut client, uri, 1, "> ```lua\n> old!\n> ```\n");

    let initial = (0..300)
        .find_map(|_| {
            let response = client.send_request(
                "textDocument/semanticTokens/full",
                json!({ "textDocument": { "uri": uri } }),
            );
            let result = response.get("result")?;
            (result.get("resultId").and_then(Value::as_str).is_some()
                && result.get("data").and_then(Value::as_array).is_some())
            .then(|| result.clone())
            .or_else(|| {
                std::thread::sleep(std::time::Duration::from_millis(50));
                None
            })
        })
        .expect("bridged full result should establish a wire baseline");
    let previous_result_id = initial["resultId"]
        .as_str()
        .expect("full resultId")
        .to_string();
    let initial_data = initial["data"].as_array().expect("full data").clone();
    assert!(
        initial_data
            .chunks_exact(5)
            .any(|token| token == json!([1, 2, 4, 17, 8]).as_array().unwrap()),
        "the initial full result must contain the rebased virtual-server token: {initial:?}"
    );

    client.send_notification(
        "textDocument/didChange",
        json!({
            "textDocument": { "uri": uri, "version": 2 },
            "contentChanges": [{ "text": "> ```lua\n> changed\n> ```\n" }]
        }),
    );
    assert!(
        (0..60).any(|_| {
            if event_dir.path().join("changed").exists() {
                true
            } else {
                std::thread::sleep(std::time::Duration::from_millis(100));
                false
            }
        }),
        "didChange should be admitted before the delta request"
    );

    let delta_response = client.send_request(
        "textDocument/semanticTokens/full/delta",
        json!({
            "textDocument": { "uri": uri },
            "previousResultId": previous_result_id
        }),
    );
    assert!(delta_response.get("error").is_none(), "{delta_response:?}");
    let edits = delta_response
        .pointer("/result/edits")
        .and_then(Value::as_array)
        .expect("a valid merged baseline should produce delta edits");
    assert_eq!(edits.len(), 1, "the delta algorithm emits one splice");
    assert!(
        delta_response
            .pointer("/result/resultId")
            .and_then(Value::as_str)
            .is_some(),
        "the merged delta should establish its next baseline"
    );
    let edit = &edits[0];
    let start = edit["start"].as_u64().expect("edit start") as usize;
    let delete_count = edit["deleteCount"].as_u64().expect("delete count") as usize;
    let replacement = edit
        .get("data")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut reconstructed = initial_data;
    reconstructed.splice(start..start + delete_count, replacement);

    let current_full = client.send_request(
        "textDocument/semanticTokens/full",
        json!({ "textDocument": { "uri": uri } }),
    );
    let current_data = current_full["result"]["data"]
        .as_array()
        .expect("current full data")
        .clone();
    assert!(
        current_data
            .chunks_exact(5)
            .any(|token| token == json!([1, 2, 7, 17, 8]).as_array().unwrap()),
        "the changed full result must contain the updated rebased virtual-server token: {current_full:?}"
    );
    assert_eq!(
        reconstructed, current_data,
        "applying the delta must reproduce the same host/virt/native token set as full"
    );
    shutdown(&mut client);
}

#[test]
fn e2e_semantic_tokens_full_delta_rejects_stale_commits() {
    let config_dir = tempfile::TempDir::new().expect("config dir");
    let config_path = config_dir
        .path()
        .join("semantic_tokens_full_delta_stale.toml");
    std::fs::write(
        &config_path,
        r#"
[languages.markdown.layers.aggregation."textDocument/semanticTokens/full"]
priorities = ["virt", "native"]
"#,
    )
    .expect("write config");
    let barrier_dir = tempfile::TempDir::new().expect("barrier dir");
    let mut client = LspClient::builder()
        .arg("--config-file")
        .arg(config_path.to_str().expect("temp path should be UTF-8"))
        .env(
            "KAKEHASHI_E2E_SEMANTIC_DELTA_COMMIT_BARRIER_DIR",
            barrier_dir.path().to_string_lossy(),
        )
        .build();
    client.send_request(
        "initialize",
        json!({
            "processId": std::process::id(),
            "rootUri": null,
            "capabilities": {},
            "workspaceFolders": null,
            "initializationOptions": {
                "languageServers": {
                    "mock-virt-semantic-full": {
                        "cmd": [mock_bin(), "semantic-tokens-full-changing"],
                        "languages": ["lua"]
                    }
                }
            }
        }),
    );
    client.send_notification("initialized", json!({}));
    let uri = "file:///test_semantic_tokens_full_delta_stale.md";
    open_inline_value_document(&mut client, uri, 1, "> ```lua\n> old!\n> ```\n");

    let previous_result_id = (0..300)
        .find_map(|_| {
            let response = client.send_request(
                "textDocument/semanticTokens/full",
                json!({ "textDocument": { "uri": uri } }),
            );
            response
                .pointer("/result/resultId")
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                    None
                })
        })
        .expect("bridged full result should establish a wire baseline");
    let delta_id = client.send_request_async(
        "textDocument/semanticTokens/full/delta",
        json!({
            "textDocument": { "uri": uri },
            "previousResultId": previous_result_id
        }),
    );
    let captured = barrier_dir.path().join("captured");
    assert!(
        (0..60).any(|_| {
            if captured.exists() {
                true
            } else {
                std::thread::sleep(std::time::Duration::from_millis(100));
                false
            }
        }),
        "the delta response should reach its final lifecycle fence"
    );

    client.send_notification(
        "textDocument/didChange",
        json!({
            "textDocument": { "uri": uri, "version": 2 },
            "contentChanges": [{ "text": "> ```lua\n> changed\n> ```\n" }]
        }),
    );
    let ingress_barrier = client.send_request(
        "textDocument/hover",
        json!({
            "textDocument": { "uri": uri },
            "position": { "line": 1, "character": 2 }
        }),
    );
    assert!(
        ingress_barrier.get("error").is_none(),
        "{ingress_barrier:?}"
    );
    std::fs::write(barrier_dir.path().join("release"), b"release")
        .expect("release delta commit barrier");

    let response = client.receive_response_for_id_public(delta_id);
    assert_eq!(
        response["result"],
        Value::Null,
        "a delta computed before didChange must not commit afterwards: {response:?}"
    );

    shutdown(&mut client);
}

#[test]
fn e2e_semantic_tokens_range_drops_a_virtual_response_after_content_changes() {
    let config_dir = tempfile::TempDir::new().expect("config dir");
    let config_path = config_dir.path().join("semantic_tokens_range.toml");
    std::fs::write(
        &config_path,
        r#"
[languages.markdown.layers.aggregation."textDocument/semanticTokens/range"]
priorities = ["virt"]
"#,
    )
    .expect("write config");
    let event_dir = tempfile::TempDir::new().expect("event dir");
    let mut client = LspClient::builder()
        .arg("--config-file")
        .arg(config_path.to_str().expect("temp path should be UTF-8"))
        .env("MOCK_LSP_CANCEL_DIR", event_dir.path().to_string_lossy())
        .env(
            "KAKEHASHI_E2E_INLINE_VALUE_CHANGE_DIR",
            event_dir.path().to_string_lossy(),
        )
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
                    "mock-virt-semantic-range": {
                        "cmd": [mock_bin(), "semantic-tokens-range-delayed"],
                        "languages": ["lua"]
                    }
                }
            }
        }),
    );
    client.send_notification("initialized", json!({}));
    let uri = "file:///test_semantic_tokens_range_stale.md";
    open_inline_value_document(&mut client, uri, 1, "> ```lua\n> old!\n> ```\n");

    let request_id = client.send_request_async(
        "textDocument/semanticTokens/range",
        json!({
            "textDocument": { "uri": uri },
            "range": {
                "start": { "line": 1, "character": 2 },
                "end": { "line": 1, "character": 6 }
            }
        }),
    );
    let request_marker = event_dir
        .path()
        .join("semantic-tokens-range-delayed.request.json");
    assert!(
        (0..60).any(|_| {
            if request_marker.exists() {
                true
            } else {
                std::thread::sleep(std::time::Duration::from_millis(100));
                false
            }
        }),
        "the delayed semantic token request should reach the virtual server"
    );

    client.send_notification(
        "textDocument/didChange",
        json!({
            "textDocument": { "uri": uri, "version": 2 },
            "contentChanges": [{ "text": "> ```lua\n> new!\n> ```\n" }]
        }),
    );
    let changed = event_dir.path().join("changed");
    assert!(
        (0..60).any(|_| {
            if changed.exists() {
                true
            } else {
                std::thread::sleep(std::time::Duration::from_millis(100));
                false
            }
        }),
        "Kakehashi should apply didChange before the delayed response is released"
    );
    std::fs::write(
        event_dir
            .path()
            .join("semantic-tokens-range-delayed.release"),
        b"release",
    )
    .expect("release delayed semantic token response");
    let response = client.receive_response_for_id_public(request_id);
    assert_eq!(
        response["result"],
        Value::Null,
        "semantic tokens authored against the old content must be dropped: {response:?}"
    );
    shutdown(&mut client);
}

fn init_inline_value_virt_client(mode: &str, event_dir: &std::path::Path) -> LspClient {
    let mut client = LspClient::builder()
        .env("MOCK_LSP_CANCEL_DIR", event_dir.to_string_lossy())
        .env(
            "KAKEHASHI_E2E_INLINE_VALUE_CHANGE_DIR",
            event_dir.to_string_lossy(),
        )
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
                    "mock-virt-inline-value": {
                        "cmd": [mock_bin(), mode],
                        "languages": ["lua"]
                    }
                }
            }
        }),
    );
    client.send_notification("initialized", json!({}));
    client
}

fn open_inline_value_document(client: &mut LspClient, uri: &str, version: i32, text: &str) {
    client.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri,
                "languageId": "markdown",
                "version": version,
                "text": text
            }
        }),
    );
}

fn inline_value_params(uri: &str) -> Value {
    json!({
        "textDocument": { "uri": uri },
        "range": {
            "start": { "line": 1, "character": 2 },
            "end": { "line": 1, "character": 6 }
        },
        "context": {
            "frameId": 7,
            "stoppedLocation": {
                "start": { "line": 1, "character": 2 },
                "end": { "line": 1, "character": 6 }
            }
        }
    })
}

#[test]
fn e2e_inline_value_drops_a_response_after_content_changes() {
    let event_dir = tempfile::TempDir::new().expect("event dir");
    let mut client = init_inline_value_virt_client("inline-value-delayed", event_dir.path());
    let uri = "file:///test_inline_value_stale.md";
    open_inline_value_document(&mut client, uri, 1, "> ```lua\n> old!\n> ```\n");
    let request_id =
        client.send_request_async("textDocument/inlineValue", inline_value_params(uri));
    let request_marker = event_dir.path().join("inline-value-delayed.request.json");
    assert!(
        (0..60).any(|_| {
            if request_marker.exists() {
                true
            } else {
                std::thread::sleep(std::time::Duration::from_millis(100));
                false
            }
        }),
        "the delayed inlineValue request should reach the virtual server"
    );

    client.send_notification(
        "textDocument/didChange",
        json!({
            "textDocument": { "uri": uri, "version": 2 },
            "contentChanges": [{ "text": "> ```lua\n> new!\n> ```\n" }]
        }),
    );
    let changed = event_dir.path().join("changed");
    assert!(
        (0..60).any(|_| {
            if changed.exists() {
                true
            } else {
                std::thread::sleep(std::time::Duration::from_millis(100));
                false
            }
        }),
        "Kakehashi should apply didChange before the delayed response is released"
    );
    std::fs::write(
        event_dir.path().join("inline-value-delayed.release"),
        b"release",
    )
    .expect("release delayed virtual inlineValue response");
    let response = client.receive_response_for_id_public(request_id);
    assert_eq!(
        response["result"],
        Value::Null,
        "inline values authored against the old content must be dropped: {response:?}"
    );
    shutdown(&mut client);
}

#[test]
fn e2e_inline_value_cancellation_reaches_the_virtual_request() {
    let event_dir = tempfile::TempDir::new().expect("event dir");
    let mut client = init_inline_value_virt_client("inline-value-slow", event_dir.path());
    let uri = "file:///test_inline_value_cancel.md";
    open_inline_value_document(&mut client, uri, 1, "> ```lua\n> code\n> ```\n");
    let request_id =
        client.send_request_async("textDocument/inlineValue", inline_value_params(uri));
    let request_marker = event_dir.path().join("inline-value-slow.request.json");
    assert!(
        (0..60).any(|_| {
            if request_marker.exists() {
                true
            } else {
                std::thread::sleep(std::time::Duration::from_millis(100));
                false
            }
        }),
        "the slow inlineValue request should reach the virtual server"
    );
    let downstream_id = serde_json::from_slice::<Value>(
        &std::fs::read(&request_marker).expect("read downstream request marker"),
    )
    .expect("parse downstream request marker")["id"]
        .clone();

    client.send_notification("$/cancelRequest", json!({ "id": request_id }));
    let response = client.receive_response_for_id_public(request_id);
    assert_eq!(
        response.pointer("/error/code").and_then(Value::as_i64),
        Some(-32800),
        "inlineValue must answer RequestCancelled: {response:?}"
    );
    let cancel_marker = event_dir.path().join("inline-value-slow.cancel.json");
    assert!(
        (0..60).any(|_| {
            if cancel_marker.exists() {
                true
            } else {
                std::thread::sleep(std::time::Duration::from_millis(100));
                false
            }
        }),
        "the virtual server should observe downstream cancellation"
    );
    let cancelled = serde_json::from_slice::<Value>(
        &std::fs::read(cancel_marker).expect("read downstream cancel marker"),
    )
    .expect("parse downstream cancel marker");
    assert_eq!(cancelled["params"]["id"], downstream_id);
    shutdown(&mut client);
}

#[test]
fn e2e_inline_value_rejects_virtual_reopen_during_admission() {
    let config_dir = tempfile::TempDir::new().expect("config dir");
    let config_path = config_dir.path().join("inline_value_reopen.toml");
    std::fs::write(
        &config_path,
        r#"
[languages.markdown.layers.aggregation."textDocument/inlineValue"]
priorities = ["virt"]
"#,
    )
    .expect("write config");
    let barrier_dir = tempfile::TempDir::new().expect("barrier dir");
    let mut client = LspClient::builder()
        .arg("--config-file")
        .arg(config_path.to_str().expect("temp path should be UTF-8"))
        .env(
            "KAKEHASHI_E2E_INLINE_VALUE_BARRIER_DIR",
            barrier_dir.path().to_string_lossy(),
        )
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
                    "mock-virt-inline-value": {
                        "cmd": [mock_bin(), "inline-value-virt"],
                        "languages": ["lua"]
                    }
                }
            }
        }),
    );
    client.send_notification("initialized", json!({}));

    let uri = "file:///test_inline_value_reopen.md";
    let open = |client: &mut LspClient, text: &str| {
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
    };
    open(&mut client, "> ```lua\n> old!\n> ```\n");
    let request_id = client.send_request_async(
        "textDocument/inlineValue",
        json!({
            "textDocument": { "uri": uri },
            "range": {
                "start": { "line": 1, "character": 2 },
                "end": { "line": 1, "character": 6 }
            },
            "context": {
                "frameId": 7,
                "stoppedLocation": {
                    "start": { "line": 1, "character": 2 },
                    "end": { "line": 1, "character": 6 }
                }
            }
        }),
    );
    let captured = barrier_dir.path().join("captured");
    assert!(
        (0..60).any(|_| {
            if captured.exists() {
                true
            } else {
                std::thread::sleep(std::time::Duration::from_millis(100));
                false
            }
        }),
        "the old virtual context should be captured before dispatch admission"
    );

    client.send_notification(
        "textDocument/didClose",
        json!({ "textDocument": { "uri": uri } }),
    );
    open(&mut client, "> ```lua\n> new!\n> ```\n");
    let reopen_barrier = client.send_request(
        "textDocument/semanticTokens/full",
        json!({ "textDocument": { "uri": uri } }),
    );
    assert!(reopen_barrier.get("error").is_none());
    std::fs::write(barrier_dir.path().join("release"), b"release")
        .expect("release inline value admission");

    let response = client.receive_response_for_id_public(request_id);
    assert_eq!(
        response["result"],
        Value::Null,
        "a request carrying the closed incarnation must not answer from the reopened document: {response:?}"
    );
    shutdown(&mut client);
}

#[test]
fn e2e_inline_value_rejects_host_edit_during_admission() {
    let config_dir = tempfile::TempDir::new().expect("config dir");
    let config_path = config_dir.path().join("inline_value_host_admission.toml");
    std::fs::write(
        &config_path,
        r#"
[languages.markdown.bridge._self]
enabled = true

[languages.markdown.layers.aggregation."textDocument/inlineValue"]
priorities = ["host"]
"#,
    )
    .expect("write config");
    let barrier_dir = tempfile::TempDir::new().expect("barrier dir");
    let mut client = LspClient::builder()
        .arg("--config-file")
        .arg(config_path.to_str().expect("temp path should be UTF-8"))
        .env(
            "KAKEHASHI_E2E_INLINE_VALUE_BARRIER_DIR",
            barrier_dir.path().to_string_lossy(),
        )
        .env(
            "KAKEHASHI_E2E_INLINE_VALUE_CHANGE_DIR",
            barrier_dir.path().to_string_lossy(),
        )
        .env("MOCK_LSP_CANCEL_DIR", barrier_dir.path().to_string_lossy())
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
                    "mock-host-inline-value": {
                        "cmd": [mock_bin(), "inline-value-record-host"],
                        "languages": ["markdown"]
                    }
                }
            }
        }),
    );
    client.send_notification("initialized", json!({}));

    let uri = "file:///test_inline_value_host_admission.md";
    open_inline_value_document(&mut client, uri, 1, "old line\ncode\n");
    let open_barrier = client.send_request(
        "textDocument/semanticTokens/full",
        json!({ "textDocument": { "uri": uri } }),
    );
    assert!(open_barrier.get("error").is_none());
    let request_id =
        client.send_request_async("textDocument/inlineValue", inline_value_params(uri));
    let captured = barrier_dir.path().join("captured");
    assert!(
        (0..60).any(|_| {
            if captured.exists() {
                true
            } else {
                std::thread::sleep(std::time::Duration::from_millis(100));
                false
            }
        }),
        "the old host context should be captured before dispatch admission"
    );

    client.send_notification(
        "textDocument/didChange",
        json!({
            "textDocument": { "uri": uri, "version": 2 },
            "contentChanges": [{ "text": "new line\ncode\n" }]
        }),
    );
    let changed = barrier_dir.path().join("changed");
    assert!(
        (0..60).any(|_| {
            if changed.exists() {
                true
            } else {
                std::thread::sleep(std::time::Duration::from_millis(100));
                false
            }
        }),
        "Kakehashi should apply didChange while host dispatch admission is parked"
    );
    std::fs::write(barrier_dir.path().join("release"), b"release")
        .expect("release host inlineValue admission");

    let response = client.receive_response_for_id_public(request_id);
    assert_eq!(response["result"], Value::Null);
    assert!(
        !barrier_dir
            .path()
            .join("inline-value-record-host.request.json")
            .exists(),
        "a stale host snapshot must be rejected before downstream dispatch"
    );
    shutdown(&mut client);
}

#[test]
fn e2e_color_presentation_cancel_reaches_host_after_empty_virt_arm() {
    let config_dir = tempfile::TempDir::new().expect("config dir");
    let config_path = config_dir.path().join("color_presentation_cancel.toml");
    std::fs::write(
        &config_path,
        r#"
[languages.markdown.bridge._self]
enabled = true

[languages.markdown.layers.aggregation."textDocument/colorPresentation"]
strategy = "preferred"
priorities = ["virt", "host"]

[languages.markdown.layers.aggregation."textDocument/documentColor"]
strategy = "concatenated"
priorities = ["virt", "host"]
"#,
    )
    .expect("write config");

    let event_dir = tempfile::TempDir::new().expect("event dir");
    let mut client = LspClient::builder()
        .arg("--config-file")
        .arg(config_path.to_str().expect("temp path should be UTF-8"))
        .env("KAKEHASHI_EXPERIMENTAL", "true")
        .env("MOCK_LSP_CANCEL_DIR", event_dir.path().to_string_lossy())
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
                    "mock-host-color": {
                        "cmd": [mock_bin(), "document-color-slow-presentation"],
                        "languages": ["markdown"]
                    },
                    "mock-virt-color": {
                        "cmd": [mock_bin(), "document-color-empty-presentation"],
                        "languages": ["lua"]
                    }
                }
            }
        }),
    );
    client.send_notification("initialized", json!({}));

    let uri = "file:///test_color_presentation_cancel.md";
    client.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri,
                "languageId": "markdown",
                "version": 1,
                "text": "> ```lua\n> red!\n> ```\n"
            }
        }),
    );

    (0..100)
        .find_map(|_| {
            let response = client.send_request(
                "textDocument/documentColor",
                json!({ "textDocument": { "uri": uri } }),
            );
            if response["result"]
                .as_array()
                .is_some_and(|items| items.len() >= 2)
            {
                Some(())
            } else {
                std::thread::sleep(std::time::Duration::from_millis(50));
                None
            }
        })
        .expect("host and virtual color servers should warm up");

    let request_id = client.send_request_async(
        "textDocument/colorPresentation",
        json!({
            "textDocument": { "uri": uri },
            "range": {
                "start": { "line": 1, "character": 2 },
                "end": { "line": 1, "character": 6 }
            },
            "color": { "red": 1.0, "green": 0.0, "blue": 0.0, "alpha": 1.0 }
        }),
    );
    let host_request = event_dir
        .path()
        .join("document-color-slow-presentation.request.json");
    assert!(
        (0..60).any(|_| {
            if host_request.exists() {
                true
            } else {
                std::thread::sleep(std::time::Duration::from_millis(100));
                false
            }
        }),
        "host colorPresentation request should start"
    );

    client.send_notification("$/cancelRequest", json!({ "id": request_id }));
    let response = client.receive_response_for_id_public(request_id);
    assert_eq!(
        response.pointer("/error/code").and_then(Value::as_i64),
        Some(-32800),
        "colorPresentation should answer RequestCancelled: {response:?}"
    );

    let host_cancel = event_dir
        .path()
        .join("document-color-slow-presentation.cancel.json");
    assert!(
        (0..60).any(|_| {
            if host_cancel.exists() {
                true
            } else {
                std::thread::sleep(std::time::Duration::from_millis(100));
                false
            }
        }),
        "cancellation should reach the host server after the virt arm settles"
    );

    shutdown(&mut client);
}

#[test]
fn e2e_document_color_rejects_host_reopen_during_connection_admission() {
    let config_dir = tempfile::TempDir::new().expect("config dir");
    let config_path = config_dir.path().join("document_color_reopen.toml");
    std::fs::write(
        &config_path,
        r#"
[languages.markdown.bridge._self]
enabled = true

[languages.markdown.layers.aggregation."textDocument/documentColor"]
priorities = ["host"]
"#,
    )
    .expect("write config");

    let barrier_dir = tempfile::TempDir::new().expect("barrier dir");
    let mut client = LspClient::builder()
        .arg("--config-file")
        .arg(config_path.to_str().expect("temp path should be UTF-8"))
        .env("KAKEHASHI_EXPERIMENTAL", "true")
        .env(
            "KAKEHASHI_E2E_WHOLE_DOCUMENT_HOST_BARRIER_DIR",
            barrier_dir.path().to_string_lossy(),
        )
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
                    "mock-host-color": {
                        "cmd": [mock_bin(), "document-color-host"],
                        "languages": ["markdown"]
                    }
                }
            }
        }),
    );
    client.send_notification("initialized", json!({}));

    let uri = "file:///test_document_color_reopen.md";
    client.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri,
                "languageId": "markdown",
                "version": 1,
                "text": "old lifetime"
            }
        }),
    );
    let request_id = client.send_request_async(
        "textDocument/documentColor",
        json!({ "textDocument": { "uri": uri } }),
    );

    let captured = barrier_dir.path().join("captured");
    assert!(
        (0..60).any(|_| {
            if captured.exists() {
                true
            } else {
                std::thread::sleep(std::time::Duration::from_millis(100));
                false
            }
        }),
        "the old host context should be captured before dispatch admission"
    );

    client.send_notification(
        "textDocument/didClose",
        json!({ "textDocument": { "uri": uri } }),
    );
    client.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri,
                "languageId": "markdown",
                "version": 1,
                "text": "new lifetime"
            }
        }),
    );

    let reopen_barrier = client.send_request(
        "textDocument/semanticTokens/full",
        json!({ "textDocument": { "uri": uri } }),
    );
    assert!(
        reopen_barrier.get("error").is_none(),
        "the post-reopen reader barrier should complete: {reopen_barrier:?}"
    );
    std::fs::write(barrier_dir.path().join("release"), b"release")
        .expect("release host request admission");

    let response = client.receive_response_for_id_public(request_id);
    assert_eq!(
        response["result"],
        json!([]),
        "a request carrying the closed incarnation must not answer from the reopened document: {response:?}"
    );

    shutdown(&mut client);
}

#[test]
fn e2e_whole_document_link_cancel_forwards_to_concatenated_layers() {
    let config_dir = tempfile::TempDir::new().expect("config dir");
    let config_path = config_dir.path().join("whole_doc_links_cancel.toml");
    std::fs::write(
        &config_path,
        r#"
[languages.markdown.bridge._self]
enabled = true

[languages.markdown.layers.aggregation."textDocument/documentLink"]
strategy = "concatenated"
priorities = ["virt", "host"]
"#,
    )
    .expect("write config");

    let cancel_dir = tempfile::TempDir::new().expect("cancel dir");
    let mut client = LspClient::builder()
        .arg("--config-file")
        .arg(config_path.to_str().expect("temp path should be UTF-8"))
        .env("MOCK_LSP_CANCEL_DIR", cancel_dir.path().to_string_lossy())
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
                    "mock-host-link": {
                        "cmd": [mock_bin(), "document-link-slow-host"],
                        "languages": ["markdown"]
                    },
                    "mock-virt-link": {
                        "cmd": [mock_bin(), "document-link-slow-virt"],
                        "languages": ["lua"]
                    }
                }
            }
        }),
    );
    client.send_notification("initialized", json!({}));

    let uri = "file:///test_whole_doc_links_cancel.md";
    client.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": uri,
                "languageId": "markdown",
                "version": 1,
                "text": "# Title\n\n```lua\nprint(1)\n```\n"
            }
        }),
    );

    (0..5)
        .find_map(|_| {
            let response = client.send_request(
                "textDocument/documentLink",
                json!({ "textDocument": { "uri": uri } }),
            );
            assert!(
                response.get("error").is_none(),
                "documentLink must not surface a top-level error; got: {:?}",
                response.get("error")
            );
            let links = response["result"].as_array().cloned().unwrap_or_default();
            if links.len() >= 2 {
                Some(())
            } else {
                std::thread::sleep(std::time::Duration::from_millis(100));
                None
            }
        })
        .expect("slow documentLink mocks should warm up with virt and host results");

    let host_request = cancel_dir
        .path()
        .join("document-link-slow-host.request.json");
    let virt_request = cancel_dir
        .path()
        .join("document-link-slow-virt.request.json");
    let host_cancel = cancel_dir
        .path()
        .join("document-link-slow-host.cancel.json");
    let virt_cancel = cancel_dir
        .path()
        .join("document-link-slow-virt.cancel.json");
    for path in [&host_request, &virt_request, &host_cancel, &virt_cancel] {
        let _ = std::fs::remove_file(path);
    }

    let request_id = client.send_request_async(
        "textDocument/documentLink",
        json!({ "textDocument": { "uri": uri } }),
    );
    let saw_requests = (0..60).any(|_| {
        if host_request.exists() && virt_request.exists() {
            true
        } else {
            std::thread::sleep(std::time::Duration::from_millis(100));
            false
        }
    });
    assert!(
        saw_requests,
        "both downstream documentLink requests should start before cancellation; files in {:?}: {:?}",
        cancel_dir.path(),
        std::fs::read_dir(cancel_dir.path())
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .map(|entry| entry.file_name())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    );
    client.send_notification("$/cancelRequest", json!({ "id": request_id }));

    let response = client.receive_response_for_id_public(request_id);
    assert_eq!(
        response.pointer("/error/code").and_then(Value::as_i64),
        Some(-32800),
        "concatenated documentLink must answer RequestCancelled; got {response:?}"
    );

    let saw_both = (0..60).any(|_| {
        if host_cancel.exists() && virt_cancel.exists() {
            true
        } else {
            std::thread::sleep(std::time::Duration::from_millis(100));
            false
        }
    });
    assert!(
        saw_both,
        "client cancel should be forwarded to both concatenated layer servers; files in {:?}: {:?}",
        cancel_dir.path(),
        std::fs::read_dir(cancel_dir.path())
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .map(|entry| entry.file_name())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    );

    shutdown(&mut client);
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
fn e2e_host_bridge_respects_layers_priorities() {
    // Omitting "host" from layers.priorities must gate the host layer off even
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
                                "textDocument/definition": { "priorities": ["virt", "native"] }
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
        "layers.priorities without 'host' must gate the host layer off"
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

/// Host-only formatting under an explicit `preferred` layer strategy (the
/// default is `concatenated` since cross-layer formatting became a
/// pipeline): no virt server is configured, so the lazy walk falls through
/// the empty virt layer and the host layer's whole-document edits win and
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

[languages.markdown.layers.aggregation."textDocument/formatting"]
strategy = "preferred"
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

// ==========================================================================
// Host-bridge willSave / willSaveWaitUntil (host-document-bridge, #357)
// ==========================================================================

const SAVE_URI: &str = "file:///test_host_will_save.md";

/// Initialize a client whose markdown host server runs the mock's `will-save`
/// mode, opening [`SAVE_URI`]. Returns the raw `initialize` response so the
/// capability-gate tests can inspect the advertised `textDocumentSync`.
fn init_will_save_client(config_toml: &str) -> (LspClient, tempfile::TempDir, Value) {
    init_will_save_client_with_mode(config_toml, "will-save")
}

/// As [`init_will_save_client`] but selects the mock server `mode` — used by
/// the timeout test to run the `will-save-slow` mock.
fn init_will_save_client_with_mode(
    config_toml: &str,
    mode: &str,
) -> (LspClient, tempfile::TempDir, Value) {
    let config_dir = tempfile::TempDir::new().expect("config dir");
    let config_path = config_dir.path().join("will_save.toml");
    std::fs::write(&config_path, config_toml).expect("write config");

    let mut client = LspClient::builder()
        .arg("--config-file")
        .arg(config_path.to_str().expect("temp path should be UTF-8"))
        .build();

    let init = client.send_request(
        "initialize",
        json!({
            "processId": std::process::id(),
            "rootUri": null,
            "capabilities": {},
            "workspaceFolders": null,
            "initializationOptions": {
                "languageServers": {
                    "mock-host": {
                        "cmd": [mock_bin(), mode],
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
                "uri": SAVE_URI,
                "languageId": "markdown",
                "version": 1,
                "text": MARKDOWN
            }
        }),
    );
    (client, config_dir, init)
}

/// Hover `uri` at `(line, character)` and parse the `will-save` mock's JSON
/// state (`{will,reason,willUri,did,didUri}`) from the hover contents, or `None`
/// while the bridge is still warming up.
fn save_state_hover(client: &mut LspClient, uri: &str, line: u64, character: u64) -> Option<Value> {
    let response = client.send_request(
        "textDocument/hover",
        json!({
            "textDocument": { "uri": uri },
            "position": { "line": line, "character": character },
        }),
    );
    assert!(response.get("error").is_none(), "hover must not error");
    response
        .pointer("/result/contents")
        .and_then(Value::as_str)
        .and_then(|s| serde_json::from_str::<Value>(s).ok())
}

/// Hover the host document itself (host-bridge `will-save` server).
fn host_save_hover(client: &mut LspClient) -> Option<Value> {
    save_state_hover(client, SAVE_URI, 0, 0)
}

#[test]
fn e2e_host_bridge_advertises_save_capabilities_when_enabled() {
    let (mut client, _config_dir, init) =
        init_will_save_client("[languages.markdown.bridge._self]\nenabled = true\n");

    let sync = init
        .pointer("/result/capabilities/textDocumentSync")
        .expect("textDocumentSync must be present");
    assert_eq!(
        sync.get("willSave").and_then(Value::as_bool),
        Some(true),
        "willSave must be advertised when a host bridge is configured; got {sync}"
    );
    assert_eq!(
        sync.get("willSaveWaitUntil").and_then(Value::as_bool),
        Some(true),
        "willSaveWaitUntil must be advertised when a host bridge is configured; got {sync}"
    );

    shutdown(&mut client);
}

#[test]
fn e2e_host_bridge_save_capabilities_decouple_willsave_from_waituntil() {
    // A server IS configured (so virt bridging can consume willSave) but host
    // bridging is OFF. willSave now fans out to virt too, so it must be
    // advertised; willSaveWaitUntil is host-only, so it must stay hidden (#357).
    let (mut client, _config_dir, init) = init_will_save_client("");

    let sync = init
        .pointer("/result/capabilities/textDocumentSync")
        .expect("textDocumentSync must be present");
    assert_eq!(
        sync.get("willSave").and_then(Value::as_bool),
        Some(true),
        "willSave must be advertised when any bridge server is configured; got {sync}"
    );
    assert!(
        sync.get("willSaveWaitUntil").is_none_or(Value::is_null),
        "willSaveWaitUntil must stay host-only (hidden without host bridging); got {sync}"
    );

    shutdown(&mut client);
}

#[test]
fn e2e_host_bridge_hides_save_capabilities_without_any_server() {
    // With no RUNNABLE bridge servers configured (only the built-in `_`
    // wildcard defaults entry, which has an empty cmd), neither save method has
    // a possible consumer, so kakehashi advertises neither (today's behavior).
    // Use an explicit empty config file and omit initializationOptions so no
    // default/user config can leak servers or host bridging in.
    let config_dir = tempfile::TempDir::new().expect("config dir");
    let config_path = config_dir.path().join("empty.toml");
    std::fs::write(&config_path, "").expect("write config");
    let mut client = LspClient::builder()
        .arg("--config-file")
        .arg(config_path.to_str().expect("temp path should be UTF-8"))
        .build();
    let init = client.send_request(
        "initialize",
        json!({
            "processId": std::process::id(),
            "rootUri": null,
            "capabilities": {},
        }),
    );

    let sync = init
        .pointer("/result/capabilities/textDocumentSync")
        .expect("textDocumentSync must be present");
    assert!(
        sync.get("willSave").is_none_or(Value::is_null),
        "willSave must NOT be advertised with no servers configured; got {sync}"
    );
    assert!(
        sync.get("willSaveWaitUntil").is_none_or(Value::is_null),
        "willSaveWaitUntil must NOT be advertised with no servers configured; got {sync}"
    );

    shutdown(&mut client);
}

#[test]
fn e2e_host_will_save_wait_until_returns_host_edits() {
    let (mut client, _config_dir, _init) =
        init_will_save_client("[languages.markdown.bridge._self]\nenabled = true\n");

    let mut hit = None;
    for _ in 0..300 {
        let response = client.send_request(
            "textDocument/willSaveWaitUntil",
            json!({
                "textDocument": { "uri": SAVE_URI },
                "reason": 1
            }),
        );
        assert!(
            response.get("error").is_none(),
            "willSaveWaitUntil must not surface a top-level error; got: {:?}",
            response.get("error")
        );
        let result = response["result"].clone();
        if !result.is_null() {
            hit = Some(result);
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let result = hit.expect("host willSaveWaitUntil must produce edits");

    let new_text = result[0]["newText"].as_str().expect("edit newText");
    assert_eq!(
        new_text,
        format!("willsave-edit:{SAVE_URI}\n"),
        "willSaveWaitUntil edit must echo the real host URI and return verbatim"
    );

    shutdown(&mut client);
}

#[test]
fn e2e_host_will_save_notification_reaches_host() {
    let (mut client, _config_dir, _init) =
        init_will_save_client("[languages.markdown.bridge._self]\nenabled = true\n");

    // Warm up: a hover opens the host document downstream (the host bridge syncs
    // lazily on the first request) and reports the willSave count — zero before
    // any willSave is forwarded.
    let mut warmed = false;
    for _ in 0..300 {
        if let Some(state) = host_save_hover(&mut client) {
            assert_eq!(
                state["will"], 0,
                "before any willSave the host server must report zero; got {state}"
            );
            warmed = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(
        warmed,
        "host hover must answer (document synced) before the willSave"
    );

    // Forward one willSave (reason 2 = AfterDelay).
    client.send_notification(
        "textDocument/willSave",
        json!({ "textDocument": { "uri": SAVE_URI }, "reason": 2 }),
    );

    // Subsequent hovers must reflect the recorded willSave: count 1, reason 2,
    // and the REAL host URI (the host path forwards verbatim).
    let mut seen = None;
    for _ in 0..300 {
        if let Some(state) = host_save_hover(&mut client)
            && state["will"] != 0
        {
            seen = Some(state);
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let state = seen.expect("the forwarded willSave must reach the host server");
    assert_eq!(state["will"], 1, "host server must record one willSave");
    assert_eq!(state["reason"], 2, "host server must record the reason");
    assert_eq!(
        state["willUri"], SAVE_URI,
        "host willSave must carry the real host URI verbatim"
    );

    shutdown(&mut client);
}

#[test]
fn e2e_host_did_save_notification_reaches_host() {
    let (mut client, _config_dir, _init) =
        init_will_save_client("[languages.markdown.bridge._self]\nenabled = true\n");

    let mut warmed = false;
    for _ in 0..300 {
        if let Some(state) = host_save_hover(&mut client) {
            assert_eq!(state["did"], 0, "no didSave before notification: {state}");
            warmed = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(warmed, "host document must be open before didSave");

    client.send_notification(
        "textDocument/didSave",
        json!({ "textDocument": { "uri": SAVE_URI } }),
    );

    let mut seen = None;
    for _ in 0..300 {
        if let Some(state) = host_save_hover(&mut client)
            && state["did"] != 0
        {
            seen = Some(state);
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let state = seen.expect("the forwarded didSave must reach the host server");
    assert_eq!(state["did"], 1);
    assert_eq!(state["didUri"], SAVE_URI, "host URI must remain verbatim");
    assert_eq!(
        state["didHadText"], false,
        "host didSave must omit the text field"
    );

    shutdown(&mut client);
}

#[test]
fn e2e_host_did_save_observes_the_latest_host_text() {
    const SAVED_TEXT: &str = "# Saved immediately\n";
    let (mut client, _config_dir, _init) =
        init_will_save_client("[languages.markdown.bridge._self]\nenabled = true\n");

    for _ in 0..300 {
        if host_save_hover(&mut client).is_some() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    client.send_notification(
        "textDocument/didChange",
        json!({
            "textDocument": { "uri": SAVE_URI, "version": 2 },
            "contentChanges": [{ "text": SAVED_TEXT }],
        }),
    );
    client.send_notification(
        "textDocument/didSave",
        json!({ "textDocument": { "uri": SAVE_URI } }),
    );

    let mut seen = None;
    for _ in 0..300 {
        if let Some(state) = host_save_hover(&mut client)
            && state["did"] != 0
        {
            seen = Some(state);
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let state = seen.expect("the immediate didSave must reach the host server");
    assert_eq!(
        state["didDocumentText"], SAVED_TEXT,
        "host didChange must precede didSave on the downstream wire"
    );

    shutdown(&mut client);
}

fn assert_host_did_save_is_skipped(mode: &str) {
    let (mut client, _config_dir, _init) = init_will_save_client_with_mode(
        "[languages.markdown.bridge._self]\nenabled = true\n",
        mode,
    );

    let mut warmed = false;
    for _ in 0..300 {
        if host_save_hover(&mut client).is_some() {
            warmed = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(warmed, "host document must be open before didSave");

    client.send_notification(
        "textDocument/didSave",
        json!({ "textDocument": { "uri": SAVE_URI } }),
    );
    for _ in 0..10 {
        std::thread::sleep(std::time::Duration::from_millis(100));
        let state = host_save_hover(&mut client).expect("hover must answer");
        assert_eq!(
            state["did"], 0,
            "didSave must not reach an incapable server: {state}"
        );
    }

    shutdown(&mut client);
}

#[test]
fn e2e_host_did_save_includes_saved_text_when_requested() {
    const INCLUDED_SAVED_TEXT: &str = "# Included saved text\n";
    let (mut client, _config_dir, _init) = init_will_save_client_with_mode(
        "[languages.markdown.bridge._self]\nenabled = true\n",
        "will-save-include-text",
    );

    for _ in 0..300 {
        if host_save_hover(&mut client).is_some() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    client.send_notification(
        "textDocument/didChange",
        json!({
            "textDocument": { "uri": SAVE_URI, "version": 2 },
            "contentChanges": [{ "text": INCLUDED_SAVED_TEXT }],
        }),
    );
    client.send_notification(
        "textDocument/didSave",
        json!({ "textDocument": { "uri": SAVE_URI } }),
    );

    let mut seen = None;
    for _ in 0..300 {
        if let Some(state) = host_save_hover(&mut client)
            && state["did"] != 0
        {
            seen = Some(state);
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let state = seen.expect("includeText=true server must receive didSave");
    assert_eq!(state["didHadText"], true);
    assert_eq!(state["didText"], INCLUDED_SAVED_TEXT);
    assert_eq!(state["didDocumentText"], INCLUDED_SAVED_TEXT);

    shutdown(&mut client);
}

#[test]
fn e2e_host_did_save_skips_server_without_save_capability() {
    assert_host_did_save_is_skipped("will-save-incapable");
}

#[test]
fn e2e_host_will_save_wait_until_times_out_without_hanging_save() {
    // The host server stalls 8s on willSaveWaitUntil; kakehashi's 5s save budget
    // must abandon the request and return null near 5s — NOT wait the 30s
    // request timeout (#357 Q3). Without the budget this test would block ~30s.
    let (mut client, _config_dir, _init) = init_will_save_client_with_mode(
        "[languages.markdown.bridge._self]\nenabled = true\n",
        "will-save-slow",
    );

    // Warm up so the connection is Ready before the timed request: a cold
    // FailFast request returns null instantly (no wait), which would not
    // exercise the timeout path.
    let mut warmed = false;
    for _ in 0..300 {
        if host_save_hover(&mut client).is_some() {
            warmed = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(warmed, "host bridge must be warm before the timed request");

    let start = std::time::Instant::now();
    let response = client.send_request(
        "textDocument/willSaveWaitUntil",
        json!({
            "textDocument": { "uri": SAVE_URI },
            "reason": 1
        }),
    );
    let elapsed = start.elapsed();

    assert!(
        response.get("error").is_none(),
        "a timed-out willSaveWaitUntil must return a result, not an error; got: {:?}",
        response.get("error")
    );
    assert!(
        response["result"].is_null(),
        "a timed-out willSaveWaitUntil must return null (save proceeds editless); got: {}",
        response["result"]
    );
    // The 5s budget — not an instant cold null (< 1s) and not the 30s request
    // timeout. Generous bounds keep this robust under CI load.
    assert!(
        elapsed >= std::time::Duration::from_secs(3),
        "must wait for the budget, not return an instant cold null; elapsed {elapsed:?}"
    );
    assert!(
        elapsed < std::time::Duration::from_secs(20),
        "must time out on the 5s budget, not the 30s request timeout; elapsed {elapsed:?}"
    );

    shutdown(&mut client);
}

// ==========================================================================
// Virt-bridge willSave / didSave (notifications fan out to virtual docs, #357)
// ==========================================================================

const VIRT_SAVE_URI: &str = "file:///test_virt_save.md";
/// A markdown host whose lua fence content sits on LSP line 3 (`print(1)`).
const VIRT_SAVE_MARKDOWN: &str = "# Title\n\n```lua\nprint(1)\n```\n";

/// Initialize a client whose **lua** injections are served by the mock's
/// `will-save` mode (a virt bridge, no host bridging), and open a markdown host
/// with a lua fence.
fn init_virt_save_client() -> (LspClient, tempfile::TempDir) {
    init_virt_save_client_with_mode("will-save")
}

fn init_virt_save_client_with_mode(mode: &str) -> (LspClient, tempfile::TempDir) {
    let config_dir = tempfile::TempDir::new().expect("config dir");
    let config_path = config_dir.path().join("virt_save.toml");
    std::fs::write(&config_path, "").expect("write config");

    let mut client = LspClient::builder()
        .arg("--config-file")
        .arg(config_path.to_str().expect("temp path should be UTF-8"))
        .build();
    client.send_request(
        "initialize",
        json!({
            "processId": std::process::id(),
            "rootUri": null,
            "capabilities": {},
            "workspaceFolders": null,
            "initializationOptions": {
                "languageServers": {
                    "lua-save": { "cmd": [mock_bin(), mode], "languages": ["lua"] }
                }
            }
        }),
    );
    client.send_notification("initialized", json!({}));
    client.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": VIRT_SAVE_URI,
                "languageId": "markdown",
                "version": 1,
                "text": VIRT_SAVE_MARKDOWN
            }
        }),
    );
    (client, config_dir)
}

/// Hover inside the lua fence content (LSP line 3 = `print(1)`), which routes to
/// the lua virt server and opens its virtual document.
fn virt_save_hover(client: &mut LspClient) -> Option<Value> {
    save_state_hover(client, VIRT_SAVE_URI, 3, 2)
}

#[test]
fn e2e_virt_will_save_and_did_save_reach_virtual_doc() {
    let (mut client, _config_dir) = init_virt_save_client();

    // Warm up: a hover inside the lua fence opens the virtual document on the
    // virt server (lazy didOpen). Zero saves recorded yet.
    let mut warmed = false;
    for _ in 0..300 {
        if let Some(state) = virt_save_hover(&mut client) {
            assert_eq!(state["will"], 0, "no willSave yet; got {state}");
            assert_eq!(state["did"], 0, "no didSave yet; got {state}");
            warmed = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(
        warmed,
        "virt lua server must answer hover (virtual doc opened) before the saves"
    );

    // Forward willSave (reason 1) and didSave on the HOST document.
    client.send_notification(
        "textDocument/willSave",
        json!({ "textDocument": { "uri": VIRT_SAVE_URI }, "reason": 1 }),
    );
    client.send_notification(
        "textDocument/didSave",
        json!({ "textDocument": { "uri": VIRT_SAVE_URI } }),
    );

    // The virt server must receive BOTH, carrying the VIRTUAL document URI — a
    // host URI here would betray a routing bug.
    let mut seen = None;
    for _ in 0..300 {
        if let Some(state) = virt_save_hover(&mut client)
            && state["will"] != 0
            && state["did"] != 0
        {
            seen = Some(state);
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let state = seen.expect("willSave + didSave must reach the virt server");
    assert_eq!(
        state["reason"], 1,
        "willSave reason must be forwarded verbatim"
    );

    for key in ["willUri", "didUri"] {
        let uri = state[key].as_str().unwrap_or_default();
        assert!(
            uri.contains("kakehashi-virtual-uri"),
            "{key} must be the VIRTUAL document URI; got {uri}"
        );
        assert_ne!(uri, VIRT_SAVE_URI, "{key} must NOT be the host URI");
    }

    shutdown(&mut client);
}

#[test]
fn e2e_virt_did_save_observes_the_latest_virtual_text() {
    const SAVED_MARKDOWN: &str = "# Title\n\n```lua\nprint(2)\n```\n";
    let (mut client, _config_dir) = init_virt_save_client();

    let mut warmed = false;
    for _ in 0..300 {
        if virt_save_hover(&mut client).is_some() {
            warmed = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(warmed, "virtual document must be open before the edit");

    client.send_notification(
        "textDocument/didChange",
        json!({
            "textDocument": { "uri": VIRT_SAVE_URI, "version": 2 },
            "contentChanges": [{ "text": SAVED_MARKDOWN }],
        }),
    );
    client.send_notification(
        "textDocument/didSave",
        json!({ "textDocument": { "uri": VIRT_SAVE_URI } }),
    );

    let mut seen = None;
    for _ in 0..300 {
        if let Some(state) = virt_save_hover(&mut client)
            && state["did"] != 0
        {
            seen = Some(state);
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let state = seen.expect("the immediate didSave must reach the virtual server");
    let saved_text = state["didDocumentText"]
        .as_str()
        .expect("the server must retain its virtual document text at didSave");
    assert!(
        saved_text.contains("print(2)"),
        "virtual didChange must precede didSave; got {saved_text:?}"
    );
    assert!(
        !saved_text.contains("print(1)"),
        "stale virtual text remained"
    );

    shutdown(&mut client);
}

#[test]
fn e2e_virt_did_save_includes_projected_text_when_requested() {
    const INCLUDED_SAVED_MARKDOWN: &str = "# Title\n\n```lua\nprint(2)\n```\n";
    let (mut client, _config_dir) = init_virt_save_client_with_mode("will-save-include-text");

    for _ in 0..300 {
        if virt_save_hover(&mut client).is_some() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    client.send_notification(
        "textDocument/didChange",
        json!({
            "textDocument": { "uri": VIRT_SAVE_URI, "version": 2 },
            "contentChanges": [{ "text": INCLUDED_SAVED_MARKDOWN }],
        }),
    );
    client.send_notification(
        "textDocument/didSave",
        json!({ "textDocument": { "uri": VIRT_SAVE_URI } }),
    );

    let mut seen = None;
    for _ in 0..300 {
        if let Some(state) = virt_save_hover(&mut client)
            && state["did"] != 0
        {
            seen = Some(state);
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let state = seen.expect("includeText=true virtual server must receive didSave");
    assert_eq!(state["didHadText"], true);
    let text = state["didText"]
        .as_str()
        .expect("didSave must include projected virtual text");
    assert_eq!(state["didText"], state["didDocumentText"]);
    assert!(text.contains("print(2)"));
    assert!(!text.contains("# Title"));

    shutdown(&mut client);
}

#[test]
fn e2e_virt_did_save_never_observes_a_later_unsaved_edit() {
    const SAVED_MARKDOWN: &str = "# Title\n\n```lua\nprint(2)\n```\n";
    const UNSAVED_MARKDOWN: &str = "# Title\n\n```lua\nprint(3)\n```\n";
    let (mut client, _config_dir) = init_virt_save_client();

    let mut warmed = false;
    for _ in 0..300 {
        if virt_save_hover(&mut client).is_some() {
            warmed = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(warmed, "virtual document must be open before the edits");

    client.send_notification(
        "textDocument/didChange",
        json!({
            "textDocument": { "uri": VIRT_SAVE_URI, "version": 2 },
            "contentChanges": [{ "text": SAVED_MARKDOWN }],
        }),
    );
    client.send_notification(
        "textDocument/didSave",
        json!({ "textDocument": { "uri": VIRT_SAVE_URI } }),
    );
    client.send_notification(
        "textDocument/didChange",
        json!({
            "textDocument": { "uri": VIRT_SAVE_URI, "version": 3 },
            "contentChanges": [{ "text": UNSAVED_MARKDOWN }],
        }),
    );

    let mut settled = None;
    for _ in 0..300 {
        if let Some(state) = virt_save_hover(&mut client)
            && state["documentText"]
                .as_str()
                .is_some_and(|text| text.contains("print(3)"))
        {
            settled = Some(state);
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let state = settled.expect("the later unsaved didChange must reach the virtual server");
    if state["did"] != 0 {
        let saved_text = state["didDocumentText"]
            .as_str()
            .expect("a delivered didSave must record its document text");
        assert!(
            saved_text.contains("print(2)"),
            "didSave may observe saved text or be omitted, never later unsaved text: {saved_text:?}"
        );
    }

    shutdown(&mut client);
}

#[test]
fn e2e_virt_save_skips_server_without_save_capability() {
    // The per-server capability gate is the phantom-save mitigation: a virt
    // server that advertises neither `willSave` nor `save` must NOT be told
    // about the host save, even with its virtual doc open (#357).
    let (mut client, _config_dir) = init_virt_save_client_with_mode("will-save-incapable");

    // Warm up: open the virtual doc (hover still works — the mode advertises it)
    // and confirm zero saves recorded.
    let mut warmed = false;
    for _ in 0..300 {
        if let Some(state) = virt_save_hover(&mut client) {
            assert_eq!(state["will"], 0);
            assert_eq!(state["did"], 0);
            warmed = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(warmed, "incapable virt server must still answer hover");

    client.send_notification(
        "textDocument/willSave",
        json!({ "textDocument": { "uri": VIRT_SAVE_URI }, "reason": 1 }),
    );
    client.send_notification(
        "textDocument/didSave",
        json!({ "textDocument": { "uri": VIRT_SAVE_URI } }),
    );

    // Give the (incorrect) fan-out time to land, then confirm the gate held:
    // counts stay zero. Poll a handful of times so a late delivery would fail.
    for _ in 0..10 {
        std::thread::sleep(std::time::Duration::from_millis(100));
        let state = virt_save_hover(&mut client).expect("hover must answer");
        assert_eq!(
            state["will"], 0,
            "willSave must NOT reach a server lacking the willSave capability; got {state}"
        );
        assert_eq!(
            state["did"], 0,
            "didSave must NOT reach a server lacking the save capability; got {state}"
        );
    }

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

/// Cross-layer pull diagnostics (cross-layer-aggregation): with the default
/// `concatenated` layers strategy and no virt servers, the host layer's
/// diagnostics flow into the `textDocument/diagnostic` report — carrying the
/// real URI, proving the host document was synced and the pull was answered
/// by the host server.
#[test]
fn e2e_host_diagnostics_merge_into_pull_report() {
    let config_dir = tempfile::TempDir::new().expect("config dir");
    let config_path = config_dir.path().join("host_diag.toml");
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
                    "mock-host-diag": { "cmd": [mock_bin(), "diagnostics"], "languages": ["markdown"] },
                }
            }
        }),
    );
    client.send_notification("initialized", json!({}));

    let uri = "file:///test_host_diag.md";
    client.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": { "uri": uri, "languageId": "markdown", "version": 1,
                              "text": "# title\n\nprose body\n" }
        }),
    );

    let mut items = None;
    for _ in 0..300 {
        let response = client.send_request(
            "textDocument/diagnostic",
            json!({ "textDocument": { "uri": uri } }),
        );
        assert!(
            response.get("error").is_none(),
            "diagnostic must not surface a top-level error; got: {:?}",
            response.get("error")
        );
        if let Some(found) = response
            .pointer("/result/items")
            .and_then(Value::as_array)
            .filter(|a| !a.is_empty())
        {
            items = Some(found.clone());
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let items = items.expect("host diagnostics must reach the pull report");
    let message = items[0]["message"].as_str().expect("diagnostic message");
    assert_eq!(
        message,
        format!("mock-diagnostic:{uri}"),
        "the host server must have been pulled with the real URI"
    );

    shutdown(&mut client);
}

/// Cross-layer push diagnostics: the synthetic publish triggered by didOpen
/// must carry the host layer's diagnostics under the default `concatenated`
/// layers strategy.
#[test]
fn e2e_host_diagnostics_merge_into_synthetic_push() {
    let config_dir = tempfile::TempDir::new().expect("config dir");
    let config_path = config_dir.path().join("host_push_diag.toml");
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
                    "mock-host-diag": { "cmd": [mock_bin(), "diagnostics"], "languages": ["markdown"] },
                }
            }
        }),
    );
    client.send_notification("initialized", json!({}));

    let uri = "file:///test_host_push_diag.md";
    client.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": { "uri": uri, "languageId": "markdown", "version": 1,
                              "text": "# title\n\nprose body\n" }
        }),
    );

    // The first publish may be empty (cold host server answered nothing
    // within its window) — didSave retriggers the synthetic push, so keep
    // saving until a non-empty publish arrives.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    let mut hit = None;
    while std::time::Instant::now() < deadline {
        if let Some(params) = client.wait_for_notification(
            "textDocument/publishDiagnostics",
            std::time::Duration::from_secs(2),
        ) {
            let diagnostics = params["diagnostics"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            if params["uri"].as_str() == Some(uri) && !diagnostics.is_empty() {
                hit = Some(diagnostics);
                break;
            }
        }
        client.send_notification(
            "textDocument/didSave",
            json!({ "textDocument": { "uri": uri } }),
        );
    }
    let diagnostics = hit.expect("synthetic push must carry the host layer's diagnostics");
    assert_eq!(
        diagnostics[0]["message"].as_str(),
        Some(format!("mock-diagnostic:{uri}").as_str()),
        "the host server must have been pulled with the real URI"
    );

    shutdown(&mut client);
}

#[test]
fn e2e_host_bridge_codeaction_resolve_round_trips_verbatim() {
    // #627: a HOST-layer (bridge._self) lazy code action is enveloped, and its
    // codeAction/resolve routes back to the host server VERBATIM (no coordinate
    // translation), materializing the edit on the host document. Guards against
    // the resolve path being inert (the lsp_impl region-freshness gate must be
    // skipped for host envelopes, which carry no region).
    let config_dir = tempfile::TempDir::new().expect("temp dir");
    let config_path = config_dir.path().join("host_ca.toml");
    std::fs::write(
        &config_path,
        "[languages.markdown.bridge._self]\nenabled = true\n",
    )
    .expect("write config");

    let mut client = LspClient::builder()
        .arg("--config-file")
        .arg(config_path.to_str().expect("utf-8 path"))
        .build();

    client.send_request(
        "initialize",
        json!({
            "processId": std::process::id(),
            "rootUri": null,
            "capabilities": {
                "textDocument": { "codeAction": {
                    "codeActionLiteralSupport": { "codeActionKind": {
                        "valueSet": ["quickfix", "source", "source.organizeImports"]
                    } },
                    "dataSupport": true,
                    "resolveSupport": { "properties": ["edit"] },
                    "disabledSupport": true,
                    "isPreferredSupport": true
                } }
            },
            "workspaceFolders": null,
            "initializationOptions": {
                "languageServers": {
                    "mock-host": {
                        "cmd": [mock_bin(), "code-action-lazy"],
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

    // codeAction on the host document -> one lazy action, enveloped (host_layer)
    // so it can be resolved. Retry while the host connection warms up.
    let action = (0..300)
        .find_map(|_| {
            let resp = client.send_request(
                "textDocument/codeAction",
                json!({
                    "textDocument": { "uri": MARKDOWN_URI },
                    "range": {
                        "start": { "line": 0, "character": 0 },
                        "end": { "line": 0, "character": 1 }
                    },
                    "context": { "diagnostics": [] }
                }),
            );
            let first = resp["result"].as_array().and_then(|a| a.first().cloned());
            if first.is_none() {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            first
        })
        .expect("host codeAction returns a lazy action");

    assert_eq!(action["title"], "Lazy organize imports — mock-host");
    assert!(action["edit"].is_null(), "the action is lazy (no edit yet)");
    assert!(
        action["data"].is_object(),
        "a lazy host action carries a routing envelope, got: {action}"
    );

    // Resolve it -> routes to the host server verbatim; the edit materializes on
    // the HOST document, and the mock's newText echoes the RESTORED (unsuffixed)
    // original title.
    let resolved = client.send_request("codeAction/resolve", action);
    assert!(
        resolved.get("error").is_none(),
        "resolve errored: {resolved}"
    );
    let result = &resolved["result"];
    assert_eq!(
        result["title"], "Lazy organize imports — mock-host",
        "resolved title is re-suffixed"
    );
    let edits = result["edit"]["changes"][MARKDOWN_URI]
        .as_array()
        .unwrap_or_else(|| panic!("resolved edit on the host doc, got: {result}"));
    assert_eq!(
        edits[0]["newText"], "organized:Lazy organize imports",
        "host resolve routed with the original title restored and the edit verbatim"
    );

    shutdown(&mut client);
}

/// Bring up a host-bridged markdown client backed by one `mock-host` server
/// running `mode`, and return it with the first completion response for a
/// position outside any injection (so only the host layer can answer). Retries
/// while the host connection warms up.
fn init_host_completion_client(mode: &str) -> (LspClient, tempfile::TempDir, Value) {
    let config_dir = tempfile::TempDir::new().expect("temp dir");
    let config_path = config_dir.path().join("host_completion.toml");
    std::fs::write(
        &config_path,
        "[languages.markdown.bridge._self]\nenabled = true\n",
    )
    .expect("write config");

    let mut client = LspClient::builder()
        .arg("--config-file")
        .arg(config_path.to_str().expect("utf-8 path"))
        .build();

    client.send_request(
        "initialize",
        json!({
            "processId": std::process::id(),
            "rootUri": null,
            "capabilities": {},
            "workspaceFolders": null,
            "initializationOptions": {
                "languageServers": {
                    "mock-host": {
                        "cmd": [mock_bin(), mode],
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

    let item = (0..300)
        .find_map(|_| {
            let resp = client.send_request(
                "textDocument/completion",
                json!({
                    "textDocument": { "uri": MARKDOWN_URI },
                    "position": { "line": 2, "character": 6 }
                }),
            );
            let first = resp["result"]["items"]
                .as_array()
                .and_then(|items| items.first().cloned());
            if first.is_none() {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            first
        })
        .expect("host completion returns an item");

    (client, config_dir, item)
}

#[test]
fn e2e_host_bridge_completion_resolve_round_trips_verbatim() {
    // #958: a HOST-layer (bridge._self) completion item is enveloped, and its
    // completionItem/resolve routes back to the host server VERBATIM (no
    // coordinate translation), so the server can fill in the lazy fields.
    // Guards against the resolve path being inert — the symptom was a response
    // byte-for-byte identical to the request, because an un-enveloped item has
    // no origin to route to and the lsp_impl region-freshness gate would reject
    // a host envelope, which carries no region.
    let (mut client, _config_dir, item) = init_host_completion_client("completion-resolve");

    assert_eq!(item["label"], "./test");
    assert!(
        item["detail"].is_null(),
        "the item is unresolved (no detail yet), got: {item}"
    );
    assert_eq!(
        item["data"]["kakehashi"]["host_layer"], true,
        "a resolvable host item carries a HOST-layer routing envelope: {item}"
    );
    assert_eq!(
        item["data"]["kakehashi"]["inner"],
        json!({ "mockPath": MARKDOWN_URI }),
        "with the server's own data preserved inside it: {item}"
    );

    // Resolve twice: the second round is what proves the envelope survives a
    // resolve with its layer marker intact. With the marker dropped, round two
    // takes the virt path, fails the region gate, and comes back unresolved.
    let mut resolved = client.send_request("completionItem/resolve", item);
    for round in 1..=2 {
        assert!(
            resolved.get("error").is_none(),
            "resolve round {round} errored: {resolved}"
        );
        let result = &resolved["result"];
        assert_eq!(
            result["detail"],
            format!("mock-resolved:{MARKDOWN_URI}"),
            "round {round}: the resolve must reach the host server, which fills detail from \
             the item's own data — proving both the routing and the data round trip: {result}"
        );
        // The materialized edit sits on host line 2, outside any injection
        // region. The virt resolve path would reject it as unsafe and serve
        // the item unresolved, so its arrival — at its original coordinates —
        // is what proves the VERBATIM host path ran.
        assert_eq!(
            result["textEdit"]["range"]["start"]["line"], 2,
            "round {round}: the host resolve must not translate coordinates: {result}"
        );
        assert_eq!(result["textEdit"]["newText"], "resolved-edit");
        assert_eq!(
            result["data"]["kakehashi"]["host_layer"], true,
            "round {round}: the resolved item keeps its host marker so the NEXT \
             resolve still routes to the host server: {result}"
        );
        resolved = client.send_request("completionItem/resolve", result.clone());
    }

    shutdown(&mut client);
}

#[test]
fn e2e_host_bridge_completion_skips_envelope_without_resolve_support() {
    // The envelope exists only to route completionItem/resolve. A host server
    // that does not advertise resolveProvider gets none: it would be pure wire
    // weight on every item of every completion.
    let (mut client, _config_dir, item) = init_host_completion_client("completion-no-resolve");

    assert_eq!(item["label"], "./test");
    assert_eq!(
        item["data"],
        json!({ "mockPath": MARKDOWN_URI }),
        "the server's own data must reach the client untouched, with no \
         routing envelope wrapped around it: {item}"
    );

    shutdown(&mut client);
}
