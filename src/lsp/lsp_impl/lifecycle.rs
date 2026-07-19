//! Lifecycle methods for Kakehashi (initialize, initialized, shutdown).

use std::sync::Arc;

use tower_lsp_server::Client;
use tower_lsp_server::jsonrpc::Result;
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
};
use url::Url;

use crate::analysis::{LEGEND_MODIFIERS, LEGEND_TYPES};
use crate::config::WorkspaceSettings;
use crate::lsp::client::check_semantic_tokens_refresh_support;
use crate::lsp::{SettingsSource, load_settings};

use super::apply_edit_translation::ApplyEditTranslator;
use super::show_document_translation::ShowDocumentTranslator;
use super::{Kakehashi, uri_to_url};

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

impl Kakehashi {
    pub(crate) async fn initialize_impl(
        &self,
        params: InitializeParams,
    ) -> Result<InitializeResult> {
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

        // Extract first workspace folder for reuse
        let first_folder = params.workspace_folders.as_ref().and_then(|f| f.first());

        // Determine primary URI from workspace folders or deprecated root_uri
        #[allow(deprecated)]
        let primary_uri: Option<&Uri> = first_folder.map(|f| &f.uri).or(params.root_uri.as_ref());

        // Get root URI string for downstream servers, falling back to current directory
        let root_uri_for_bridge: Option<String> =
            primary_uri.map(|uri| uri.to_string()).or_else(|| {
                std::env::current_dir()
                    .ok()
                    .and_then(|p| Url::from_file_path(p).ok())
                    .map(|u| u.to_string())
            });

        // Forward root_uri and workspace_folders to bridge pool for downstream server initialization
        self.bridge.pool().set_root_uri(root_uri_for_bridge.clone());

        use std::str::FromStr as _;
        let workspace_folders_for_bridge = params.workspace_folders.clone().or_else(|| {
            root_uri_for_bridge.as_ref().and_then(|uri| {
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
                let folder_uri = tower_lsp_server::ls_types::Uri::from_str(uri).ok()?;
                Some(vec![tower_lsp_server::ls_types::WorkspaceFolder {
                    uri: folder_uri,
                    name,
                }])
            })
        });
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

        // Get root path from primary URI, falling back to current directory
        let uri_to_path = |uri: &Uri| uri_to_url(uri).ok().and_then(|url| url.to_file_path().ok());
        let root_path = primary_uri
            .and_then(uri_to_path)
            .or_else(|| std::env::current_dir().ok());

        // Store root path for later use and log the source
        #[allow(deprecated)]
        if let Some(ref path) = root_path {
            let source = if params.workspace_folders.is_some() {
                "workspace folders"
            } else if params.root_uri.is_some() {
                "root_uri (deprecated)"
            } else {
                "current working directory (fallback)"
            };

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
        let settings_outcome = load_settings(
            root_path.as_deref(),
            params
                .initialization_options
                .map(|options| (SettingsSource::InitializationOptions, options)),
            self.home_dir.as_deref(),
            |var| std::env::var(var).ok(),
        );
        let settings_events = settings_outcome.events;
        let mut default_settings_warning = None;

        // Nudge users off the deprecated `rootMarkers` config key. The claim
        // guard latches session-wide so a later didChangeConfiguration carrying
        // `rootMarkers` does not warn a second time (and vice versa).
        if settings_outcome.used_deprecated_root_markers
            && self
                .settings_manager
                .claim_root_markers_deprecation_warning()
        {
            self.notifier()
                .show_warning(crate::config::deprecation::ROOT_MARKERS_DEPRECATION_NOTICE)
                .await;
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
                // their command names aren't known at initialize (and each routed
                // name embeds a per-document host_uri, so it could never be a
                // stable advertised entry anyway). Each server's RAW command
                // names — those from its static initialize result; a
                // downstream's later dynamic command registrations are not
                // collected — are dynamically registered as it reaches Ready
                // (`UpstreamRequest::RegisterCommands` below, gated on client
                // `dynamicRegistration`), which serves palette-fired commands
                // — via a session-global registry keyed by raw command id, so
                // a name advertised by several servers/roots routes to the
                // latest advertiser (accepted limitation).
                // Action-embedded commands carry ENCODED per-document names that
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
    }

    /// Arm a Unix-signal watcher that reaps the downstream server pool when
    /// the process is terminated WITHOUT the LSP shutdown handshake.
    ///
    /// Editors escalate: Neovim SIGTERMs a server that hasn't exited shortly
    /// after `shutdown`/`exit`, and a user restart can kill it outright. With
    /// no handler, kakehashi dies mid-handshake and its downstream children
    /// are orphaned — observed live as a `with-logging emmylua_ls` wrapper
    /// re-parented to launchd and running for hours, because not every
    /// downstream exits on stdin EOF. On SIGTERM/SIGHUP this persists the
    /// crash-detection marker (`parsing_in_progress` — a kill mid-parse is
    /// precisely what that marker detects) and runs the same bounded
    /// `shutdown_all` as the graceful path (LSP handshake, then
    /// SIGTERM→SIGKILL escalation, global timeout), then exits with the
    /// conventional 128+signal status. Unlike `shutdown_impl` it does NOT
    /// stop in-process work (forwarding loops, timers) — the process exits
    /// immediately after the reap, so only the two effects that outlive the
    /// process matter. A second signal during the reap aborts it and exits
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
        let failed_parsers = self.auto_install.failed_parsers_handle();
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
            // Crash-detection parity with `shutdown_impl`: a kill mid-parse is
            // exactly the case the `parsing_in_progress` marker exists for, so
            // persist it before the (comparatively slow) downstream reap.
            if let Err(e) = failed_parsers.persist_state() {
                log::warn!(
                    target: "kakehashi::crash_recovery",
                    "Failed to persist crash detection state on signal: {e}"
                );
            }
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
        // Persist crash detection state before shutdown
        // This enables crash recovery to detect if parsing was in progress
        if let Err(e) = self.auto_install.persist_state() {
            log::warn!(
                target: "kakehashi::crash_recovery",
                "Failed to persist crash detection state on shutdown: {}",
                e
            );
        }

        // Abort all synthetic diagnostic tasks (pull-first-diagnostic-forwarding Phase 2)
        self.synthetic_diagnostics.abort_all();

        // Cancel all debounced diagnostic timers (pull-first-diagnostic-forwarding Phase 3)
        self.debounced_diagnostics.cancel_all();

        // Abort all eager-open tasks to prevent orphaned didOpen during shutdown
        self.bridge.abort_all_eager_open();

        // Cancel the upstream forwarding task for deterministic shutdown.
        // Without this, the task only exits when all senders are dropped.
        self.shutdown_token.cancel();

        self.tree_worker_shadow.shutdown();

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

fn spawn_upstream_request(
    inbound_request_registry: crate::lsp::bridge::InboundRequestRegistry,
    translators: Option<Arc<UpstreamRequestTranslators>>,
    client: &Client,
    request: crate::lsp::bridge::UpstreamRequest,
    editor_supports_apply_edit: bool,
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
                // Fire-and-forget dynamic registration of palette command names.
                // A unique registration id (we never unregister — a disconnected
                // server's command simply routes fail-soft). Register-options
                // carry the `commands` for `workspace/executeCommand`.
                let id = format!("kakehashi/executeCommand/{}", client.next_request_id());
                let registration = Registration {
                    id,
                    method: "workspace/executeCommand".to_string(),
                    register_options: Some(serde_json::json!({ "commands": commands })),
                };
                // Bound the await: a non-responsive editor must not leak a task
                // pending forever on the registration request.
                match tokio::time::timeout(
                    std::time::Duration::from_secs(10),
                    client.register_capability(vec![registration]),
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
            // through `request_forwarded_diagnostic_refresh`, which forwards the
            // leading edge immediately and debounces later burst activity before
            // reusing the capability-gated, detached forced-refresh path. Detaching
            // avoids blocking this delivery loop on the editor round-trip
            // (head-of-line). A `None` publisher (test loop) has no settings to gate
            // on, so the forward is dropped; production always has one (#521, #789).
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
