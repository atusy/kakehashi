//! E2E (#427): a downstream virt-server's **spontaneous** `publishDiagnostics`
//! push reaches the editor in **host** coordinates, and a follow-up empty push
//! clears that source's contribution.
//!
//! The push path already has strong unit coverage (merge, transform, reader
//! routing); this proves the integrated flow end-to-end with a real downstream
//! that pushes on its own (not in response to a pull).
//!
//! Uses the `mock-lsp-formatter` binary in `diagnostics-push` mode: it pushes one
//! diagnostic on **virtual line 0** of each opened virtual doc, and pushes an empty
//! list on `didChange`.
//!
//! No-resurrection-after-close (a late push dropped once the editor closed the doc)
//! is covered by the `#421` push-accept-guard unit tests; an e2e for it would be a
//! flaky negative-timeout assertion, so it is intentionally left out here.
//!
//! A second test (`diagnostics-push-crash` mode) proves crash eviction (#469): the
//! downstream pushes a diagnostic, then exits; the bridge evicts that connection's
//! slots and republishes the host **cleared** — a positive (non-flaky) assertion.

#![cfg(feature = "e2e")]

mod helpers;

use std::time::Duration;

use helpers::lsp_client::LspClient;
use serde_json::{Value, json};

/// Path to the mock downstream server binary (built by Cargo for E2E runs).
fn mock_bin() -> &'static str {
    env!("CARGO_BIN_EXE_mock-lsp-formatter")
}

const MD_URI: &str = "file:///test_push_diagnostics.md";

/// `local x = 1` (the lua fence content) is host line 3 (0-indexed):
/// `0:"# Test"  1:""  2:"```lua"  3:"local x = 1"  4:"```"`. So the injected
/// virtual document's line 0 maps to host line 3.
const MD_TEXT: &str = "# Test\n\n```lua\nlocal x = 1\n```\n";
/// Same shape as `MD_TEXT` but the lua region's *content* differs (`local x = 2`),
/// so a `didChange` carrying it is NOT skipped by the content-fingerprint guard
/// (#422) and reaches the downstream mock.
const MD_TEXT_EDITED: &str = "# Test\n\n```lua\nlocal x = 2\n```\n";
const HOST_LINE: i64 = 3;

fn init_client() -> (LspClient, tempfile::TempDir) {
    init_client_with_mode("diagnostics-push")
}

fn init_client_with_mode(mode: &str) -> (LspClient, tempfile::TempDir) {
    init_client_with_mode_caps(mode, json!({}))
}

fn init_client_with_mode_caps(mode: &str, capabilities: Value) -> (LspClient, tempfile::TempDir) {
    let config_dir = tempfile::TempDir::new().expect("temp dir");
    let config_path = config_dir.path().join("push_diagnostics.toml");
    std::fs::write(&config_path, "").expect("write config");

    let mut client = LspClient::builder()
        .arg("--config-file")
        .arg(config_path.to_str().expect("utf8 path"))
        .build();

    client.send_request(
        "initialize",
        json!({
            "processId": std::process::id(),
            "rootUri": null,
            "capabilities": capabilities,
            "workspaceFolders": null,
            "initializationOptions": {
                "languageServers": {
                    "mock-push": { "cmd": [mock_bin(), mode], "languages": ["lua"] }
                }
            }
        }),
    );
    client.send_notification("initialized", json!({}));
    (client, config_dir)
}

/// True if `params` is a `publishDiagnostics` for the host doc that contains the
/// mock's pushed diagnostic.
fn has_pushed_diag(params: &Value) -> bool {
    params["uri"] == json!(MD_URI)
        && params["diagnostics"]
            .as_array()
            .is_some_and(|ds| ds.iter().any(is_mock_push))
}

/// True if `params` is a `publishDiagnostics` for the host doc that NO LONGER
/// contains the mock's pushed diagnostic (cleared).
fn cleared_host_diag(params: &Value) -> bool {
    params["uri"] == json!(MD_URI)
        && params["diagnostics"]
            .as_array()
            .is_some_and(|ds| !ds.iter().any(is_mock_push))
}

