//! E2E tests for bridged `textDocument/codeAction` (#568), using the
//! `mock-lsp-formatter` test binary in `code-action` mode (one edit-carrying
//! quickfix + one bare Command action) and `code-action-lazy` mode (one
//! resolve-only action).
//!
//! Proves end-to-end:
//! - `codeActionProvider{resolveProvider}` is advertised upstream.
//! - The request range is translated host→virtual, the returned edit is
//!   re-keyed to the host URI with host-translated ranges, and the title
//!   carries the `"{title} — {server}"` suffix.
//! - A bare Command action surfaces as an EXECUTABLE command with a routed
//!   name (regardless of `disabledSupport`); executing it drives a bridged
//!   `workspace/executeCommand` back to the origin server, which answers by
//!   asking the client to apply an edit (PR 5 + PR 6 compose).
//! - A lazy action is enveloped and resolved via `codeAction/resolve` for a
//!   resolve-capable client, and eager-resolved downstream (edit inline) for a
//!   client lacking resolve/data support (PR 4).

#![cfg(feature = "e2e")]

mod helpers;

use std::time::Duration;

use helpers::lsp_client::LspClient;
use serde_json::{Value, json};

fn mock_formatter_bin() -> &'static str {
    env!("CARGO_BIN_EXE_mock-lsp-formatter")
}

/// A bridged command name encodes `kakehashi\u{1f}{origin}\u{1f}{host_uri}\u{1f}{command}`
/// (see `bridge::protocol::command_routing`). Assert the routing key without
/// depending on the exact host-URI field.
fn is_routed_command(name: &str, origin: &str, command: &str) -> bool {
    name.starts_with(&format!("kakehashi\u{1f}{origin}\u{1f}"))
        && name.ends_with(&format!("\u{1f}{command}"))
}

fn init_client(client_capabilities: Value) -> (LspClient, Value, tempfile::TempDir) {
    init_client_mode("code-action", client_capabilities)
}

fn init_client_mode(
    mock_mode: &str,
    client_capabilities: Value,
) -> (LspClient, Value, tempfile::TempDir) {
    let bin = mock_formatter_bin();
    let config_dir = tempfile::TempDir::new().expect("Failed to create config temp dir");
    let config_path = config_dir.path().join("code_action.toml");
    std::fs::write(&config_path, "").expect("Failed to write config");

    let mut client = LspClient::builder()
        .arg("--config-file")
        .arg(config_path.to_str().expect("temp path should be UTF-8"))
        .build();

    let init_response = client.send_request(
        "initialize",
        json!({
            "processId": std::process::id(),
            "rootUri": null,
            "capabilities": client_capabilities,
            "workspaceFolders": null,
            "initializationOptions": { "languageServers": {
                "mock-codeaction": { "cmd": [bin, mock_mode], "languages": ["lua"] }
            }}
        }),
    );
    client.send_notification("initialized", json!({}));
    (client, init_response, config_dir)
}

/// Two virt servers (`mock-a`, `mock-b`) both bridging the lua region in
/// `code-action` mode — for the concatenated-aggregation test.
fn init_client_two_servers(client_capabilities: Value) -> (LspClient, Value, tempfile::TempDir) {
    let bin = mock_formatter_bin();
    let config_dir = tempfile::TempDir::new().expect("Failed to create config temp dir");
    let config_path = config_dir.path().join("code_action.toml");
    std::fs::write(&config_path, "").expect("Failed to write config");

    let mut client = LspClient::builder()
        .arg("--config-file")
        .arg(config_path.to_str().expect("temp path should be UTF-8"))
        .build();

    let init_response = client.send_request(
        "initialize",
        json!({
            "processId": std::process::id(),
            "rootUri": null,
            "capabilities": client_capabilities,
            "workspaceFolders": null,
            "initializationOptions": { "languageServers": {
                "mock-a": { "cmd": [bin, "code-action"], "languages": ["lua"] },
                "mock-b": { "cmd": [bin, "code-action"], "languages": ["lua"] }
            }}
        }),
    );
    client.send_notification("initialized", json!({}));
    (client, init_response, config_dir)
}

/// Client capabilities with codeActionLiteralSupport (required for the
/// bridge to advertise codeActionProvider at all — it can only produce
/// CodeAction literals), optionally with disabledSupport.
fn literal_support_caps(disabled_support: bool) -> Value {
    let mut code_action = json!({
        "codeActionLiteralSupport": { "codeActionKind": { "valueSet": [] } }
    });
    if disabled_support {
        code_action["disabledSupport"] = json!(true);
    }
    json!({ "textDocument": { "codeAction": code_action } })
}

