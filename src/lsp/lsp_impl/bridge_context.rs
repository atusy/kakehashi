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

/// Whether the capability prefilter (capability-prefilter-fanout) may drop a
/// server known to lack `method` before fan-out.
///
/// The prefilter relies on `has_capability(method) == false` being observably
/// equivalent to the handler returning an empty result. That holds for every
/// routed method EXCEPT the ones whose handler falls back to a *different*
/// capability when the requested one is absent — dropping a server by the
/// requested capability alone would then discard a server that would have
/// answered via the fallback:
/// - `textDocument/prepareRename` → `send_prepare_rename_request` returns
///   `DefaultBehavior` (rename stays enabled) when the server advertises
///   `textDocument/rename` but not `prepareRename`.
/// - `textDocument/formatting` / `textDocument/rangeFormatting` → the
///   Concatenated pipeline (`pipeline_step_request_kind` in the
///   [`formatting`](crate::lsp::lsp_impl::text_document::formatting) module) uses
///   a server that supports EITHER full OR range formatting, sending whichever
///   it advertises; filtering by one alone would drop a server capable of the
///   other.
///
/// For exempt methods the per-handler capability gate still runs and handles
/// the fallback correctly.
fn capability_prefilter_applies(method: &str) -> bool {
    !matches!(
        method,
        "textDocument/prepareRename" | "textDocument/formatting" | "textDocument/rangeFormatting"
    )
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
    /// The editor's `workDoneToken` for this request, if any. When set, dispatch
    /// aggregates the fanned-out downstreams' `$/progress` onto it
    /// (ls-bridge-client-progress); `None` disables client-progress aggregation.
    pub(crate) client_progress_token: Option<tower_lsp_server::ls_types::NumberOrString>,
}

