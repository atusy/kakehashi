//! Client notification abstraction for LSP communication.
//!
//! This module provides `ClientNotifier`, a wrapper around `tower_lsp_server::Client`
//! that centralizes all client-facing communication: logging, progress notifications,
//! and semantic token refresh requests.
//!
//! # Design Rationale
//!
//! Extracting client communication into a dedicated struct provides:
//! - **Single Responsibility**: All client notification logic in one place
//! - **Testability**: Methods can be unit tested without full LSP setup
//! - **Consistency**: Uniform logging and notification patterns across the codebase
//!
//! # Initialization Lifecycle
//!
//! The `ClientNotifier` wraps the LSP `Client` and a reference to `ClientCapabilities`
//! stored in an `OnceLock`. This allows capability-dependent behavior (like semantic
//! tokens refresh) to work correctly both before and after `initialize()`:
//! - Before `initialize()`: Capabilities are not set, so refresh is skipped (safe default)
//! - After `initialize()`: Capabilities are checked to determine if refresh is supported

use std::sync::OnceLock;
use tower_lsp_server::Client;
use tower_lsp_server::ls_types::notification::Progress;
use tower_lsp_server::ls_types::{ClientCapabilities, MessageType};

use crate::language::{LanguageEvent, LanguageLogLevel};
use crate::lsp::{SettingsEvent, SettingsEventKind};

use super::progress::{create_progress_begin, create_progress_end};

/// Check if client capabilities indicate semantic tokens refresh support.
///
/// Extracted as a pure function for unit testability - the Kakehashi struct
/// cannot be constructed in unit tests due to tower_lsp_server::Client dependency.
///
/// Returns `false` for any missing/null capability in the chain (defensive
/// default per LSP spec @since 3.16.0).
pub(crate) fn check_semantic_tokens_refresh_support(caps: &ClientCapabilities) -> bool {
    caps.workspace
        .as_ref()
        .and_then(|w| w.semantic_tokens.as_ref())
        .and_then(|st| st.refresh_support)
        .unwrap_or(false)
}

/// Wrapper around LSP client for centralized notification handling.
///
/// `ClientNotifier` encapsulates all communication from server to client,
/// providing a clean API for:
/// - Log messages at various severity levels
/// - Progress notifications for long-running operations
/// - Semantic token refresh requests
///
/// # Thread Safety
///
/// `ClientNotifier` is `Clone` and thread-safe. The underlying `Client`
/// uses internal synchronization, and capabilities are read from a shared
/// `OnceLock` reference.
///
/// # Capability Checking
///
/// Capability-dependent operations (like semantic tokens refresh) check the
/// `client_capabilities` OnceLock. Before `initialize()` sets capabilities,
/// these operations safely default to no-op behavior.
#[derive(Clone)]
pub(crate) struct ClientNotifier<'a> {
    client: Client,
    /// Reference to capabilities stored in Kakehashi.
    /// Uses OnceLock to handle pre/post initialization states.
    client_capabilities: &'a OnceLock<ClientCapabilities>,
}

impl std::fmt::Debug for ClientNotifier<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientNotifier")
            .field("client", &self.client)
            .field("client_capabilities", &"&OnceLock<ClientCapabilities>")
            .finish()
    }
}

impl<'a> ClientNotifier<'a> {
    /// Create a new `ClientNotifier` wrapping the given LSP client.
    ///
    /// # Arguments
    /// * `client` - The tower-lsp Client for server-to-client communication
    /// * `client_capabilities` - Reference to the OnceLock storing client capabilities
    pub(crate) fn new(
        client: Client,
        client_capabilities: &'a OnceLock<ClientCapabilities>,
    ) -> Self {
        Self {
            client,
            client_capabilities,
        }
    }

    /// Log a message to the client at the specified severity level.
    pub(crate) async fn log(&self, level: MessageType, message: impl Into<String>) {
        self.client.log_message(level, message.into()).await;
    }

    /// Log an informational message.
    pub(crate) async fn log_info(&self, message: impl Into<String>) {
        self.log(MessageType::INFO, message).await;
    }

    /// Log a warning message.
    pub(crate) async fn log_warning(&self, message: impl Into<String>) {
        self.log(MessageType::WARNING, message).await;
    }

    /// Log a debug/trace message (LOG level in LSP).
    pub(crate) async fn log_trace(&self, message: impl Into<String>) {
        self.log(MessageType::LOG, message).await;
    }

    /// Returns true only if client declared workspace.semanticTokens.refreshSupport.
    /// Returns false if initialize() hasn't been called yet (OnceLock is empty).
    pub(crate) fn supports_semantic_tokens_refresh(&self) -> bool {
        self.client_capabilities
            .get()
            .map(check_semantic_tokens_refresh_support)
            .unwrap_or(false)
    }