/// Literal support PLUS dataSupport + resolveSupport — the client can hold a
/// lazy action's routing envelope and issue codeAction/resolve (PR 4).
fn resolve_support_caps() -> Value {
    json!({ "textDocument": { "codeAction": {
        "codeActionLiteralSupport": { "codeActionKind": { "valueSet": [] } },
        "disabledSupport": true,
        "dataSupport": true,
        "resolveSupport": { "properties": ["edit"] }
    }}})
}

fn assert_advertised(init_response: &Value) {
    // PR 4 advertises the Options form so `resolveProvider` can be declared.
    assert_eq!(
        init_response["result"]["capabilities"]["codeActionProvider"]["resolveProvider"],
        json!(true),
        "codeActionProvider{{resolveProvider}} must be advertised for literal-support clients (#568)"
    );
    // PR 6: executeCommand must be advertised (empty command list) so a real
    // client actually fires action-embedded commands — the one thing the mock
    // harness cannot gate on, since LspClient sends regardless.
    let execute = &init_response["result"]["capabilities"]["executeCommandProvider"];
    assert_eq!(
        execute["commands"],
        json!([]),
        "executeCommandProvider must be advertised with an empty command list (#568 PR 6)"
    );
}

/// Markdown host: the lua fence content sits on host line 3.
const MARKDOWN: &str = "# Test\n\n```lua\nlocal x = 1\n```\n";
const MARKDOWN_URI: &str = "file:///test_code_action.md";

fn shutdown(client: &mut LspClient) {
    let _ = client.send_request("shutdown", json!(null));
    client.send_notification("exit", json!(null));
}

fn open_markdown(client: &mut LspClient) {
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
    // No fixed settle sleep: every caller drives the codeAction request through
    // a retry loop (`code_action_with_retry` / `code_action_over_fence`, 300 ×
    // 50ms, waiting for a non-empty result), which already absorbs the cold
    // downstream handshake — a hard 1s sleep was pure per-test latency.
}

