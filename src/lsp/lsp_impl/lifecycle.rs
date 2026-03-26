//! Lifecycle methods for Kakehashi (initialize, initialized, shutdown).

use tower_lsp_server::Client;
use tower_lsp_server::jsonrpc::Result;
#[cfg(feature = "experimental")]
use tower_lsp_server::ls_types::ColorProviderCapability;
use tower_lsp_server::ls_types::{
    CompletionOptions, DeclarationCapability, DiagnosticOptions, DiagnosticServerCapabilities,
    DocumentLinkOptions, HoverProviderCapability, ImplementationProviderCapability,
    InitializeParams, InitializeResult, InitializedParams, OneOf, RenameOptions, SaveOptions,
    SelectionRangeProviderCapability, SemanticTokenModifier, SemanticTokenType,
    SemanticTokensFullOptions, SemanticTokensLegend, SemanticTokensOptions,
    SemanticTokensServerCapabilities, ServerCapabilities, ServerInfo, SignatureHelpOptions,
    TextDocumentSyncCapability, TextDocumentSyncKind, TextDocumentSyncOptions,
    TextDocumentSyncSaveOptions, TypeDefinitionProviderCapability, Uri, WorkDoneProgressOptions,
};
use url::Url;

use crate::analysis::{LEGEND_MODIFIERS, LEGEND_TYPES};
use crate::config::WorkspaceSettings;
use crate::lsp::client::check_semantic_tokens_refresh_support;
use crate::lsp::{SettingsSource, load_settings};

use super::{Kakehashi, uri_to_url};

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

impl Kakehashi {
    pub(crate) async fn initialize_impl(
        &self,
        params: InitializeParams,
    ) -> Result<InitializeResult> {
        // Store client capabilities for LSP compliance checks (e.g., refresh support).
        // Uses SettingsManager which wraps OnceLock for "set once, read many" semantics.
        self.settings_manager
            .set_capabilities(params.capabilities.clone());

        // Log capability state for troubleshooting client compatibility issues.
        log::debug!(
            "Client capabilities stored: semantic_tokens_refresh={}",
            check_semantic_tokens_refresh_support(&params.capabilities)
        );

        // Debug: Log initialization
        self.notifier()
            .log_info("Received initialization request")
            .await;

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

            self.notifier()
                .log_info(format!(
                    "Using workspace root from {}: {}",
                    source,
                    path.display()
                ))
                .await;
            self.settings_manager.set_root_path(Some(path.clone()));
        } else {
            self.notifier()
                .log_warning("Failed to determine workspace root - config file will not be loaded")
                .await;
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
        self.notifier()
            .log_settings_events(&settings_outcome.events)
            .await;

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
                &crate::config::defaults::default_settings(),
                self.home_dir.as_deref(),
                crate::config::expand::with_kakehashi_defaults(|var| std::env::var(var).ok()),
            ) {
                Ok(ws) => ws,
                Err(e) => {
                    log::error!(
                        "Failed to expand default settings: {e}. Falling back to empty defaults."
                    );
                    self.notifier()
                        .log_warning(format!(
                            "Failed to expand default settings: {e}. Some features (e.g., semantic highlighting, parser detection) may be degraded."
                        ))
                        .await;
                    WorkspaceSettings::default()
                }
            };
            (raw_settings, settings)
        };
        self.apply_raw_settings(raw_settings, settings).await;

        self.notifier().log_info("server initialized!").await;
        Ok(InitializeResult {
            server_info: Some(ServerInfo {
                name: "kakehashi".to_string(),
                version: Some(env!("CARGO_PKG_VERSION").to_string()),
            }),
            offset_encoding: None,
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Options(
                    TextDocumentSyncOptions {
                        open_close: Some(true),
                        change: Some(TextDocumentSyncKind::INCREMENTAL),
                        will_save: None,
                        will_save_wait_until: None,
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
                declaration_provider: Some(DeclarationCapability::Simple(true)),
                definition_provider: Some(OneOf::Left(true)),
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
                references_provider: Some(OneOf::Left(true)),
                document_highlight_provider: Some(OneOf::Left(true)),
                document_link_provider: Some(DocumentLinkOptions {
                    resolve_provider: None,
                    work_done_progress_options: WorkDoneProgressOptions::default(),
                }),
                document_symbol_provider: Some(OneOf::Left(true)),
                rename_provider: Some(OneOf::Right(RenameOptions {
                    prepare_provider: Some(true),
                    work_done_progress_options: WorkDoneProgressOptions::default(),
                })),
                inlay_hint_provider: Some(OneOf::Left(true)),
                #[cfg(feature = "experimental")]
                color_provider: Some(ColorProviderCapability::Simple(true)),
                #[cfg(not(feature = "experimental"))]
                color_provider: None,
                moniker_provider: Some(OneOf::Left(true)),
                // ADR-0020: Pull-first diagnostic forwarding
                diagnostic_provider: Some(DiagnosticServerCapabilities::Options(
                    DiagnosticOptions {
                        inter_file_dependencies: false,
                        workspace_diagnostics: false,
                        ..Default::default()
                    },
                )),
                ..ServerCapabilities::default()
            },
        })
    }

    pub(crate) async fn initialized_impl(&self, _: InitializedParams) {
        self.notifier().log_info("server is ready").await;

        // Forward downstream server notifications to upstream editor.
        // When a downstream LS sends workspace/diagnostic/refresh, the reader
        // task puts DiagnosticRefresh on this channel. We forward it to the
        // editor via Client::workspace_diagnostic_refresh() so the editor
        // triggers a fresh textDocument/diagnostic pull.
        if let Some(upstream_rx) = self.bridge.take_upstream_rx() {
            let client = self.client.clone();
            let token = self.shutdown_token.clone();
            tokio::spawn(upstream_forwarding_loop(upstream_rx, client, token));
        }
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

        // Abort all synthetic diagnostic tasks (ADR-0020 Phase 2)
        self.synthetic_diagnostics.abort_all();

        // Cancel all debounced diagnostic timers (ADR-0020 Phase 3)
        self.debounced_diagnostics.cancel_all();

        // Abort all eager-open tasks to prevent orphaned didOpen during shutdown
        self.bridge.abort_all_eager_open();

        // Cancel the upstream forwarding task for deterministic shutdown.
        // Without this, the task only exits when all senders are dropped.
        self.shutdown_token.cancel();

        // Graceful shutdown of all downstream language server connections (ADR-0017)
        // - Transitions to Closing state, sends LSP shutdown/exit handshake
        // - Escalates to SIGTERM/SIGKILL for unresponsive servers (Unix)
        self.bridge.shutdown_all().await;

        Ok(())
    }
}