fn is_mock_push(d: &Value) -> bool {
    d["message"]
        .as_str()
        .is_some_and(|m| m.contains("mock-push-diag"))
}

#[test]
fn e2e_push_diagnostic_reaches_editor_in_host_coords_then_clears() {
    let (mut client, _config_dir) = init_client();

    client.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": MD_URI,
                "languageId": "markdown",
                "version": 1,
                "text": MD_TEXT
            }
        }),
    );

    // The mock pushes a diagnostic on virtual line 0 of the lua region; the bridge
    // translates it to host coordinates and publishes it for the host document.
    let (_method, params) = client
        .wait_for_notification_where(
            &["textDocument/publishDiagnostics"],
            Duration::from_secs(15),
            has_pushed_diag,
        )
        .expect("editor should receive the spontaneously pushed diagnostic for the host doc");

    let pushed = params["diagnostics"]
        .as_array()
        .expect("diagnostics array")
        .iter()
        .find(|d| is_mock_push(d))
        .expect("the mock-push diagnostic should be present");
    assert_eq!(
        pushed["range"]["start"]["line"],
        json!(HOST_LINE),
        "virtual line 0 must be translated to host line {HOST_LINE} (the lua fence content line)"
    );

    // A follow-up edit that CHANGES the lua region's content (`local x = 1` →
    // `local x = 2`) reaches the mock (the content-fingerprint guard only skips
    // *unchanged* content, #422); the mock pushes `[]` on didChange, clearing this
    // source's contribution for the host doc.
    client.send_notification(
        "textDocument/didChange",
        json!({
            "textDocument": { "uri": MD_URI, "version": 2 },
            "contentChanges": [{ "text": MD_TEXT_EDITED }]
        }),
    );

    let cleared = client.wait_for_notification_where(
        &["textDocument/publishDiagnostics"],
        Duration::from_secs(15),
        cleared_host_diag,
    );
    assert!(
        cleared.is_some(),
        "an empty push should clear the source's diagnostic for the host doc"
    );

    client.send_request("shutdown", json!(null));
    client.send_notification("exit", json!(null));
}

#[test]
fn e2e_pushfallback_folds_push_driven_server_into_client_pull() {
    // pushFallback (Path B, #425): a push-driven downstream (the `diagnostics-push`
    // mock advertises NO `diagnosticProvider`) contributes nothing to a live
    // `textDocument/diagnostic` fan-out — the capability gate skips it. Its cached
    // spontaneous push is instead folded into the client-pull response, in host
    // coordinates. Default `pushFallback = true`.
    let (mut client, _config_dir) = init_client();

    client.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": { "uri": MD_URI, "languageId": "markdown", "version": 1, "text": MD_TEXT }
        }),
    );

    // Wait for the spontaneous push so the slot is cached (it also reaches the editor).
    client
        .wait_for_notification_where(
            &["textDocument/publishDiagnostics"],
            Duration::from_secs(15),
            has_pushed_diag,
        )
        .expect("the push-driven mock should push on didOpen");

    // A client pull: the live fan-out skips the push-only mock (no
    // `diagnosticProvider`), so only `pushFallback` can surface its diagnostic.
    let response = client.send_request(
        "textDocument/diagnostic",
        json!({ "textDocument": { "uri": MD_URI } }),
    );
    let items = response["result"]["items"]
        .as_array()
        .expect("the pull answer is a full diagnostic report with an items array");
    let folded = items.iter().find(|d| is_mock_push(d)).expect(
        "pushFallback must fold the push-driven server's cached push into the client-pull response",
    );
    assert_eq!(
        folded["range"]["start"]["line"],
        json!(HOST_LINE),
        "the folded push must be transformed to host coordinates (host line {HOST_LINE})"
    );

    client.send_request("shutdown", json!(null));
    client.send_notification("exit", json!(null));
}