/// Issue `textDocument/codeAction` over the lua fence line, retrying while
/// the result is null (cold downstream still handshaking).
fn code_action_with_retry(client: &mut LspClient) -> Vec<Value> {
    for _ in 0..300 {
        let response = client.send_request(
            "textDocument/codeAction",
            json!({
                "textDocument": { "uri": MARKDOWN_URI },
                "range": {
                    "start": { "line": 3, "character": 0 },
                    "end": { "line": 3, "character": 5 }
                },
                "context": { "diagnostics": [] }
            }),
        );
        if let Some(actions) = response["result"].as_array()
            && !actions.is_empty()
        {
            return actions.clone();
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("textDocument/codeAction never returned actions");
}

#[test]
fn code_action_edit_is_host_translated_and_suffixed() {
    // Even without disabledSupport, the bare Command surfaces (executable, not
    // disabled): the edit action + the command action both come back.
    let (mut client, init_response, _config_dir) = init_client(literal_support_caps(false));
    assert_advertised(&init_response);
    open_markdown(&mut client);

    let actions = code_action_with_retry(&mut client);

    assert_eq!(actions.len(), 2, "edit + command action, got: {actions:?}");
    let action = actions
        .iter()
        .find(|a| a["kind"] == "quickfix")
        .expect("the edit-carrying quickfix");
    assert_eq!(action["title"], "Replace with fixed — mock-codeaction");
    let edits = &action["edit"]["changes"][MARKDOWN_URI];
    assert!(
        edits.is_array(),
        "edit must be re-keyed to the host URI, got: {action:?}"
    );
    // Virtual line 0 = host line 3 (fence content line).
    assert_eq!(edits[0]["range"]["start"]["line"], 3);
    assert_eq!(edits[0]["newText"], "fixed");

    // The bare Command carries a routed name that encodes the origin server.
    let command = actions
        .iter()
        .find(|a| a["command"].is_string())
        .expect("the executable command action");
    assert_eq!(command["title"], "Run mock command — mock-codeaction");
    assert!(
        is_routed_command(
            command["command"].as_str().unwrap(),
            "mock-codeaction",
            "mock.run"
        ),
        "command name must encode the origin server, got: {command:?}"
    );

    shutdown(&mut client);
}

#[test]
fn whole_document_range_reaches_the_injection_region() {
    // Save-time flows (editor.codeActionsOnSave) request with a range that
    // starts at (0,0), OUTSIDE any injection — the bridge must still find
    // the overlapped region instead of skipping the virt layer.
    let (mut client, init_response, _config_dir) = init_client(literal_support_caps(false));
    assert_advertised(&init_response);
    open_markdown(&mut client);

    let actions = (0..300)
        .find_map(|_| {
            let response = client.send_request(
                "textDocument/codeAction",
                json!({
                    "textDocument": { "uri": MARKDOWN_URI },
                    "range": {
                        "start": { "line": 0, "character": 0 },
                        "end": { "line": 5, "character": 0 }
                    },
                    "context": { "diagnostics": [] }
                }),
            );
            match response["result"].as_array() {
                Some(actions) if !actions.is_empty() => Some(actions.clone()),
                _ => {
                    std::thread::sleep(Duration::from_millis(50));
                    None
                }
            }
        })
        .expect("whole-document codeAction never returned actions");

    assert_eq!(actions[0]["title"], "Replace with fixed — mock-codeaction");
    // The edit still lands on the fence content line in host coordinates.
    assert_eq!(
        actions[0]["edit"]["changes"][MARKDOWN_URI][0]["range"]["start"]["line"],
        3
    );

    shutdown(&mut client);
}

/// Two `code-action-preferred` servers with the CROSS-layer strategy forced to
/// `preferred` (within-layer stays the default `concatenated`), so the virt
/// layer concatenates both servers' preferred actions but the cross layer does
/// not merge — exercising the isPreferred collapse on the preferred cross-layer
/// path (#568 PR 7, codex finding).
fn init_client_preferred_crosslayer_two_servers(
    client_capabilities: Value,
) -> (LspClient, Value, tempfile::TempDir) {
    let bin = mock_formatter_bin();
    let config_dir = tempfile::TempDir::new().expect("Failed to create config temp dir");
    let config_path = config_dir.path().join("code_action.toml");
    std::fs::write(
        &config_path,
        r#"[languages._.layers.aggregation."textDocument/codeAction"]
strategy = "preferred"
"#,
    )
    .expect("Failed to write config");

    let mut client = LspClient::builder()
        .arg("--config-file")
        .arg(config_path.to_str().expect("temp path should be UTF-8"))
        .build();

    let init_response = client.send_request(
        "initialize",
        json!({
            "processId": std::process::id(),
            "rootUri": null,
            "capabilities": client_capabilities,
            "workspaceFolders": null,
            "initializationOptions": { "languageServers": {
                "mock-a": { "cmd": [bin, "code-action-preferred"], "languages": ["lua"] },
                "mock-b": { "cmd": [bin, "code-action-preferred"], "languages": ["lua"] }
            }}
        }),
    );
    client.send_notification("initialized", json!({}));
    (client, init_response, config_dir)
}

#[test]
fn ispreferred_collapse_runs_even_under_crosslayer_preferred() {
    // Cross-layer `preferred` + within-layer `concatenated`: the virt layer
    // merges both servers' isPreferred quickfixes (two `isPreferred: true`), and
    // the cross layer returns that virt result as a unit. The final menu must
    // still collapse to exactly ONE preferred action — reverting the collapse to
    // the concatenated-only arm would leave two.
    // isPreferredSupport is required, else bridge_code_action strips is_preferred
    // entirely and there's nothing to collapse.
    let mut caps = literal_support_caps(true);
    caps["textDocument"]["codeAction"]["isPreferredSupport"] = json!(true);
    let (mut client, init_response, _config_dir) =
        init_client_preferred_crosslayer_two_servers(caps);
    assert_advertised(&init_response);
    open_markdown(&mut client);

    let actions = code_action_with_retry(&mut client);
    let preferred_count = actions
        .iter()
        .filter(|a| a["isPreferred"] == json!(true))
        .count();
    assert_eq!(
        preferred_count, 1,
        "exactly one action stays preferred after the collapse, got: {actions:?}"
    );

    shutdown(&mut client);
}

#[test]
fn concatenated_default_merges_actions_from_both_servers() {
    // codeAction defaults to `concatenated` at both aggregation levels (#568
    // PR 7), so BOTH virt servers' actions appear in one menu. Under the old
    // `preferred` default only the highest-priority server's actions would
    // survive — so this asserting both is the discriminating test.
    let (mut client, init_response, _config_dir) =
        init_client_two_servers(literal_support_caps(true));
    assert_advertised(&init_response);
    open_markdown(&mut client);

    let actions = code_action_with_retry(&mut client);
    let titles: Vec<&str> = actions.iter().filter_map(|a| a["title"].as_str()).collect();

    assert!(
        titles.iter().any(|t| t.ends_with("— mock-a")),
        "mock-a's actions must appear, got: {titles:?}"
    );
    assert!(
        titles.iter().any(|t| t.ends_with("— mock-b")),
        "mock-b's actions must appear (dropped under preferred), got: {titles:?}"
    );

    shutdown(&mut client);
}

#[test]
fn code_action_not_advertised_without_literal_support() {
    // A client without codeActionLiteralSupport only understands Command[]
    // responses, which the bridge cannot produce — the capability must be
    // withheld so the client never asks.
    let (mut client, init_response, _config_dir) = init_client(json!({}));
    assert_eq!(
        init_response["result"]["capabilities"]["codeActionProvider"],
        Value::Null,
        "codeActionProvider must be withheld without literal support"
    );
    // executeCommand is gated on the same condition — commands only reach the
    // bridge through a bridged code action, so it must be withheld too (pins
    // the gating expression, not just the capability's presence).
    assert_eq!(
        init_response["result"]["capabilities"]["executeCommandProvider"],
        Value::Null,
        "executeCommandProvider must be withheld without literal support"
    );
    shutdown(&mut client);
}

#[test]
fn command_action_surfaces_as_executable_with_a_routed_name() {
    let (mut client, init_response, _config_dir) = init_client(literal_support_caps(true));
    assert_advertised(&init_response);
    open_markdown(&mut client);

    let actions = code_action_with_retry(&mut client);

    assert_eq!(actions.len(), 2, "got: {actions:?}");
    let command = actions
        .iter()
        .find(|a| a["command"].is_string())
        .expect("the executable command action");
    assert_eq!(command["title"], "Run mock command — mock-codeaction");
    assert!(
        command["disabled"].is_null(),
        "an executable command is not disabled, got: {command:?}"
    );
    assert!(
        is_routed_command(
            command["command"].as_str().unwrap(),
            "mock-codeaction",
            "mock.run"
        ),
        "command name must encode the origin server, got: {command:?}"
    );

    shutdown(&mut client);
}

#[test]
fn executing_a_bridged_command_routes_back_and_relays_the_server_applyedit() {
    // The full PR 5 + PR 6 composition: the client executes a surfaced command;
    // the bridge decodes its routed name, forwards executeCommand to the origin
    // server with the ORIGINAL name; the server answers by asking the client to
    // apply an edit (host-translated by the bridge) and returns a result the
    // bridge relays verbatim.
    let (mut client, init_response, _config_dir) = init_client(literal_support_caps(true));
    assert_advertised(&init_response);
    open_markdown(&mut client);

    let actions = code_action_with_retry(&mut client);
    let routed = actions
        .iter()
        .find(|a| a["command"].is_string())
        .expect("the executable command action")["command"]
        .as_str()
        .expect("command name")
        .to_string();

    // Fire executeCommand without blocking — the server's applyEdit request
    // interleaves before the executeCommand response arrives.
    let exec_id = client.send_request_async(
        "workspace/executeCommand",
        json!({ "command": routed, "arguments": [] }),
    );

    // The server's applyEdit reaches the client, re-keyed to the host URI and
    // its virtual line 0 translated to host line 3.
    let (apply_id, apply_params) = client
        .wait_for_server_request("workspace/applyEdit", Duration::from_secs(5))
        .expect("the server's applyEdit must reach the client");
    let edits = &apply_params["edit"]["changes"][MARKDOWN_URI];
    assert!(
        edits.is_array(),
        "applyEdit must be re-keyed to the host URI, got: {apply_params:?}"
    );
    assert_eq!(edits[0]["range"]["start"]["line"], 3);
    assert_eq!(edits[0]["newText"], "executed");
    client.send_response(apply_id, json!({ "applied": true }));

    // The executeCommand result is relayed verbatim; the server saw its own
    // ORIGINAL command name (the routing prefix was stripped).
    let response = client.receive_response_for_id_public(exec_id);
    assert_eq!(
        response["result"]["executed"], "mock.run",
        "the origin server must receive its original command name, got: {response:?}"
    );

    shutdown(&mut client);
}

/// Issue codeAction over the lua fence line for the given client, retrying
/// while the result is null OR an empty array (cold downstream still
/// handshaking) until a non-empty action list arrives.
fn code_action_over_fence(client: &mut LspClient) -> Vec<Value> {
    for _ in 0..300 {
        let response = client.send_request(
            "textDocument/codeAction",
            json!({
                "textDocument": { "uri": MARKDOWN_URI },
                "range": {
                    "start": { "line": 3, "character": 0 },
                    "end": { "line": 3, "character": 5 }
                },
                "context": { "diagnostics": [] }
            }),
        );
        if let Some(actions) = response["result"].as_array()
            && !actions.is_empty()
        {
            return actions.clone();
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("textDocument/codeAction never returned actions");
}

#[test]
fn resolve_fails_soft_when_envelope_offset_diverges_from_live() {
    // The resolve path translates using the envelope's SNAPSHOT offset. If the
    // live region offset has diverged (e.g. an interior blockquote-prefix edit
    // changed a per-line column offset while the start held), translating with
    // the stale offset would bind the edit to wrong host columns. The freshness
    // gate re-resolves the live offset and compares the WHOLE thing, so a
    // divergence must fail soft (action returned unresolved, envelope intact).
    //
    // Simulated by tampering the envelope's `line_column_offsets` to a vector
    // that can't match the live single-line region (whose offset is `[0]`): a
    // start-only freshness check would still pass it (same start line/column).
    let (mut client, init_response, _config_dir) =
        init_client_mode("code-action-lazy", resolve_support_caps());
    assert_advertised(&init_response);
    open_markdown(&mut client);

    let actions = code_action_over_fence(&mut client);
    let mut tampered = actions[0].clone();
    // Live region offset is `[0]`; make the snapshot offset diverge on an
    // interior line the start-only check never looked at.
    tampered["data"]["kakehashi"]["offset"]["line_column_offsets"] = json!([0, 99]);

    let resolved = client.send_request("codeAction/resolve", tampered.clone());
    let resolved = &resolved["result"];
    assert!(
        resolved["edit"].is_null(),
        "a diverged snapshot offset must fail soft (no edit), got: {resolved:?}"
    );
    assert_eq!(
        resolved["data"]["kakehashi"]["origin"], "mock-codeaction",
        "the routing envelope is kept intact for a re-request, got: {resolved:?}"
    );

    shutdown(&mut client);
}

#[test]
fn lazy_action_is_resolved_via_code_action_resolve() {
    // A resolve-capable client gets the lazy action back with a routing
    // envelope; codeAction/resolve then materializes the edit, re-keyed to the
    // host document with the suffix preserved.
    let (mut client, init_response, _config_dir) =
        init_client_mode("code-action-lazy", resolve_support_caps());
    assert_advertised(&init_response);
    open_markdown(&mut client);

    let actions = code_action_over_fence(&mut client);
    assert_eq!(actions.len(), 1, "one lazy action, got: {actions:?}");
    let lazy = &actions[0];
    assert_eq!(lazy["title"], "Lazy organize imports — mock-codeaction");
    assert!(
        lazy["edit"].is_null(),
        "the action is still lazy (no edit yet), got: {lazy:?}"
    );
    // The routing envelope must be present under the kakehashi data key.
    assert_eq!(lazy["data"]["kakehashi"]["origin"], "mock-codeaction");
    assert_eq!(
        lazy["data"]["kakehashi"]["original_title"], "Lazy organize imports",
        "the unsuffixed title is stored for downstream title matching"
    );

    let resolved = client.send_request("codeAction/resolve", lazy.clone());
    let resolved = &resolved["result"];
    assert_eq!(
        resolved["title"], "Lazy organize imports — mock-codeaction",
        "the suffix is re-applied after resolve"
    );
    let edits = &resolved["edit"]["changes"][MARKDOWN_URI];
    assert!(
        edits.is_array(),
        "the resolved edit must be re-keyed to the host URI, got: {resolved:?}"
    );
    // Virtual line 0 = host line 3 (fence content line).
    assert_eq!(edits[0]["range"]["start"]["line"], 3);
    // The mock embeds the title it RECEIVED into newText. The bridge must have
    // restored the unsuffixed original before forwarding the resolve — if it
    // forwarded the suffixed title, this would be "organized:... — mock-codeaction".
    assert_eq!(edits[0]["newText"], "organized:Lazy organize imports");

    shutdown(&mut client);
}

#[test]
fn lazy_action_resolve_surfaces_server_changed_title() {
    // LSP lets a server rewrite the action title on codeAction/resolve. The
    // bridge must surface that NEW title (re-suffixed), not the pre-resolve
    // title it remembered for the suffix. This discriminates the fix: an
    // echo-only mock would look identical under the buggy "reuse suffixed_title"
    // path, so the mock deliberately returns a changed title here.
    let (mut client, init_response, _config_dir) =
        init_client_mode("code-action-lazy-retitle", resolve_support_caps());
    assert_advertised(&init_response);
    open_markdown(&mut client);

    let actions = code_action_over_fence(&mut client);
    assert_eq!(actions.len(), 1, "one lazy action, got: {actions:?}");
    let lazy = &actions[0];
    assert_eq!(lazy["title"], "Lazy organize imports — mock-codeaction");

    let resolved = client.send_request("codeAction/resolve", lazy.clone());
    let resolved = &resolved["result"];
    // The server changed the title to "Lazy organize imports (resolved)"; the
    // bridge must keep that change and re-suffix it — NOT fall back to the
    // remembered pre-resolve "Lazy organize imports — mock-codeaction".
    assert_eq!(
        resolved["title"], "Lazy organize imports (resolved) — mock-codeaction",
        "the server's resolve-time title change must be surfaced (re-suffixed)"
    );
    // The edit still materializes: the mock forwards the RECEIVED (unsuffixed)
    // title into newText, proving the round-trip restored the original title.
    let edits = &resolved["edit"]["changes"][MARKDOWN_URI];
    assert_eq!(edits[0]["newText"], "organized:Lazy organize imports");

    shutdown(&mut client);
}

#[test]
fn multistep_resolve_forwards_the_server_changed_title() {
    // A still-lazy resolve (no edit) that CHANGES the title must carry the new
    // title into the routing envelope, so a SECOND resolve forwards the title
    // the server last advertised — the match-by-title contract. The mock's
    // second resolve only materializes an edit when it receives the "(step2)"
    // title; if the bridge dropped the tracked title, the action stays lazy
    // forever and no edit ever appears.
    let (mut client, init_response, _config_dir) =
        init_client_mode("code-action-lazy-multistep", resolve_support_caps());
    assert_advertised(&init_response);
    open_markdown(&mut client);

    let actions = code_action_over_fence(&mut client);
    assert_eq!(actions.len(), 1, "one lazy action, got: {actions:?}");
    let lazy = &actions[0];
    assert_eq!(lazy["title"], "Lazy organize imports — mock-codeaction");

    // First resolve: the server stays lazy (no edit) but renames the action.
    let step1 = client.send_request("codeAction/resolve", lazy.clone());
    let step1 = &step1["result"];
    assert_eq!(
        step1["title"], "Lazy organize imports (step2) — mock-codeaction",
        "the server's step-1 title change must be surfaced"
    );
    assert!(
        step1["edit"].is_null(),
        "still lazy after step 1 (no edit yet), got: {step1:?}"
    );
    // The envelope must still be present so a second resolve routes back.
    assert_eq!(step1["data"]["kakehashi"]["origin"], "mock-codeaction");
    assert_eq!(
        step1["data"]["kakehashi"]["original_title"], "Lazy organize imports (step2)",
        "the envelope must track the server-changed title for the next resolve"
    );

    // Second resolve: the bridge must forward the tracked "(step2)" title, so
    // the mock now materializes the edit.
    let step2 = client.send_request("codeAction/resolve", step1.clone());
    let step2 = &step2["result"];
    let edits = &step2["edit"]["changes"][MARKDOWN_URI];
    assert!(
        edits.is_array(),
        "the second resolve must materialize the edit — proves the tracked \
         title reached the server, got: {step2:?}"
    );
    assert_eq!(
        edits[0]["newText"],
        "organized:Lazy organize imports (step2)"
    );

    shutdown(&mut client);
}

#[test]
fn lazy_action_resolving_to_untranslatable_edit_is_disabled() {
    // A lazy action that resolves to an edit the bridge cannot represent in the
    // host document (here a virtual-URI file op) is a PERMANENT failure —
    // re-requesting yields the same result. For a disabledSupport client the
    // bridge must return it `disabled` with a reason, NOT an enabled action
    // that applies nothing (checklist item 5; mirrors the initial path).
    let (mut client, init_response, _config_dir) =
        init_client_mode("code-action-lazy-fileop", resolve_support_caps());
    assert_advertised(&init_response);
    open_markdown(&mut client);

    let actions = code_action_over_fence(&mut client);
    assert_eq!(actions.len(), 1, "one lazy action, got: {actions:?}");
    let lazy = &actions[0];

    let resolved = client.send_request("codeAction/resolve", lazy.clone());
    let resolved = &resolved["result"];
    // The discriminating assertion: an enabled no-op and a disabled action are
    // indistinguishable to a test that only checks "no edit", so assert on
    // `disabled` + reason.
    assert_eq!(
        resolved["disabled"]["reason"], "the edit cannot be represented in the host document",
        "an untranslatable resolved edit must surface disabled, got: {resolved:?}"
    );
    // The disabled outcome must reflect the RESOLVE RESPONSE, not the pre-resolve
    // action: the server changed the title to "... (fileop)" and attached a
    // diagnostic on virtual line 0 — both must survive on the disabled action
    // (title re-suffixed, diagnostic host-translated to line 3).
    assert_eq!(
        resolved["title"], "Lazy organize imports (fileop) — mock-codeaction",
        "the server's resolve-time title change must survive on the disabled action, got: {resolved:?}"
    );
    assert_eq!(
        resolved["diagnostics"][0]["range"]["start"]["line"], 3,
        "the resolve's host-translated diagnostics must survive on the disabled action, got: {resolved:?}"
    );
    assert!(
        resolved["edit"].is_null(),
        "the unusable edit must be stripped, got: {resolved:?}"
    );
    assert!(
        resolved["data"].is_null(),
        "a disabled action carries no routing envelope, got: {resolved:?}"
    );

    shutdown(&mut client);
}

#[test]
fn lazy_action_resolving_out_of_region_is_disabled() {
    // A resolve whose edit range runs PAST the injected region would, once
    // translated by the region offset, land in host text after the fence —
    // buffer corruption. The bridge must bound the resolved edit to the region
    // and disable it (disabledSupport), never forward an out-of-region edit.
    let (mut client, init_response, _config_dir) =
        init_client_mode("code-action-lazy-oob", resolve_support_caps());
    assert_advertised(&init_response);
    open_markdown(&mut client);

    let actions = code_action_over_fence(&mut client);
    assert_eq!(actions.len(), 1, "one lazy action, got: {actions:?}");
    let lazy = &actions[0];

    let resolved = client.send_request("codeAction/resolve", lazy.clone());
    let resolved = &resolved["result"];
    assert_eq!(
        resolved["disabled"]["reason"], "the edit cannot be represented in the host document",
        "an out-of-region resolved edit must be disabled, not forwarded: {resolved:?}"
    );
    assert!(
        resolved["edit"].is_null(),
        "the out-of-region edit must be stripped, got: {resolved:?}"
    );

    shutdown(&mut client);
}

#[test]
fn lazy_action_is_eager_resolved_without_resolve_support() {
    // A client WITHOUT resolveSupport/dataSupport cannot resolve a lazy
    // action, so the bridge eager-resolves it downstream: the codeAction
    // response already carries the materialized, host-translated edit.
    let (mut client, init_response, _config_dir) =
        init_client_mode("code-action-lazy", literal_support_caps(false));
    assert_advertised(&init_response);
    open_markdown(&mut client);

    let actions = code_action_over_fence(&mut client);
    assert_eq!(actions.len(), 1, "got: {actions:?}");
    let action = &actions[0];
    assert_eq!(action["title"], "Lazy organize imports — mock-codeaction");
    let edits = &action["edit"]["changes"][MARKDOWN_URI];
    assert!(
        edits.is_array(),
        "eager-resolve must materialize the edit inline, got: {action:?}"
    );
    assert_eq!(edits[0]["range"]["start"]["line"], 3);
    // Eager-resolve forwards the raw (unsuffixed) title downstream; the mock
    // echoes it into newText, proving the materialization came from the
    // resolve round-trip with the correct title.
    assert_eq!(edits[0]["newText"], "organized:Lazy organize imports");
    assert!(
        action["data"].is_null(),
        "a client without dataSupport must not receive a routing envelope"
    );

    shutdown(&mut client);
}

#[test]
fn lazy_action_resolving_to_command_surfaces_it_routed() {
    // A lazy action whose resolve materializes a COMMAND (not an edit) is now
    // executable (#568 PR 6): the resolved action comes back with a routed
    // command name and no `data` (a command-complete action is not re-resolved).
    let (mut client, init_response, _config_dir) =
        init_client_mode("code-action-lazy-cmd", resolve_support_caps());
    assert_advertised(&init_response);
    open_markdown(&mut client);

    let actions = code_action_over_fence(&mut client);
    assert_eq!(actions.len(), 1, "one lazy action, got: {actions:?}");
    let lazy = &actions[0];
    assert_eq!(lazy["data"]["kakehashi"]["origin"], "mock-codeaction");

    let resolved = client.send_request("codeAction/resolve", lazy.clone());
    let resolved = &resolved["result"];
    assert!(
        is_routed_command(
            resolved["command"]["command"].as_str().unwrap_or_default(),
            "mock-codeaction",
            "mock.run"
        ),
        "the resolved command must carry a routed name, got: {resolved:?}"
    );
    assert!(
        resolved["edit"].is_null(),
        "a command-only resolve carries no edit, got: {resolved:?}"
    );
    assert!(
        resolved["data"].is_null(),
        "a command-complete action is not re-resolved (data stripped), got: {resolved:?}"
    );

    shutdown(&mut client);
}

#[test]
fn code_action_over_a_multi_fence_range_merges_actions_from_every_region() {
    // #628 multi-region: a codeAction range spanning TWO injected lua fences
    // bridges BOTH and merges their actions into one menu — previously only the
    // first fence was bridged.
    // Two lua fences: bodies on host lines 3 and 9.
    const TWO_FENCE: &str = "# A\n\n```lua\nlocal x = 1\n```\n\n# B\n\n```lua\nlocal y = 2\n```\n";
    let (mut client, init_response, _config_dir) = init_client(literal_support_caps(true));
    assert_advertised(&init_response);

    client.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": MARKDOWN_URI,
                "languageId": "markdown",
                "version": 1,
                "text": TWO_FENCE
            }
        }),
    );

    // Range covering BOTH fence bodies (host line 3 through line 9).
    let actions = (0..300)
        .find_map(|_| {
            let response = client.send_request(
                "textDocument/codeAction",
                json!({
                    "textDocument": { "uri": MARKDOWN_URI },
                    "range": {
                        "start": { "line": 3, "character": 0 },
                        "end": { "line": 9, "character": 5 }
                    },
                    "context": { "diagnostics": [] }
                }),
            );
            let actions = response["result"].as_array().cloned().unwrap_or_default();
            if actions.len() >= 4 {
                return Some(actions);
            }
            std::thread::sleep(Duration::from_millis(50));
            None
        })
        .expect("codeAction over a two-fence range returns actions from both fences");

    // Each fence contributes an edit action (targeting its own host line) plus a
    // bare command → 4 actions. The two edit actions target the two DIFFERENT
    // fences (host lines 3 and 9), proving both regions were bridged and merged.
    let edit_lines: Vec<u64> = actions
        .into_iter()
        .filter_map(|a| a["edit"]["changes"][MARKDOWN_URI][0]["range"]["start"]["line"].as_u64())
        .collect();
    assert!(
        edit_lines.contains(&3) && edit_lines.contains(&9),
        "edits must come from BOTH fences (host lines 3 and 9), got: {edit_lines:?}"
    );

    shutdown(&mut client);
}

