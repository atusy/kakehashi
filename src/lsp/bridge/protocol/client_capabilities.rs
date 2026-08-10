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
        CodeActionCapabilityResolveSupport, CodeActionClientCapabilities,
        CodeActionKindLiteralSupport, CodeActionLiteralSupport, CompletionClientCapabilities,
        CompletionItemCapability, DiagnosticClientCapabilities,
        DiagnosticWorkspaceClientCapabilities, DocumentLinkClientCapabilities,
        DocumentSymbolClientCapabilities, DynamicRegistrationClientCapabilities,
        GeneralClientCapabilities, GotoCapability, HoverClientCapabilities,
        InlayHintClientCapabilities, PositionEncodingKind, SignatureHelpClientCapabilities,
        TextDocumentClientCapabilities, TextDocumentSyncClientCapabilities,
        WorkspaceClientCapabilities,
    };

    let goto_link = Some(GotoCapability {
        dynamic_registration: Some(false),
        link_support: Some(true),
    });

    let mut text_document = TextDocumentClientCapabilities {
        synchronization: Some(TextDocumentSyncClientCapabilities {
            dynamic_registration: Some(false),
            did_save: Some(true),
            ..Default::default()
        }),
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
        // returning bare Commands only (issue #568). `dataSupport` +
        // `resolveSupport(["edit"])` let lazy servers (rust-analyzer-style)
        // return actions whose `edit` is filled in on `codeAction/resolve`
        // (PR 4) — the bridge routes that resolve back to the origin server.
        code_action: Some(CodeActionClientCapabilities {
            dynamic_registration: Some(false),
            code_action_literal_support: Some(CodeActionLiteralSupport {
                code_action_kind: CodeActionKindLiteralSupport {
                    value_set: [
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
            data_support: Some(true),
            resolve_support: Some(CodeActionCapabilityResolveSupport {
                properties: vec!["edit".to_string()],
            }),
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
            // The bridge handles inbound `workspace/applyEdit` (#568 PR 5),
            // translating virtual-document edits to the host document and
            // relaying the editor's response — but the relay terminates at the
            // EDITOR, so the capability is only honest when the editor itself
            // declared `workspace.applyEdit`. Gated in
            // `merge_upstream_capabilities` (like `window.workDoneProgress`):
            // withheld here, advertised downstream only when the editor
            // genuinely supports it.
            apply_edit: None,
            // The bridge executes a surfaced command via `workspace/executeCommand`
            // (#568 PR 6), so advertise the client capability — a spec-compliant
            // server may withhold command-carrying actions otherwise. No dynamic
            // registration (the bridge routes by the static command name).
            execute_command: Some(DynamicRegistrationClientCapabilities {
                dynamic_registration: Some(false),
            }),
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
        // The routing protocol is negotiated independently of the
        // process-wide experimental feature gate.  Downstream servers opt in
        // by returning the matching advertisement in their capabilities.
        experimental: Some(serde_json::json!({
            "kakehashi": {
                "bridgeRouting": true,
            },
        })),
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
/// `signatureHelp.signatureInformation`, `inlayHint.resolveSupport`,
/// `window.workDoneProgress`,
/// `window.showDocument`, `window.showMessage`,
/// `workspace.workspaceEdit` (mirrored minus `changeAnnotationSupport` — see
/// the merge site for why annotations are withheld), and `workspace.applyEdit`
/// (gated on the editor genuinely declaring it — the relay terminates at the
/// editor).
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

    // Preserve editor-provided experimental extensions. Object-shaped values
    // can carry the bridge advertisement alongside the editor's keys; a
    // non-object value is retained under `upstream` while the bridge-owned
    // advertisement remains present.
    if let Some(upstream_experimental) = &upstream.experimental {
        match upstream_experimental {
            serde_json::Value::Object(upstream_experimental) => {
                let mut experimental = upstream_experimental.clone();
                let kakehashi = experimental
                    .entry("kakehashi".to_string())
                    .or_insert_with(|| serde_json::json!({}));
                if let serde_json::Value::Object(kakehashi) = kakehashi {
                    kakehashi.insert("bridgeRouting".to_string(), serde_json::Value::Bool(true));
                    base.experimental = Some(serde_json::Value::Object(experimental));
                } else {
                    let upstream_kakehashi = experimental
                        .get("kakehashi")
                        .cloned()
                        .expect("kakehashi entry exists");
                    let mut kakehashi = serde_json::Map::new();
                    kakehashi.insert("bridgeRouting".to_string(), serde_json::Value::Bool(true));
                    kakehashi.insert("upstream".to_string(), upstream_kakehashi);
                    experimental.insert(
                        "kakehashi".to_string(),
                        serde_json::Value::Object(kakehashi),
                    );
                    base.experimental = Some(serde_json::Value::Object(experimental));
                }
            }
            upstream_experimental => {
                base.experimental = Some(serde_json::json!({
                    "kakehashi": {"bridgeRouting": true},
                    "upstream": upstream_experimental,
                }));
            }
        }
    }

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

        // The bridge can translate every standard lazy inlay-hint property,
        // but the editor ultimately consumes it. Forward only the editor's
        // declared property set so downstream servers choose eager vs lazy
        // materialization honestly.
        if let Some(resolve_support) = upstream_td
            .inlay_hint
            .as_ref()
            .and_then(|capability| capability.resolve_support.clone())
        {
            base_td
                .inlay_hint
                .get_or_insert_with(Default::default)
                .resolve_support = Some(resolve_support);
        }
    }

    // --- workspace.workspaceEdit (mirror, minus changeAnnotationSupport) ---
    // A client that omits `workspace.workspaceEdit` implies
    // `documentChanges: false` and no `resourceOperations`, so a spec-compliant
    // downstream withholds documentChanges-shaped edits and every
    // create/rename/delete-carrying refactor ("extract into file") — even
    // though the bridge transform handles both (real-file ops pass through;
    // virtual-URI ops reject the whole edit). Mirror the editor's declaration
    // so downstream servers offer exactly what the editor can apply. Unlike
    // the `window.*` fields below — gated because they invite server-initiated
    // REQUESTS the bridge must relay — this declares a response SHAPE the
    // editor ultimately applies, so full-fidelity mirroring (including
    // `failureHandling`/`normalizesLineEndings`) is the honest semantics.
    // `changeAnnotationSupport` is deliberately withheld: ls-types' untagged
    // `OneOf` drops `annotationId` when deserializing downstream responses, so
    // inviting annotated edits would silently lose `needsConfirmation`. The
    // proposed 3.18 `metadataSupport`/`snippetEditSupport` fields are also
    // effectively withheld — ls-types' WorkspaceEditClientCapabilities has no
    // such fields, so the typed InitializeParams parse drops them before this
    // clone ever sees them.
    if let Some(upstream_workspace_edit) = upstream
        .workspace
        .as_ref()
        .and_then(|w| w.workspace_edit.as_ref())
    {
        let mut mirrored = upstream_workspace_edit.clone();
        mirrored.change_annotation_support = None;
        base.workspace
            .get_or_insert_with(Default::default)
            .workspace_edit = Some(mirrored);
    }

    // --- workspace.applyEdit (gated on real upstream support) ---
    // The bridge can only relay a downstream `workspace/applyEdit` to an
    // editor that declared the capability itself; advertising it regardless
    // would invite requests the bridge can only answer `applied: false`.
    // Same gating rationale as `window.workDoneProgress` below. An explicit
    // `false` or an absent value stays unadvertised.
    if upstream
        .workspace
        .as_ref()
        .and_then(|w| w.apply_edit)
        .unwrap_or(false)
    {
        base.workspace
            .get_or_insert_with(Default::default)
            .apply_edit = Some(true);
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

/// Fold a user's `clientCapabilities` config override into the serialized
/// capabilities (issue #976).
///
/// Runs at the JSON layer, after [`merge_upstream_capabilities`], so the user
/// is the last merge layer (their `false` reliably masks an
/// upstream-propagated `true`) and fields the typed `ClientCapabilities`
/// doesn't model pass through instead of being dropped by a typed round-trip.
/// Merge semantics are [`crate::config::merge::deep_merge_json`] — the same
/// deep merge that combines the override across config layers.
///
/// Two fields are protected as post-merge invariants (enforced on the merged
/// result, so no override shape can bypass them the way an input filter
/// could): `general.positionEncodings` — kakehashi's coordinate translation
/// requires UTF-16, and an override there would silently corrupt every
/// position in every bridged response — and
/// `workspace.workspaceEdit.changeAnnotationSupport` — see
/// [`strip_change_annotation_support`]. A non-object override at the root is
/// ignored with a warning — deep-merging it would replace the whole
/// capabilities object.
pub(super) fn apply_capability_override(
    capabilities: &mut serde_json::Value,
    override_json: &serde_json::Value,
) {
    if !override_json.is_object() {
        log::warn!(
            target: "kakehashi::bridge",
            "clientCapabilities override must be a table, got {override_json}; ignoring it"
        );
        return;
    }
    let baseline_encodings = capabilities.pointer("/general/positionEncodings").cloned();
    let merged = crate::config::merge::deep_merge_json(capabilities, override_json);
    *capabilities = merged;

    strip_change_annotation_support(capabilities);

    let Some(baseline) = baseline_encodings else {
        return;
    };
    if capabilities.pointer("/general/positionEncodings") == Some(&baseline) {
        return;
    }
    log::warn!(
        target: "kakehashi::bridge",
        "clientCapabilities override cannot change general.positionEncodings: \
         kakehashi's coordinate translation requires UTF-16; keeping {baseline}"
    );
    match capabilities.get_mut("general") {
        Some(serde_json::Value::Object(general)) => {
            general.insert("positionEncodings".to_string(), baseline);
        }
        // A non-object `general` from the override replaced the subtree; a
        // scalar there is spec-invalid anyway, so rebuild the object around
        // the load-bearing field. (Any baseline `general` siblings are lost
        // here — today positionEncodings is the only one.)
        Some(other) => {
            *other = serde_json::json!({ "positionEncodings": baseline });
        }
        // Unreachable today — deep_merge_json never removes keys, so a
        // baseline `general` survives every merge — kept so the invariant
        // holds even if the merge semantics change.
        None => {
            if let Some(capabilities) = capabilities.as_object_mut() {
                capabilities.insert(
                    "general".to_string(),
                    serde_json::json!({ "positionEncodings": baseline }),
                );
            }
        }
    }
}

/// Config-shape problems in a `clientCapabilities` override worth telling the
/// USER about, detectable from the override alone (no baseline needed).
///
/// Mirrors the conditions [`apply_capability_override`] enforces at merge
/// time. Enforcement keeps its `log::warn!` backstops, but `log::warn!` is
/// invisible at the default log level, so the pool surfaces these through
/// `warn_to_editor` at spawn — where the server name is in scope — as the
/// user-facing channel. `advertise_configuration` is the settings-presence
/// gate the override may contradict (downstream-settings-propagation).
pub(crate) fn capability_override_user_warnings(
    override_json: &serde_json::Value,
    advertise_configuration: bool,
) -> Vec<String> {
    if !override_json.is_object() {
        return vec!["clientCapabilities must be a table; ignoring the override".to_string()];
    }
    let mut warnings = Vec::new();
    match override_json.get("general") {
        Some(general) if !general.is_object() => warnings.push(
            "clientCapabilities.general must be a table; general.positionEncodings stays utf-16"
                .to_string(),
        ),
        // An override restating the enforced utf-16 baseline is a no-op, not
        // a conflict — warn only when the value would actually change.
        Some(general)
            if general
                .get("positionEncodings")
                .is_some_and(|encodings| encodings != &serde_json::json!(["utf-16"])) =>
        {
            warnings.push(
                "clientCapabilities cannot change general.positionEncodings \
                 (kakehashi's coordinate translation requires utf-16); keeping utf-16"
                    .to_string(),
            )
        }
        _ => {}
    }
    if override_json
        .pointer("/workspace/workspaceEdit/changeAnnotationSupport")
        .is_some()
    {
        warnings.push(
            "clientCapabilities cannot advertise workspace.workspaceEdit.changeAnnotationSupport \
             (annotated edits would lose needsConfirmation in the bridge); removing it"
                .to_string(),
        );
    }
    // The conflict check reasons about the EFFECTIVE post-merge value, not
    // just a boolean leaf: a non-object `workspace` replaces the whole
    // subtree, and `configuration: null`/scalar displaces the advertised
    // `true` — both turn the capability off as surely as `false` does.
    match override_json.get("workspace") {
        Some(workspace) if !workspace.is_object() => warnings.push(
            "clientCapabilities.workspace must be a table; replacing it wholesale drops every \
             workspace capability kakehashi advertised"
                .to_string(),
        ),
        Some(workspace) => {
            let effective_on = workspace
                .get("configuration")
                .map(|value| value.as_bool() == Some(true));
            if let Some(effective_on) = effective_on
                && effective_on != advertise_configuration
            {
                warnings.push(if effective_on {
                    "clientCapabilities forces workspace.configuration=true but this server has \
                     no settings to serve: every configuration pull will be answered null"
                        .to_string()
                } else {
                    "clientCapabilities overrides workspace.configuration away from true while \
                     this server has settings: a pull-model server may never read them"
                        .to_string()
                });
            }
        }
        None => {}
    }
    warnings
}

/// Post-merge invariant #2: `workspace.workspaceEdit.changeAnnotationSupport`
/// must never be advertised. The upstream mirror deliberately withholds it —
/// ls-types' untagged `OneOf` drops `annotationId` when deserializing
/// downstream edits, so inviting annotated edits would silently strip
/// `needsConfirmation` and let a downstream's guarded edit apply without the
/// confirmation it demanded (the same silent-corruption class as
/// `positionEncodings`). A user override cannot be allowed to re-open it.
fn strip_change_annotation_support(capabilities: &mut serde_json::Value) {
    let Some(workspace_edit) = capabilities
        .pointer_mut("/workspace/workspaceEdit")
        .and_then(|value| value.as_object_mut())
    else {
        return;
    };
    if workspace_edit.remove("changeAnnotationSupport").is_some() {
        log::warn!(
            target: "kakehashi::bridge",
            "clientCapabilities override cannot advertise workspace.workspaceEdit.changeAnnotationSupport: \
             annotated edits would lose needsConfirmation in the bridge; removing it"
        );
    }
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
    fn bridge_advertises_static_did_save_support() {
        let capabilities = build_bridge_client_capabilities(None, true, false);
        let synchronization = capabilities
            .text_document
            .as_ref()
            .and_then(|text_document| text_document.synchronization.as_ref())
            .expect("the bridge must advertise text-document synchronization");

        assert_eq!(synchronization.dynamic_registration, Some(false));
        assert_eq!(synchronization.did_save, Some(true));
    }

    #[test]
    fn merge_mirrors_workspace_edit_capability_without_annotations() {
        use tower_lsp_server::ls_types::{
            ChangeAnnotationWorkspaceEditClientCapabilities, FailureHandlingKind,
            ResourceOperationKind, WorkspaceClientCapabilities, WorkspaceEditClientCapabilities,
        };

        let upstream = ClientCapabilities {
            workspace: Some(WorkspaceClientCapabilities {
                workspace_edit: Some(WorkspaceEditClientCapabilities {
                    document_changes: Some(true),
                    resource_operations: Some(vec![
                        ResourceOperationKind::Create,
                        ResourceOperationKind::Rename,
                        ResourceOperationKind::Delete,
                    ]),
                    failure_handling: Some(FailureHandlingKind::Abort),
                    normalizes_line_endings: Some(true),
                    change_annotation_support: Some(
                        ChangeAnnotationWorkspaceEditClientCapabilities {
                            groups_on_label: Some(true),
                        },
                    ),
                }),
                ..Default::default()
            }),
            ..Default::default()
        };

        let merged = build_bridge_client_capabilities(Some(&upstream), true, false);
        let workspace_edit = merged
            .workspace
            .as_ref()
            .and_then(|w| w.workspace_edit.as_ref())
            .expect("the editor's workspaceEdit capability must be mirrored downstream");
        assert_eq!(workspace_edit.document_changes, Some(true));
        assert_eq!(
            workspace_edit
                .resource_operations
                .as_ref()
                .map(|ops| ops.len()),
            Some(3)
        );
        assert_eq!(
            workspace_edit.failure_handling,
            Some(FailureHandlingKind::Abort)
        );
        assert_eq!(workspace_edit.normalizes_line_endings, Some(true));
        assert_eq!(
            workspace_edit.change_annotation_support, None,
            "annotation support must be withheld: ls-types drops annotationId on parse"
        );
    }

    #[test]
    fn merge_without_upstream_workspace_edit_advertises_none() {
        let merged =
            build_bridge_client_capabilities(Some(&ClientCapabilities::default()), true, false);
        assert!(
            merged
                .workspace
                .as_ref()
                .and_then(|w| w.workspace_edit.as_ref())
                .is_none(),
            "an editor that omits workspaceEdit implies documentChanges:false — do not overclaim"
        );
    }

    #[test]
    fn apply_edit_gated_on_upstream_declaration() {
        use tower_lsp_server::ls_types::WorkspaceClientCapabilities;

        // Editor declares workspace.applyEdit → advertised downstream.
        let upstream = ClientCapabilities {
            workspace: Some(WorkspaceClientCapabilities {
                apply_edit: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        };
        let merged = build_bridge_client_capabilities(Some(&upstream), true, false);
        assert_eq!(
            merged.workspace.as_ref().and_then(|w| w.apply_edit),
            Some(true)
        );

        // Editor silent (or explicit false) → withheld: the bridge could only
        // ever answer applied:false, so inviting applyEdits would overclaim.
        let merged =
            build_bridge_client_capabilities(Some(&ClientCapabilities::default()), true, false);
        assert_eq!(merged.workspace.as_ref().and_then(|w| w.apply_edit), None);

        let upstream_false = ClientCapabilities {
            workspace: Some(WorkspaceClientCapabilities {
                apply_edit: Some(false),
                ..Default::default()
            }),
            ..Default::default()
        };
        let merged = build_bridge_client_capabilities(Some(&upstream_false), true, false);
        assert_eq!(merged.workspace.as_ref().and_then(|w| w.apply_edit), None);
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
    fn merge_propagates_inlay_hint_resolve_properties() {
        use tower_lsp_server::ls_types::{
            InlayHintClientCapabilities, InlayHintResolveClientCapabilities,
            TextDocumentClientCapabilities,
        };

        let properties = vec![
            "tooltip".to_string(),
            "textEdits".to_string(),
            "label.location".to_string(),
            "label.command".to_string(),
        ];
        let upstream = ClientCapabilities {
            text_document: Some(TextDocumentClientCapabilities {
                inlay_hint: Some(InlayHintClientCapabilities {
                    resolve_support: Some(InlayHintResolveClientCapabilities {
                        properties: properties.clone(),
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };

        let merged = build_bridge_client_capabilities(Some(&upstream), true, false);
        assert_eq!(
            merged
                .text_document
                .as_ref()
                .and_then(|td| td.inlay_hint.as_ref())
                .and_then(|hint| hint.resolve_support.as_ref())
                .map(|support| support.properties.as_slice()),
            Some(properties.as_slice())
        );
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
            ResourceOperationKind, SignatureHelpClientCapabilities, SignatureInformationSettings,
            TagSupport, TextDocumentClientCapabilities, WorkspaceClientCapabilities,
            WorkspaceEditClientCapabilities,
        };

        // Simulate typical Neovim capabilities
        let upstream = ClientCapabilities {
            workspace: Some(WorkspaceClientCapabilities {
                // Neovim's default make_client_capabilities() really does omit
                // `documentChanges` — it advertises only resourceOperations
                // (in this exact order) even though apply_workspace_edit
                // handles documentChanges. Mirrored faithfully.
                workspace_edit: Some(WorkspaceEditClientCapabilities {
                    resource_operations: Some(vec![
                        ResourceOperationKind::Rename,
                        ResourceOperationKind::Create,
                        ResourceOperationKind::Delete,
                    ]),
                    ..Default::default()
                }),
                // Neovim's default capabilities declare applyEdit.
                apply_edit: Some(true),
                ..Default::default()
            }),
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
    fn capability_override_wins_over_upstream_merge() {
        use serde_json::json;
        use tower_lsp_server::ls_types::WindowClientCapabilities;

        // The editor supports progress, so the merge advertises it — the
        // user's override must still win (user is the last merge layer).
        let upstream = ClientCapabilities {
            window: Some(WindowClientCapabilities {
                work_done_progress: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        };
        let merged = build_bridge_client_capabilities(Some(&upstream), true, false);
        let mut capabilities = serde_json::to_value(&merged).unwrap();

        apply_capability_override(
            &mut capabilities,
            &json!({"window": {"workDoneProgress": false}}),
        );

        assert_eq!(
            capabilities.pointer("/window/workDoneProgress"),
            Some(&json!(false)),
            "the user override must mask the upstream-propagated capability"
        );
        assert!(
            capabilities.pointer("/textDocument/completion").is_some(),
            "sibling baseline capabilities must survive the deep merge"
        );
    }

    #[test]
    fn capability_override_cannot_touch_position_encodings() {
        use serde_json::json;

        let merged = build_bridge_client_capabilities(None, true, false);
        let mut capabilities = serde_json::to_value(&merged).unwrap();

        apply_capability_override(
            &mut capabilities,
            &json!({
                "general": {"positionEncodings": ["utf-8"]},
                "window": {"workDoneProgress": false}
            }),
        );

        assert_eq!(
            capabilities.pointer("/general/positionEncodings"),
            Some(&json!(["utf-16"])),
            "positionEncodings is bridge-load-bearing: an override would \
             silently corrupt every coordinate translation"
        );
        assert_eq!(
            capabilities.pointer("/window/workDoneProgress"),
            Some(&json!(false)),
            "the rest of the override must still apply"
        );
    }

    /// The positionEncodings invariant must hold for every override shape,
    /// not just an object-shaped `general`: a non-object `general` (or a JSON
    /// null arriving via didChangeConfiguration) replaces the whole subtree in
    /// a deep merge, which used to bypass an input-filter-style guard.
    #[test]
    fn position_encodings_survive_non_object_general_overrides() {
        use serde_json::json;

        for hostile_general in [json!("utf-8"), json!(null), json!(["utf-8"]), json!(5)] {
            let merged = build_bridge_client_capabilities(None, true, false);
            let mut capabilities = serde_json::to_value(&merged).unwrap();

            apply_capability_override(
                &mut capabilities,
                &json!({"general": hostile_general, "window": {"workDoneProgress": false}}),
            );

            assert_eq!(
                capabilities.pointer("/general/positionEncodings"),
                Some(&json!(["utf-16"])),
                "a non-object general ({hostile_general}) must not delete positionEncodings"
            );
            assert_eq!(
                capabilities.pointer("/window/workDoneProgress"),
                Some(&json!(false)),
                "the rest of the override must still apply"
            );
        }
    }

    /// `changeAnnotationSupport` is deliberately withheld from the upstream
    /// mirror (ls-types drops `annotationId`, so `needsConfirmation` would be
    /// silently lost); an override must not be able to re-open that hole —
    /// neither on a mirrored `workspaceEdit` nor by creating one from scratch.
    #[test]
    fn change_annotation_support_cannot_be_reopened_by_override() {
        use serde_json::json;
        use tower_lsp_server::ls_types::{
            ChangeAnnotationWorkspaceEditClientCapabilities, WorkspaceClientCapabilities,
            WorkspaceEditClientCapabilities,
        };

        let annotation_override = json!({
            "workspace": {"workspaceEdit": {"changeAnnotationSupport": {"groupsOnLabel": true}}}
        });

        // Upstream mirrors workspaceEdit (annotations withheld by the merge).
        let upstream = ClientCapabilities {
            workspace: Some(WorkspaceClientCapabilities {
                workspace_edit: Some(WorkspaceEditClientCapabilities {
                    document_changes: Some(true),
                    change_annotation_support: Some(
                        ChangeAnnotationWorkspaceEditClientCapabilities {
                            groups_on_label: Some(true),
                        },
                    ),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let merged = build_bridge_client_capabilities(Some(&upstream), true, false);
        let mut capabilities = serde_json::to_value(&merged).unwrap();
        apply_capability_override(&mut capabilities, &annotation_override);
        assert_eq!(
            capabilities.pointer("/workspace/workspaceEdit/changeAnnotationSupport"),
            None,
            "the override must not re-advertise annotation support the mirror withheld"
        );
        assert_eq!(
            capabilities.pointer("/workspace/workspaceEdit/documentChanges"),
            Some(&json!(true)),
            "sibling workspaceEdit fields must survive the strip"
        );

        // No upstream workspaceEdit: the override creating one from scratch is
        // stripped of the annotation key all the same.
        let merged = build_bridge_client_capabilities(None, true, false);
        let mut capabilities = serde_json::to_value(&merged).unwrap();
        apply_capability_override(&mut capabilities, &annotation_override);
        assert_eq!(
            capabilities.pointer("/workspace/workspaceEdit/changeAnnotationSupport"),
            None,
        );
    }

    /// The spawn-time user warnings must mirror what enforcement does: one
    /// message per problem, none for a benign override.
    #[test]
    fn override_user_warnings_cover_each_enforced_condition() {
        use serde_json::json;

        assert_eq!(
            capability_override_user_warnings(&json!("nope"), false).len(),
            1,
            "a non-object override warns exactly once (it is ignored wholesale)"
        );
        assert!(
            capability_override_user_warnings(
                &json!({"window": {"workDoneProgress": false}}),
                false
            )
            .is_empty(),
            "a benign override must not warn"
        );

        let noisy = json!({
            "general": {"positionEncodings": ["utf-8"]},
            "workspace": {
                "workspaceEdit": {"changeAnnotationSupport": {"groupsOnLabel": true}},
                "configuration": true
            }
        });
        let warnings = capability_override_user_warnings(&noisy, false);
        assert_eq!(
            warnings.len(),
            3,
            "positionEncodings + changeAnnotationSupport + configuration conflict: {warnings:?}"
        );

        assert_eq!(
            capability_override_user_warnings(&json!({"general": "utf-8"}), false).len(),
            1,
            "a non-object general warns about the protected encoding"
        );
        assert!(
            capability_override_user_warnings(&json!({"workspace": {"configuration": true}}), true)
                .is_empty(),
            "configuration matching the gate is not a conflict"
        );
        assert!(
            capability_override_user_warnings(
                &json!({"general": {"positionEncodings": ["utf-16"]}}),
                false
            )
            .is_empty(),
            "restating the enforced utf-16 baseline is a no-op, not a conflict"
        );

        // The conflict check must see through non-boolean displacement, not
        // just boolean leaves: null/scalar shapes turn the capability off too.
        assert_eq!(
            capability_override_user_warnings(&json!({"workspace": null}), true).len(),
            1,
            "a non-object workspace wholesale-drops advertised capabilities"
        );
        assert_eq!(
            capability_override_user_warnings(&json!({"workspace": {"configuration": null}}), true)
                .len(),
            1,
            "configuration:null displaces the advertised true as surely as false"
        );
        assert!(
            capability_override_user_warnings(
                &json!({"workspace": {"configuration": null}}),
                false
            )
            .is_empty(),
            "configuration:null where nothing was advertised changes nothing"
        );
    }

    /// A non-object override at the root would replace the entire capabilities
    /// object in a deep merge; it must be ignored wholesale.
    #[test]
    fn non_object_override_is_ignored() {
        use serde_json::json;

        let merged = build_bridge_client_capabilities(None, true, false);
        let mut capabilities = serde_json::to_value(&merged).unwrap();
        let untouched = capabilities.clone();

        apply_capability_override(&mut capabilities, &json!("nope"));

        assert_eq!(
            capabilities, untouched,
            "a scalar override must leave the advertised capabilities unchanged"
        );
    }

    #[test]
    fn capability_override_passes_through_unknown_fields() {
        use serde_json::json;

        // The override is merged at the JSON layer so fields the typed
        // ClientCapabilities doesn't model are advertised verbatim instead of
        // being silently dropped by a typed round-trip.
        let merged = build_bridge_client_capabilities(None, true, false);
        let mut capabilities = serde_json::to_value(&merged).unwrap();

        apply_capability_override(
            &mut capabilities,
            &json!({"workspace": {"futureCapability": {"nested": true}}}),
        );

        assert_eq!(
            capabilities.pointer("/workspace/futureCapability/nested"),
            Some(&json!(true)),
        );
        assert_eq!(
            capabilities.pointer("/workspace/workspaceFolders"),
            Some(&json!(true)),
            "existing workspace keys must survive alongside the addition"
        );
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

    #[test]
    fn bridge_routing_capability_is_always_advertised() {
        for experimental in [false, true] {
            let capabilities = build_bridge_client_capabilities(None, false, experimental);
            assert_eq!(
                capabilities
                    .experimental
                    .as_ref()
                    .and_then(|value| value.get("kakehashi"))
                    .and_then(|value| value.get("bridgeRouting")),
                Some(&serde_json::Value::Bool(true)),
            );
        }
    }

    #[test]
    fn merge_preserves_upstream_experimental_extensions() {
        let upstream = ClientCapabilities {
            experimental: Some(serde_json::json!({
                "editorFeature": {"enabled": true},
                "kakehashi": {"other": "preserved"},
            })),
            ..Default::default()
        };

        assert_eq!(
            build_bridge_client_capabilities(Some(&upstream), false, false).experimental,
            Some(serde_json::json!({
                "editorFeature": {"enabled": true},
                "kakehashi": {
                    "other": "preserved",
                    "bridgeRouting": true,
                },
            })),
        );
    }

    #[test]
    fn merge_preserves_non_object_upstream_experimental_value_and_advertises_routing() {
        let upstream = ClientCapabilities {
            experimental: Some(serde_json::json!("editor-extension-payload")),
            ..Default::default()
        };

        assert_eq!(
            build_bridge_client_capabilities(Some(&upstream), false, false).experimental,
            Some(serde_json::json!({
                "kakehashi": {"bridgeRouting": true},
                "upstream": "editor-extension-payload",
            })),
        );
    }
}