#[test]
fn e2e_pullfallback_false_still_publishes_a_pull_driven_server_spontaneous_push() {
    // #425 guarantee: `pullFallback = false` stops kakehashi from PULLING a
    // pull-driven server, but its spontaneous publishDiagnostics push must still
    // reach the editor (#380 stays closed). Regression guard for the
    // empty-PullLayer-suppresses-the-push bug: with no pull contributors the
    // host's PullLayer is EVICTED (not stored empty), so the merge's pull/push
    // dedup does not suppress the pull-driven server's cached push.
    let config_dir = tempfile::TempDir::new().expect("temp dir");
    let config_path = config_dir.path().join("pullfallback.toml");
    std::fs::write(&config_path, "").expect("write config");

    let mut client = LspClient::builder()
        .arg("--config-file")
        .arg(config_path.to_str().expect("utf8 path"))
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
                    "mock-pullcap": {
                        "cmd": [mock_bin(), "diagnostics-push-pullcap"],
                        "languages": ["lua"]
                    }
                },
                "languages": {
                    "markdown": {
                        "bridge": {
                            "lua": {
                                "aggregation": {
                                    "textDocument/publishDiagnostics": { "pullFallback": false }
                                }
                            }
                        }
                    }
                }
            }
        }),
    );
    client.send_notification("initialized", json!({}));

    client.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": { "uri": MD_URI, "languageId": "markdown", "version": 1, "text": MD_TEXT }
        }),
    );

    // The pull-driven mock pushes on didOpen. pullFallback = false means the
    // bridge never pulls it, so the only way its diagnostic reaches the editor is
    // the spontaneous push surviving the merge — at host line 3.
    let got = client.wait_for_notification_where(
        &["textDocument/publishDiagnostics"],
        Duration::from_secs(15),
        pushed_diag_at_line(HOST_LINE),
    );
    assert!(
        got.is_some(),
        "pullFallback = false must still publish a pull-driven server's spontaneous push \
         (an empty PullLayer must not suppress it)"
    );

    client.send_request("shutdown", json!(null));
    client.send_notification("exit", json!(null));
}

/// True if the host publish carries the mock's pushed diagnostic at host `line`.
fn pushed_diag_at_line(line: i64) -> impl Fn(&Value) -> bool {
    move |params: &Value| {
        params["uri"] == json!(MD_URI)
            && params["diagnostics"].as_array().is_some_and(|ds| {
                ds.iter()
                    .any(|d| is_mock_push(d) && d["range"]["start"]["line"] == json!(line))
            })
    }
}

