//! LSP lifecycle message builders.
//!
//! Provides builders for initialize, shutdown, and exit messages
//! used during connection lifecycle management.

use tower_lsp_server::ls_types::ClientCapabilities;

use super::request_id::RequestId;

/// Build the baseline client capabilities the bridge declares to downstream servers.
///
/// Returns typed `ClientCapabilities` for use with [`merge_upstream_capabilities`].
fn build_baseline_capabilities() -> ClientCapabilities {
    use tower_lsp_server::ls_types::{
        CompletionClientCapabilities, CompletionItemCapability, DiagnosticClientCapabilities,
        DiagnosticWorkspaceClientCapabilities, DocumentLinkClientCapabilities,
        DocumentSymbolClientCapabilities, DynamicRegistrationClientCapabilities,
        GeneralClientCapabilities, GotoCapability, HoverClientCapabilities,
        InlayHintClientCapabilities, PositionEncodingKind, SignatureHelpClientCapabilities,
        TextDocumentClientCapabilities, WorkspaceClientCapabilities,
    };

    let goto_link = Some(GotoCapability {
        dynamic_registration: Some(false),
        link_support: Some(true),
    });

    #[allow(unused_mut)] // mutated only with "experimental" feature
    let mut text_document = TextDocumentClientCapabilities {
        hover: Some(HoverClientCapabilities {
            dynamic_registration: Some(false),
            ..Default::default()
        }),
        completion: Some(CompletionClientCapabilities {
            dynamic_registration: Some(false),
            completion_item: Some(CompletionItemCapability {
                insert_replace_support: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        }),
        definition: goto_link,
        type_definition: goto_link,
        implementation: goto_link,
        declaration: goto_link,
        references: Some(DynamicRegistrationClientCapabilities {
            dynamic_registration: Some(false),
        }),
        signature_help: Some(SignatureHelpClientCapabilities {
            dynamic_registration: Some(false),
            ..Default::default()
        }),
        document_highlight: Some(DynamicRegistrationClientCapabilities {
            dynamic_registration: Some(false),
        }),
        document_symbol: Some(DocumentSymbolClientCapabilities {
            dynamic_registration: Some(false),
            hierarchical_document_symbol_support: Some(true),
            ..Default::default()
        }),
        document_link: Some(DocumentLinkClientCapabilities {
            dynamic_registration: Some(false),
            tooltip_support: Some(true),
        }),
        inlay_hint: Some(InlayHintClientCapabilities {
            dynamic_registration: Some(false),
            ..Default::default()
        }),
        diagnostic: Some(DiagnosticClientCapabilities {
            dynamic_registration: Some(true),
            related_document_support: Some(true),
        }),
        moniker: Some(DynamicRegistrationClientCapabilities {
            dynamic_registration: Some(false),
        }),
        ..Default::default()
    };

    #[cfg(feature = "experimental")]
    {
        text_document.color_provider = Some(DynamicRegistrationClientCapabilities {
            dynamic_registration: Some(false),
        });
    }

    ClientCapabilities {
        text_document: Some(text_document),
        workspace: Some(WorkspaceClientCapabilities {
            diagnostics: Some(DiagnosticWorkspaceClientCapabilities {
                refresh_support: Some(true),
            }),
            ..Default::default()
        }),
        general: Some(GeneralClientCapabilities {
            position_encodings: Some(vec![PositionEncodingKind::UTF16]),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Merge upstream client capabilities into the bridge baseline.
///
/// # Capability categories
///
/// **Category A — Bridge-controlled** (never overridden):
/// - `general.positionEncodings` — bridge requires UTF-16
/// - `insertReplaceSupport` — bridge transforms insert/replace edits
/// - All `linkSupport` fields — bridge transforms `LocationLink` → `Location`
/// - `hierarchicalDocumentSymbolSupport` — bridge transforms symbol responses
/// - All `dynamicRegistration` fields — bridge manages registration lifecycle
///
/// **Category B — Pass-through** (propagated from upstream when present):
/// - `completionItem`: `documentationFormat`, `snippetSupport`, `deprecatedSupport`,
///   `tagSupport`, `commitCharactersSupport`, `resolveSupport`,
///   `insertTextModeSupport`, `labelDetailsSupport`, `preselectSupport`
/// - `hover.contentFormat`
/// - `signatureHelp.signatureInformation`
///
/// # Merge semantics
///
/// - **Replace**: upstream value fully replaces bridge default (preference order matters per LSP)
/// - **Fallback**: when upstream is `None`, bridge default is kept
fn merge_upstream_capabilities(
    mut base: ClientCapabilities,
    upstream: Option<&ClientCapabilities>,
) -> ClientCapabilities {
    let Some(upstream) = upstream else {
        return base;
    };

    // Helper: replace base option with upstream if upstream is Some
    fn merge_option<T>(base: &mut Option<T>, upstream: Option<T>) {
        if upstream.is_some() {
            *base = upstream;
        }
    }

    // --- Completion item fields (Category B) ---
    if let Some(upstream_td) = &upstream.text_document {
        let base_td = base.text_document.get_or_insert_with(Default::default);

        if let Some(upstream_completion) = &upstream_td.completion {
            let base_completion = base_td.completion.get_or_insert_with(Default::default);

            if let Some(upstream_item) = &upstream_completion.completion_item {
                let base_item = base_completion
                    .completion_item
                    .get_or_insert_with(Default::default);

                merge_option(
                    &mut base_item.documentation_format,
                    upstream_item.documentation_format.clone(),
                );
                merge_option(
                    &mut base_item.snippet_support,
                    upstream_item.snippet_support,
                );
                merge_option(
                    &mut base_item.deprecated_support,
                    upstream_item.deprecated_support,
                );
                merge_option(
                    &mut base_item.tag_support,
                    upstream_item.tag_support.clone(),
                );
                merge_option(
                    &mut base_item.commit_characters_support,
                    upstream_item.commit_characters_support,
                );
                merge_option(
                    &mut base_item.resolve_support,
                    upstream_item.resolve_support.clone(),
                );
                merge_option(
                    &mut base_item.insert_text_mode_support,
                    upstream_item.insert_text_mode_support.clone(),
                );
                merge_option(
                    &mut base_item.label_details_support,
                    upstream_item.label_details_support,
                );
                merge_option(
                    &mut base_item.preselect_support,
                    upstream_item.preselect_support,
                );
            }
        }

        // --- Hover contentFormat (Category B) ---
        if let Some(upstream_hover) = &upstream_td.hover {
            let base_hover = base_td.hover.get_or_insert_with(Default::default);
            merge_option(
                &mut base_hover.content_format,
                upstream_hover.content_format.clone(),
            );
        }

        // --- SignatureHelp signatureInformation (Category B) ---
        if let Some(upstream_sig) = &upstream_td.signature_help {
            let base_sig = base_td.signature_help.get_or_insert_with(Default::default);
            merge_option(
                &mut base_sig.signature_information,
                upstream_sig.signature_information.clone(),
            );
        }
    }

    base
}

/// Build the client capabilities the bridge declares to downstream servers.
///
/// Combines bridge baseline capabilities with upstream client capabilities.
/// See [`merge_upstream_capabilities`] for merge semantics.
fn build_bridge_client_capabilities(upstream: Option<&ClientCapabilities>) -> serde_json::Value {
    let capabilities = merge_upstream_capabilities(build_baseline_capabilities(), upstream);

    serde_json::to_value(capabilities).unwrap_or_else(|e| {
        log::warn!(
            target: "kakehashi::bridge",
            "Failed to serialize ClientCapabilities, falling back to empty: {}",
            e
        );
        serde_json::json!({})
    })
}

/// Build an LSP initialize request.
///
/// # Arguments
/// * `request_id` - The JSON-RPC request ID
/// * `initialization_options` - Server-specific initialization options
/// * `root_uri` - The workspace root URI (forwarded from upstream client)
/// * `workspace_folders` - The workspace folders (forwarded from upstream client)
/// * `upstream_capabilities` - The upstream client's capabilities (merged into bridge defaults)
pub(crate) fn build_initialize_request(
    request_id: RequestId,
    initialization_options: Option<serde_json::Value>,
    root_uri: Option<String>,
    workspace_folders: Option<serde_json::Value>,
    upstream_capabilities: Option<&ClientCapabilities>,
) -> serde_json::Value {
    let root_path = root_uri.as_deref().and_then(|uri| {
        url::Url::parse(uri)
            .ok()
            .and_then(|u| u.to_file_path().ok())
            .map(|p| p.to_string_lossy().into_owned())
    });

    serde_json::json!({
        "jsonrpc": "2.0",
        "id": request_id.as_i64(),
        "method": "initialize",
        "params": {
            "processId": std::process::id(),
            "rootUri": root_uri,
            "rootPath": root_path,
            "workspaceFolders": workspace_folders,
            "capabilities": build_bridge_client_capabilities(upstream_capabilities),
            "initializationOptions": initialization_options
        }
    })
}

/// Build an LSP initialized notification.
///
/// Sent after receiving the initialize response to signal
/// that the client is ready to receive requests.
pub(crate) fn build_initialized_notification() -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "method": "initialized",
        "params": {}
    })
}

/// Build an LSP shutdown request.
///
/// # Arguments
/// * `request_id` - The JSON-RPC request ID
pub(crate) fn build_shutdown_request(request_id: RequestId) -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": request_id.as_i64(),
        "method": "shutdown",
        "params": null
    })
}

