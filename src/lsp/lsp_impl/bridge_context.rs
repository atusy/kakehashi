//! Shared preamble for bridge endpoint implementations.
//!
//! All bridge endpoints (definition, type_definition, implementation, declaration,
//! references, hover, completion, color_presentation, etc.) follow the same pattern
//! of resolving injection context before sending requests. This module extracts
//! that shared preamble into a reusable method: `resolve_bridge_contexts`.

use tower_lsp_server::jsonrpc::Id;
use tower_lsp_server::ls_types::{MessageType, Position, Range, Uri};
use url::Url;

use crate::config::settings::AggregationStrategy;
use crate::language::injection::ResolvedInjection;
use crate::lsp::bridge::{ResolvedServerConfig, UpstreamId};
use crate::lsp::get_current_request_id;
use crate::lsp::request_id::{CancelReceiver, CancelSubscriptionGuard};
use crate::text::PositionMapper;

use super::{Kakehashi, uri_to_url};

/// Extract the upstream request ID from task-local storage.
///
/// Converts the tower-lsp `Id` (set by RequestIdCapture middleware) into
/// our domain `UpstreamId`. Returns `None` for null or missing IDs.
pub(crate) fn current_upstream_id() -> Option<UpstreamId> {
    match get_current_request_id() {
        Some(Id::Number(n)) => Some(UpstreamId::Number(n)),
        Some(Id::String(s)) => Some(UpstreamId::String(s)),
        None | Some(Id::Null) => None,
    }
}

/// All resolved context needed to send bridge requests to multiple servers.
///
/// Produced by `Kakehashi::resolve_bridge_contexts`. Returns ALL matching server
/// configs for the injection language, enabling fan-out to multiple downstream
/// servers via [`fan_out()`](crate::lsp::aggregation::server::fan_out::fan_out).
///
/// This is the document-level context (no position). Position-based handlers
/// use [`PositionRequestContext`] which wraps this struct.
pub(crate) struct DocumentRequestContext {
    /// The parsed document URL (url::Url).
    pub(crate) uri: Url,
    /// The resolved injection region with virtual content and region metadata.
    pub(crate) resolved: ResolvedInjection,
    /// All matching bridge server configs for this injection language.
    pub(crate) configs: Vec<ResolvedServerConfig>,
    /// The upstream JSON-RPC request ID for cancel forwarding.
    pub(crate) upstream_request_id: Option<UpstreamId>,
    /// Server names in priority order for aggregation.
    /// Resolved from the bridge language config's aggregation settings.
    /// Empty means pure first-win behavior (no priority ordering).
    pub(crate) priorities: Vec<String>,
    /// Aggregation strategy for this region.
    /// Resolved from the bridge language config's aggregation settings.
    pub(crate) strategy: AggregationStrategy,
}

/// Document context plus a cursor position.
///
/// Used by position-based handlers (definition, hover, completion, etc.).
pub(crate) struct PositionRequestContext {
    /// The document-level context.
    pub(crate) document: DocumentRequestContext,
    /// The cursor position within the document.
    pub(crate) position: Position,
}

/// Document context plus a range.
///
/// Used by range-based handlers (inlay_hint, color_presentation).
pub(crate) struct RangeRequestContext {
    /// The document-level context.
    pub(crate) document: DocumentRequestContext,
    /// The range within the document.
    pub(crate) range: Range,
}

/// Intermediate result from the shared preamble, before server config lookup.
struct PreambleResult {
    uri: Url,
    position: Position,
    resolved: ResolvedInjection,
    language_name: String,
    upstream_request_id: Option<UpstreamId>,
}

