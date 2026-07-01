use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

pub(crate) type CaptureMapping = HashMap<String, String>;

/// Reserved key in the `bridge` map for the host language acting as its own
/// bridge target (host-document-bridge): requests for the host document are
/// forwarded with the real URI and no coordinate translation.
pub(crate) const HOST_BRIDGE_KEY: &str = "_self";

/// Reserved `priorities` element standing for "every configured server not
/// named elsewhere in the list" (aggregation-priorities-wildcard).
///
/// Glob-like `*` rather than the `_` wildcard sigil: `_` keys carry
/// field-level inheritance semantics, a rest-of-the-candidates list element
/// does not.
pub(crate) const PRIORITIES_WILDCARD: &str = "*";

/// The resolved default for an absent `priorities`: `["*"]`, i.e. fan out to
/// every configured server with no ranking (first-win).
fn default_priorities() -> Vec<String> {
    vec![PRIORITIES_WILDCARD.to_string()]
}

/// Aggregation strategy for combining results from multiple bridge servers.
///
/// - `Preferred`: Use the first non-empty response (priority-ordered).
///   This is the default for most LSP methods.
/// - `Concatenated`: Collect and merge responses from all servers.
///   This is the default for `textDocument/diagnostic`.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum AggregationStrategy {
    Preferred,
    Concatenated,
}

/// Per-method aggregation configuration.
///
/// Controls how results from multiple bridge servers are aggregated for a
/// specific LSP method. The `priorities` list is an **ordered allowlist**
/// (aggregation-priorities-wildcard): listed servers run in order, servers
/// absent from the list do not run, and a `"*"` element stands for every
/// configured-but-unlisted server (a first-win group at that position).
/// `None` means "inherit from wildcard/base", resolving to `["*"]` when
/// nothing is configured; `Some(vec![])` disables fan-out for the method.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AggregationConfig {
    /// Server names in priority order (highest first), as an ordered
    /// allowlist. `"*"` = all unlisted servers; `Some(vec![])` = disable
    /// fan-out for this method; `None` = inherit (default `["*"]`).
    #[serde(default)]
    pub priorities: Option<Vec<String>>,
    /// Aggregation strategy override.
    /// Omit to inherit from wildcard/base configs; if none is resolved, the
    /// handler default strategy is used.
    #[serde(default)]
    pub strategy: Option<AggregationStrategy>,
    /// Maximum number of servers to fan out to.
    ///
    /// - `None` / absent: no limit (fan out to all matching servers)
    /// - `0`: disable fan-out entirely
    /// - Positive: cap the number of concurrent server requests
    /// - Negative: treated as no limit (silently ignored via `usize::try_from`)
    #[serde(default)]
    pub max_fan_out: Option<i64>,
    /// Whether **pull-driven** servers (those advertising
    /// `diagnosticProvider`) are pulled on host events and merged into the
    /// proactive `publishDiagnostics` cache, so they too publish proactively
    /// (push-propagation-diagnostic-forwarding "Per-server source and
    /// fallback"). Belongs in the `textDocument/publishDiagnostics` (Path A)
    /// method block.
    ///
    /// `None` = inherit (default `true`). `false` drops pull-driven servers
    /// from the proactive path entirely; their *spontaneous* pushes are still
    /// cached and published (it only stops kakehashi from pulling them).
    #[serde(default)]
    pub pull_fallback: Option<bool>,
    /// Whether **push-driven** servers' (those *not* advertising
    /// `diagnosticProvider`) cached pushes are folded into the client-pull
    /// `textDocument/diagnostic` response (push-propagation-diagnostic-forwarding
    /// "Per-server source and fallback"). Belongs in the
    /// `textDocument/diagnostic` (Path B) method block.
    ///
    /// `None` = inherit (default `true`). `false` answers a client pull from
    /// live pull-driven servers only, ignoring cached pushes.
    #[serde(default)]
    pub push_fallback: Option<bool>,
}

/// Configuration for a single bridged language within a host filetype.
///
/// Used in the bridge filter map to control whether a specific injection language
/// should be bridged and how results are aggregated.
/// Example: `{ python = { enabled = true, aggregation = { "_" = { priorities = ["pyright"] } } } }`.
///
/// Omitted fields inherit from the wildcard `_` entry (see wildcard-config-inheritance).
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, Default, JsonSchema)]
pub struct BridgeLanguageConfig {
    /// Whether bridging is enabled for this language.
    /// Omit to inherit from wildcard (defaults to true).
    pub enabled: Option<bool>,
    /// Per-method aggregation config. Key = LSP method name or "_" for default.
    pub aggregation: Option<HashMap<String, AggregationConfig>>,
}

/// One result layer that can answer an LSP request
/// (cross-layer-aggregation). NOT injection nesting depth (tree-sitter
/// "language layers").
///
/// The set is closed and three-valued, so layer order is expressed by
/// explicit enumeration — no `"*"` element exists on this axis.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, Hash, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum LayerSource {
    /// kakehashi's own features (Tree-sitter based).
    Native,
    /// The host-document bridge (`bridge._self`, host-document-bridge):
    /// servers handling the host document itself with the real URI. An
    /// empty contributor until opted in via `bridge._self.enabled = true`.
    Host,
    /// The injection bridges (`bridge.<language>`).
    Virt,
}

/// Cross-layer configuration for one host language
/// (cross-layer-aggregation). Lives at `languages.<lang>.layers`.
///
/// The per-method map sits under `aggregation`, mirroring
/// `bridge.<key>.aggregation`: "aggregation" uniformly names the
/// method-keyed map across the config, and `layers` keeps headroom for
/// future layer-wide fields without colliding with method names.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct LayersConfig {
    /// Per-method cross-layer aggregation. Key = LSP method name (e.g.,
    /// `textDocument/formatting`) or `"_"` for the method wildcard.
    #[serde(default)]
    pub aggregation: Option<HashMap<String, LayerAggregationConfig>>,
}

/// Per-method cross-layer aggregation configuration
/// (cross-layer-aggregation). Lives in `LayersConfig::aggregation`, keyed by
/// LSP method name or `"_"` for the method wildcard.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct LayerAggregationConfig {
    /// Ordered allowlist of result layers, highest priority first: layers
    /// omitted from the list do not participate for the method. `[]`
    /// disables the method across all layers. `None` = inherit (default
    /// `["virt", "host", "native"]` — innermost first).
    #[serde(default)]
    pub priorities: Option<Vec<LayerSource>>,
    /// Cross-layer combine strategy: `preferred` (first non-empty layer
    /// wins) or `concatenated`. Consumed by `textDocument/formatting`
    /// (default `concatenated`: the pipeline composes disjoint work across
    /// layers) and the diagnostics methods (default `concatenated`: virt
    /// region diagnostics and host-server diagnostics merge into one
    /// report). Every other method combines with `preferred` regardless.
    #[serde(default)]
    pub strategy: Option<AggregationStrategy>,
}

/// The built-in default layer priorities: virt, host, native — innermost first,
/// mirroring the "deeper wins" semantic-token convention.
fn default_layer_priorities() -> Vec<LayerSource> {
    vec![LayerSource::Virt, LayerSource::Host, LayerSource::Native]
}

/// Drop repeated layers, keeping the first occurrence (order preserved).
fn dedup_layer_priorities(priorities: Vec<LayerSource>) -> Vec<LayerSource> {
    let mut seen = std::collections::HashSet::new();
    priorities
        .into_iter()
        .filter(|layer| seen.insert(*layer))
        .collect()
}

/// Fully resolved cross-layer settings for a single LSP method.
///
/// Produced by [`LanguageSettings::resolve_layers`]; all optional fields are
/// resolved with their defaults.
#[derive(Debug, Clone)]
pub(crate) struct ResolvedLayerConfig {
    pub(crate) priorities: Vec<LayerSource>,
    pub(crate) strategy: AggregationStrategy,
}

impl ResolvedLayerConfig {
    /// Defaults for a method with no `layers` configuration: the built-in
    /// order and the per-method strategy default.
    pub(crate) fn with_defaults(method: &str) -> Self {
        Self {
            priorities: default_layer_priorities(),
            strategy: default_layer_strategy_for_method(method),
        }
    }

    /// Whether the given layer participates for this method.
    pub(crate) fn allows(&self, layer: LayerSource) -> bool {
        self.priorities.contains(&layer)
    }
}

/// Fully resolved aggregation settings for a single LSP method.
///
/// Produced by [`BridgeLanguageConfig::resolve_aggregation`]. Unlike
/// [`AggregationConfig`] (the raw TOML struct), all optional fields are
/// resolved with their defaults.
#[derive(Debug, Clone)]
pub(crate) struct ResolvedAggregationConfig {
    pub(crate) strategy: AggregationStrategy,
    pub(crate) priorities: Vec<String>,
    pub(crate) max_fan_out: Option<usize>,
    /// Resolved `pullFallback` (default `true`): whether pull-driven servers are
    /// pulled on host events into the proactive cache. Only meaningful for the
    /// `textDocument/publishDiagnostics` method (Path A).
    pub(crate) pull_fallback: bool,
    /// Resolved `pushFallback` (default `true`): whether push-driven servers'
    /// cached pushes are folded into the client-pull response. Only meaningful
    /// for the `textDocument/diagnostic` method (Path B).
    pub(crate) push_fallback: bool,
}

/// The resolved default for an absent `pullFallback` / `pushFallback`: both
/// fallback channels are on, so every server contributes to both diagnostic
/// paths regardless of which single mechanism it natively supports.
const DEFAULT_DIAGNOSTIC_FALLBACK: bool = true;

impl ResolvedAggregationConfig {
    /// Create a config with all fields at their defaults (`Preferred`
    /// strategy, `["*"]` priorities = all servers, first-win, fallbacks on).
    pub(crate) fn with_defaults() -> Self {
        Self {
            strategy: AggregationStrategy::Preferred,
            priorities: default_priorities(),
            max_fan_out: None,
            pull_fallback: DEFAULT_DIAGNOSTIC_FALLBACK,
            push_fallback: DEFAULT_DIAGNOSTIC_FALLBACK,
        }
    }
}

fn default_aggregation_strategy_for_method(method: &str) -> AggregationStrategy {
    match method {
        "textDocument/diagnostic" | "textDocument/publishDiagnostics" => {
            AggregationStrategy::Concatenated
        }
        _ => AggregationStrategy::Preferred,
    }
}

/// Layer-level default strategy (cross-layer-aggregation): formatting is
/// `concatenated` in addition to the diagnostics defaults, because the
/// cross-layer formatting pipeline composes disjoint work (virt formats
/// injection regions, host formats the resulting text). At the bridge level
/// the same method stays `preferred` — there, multiple servers of one
/// target produce *competing* whole-document edits.
fn default_layer_strategy_for_method(method: &str) -> AggregationStrategy {
    match method {
        "textDocument/formatting" => AggregationStrategy::Concatenated,
        _ => default_aggregation_strategy_for_method(method),
    }
}

impl BridgeLanguageConfig {
    /// Look up the aggregation entry for a method with field-level wildcard merge.
    ///
    /// Uses [`crate::config::resolve_with_wildcard`] so that a method-specific entry
    /// inherits any unset fields from the `_` wildcard entry (wildcard-config-inheritance).
    fn resolve_aggregation_entry(&self, method: &str) -> Option<AggregationConfig> {
        let map = self.aggregation.as_ref()?;
        crate::config::resolve_with_wildcard(map, method, crate::config::merge_aggregation_configs)
    }

    /// Resolve all aggregation settings for a specific LSP method in a single call.
    ///
    /// Performs one config lookup and extracts strategy, priorities, and max_fan_out
    /// together, avoiding redundant resolution when all three are needed.
    ///
    /// Fallback rules for `strategy`:
    /// - No matching aggregation entry: method default from
    ///   [`default_aggregation_strategy_for_method`]
    /// - Matching entry with `strategy = None`: method default from
    ///   [`default_aggregation_strategy_for_method`] (`Concatenated` for diagnostics,
    ///   otherwise `Preferred`)
    /// - Matching entry with explicit `strategy`: use that value
    pub(crate) fn resolve_aggregation(&self, method: &str) -> ResolvedAggregationConfig {
        match self.resolve_aggregation_entry(method) {
            Some(entry) => ResolvedAggregationConfig {
                strategy: entry
                    .strategy
                    .unwrap_or_else(|| default_aggregation_strategy_for_method(method)),
                // None (unset after merge) resolves to ["*"] — all servers,
                // first-win. An explicit [] survives as the per-method
                // fan-out kill switch (aggregation-priorities-wildcard).
                priorities: entry.priorities.unwrap_or_else(default_priorities),
                max_fan_out: entry.max_fan_out.and_then(|raw| usize::try_from(raw).ok()),
                pull_fallback: entry.pull_fallback.unwrap_or(DEFAULT_DIAGNOSTIC_FALLBACK),
                push_fallback: entry.push_fallback.unwrap_or(DEFAULT_DIAGNOSTIC_FALLBACK),
            },
            None => ResolvedAggregationConfig {
                strategy: default_aggregation_strategy_for_method(method),
                priorities: default_priorities(),
                max_fan_out: None,
                pull_fallback: DEFAULT_DIAGNOSTIC_FALLBACK,
                push_fallback: DEFAULT_DIAGNOSTIC_FALLBACK,
            },
        }
    }
}