#[test]
fn e2e_position_only_edit_reanchors_pushed_diagnostic_without_a_repush() {
    // The headline #422 flicker case: a host edit that moves a region without
    // changing its content must re-position the region's pushed diagnostic WITHOUT
    // re-analyzing it. The content-fingerprint guard skips the didChange to the mock
    // (so it never re-pushes — and never clears via its didChange "empty" push), and
    // the geometry re-merge re-anchors the cached slot to the new host line.
    let (mut client, _config_dir) = init_client_with_mode("diagnostics-push");

    client.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": { "uri": MD_URI, "languageId": "markdown", "version": 1, "text": MD_TEXT }
        }),
    );

    // The mock pushes one diagnostic on virtual line 0 → host line 3.
    client
        .wait_for_notification_where(
            &["textDocument/publishDiagnostics"],
            Duration::from_secs(15),
            pushed_diag_at_line(HOST_LINE),
        )
        .expect("editor should receive the pushed diagnostic at the original host line");

    // Insert a blank line ABOVE the fence: the lua region shifts from host line 3 to
    // 4, but its content (`local x = 1`) is unchanged. The mock is NOT driven (its
    // empty-on-didChange push never fires) because the region content didn't change.
    let shifted_text = format!("\n{MD_TEXT}");
    client.send_notification(
        "textDocument/didChange",
        json!({
            "textDocument": { "uri": MD_URI, "version": 2 },
            "contentChanges": [{ "text": shifted_text }]
        }),
    );

    // The cached diagnostic must reappear at the NEW host line (4) via geometry
    // re-merge — re-anchored, not re-pushed (a positive assertion; the diagnostic is
    // never lost, proving the region kept its identity across the position shift).
    let reanchored = client.wait_for_notification_where(
        &["textDocument/publishDiagnostics"],
        Duration::from_secs(15),
        pushed_diag_at_line(HOST_LINE + 1),
    );
    assert!(
        reanchored.is_some(),
        "a position-only host edit must re-anchor the pushed diagnostic to its new host line without a re-push"
    );

    // A SECOND position-only edit shifts the region to line 5. This checks the
    // diagnostic *persisted* (was not cleared by the first edit): the content guard
    // must keep the first edit's didChange from reaching the mock — otherwise the
    // mock's empty-on-didChange push would empty this region's slot, and the
    // re-anchor would carry no diagnostic to line 5. (Not a hermetic proof that no
    // didChange was sent — a late empty push could still race — but a strong guard
    // that the steady state is the diagnostic re-anchored, not cleared.)
    let shifted_text_2 = format!("\n\n{MD_TEXT}");
    client.send_notification(
        "textDocument/didChange",
        json!({
            "textDocument": { "uri": MD_URI, "version": 3 },
            "contentChanges": [{ "text": shifted_text_2 }]
        }),
    );
    let reanchored_again = client.wait_for_notification_where(
        &["textDocument/publishDiagnostics"],
        Duration::from_secs(15),
        pushed_diag_at_line(HOST_LINE + 2),
    );
    assert!(
        reanchored_again.is_some(),
        "the diagnostic must survive the first edit and re-anchor again — proving it was not cleared"
    );

    client.send_request("shutdown", json!(null));
    client.send_notification("exit", json!(null));
}

#[test]
fn e2e_downstream_crash_evicts_its_pushed_diagnostics() {
    let (mut client, _config_dir) = init_client_with_mode("diagnostics-push-crash");

    client.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": MD_URI,
                "languageId": "markdown",
                "version": 1,
                "text": MD_TEXT
            }
        }),
    );

    // The crash mock pushes one diagnostic on didOpen; the editor receives it.
    client
        .wait_for_notification_where(
            &["textDocument/publishDiagnostics"],
            Duration::from_secs(15),
            has_pushed_diag,
        )
        .expect("editor should receive the pushed diagnostic before the crash");

    // A content-changing edit (`local x = 1` → `local x = 2`) reaches the mock — the
    // fingerprint guard only skips *unchanged* content (#422) — driving it to exit
    // (crash) while the host stays open. The bridge's reader sees EOF, evicts that
    // connection's slots, and republishes the host cleared — a positive assertion (no
    // negative timeout): we wait for the publish that no longer carries the dead
    // server's diagnostic (#469).
    client.send_notification(
        "textDocument/didChange",
        json!({
            "textDocument": { "uri": MD_URI, "version": 2 },
            "contentChanges": [{ "text": MD_TEXT_EDITED }]
        }),
    );

    let cleared = client.wait_for_notification_where(
        &["textDocument/publishDiagnostics"],
        Duration::from_secs(15),
        cleared_host_diag,
    );
    assert!(
        cleared.is_some(),
        "a crashed downstream's pushed diagnostics must be evicted and the host republished cleared"
    );

    client.send_request("shutdown", json!(null));
    client.send_notification("exit", json!(null));
}

/// Client capabilities advertising pull-diagnostic refresh support, so the bridge
/// will send `workspace/diagnostic/refresh` (it's gated on this; an editor that
/// doesn't advertise it is never sent the request).
fn refresh_capable_caps() -> Value {
    json!({ "workspace": { "diagnostics": { "refreshSupport": true } } })
}