    /// Handle language events by logging messages and triggering refresh requests.
    ///
    /// This processes events emitted by `LanguageCoordinator` operations:
    /// - `Log` events are sent as log messages to the client
    /// - `SemanticTokensRefresh` events trigger workspace/semanticTokens/refresh
    ///   (only if client declared support via capabilities)
    pub(crate) async fn log_language_events(&self, events: &[LanguageEvent]) {
        for event in events {
            match event {
                LanguageEvent::Log { level, message } => {
                    let message_type = match level {
                        LanguageLogLevel::Error => MessageType::ERROR,
                        LanguageLogLevel::Warning => MessageType::WARNING,
                        LanguageLogLevel::Info => MessageType::INFO,
                    };
                    self.client.log_message(message_type, message.clone()).await;
                }
                LanguageEvent::ShowMessage { level, message } => {
                    let message_type = match level {
                        LanguageLogLevel::Error => MessageType::ERROR,
                        LanguageLogLevel::Warning => MessageType::WARNING,
                        LanguageLogLevel::Info => MessageType::INFO,
                    };
                    self.client
                        .show_message(message_type, message.clone())
                        .await;
                }
                LanguageEvent::SemanticTokensRefresh { language_id } => {
                    // Only send refresh if client supports it (LSP @since 3.16.0 compliance).
                    // Check MUST be before tokio::spawn - can't `continue` from async block.
                    if !self.supports_semantic_tokens_refresh() {
                        log::debug!(
                            "Skipping semantic_tokens_refresh for {} - client does not support it",
                            language_id
                        );
                        continue;
                    }

                    // Fire-and-forget because the response is just null
                    //
                    // Keep the receiver alive without dropping by timeout in order to
                    // avoid tower-lsp panics (see commit b902e28d)
                    //
                    // Trade-off: If a client never responds (e.g., vim-lsp), this causes:
                    // - A small memory leak in tower-lsp's pending requests map
                    // - A spawned task waiting indefinitely
                    let client = self.client.clone();
                    let lang_id = language_id.clone();
                    tokio::spawn(async move {
                        if let Err(err) = client.semantic_tokens_refresh().await {
                            log::debug!("semantic_tokens_refresh failed for {}: {}", lang_id, err);
                        }
                    });
                }
            }
        }
    }

    /// Handle settings events by logging messages at appropriate levels.
    ///
    /// Settings events are emitted during configuration loading and include
    /// informational messages and warnings about configuration issues.
    /// Error events use `window/showMessage` so they appear as visible
    /// notifications in the editor, not just in the output panel.
    pub(crate) async fn log_settings_events(&self, events: &[SettingsEvent]) {
        for event in events {
            match event.kind {
                SettingsEventKind::Error => {
                    self.client
                        .show_message(MessageType::ERROR, event.message.clone())
                        .await;
                }
                SettingsEventKind::Warning => {
                    self.client
                        .log_message(MessageType::WARNING, event.message.clone())
                        .await;
                }
                SettingsEventKind::Info => {
                    self.client
                        .log_message(MessageType::INFO, event.message.clone())
                        .await;
                }
            }
        }
    }

    /// Send a progress begin notification for parser installation.
    ///
    /// Creates a `$/progress` notification with `WorkDoneProgressBegin` payload.
    /// The progress token is `kakehashi/install/{language}`.
    pub(crate) async fn progress_begin(&self, language: &str) {
        self.client
            .send_notification::<Progress>(create_progress_begin(language))
            .await;
    }

    /// Send a progress end notification for parser installation.
    ///
    /// Creates a `$/progress` notification with `WorkDoneProgressEnd` payload.
    /// The message indicates success or failure.
    pub(crate) async fn progress_end(&self, language: &str, success: bool) {
        self.client
            .send_notification::<Progress>(create_progress_end(language, success))
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;
    use tower_lsp_server::ls_types::{
        SemanticTokensWorkspaceClientCapabilities, WorkspaceClientCapabilities,
    };

    /// Tests for check_semantic_tokens_refresh_support pure function.
    /// These test the capability checking logic without needing to construct ClientNotifier.
    ///
    /// Arguments control what's present at each level:
    /// - `workspace`: Whether workspace field is Some
    /// - `semantic_tokens`: Whether semantic_tokens is Some (requires workspace)
    /// - `refresh_support`: The actual refresh_support value (requires semantic_tokens)
    /// - `expected`: The expected result of refresh support
    #[rstest]
    #[case::refresh_support_true(true, true, Some(true), true)]
    #[case::refresh_support_false(true, true, Some(false), false)]
    #[case::refresh_support_none(true, true, None, false)]
    #[case::semantic_tokens_none(true, false, None, false)]
    #[case::workspace_empty(true, false, None, false)]
    #[case::workspace_none(false, false, None, false)]
    fn test_check_refresh_support(
        #[case] workspace: bool,
        #[case] semantic_tokens: bool,
        #[case] refresh_support: Option<bool>,
        #[case] expected: bool,
    ) {
        let caps = ClientCapabilities {
            workspace: if workspace {
                Some(WorkspaceClientCapabilities {
                    semantic_tokens: if semantic_tokens {
                        Some(SemanticTokensWorkspaceClientCapabilities { refresh_support })
                    } else {
                        None
                    },
                    ..Default::default()
                })
            } else {
                None
            },
            ..Default::default()
        };
        assert_eq!(check_semantic_tokens_refresh_support(&caps), expected);
    }

    #[test]
    fn test_check_refresh_support_when_capabilities_empty() {
        // Completely empty capabilities (pre-init state)
        // Keep as separate test to explicitly document Default behavior
        let caps = ClientCapabilities::default();
        assert!(!check_semantic_tokens_refresh_support(&caps));
    }
}