/// A single `workspaceMarkers` entry, mirroring Neovim's `vim.fs.root`
/// `(string|string[])[]` shape: a bare string is its own priority tier; a
/// nested array is an equal-priority group. Entries are tried in list order
/// (earlier wins); within a group all names are equal and the nearest
/// ancestor containing any of them decides the root.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, JsonSchema)]
#[serde(untagged)]
pub enum RootMarker {
    /// A single marker name forming its own priority tier.
    Single(String),
    /// An equal-priority group; proximity decides among its names.
    Group(Vec<String>),
}

impl RootMarker {
    /// The marker names this entry tests, regardless of single/group shape.
    pub(crate) fn names(&self) -> &[String] {
        match self {
            RootMarker::Single(name) => std::slice::from_ref(name),
            RootMarker::Group(names) => names.as_slice(),
        }
    }
}

/// Configuration for a bridge language server.
///
/// This is used to configure external language servers (like rust-analyzer, pyright)
/// that kakehashi can redirect requests to for injection regions.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct BridgeServerConfig {
    /// Command array: first element is the program, rest are arguments
    /// e.g., ["rust-analyzer"] or ["pyright-langserver", "--stdio"].
    /// Optional so a wildcard `_` entry can carry defaults only; a concrete
    /// server whose resolved cmd is still empty is skipped at lookup.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cmd: Vec<String>,
    /// Languages this server handles (e.g., ["rust"], ["python"]).
    /// Optional for the same wildcard-defaults reason as `cmd`; a server
    /// with no languages never matches a lookup.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub languages: Vec<String>,
    /// Optional initialization options to pass to the server during initialize
    pub initialization_options: Option<Value>,
    /// Optional workspace settings for the server, propagated *after*
    /// initialize: answered to the server's `workspace/configuration` pulls and
    /// pushed via `workspace/didChangeConfiguration` (downstream-settings-propagation).
    ///
    /// Distinct from `initialization_options`, which is consumed only at
    /// `initialize` time: a runtime change to `settings` reaches a running
    /// server, whereas an `initialization_options` change needs a restart.
    ///
    /// Treated as the editor-style settings root: a downstream
    /// `workspace/configuration` item's `section` (e.g. `"rust-analyzer.cargo"`)
    /// is resolved as a dotted path into this object. The contents are opaque to
    /// kakehashi (passthrough), exactly like `initialization_options`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settings: Option<Value>,
    /// Marker files/directories that locate the workspace root for this
    /// server, mirroring Neovim's `vim.fs.root` `(string|string[])[]` shape:
    /// entries are tried in list order (earlier = higher priority) and each
    /// entry is searched up the document's ancestors nearest-first; a nested
    /// array is an equal-priority group where the nearest ancestor containing
    /// any of its names wins. The first matching entry's directory becomes the
    /// server's rootUri.
    /// `None` = inherit (built-in default `[".git"]`); an explicit `[]`
    /// disables the search (the client-supplied root is forwarded as-is).
    /// When no marker matches, the client-supplied root is the fallback.
    ///
    /// The wire key is `workspaceMarkers`; the pre-rename key `rootMarkers` is
    /// kept as a deprecated serde alias for backward compatibility.
    #[serde(alias = "rootMarkers")]
    pub workspace_markers: Option<Vec<RootMarker>>,
    /// Trigger characters for bridged `textDocument/onTypeFormatting` (#354).
    ///
    /// kakehashi cannot know downstream trigger characters at initialize time
    /// (servers spawn lazily), so users declare them here. The union across
    /// all servers is advertised as `documentOnTypeFormattingProvider`; a
    /// request is forwarded to a downstream server only when that server's
    /// own capabilities also declare the typed character. Unset or empty
    /// everywhere → the capability is not advertised (today's behavior).
    ///
    /// Note: this field feeds *only* the `initialize`-time capability
    /// advertisement; it is never consulted when forwarding a request (that
    /// filter uses the downstream server's own declared triggers). The
    /// capability is negotiated once and frozen for the session, so updating
    /// the triggers via `didChangeConfiguration` has no runtime effect at all
    /// — neither re-advertising the capability nor changing forwarding. A
    /// restart is required for new triggers to take effect.
    pub on_type_formatting_triggers: Option<Vec<String>>,
    /// Prefer reusing **one** downstream process across every workspace root for
    /// this server, instead of spawning a separate process per marker root
    /// (issue #391). It is a *preference*, honored only when the downstream
    /// server advertises `workspace.workspaceFolders.{supported,
    /// changeNotifications}`: when it does, kakehashi routes all roots to a
    /// single connection and announces each new root with
    /// `workspace/didChangeWorkspaceFolders`; when it does not, kakehashi logs
    /// once and silently falls back to the per-root-instance model. That
    /// universal fallback makes a blanket `languageServers._` opt-in safe.
    ///
    /// `None` = inherit (built-in default `false` = per-root instances). Like
    /// `workspace_markers`, a concrete server's explicit value overrides the
    /// wildcard, so `_.preferSharedInstance: true` can be opted out of
    /// per server with `preferSharedInstance: false`.
    pub prefer_shared_instance: Option<bool>,
    /// Whether this server is eligible to be spawned/used at all.
    ///
    /// `None` = inherit (built-in default `true`). Like `root_markers` and
    /// `prefer_shared_instance`, a concrete server's explicit value overrides
    /// the wildcard, so `_.enabled: false` can disable every server by
    /// default while individual servers opt back in with `enabled: true`.
    pub enabled: Option<bool>,
}

impl BridgeServerConfig {
    /// Effective `prefer_shared_instance` preference, resolving the inherit
    /// (`None`) case to the built-in default `false` (per-root instances).
    pub(crate) fn prefers_shared_instance(&self) -> bool {
        self.prefer_shared_instance.unwrap_or(false)
    }

    /// Effective `enabled` state, resolving the inherit (`None`) case to the
    /// built-in default `true`.
    pub(crate) fn is_enabled(&self) -> bool {
        self.enabled.unwrap_or(true)
    }

    /// A resolved config is a real, usable server: it has a command to run
    /// AND it hasn't been disabled. Every "is this server usable" call site
    /// (spawn-resolution, capability advertisement, resolve-dispatch) should
    /// share this single predicate rather than re-deriving it.
    pub(crate) fn is_spawnable(&self) -> bool {
        !self.cmd.is_empty() && self.is_enabled()
    }
}

/// Union of every server's `onTypeFormattingTriggers`, shaped for the LSP
/// `documentOnTypeFormattingProvider` capability: `(first, more)`.
///
/// Sorted and deduplicated so the advertisement is deterministic regardless of
/// `HashMap` iteration order. Returns `None` when no server declares any
/// trigger (capability stays unadvertised).
///
/// Every concrete server key is independently resolved through
/// wildcard-config-inheritance before its triggers are read, so a server that
/// overrides the wildcard's triggers (including with an explicit empty list)
/// or its `enabled` state contributes its own *effective*, merged set — not
/// the wildcard's raw one. The `_` wildcard key itself is never a direct
/// contributor: it's excluded from iteration, since a wildcard-only config
/// (no concrete servers) has nothing to advertise on its own. A resolved
/// config that isn't [`BridgeServerConfig::is_spawnable`] (no cmd, or
/// disabled) is excluded entirely: advertising its triggers would make the
/// client send an `onTypeFormatting` *request* — not a fire-and-forget
/// notification — on every matching keystroke, only to resolve to null.
pub(crate) fn on_type_formatting_trigger_union(
    servers: &HashMap<String, BridgeServerConfig>,
) -> Option<(String, Vec<String>)> {
    let mut triggers: Vec<String> = servers
        .keys()
        .filter(|name| name.as_str() != crate::config::WILDCARD_KEY)
        .filter_map(|name| {
            crate::config::resolve_with_wildcard(
                servers,
                name,
                crate::config::merge_bridge_server_configs,
            )
        })
        .filter(BridgeServerConfig::is_spawnable)
        .filter_map(|s| s.on_type_formatting_triggers)
        .flatten()
        .filter(|t| t.chars().count() == 1)
        .collect();
    triggers.sort();
    triggers.dedup();
    let mut iter = triggers.into_iter();
    let first = iter.next()?;
    Some((first, iter.collect()))
}

/// Custom mappings from Tree-sitter capture names to semantic token types, per query kind.
#[derive(Debug, Clone, Deserialize, Serialize, Default, PartialEq, Eq, JsonSchema)]
pub struct QueryTypeMappings {
    /// Capture mappings for highlights queries.
    #[serde(default)]
    pub highlights: CaptureMapping,
    /// Capture mappings for locals queries.
    #[serde(default)]
    pub locals: CaptureMapping,
    /// Capture mappings for folds queries.
    /// Reserved for future folding range support — populated and merged but not yet consumed by analysis.
    #[serde(default)]
    pub folds: CaptureMapping,
}

pub type CaptureMappings = HashMap<String, QueryTypeMappings>;

/// Query type for tree-sitter query files.
///
/// Used in the unified `queries` field to specify what kind of query a file contains.
/// When not specified, the kind is inferred from the filename pattern.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum QueryKind {
    /// Syntax highlighting queries
    Highlights,
    /// Local definitions/references queries (for scope analysis)
    Locals,
    /// Language injection queries (for embedded languages)
    Injections,
}

impl QueryKind {
    /// All query kinds in standard processing order.
    pub const ALL: [QueryKind; 3] = [
        QueryKind::Highlights,
        QueryKind::Locals,
        QueryKind::Injections,
    ];

    /// The lowercase name used in filenames and log messages (e.g. `"highlights"`).
    pub fn name(self) -> &'static str {
        match self {
            QueryKind::Highlights => "highlights",
            QueryKind::Locals => "locals",
            QueryKind::Injections => "injections",
        }
    }

    /// The query filename (e.g. `"highlights.scm"`).
    pub fn filename(self) -> &'static str {
        match self {
            QueryKind::Highlights => "highlights.scm",
            QueryKind::Locals => "locals.scm",
            QueryKind::Injections => "injections.scm",
        }
    }
}

/// A single query file configuration entry.
///
/// Used in the unified `queries` array to specify query files with optional type.
/// Example: `{ path = "./highlights.scm", kind = "highlights" }`
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, JsonSchema)]
pub struct QueryItem {
    /// Path to the query file (required)
    pub path: String,
    /// Query type: highlights, locals, or injections (optional - inferred from filename if omitted)
    pub kind: Option<QueryKind>,
}

