//! Shared preamble for bridge endpoint implementations.
//!
//! All bridge endpoints (definition, type_definition, implementation, declaration,
//! references, hover, completion, color_presentation, etc.) follow the same pattern
//! of resolving injection context before sending requests. This module extracts
//! that shared preamble into a reusable method: `resolve_bridge_contexts`.

use tower_lsp_server::ls_types::{Position, Range, Uri};
use url::Url;

use crate::config::WorkspaceSettings;
use crate::config::settings::{
    AggregationStrategy, BridgeLanguageConfig, LayerSource, ResolvedAggregationConfig,
    ResolvedLayerConfig,
};
use crate::language::injection::ResolvedInjection;
use crate::lsp::bridge::{ResolvedServerConfig, UpstreamId};
use crate::lsp::request_id::{CancelReceiver, CancelSubscriptionGuard, current_upstream_id};
use crate::text::PositionMapper;

use super::{Kakehashi, uri_to_url};

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
    /// Server names in priority order for aggregation — an ordered allowlist
    /// (aggregation-priorities-wildcard). `"*"` stands for the unlisted rest
    /// (first-win group); the resolved default is `["*"]` (all servers,
    /// first-win). Empty means fan-out is disabled for this method.
    pub(crate) priorities: Vec<String>,
    /// Aggregation strategy for this region.
    ///
    /// Resolved from the bridge language config's aggregation settings via
    /// `Kakehashi::resolve_aggregation_config`, defaulting to
    /// `AggregationStrategy::Preferred` for position-based and range-based handlers.
    pub(crate) strategy: AggregationStrategy,
    /// Maximum number of servers to fan out to.
    /// `None` = no limit, `Some(0)` = disable fan-out.
    pub(crate) max_fan_out: Option<usize>,
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
    resolved: ResolvedInjection,
    language_name: String,
    upstream_request_id: Option<UpstreamId>,
}

fn resolve_bridge_language_config_from_settings(
    settings: &WorkspaceSettings,
    host_language: &str,
    injection_language: &str,
) -> Option<BridgeLanguageConfig> {
    // Phase 2 (resolve_base_configs) generally resolves inherited settings, so
    // most configured languages already have "_" merged into them. However,
    // self-referential roots and detected cycles terminate before wildcard
    // defaults are applied. This helper only falls back to "_" when the host
    // language entry itself is missing, so blank-slate roots keep blocking
    // wildcard bridge inheritance here. Auto-discovered languages (not in
    // config) fall back to "_" so they still inherit bridge/aggregation
    // settings.
    settings
        .resolve_host_language_settings(host_language)
        .and_then(|lang_settings| lang_settings.bridge.as_ref())
        .and_then(|bridge_map| {
            crate::config::resolve_with_wildcard(
                bridge_map,
                injection_language,
                crate::config::merge_bridge_language_configs,
            )
        })
}

/// Resolve the cross-layer config (cross-layer-aggregation) for a host
/// language and method. Falls back to the built-in defaults — order
/// `[virt, host, native]`, per-method strategy — when the host language has
/// no settings entry at all.
pub(crate) fn resolve_layer_config_from_settings(
    settings: &WorkspaceSettings,
    host_language: &str,
    method_name: &str,
) -> ResolvedLayerConfig {
    settings
        .resolve_host_language_settings(host_language)
        .map(|lang_settings| lang_settings.resolve_layers(method_name))
        .unwrap_or_else(|| ResolvedLayerConfig::with_defaults(method_name))
}

pub(crate) fn resolve_aggregation_config_from_settings(
    settings: &WorkspaceSettings,
    host_language: &str,
    injection_language: &str,
    method_name: &str,
) -> ResolvedAggregationConfig {
    resolve_bridge_language_config_from_settings(settings, host_language, injection_language)
        .map(|bridge_config| bridge_config.resolve_aggregation(method_name))
        // Intentionally use a stable hard-coded fallback when no bridge config
        // resolves at all. The wildcard "_" strategy may evolve in the future,
        // but this path should remain predictable even if wildcard defaults do.
        .unwrap_or_else(ResolvedAggregationConfig::with_defaults)
}