/// All resolved context needed to send a **host** bridge request
/// (host-document-bridge): the real URI, the host language and text
/// verbatim, plus the servers selected for the host role and the `_self`
/// aggregation settings.
///
/// `strategy` is the *within-host-layer* combine from
/// `bridge._self.aggregation` — consumed by diagnostics (concatenated
/// across host servers by default); the verbatim raw-request path combines
/// with `preferred` regardless. The *cross-layer* strategy lives in the
/// caller (`walk_layers` / the formatting pipeline / the diagnostics
/// layer merge).
pub(crate) struct HostRequestContext {
    /// The real client URI (forwarded verbatim — no virtual URI).
    pub(crate) uri: Url,
    /// The host language, used as the downstream `languageId`.
    pub(crate) language_id: String,
    /// The current host text, shared across per-server tasks.
    pub(crate) text: std::sync::Arc<str>,
    /// Host-capable server configs (`languages` contains the host language),
    /// gated on the explicit `bridge._self.enabled = true` opt-in.
    pub(crate) configs: Vec<ResolvedServerConfig>,
    /// Ordered allowlist from `bridge._self.aggregation` (wildcard-merged).
    pub(crate) priorities: Vec<String>,
    /// Within-host-layer combine strategy from `bridge._self.aggregation`.
    pub(crate) strategy: AggregationStrategy,
    /// Fan-out cap from `bridge._self.aggregation`.
    pub(crate) max_fan_out: Option<usize>,
    /// The upstream JSON-RPC request ID for cancel forwarding.
    pub(crate) upstream_request_id: Option<UpstreamId>,
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

/// Generic emptiness check for raw host-layer results in the `preferred`
/// fan-in: `null` and `[]` are "no result", as are the object-shaped
/// "empty but valid" responses whose single canonical list field is empty —
/// `CompletionList.items`, `SignatureHelp.signatures`,
/// `LinkedEditingRanges.ranges`. Without the object shapes, a
/// higher-priority host server's empty list would prematurely win the
/// fan-in over a lower-priority server with actual results.
pub(crate) fn is_empty_layer_value(value: &serde_json::Value) -> bool {
    if value.is_null() {
        return true;
    }
    if let Some(items) = value.as_array() {
        return items.is_empty();
    }
    if let Some(object) = value.as_object() {
        for key in ["items", "signatures", "ranges"] {
            if let Some(list) = object.get(key).and_then(serde_json::Value::as_array) {
                return list.is_empty();
            }
        }
    }
    false
}

/// Race the virt, host, and native layer futures **concurrently** and decide
/// by the resolved layer `priorities` (cross-layer-aggregation, `preferred`
/// semantics) — the layer-level analogue of the stage-1 `preferred` fan-in:
///
/// - all layers' requests are in flight at once (latency = max, not sum);
/// - a completed lower-priority result is buffered while a higher-priority
///   layer is still pending — priority decides, not arrival order;
/// - a higher-priority layer that completes non-empty wins immediately and
///   the still-pending loser futures are dropped (best-effort abandonment,
///   like the stage-1 `abort_all`);
/// - layers absent from `priorities` are never polled (their future is
///   created but async blocks are lazy);
/// - native is the lexical-name-resolution contributor; handlers without one
///   pass a ready `Ok(None)`.
pub(crate) async fn race_layers_preferred<R>(
    priorities: &[LayerSource],
    virt: impl Future<Output = tower_lsp_server::jsonrpc::Result<Option<R>>>,
    host: impl Future<Output = tower_lsp_server::jsonrpc::Result<Option<R>>>,
    native: impl Future<Output = tower_lsp_server::jsonrpc::Result<Option<R>>>,
    is_nonempty: impl Fn(&R) -> bool,
) -> tower_lsp_server::jsonrpc::Result<Option<R>> {
    let mut virt_fut = std::pin::pin!(virt);
    let mut host_fut = std::pin::pin!(host);
    let mut native_fut = std::pin::pin!(native);
    // `None` = still pending; `Some(result)` = completed. Layers not in
    // `priorities` start as completed-empty so their guard never enables and
    // the decision walk skips them.
    let mut virt_state: Option<Option<R>> =
        (!priorities.contains(&LayerSource::Virt)).then_some(None);
    let mut host_state: Option<Option<R>> =
        (!priorities.contains(&LayerSource::Host)).then_some(None);
    let mut native_state: Option<Option<R>> =
        (!priorities.contains(&LayerSource::Native)).then_some(None);

    loop {
        // Decision walk in priority order: a pending higher-priority layer
        // blocks; a completed empty one falls through; a completed non-empty
        // one wins.
        let mut blocked = false;
        for layer in priorities {
            let slot = match layer {
                LayerSource::Virt => &mut virt_state,
                LayerSource::Host => &mut host_state,
                LayerSource::Native => &mut native_state,
            };
            match slot {
                None => {
                    blocked = true;
                    break;
                }
                Some(result) => {
                    if result.as_ref().is_some_and(&is_nonempty) {
                        return Ok(result.take());
                    }
                }
            }
        }
        if !blocked {
            return Ok(None);
        }

        tokio::select! {
            result = &mut virt_fut, if virt_state.is_none() => {
                virt_state = Some(result?);
            }
            result = &mut host_fut, if host_state.is_none() => {
                host_state = Some(result?);
            }
            result = &mut native_fut, if native_state.is_none() => {
                native_state = Some(result?);
            }
        }
    }
}

/// Deserialize a verbatim host-layer result into the handler's response
/// type, logging (not erroring) on shape mismatch.
pub(crate) fn parse_host_verbatim<R: serde::de::DeserializeOwned>(
    value: serde_json::Value,
) -> Option<R> {
    match serde_json::from_value::<R>(value) {
        Ok(parsed) => Some(parsed),
        Err(e) => {
            log::warn!(
                target: "kakehashi::bridge",
                "host response failed to deserialize: {e}"
            );
            None
        }
    }
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

/// Names of concrete `languageServers` entries whose wildcard-resolved `cmd`
/// is still empty — unspawnable, so every lookup silently skips them
/// (wildcard-config-inheritance). Before cmd became optional (to allow a
/// defaults-only `languageServers._` entry) this was a hard deserialization
/// error; the warning restores that feedback at settings-apply time instead
/// of leaving the user with a server that never runs.
///
/// A server that resolves to `enabled: false` is excluded even if its `cmd`
/// is also empty: disabling a server is a deliberate choice, not the
/// misconfiguration this warning exists to surface.
pub(crate) fn unspawnable_language_servers(settings: &WorkspaceSettings) -> Vec<String> {
    let servers = &settings.language_servers;
    let mut names: Vec<String> = servers
        .keys()
        .filter(|name| *name != crate::config::WILDCARD_KEY)
        .filter(|name| {
            crate::config::resolve_with_wildcard(
                servers,
                name,
                crate::config::merge_bridge_server_configs,
            )
            // A deliberately disabled server is not a misconfiguration —
            // don't warn about its missing cmd.
            .is_none_or(|config| config.cmd.is_empty() && config.is_enabled())
        })
        .cloned()
        .collect();
    names.sort();
    names
}

/// Render the single aggregated client-facing warning for unspawnable
/// `languageServers` entries (empty resolved `cmd`). `None` when there is
/// nothing to warn about.
pub(crate) fn format_unspawnable_servers_warning(names: &[String]) -> Option<String> {
    if names.is_empty() {
        return None;
    }
    Some(format!(
        "languageServers with no 'cmd' (after merging the '_' wildcard \
         defaults): {}. Such servers cannot be spawned and are skipped by \
         every request.",
        names.join(", ")
    ))
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
                // Expected under concurrent layer fan-out: `walk_layers`
                // subscribes for the whole walk, so the virt/host arms'
                // own subscribe attempts find the id taken. The arm runs
                // without a local receiver; prompt cancellation is provided
                // by the walk-level select (and downstream forwarding by the
                // upstream-request registry, which is independent of this).
                log::debug!(
                    "Cancel subscribe for {}: already subscribed (another \
                     layer holds it); proceeding without a local receiver",
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
    /// resolved `layers.priorities`, the bridge dispatch is skipped entirely.
    ///
    /// Used by entry points outside the [`Self::walk_layers`] race — the
    /// shared bridge preamble and the virt-only handlers (rangeFormatting's
    /// region pass, documentColor) that have no host contributor yet.
    /// Handlers on the walk get layer membership from the race itself, and
    /// the diagnostics paths resolve the full layer config directly (they
    /// gate virt and host independently).
    pub(crate) fn virt_layer_enabled(&self, host_language: &str, method_name: &str) -> bool {
        self.resolve_layer_config(host_language, method_name)
            .allows(LayerSource::Virt)
    }

    /// Shared capability prefilter (capability-prefilter-fanout): the set of
    /// server names to drop from `method`'s virt fan-out across the given
    /// `injection_languages` — those with a live `Ready` connection already
    /// known to lack the capability (so the fan-out never spawns a task +
    /// connection lookup only to hit the per-handler gate and return empty).
    ///
    /// One pool query for the whole request; every virt fan-out entry point
    /// (per-region diagnostic loop, whole-document helper, documentSymbol /
    /// documentColor, position/range preamble) routes through here so the drop
    /// policy is uniform. Returns an empty set — a cheap no-op for callers —
    /// when `method` is exempt (see [`capability_prefilter_applies`]) or nothing
    /// is configured. `injection_languages` may repeat; it is deduped here.
    ///
    /// The owned `configs_by_lang` exists only so the candidate `&str`s can
    /// borrow across the `await`; the per-language resolution is a cached lookup
    /// and callers re-resolve their own per-region configs (same cached call).
    pub(super) async fn incapable_virt_servers<'a>(
        &self,
        host_language: &str,
        injection_languages: impl IntoIterator<Item = &'a str>,
        method: &str,
    ) -> std::collections::HashSet<String> {
        if !capability_prefilter_applies(method) {
            return std::collections::HashSet::new();
        }
        let mut langs: Vec<&str> = injection_languages.into_iter().collect();
        langs.sort_unstable();
        langs.dedup();
        let configs_by_lang: Vec<_> = langs
            .into_iter()
            .map(|lang| self.bridge_configs_for_injection_language(host_language, lang))
            .collect();
        let candidates: std::collections::HashSet<&str> = configs_by_lang
            .iter()
            .flatten()
            .map(|c| c.server_name.as_str())
            .collect();
        if candidates.is_empty() {
            return std::collections::HashSet::new();
        }
        self.bridge
            .pool_arc()
            .servers_known_incapable(&candidates, method)
            .await
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
        if !self.virt_layer_enabled(&preamble.language_name, method_name) {
            log::debug!(
                "{}: virt layer disabled for {} via layers.aggregation priorities",
                method_name,
                preamble.language_name
            );
            return None;
        }

        let mut configs = self.bridge_configs_for_injection_language(
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

        // Drop servers already known (a live, `Ready` connection) NOT to support
        // this method, so the fan-out never spawns a task + connection lookup
        // only to hit the per-handler capability gate and return empty
        // (capability-prefilter-fanout).
        let incapable = self
            .incapable_virt_servers(
                &preamble.language_name,
                std::iter::once(preamble.resolved.injection_language.as_str()),
                method_name,
            )
            .await;
        if !incapable.is_empty() {
            configs.retain(|c| !incapable.contains(&c.server_name));
            if configs.is_empty() {
                log::debug!(
                    "{}: all configured servers for {} are known-incapable; skipping virt layer",
                    method_name,
                    preamble.resolved.injection_language
                );
                return None;
            }
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
            client_progress_token: None,
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

    /// Resolve the context for a **host** bridge request
    /// (host-document-bridge): the host language's own servers, gated on the
    /// explicit `bridge._self.enabled = true` opt-in.
    ///
    /// Returns `None` when the document is missing, host bridging is not
    /// opted in, or no configured server lists the host language.
    pub(crate) fn resolve_host_bridge_context(
        &self,
        lsp_uri: &Uri,
        method_name: &str,
    ) -> Option<HostRequestContext> {
        let uri = uri_to_url(lsp_uri).ok()?;
        // Host tier needs only the text, never the parse tree
        // (parse-decoupled-document-lifecycle ADR): read `text_arc()` directly
        // rather than `snapshot()?`, which requires a tree. Otherwise — now that
        // `didChange` clears the tree and reparses off-ingress — every host-bridged
        // request (hover / definition / formatting / will-save / diagnostics) would
        // bail to `None` for the whole reparse window after each edit, even though
        // it forwards the real URI + text verbatim and depends on no tree.
        let text = self.documents.get(&uri)?.text_arc();
        let language_name = self.document_language(&uri)?;

        let settings = self.settings_manager.load_settings();
        let lang_settings = settings.resolve_host_language_settings(&language_name)?;
        if !lang_settings.is_host_bridging_enabled() {
            log::debug!(
                "{}: host bridging not opted in for {} (bridge._self.enabled)",
                method_name,
                language_name
            );
            return None;
        }

        let configs = self
            .bridge
            .cached_host_configs_for_language(&settings, &language_name);
        if configs.is_empty() {
            log::debug!(
                "{}: no host-capable server configured for {}",
                method_name,
                language_name
            );
            return None;
        }

        let agg = lang_settings.resolve_host_aggregation(method_name);

        Some(HostRequestContext {
            uri,
            text,
            language_id: language_name,
            configs,
            priorities: agg.priorities,
            strategy: agg.strategy,
            max_fan_out: agg.max_fan_out,
            upstream_request_id: current_upstream_id(),
        })
    }

    /// Dispatch a host bridge request over a resolved [`HostRequestContext`]:
    /// the upstream `params` are forwarded verbatim (real URI, real
    /// coordinates) and the raw `result` value comes back untranslated.
    pub(crate) async fn host_layer_value_with_ctx(
        &self,
        ctx: &HostRequestContext,
        request_method: &'static str,
        params: serde_json::Value,
    ) -> tower_lsp_server::jsonrpc::Result<Option<serde_json::Value>> {
        let (cancel_rx, _cancel_guard) = self.subscribe_cancel(ctx.upstream_request_id.as_ref());
        let pool = self.bridge.pool_arc();
        let result = crate::lsp::aggregation::server::dispatch_host_preferred(
            ctx,
            pool.clone(),
            move |t| {
                let params = params.clone();
                async move {
                    t.pool
                        .send_host_raw_request(
                            &t.server_name,
                            &t.server_config,
                            &crate::lsp::bridge::HostDocument {
                                uri: &t.uri,
                                language_id: &t.language_id,
                                text: &t.text,
                            },
                            request_method,
                            params,
                            t.upstream_id,
                        )
                        .await
                }
            },
            |opt| matches!(opt, Some(v) if !is_empty_layer_value(v)),
            cancel_rx,
        )
        .await;
        pool.unregister_all_for_upstream_id(ctx.upstream_request_id.as_ref());
        // Quieter than `FanInResult::handle`: an all-empty host layer is the
        // normal outcome whenever virt answers, and the virt arm already
        // emits the client-visible "no response" LOG — only real host
        // failures get surfaced here.
        match result {
            crate::lsp::aggregation::server::FanInResult::Done(value) => Ok(value),
            crate::lsp::aggregation::server::FanInResult::NoResult { errors } => {
                if errors > 0 {
                    self.client
                        .log_message(
                            tower_lsp_server::ls_types::MessageType::WARNING,
                            format!("No {request_method} response from any host bridge server"),
                        )
                        .await;
                }
                Ok(None)
            }
            crate::lsp::aggregation::server::FanInResult::Cancelled => {
                Err(tower_lsp_server::jsonrpc::Error::request_cancelled())
            }
        }
    }

    /// Walk the resolved layer order for a request method
    /// (cross-layer-aggregation, `preferred` semantics): the virt and host
    /// layers fan out **concurrently** ([`race_layers_preferred`]) and the
    /// highest-priority non-empty result wins.
    ///
    /// - `virt` is the handler's existing injection-bridge future, polled
    ///   only when the virt layer is in `priorities`.
    /// - The host layer forwards `raw_params` verbatim
    ///   (host-document-bridge) and maps the raw result via `host_parse`.
    /// - `layer_method` keys `layers.priorities` and the `_self` aggregation
    ///   (e.g. rangeFormatting shares `textDocument/formatting`);
    ///   `request_method` is what goes on the wire and gates capability.
    /// - Native has no contributor for bridged methods.
    ///
    /// A losing in-flight future is dropped; its RAII guards clean up the
    /// response router and cancel subscription, and the trailing
    /// `unregister_all_for_upstream_id` sweeps any upstream-registry entries
    /// the dropped future did not get to remove itself.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn walk_layers<R>(
        &self,
        lsp_uri: &Uri,
        layer_method: &'static str,
        request_method: &'static str,
        raw_params: serde_json::Value,
        virt: impl Future<Output = tower_lsp_server::jsonrpc::Result<Option<R>>>,
        host_parse: impl Fn(serde_json::Value) -> Option<R>,
        is_nonempty: impl Fn(&R) -> bool,
    ) -> tower_lsp_server::jsonrpc::Result<Option<R>> {
        // Methods without a native contributor: the native layer completes
        // empty immediately and the walk falls through it.
        self.walk_layers_with_native(
            lsp_uri,
            layer_method,
            request_method,
            raw_params,
            virt,
            std::future::ready(Ok(None)),
            host_parse,
            is_nonempty,
        )
        .await
    }

    /// [`Self::walk_layers`] with a native-layer contributor
    /// (lexical-name-resolution) racing alongside virt and host.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn walk_layers_with_native<R>(
        &self,
        lsp_uri: &Uri,
        layer_method: &'static str,
        request_method: &'static str,
        raw_params: serde_json::Value,
        virt: impl Future<Output = tower_lsp_server::jsonrpc::Result<Option<R>>>,
        native: impl Future<Output = tower_lsp_server::jsonrpc::Result<Option<R>>>,
        host_parse: impl Fn(serde_json::Value) -> Option<R>,
        is_nonempty: impl Fn(&R) -> bool,
    ) -> tower_lsp_server::jsonrpc::Result<Option<R>> {
        let host = async {
            match self.resolve_host_bridge_context(lsp_uri, layer_method) {
                Some(ctx) => Ok(self
                    .host_layer_value_with_ctx(&ctx, request_method, raw_params.clone())
                    .await?
                    .and_then(&host_parse)),
                None => Ok(None),
            }
        };

        self.walk_layer_futures(
            lsp_uri,
            layer_method,
            request_method,
            virt,
            host,
            native,
            is_nonempty,
        )
        .await
    }

    /// Race pre-built virt/host/native layer futures under the resolved
    /// layer priorities (cross-layer-aggregation, `preferred` semantics),
    /// with walk-wide cancel subscription and upstream-registry sweep.
    ///
    /// This is the walk core behind [`Self::walk_layers_with_native`];
    /// handlers that need a custom host arm (e.g. codeAction's per-server
    /// title suffixing, which the verbatim raw-value host arm cannot
    /// express) build their own host future and call this directly.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn walk_layer_futures<R>(
        &self,
        lsp_uri: &Uri,
        layer_method: &'static str,
        request_method: &'static str,
        virt: impl Future<Output = tower_lsp_server::jsonrpc::Result<Option<R>>>,
        host: impl Future<Output = tower_lsp_server::jsonrpc::Result<Option<R>>>,
        native: impl Future<Output = tower_lsp_server::jsonrpc::Result<Option<R>>>,
        is_nonempty: impl Fn(&R) -> bool,
    ) -> tower_lsp_server::jsonrpc::Result<Option<R>> {
        let Ok(uri) = uri_to_url(lsp_uri) else {
            log::warn!("Invalid URI in {}: {}", request_method, lsp_uri.as_str());
            return Ok(None);
        };
        let Some(host_language) = self.document_language(&uri) else {
            return Ok(None);
        };
        let layer_cfg = self.resolve_layer_config(&host_language, layer_method);

        // Subscribe for the WHOLE walk before either arm runs: the arms'
        // own subscribe attempts then find the id taken and proceed without
        // a local receiver (logged at debug), and this select cancels the
        // race — dropping both in-flight arms — the moment `$/cancelRequest`
        // arrives, regardless of which arm would have held the subscription.
        // Best-effort gap: between the race's completion and the next
        // request there is no subscriber; a cancel landing there is only
        // forwarded downstream via the upstream-request registry.
        let (cancel_rx, _cancel_guard) = self.subscribe_cancel(current_upstream_id().as_ref());
        let race = race_layers_preferred(&layer_cfg.priorities, virt, host, native, is_nonempty);
        let result = match cancel_rx {
            Some(mut cancel_rx) => {
                tokio::select! {
                    biased;
                    _ = &mut cancel_rx => Err(tower_lsp_server::jsonrpc::Error::request_cancelled()),
                    result = race => result,
                }
            }
            None => race.await,
        };
        // Sweep upstream-registry entries a dropped (losing or cancelled)
        // layer future did not get to unregister itself. Idempotent; a
        // completed arm already cleaned up its own.
        self.bridge
            .pool_arc()
            .unregister_all_for_upstream_id(current_upstream_id().as_ref());
        result
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
        // Ensure a fresh tree before the (sync) preamble snapshots it: `didChange`
        // now clears the tree and reparses off-ingress, so without this an injection
        // request (hover / definition / completion / …) issued in the reparse window
        // would find `snapshot()` empty and return null after every edit.
        self.ensure_fresh_tree_for_bridge(lsp_uri).await;
        let preamble = self.resolve_bridge_preamble(lsp_uri, position, method_name)?;
        let document = self
            .preamble_to_document_context(preamble, method_name)
            .await?;

        Some(PositionRequestContext { document, position })
    }

    /// Wait for / on-demand the document's tree before a sync bridge-preamble
    /// snapshot (shared by the position and range resolvers).
    async fn ensure_fresh_tree_for_bridge(&self, lsp_uri: &Uri) {
        if let Ok(uri) = uri_to_url(lsp_uri) {
            self.ensure_document_parsed(&uri).await;
        }
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
        self.ensure_fresh_tree_for_bridge(lsp_uri).await;
        let preamble = self.resolve_bridge_preamble(lsp_uri, range.start, method_name)?;
        let document = self
            .preamble_to_document_context(preamble, method_name)
            .await?;

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
            diagnostics_debounce_ms: crate::config::settings::DEFAULT_DEBOUNCE_MS,
            language_servers: HashMap::new(),
        }
    }

    fn lang_settings(bridge: HashMap<String, BridgeLanguageConfig>) -> LanguageSettings {
        LanguageSettings {
            bridge: Some(bridge),
            ..Default::default()
        }
    }

    fn server(cmd: &[&str]) -> crate::config::settings::BridgeServerConfig {
        crate::config::settings::BridgeServerConfig {
            cmd: cmd.iter().map(|s| s.to_string()).collect(),
            languages: vec!["lua".to_string()],
            initialization_options: None,
            workspace_markers: None,
            on_type_formatting_triggers: None,
            prefer_shared_instance: None,
            enabled: None,
            settings: None,
        }
    }

    #[test]
    fn unspawnable_language_servers_flags_empty_cmd_but_not_wildcard() {
        let mut settings = settings_with(HashMap::new());
        settings.language_servers = HashMap::from([
            ("_".to_string(), server(&[])), // defaults-only entry: never flagged
            ("good".to_string(), server(&["lua-language-server"])),
            ("broken".to_string(), server(&[])),
            ("also-broken".to_string(), server(&[])),
        ]);

        assert_eq!(
            unspawnable_language_servers(&settings),
            vec!["also-broken".to_string(), "broken".to_string()],
        );
    }

    #[test]
    fn unspawnable_language_servers_honors_cmd_inherited_from_wildcard() {
        let mut settings = settings_with(HashMap::new());
        settings.language_servers = HashMap::from([
            ("_".to_string(), server(&["shared-ls", "--stdio"])),
            ("inherits-cmd".to_string(), server(&[])),
        ]);

        assert!(
            unspawnable_language_servers(&settings).is_empty(),
            "cmd supplied by the '_' wildcard makes the entry spawnable"
        );
    }

    #[test]
    fn unspawnable_language_servers_excludes_deliberately_disabled_servers() {
        // A server with `enabled: false` is a deliberate user opt-out, not a
        // misconfiguration — it must not be flagged alongside genuinely
        // broken (empty-cmd) entries.
        let mut settings = settings_with(HashMap::new());
        let mut disabled = server(&["lua-language-server"]);
        disabled.enabled = Some(false);
        settings.language_servers = HashMap::from([
            ("_".to_string(), server(&[])),
            ("disabled".to_string(), disabled),
            ("broken".to_string(), server(&[])),
        ]);

        assert_eq!(
            unspawnable_language_servers(&settings),
            vec!["broken".to_string()],
            "a deliberately disabled server is not an unspawnable misconfiguration"
        );
    }

    #[test]
    fn unspawnable_language_servers_excludes_disabled_server_with_empty_cmd() {
        // The meaningful case: a server disabled AND left with no cmd (e.g.
        // a placeholder entry the user hasn't filled in yet, or one they
        // turned off without deleting). Must not be flagged either.
        let mut settings = settings_with(HashMap::new());
        let mut disabled = server(&[]);
        disabled.enabled = Some(false);
        settings.language_servers = HashMap::from([
            ("_".to_string(), server(&[])),
            ("disabled".to_string(), disabled),
            ("broken".to_string(), server(&[])),
        ]);

        assert_eq!(
            unspawnable_language_servers(&settings),
            vec!["broken".to_string()],
            "a disabled server with an empty cmd is an intentional no-op, not a misconfiguration"
        );
    }

    #[test]
    fn format_unspawnable_servers_warning_lists_names_or_is_silent() {
        assert_eq!(format_unspawnable_servers_warning(&[]), None);
        let msg = format_unspawnable_servers_warning(&["broken".to_string()])
            .expect("non-empty list warns");
        assert!(msg.contains("broken"), "warning names the server: {msg}");
        assert!(msg.contains("cmd"), "warning explains the cause: {msg}");
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
                ..Default::default()
            },
        );
        BridgeLanguageConfig {
            enabled: None,
            aggregation: Some(agg),
        }
    }

    // ==========================================================================
    // is_empty_layer_value (host-layer fan-in emptiness)
    // ==========================================================================

    #[test]
    fn empty_layer_value_recognizes_null_and_empty_array() {
        assert!(is_empty_layer_value(&serde_json::Value::Null));
        assert!(is_empty_layer_value(&serde_json::json!([])));
        assert!(!is_empty_layer_value(&serde_json::json!([1])));
    }

    #[test]
    fn empty_layer_value_recognizes_object_shaped_empties() {
        // CompletionList / SignatureHelp / LinkedEditingRanges with empty
        // canonical lists are "empty but valid" — they must not win the
        // host fan-in over a server with actual results.
        assert!(is_empty_layer_value(&serde_json::json!({
            "isIncomplete": false, "items": []
        })));
        assert!(is_empty_layer_value(
            &serde_json::json!({ "signatures": [] })
        ));
        assert!(is_empty_layer_value(&serde_json::json!({ "ranges": [] })));
        assert!(!is_empty_layer_value(&serde_json::json!({
            "isIncomplete": false, "items": [{ "label": "x" }]
        })));
    }

    #[test]
    fn empty_layer_value_treats_other_objects_as_results() {
        // Hover and friends: object results without a canonical list field
        // count as results.
        assert!(!is_empty_layer_value(&serde_json::json!({
            "contents": "docs"
        })));
    }

    // ==========================================================================
    // race_layers_preferred (cross-layer-aggregation, parallel fan-out)
    // ==========================================================================

    const VHN: &[LayerSource] = &[LayerSource::Virt, LayerSource::Host, LayerSource::Native];

    fn ok<R>(value: Option<R>) -> tower_lsp_server::jsonrpc::Result<Option<R>> {
        Ok(value)
    }

    #[tokio::test]
    async fn race_prefers_higher_layer_even_when_lower_arrives_first() {
        // Host completes instantly with a result; virt completes later but
        // non-empty. Priority decides, not arrival order.
        let virt = async {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            ok(Some("virt"))
        };
        let host = async { ok(Some("host")) };
        let won = race_layers_preferred(VHN, virt, host, async { ok(None) }, |_| true)
            .await
            .unwrap();
        assert_eq!(won, Some("virt"));
    }

    #[tokio::test]
    async fn race_falls_back_to_host_when_virt_is_empty() {
        let virt = async { ok(None) };
        let host = async { ok(Some("host")) };
        let won = race_layers_preferred(VHN, virt, host, async { ok(None) }, |_| true)
            .await
            .unwrap();
        assert_eq!(won, Some("host"));
    }

    #[tokio::test]
    async fn race_polls_layers_concurrently_not_serially() {
        // The virt future only completes AFTER the host future completed
        // (oneshot handshake). A serial walk (virt awaited to completion
        // before host is even polled) would deadlock here; the concurrent
        // race completes promptly.
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let virt = async move {
            let _ = rx.await;
            ok(Some("virt"))
        };
        let host = async move {
            let _ = tx.send(());
            // Stay pending long enough that the race must keep polling virt.
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            ok(Some("host"))
        };
        let won = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            race_layers_preferred(VHN, virt, host, async { ok(None) }, |_| true),
        )
        .await
        .expect("layers must fan out concurrently — a serial walk deadlocks this handshake")
        .unwrap();
        assert_eq!(won, Some("virt"), "priority still decides");
    }

    #[tokio::test]
    async fn race_never_polls_layers_absent_from_order() {
        // An allowlist without virt must not poll the virt future at all.
        let virt = async {
            panic!("virt must not be polled when absent from order");
            #[allow(unreachable_code)]
            ok(Some("virt"))
        };
        let host = async { ok(Some("host")) };
        let won =
            race_layers_preferred(&[LayerSource::Host], virt, host, async { ok(None) }, |_| {
                true
            })
            .await
            .unwrap();
        assert_eq!(won, Some("host"));
    }

    #[tokio::test]
    async fn race_returns_none_when_all_layers_are_empty() {
        let virt = async { ok(None) };
        let host = async { ok(None) };
        let won: Option<&str> =
            race_layers_preferred(VHN, virt, host, async { ok(None) }, |_| true)
                .await
                .unwrap();
        assert_eq!(won, None);
    }

    #[tokio::test]
    async fn race_treats_empty_results_via_is_nonempty() {
        // A Some result that the emptiness check rejects falls through.
        let virt = async { ok(Some(Vec::<i32>::new())) };
        let host = async { ok(Some(vec![1])) };
        let won = race_layers_preferred(VHN, virt, host, async { ok(None) }, |v: &Vec<i32>| {
            !v.is_empty()
        })
        .await
        .unwrap();
        assert_eq!(won, Some(vec![1]));
    }

    #[tokio::test]
    async fn race_falls_through_to_native_when_bridge_layers_are_empty() {
        // The lexical-name-resolution contributor answers when no bridge
        // layer produced anything.
        let virt = async { ok(None) };
        let host = async { ok(None) };
        let native = async { ok(Some("native")) };
        let won = race_layers_preferred(VHN, virt, host, native, |_| true)
            .await
            .unwrap();
        assert_eq!(won, Some("native"));
    }

    #[tokio::test]
    async fn race_prefers_bridge_layers_over_native() {
        // The bridge stays the accuracy path: a non-empty host beats a
        // completed native result under the default [virt, host, native].
        let virt = async { ok(None) };
        let host = async { ok(Some("host")) };
        let native = async { ok(Some("native")) };
        let won = race_layers_preferred(VHN, virt, host, native, |_| true)
            .await
            .unwrap();
        assert_eq!(won, Some("host"));
    }

    #[tokio::test]
    async fn race_propagates_errors() {
        let virt =
            async { Err::<Option<&str>, _>(tower_lsp_server::jsonrpc::Error::request_cancelled()) };
        let host = async { ok(Some("host")) };
        let result = race_layers_preferred(VHN, virt, host, async { ok(None) }, |_| true).await;
        assert!(result.is_err(), "a layer error must propagate");
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
            resolved.priorities,
            vec![LayerSource::Virt, LayerSource::Host, LayerSource::Native]
        );
    }

    #[test]
    fn resolve_layer_config_respects_language_entry() {
        let mut langs = HashMap::new();
        langs.insert(
            "markdown".to_string(),
            LanguageSettings {
                layers: Some(crate::config::settings::LayersConfig {
                    aggregation: Some(HashMap::from([(
                        "textDocument/hover".to_string(),
                        crate::config::settings::LayerAggregationConfig {
                            priorities: Some(vec![LayerSource::Native]),
                            strategy: None,
                        },
                    )])),
                }),
                ..Default::default()
            },
        );
        let settings = settings_with(langs);

        let hover = resolve_layer_config_from_settings(&settings, "markdown", "textDocument/hover");
        assert_eq!(hover.priorities, vec![LayerSource::Native]);
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
                layers: Some(crate::config::settings::LayersConfig {
                    aggregation: Some(HashMap::from([(
                        "_".to_string(),
                        crate::config::settings::LayerAggregationConfig {
                            priorities: Some(vec![]),
                            strategy: None,
                        },
                    )])),
                }),
                ..Default::default()
            },
        );
        let settings = settings_with(langs);
        let resolved =
            resolve_layer_config_from_settings(&settings, "markdown", "textDocument/hover");
        assert!(
            resolved.priorities.is_empty(),
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
                ..Default::default()
            },
        );
        agg.insert(
            "textDocument/formatting".to_string(),
            AggregationConfig {
                priorities: None,
                strategy: Some(AggregationStrategy::Preferred),
                max_fan_out: None,
                ..Default::default()
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
                ..Default::default()
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
                ..Default::default()
            },
        );
        agg.insert(
            "_".to_string(),
            AggregationConfig {
                priorities: Some(vec!["black".to_string()]),
                strategy: None,
                max_fan_out: None,
                ..Default::default()
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
                ..Default::default()
            },
        );
        let mut wildcard_agg = HashMap::new();
        wildcard_agg.insert(
            "textDocument/formatting".to_string(),
            AggregationConfig {
                priorities: Some(vec!["black".to_string()]),
                strategy: None,
                max_fan_out: None,
                ..Default::default()
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
                ..Default::default()
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
                ..Default::default()
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

    /// The capability prefilter is exempt for the methods whose capability-absent
    /// handler branch is NOT empty because it falls back to a different
    /// capability: `prepareRename` (→ `rename`'s `DefaultBehavior`) and
    /// `formatting`/`rangeFormatting` (→ the other formatting kind via the
    /// Concatenated pipeline). Dropping a server by the requested capability
    /// alone would discard one that would have answered via the fallback. All
    /// other routed methods are prefiltered.
    #[test]
    fn capability_prefilter_exempts_fallback_methods() {
        for exempt in [
            "textDocument/prepareRename",
            "textDocument/formatting",
            "textDocument/rangeFormatting",
        ] {
            assert!(
                !capability_prefilter_applies(exempt),
                "{exempt} must be exempt"
            );
        }
        for method in [
            "textDocument/hover",
            "textDocument/definition",
            "textDocument/completion",
            "textDocument/references",
            "textDocument/rename",
            "textDocument/documentColor",
            "textDocument/documentSymbol",
            "textDocument/foldingRange",
            "textDocument/diagnostic",
        ] {
            assert!(
                capability_prefilter_applies(method),
                "{method} must be prefiltered"
            );
        }
    }
}
