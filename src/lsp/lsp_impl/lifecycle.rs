//! Lifecycle methods for Kakehashi (initialize, initialized, shutdown).

use std::sync::Arc;

use tower_lsp_server::Client;
use tower_lsp_server::jsonrpc::Result;
#[cfg(feature = "experimental")]
use tower_lsp_server::ls_types::ColorProviderCapability;
use tower_lsp_server::ls_types::{
    CodeLensOptions, CompletionOptions, DeclarationCapability, DiagnosticOptions,
    DiagnosticServerCapabilities, DocumentLinkOptions, DocumentOnTypeFormattingOptions,
    FoldingRangeProviderCapability, HoverProviderCapability, ImplementationProviderCapability,
    InitializeParams, InitializeResult, InitializedParams, LinkedEditingRangeServerCapabilities,
    OneOf, RenameOptions, SaveOptions, SelectionRangeProviderCapability, SemanticTokenModifier,
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

use super::show_document_translation::ShowDocumentTranslator;
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
                &raw_settings,
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
                folding_range_provider: Some(FoldingRangeProviderCapability::Simple(true)),
                // codeLens/resolve is routed to the origin downstream server
                // via the envelope in lens.data (#355, see
                // bridge/text_document/code_lens.rs).
                code_lens_provider: Some(CodeLensOptions {
                    resolve_provider: Some(true),
                }),
                rename_provider: Some(OneOf::Right(RenameOptions {
                    prepare_provider: Some(true),
                    work_done_progress_options: WorkDoneProgressOptions::default(),
                })),
                document_formatting_provider: Some(OneOf::Left(true)),
                document_range_formatting_provider: Some(OneOf::Left(true)),
                document_on_type_formatting_provider: on_type_formatting_triggers.map(
                    |(first, more)| DocumentOnTypeFormattingOptions {
                        first_trigger_character: first,
                        more_trigger_character: (!more.is_empty()).then_some(more),
                    },
                ),
                inlay_hint_provider: Some(OneOf::Left(true)),
                linked_editing_range_provider: Some(LinkedEditingRangeServerCapabilities::Simple(
                    true,
                )),
                #[cfg(feature = "experimental")]
                color_provider: Some(ColorProviderCapability::Simple(true)),
                #[cfg(not(feature = "experimental"))]
                color_provider: None,
                moniker_provider: Some(OneOf::Left(true)),
                // pull-first-diagnostic-forwarding: Pull-first diagnostic forwarding
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

        // Forward downstream-initiated messages to the upstream editor. The
        // reader tasks feed three channels:
        // - unbounded `upstream_rx` (loss-intolerant): DiagnosticRefresh and
        //   work-done progress (create/$progress/forget).
        // - bounded `window_rx` (best-effort, drop-on-full): window/logMessage,
        //   window/showMessage, and telemetry/event.
        // - unbounded `upstream_request_rx` (loss-intolerant): downstream
        //   requests forwarded with a response relayed back
        //   (window/showMessageRequest, window/showDocument).
        if let Some(upstream_rx) = self.bridge.take_upstream_rx()
            && let Some(window_rx) = self.bridge.take_window_rx()
            && let Some(upstream_request_rx) = self.bridge.take_upstream_request_rx()
        {
            let client = self.client.clone();
            let token = self.shutdown_token.clone();
            // Translates downstream-initiated window/showDocument virtual URIs +
            // selections back to host coordinates before forwarding (#403).
            let show_document_translator = Some(Arc::new(ShowDocumentTranslator::new(
                Arc::clone(&self.documents),
                Arc::clone(&self.language),
                Arc::clone(&self.bridge),
            )));
            let inbound_request_registry = self.bridge.pool().inbound_request_registry();
            // The single proactive diagnostics publisher: region pushes routed up
            // by the reader resolve to a host + region and republish the merged
            // host set (push-propagation-diagnostic-forwarding).
            let diagnostic_publisher = Some(Arc::new(
                crate::lsp::lsp_impl::coordinator::DiagnosticPublisher::new(self),
            ));
            tokio::spawn(upstream_forwarding_loop(
                upstream_rx,
                window_rx,
                upstream_request_rx,
                show_document_translator,
                inbound_request_registry,
                client,
                diagnostic_publisher,
                token,
            ));
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
///   `workspace/diagnostic/refresh` — and the server-declared work-done
///   progress notifications (`CreateWorkDoneProgress`/`Progress`/
///   `ForgetWorkDoneProgress`, window-work-done-progress), which must not be
///   lost or reordered.
/// - `upstream_request_rx` (unbounded): downstream-initiated *requests*
///   (`window/showMessageRequest`, `window/showDocument`) forwarded with the
///   editor's response relayed back; loss-intolerant (a dropped request hangs
///   the downstream). Serviced via [`spawn_upstream_request`] so a slow/human
///   editor never stalls the loop.
/// - `window_rx` (bounded, reader drops on full): `LogMessage`/`ShowMessage` and
///   `telemetry/event` — best-effort notifications, forwarded unconditionally.
///
/// Notification dispatch awaits tower-lsp's internal bounded channel, so a slow
/// editor stalls the loop — but the `biased` select drains the two loss-intolerant
/// channels (`upstream_rx`, then `upstream_request_rx`) before the best-effort
/// `window_rx`, so a `window/*` burst cannot starve `DiagnosticRefresh`, progress,
/// or request forwarding, and the bounded window queue caps memory. FIFO order is
/// preserved within each channel (the window-notification e2e relies on
/// window-channel FIFO; create-before-progress relies on upstream-channel FIFO).
///
/// Exits when:
/// - Either channel is closed (all senders dropped — both senders live in the
///   pool, so they close together at shutdown), OR
/// - The `cancel_token` is cancelled (deterministic shutdown)
// Heterogeneous channels + collaborators threaded into one long-lived loop task;
// bundling them into a struct would just move the list, not shorten it.
#[allow(clippy::too_many_arguments)]
async fn upstream_forwarding_loop(
    mut upstream_rx: tokio::sync::mpsc::UnboundedReceiver<crate::lsp::bridge::UpstreamNotification>,
    mut window_rx: tokio::sync::mpsc::Receiver<crate::lsp::bridge::UpstreamNotification>,
    mut upstream_request_rx: tokio::sync::mpsc::UnboundedReceiver<
        crate::lsp::bridge::UpstreamRequest,
    >,
    show_document_translator: Option<Arc<ShowDocumentTranslator>>,
    inbound_request_registry: crate::lsp::bridge::InboundRequestRegistry,
    client: Client,
    diagnostic_publisher: Option<Arc<crate::lsp::lsp_impl::coordinator::DiagnosticPublisher>>,
    cancel_token: tokio_util::sync::CancellationToken,
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
                    Some(notification) => {
                        deliver_upstream_notification(
                            &client,
                            notification,
                            &mut created_tokens,
                            &mut begun_tokens,
                            diagnostic_publisher.as_deref(),
                        )
                        .await
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
                            show_document_translator.clone(),
                            &client,
                            request,
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
                            diagnostic_publisher.as_deref(),
                        )
                        .await
                    }
                    None => break, // Channel closed
                }
            }
        }
    }
}

