//! Lifecycle methods for Kakehashi (initialize, initialized, shutdown).

use std::sync::Arc;

use tower_lsp_server::Client;
use tower_lsp_server::jsonrpc::{Error, ErrorCode, Result};
use tower_lsp_server::ls_types::ColorProviderCapability;
use tower_lsp_server::ls_types::{
    ClientCapabilities, CodeActionOptions, CodeActionProviderCapability, CodeLensOptions,
    CompletionOptions, DeclarationCapability, DeclarationOptions, DefinitionOptions,
    DiagnosticOptions, DiagnosticServerCapabilities, DocumentFormattingOptions,
    DocumentLinkOptions, DocumentOnTypeFormattingOptions, DocumentRangeFormattingOptions,
    DocumentSymbolOptions, ExecuteCommandOptions, FoldingRangeProviderCapability,
    HoverProviderCapability, ImplementationProviderCapability, InitializeParams, InitializeResult,
    InitializedParams, InlayHintOptions, InlayHintServerCapabilities,
    LinkedEditingRangeServerCapabilities, OneOf, PositionEncodingKind, ReferenceOptions,
    RenameOptions, SaveOptions, SelectionRangeProviderCapability, SemanticTokenModifier,
    SemanticTokenType, SemanticTokensFullOptions, SemanticTokensLegend, SemanticTokensOptions,
    SemanticTokensServerCapabilities, ServerCapabilities, ServerInfo, SignatureHelpOptions,
    TextDocumentSyncCapability, TextDocumentSyncKind, TextDocumentSyncOptions,
    TextDocumentSyncSaveOptions, TypeDefinitionProviderCapability, Uri, WorkDoneProgressOptions,
    WorkspaceFoldersServerCapabilities, WorkspaceServerCapabilities,
};
use url::Url;

use crate::analysis::{LEGEND_MODIFIERS, LEGEND_TYPES};
use crate::config::WorkspaceSettings;
use crate::lsp::client::check_semantic_tokens_refresh_support;
use crate::lsp::{SettingsSource, load_settings};

use super::apply_edit_translation::ApplyEditTranslator;
use super::show_document_translation::ShowDocumentTranslator;
use super::{Kakehashi, uri_to_url};

// LSP `RequestFailed`; ls-types does not currently expose this error code.
const REQUEST_FAILED_ERROR_CODE: i64 = -32803;

fn configuration_load_error(message: String) -> Error {
    Error {
        code: ErrorCode::ServerError(REQUEST_FAILED_ERROR_CODE),
        message: message.into(),
        data: None,
    }
}

/// Translators for downstream-initiated request payloads that carry
/// virtual-document coordinates, bundled so the forwarding loop threads one
/// handle. Both are built from the same shared (cheaply cloneable) service
/// handles; `None` for the whole bundle means "forward verbatim" (test loops).
struct UpstreamRequestTranslators {
    show_document: ShowDocumentTranslator,
    apply_edit: ApplyEditTranslator,
}

fn lsp_legend_types() -> Vec<SemanticTokenType> {
    LEGEND_TYPES
        .iter()
        .map(|t| SemanticTokenType::new(t.as_str()))
        .collect()
}

fn lsp_legend_modifiers() -> Vec<SemanticTokenModifier> {
    LEGEND_MODIFIERS
        .iter()
        .map(|m| SemanticTokenModifier::new(m.as_str()))
        .collect()
}

fn host_position_encoding(capabilities: &ClientCapabilities) -> Option<PositionEncodingKind> {
    capabilities
        .general
        .as_ref()
        .and_then(|general| general.position_encodings.as_ref())
        .map(|_| PositionEncodingKind::UTF16)
}

/// The workspace root the upstream client supplied, in the precedence order LSP
/// defines: `workspaceFolders[0]`, then the deprecated `rootUri`, then the
/// deprecated `rootPath`.
///
/// `None` means the client opened no workspace at all — a state clients choose
/// deliberately (single-file sessions, embedded clients). The forwarded
/// handshake must preserve it: inventing a root there is what #742 reported.
/// Config discovery may still fall back to the process CWD for its own root,
/// which the startup log names, because that root anchors Kakehashi's relative
/// paths and never reaches a downstream server.
///
/// The two consumers differ in what they do with a root once found, which is
/// why this shares the ladder and not the conversion.
enum ClientRoot<'a> {
    /// The first entry of `workspaceFolders`.
    WorkspaceFolder(&'a Uri),
    /// The deprecated `rootUri`.
    RootUri(&'a Uri),
    /// The deprecated `rootPath`, which is a filesystem path and not a URI.
    LegacyPath(&'a str),
}

impl ClientRoot<'_> {
    /// How this root's origin reads in the startup log.
    fn source(&self) -> &'static str {
        match self {
            Self::WorkspaceFolder(_) => "workspace folders",
            Self::RootUri(_) => "root_uri (deprecated)",
            Self::LegacyPath(_) => "root_path (deprecated)",
        }
    }

    /// The root as a filesystem path, for Kakehashi's own config discovery.
    fn to_file_path(&self) -> Option<std::path::PathBuf> {
        match self {
            Self::WorkspaceFolder(uri) | Self::RootUri(uri) => {
                uri_to_url(uri).ok().and_then(|url| url.to_file_path().ok())
            }
            // Only an absolute `rootPath` can anchor relative config paths; a
            // relative one would resolve against the launch directory, which is
            // the dependence this handshake exists to avoid.
            Self::LegacyPath(path) => {
                let path = std::path::Path::new(path);
                path.is_absolute().then(|| path.to_path_buf())
            }
        }
    }
}

/// Pick the root from the workspace inputs the upstream client actually sent.
///
/// An empty `workspaceFolders` does not suppress the deprecated fields, so
/// `{workspaceFolders: [], rootUri: X}` still roots at `X`. Reviewers read that
/// pair as contradictory and propose making the empty list authoritative;
/// it costs more than it looks. LSP gives `[]` no meaning distinct from `null`
/// ("supports folders, none configured"), while `rootUri` is documented as null
/// when no folder is open — so a client that meant "no workspace" left the field
/// that says it unused. And because this ladder also feeds config discovery,
/// suppressing `X` would not leave that root empty: it would fall through to the
/// process CWD, trading a location the client named for the launch directory
/// #742 exists to stop depending on.
fn client_root(params: &InitializeParams) -> Option<ClientRoot<'_>> {
    if let Some(folder) = params
        .workspace_folders
        .as_ref()
        .and_then(|folders| folders.first())
    {
        return Some(ClientRoot::WorkspaceFolder(&folder.uri));
    }
    client_root_without_folders(params)
}

/// The rungs of [`client_root`]'s ladder below `workspaceFolders`.
///
/// Split out because the folder list is the one input that changes after
/// `initialize`: when `workspace/didChangeWorkspaceFolders` empties it, the
/// session's root is whatever it would have been had the client never sent a
/// folder, and that is exactly this ladder.
#[allow(deprecated)]
fn client_root_without_folders(params: &InitializeParams) -> Option<ClientRoot<'_>> {
    if let Some(uri) = params.root_uri.as_ref() {
        return Some(ClientRoot::RootUri(uri));
    }
    params.root_path.as_deref().map(ClientRoot::LegacyPath)
}

/// Kakehashi's own root, paired with the origin the startup log reports: the
/// client's root when it named an anchorable one, else the process CWD.
///
/// This is the fallback the bridge side deliberately does not have. It anchors
/// relative config paths and `kakehashi.toml` discovery and never reaches a
/// downstream server, so a no-workspace session forwards nothing while still
/// resolving Kakehashi's own configuration.
fn config_root_path(root: Option<ClientRoot<'_>>) -> (Option<std::path::PathBuf>, &'static str) {
    match root.and_then(|root| root.to_file_path().map(|path| (path, root.source()))) {
        Some((path, source)) => (Some(path), source),
        None => (
            std::env::current_dir().ok(),
            "current working directory (fallback)",
        ),
    }
}

/// Kakehashi's root once the client's folder list has changed: the current first
/// folder, else `folderless` — the rungs `initialize` resolved below
/// `workspaceFolders`.
///
/// Deliberately **not** [`config_root_path`]: this ladder stops at roots the
/// client named and never reaches the process CWD. That last rung exists so a
/// session opened with no workspace at all can still find a configuration; it is
/// a property of how the server was launched, not of the workspace. Migrating an
/// established session to the launch directory — where an unrelated
/// `kakehashi.toml` may name parser libraries to load — is not something closing
/// a folder should do. A client that named no other root gets no project layer,
/// which is what it had before it opened the folder.
pub(super) fn config_root_after_folder_change(
    folder: Option<&Uri>,
    folderless: Option<std::path::PathBuf>,
) -> Option<std::path::PathBuf> {
    match folder {
        Some(uri) => ClientRoot::WorkspaceFolder(uri).to_file_path(),
        None => folderless,
    }
}

/// Derive a root URI only from workspace inputs the upstream client supplied,
/// for downstream initialization. Kakehashi may use its process CWD internally
/// for config discovery, but forwarding that fallback would turn a
/// no-workspace session into an unrelated workspace for every bridged server.
fn bridge_root_uri(params: &InitializeParams) -> Option<String> {
    match client_root(params)? {
        ClientRoot::WorkspaceFolder(uri) | ClientRoot::RootUri(uri) => {
            Some(uri.as_str().to_string())
        }
        // `Url::from_file_path` rejects a relative path, so an unanchorable
        // `rootPath` forwards no workspace rather than one built from the CWD.
        ClientRoot::LegacyPath(path) => Url::from_file_path(path).ok().map(|uri| uri.to_string()),
    }
}

/// Forward the client's `workspaceFolders` verbatim — including an empty list,
/// which says "no folders" as deliberately as a missing one — and otherwise
/// synthesize the single folder that folder-only downstream servers expect from
/// `root_uri`. Synthesizing translates a root the client did supply; it never
/// invents one, so a no-workspace session still forwards nothing.
///
/// `root_uri` must be [`bridge_root_uri`]'s result for the same `params`.
///
/// The folder name mirrors `root_markers::workspace_at_root`, except that this
/// falls back to a fixed name where that one falls back to the whole URI: the
/// name here is only ever shown to a downstream server.
fn bridge_workspace_folders(
    params: &InitializeParams,
    root_uri: Option<&str>,
) -> Option<Vec<tower_lsp_server::ls_types::WorkspaceFolder>> {
    use std::str::FromStr as _;
    params.workspace_folders.clone().or_else(|| {
        root_uri.and_then(|uri| {
            let name = Url::parse(uri)
                .ok()
                .and_then(|url| {
                    url.to_file_path()
                        .ok()
                        .and_then(|path| {
                            path.file_name().and_then(|s| s.to_str().map(String::from))
                        })
                        .or_else(|| {
                            url.path_segments()
                                .and_then(|mut seg| seg.next_back().map(|s| s.to_string()))
                        })
                })
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "workspace".to_string());
            let folder_uri = Uri::from_str(uri).ok()?;
            Some(vec![tower_lsp_server::ls_types::WorkspaceFolder {
                uri: folder_uri,
                name,
            }])
        })
    })
}

fn workspace_server_capabilities() -> WorkspaceServerCapabilities {
    WorkspaceServerCapabilities {
        workspace_folders: Some(WorkspaceFoldersServerCapabilities {
            supported: Some(true),
            change_notifications: Some(OneOf::Left(true)),
        }),
        file_operations: None,
    }
}

impl Kakehashi {
    pub(crate) async fn initialize_impl(
        &self,
        params: InitializeParams,
    ) -> Result<InitializeResult> {
        // Reject an unusable `--config-file` before anything from `params` is
        // latched. Several of the stores below are first-write-wins
        // (`set_capabilities`, `set_folderless_root_path`), and
        // tower-lsp-server resets to `Uninitialized` after an error response,
        // so a client may fix the file and retry: without this, the retry would
        // load the corrected settings while downstream servers kept the failed
        // attempt's capabilities, root URI, and workspace folders.
        //
        // Reported only through this response — the settings events carrying
        // the same text are never sent, so the client does not also get a
        // `window/showMessage` popup on top of a handshake it already failed.
        // Pinned by `test_config_file_fatal_error_is_not_also_shown_as_a_message`.
        //
        // The files are read here and the result carried into `load_settings`
        // below, never re-read: a `--config-file` may name a stream, and a file
        // swapped between two reads would slip past whichever check ran first.
        let explicit_config =
            crate::lsp::settings::load_explicit_config(self.home_dir.as_deref(), |var| {
                std::env::var(var).ok()
            });
        if let Some(error) = explicit_config
            .as_ref()
            .and_then(|config| config.fatal_error.clone())
        {
            return Err(configuration_load_error(error));
        }

        let position_encoding = host_position_encoding(&params.capabilities);
        // Store client capabilities for LSP compliance checks (e.g., refresh support).
        // Uses SettingsManager which wraps OnceLock for "set once, read many" semantics.
        self.settings_manager
            .set_capabilities(params.capabilities.clone());

        // Log capability state for troubleshooting client compatibility issues.
        log::debug!(
            "Client capabilities stored: semantic_tokens_refresh={}",
            check_semantic_tokens_refresh_support(&params.capabilities)
        );

        // Client-facing startup logs are held until settings are resolved and
        // applied, so initializationOptions can suppress them with the same
        // global policy as every subsequent internal message.
        let mut startup_logs = vec![(
            tower_lsp_server::ls_types::MessageType::INFO,
            "Received initialization request".to_string(),
        )];

        // Preserve the upstream workspace contract for downstream servers. The
        // separate internal root path below may still fall back to the CWD.
        let root_uri_for_bridge = bridge_root_uri(&params);

        // Resolved here, into owned values, because `params.capabilities` is
        // moved into the pool below and that ends any borrow of `params`.
        let (root_path, source) = config_root_path(client_root(&params));
        // The root a later `didChangeWorkspaceFolders` falls back to once it
        // empties the folder list. Resolved here because `params` does not
        // outlive this request, and deliberately without `config_root_path`'s
        // process-CWD rung: a session that started folderless may load the
        // launch directory's config, while one that lost its last folder gets
        // no project layer — see `config_root_after_folder_change`.
        self.settings_manager.set_folderless_root_path(
            client_root_without_folders(&params).and_then(|root| root.to_file_path()),
        );

        // Forward root_uri and workspace_folders to bridge pool for downstream server initialization
        let workspace_folders_for_bridge =
            bridge_workspace_folders(&params, root_uri_for_bridge.as_deref());
        self.bridge.pool().set_root_uri(root_uri_for_bridge);
        self.bridge
            .pool()
            .set_workspace_folders(workspace_folders_for_bridge);
        // Clients without codeActionLiteralSupport only understand
        // `Command[]` responses. The bridge surfaces CodeAction literals and
        // cannot guarantee a Command-only response (bare downstream Commands
        // stay bare — renamed for routing — but literal actions are never
        // downgraded to Commands), so withhold the capability for such
        // clients (#568).
        let client_supports_code_action_literals = params
            .capabilities
            .text_document
            .as_ref()
            .and_then(|td| td.code_action.as_ref())
            .and_then(|ca| ca.code_action_literal_support.as_ref())
            .is_some();
        self.bridge
            .pool()
            .set_client_capabilities(params.capabilities);

        // Store root path for later use and log the source
        if let Some(ref path) = root_path {
            startup_logs.push((
                tower_lsp_server::ls_types::MessageType::INFO,
                format!("Using workspace root from {}: {}", source, path.display()),
            ));
            self.settings_manager.set_root_path(Some(path.clone()));
        } else {
            startup_logs.push((
                tower_lsp_server::ls_types::MessageType::WARNING,
                "Failed to determine workspace root - config file will not be loaded".to_string(),
            ));
        }

        let root_path = self.settings_manager.root_path().as_ref().clone();
        let initialization_options = params.initialization_options;
        let settings_outcome = load_settings(
            root_path.as_deref(),
            initialization_options
                .clone()
                .map(|options| (SettingsSource::InitializationOptions, options)),
            self.home_dir.as_deref(),
            |var| std::env::var(var).ok(),
            explicit_config,
        );

        // There is deliberately no second fatal check here. Every verdict on
        // the explicit configuration was reached above, before any of the
        // stores in between — and the files must not be read again to reach
        // one, since a `--config-file` may name a stream.
        let settings_events = settings_outcome.events;
        let mut default_settings_warning = None;

        // Nudge users off deprecated config keys. Each claim guard latches
        // session-wide so a later didChangeConfiguration carrying the same key
        // does not warn a second time (and vice versa). The two keys claim
        // independently: seeing one must not suppress the other.
        if settings_outcome.deprecated_keys.root_markers
            && self
                .settings_manager
                .claim_root_markers_deprecation_warning()
        {
            self.notifier()
                .show_warning(crate::config::deprecation::ROOT_MARKERS_DEPRECATION_NOTICE)
                .await;
        }
        if settings_outcome.deprecated_keys.auto_install
            && self
                .settings_manager
                .claim_auto_install_deprecation_warning()
        {
            self.notifier()
                .show_warning(crate::config::deprecation::AUTO_INSTALL_DEPRECATION_NOTICE)
                .await;
        }
        // Same shape for the empty-container rule change: what the layers said
        // still parses, and now means something else.
        if let Some(notice) =
            crate::lsp::settings::emptied_container_notice(settings_outcome.raw_settings.as_ref())
            && self
                .settings_manager
                .claim_empty_container_migration_warning()
        {
            self.notifier().show_warning(notice).await;
        }

        // Always apply settings (use defaults if none were loaded)
        // This ensures auto_install=true, default capture_mappings, and other defaults are active
        // for zero-config experience. Use default_settings() instead of RawWorkspaceSettings::default()
        // because the derived Default creates empty capture_mappings while default_settings() includes
        // the full default capture_mappings (markup.strong → "", etc.)
        let (raw_settings, settings) = if let Some(s) = settings_outcome.settings {
            (
                settings_outcome
                    .raw_settings
                    .unwrap_or_else(|| crate::config::RawWorkspaceSettings::from(&s)),
                s,
            )
        } else {
            let raw_settings = crate::config::defaults::default_settings();
            let settings = match WorkspaceSettings::try_from_settings(
                &raw_settings,
                self.home_dir.as_deref(),
                crate::config::expand::with_kakehashi_defaults(|var| std::env::var(var).ok()),
            ) {
                Ok(ws) => ws,
                Err(e) => {
                    log::error!(
                        "Failed to expand default settings: {e}. Falling back to empty defaults."
                    );
                    default_settings_warning = Some(format!(
                        "Failed to expand default settings: {e}. Some features (e.g., semantic highlighting, parser detection) may be degraded."
                    ));
                    WorkspaceSettings::default()
                }
            };
            (raw_settings, settings)
        };
        self.client_settings_override.store(
            initialization_options
                .and_then(|value| serde_json::from_value(value).ok())
                .map(std::sync::Arc::new),
        );
        // Derive the onTypeFormatting trigger union before settings move into
        // apply_raw_settings: kakehashi cannot know downstream trigger
        // characters at initialize time (servers spawn lazily), so the
        // advertised set is config-driven (#354). No config → None →
        // capability not advertised, matching previous behavior.
        let on_type_formatting_triggers =
            crate::config::settings::on_type_formatting_trigger_union(&settings.language_servers);
        // Gate the save capabilities (#357), computed before `settings` moves
        // into apply_raw_settings (like the trigger union above):
        // - willSave now fans out to BOTH host AND virt bridges, so advertise it
        //   whenever any runnable bridge server is configured (the built-in `_`
        //   defaults entry has an empty cmd and doesn't count) — otherwise the
        //   editor never sends a willSave for virt servers to react to.
        // - willSaveWaitUntil stays host-only (its edits would need virtual→host
        //   translation + cross-region aggregation), so it keeps the stricter
        //   host-bridging gate; advertising it without a host bridge would block
        //   every save on a round trip that can only return "no edits".
        let host_bridging_enabled = settings.any_host_bridging_enabled();
        let will_save_advertised = host_bridging_enabled || settings.any_bridge_server_runnable();
        self.apply_initial_settings(raw_settings, settings).await;

        let notifier = self.notifier();
        for (level, message) in startup_logs {
            notifier.log(level, message).await;
        }
        notifier.log_settings_events(&settings_events).await;
        if let Some(message) = default_settings_warning {
            notifier.log_warning(message).await;
        }
        notifier.log_info("server initialized!").await;
        Ok(InitializeResult {
            server_info: Some(ServerInfo {
                name: "kakehashi".to_string(),
                version: Some(env!("CARGO_PKG_VERSION").to_string()),
            }),
            offset_encoding: None,
            capabilities: ServerCapabilities {
                position_encoding,
                text_document_sync: Some(TextDocumentSyncCapability::Options(
                    TextDocumentSyncOptions {
                        open_close: Some(true),
                        change: Some(TextDocumentSyncKind::INCREMENTAL),
                        // willSave: any bridge (host or virt) may consume it.
                        // willSaveWaitUntil: host-only (#357).
                        will_save: will_save_advertised.then_some(true),
                        will_save_wait_until: host_bridging_enabled.then_some(true),
                        save: Some(TextDocumentSyncSaveOptions::SaveOptions(SaveOptions {
                            include_text: Some(false),
                        })),
                    },
                )),
                semantic_tokens_provider: Some(
                    SemanticTokensServerCapabilities::SemanticTokensOptions(
                        SemanticTokensOptions {
                            legend: SemanticTokensLegend {
                                token_types: lsp_legend_types(),
                                token_modifiers: lsp_legend_modifiers(),
                            },
                            full: Some(SemanticTokensFullOptions::Delta { delta: Some(true) }),
                            range: Some(true),
                            ..Default::default()
                        },
                    ),
                ),
                selection_range_provider: Some(SelectionRangeProviderCapability::Simple(true)),
                // Advertise `workDoneProgress` so clients attach a `workDoneToken`
                // we can bridge (ls-bridge-client-progress, #445). Per LSP this is
                // unconditional — client-initiated progress has no client
                // capability; the provider's advertisement alone prompts the token
                // (`window.workDoneProgress` governs *server*-initiated progress, a
                // different mechanism). NOTE: `type_definition`/`implementation`
                // also have the plumbing, but cannot advertise it via this crate's
                // typed API — in ls-types 0.0.6 their only `Options` variant wraps
                // `StaticTextDocumentRegistrationOptions`, which has no
                // `workDoneProgress` field (the LSP spec *does* define it). They
                // stay `Simple(true)`, so their client-progress plumbing is inert
                // for spec-compliant clients until that crate gap is closed (#447).
                declaration_provider: Some(DeclarationCapability::Options(DeclarationOptions {
                    work_done_progress_options: WorkDoneProgressOptions {
                        work_done_progress: Some(true),
                    },
                })),
                definition_provider: Some(OneOf::Right(DefinitionOptions {
                    work_done_progress_options: WorkDoneProgressOptions {
                        work_done_progress: Some(true),
                    },
                })),
                type_definition_provider: Some(TypeDefinitionProviderCapability::Simple(true)),
                implementation_provider: Some(ImplementationProviderCapability::Simple(true)),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                completion_provider: Some(CompletionOptions {
                    trigger_characters: Some(vec![".".to_string(), ":".to_string()]),
                    resolve_provider: Some(true),
                    ..Default::default()
                }),
                signature_help_provider: Some(SignatureHelpOptions {
                    trigger_characters: Some(vec!["(".to_string(), ",".to_string()]),
                    retrigger_characters: Some(vec![",".to_string()]),
                    ..Default::default()
                }),
                references_provider: Some(OneOf::Right(ReferenceOptions {
                    work_done_progress_options: WorkDoneProgressOptions {
                        work_done_progress: Some(true),
                    },
                })),
                document_highlight_provider: Some(OneOf::Left(true)),
                document_link_provider: Some(DocumentLinkOptions {
                    resolve_provider: None,
                    work_done_progress_options: WorkDoneProgressOptions::default(),
                }),
                // Advertise workDoneProgress so spec-compliant clients attach a
                // `workDoneToken` — the bridge relays the fanned-out regions'
                // `$/progress` onto it (ls-bridge-client-progress, #450).
                document_symbol_provider: Some(OneOf::Right(DocumentSymbolOptions {
                    label: None,
                    work_done_progress_options: WorkDoneProgressOptions {
                        work_done_progress: Some(true),
                    },
                })),
                folding_range_provider: Some(FoldingRangeProviderCapability::Simple(true)),
                // codeLens/resolve is routed to the origin downstream server
                // via the envelope in lens.data (#355, see
                // bridge/text_document/code_lens.rs).
                // `resolveProvider: true` lets clients resolve lazy actions
                // (rust-analyzer-style) via `codeAction/resolve`, routed back
                // to the origin downstream server by the envelope in
                // `action.data` (#568 PR 4). Still no `codeActionKinds`:
                // narrowing kinds would stop clients from asking at all.
                code_action_provider: client_supports_code_action_literals.then_some(
                    CodeActionProviderCapability::Options(CodeActionOptions {
                        resolve_provider: Some(true),
                        code_action_kinds: None,
                        work_done_progress_options: Default::default(),
                    }),
                ),
                code_lens_provider: Some(CodeLensOptions {
                    resolve_provider: Some(true),
                }),
                // Bridged commands (a `Command` surfaced in a code action) are
                // executed via `workspace/executeCommand`, routed back to their
                // origin server by the encoded command name (#568 PR 6). Gated
                // on the same literal-support condition as `code_action_provider`.
                // No STATIC `commands` here: downstream servers connect lazily so
                // their command names aren't known at initialize. Routed names
                // are now per-CONNECTION rather than per-document
                // (execute-command-routing-token), so the set IS finite — but
                // roots are still discovered lazily, so advertising them remains
                // a deferred follow-up (see that record's Gap section). Each
                // server's RAW command
                // names — those from its static initialize result; a
                // downstream's later dynamic command registrations are not
                // collected — are dynamically registered as it reaches Ready
                // (`UpstreamRequest::RegisterCommands` below, gated on client
                // `dynamicRegistration`), which serves palette-fired commands
                // — via a session-global registry keyed by raw command id. That
                // id carries no workspace context, so when several LIVE
                // connections advertise the same one the dispatcher refuses
                // rather than picking by handshake order (#823); the refusal is
                // reported to the editor, not just logged.
                // Action-embedded commands carry ENCODED per-connection names that
                // are never registered: a client that dispatches an action's
                // command on provider PRESENCE (Neovim's built-in client)
                // executes them regardless; one that only dispatches command ids
                // from registered lists (VS Code's vscode-languageclient) still
                // shows such an action without running its command — a known
                // limitation.
                execute_command_provider: client_supports_code_action_literals.then(|| {
                    ExecuteCommandOptions {
                        commands: vec![],
                        work_done_progress_options: Default::default(),
                    }
                }),
                rename_provider: Some(OneOf::Right(RenameOptions {
                    prepare_provider: Some(true),
                    // Advertise workDoneProgress so spec-compliant clients attach a
                    // workDoneToken, which the bridge relays onto downstream rename
                    // progress (#437, ls-bridge-client-progress).
                    work_done_progress_options: WorkDoneProgressOptions {
                        work_done_progress: Some(true),
                    },
                })),
                document_formatting_provider: Some(OneOf::Right(DocumentFormattingOptions {
                    work_done_progress_options: WorkDoneProgressOptions {
                        work_done_progress: Some(true),
                    },
                })),
                document_range_formatting_provider: Some(OneOf::Right(
                    DocumentRangeFormattingOptions {
                        work_done_progress_options: WorkDoneProgressOptions {
                            work_done_progress: Some(true),
                        },
                    },
                )),
                document_on_type_formatting_provider: on_type_formatting_triggers.map(
                    |(first, more)| DocumentOnTypeFormattingOptions {
                        first_trigger_character: first,
                        more_trigger_character: (!more.is_empty()).then_some(more),
                    },
                ),
                // Advertise workDoneProgress so spec-compliant clients attach a
                // `workDoneToken`; the bridge relays the region's `$/progress` onto
                // it (ls-bridge-client-progress, #455).
                inlay_hint_provider: Some(OneOf::Right(InlayHintServerCapabilities::Options(
                    InlayHintOptions {
                        work_done_progress_options: WorkDoneProgressOptions {
                            work_done_progress: Some(true),
                        },
                        resolve_provider: None,
                    },
                ))),
                linked_editing_range_provider: Some(LinkedEditingRangeServerCapabilities::Simple(
                    true,
                )),
                color_provider: self
                    .experimental_enabled()
                    .then_some(ColorProviderCapability::Simple(true)),
                moniker_provider: Some(OneOf::Left(true)),
                // pull-first-diagnostic-forwarding: Pull-first diagnostic forwarding
                diagnostic_provider: Some(DiagnosticServerCapabilities::Options(
                    DiagnosticOptions {
                        inter_file_dependencies: false,
                        workspace_diagnostics: false,
                        ..Default::default()
                    },
                )),
                workspace: Some(workspace_server_capabilities()),
                experimental: Some(serde_json::json!({
                    "kakehashi": {
                        "wrappedDidChangeConfigurationSettings": true,
                    },
                })),
                ..ServerCapabilities::default()
            },
        })
    }