/// Find every (host_language, injection_language) pair whose configured
/// aggregation for `textDocument/formatting` is the **misconfigured**
/// `Concatenated`-without-explicit-`priorities` combination.
///
/// Since the concatenated formatting pipeline landed
/// (concatenated-formatting-pipeline), `strategy = "concatenated"` with
/// explicit server names in `priorities` is a valid configuration that runs
/// the sequential pipeline. A `priorities` carrying no explicit name — only
/// the `"*"` wildcard, e.g. the resolved default for an absent list — is a
/// misconfiguration (the pipeline's order would be undefined, so the region
/// falls back to `preferred`; ADR Decision point 2). An explicit `[]` is the
/// deliberate per-method kill switch (aggregation-priorities-wildcard) and is
/// NOT warned about. We warn the user once at settings-apply time
/// (initialize + didChangeConfiguration) so the mistake surfaces immediately
/// rather than silently degrading every format request.
///
/// Each bridge entry is resolved through
/// [`BridgeLanguageConfig::resolve_aggregation`] — the same field-level
/// wildcard merge the runtime uses — so priorities supplied via the `_`
/// wildcard entry count as configured.
pub(crate) fn concatenated_formatting_pairs(settings: &WorkspaceSettings) -> Vec<(String, String)> {
    use crate::config::settings::{AggregationStrategy, PRIORITIES_WILDCARD};

    let mut pairs = Vec::new();
    for (host_language, lang_settings) in &settings.languages {
        let Some(bridge_map) = lang_settings.bridge.as_ref() else {
            continue;
        };
        for injection_language in bridge_map.keys() {
            // Resolve through the bridge-key wildcard merge so this warning
            // path sees exactly what the runtime sees: priorities supplied by
            // a `bridge._` entry must count for a `bridge.<lang>` that only
            // sets the strategy. (For the literal `_` key this self-merges,
            // which is idempotent for the field-level merge.)
            let Some(bridge_cfg) = crate::config::resolve_with_wildcard(
                bridge_map,
                injection_language,
                crate::config::merge_bridge_language_configs,
            ) else {
                continue;
            };
            let agg = bridge_cfg.resolve_aggregation("textDocument/formatting");
            let has_explicit_name = agg
                .priorities
                .iter()
                .any(|name| name != PRIORITIES_WILDCARD);
            if agg.strategy == AggregationStrategy::Concatenated
                && !agg.priorities.is_empty()
                && !has_explicit_name
            {
                pairs.push((host_language.clone(), injection_language.clone()));
            }
        }
    }
    pairs.sort();
    pairs
}

