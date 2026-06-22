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
