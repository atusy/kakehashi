//! Code action request handling for bridge connections (#568, stage "PR 3").
//!
//! Edit-carrying actions only: `codeAction/resolve` and `Command` execution
//! are not bridged yet. Actions that would need them surface as
//! `disabled: { reason }` when the upstream client supports it and are
//! dropped otherwise (LSP 3.18 `disabledSupport`).
//!
//! Every bridged action title gets the `"{title} — {server}"` suffix so
//! users can see which downstream server each action comes from.

use std::io;

use crate::config::settings::BridgeServerConfig;
use tower_lsp_server::ls_types::{
    CodeAction, CodeActionContext, CodeActionDisabled, CodeActionOrCommand, CodeActionParams,
    CodeActionResponse, NumberOrString, PartialResultParams, Range, TextDocumentIdentifier, Uri,
    WorkDoneProgressParams,
};
use url::Url;

use super::super::pool::{LanguageServerPool, UpstreamId};
use super::super::protocol::{
    JsonRpcRequest, RegionOffset, RequestId, VirtualDocumentUri, host_position_within_region,
    response_has_jsonrpc_error, transform_workspace_edit_to_host, translate_host_range_to_virtual,
    translate_virtual_range_to_host, virtual_uri_to_lsp_uri,
};

impl LanguageServerPool {
    /// Send a code action request and wait for the response.
    ///
    /// The request's range and `context.diagnostics` ranges are translated
    /// host→virtual (diagnostic `data`/`source`/`code` stay byte-identical so
    /// the downstream can match them against what it published); the
    /// response's edits and action diagnostics are translated back.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn send_code_action_request(
        &self,
        server_name: &str,
        server_config: &BridgeServerConfig,
        host_uri: &Url,
        host_range: Range,
        context: CodeActionContext,
        injection_language: &str,
        region_id: &str,
        offset: RegionOffset,
        virtual_content: &str,
        upstream_request_id: Option<UpstreamId>,
        client_progress_token: Option<NumberOrString>,
        client_supports_disabled: bool,
    ) -> io::Result<Option<CodeActionResponse>> {
        let handle = self
            .get_or_create_connection(server_name, server_config, Some(host_uri))
            .await?;
        if !handle.has_capability("textDocument/codeAction") {
            return Ok(None);
        }
        self.execute_bridge_request_with_handle(
            handle,
            host_uri,
            injection_language,
            region_id,
            &offset,
            virtual_content,
            upstream_request_id,
            |virtual_uri, request_id| {
                build_code_action_request(
                    virtual_uri,
                    host_range,
                    context,
                    &offset,
                    request_id,
                    client_progress_token,
                )
            },
            |response, ctx| {
                transform_code_action_response_to_host(
                    response,
                    &ctx.virtual_uri_string,
                    ctx.host_uri_lsp,
                    ctx.offset,
                    server_name,
                    client_supports_disabled,
                )
            },
        )
        .await
    }
}

/// Build a JSON-RPC code action request for a downstream language server.
///
/// Translates the range AND every `context.diagnostics` range host→virtual;
/// `only` and `triggerKind` pass through untouched (dropping `only` would
/// kill save-time flows like `editor.codeActionsOnSave`).
fn build_code_action_request(
    virtual_uri: &VirtualDocumentUri,
    host_range: Range,
    mut context: CodeActionContext,
    offset: &RegionOffset,
    request_id: RequestId,
    client_progress_token: Option<NumberOrString>,
) -> JsonRpcRequest<CodeActionParams> {
    let mut virtual_range = host_range;
    translate_host_range_to_virtual(&mut virtual_range, offset);
    // Out-of-region diagnostics would saturate to virtual (0,0) and could
    // false-match a different diagnostic the server published at the top of
    // the virtual document — drop them instead of clamping.
    context
        .diagnostics
        .retain(|diagnostic| host_position_within_region(diagnostic.range.start, offset));
    for diagnostic in &mut context.diagnostics {
        translate_host_range_to_virtual(&mut diagnostic.range, offset);
    }

    let params = CodeActionParams {
        text_document: TextDocumentIdentifier {
            uri: virtual_uri_to_lsp_uri(virtual_uri),
        },
        range: virtual_range,
        context,
        work_done_progress_params: WorkDoneProgressParams {
            work_done_token: client_progress_token,
        },
        partial_result_params: PartialResultParams::default(),
    };
    JsonRpcRequest::new(request_id.as_i64(), "textDocument/codeAction", params)
}