#[test]
fn palette_fired_command_routes_to_its_origin_server() {
    // #628: a command the client fires WITHOUT an action context — a raw name
    // from the palette, keyed off the advertised executeCommandProvider.commands
    // — routes to the server that advertised it (recorded at handshake), not just
    // commands surfaced through a bridged code action.
    let (mut client, init_response, _config_dir) = init_client(literal_support_caps(true));
    assert_advertised(&init_response);
    open_markdown(&mut client);
    // Drive a codeAction so mock-codeaction handshakes and registers "mock.run".
    let _ = code_action_with_retry(&mut client);

    // Fire the RAW command name (palette style): no routing prefix, no action.
    let exec_id = client.send_request_async(
        "workspace/executeCommand",
        json!({ "command": "mock.run", "arguments": [] }),
    );
    let (apply_id, _apply_params) = client
        .wait_for_server_request("workspace/applyEdit", Duration::from_secs(5))
        .expect("the palette command must reach the origin server (which issues applyEdit)");
    client.send_response(apply_id, json!({ "applied": true }));

    let response = client.receive_response_for_id_public(exec_id);
    assert_eq!(
        response["result"]["executed"], "mock.run",
        "the palette command must reach its origin with the raw name, got: {response:?}"
    );

    shutdown(&mut client);
}