    pub(crate) async fn initialized_impl(&self, _: InitializedParams) {
        self.notifier().log_info("server is ready").await;

        // Forward downstream-initiated messages to the upstream editor
        // (workspace/applyEdit is answered locally instead when the editor
        // never declared the capability). The reader tasks feed three
        // channels:
        // - unbounded `upstream_rx` (loss-intolerant): DiagnosticRefresh and
        //   work-done progress (create/$progress/forget).
        // - bounded `window_rx` (best-effort, drop-on-full): window/logMessage,
        //   window/showMessage, and telemetry/event.
        // - unbounded `upstream_request_rx` (loss-intolerant): downstream
        //   requests forwarded with a response relayed back
        //   (window/showMessageRequest, window/showDocument,
        //   workspace/applyEdit — though when the editor never declared the
        //   applyEdit capability, the forwarding loop answers applied:false
        //   itself instead of forwarding to the editor).
        if let Some(upstream_rx) = self.bridge.take_upstream_rx()
            && let Some(window_rx) = self.bridge.take_window_rx()
            && let Some(upstream_request_rx) = self.bridge.take_upstream_request_rx()
        {
            let client = self.client.clone();
            let token = self.shutdown_token.clone();
            // Translates downstream-initiated payloads carrying virtual-document
            // coordinates back to the host document before forwarding:
            // window/showDocument URIs + selections (#403) and
            // workspace/applyEdit edits (#568).
            let translators = Some(Arc::new(UpstreamRequestTranslators {
                show_document: ShowDocumentTranslator::new(
                    Arc::clone(&self.documents),
                    Arc::clone(&self.language),
                    Arc::clone(&self.bridge),
                ),
                apply_edit: ApplyEditTranslator::new(
                    Arc::clone(&self.documents),
                    Arc::clone(&self.language),
                    Arc::clone(&self.bridge),
                ),
            }));
            let inbound_request_registry = self.bridge.pool().inbound_request_registry();
            // The single proactive diagnostics publisher: region pushes routed up
            // by the reader resolve to a host + region and republish the merged
            // host set (push-propagation-diagnostic-forwarding).
            let delivery_context = Some(Arc::new(UpstreamDeliveryContext {
                diagnostic_publisher: Arc::new(
                    crate::lsp::lsp_impl::coordinator::DiagnosticPublisher::new(self),
                ),
                settings_manager: Arc::clone(&self.settings_manager),
                injection: self.injection_coordinator(),
            }));
            // LSP conditions workspace/applyEdit on the client capability;
            // resolved once here — client capabilities are fixed after
            // initialize.
            let editor_supports_apply_edit = self
                .settings_manager
                .client_capabilities_lock()
                .get()
                .and_then(|caps| caps.workspace.as_ref())
                .and_then(|w| w.apply_edit)
                .unwrap_or(false);
            tokio::spawn(upstream_forwarding_loop(
                upstream_rx,
                window_rx,
                upstream_request_rx,
                translators,
                inbound_request_registry,
                client,
                delivery_context,
                token,
                editor_supports_apply_edit,
            ));
        }

        // Ask a pull-capable client for its configuration now that the
        // handshake is complete. Editors that send `didChangeConfiguration`
        // with no usable `settings` have no other way to configure kakehashi
        // past `initializationOptions`.
        //
        // Last, deliberately: this awaits a response from the client, and a
        // client that is slow to answer — or never does — must not hold up the
        // forwarding loops above. Nothing follows it, so the only thing such a
        // client delays is its own configuration.
        self.pull_client_configuration().await;
    }

    /// Arm a Unix-signal watcher that reaps the downstream server pool when
    /// the process is terminated WITHOUT the LSP shutdown handshake.
    ///
    /// Editors escalate: Neovim SIGTERMs a server that hasn't exited shortly
    /// after `shutdown`/`exit`, and a user restart can kill it outright. With
    /// no handler, kakehashi dies mid-handshake and its downstream children
    /// are orphaned — observed live as a `with-logging emmylua_ls` wrapper
    /// re-parented to launchd and running for hours, because not every
    /// downstream exits on stdin EOF. On SIGTERM/SIGHUP this runs the same
    /// bounded `shutdown_all` as the graceful path (LSP handshake, then
    /// SIGTERM→SIGKILL escalation, global timeout), then exits with the
    /// conventional 128+signal status. Unlike `shutdown_impl` it does NOT
    /// stop in-process work (forwarding loops, timers) — the process exits
    /// immediately after the reap, so only the effect that outlives the
    /// process matters. A second signal during the reap aborts it and exits
    /// immediately — installing a handler replaces the default kill-now
    /// disposition, so impatient senders must keep working. A SIGKILL still
    /// orphans children — nothing can intercept it — but the escalation path
    /// an editor actually takes starts with SIGTERM, which this converts
    /// into a clean reap.
    ///
    /// Server mode only (the one-shot CLI has no downstream pool worth a
    /// watcher); `pub` because the binary arms it before serving.
    #[cfg(unix)]
    pub fn spawn_termination_cleanup(&self) {
        let bridge = std::sync::Arc::clone(&self.bridge);
        tokio::spawn(async move {
            use tokio::signal::unix::{SignalKind, signal};
            let (mut term, mut hup) = match (
                signal(SignalKind::terminate()),
                signal(SignalKind::hangup()),
            ) {
                (Ok(term), Ok(hup)) => (term, hup),
                (term, hup) => {
                    log::warn!(
                        "termination cleanup disabled: signal handler install failed \
                         (SIGTERM: {:?}, SIGHUP: {:?})",
                        term.err(),
                        hup.err()
                    );
                    return;
                }
            };
            // Platform signal numbers straight from the SignalKind
            // constructors (tokio exposes the libc constants via
            // `as_raw_value`), so the 128+signum exit status is correct even
            // on a Unix that renumbers them — no hard-coded 1/15.
            let signum = tokio::select! {
                _ = term.recv() => SignalKind::terminate().as_raw_value(),
                _ = hup.recv() => SignalKind::hangup().as_raw_value(),
            };
            log::info!("received signal {signum}: reaping downstream servers before exit");
            // Race the reap against a SECOND signal: the installed handler
            // replaced the default kill-now disposition, so without this arm a
            // repeat SIGTERM during the bounded (~13s worst-case) reap would be
            // silently swallowed. An impatient sender gets an immediate exit;
            // still-running downstreams fall back to their own stdin-EOF /
            // kill handling.
            tokio::select! {
                _ = bridge.shutdown_all() => {}
                _ = term.recv() => {
                    log::warn!("second SIGTERM during reap: exiting immediately");
                }
                _ = hup.recv() => {
                    log::warn!("second SIGHUP during reap: exiting immediately");
                }
            }
            std::process::exit(128 + signum);
        });
    }

    pub(crate) async fn shutdown_impl(&self) -> Result<()> {
        // Abort all synthetic diagnostic tasks (pull-first-diagnostic-forwarding Phase 2)
        self.synthetic_diagnostics.abort_all();

        // Cancel all debounced diagnostic timers (pull-first-diagnostic-forwarding Phase 3)
        self.debounced_diagnostics.cancel_all();

        // Abort all eager-open tasks to prevent orphaned didOpen during shutdown
        self.bridge.abort_all_eager_open();

        // Cancel the upstream forwarding task for deterministic shutdown.
        // Without this, the task only exits when all senders are dropped.
        self.shutdown_token.cancel();

        // Graceful shutdown of all downstream language server connections (ls-bridge-graceful-shutdown)
        // - Transitions to Closing state, sends LSP shutdown/exit handshake
        // - Escalates to SIGTERM/SIGKILL for unresponsive servers (Unix)
        self.bridge.shutdown_all().await;

        // Dump diagnostic-path counters (#533) so a session's refresh amplification
        // (push republishes in → refreshes requested vs sent → pulls answered) is
        // readable without a profiler. `requested - sent` includes refreshes
        // coalesced, gated, or suppressed during shutdown.
        let m = self.diagnostics.metrics_snapshot();
        log::info!(
            target: "kakehashi::diagnostic_metrics",
            "diagnostic path totals: push_republishes={} refreshes_requested={} refreshes_sent={} (not sent: coalesced/gated/shutdown {}) pulls_answered={} mean_pull_us={}",
            m.push_republishes,
            m.refreshes_requested,
            m.refreshes_sent,
            m.refreshes_requested.saturating_sub(m.refreshes_sent),
            m.pulls_answered,
            m.mean_pull_micros(),
        );

        Ok(())
    }
}

/// Upper bound on how long the shared forwarding loop waits for the editor to
/// answer a server→client *request*. The loop is a single FIFO consumer for all
/// connections, so an editor that accepts a request but never replies would
/// otherwise wedge it (and let the unbounded `upstream_tx` channel grow). A
/// generous bound degrades that to a logged timeout without harming normal use.
const UPSTREAM_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Await an editor-bound request on the forwarding loop with a timeout, logging
/// (rather than propagating) both editor-side errors and timeouts — forwarding
/// is best-effort and must never wedge the shared loop. Returns whether the
/// editor acknowledged the request successfully.
async fn forward_upstream_request(
    method: &str,
    request: impl std::future::Future<Output = tower_lsp_server::jsonrpc::Result<()>>,
) -> bool {
    match tokio::time::timeout(UPSTREAM_REQUEST_TIMEOUT, request).await {
        Ok(Ok(())) => true,
        Ok(Err(e)) => {
            log::debug!(
                target: "kakehashi::bridge",
                "{} forwarding failed: {}",
                method, e
            );
            false
        }
        Err(_) => {
            log::warn!(
                target: "kakehashi::bridge",
                "{} forwarding timed out after {:?}; editor did not reply",
                method, UPSTREAM_REQUEST_TIMEOUT
            );
            false
        }
    }
}

/// Forward downstream-initiated messages from language servers to the editor.
///
/// Consumes from three channels (loss-tolerance split, #378) and dispatches them
/// to the LSP client:
/// - `upstream_rx` (unbounded): `DiagnosticRefresh` — forwarded as
///   `workspace/diagnostic/refresh` — the server-declared work-done progress
///   notifications (`CreateWorkDoneProgress`/`Progress`/`ForgetWorkDoneProgress`,
///   window-work-done-progress), and `PublishDiagnostics`/`EvictConnectionDiagnostics`,
///   which may not be lost. Each wake-up drains a capped burst and coalesces
///   same-`(connection, uri)` `PublishDiagnostics` to the latest
///   (`coalesce_upstream_batch`, #426), then records every surviving push in a
///   barrier-delimited run and publishes the final state once per resolved host.
///   Every region slot is retained and barriers remain FIFO, so
///   `publish`↔`evict` order and create-before-progress hold.
/// - `upstream_request_rx` (unbounded): downstream-initiated *requests*
///   (`window/showMessageRequest`, `window/showDocument`,
///   `workspace/applyEdit` — the latter answered `applied: false` locally
///   when the editor never declared the capability) forwarded with the
///   editor's response relayed back; loss-intolerant (a dropped request hangs
///   the downstream). Serviced via [`spawn_upstream_request`] so a slow/human
///   editor never stalls the loop.
/// - `window_rx` (bounded, reader drops on full): threshold-admitted `LogMessage`,
///   unfiltered `ShowMessage`, and `telemetry/event` — best-effort notifications.
///
/// Notification dispatch awaits tower-lsp's internal bounded channel, so a slow
/// editor stalls the loop — but the `biased` select drains the two loss-intolerant
/// channels (`upstream_rx`, then `upstream_request_rx`) before the best-effort
/// `window_rx`, so a `window/*` burst cannot starve `DiagnosticRefresh`, progress,
/// or request forwarding, and the bounded window queue caps memory. The window
/// channel preserves strict FIFO (the window-notification e2e relies on it). The
/// upstream channel preserves **barrier** order — every non-publish notification
/// keeps its position, so create-before-progress holds — while coalescing collapses
/// superseded same-`(connection, uri)` `PublishDiagnostics` within a drained burst
/// and final host aggregates within each barrier-delimited run.
///
/// Exits when:
/// - Either channel is closed (all senders dropped — both senders live in the
///   pool, so they close together at shutdown), OR
/// - The `cancel_token` is cancelled (deterministic shutdown)
// Heterogeneous channels + collaborators threaded into one long-lived loop task;
// bundling them into a struct would just move the list, not shorten it.
struct UpstreamDeliveryContext {
    diagnostic_publisher: Arc<crate::lsp::lsp_impl::coordinator::DiagnosticPublisher>,
    settings_manager: Arc<crate::lsp::settings_manager::SettingsManager>,
    /// Re-opens a respawned connection's virtual documents
    /// (respawn-reopen-derives-its-targets). Lives here because the pool cannot
    /// resolve injections itself — the document store and injection query are
    /// server-side — so the pool signals *when* and this supplies *what*.
    injection: crate::lsp::lsp_impl::coordinator::InjectionCoordinator,
}