/// Render the single aggregated client-facing warning for misconfigured
/// `textDocument/formatting` aggregation strategies.
///
/// The previous implementation emitted one `window/logMessage` per
/// (host, injection) pair — N notifications per initialize AND every
/// `didChangeConfiguration` — which floods the editor log for workspaces
/// that configure many bridge entries.
///
/// Returns `None` when `pairs` is empty (nothing to warn about); callers
/// should skip emitting in that case. Pairs are listed in their incoming
/// (already sorted) order so the message is deterministic across runs.
pub(crate) fn format_concatenated_formatting_warning(pairs: &[(String, String)]) -> Option<String> {
    if pairs.is_empty() {
        return None;
    }
    let listed = pairs
        .iter()
        .map(|(host, injection)| {
            // A `_` injection key is the wildcard template: it is not one
            // request path but the default every injection language without
            // its own override inherits — render it so the warning does not
            // read as a literal language named "_".
            if injection == crate::config::WILDCARD_KEY {
                format!("{}->(any other injection)", host)
            } else {
                format!("{}->{}", host, injection)
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    Some(format!(
        "Bridge config sets aggregation strategy 'concatenated' for \
         textDocument/formatting with no explicit server names in \
         'priorities' on {} (host->injection) pair(s): {}. The concatenated \
         formatting pipeline requires explicitly named servers ('priorities' \
         defines which servers run and in what order; '*' has no \
         deterministic order and is ignored); these pairs fall back to \
         'preferred'.",
        pairs.len(),
        listed
    ))
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
    fn resolve_bridge_preamble(
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

        log::debug!(
            "{} called for {} at line {} col {}",
            method_name,
            uri,
            position.line,
            position.character
        );

        // Get document snapshot (minimizes lock duration)
        let snapshot = match self.documents.get(&uri) {
            None => {
                log::debug!("{}: No document found for {}", method_name, uri);
                return None;
            }
            Some(doc) => match doc.snapshot() {
                None => {
                    log::debug!(
                        "{}: Document not fully initialized for {}",
                        method_name,
                        uri
                    );
                    return None;
                }
                Some(snapshot) => snapshot,
            },
            // doc automatically dropped here, lock released
        };

        // Get the language for this document
        let Some(language_name) = self.document_language(&uri) else {
            log::debug!("kakehashi::{}: No language detected", method_name);
            return None;
        };

        // Get injection query to detect injection regions
        let injection_query = self.language.injection_query(&language_name)?;

        // Resolve injection region at position
        let mapper = PositionMapper::new(snapshot.text());
        let byte_offset = mapper.position_to_byte(position)?;

        let Some(resolved) = crate::language::InjectionResolver::resolve_at_byte_offset(
            &self.language,
            self.bridge.node_tracker(),
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
            resolved,
            language_name,
            upstream_request_id,
        })
    }

    /// Resolve the cross-layer config (cross-layer-aggregation) for a host
    /// language and LSP method from the current settings.
    pub(crate) fn resolve_layer_config(
        &self,
        host_language: &str,
        method_name: &str,
    ) -> ResolvedLayerConfig {
        let settings = self.settings_manager.load_settings();
        resolve_layer_config_from_settings(&settings, host_language, method_name)
    }

    /// Whether the virt layer participates for this host language and LSP
    /// method (cross-layer-aggregation): when `"virt"` is absent from the
    /// resolved `layers.order`, the bridge dispatch is skipped entirely.
    ///
    /// This gate is the whole of the stage-2 `preferred` walk for today's
    /// contributor set — host (`bridge._self`) is unimplemented and the
    /// bridged methods have no native contributor — so "first non-empty
    /// layer in order" degenerates to "virt if allowed, else nothing".
    /// Formatting consumes the full config instead (it dispatches on the
    /// layer strategy too — see `combine_layer_formatting_results`).
    pub(crate) fn virt_layer_enabled(&self, host_language: &str, method_name: &str) -> bool {
        self.resolve_layer_config(host_language, method_name)
            .allows(LayerSource::Virt)
    }

    /// Convert a preamble result into a `DocumentRequestContext` by looking up
    /// bridge server configs for the injection language.
    ///
    /// `method_name` is the LSP method (e.g., `"textDocument/definition"`) used
    /// to resolve per-method aggregation priorities from the bridge language config.
    ///
    /// Returns `None` if no configs are found.
    fn preamble_to_document_context(
        &self,
        preamble: PreambleResult,
        method_name: &str,
    ) -> Option<DocumentRequestContext> {
        if !self.virt_layer_enabled(&preamble.language_name, method_name) {
            log::debug!(
                "{}: virt layer disabled for {} via layers.order",
                method_name,
                preamble.language_name
            );
            return None;
        }

        let configs = self.bridge_configs_for_injection_language(
            &preamble.language_name,
            &preamble.resolved.injection_language,
        );

        if configs.is_empty() {
            log::debug!(
                "No bridge server configured for language: {} (host: {})",
                preamble.resolved.injection_language,
                preamble.language_name
            );
            return None;
        }

        let agg = self.resolve_aggregation_config(
            &preamble.language_name,
            &preamble.resolved.injection_language,
            method_name,
        );

        Some(DocumentRequestContext {
            uri: preamble.uri,
            resolved: preamble.resolved,
            configs,
            upstream_request_id: preamble.upstream_request_id,
            priorities: agg.priorities,
            strategy: agg.strategy,
            max_fan_out: agg.max_fan_out,
        })
    }

    /// Resolve all aggregation settings (strategy, priorities, max_fan_out) for a
    /// given host language, injection language, and LSP method in a single call.
    ///
    /// Performs one config lookup instead of three, avoiding redundant
    /// `resolve_bridge_language_config` calls when all settings are needed.
    pub(crate) fn resolve_aggregation_config(
        &self,
        host_language: &str,
        injection_language: &str,
        method_name: &str,
    ) -> ResolvedAggregationConfig {
        let settings = self.settings_manager.load_settings();
        resolve_aggregation_config_from_settings(
            &settings,
            host_language,
            injection_language,
            method_name,
        )
    }

    /// Resolve injection context for a bridge endpoint request (all matching servers).
    ///
    /// Delegates to the shared preamble, then looks up ALL bridge server configs
    /// for the injection language. Returns `None` if no configs found.
    pub(crate) fn resolve_bridge_contexts(
        &self,
        lsp_uri: &Uri,
        position: Position,
        method_name: &str,
    ) -> Option<PositionRequestContext> {
        let preamble = self.resolve_bridge_preamble(lsp_uri, position, method_name)?;
        let document = self.preamble_to_document_context(preamble, method_name)?;

        Some(PositionRequestContext { document, position })
    }

    /// Resolve injection context for a range-based bridge endpoint request.
    ///
    /// Uses `range.start` to find the injection region, then returns a
    /// [`RangeRequestContext`] with the full range for the handler to use.
    pub(crate) fn resolve_bridge_contexts_for_range(
        &self,
        lsp_uri: &Uri,
        range: Range,
        method_name: &str,
    ) -> Option<RangeRequestContext> {
        let preamble = self.resolve_bridge_preamble(lsp_uri, range.start, method_name)?;
        let document = self.preamble_to_document_context(preamble, method_name)?;

        Some(RangeRequestContext { document, range })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::settings::{
        AggregationConfig, AggregationStrategy, BridgeLanguageConfig, LanguageSettings,
    };
    use std::collections::HashMap;

    fn settings_with(languages: HashMap<String, LanguageSettings>) -> WorkspaceSettings {
        WorkspaceSettings {
            search_paths: Vec::new(),
            languages,
            capture_mappings: Default::default(),
            auto_install: false,
            language_servers: HashMap::new(),
        }
    }

    fn lang_settings(bridge: HashMap<String, BridgeLanguageConfig>) -> LanguageSettings {
        LanguageSettings {
            bridge: Some(bridge),
            ..Default::default()
        }
    }

    fn bridge_cfg_with_aggregation(
        method: &str,
        strategy: AggregationStrategy,
    ) -> BridgeLanguageConfig {
        let mut agg = HashMap::new();
        agg.insert(
            method.to_string(),
            AggregationConfig {
                priorities: None,
                strategy: Some(strategy),
                max_fan_out: None,
            },
        );
        BridgeLanguageConfig {
            enabled: None,
            aggregation: Some(agg),
        }
    }

    // ==========================================================================
    // resolve_layer_config_from_settings (cross-layer-aggregation)
    // ==========================================================================

    #[test]
    fn resolve_layer_config_defaults_when_language_has_no_settings() {
        let settings = settings_with(HashMap::new());
        let resolved =
            resolve_layer_config_from_settings(&settings, "markdown", "textDocument/hover");
        assert_eq!(
            resolved.order,
            vec![LayerSource::Virt, LayerSource::Host, LayerSource::Native]
        );
    }

    #[test]
    fn resolve_layer_config_respects_language_entry() {
        let mut langs = HashMap::new();
        langs.insert(
            "markdown".to_string(),
            LanguageSettings {
                layers: Some(HashMap::from([(
                    "textDocument/hover".to_string(),
                    crate::config::settings::LayerAggregationConfig {
                        order: Some(vec![LayerSource::Native]),
                        strategy: None,
                    },
                )])),
                ..Default::default()
            },
        );
        let settings = settings_with(langs);

        let hover = resolve_layer_config_from_settings(&settings, "markdown", "textDocument/hover");
        assert_eq!(hover.order, vec![LayerSource::Native]);
        assert!(!hover.allows(LayerSource::Virt));

        let definition =
            resolve_layer_config_from_settings(&settings, "markdown", "textDocument/definition");
        assert!(
            definition.allows(LayerSource::Virt),
            "unconfigured methods keep the default order"
        );
    }

    #[test]
    fn resolve_layer_config_falls_back_to_language_wildcard_entry() {
        // resolve_host_language_settings falls back to the "_" language for
        // hosts without their own entry, so layers configured there apply.
        let mut langs = HashMap::new();
        langs.insert(
            "_".to_string(),
            LanguageSettings {
                layers: Some(HashMap::from([(
                    "_".to_string(),
                    crate::config::settings::LayerAggregationConfig {
                        order: Some(vec![]),
                        strategy: None,
                    },
                )])),
                ..Default::default()
            },
        );
        let settings = settings_with(langs);
        let resolved =
            resolve_layer_config_from_settings(&settings, "markdown", "textDocument/hover");
        assert!(
            resolved.order.is_empty(),
            "wildcard-language layers must apply to unconfigured hosts"
        );
    }

    #[test]
    fn concatenated_formatting_pairs_finds_explicit_method_entry() {
        // Host "markdown" → injection "python" with concatenated formatting.
        let mut bridge = HashMap::new();
        bridge.insert(
            "python".to_string(),
            bridge_cfg_with_aggregation(
                "textDocument/formatting",
                AggregationStrategy::Concatenated,
            ),
        );
        let mut langs = HashMap::new();
        langs.insert("markdown".to_string(), lang_settings(bridge));

        let pairs = concatenated_formatting_pairs(&settings_with(langs));

        assert_eq!(pairs, vec![("markdown".to_string(), "python".to_string())]);
    }

    #[test]
    fn concatenated_formatting_pairs_finds_wildcard_method_entry() {
        // Wildcard "_" method entry with Concatenated applies to formatting too.
        let mut bridge = HashMap::new();
        bridge.insert(
            "python".to_string(),
            bridge_cfg_with_aggregation("_", AggregationStrategy::Concatenated),
        );
        let mut langs = HashMap::new();
        langs.insert("markdown".to_string(), lang_settings(bridge));

        let pairs = concatenated_formatting_pairs(&settings_with(langs));

        assert_eq!(
            pairs,
            vec![("markdown".to_string(), "python".to_string())],
            "wildcard '_' aggregation entry must apply to formatting"
        );
    }

    #[test]
    fn concatenated_formatting_pairs_method_entry_overrides_wildcard() {
        // Wildcard says Concatenated, but explicit formatting entry says Preferred.
        // Result: not warned (method entry wins).
        let mut agg = HashMap::new();
        agg.insert(
            "_".to_string(),
            AggregationConfig {
                priorities: None,
                strategy: Some(AggregationStrategy::Concatenated),
                max_fan_out: None,
            },
        );
        agg.insert(
            "textDocument/formatting".to_string(),
            AggregationConfig {
                priorities: None,
                strategy: Some(AggregationStrategy::Preferred),
                max_fan_out: None,
            },
        );
        let mut bridge = HashMap::new();
        bridge.insert(
            "python".to_string(),
            BridgeLanguageConfig {
                enabled: None,
                aggregation: Some(agg),
            },
        );
        let mut langs = HashMap::new();
        langs.insert("markdown".to_string(), lang_settings(bridge));

        let pairs = concatenated_formatting_pairs(&settings_with(langs));

        assert!(
            pairs.is_empty(),
            "method-specific Preferred must override wildcard Concatenated"
        );
    }

    #[test]
    fn concatenated_formatting_pairs_excludes_pairs_with_non_empty_priorities() {
        // Since the concatenated formatting pipeline landed (#327), a
        // `concatenated` strategy WITH a non-empty `priorities` list is a valid
        // configuration that runs the pipeline — it must NOT be warned about.
        let mut agg = HashMap::new();
        agg.insert(
            "textDocument/formatting".to_string(),
            AggregationConfig {
                priorities: Some(vec!["black".to_string(), "isort".to_string()]),
                strategy: Some(AggregationStrategy::Concatenated),
                max_fan_out: None,
            },
        );
        let mut bridge = HashMap::new();
        bridge.insert(
            "python".to_string(),
            BridgeLanguageConfig {
                enabled: None,
                aggregation: Some(agg),
            },
        );
        let mut langs = HashMap::new();
        langs.insert("markdown".to_string(), lang_settings(bridge));

        let pairs = concatenated_formatting_pairs(&settings_with(langs));

        assert!(
            pairs.is_empty(),
            "concatenated formatting with non-empty priorities runs the \
             pipeline and must not be flagged as a misconfiguration"
        );
    }

    #[test]
    fn concatenated_formatting_pairs_inherits_priorities_from_wildcard_entry() {
        // Field-level wildcard inheritance: the method entry sets the strategy,
        // the "_" wildcard supplies the priorities. The resolved config has a
        // non-empty priorities list, so the pipeline runs — no warning.
        let mut agg = HashMap::new();
        agg.insert(
            "textDocument/formatting".to_string(),
            AggregationConfig {
                priorities: None,
                strategy: Some(AggregationStrategy::Concatenated),
                max_fan_out: None,
            },
        );
        agg.insert(
            "_".to_string(),
            AggregationConfig {
                priorities: Some(vec!["black".to_string()]),
                strategy: None,
                max_fan_out: None,
            },
        );
        let mut bridge = HashMap::new();
        bridge.insert(
            "python".to_string(),
            BridgeLanguageConfig {
                enabled: None,
                aggregation: Some(agg),
            },
        );
        let mut langs = HashMap::new();
        langs.insert("markdown".to_string(), lang_settings(bridge));

        let pairs = concatenated_formatting_pairs(&settings_with(langs));

        assert!(
            pairs.is_empty(),
            "priorities inherited from the wildcard entry must count as \
             a configured (non-empty) pipeline definition"
        );
    }

    #[test]
    fn format_concatenated_formatting_warning_explains_empty_priorities_fallback() {
        // The warning is now scoped to the actual misconfiguration —
        // `concatenated` with EMPTY priorities — so the message must say the
        // pipeline needs a non-empty priorities list and that these pairs fall
        // back to 'preferred', not that the strategy is always ignored.
        let msg = format_concatenated_formatting_warning(&[(
            "markdown".to_string(),
            "python".to_string(),
        )])
        .expect("non-empty input must yield a message");
        assert!(
            msg.contains("priorities"),
            "message must mention the empty priorities cause; got: {msg}"
        );
        assert!(
            msg.contains("preferred"),
            "message must state the preferred fallback; got: {msg}"
        );
        assert!(
            !msg.contains("the configured strategy is ignored"),
            "stale 'strategy is ignored' phrasing must be retired; got: {msg}"
        );
    }

    #[test]
    fn concatenated_formatting_pairs_resolves_bridge_key_wildcard_inheritance() {
        // Priorities supplied by the `bridge._` wildcard entry, strategy by
        // the concrete `bridge.python` entry. The runtime merges the two
        // (resolve_with_wildcard), so the warning path must too — flagging
        // this valid configuration would be a false positive.
        let mut python_agg = HashMap::new();
        python_agg.insert(
            "textDocument/formatting".to_string(),
            AggregationConfig {
                priorities: None,
                strategy: Some(AggregationStrategy::Concatenated),
                max_fan_out: None,
            },
        );
        let mut wildcard_agg = HashMap::new();
        wildcard_agg.insert(
            "textDocument/formatting".to_string(),
            AggregationConfig {
                priorities: Some(vec!["black".to_string()]),
                strategy: None,
                max_fan_out: None,
            },
        );
        let mut bridge = HashMap::new();
        bridge.insert(
            "python".to_string(),
            BridgeLanguageConfig {
                enabled: None,
                aggregation: Some(python_agg),
            },
        );
        bridge.insert(
            "_".to_string(),
            BridgeLanguageConfig {
                enabled: None,
                aggregation: Some(wildcard_agg),
            },
        );
        let mut langs = HashMap::new();
        langs.insert("markdown".to_string(), lang_settings(bridge));

        let pairs = concatenated_formatting_pairs(&settings_with(langs));

        assert!(
            pairs.is_empty(),
            "priorities inherited from the bridge._ wildcard entry must count \
             as a configured pipeline definition; got: {pairs:?}"
        );
    }

    #[test]
    fn concatenated_formatting_pairs_excludes_explicit_empty_priorities() {
        // priorities = [] is the deliberate per-method kill switch
        // (aggregation-priorities-wildcard): the region runs nothing. That is
        // intended behavior, not a misconfiguration — no warning.
        let mut agg = HashMap::new();
        agg.insert(
            "textDocument/formatting".to_string(),
            AggregationConfig {
                priorities: Some(vec![]),
                strategy: Some(AggregationStrategy::Concatenated),
                max_fan_out: None,
            },
        );
        let mut bridge = HashMap::new();
        bridge.insert(
            "python".to_string(),
            BridgeLanguageConfig {
                enabled: None,
                aggregation: Some(agg),
            },
        );
        let mut langs = HashMap::new();
        langs.insert("markdown".to_string(), lang_settings(bridge));

        let pairs = concatenated_formatting_pairs(&settings_with(langs));

        assert!(
            pairs.is_empty(),
            "an explicit [] disables the method deliberately and must not be warned about"
        );
    }

    #[test]
    fn concatenated_formatting_pairs_flags_wildcard_only_priorities() {
        // priorities = ["*"] (also the resolved default for an absent list)
        // carries no explicit name: the pipeline's order is undefined, so the
        // pair must be flagged for the settings-apply warning.
        let mut agg = HashMap::new();
        agg.insert(
            "textDocument/formatting".to_string(),
            AggregationConfig {
                priorities: Some(vec![
                    crate::config::settings::PRIORITIES_WILDCARD.to_string(),
                ]),
                strategy: Some(AggregationStrategy::Concatenated),
                max_fan_out: None,
            },
        );
        let mut bridge = HashMap::new();
        bridge.insert(
            "python".to_string(),
            BridgeLanguageConfig {
                enabled: None,
                aggregation: Some(agg),
            },
        );
        let mut langs = HashMap::new();
        langs.insert("markdown".to_string(), lang_settings(bridge));

        let pairs = concatenated_formatting_pairs(&settings_with(langs));

        assert_eq!(pairs, vec![("markdown".to_string(), "python".to_string())]);
    }

    #[test]
    fn concatenated_formatting_pairs_returns_empty_for_preferred() {
        let mut bridge = HashMap::new();
        bridge.insert(
            "python".to_string(),
            bridge_cfg_with_aggregation("textDocument/formatting", AggregationStrategy::Preferred),
        );
        let mut langs = HashMap::new();
        langs.insert("markdown".to_string(), lang_settings(bridge));

        let pairs = concatenated_formatting_pairs(&settings_with(langs));

        assert!(pairs.is_empty());
    }

    #[test]
    fn concatenated_formatting_pairs_ignores_other_methods() {
        // Concatenated for diagnostic (its default) — must NOT warn.
        let mut bridge = HashMap::new();
        bridge.insert(
            "python".to_string(),
            bridge_cfg_with_aggregation(
                "textDocument/diagnostic",
                AggregationStrategy::Concatenated,
            ),
        );
        let mut langs = HashMap::new();
        langs.insert("markdown".to_string(), lang_settings(bridge));

        let pairs = concatenated_formatting_pairs(&settings_with(langs));

        assert!(pairs.is_empty());
    }

    // ==========================================================================
    // format_concatenated_formatting_warning (review MEDIUM follow-up)
    // ==========================================================================
    //
    // The previous emitter looped over pairs and called `log_warning` N times,
    // spamming the editor log on every settings reload. The aggregated
    // formatter renders a single message; these tests pin the shape so
    // changes to the warning text remain deliberate.

    #[test]
    fn format_concatenated_formatting_warning_returns_none_for_empty_input() {
        assert_eq!(
            format_concatenated_formatting_warning(&[]),
            None,
            "no pairs => no warning to emit (caller skips log_warning entirely)"
        );
    }

    #[test]
    fn format_concatenated_formatting_warning_lists_single_pair() {
        let msg = format_concatenated_formatting_warning(&[(
            "markdown".to_string(),
            "python".to_string(),
        )])
        .expect("non-empty input must yield a message");
        assert!(
            msg.contains("1 (host->injection) pair(s)"),
            "message must report count; got: {msg}"
        );
        assert!(
            msg.contains("markdown->python"),
            "message must contain the pair; got: {msg}"
        );
    }

    #[test]
    fn format_concatenated_formatting_warning_aggregates_multiple_pairs_into_single_message() {
        let pairs = vec![
            ("markdown".to_string(), "lua".to_string()),
            ("markdown".to_string(), "python".to_string()),
            ("rust".to_string(), "python".to_string()),
        ];

        let msg = format_concatenated_formatting_warning(&pairs)
            .expect("non-empty input must yield a message");

        assert!(
            msg.contains("3 (host->injection) pair(s)"),
            "count must reflect input length; got: {msg}"
        );
        // All three pairs must appear, separated for readability. We don't
        // pin the exact separator beyond "comma-separated" so future tidying
        // can choose a different delimiter without breaking this test.
        for needle in ["markdown->lua", "markdown->python", "rust->python"] {
            assert!(msg.contains(needle), "missing '{needle}' in: {msg}");
        }
    }

    #[test]
    fn format_concatenated_formatting_warning_renders_wildcard_injection_meaningfully() {
        // A `_` injection key is the wildcard template every unlisted
        // injection language inherits — the warning must not present it as a
        // literal language named "_".
        let msg =
            format_concatenated_formatting_warning(&[("markdown".to_string(), "_".to_string())])
                .expect("non-empty input must yield a message");
        assert!(
            msg.contains("markdown->(any other injection)"),
            "wildcard pair must be rendered as a template, got: {msg}"
        );
        assert!(
            !msg.contains("markdown->_"),
            "raw '_' rendering must be replaced; got: {msg}"
        );
    }

    #[test]
    fn format_concatenated_formatting_warning_preserves_input_order() {
        // Caller passes a sorted vec; the message should list them in that
        // order so consecutive emissions are byte-identical and editors can
        // dedupe notifications. (Sorting at this layer would hide bugs in
        // the upstream `concatenated_formatting_pairs.sort()` call.)
        let pairs = vec![
            ("zeta".to_string(), "alpha".to_string()),
            ("alpha".to_string(), "zeta".to_string()),
        ];

        let msg = format_concatenated_formatting_warning(&pairs).unwrap();

        let zeta_idx = msg.find("zeta->alpha").expect("first pair must appear");
        let alpha_idx = msg.find("alpha->zeta").expect("second pair must appear");
        assert!(
            zeta_idx < alpha_idx,
            "input order must be preserved verbatim; got: {msg}"
        );
    }

    #[test]
    fn concatenated_formatting_pairs_sorts_deterministically() {
        // HashMap iteration order is non-deterministic; the function must
        // sort the output so warnings are emitted in a stable order.
        let mut langs = HashMap::new();
        for (host, injection) in [
            ("rust", "python"),
            ("markdown", "lua"),
            ("markdown", "python"),
        ] {
            let mut bridge = HashMap::new();
            bridge.insert(
                injection.to_string(),
                bridge_cfg_with_aggregation(
                    "textDocument/formatting",
                    AggregationStrategy::Concatenated,
                ),
            );
            langs
                .entry(host.to_string())
                .or_insert_with(|| lang_settings(HashMap::new()))
                .bridge
                .as_mut()
                .unwrap()
                .extend(bridge);
        }

        let pairs = concatenated_formatting_pairs(&settings_with(langs));

        assert_eq!(
            pairs,
            vec![
                ("markdown".to_string(), "lua".to_string()),
                ("markdown".to_string(), "python".to_string()),
                ("rust".to_string(), "python".to_string()),
            ]
        );
    }
}