#[test]
fn downstream_command_names_are_registered_upstream_for_the_palette() {
    // #628: a downstream that advertises `executeCommandProvider.commands` has
    // those names dynamically registered with the editor via
    // `client/registerCapability`, so the palette lists them — gated on the
    // client advertising `workspace.executeCommand.dynamicRegistration`.
    let caps = json!({
        "textDocument": { "codeAction": {
            "codeActionLiteralSupport": { "codeActionKind": { "valueSet": [] } }
        }},
        "workspace": { "executeCommand": { "dynamicRegistration": true } }
    });
    let (mut client, init_response, _config_dir) = init_client(caps);
    assert_advertised(&init_response);
    // Opening the doc eager-spawns mock-codeaction (the lua fence's bridge
    // server); its handshake advertises `executeCommandProvider.commands`.
    open_markdown(&mut client);

    let (reg_id, reg_params) = client
        .wait_for_server_request("client/registerCapability", Duration::from_secs(5))
        .expect("the bridge must register the downstream's palette commands upstream");
    let registrations = reg_params["registrations"]
        .as_array()
        .expect("registrations array");
    let exec = registrations
        .iter()
        .find(|r| r["method"] == "workspace/executeCommand")
        .expect("a workspace/executeCommand registration");
    let commands = exec["registerOptions"]["commands"]
        .as_array()
        .expect("registerOptions.commands");
    assert!(
        commands.iter().any(|c| c == "mock.run"),
        "the mock's advertised command must be registered, got: {commands:?}"
    );
    client.send_response(reg_id, json!(null));

    shutdown(&mut client);
}
