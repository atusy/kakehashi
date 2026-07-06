//! Bridge client capabilities: baseline definitions and upstream merging.
//!
//! Defines the capabilities the bridge declares to downstream servers and
//! provides merging logic to propagate upstream client preferences.

use tower_lsp_server::ls_types::ClientCapabilities;

/// Build the baseline client capabilities the bridge declares to downstream servers.
///
/// `experimental` is the process-wide `KAKEHASHI_EXPERIMENTAL=true` opt-in
/// (passed in so both variants stay testable); it adds the capabilities of
/// experimental features (currently `colorProvider`).
///
/// Returns typed `ClientCapabilities` for use with [`merge_upstream_capabilities`].
fn build_baseline_capabilities(
    advertise_configuration: bool,
    experimental: bool,
) -> ClientCapabilities {
    use tower_lsp_server::ls_types::{
        CodeActionClientCapabilities, CodeActionKindLiteralSupport, CodeActionLiteralSupport,
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
        // Without codeActionLiteralSupport, older servers fall back to
        // returning bare Commands only (issue #568). No dataSupport /
        // resolveSupport yet: servers must return complete (edit-carrying)
        // actions until codeAction/resolve is bridged.
        code_action: Some(CodeActionClientCapabilities {
            dynamic_registration: Some(false),
            code_action_literal_support: Some(CodeActionLiteralSupport {
                code_action_kind: CodeActionKindLiteralSupport {
                    value_set: [
                        "",
                        "quickfix",
                        "refactor",
                        "refactor.extract",
                        "refactor.inline",
                        "refactor.rewrite",
                        "source",
                        "source.organizeImports",
                        "source.fixAll",
                    ]
                    .into_iter()
                    .map(String::from)
                    .collect(),
                },
            }),
            is_preferred_support: Some(true),
            disabled_support: Some(true),
            ..Default::default()
        }),
        diagnostic: Some(DiagnosticClientCapabilities {
            dynamic_registration: Some(true),
            related_document_support: Some(true),
            ..Default::default()
        }),
        moniker: Some(DynamicRegistrationClientCapabilities {
            dynamic_registration: Some(false),
        }),
        ..Default::default()
    };

    if experimental {
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
            // The bridge sends InitializeParams.workspaceFolders (upstream
            // passthrough or the workspaceMarkers-derived folder), which LSP makes
            // conditional on this capability.
            workspace_folders: Some(true),
            // The bridge owns and serves each server's workspace settings
            // (downstream-settings-propagation): advertise `configuration` so a
            // spec-compliant downstream server pulls via `workspace/configuration`,
            // answered from the per-connection settings cell. Gated per-server on
            // having settings to serve: advertising it for a server with no
            // `settings` would flip an `initializationOptions`-configured server
            // to pull and answer every section `null`, clobbering config it held.
            configuration: advertise_configuration.then_some(true),
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
/// Bridge-controlled fields are never overridden because the bridge depends on
/// them: `general.positionEncodings` (UTF-16), `insertReplaceSupport`, all
/// `linkSupport` (we collapse `LocationLink` → `Location`),
/// `hierarchicalDocumentSymbolSupport`, every `dynamicRegistration`.
///
/// Pass-through fields propagate from upstream when `Some` (otherwise the
/// bridge default is kept; LSP order-sensitivity applies on replace):
/// `completionItem.{documentationFormat, snippetSupport, deprecatedSupport,
/// tagSupport, commitCharactersSupport, resolveSupport, insertTextModeSupport,
/// labelDetailsSupport, preselectSupport}`, `hover.contentFormat`,
/// `signatureHelp.signatureInformation`, `window.workDoneProgress`,
/// `window.showDocument`, `window.showMessage`.
///
/// `window.workDoneProgress` and `window.showDocument` are gated on the real
/// upstream editor so the bridge only invites a downstream server-initiated
/// request (`window/workDoneProgress/create`, `window/showDocument`) when it can
/// actually relay it to the editor — see ls-bridge-work-done-progress.
/// `window.showMessage` (the `messageActionItem` refinement) is a plain
/// pass-through: `window/showMessageRequest` is a base-protocol request the
/// bridge always relays.
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

        if let Some(upstream_item) = upstream_td
            .completion
            .as_ref()
            .and_then(|c| c.completion_item.as_ref())
        {
            let base_item = base_td
                .completion
                .get_or_insert_with(Default::default)
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

        // --- Hover contentFormat (Category B) ---
        if let Some(upstream_hover) = &upstream_td.hover {
            let base_hover = base_td.hover.get_or_insert_with(Default::default);
            merge_option(
                &mut base_hover.content_format,
                upstream_hover.content_format.clone(),
            );
        }

        // --- SignatureHelp signatureInformation sub-fields (Category B) ---
        if let Some(upstream_sig_info) = upstream_td
            .signature_help
            .as_ref()
            .and_then(|s| s.signature_information.as_ref())
        {
            let base_sig_info = base_td
                .signature_help
                .get_or_insert_with(Default::default)
                .signature_information
                .get_or_insert_with(Default::default);
            merge_option(
                &mut base_sig_info.documentation_format,
                upstream_sig_info.documentation_format.clone(),
            );
            merge_option(
                &mut base_sig_info.parameter_information,
                upstream_sig_info.parameter_information.clone(),
            );
            merge_option(
                &mut base_sig_info.active_parameter_support,
                upstream_sig_info.active_parameter_support,
            );
        }
    }

    // --- window.workDoneProgress (gated on real upstream support) ---
    // Advertise server-initiated progress downstream ONLY when the editor
    // genuinely supports it (`Some(true)`), so the bridge never invites progress
    // it can't relay (ls-bridge-work-done-progress). An explicit `false` or an
    // absent value is left unadvertised — and we never materialize an empty
    // `window: {}` the baseline lacked — so a server that misreads field presence
    // as support is not misled.
    if upstream.window.as_ref().and_then(|w| w.work_done_progress) == Some(true) {
        base.window
            .get_or_insert_with(Default::default)
            .work_done_progress = Some(true);
    }

    // --- window.showDocument (gated on real upstream support) ---
    // Same rationale as workDoneProgress: advertise downstream ONLY when the
    // editor genuinely supports `window/showDocument` (`support == true`), so the
    // bridge never invites a request it could only ever answer `success:false`.
    // An absent or `false` capability leaves `window.showDocument` unadvertised
    // (and never materializes an empty `window: {}` the baseline lacked).
    if upstream
        .window
        .as_ref()
        .and_then(|w| w.show_document.as_ref())
        .map(|s| s.support)
        == Some(true)
    {
        use tower_lsp_server::ls_types::ShowDocumentClientCapabilities;
        base.window
            .get_or_insert_with(Default::default)
            .show_document = Some(ShowDocumentClientCapabilities { support: true });
    }

    // --- window.showMessage messageActionItem (passthrough) ---
    // `window/showMessageRequest` is a base-protocol request the bridge always
    // relays, so this only forwards the editor's `messageActionItem` refinement
    // (e.g. `additionalPropertiesSupport`) when present, keeping the action items
    // the downstream receives — and the selection it sends back — faithful.
    if let Some(show_message) = upstream
        .window
        .as_ref()
        .and_then(|w| w.show_message.as_ref())
    {
        base.window
            .get_or_insert_with(Default::default)
            .show_message = Some(show_message.clone());
    }

    base
}

/// Build the client capabilities the bridge declares to downstream servers.
///
/// Combines bridge baseline capabilities with upstream client capabilities.
/// See [`merge_upstream_capabilities`] for merge semantics and
/// [`build_baseline_capabilities`] for the `experimental` opt-in.
pub(super) fn build_bridge_client_capabilities(
    upstream: Option<&ClientCapabilities>,
    advertise_configuration: bool,
    experimental: bool,
) -> ClientCapabilities {
    merge_upstream_capabilities(
        build_baseline_capabilities(advertise_configuration, experimental),
        upstream,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Snapshot suffixes predate the runtime opt-in (they matched the old
    /// "experimental" cargo feature); both variants now run in one process.
    const EXPERIMENTAL_VARIANTS: [(bool, &str); 2] = [(false, "default"), (true, "experimental")];

    #[test]
    fn bridge_client_capabilities_snapshot() {
        for (experimental, suffix) in EXPERIMENTAL_VARIANTS {
            let capabilities = build_bridge_client_capabilities(None, true, experimental);
            insta::with_settings!({snapshot_suffix => suffix}, {
                insta::assert_json_snapshot!(capabilities);
            });
        }
    }

    #[test]
    fn merge_with_none_upstream_equals_baseline() {
        let base = build_baseline_capabilities(true, false);
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
        let base = build_baseline_capabilities(true, false);
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

        let base = build_baseline_capabilities(true, false);
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

        let base = build_baseline_capabilities(true, false);
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

        let base = build_baseline_capabilities(true, false);
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
    fn merge_propagates_window_work_done_progress_only_when_upstream_supports() {
        use tower_lsp_server::ls_types::WindowClientCapabilities;

        // Baseline declares no window capability, so no upstream → none downstream.
        let baseline = build_baseline_capabilities(true, false);
        assert!(
            baseline.window.is_none(),
            "baseline must not advertise window.workDoneProgress on its own"
        );

        // Upstream supports it → propagated downstream.
        let supporting = ClientCapabilities {
            window: Some(WindowClientCapabilities {
                work_done_progress: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        };
        let merged = merge_upstream_capabilities(
            build_baseline_capabilities(true, false),
            Some(&supporting),
        );
        assert_eq!(
            merged.window.and_then(|w| w.work_done_progress),
            Some(true),
            "must advertise downstream when the editor supports progress"
        );

        // Upstream omits it → not advertised downstream (gated).
        let non_supporting = ClientCapabilities::default();
        let merged = merge_upstream_capabilities(
            build_baseline_capabilities(true, false),
            Some(&non_supporting),
        );
        assert!(
            merged.window.and_then(|w| w.work_done_progress).is_none(),
            "must not invite progress the editor can't handle"
        );

        // Upstream explicitly false → not advertised, and no empty `window` is
        // materialized (a server must not misread field presence as support).
        let explicit_false = ClientCapabilities {
            window: Some(WindowClientCapabilities {
                work_done_progress: Some(false),
                ..Default::default()
            }),
            ..Default::default()
        };
        let merged = merge_upstream_capabilities(
            build_baseline_capabilities(true, false),
            Some(&explicit_false),
        );
        assert!(
            merged.window.is_none(),
            "explicit false must leave window unadvertised, not materialize workDoneProgress:false"
        );
    }

    #[test]
    fn merge_advertises_show_document_only_when_upstream_supports() {
        use tower_lsp_server::ls_types::{
            ShowDocumentClientCapabilities, WindowClientCapabilities,
        };

        // Upstream supports showDocument → advertised downstream.
        let supporting = ClientCapabilities {
            window: Some(WindowClientCapabilities {
                show_document: Some(ShowDocumentClientCapabilities { support: true }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let merged = merge_upstream_capabilities(
            build_baseline_capabilities(true, false),
            Some(&supporting),
        );
        assert_eq!(
            merged
                .window
                .and_then(|w| w.show_document)
                .map(|s| s.support),
            Some(true),
            "must advertise showDocument when the editor supports it"
        );

        // Upstream support=false → not advertised, no empty `window` materialized.
        let unsupported = ClientCapabilities {
            window: Some(WindowClientCapabilities {
                show_document: Some(ShowDocumentClientCapabilities { support: false }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let merged = merge_upstream_capabilities(
            build_baseline_capabilities(true, false),
            Some(&unsupported),
        );
        assert!(
            merged.window.is_none(),
            "support=false must leave window unadvertised (bridge would only answer success:false)"
        );
    }

    #[test]
    fn merge_passes_through_show_message_message_action_item() {
        use tower_lsp_server::ls_types::{
            MessageActionItemCapabilities, ShowMessageRequestClientCapabilities,
            WindowClientCapabilities,
        };

        let upstream = ClientCapabilities {
            window: Some(WindowClientCapabilities {
                show_message: Some(ShowMessageRequestClientCapabilities {
                    message_action_item: Some(MessageActionItemCapabilities {
                        additional_properties_support: Some(true),
                    }),
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let merged =
            merge_upstream_capabilities(build_baseline_capabilities(true, false), Some(&upstream));
        assert_eq!(
            merged
                .window
                .and_then(|w| w.show_message)
                .and_then(|s| s.message_action_item)
                .and_then(|m| m.additional_properties_support),
            Some(true),
            "showMessage messageActionItem must pass through from upstream"
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

        for (experimental, suffix) in EXPERIMENTAL_VARIANTS {
            let merged = build_bridge_client_capabilities(Some(&upstream), true, experimental);
            insta::with_settings!({snapshot_suffix => suffix}, {
                insta::assert_json_snapshot!(merged);
            });
        }
    }

    #[test]
    fn configuration_capability_is_gated_on_advertise_flag() {
        // Advertised only when the server has settings to serve
        // (downstream-settings-propagation): otherwise an
        // initializationOptions-configured server would be flipped to pull and
        // answered `null`.
        let advertised = build_bridge_client_capabilities(None, true, false);
        assert_eq!(
            advertised.workspace.as_ref().and_then(|w| w.configuration),
            Some(true),
        );

        let not_advertised = build_bridge_client_capabilities(None, false, false);
        assert_eq!(
            not_advertised
                .workspace
                .as_ref()
                .and_then(|w| w.configuration),
            None,
            "no settings to serve → capability withheld",
        );
    }
}