/// Infer the query kind from a file path based on filename patterns.
///
/// Rules:
/// - Exact match `highlights.scm` -> `Some(Highlights)`
/// - Exact match `locals.scm` -> `Some(Locals)`
/// - Exact match `injections.scm` -> `Some(Injections)`
/// - Otherwise -> `None`
///
/// Examples:
/// - `injections.scm` -> matches
/// - `rust-injections.scm` -> does NOT match (only exact filename matches)
/// - `local-injections.scm` -> does NOT match (only exact filename matches)
pub(crate) fn infer_query_kind(path: &str) -> Option<QueryKind> {
    // Extract filename from path using std::path for cross-platform support
    let filename = std::path::Path::new(path)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(path);

    // Only match exact filenames
    match filename {
        "injections.scm" => Some(QueryKind::Injections),
        "locals.scm" => Some(QueryKind::Locals),
        "highlights.scm" => Some(QueryKind::Highlights),
        _ => None,
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RawWorkspaceSettings {
    /// Directories to search for Tree-sitter parser libraries and query files.
    pub search_paths: Option<Vec<String>>,
    /// Per-language configuration (parser paths, queries, bridge filters, base inheritance).
    #[serde(default)]
    pub languages: HashMap<String, LanguageSettings>,
    /// Custom mappings from Tree-sitter capture names to semantic token types.
    #[serde(default)]
    pub capture_mappings: CaptureMappings,
    /// Whether to automatically install missing parsers and queries.
    pub auto_install: Option<bool>,
    /// Debounce delay, in milliseconds, between a `didChange` and the diagnostic
    /// pull it triggers. Higher values cut refresh/pull volume during rapid typing
    /// at the cost of latency. Defaults to [`DEFAULT_DEBOUNCE_MS`] when unset.
    pub diagnostics_debounce_ms: Option<u64>,
    /// Language servers for bridging LSP requests to injection regions.
    /// Map of server name to server configuration.
    pub language_servers: Option<HashMap<String, BridgeServerConfig>>,
}

/// Generate JSON Schema for the workspace configuration.
pub fn json_schema() -> schemars::Schema {
    schemars::schema_for!(RawWorkspaceSettings)
}

// Domain types - used throughout the application and also exposed in JSON Schema
// via RawWorkspaceSettings.

/// Per-language Tree-sitter configuration.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
pub struct LanguageSettings {
    /// Base language to inherit unfilled fields from (most-specific-wins).
    /// E.g., `base = "markdown"` on `rmd` means rmd inherits markdown's
    /// parser/queries/bridge for fields rmd does not set itself.
    pub base: Option<String>,
    /// Path to the parser library (.so/.dylib/.dll)
    pub parser: Option<String>,
    /// Omit to inherit from wildcard/defaults. Use an empty array `[]` to explicitly clear queries.
    pub queries: Option<Vec<QueryItem>>,
    /// Omit to bridge all configured languages (default). Use an empty object `{}` to disable bridging. Use `{ "python": { "enabled": true } }` to bridge specific languages.
    pub bridge: Option<HashMap<String, BridgeLanguageConfig>>,
    /// Cross-layer configuration (cross-layer-aggregation): the
    /// `aggregation` map under it orders the result layers
    /// (`virt`/`host`/`native`) per LSP method (`"_"` = method wildcard).
    /// Omit to use the default order `["virt", "host", "native"]`.
    pub layers: Option<LayersConfig>,
    /// Deprecated: use `base` on the derived language instead.
    /// Alternative languageId values that should use this parser.
    pub aliases: Option<Vec<String>>,
}

impl LanguageSettings {
    /// Resolve the cross-layer settings for an LSP method
    /// (cross-layer-aggregation), with field-level wildcard merge over the
    /// `"_"` method entry — the same machinery as
    /// [`BridgeLanguageConfig::resolve_aggregation`].
    ///
    /// `priorities` is a single field: a method-specific `priorities`
    /// replaces the wildcard's list wholesale, it does not merge
    /// element-wise.
    pub(crate) fn resolve_layers(&self, method: &str) -> ResolvedLayerConfig {
        let entry = self
            .layers
            .as_ref()
            .and_then(|layers| layers.aggregation.as_ref())
            .and_then(|map| {
                crate::config::resolve_with_wildcard(
                    map,
                    method,
                    crate::config::merge_layer_aggregation_configs,
                )
            });
        match entry {
            Some(cfg) => ResolvedLayerConfig {
                // Dedup defensively (first occurrence wins): a repeated layer
                // in user config would otherwise make downstream walks visit
                // it twice.
                priorities: dedup_layer_priorities(
                    cfg.priorities.unwrap_or_else(default_layer_priorities),
                ),
                strategy: cfg
                    .strategy
                    .unwrap_or_else(|| default_layer_strategy_for_method(method)),
            },
            None => ResolvedLayerConfig::with_defaults(method),
        }
    }

    /// Whether host bridging (`bridge._self`, host-document-bridge) is
    /// enabled for this language.
    ///
    /// Host bridging is **opt-in**: it requires an explicit
    /// `bridge._self.enabled = true`. Unlike injection entries, the `enabled`
    /// field deliberately does NOT inherit from the `_` wildcard entry —
    /// that implements the ADR's built-in `languages._.bridge._self.enabled
    /// = false` default, which must beat the wildcard's `enabled = true`
    /// virt default (key-specific wins). Aggregation fields DO inherit from
    /// `_` via [`Self::resolve_host_aggregation`].
    pub(crate) fn is_host_bridging_enabled(&self) -> bool {
        self.bridge
            .as_ref()
            .and_then(|map| map.get(HOST_BRIDGE_KEY))
            .and_then(|cfg| cfg.enabled)
            .unwrap_or(false)
    }

    /// Resolve the per-method aggregation config for the host bridge target
    /// (`bridge._self`), with the same field-level wildcard merge as any
    /// other bridge key: an unset `_self` field inherits from `_`.
    pub(crate) fn resolve_host_aggregation(&self, method: &str) -> ResolvedAggregationConfig {
        self.bridge
            .as_ref()
            .and_then(|map| {
                crate::config::resolve_with_wildcard(
                    map,
                    HOST_BRIDGE_KEY,
                    crate::config::merge_bridge_language_configs,
                )
            })
            .map(|cfg| cfg.resolve_aggregation(method))
            .unwrap_or_else(ResolvedAggregationConfig::with_defaults)
    }

    /// Check if a language is allowed for bridging based on the bridge filter.
    ///
    /// Returns:
    /// - `true` if `bridge` is `None` (default: bridge all languages)
    /// - `false` if `bridge` is `Some({})` (empty map: bridge nothing)
    /// - For non-empty maps: resolves wildcard `_` with specific key, then checks `enabled`
    ///   - `enabled: None` → defaults to `true` (bridging enabled by default)
    ///   - `enabled: Some(v)` → uses explicit value
    ///   - No wildcard and no specific key → `false` (not in map)
    pub fn is_language_bridgeable(&self, injection_language: &str) -> bool {
        match &self.bridge {
            None => true,                         // Default: bridge all configured languages
            Some(map) if map.is_empty() => false, // Empty map: bridge nothing
            Some(map) => {
                let resolved = crate::config::resolve_with_wildcard(
                    map,
                    injection_language,
                    crate::config::merge_bridge_language_configs,
                );
                resolved.map(|c| c.enabled.unwrap_or(true)).unwrap_or(false)
            }
        }
    }
}

/// Workspace-wide Tree-sitter configuration as required by the domain.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkspaceSettings {
    pub search_paths: Vec<String>,
    pub languages: HashMap<String, LanguageSettings>,
    pub capture_mappings: CaptureMappings,
    pub auto_install: bool,
    pub diagnostics_debounce_ms: u64,
    pub language_servers: HashMap<String, BridgeServerConfig>,
}

impl Default for WorkspaceSettings {
    fn default() -> Self {
        Self {
            search_paths: Vec::new(),
            languages: HashMap::new(),
            capture_mappings: CaptureMappings::default(),
            auto_install: true, // Default to true for zero-config experience
            diagnostics_debounce_ms: DEFAULT_DEBOUNCE_MS,
            language_servers: HashMap::new(),
        }
    }
}

/// Default `didChange`→diagnostic debounce, in milliseconds. The runtime default
/// (used when `diagnostics_debounce_ms` is unset) and the `config init` template
/// both resolve to this; [`crate::config::defaults`] asserts the mirror.
pub const DEFAULT_DEBOUNCE_MS: u64 = 500;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::WILDCARD_KEY;
    use rstest::rstest;

    #[test]
    fn test_json_schema_generation() {
        let schema = schemars::schema_for!(RawWorkspaceSettings);
        let json = serde_json::to_string_pretty(&schema).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();

        // Top-level properties should use camelCase (from serde renames)
        let props = value.get("properties").expect("should have properties");
        assert!(props.get("searchPaths").is_some(), "missing searchPaths");
        assert!(props.get("autoInstall").is_some(), "missing autoInstall");
        assert!(
            props.get("diagnosticsDebounceMs").is_some(),
            "missing diagnosticsDebounceMs"
        );
        assert!(
            props.get("captureMappings").is_some(),
            "missing captureMappings"
        );
        assert!(
            props.get("languageServers").is_some(),
            "missing languageServers"
        );

        // snake_case variants must NOT appear
        assert!(
            props.get("search_paths").is_none(),
            "snake_case leak: search_paths"
        );
        assert!(
            props.get("auto_install").is_none(),
            "snake_case leak: auto_install"
        );
        assert!(
            props.get("capture_mappings").is_none(),
            "snake_case leak: capture_mappings"
        );
        assert!(
            props.get("language_servers").is_none(),
            "snake_case leak: language_servers"
        );

        // $defs should contain key referenced types
        let defs = value.get("$defs").expect("should have $defs");
        for expected in [
            "LanguageSettings",
            "BridgeServerConfig",
            "BridgeLanguageConfig",
            "QueryItem",
            "AggregationConfig",
        ] {
            assert!(
                defs.get(expected).is_some(),
                "missing $defs entry: {expected}"
            );
        }
    }

    #[test]
    fn should_distinguish_between_unspecified_and_empty_queries() {
        // This test demonstrates the need for Option<Vec<QueryItem>>
        // to distinguish between "not specified" and "explicitly empty"
        // which is critical for merging logic in resolve_with_wildcard + merge_language_settings

        // Case 1: queries not specified (should be None)
        // User didn't specify queries - should inherit from wildcard/defaults
        let unspecified = LanguageSettings::default();
        assert!(
            unspecified.queries.is_none(),
            "Unspecified queries should be None"
        );

        // Case 2: queries explicitly empty (should be Some([]))
        // User explicitly set queries to empty - should override wildcard with empty list
        let explicitly_empty = LanguageSettings {
            queries: Some(vec![]),
            ..Default::default()
        };
        assert!(
            explicitly_empty.queries.is_some(),
            "Explicitly empty should be Some"
        );
        assert!(
            explicitly_empty.queries.as_ref().unwrap().is_empty(),
            "Should be empty vec"
        );

        // Case 3: queries with items
        let with_items = LanguageSettings {
            queries: Some(vec![QueryItem {
                path: "/path/to/highlights.scm".to_string(),
                kind: Some(QueryKind::Highlights),
            }]),
            ..Default::default()
        };
        assert!(with_items.queries.is_some());
        assert_eq!(with_items.queries.as_ref().unwrap().len(), 1);
    }

    #[test]
    fn should_parse_valid_configuration_with_queries() {
        // Configuration using unified queries field
        let config_json = r#"{
            "languages": {
                "rust": {
                    "parser": "/path/to/rust.so",
                    "queries": [
                        {"path": "/path/to/highlights.scm", "kind": "highlights"},
                        {"path": "/path/to/custom.scm"}
                    ]
                }
            }
        }"#;

        let settings: RawWorkspaceSettings = serde_json::from_str(config_json).unwrap();

        assert!(settings.search_paths.is_none());
        assert!(settings.languages.contains_key("rust"));
        assert_eq!(
            settings.languages["rust"].parser,
            Some("/path/to/rust.so".to_string())
        );

        let queries = settings.languages["rust"].queries.as_ref().unwrap();
        assert_eq!(queries.len(), 2);
        assert_eq!(queries[0].path, "/path/to/highlights.scm");
        assert_eq!(queries[0].kind, Some(QueryKind::Highlights));
        assert_eq!(queries[1].path, "/path/to/custom.scm");
        assert_eq!(queries[1].kind, None); // Kind not specified - will be inferred later
    }

    #[rstest]
    #[case::locals("locals", QueryKind::Locals)]
    #[case::injections("injections", QueryKind::Injections)]
    fn should_parse_configuration_with_query_kind(
        #[case] kind_str: &str,
        #[case] expected: QueryKind,
    ) {
        let config_json = format!(
            r#"{{
            "languages": {{
                "rust": {{
                    "queries": [
                        {{"path": "/path/to/highlights.scm", "kind": "highlights"}},
                        {{"path": "/path/to/{kind_str}.scm", "kind": "{kind_str}"}}
                    ]
                }}
            }}
        }}"#
        );

        let settings: RawWorkspaceSettings = serde_json::from_str(&config_json).unwrap();
        let queries = settings.languages["rust"].queries.as_ref().unwrap();
        assert_eq!(queries.len(), 2);
        assert_eq!(queries[1].path, format!("/path/to/{kind_str}.scm"));
        assert_eq!(queries[1].kind, Some(expected));
    }

    #[test]
    fn should_reject_invalid_json() {
        let invalid_json = r#"{
            "treesitter": {
                "rust": {
                    "parser": "/path/to/rust.so"
                    // Missing comma - invalid JSON
                    "highlight": []
                }
            }
        }"#;

        let result = serde_json::from_str::<RawWorkspaceSettings>(invalid_json);
        assert!(result.is_err());
    }

    #[test]
    fn should_handle_empty_configurations() {
        let empty_json = r#"{
            "languages": {}
        }"#;

        let settings: RawWorkspaceSettings = serde_json::from_str(empty_json).unwrap();
        assert!(settings.languages.is_empty());
    }

    #[test]
    fn should_handle_completely_empty_json_object() {
        // This is crucial for zero-config: InitializationOptions = {} should work
        let completely_empty = r#"{}"#;

        let settings: RawWorkspaceSettings = serde_json::from_str(completely_empty).unwrap();
        assert!(settings.languages.is_empty());
        assert!(settings.search_paths.is_none());
        assert!(settings.auto_install.is_none());
        assert!(settings.capture_mappings.is_empty());
    }

    #[test]
    fn should_handle_missing_languages_field() {
        let json_without_languages = r#"{
            "searchPaths": ["/some/path"],
            "captureMappings": {
                "_": {
                    "highlights": {}
                }
            }
        }"#;

        let settings: RawWorkspaceSettings = serde_json::from_str(json_without_languages).unwrap();
        assert!(settings.languages.is_empty());
        assert_eq!(settings.search_paths, Some(vec!["/some/path".to_string()]));
    }

    #[test]
    fn should_parse_toml_without_languages_field() {
        let toml_without_languages = r#"
            [captureMappings._.highlights]
        "#;

        let settings: RawWorkspaceSettings = toml::from_str(toml_without_languages).unwrap();
        assert!(settings.languages.is_empty());
    }

    #[rstest]
    #[case::basic(
        r#"{"searchPaths": ["/usr/local/lib/tree-sitter", "/opt/tree-sitter/parsers"],
            "languages": {"rust": {"queries": [{"path": "/path/to/highlights.scm", "kind": "highlights"}]}}}"#,
        &["/usr/local/lib/tree-sitter", "/opt/tree-sitter/parsers"],
    )]
    #[case::different_paths(
        r#"{"searchPaths": ["/data/tree-sitter", "/assets/ts"],
            "languages": {"lua": {"queries": [{"path": "/path/to/highlights.scm", "kind": "highlights"}]}}}"#,
        &["/data/tree-sitter", "/assets/ts"],
    )]
    fn should_parse_searchpaths_configuration(
        #[case] config_json: &str,
        #[case] expected_paths: &[&str],
    ) {
        let settings: RawWorkspaceSettings = serde_json::from_str(config_json).unwrap();
        assert_eq!(
            settings.search_paths.unwrap(),
            expected_paths
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn should_parse_mixed_configuration_with_searchpaths_and_explicit_parser() {
        let config_json = r#"{
            "searchPaths": ["/usr/local/lib/tree-sitter"],
            "languages": {
                "rust": {
                    "parser": "/custom/path/rust.so",
                    "queries": [{"path": "/path/to/highlights.scm", "kind": "highlights"}]
                },
                "python": {
                    "queries": [{"path": "/path/to/python.scm", "kind": "highlights"}]
                }
            }
        }"#;

        let settings: RawWorkspaceSettings = serde_json::from_str(config_json).unwrap();

        assert!(settings.search_paths.is_some());
        assert_eq!(
            settings.search_paths.unwrap(),
            vec!["/usr/local/lib/tree-sitter"]
        );

        // rust has explicit parser path
        assert_eq!(
            settings.languages["rust"].parser,
            Some("/custom/path/rust.so".to_string())
        );

        // python will use searchPaths
        assert_eq!(settings.languages["python"].parser, None);
    }

    #[test]
    fn should_handle_malformed_json_gracefully() {
        let malformed_configs = vec![
            r#"{"languages": {"rust": {"parser": "/path"}"#, // Missing closing braces
            r#"{"languages": {"rust": {"parser": "/path", "queries": [}}"#, // Invalid array
        ];

        for config in malformed_configs {
            let result = serde_json::from_str::<RawWorkspaceSettings>(config);
            assert!(result.is_err());
        }
    }

    #[test]
    fn should_parse_capture_mappings() {
        let config_json = r#"{
            "languages": {
                "rust": {
                    "queries": [{"path": "/path/to/highlights.scm", "kind": "highlights"}]
                }
            },
            "captureMappings": {
                "_": {
                    "highlights": {
                        "variable.builtin": "variable.defaultLibrary",
                        "function.builtin": "function.defaultLibrary"
                    }
                },
                "rust": {
                    "highlights": {
                        "type.builtin": "type.defaultLibrary"
                    },
                    "locals": {
                        "definition.var": "definition.variable"
                    }
                }
            }
        }"#;

        let settings: RawWorkspaceSettings = serde_json::from_str(config_json).unwrap();
        let mut snap_settings = insta::Settings::clone_current();
        snap_settings.set_sort_maps(true);
        snap_settings.bind(|| {
            insta::assert_json_snapshot!(settings.capture_mappings);
        });
    }

    #[test]
    fn should_parse_auto_install_setting() {
        let config_json = r#"{
            "autoInstall": true,
            "languages": {}
        }"#;

        let settings: RawWorkspaceSettings = serde_json::from_str(config_json).unwrap();
        assert_eq!(settings.auto_install, Some(true));

        // Test with false
        let config_false = r#"{
            "autoInstall": false,
            "languages": {}
        }"#;
        let settings_false: RawWorkspaceSettings = serde_json::from_str(config_false).unwrap();
        assert_eq!(settings_false.auto_install, Some(false));

        // Test missing (should be None)
        let config_missing = r#"{
            "languages": {}
        }"#;
        let settings_missing: RawWorkspaceSettings = serde_json::from_str(config_missing).unwrap();
        assert_eq!(settings_missing.auto_install, None);
    }

    #[test]
    fn should_handle_complex_configurations_efficiently() {
        let mut config = RawWorkspaceSettings {
            search_paths: None,
            languages: HashMap::new(),
            capture_mappings: HashMap::new(),
            auto_install: None,
            diagnostics_debounce_ms: None,
            language_servers: None,
        };

        // Add multiple language configurations
        let languages = vec!["rust", "python", "javascript", "typescript", "go"];

        for lang in languages {
            config.languages.insert(
                lang.to_string(),
                LanguageSettings {
                    parser: Some(format!("/usr/lib/libtree-sitter-{}.so", lang)),
                    queries: Some(vec![QueryItem {
                        path: format!("/etc/tree-sitter/{}/highlights.scm", lang),
                        kind: Some(QueryKind::Highlights),
                    }]),
                    ..Default::default()
                },
            );
        }

        assert_eq!(config.languages.len(), 5);

        // Verify serialization/deserialization still works
        let json = serde_json::to_string(&config).unwrap();
        let deserialized: RawWorkspaceSettings = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.languages.len(), config.languages.len());
    }

    #[test]
    fn should_not_have_legacy_fields_in_language_config() {
        // Legacy highlights/locals/injections fields have been removed from LanguageSettings
        // All query configuration now uses the unified queries field
        let config = LanguageSettings {
            parser: Some("/path/to/parser.so".to_string()),
            queries: Some(vec![QueryItem {
                path: "/path/to/highlights.scm".to_string(),
                kind: Some(QueryKind::Highlights),
            }]),
            ..Default::default()
        };

        // Serialize and verify no legacy fields in output
        let json = serde_json::to_string(&config).unwrap();
        assert!(
            !json.contains("\"highlights\":"),
            "LanguageSettings should not have highlights field, but found in JSON: {}",
            json
        );
        assert!(
            !json.contains("\"locals\":"),
            "LanguageSettings should not have locals field, but found in JSON: {}",
            json
        );
        assert!(
            !json.contains("\"injections\":"),
            "LanguageSettings should not have injections field, but found in JSON: {}",
            json
        );
    }

    #[test]
    fn should_create_language_settings_with_parser_and_queries() {
        // LanguageSettings uses parser (not library) and unified queries
        let settings = LanguageSettings {
            parser: Some("/path/to/parser.so".to_string()),
            queries: Some(vec![QueryItem {
                path: "/path/to/highlights.scm".to_string(),
                kind: Some(QueryKind::Highlights),
            }]),
            ..Default::default()
        };

        assert_eq!(settings.parser, Some("/path/to/parser.so".to_string()));
        assert_eq!(settings.queries.as_ref().unwrap().len(), 1);
        assert_eq!(
            settings.queries.as_ref().unwrap()[0].path,
            "/path/to/highlights.scm"
        );
        assert_eq!(
            settings.queries.as_ref().unwrap()[0].kind,
            Some(QueryKind::Highlights)
        );
    }

    #[test]
    fn should_parse_bridge_server_config() {
        // Test that BridgeServerConfig can deserialize all fields:
        // cmd (required), languages (required), initialization_options (optional)
        let config_json = r#"{
            "cmd": ["rust-analyzer", "--log-file", "/tmp/ra.log"],
            "languages": ["rust"],
            "initializationOptions": {
                "linkedProjects": ["/path/to/Cargo.toml"]
            }
        }"#;

        let config: BridgeServerConfig = serde_json::from_str(config_json).unwrap();

        assert_eq!(
            config.cmd,
            vec![
                "rust-analyzer".to_string(),
                "--log-file".to_string(),
                "/tmp/ra.log".to_string()
            ]
        );
        assert_eq!(config.languages, vec!["rust".to_string()]);
        assert!(config.initialization_options.is_some());
        let init_opts = config.initialization_options.unwrap();
        assert!(init_opts.get("linkedProjects").is_some());
    }

    #[test]
    fn should_parse_bridge_server_config_minimal() {
        // Test that only required fields need to be present
        let config_json = r#"{
            "cmd": ["pyright"],
            "languages": ["python"]
        }"#;

        let config: BridgeServerConfig = serde_json::from_str(config_json).unwrap();

        assert_eq!(config.cmd, vec!["pyright".to_string()]);
        assert_eq!(config.languages, vec!["python".to_string()]);
        assert!(config.initialization_options.is_none());
    }

    #[test]
    fn should_parse_language_config_with_bridge_map_enabled() {
        // bridge = { python = { enabled = true }, r = { enabled = true } } parses as
        // HashMap<String, BridgeLanguageConfig>.
        let config_json = r#"{
            "parser": "/path/to/parser.so",
            "queries": [{"path": "/path/to/highlights.scm", "kind": "highlights"}],
            "bridge": {
                "python": { "enabled": true },
                "r": { "enabled": true }
            }
        }"#;

        let config: LanguageSettings = serde_json::from_str(config_json).unwrap();

        assert!(config.bridge.is_some(), "bridge field should be Some");
        let bridge = config.bridge.unwrap();
        assert_eq!(bridge.len(), 2);
        assert_eq!(bridge.get("python").unwrap().enabled, Some(true));
        assert_eq!(bridge.get("r").unwrap().enabled, Some(true));
    }

    #[test]
    fn should_parse_language_config_with_empty_bridge_map() {
        // Empty bridge map (`bridge: {}`) disables all bridging for that host filetype.
        let config_json = r#"{
            "parser": "/path/to/parser.so",
            "queries": [{"path": "/path/to/highlights.scm", "kind": "highlights"}],
            "bridge": {}
        }"#;

        let config: LanguageSettings = serde_json::from_str(config_json).unwrap();

        assert!(
            config.bridge.is_some(),
            "bridge field should be Some(empty map)"
        );
        let bridge = config.bridge.unwrap();
        assert!(bridge.is_empty(), "bridge map should be empty");
    }

    #[test]
    fn should_parse_language_config_without_bridge_field() {
        // Omitted bridge field should be None (bridges all configured languages).
        let config_json = r#"{
            "parser": "/path/to/parser.so",
            "queries": [{"path": "/path/to/highlights.scm", "kind": "highlights"}]
        }"#;

        let config: LanguageSettings = serde_json::from_str(config_json).unwrap();

        assert!(
            config.bridge.is_none(),
            "omitted bridge field should be None"
        );
    }

    /// Bridge filter: None (default) bridges all, empty map bridges nothing,
    /// explicit entries control per-language bridging.
    #[rstest]
    // None bridge → all languages bridgeable
    #[case::null_allows_all(None, "python", true)]
    #[case::null_allows_any(None, "any_language", true)]
    // Empty map → nothing bridgeable
    #[case::empty_blocks_all(Some(HashMap::new()), "python", false)]
    #[case::empty_blocks_rust(Some(HashMap::new()), "rust", false)]
    // Explicit enabled entries
    #[case::enabled_language(
        Some(HashMap::from([
            ("python".to_string(), BridgeLanguageConfig { enabled: Some(true), ..Default::default() }),
        ])),
        "python",
        true
    )]
    #[case::unlisted_language(
        Some(HashMap::from([
            ("python".to_string(), BridgeLanguageConfig { enabled: Some(true), ..Default::default() }),
        ])),
        "rust",
        false
    )]
    #[case::disabled_language(
        Some(HashMap::from([
            ("r".to_string(), BridgeLanguageConfig { enabled: Some(false), ..Default::default() }),
        ])),
        "r",
        false
    )]
    // Multi-entry map: enabled + disabled coexist
    #[case::multi_entry_enabled(
        Some(HashMap::from([
            ("python".to_string(), BridgeLanguageConfig { enabled: Some(true), ..Default::default() }),
            ("r".to_string(), BridgeLanguageConfig { enabled: Some(false), ..Default::default() }),
        ])),
        "python",
        true
    )]
    #[case::multi_entry_disabled(
        Some(HashMap::from([
            ("python".to_string(), BridgeLanguageConfig { enabled: Some(true), ..Default::default() }),
            ("r".to_string(), BridgeLanguageConfig { enabled: Some(false), ..Default::default() }),
        ])),
        "r",
        false
    )]
    #[case::multi_entry_unlisted(
        Some(HashMap::from([
            ("python".to_string(), BridgeLanguageConfig { enabled: Some(true), ..Default::default() }),
            ("r".to_string(), BridgeLanguageConfig { enabled: Some(false), ..Default::default() }),
        ])),
        "rust",
        false
    )]
    fn test_bridge_filter(
        #[case] bridge: Option<HashMap<String, BridgeLanguageConfig>>,
        #[case] language: &str,
        #[case] expected: bool,
    ) {
        let settings = LanguageSettings {
            bridge,
            ..Default::default()
        };
        assert_eq!(
            settings.is_language_bridgeable(language),
            expected,
            "is_language_bridgeable({language:?}) should be {expected}"
        );
    }

    #[test]
    fn should_parse_language_servers_at_root() {
        let config_json = r#"{
            "searchPaths": ["/usr/local/lib"],
            "languageServers": {
                "rust-analyzer": {
                    "cmd": ["rust-analyzer"],
                    "languages": ["rust"]
                },
                "pyright": {
                    "cmd": ["pyright-langserver", "--stdio"],
                    "languages": ["python"]
                }
            }
        }"#;

        let settings: RawWorkspaceSettings = serde_json::from_str(config_json).unwrap();
        assert!(
            settings.language_servers.is_some(),
            "languageServers should be parsed as Some"
        );
        let mut snap_settings = insta::Settings::clone_current();
        snap_settings.set_sort_maps(true);
        snap_settings.bind(|| {
            insta::assert_json_snapshot!(settings.language_servers);
        });
    }

    #[test]
    fn should_parse_prefer_shared_instance() {
        let config_json = r#"{
            "languageServers": {
                "tsgo": {
                    "cmd": ["typescript-language-server", "--stdio"],
                    "languages": ["typescript"],
                    "preferSharedInstance": true
                },
                "pyright": {
                    "cmd": ["pyright-langserver", "--stdio"],
                    "languages": ["python"]
                }
            }
        }"#;

        let settings: RawWorkspaceSettings = serde_json::from_str(config_json).unwrap();
        let servers = settings.language_servers.expect("languageServers parses");
        assert_eq!(
            servers["tsgo"].prefer_shared_instance,
            Some(true),
            "explicit preferSharedInstance is preserved"
        );
        assert_eq!(
            servers["pyright"].prefer_shared_instance, None,
            "absent preferSharedInstance parses as None (inherit -> per-root)"
        );
    }

    #[test]
    fn should_parse_enabled() {
        let config_json = r#"{
            "languageServers": {
                "tsgo": {
                    "cmd": ["typescript-language-server", "--stdio"],
                    "languages": ["typescript"],
                    "enabled": false
                },
                "pyright": {
                    "cmd": ["pyright-langserver", "--stdio"],
                    "languages": ["python"]
                }
            }
        }"#;

        let settings: RawWorkspaceSettings = serde_json::from_str(config_json).unwrap();
        let servers = settings.language_servers.expect("languageServers parses");
        assert_eq!(
            servers["tsgo"].enabled,
            Some(false),
            "explicit enabled is preserved"
        );
        assert_eq!(
            servers["pyright"].enabled, None,
            "absent enabled parses as None (inherit -> default true)"
        );
    }

    #[test]
    fn should_parse_on_type_formatting_triggers() {
        let config_json = r#"{
            "languageServers": {
                "clangd": {
                    "cmd": ["clangd"],
                    "languages": ["cpp"],
                    "onTypeFormattingTriggers": ["}", ";"]
                },
                "pyright": {
                    "cmd": ["pyright-langserver", "--stdio"],
                    "languages": ["python"]
                }
            }
        }"#;

        let settings: RawWorkspaceSettings = serde_json::from_str(config_json).unwrap();
        let servers = settings.language_servers.expect("languageServers parses");
        assert_eq!(
            servers["clangd"].on_type_formatting_triggers,
            Some(vec!["}".to_string(), ";".to_string()]),
            "explicit trigger list is preserved"
        );
        assert_eq!(
            servers["pyright"].on_type_formatting_triggers, None,
            "absent onTypeFormattingTriggers parses as None"
        );
    }

    #[test]
    fn on_type_formatting_trigger_union_is_sorted_and_deduped() {
        let server = |triggers: Option<Vec<&str>>| BridgeServerConfig {
            cmd: vec!["x".to_string()],
            languages: vec![],
            initialization_options: None,
            workspace_markers: None,
            on_type_formatting_triggers: triggers
                .map(|t| t.into_iter().map(String::from).collect()),
            prefer_shared_instance: None,
            enabled: None,
            settings: None,
        };
        let servers = HashMap::from([
            ("a".to_string(), server(Some(vec!["}", ";"]))),
            ("b".to_string(), server(Some(vec![";", "\n"]))),
            ("c".to_string(), server(None)),
        ]);

        let (first, more) =
            on_type_formatting_trigger_union(&servers).expect("triggers configured");
        // Sorted + deduped: deterministic regardless of HashMap iteration order.
        assert_eq!(first, "\n");
        assert_eq!(more, vec![";".to_string(), "}".to_string()]);

        // Same logical contents inserted in a different order must produce
        // the identical advertisement.
        let servers_reordered = HashMap::from([
            ("c".to_string(), server(None)),
            ("b".to_string(), server(Some(vec![";", "\n"]))),
            ("a".to_string(), server(Some(vec!["}", ";"]))),
        ]);
        assert_eq!(
            on_type_formatting_trigger_union(&servers_reordered),
            Some((first, more)),
            "union must not depend on map construction order"
        );
    }

    #[test]
    fn on_type_formatting_trigger_union_excludes_disabled_server() {
        let server = |triggers: Option<Vec<&str>>, enabled: Option<bool>| BridgeServerConfig {
            cmd: vec!["x".to_string()],
            languages: vec![],
            initialization_options: None,
            root_markers: None,
            on_type_formatting_triggers: triggers
                .map(|t| t.into_iter().map(String::from).collect()),
            prefer_shared_instance: None,
            enabled,
            settings: None,
        };

        // A disabled server's own trigger never spawns anything, so it must
        // not be advertised (it would cause per-keystroke dead requests).
        let servers = HashMap::from([
            ("a".to_string(), server(Some(vec!["}"]), Some(false))),
            ("b".to_string(), server(Some(vec![";"]), None)),
        ]);
        let (first, more) =
            on_type_formatting_trigger_union(&servers).expect("enabled server has a trigger");
        assert_eq!(first, ";");
        assert_eq!(more, Vec::<String>::new());

        // Disabled via the wildcard's inherited default, too.
        let servers_wildcard_disabled = HashMap::from([
            (
                "_".to_string(),
                BridgeServerConfig {
                    enabled: Some(false),
                    ..server(None, None)
                },
            ),
            ("a".to_string(), server(Some(vec!["}"]), None)),
        ]);
        assert_eq!(
            on_type_formatting_trigger_union(&servers_wildcard_disabled),
            None,
            "a server disabled via the wildcard default contributes no triggers"
        );

        // The wildcard key itself is never a direct contributor — it's
        // excluded from iteration entirely (see the function's doc comment),
        // so a config with only a `_` entry always advertises nothing,
        // regardless of its own `enabled` or triggers: nothing inherits from
        // it when there are no concrete servers.
        let wildcard_only = HashMap::from([(
            "_".to_string(),
            BridgeServerConfig {
                enabled: Some(true),
                ..server(Some(vec!["}"]), None)
            },
        )]);
        assert_eq!(
            on_type_formatting_trigger_union(&wildcard_only),
            None,
            "a wildcard-only config has no concrete server to inherit its triggers"
        );

        // A concrete server that opts back IN over a disabled wildcard must
        // still inherit — and advertise — the wildcard's trigger characters
        // (the gate is per-server effective state, not raw wildcard state).
        let reenabled_inherits_wildcard_triggers = HashMap::from([
            (
                "_".to_string(),
                BridgeServerConfig {
                    enabled: Some(false),
                    ..server(Some(vec!["}"]), None)
                },
            ),
            ("lua_ls".to_string(), server(None, Some(true))),
        ]);
        assert_eq!(
            on_type_formatting_trigger_union(&reenabled_inherits_wildcard_triggers),
            Some(("}".to_string(), Vec::new())),
            "a server re-enabled over a disabled wildcard still inherits its triggers"
        );
    }

    #[test]
    fn on_type_formatting_trigger_union_excludes_empty_cmd_server() {
        // Enabled but unspawnable (no resolved cmd) must be excluded exactly
        // like a disabled server: gate on is_spawnable(), not is_enabled()
        // alone, to match every other "is this server usable" call site.
        let servers = HashMap::from([(
            "tsgo".to_string(),
            BridgeServerConfig {
                cmd: vec![],
                languages: vec![],
                initialization_options: None,
                root_markers: None,
                on_type_formatting_triggers: Some(vec![";".to_string()]),
                prefer_shared_instance: None,
                enabled: None,
                settings: None,
            },
        )]);
        assert_eq!(
            on_type_formatting_trigger_union(&servers),
            None,
            "a server with no resolved cmd can never service the request, \
             so its triggers must not be advertised"
        );
    }

    #[test]
    fn on_type_formatting_trigger_union_without_triggers_is_none() {
        let servers = HashMap::from([(
            "a".to_string(),
            BridgeServerConfig {
                cmd: vec!["x".to_string()],
                languages: vec![],
                initialization_options: None,
                workspace_markers: None,
                on_type_formatting_triggers: Some(vec![String::new()]),
                prefer_shared_instance: None,
                enabled: None,
                settings: None,
            },
        )]);
        assert_eq!(
            on_type_formatting_trigger_union(&servers),
            None,
            "empty-string triggers are ignored; no triggers means no capability"
        );
        assert_eq!(on_type_formatting_trigger_union(&HashMap::new()), None);
    }

    #[test]
    fn should_parse_language_servers_empty() {
        // Empty languageServers map should be valid.
        let config_json = r#"{
            "languageServers": {}
        }"#;

        let settings: RawWorkspaceSettings = serde_json::from_str(config_json).unwrap();

        assert!(settings.language_servers.is_some());
        assert!(settings.language_servers.as_ref().unwrap().is_empty());
    }

    #[test]
    fn should_parse_defaults_only_language_server_entry() {
        // A wildcard `_` entry exists to supply defaults (e.g. workspaceMarkers)
        // to concrete servers, so cmd/languages must be optional in TOML.
        let toml_str = r#"
            [languageServers._]
            workspaceMarkers = [".git"]
        "#;

        let settings: RawWorkspaceSettings = toml::from_str(toml_str).unwrap();
        let servers = settings.language_servers.unwrap();
        let wildcard = &servers["_"];
        assert!(wildcard.cmd.is_empty());
        assert!(wildcard.languages.is_empty());
        assert_eq!(
            wildcard.workspace_markers,
            Some(vec![RootMarker::Single(".git".to_string())])
        );
    }

    #[test]
    fn should_parse_workspace_markers_camel_case() {
        let config_json = r#"{
            "cmd": ["rust-analyzer"],
            "languages": ["rust"],
            "workspaceMarkers": [".git", "Cargo.toml"]
        }"#;

        let config: BridgeServerConfig = serde_json::from_str(config_json).unwrap();
        assert_eq!(
            config.workspace_markers,
            Some(vec![
                RootMarker::Single(".git".to_string()),
                RootMarker::Single("Cargo.toml".to_string()),
            ])
        );
    }

    #[test]
    fn should_parse_workspace_markers_toml() {
        // The canonical key via TOML (the primary path users type), not just
        // the JSON/serde_json round-trip.
        let toml_str = r#"
            [languageServers._]
            workspaceMarkers = [".git", "Cargo.toml"]
        "#;

        let settings: RawWorkspaceSettings = toml::from_str(toml_str).unwrap();
        let servers = settings.language_servers.unwrap();
        assert_eq!(
            servers["_"].workspace_markers,
            Some(vec![
                RootMarker::Single(".git".to_string()),
                RootMarker::Single("Cargo.toml".to_string()),
            ])
        );
    }

    #[test]
    fn should_parse_deprecated_root_markers_alias() {
        // `rootMarkers` is the pre-rename key, kept as a deprecated serde alias
        // for backward compatibility; it deserializes into `workspace_markers`.
        let config_json = r#"{
            "cmd": ["rust-analyzer"],
            "languages": ["rust"],
            "rootMarkers": [".git", "Cargo.toml"]
        }"#;

        let config: BridgeServerConfig = serde_json::from_str(config_json).unwrap();
        assert_eq!(
            config.workspace_markers,
            Some(vec![
                RootMarker::Single(".git".to_string()),
                RootMarker::Single("Cargo.toml".to_string()),
            ])
        );
    }

    #[test]
    fn empty_workspace_markers_deserializes_to_some_empty_kill_switch() {
        // The kill-switch contract (explicit `[]` disables the marker search)
        // rests on `[]` deserializing to `Some(vec![])`, not `None`. Pin it on
        // the deserialize path directly, not just via the merge test.
        let config: BridgeServerConfig =
            serde_json::from_str(r#"{ "workspaceMarkers": [] }"#).unwrap();
        assert_eq!(config.workspace_markers, Some(vec![]));
    }

    #[test]
    fn both_marker_keys_present_is_a_hard_error() {
        // `workspaceMarkers` and its `rootMarkers` alias are the same field, so
        // supplying both is a deterministic serde "duplicate field" error (no
        // silent last-wins). A mid-migration config that sets both fails safe:
        // the offending config layer is dropped with a warning, not merged.
        let err = serde_json::from_str::<BridgeServerConfig>(
            r#"{ "workspaceMarkers": [".git"], "rootMarkers": [".git"] }"#,
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("duplicate field"),
            "expected a duplicate-field error, got: {err}"
        );
    }

    #[test]
    fn toml_both_marker_keys_present_is_a_hard_error() {
        // The toml crate's duplicate-key detection is independent of
        // serde_json's, so pin the same fail-safe on the TOML load path too.
        let toml_str = r#"
            [languageServers._]
            workspaceMarkers = [".git"]
            rootMarkers = [".cargo"]
        "#;
        let err = toml::from_str::<RawWorkspaceSettings>(toml_str).unwrap_err();
        assert!(
            err.to_string().to_lowercase().contains("duplicate"),
            "expected a duplicate-key error, got: {err}"
        );
    }

    #[test]
    fn should_parse_workspace_markers_mixed_string_and_group_json() {
        // Neovim's (string|string[])[] shape: a bare string is one tier, a
        // nested array is an equal-priority group.
        let config_json = r#"{
            "workspaceMarkers": [["stylua.toml", ".luarc.json"], ".git"]
        }"#;

        let config: BridgeServerConfig = serde_json::from_str(config_json).unwrap();
        assert_eq!(
            config.workspace_markers,
            Some(vec![
                RootMarker::Group(vec!["stylua.toml".to_string(), ".luarc.json".to_string()]),
                RootMarker::Single(".git".to_string()),
            ])
        );
    }

    #[test]
    fn workspace_markers_group_survives_json_serialize_round_trip() {
        // `kakehashi/effectiveConfiguration` re-serializes the user's settings
        // via serde_json, so a Group must round-trip: array for a group, bare
        // string for a single, with entry order preserved.
        let config = BridgeServerConfig {
            cmd: vec![],
            languages: vec![],
            initialization_options: None,
            workspace_markers: Some(vec![
                RootMarker::Group(vec!["stylua.toml".to_string(), ".luarc.json".to_string()]),
                RootMarker::Single(".git".to_string()),
            ]),
            on_type_formatting_triggers: None,
            prefer_shared_instance: None,
            enabled: None,
            settings: None,
        };

        let json = serde_json::to_value(&config).unwrap();
        assert_eq!(
            json["workspaceMarkers"],
            serde_json::json!([["stylua.toml", ".luarc.json"], ".git"])
        );
        // The deprecated key is never *emitted* (alias is deserialize-only).
        assert!(json.get("rootMarkers").is_none());

        let reparsed: BridgeServerConfig = serde_json::from_value(json).unwrap();
        assert_eq!(reparsed.workspace_markers, config.workspace_markers);
    }

    #[test]
    fn should_parse_workspace_markers_mixed_string_and_group_toml() {
        // TOML 1.0 allows heterogeneous arrays; the untagged enum must round
        // trip a mix of bare names and nested groups through the toml crate.
        let toml_str = r#"
            [languageServers._]
            workspaceMarkers = [["stylua.toml", ".luarc.json"], ".git"]
        "#;

        let settings: RawWorkspaceSettings = toml::from_str(toml_str).unwrap();
        let servers = settings.language_servers.unwrap();
        let wildcard = &servers["_"];
        assert_eq!(
            wildcard.workspace_markers,
            Some(vec![
                RootMarker::Group(vec!["stylua.toml".to_string(), ".luarc.json".to_string()]),
                RootMarker::Single(".git".to_string()),
            ])
        );
    }

    #[test]
    fn should_parse_without_language_servers() {
        // Missing languageServers should deserialize as None.
        let config_json = r#"{
            "searchPaths": ["/usr/local/lib"]
        }"#;

        let settings: RawWorkspaceSettings = serde_json::from_str(config_json).unwrap();

        assert!(settings.language_servers.is_none());
    }

    #[test]
    fn should_parse_bridge_language_config() {
        // BridgeLanguageConfig deserializes the `enabled` field.
        let config_json = r#"{
            "enabled": true
        }"#;

        let config: BridgeLanguageConfig = serde_json::from_str(config_json).unwrap();
        assert_eq!(config.enabled, Some(true));

        // Test disabled
        let config_false_json = r#"{
            "enabled": false
        }"#;
        let config_false: BridgeLanguageConfig = serde_json::from_str(config_false_json).unwrap();
        assert_eq!(config_false.enabled, Some(false));
    }

    #[test]
    fn should_parse_language_config_with_bridge_map() {
        // LanguageSettings.bridge is a HashMap<String, BridgeLanguageConfig>.
        let config_json = r#"{
            "parser": "/path/to/parser.so",
            "queries": [{"path": "/path/to/highlights.scm", "kind": "highlights"}],
            "bridge": {
                "python": { "enabled": true },
                "r": { "enabled": false }
            }
        }"#;

        let config: LanguageSettings = serde_json::from_str(config_json).unwrap();

        assert!(config.bridge.is_some(), "bridge field should be Some");
        let bridge = config.bridge.unwrap();
        assert_eq!(bridge.len(), 2);
        assert_eq!(bridge.get("python").unwrap().enabled, Some(true));
        assert_eq!(bridge.get("r").unwrap().enabled, Some(false));
    }

    #[test]
    fn should_parse_query_item_with_path_and_kind() {
        // QueryItem should have path (required) and kind (optional) fields
        // kind can be "highlights", "locals", or "injections"
        let toml_str = r#"
            path = "/path/to/highlights.scm"
            kind = "highlights"
        "#;

        let item: QueryItem = toml::from_str(toml_str).unwrap();
        assert_eq!(item.path, "/path/to/highlights.scm");
        assert_eq!(item.kind, Some(QueryKind::Highlights));
    }

    #[test]
    fn should_parse_query_item_without_kind() {
        // kind is optional - defaults to None (type inference happens later)
        let toml_str = r#"
            path = "/path/to/custom.scm"
        "#;

        let item: QueryItem = toml::from_str(toml_str).unwrap();
        assert_eq!(item.path, "/path/to/custom.scm");
        assert!(item.kind.is_none());
    }

    #[test]
    fn should_parse_query_kind_enum_variants() {
        // QueryKind enum should have Highlights, Locals, Injections variants
        let highlights_toml = r#"path = "/a.scm"
kind = "highlights""#;
        let locals_toml = r#"path = "/b.scm"
kind = "locals""#;
        let injections_toml = r#"path = "/c.scm"
kind = "injections""#;

        let h: QueryItem = toml::from_str(highlights_toml).unwrap();
        let l: QueryItem = toml::from_str(locals_toml).unwrap();
        let i: QueryItem = toml::from_str(injections_toml).unwrap();

        assert_eq!(h.kind, Some(QueryKind::Highlights));
        assert_eq!(l.kind, Some(QueryKind::Locals));
        assert_eq!(i.kind, Some(QueryKind::Injections));
    }

    #[test]
    fn should_parse_queries_array_in_language_config() {
        // LanguageSettings should have queries: Option<Vec<QueryItem>>
        let config_toml = r#"
            parser ="/path/to/parser.so"
            [[queries]]
            path = "/path/to/highlights.scm"

            [[queries]]
            path = "/path/to/locals.scm"
            kind = "locals"
        "#;

        let config: LanguageSettings = toml::from_str(config_toml).unwrap();
        assert!(config.queries.is_some());
        let queries = config.queries.unwrap();
        assert_eq!(queries.len(), 2);
        assert_eq!(queries[0].path, "/path/to/highlights.scm");
        assert!(queries[0].kind.is_none());
        assert_eq!(queries[1].path, "/path/to/locals.scm");
        assert_eq!(queries[1].kind, Some(QueryKind::Locals));
    }

    // Type inference for query kinds

    #[test]
    fn should_infer_highlights_from_filename_pattern() {
        // Only exact match "highlights.scm" -> Some(Highlights)
        assert_eq!(
            infer_query_kind("highlights.scm"),
            Some(QueryKind::Highlights)
        );
        assert_eq!(
            infer_query_kind("/path/to/highlights.scm"),
            Some(QueryKind::Highlights)
        );
        assert_eq!(
            infer_query_kind("/usr/share/python/highlights.scm"),
            Some(QueryKind::Highlights)
        );
        // Prefixed variants should NOT match (only exact filename)
        assert_eq!(infer_query_kind("./queries/python-highlights.scm"), None);
        assert_eq!(infer_query_kind("rust-highlights.scm"), None);
    }

    #[test]
    fn should_infer_locals_from_filename_pattern() {
        // Only exact match "locals.scm" -> Some(Locals)
        assert_eq!(infer_query_kind("locals.scm"), Some(QueryKind::Locals));
        assert_eq!(
            infer_query_kind("/path/to/locals.scm"),
            Some(QueryKind::Locals)
        );
        // Prefixed variants should NOT match (only exact filename)
        assert_eq!(infer_query_kind("./queries/rust-locals.scm"), None);
        assert_eq!(infer_query_kind("/usr/share/python-locals.scm"), None);
        assert_eq!(infer_query_kind("javascript-locals.scm"), None);
    }

    #[test]
    fn should_infer_injections_from_filename_pattern() {
        // Only exact match "injections.scm" -> Some(Injections)
        assert_eq!(
            infer_query_kind("injections.scm"),
            Some(QueryKind::Injections)
        );
        assert_eq!(
            infer_query_kind("/path/to/injections.scm"),
            Some(QueryKind::Injections)
        );
        // Prefixed variants should NOT match (only exact filename)
        assert_eq!(infer_query_kind("./markdown-injections.scm"), None);
        assert_eq!(infer_query_kind("/usr/share/markdown-injections.scm"), None);
        assert_eq!(infer_query_kind("rust-injections.scm"), None);
    }

    #[test]
    fn should_return_none_for_unrecognized_patterns() {
        // Files without highlights/locals/injections in the name should return None
        // (callers skip these files silently)
        assert_eq!(infer_query_kind("custom.scm"), None);
        assert_eq!(infer_query_kind("python.scm"), None);
        assert_eq!(infer_query_kind("/path/to/queries.scm"), None);
        assert_eq!(infer_query_kind("./custom-queries.scm"), None);
        assert_eq!(infer_query_kind("/usr/share/rust.scm"), None);
    }

    #[test]
    fn should_not_match_files_with_prefixes_before_pattern() {
        // Files like "local-injections.scm" should NOT match because they have
        // additional text before the pattern. Only exact matches like "injections.scm"
        // or suffix matches like "rust-injections.scm" should match.
        assert_eq!(
            infer_query_kind("local-injections.scm"),
            None,
            "local-injections.scm should not match injections pattern"
        );
        assert_eq!(
            infer_query_kind("global-locals.scm"),
            None,
            "global-locals.scm should not match locals pattern"
        );
        assert_eq!(
            infer_query_kind("custom-highlights.scm"),
            None,
            "custom-highlights.scm should not match highlights pattern"
        );
        // Files with multiple dashes before the pattern should also not match
        assert_eq!(
            infer_query_kind("very-local-injections.scm"),
            None,
            "very-local-injections.scm should not match"
        );
    }

    #[test]
    fn should_parse_aggregation_config_from_json() {
        let json = r#"{"priorities": ["server_a", "server_b"]}"#;
        let config: AggregationConfig = serde_json::from_str(json).unwrap();
        assert_eq!(
            config.priorities,
            Some(vec!["server_a".to_string(), "server_b".to_string()])
        );
    }

    #[test]
    fn should_parse_aggregation_config_empty_priorities() {
        let json = r#"{}"#;
        let config: AggregationConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.priorities, None);
    }

    #[test]
    fn should_parse_aggregation_config_explicit_empty_priorities() {
        let json = r#"{"priorities": []}"#;
        let config: AggregationConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.priorities, Some(vec![]));
    }

    #[test]
    fn should_parse_aggregation_config_from_toml() {
        let toml_str = r#"priorities = ["server_a"]"#;
        let config: AggregationConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.priorities, Some(vec!["server_a".to_string()]));
    }

    #[test]
    fn should_resolve_aggregation_priorities_for_specific_method() {
        let config = BridgeLanguageConfig {
            enabled: Some(true),
            aggregation: Some(HashMap::from([
                (
                    "textDocument/completion".to_string(),
                    AggregationConfig {
                        priorities: Some(vec!["server_a".to_string()]),
                        ..Default::default()
                    },
                ),
                (
                    WILDCARD_KEY.to_string(),
                    AggregationConfig {
                        priorities: Some(vec!["server_b".to_string()]),
                        ..Default::default()
                    },
                ),
            ])),
        };
        let agg = config.resolve_aggregation("textDocument/completion");
        assert_eq!(agg.priorities, &["server_a".to_string()]);
    }

    // ==========================================================================
    // Host bridging (host-document-bridge)
    // ==========================================================================

    #[test]
    fn host_bridging_is_disabled_by_default() {
        assert!(!LanguageSettings::default().is_host_bridging_enabled());
    }

    #[test]
    fn host_bridging_enabled_by_explicit_self_entry() {
        let settings = LanguageSettings {
            bridge: Some(HashMap::from([(
                HOST_BRIDGE_KEY.to_string(),
                BridgeLanguageConfig {
                    enabled: Some(true),
                    aggregation: None,
                },
            )])),
            ..Default::default()
        };
        assert!(settings.is_host_bridging_enabled());
    }

    #[test]
    fn host_bridging_not_enabled_by_bridge_wildcard() {
        // The `_` wildcard's enabled = true is the VIRT default; it must not
        // silently turn host bridging on (host-document-bridge: the built-in
        // `_self.enabled = false` default is key-specific and wins).
        let settings = LanguageSettings {
            bridge: Some(HashMap::from([(
                WILDCARD_KEY.to_string(),
                BridgeLanguageConfig {
                    enabled: Some(true),
                    aggregation: None,
                },
            )])),
            ..Default::default()
        };
        assert!(!settings.is_host_bridging_enabled());
    }

    #[test]
    fn host_bridging_explicit_false_stays_off() {
        let settings = LanguageSettings {
            bridge: Some(HashMap::from([(
                HOST_BRIDGE_KEY.to_string(),
                BridgeLanguageConfig {
                    enabled: Some(false),
                    aggregation: None,
                },
            )])),
            ..Default::default()
        };
        assert!(!settings.is_host_bridging_enabled());
    }

    #[test]
    fn host_aggregation_inherits_from_bridge_wildcard() {
        // Aggregation fields (unlike `enabled`) DO inherit from `_`.
        let settings = LanguageSettings {
            bridge: Some(HashMap::from([
                (
                    HOST_BRIDGE_KEY.to_string(),
                    BridgeLanguageConfig {
                        enabled: Some(true),
                        aggregation: None,
                    },
                ),
                (
                    WILDCARD_KEY.to_string(),
                    BridgeLanguageConfig {
                        enabled: None,
                        aggregation: Some(HashMap::from([(
                            "_".to_string(),
                            AggregationConfig {
                                priorities: Some(vec!["marksman".to_string()]),
                                ..Default::default()
                            },
                        )])),
                    },
                ),
            ])),
            ..Default::default()
        };
        let agg = settings.resolve_host_aggregation("textDocument/definition");
        assert_eq!(agg.priorities, vec!["marksman".to_string()]);
    }

    #[test]
    fn host_aggregation_defaults_to_wildcard_priorities_when_unconfigured() {
        let settings = LanguageSettings::default();
        let agg = settings.resolve_host_aggregation("textDocument/definition");
        assert_eq!(agg.priorities, vec![PRIORITIES_WILDCARD.to_string()]);
    }

    // ==========================================================================
    // Cross-layer aggregation (cross-layer-aggregation)
    // ==========================================================================

    #[test]
    fn layer_source_deserializes_lowercase() {
        let priorities: Vec<LayerSource> = serde_json::from_str(r#"["virt", "host", "native"]"#)
            .expect("lowercase layer names must parse");
        assert_eq!(
            priorities,
            vec![LayerSource::Virt, LayerSource::Host, LayerSource::Native]
        );
    }

    #[test]
    fn layer_source_rejects_unknown_names() {
        // Closed enum: a typo is a deserialization error, not a server that
        // never responds (contrast with stage-1 priorities strings).
        let result: std::result::Result<Vec<LayerSource>, _> = serde_json::from_str(r#"["virtt"]"#);
        assert!(result.is_err(), "unknown layer names must fail to parse");
    }

    #[test]
    fn resolve_layers_defaults_to_virt_host_native() {
        let settings = LanguageSettings::default();
        let resolved = settings.resolve_layers("textDocument/hover");
        assert_eq!(
            resolved.priorities,
            vec![LayerSource::Virt, LayerSource::Host, LayerSource::Native]
        );
        assert_eq!(resolved.strategy, AggregationStrategy::Preferred);
    }

    #[test]
    fn resolve_layers_strategy_defaults_per_method() {
        let settings = LanguageSettings::default();
        let resolved = settings.resolve_layers("textDocument/diagnostic");
        assert_eq!(resolved.strategy, AggregationStrategy::Concatenated);
    }

    #[test]
    fn resolve_layers_formatting_defaults_to_concatenated() {
        // Layer-level formatting is a sequential pipeline (virt regions
        // first, then the host formatter) — every producing layer should
        // contribute by default. Contrast with the bridge level, where
        // concatenating whole-document edits from multiple servers of one
        // target would conflict, so its default stays preferred.
        let settings = LanguageSettings::default();
        let resolved = settings.resolve_layers("textDocument/formatting");
        assert_eq!(resolved.strategy, AggregationStrategy::Concatenated);
    }

    #[test]
    fn resolve_bridge_aggregation_formatting_stays_preferred() {
        let config = BridgeLanguageConfig::default();
        let agg = config.resolve_aggregation("textDocument/formatting");
        assert_eq!(agg.strategy, AggregationStrategy::Preferred);
    }

    #[test]
    fn resolve_layers_uses_method_specific_entry() {
        let settings = LanguageSettings {
            layers: Some(LayersConfig {
                aggregation: Some(HashMap::from([(
                    "textDocument/hover".to_string(),
                    LayerAggregationConfig {
                        priorities: Some(vec![LayerSource::Host, LayerSource::Virt]),
                        strategy: None,
                    },
                )])),
            }),
            ..Default::default()
        };
        let resolved = settings.resolve_layers("textDocument/hover");
        assert_eq!(
            resolved.priorities,
            vec![LayerSource::Host, LayerSource::Virt]
        );
        assert!(
            !resolved.allows(LayerSource::Native),
            "omitted layer is excluded"
        );
    }

    #[test]
    fn resolve_layers_method_entry_inherits_unset_fields_from_method_wildcard() {
        // Field-level wildcard merge: the method entry sets strategy only,
        // the "_" entry supplies order.
        let settings = LanguageSettings {
            layers: Some(LayersConfig {
                aggregation: Some(HashMap::from([
                    (
                        WILDCARD_KEY.to_string(),
                        LayerAggregationConfig {
                            priorities: Some(vec![LayerSource::Native]),
                            strategy: None,
                        },
                    ),
                    (
                        "textDocument/hover".to_string(),
                        LayerAggregationConfig {
                            priorities: None,
                            strategy: Some(AggregationStrategy::Concatenated),
                        },
                    ),
                ])),
            }),
            ..Default::default()
        };
        let resolved = settings.resolve_layers("textDocument/hover");
        assert_eq!(resolved.priorities, vec![LayerSource::Native]);
        assert_eq!(resolved.strategy, AggregationStrategy::Concatenated);
    }

    #[test]
    fn resolve_layers_method_priorities_replace_wildcard_priorities_wholesale() {
        let settings = LanguageSettings {
            layers: Some(LayersConfig {
                aggregation: Some(HashMap::from([
                    (
                        WILDCARD_KEY.to_string(),
                        LayerAggregationConfig {
                            priorities: Some(vec![LayerSource::Native, LayerSource::Host]),
                            strategy: None,
                        },
                    ),
                    (
                        "textDocument/hover".to_string(),
                        LayerAggregationConfig {
                            priorities: Some(vec![LayerSource::Virt]),
                            strategy: None,
                        },
                    ),
                ])),
            }),
            ..Default::default()
        };
        let resolved = settings.resolve_layers("textDocument/hover");
        assert_eq!(
            resolved.priorities,
            vec![LayerSource::Virt],
            "priorities is a single field: the method entry replaces the wildcard list, no element merge"
        );
    }

    #[test]
    fn resolve_layers_empty_priorities_disable_all_layers() {
        let settings = LanguageSettings {
            layers: Some(LayersConfig {
                aggregation: Some(HashMap::from([(
                    WILDCARD_KEY.to_string(),
                    LayerAggregationConfig {
                        priorities: Some(vec![]),
                        strategy: None,
                    },
                )])),
            }),
            ..Default::default()
        };
        let resolved = settings.resolve_layers("textDocument/hover");
        assert!(resolved.priorities.is_empty());
        assert!(!resolved.allows(LayerSource::Virt));
    }

    #[test]
    fn resolve_layers_dedups_repeated_layers_first_occurrence_wins() {
        let settings = LanguageSettings {
            layers: Some(LayersConfig {
                aggregation: Some(HashMap::from([(
                    WILDCARD_KEY.to_string(),
                    LayerAggregationConfig {
                        priorities: Some(vec![
                            LayerSource::Virt,
                            LayerSource::Native,
                            LayerSource::Virt,
                        ]),
                        strategy: None,
                    },
                )])),
            }),
            ..Default::default()
        };
        let resolved = settings.resolve_layers("textDocument/hover");
        assert_eq!(
            resolved.priorities,
            vec![LayerSource::Virt, LayerSource::Native],
            "a repeated layer must not be walked twice"
        );
    }

    #[test]
    fn layer_aggregation_config_parses_from_toml() {
        let toml_str = r#"
            priorities = ["host", "virt"]
            strategy = "preferred"
        "#;
        let config: LayerAggregationConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(
            config.priorities,
            Some(vec![LayerSource::Host, LayerSource::Virt])
        );
        assert_eq!(config.strategy, Some(AggregationStrategy::Preferred));
    }

    #[test]
    fn should_resolve_aggregation_priorities_falls_back_to_wildcard() {
        let config = BridgeLanguageConfig {
            enabled: Some(true),
            aggregation: Some(HashMap::from([(
                WILDCARD_KEY.to_string(),
                AggregationConfig {
                    priorities: Some(vec!["server_b".to_string()]),
                    ..Default::default()
                },
            )])),
        };
        let agg = config.resolve_aggregation("textDocument/hover");
        assert_eq!(agg.priorities, &["server_b".to_string()]);
    }

    #[test]
    fn should_resolve_aggregation_priorities_defaults_to_wildcard_when_no_aggregation() {
        // Absent priorities resolve to ["*"] — all servers, first-win
        // (aggregation-priorities-wildcard). NOT [] — that now disables
        // fan-out for the method.
        let config = BridgeLanguageConfig::default();
        let agg = config.resolve_aggregation("textDocument/hover");
        assert_eq!(agg.priorities, vec![PRIORITIES_WILDCARD.to_string()]);
    }

    #[test]
    fn should_resolve_aggregation_priorities_preserves_explicit_empty_list() {
        // Some(vec![]) is the per-method fan-out kill switch and must survive
        // resolution verbatim, not be replaced with the ["*"] default.
        let config = BridgeLanguageConfig {
            enabled: Some(true),
            aggregation: Some(HashMap::from([(
                "textDocument/hover".to_string(),
                AggregationConfig {
                    priorities: Some(vec![]),
                    ..Default::default()
                },
            )])),
        };
        let agg = config.resolve_aggregation("textDocument/hover");
        assert!(agg.priorities.is_empty());
    }

    #[test]
    fn should_resolve_aggregation_strategy_for_specific_method() {
        let config = BridgeLanguageConfig {
            enabled: Some(true),
            aggregation: Some(HashMap::from([(
                "textDocument/diagnostic".to_string(),
                AggregationConfig {
                    strategy: Some(AggregationStrategy::Preferred),
                    ..Default::default()
                },
            )])),
        };
        let agg = config.resolve_aggregation("textDocument/diagnostic");
        assert_eq!(agg.strategy, AggregationStrategy::Preferred);
    }

    #[test]
    fn should_resolve_aggregation_strategy_falls_back_to_wildcard() {
        let config = BridgeLanguageConfig {
            enabled: Some(true),
            aggregation: Some(HashMap::from([(
                WILDCARD_KEY.to_string(),
                AggregationConfig {
                    strategy: Some(AggregationStrategy::Concatenated),
                    ..Default::default()
                },
            )])),
        };
        let agg = config.resolve_aggregation("textDocument/hover");
        assert_eq!(agg.strategy, AggregationStrategy::Concatenated);
    }

    #[test]
    fn should_resolve_aggregation_strategy_returns_default_when_no_aggregation() {
        // No aggregation config at all → method-specific handler default.
        let config = BridgeLanguageConfig::default();
        let agg = config.resolve_aggregation("textDocument/diagnostic");
        assert_eq!(agg.strategy, AggregationStrategy::Concatenated);
    }

    #[test]
    fn should_resolve_aggregation_strategy_uses_handler_default_when_strategy_is_none() {
        // Entry exists but strategy = None → falls back to the handler default.
        let config = BridgeLanguageConfig {
            enabled: Some(true),
            aggregation: Some(HashMap::from([(
                "textDocument/diagnostic".to_string(),
                AggregationConfig {
                    priorities: Some(vec!["server_a".to_string()]),
                    strategy: None,
                    ..Default::default()
                },
            )])),
        };
        let agg = config.resolve_aggregation("textDocument/diagnostic");
        assert_eq!(agg.strategy, AggregationStrategy::Concatenated);
    }

    #[test]
    fn should_resolve_aggregation_max_fan_out_for_specific_method() {
        let config = BridgeLanguageConfig {
            enabled: Some(true),
            aggregation: Some(HashMap::from([
                (
                    "textDocument/completion".to_string(),
                    AggregationConfig {
                        max_fan_out: Some(2),
                        ..Default::default()
                    },
                ),
                (
                    WILDCARD_KEY.to_string(),
                    AggregationConfig {
                        max_fan_out: Some(5),
                        ..Default::default()
                    },
                ),
            ])),
        };
        // Method-specific should win over wildcard
        let agg = config.resolve_aggregation("textDocument/completion");
        assert_eq!(agg.max_fan_out, Some(2));
    }

    #[test]
    fn should_resolve_aggregation_max_fan_out_falls_back_to_wildcard() {
        let config = BridgeLanguageConfig {
            enabled: Some(true),
            aggregation: Some(HashMap::from([(
                WILDCARD_KEY.to_string(),
                AggregationConfig {
                    max_fan_out: Some(3),
                    ..Default::default()
                },
            )])),
        };
        let agg = config.resolve_aggregation("textDocument/hover");
        assert_eq!(agg.max_fan_out, Some(3));
    }

    #[test]
    fn should_resolve_aggregation_max_fan_out_none_when_no_aggregation() {
        let config = BridgeLanguageConfig::default();
        let agg = config.resolve_aggregation("textDocument/hover");
        assert_eq!(agg.max_fan_out, None);
    }

    #[test]
    fn should_resolve_aggregation_entry_inherits_missing_fields_from_wildcard() {
        // When a method-specific AggregationConfig exists but has max_fan_out: None,
        // the wildcard's max_fan_out IS applied via field-level merge (wildcard-config-inheritance).
        let config = BridgeLanguageConfig {
            enabled: Some(true),
            aggregation: Some(HashMap::from([
                (
                    "textDocument/completion".to_string(),
                    AggregationConfig {
                        priorities: Some(vec!["server_a".to_string()]),
                        ..Default::default() // max_fan_out: None
                    },
                ),
                (
                    WILDCARD_KEY.to_string(),
                    AggregationConfig {
                        max_fan_out: Some(5),
                        ..Default::default()
                    },
                ),
            ])),
        };
        let agg = config.resolve_aggregation("textDocument/completion");
        assert_eq!(
            agg.max_fan_out,
            Some(5),
            "method-specific entry without maxFanOut should inherit from wildcard via field-level merge"
        );
    }

    #[test]
    fn should_resolve_aggregation_max_fan_out_negative_as_none() {
        let config = BridgeLanguageConfig {
            enabled: Some(true),
            aggregation: Some(HashMap::from([(
                WILDCARD_KEY.to_string(),
                AggregationConfig {
                    max_fan_out: Some(-1),
                    ..Default::default()
                },
            )])),
        };
        let agg = config.resolve_aggregation("textDocument/hover");
        assert_eq!(agg.max_fan_out, None);
    }

    #[test]
    fn should_resolve_aggregation_max_fan_out_zero_as_some_zero() {
        let config = BridgeLanguageConfig {
            enabled: Some(true),
            aggregation: Some(HashMap::from([(
                WILDCARD_KEY.to_string(),
                AggregationConfig {
                    max_fan_out: Some(0),
                    ..Default::default()
                },
            )])),
        };
        let agg = config.resolve_aggregation("textDocument/hover");
        assert_eq!(agg.max_fan_out, Some(0));
    }

    #[test]
    fn should_resolve_aggregation_all_fields_together() {
        let config = BridgeLanguageConfig {
            enabled: Some(true),
            aggregation: Some(HashMap::from([(
                "textDocument/completion".to_string(),
                AggregationConfig {
                    strategy: Some(AggregationStrategy::Concatenated),
                    priorities: Some(vec!["server_a".to_string(), "server_b".to_string()]),
                    max_fan_out: Some(3),
                    ..Default::default()
                },
            )])),
        };
        let agg = config.resolve_aggregation("textDocument/completion");
        assert_eq!(agg.strategy, AggregationStrategy::Concatenated);
        assert_eq!(agg.priorities, vec!["server_a", "server_b"]);
        assert_eq!(agg.max_fan_out, Some(3));
    }

    #[test]
    fn should_resolve_aggregation_with_defaults_returns_preferred() {
        let agg = ResolvedAggregationConfig::with_defaults();
        assert_eq!(agg.strategy, AggregationStrategy::Preferred);
        assert_eq!(
            agg.priorities,
            vec![PRIORITIES_WILDCARD.to_string()],
            "default priorities must be [\"*\"] (all servers), not [] (disabled)"
        );
        assert_eq!(agg.max_fan_out, None);
    }

    #[test]
    fn resolve_aggregation_fallbacks_default_true_and_honor_explicit() {
        // Absent → both default true (every server contributes to both paths).
        let none =
            BridgeLanguageConfig::default().resolve_aggregation("textDocument/publishDiagnostics");
        assert!(none.pull_fallback, "absent pullFallback resolves to true");
        assert!(none.push_fallback, "absent pushFallback resolves to true");

        // Explicit false on each toggle survives resolution independently.
        let config = BridgeLanguageConfig {
            enabled: Some(true),
            aggregation: Some(HashMap::from([(
                "textDocument/publishDiagnostics".to_string(),
                AggregationConfig {
                    pull_fallback: Some(false),
                    push_fallback: Some(false),
                    ..Default::default()
                },
            )])),
        };
        let agg = config.resolve_aggregation("textDocument/publishDiagnostics");
        assert!(
            !agg.pull_fallback,
            "explicit pullFallback = false is honored"
        );
        assert!(
            !agg.push_fallback,
            "explicit pushFallback = false is honored"
        );
    }

    #[test]
    fn aggregation_config_deserializes_fallback_toggles_as_camel_case() {
        // The wire form is camelCase (push-propagation-diagnostic-forwarding):
        // `pullFallback` / `pushFallback`, both optional.
        let cfg: AggregationConfig =
            serde_json::from_str(r#"{ "pullFallback": false, "pushFallback": true }"#).unwrap();
        assert_eq!(cfg.pull_fallback, Some(false));
        assert_eq!(cfg.push_fallback, Some(true));

        // Absent toggles stay `None` (inherit), distinct from an explicit value.
        let absent: AggregationConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(absent.pull_fallback, None);
        assert_eq!(absent.push_fallback, None);
    }

    #[test]
    fn should_parse_bridge_language_config_with_aggregation() {
        let json = r#"{
            "enabled": true,
            "aggregation": {
                "textDocument/completion": { "priorities": ["server_a"] },
                "_": { "priorities": ["server_b"] }
            }
        }"#;
        let config: BridgeLanguageConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.enabled, Some(true));
        let agg = config.aggregation.as_ref().unwrap();
        assert_eq!(agg.len(), 2);
        assert_eq!(
            agg["textDocument/completion"].priorities,
            Some(vec!["server_a".to_string()])
        );
        assert_eq!(agg["_"].priorities, Some(vec!["server_b".to_string()]));
    }

    #[test]
    fn should_parse_bridge_language_config_enabled_defaults_to_none() {
        // When enabled is omitted, it should be None (for wildcard inheritance)
        let json = r#"{}"#;
        let config: BridgeLanguageConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.enabled, None);
        assert!(config.aggregation.is_none());
    }

    #[test]
    fn should_parse_aggregation_strategy_preferred() {
        let json = r#"{ "strategy": "preferred" }"#;
        let config: AggregationConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.strategy, Some(AggregationStrategy::Preferred));
    }

    #[test]
    fn should_parse_aggregation_strategy_concatenated() {
        let json = r#"{ "strategy": "concatenated" }"#;
        let config: AggregationConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.strategy, Some(AggregationStrategy::Concatenated));
    }

    #[test]
    fn should_parse_aggregation_config_without_strategy() {
        let json = r#"{}"#;
        let config: AggregationConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.strategy, None);
    }

    #[test]
    fn should_parse_max_fan_out_with_value() {
        let json = r#"{ "maxFanOut": 2 }"#;
        let config: AggregationConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.max_fan_out, Some(2));
    }

    #[test]
    fn should_parse_max_fan_out_null_as_none() {
        let json = r#"{ "maxFanOut": null }"#;
        let config: AggregationConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.max_fan_out, None);
    }

    #[test]
    fn should_parse_max_fan_out_absent_as_none() {
        let json = r#"{}"#;
        let config: AggregationConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.max_fan_out, None);
    }

    #[test]
    fn should_parse_aggregation_strategy_from_toml() {
        let toml_str = r#"strategy = "preferred""#;
        let config: AggregationConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.strategy, Some(AggregationStrategy::Preferred));
    }

    #[test]
    fn should_parse_language_config_with_aliases() {
        // aliases allows mapping multiple languageId values to one parser
        // Example: markdown parser handles rmd, qmd, mdx files
        let config_toml = r#"
            parser = "/path/to/markdown.so"
            aliases = ["rmd", "qmd", "mdx"]

            [[queries]]
            path = "/path/to/highlights.scm"
            kind = "highlights"
        "#;

        let config: LanguageSettings = toml::from_str(config_toml).unwrap();

        assert!(config.aliases.is_some(), "aliases field should be present");
        let aliases = config.aliases.unwrap();
        assert_eq!(aliases.len(), 3);
        assert_eq!(aliases[0], "rmd");
        assert_eq!(aliases[1], "qmd");
        assert_eq!(aliases[2], "mdx");
    }

    #[test]
    fn should_parse_language_config_without_aliases() {
        // When aliases is omitted, it should be None
        let config_toml = r#"
            parser = "/path/to/rust.so"

            [[queries]]
            path = "/path/to/highlights.scm"
            kind = "highlights"
        "#;

        let config: LanguageSettings = toml::from_str(config_toml).unwrap();

        assert!(config.aliases.is_none(), "omitted aliases should be None");
    }

    #[test]
    fn should_parse_language_config_with_empty_aliases() {
        // Empty aliases array should be Some([])
        let config_toml = r#"
            parser ="/path/to/parser.so"
            aliases = []
        "#;

        let config: LanguageSettings = toml::from_str(config_toml).unwrap();

        assert!(config.aliases.is_some(), "empty aliases should be Some");
        assert!(
            config.aliases.as_ref().unwrap().is_empty(),
            "should be empty vec"
        );
    }

    #[test]
    fn should_parse_language_config_with_base() {
        let config_toml = r#"
            base = "markdown"
        "#;

        let config: LanguageSettings = toml::from_str(config_toml).unwrap();

        assert_eq!(config.base, Some("markdown".to_string()));
        assert!(config.parser.is_none());
    }

    #[test]
    fn should_parse_language_config_without_base() {
        let config_toml = r#"
            parser = "/path/to/rust.so"
        "#;

        let config: LanguageSettings = toml::from_str(config_toml).unwrap();

        assert!(config.base.is_none());
    }

    #[test]
    fn should_resolve_aggregation_defaults_to_preferred_without_explicit_strategy() {
        // When no aggregation config exists at all, the default strategy should be
        // Preferred — the hardcoded default when no explicit strategy is configured.
        let config = BridgeLanguageConfig {
            enabled: Some(true),
            aggregation: None,
        };
        let agg = config.resolve_aggregation("textDocument/hover");
        assert_eq!(agg.strategy, AggregationStrategy::Preferred);
        assert_eq!(agg.priorities, vec![PRIORITIES_WILDCARD.to_string()]);
        assert_eq!(agg.max_fan_out, None);
    }

    #[test]
    fn default_strategy_is_concatenated_for_diagnostics() {
        assert_eq!(
            default_aggregation_strategy_for_method("textDocument/diagnostic"),
            AggregationStrategy::Concatenated
        );
        assert_eq!(
            default_aggregation_strategy_for_method("textDocument/publishDiagnostics"),
            AggregationStrategy::Concatenated
        );
    }

    #[test]
    fn default_strategy_is_preferred_for_other_methods() {
        assert_eq!(
            default_aggregation_strategy_for_method("textDocument/hover"),
            AggregationStrategy::Preferred
        );
        assert_eq!(
            default_aggregation_strategy_for_method("textDocument/completion"),
            AggregationStrategy::Preferred
        );
    }
}