#[test]
fn e2e_downstream_crash_refreshes_pull_clients() {
    // #499: when a downstream server crash evicts its diagnostics and clears the
    // host, a pull-mode editor (which displays what it *pulled*, not our push) has
    // no event to learn the diagnostics vanished — so the bridge must nudge it with
    // `workspace/diagnostic/refresh`. With a refresh-capable client this asserts
    // the wire path the unit test can't reach (server→client requests are
    // suppressed until `initialize`, which only the spawned-server e2e harness drives).
    let (mut client, _config_dir) =
        init_client_with_mode_caps("diagnostics-push-crash", refresh_capable_caps());

    client.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": MD_URI,
                "languageId": "markdown",
                "version": 1,
                "text": MD_TEXT
            }
        }),
    );

    // The spontaneous push changes the merged set, driving BOTH a refresh
    // (push/pull-divergence, #422) and a publish. The publish can lag the
    // refresh — the wire quiet-window may defer it to the trailing flush — so
    // wait for the refresh while collecting publishes, then make sure the
    // pushed publish arrived (from the collected ones or a fresh wait). Ack
    // the refresh: the bridge single-flights it (#497), so the crash-eviction
    // refresh below won't be sent until this one is answered.
    let (push_refresh_id, _, publishes) = client
        .wait_for_server_request_watching(
            "workspace/diagnostic/refresh",
            Duration::from_secs(15),
            &["textDocument/publishDiagnostics"],
        )
        .expect("the spontaneous push must drive a workspace/diagnostic/refresh (#422)");
    client.send_response(push_refresh_id, json!(null));
    if !publishes.iter().any(|(_, params)| has_pushed_diag(params)) {
        client
            .wait_for_notification_where(
                &["textDocument/publishDiagnostics"],
                Duration::from_secs(15),
                has_pushed_diag,
            )
            .expect("editor should receive the pushed diagnostic before the crash");
    }

    // The content-changing edit reaches the mock, driving it to exit (crash). The
    // bridge's reader sees EOF, evicts the dead connection's slots, and republishes
    // the host cleared.
    client.send_notification(
        "textDocument/didChange",
        json!({
            "textDocument": { "uri": MD_URI, "version": 2 },
            "contentChanges": [{ "text": MD_TEXT_EDITED }]
        }),
    );

    // The eviction changed the merged set with no event the editor could see,
    // so the bridge must emit a second `workspace/diagnostic/refresh` (#499),
    // and the host must be republished cleared — again in either order (the
    // cleared publish may ride the trailing quiet-window flush).
    let (evict_refresh_id, _, publishes) = client
        .wait_for_server_request_watching(
            "workspace/diagnostic/refresh",
            Duration::from_secs(15),
            &["textDocument/publishDiagnostics"],
        )
        .expect(
            "a crash eviction that clears the host must drive a workspace/diagnostic/refresh (#499)",
        );
    client.send_response(evict_refresh_id, json!(null));
    if !publishes
        .iter()
        .any(|(_, params)| cleared_host_diag(params))
    {
        client
            .wait_for_notification_where(
                &["textDocument/publishDiagnostics"],
                Duration::from_secs(15),
                cleared_host_diag,
            )
            .expect("the crash must evict the dead server's diagnostic and republish cleared");
    }

    client.send_request("shutdown", json!(null));
    client.send_notification("exit", json!(null));
}