/// Transform a code action response from virtual to host coordinates.
fn transform_code_action_response_to_host(
    mut response: serde_json::Value,
    request_virtual_uri: &str,
    host_uri: &Uri,
    offset: &RegionOffset,
    server_name: &str,
    client_supports_disabled: bool,
) -> Option<CodeActionResponse> {
    if response_has_jsonrpc_error(&response, "textDocument/codeAction") {
        return None;
    }
    let result = response.get_mut("result").map(serde_json::Value::take)?;
    if result.is_null() {
        return None;
    }
    let actions: CodeActionResponse = serde_json::from_value(result).ok()?;

    Some(bridge_code_actions(
        actions,
        server_name,
        client_supports_disabled,
        Some(&VirtLayerContext {
            request_virtual_uri,
            host_uri,
            offset,
        }),
    ))
}

/// Virtual-layer coordinate context for [`bridge_code_actions`]; `None`
/// means the host layer (real URIs and coordinates, nothing to translate).
pub(crate) struct VirtLayerContext<'a> {
    pub(crate) request_virtual_uri: &'a str,
    pub(crate) host_uri: &'a Uri,
    pub(crate) offset: &'a RegionOffset,
}

/// Apply the bridge policy to a downstream server's actions: title suffix,
/// command/lazy-action disabling, and (virt layer) coordinate translation.
pub(crate) fn bridge_code_actions(
    actions: Vec<CodeActionOrCommand>,
    server_name: &str,
    client_supports_disabled: bool,
    virt: Option<&VirtLayerContext<'_>>,
) -> Vec<CodeActionOrCommand> {
    actions
        .into_iter()
        .filter_map(|item| bridge_code_action(item, server_name, client_supports_disabled, virt))
        .collect()
}

const REASON_COMMANDS: &str = "kakehashi does not bridge command execution yet";
const REASON_RESOLVE: &str = "kakehashi does not bridge codeAction/resolve yet";
const REASON_CROSS_REGION: &str = "the edit cannot be represented in the host document";

fn bridge_code_action(
    item: CodeActionOrCommand,
    server_name: &str,
    client_supports_disabled: bool,
    virt: Option<&VirtLayerContext<'_>>,
) -> Option<CodeActionOrCommand> {
    let mut action = match item {
        // A bare Command cannot carry `disabled`; represent it as a disabled
        // CodeAction so the user still sees it exists (checklist: never
        // silently drop when the client can render why).
        CodeActionOrCommand::Command(command) => {
            return disabled_placeholder(
                command.title,
                REASON_COMMANDS,
                server_name,
                client_supports_disabled,
            );
        }
        CodeActionOrCommand::CodeAction(action) => action,
    };

    // A server-side disabled action is only representable with disabledSupport.
    if action.disabled.is_some() && !client_supports_disabled {
        return None;
    }

    // The action's own diagnostics came back in virtual coordinates.
    if let Some(virt) = virt
        && let Some(diagnostics) = &mut action.diagnostics
    {
        for diagnostic in diagnostics {
            translate_virtual_range_to_host(&mut diagnostic.range, virt.offset);
        }
    }

    // Commands are not executable until executeCommand is bridged; applying
    // only the edit half of an edit+command action would be worse than
    // disabling the whole action.
    if action.command.is_some() {
        return disable_action(
            action,
            REASON_COMMANDS,
            server_name,
            client_supports_disabled,
        );
    }

    match &mut action.edit {
        Some(edit) => {
            if let Some(virt) = virt
                && !transform_workspace_edit_to_host(
                    edit,
                    virt.request_virtual_uri,
                    virt.host_uri,
                    virt.offset,
                )
            {
                return disable_action(
                    action,
                    REASON_CROSS_REGION,
                    server_name,
                    client_supports_disabled,
                );
            }
        }
        None => {
            // No edit, no command: a lazy action that needs codeAction/resolve
            // (its payload hides behind `data`). We don't advertise resolve
            // yet, so it can never be completed.
            if action.data.is_some() {
                return disable_action(
                    action,
                    REASON_RESOLVE,
                    server_name,
                    client_supports_disabled,
                );
            }
        }
    }

    action.title = suffix_title(action.title, server_name);
    Some(CodeActionOrCommand::CodeAction(action))
}