#[allow(clippy::too_many_arguments)]
async fn upstream_forwarding_loop(
    mut upstream_rx: tokio::sync::mpsc::UnboundedReceiver<crate::lsp::bridge::UpstreamNotification>,
    mut window_rx: tokio::sync::mpsc::Receiver<crate::lsp::bridge::UpstreamNotification>,
    mut upstream_request_rx: tokio::sync::mpsc::UnboundedReceiver<
        crate::lsp::bridge::UpstreamRequest,
    >,
    translators: Option<Arc<UpstreamRequestTranslators>>,
    inbound_request_registry: crate::lsp::bridge::InboundRequestRegistry,
    client: Client,
    delivery_context: Option<Arc<UpstreamDeliveryContext>>,
    cancel_token: tokio_util::sync::CancellationToken,
    editor_supports_apply_edit: bool,
) {
    // Tokens the editor successfully created. `$/progress` is forwarded only for
    // these: if a create timed out or was rejected, the editor never created the
    // token, so reporting progress against it would violate LSP's create-before-
    // progress contract (window-work-done-progress). Loop-local + FIFO, so the
    // create for a token is always processed before its progress.
    let mut created_tokens: std::collections::HashSet<tower_lsp_server::ls_types::NumberOrString> =
        std::collections::HashSet::new();
    // Tokens whose `Begin` has been forwarded but whose `End` has not yet arrived
    // (a subset of `created_tokens`). On connection teardown the bridge synthesizes
    // an `End` for each still in this set so the editor's indicator does not dangle
    // (ls-bridge-progress-disconnect-cleanup). A token created but never begun has
    // no visible progress to terminate and is absent here.
    let mut begun_tokens: std::collections::HashSet<tower_lsp_server::ls_types::NumberOrString> =
        std::collections::HashSet::new();

    loop {
        tokio::select! {
            biased;

            _ = cancel_token.cancelled() => {
                log::debug!(
                    target: "kakehashi::bridge",
                    "Upstream forwarding loop cancelled"
                );
                break;
            }

            notification = upstream_rx.recv() => {
                match notification {
                    Some(first) => {
                        // Drain the rest of the currently-queued burst (capped) and
                        // coalesce same-(connection,uri) PublishDiagnostics to the
                        // latest, then publish each resolved host once per
                        // barrier-delimited run (#426). The common case (nothing
                        // else queued) is one extra non-blocking try_recv and a
                        // single-element passthrough.
                        let mut batch = vec![first];
                        while batch.len() < UPSTREAM_COALESCE_BATCH_CAP {
                            match upstream_rx.try_recv() {
                                Ok(next) => batch.push(next),
                                Err(_) => break, // empty or disconnected
                            }
                        }
                        deliver_upstream_batch(
                            &client,
                            coalesce_upstream_batch(batch),
                            &mut created_tokens,
                            &mut begun_tokens,
                            delivery_context.as_deref(),
                            &cancel_token,
                        )
                        .await;
                    }
                    None => break, // Channel closed
                }
            }

            request = upstream_request_rx.recv() => {
                match request {
                    // Serviced on a spawned task, never awaited inline: these are
                    // user-interactive (showMessageRequest can pend for minutes),
                    // so awaiting here would freeze forwarding for every bridged
                    // server. The reply travels back through the request's oneshot.
                    //
                    // Ordered before `window_rx` (best-effort) so a `window/*` flood
                    // (e.g. logMessage) can't starve loss-intolerant request
                    // forwarding under `biased`. Servicing is just a spawn, and
                    // requests are user-paced/low-volume, so this can't starve
                    // `window_rx` in turn.
                    Some(request) => {
                        spawn_upstream_request(
                            inbound_request_registry.clone(),
                            translators.clone(),
                            &client,
                            request,
                            editor_supports_apply_edit,
                            delivery_context.clone(),
                        )
                    }
                    None => break, // Channel closed
                }
            }

            notification = window_rx.recv() => {
                match notification {
                    Some(notification) => {
                        deliver_upstream_notification(
                            &client,
                            notification,
                            &mut created_tokens,
                            &mut begun_tokens,
                            delivery_context.as_deref(),
                        )
                        .await
                    }
                    None => break, // Channel closed
                }
            }
        }
    }
}

/// Deliver one drained upstream batch while collapsing every consecutive run of
/// diagnostic pushes at the resolved-host boundary. The reader-level coalescer
/// removes superseded pushes for one downstream URI; this second stage records
/// all surviving region/host slots first, then publishes each affected host's
/// final aggregate once. Non-publish notifications remain exact FIFO barriers.
async fn deliver_upstream_batch(
    client: &Client,
    batch: Vec<crate::lsp::bridge::UpstreamNotification>,
    created_tokens: &mut std::collections::HashSet<tower_lsp_server::ls_types::NumberOrString>,
    begun_tokens: &mut std::collections::HashSet<tower_lsp_server::ls_types::NumberOrString>,
    delivery_context: Option<&UpstreamDeliveryContext>,
    cancel_token: &tokio_util::sync::CancellationToken,
) {
    use crate::lsp::bridge::UpstreamNotification;
    use crate::lsp::lsp_impl::coordinator::DiagnosticPush;

    let mut pushes = Vec::new();
    for notification in batch {
        if cancel_token.is_cancelled() {
            return;
        }
        match notification {
            UpstreamNotification::PublishDiagnostics {
                uri,
                server,
                connection_id,
                diagnostics,
            } => pushes.push(DiagnosticPush {
                uri,
                server,
                connection_id,
                diagnostics,
            }),
            barrier => {
                if !pushes.is_empty() {
                    if let Some(publisher) =
                        delivery_context.map(|context| context.diagnostic_publisher.as_ref())
                    {
                        tokio::select! {
                            biased;
                            _ = cancel_token.cancelled() => return,
                            _ = publisher.publish_push_batch(std::mem::take(&mut pushes)) => {}
                        }
                    } else {
                        pushes.clear();
                    }
                }
                if cancel_token.is_cancelled() {
                    return;
                }
                deliver_upstream_notification(
                    client,
                    barrier,
                    created_tokens,
                    begun_tokens,
                    delivery_context,
                )
                .await;
            }
        }
    }
    if !pushes.is_empty()
        && let Some(publisher) =
            delivery_context.map(|context| context.diagnostic_publisher.as_ref())
    {
        tokio::select! {
            biased;
            _ = cancel_token.cancelled() => {}
            _ = publisher.publish_push_batch(pushes) => {}
        }
    }
}

/// Max notifications drained into one coalescing batch per loop wake-up. Bounds the
/// transient batch while still collapsing a burst; under a continuous flood the loop
/// processes the channel in capped chunks, so it keeps making publish progress
/// instead of draining forever (#426).
const UPSTREAM_COALESCE_BATCH_CAP: usize = 256;

/// Collapse a drained burst of upstream notifications, coalescing the
/// `PublishDiagnostics` for each `(connection_id, uri)` within a barrier-delimited
/// run (not necessarily adjacent — other keys may interleave) down to the latest one
/// (#426). A push-happy or misbehaving downstream can pile arbitrary-size
/// `Vec<Diagnostic>` on the unbounded upstream channel faster than the loop
/// records them; since `record` already keeps only the latest per
/// `(host, source, server)`, the earlier same-key writes are wasted. This first
/// stage skips them; [`deliver_upstream_batch`] then collapses distinct surviving
/// region keys that resolve to the same host into one final aggregate publish.
///
/// Coalescing **drops superseded earlier pushes** and keeps every survivor in its
/// original FIFO order: a same-key push tombstones its earlier occurrence and lands
/// at its own (later) position. This is required for correctness, not just tidiness —
/// a restarted server pushes under a *new* `connection_id` for the *same* uri, which
/// is a distinct coalescing key but the **same** cache slot `(host, source, server)`;
/// keeping last-occurrence order makes the delivered sequence equivalent to FIFO with
/// the superseded entries removed, so the slot's final writer (and any later
/// `Evict(connection)`) behaves exactly as in the un-coalesced FIFO.
///
/// **Barrier order is exact**: every non-publish notification — including
/// [`UpstreamNotification::EvictConnectionDiagnostics`] — is a barrier that the
/// pending coalesced publishes stay *before*, so a publish can never be reordered
/// across one (a `Publish(c)` then `Evict(c)` still nets to evicted, and
/// create-before-progress holds).
fn coalesce_upstream_batch(
    batch: Vec<crate::lsp::bridge::UpstreamNotification>,
) -> Vec<crate::lsp::bridge::UpstreamNotification> {
    use crate::lsp::bridge::{ProgressConnectionId, UpstreamNotification};
    use std::collections::HashMap;

    // Common case — a lone notification (no burst queued): nothing to coalesce, so
    // skip the `output`/`pending` allocations and the tombstone pass entirely.
    if batch.len() <= 1 {
        return batch;
    }

    // `None` entries are tombstones: a publish superseded by a later same-key one.
    let mut output: Vec<Option<UpstreamNotification>> = Vec::with_capacity(batch.len());
    // Pending coalesced publishes since the last barrier: connection → uri → its
    // (live, latest) index in `output`. Nested (rather than a flat `(conn, uri)`
    // tuple key) so the coalescing-repeat path looks up by `&uri` with `get_mut` and
    // only clones the `uri` when first inserting it.
    let mut pending: HashMap<ProgressConnectionId, HashMap<String, usize>> = HashMap::new();

    for notification in batch {
        match notification {
            UpstreamNotification::PublishDiagnostics {
                uri,
                server,
                connection_id,
                diagnostics,
            } => {
                let idx = output.len();
                let by_uri = pending.entry(connection_id).or_default();
                if let Some(prev) = by_uri.get_mut(&uri) {
                    // Tombstone the earlier occurrence so only the latest survives, at
                    // its own later position (no `uri` clone on this hot repeat path).
                    output[*prev] = None;
                    *prev = idx;
                } else {
                    by_uri.insert(uri.clone(), idx);
                }
                output.push(Some(UpstreamNotification::PublishDiagnostics {
                    uri,
                    server,
                    connection_id,
                    diagnostics,
                }));
            }
            // Any non-publish notification is a barrier: the pending publishes are
            // already committed to `output` ahead of it (order preserved); stop
            // coalescing across it so a later same-key push is emitted separately.
            // Clear the inner maps rather than the outer one, so a batch with several
            // barriers reuses the per-connection allocations instead of dropping and
            // re-allocating them each time.
            barrier => {
                for by_uri in pending.values_mut() {
                    by_uri.clear();
                }
                output.push(Some(barrier));
            }
        }
    }
    output.into_iter().flatten().collect()
}

/// Service a downstream-initiated request by forwarding it to the editor on a
/// detached task and relaying the editor's answer through the request's `reply`
/// oneshot. (Exception: a `workspace/applyEdit` the editor never declared
/// support for is answered `applied: false` locally, without an editor
/// round-trip.)
///
/// Spawned (not awaited) so the shared forwarding loop keeps draining
/// notifications while the editor — possibly a human — takes its time. On editor
/// error the protocol default is sent (`None` selection / `success:false`); if
/// the downstream connection drops, the receiving oneshot end is gone and
/// `reply.send` simply no-ops.
///
/// **No bridge-imposed timeout** (unlike `create_work_done_progress`):
/// `showMessageRequest` legitimately pends on user interaction, and `showDocument`
/// deliberately opts out too — a timeout there would answer `success:false` while
/// the editor might still open the document moments later, which is worse than
/// waiting. Both are relayed as-is and resolve when the editor answers or the
/// client closes.
///
/// **No concurrency cap / unbounded request channel** is a deliberate tradeoff,
/// matching the unbounded loss-intolerant `upstream_tx`: a forwarded request must
/// be answered (a dropped one would hang the downstream), and these are
/// user-paced, low-volume requests rather than a flood-prone stream like
/// `window/logMessage` (which is what the *bounded* window channel guards). The
/// detached tasks are not tracked for abort on shutdown, but they self-terminate:
/// when the service shuts down the editor `Client` closes, so each pending
/// `client.*` call returns `Err` promptly and the task ends.
///
/// Why not bound this as flood protection? A request flood from an
/// adversarial/buggy downstream propagates to the editor either way (modulo
/// the capability-gated applyEdit local answer) — exactly as
/// it would if the editor spoke to that server directly, with no bridge. The
/// bridge cannot shield the client from such floods, and rate-limiting
/// client-facing requests is the *client's* responsibility; the bridge's job is
/// to forward transparently. A cap whose overflow behavior answered the protocol
/// default would be strictly worse: the bridge would fabricate responses the
/// editor never saw, a divergence a direct connection never produces. The only
/// concern the bridge can't delegate is its own survival (it is one process
/// shared by all downstream connections), but the per-request cost it holds — a
/// lightweight task awaiting a `oneshot` — is far smaller than the editor's
/// per-dialog cost, so the editor pushes back first. See issue #405
/// (closed as not planned) for the full rationale.
/// Send a server→client request to the editor with an id *we* mint, returning
/// the parsed result value (`None` on an error response or transport failure).
///
/// This mirrors what `Client::send_request` does internally, but mints the id
/// via `next_request_id` and sends through the `Client`'s `tower::Service` so we
/// hold the editor-facing request id — needed to cancel an in-flight request by
/// sending a correlated `$/cancelRequest` to the editor (#404); the `Client`
/// exposes no cancel API for outgoing requests.
async fn send_editor_request(
    client: &Client,
    id: tower_lsp_server::jsonrpc::Id,
    method: &'static str,
    params: serde_json::Value,
) -> Option<serde_json::Value> {
    use tower::Service as _;
    let request = tower_lsp_server::jsonrpc::Request::build(method)
        .id(id)
        .params(params)
        .finish();
    match client.clone().call(request).await {
        Ok(Some(response)) => match response.into_parts().1 {
            Ok(value) => Some(value),
            // An error response from the editor (e.g. method unsupported) — log
            // for observability, then fall back to the protocol default like the
            // replaced client.show_message_request/show_document path did.
            Err(e) => {
                log::debug!(
                    target: "kakehashi::bridge",
                    "{} returned an error from the editor: {}",
                    method, e
                );
                None
            }
        },
        Ok(None) => None,
        Err(e) => {
            log::debug!(
                target: "kakehashi::bridge",
                "{} forwarding to editor failed: {}",
                method, e
            );
            None
        }
    }
}

/// Forward a request to the editor, racing it against the downstream's cancel
/// signal (#404). If the cancel fires first, send a correlated `$/cancelRequest`
/// to the editor with the id we minted — so a `showMessageRequest` dialog is
/// dismissed — and return `None`, so the downstream gets the protocol default.
///
/// On cancel the in-flight `send_editor_request` future is dropped. tower-lsp's
/// client registers a pending-response slot inside `call` (no cancel/remove API),
/// so dropping the future can leave that slot parked — whether the request had
/// already been written to the editor or cancellation won before the write. A
/// later response from the editor reclaims the slot, so the leak is bounded to
/// requests the editor never answers (including ones it never received).
async fn forward_with_cancel(
    client: &Client,
    editor_id: tower_lsp_server::jsonrpc::Id,
    method: &'static str,
    params: serde_json::Value,
    cancel_token: &tokio_util::sync::CancellationToken,
) -> Option<serde_json::Value> {
    // Already cancelled before we could forward (cancelled while it sat in the
    // channel): the editor never saw this request, so don't send it OR a
    // `$/cancelRequest` for an id it never received — just answer the default.
    if cancel_token.is_cancelled() {
        return None;
    }
    tokio::select! {
        // `biased`: poll the cancel branch first so a request cancelled the
        // instant after the check above still wins the race before
        // `send_editor_request` makes progress where possible.
        biased;
        () = cancel_token.cancelled() => {
            use tower_lsp_server::ls_types::notification::Cancel;
            use tower_lsp_server::ls_types::{CancelParams, NumberOrString};
            // The id we minted is always numeric (next_request_id, an AtomicU32
            // counter); map it to the notification's `NumberOrString`. The cancel
            // must carry the *same* numeric id the editor saw, so for the
            // (astronomically unlikely) ids beyond i32 — which `NumberOrString`
            // can't represent as a number — there's no correlating cancel to
            // send; skip it rather than wrap to a wrong id.
            let id = match editor_id {
                tower_lsp_server::jsonrpc::Id::Number(n) => match i32::try_from(n) {
                    Ok(n) => NumberOrString::Number(n),
                    Err(_) => return None,
                },
                tower_lsp_server::jsonrpc::Id::String(s) => NumberOrString::String(s),
                tower_lsp_server::jsonrpc::Id::Null => return None,
            };
            client.send_notification::<Cancel>(CancelParams { id }).await;
            None
        }
        result = send_editor_request(client, editor_id.clone(), method, params) => result,
    }
}

/// e2e-only fault injection (`KAKEHASHI_E2E_STALL_REOPEN_MS`): hold the respawn
/// re-open before it enqueues any `didOpen`. Set LONGER than `REOPEN_WAIT` by
/// the ordering e2e, this forces the barrier's contract to be load-bearing —
/// the first command must be DROPPED (fail soft), never sent ahead of the
/// didOpen — rather than won by racing. Unset (every other test, and any
/// production use of an e2e build), this is a no-op. Not compiled into release
/// builds at all: `cargo build --release` carries no `e2e` feature.
#[cfg(feature = "e2e")]
async fn e2e_stall_reopen() {
    let Ok(ms) = std::env::var("KAKEHASHI_E2E_STALL_REOPEN_MS") else {
        return;
    };
    let Ok(ms) = ms.parse::<u64>() else {
        return;
    };
    tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
}

/// The `client/registerCapability` id kakehashi uses for one palette command.
///
/// Derived from the name rather than allocated and stored: one registration per
/// name means the id can be recomputed when the name is retired, so the mapping
/// needs no state to survive between the two events. Ids only have to be unique
/// among ACTIVE registrations, and a name is registered at most once at a time.
fn palette_registration_id(command: &str) -> String {
    format!("kakehashi/executeCommand/{command}")
}

/// Serializes palette registration against palette RETIREMENT.
///
/// The two carry the same derived id — that is the point of deriving it — so
/// their order is load-bearing: a settings reload that removes a server and a
/// handshake that re-adds it produce an unregister and a register for the same
/// id, and the wrong order leaves the editor either holding a dead entry or
/// missing a live one.
///
/// The channel delivers them in order, but each message is dispatched into its
/// own task, so nothing downstream of the channel preserves it. One lock held
/// across the round trip does. Only these two message kinds contend for it, and
/// only at handshake and settings-reload rate.
static PALETTE_REGISTRATION_ORDER: std::sync::LazyLock<tokio::sync::Mutex<()>> =
    std::sync::LazyLock::new(|| tokio::sync::Mutex::new(()));