/// Forward upstream notifications from downstream language servers to the editor.
///
/// Consumes notifications from `upstream_rx` and dispatches them to the LSP client.
/// Currently handles:
/// - `DiagnosticRefresh`: forwards `workspace/diagnostic/refresh` to trigger a
///   fresh diagnostic pull from the editor.
///
/// Exits when:
/// - The channel is closed (all senders dropped), OR
/// - The `cancel_token` is cancelled (deterministic shutdown)
async fn upstream_forwarding_loop(
    mut upstream_rx: tokio::sync::mpsc::UnboundedReceiver<crate::lsp::bridge::UpstreamNotification>,
    client: Client,
    cancel_token: tokio_util::sync::CancellationToken,
) {
    use crate::lsp::bridge::UpstreamNotification;
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
                    Some(UpstreamNotification::DiagnosticRefresh) => {
                        if let Err(e) = client.workspace_diagnostic_refresh().await {
                            log::debug!(
                                target: "kakehashi::bridge",
                                "workspace/diagnostic/refresh forwarding failed: {}",
                                e
                            );
                        }
                    }
                    None => break, // Channel closed
                }
            }
        }
    }
}

/// Cancellable upstream forwarding loop without a Client (for testing).
///
/// Drains notifications from the channel and exits when the token is cancelled
/// or the channel closes. Does not forward to any client.
#[cfg(test)]
async fn upstream_forwarding_loop_with_cancel(
    mut upstream_rx: tokio::sync::mpsc::UnboundedReceiver<crate::lsp::bridge::UpstreamNotification>,
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
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test that upstream_forwarding_loop exits when its CancellationToken is cancelled,
    /// even if the channel is still open.
    #[tokio::test]
    async fn upstream_forwarding_loop_exits_on_cancellation() {
        use crate::lsp::bridge::UpstreamNotification;
        use std::time::Duration;
        use tokio_util::sync::CancellationToken;

        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let token = CancellationToken::new();

        // Send a notification before cancellation — it should be received/drained by the loop
        tx.send(UpstreamNotification::DiagnosticRefresh).unwrap();

        // Spawn the loop with a cancellation token (channel stays open via `tx`)
        let token_clone = token.clone();
        let handle = tokio::spawn(upstream_forwarding_loop_with_cancel(rx, token_clone));

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
}