#[test]
fn e2e_publish_seal_via_language_wildcard_reaches_configured_languages() {
    // The `languages._` wildcard's layers are field-merged into every
    // configured language by Phase 2 base resolution (resolve_base_configs),
    // so a wildcard seal reaches the markdown host even though this config
    // also has explicit language entries via built-in defaults. Same shape as
    // the exact-language seal e2e, minus the follow-up pull.
    let config_dir = tempfile::TempDir::new().expect("temp dir");
    let config_path = config_dir.path().join("publish_seal_wildcard.toml");
    std::fs::write(&config_path, "").expect("write config");

    let mut client = LspClient::builder()
        .arg("--config-file")
        .arg(config_path.to_str().expect("utf8 path"))
        .build();

    client.send_request(
        "initialize",
        json!({
            "processId": std::process::id(),
            "rootUri": null,
            "capabilities": { "workspace": { "diagnostics": { "refreshSupport": true } } },
            "workspaceFolders": null,
            "initializationOptions": {
                "languageServers": {
                    "mock-push": { "cmd": [mock_bin(), "diagnostics-push"], "languages": ["lua"] }
                },
                "languages": {
                    "_": {
                        "layers": {
                            "aggregation": {
                                "textDocument/publishDiagnostics": { "priorities": [] }
                            }
                        }
                    }
                }
            }
        }),
    );
    client.send_notification("initialized", json!({}));

    client.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": { "uri": MD_URI, "languageId": "markdown", "version": 1, "text": MD_TEXT }
        }),
    );

    let (refresh_id, _, publishes) = client
        .wait_for_server_request_watching(
            "workspace/diagnostic/refresh",
            Duration::from_secs(15),
            &["textDocument/publishDiagnostics"],
        )
        .expect("a wildcard-sealed setup must still drive workspace/diagnostic/refresh");
    let host_publishes: Vec<_> = publishes
        .iter()
        .filter(|(_, params)| params["uri"] == json!(MD_URI))
        .collect();
    assert!(
        host_publishes.is_empty(),
        "a languages._ seal must reach the markdown host (got {host_publishes:?})"
    );
    client.send_response(refresh_id, json!(null));

    client.send_request("shutdown", json!(null));
    client.send_notification("exit", json!(null));
}

#[test]
fn e2e_didclose_clears_promptly_even_within_the_quiet_window() {
    // The regression this pins: a clearing push withheld by the quiet window
    // must not be dropped when the document closes — didClose's clearing
    // publish bypasses the window and the withheld `dirty` debt forces it past
    // the unchanged-skip, so a push-mode editor never keeps a closed buffer's
    // diagnostics. Positive assertion: an empty publish for the host arrives
    // regardless of whether the mock's clearing push raced the close.
    let (mut client, _config_dir) = init_client();

    client.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": { "uri": MD_URI, "languageId": "markdown", "version": 1, "text": MD_TEXT }
        }),
    );
    client
        .wait_for_notification_where(
            &["textDocument/publishDiagnostics"],
            Duration::from_secs(15),
            has_pushed_diag,
        )
        .expect("editor should receive the pushed diagnostic");

    // The edit drives the mock's clearing `[]` push; within 1 s of the last
    // publish it is withheld by the quiet window. Closing right away must
    // still clear the editor.
    client.send_notification(
        "textDocument/didChange",
        json!({
            "textDocument": { "uri": MD_URI, "version": 2 },
            "contentChanges": [{ "text": MD_TEXT_EDITED }]
        }),
    );
    client.send_notification(
        "textDocument/didClose",
        json!({ "textDocument": { "uri": MD_URI } }),
    );

    let cleared = client.wait_for_notification_where(
        &["textDocument/publishDiagnostics"],
        Duration::from_secs(15),
        cleared_host_diag,
    );
    assert!(
        cleared.is_some(),
        "closing within the quiet window must still deliver a clearing publish"
    );

    client.send_request("shutdown", json!(null));
    client.send_notification("exit", json!(null));
}