fn spawn_upstream_request(
    inbound_request_registry: crate::lsp::bridge::InboundRequestRegistry,
    translators: Option<Arc<UpstreamRequestTranslators>>,
    client: &Client,
    request: crate::lsp::bridge::UpstreamRequest,
    editor_supports_apply_edit: bool,
    delivery_context: Option<Arc<UpstreamDeliveryContext>>,
) {
    use crate::lsp::bridge::UpstreamRequest;
    use tower_lsp_server::ls_types::{
        MessageActionItem, ShowDocumentResult, ShowMessageRequestParams,
    };
    let client = client.clone();
    tokio::spawn(async move {
        match request {
            UpstreamRequest::RegisterCommands { commands } => {
                use tower_lsp_server::ls_types::Registration;
                let _order = PALETTE_REGISTRATION_ORDER.lock().await;
                // ONE registration per command name, batched into one request.
                // Batching them under a single id would be fewer objects but
                // would make the set un-retirable: `client/unregisterCapability`
                // names a registration, so dropping one command from a shared id
                // means unregistering the batch and re-registering the rest.
                let registrations: Vec<_> = commands
                    .iter()
                    .map(|command| Registration {
                        id: palette_registration_id(command),
                        method: "workspace/executeCommand".to_string(),
                        register_options: Some(serde_json::json!({ "commands": [command] })),
                    })
                    .collect();
                // Bound the await: a non-responsive editor must not leak a task
                // pending forever on the registration request.
                match tokio::time::timeout(
                    std::time::Duration::from_secs(10),
                    client.register_capability(registrations),
                )
                .await
                {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => log::warn!(
                        target: "kakehashi::bridge",
                        "Failed to register palette commands upstream: {e}"
                    ),
                    Err(_) => log::warn!(
                        target: "kakehashi::bridge",
                        "Timed out registering palette commands upstream"
                    ),
                }
            }
            UpstreamRequest::UnregisterCommands { commands } => {
                use tower_lsp_server::ls_types::Unregistration;
                let _order = PALETTE_REGISTRATION_ORDER.lock().await;
                // The id is DERIVED from the name, so nothing has to be
                // remembered between registering and retiring it.
                let unregistrations: Vec<_> = commands
                    .iter()
                    .map(|command| Unregistration {
                        id: palette_registration_id(command),
                        method: "workspace/executeCommand".to_string(),
                    })
                    .collect();
                match tokio::time::timeout(
                    std::time::Duration::from_secs(10),
                    client.unregister_capability(unregistrations),
                )
                .await
                {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => log::warn!(
                        target: "kakehashi::bridge",
                        "Failed to unregister palette commands upstream: {e}"
                    ),
                    Err(_) => log::warn!(
                        target: "kakehashi::bridge",
                        "Timed out unregistering palette commands upstream"
                    ),
                }
            }
            UpstreamRequest::ReopenDocuments { key, done } => {
                // One source of truth for the server: carrying it alongside the
                // key would be an invariant nobody checks, and a divergence
                // would make the repair a silent no-op.
                let server = key.server().to_string();
                // A respawned connection has nothing open; bring it up to date
                // (respawn-reopen-derives-its-targets).
                //
                // `ensure_server_documents_open` — NOT `process_injections`.
                // `process_injections` reaches the open through
                // `eager_spawn_and_open_documents`, which SPAWNS a detached task
                // per server and returns; `done` would then be signalled before a
                // single `didOpen` was enqueued, releasing a waiting command to
                // overtake the very notification it is waiting for. This path
                // awaits the open, which is the whole reason the barrier exists.
                // It is also scoped to the server that respawned, rather than
                // every server the host bridges to.
                let Some(context) = delivery_context else {
                    // Unreachable in the wired server (the loop is spawned with a
                    // context), but the fallback must not silently skip a heal.
                    log::warn!(
                        target: "kakehashi::bridge",
                        "Cannot re-open documents for respawned {server:?}: \
                         no delivery context"
                    );
                    return;
                };
                // DERIVE the target set rather than replay one captured at purge
                // time. Every open document is a candidate; which of them belong
                // to this connection is decided below, per host, against current
                // settings. A captured list answers the question as it stood
                // before the respawn — it re-opens documents the editor has since
                // closed, misses ones opened since, and is simply EMPTY when the
                // dead connection never got far enough to hold anything, which is
                // exactly when a replacement most needs the repair.
                let hosts = context.injection.open_host_uris();
                log::debug!(
                    target: "kakehashi::bridge",
                    "Bringing {key} up to date after {server:?} respawned: \
                     {} open document(s) to consider",
                    hosts.len()
                );
                use crate::lsp::bridge::{OpenOutcome, REOPEN_WAIT};
                let settings = context.settings_manager.load_settings();
                // Naming the connection still matters: the open is ACQUIRED by
                // this key, never by whatever a host routes to, so the repair
                // lands on the connection `done` signals for and a routed
                // command names. What the key now ALSO does is filter — a host
                // that does not route here supplies nothing for this connection,
                // and saying so is how the derivation stays scoped to
                // `(server, root)` instead of cross-opening one root's documents
                // onto another root's process.
                // Bound the WAIT, not the work — the shape the inline heal used.
                // `ensure_server_documents_open` can block up to the init timeout
                // on a cold downstream, and `done` gates every command on this
                // connection: an unbounded loop would keep the barrier
                // outstanding for tens of seconds, making each command pay the
                // full wait repeatedly. On expiry the opens keep running detached
                // and waiters are released, degrading to the pre-existing lazy
                // heal rather than stalling.
                let injection = context.injection.clone();
                let reopen_server = server.clone();
                // `done` moves INTO the work task, so ONLY real completion
                // signals it. Signalling from the timeout branch below would mark
                // the re-open complete while its didOpens were still queued, and
                // any request still waiting would sail through and overtake them
                // — the failure this barrier exists to prevent. A panic drops the
                // sender instead, which waiters read as "can never finish".
                let mut work = tokio::spawn(async move {
                    // Whether this connection ended up holding everything current
                    // state says it should. Only an APPLICABLE host that failed
                    // to open clears it — a host that supplies nothing for this
                    // connection is not a failed repair, it is not this
                    // connection's document. Counting those would report failure
                    // on essentially every respawn (most open documents bridge
                    // nowhere near any one server), holding the barrier shut and
                    // making every command pay the full wait and then fail soft.
                    let mut repaired = true;
                    // e2e-only fault injection: hold the re-open BEFORE any
                    // didOpen goes out, so the ordering e2e can force the window
                    // in which a command could overtake its own didOpen instead
                    // of racing it (the race resolves correctly by accident on a
                    // fast machine, which is what made the naive test
                    // non-discriminating). Compiled only with the `e2e` feature;
                    // release builds do not contain this branch.
                    #[cfg(feature = "e2e")]
                    e2e_stall_reopen().await;
                    // ONE budget for the whole sweep, not one per host. Each
                    // surviving host can park waiting for its tree, so a
                    // per-host bound lets ten of them spend `REOPEN_WAIT` ten
                    // times over — and the barrier promises to settle inside it
                    // ONCE. Deriving the set is what makes that reachable: the
                    // sweep is now sized by the workspace rather than by what
                    // one dead connection held.
                    let sweep_deadline = std::time::Instant::now() + REOPEN_WAIT;
                    for host in hosts {
                        // Stop if nobody can hear the answer. `rearm` and a
                        // later `claim` both drop the registry's receiver, so a
                        // closed channel means this re-open has been superseded
                        // by a newer respawn of the same key — and the sweep is
                        // now O(open documents), so continuing would spend
                        // marker walks and parse waits producing a result that
                        // will be discarded.
                        if done.is_closed() {
                            log::debug!(
                                target: "kakehashi::bridge",
                                "Re-open of {key} was superseded; stopping the sweep"
                            );
                            return;
                        }
                        // Cheapest question first. Deriving means asking about
                        // every open document, and for most of them the answer
                        // is "this host bridges nowhere near that server" —
                        // which is pure configuration, answered from a memo
                        // with no parse, no tree and no I/O. Paying the parse
                        // wait and the injection resolution before asking it
                        // would spend this connection's fixed budget in
                        // proportion to WORKSPACE SIZE rather than to the work
                        // that belongs to it, and the budget is what `done`
                        // must signal inside.
                        //
                        // Reading the language before the parse wait keeps the
                        // incarnation/injections ordering intact, because the
                        // authoritative language is re-read with the injections
                        // below. The screen is one-directional, though, and not
                        // symmetric: a stale ACCEPT costs only an unnecessary
                        // resolution, but a stale REJECT skips the document for
                        // this round while `repaired` still reports success —
                        // indistinguishable from "nothing to repair". Every
                        // widening of this screen has to be weighed in that
                        // direction. (A `languageId` change needs
                        // didClose+didOpen, which bumps the incarnation, so the
                        // stale-reject window is narrow rather than absent.)
                        let screened_at = injection.document_incarnation(&host);
                        let reachable =
                            injection
                                .document_language(&host)
                                .is_some_and(|candidate_language| {
                                    injection.bridge().host_language_can_reach_server(
                                        &settings,
                                        &candidate_language,
                                        &reopen_server,
                                    )
                                });
                        // Only trust a REJECTION if the document did not change
                        // lifetime underneath it. A close+reopen under a
                        // different `languageId` between the two reads would
                        // otherwise reject on the OLD language and skip a
                        // document the NEW lifetime does bridge — silently,
                        // since a skip is indistinguishable from "nothing to
                        // repair". On a mismatch fall through and let the
                        // authoritative path, which re-reads both, decide.
                        if !reachable && injection.document_incarnation(&host) == screened_at {
                            continue;
                        }
                        // Await the tree first: a re-open racing an edit would
                        // otherwise resolve no injections and open nothing.
                        // Bounded by what is LEFT of the sweep's budget.
                        let remaining = sweep_deadline
                            .checked_duration_since(std::time::Instant::now())
                            .unwrap_or_default();
                        if remaining.is_zero() {
                            // Out of budget with candidates still unexamined.
                            // Report the connection as NOT caught up: the
                            // waiters are about to time out anyway, and telling
                            // them the sweep finished would release commands
                            // onto documents this pass never reached.
                            log::debug!(
                                target: "kakehashi::bridge",
                                "Re-open of {key} ran out of budget with candidates \
                                 left; reporting it incomplete"
                            );
                            repaired = false;
                            break;
                        }
                        use crate::lsp::lsp_impl::coordinator::ParseWait;
                        match injection.ensure_document_parsed(&host, remaining).await {
                            ParseWait::Current => {}
                            // Closed underneath the sweep. It was in the
                            // snapshot this pass started from, but a buffer the
                            // user closed is not a repair this connection is
                            // owed, and calling it a failure would hold the
                            // barrier shut over it.
                            ParseWait::Gone => continue,
                            ParseWait::Unsettled => {
                                // The budget expired with this document's parse
                                // still outstanding. Resolving injections now yields
                                // ZERO — not because the host has none, but because
                                // there is no tree to find them in — and the skip
                                // below would then read as "nothing to repair here".
                                // This host passed the configuration screen, so it
                                // is a plausible member of this connection's set and
                                // saying otherwise is the one direction that
                                // releases a command onto a document that was never
                                // opened. Report the connection as not caught up and
                                // let the next parse's eager open heal it.
                                log::debug!(
                                    target: "kakehashi::bridge",
                                    "Re-open of {key}: {host} did not settle within the \
                                     remaining budget; reporting the connection incomplete"
                                );
                                repaired = false;
                                continue;
                            }
                        }
                        // Incarnation BEFORE injections, matching the ordering the
                        // inline heal used: a close+reopen landing between the two
                        // reads then pairs a stale incarnation with fresh
                        // injections, which the downstream sync rejects. The
                        // reverse pairs stale injections with a fresh incarnation,
                        // which reads as current.
                        let Some(incarnation) = injection.document_incarnation(&host) else {
                            continue;
                        };
                        let Some((host_language, injections)) = injection.bridge_injections(&host)
                        else {
                            continue;
                        };
                        if injections.is_empty() {
                            // Empty means one of two very different things: this
                            // host genuinely has no region for this server, or
                            // an edit cleared the tree between the currency
                            // check above and this resolution — `didChange`
                            // clears it WITHOUT bumping the incarnation, so
                            // neither guard above catches that. Re-check rather
                            // than assume the benign reading, because the benign
                            // reading is the one that releases commands.
                            // ...unless the host is simply gone. A buffer
                            // closed mid-sweep is not a repair this connection
                            // is owed, and `document_language` falls back to the
                            // URI extension, so a closed document can reach here
                            // and would otherwise wedge the barrier shut.
                            if injection.document_incarnation(&host).is_some()
                                && !injection.snapshot_is_current(&host)
                            {
                                repaired = false;
                            }
                            continue;
                        }
                        // Sequential: each host's didOpen goes out on the SAME
                        // connection, so fanning out would only contend on the
                        // single-writer outbound queue.
                        let outcome = injection
                            .bridge()
                            .ensure_server_documents_open(
                                &settings,
                                &host_language,
                                &host,
                                crate::lsp::bridge::OpenExpectation {
                                    incarnation,
                                    // Both the filter and the target: only hosts
                                    // that route here are opened, and they are
                                    // opened HERE.
                                    connection: Some(&key),
                                },
                                injections,
                                &reopen_server,
                            )
                            .await;
                        match outcome {
                            OpenOutcome::Opened => {}
                            // Not this connection's document. Nothing to report.
                            OpenOutcome::NotApplicable => {}
                            // It was this connection's and it did not open —
                            // unless the reason is that the host closed while
                            // the open was running, which is the same benign
                            // case as `ParseWait::Gone` arriving one step later.
                            OpenOutcome::NotOpened => {
                                if injection.document_incarnation(&host).is_some() {
                                    repaired = false;
                                }
                            }
                        }
                    }
                    // Report what actually happened. `true` releases waiters;
                    // `false` leaves them to time out and fail soft, which is
                    // correct when the claimed connection is still empty — a
                    // command sent there would fail downstream anyway, less
                    // legibly. A send error means a later respawn's claim
                    // superseded this barrier, and that respawn owns it now.
                    let _ = done.send(repaired);
                });
                // Bound only how long THIS task waits. The work owns the signal,
                // so exceeding the budget stops us watching without pretending
                // the re-open finished: requests still waiting time out and fail
                // soft, and a request arriving later waits on the same barrier.
                match tokio::time::timeout(REOPEN_WAIT, &mut work).await {
                    Ok(Ok(())) => {}
                    // A panic in the re-open would otherwise vanish with the
                    // dropped JoinHandle, leaving only "the heal didn't help".
                    Ok(Err(join_error)) => log::warn!(
                        target: "kakehashi::bridge",
                        "Re-open of {server:?} documents failed: {join_error}"
                    ),
                    Err(_) => {
                        log::debug!(
                            target: "kakehashi::bridge",
                            "Re-open of {server:?} documents exceeded {REOPEN_WAIT:?}; \
                             finishing in the background (the barrier stays \
                             pending until it does)"
                        );
                        tokio::spawn(async move {
                            if let Err(join_error) = work.await {
                                log::warn!(
                                    target: "kakehashi::bridge",
                                    "Background re-open of {server:?} documents failed: \
                                     {join_error}"
                                );
                            }
                        });
                    }
                }
            }
            UpstreamRequest::ShowMessageRequest {
                typ,
                message,
                actions,
                reply,
                cancel,
            } => {
                let id = client.next_request_id();
                let params = serde_json::to_value(ShowMessageRequestParams {
                    typ,
                    message,
                    actions,
                })
                .unwrap_or(serde_json::Value::Null);
                let action = forward_with_cancel(
                    &client,
                    id,
                    "window/showMessageRequest",
                    params,
                    &cancel.token,
                )
                .await
                .and_then(|v| serde_json::from_value::<Option<MessageActionItem>>(v).ok())
                .flatten();
                inbound_request_registry.unregister(
                    cancel.connection_id,
                    &cancel.request_id,
                    cancel.generation,
                );
                let _ = reply.send(action);
            }
            UpstreamRequest::ShowDocument {
                params,
                reply,
                cancel,
            } => {
                // Translate a virtual-document URI + selection back to the host
                // document before forwarding, so the editor opens the real file
                // (#403). For a resolvable virtual URI the host URI is always
                // used (selection translated, or dropped if the offset can't be
                // rebuilt); only a non-virtual/unresolvable URI is forwarded
                // unchanged. See `ShowDocumentTranslator::translate`.
                let params = match &translators {
                    Some(translators) => translators.show_document.translate(params).await,
                    None => params,
                };
                let id = client.next_request_id();
                let value = serde_json::to_value(params).unwrap_or(serde_json::Value::Null);
                let success =
                    forward_with_cancel(&client, id, "window/showDocument", value, &cancel.token)
                        .await
                        .and_then(|v| serde_json::from_value::<ShowDocumentResult>(v).ok())
                        .map(|r| r.success)
                        .unwrap_or(false);
                inbound_request_registry.unregister(
                    cancel.connection_id,
                    &cancel.request_id,
                    cancel.generation,
                );
                let _ = reply.send(success);
            }
            UpstreamRequest::ApplyEdit {
                params,
                connection,
                reply,
                cancel,
            } => {
                use tower_lsp_server::ls_types::ApplyWorkspaceEditResponse;
                // LSP makes workspace/applyEdit conditional on the CLIENT
                // capability: an editor that did not declare
                // `workspace.applyEdit` must not receive the request (the
                // bridge also stops advertising it downstream in that case,
                // so this is the fail-soft for servers that send one anyway).
                if !editor_supports_apply_edit {
                    inbound_request_registry.unregister(
                        cancel.connection_id,
                        &cancel.request_id,
                        cancel.generation,
                    );
                    let _ = reply.send(ApplyWorkspaceEditResponse {
                        applied: false,
                        failure_reason: Some(
                            "kakehashi: the editor did not declare the workspace.applyEdit \
                             capability"
                                .to_string(),
                        ),
                        failed_change: None,
                    });
                    return;
                }
                // Translate virtual-document edits back to host coordinates
                // before forwarding (#568). Unlike showDocument there is no
                // safe degraded forward: an untranslatable edit (unknown/stale
                // region, virtual-URI file ops, multi-region edit, or a
                // versioned edit whose version no longer matches what the
                // bridge tracks for `connection`) is answered `applied: false`
                // locally with a failureReason, never sent to the editor. See
                // `ApplyEditTranslator::translate`.
                // The editor answers `failedChange` as an index into the
                // FORWARDED documentChanges array; the downstream interprets
                // it against the array it SENT. Translation can REMOVE no-op
                // entries (never reorder or insert), so a changed entry count
                // means the two index spaces diverged and the index must be
                // dropped rather than relayed misaligned — `applied` and
                // `failureReason` still relay.
                let sent_change_count =
                    super::apply_edit_translation::document_change_count(&params);
                let params = match &translators {
                    Some(translators) => {
                        translators.apply_edit.translate(params, &connection).await
                    }
                    None => Ok(params),
                };
                let response = match params {
                    Err(failure_reason) => {
                        // The reason is otherwise write-only: it goes to the
                        // DOWNSTREAM (which typically ignores failureReason),
                        // so without this log a rejected server-driven edit is
                        // invisible on the kakehashi side.
                        log::warn!(
                            target: "kakehashi::bridge",
                            "workspace/applyEdit rejected locally: {failure_reason:?}"
                        );
                        ApplyWorkspaceEditResponse {
                            applied: false,
                            failure_reason: Some(failure_reason),
                            failed_change: None,
                        }
                    }
                    // Serializing the (typed) translated params ~never fails,
                    // but forwarding `params: null` on the off chance it did
                    // would send the editor an invalid request; answer local
                    // applied:false with a serialization reason instead.
                    Ok(params) => {
                        let forwarded_change_count =
                            super::apply_edit_translation::document_change_count(&params);
                        match serde_json::to_value(params) {
                            Ok(value) => {
                                let id = client.next_request_id();
                                let mut response = forward_with_cancel(
                                    &client,
                                    id,
                                    "workspace/applyEdit",
                                    value,
                                    &cancel.token,
                                )
                                .await
                                .and_then(|v| {
                                    serde_json::from_value::<ApplyWorkspaceEditResponse>(v).ok()
                                })
                                // Editor error/cancel, or a response that didn't
                                // parse as an ApplyWorkspaceEditResponse: the
                                // protocol default — the edit was not applied.
                                .unwrap_or(
                                    ApplyWorkspaceEditResponse {
                                        applied: false,
                                        // Covers editor error, cancellation, AND an
                                        // unparseable response — neutral wording (like
                                        // the reader drop-path) so a cancel/transport
                                        // failure isn't misattributed to the editor.
                                        failure_reason: Some(
                                            "kakehashi: no valid workspace/applyEdit response"
                                                .to_string(),
                                        ),
                                        failed_change: None,
                                    },
                                );
                                if forwarded_change_count != sent_change_count {
                                    // Index spaces diverged; see above.
                                    response.failed_change = None;
                                }
                                response
                            }
                            Err(e) => ApplyWorkspaceEditResponse {
                                applied: false,
                                failure_reason: Some(format!(
                                    "kakehashi: could not serialize the workspace/applyEdit request: {e}"
                                )),
                                failed_change: None,
                            },
                        }
                    }
                };
                inbound_request_registry.unregister(
                    cancel.connection_id,
                    &cancel.request_id,
                    cancel.generation,
                );
                let _ = reply.send(response);
            }
        }
    });
}

/// A `telemetry/event` notification whose `Params` is raw `serde_json::Value`,
/// so the downstream payload is forwarded to the editor as the same JSON value
/// (its shape is preserved — scalars are not wrapped, no fields added/dropped;
/// re-serialization may still normalize whitespace/number formatting). The
/// `ls_types` `TelemetryEvent` models params as `OneOf<Map, Vec>`, which can't
/// carry a scalar LSPAny payload unchanged.
enum RawTelemetryEvent {}

impl tower_lsp_server::ls_types::notification::Notification for RawTelemetryEvent {
    type Params = serde_json::Value;
    const METHOD: &'static str = "telemetry/event";
}