/// `"{title} — {server}"`: applied to every bridged action, unconditionally.
fn suffix_title(title: String, server_name: &str) -> String {
    format!("{title} — {server_name}")
}

/// Turn an action the bridge cannot execute into a `disabled` entry: the
/// unusable payload (edit/command/data) is stripped so a client that applies
/// it anyway cannot act on untranslated coordinates.
fn disable_action(
    mut action: CodeAction,
    reason: &str,
    server_name: &str,
    client_supports_disabled: bool,
) -> Option<CodeActionOrCommand> {
    if !client_supports_disabled {
        return None;
    }
    action.title = suffix_title(action.title, server_name);
    action.edit = None;
    action.command = None;
    action.data = None;
    action.disabled = Some(CodeActionDisabled {
        reason: reason.to_string(),
    });
    Some(CodeActionOrCommand::CodeAction(action))
}

fn disabled_placeholder(
    title: String,
    reason: &str,
    server_name: &str,
    client_supports_disabled: bool,
) -> Option<CodeActionOrCommand> {
    if !client_supports_disabled {
        return None;
    }
    Some(CodeActionOrCommand::CodeAction(CodeAction {
        title: suffix_title(title, server_name),
        disabled: Some(CodeActionDisabled {
            reason: reason.to_string(),
        }),
        ..CodeAction::default()
    }))
}

#[cfg(test)]
mod tests {
    use super::super::test_helpers::*;
    use super::*;
    use serde_json::json;
    use tower_lsp_server::ls_types::{CodeActionKind, CodeActionTriggerKind, Diagnostic, Position};

    fn make_host_uri() -> Uri {
        crate::lsp::lsp_impl::url_to_uri(&Url::parse("file:///test.md").unwrap()).unwrap()
    }

    fn make_virtual_uri_string() -> String {
        VirtualDocumentUri::new(&make_host_uri(), "lua", "region-0").to_uri_string()
    }

    fn range(start_line: u32, end_line: u32) -> Range {
        Range {
            start: Position {
                line: start_line,
                character: 0,
            },
            end: Position {
                line: end_line,
                character: 5,
            },
        }
    }

    fn diagnostic_at(line: u32) -> Diagnostic {
        Diagnostic {
            range: range(line, line),
            message: "unused import".to_string(),
            code: Some(tower_lsp_server::ls_types::NumberOrString::String(
                "F401".to_string(),
            )),
            data: Some(json!({"fix": "remove"})),
            ..Diagnostic::default()
        }
    }

    // ==========================================================================
    // Request builder
    // ==========================================================================