#[test]
fn e2e_pull_result_id_roundtrip_reports_unchanged() {
    // LSP 3.17 pull diagnostics: the full report carries a resultId; a re-pull
    // echoing it as previousResultId while the set is unchanged is answered
    // with an `unchanged` report (no items re-shipped — on a diagnostics-heavy
    // host the full report is ~1 MB, and every refresh-induced re-pull of a
    // settled set previously re-shipped all of it). A content change makes the
    // next pull full again, with a different id.
    let (mut client, _config_dir) = init_client();

    client.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": { "uri": MD_URI, "languageId": "markdown", "version": 1, "text": MD_TEXT }
        }),
    );

    // Wait for the spontaneous push so the folded set is stable across the two
    // pulls below.
    client
        .wait_for_notification_where(
            &["textDocument/publishDiagnostics"],
            Duration::from_secs(15),
            has_pushed_diag,
        )
        .expect("the push-driven mock should push on didOpen");

    let first = client.send_request(
        "textDocument/diagnostic",
        json!({ "textDocument": { "uri": MD_URI } }),
    );
    assert_eq!(first["result"]["kind"], "full");
    let result_id = first["result"]["resultId"]
        .as_str()
        .expect("a full pull report carries a resultId")
        .to_string();

    // Same set + matching previousResultId → unchanged, same id, no items.
    let second = client.send_request(
        "textDocument/diagnostic",
        json!({
            "textDocument": { "uri": MD_URI },
            "previousResultId": result_id
        }),
    );
    assert_eq!(
        second["result"]["kind"], "unchanged",
        "an unchanged set answered against a matching previousResultId re-ships nothing"
    );
    assert_eq!(second["result"]["resultId"], json!(result_id));
    assert!(second["result"].get("items").is_none());

    // A content change (the mock clears on didChange) invalidates the id: the
    // next pull is full again with a different id. Wait for the cleared
    // publish first so the cache settled.
    client.send_notification(
        "textDocument/didChange",
        json!({
            "textDocument": { "uri": MD_URI, "version": 2 },
            "contentChanges": [{ "text": MD_TEXT_EDITED }]
        }),
    );
    client
        .wait_for_notification_where(
            &["textDocument/publishDiagnostics"],
            Duration::from_secs(15),
            cleared_host_diag,
        )
        .expect("the mock's empty push should clear the host set");

    let third = client.send_request(
        "textDocument/diagnostic",
        json!({
            "textDocument": { "uri": MD_URI },
            "previousResultId": result_id
        }),
    );
    assert_eq!(
        third["result"]["kind"], "full",
        "a changed set must be answered full even with a previousResultId"
    );
    assert_ne!(
        third["result"]["resultId"],
        json!(result_id),
        "the id is content-addressed, so a changed set gets a new one"
    );

    client.send_request("shutdown", json!(null));
    client.send_notification("exit", json!(null));
}

#[test]
fn e2e_publish_seal_delivers_via_refresh_and_pull_only() {
    // The cross-layer gate for `textDocument/publishDiagnostics` doubles as a
    // wire seal: `priorities = []` stops the proactive publishes entirely (a
    // pull-first editor setup ignores them or renders them as a duplicate
    // namespace; measured at ~1 MB × dozens per typing burst on a
    // diagnostics-heavy host) while delivery continues via
    // `workspace/diagnostic/refresh` → client re-pull. Deterministic ordering:
    // a publish (were the seal broken) would be a host's FIRST — which the
    // wire quiet-window never defers (leading edge) — so it would be enqueued
    // strictly before the refresh the same set-change drives; collecting
    // publishes while waiting for the refresh therefore proves the seal
    // without a negative timeout.
    let config_dir = tempfile::TempDir::new().expect("temp dir");
    let config_path = config_dir.path().join("publish_seal.toml");
    std::fs::write(&config_path, "").expect("write config");

    let mut client = LspClient::builder()
        .arg("--config-file")
        .arg(config_path.to_str().expect("utf8 path"))
        .build();

    client.send_request(
        "initialize",
        json!({
            "processId": std::process::id(),
            "rootUri": null,
            "capabilities": { "workspace": { "diagnostics": { "refreshSupport": true } } },
            "workspaceFolders": null,
            "initializationOptions": {
                "languageServers": {
                    "mock-push": { "cmd": [mock_bin(), "diagnostics-push"], "languages": ["lua"] }
                },
                "languages": {
                    "markdown": {
                        "layers": {
                            "aggregation": {
                                "textDocument/publishDiagnostics": { "priorities": [] }
                            }
                        }
                    }
                }
            }
        }),
    );
    client.send_notification("initialized", json!({}));

    client.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": { "uri": MD_URI, "languageId": "markdown", "version": 1, "text": MD_TEXT }
        }),
    );

    // The mock's spontaneous push changes the merged set → the refresh nudge
    // still fires (it is the sealed setup's delivery signal) — and no
    // publishDiagnostics for the host may precede it.
    let (refresh_id, _, publishes) = client
        .wait_for_server_request_watching(
            "workspace/diagnostic/refresh",
            Duration::from_secs(15),
            &["textDocument/publishDiagnostics"],
        )
        .expect("a sealed setup must still drive workspace/diagnostic/refresh on a push");
    let host_publishes: Vec<_> = publishes
        .iter()
        .filter(|(_, params)| params["uri"] == json!(MD_URI))
        .collect();
    assert!(
        host_publishes.is_empty(),
        "the seal must stop publishDiagnostics for the host (got {host_publishes:?})"
    );
    client.send_response(refresh_id, json!(null));

    // Delivery works through the pull: the push-only mock's cached push is
    // folded into the client-pull response in host coordinates (pushFallback).
    let response = client.send_request(
        "textDocument/diagnostic",
        json!({ "textDocument": { "uri": MD_URI } }),
    );
    let items = response["result"]["items"]
        .as_array()
        .expect("the pull answer is a full diagnostic report with an items array");
    let pulled = items
        .iter()
        .find(|d| is_mock_push(d))
        .expect("the pushed diagnostic must be deliverable through the client pull");
    assert_eq!(
        pulled["range"]["start"]["line"],
        json!(HOST_LINE),
        "the pulled diagnostic must be in host coordinates (host line {HOST_LINE})"
    );

    client.send_request("shutdown", json!(null));
    client.send_notification("exit", json!(null));
}