/// Build an LSP exit notification.
///
/// Sent after receiving the shutdown response to terminate the server.
pub(crate) fn build_exit_notification() -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "method": "exit",
        "params": null
    })
}

/// Build a textDocument/didClose notification.
///
/// # Arguments
/// * `uri` - The URI of the document being closed
pub(crate) fn build_didclose_notification(uri: &str) -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "method": "textDocument/didClose",
        "params": {
            "textDocument": {
                "uri": uri
            }
        }
    })
}

/// Validates a JSON-RPC initialize response.
///
/// Uses lenient interpretation to maximize compatibility with non-conformant servers:
/// - Prioritizes error field if present and non-null
/// - Accepts result with null error field (`{"result": {...}, "error": null}`)
/// - Rejects null or missing result field
///
/// # Returns
/// * `Ok(())` - Response is valid (has non-null result, no error)
/// * `Err(e)` - Response has error or missing/null result
pub(crate) fn validate_initialize_response(response: &serde_json::Value) -> std::io::Result<()> {
    // 1. Check for error response (prioritize error if present)
    if let Some(error) = response.get("error").filter(|e| !e.is_null()) {
        // Error field is non-null: treat as error regardless of result
        let code = error.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
        let message = error
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("unknown error");

        return Err(std::io::Error::other(format!(
            "bridge: initialize failed (code {}): {}",
            code, message
        )));
    }

    // 2. Reject if result is absent or null
    if response.get("result").filter(|r| !r.is_null()).is_none() {
        return Err(std::io::Error::other(
            "bridge: initialize response missing valid result",
        ));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[test]
    fn bridge_client_capabilities_snapshot() {
        let capabilities = build_bridge_client_capabilities(None);
        insta::assert_json_snapshot!(capabilities);
    }

    #[test]
    fn initialize_request_has_correct_structure() {
        let request = build_initialize_request(RequestId::new(1), None, None, None, None);

        insta::assert_json_snapshot!(request, {
            ".params.processId" => "[PID]",
        });
    }

    #[test]
    fn initialize_request_includes_bridge_capabilities() {
        let request = build_initialize_request(RequestId::new(1), None, None, None, None);
        let capabilities = &request["params"]["capabilities"];

        // Should declare linkSupport for goto-family methods
        assert_eq!(
            capabilities["textDocument"]["definition"]["linkSupport"],
            true
        );
        // Should declare hierarchical document symbol support
        assert_eq!(
            capabilities["textDocument"]["documentSymbol"]["hierarchicalDocumentSymbolSupport"],
            true
        );
    }

    #[test]
    fn initialize_request_includes_initialization_options() {
        let options = serde_json::json!({
            "settings": {
                "lua": {
                    "diagnostics": { "globals": ["vim"] }
                }
            }
        });
        let request =
            build_initialize_request(RequestId::new(42), Some(options.clone()), None, None, None);

        assert_eq!(request["id"], 42);
        assert_eq!(request["params"]["initializationOptions"], options);
    }

    #[test]
    fn initialize_request_includes_root_uri_when_provided() {
        let root_uri = "file:///home/user/project";
        let request = build_initialize_request(
            RequestId::new(1),
            None,
            Some(root_uri.to_string()),
            None,
            None,
        );

        assert_eq!(request["params"]["rootUri"], root_uri);
    }

    #[test]
    fn initialize_request_has_null_root_uri_when_not_provided() {
        let request = build_initialize_request(RequestId::new(1), None, None, None, None);

        assert!(request["params"]["rootUri"].is_null());
    }

    #[test]
    fn initialize_request_includes_position_encoding() {
        let request = build_initialize_request(RequestId::new(1), None, None, None, None);
        let general = &request["params"]["capabilities"]["general"];

        assert_eq!(general["positionEncodings"], serde_json::json!(["utf-16"]));
    }

    #[test]
    fn initialize_request_includes_workspace_folders_when_provided() {
        let folders = serde_json::json!([
            { "uri": "file:///home/user/project", "name": "project" }
        ]);
        let request =
            build_initialize_request(RequestId::new(1), None, None, Some(folders.clone()), None);

        assert_eq!(request["params"]["workspaceFolders"], folders);
    }

    #[test]
    fn initialize_request_includes_workspace_capabilities() {
        let request = build_initialize_request(RequestId::new(1), None, None, None, None);
        let workspace = &request["params"]["capabilities"]["workspace"];

        // Only declare capabilities that the bridge actually handles
        assert_eq!(workspace["diagnostics"]["refreshSupport"], true);
        // workspaceFolders and configuration are NOT declared because
        // the bridge doesn't implement the corresponding handlers
        assert!(workspace.get("workspaceFolders").is_none());
        assert!(workspace.get("configuration").is_none());
    }

    #[test]
    fn initialize_request_has_null_workspace_folders_when_not_provided() {
        let request = build_initialize_request(RequestId::new(1), None, None, None, None);

        assert!(request["params"]["workspaceFolders"].is_null());
    }

    #[test]
    fn initialize_request_includes_root_path_derived_from_root_uri() {
        let root_uri = "file:///home/user/project";
        let request = build_initialize_request(
            RequestId::new(1),
            None,
            Some(root_uri.to_string()),
            None,
            None,
        );

        assert_eq!(request["params"]["rootPath"], "/home/user/project");
    }

    #[test]
    fn initialize_request_has_null_root_path_when_no_root_uri() {
        let request = build_initialize_request(RequestId::new(1), None, None, None, None);

        assert!(request["params"]["rootPath"].is_null());
    }

    // --- merge_upstream_capabilities tests ---

    #[test]
    fn merge_with_none_upstream_equals_baseline() {
        let base = build_baseline_capabilities();
        let merged = merge_upstream_capabilities(base.clone(), None);
        // Serializing both should produce identical JSON
        assert_eq!(
            serde_json::to_value(&base).unwrap(),
            serde_json::to_value(&merged).unwrap(),
        );
    }

    #[test]
    fn merge_with_no_text_document_equals_baseline() {
        use tower_lsp_server::ls_types::{GeneralClientCapabilities, PositionEncodingKind};

        // Upstream has other fields but no text_document — Category B merge should be skipped
        let upstream = ClientCapabilities {
            general: Some(GeneralClientCapabilities {
                position_encodings: Some(vec![PositionEncodingKind::UTF32]),
                ..Default::default()
            }),
            ..Default::default()
        };
        let base = build_baseline_capabilities();
        let base_json = serde_json::to_value(&base).unwrap();
        let merged = merge_upstream_capabilities(base, Some(&upstream));
        let merged_json = serde_json::to_value(&merged).unwrap();

        // textDocument subtree must be unchanged (Category B merge only fires with text_document)
        assert_eq!(
            merged_json["textDocument"], base_json["textDocument"],
            "textDocument must equal baseline when upstream has no text_document"
        );
        // Bridge-controlled general.positionEncodings must be unchanged
        assert_eq!(
            merged_json["general"]["positionEncodings"], base_json["general"]["positionEncodings"],
            "positionEncodings must not change"
        );
    }

    #[test]
    fn merge_propagates_completion_item_fields() {
        use tower_lsp_server::ls_types::{
            CompletionClientCapabilities, CompletionItemCapability,
            CompletionItemCapabilityResolveSupport, CompletionItemTag, InsertTextMode,
            InsertTextModeSupport, MarkupKind, TagSupport, TextDocumentClientCapabilities,
        };

        let upstream = ClientCapabilities {
            text_document: Some(TextDocumentClientCapabilities {
                completion: Some(CompletionClientCapabilities {
                    completion_item: Some(CompletionItemCapability {
                        documentation_format: Some(vec![
                            MarkupKind::Markdown,
                            MarkupKind::PlainText,
                        ]),
                        snippet_support: Some(false),
                        deprecated_support: Some(true),
                        tag_support: Some(TagSupport {
                            value_set: vec![CompletionItemTag::DEPRECATED],
                        }),
                        commit_characters_support: Some(true),
                        resolve_support: Some(CompletionItemCapabilityResolveSupport {
                            properties: vec!["documentation".to_string(), "detail".to_string()],
                        }),
                        insert_text_mode_support: Some(InsertTextModeSupport {
                            value_set: vec![InsertTextMode::ADJUST_INDENTATION],
                        }),
                        label_details_support: Some(true),
                        preselect_support: Some(true),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };

        let base = build_baseline_capabilities();
        let merged = merge_upstream_capabilities(base, Some(&upstream));
        let item = merged
            .text_document
            .as_ref()
            .unwrap()
            .completion
            .as_ref()
            .unwrap()
            .completion_item
            .as_ref()
            .unwrap();

        assert_eq!(
            item.documentation_format,
            Some(vec![MarkupKind::Markdown, MarkupKind::PlainText])
        );
        // snippetSupport overridden to false (upstream says no)
        assert_eq!(item.snippet_support, Some(false));
        assert_eq!(item.deprecated_support, Some(true));
        assert!(item.tag_support.is_some());
        assert_eq!(item.commit_characters_support, Some(true));
        assert_eq!(
            item.resolve_support.as_ref().unwrap().properties,
            vec!["documentation", "detail"]
        );
        assert_eq!(
            item.insert_text_mode_support
                .as_ref()
                .unwrap()
                .value_set
                .len(),
            1
        );
        assert_eq!(item.label_details_support, Some(true));
        assert_eq!(item.preselect_support, Some(true));
        // Bridge-controlled field must remain unchanged
        assert_eq!(item.insert_replace_support, Some(true));
    }

    #[test]
    fn merge_propagates_hover_content_format_and_signature_information() {
        use tower_lsp_server::ls_types::{
            HoverClientCapabilities, MarkupKind, ParameterInformationSettings,
            SignatureHelpClientCapabilities, SignatureInformationSettings,
            TextDocumentClientCapabilities,
        };

        let upstream = ClientCapabilities {
            text_document: Some(TextDocumentClientCapabilities {
                hover: Some(HoverClientCapabilities {
                    content_format: Some(vec![MarkupKind::PlainText]),
                    ..Default::default()
                }),
                signature_help: Some(SignatureHelpClientCapabilities {
                    signature_information: Some(SignatureInformationSettings {
                        documentation_format: Some(vec![
                            MarkupKind::Markdown,
                            MarkupKind::PlainText,
                        ]),
                        parameter_information: Some(ParameterInformationSettings {
                            label_offset_support: Some(true),
                        }),
                        active_parameter_support: Some(true),
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };

        let base = build_baseline_capabilities();
        let merged = merge_upstream_capabilities(base, Some(&upstream));
        let td = merged.text_document.as_ref().unwrap();

        // Hover contentFormat replaced (upstream prefers plaintext only)
        assert_eq!(
            td.hover.as_ref().unwrap().content_format,
            Some(vec![MarkupKind::PlainText])
        );
        // Hover dynamicRegistration remains bridge-controlled
        assert_eq!(td.hover.as_ref().unwrap().dynamic_registration, Some(false));

        // SignatureHelp signatureInformation propagated
        let sig_info = td
            .signature_help
            .as_ref()
            .unwrap()
            .signature_information
            .as_ref()
            .unwrap();
        assert_eq!(
            sig_info.documentation_format,
            Some(vec![MarkupKind::Markdown, MarkupKind::PlainText])
        );
        assert_eq!(sig_info.active_parameter_support, Some(true));
        assert!(sig_info.parameter_information.is_some());
        // SignatureHelp dynamicRegistration remains bridge-controlled
        assert_eq!(
            td.signature_help.as_ref().unwrap().dynamic_registration,
            Some(false)
        );
    }

    #[test]
    fn merge_does_not_override_bridge_controlled_fields() {
        use tower_lsp_server::ls_types::{
            CompletionClientCapabilities, CompletionItemCapability,
            DocumentSymbolClientCapabilities, DynamicRegistrationClientCapabilities,
            GeneralClientCapabilities, GotoCapability, HoverClientCapabilities,
            PositionEncodingKind, TextDocumentClientCapabilities,
        };

        // Upstream tries to override all Category A fields
        let upstream = ClientCapabilities {
            text_document: Some(TextDocumentClientCapabilities {
                completion: Some(CompletionClientCapabilities {
                    dynamic_registration: Some(true), // Category A
                    completion_item: Some(CompletionItemCapability {
                        insert_replace_support: Some(false), // Category A
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                definition: Some(GotoCapability {
                    dynamic_registration: Some(true), // Category A
                    link_support: Some(false),        // Category A
                }),
                hover: Some(HoverClientCapabilities {
                    dynamic_registration: Some(true), // Category A
                    ..Default::default()
                }),
                document_symbol: Some(DocumentSymbolClientCapabilities {
                    dynamic_registration: Some(true),                  // Category A
                    hierarchical_document_symbol_support: Some(false), // Category A
                    ..Default::default()
                }),
                references: Some(DynamicRegistrationClientCapabilities {
                    dynamic_registration: Some(true), // Category A
                }),
                ..Default::default()
            }),
            general: Some(GeneralClientCapabilities {
                position_encodings: Some(vec![PositionEncodingKind::UTF32]), // Category A
                ..Default::default()
            }),
            ..Default::default()
        };

        let base = build_baseline_capabilities();
        let base_json = serde_json::to_value(&base).unwrap();
        let merged = merge_upstream_capabilities(base, Some(&upstream));
        let merged_json = serde_json::to_value(&merged).unwrap();

        // All bridge-controlled fields must be unchanged
        assert_eq!(
            merged_json["general"]["positionEncodings"], base_json["general"]["positionEncodings"],
            "positionEncodings must not change"
        );
        assert_eq!(
            merged_json["textDocument"]["completion"]["completionItem"]["insertReplaceSupport"],
            base_json["textDocument"]["completion"]["completionItem"]["insertReplaceSupport"],
            "insertReplaceSupport must not change"
        );
        assert_eq!(
            merged_json["textDocument"]["definition"]["linkSupport"],
            base_json["textDocument"]["definition"]["linkSupport"],
            "definition linkSupport must not change"
        );
        assert_eq!(
            merged_json["textDocument"]["definition"]["dynamicRegistration"],
            base_json["textDocument"]["definition"]["dynamicRegistration"],
            "definition dynamicRegistration must not change"
        );
        assert_eq!(
            merged_json["textDocument"]["documentSymbol"]["hierarchicalDocumentSymbolSupport"],
            base_json["textDocument"]["documentSymbol"]["hierarchicalDocumentSymbolSupport"],
            "hierarchicalDocumentSymbolSupport must not change"
        );
        assert_eq!(
            merged_json["textDocument"]["hover"]["dynamicRegistration"],
            base_json["textDocument"]["hover"]["dynamicRegistration"],
            "hover dynamicRegistration must not change"
        );
        assert_eq!(
            merged_json["textDocument"]["completion"]["dynamicRegistration"],
            base_json["textDocument"]["completion"]["dynamicRegistration"],
            "completion dynamicRegistration must not change"
        );
        assert_eq!(
            merged_json["textDocument"]["references"]["dynamicRegistration"],
            base_json["textDocument"]["references"]["dynamicRegistration"],
            "references dynamicRegistration must not change"
        );
    }

    #[test]
    fn bridge_client_capabilities_merged_with_typical_upstream() {
        use tower_lsp_server::ls_types::{
            CompletionClientCapabilities, CompletionItemCapability,
            CompletionItemCapabilityResolveSupport, CompletionItemTag, HoverClientCapabilities,
            InsertTextMode, InsertTextModeSupport, MarkupKind, ParameterInformationSettings,
            SignatureHelpClientCapabilities, SignatureInformationSettings, TagSupport,
            TextDocumentClientCapabilities,
        };

        // Simulate typical Neovim capabilities
        let upstream = ClientCapabilities {
            text_document: Some(TextDocumentClientCapabilities {
                completion: Some(CompletionClientCapabilities {
                    completion_item: Some(CompletionItemCapability {
                        documentation_format: Some(vec![
                            MarkupKind::Markdown,
                            MarkupKind::PlainText,
                        ]),
                        snippet_support: Some(true),
                        deprecated_support: Some(true),
                        tag_support: Some(TagSupport {
                            value_set: vec![CompletionItemTag::DEPRECATED],
                        }),
                        commit_characters_support: Some(true),
                        resolve_support: Some(CompletionItemCapabilityResolveSupport {
                            properties: vec![
                                "documentation".to_string(),
                                "detail".to_string(),
                                "additionalTextEdits".to_string(),
                                "sortText".to_string(),
                                "filterText".to_string(),
                                "insertText".to_string(),
                                "textEdit".to_string(),
                                "insertTextFormat".to_string(),
                                "insertTextMode".to_string(),
                            ],
                        }),
                        insert_text_mode_support: Some(InsertTextModeSupport {
                            value_set: vec![
                                InsertTextMode::AS_IS,
                                InsertTextMode::ADJUST_INDENTATION,
                            ],
                        }),
                        label_details_support: Some(true),
                        preselect_support: Some(true),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                hover: Some(HoverClientCapabilities {
                    content_format: Some(vec![MarkupKind::Markdown, MarkupKind::PlainText]),
                    ..Default::default()
                }),
                signature_help: Some(SignatureHelpClientCapabilities {
                    signature_information: Some(SignatureInformationSettings {
                        documentation_format: Some(vec![
                            MarkupKind::Markdown,
                            MarkupKind::PlainText,
                        ]),
                        parameter_information: Some(ParameterInformationSettings {
                            label_offset_support: Some(true),
                        }),
                        active_parameter_support: Some(true),
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };

        let merged = build_bridge_client_capabilities(Some(&upstream));
        insta::assert_json_snapshot!(merged);
    }

    #[test]
    fn initialize_request_with_upstream_capabilities() {
        use tower_lsp_server::ls_types::{
            CompletionClientCapabilities, CompletionItemCapability,
            CompletionItemCapabilityResolveSupport, MarkupKind, TextDocumentClientCapabilities,
        };

        let upstream = ClientCapabilities {
            text_document: Some(TextDocumentClientCapabilities {
                completion: Some(CompletionClientCapabilities {
                    completion_item: Some(CompletionItemCapability {
                        documentation_format: Some(vec![
                            MarkupKind::Markdown,
                            MarkupKind::PlainText,
                        ]),
                        resolve_support: Some(CompletionItemCapabilityResolveSupport {
                            properties: vec!["documentation".to_string()],
                        }),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };

        let request =
            build_initialize_request(RequestId::new(1), None, None, None, Some(&upstream));
        let caps = &request["params"]["capabilities"];

        // Merged fields should be present
        assert_eq!(
            caps["textDocument"]["completion"]["completionItem"]["documentationFormat"],
            serde_json::json!(["markdown", "plaintext"])
        );
        assert_eq!(
            caps["textDocument"]["completion"]["completionItem"]["resolveSupport"]["properties"],
            serde_json::json!(["documentation"])
        );
        // Bridge-controlled fields unchanged
        assert_eq!(
            caps["textDocument"]["completion"]["completionItem"]["insertReplaceSupport"],
            true
        );
    }

    #[test]
    fn initialized_notification_has_correct_structure() {
        let notification = build_initialized_notification();

        insta::assert_json_snapshot!(notification);
    }

    #[test]
    fn shutdown_request_has_correct_structure() {
        let request = build_shutdown_request(RequestId::new(99));

        insta::assert_json_snapshot!(request);
    }

    #[test]
    fn exit_notification_has_correct_structure() {
        let notification = build_exit_notification();

        insta::assert_json_snapshot!(notification);
    }

    #[test]
    fn didclose_notification_has_correct_structure() {
        let notification = build_didclose_notification("file:///project/test.lua");

        insta::assert_json_snapshot!(notification);
    }

    #[test]
    fn didclose_notification_with_virtual_uri() {
        let uri = "file:///project/kakehashi-virtual-uri-abc123.lua";
        let notification = build_didclose_notification(uri);

        assert_eq!(notification["params"]["textDocument"]["uri"], uri);
    }

    // Tests for validate_initialize_response

    #[rstest]
    #[case::valid_result_without_error(
        serde_json::json!({"result": {"capabilities": {}}})
    )]
    #[case::valid_result_with_null_error(
        serde_json::json!({"result": {"capabilities": {}}, "error": null})
    )]
    #[case::complex_result_object(
        serde_json::json!({
            "result": {
                "capabilities": {
                    "textDocumentSync": 1,
                    "completionProvider": {
                        "triggerCharacters": ["."]
                    }
                },
                "serverInfo": {
                    "name": "test-server",
                    "version": "1.0.0"
                }
            }
        })
    )]
    #[trace]
    fn validate_accepts_valid_response(#[case] response: serde_json::Value) {
        assert!(
            validate_initialize_response(&response).is_ok(),
            "Expected valid response to be accepted: {:?}",
            response
        );
    }

    #[rstest]
    #[case::null_result(
        serde_json::json!({"result": null}),
        "missing valid result"
    )]
    #[case::missing_result_and_error(
        serde_json::json!({}),
        "missing valid result"
    )]
    #[case::null_result_with_null_error(
        serde_json::json!({"result": null, "error": null}),
        "missing valid result"
    )]
    #[trace]
    fn validate_rejects_missing_result(
        #[case] response: serde_json::Value,
        #[case] expected_error: &str,
    ) {
        let result = validate_initialize_response(&response);
        assert!(result.is_err(), "Expected rejection for: {:?}", response);
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains(expected_error),
            "Expected error containing {:?}, got: {}",
            expected_error,
            err_msg
        );
    }

    #[rstest]
    #[case::error_response(
        serde_json::json!({
            "error": {
                "code": -32600,
                "message": "Invalid Request"
            }
        }),
        "code -32600",
        "Invalid Request"
    )]
    #[case::error_response_even_with_result(
        serde_json::json!({
            "result": {"capabilities": {}},
            "error": {
                "code": -32603,
                "message": "Internal error"
            }
        }),
        "code -32603",
        "Internal error"
    )]
    #[case::malformed_error_missing_code(
        serde_json::json!({
            "error": {
                "message": "Something went wrong"
            }
        }),
        "code -1",  // Default code
        "Something went wrong"
    )]
    #[case::malformed_error_missing_message(
        serde_json::json!({
            "error": {
                "code": -32700
            }
        }),
        "code -32700",
        "unknown error"  // Default message
    )]
    #[case::malformed_error_empty_object(
        serde_json::json!({"error": {}}),
        "code -1",
        "unknown error"
    )]
    #[case::malformed_error_wrong_types(
        serde_json::json!({
            "error": {
                "code": "not-a-number",
                "message": 123
            }
        }),
        "code -1",  // Can't parse string as i64
        "unknown error"  // Can't parse number as str
    )]
    #[trace]
    fn validate_rejects_error_response(
        #[case] response: serde_json::Value,
        #[case] expected_code: &str,
        #[case] expected_message: &str,
    ) {
        let result = validate_initialize_response(&response);
        assert!(result.is_err(), "Expected rejection for: {:?}", response);
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains(expected_code),
            "Expected code {:?} in error: {}",
            expected_code,
            err_msg
        );
        assert!(
            err_msg.contains(expected_message),
            "Expected message {:?} in error: {}",
            expected_message,
            err_msg
        );
    }
}