    #[test]
    fn code_action_request_translates_range_and_diagnostics() {
        // Host line 5, region starts at line 3 → virtual line 2.
        let virtual_uri = VirtualDocumentUri::new(&test_host_uri(), "lua", "region-0");
        let context = CodeActionContext {
            diagnostics: vec![diagnostic_at(5)],
            only: Some(vec![CodeActionKind::SOURCE_ORGANIZE_IMPORTS]),
            trigger_kind: Some(CodeActionTriggerKind::INVOKED),
        };
        let request = build_code_action_request(
            &virtual_uri,
            range(5, 5),
            context,
            &RegionOffset::new(3, 0),
            RequestId::new(1),
            None,
        );

        assert_uses_virtual_uri(&request, "lua");
        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["params"]["range"]["start"]["line"], 2);
        let diag = &json["params"]["context"]["diagnostics"][0];
        assert_eq!(diag["range"]["start"]["line"], 2);
        // Identity fields must survive byte-identical for downstream matching.
        assert_eq!(diag["code"], "F401");
        assert_eq!(diag["data"], json!({"fix": "remove"}));
        // `only` + `triggerKind` pass through.
        assert_eq!(
            json["params"]["context"]["only"][0],
            "source.organizeImports"
        );
        assert_eq!(json["params"]["context"]["triggerKind"], 1);
    }

    #[test]
    fn code_action_request_drops_out_of_region_diagnostics() {
        // A diagnostic above the region would clamp to virtual (0,0) and
        // could false-match a different diagnostic the server published at
        // the top of the virtual document — it must be dropped, not clamped.
        let virtual_uri = VirtualDocumentUri::new(&test_host_uri(), "lua", "region-0");
        let context = CodeActionContext {
            diagnostics: vec![diagnostic_at(0), diagnostic_at(5)],
            ..CodeActionContext::default()
        };
        let request = build_code_action_request(
            &virtual_uri,
            range(5, 5),
            context,
            &RegionOffset::new(3, 0),
            RequestId::new(1),
            None,
        );

        let json = serde_json::to_value(&request).unwrap();
        let diags = json["params"]["context"]["diagnostics"].as_array().unwrap();
        assert_eq!(
            diags.len(),
            1,
            "the above-region diagnostic must be dropped"
        );
        assert_eq!(diags[0]["range"]["start"]["line"], 2, "5 - region start 3");
    }

    #[test]
    fn code_action_request_carries_work_done_token() {
        let virtual_uri = VirtualDocumentUri::new(&test_host_uri(), "lua", "region-0");
        let request = build_code_action_request(
            &virtual_uri,
            range(5, 5),
            CodeActionContext::default(),
            &RegionOffset::new(3, 0),
            test_request_id(),
            Some(NumberOrString::String("wd-1".to_string())),
        );
        assert_eq!(
            serde_json::to_value(&request).unwrap()["params"]["workDoneToken"],
            "wd-1"
        );
    }

    // ==========================================================================
    // Response transform
    // ==========================================================================

    fn transform(
        result: serde_json::Value,
        client_supports_disabled: bool,
    ) -> Option<CodeActionResponse> {
        let virtual_uri = make_virtual_uri_string();
        let host_uri = make_host_uri();
        transform_code_action_response_to_host(
            json!({"jsonrpc": "2.0", "id": 42, "result": result}),
            &virtual_uri,
            &host_uri,
            &RegionOffset::new(10, 0),
            "ruff",
            client_supports_disabled,
        )
    }

    fn edit_carrying_action(title: &str) -> serde_json::Value {
        json!({
            "title": title,
            "kind": "quickfix",
            "edit": {
                "changes": {
                    make_virtual_uri_string(): [{
                        "range": {
                            "start": { "line": 0, "character": 0 },
                            "end": { "line": 0, "character": 5 }
                        },
                        "newText": "fixed"
                    }]
                }
            }
        })
    }

    #[test]
    fn null_result_returns_none() {
        assert!(transform(serde_json::Value::Null, true).is_none());
    }

    #[test]
    fn edit_carrying_action_is_translated_and_suffixed() {
        let actions =
            transform(json!([edit_carrying_action("Remove unused import")]), true).unwrap();

        assert_eq!(actions.len(), 1);
        let CodeActionOrCommand::CodeAction(action) = &actions[0] else {
            panic!("Expected CodeAction");
        };
        assert_eq!(action.title, "Remove unused import — ruff");
        assert!(action.disabled.is_none());
        let changes = action.edit.as_ref().unwrap().changes.as_ref().unwrap();
        let edits = changes
            .get(&make_host_uri())
            .expect("edit must be re-keyed to the host URI");
        assert_eq!(edits[0].range.start.line, 10, "0 + region offset 10");
    }

    #[test]
    fn action_diagnostics_are_translated_back_to_host() {
        let mut action = edit_carrying_action("Fix");
        action["diagnostics"] = json!([{
            "range": {
                "start": { "line": 2, "character": 0 },
                "end": { "line": 2, "character": 5 }
            },
            "message": "unused"
        }]);
        let actions = transform(json!([action]), true).unwrap();
        let CodeActionOrCommand::CodeAction(action) = &actions[0] else {
            panic!("Expected CodeAction");
        };
        assert_eq!(
            action.diagnostics.as_ref().unwrap()[0].range.start.line,
            12,
            "2 + region offset 10"
        );
    }

    #[test]
    fn bare_command_becomes_disabled_placeholder() {
        let actions = transform(
            json!([{ "title": "Run organize imports", "command": "ruff.organizeImports" }]),
            true,
        )
        .unwrap();

        let CodeActionOrCommand::CodeAction(action) = &actions[0] else {
            panic!("Expected CodeAction placeholder");
        };
        assert_eq!(action.title, "Run organize imports — ruff");
        assert_eq!(action.disabled.as_ref().unwrap().reason, REASON_COMMANDS);
        assert!(action.command.is_none() && action.edit.is_none());
    }

    #[test]
    fn bare_command_is_dropped_without_disabled_support() {
        let actions = transform(
            json!([{ "title": "Run organize imports", "command": "ruff.organizeImports" }]),
            false,
        )
        .unwrap();
        assert!(actions.is_empty());
    }

    #[test]
    fn command_carrying_action_is_disabled_with_payload_stripped() {
        let mut action = edit_carrying_action("Fix all");
        action["command"] = json!({ "title": "post", "command": "ruff.postFix" });
        let actions = transform(json!([action]), true).unwrap();

        let CodeActionOrCommand::CodeAction(action) = &actions[0] else {
            panic!("Expected CodeAction");
        };
        assert_eq!(action.disabled.as_ref().unwrap().reason, REASON_COMMANDS);
        assert!(
            action.edit.is_none() && action.command.is_none(),
            "an edit+command action must not ship a half-appliable payload"
        );
        assert_eq!(action.title, "Fix all — ruff");
    }

    #[test]
    fn lazy_data_only_action_is_disabled() {
        let actions =
            transform(json!([{ "title": "Lazy fix", "data": { "id": 7 } }]), true).unwrap();
        let CodeActionOrCommand::CodeAction(action) = &actions[0] else {
            panic!("Expected CodeAction");
        };
        assert_eq!(action.disabled.as_ref().unwrap().reason, REASON_RESOLVE);
        assert!(action.data.is_none());
    }

    #[test]
    fn unrepresentable_edit_disables_the_action() {
        // A file op on the virtual URI makes the edit unrepresentable in host
        // coordinates (see workspace_edit.rs); the whole action is disabled
        // and the edit stripped.
        let action = json!({
            "title": "Extract to new file",
            "edit": {
                "documentChanges": [
                    { "kind": "create", "uri": make_virtual_uri_string() }
                ]
            }
        });
        let actions = transform(json!([action]), true).unwrap();
        let CodeActionOrCommand::CodeAction(action) = &actions[0] else {
            panic!("Expected CodeAction");
        };
        assert_eq!(
            action.disabled.as_ref().unwrap().reason,
            REASON_CROSS_REGION
        );
        assert!(action.edit.is_none(), "untranslated edit must not leak");
    }

    #[test]
    fn unrepresentable_edit_drops_action_without_disabled_support() {
        let action = json!({
            "title": "Extract to new file",
            "edit": {
                "documentChanges": [
                    { "kind": "create", "uri": make_virtual_uri_string() }
                ]
            }
        });
        let actions = transform(json!([action]), false).unwrap();
        assert!(actions.is_empty());
    }

    #[test]
    fn server_disabled_action_dropped_without_disabled_support() {
        let actions = transform(
            json!([{ "title": "Not applicable here", "disabled": { "reason": "wrong scope" } }]),
            false,
        )
        .unwrap();
        assert!(actions.is_empty());
    }

    #[test]
    fn host_layer_policy_keeps_edit_verbatim() {
        // Host layer (virt = None): real URIs/coordinates, no translation —
        // but the suffix and command policy still apply.
        let actions: Vec<CodeActionOrCommand> = serde_json::from_value(json!([
            {
                "title": "Sort imports",
                "edit": {
                    "changes": {
                        "file:///test.md": [{
                            "range": {
                                "start": { "line": 50, "character": 0 },
                                "end": { "line": 50, "character": 5 }
                            },
                            "newText": "sorted"
                        }]
                    }
                }
            },
            { "title": "Run linter", "command": "lint.run" }
        ]))
        .unwrap();

        let bridged = bridge_code_actions(actions, "marksman", true, None);
        assert_eq!(bridged.len(), 2);
        let CodeActionOrCommand::CodeAction(action) = &bridged[0] else {
            panic!("Expected CodeAction");
        };
        assert_eq!(action.title, "Sort imports — marksman");
        let changes = action.edit.as_ref().unwrap().changes.as_ref().unwrap();
        let edits = changes.get(&make_host_uri()).unwrap();
        assert_eq!(edits[0].range.start.line, 50, "host edits stay verbatim");
        let CodeActionOrCommand::CodeAction(placeholder) = &bridged[1] else {
            panic!("Expected disabled placeholder");
        };
        assert_eq!(
            placeholder.disabled.as_ref().unwrap().reason,
            REASON_COMMANDS
        );
    }
}