/// Service a downstream-initiated request by forwarding it to the editor on a
/// detached task and relaying the editor's answer through the request's `reply`
/// oneshot.
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
/// adversarial/buggy downstream propagates to the editor either way — exactly as
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
    show_document_translator: Option<Arc<ShowDocumentTranslator>>,
    client: &Client,
    request: crate::lsp::bridge::UpstreamRequest,
) {
    use crate::lsp::bridge::UpstreamRequest;
    use tower_lsp_server::ls_types::{
        MessageActionItem, ShowDocumentResult, ShowMessageRequestParams,
    };
    let client = client.clone();
    tokio::spawn(async move {
        match request {
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
                let params = match &show_document_translator {
                    Some(translator) => translator.translate(params).await,
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
    diagnostic_publisher: Option<&crate::lsp::lsp_impl::coordinator::DiagnosticPublisher>,
) {
    use crate::lsp::bridge::UpstreamNotification;
    use tower_lsp_server::ls_types::{ProgressParamsValue, WorkDoneProgress};
    match notification {
        UpstreamNotification::DiagnosticRefresh => {
            if let Err(e) = client.workspace_diagnostic_refresh().await {
                log::debug!(
                    target: "kakehashi::bridge",
                    "workspace/diagnostic/refresh forwarding failed: {}",
                    e
                );
            }
        }
        UpstreamNotification::PublishDiagnostics {
            uri,
            server,
            diagnostics,
        } => {
            // Cache the downstream push and republish the merged host set
            // (push-propagation-diagnostic-forwarding). The publisher classifies the
            // URI (virtual → region, real → `_self` host layer); a `None` publisher
            // (test loop) drops it. (Pushes without a server name were already
            // dropped at the reader, so `server` is always set here.)
            if let Some(publisher) = diagnostic_publisher {
                publisher.publish_push(uri, server, diagnostics).await;
            }
        }
        UpstreamNotification::LogMessage { typ, message } => {
            client.log_message(typ, message).await;
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

    /// A throwaway cancel context for tests that don't exercise cancellation.
    fn test_forwarded_cancel() -> crate::lsp::bridge::ForwardedRequestCancel {
        crate::lsp::bridge::ForwardedRequestCancel {
            connection_id: crate::lsp::bridge::ProgressConnectionId::for_test(0),
            request_id: tower_lsp_server::jsonrpc::Id::Number(1),
            token: tokio_util::sync::CancellationToken::new(),
            generation: 0,
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
        // A later, unrelated request that IS expected to reach the editor.
        tx.send(UpstreamNotification::DiagnosticRefresh).unwrap();

        // First message: the create request — reject it.
        let first = requests.next().await.expect("create request emitted");
        assert_eq!(first.method(), "window/workDoneProgress/create");
        let id = first.id().expect("create request has an id").clone();
        responses
            .send(Response::from_error(id, Error::internal_error()))
            .await
            .unwrap();

        // Next message MUST be the diagnostic refresh, NOT $/progress — the
        // rejected token's progress was dropped.
        let next = requests.next().await.expect("a follow-up request emitted");
        assert_eq!(
            next.method(),
            "workspace/diagnostic/refresh",
            "progress for a rejected token must be dropped; only the later request survives"
        );
        // Respond so the loop's inline (un-timed) refresh await completes.
        let id = next.id().expect("refresh request has an id").clone();
        responses
            .send(Response::from_ok(id, serde_json::Value::Null))
            .await
            .unwrap();

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
        tx.send(UpstreamNotification::DiagnosticRefresh).unwrap();

        // The forgotten token's progress must be dropped; the next editor-bound
        // message is the diagnostic refresh.
        let next = requests.next().await.expect("a follow-up request emitted");
        assert_eq!(
            next.method(),
            "workspace/diagnostic/refresh",
            "progress for a forgotten token must be dropped"
        );
        // Respond so the loop's inline (un-timed) refresh await completes.
        let id = next.id().expect("refresh request has an id").clone();
        responses
            .send(Response::from_ok(id, serde_json::Value::Null))
            .await
            .unwrap();

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
        tx.send(UpstreamNotification::DiagnosticRefresh).unwrap();
        let next = requests.next().await.expect("a follow-up request emitted");
        assert_eq!(
            next.method(),
            "workspace/diagnostic/refresh",
            "an already-ended token must not be ended a second time on purge"
        );
        let id = next.id().expect("refresh request has an id").clone();
        responses
            .send(Response::from_ok(id, serde_json::Value::Null))
            .await
            .unwrap();

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
        tx.send(UpstreamNotification::DiagnosticRefresh).unwrap();
        let next = requests.next().await.expect("a follow-up request emitted");
        assert_eq!(
            next.method(),
            "workspace/diagnostic/refresh",
            "fresh token was ended normally; its purge must synthesize no second End"
        );
        responses
            .send(Response::from_ok(
                next.id().unwrap().clone(),
                serde_json::Value::Null,
            ))
            .await
            .unwrap();

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