impl Kakehashi {
    /// Subscribe to cancel notifications for an upstream request.
    ///
    /// Returns `(Some(receiver), Some(guard))` on success, or `(None, None)` if
    /// subscription fails (e.g., duplicate subscription). The guard ensures
    /// automatic unsubscribe on drop.
    ///
    /// The tuple return type allows callers to destructure directly:
    /// ```ignore
    /// let (cancel_rx, _cancel_guard) = self.subscribe_cancel(ctx.upstream_request_id.as_ref());
    /// ```
    pub(crate) fn subscribe_cancel(
        &self,
        upstream_id: Option<&UpstreamId>,
    ) -> (Option<CancelReceiver>, Option<CancelSubscriptionGuard<'_>>) {
        let Some(upstream_id) = upstream_id else {
            return (None, None);
        };
        match self
            .bridge
            .cancel_forwarder()
            .subscribe(upstream_id.clone())
        {
            Ok(rx) => {
                let guard = CancelSubscriptionGuard::new(
                    self.bridge.cancel_forwarder(),
                    upstream_id.clone(),
                );
                (Some(rx), Some(guard))
            }
            Err(e) => {
                log::error!(
                    "Cancel subscribe failed for {}: already subscribed. \
                     Proceeding without cancel.",
                    e.0
                );
                (None, None)
            }
        }
    }

    /// Shared preamble for bridge endpoint resolution.
    ///
    /// Handles all common steps before server config lookup:
    /// 1. Converts URI from ls_types to url::Url
    /// 2. Logs the method invocation
    /// 3. Gets document snapshot
    /// 4. Detects document language
    /// 5. Gets injection query
    /// 6. Resolves injection region at position
    /// 7. Extracts upstream request ID from task-local storage
    ///
    /// Returns `None` for any early-exit condition (invalid URI, no document,
    /// no language, no injection at position).
    async fn resolve_bridge_preamble(
        &self,
        lsp_uri: &Uri,
        position: Position,
        method_name: &str,
    ) -> Option<PreambleResult> {
        // Convert ls_types::Uri to url::Url for internal use
        let Ok(uri) = uri_to_url(lsp_uri) else {
            log::warn!("Invalid URI in {}: {}", method_name, lsp_uri.as_str());
            return None;
        };

        self.client
            .log_message(
                MessageType::INFO,
                format!(
                    "{} called for {} at line {} col {}",
                    method_name, uri, position.line, position.character
                ),
            )
            .await;

        // Get document snapshot (minimizes lock duration)
        let snapshot = match self.documents.get(&uri) {
            None => {
                self.client
                    .log_message(MessageType::INFO, "No document found")
                    .await;
                return None;
            }
            Some(doc) => match doc.snapshot() {
                None => {
                    self.client
                        .log_message(MessageType::INFO, "Document not fully initialized")
                        .await;
                    return None;
                }
                Some(snapshot) => snapshot,
            },
            // doc automatically dropped here, lock released
        };

        // Get the language for this document
        let Some(language_name) = self.get_language_for_document(&uri) else {
            log::debug!("kakehashi::{}: No language detected", method_name);
            return None;
        };

        // Get injection query to detect injection regions
        let injection_query = self.language.get_injection_query(&language_name)?;

        // Resolve injection region at position
        let mapper = PositionMapper::new(snapshot.text());
        let byte_offset = mapper.position_to_byte(position)?;

        let Some(resolved) = crate::language::InjectionResolver::resolve_at_byte_offset(
            &self.language,
            self.bridge.region_id_tracker(),
            &uri,
            snapshot.tree(),
            snapshot.text(),
            injection_query.as_ref(),
            byte_offset,
        ) else {
            // Not in an injection region - return None
            return None;
        };

        // Get upstream request ID from task-local storage (set by RequestIdCapture middleware)
        let upstream_request_id = current_upstream_id();

        Some(PreambleResult {
            uri,
            position,
            resolved,
            language_name,
            upstream_request_id,
        })
    }

    /// Convert a preamble result into a `DocumentRequestContext` by looking up
    /// bridge server configs for the injection language.
    ///
    /// `method_name` is the LSP method (e.g., `"textDocument/definition"`) used
    /// to resolve per-method aggregation priorities from the bridge language config.
    ///
    /// Returns `None` if no configs are found.
    async fn preamble_to_document_context(
        &self,
        preamble: PreambleResult,
        method_name: &str,
    ) -> Option<DocumentRequestContext> {
        let configs = self.get_all_bridge_configs_for_language(
            &preamble.language_name,
            &preamble.resolved.injection_language,
        );

        if configs.is_empty() {
            self.client
                .log_message(
                    MessageType::INFO,
                    format!(
                        "No bridge server configured for language: {} (host: {})",
                        preamble.resolved.injection_language, preamble.language_name
                    ),
                )
                .await;
            return None;
        }

        // Resolve aggregation priorities from the bridge language config.
        let priorities = self.resolve_aggregation_priorities(
            &preamble.language_name,
            &preamble.resolved.injection_language,
            method_name,
        );

        Some(DocumentRequestContext {
            uri: preamble.uri,
            resolved: preamble.resolved,
            configs,
            upstream_request_id: preamble.upstream_request_id,
            priorities,
            strategy: AggregationStrategy::Preferred,
        })
    }