/// Dispatch one upstream notification to the editor client.
///
/// `created_tokens` tracks work-done progress tokens the editor successfully
/// created; it gates `$/progress` so progress for a token the editor rejected
/// (or never replied to) is dropped (window-work-done-progress).
async fn deliver_upstream_notification(
    client: &Client,
    notification: crate::lsp::bridge::UpstreamNotification,
    created_tokens: &mut std::collections::HashSet<tower_lsp_server::ls_types::NumberOrString>,
    begun_tokens: &mut std::collections::HashSet<tower_lsp_server::ls_types::NumberOrString>,
    delivery_context: Option<&UpstreamDeliveryContext>,
) {
    use crate::lsp::bridge::UpstreamNotification;
    use tower_lsp_server::ls_types::{ProgressParamsValue, WorkDoneProgress};
    match notification {
        UpstreamNotification::DiagnosticRefresh => {
            // A downstream server asked the editor to re-pull diagnostics. Route it
            // through `request_forwarded_diagnostic_refresh`, which runs the leading
            // cycle immediately (prefetch, then a conditional editor forward — an
            // unchanged covering prefetch absorbs the nudge) and debounces later
            // burst activity, reusing the capability-gated, detached forced-refresh
            // path when it does send. Detaching avoids blocking this delivery loop
            // on the editor round-trip (head-of-line). A `None` publisher (test
            // loop) has no settings to gate on, so the forward is dropped;
            // production always has one (#521, #789).
            if let Some(publisher) =
                delivery_context.map(|context| context.diagnostic_publisher.as_ref())
            {
                publisher.request_forwarded_diagnostic_refresh();
            }
        }
        UpstreamNotification::PublishDiagnostics {
            uri,
            server,
            connection_id,
            diagnostics,
        } => {
            // Cache the downstream push and republish the merged host set
            // (push-propagation-diagnostic-forwarding). The publisher classifies the
            // URI (virtual → region, real → `_self` host layer); a `None` publisher
            // (test loop) drops it. (Pushes without a server name were already
            // dropped at the reader, so `server` is always set here.) The
            // `connection_id` tags the cached slot so a later crash can evict it (#469).
            if let Some(publisher) =
                delivery_context.map(|context| context.diagnostic_publisher.as_ref())
            {
                publisher
                    .publish_push(uri, server, connection_id, diagnostics)
                    .await;
            }
        }
        UpstreamNotification::LogMessage { typ, message } => {
            if delivery_context.is_some_and(|context| {
                context
                    .settings_manager
                    .load_settings()
                    .features
                    .window_log_message
                    .allows(typ)
            }) {
                client.log_message(typ, message).await;
            }
        }
        UpstreamNotification::ShowMessage { typ, message } => {
            client.show_message(typ, message).await;
        }
        UpstreamNotification::TelemetryEvent { data } => {
            // Forward the raw LSPAny `params` as the same JSON value. We can't
            // use `client.telemetry_event` (it wraps any non-object/array scalar
            // in a single-element array) or `send_notification::<ls_types
            // TelemetryEvent>` (its `Params` is `OneOf<Map, Vec>`, rejecting
            // scalars). A local marker with `Params = serde_json::Value` preserves
            // the payload's JSON shape (no scalar-wrapping), matching how
            // `$/progress` is forwarded.
            client.send_notification::<RawTelemetryEvent>(data).await;
        }
        UpstreamNotification::CreateWorkDoneProgress { token } => {
            // Awaited inline so the editor processes the create before the
            // `$/progress` notifications that follow it on this same FIFO
            // channel (LSP requires create-first). Only on success do we admit
            // the token for progress.
            if forward_upstream_request(
                "window/workDoneProgress/create",
                client.create_work_done_progress(token.clone()),
            )
            .await
            {
                created_tokens.insert(token);
            }
        }
        UpstreamNotification::Progress { params } => {
            let is_begin = matches!(
                &params.value,
                ProgressParamsValue::WorkDone(WorkDoneProgress::Begin(_))
            );
            let is_end = matches!(
                &params.value,
                ProgressParamsValue::WorkDone(WorkDoneProgress::End(_))
            );
            // Single set lookup: `End` removes the admission (and reports whether
            // it was admitted); others just check.
            let admitted = if is_end {
                created_tokens.remove(&params.token)
            } else {
                created_tokens.contains(&params.token)
            };
            if !admitted {
                // Create timed out / was rejected — the editor never created this
                // token, so drop its progress.
                log::debug!(
                    target: "kakehashi::bridge",
                    "Dropping $/progress for token the editor did not create: {:?}",
                    params.token
                );
                return;
            }
            // Track begun-not-ended so a disconnect can synthesize the missing
            // `End` (ls-bridge-progress-disconnect-cleanup). The real `End` shares
            // this FIFO channel and clears the entry before any later forget, so a
            // normally-ended token is never double-ended.
            if is_begin {
                begun_tokens.insert(params.token.clone());
            } else if is_end {
                begun_tokens.remove(&params.token);
            }
            client
                .send_notification::<tower_lsp_server::ls_types::notification::Progress>(params)
                .await;
        }
        UpstreamNotification::ClientProgress { params } => {
            // Aggregated client-provided progress: the editor minted the token
            // and needs no `window/workDoneProgress/create`, so forward it
            // ungated (ls-bridge-client-progress). The aggregator already
            // guarantees a single coherent Begin/report/End lifecycle.
            client
                .send_notification::<tower_lsp_server::ls_types::notification::Progress>(params)
                .await;
        }
        UpstreamNotification::ForgetWorkDoneProgress(tokens) => {
            // A downstream reader exited with progress in flight; drop its
            // admissions so the set can't leak across respawns. For any token that
            // was begun but not ended, synthesize a terminating `End` first so the
            // editor's indicator clears (ls-bridge-progress-disconnect-cleanup); a
            // token created but never begun has no visible progress and needs none.
            for token in tokens {
                created_tokens.remove(&token);
                if begun_tokens.remove(&token) {
                    let end = tower_lsp_server::ls_types::ProgressParams {
                        token,
                        value: ProgressParamsValue::WorkDone(WorkDoneProgress::End(
                            tower_lsp_server::ls_types::WorkDoneProgressEnd { message: None },
                        )),
                    };
                    client
                        .send_notification::<tower_lsp_server::ls_types::notification::Progress>(
                            end,
                        )
                        .await;
                }
            }
        }
        UpstreamNotification::EvictConnectionDiagnostics { connection_id } => {
            // A downstream connection's reader exited (crash/respawn): drop the
            // diagnostic slots it produced and republish the affected hosts so a
            // dead server's diagnostics don't linger until didClose (#469). A
            // `None` publisher (test loop) has no cache to evict.
            if let Some(publisher) =
                delivery_context.map(|context| context.diagnostic_publisher.as_ref())
            {
                publisher.evict_connection_diagnostics(connection_id).await;
            }
        }
    }
}

