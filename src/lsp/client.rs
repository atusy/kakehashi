//! `ClientNotifier`: thin wrapper over `tower_lsp_server::Client` for logging,
//! progress, and semantic-token refresh requests.
//!
//! Holds `ClientCapabilities` in a `OnceLock` so capability-gated calls (like
//! semantic-token refresh) work safely before `initialize()` — pre-init,
//! capabilities are absent and the call is skipped instead of panicking.

use std::sync::OnceLock;
use std::sync::atomic::Ordering;
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

/// Whether the client advertises `workspace.diagnostics.refreshSupport`
/// (LSP @since 3.17.0): it will honor a `workspace/diagnostic/refresh` by
/// re-pulling. Returns `false` for any missing/null capability in the chain, so
/// we never send a refresh a client would silently ignore — which would leak a
/// tower-lsp pending-request entry plus a parked task (the same hazard the
/// [`check_semantic_tokens_refresh_support`] gate guards against).
pub(crate) fn check_diagnostic_refresh_support(caps: &ClientCapabilities) -> bool {
    caps.workspace
        .as_ref()
        .and_then(|w| w.diagnostics.as_ref())
        .and_then(|d| d.refresh_support)
        .unwrap_or(false)
}

/// Whether a batch of language events asks for a `workspace/semanticTokens/refresh`.
///
/// The refresh request is parameterless and workspace-wide, so every
/// `SemanticTokensRefresh` event in a single batch is interchangeable: the batch
/// either wants a refresh or it doesn't. [`ClientNotifier::log_language_events`]
/// uses this to collapse N refresh events into a single request (#531) — firing N
/// would only force `(N-1)` redundant workspace re-tokenizations and leak that many
/// tower-lsp pending entries on a non-responding client.
///
/// Extracted as a pure function for unit testing (the notifier cannot be built in
/// unit tests). Note: this tests the *trigger* (does the batch want a refresh), not
/// the N→1 *collapse* — the collapse is guaranteed structurally by the single spawn
/// site in `log_language_events`.
/// Single-flight state for `workspace/semanticTokens/refresh` (#531): one
/// in-flight request plus at most one trailing covers any burst of publishes.
/// Process-global because the server serves exactly one client; reset is
/// unnecessary (a stuck `in_flight` on a never-responding client is the same
/// accepted trade-off as the pending-map leak documented at the send site,
/// and the per-keystroke client re-request heals the visible highlight).
struct RefreshFlight {
    in_flight: std::sync::atomic::AtomicBool,
    pending: std::sync::atomic::AtomicBool,
}

static REFRESH_FLIGHT: RefreshFlight = RefreshFlight {
    in_flight: std::sync::atomic::AtomicBool::new(false),
    pending: std::sync::atomic::AtomicBool::new(false),
};

fn batch_requests_semantic_tokens_refresh(events: &[LanguageEvent]) -> bool {
    events
        .iter()
        .any(|event| matches!(event, LanguageEvent::SemanticTokensRefresh { .. }))
}