    /// Resolve aggregation strategy for a given host language, injection language,
    /// and LSP method.
    ///
    /// Resolution chain:
    /// 1. `resolve_language_settings_with_wildcard(languages, host_lang)` → host settings
    /// 2. `resolve_bridge_language_with_wildcard(bridge_map, injection_lang)` → bridge config
    /// 3. `bridge_config.resolve_strategy(method, default)` → strategy
    pub(crate) fn resolve_aggregation_strategy(
        &self,
        host_language: &str,
        injection_language: &str,
        method_name: &str,
        default: crate::config::settings::AggregationStrategy,
    ) -> crate::config::settings::AggregationStrategy {
        let settings = self.settings_manager.load_settings();
        crate::config::resolve_language_settings_with_wildcard(&settings.languages, host_language)
            .and_then(|lang_settings| lang_settings.bridge)
            .and_then(|bridge_map| {
                crate::config::resolve_bridge_language_with_wildcard(
                    &bridge_map,
                    injection_language,
                )
            })
            .map(|bridge_config| bridge_config.resolve_strategy(method_name, default))
            .unwrap_or(default)
    }

    /// Resolve aggregation priorities for a given host language, injection language,
    /// and LSP method.
    ///
    /// Resolution chain:
    /// 1. `resolve_language_settings_with_wildcard(languages, host_lang)` → host settings
    /// 2. `resolve_bridge_language_with_wildcard(bridge_map, injection_lang)` → bridge config
    /// 3. `bridge_config.resolve_priorities(method)` → priority list
    pub(crate) fn resolve_aggregation_priorities(
        &self,
        host_language: &str,
        injection_language: &str,
        method_name: &str,
    ) -> Vec<String> {
        let settings = self.settings_manager.load_settings();
        crate::config::resolve_language_settings_with_wildcard(&settings.languages, host_language)
            .and_then(|lang_settings| lang_settings.bridge)
            .and_then(|bridge_map| {
                crate::config::resolve_bridge_language_with_wildcard(
                    &bridge_map,
                    injection_language,
                )
            })
            .map(|bridge_config| bridge_config.resolve_priorities(method_name))
            .unwrap_or_default()
    }

    /// Resolve injection context for a bridge endpoint request (all matching servers).
    ///
    /// Delegates to the shared preamble, then looks up ALL bridge server configs
    /// for the injection language. Returns `None` if no configs found.
    pub(crate) async fn resolve_bridge_contexts(
        &self,
        lsp_uri: &Uri,
        position: Position,
        method_name: &str,
    ) -> Option<PositionRequestContext> {
        let preamble = self
            .resolve_bridge_preamble(lsp_uri, position, method_name)
            .await?;
        let position = preamble.position;
        let document = self
            .preamble_to_document_context(preamble, method_name)
            .await?;

        Some(PositionRequestContext { document, position })
    }

    /// Resolve injection context for a range-based bridge endpoint request.
    ///
    /// Uses `range.start` to find the injection region, then returns a
    /// [`RangeRequestContext`] with the full range for the handler to use.
    pub(crate) async fn resolve_bridge_contexts_for_range(
        &self,
        lsp_uri: &Uri,
        range: Range,
        method_name: &str,
    ) -> Option<RangeRequestContext> {
        let preamble = self
            .resolve_bridge_preamble(lsp_uri, range.start, method_name)
            .await?;
        let document = self
            .preamble_to_document_context(preamble, method_name)
            .await?;

        Some(RangeRequestContext { document, range })
    }
}