/// Cancellable upstream forwarding loop without a Client (for testing).
///
/// Drains notifications from both channels and exits when the token is
/// cancelled or a channel closes. Does not forward to any client.
#[cfg(test)]
async fn upstream_forwarding_loop_with_cancel(
    mut upstream_rx: tokio::sync::mpsc::UnboundedReceiver<crate::lsp::bridge::UpstreamNotification>,
    mut window_rx: tokio::sync::mpsc::Receiver<crate::lsp::bridge::UpstreamNotification>,
    cancel_token: tokio_util::sync::CancellationToken,
) {
    loop {
        tokio::select! {
            biased;

            _ = cancel_token.cancelled() => break,

            notification = upstream_rx.recv() => {
                if notification.is_none() {
                    break; // Channel closed
                }
                // Notification consumed but not forwarded (no client in test)
            }

            notification = window_rx.recv() => {
                if notification.is_none() {
                    break; // Channel closed
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_announces_utf16_when_position_encodings_are_advertised() {
        use tower_lsp_server::ls_types::{
            ClientCapabilities, GeneralClientCapabilities, PositionEncodingKind,
        };

        let capabilities = ClientCapabilities {
            general: Some(GeneralClientCapabilities {
                position_encodings: Some(vec![PositionEncodingKind::UTF8]),
                ..Default::default()
            }),
            ..Default::default()
        };

        assert_eq!(
            host_position_encoding(&capabilities),
            Some(PositionEncodingKind::UTF16),
        );
        assert_eq!(
            host_position_encoding(&ClientCapabilities::default()),
            None,
            "omitted capability uses the protocol's UTF-16 default",
        );
    }

    /// Initialize params carrying only the fields a test names.
    fn params_with(workspace: serde_json::Value) -> InitializeParams {
        let mut value = serde_json::json!({ "capabilities": {} });
        let object = value.as_object_mut().expect("an object");
        for (key, field) in workspace.as_object().expect("an object") {
            object.insert(key.clone(), field.clone());
        }
        serde_json::from_value(value).expect("valid initialize params")
    }

    /// The folder-change ladder resolves the current first folder, exactly as
    /// the handshake resolves `workspaceFolders[0]`.
    #[test]
    fn config_root_after_folder_change_uses_the_current_first_folder() {
        use std::path::PathBuf;
        use std::str::FromStr as _;
        let uri = Uri::from_str("file:///current").expect("a file URI");

        assert_eq!(
            config_root_after_folder_change(Some(&uri), Some(PathBuf::from("/folderless"))),
            Some(PathBuf::from("/current")),
            "a folder outranks the folderless fallback",
        );
    }

    /// An emptied folder list falls back to the rungs `initialize` resolved
    /// below `workspaceFolders`, and to nothing when the client named none.
    #[test]
    fn config_root_after_folder_change_falls_back_when_no_folder_remains() {
        use std::path::PathBuf;

        assert_eq!(
            config_root_after_folder_change(None, Some(PathBuf::from("/folderless"))),
            Some(PathBuf::from("/folderless")),
        );
        assert_eq!(
            config_root_after_folder_change(None, None),
            None,
            "a client that named no other root gets no project layer",
        );
    }

    /// The launch directory is a handshake-time last resort, not somewhere a
    /// folder change may migrate an established session: a folder URI naming no
    /// file path leaves the session rootless rather than reaching the CWD rung
    /// that `config_root_path` ends with.
    #[test]
    fn config_root_after_folder_change_never_reaches_the_process_cwd() {
        use std::path::PathBuf;
        use std::str::FromStr as _;
        let uri = Uri::from_str("untitled:Untitled-1").expect("a non-file URI");

        assert_eq!(config_root_after_folder_change(Some(&uri), None), None);
        assert_eq!(
            config_root_after_folder_change(Some(&uri), Some(PathBuf::from("/folderless"))),
            None,
            "an unresolvable folder does not fall through to the folderless rungs \
             either — the handshake ladder stops at its top rung too",
        );
    }

    #[test]
    fn client_root_anchors_config_discovery_to_a_legacy_root_path() {
        // The forwarded handshake and config discovery read one ladder, so a
        // client that names its workspace only through the deprecated
        // `rootPath` still gets its own `kakehashi.toml` — not the launch
        // directory's.
        let root_path = std::env::current_dir()
            .expect("current directory")
            .join("legacy-workspace");
        let params = params_with(serde_json::json!({
            "rootUri": null,
            "rootPath": root_path,
            "workspaceFolders": null
        }));

        let root = client_root(&params).expect("a client-supplied root");
        assert_eq!(root.to_file_path(), Some(root_path));
        assert_eq!(root.source(), "root_path (deprecated)");
    }

    #[test]
    fn config_root_keeps_the_cwd_when_the_client_opened_no_workspace() {
        // The asymmetry this change turns on, in one place: the handshake
        // forwards nothing, while Kakehashi still resolves its own config
        // against the launch directory. Deleting the surviving fallback for
        // symmetry with the bridge would break config discovery for exactly
        // the sessions #742 is about.
        let params = params_with(serde_json::json!({
            "rootUri": null,
            "workspaceFolders": null
        }));

        assert_eq!(bridge_root_uri(&params), None, "nothing is forwarded");

        let (root_path, source) = config_root_path(client_root(&params));
        assert_eq!(root_path, std::env::current_dir().ok());
        assert_eq!(source, "current working directory (fallback)");
    }

    #[test]
    fn config_root_reports_the_rung_it_resolved() {
        let root_path = std::env::current_dir()
            .expect("current directory")
            .join("legacy-workspace");
        let params = params_with(serde_json::json!({
            "rootUri": null,
            "rootPath": root_path,
            "workspaceFolders": null
        }));

        let (resolved, source) = config_root_path(client_root(&params));
        assert_eq!(resolved, Some(root_path));
        assert_eq!(
            source, "root_path (deprecated)",
            "the log must not call a client-named root a CWD fallback"
        );
    }

    #[test]
    fn config_root_falls_back_when_the_legacy_root_path_is_unanchorable() {
        let params = params_with(serde_json::json!({
            "rootUri": null,
            "rootPath": "relative/workspace",
            "workspaceFolders": null
        }));

        let (root_path, source) = config_root_path(client_root(&params));
        assert_eq!(root_path, std::env::current_dir().ok());
        assert_eq!(
            source, "current working directory (fallback)",
            "a dropped root must be reported as the fallback it became"
        );
    }

    #[test]
    fn client_root_rejects_a_relative_legacy_root_path() {
        // A relative `rootPath` resolves against the launch directory, so it
        // cannot anchor config paths; falling through to the CWD fallback keeps
        // that dependence named in the startup log instead of hidden.
        let params = params_with(serde_json::json!({
            "rootUri": null,
            "rootPath": "relative/workspace",
            "workspaceFolders": null
        }));

        let root = client_root(&params).expect("a client-supplied root");
        assert_eq!(root.to_file_path(), None);
        assert_eq!(bridge_root_uri(&params), None);
    }

    #[test]
    fn client_root_prefers_workspace_folders_over_the_deprecated_fields() {
        let params = params_with(serde_json::json!({
            "rootUri": "file:///from-root-uri",
            "rootPath": "/from-root-path",
            "workspaceFolders": [{ "uri": "file:///from-folders", "name": "folders" }]
        }));

        assert_eq!(
            bridge_root_uri(&params).as_deref(),
            Some("file:///from-folders")
        );
        assert_eq!(
            client_root(&params).expect("a root").source(),
            "workspace folders"
        );
    }

    #[test]
    fn client_root_prefers_root_uri_over_the_legacy_root_path() {
        let params = params_with(serde_json::json!({
            "rootUri": "file:///from-root-uri",
            "rootPath": "/from-root-path",
            "workspaceFolders": null
        }));

        assert_eq!(
            bridge_root_uri(&params).as_deref(),
            Some("file:///from-root-uri")
        );
        assert_eq!(
            client_root(&params).expect("a root").source(),
            "root_uri (deprecated)"
        );
    }

    /// The folder `bridge_workspace_folders` forwards for `params`, as
    /// `(uri, name)` — the shape a downstream server receives.
    fn forwarded_folders(params: &InitializeParams) -> Option<Vec<(String, String)>> {
        let root_uri = bridge_root_uri(params);
        bridge_workspace_folders(params, root_uri.as_deref()).map(|folders| {
            folders
                .into_iter()
                .map(|folder| (folder.uri.as_str().to_string(), folder.name))
                .collect()
        })
    }

    #[test]
    fn bridge_workspace_folders_names_a_synthesized_folder_after_the_root() {
        let params = params_with(serde_json::json!({
            "rootUri": "file:///home/dev/my-project",
            "workspaceFolders": null
        }));

        assert_eq!(
            forwarded_folders(&params),
            Some(vec![(
                "file:///home/dev/my-project".to_string(),
                "my-project".to_string()
            )])
        );
    }

    #[test]
    fn bridge_workspace_folders_synthesizes_from_a_legacy_root_path() {
        let root_path = std::env::current_dir()
            .expect("current directory")
            .join("legacy-workspace");
        let expected_uri = Url::from_file_path(&root_path).expect("an absolute root path");
        let params = params_with(serde_json::json!({
            "rootUri": null,
            "rootPath": root_path,
            "workspaceFolders": null
        }));

        assert_eq!(
            forwarded_folders(&params),
            Some(vec![(
                expected_uri.as_str().to_string(),
                "legacy-workspace".to_string()
            )])
        );
    }

    #[test]
    fn bridge_workspace_folders_falls_back_to_a_fixed_name_without_a_segment() {
        // A root with no last segment to name: `path_segments` yields the empty
        // string, which must not become the folder name.
        let params = params_with(serde_json::json!({
            "rootUri": "file:///",
            "workspaceFolders": null
        }));

        assert_eq!(
            forwarded_folders(&params),
            Some(vec![("file:///".to_string(), "workspace".to_string())])
        );
    }

    #[test]
    fn bridge_workspace_folders_forwards_an_empty_list_verbatim() {
        // `[]` is the client saying it has no folders. Synthesizing one from
        // `rootUri` here would invent the workspace #742 is about, so the empty
        // list is forwarded as sent.
        let params = params_with(serde_json::json!({
            "rootUri": "file:///home/dev/my-project",
            "workspaceFolders": []
        }));

        assert_eq!(forwarded_folders(&params), Some(vec![]));
        assert_eq!(
            bridge_root_uri(&params).as_deref(),
            Some("file:///home/dev/my-project"),
            "an empty folder list leaves the client's own rootUri untouched"
        );
    }

    #[test]
    fn bridge_root_uri_preserves_no_workspace_initialize() {
        let params: InitializeParams = serde_json::from_value(serde_json::json!({
            "capabilities": {},
            "rootUri": null,
            "workspaceFolders": null
        }))
        .expect("valid initialize params");

        let root_uri = bridge_root_uri(&params);
        assert_eq!(root_uri, None);
        assert_eq!(bridge_workspace_folders(&params, root_uri.as_deref()), None);
    }

    #[test]
    fn bridge_root_uri_preserves_legacy_root_path() {
        // A directory the process is NOT running in: the removed fallback
        // forwarded the CWD, so a CWD-valued fixture would pass under both the
        // old and the new derivation and pin nothing. Built from the CWD rather
        // than a literal so the path stays absolute on Windows too.
        let root_path = std::env::current_dir()
            .expect("current directory")
            .join("legacy-workspace");
        let expected = Url::from_file_path(&root_path).expect("an absolute root path");
        let params: InitializeParams = serde_json::from_value(serde_json::json!({
            "capabilities": {},
            "rootUri": null,
            "rootPath": root_path,
            "workspaceFolders": null
        }))
        .expect("valid initialize params");

        assert_eq!(bridge_root_uri(&params).as_deref(), Some(expected.as_str()));
    }

    #[test]
    fn workspace_capabilities_request_folder_change_notifications() {
        let workspace = workspace_server_capabilities();
        let folders = workspace
            .workspace_folders
            .expect("workspace folder capability");
        assert_eq!(folders.supported, Some(true));
        assert_eq!(folders.change_notifications, Some(OneOf::Left(true)));
    }

    /// A throwaway cancel context for tests that don't exercise cancellation.
    fn test_forwarded_cancel() -> crate::lsp::bridge::ForwardedRequestCancel {
        crate::lsp::bridge::ForwardedRequestCancel {
            connection_id: crate::lsp::bridge::ProgressConnectionId::for_test(0),
            request_id: tower_lsp_server::jsonrpc::Id::Number(1),
            token: tokio_util::sync::CancellationToken::new(),
            generation: 0,
        }
    }

    mod coalesce {
        use super::super::coalesce_upstream_batch;
        use crate::lsp::bridge::{ProgressConnectionId, UpstreamNotification};
        use tower_lsp_server::ls_types::Diagnostic;

        fn publish(conn: u64, uri: &str, msg: &str) -> UpstreamNotification {
            UpstreamNotification::PublishDiagnostics {
                uri: uri.to_string(),
                server: "srv".to_string(),
                connection_id: ProgressConnectionId::for_test(conn),
                diagnostics: vec![Diagnostic {
                    message: msg.to_string(),
                    ..Default::default()
                }],
            }
        }

        /// A `PublishDiagnostics` carrying an empty list — a *clearing* push.
        fn publish_clear(conn: u64, uri: &str) -> UpstreamNotification {
            UpstreamNotification::PublishDiagnostics {
                uri: uri.to_string(),
                server: "srv".to_string(),
                connection_id: ProgressConnectionId::for_test(conn),
                diagnostics: vec![],
            }
        }

        fn evict(conn: u64) -> UpstreamNotification {
            UpstreamNotification::EvictConnectionDiagnostics {
                connection_id: ProgressConnectionId::for_test(conn),
            }
        }

        fn is_empty_publish(n: &UpstreamNotification) -> bool {
            matches!(
                n,
                UpstreamNotification::PublishDiagnostics { diagnostics, .. } if diagnostics.is_empty()
            )
        }

        /// The latest message of `out[idx]`, which must be a `PublishDiagnostics`.
        fn msg_at(out: &[UpstreamNotification], idx: usize) -> &str {
            match &out[idx] {
                UpstreamNotification::PublishDiagnostics { diagnostics, .. } => {
                    diagnostics[0].message.as_str()
                }
                other => panic!("expected PublishDiagnostics at {idx}, got {other:?}"),
            }
        }

        #[test]
        fn collapses_consecutive_same_key_to_latest() {
            let out = coalesce_upstream_batch(vec![
                publish(1, "u", "a"),
                publish(1, "u", "b"),
                publish(1, "u", "c"),
            ]);
            assert_eq!(out.len(), 1, "three pushes for one key collapse to one");
            assert_eq!(msg_at(&out, 0), "c", "the latest push wins");
        }

        #[test]
        fn keeps_distinct_keys() {
            // Different uri, and different connection on the same uri, are distinct.
            let out = coalesce_upstream_batch(vec![
                publish(1, "u", "a"),
                publish(1, "v", "b"),
                publish(2, "u", "c"),
            ]);
            assert_eq!(out.len(), 3, "distinct (connection, uri) keys are all kept");
        }

        #[test]
        fn does_not_coalesce_across_an_evict_barrier() {
            // Publish, evict the same connection, publish again: the evict is a
            // barrier, so the two same-key pushes are NOT collapsed and stay ordered.
            let out =
                coalesce_upstream_batch(vec![publish(1, "u", "a"), evict(1), publish(1, "u", "b")]);
            assert_eq!(out.len(), 3);
            assert_eq!(msg_at(&out, 0), "a");
            assert!(matches!(
                out[1],
                UpstreamNotification::EvictConnectionDiagnostics { .. }
            ));
            assert_eq!(msg_at(&out, 2), "b");
        }

        #[test]
        fn publish_then_evict_keeps_publish_first() {
            // The ordering invariant: a publish for a connection is delivered before
            // the evict for that connection (publish-then-evict nets to evicted).
            let out = coalesce_upstream_batch(vec![publish(1, "u", "a"), evict(1)]);
            assert_eq!(out.len(), 2);
            assert!(matches!(
                out[0],
                UpstreamNotification::PublishDiagnostics { .. }
            ));
            assert!(matches!(
                out[1],
                UpstreamNotification::EvictConnectionDiagnostics { .. }
            ));
        }

        #[test]
        fn non_publish_notification_is_a_barrier() {
            // A non-publish notification (DiagnosticRefresh) flushes pending pushes
            // ahead of it and stops coalescing across it — order is preserved.
            let out = coalesce_upstream_batch(vec![
                publish(1, "u", "a1"),
                publish(1, "u", "a2"),
                UpstreamNotification::DiagnosticRefresh,
                publish(1, "u", "a3"),
            ]);
            assert_eq!(out.len(), 3);
            assert_eq!(
                msg_at(&out, 0),
                "a2",
                "the run before the barrier coalesces"
            );
            assert!(matches!(out[1], UpstreamNotification::DiagnosticRefresh));
            assert_eq!(
                msg_at(&out, 2),
                "a3",
                "the push after the barrier is separate"
            );
        }

        #[test]
        fn passes_a_lone_non_publish_through_unchanged() {
            let out = coalesce_upstream_batch(vec![UpstreamNotification::DiagnosticRefresh]);
            assert_eq!(out.len(), 1);
            assert!(matches!(out[0], UpstreamNotification::DiagnosticRefresh));
        }

        #[test]
        fn coalesces_a_run_then_delivers_the_evict_after_it() {
            // [P, P, Evict]: the two same-key pushes collapse to the latest, then the
            // evict (which originally followed both) is delivered after it.
            let out =
                coalesce_upstream_batch(vec![publish(1, "u", "a"), publish(1, "u", "b"), evict(1)]);
            assert_eq!(out.len(), 2);
            assert_eq!(msg_at(&out, 0), "b");
            assert!(matches!(
                out[1],
                UpstreamNotification::EvictConnectionDiagnostics { .. }
            ));
        }

        #[test]
        fn a_later_clear_supersedes_an_earlier_error_in_a_run() {
            // The clear must win (latest), so the editor ends cleared — a dropped
            // clear would leave a stale diagnostic on screen.
            let out = coalesce_upstream_batch(vec![publish(1, "u", "err"), publish_clear(1, "u")]);
            assert_eq!(out.len(), 1);
            assert!(
                is_empty_publish(&out[0]),
                "the clearing push supersedes the earlier error"
            );
        }

        #[test]
        fn survivors_keep_fifo_order_even_when_two_connections_share_a_slot() {
            // A restarted server pushes for the same uri under a NEW connection id:
            // [A1(c1,u), B(c2,u), A2(c1,u)]. (c1,u) and (c2,u) are distinct coalescing
            // keys but the same cache slot (host, source, server). The superseded A1 is
            // dropped and the survivors keep FIFO order — B then A2 — so the slot's last
            // writer stays A2, matching the un-coalesced FIFO (NOT A2 then B, which would
            // make B the last writer and mis-handle a later Evict(c1)).
            let out = coalesce_upstream_batch(vec![
                publish(1, "u", "a1"),
                publish(2, "u", "b"),
                publish(1, "u", "a2"),
            ]);
            assert_eq!(out.len(), 2, "the superseded a1 is dropped");
            assert_eq!(
                msg_at(&out, 0),
                "b",
                "survivors keep FIFO order: b precedes a2"
            );
            assert_eq!(msg_at(&out, 1), "a2", "a2 stays the slot's last writer");
        }

        #[test]
        fn coalesces_each_key_independently_around_a_barrier() {
            // Two keys before a barrier (one coalesced to its latest, last-occurrence
            // order), the barrier, then the first key again after it (kept separate).
            let out = coalesce_upstream_batch(vec![
                publish(1, "u", "a1"),
                publish(2, "v", "b"),
                publish(1, "u", "a2"),
                UpstreamNotification::DiagnosticRefresh,
                publish(1, "u", "a3"),
            ]);
            assert_eq!(out.len(), 4);
            assert_eq!(
                msg_at(&out, 0),
                "b",
                "the surviving a2 follows b in FIFO order"
            );
            assert_eq!(msg_at(&out, 1), "a2", "key (1,u) coalesces to its latest");
            assert!(matches!(out[2], UpstreamNotification::DiagnosticRefresh));
            assert_eq!(
                msg_at(&out, 3),
                "a3",
                "the push after the barrier is separate"
            );
        }
    }

    /// Regression guard for the create-before-progress ordering the feature
    /// depends on: the forwarding loop must deliver `window/workDoneProgress/create`
    /// to the editor (and receive its reply) BEFORE the corresponding `$/progress`.
    /// A refactor of the inline `await` to a `tokio::spawn` would break this and
    /// is exactly what this asserts against (window-work-done-progress bridging).
    #[tokio::test]
    async fn forwarding_loop_delivers_create_before_progress() {
        use crate::lsp::bridge::UpstreamNotification;
        use futures::{SinkExt, StreamExt};
        use std::sync::{Arc, Mutex};
        use tower::{Service, ServiceExt};
        use tower_lsp_server::jsonrpc::{Request, Response};
        use tower_lsp_server::ls_types::{
            InitializeParams, InitializeResult, NumberOrString, ProgressParams,
            ProgressParamsValue, WorkDoneProgress, WorkDoneProgressBegin,
        };
        use tower_lsp_server::{LanguageServer, LspService};

        struct Dummy;
        impl LanguageServer for Dummy {
            async fn initialize(
                &self,
                _: InitializeParams,
            ) -> tower_lsp_server::jsonrpc::Result<InitializeResult> {
                Ok(InitializeResult::default())
            }
            async fn shutdown(&self) -> tower_lsp_server::jsonrpc::Result<()> {
                Ok(())
            }
        }

        // Build a real tower-lsp Client + socket; capture the Client.
        let captured: Arc<Mutex<Option<Client>>> = Arc::new(Mutex::new(None));
        let captured_for_init = Arc::clone(&captured);
        let (mut service, socket) = LspService::build(move |client| {
            *captured_for_init.lock().unwrap() = Some(client);
            Dummy
        })
        .finish();
        let client = captured.lock().unwrap().take().unwrap();

        // Server→client messages are suppressed until the client is Initialized;
        // drive an initialize request to flip that state.
        let init = Request::build("initialize")
            .params(serde_json::json!({ "capabilities": {} }))
            .id(1)
            .finish();
        let _ = service.ready().await.unwrap().call(init).await;

        let (mut requests, mut responses) = socket.split();

        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let cancel = tokio_util::sync::CancellationToken::new();
        // Keep `_window_tx` alive so the bounded window channel does not close
        // and break the loop early; this test exercises only the upstream channel.
        let (_window_tx, window_rx) = tokio::sync::mpsc::channel(16);
        // Keep `_request_tx` alive so the request channel does not close and
        // break the loop early; this test exercises only the upstream channel.
        let (_request_tx, request_rx) = tokio::sync::mpsc::unbounded_channel();
        let loop_handle = tokio::spawn(upstream_forwarding_loop(
            rx,
            window_rx,
            request_rx,
            None,
            crate::lsp::bridge::InboundRequestRegistry::default(),
            client,
            None,
            cancel.clone(),
            true,
        ));

        let token = NumberOrString::String("kakehashi/bridge/progress/0".to_string());
        tx.send(UpstreamNotification::CreateWorkDoneProgress {
            token: token.clone(),
        })
        .unwrap();
        tx.send(UpstreamNotification::Progress {
            params: ProgressParams {
                token: token.clone(),
                value: ProgressParamsValue::WorkDone(WorkDoneProgress::Begin(
                    WorkDoneProgressBegin {
                        title: "Indexing".to_string(),
                        cancellable: None,
                        message: None,
                        percentage: None,
                    },
                )),
            },
        })
        .unwrap();

        // First server→client message MUST be the create request.
        let first = requests.next().await.expect("create request emitted");
        assert_eq!(first.method(), "window/workDoneProgress/create");
        let id = first.id().expect("create request has an id").clone();
        // Reply so the loop's inline await completes and it forwards progress.
        responses
            .send(Response::from_ok(id, serde_json::Value::Null))
            .await
            .unwrap();

        // Second message MUST be the $/progress notification — proving create
        // was delivered (and answered) strictly before progress.
        let second = requests
            .next()
            .await
            .expect("progress notification emitted");
        assert_eq!(second.method(), "$/progress");
        assert_eq!(
            second.params().unwrap()["token"],
            serde_json::json!("kakehashi/bridge/progress/0")
        );

        cancel.cancel();
        let _ = loop_handle.await;
    }

    /// Cancellation is re-checked *between* deliveries of a coalesced batch, so a
    /// shutdown mid-batch is not delayed by the rest of the batch (#426). Two
    /// `CreateWorkDoneProgress` (distinct tokens — each an inline-awaited request, so
    /// both survive coalescing as a 2-item barrier batch) are drained into one batch;
    /// cancelling during the first's editor round-trip must make the loop skip the
    /// second and exit.
    #[tokio::test]
    async fn forwarding_loop_rechecks_cancellation_between_batched_deliveries() {
        use crate::lsp::bridge::UpstreamNotification;
        use futures::{SinkExt, StreamExt};
        use std::sync::{Arc, Mutex};
        use tower::{Service, ServiceExt};
        use tower_lsp_server::jsonrpc::{Request, Response};
        use tower_lsp_server::ls_types::{InitializeParams, InitializeResult, NumberOrString};
        use tower_lsp_server::{LanguageServer, LspService};

        struct Dummy;
        impl LanguageServer for Dummy {
            async fn initialize(
                &self,
                _: InitializeParams,
            ) -> tower_lsp_server::jsonrpc::Result<InitializeResult> {
                Ok(InitializeResult::default())
            }
            async fn shutdown(&self) -> tower_lsp_server::jsonrpc::Result<()> {
                Ok(())
            }
        }

        let captured: Arc<Mutex<Option<Client>>> = Arc::new(Mutex::new(None));
        let captured_for_init = Arc::clone(&captured);
        let (mut service, socket) = LspService::build(move |client| {
            *captured_for_init.lock().unwrap() = Some(client);
            Dummy
        })
        .finish();
        let client = captured.lock().unwrap().take().unwrap();

        let init = Request::build("initialize")
            .params(serde_json::json!({ "capabilities": {} }))
            .id(1)
            .finish();
        let _ = service.ready().await.unwrap().call(init).await;

        let (mut requests, mut responses) = socket.split();

        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let cancel = tokio_util::sync::CancellationToken::new();
        let (_window_tx, window_rx) = tokio::sync::mpsc::channel(16);
        let (_request_tx, request_rx) = tokio::sync::mpsc::unbounded_channel();

        // Two creates (distinct tokens) queued before the loop runs → drained into
        // one batch. Each is an inline-awaited server→client request, so they map to
        // the "deliver first / cancel mid-round-trip / skip second" scenario.
        tx.send(UpstreamNotification::CreateWorkDoneProgress {
            token: NumberOrString::String("batch-canary-a".to_string()),
        })
        .unwrap();
        tx.send(UpstreamNotification::CreateWorkDoneProgress {
            token: NumberOrString::String("batch-canary-b".to_string()),
        })
        .unwrap();

        let loop_handle = tokio::spawn(upstream_forwarding_loop(
            rx,
            window_rx,
            request_rx,
            None,
            crate::lsp::bridge::InboundRequestRegistry::default(),
            client,
            None,
            cancel.clone(),
            true,
        ));

        // The loop delivers the first create (a request that awaits the editor).
        let first = requests.next().await.expect("first create request emitted");
        assert_eq!(first.method(), "window/workDoneProgress/create");
        // Cancel during the first delivery's round-trip; when it completes the loop
        // must re-check cancellation and skip the second batched create.
        cancel.cancel();
        responses
            .send(Response::from_ok(
                first.id().expect("create request id").clone(),
                serde_json::Value::Null,
            ))
            .await
            .unwrap();

        // The loop must skip the second create and exit. Were cancellation NOT
        // re-checked mid-batch, it would deliver the second create and block on its
        // (never-sent) response — so this would hang. The timeout turns that
        // regression into a failure instead of an infinite hang.
        tokio::time::timeout(std::time::Duration::from_secs(5), loop_handle)
            .await
            .expect("loop must exit after cancellation, not block on the skipped refresh")
            .unwrap();
        // `requests`/`responses` intentionally dropped without asserting stream
        // closure: `service` still holds a Client clone, so the stream stays open.
        drop((requests, responses));
    }

    /// When the editor REJECTS `window/workDoneProgress/create`, the loop must
    /// NOT forward that token's `$/progress` (the editor never created it). A
    /// later, unrelated request still goes through — proving only the progress
    /// was dropped, not the loop (window-work-done-progress).
    #[tokio::test]
    async fn forwarding_loop_drops_progress_when_create_rejected() {
        use crate::lsp::bridge::UpstreamNotification;
        use futures::{SinkExt, StreamExt};
        use std::sync::{Arc, Mutex};
        use tower::{Service, ServiceExt};
        use tower_lsp_server::jsonrpc::{Error, Request, Response};
        use tower_lsp_server::ls_types::{
            InitializeParams, InitializeResult, NumberOrString, ProgressParams,
            ProgressParamsValue, WorkDoneProgress, WorkDoneProgressBegin,
        };
        use tower_lsp_server::{LanguageServer, LspService};

        struct Dummy;
        impl LanguageServer for Dummy {
            async fn initialize(
                &self,
                _: InitializeParams,
            ) -> tower_lsp_server::jsonrpc::Result<InitializeResult> {
                Ok(InitializeResult::default())
            }
            async fn shutdown(&self) -> tower_lsp_server::jsonrpc::Result<()> {
                Ok(())
            }
        }

        let captured: Arc<Mutex<Option<Client>>> = Arc::new(Mutex::new(None));
        let captured_for_init = Arc::clone(&captured);
        let (mut service, socket) = LspService::build(move |client| {
            *captured_for_init.lock().unwrap() = Some(client);
            Dummy
        })
        .finish();
        let client = captured.lock().unwrap().take().unwrap();
        let init = Request::build("initialize")
            .params(serde_json::json!({ "capabilities": {} }))
            .id(1)
            .finish();
        let _ = service.ready().await.unwrap().call(init).await;

        let (mut requests, mut responses) = socket.split();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let cancel = tokio_util::sync::CancellationToken::new();
        // Keep `_window_tx` alive so the bounded window channel does not close
        // and break the loop early; this test exercises only the upstream channel.
        let (_window_tx, window_rx) = tokio::sync::mpsc::channel(16);
        // Keep `_request_tx` alive so the request channel does not close and
        // break the loop early; this test exercises only the upstream channel.
        let (_request_tx, request_rx) = tokio::sync::mpsc::unbounded_channel();
        let loop_handle = tokio::spawn(upstream_forwarding_loop(
            rx,
            window_rx,
            request_rx,
            None,
            crate::lsp::bridge::InboundRequestRegistry::default(),
            client,
            None,
            cancel.clone(),
            true,
        ));

        let token = NumberOrString::String("kakehashi/bridge/progress/0".to_string());
        tx.send(UpstreamNotification::CreateWorkDoneProgress {
            token: token.clone(),
        })
        .unwrap();
        tx.send(UpstreamNotification::Progress {
            params: ProgressParams {
                token,
                value: ProgressParamsValue::WorkDone(WorkDoneProgress::Begin(
                    WorkDoneProgressBegin {
                        title: "Indexing".to_string(),
                        cancellable: None,
                        message: None,
                        percentage: None,
                    },
                )),
            },
        })
        .unwrap();
        // A later, unrelated message that IS expected to reach the editor — a
        // telemetry notification, used as the delivery canary because it is
        // forwarded unconditionally (the refresh forward is now publisher-gated, so
        // it no longer works as a publisher-independent canary, #521).
        tx.send(UpstreamNotification::TelemetryEvent {
            data: serde_json::json!({ "canary": "after-rejected-create" }),
        })
        .unwrap();

        // First message: the create request — reject it.
        let first = requests.next().await.expect("create request emitted");
        assert_eq!(first.method(), "window/workDoneProgress/create");
        let id = first.id().expect("create request has an id").clone();
        responses
            .send(Response::from_error(id, Error::internal_error()))
            .await
            .unwrap();

        // Next message MUST be the telemetry event, NOT $/progress — the rejected
        // token's progress was dropped. (telemetry/event is a notification, so there
        // is no response to send.)
        let next = tokio::time::timeout(std::time::Duration::from_secs(5), requests.next())
            .await
            .expect("the telemetry canary must arrive, not hang (fail fast on a dropped forward)")
            .expect("a follow-up message emitted");
        assert_eq!(
            next.method(),
            "telemetry/event",
            "progress for a rejected token must be dropped; only the later message survives"
        );

        cancel.cancel();
        let _ = loop_handle.await;
    }

    /// `ForgetWorkDoneProgress` (sent when a downstream reader exits mid-progress)
    /// drops the token's admission, so a late `$/progress` for it is not
    /// forwarded — preventing the created-token set from leaking across respawns.
    #[tokio::test]
    async fn forwarding_loop_forgets_progress_on_connection_purge() {
        use crate::lsp::bridge::UpstreamNotification;
        use futures::{SinkExt, StreamExt};
        use std::sync::{Arc, Mutex};
        use tower::{Service, ServiceExt};
        use tower_lsp_server::jsonrpc::{Request, Response};
        use tower_lsp_server::ls_types::{
            InitializeParams, InitializeResult, NumberOrString, ProgressParams,
            ProgressParamsValue, WorkDoneProgress, WorkDoneProgressBegin,
        };
        use tower_lsp_server::{LanguageServer, LspService};

        struct Dummy;
        impl LanguageServer for Dummy {
            async fn initialize(
                &self,
                _: InitializeParams,
            ) -> tower_lsp_server::jsonrpc::Result<InitializeResult> {
                Ok(InitializeResult::default())
            }
            async fn shutdown(&self) -> tower_lsp_server::jsonrpc::Result<()> {
                Ok(())
            }
        }

        let captured: Arc<Mutex<Option<Client>>> = Arc::new(Mutex::new(None));
        let captured_for_init = Arc::clone(&captured);
        let (mut service, socket) = LspService::build(move |client| {
            *captured_for_init.lock().unwrap() = Some(client);
            Dummy
        })
        .finish();
        let client = captured.lock().unwrap().take().unwrap();
        let init = Request::build("initialize")
            .params(serde_json::json!({ "capabilities": {} }))
            .id(1)
            .finish();
        let _ = service.ready().await.unwrap().call(init).await;

        let (mut requests, mut responses) = socket.split();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let cancel = tokio_util::sync::CancellationToken::new();
        // Keep `_window_tx` alive so the bounded window channel does not close
        // and break the loop early; this test exercises only the upstream channel.
        let (_window_tx, window_rx) = tokio::sync::mpsc::channel(16);
        // Keep `_request_tx` alive so the request channel does not close and
        // break the loop early; this test exercises only the upstream channel.
        let (_request_tx, request_rx) = tokio::sync::mpsc::unbounded_channel();
        let loop_handle = tokio::spawn(upstream_forwarding_loop(
            rx,
            window_rx,
            request_rx,
            None,
            crate::lsp::bridge::InboundRequestRegistry::default(),
            client,
            None,
            cancel.clone(),
            true,
        ));

        let token = NumberOrString::String("kakehashi/bridge/progress/0".to_string());
        // Editor accepts the create (admits the token).
        tx.send(UpstreamNotification::CreateWorkDoneProgress {
            token: token.clone(),
        })
        .unwrap();
        let first = requests.next().await.expect("create request emitted");
        assert_eq!(first.method(), "window/workDoneProgress/create");
        let id = first.id().expect("create request has an id").clone();
        responses
            .send(Response::from_ok(id, serde_json::Value::Null))
            .await
            .unwrap();

        // Connection dies mid-progress: forget the token, then a late progress
        // arrives, then an unrelated request.
        tx.send(UpstreamNotification::ForgetWorkDoneProgress(vec![
            token.clone(),
        ]))
        .unwrap();
        tx.send(UpstreamNotification::Progress {
            params: ProgressParams {
                token,
                value: ProgressParamsValue::WorkDone(WorkDoneProgress::Begin(
                    WorkDoneProgressBegin {
                        title: "Indexing".to_string(),
                        cancellable: None,
                        message: None,
                        percentage: None,
                    },
                )),
            },
        })
        .unwrap();
        // Telemetry notification as the delivery canary (unconditionally forwarded,
        // unlike the now publisher-gated refresh, #521).
        tx.send(UpstreamNotification::TelemetryEvent {
            data: serde_json::json!({ "canary": "after-forget" }),
        })
        .unwrap();

        // The forgotten token's progress must be dropped; the next editor-bound
        // message is the telemetry event.
        let next = tokio::time::timeout(std::time::Duration::from_secs(5), requests.next())
            .await
            .expect("the telemetry canary must arrive, not hang (fail fast on a dropped forward)")
            .expect("a follow-up message emitted");
        assert_eq!(
            next.method(),
            "telemetry/event",
            "progress for a forgotten token must be dropped"
        );

        cancel.cancel();
        let _ = loop_handle.await;
    }

    /// On connection teardown, a token that was **begun but not yet ended** gets a
    /// synthetic `$/progress` `End` forwarded to the editor, so its progress
    /// indicator does not dangle (ls-bridge-progress-disconnect-cleanup). (A token
    /// created but never begun gets none — see the sibling test above, where the
    /// `Begin` arrives after the forget and is dropped.)
    #[tokio::test]
    async fn forwarding_loop_synthesizes_end_for_begun_token_on_purge() {
        use crate::lsp::bridge::UpstreamNotification;
        use futures::{SinkExt, StreamExt};
        use std::sync::{Arc, Mutex};
        use tower::{Service, ServiceExt};
        use tower_lsp_server::jsonrpc::{Request, Response};
        use tower_lsp_server::ls_types::{
            InitializeParams, InitializeResult, NumberOrString, ProgressParams,
            ProgressParamsValue, WorkDoneProgress, WorkDoneProgressBegin,
        };
        use tower_lsp_server::{LanguageServer, LspService};

        struct Dummy;
        impl LanguageServer for Dummy {
            async fn initialize(
                &self,
                _: InitializeParams,
            ) -> tower_lsp_server::jsonrpc::Result<InitializeResult> {
                Ok(InitializeResult::default())
            }
            async fn shutdown(&self) -> tower_lsp_server::jsonrpc::Result<()> {
                Ok(())
            }
        }

        let captured: Arc<Mutex<Option<Client>>> = Arc::new(Mutex::new(None));
        let captured_for_init = Arc::clone(&captured);
        let (mut service, socket) = LspService::build(move |client| {
            *captured_for_init.lock().unwrap() = Some(client);
            Dummy
        })
        .finish();
        let client = captured.lock().unwrap().take().unwrap();
        let init = Request::build("initialize")
            .params(serde_json::json!({ "capabilities": {} }))
            .id(1)
            .finish();
        let _ = service.ready().await.unwrap().call(init).await;

        let (mut requests, mut responses) = socket.split();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let cancel = tokio_util::sync::CancellationToken::new();
        let (_window_tx, window_rx) = tokio::sync::mpsc::channel(16);
        let (_request_tx, request_rx) = tokio::sync::mpsc::unbounded_channel();
        let loop_handle = tokio::spawn(upstream_forwarding_loop(
            rx,
            window_rx,
            request_rx,
            None,
            crate::lsp::bridge::InboundRequestRegistry::default(),
            client,
            None,
            cancel.clone(),
            true,
        ));

        let token = NumberOrString::String("kakehashi/bridge/progress/0".to_string());
        // Editor accepts the create (admits the token).
        tx.send(UpstreamNotification::CreateWorkDoneProgress {
            token: token.clone(),
        })
        .unwrap();
        let first = requests.next().await.expect("create request emitted");
        assert_eq!(first.method(), "window/workDoneProgress/create");
        let id = first.id().expect("create request has an id").clone();
        responses
            .send(Response::from_ok(id, serde_json::Value::Null))
            .await
            .unwrap();

        // Downstream begins the work; the `Begin` is forwarded to the editor,
        // marking the token begun-not-ended.
        tx.send(UpstreamNotification::Progress {
            params: ProgressParams {
                token: token.clone(),
                value: ProgressParamsValue::WorkDone(WorkDoneProgress::Begin(
                    WorkDoneProgressBegin {
                        title: "Indexing".to_string(),
                        cancellable: None,
                        message: None,
                        percentage: None,
                    },
                )),
            },
        })
        .unwrap();
        let begin = requests.next().await.expect("begin progress forwarded");
        assert_eq!(begin.method(), "$/progress");

        // Connection dies mid-progress: the loop must synthesize an `End` for the
        // begun-not-ended token so the editor's indicator clears.
        tx.send(UpstreamNotification::ForgetWorkDoneProgress(vec![
            token.clone(),
        ]))
        .unwrap();
        let end = tokio::time::timeout(std::time::Duration::from_secs(2), requests.next())
            .await
            .expect("a synthetic End must be forwarded on purge of a begun token")
            .expect("stream yielded a message");
        assert_eq!(end.method(), "$/progress");
        let params: ProgressParams =
            serde_json::from_value(end.params().expect("progress has params").clone())
                .expect("valid ProgressParams");
        assert_eq!(params.token, token, "synthetic End targets the begun token");
        assert!(
            matches!(
                params.value,
                ProgressParamsValue::WorkDone(WorkDoneProgress::End(_))
            ),
            "synthetic notification must be an End"
        );

        cancel.cancel();
        let _ = loop_handle.await;
    }

    /// A token that already received its real `End` is **not** double-ended when a
    /// later `ForgetWorkDoneProgress` arrives: the real `End` clears the
    /// begun-not-ended set first (both share the FIFO upstream channel), so the
    /// purge finds nothing to synthesize (ls-bridge-progress-disconnect-cleanup).
    #[tokio::test]
    async fn forwarding_loop_does_not_double_end_a_finished_token_on_purge() {
        use crate::lsp::bridge::UpstreamNotification;
        use futures::{SinkExt, StreamExt};
        use std::sync::{Arc, Mutex};
        use tower::{Service, ServiceExt};
        use tower_lsp_server::jsonrpc::{Request, Response};
        use tower_lsp_server::ls_types::{
            InitializeParams, InitializeResult, NumberOrString, ProgressParams,
            ProgressParamsValue, WorkDoneProgress, WorkDoneProgressBegin, WorkDoneProgressEnd,
        };
        use tower_lsp_server::{LanguageServer, LspService};

        struct Dummy;
        impl LanguageServer for Dummy {
            async fn initialize(
                &self,
                _: InitializeParams,
            ) -> tower_lsp_server::jsonrpc::Result<InitializeResult> {
                Ok(InitializeResult::default())
            }
            async fn shutdown(&self) -> tower_lsp_server::jsonrpc::Result<()> {
                Ok(())
            }
        }

        let captured: Arc<Mutex<Option<Client>>> = Arc::new(Mutex::new(None));
        let captured_for_init = Arc::clone(&captured);
        let (mut service, socket) = LspService::build(move |client| {
            *captured_for_init.lock().unwrap() = Some(client);
            Dummy
        })
        .finish();
        let client = captured.lock().unwrap().take().unwrap();
        let init = Request::build("initialize")
            .params(serde_json::json!({ "capabilities": {} }))
            .id(1)
            .finish();
        let _ = service.ready().await.unwrap().call(init).await;

        let (mut requests, mut responses) = socket.split();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let cancel = tokio_util::sync::CancellationToken::new();
        let (_window_tx, window_rx) = tokio::sync::mpsc::channel(16);
        let (_request_tx, request_rx) = tokio::sync::mpsc::unbounded_channel();
        let loop_handle = tokio::spawn(upstream_forwarding_loop(
            rx,
            window_rx,
            request_rx,
            None,
            crate::lsp::bridge::InboundRequestRegistry::default(),
            client,
            None,
            cancel.clone(),
            true,
        ));

        let token = NumberOrString::String("kakehashi/bridge/progress/0".to_string());
        tx.send(UpstreamNotification::CreateWorkDoneProgress {
            token: token.clone(),
        })
        .unwrap();
        let first = requests.next().await.expect("create request emitted");
        let id = first.id().expect("create request has an id").clone();
        responses
            .send(Response::from_ok(id, serde_json::Value::Null))
            .await
            .unwrap();

        // Begin then a real End: the token is now ended (cleared from begun set).
        for value in [
            ProgressParamsValue::WorkDone(WorkDoneProgress::Begin(WorkDoneProgressBegin {
                title: "Indexing".to_string(),
                cancellable: None,
                message: None,
                percentage: None,
            })),
            ProgressParamsValue::WorkDone(WorkDoneProgress::End(WorkDoneProgressEnd {
                message: None,
            })),
        ] {
            tx.send(UpstreamNotification::Progress {
                params: ProgressParams {
                    token: token.clone(),
                    value,
                },
            })
            .unwrap();
            let p = requests.next().await.expect("progress forwarded");
            assert_eq!(p.method(), "$/progress");
        }

        // Now the connection is purged. Because the token already ended, no second
        // `End` is synthesized; the next editor-bound message is the sentinel.
        tx.send(UpstreamNotification::ForgetWorkDoneProgress(vec![
            token.clone(),
        ]))
        .unwrap();
        // Telemetry notification as the delivery canary (unconditionally forwarded,
        // unlike the now publisher-gated refresh, #521).
        tx.send(UpstreamNotification::TelemetryEvent {
            data: serde_json::json!({ "canary": "after-purge-ended" }),
        })
        .unwrap();
        let next = tokio::time::timeout(std::time::Duration::from_secs(5), requests.next())
            .await
            .expect("the telemetry canary must arrive, not hang (fail fast on a dropped forward)")
            .expect("a follow-up message emitted");
        assert_eq!(
            next.method(),
            "telemetry/event",
            "an already-ended token must not be ended a second time on purge"
        );

        cancel.cancel();
        let _ = loop_handle.await;
    }

    /// Re-create / stale-eviction edge: when a begun token's upstream id is
    /// evicted (the downstream re-creates the token), the stale id is forgotten
    /// — synthesizing its `End` so the stale indicator clears — while the new
    /// upstream id runs an independent lifecycle, ended normally with no second
    /// `End`. The two ids stay separate through `begun_tokens`
    /// (ls-bridge-progress-disconnect-cleanup).
    #[tokio::test]
    async fn forwarding_loop_keeps_recreated_token_separate_from_evicted_one() {
        use crate::lsp::bridge::UpstreamNotification;
        use futures::{SinkExt, StreamExt};
        use std::sync::{Arc, Mutex};
        use tower::{Service, ServiceExt};
        use tower_lsp_server::jsonrpc::{Request, Response};
        use tower_lsp_server::ls_types::{
            InitializeParams, InitializeResult, NumberOrString, ProgressParams,
            ProgressParamsValue, WorkDoneProgress, WorkDoneProgressBegin, WorkDoneProgressEnd,
        };
        use tower_lsp_server::{LanguageServer, LspService};

        struct Dummy;
        impl LanguageServer for Dummy {
            async fn initialize(
                &self,
                _: InitializeParams,
            ) -> tower_lsp_server::jsonrpc::Result<InitializeResult> {
                Ok(InitializeResult::default())
            }
            async fn shutdown(&self) -> tower_lsp_server::jsonrpc::Result<()> {
                Ok(())
            }
        }

        let captured: Arc<Mutex<Option<Client>>> = Arc::new(Mutex::new(None));
        let captured_for_init = Arc::clone(&captured);
        let (mut service, socket) = LspService::build(move |client| {
            *captured_for_init.lock().unwrap() = Some(client);
            Dummy
        })
        .finish();
        let client = captured.lock().unwrap().take().unwrap();
        let init = Request::build("initialize")
            .params(serde_json::json!({ "capabilities": {} }))
            .id(1)
            .finish();
        let _ = service.ready().await.unwrap().call(init).await;

        let (mut requests, mut responses) = socket.split();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let cancel = tokio_util::sync::CancellationToken::new();
        let (_window_tx, window_rx) = tokio::sync::mpsc::channel(16);
        let (_request_tx, request_rx) = tokio::sync::mpsc::unbounded_channel();
        let loop_handle = tokio::spawn(upstream_forwarding_loop(
            rx,
            window_rx,
            request_rx,
            None,
            crate::lsp::bridge::InboundRequestRegistry::default(),
            client,
            None,
            cancel.clone(),
            true,
        ));

        let stale = NumberOrString::String("kakehashi/bridge/progress/0".to_string());
        let fresh = NumberOrString::String("kakehashi/bridge/progress/1".to_string());
        let begin = |token: &NumberOrString| UpstreamNotification::Progress {
            params: ProgressParams {
                token: token.clone(),
                value: ProgressParamsValue::WorkDone(WorkDoneProgress::Begin(
                    WorkDoneProgressBegin {
                        title: "Indexing".to_string(),
                        cancellable: None,
                        message: None,
                        percentage: None,
                    },
                )),
            },
        };

        // Stale token: created (accept) then begun (forwarded).
        tx.send(UpstreamNotification::CreateWorkDoneProgress {
            token: stale.clone(),
        })
        .unwrap();
        let req = requests.next().await.expect("create(stale) emitted");
        assert_eq!(req.method(), "window/workDoneProgress/create");
        responses
            .send(Response::from_ok(
                req.id().unwrap().clone(),
                serde_json::Value::Null,
            ))
            .await
            .unwrap();
        tx.send(begin(&stale)).unwrap();
        let p = requests.next().await.expect("begin(stale) forwarded");
        assert_eq!(p.method(), "$/progress");

        // Re-create evicts the stale upstream id: forget(stale). The begun stale
        // id must be ended so its indicator clears.
        tx.send(UpstreamNotification::ForgetWorkDoneProgress(vec![
            stale.clone(),
        ]))
        .unwrap();
        let end = requests
            .next()
            .await
            .expect("synthetic End(stale) forwarded");
        assert_eq!(end.method(), "$/progress");
        let end: ProgressParams =
            serde_json::from_value(end.params().expect("has params").clone()).unwrap();
        assert_eq!(end.token, stale, "synthetic End targets the evicted id");
        assert!(matches!(
            end.value,
            ProgressParamsValue::WorkDone(WorkDoneProgress::End(_))
        ));

        // Fresh token: independent lifecycle, ended normally.
        tx.send(UpstreamNotification::CreateWorkDoneProgress {
            token: fresh.clone(),
        })
        .unwrap();
        let req = requests.next().await.expect("create(fresh) emitted");
        assert_eq!(req.method(), "window/workDoneProgress/create");
        responses
            .send(Response::from_ok(
                req.id().unwrap().clone(),
                serde_json::Value::Null,
            ))
            .await
            .unwrap();
        tx.send(begin(&fresh)).unwrap();
        let p = requests.next().await.expect("begin(fresh) forwarded");
        assert_eq!(p.method(), "$/progress");
        tx.send(UpstreamNotification::Progress {
            params: ProgressParams {
                token: fresh.clone(),
                value: ProgressParamsValue::WorkDone(WorkDoneProgress::End(WorkDoneProgressEnd {
                    message: None,
                })),
            },
        })
        .unwrap();
        let p = requests.next().await.expect("end(fresh) forwarded");
        assert_eq!(p.method(), "$/progress");

        // The fresh token ended normally, so its later purge synthesizes nothing.
        tx.send(UpstreamNotification::ForgetWorkDoneProgress(vec![
            fresh.clone(),
        ]))
        .unwrap();
        // Telemetry notification as the delivery canary (unconditionally forwarded,
        // unlike the now publisher-gated refresh, #521).
        tx.send(UpstreamNotification::TelemetryEvent {
            data: serde_json::json!({ "canary": "after-purge-fresh" }),
        })
        .unwrap();
        let next = tokio::time::timeout(std::time::Duration::from_secs(5), requests.next())
            .await
            .expect("the telemetry canary must arrive, not hang (fail fast on a dropped forward)")
            .expect("a follow-up message emitted");
        assert_eq!(
            next.method(),
            "telemetry/event",
            "fresh token was ended normally; its purge must synthesize no second End"
        );

        cancel.cancel();
        let _ = loop_handle.await;
    }

    /// `ClientProgress` is forwarded to the editor **ungated** — without any
    /// `window/workDoneProgress/create` admission, because the editor minted the
    /// client `workDoneToken` itself (ls-bridge-client-progress). Contrast
    /// `forwarding_loop_drops_progress_when_create_rejected`, where server-declared
    /// `Progress` for an un-created token is dropped.
    #[tokio::test]
    async fn forwarding_loop_forwards_client_progress_ungated() {
        use crate::lsp::bridge::UpstreamNotification;
        use futures::StreamExt;
        use std::sync::{Arc, Mutex};
        use tower::{Service, ServiceExt};
        use tower_lsp_server::jsonrpc::Request;
        use tower_lsp_server::ls_types::{
            InitializeParams, InitializeResult, NumberOrString, ProgressParams,
            ProgressParamsValue, WorkDoneProgress, WorkDoneProgressBegin,
        };
        use tower_lsp_server::{LanguageServer, LspService};

        struct Dummy;
        impl LanguageServer for Dummy {
            async fn initialize(
                &self,
                _: InitializeParams,
            ) -> tower_lsp_server::jsonrpc::Result<InitializeResult> {
                Ok(InitializeResult::default())
            }
            async fn shutdown(&self) -> tower_lsp_server::jsonrpc::Result<()> {
                Ok(())
            }
        }

        let captured: Arc<Mutex<Option<Client>>> = Arc::new(Mutex::new(None));
        let captured_for_init = Arc::clone(&captured);
        let (mut service, socket) = LspService::build(move |client| {
            *captured_for_init.lock().unwrap() = Some(client);
            Dummy
        })
        .finish();
        let client = captured.lock().unwrap().take().unwrap();
        let init = Request::build("initialize")
            .params(serde_json::json!({ "capabilities": {} }))
            .id(1)
            .finish();
        let _ = service.ready().await.unwrap().call(init).await;

        let (mut requests, _responses) = socket.split();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let cancel = tokio_util::sync::CancellationToken::new();
        let (_window_tx, window_rx) = tokio::sync::mpsc::channel(16);
        let (_request_tx, request_rx) = tokio::sync::mpsc::unbounded_channel();
        let loop_handle = tokio::spawn(upstream_forwarding_loop(
            rx,
            window_rx,
            request_rx,
            None,
            crate::lsp::bridge::InboundRequestRegistry::default(),
            client,
            None,
            cancel.clone(),
            true,
        ));

        // No create was ever sent for this token, yet ClientProgress must be
        // forwarded (the editor owns the token).
        let token = NumberOrString::String("editor-wd-1".to_string());
        tx.send(UpstreamNotification::ClientProgress {
            params: ProgressParams {
                token: token.clone(),
                value: ProgressParamsValue::WorkDone(WorkDoneProgress::Begin(
                    WorkDoneProgressBegin {
                        title: "Finding references".to_string(),
                        cancellable: None,
                        message: None,
                        percentage: None,
                    },
                )),
            },
        })
        .unwrap();

        let msg = tokio::time::timeout(std::time::Duration::from_secs(2), requests.next())
            .await
            .expect("client progress must be forwarded ungated")
            .expect("stream yielded a message");
        assert_eq!(msg.method(), "$/progress");
        let params: ProgressParams =
            serde_json::from_value(msg.params().expect("has params").clone()).unwrap();
        assert_eq!(params.token, token, "forwarded under the client token");

        cancel.cancel();
        let _ = loop_handle.await;
    }

    /// Test that upstream_forwarding_loop exits when its CancellationToken is cancelled,
    /// even if the channel is still open.
    #[tokio::test]
    async fn upstream_forwarding_loop_exits_on_cancellation() {
        use crate::lsp::bridge::UpstreamNotification;
        use std::time::Duration;
        use tokio_util::sync::CancellationToken;

        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let (window_tx, window_rx) = tokio::sync::mpsc::channel(16);
        let token = CancellationToken::new();

        // Send notifications on both channels before cancellation — they
        // should be received/drained by the loop
        tx.send(UpstreamNotification::DiagnosticRefresh).unwrap();
        window_tx
            .try_send(UpstreamNotification::LogMessage {
                typ: tower_lsp_server::ls_types::MessageType::INFO,
                message: "[kakehashi:test] hello".to_string(),
            })
            .unwrap();

        // Spawn the loop with a cancellation token (channels stay open via the senders)
        let token_clone = token.clone();
        let handle = tokio::spawn(upstream_forwarding_loop_with_cancel(
            rx,
            window_rx,
            token_clone,
        ));

        // Give the loop time to process the notification
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Cancel the token — loop should exit even though tx is still alive
        token.cancel();

        // The loop should exit promptly
        let result = tokio::time::timeout(Duration::from_secs(1), handle).await;
        let join_result = result
            .expect("upstream_forwarding_loop should exit when token is cancelled (timed out)");
        assert!(
            join_result.is_ok(),
            "upstream_forwarding_loop task panicked or was aborted after cancellation"
        );
    }

    /// Build an initialized tower-lsp `Client` plus the socket halves, so a test
    /// can observe server→client traffic and answer requests. Server→client
    /// messages are suppressed until the client is `Initialized`, so an
    /// `initialize` request is driven through first.
    #[cfg(test)]
    async fn init_client_and_socket() -> (
        Client,
        impl futures::Stream<Item = tower_lsp_server::jsonrpc::Request> + Unpin,
        impl futures::Sink<tower_lsp_server::jsonrpc::Response> + Unpin,
    ) {
        use std::sync::{Arc, Mutex};
        use tower::{Service, ServiceExt};
        use tower_lsp_server::jsonrpc::Request;
        use tower_lsp_server::ls_types::{InitializeParams, InitializeResult};
        use tower_lsp_server::{LanguageServer, LspService};

        struct Dummy;
        impl LanguageServer for Dummy {
            async fn initialize(
                &self,
                _: InitializeParams,
            ) -> tower_lsp_server::jsonrpc::Result<InitializeResult> {
                Ok(InitializeResult::default())
            }
            async fn shutdown(&self) -> tower_lsp_server::jsonrpc::Result<()> {
                Ok(())
            }
        }

        let captured: Arc<Mutex<Option<Client>>> = Arc::new(Mutex::new(None));
        let captured_for_init = Arc::clone(&captured);
        let (mut service, socket) = LspService::build(move |client| {
            *captured_for_init.lock().unwrap() = Some(client);
            Dummy
        })
        .finish();
        let client = captured.lock().unwrap().take().unwrap();

        let init = Request::build("initialize")
            .params(serde_json::json!({ "capabilities": {} }))
            .id(1)
            .finish();
        let _ = service.ready().await.unwrap().call(init).await;

        let (requests, responses) = socket.split();
        (client, requests, responses)
    }

    #[tokio::test]
    async fn forwarding_loop_relays_show_message_request_response() {
        use crate::lsp::bridge::UpstreamRequest;
        use futures::{SinkExt, StreamExt};
        use tower_lsp_server::jsonrpc::Response;
        use tower_lsp_server::ls_types::MessageType;

        let (client, mut requests, mut responses) = init_client_and_socket().await;

        // `_upstream_tx`/`_window_tx` kept alive so those channels stay open and
        // the loop doesn't exit early; this test drives only the request channel.
        let (_upstream_tx, upstream_rx) = tokio::sync::mpsc::unbounded_channel();
        let (_window_tx, window_rx) = tokio::sync::mpsc::channel(16);
        let (request_tx, request_rx) = tokio::sync::mpsc::unbounded_channel();
        let cancel = tokio_util::sync::CancellationToken::new();
        let loop_handle = tokio::spawn(upstream_forwarding_loop(
            upstream_rx,
            window_rx,
            request_rx,
            None,
            crate::lsp::bridge::InboundRequestRegistry::default(),
            client,
            None,
            cancel.clone(),
            true,
        ));

        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        request_tx
            .send(UpstreamRequest::ShowMessageRequest {
                typ: MessageType::INFO,
                message: "pick one".to_string(),
                actions: Some(vec![
                    serde_json::from_value(serde_json::json!({ "title": "Retry" })).unwrap(),
                ]),
                reply: reply_tx,
                cancel: test_forwarded_cancel(),
            })
            .unwrap();

        // The editor receives the request and answers with the selected action.
        let req = requests.next().await.expect("showMessageRequest emitted");
        assert_eq!(req.method(), "window/showMessageRequest");
        let id = req.id().expect("request has an id").clone();
        let _ = responses
            .send(Response::from_ok(
                id,
                serde_json::json!({ "title": "Retry" }),
            ))
            .await;

        let action = reply_rx.await.expect("reply delivered");
        assert_eq!(action.expect("an action selected").title, "Retry");

        cancel.cancel();
        let _ = loop_handle.await;
    }

    #[tokio::test]
    async fn log_message_without_delivery_context_fails_closed() {
        use crate::lsp::bridge::UpstreamNotification;
        use futures::StreamExt;
        use tower_lsp_server::ls_types::MessageType;

        let (client, mut requests, _responses) = init_client_and_socket().await;
        deliver_upstream_notification(
            &client,
            UpstreamNotification::LogMessage {
                typ: MessageType::ERROR,
                message: "must be gated".to_string(),
            },
            &mut std::collections::HashSet::new(),
            &mut std::collections::HashSet::new(),
            None,
        )
        .await;

        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), requests.next())
                .await
                .is_err(),
            "missing policy context must not bypass the global log gate"
        );
    }

    #[tokio::test]
    async fn forwarding_loop_relays_show_document_response() {
        use crate::lsp::bridge::UpstreamRequest;
        use futures::{SinkExt, StreamExt};
        use tower_lsp_server::jsonrpc::Response;

        let (client, mut requests, mut responses) = init_client_and_socket().await;

        // `_upstream_tx`/`_window_tx` kept alive so those channels stay open and
        // the loop doesn't exit early; this test drives only the request channel.
        let (_upstream_tx, upstream_rx) = tokio::sync::mpsc::unbounded_channel();
        let (_window_tx, window_rx) = tokio::sync::mpsc::channel(16);
        let (request_tx, request_rx) = tokio::sync::mpsc::unbounded_channel();
        let cancel = tokio_util::sync::CancellationToken::new();
        let loop_handle = tokio::spawn(upstream_forwarding_loop(
            upstream_rx,
            window_rx,
            request_rx,
            None,
            crate::lsp::bridge::InboundRequestRegistry::default(),
            client,
            None,
            cancel.clone(),
            true,
        ));

        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        request_tx
            .send(UpstreamRequest::ShowDocument {
                params: serde_json::from_value(serde_json::json!({ "uri": "file:///x.rs" }))
                    .unwrap(),
                reply: reply_tx,
                cancel: test_forwarded_cancel(),
            })
            .unwrap();

        let req = requests.next().await.expect("showDocument emitted");
        assert_eq!(req.method(), "window/showDocument");
        let id = req.id().expect("request has an id").clone();
        let _ = responses
            .send(Response::from_ok(
                id,
                serde_json::json!({ "success": true }),
            ))
            .await;

        assert!(reply_rx.await.expect("reply delivered"));

        cancel.cancel();
        let _ = loop_handle.await;
    }

    #[tokio::test]
    async fn apply_edit_answers_applied_false_when_editor_lacks_capability() {
        use crate::lsp::bridge::UpstreamRequest;

        let (client, mut requests, _responses) = init_client_and_socket().await;

        let (_upstream_tx, upstream_rx) = tokio::sync::mpsc::unbounded_channel();
        let (_window_tx, window_rx) = tokio::sync::mpsc::channel(16);
        let (request_tx, request_rx) = tokio::sync::mpsc::unbounded_channel();
        let cancel = tokio_util::sync::CancellationToken::new();
        let registry = crate::lsp::bridge::InboundRequestRegistry::default();
        let loop_handle = tokio::spawn(upstream_forwarding_loop(
            upstream_rx,
            window_rx,
            request_rx,
            None,
            registry.clone(),
            client,
            None,
            cancel.clone(),
            // The editor did NOT declare workspace.applyEdit: the request
            // must be answered applied:false locally, never forwarded.
            false,
        ));

        // Register the request the way the reader does, so the test verifies
        // the local-answer path unregisters it.
        let connection_id = crate::lsp::bridge::ProgressConnectionId::for_test(0);
        let request_id = tower_lsp_server::jsonrpc::Id::Number(1);
        let (token, generation) = registry.register(connection_id, request_id.clone());
        let forwarded_cancel = crate::lsp::bridge::ForwardedRequestCancel {
            connection_id,
            request_id: request_id.clone(),
            token: token.clone(),
            generation,
        };
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        request_tx
            .send(UpstreamRequest::ApplyEdit {
                params: serde_json::from_value(serde_json::json!({
                    "edit": { "changes": { "file:///x.rs": [] } }
                }))
                .unwrap(),
                connection: crate::lsp::bridge::ConnectionKey::for_server("test"),
                reply: reply_tx,
                cancel: forwarded_cancel,
            })
            .unwrap();

        // The reply arrives WITHOUT any editor round-trip (no response is ever
        // fed to `_responses`), proving the request was answered locally.
        let response = tokio::time::timeout(std::time::Duration::from_secs(5), reply_rx)
            .await
            .expect("a local answer must arrive without any editor round-trip")
            .expect("reply delivered");
        // And nothing was emitted toward the editor: the request stream must
        // be empty (a regression that both forwards and answers locally would
        // otherwise pass). Yield first so a wrongly-spawned forward gets a
        // fair chance to run before the bounded negative-observation window.
        tokio::task::yield_now().await;
        let no_request = tokio::time::timeout(
            std::time::Duration::from_millis(250),
            futures::StreamExt::next(&mut requests),
        )
        .await;
        assert!(
            no_request.is_err(),
            "no editor-bound request may be emitted, got: {no_request:?}"
        );
        assert!(!response.applied);
        assert!(
            response
                .failure_reason
                .as_deref()
                .is_some_and(|r| r.contains("workspace.applyEdit")),
            "failureReason should name the missing capability: {:?}",
            response.failure_reason
        );

        // The registry entry must be gone: a $/cancelRequest after the local
        // answer must find nothing to cancel.
        registry.cancel(connection_id, &request_id);
        assert!(
            !token.is_cancelled(),
            "the entry must have been unregistered by the local-answer path"
        );

        cancel.cancel();
        let _ = loop_handle.await;
    }

    #[tokio::test]
    async fn forwarding_loop_relays_apply_edit_response() {
        use crate::lsp::bridge::UpstreamRequest;
        use futures::{SinkExt, StreamExt};
        use tower_lsp_server::jsonrpc::Response;

        let (client, mut requests, mut responses) = init_client_and_socket().await;

        // `_upstream_tx`/`_window_tx` kept alive so those channels stay open and
        // the loop doesn't exit early; this test drives only the request channel.
        let (_upstream_tx, upstream_rx) = tokio::sync::mpsc::unbounded_channel();
        let (_window_tx, window_rx) = tokio::sync::mpsc::channel(16);
        let (request_tx, request_rx) = tokio::sync::mpsc::unbounded_channel();
        let cancel = tokio_util::sync::CancellationToken::new();
        let loop_handle = tokio::spawn(upstream_forwarding_loop(
            upstream_rx,
            window_rx,
            request_rx,
            None,
            crate::lsp::bridge::InboundRequestRegistry::default(),
            client,
            None,
            cancel.clone(),
            true,
        ));

        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        request_tx
            .send(UpstreamRequest::ApplyEdit {
                connection: crate::lsp::bridge::ConnectionKey::for_server("test"),
                params: serde_json::from_value(serde_json::json!({
                    "edit": { "changes": { "file:///x.rs": [] } }
                }))
                .unwrap(),
                reply: reply_tx,
                cancel: test_forwarded_cancel(),
            })
            .unwrap();

        let req = requests.next().await.expect("applyEdit emitted");
        assert_eq!(req.method(), "workspace/applyEdit");
        let id = req.id().expect("request has an id").clone();
        let _ = responses
            .send(Response::from_ok(
                id,
                serde_json::json!({ "applied": true }),
            ))
            .await;

        let response = reply_rx.await.expect("reply delivered");
        assert!(response.applied);
        assert_eq!(response.failure_reason, None);

        cancel.cancel();
        let _ = loop_handle.await;
    }

    /// The editor's `failedChange` indexes the FORWARDED documentChanges
    /// array. When translation removed an entry (here: a no-op virtual
    /// entry), the index spaces diverge and the relayed response must DROP
    /// the index instead of misindexing the downstream's original array;
    /// with no removal the index must relay untouched.
    #[tokio::test]
    async fn forwarding_loop_drops_failed_change_only_when_translation_removed_entries() {
        use crate::lsp::bridge::{BridgeCoordinator, UpstreamRequest, VirtualDocumentUri};
        use futures::{SinkExt, StreamExt};
        use std::str::FromStr;
        use tower_lsp_server::jsonrpc::Response;

        let (client, mut requests, mut responses) = init_client_and_socket().await;

        let (_upstream_tx, upstream_rx) = tokio::sync::mpsc::unbounded_channel();
        let (_window_tx, window_rx) = tokio::sync::mpsc::channel(16);
        let (request_tx, request_rx) = tokio::sync::mpsc::unbounded_channel();
        let cancel = tokio_util::sync::CancellationToken::new();
        let translators = Some(Arc::new(UpstreamRequestTranslators {
            show_document: ShowDocumentTranslator::new(
                Arc::new(crate::document::DocumentStore::new()),
                Arc::new(crate::language::LanguageCoordinator::new()),
                Arc::new(BridgeCoordinator::new()),
            ),
            apply_edit: ApplyEditTranslator::new(
                Arc::new(crate::document::DocumentStore::new()),
                Arc::new(crate::language::LanguageCoordinator::new()),
                Arc::new(BridgeCoordinator::new()),
            ),
        }));
        let loop_handle = tokio::spawn(upstream_forwarding_loop(
            upstream_rx,
            window_rx,
            request_rx,
            translators,
            crate::lsp::bridge::InboundRequestRegistry::default(),
            client,
            None,
            cancel.clone(),
            true,
        ));

        let host = tower_lsp_server::ls_types::Uri::from_str("file:///project/doc.md").unwrap();
        let virtual_uri =
            VirtualDocumentUri::new(&host, "lua", "01ARZ3NDEKTSV4RRFFQ69G5FAV").to_uri_string();
        let real_edit = serde_json::json!({
            "textDocument": { "uri": "file:///x.rs", "version": null },
            "edits": [{
                "range": {
                    "start": { "line": 0, "character": 0 },
                    "end": { "line": 0, "character": 1 }
                },
                "newText": "x"
            }]
        });

        // Round 1: [no-op virtual entry, real edit] — the no-op is removed
        // before forwarding, so the editor's failedChange: 0 (the real edit,
        // index 0 FORWARDED) would misindex the downstream's array (where
        // index 0 is the virtual no-op). It must be dropped.
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        request_tx
            .send(UpstreamRequest::ApplyEdit {
                connection: crate::lsp::bridge::ConnectionKey::for_server("test"),
                params: serde_json::from_value(serde_json::json!({
                    "edit": { "documentChanges": [
                        {
                            "textDocument": { "uri": virtual_uri, "version": null },
                            "edits": []
                        },
                        real_edit.clone()
                    ] }
                }))
                .unwrap(),
                reply: reply_tx,
                cancel: test_forwarded_cancel(),
            })
            .unwrap();
        let req = requests.next().await.expect("applyEdit emitted");
        let id = req.id().expect("request has an id").clone();
        let _ = responses
            .send(Response::from_ok(
                id,
                serde_json::json!({ "applied": false, "failedChange": 0 }),
            ))
            .await;
        let response = reply_rx.await.expect("reply delivered");
        assert!(!response.applied);
        assert_eq!(
            response.failed_change, None,
            "a failedChange computed against a shrunken array must be dropped"
        );

        // Round 2 (control): real-only edit, nothing removed — the index
        // spaces align and failedChange must relay untouched.
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        request_tx
            .send(UpstreamRequest::ApplyEdit {
                connection: crate::lsp::bridge::ConnectionKey::for_server("test"),
                params: serde_json::from_value(serde_json::json!({
                    "edit": { "documentChanges": [real_edit] }
                }))
                .unwrap(),
                reply: reply_tx,
                cancel: test_forwarded_cancel(),
            })
            .unwrap();
        let req = requests.next().await.expect("applyEdit emitted");
        let id = req.id().expect("request has an id").clone();
        let _ = responses
            .send(Response::from_ok(
                id,
                serde_json::json!({ "applied": false, "failedChange": 0 }),
            ))
            .await;
        let response = reply_rx.await.expect("reply delivered");
        assert_eq!(
            response.failed_change,
            Some(0),
            "with aligned index spaces the editor's failedChange must relay"
        );

        cancel.cancel();
        let _ = loop_handle.await;
    }

    /// An applyEdit whose edit can't be translated to host coordinates (here: a
    /// virtual URI no open document maps to) must be answered `applied: false`
    /// locally — the editor must never see the corrupted edit (#568).
    #[tokio::test]
    async fn forwarding_loop_answers_untranslatable_apply_edit_locally() {
        use crate::lsp::bridge::{BridgeCoordinator, UpstreamRequest, VirtualDocumentUri};
        use std::str::FromStr;
        use std::time::Duration;
        use tokio::time::timeout;

        let (client, _requests, _responses) = init_client_and_socket().await;

        let (_upstream_tx, upstream_rx) = tokio::sync::mpsc::unbounded_channel();
        let (_window_tx, window_rx) = tokio::sync::mpsc::channel(16);
        let (request_tx, request_rx) = tokio::sync::mpsc::unbounded_channel();
        let cancel = tokio_util::sync::CancellationToken::new();
        // Real translators over empty stores: the virtual URI resolves to no
        // open document, so translation rejects the edit.
        let translators = Some(Arc::new(UpstreamRequestTranslators {
            show_document: ShowDocumentTranslator::new(
                Arc::new(crate::document::DocumentStore::new()),
                Arc::new(crate::language::LanguageCoordinator::new()),
                Arc::new(BridgeCoordinator::new()),
            ),
            apply_edit: ApplyEditTranslator::new(
                Arc::new(crate::document::DocumentStore::new()),
                Arc::new(crate::language::LanguageCoordinator::new()),
                Arc::new(BridgeCoordinator::new()),
            ),
        }));
        let loop_handle = tokio::spawn(upstream_forwarding_loop(
            upstream_rx,
            window_rx,
            request_rx,
            translators,
            crate::lsp::bridge::InboundRequestRegistry::default(),
            client,
            None,
            cancel.clone(),
            true,
        ));

        let host = tower_lsp_server::ls_types::Uri::from_str("file:///project/doc.md").unwrap();
        let virtual_uri =
            VirtualDocumentUri::new(&host, "lua", "01ARZ3NDEKTSV4RRFFQ69G5FAV").to_uri_string();
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        request_tx
            .send(UpstreamRequest::ApplyEdit {
                connection: crate::lsp::bridge::ConnectionKey::for_server("test"),
                // A NON-empty edit against a virtual URI that resolves to no open
                // document: the translator can't map it, so the loop must reject
                // it locally. (An empty edit array would be a no-op removed
                // before forwarding — see `remove_empty_virtual_entries`.)
                params: serde_json::from_value(serde_json::json!({
                    "edit": { "changes": { virtual_uri: [
                        { "range": {
                            "start": { "line": 0, "character": 0 },
                            "end": { "line": 0, "character": 1 }
                        }, "newText": "x" }
                    ] } }
                }))
                .unwrap(),
                reply: reply_tx,
                cancel: test_forwarded_cancel(),
            })
            .unwrap();

        // The reply arrives without the editor (`_requests` never serviced)
        // answering anything — proof the loop answered locally.
        let response = timeout(Duration::from_secs(5), reply_rx)
            .await
            .expect("locally-answered reply must not pend on the editor")
            .expect("reply delivered");
        assert!(!response.applied);
        assert!(
            response
                .failure_reason
                .as_deref()
                .is_some_and(|r| r.contains("unknown virtual document")),
            "failureReason should name the failure: {:?}",
            response.failure_reason
        );

        cancel.cancel();
        let _ = loop_handle.await;
    }

    /// #404: when a downstream's in-flight forwarded request is cancelled, the
    /// loop must forward a correlated `$/cancelRequest` to the editor (same id it
    /// minted) so the dialog is dismissed, and answer the downstream with the
    /// protocol default.
    #[tokio::test]
    async fn forwarding_loop_forwards_cancel_to_editor() {
        use crate::lsp::bridge::{ForwardedRequestCancel, InboundRequestRegistry, UpstreamRequest};
        use futures::StreamExt;
        use std::time::Duration;
        use tokio::time::timeout;
        use tower_lsp_server::ls_types::MessageType;

        let (client, mut requests, mut _responses) = init_client_and_socket().await;
        let (_upstream_tx, upstream_rx) = tokio::sync::mpsc::unbounded_channel();
        let (_window_tx, window_rx) = tokio::sync::mpsc::channel(16);
        let (request_tx, request_rx) = tokio::sync::mpsc::unbounded_channel();
        let loop_cancel = tokio_util::sync::CancellationToken::new();
        let loop_handle = tokio::spawn(upstream_forwarding_loop(
            upstream_rx,
            window_rx,
            request_rx,
            None,
            InboundRequestRegistry::default(),
            client,
            None,
            loop_cancel.clone(),
            true,
        ));

        // The per-request cancel token the reader would have registered.
        let request_token = tokio_util::sync::CancellationToken::new();
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        request_tx
            .send(UpstreamRequest::ShowMessageRequest {
                typ: MessageType::INFO,
                message: "pick one".to_string(),
                actions: None,
                reply: reply_tx,
                cancel: ForwardedRequestCancel {
                    connection_id: crate::lsp::bridge::ProgressConnectionId::for_test(0),
                    request_id: tower_lsp_server::jsonrpc::Id::Number(1),
                    token: request_token.clone(),
                    generation: 0,
                },
            })
            .unwrap();

        // The editor receives the forwarded request; we deliberately never answer.
        let req = timeout(Duration::from_secs(5), requests.next())
            .await
            .expect("timed out awaiting showMessageRequest")
            .expect("showMessageRequest emitted");
        assert_eq!(req.method(), "window/showMessageRequest");
        let editor_id = req.id().expect("request has an id").clone();

        // Downstream cancels → the loop forwards `$/cancelRequest` to the editor
        // with the same id, then answers the downstream with the default.
        request_token.cancel();

        let cancel_msg = timeout(Duration::from_secs(5), requests.next())
            .await
            .expect("timed out awaiting forwarded $/cancelRequest")
            .expect("cancel forwarded to editor");
        let wire = serde_json::to_value(&cancel_msg).expect("serialize cancel request");
        assert_eq!(wire["method"], "$/cancelRequest");
        assert_eq!(
            wire["params"]["id"],
            serde_json::to_value(&editor_id).unwrap(),
            "cancel targets the id the editor saw"
        );

        let action = timeout(Duration::from_secs(5), reply_rx)
            .await
            .expect("timed out awaiting downstream reply")
            .expect("reply delivered");
        assert!(
            action.is_none(),
            "a cancelled request answers with no selection"
        );

        loop_cancel.cancel();
        let _ = loop_handle.await;
    }

    #[tokio::test]
    async fn forwarding_loop_delivers_telemetry_event() {
        use crate::lsp::bridge::UpstreamNotification;
        use futures::StreamExt;

        let (client, mut requests, _responses) = init_client_and_socket().await;

        let (upstream_tx, upstream_rx) = tokio::sync::mpsc::unbounded_channel();
        let (_window_tx, window_rx) = tokio::sync::mpsc::channel(16);
        let (_request_tx, request_rx) = tokio::sync::mpsc::unbounded_channel();
        let cancel = tokio_util::sync::CancellationToken::new();
        let loop_handle = tokio::spawn(upstream_forwarding_loop(
            upstream_rx,
            window_rx,
            request_rx,
            None,
            crate::lsp::bridge::InboundRequestRegistry::default(),
            client,
            None,
            cancel.clone(),
            true,
        ));

        // An object payload passes through unchanged.
        upstream_tx
            .send(UpstreamNotification::TelemetryEvent {
                data: serde_json::json!({ "kind": "metric", "value": 42 }),
            })
            .unwrap();

        let event = requests.next().await.expect("telemetry/event emitted");
        assert_eq!(event.method(), "telemetry/event");
        assert_eq!(
            event.params().expect("telemetry params"),
            &serde_json::json!({ "kind": "metric", "value": 42 })
        );

        // A scalar payload is forwarded verbatim, NOT wrapped in an array (which
        // is what `client.telemetry_event` would do) — this is why a raw-`Value`
        // notification marker is used.
        upstream_tx
            .send(UpstreamNotification::TelemetryEvent {
                data: serde_json::json!(42),
            })
            .unwrap();

        let scalar = requests.next().await.expect("scalar telemetry emitted");
        assert_eq!(scalar.method(), "telemetry/event");
        assert_eq!(
            scalar.params().expect("telemetry params"),
            &serde_json::json!(42),
            "scalar telemetry payload must be forwarded verbatim, not wrapped"
        );

        cancel.cancel();
        let _ = loop_handle.await;
    }
}