/// Server→client notifier: logging, progress, semantic-token refresh. `Clone`
/// and thread-safe (the underlying `Client` synchronizes internally, and
/// `client_capabilities` is a shared `OnceLock`).
///
/// Capability-gated calls (e.g. semantic-tokens refresh) inspect the
/// `OnceLock`; before `initialize()` populates it they no-op instead of
/// panicking.
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

    /// Surface a warning as a visible `window/showMessage` popup (not just the
    /// output panel), for messages the user should notice — e.g. a one-time
    /// deprecation notice.
    pub(crate) async fn show_warning(&self, message: impl Into<String>) {
        self.client
            .show_message(MessageType::WARNING, message.into())
            .await;
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
    /// `SemanticTokensRefresh` only fires when the client declared support, since
    /// sending the request unconditionally would violate the LSP capability gate.
    pub(crate) async fn log_language_events(&self, events: &[LanguageEvent]) {
        // Send at most one workspace/semanticTokens/refresh for the whole batch,
        // and spawn it BEFORE the per-event log/show-message awaits below so a
        // slow/blocked client log channel can't delay when the refresh task starts
        // (the refresh is detached, so it then runs concurrently with the logs).
        // The request is parameterless and workspace-wide, so multiple refresh
        // events (e.g. one per derived language on a config reload) collapse into a
        // single request: firing N would only force (N-1) redundant workspace
        // re-tokenizations and leak that many tower-lsp pending entries on a
        // non-responding client. See #531 (twin of #497).
        if batch_requests_semantic_tokens_refresh(events) {
            if self.supports_semantic_tokens_refresh() {
                // Fire-and-forget because the response is just null
                //
                // Keep the receiver alive without dropping by timeout in order to
                // avoid tower-lsp panics (see commit b902e28d)
                //
                // Trade-off: If a client never responds (e.g., vim-lsp), this causes:
                // - A small memory leak in tower-lsp's pending requests map
                // - A spawned task waiting indefinitely
                //
                // Workspace-wide single-flight (#531, the twin of the #497
                // diagnostic-refresh guard): during a typing burst every
                // parse-loop publish requests a refresh, and each un-answered
                // duplicate both forces a redundant client re-tokenization
                // pass and (on a non-responding client) leaks another pending
                // entry. The request is parameterless, so one in-flight
                // refresh plus at most one trailing covers any number of
                // publishes: a publish landing mid-flight sets `pending`, and
                // the flight loops once more after the answer.
                if !REFRESH_FLIGHT.in_flight.swap(true, Ordering::AcqRel) {
                    let client = self.client.clone();
                    tokio::spawn(async move {
                        loop {
                            if let Err(err) = client.semantic_tokens_refresh().await {
                                log::debug!("semantic_tokens_refresh failed: {}", err);
                            }
                            // Claim the trailing slot; loop while publishes
                            // kept arriving during the request.
                            if REFRESH_FLIGHT.pending.swap(false, Ordering::AcqRel) {
                                continue;
                            }
                            REFRESH_FLIGHT.in_flight.store(false, Ordering::Release);
                            // Narrow re-check: a publish that set `pending`
                            // between the swap above and the store may have
                            // found `in_flight` still true and NOT spawned its
                            // own flight — reclaim it here. A publish that
                            // instead won the new `swap(true)` runs its own
                            // flight and this one stops.
                            if REFRESH_FLIGHT.pending.swap(false, Ordering::AcqRel) {
                                if REFRESH_FLIGHT.in_flight.swap(true, Ordering::AcqRel) {
                                    break;
                                }
                                continue;
                            }
                            break;
                        }
                    });
                } else {
                    REFRESH_FLIGHT.pending.store(true, Ordering::Release);
                    log::debug!("semantic_tokens_refresh coalesced into the in-flight request");
                }
            } else {
                // Only send refresh if client supports it (LSP @since 3.16.0 compliance).
                log::debug!(
                    "Skipping semantic_tokens_refresh - client does not support it (batch of {} event(s))",
                    events.len()
                );
            }
        }

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
                // Refresh requests are handled once, before the loop — see above.
                LanguageEvent::SemanticTokensRefresh { .. } => {}
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
    /// The progress token is `kakehashi/install/{language}`.
    pub(crate) async fn progress_begin(&self, language: &str) {
        self.client
            .send_notification::<Progress>(create_progress_begin(language))
            .await;
    }

    /// Send a progress end notification for parser installation, reporting
    /// success or failure in the message.
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
        DiagnosticWorkspaceClientCapabilities, SemanticTokensWorkspaceClientCapabilities,
        WorkspaceClientCapabilities,
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

    /// Tests for `check_diagnostic_refresh_support` — same shape as the
    /// semantic-tokens gate, but against `workspace.diagnostics.refreshSupport`.
    /// This is the gate `request_pull_diagnostic_refresh` uses, so a missing
    /// capability anywhere in the chain must read as `false` (don't send a refresh
    /// the client would silently ignore).
    #[rstest]
    #[case::refresh_support_true(true, true, Some(true), true)]
    #[case::refresh_support_false(true, true, Some(false), false)]
    #[case::refresh_support_none(true, true, None, false)]
    #[case::diagnostics_none(true, false, None, false)]
    #[case::workspace_none(false, false, None, false)]
    fn test_check_diagnostic_refresh_support(
        #[case] workspace: bool,
        #[case] diagnostics: bool,
        #[case] refresh_support: Option<bool>,
        #[case] expected: bool,
    ) {
        let caps = ClientCapabilities {
            workspace: if workspace {
                Some(WorkspaceClientCapabilities {
                    diagnostics: if diagnostics {
                        Some(DiagnosticWorkspaceClientCapabilities { refresh_support })
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
        assert_eq!(check_diagnostic_refresh_support(&caps), expected);
    }

    /// Tests for `batch_requests_semantic_tokens_refresh` (#531).
    ///
    /// This tests the *trigger* — whether a batch wants a refresh at all. The N→1
    /// collapse itself is guaranteed structurally (a single spawn site after the
    /// loop in `log_language_events`), not by this predicate.
    #[test]
    fn batch_requests_refresh_when_any_refresh_event_present() {
        let events = vec![
            LanguageEvent::log(LanguageLogLevel::Info, "loaded a"),
            LanguageEvent::semantic_tokens_refresh("python"),
            LanguageEvent::log(LanguageLogLevel::Info, "loaded b"),
            LanguageEvent::semantic_tokens_refresh("lua"),
        ];
        assert!(batch_requests_semantic_tokens_refresh(&events));
    }

    #[test]
    fn batch_requests_refresh_for_single_refresh_event() {
        let events = vec![LanguageEvent::semantic_tokens_refresh("python")];
        assert!(batch_requests_semantic_tokens_refresh(&events));
    }

    #[test]
    fn batch_does_not_request_refresh_without_refresh_event() {
        let events = vec![
            LanguageEvent::log(LanguageLogLevel::Info, "info"),
            LanguageEvent::show_message(LanguageLogLevel::Warning, "warn"),
        ];
        assert!(!batch_requests_semantic_tokens_refresh(&events));
    }

    #[test]
    fn batch_does_not_request_refresh_when_empty() {
        assert!(!batch_requests_semantic_tokens_refresh(&[]));
    }
}