/// Send a `didOpen` for the markdown host (with its lua region) so the bridge
/// spawns the downstream mock for that region and forwards the region's events.
fn open_host(client: &mut LspClient) {
    client.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": MD_URI,
                "languageId": "markdown",
                "version": 1,
                "text": MD_TEXT
            }
        }),
    );
}

#[test]
fn e2e_downstream_refresh_forwarded_to_refresh_capable_client() {
    // #521: a downstream server's own `workspace/diagnostic/refresh` request is
    // forwarded upstream to the editor. With a refresh-capable client the bridge
    // relays it (the `diagnostics-refresh` mock fires one on didOpen).
    let (mut client, _config_dir) =
        init_client_with_mode_caps("diagnostics-refresh", refresh_capable_caps());
    open_host(&mut client);

    assert!(
        client
            .wait_for_server_request("workspace/diagnostic/refresh", Duration::from_secs(15))
            .is_some(),
        "a downstream's workspace/diagnostic/refresh must reach a refresh-capable editor (#521)"
    );

    client.send_request("shutdown", json!(null));
    client.send_notification("exit", json!(null));
}

#[test]
fn e2e_downstream_refresh_burst_is_coalesced() {
    let (mut client, _config_dir) =
        init_client_with_mode_caps("diagnostics-refresh-burst", refresh_capable_caps());
    open_host(&mut client);

    let (id, _) = client
        .wait_for_server_request("workspace/diagnostic/refresh", Duration::from_secs(15))
        .expect("a downstream refresh burst must eventually reach the editor");
    client.send_response(id, json!(null));
    assert!(
        client
            .wait_for_server_request("workspace/diagnostic/refresh", Duration::from_millis(1200))
            .is_none(),
        "one burst must produce at most one editor-visible refresh"
    );

    client.send_request("shutdown", json!(null));
    client.send_notification("exit", json!(null));
}

#[test]
fn e2e_downstream_refresh_gated_off_for_refresh_incapable_client() {
    // #521: the same downstream refresh must NOT be forwarded when the client did
    // not advertise `workspace.diagnostics.refreshSupport` — forwarding it would
    // leak a tower-lsp pending-request entry on a client that silently ignores it.
    // Empty capabilities (`init_client_with_mode`) → refresh unsupported. Paired
    // with the positive test above (same mock mode), so a "no refresh" result here
    // is the gate working, not the mock failing to emit.
    let (mut client, _config_dir) = init_client_with_mode("diagnostics-refresh");
    open_host(&mut client);

    assert!(
        client
            .wait_for_server_request("workspace/diagnostic/refresh", Duration::from_secs(6))
            .is_none(),
        "the refresh forward must be gated off for a client without refreshSupport (#521)"
    );

    client.send_request("shutdown", json!(null));
    client.send_notification("exit", json!(null));
}
