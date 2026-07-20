//! Development-only process boundary for the tree-worker prototype.
//!
//! The protocol deliberately returns owned, serializable tree-derived data.
//! Tree-sitter pointer-bearing values never cross this module's transport.

use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};
use std::{
    cell::RefCell,
    collections::{HashMap, VecDeque},
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};

use crate::language::node_tracker::{EditInfo, NodeTracker};

pub const PROTOCOL_VERSION: u32 = 15;
pub const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_DOCUMENT_REPLICAS: usize = 4_096;
pub const MAX_DOCUMENT_BYTES: usize = 32 * 1024 * 1024;
pub const MAX_RETAINED_DOCUMENT_BYTES: usize = 512 * 1024 * 1024;
// Independent hard admission bound for retained source allocation plus the
// conservative host-Tree weight. Recomputable caches use the lower soft cap.
const MAX_RETAINED_ESTIMATED_DOCUMENT_BYTES: usize = 512 * 1024 * 1024;
// Preserve ordinary exact-version hits, but stop a multi-megabyte semantic
// result from retaining a second full set of injection-derived caches. Stage 15
// measured 74 KiB for the small fixture and 2.35 MiB for the pressure fixture.
const SEMANTIC_CACHE_AUXILIARY_TRIM_THRESHOLD_BYTES: usize = 2 * 1024 * 1024;
// tree-sitter does not expose allocation size. Admission therefore assigns a
// conservative portable weight per retained subtree node; Stage 17 compares
// the estimate with process footprint rather than treating it as measured RSS.
const TREE_NODE_ADMISSION_BYTES: usize = 64;
// Soft cap for recomputable worker-owned caches. This is deliberately below
// the 512 MiB retained-text guard so cache growth cannot consume the whole
// worker allowance; Stage 17 measurement validates the initial quarter-share.
const MAX_RETAINED_DERIVED_BYTES: usize = 128 * 1024 * 1024;
pub const BUILD_ID: &str = concat!(env!("CARGO_PKG_NAME"), "-", env!("CARGO_PKG_VERSION"));

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct WorkerMemoryBudgets {
    derived_cache_soft_bytes: usize,
    non_evictable_estimate_hard_bytes: usize,
}

impl WorkerMemoryBudgets {
    fn new(
        derived_cache_soft_bytes: usize,
        non_evictable_estimate_hard_bytes: usize,
    ) -> Result<Self, String> {
        if derived_cache_soft_bytes == 0 {
            return Err("worker derived-cache soft budget must be positive".into());
        }
        if non_evictable_estimate_hard_bytes == 0 {
            return Err("worker non-evictable estimate hard budget must be positive".into());
        }
        Ok(Self {
            derived_cache_soft_bytes,
            non_evictable_estimate_hard_bytes,
        })
    }
}

impl Default for WorkerMemoryBudgets {
    fn default() -> Self {
        Self {
            derived_cache_soft_bytes: MAX_RETAINED_DERIVED_BYTES,
            non_evictable_estimate_hard_bytes: MAX_RETAINED_ESTIMATED_DOCUMENT_BYTES,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RequestContext {
    pub request_id: u64,
    pub worker_generation: u64,
    pub uri: String,
    pub incarnation: u64,
    pub content_version: u64,
    pub configuration_generation: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Handshake {
    pub protocol_version: u32,
    pub build_id: String,
    pub worker_generation: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DeriveSnapshot {
    pub context: RequestContext,
    pub language: String,
    pub grammar_symbol: String,
    pub parser_path: PathBuf,
    pub artifact_digest: String,
    pub text: String,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkerQuerySources {
    pub highlights: Option<String>,
    pub bindings: Option<String>,
    pub injections: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkerLanguageAsset {
    pub language: String,
    pub grammar_symbol: String,
    pub source_path: PathBuf,
    pub parser_path: PathBuf,
    pub artifact_digest: String,
    pub queries: WorkerQuerySources,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkerLanguageCatalog {
    pub assets: Vec<WorkerLanguageAsset>,
    pub search_paths: Vec<PathBuf>,
    pub capture_mappings: crate::config::settings::CaptureMappings,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ConfigureLanguages {
    pub context: RequestContext,
    pub catalog: WorkerLanguageCatalog,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SyncDocument {
    pub context: RequestContext,
    pub language: String,
    pub grammar_symbol: String,
    pub source_path: PathBuf,
    pub parser_path: PathBuf,
    pub artifact_digest: String,
    pub queries: WorkerQuerySources,
    pub text: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ByteEdit {
    pub start_byte: usize,
    pub old_end_byte: usize,
    pub new_text: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ApplyDocumentEdits {
    pub context: RequestContext,
    pub base_version: u64,
    /// Ordered edits whose byte ranges address the text produced by all prior
    /// entries, matching the sequential semantics of LSP content changes.
    pub edits: Vec<ByteEdit>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ApplyDocumentEditsAndDerive {
    pub context: RequestContext,
    pub base_version: u64,
    /// Ordered edits whose byte ranges address the text produced by all prior
    /// entries, matching the sequential semantics of LSP content changes.
    pub edits: Vec<ByteEdit>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DeriveDocumentSnapshot {
    pub context: RequestContext,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ResolveNode {
    pub context: RequestContext,
    pub byte_offset: usize,
    pub named: bool,
    pub layer: NodeLayerSelector,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeLayerSelector {
    Host,
    Deepest,
    Index(i64),
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct NavigateNode {
    pub context: RequestContext,
    pub node_id: OpaqueNodeId,
    pub operation: NodeNavigation,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RunNodeScalar {
    pub context: RequestContext,
    pub node_id: OpaqueNodeId,
    pub operation: NodeScalarOperation,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DeriveSelectionRanges {
    pub context: RequestContext,
    pub positions: Vec<WirePosition>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DeriveInjectionRegions {
    pub context: RequestContext,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DeriveSemanticTokens {
    pub context: RequestContext,
    pub supports_multiline: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RunCaptures {
    pub context: RequestContext,
    pub kind: String,
    pub range: Option<WireRange>,
    pub injection: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DeriveNativeBindings {
    pub context: RequestContext,
    pub position: WirePosition,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeScalarOperation {
    Kind,
    GrammarName,
    IsNamed,
    IsExtra,
    HasError,
    IsError,
    IsMissing,
    StartByte,
    EndByte,
    ByteRange,
    ChildCount,
    NamedChildCount,
    DescendantCount,
    SExpression,
    Text,
    FieldNameForChild { index: u32, named: bool },
    StartPosition,
    EndPosition,
    Range,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum NodeScalarValue {
    String(String),
    Bool(bool),
    U64(u64),
    ByteRange {
        start_byte: u64,
        end_byte: u64,
    },
    NullableString(Option<String>),
    Position(WirePosition),
    Range {
        start: WirePosition,
        end: WirePosition,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeNavigation {
    Parent,
    Children,
    NamedChildren,
    Child {
        index: u32,
    },
    NamedChild {
        index: u32,
    },
    NextSibling,
    PreviousSibling,
    NextNamedSibling,
    PreviousNamedSibling,
    FirstChildForByte {
        byte: usize,
    },
    DescendantForByteRange {
        start_byte: usize,
        end_byte: usize,
        named: bool,
    },
    ChildByFieldName {
        name: String,
    },
    ChildrenByFieldName {
        name: String,
    },
    DescendantForPointRange {
        start: WirePosition,
        end: WirePosition,
        named: bool,
    },
    ChildWithDescendant {
        descendant_id: OpaqueNodeId,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WirePosition {
    pub line: u32,
    pub character: u32,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WireRange {
    pub start: WirePosition,
    pub end: WirePosition,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WireByteRange {
    pub start: usize,
    pub end: usize,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WireSelectionRange {
    pub range: WireRange,
    pub parent: Option<Box<WireSelectionRange>>,
}

#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct OpaqueNodeId {
    pub worker_generation: u64,
    pub local_id: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct OwnedNode {
    pub id: OpaqueNodeId,
    pub kind: String,
    pub start_byte: usize,
    pub end_byte: usize,
    pub named: bool,
    pub has_error: bool,
    pub layer: usize,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct NodeResult {
    pub context: RequestContext,
    pub nodes: Vec<OwnedNode>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct NodeScalarResult {
    pub context: RequestContext,
    pub value: Option<NodeScalarValue>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SelectionRangesResult {
    pub context: RequestContext,
    pub ranges: Vec<WireSelectionRange>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WireInjectionRegion {
    pub language: String,
    pub byte_start: usize,
    pub byte_end: usize,
    pub line_start: u32,
    pub line_end: u32,
    pub start_column: u32,
    pub region_id: String,
    pub content_hash: u64,
    pub virtual_content: String,
    pub line_column_offsets: Vec<u32>,
    pub contiguous: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct InjectionRegionsResult {
    pub context: RequestContext,
    pub regions: Vec<WireInjectionRegion>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DerivedSemanticTokens {
    pub context: RequestContext,
    pub tokens: Vec<WireSemanticToken>,
    pub queue_wait_ns: u64,
    pub compute_ns: u64,
    pub cache_hit: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WireSemanticToken {
    pub delta_line: u32,
    pub delta_start: u32,
    pub length: u32,
    pub token_type: u32,
    pub token_modifiers_bitset: u32,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CapturesResult {
    pub context: RequestContext,
    pub available: bool,
    #[serde(with = "wire_json_values")]
    pub matches: Vec<serde_json::Value>,
    #[serde(with = "wire_json_values")]
    pub skipped: Vec<serde_json::Value>,
}

/// Encode schemaless capture payloads without asking the binary codec to
/// implement serde's self-describing `deserialize_any` entry point.
mod wire_json_values {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    #[derive(Deserialize, Serialize)]
    enum WireJsonValue {
        Null,
        Bool(bool),
        Number(String),
        String(String),
        Array(Vec<WireJsonValue>),
        Object(Vec<(String, WireJsonValue)>),
    }

    impl From<&serde_json::Value> for WireJsonValue {
        fn from(value: &serde_json::Value) -> Self {
            match value {
                serde_json::Value::Null => Self::Null,
                serde_json::Value::Bool(value) => Self::Bool(*value),
                serde_json::Value::Number(value) => Self::Number(value.to_string()),
                serde_json::Value::String(value) => Self::String(value.clone()),
                serde_json::Value::Array(values) => {
                    Self::Array(values.iter().map(Self::from).collect())
                }
                serde_json::Value::Object(values) => Self::Object(
                    values
                        .iter()
                        .map(|(key, value)| (key.clone(), Self::from(value)))
                        .collect(),
                ),
            }
        }
    }

    impl WireJsonValue {
        fn into_json(self) -> Result<serde_json::Value, String> {
            Ok(match self {
                Self::Null => serde_json::Value::Null,
                Self::Bool(value) => serde_json::Value::Bool(value),
                Self::Number(value) => serde_json::Value::Number(
                    value
                        .parse()
                        .map_err(|error| format!("invalid JSON number in worker frame: {error}"))?,
                ),
                Self::String(value) => serde_json::Value::String(value),
                Self::Array(values) => serde_json::Value::Array(
                    values
                        .into_iter()
                        .map(Self::into_json)
                        .collect::<Result<_, _>>()?,
                ),
                Self::Object(values) => serde_json::Value::Object(
                    values
                        .into_iter()
                        .map(|(key, value)| Ok((key, value.into_json()?)))
                        .collect::<Result<_, String>>()?,
                ),
            })
        }
    }

    pub fn serialize<S>(values: &[serde_json::Value], serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        values
            .iter()
            .map(WireJsonValue::from)
            .collect::<Vec<_>>()
            .serialize(serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Vec<serde_json::Value>, D::Error>
    where
        D: Deserializer<'de>,
    {
        Vec::<WireJsonValue>::deserialize(deserializer)?
            .into_iter()
            .map(|value| value.into_json().map_err(serde::de::Error::custom))
            .collect()
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct NativeBindingsFacts {
    pub identifier: Option<WireByteRange>,
    pub definition: Option<WireByteRange>,
    pub references: Vec<WireByteRange>,
    pub definitions: Vec<WireByteRange>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct NativeBindingsResult {
    pub context: RequestContext,
    pub facts: Option<NativeBindingsFacts>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct LanguageCatalogAck {
    pub context: RequestContext,
    pub languages: usize,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CloseDocument {
    pub context: RequestContext,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DocumentAck {
    pub context: RequestContext,
    pub incremental: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DocumentClosed {
    pub context: RequestContext,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkerRestartRequired {
    pub context: RequestContext,
    pub reason: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CancelRequest {
    pub request_id: u64,
    pub worker_generation: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RequestCancelled {
    pub context: RequestContext,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum Request {
    Handshake(Handshake),
    Cancel(CancelRequest),
    DeriveSnapshot(DeriveSnapshot),
    SyncDocument(SyncDocument),
    ApplyDocumentEdits(ApplyDocumentEdits),
    ApplyDocumentEditsAndDerive(ApplyDocumentEditsAndDerive),
    DeriveDocumentSnapshot(DeriveDocumentSnapshot),
    ResolveNode(ResolveNode),
    NavigateNode(NavigateNode),
    RunNodeScalar(RunNodeScalar),
    DeriveSelectionRanges(DeriveSelectionRanges),
    DeriveInjectionRegions(DeriveInjectionRegions),
    DeriveSemanticTokens(DeriveSemanticTokens),
    RunCaptures(RunCaptures),
    DeriveNativeBindings(DeriveNativeBindings),
    ConfigureLanguages(ConfigureLanguages),
    CloseDocument(CloseDocument),
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct HandshakeReady {
    pub protocol_version: u32,
    pub build_id: String,
    pub worker_generation: u64,
    pub compute_threads: usize,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DerivedSnapshot {
    pub context: RequestContext,
    pub language: String,
    pub root_kind: String,
    pub root_start_byte: usize,
    pub root_end_byte: usize,
    pub has_error: bool,
    pub named_node_count: usize,
    pub parser_cache_hit: Option<bool>,
    pub queue_wait_ns: u64,
    pub compute_ns: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkerError {
    pub context: Option<RequestContext>,
    pub message: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum Response {
    HandshakeReady(HandshakeReady),
    DocumentAck(DocumentAck),
    DocumentClosed(DocumentClosed),
    WorkerRestartRequired(WorkerRestartRequired),
    RequestCancelled(RequestCancelled),
    Snapshot(DerivedSnapshot),
    Nodes(NodeResult),
    NodeScalar(NodeScalarResult),
    SelectionRanges(SelectionRangesResult),
    InjectionRegions(InjectionRegionsResult),
    SemanticTokens(DerivedSemanticTokens),
    Captures(CapturesResult),
    NativeBindings(NativeBindingsResult),
    LanguageCatalogAck(LanguageCatalogAck),
    Error(WorkerError),
}

struct DocumentReplica {
    context: RequestContext,
    language: String,
    grammar_key: GrammarKey,
    text: String,
    tree: tree_sitter::Tree,
    uri: url::Url,
    node_tracker: Arc<NodeTracker>,
    analysis: Arc<crate::language::LanguageCoordinator>,
    grammar_language: tree_sitter::Language,
    queries: WorkerQuerySources,
    captures_match_cache: crate::lsp::lsp_impl::kakehashi::captures_match_cache::CapturesMatchCache,
    injection_token_cache: Arc<crate::analysis::InjectionTokenCache>,
    injection_cache_edits: u8,
    cached_semantic_tokens: Option<(u64, bool, Arc<Vec<WireSemanticToken>>)>,
    semantic_discovery: Option<Arc<crate::document::DiscoveredInjections>>,
    resolved_injections: Option<(u64, Arc<Vec<crate::language::injection::ResolvedInjection>>)>,
    injection_layers: Vec<CachedInjectionLayer>,
}

struct CachedInjectionLayer {
    depth: usize,
    tree: tree_sitter::Tree,
    ranges: Vec<tree_sitter::Range>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct EstimatedDocumentMemory {
    non_evictable_bytes: usize,
    result_cache_bytes: usize,
    auxiliary_cache_bytes: usize,
}

impl EstimatedDocumentMemory {
    #[cfg(test)]
    fn evictable_bytes(self) -> usize {
        self.result_cache_bytes
            .saturating_add(self.auxiliary_cache_bytes)
    }
}

fn estimated_discovery_bytes(discovery: &crate::document::DiscoveredInjections) -> usize {
    std::mem::size_of::<crate::document::DiscoveredInjections>()
        .saturating_add(
            discovery.regions.capacity() * std::mem::size_of::<crate::document::DiscoveredRegion>(),
        )
        .saturating_add(discovery.regions.iter().fold(0, |bytes, region| {
            bytes
                .saturating_add(region.resolved_lang.capacity())
                .saturating_add(
                    region
                        .included_ranges
                        .as_ref()
                        .map(|ranges| ranges.capacity() * std::mem::size_of::<tree_sitter::Range>())
                        .unwrap_or_default(),
                )
                .saturating_add(region.prefix_byte_widths.capacity() * std::mem::size_of::<usize>())
        }))
}

fn estimated_resolved_injection_bytes(
    resolved: &[crate::language::injection::ResolvedInjection],
    capacity: usize,
) -> usize {
    std::mem::size_of::<Vec<crate::language::injection::ResolvedInjection>>()
        .saturating_add(
            capacity * std::mem::size_of::<crate::language::injection::ResolvedInjection>(),
        )
        .saturating_add(resolved.iter().fold(0, |bytes, injection| {
            bytes
                .saturating_add(injection.region.language.capacity())
                .saturating_add(injection.region.region_id.capacity())
                .saturating_add(injection.injection_language.capacity())
                .saturating_add(injection.virtual_content.capacity())
                .saturating_add(
                    injection.line_column_offsets.capacity() * std::mem::size_of::<u32>(),
                )
        }))
}

fn estimated_injection_layer_bytes(layers: &[CachedInjectionLayer], capacity: usize) -> usize {
    (capacity * std::mem::size_of::<CachedInjectionLayer>()).saturating_add(layers.iter().fold(
        0,
        |bytes, layer| {
            bytes
                .saturating_add(layer.ranges.capacity() * std::mem::size_of::<tree_sitter::Range>())
                .saturating_add(
                    layer
                        .tree
                        .root_node()
                        .descendant_count()
                        .saturating_mul(TREE_NODE_ADMISSION_BYTES),
                )
        },
    ))
}

fn estimated_non_evictable_bytes(text_capacity: usize, tree: &tree_sitter::Tree) -> usize {
    text_capacity.saturating_add(
        tree.root_node()
            .descendant_count()
            .saturating_mul(TREE_NODE_ADMISSION_BYTES),
    )
}

impl DocumentReplica {
    fn estimated_memory(&self) -> EstimatedDocumentMemory {
        EstimatedDocumentMemory {
            non_evictable_bytes: estimated_non_evictable_bytes(self.text.capacity(), &self.tree),
            result_cache_bytes: self
                .cached_semantic_tokens
                .as_ref()
                .map(|(_, _, tokens)| tokens.capacity() * std::mem::size_of::<WireSemanticToken>())
                .unwrap_or_default(),
            auxiliary_cache_bytes: self
                .semantic_discovery
                .as_deref()
                .map(estimated_discovery_bytes)
                .unwrap_or_default()
                .saturating_add(
                    self.resolved_injections
                        .as_ref()
                        .map(|(_, resolved)| {
                            estimated_resolved_injection_bytes(resolved, resolved.capacity())
                        })
                        .unwrap_or_default(),
                )
                .saturating_add(estimated_injection_layer_bytes(
                    &self.injection_layers,
                    self.injection_layers.capacity(),
                ))
                .saturating_add(
                    self.injection_token_cache
                        .estimated_document_bytes(&self.uri),
                )
                .saturating_add(
                    self.captures_match_cache
                        .estimated_document_bytes(&self.uri),
                ),
        }
    }

    fn trim_auxiliary_caches_for_pressure(&mut self) -> bool {
        let semantic_bytes = self
            .cached_semantic_tokens
            .as_ref()
            .map(|(_, _, tokens)| tokens.capacity() * std::mem::size_of::<WireSemanticToken>())
            .unwrap_or_default();
        if semantic_bytes <= SEMANTIC_CACHE_AUXILIARY_TRIM_THRESHOLD_BYTES {
            return false;
        }
        self.clear_auxiliary_caches()
    }

    fn clear_auxiliary_caches(&mut self) -> bool {
        let retained = self.estimated_memory().auxiliary_cache_bytes;
        self.semantic_discovery = None;
        self.resolved_injections = None;
        self.injection_layers.clear();
        self.injection_token_cache.clear_document(&self.uri);
        self.captures_match_cache.clear_document(&self.uri);
        retained > 0
    }

    fn clear_result_cache(&mut self) -> bool {
        self.cached_semantic_tokens.take().is_some()
    }

    fn sync(
        request: SyncDocument,
        parser: &mut tree_sitter::Parser,
    ) -> Result<(Self, DocumentAck), String> {
        Self::sync_with_analysis(
            request,
            parser,
            Arc::new(crate::language::LanguageCoordinator::new()),
        )
    }

    fn sync_with_analysis(
        request: SyncDocument,
        parser: &mut tree_sitter::Parser,
        analysis: Arc<crate::language::LanguageCoordinator>,
    ) -> Result<(Self, DocumentAck), String> {
        validate_document_size(request.text.len())?;
        let tree = parser
            .parse(request.text.as_bytes(), None)
            .ok_or_else(|| "parser returned no tree during document sync".to_string())?;
        let ack = DocumentAck {
            context: request.context.clone(),
            incremental: false,
        };
        let grammar_key = GrammarKey::from_sync(&request);
        let uri = url::Url::parse(&request.context.uri)
            .map_err(|error| format!("document URI is invalid: {error}"))?;
        let node_tracker = Arc::new(NodeTracker::new());
        node_tracker.open_incarnation(&uri, request.context.incarnation);
        let grammar_language = parser
            .language()
            .map(|language| (*language).clone())
            .ok_or_else(|| "parser has no configured language".to_string())?;
        // Parsing and node reads do not consume highlight/binding queries.
        // Compile only the injection query needed by the current fused
        // readers; the catalog path prepares the full query set lazily before
        // readers that need it. This keeps document sync and recovery cheap.
        worker_analysis(
            &analysis,
            &request.language,
            grammar_language.clone(),
            request.queries.clone(),
            WorkerQuerySet::Injections,
        )?;
        Ok((
            Self {
                context: request.context,
                language: request.language,
                grammar_key,
                text: request.text,
                tree,
                uri,
                node_tracker,
                analysis,
                grammar_language,
                queries: request.queries,
                captures_match_cache: Default::default(),
                injection_token_cache: Arc::new(crate::analysis::InjectionTokenCache::new()),
                injection_cache_edits: 0,
                cached_semantic_tokens: None,
                semantic_discovery: None,
                resolved_injections: None,
                injection_layers: Vec::new(),
            },
            ack,
        ))
    }

    fn apply(
        &mut self,
        request: ApplyDocumentEdits,
        parser: &mut tree_sitter::Parser,
        retained_bytes: Option<(&AtomicUsize, &AtomicUsize)>,
    ) -> Result<DocumentAck, String> {
        self.validate_identity(&request.context)?;
        if request.base_version != self.context.content_version {
            return Err(format!(
                "base version {} does not match {}",
                request.base_version, self.context.content_version
            ));
        }
        if request.context.content_version <= request.base_version {
            return Err("target content version must advance".into());
        }
        let mut text = self.text.clone();
        let mut edited_tree = self.tree.clone();
        let mut tracker_edits = Vec::with_capacity(request.edits.len());
        for edit in request.edits {
            if edit.start_byte > edit.old_end_byte
                || edit.old_end_byte > text.len()
                || !text.is_char_boundary(edit.start_byte)
                || !text.is_char_boundary(edit.old_end_byte)
            {
                return Err("edit range is not a valid UTF-8 byte range".into());
            }
            let start_position = point_at_byte(&text, edit.start_byte);
            let old_end_position = point_at_byte(&text, edit.old_end_byte);
            let new_end_byte = edit
                .start_byte
                .checked_add(edit.new_text.len())
                .ok_or_else(|| "edit length overflows byte offsets".to_string())?;
            tracker_edits.push(EditInfo::new(
                edit.start_byte,
                edit.old_end_byte,
                new_end_byte,
            ));
            text.replace_range(edit.start_byte..edit.old_end_byte, &edit.new_text);
            validate_document_size(text.len())?;
            let new_end_position = point_at_byte(&text, new_end_byte);
            edited_tree.edit(&tree_sitter::InputEdit {
                start_byte: edit.start_byte,
                old_end_byte: edit.old_end_byte,
                new_end_byte,
                start_position,
                old_end_position,
                new_end_position,
            });
        }
        let tree = parser
            .parse(text.as_bytes(), Some(&edited_tree))
            .ok_or_else(|| "parser returned no tree during incremental parse".to_string())?;
        if let Some((retained_text_bytes, retained_estimated_bytes)) = retained_bytes {
            replace_retained_bytes(retained_text_bytes, self.text.len(), text.len())?;
            let old_estimated = self.estimated_memory().non_evictable_bytes;
            let new_estimated = estimated_non_evictable_bytes(text.capacity(), &tree);
            if let Err(message) = replace_retained_estimated_bytes(
                retained_estimated_bytes,
                old_estimated,
                new_estimated,
                MAX_RETAINED_ESTIMATED_DOCUMENT_BYTES,
            ) {
                replace_retained_bytes(retained_text_bytes, text.len(), self.text.len()).expect(
                    "rolling back retained text after estimated admission cannot exceed its budget",
                );
                return Err(message);
            }
        }
        self.text = text;
        self.tree = tree;
        self.injection_layers.clear();
        self.semantic_discovery = None;
        self.resolved_injections = None;
        self.injection_cache_edits = self.injection_cache_edits.wrapping_add(1);
        if self.injection_cache_edits == 0 {
            self.injection_token_cache.clear_document(&self.uri);
        }
        self.node_tracker
            .apply_input_edits(&self.uri, &tracker_edits);
        self.context = request.context;
        self.cached_semantic_tokens = None;
        Ok(DocumentAck {
            context: self.context.clone(),
            incremental: true,
        })
    }

    fn derive(&self, context: RequestContext) -> Result<DerivedSnapshot, String> {
        self.derive_with_telemetry(context, None, Duration::ZERO, Instant::now())
    }

    fn derive_with_telemetry(
        &self,
        context: RequestContext,
        parser_cache_hit: Option<bool>,
        queue_wait: Duration,
        started: Instant,
    ) -> Result<DerivedSnapshot, String> {
        self.validate_identity(&context)?;
        if context.content_version != self.context.content_version {
            return Err(format!(
                "content version {} does not match {}",
                context.content_version, self.context.content_version
            ));
        }
        let root = self.tree.root_node();
        Ok(DerivedSnapshot {
            context,
            language: self.language.clone(),
            root_kind: root.kind().into(),
            root_start_byte: root.start_byte(),
            root_end_byte: root.end_byte(),
            has_error: root.has_error(),
            named_node_count: named_node_count(root),
            parser_cache_hit,
            queue_wait_ns: duration_ns(queue_wait),
            compute_ns: duration_ns(started.elapsed()),
        })
    }

    fn resolve_node(&mut self, request: ResolveNode) -> Result<NodeResult, String> {
        self.validate_read_identity(&request.context)?;
        if request.byte_offset > self.text.len() {
            return Err("node byte offset exceeds document length".into());
        }
        if self.text.is_empty() {
            return Ok(NodeResult {
                context: request.context,
                nodes: Vec::new(),
            });
        }
        let layer = match request.layer {
            NodeLayerSelector::Host => 0,
            NodeLayerSelector::Deepest | NodeLayerSelector::Index(_) => {
                self.cache_injection_stack(request.byte_offset);
                let depth = self
                    .injection_layers
                    .iter()
                    .filter(|layer| {
                        crate::lsp::lsp_impl::kakehashi::node::injection_stack::ranges_contain_byte(
                            &layer.ranges,
                            request.byte_offset,
                            self.text.len(),
                        )
                    })
                    .map(|layer| layer.depth)
                    .max()
                    .unwrap_or(0);
                match request.layer {
                    NodeLayerSelector::Deepest => depth,
                    NodeLayerSelector::Index(index) => {
                        let stack_len = depth.saturating_add(1);
                        let selected = if index >= 0 {
                            usize::try_from(index)
                                .ok()
                                .filter(|index| *index < stack_len)
                        } else {
                            i64::try_from(stack_len)
                                .ok()
                                .and_then(|len| len.checked_add(index))
                                .filter(|index| *index >= 0)
                                .and_then(|index| usize::try_from(index).ok())
                        };
                        let Some(selected) = selected else {
                            return Ok(NodeResult {
                                context: request.context,
                                nodes: Vec::new(),
                            });
                        };
                        selected
                    }
                    NodeLayerSelector::Host => unreachable!(),
                }
            }
        };
        let tree = if layer == 0 {
            &self.tree
        } else {
            let Some(layer) = self.injection_layers.iter().find(|candidate| {
                candidate.depth == layer
                    && crate::lsp::lsp_impl::kakehashi::node::injection_stack::ranges_contain_byte(
                        &candidate.ranges,
                        request.byte_offset,
                        self.text.len(),
                    )
            }) else {
                return Ok(NodeResult {
                    context: request.context,
                    nodes: Vec::new(),
                });
            };
            &layer.tree
        };
        let mut node = smallest_containing_worker_node(tree, request.byte_offset, self.text.len())
            .ok_or_else(|| "no node exists at byte offset".to_string())?;
        if request.named {
            while !node.is_named() {
                node = node
                    .parent()
                    .ok_or_else(|| "no named node exists at byte offset".to_string())?;
            }
        }
        let owned = self.register_node_in_layer(node, layer, &request.context)?;
        Ok(NodeResult {
            context: request.context,
            nodes: vec![owned],
        })
    }

    fn navigate_node(&mut self, request: NavigateNode) -> Result<NodeResult, String> {
        self.validate_read_identity(&request.context)?;
        self.ensure_tracked_layer(&request.node_id, &request.context);
        if let NodeNavigation::ChildWithDescendant { descendant_id } = &request.operation {
            self.ensure_tracked_layer(descendant_id, &request.context);
        }
        let Some((node, layer, included_ranges)) =
            self.tracked_node(&request.node_id, &request.context)
        else {
            return Ok(NodeResult {
                context: request.context,
                nodes: Vec::new(),
            });
        };
        let selected = match request.operation {
            NodeNavigation::Parent => node.parent().into_iter().collect::<Vec<_>>(),
            NodeNavigation::Children => (0..node.child_count() as u32)
                .filter_map(|index| node.child(index))
                .collect(),
            NodeNavigation::NamedChildren => (0..node.named_child_count() as u32)
                .filter_map(|index| node.named_child(index))
                .collect(),
            NodeNavigation::Child { index } => node.child(index).into_iter().collect(),
            NodeNavigation::NamedChild { index } => node.named_child(index).into_iter().collect(),
            NodeNavigation::NextSibling => node.next_sibling().into_iter().collect(),
            NodeNavigation::PreviousSibling => node.prev_sibling().into_iter().collect(),
            NodeNavigation::NextNamedSibling => node.next_named_sibling().into_iter().collect(),
            NodeNavigation::PreviousNamedSibling => node.prev_named_sibling().into_iter().collect(),
            NodeNavigation::FirstChildForByte { byte } => {
                if node.start_byte() <= byte
                    && byte <= node.end_byte()
                    && byte <= self.text.len()
                    && included_ranges.is_none_or(|ranges| {
                        crate::lsp::lsp_impl::kakehashi::node::injection_stack::ranges_contain_byte(
                            ranges,
                            byte,
                            self.text.len(),
                        )
                    })
                {
                    node.first_child_for_byte(byte).into_iter().collect()
                } else {
                    Vec::new()
                }
            }
            NodeNavigation::DescendantForByteRange {
                start_byte,
                end_byte,
                named,
            } => {
                if node.start_byte() <= start_byte
                    && start_byte <= end_byte
                    && end_byte <= node.end_byte()
                    && included_ranges.is_none_or(|ranges| {
                        ranges_cover_query(ranges, start_byte, end_byte, self.text.len())
                    })
                {
                    if named {
                        node.named_descendant_for_byte_range(start_byte, end_byte)
                            .into_iter()
                            .collect()
                    } else {
                        node.descendant_for_byte_range(start_byte, end_byte)
                            .into_iter()
                            .collect()
                    }
                } else {
                    Vec::new()
                }
            }
            NodeNavigation::ChildByFieldName { name } => {
                node.child_by_field_name(&name).into_iter().collect()
            }
            NodeNavigation::ChildrenByFieldName { name } => {
                let mut cursor = node.walk();
                node.children_by_field_name(&name, &mut cursor).collect()
            }
            NodeNavigation::DescendantForPointRange { start, end, named } => {
                let mapper = crate::text::PositionMapper::new(&self.text);
                let start = tower_lsp_server::ls_types::Position::new(start.line, start.character);
                let end = tower_lsp_server::ls_types::Position::new(end.line, end.character);
                let range = mapper
                    .position_to_byte_strict(start)
                    .zip(mapper.position_to_byte_strict(end))
                    .filter(|(start, end)| {
                        node.start_byte() <= *start
                            && start <= end
                            && *end <= node.end_byte()
                            && included_ranges.is_none_or(|ranges| {
                                ranges_cover_query(ranges, *start, *end, self.text.len())
                            })
                    });
                match range {
                    Some((start, end)) if named => node
                        .named_descendant_for_byte_range(start, end)
                        .into_iter()
                        .collect(),
                    Some((start, end)) => node
                        .descendant_for_byte_range(start, end)
                        .into_iter()
                        .collect(),
                    None => Vec::new(),
                }
            }
            NodeNavigation::ChildWithDescendant { descendant_id } => self
                .tracked_node(&descendant_id, &request.context)
                .filter(|(_, descendant_layer, _)| *descendant_layer == layer)
                .and_then(|(descendant, _, _)| {
                    let answer = node.child_with_descendant(descendant)?;
                    let mut current = answer;
                    while current.id() != descendant.id() {
                        current = current.child_with_descendant(descendant)?;
                    }
                    Some(answer)
                })
                .into_iter()
                .collect(),
        };
        let nodes = selected
            .into_iter()
            .map(|node| self.register_node_in_layer(node, layer, &request.context))
            .collect::<Result<_, _>>()?;
        Ok(NodeResult {
            context: request.context,
            nodes,
        })
    }

    fn run_node_scalar(&mut self, request: RunNodeScalar) -> Result<NodeScalarResult, String> {
        self.validate_read_identity(&request.context)?;
        self.ensure_tracked_layer(&request.node_id, &request.context);
        let Some((node, _, _)) = self.tracked_node(&request.node_id, &request.context) else {
            return Ok(NodeScalarResult {
                context: request.context,
                value: None,
            });
        };
        let value = match request.operation {
            NodeScalarOperation::Kind => NodeScalarValue::String(node.kind().into()),
            NodeScalarOperation::GrammarName => NodeScalarValue::String(node.grammar_name().into()),
            NodeScalarOperation::IsNamed => NodeScalarValue::Bool(node.is_named()),
            NodeScalarOperation::IsExtra => NodeScalarValue::Bool(node.is_extra()),
            NodeScalarOperation::HasError => NodeScalarValue::Bool(node.has_error()),
            NodeScalarOperation::IsError => NodeScalarValue::Bool(node.is_error()),
            NodeScalarOperation::IsMissing => NodeScalarValue::Bool(node.is_missing()),
            NodeScalarOperation::StartByte => {
                NodeScalarValue::U64(node.start_byte().try_into().unwrap_or(u64::MAX))
            }
            NodeScalarOperation::EndByte => {
                NodeScalarValue::U64(node.end_byte().try_into().unwrap_or(u64::MAX))
            }
            NodeScalarOperation::ByteRange => NodeScalarValue::ByteRange {
                start_byte: node.start_byte().try_into().unwrap_or(u64::MAX),
                end_byte: node.end_byte().try_into().unwrap_or(u64::MAX),
            },
            NodeScalarOperation::ChildCount => {
                NodeScalarValue::U64(node.child_count().try_into().unwrap_or(u64::MAX))
            }
            NodeScalarOperation::NamedChildCount => {
                NodeScalarValue::U64(node.named_child_count().try_into().unwrap_or(u64::MAX))
            }
            NodeScalarOperation::DescendantCount => {
                NodeScalarValue::U64(node.descendant_count().try_into().unwrap_or(u64::MAX))
            }
            NodeScalarOperation::SExpression => NodeScalarValue::String(node.to_sexp()),
            NodeScalarOperation::Text => NodeScalarValue::String(
                self.text
                    .get(node.start_byte()..node.end_byte())
                    .ok_or_else(|| "node text is not a valid UTF-8 slice".to_string())?
                    .to_string(),
            ),
            NodeScalarOperation::FieldNameForChild { index, named } => {
                let field = if named {
                    node.field_name_for_named_child(index)
                } else {
                    node.field_name_for_child(index)
                };
                NodeScalarValue::NullableString(field.map(str::to_string))
            }
            NodeScalarOperation::StartPosition => NodeScalarValue::Position(wire_position(
                crate::text::PositionMapper::new(&self.text)
                    .byte_to_position(node.start_byte())
                    .ok_or_else(|| "node start byte has no LSP position".to_string())?,
            )),
            NodeScalarOperation::EndPosition => NodeScalarValue::Position(wire_position(
                crate::text::PositionMapper::new(&self.text)
                    .byte_to_position(node.end_byte())
                    .ok_or_else(|| "node end byte has no LSP position".to_string())?,
            )),
            NodeScalarOperation::Range => {
                let mapper = crate::text::PositionMapper::new(&self.text);
                NodeScalarValue::Range {
                    start: wire_position(
                        mapper
                            .byte_to_position(node.start_byte())
                            .ok_or_else(|| "node start byte has no LSP position".to_string())?,
                    ),
                    end: wire_position(
                        mapper
                            .byte_to_position(node.end_byte())
                            .ok_or_else(|| "node end byte has no LSP position".to_string())?,
                    ),
                }
            }
        };
        Ok(NodeScalarResult {
            context: request.context,
            value: Some(value),
        })
    }

    fn derive_selection_ranges(
        &self,
        request: DeriveSelectionRanges,
    ) -> Result<SelectionRangesResult, String> {
        self.validate_read_identity(&request.context)?;
        let positions = request
            .positions
            .into_iter()
            .map(|position| {
                tower_lsp_server::ls_types::Position::new(position.line, position.character)
            })
            .collect::<Vec<_>>();
        let mut parser_pool = self.analysis.create_document_parser_pool();
        let ranges = crate::analysis::handle_selection_range(
            &self.text,
            Some(&self.tree),
            Some(&self.language),
            &positions,
            &self.analysis,
            &mut parser_pool,
        )
        .into_iter()
        .map(selection_range_to_wire)
        .collect();
        Ok(SelectionRangesResult {
            context: request.context,
            ranges,
        })
    }

    fn derive_injection_regions(
        &mut self,
        request: DeriveInjectionRegions,
    ) -> Result<InjectionRegionsResult, String> {
        self.validate_read_identity(&request.context)?;
        self.ensure_injection_facts(request.context.configuration_generation, true);
        let regions = self
            .resolved_injections
            .as_ref()
            .map(|(_, regions)| regions.iter())
            .into_iter()
            .flatten()
            .map(|resolved| WireInjectionRegion {
                language: resolved.injection_language.clone(),
                byte_start: resolved.region.byte_range.start,
                byte_end: resolved.region.byte_range.end,
                line_start: resolved.region.line_range.start,
                line_end: resolved.region.line_range.end,
                start_column: resolved.region.start_column,
                region_id: resolved.region.region_id.clone(),
                content_hash: resolved.region.content_hash,
                virtual_content: resolved.virtual_content.clone(),
                line_column_offsets: resolved.line_column_offsets.clone(),
                contiguous: resolved.contiguous,
            })
            .collect();
        Ok(InjectionRegionsResult {
            context: request.context,
            regions,
        })
    }

    #[cfg(test)]
    fn derive_semantic_tokens(
        &mut self,
        request: DeriveSemanticTokens,
    ) -> Result<DerivedSemanticTokens, String> {
        self.derive_semantic_tokens_with_telemetry(
            request,
            Duration::ZERO,
            Instant::now(),
            rayon::current_num_threads(),
            None,
            None,
        )
    }

    fn derive_semantic_tokens_with_telemetry(
        &mut self,
        request: DeriveSemanticTokens,
        queue_wait: Duration,
        started: Instant,
        compute_threads: usize,
        cancellation: Option<&crate::cancel::CancelToken>,
        nested_work_policy: Option<
            &(dyn Fn() -> crate::analysis::semantic::NestedWorkPolicy + Sync),
        >,
    ) -> Result<DerivedSemanticTokens, String> {
        self.validate_read_identity(&request.context)?;
        if let Some((generation, supports_multiline, tokens)) = &self.cached_semantic_tokens
            && *generation == request.context.configuration_generation
            && *supports_multiline == request.supports_multiline
        {
            return Ok(DerivedSemanticTokens {
                context: request.context,
                tokens: tokens.as_ref().clone(),
                queue_wait_ns: duration_ns(queue_wait),
                compute_ns: duration_ns(started.elapsed()),
                cache_hit: true,
            });
        }
        worker_analysis(
            &self.analysis,
            &self.language,
            self.grammar_language.clone(),
            self.queries.clone(),
            WorkerQuerySet::Semantic,
        )?;
        let query = self
            .analysis
            .highlight_query(&self.language)
            .ok_or_else(|| "worker highlight query is unavailable".to_string())?;
        let discovery = self.semantic_discovery(request.context.configuration_generation);
        let result = crate::analysis::compute_semantic_tokens_full(
            compute_threads,
            Arc::from(self.text.as_str()),
            self.tree.clone(),
            query,
            Some(self.language.clone()),
            Some(self.analysis.capture_mappings()),
            Arc::clone(&self.analysis),
            request.supports_multiline,
            Some(crate::analysis::semantic::InjectionCacheParams {
                uri: self.uri.clone(),
                tracker: Arc::clone(&self.node_tracker),
                cache: Arc::clone(&self.injection_token_cache),
                generation: request.context.configuration_generation,
                currency: crate::analysis::semantic::InjectionCacheCurrency::Current,
                incarnation: request.context.incarnation,
                discovery,
            }),
            cancellation.cloned(),
            nested_work_policy,
        )
        .ok_or_else(|| "worker semantic token derivation was cancelled".to_string())?;
        let tokens: Vec<WireSemanticToken> = match result {
            tower_lsp_server::ls_types::SemanticTokensResult::Tokens(tokens) => tokens.data,
            tower_lsp_server::ls_types::SemanticTokensResult::Partial(partial) => partial.data,
        }
        .into_iter()
        .map(|token| WireSemanticToken {
            delta_line: token.delta_line,
            delta_start: token.delta_start,
            length: token.length,
            token_type: token.token_type,
            token_modifiers_bitset: token.token_modifiers_bitset,
        })
        .collect();
        self.cached_semantic_tokens = Some((
            request.context.configuration_generation,
            request.supports_multiline,
            Arc::new(tokens.clone()),
        ));
        self.trim_auxiliary_caches_for_pressure();
        Ok(DerivedSemanticTokens {
            context: request.context,
            tokens,
            queue_wait_ns: duration_ns(queue_wait),
            compute_ns: duration_ns(started.elapsed()),
            cache_hit: false,
        })
    }

    fn semantic_discovery(
        &mut self,
        generation: u64,
    ) -> Option<Arc<crate::document::DiscoveredInjections>> {
        if self
            .semantic_discovery
            .as_ref()
            .is_some_and(|discovery| discovery.generation == generation)
        {
            return self.semantic_discovery.clone();
        }
        self.ensure_injection_facts(generation, false);
        self.semantic_discovery.clone()
    }

    fn ensure_injection_facts(&mut self, generation: u64, needs_resolved: bool) {
        if self
            .resolved_injections
            .as_ref()
            .is_some_and(|(cached_generation, _)| *cached_generation == generation)
        {
            return;
        }
        if !needs_resolved
            && self
                .semantic_discovery
                .as_ref()
                .is_some_and(|discovery| discovery.generation == generation)
        {
            return;
        }
        self.semantic_discovery = None;
        let Some(injection_query) = self.analysis.injection_query(&self.language) else {
            self.resolved_injections = Some((generation, Arc::new(Vec::new())));
            return;
        };
        let regions = crate::language::injection::collect_all_injections(
            &self.tree.root_node(),
            &self.text,
            Some(injection_query.as_ref()),
        )
        .unwrap_or_default();
        let Some(cacheable) = regions
            .iter()
            .map(|region| {
                let id = crate::language::InjectionResolver::calculate_region_id(
                    &self.node_tracker,
                    &self.uri,
                    region,
                    self.context.incarnation,
                )?;
                Some(
                    crate::language::injection::CacheableInjectionRegion::from_region_info(
                        region,
                        &id.to_string(),
                        &self.text,
                    ),
                )
            })
            .collect::<Option<Vec<_>>>()
        else {
            self.resolved_injections = Some((generation, Arc::new(Vec::new())));
            return;
        };
        let resolved = needs_resolved.then(|| {
            crate::language::InjectionResolver::resolve_from_prebuilt(
                &self.analysis,
                &regions,
                &cacheable,
                &self.text,
            )
        });
        self.semantic_discovery = crate::analysis::semantic::build_document_discovery(
            &regions,
            &cacheable,
            injection_query.as_ref(),
            &self.text,
            &self.analysis,
            &self.uri,
            &self.node_tracker,
            generation,
            self.context.incarnation,
        )
        .map(Arc::new);
        if let Some(resolved) = resolved {
            self.resolved_injections = Some((generation, Arc::new(resolved)));
        }
    }

    fn run_captures(&self, request: RunCaptures) -> Result<CapturesResult, String> {
        self.validate_read_identity(&request.context)?;
        let range = request.range.map(|range| {
            tower_lsp_server::ls_types::Range::new(
                tower_lsp_server::ls_types::Position::new(range.start.line, range.start.character),
                tower_lsp_server::ls_types::Position::new(range.end.line, range.end.character),
            )
        });
        let result = crate::lsp::lsp_impl::kakehashi::captures::execute_captures_walk(
            &self.uri,
            &request.kind,
            range,
            request.injection,
            &self.language,
            &self.text,
            &self.tree,
            None,
            &self.analysis,
            &self.node_tracker,
            &|| true,
            self.node_tracker.mint_epoch(&self.uri),
            request.context.incarnation,
            true,
            request.context.configuration_generation,
            &self.captures_match_cache,
            None,
        );
        let (available, matches, skipped) = match result {
            Some((matches, skipped)) => (true, matches, skipped),
            None => (false, Vec::new(), Vec::new()),
        };
        Ok(CapturesResult {
            context: request.context,
            available,
            matches,
            skipped,
        })
    }

    fn derive_native_bindings(
        &self,
        request: DeriveNativeBindings,
    ) -> Result<NativeBindingsResult, String> {
        self.validate_read_identity(&request.context)?;
        worker_analysis(
            &self.analysis,
            &self.language,
            self.grammar_language.clone(),
            self.queries.clone(),
            WorkerQuerySet::Bindings,
        )?;
        let position = tower_lsp_server::ls_types::Position::new(
            request.position.line,
            request.position.character,
        );
        let Some(byte) =
            crate::text::PositionMapper::new(&self.text).position_to_byte_strict(position)
        else {
            return Ok(NativeBindingsResult {
                context: request.context,
                facts: None,
            });
        };
        let Some((query, layer_text, layer_tree, layer_offset)) = self.native_bindings_inputs(byte)
        else {
            return Ok(NativeBindingsResult {
                context: request.context,
                facts: None,
            });
        };
        let cancel = AtomicBool::new(false);
        let facts = crate::analysis::bindings::collect::collect_cancellable(
            &layer_text,
            layer_tree.root_node(),
            &query,
            &cancel,
        )
        .map(crate::analysis::bindings::model::BindingsModel::build)
        .map(|model| native_bindings_facts(&model, byte - layer_offset, layer_offset));
        Ok(NativeBindingsResult {
            context: request.context,
            facts,
        })
    }

    fn native_bindings_inputs(
        &self,
        byte: usize,
    ) -> Option<(Arc<tree_sitter::Query>, Arc<str>, tree_sitter::Tree, usize)> {
        use crate::language::injection::{
            collect_all_injections, compute_included_ranges, parse_with_ranges,
        };

        if let Some(injection_query) = self.analysis.injection_query(&self.language)
            && let Some(regions) =
                collect_all_injections(&self.tree.root_node(), &self.text, Some(&injection_query))
            && let Some(region) = regions
                .iter()
                .filter(|region| {
                    region.content_node.start_byte() <= byte
                        && byte < region.content_node.end_byte()
                })
                .min_by_key(|region| {
                    region.content_node.end_byte() - region.content_node.start_byte()
                })
        {
            if region.offset.is_some() {
                return None;
            }
            let content_range = region.content_node.byte_range();
            let content_text = self.text.get(content_range.clone()).map(Arc::<str>::from)?;
            let (layer_language, load) = self
                .analysis
                .resolve_injection_language(&region.language, &content_text)?;
            if !load.success {
                return None;
            }
            let included_ranges =
                compute_included_ranges(&region.content_node, region.include_children);
            let mut parser_pool = self.analysis.create_document_parser_pool();
            let mut parser = parser_pool.acquire(&layer_language)?;
            let layer_tree = parse_with_ranges(
                &mut parser,
                &content_text,
                included_ranges.as_deref(),
                "kakehashi::tree_worker",
                &layer_language,
            );
            parser_pool.release(layer_language.clone(), parser);
            let layer_tree = layer_tree?;
            let query = self.analysis.bindings_query(&layer_language)?;
            return Some((query, content_text, layer_tree, content_range.start));
        }

        let query = self.analysis.bindings_query(&self.language)?;
        Some((query, Arc::from(self.text.as_str()), self.tree.clone(), 0))
    }

    fn cache_injection_stack(&mut self, byte: usize) {
        let stack = crate::lsp::lsp_impl::kakehashi::node::injection_stack::injection_stack_at(
            &self.analysis,
            &self.language,
            &self.text,
            &self.tree,
            byte,
        );
        for (depth, layer) in stack.into_iter().enumerate().skip(1) {
            if self
                .injection_layers
                .iter()
                .any(|cached| cached.depth == depth && cached.ranges == layer.ranges)
            {
                continue;
            }
            self.injection_layers.push(CachedInjectionLayer {
                depth,
                tree: layer.tree,
                ranges: layer.ranges,
            });
        }
    }

    fn ensure_tracked_layer(&mut self, node_id: &OpaqueNodeId, context: &RequestContext) {
        if node_id.worker_generation != context.worker_generation {
            return;
        }
        let Some((start_byte, _, _, layer, incarnation)) = node_id
            .local_id
            .parse::<ulid::Ulid>()
            .ok()
            .and_then(|id| self.node_tracker.lookup_node(&self.uri, &id))
        else {
            return;
        };
        if layer > 0 && incarnation == context.incarnation {
            self.cache_injection_stack(start_byte);
        }
    }

    fn tracked_node<'tree>(
        &'tree self,
        node_id: &OpaqueNodeId,
        context: &RequestContext,
    ) -> Option<(
        tree_sitter::Node<'tree>,
        usize,
        Option<&'tree [tree_sitter::Range]>,
    )> {
        if node_id.worker_generation != context.worker_generation {
            return None;
        }
        let ulid = node_id.local_id.parse::<ulid::Ulid>().ok()?;
        let (start_byte, end_byte, kind, layer, incarnation) =
            self.node_tracker.lookup_node(&self.uri, &ulid)?;
        if incarnation != context.incarnation {
            return None;
        }
        if layer == 0 {
            return find_exact_node(self.tree.root_node(), start_byte, end_byte, kind)
                .map(|node| (node, 0, None));
        }
        self.injection_layers
            .iter()
            .filter(|candidate| candidate.depth == layer)
            .find_map(|candidate| {
                find_exact_node(candidate.tree.root_node(), start_byte, end_byte, kind)
                    .map(|node| (node, layer, Some(candidate.ranges.as_slice())))
            })
    }

    fn register_node_in_layer(
        &self,
        node: tree_sitter::Node<'_>,
        layer: usize,
        context: &RequestContext,
    ) -> Result<OwnedNode, String> {
        let local_id = self
            .node_tracker
            .get_or_create_in_layer_for_incarnation(
                &self.uri,
                node.start_byte(),
                node.end_byte(),
                node.kind(),
                layer,
                context.incarnation,
            )
            .ok_or_else(|| "document incarnation cannot mint a node ID".to_string())?
            .to_string();
        Ok(OwnedNode {
            id: OpaqueNodeId {
                worker_generation: context.worker_generation,
                local_id,
            },
            kind: node.kind().into(),
            start_byte: node.start_byte(),
            end_byte: node.end_byte(),
            named: node.is_named(),
            has_error: node.has_error(),
            layer,
        })
    }

    fn validate_identity(&self, context: &RequestContext) -> Result<(), String> {
        self.validate_lifetime(context)?;
        if context.configuration_generation != self.context.configuration_generation {
            return Err("document identity does not match worker replica".into());
        }
        Ok(())
    }

    fn validate_read_identity(&self, context: &RequestContext) -> Result<(), String> {
        self.validate_identity(context)?;
        if context.content_version != self.context.content_version {
            return Err("document content version does not match worker replica".into());
        }
        Ok(())
    }

    fn validate_lifetime(&self, context: &RequestContext) -> Result<(), String> {
        if context.worker_generation != self.context.worker_generation
            || context.uri != self.context.uri
            || context.incarnation != self.context.incarnation
        {
            return Err("document lifetime does not match worker replica".into());
        }
        Ok(())
    }

    fn validate_replacement(
        &self,
        context: &RequestContext,
        language: &str,
        grammar_key: &GrammarKey,
        queries: &WorkerQuerySources,
    ) -> Result<(), String> {
        if context.worker_generation != self.context.worker_generation
            || context.uri != self.context.uri
        {
            return Err("document identity does not match worker replica".into());
        }
        if context.incarnation < self.context.incarnation {
            return Err("full sync targets a stale document incarnation".into());
        }
        if context.incarnation == self.context.incarnation {
            if language != self.language {
                return Err("language change requires a new document incarnation".into());
            }
            if grammar_key != &self.grammar_key
                && context.configuration_generation <= self.context.configuration_generation
            {
                return Err("grammar change requires a newer configuration generation".into());
            }
            if context.configuration_generation < self.context.configuration_generation {
                return Err("full sync targets a stale configuration generation".into());
            }
            if context.content_version < self.context.content_version {
                return Err("full sync targets a stale content version".into());
            }
            if context.configuration_generation == self.context.configuration_generation
                && context.content_version == self.context.content_version
                && queries == &self.queries
            {
                return Err("full sync must advance the content version".into());
            }
        }
        Ok(())
    }
}

fn selection_range_to_wire(
    selection: tower_lsp_server::ls_types::SelectionRange,
) -> WireSelectionRange {
    WireSelectionRange {
        range: WireRange {
            start: wire_position(selection.range.start),
            end: wire_position(selection.range.end),
        },
        parent: selection
            .parent
            .map(|parent| Box::new(selection_range_to_wire(*parent))),
    }
}

#[derive(Clone, Copy)]
enum WorkerQuerySet {
    Injections,
    Semantic,
    Bindings,
    All,
}

fn worker_analysis(
    analysis: &crate::language::LanguageCoordinator,
    language_name: &str,
    language: tree_sitter::Language,
    queries: WorkerQuerySources,
    query_set: WorkerQuerySet,
) -> Result<(), String> {
    analysis
        .language_registry_for_parallel()
        .register(language_name.to_string(), language.clone());
    for (kind, source) in [
        (
            crate::config::settings::QueryKind::Highlights,
            queries.highlights,
        ),
        (
            crate::config::settings::QueryKind::Bindings,
            queries.bindings,
        ),
        (
            crate::config::settings::QueryKind::Injections,
            queries.injections,
        ),
    ] {
        let selected = match query_set {
            WorkerQuerySet::Injections => kind == crate::config::settings::QueryKind::Injections,
            WorkerQuerySet::Semantic => matches!(
                kind,
                crate::config::settings::QueryKind::Highlights
                    | crate::config::settings::QueryKind::Injections
            ),
            WorkerQuerySet::Bindings => matches!(
                kind,
                crate::config::settings::QueryKind::Bindings
                    | crate::config::settings::QueryKind::Injections
            ),
            WorkerQuerySet::All => true,
        };
        if !selected {
            continue;
        }
        let Some(source) = source else {
            continue;
        };
        if analysis
            .query_store()
            .query_input(kind, language_name)
            .is_some_and(|current| current.as_ref() == source)
        {
            continue;
        }
        let parsed =
            crate::language::query_loader::QueryLoader::parse_query(&language, &source, false);
        if !parsed.skipped.is_empty() {
            log::warn!(
                target: "kakehashi::tree_worker",
                "worker skipped {} invalid {} query pattern(s) for {}",
                parsed.skipped.len(),
                kind.name(),
                language_name,
            );
        }
        let (Some(query), Some(compiled_source)) = (parsed.query, parsed.compiled_source) else {
            continue;
        };
        if analysis
            .query_store()
            .query_source(kind, language_name)
            .is_some_and(|current| current.as_ref() == compiled_source)
        {
            continue;
        }
        analysis.query_store().insert_query_with_source_input(
            kind,
            language_name.to_string(),
            Arc::new(query),
            compiled_source,
            source,
        );
    }
    Ok(())
}

fn native_bindings_facts(
    model: &crate::analysis::bindings::model::BindingsModel,
    byte: usize,
    layer_offset: usize,
) -> NativeBindingsFacts {
    let to_wire = |range: std::ops::Range<usize>| WireByteRange {
        start: range.start + layer_offset,
        end: range.end + layer_offset,
    };
    let identifier = model.resolvable_identifier_at(byte).map(&to_wire);
    let definition = model.definition_range_at(byte).map(&to_wire);
    let Some(binding) = model.binding_at(byte) else {
        return NativeBindingsFacts {
            identifier,
            definition,
            references: Vec::new(),
            definitions: Vec::new(),
        };
    };
    let mut references = model
        .references_resolving_to(binding)
        .into_iter()
        .map(&to_wire)
        .collect::<Vec<_>>();
    let mut definitions = model
        .sites(binding)
        .iter()
        .map(|site| to_wire(site.byte_range.clone()))
        .collect::<Vec<_>>();
    references.sort_by_key(|range| (range.start, range.end));
    references.dedup();
    definitions.sort_by_key(|range| (range.start, range.end));
    definitions.dedup();
    NativeBindingsFacts {
        identifier,
        definition,
        references,
        definitions,
    }
}

fn validate_document_size(bytes: usize) -> Result<(), String> {
    if bytes > MAX_DOCUMENT_BYTES {
        return Err(format!(
            "document exceeds retained size limit {MAX_DOCUMENT_BYTES} bytes"
        ));
    }
    Ok(())
}

fn replace_retained_bytes(
    retained_bytes: &AtomicUsize,
    old_bytes: usize,
    new_bytes: usize,
) -> Result<(), String> {
    let mut retained = retained_bytes.load(Ordering::Relaxed);
    loop {
        let next = retained
            .checked_sub(old_bytes)
            .and_then(|bytes| bytes.checked_add(new_bytes))
            .ok_or_else(|| "retained document byte accounting overflowed".to_string())?;
        if next > MAX_RETAINED_DOCUMENT_BYTES {
            return Err(format!(
                "worker retained document budget {MAX_RETAINED_DOCUMENT_BYTES} bytes exceeded"
            ));
        }
        match retained_bytes.compare_exchange_weak(
            retained,
            next,
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => return Ok(()),
            Err(current) => retained = current,
        }
    }
}

fn replace_retained_estimated_bytes(
    retained_bytes: &AtomicUsize,
    old_bytes: usize,
    new_bytes: usize,
    hard_limit_bytes: usize,
) -> Result<(), String> {
    let mut retained = retained_bytes.load(Ordering::Relaxed);
    loop {
        let next = retained
            .checked_sub(old_bytes)
            .and_then(|bytes| bytes.checked_add(new_bytes))
            .ok_or_else(|| "estimated document byte accounting overflowed".to_string())?;
        if next > hard_limit_bytes {
            return Err(format!(
                "worker estimated non-evictable budget {hard_limit_bytes} bytes exceeded"
            ));
        }
        match retained_bytes.compare_exchange_weak(
            retained,
            next,
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => return Ok(()),
            Err(current) => retained = current,
        }
    }
}

fn reserve_retained_growth(
    retained_bytes: &AtomicUsize,
    old_bytes: usize,
    new_bytes: usize,
) -> Result<bool, String> {
    if new_bytes <= old_bytes {
        return Ok(false);
    }
    replace_retained_bytes(retained_bytes, old_bytes, new_bytes)?;
    Ok(true)
}

fn point_at_byte(text: &str, byte: usize) -> tree_sitter::Point {
    let prefix = &text.as_bytes()[..byte];
    let row = prefix.iter().filter(|&&value| value == b'\n').count();
    let column = prefix
        .iter()
        .rposition(|&value| value == b'\n')
        .map_or(prefix.len(), |index| prefix.len() - index - 1);
    tree_sitter::Point::new(row, column)
}

fn wire_position(position: tower_lsp_server::ls_types::Position) -> WirePosition {
    WirePosition {
        line: position.line,
        character: position.character,
    }
}

fn find_exact_node<'tree>(
    root: tree_sitter::Node<'tree>,
    start_byte: usize,
    end_byte: usize,
    kind: &str,
) -> Option<tree_sitter::Node<'tree>> {
    let mut node = root.descendant_for_byte_range(start_byte, end_byte)?;
    loop {
        if node.start_byte() == start_byte && node.end_byte() == end_byte && node.kind() == kind {
            return Some(node);
        }
        node = node.parent()?;
    }
}

fn smallest_containing_worker_node(
    tree: &tree_sitter::Tree,
    byte: usize,
    document_len: usize,
) -> Option<tree_sitter::Node<'_>> {
    if byte == document_len {
        let mut cursor = tree.root_node().walk();
        let mut current = tree.root_node();
        while cursor.goto_first_child() {
            while cursor.goto_next_sibling() {}
            let child = cursor.node();
            if child.end_byte() != document_len {
                break;
            }
            current = child;
        }
        return (current.end_byte() == document_len).then_some(current);
    }
    let mut current = tree.root_node().descendant_for_byte_range(byte, byte);
    while let Some(node) = current {
        if node.start_byte() <= byte && byte < node.end_byte() {
            return Some(node);
        }
        current = node.parent();
    }
    None
}

fn ranges_cover_query(
    ranges: &[tree_sitter::Range],
    start: usize,
    end: usize,
    document_len: usize,
) -> bool {
    let contains = crate::lsp::lsp_impl::kakehashi::node::injection_stack::ranges_contain_byte;
    contains(ranges, start, document_len)
        && (end == start || contains(ranges, end - 1, document_len))
}

pub fn encode_frame<W: Write, T: Serialize>(writer: &mut W, value: &T) -> io::Result<()> {
    let payload = bincode::serialize(value)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if payload.len() > MAX_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame exceeds {MAX_FRAME_BYTES} bytes"),
        ));
    }
    let length = u32::try_from(payload.len())
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    writer.write_all(&length.to_be_bytes())?;
    writer.write_all(&payload)?;
    writer.flush()
}

pub fn decode_frame<R: Read, T: DeserializeOwned>(reader: &mut R) -> io::Result<Option<T>> {
    let mut length = [0_u8; 4];
    match reader.read(&mut length[..1])? {
        0 => return Ok(None),
        1 => reader.read_exact(&mut length[1..])?,
        _ => unreachable!("one-byte read returned more than one byte"),
    }
    let length = u32::from_be_bytes(length) as usize;
    if length > MAX_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame exceeds {MAX_FRAME_BYTES} bytes"),
        ));
    }
    let mut payload = vec![0_u8; length];
    reader.read_exact(&mut payload)?;
    bincode::deserialize(&payload)
        .map(Some)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

pub(crate) fn named_node_count(root: tree_sitter::Node<'_>) -> usize {
    let mut count = 0;
    let mut cursor = root.walk();
    let mut ascending = false;
    loop {
        if !ascending {
            count += usize::from(cursor.node().is_named());
            if cursor.goto_first_child() {
                continue;
            }
        }
        if cursor.goto_next_sibling() {
            ascending = false;
            continue;
        }
        if !cursor.goto_parent() {
            break;
        }
        ascending = true;
    }
    count
}

pub fn derive_snapshot_with_language(
    request: DeriveSnapshot,
    language: tree_sitter::Language,
) -> Response {
    let mut parser = tree_sitter::Parser::new();
    if let Err(error) = parser.set_language(&language) {
        return Response::Error(WorkerError {
            context: Some(request.context),
            message: format!("incompatible grammar: {error}"),
        });
    }
    derive_snapshot_with_parser(request, &mut parser, false, Duration::ZERO)
}

fn duration_ns(duration: Duration) -> u64 {
    duration.as_nanos().min(u128::from(u64::MAX)) as u64
}

fn derive_snapshot_with_parser(
    request: DeriveSnapshot,
    parser: &mut tree_sitter::Parser,
    parser_cache_hit: bool,
    queue_wait: Duration,
) -> Response {
    let started = Instant::now();
    let Some(tree) = parser.parse(request.text.as_bytes(), None) else {
        return Response::Error(WorkerError {
            context: Some(request.context),
            message: "parser returned no tree".into(),
        });
    };
    let root = tree.root_node();
    Response::Snapshot(DerivedSnapshot {
        context: request.context,
        language: request.language,
        root_kind: root.kind().into(),
        root_start_byte: root.start_byte(),
        root_end_byte: root.end_byte(),
        has_error: root.has_error(),
        named_node_count: named_node_count(root),
        parser_cache_hit: Some(parser_cache_hit),
        queue_wait_ns: duration_ns(queue_wait),
        compute_ns: duration_ns(started.elapsed()),
    })
}

#[derive(Clone, Debug)]
struct GrammarKey {
    parser_path: PathBuf,
    grammar_symbol: String,
    artifact_digest: String,
}

impl GrammarKey {
    fn from_sync(request: &SyncDocument) -> Self {
        Self {
            parser_path: request
                .parser_path
                .canonicalize()
                .unwrap_or_else(|_| request.parser_path.clone()),
            grammar_symbol: request.grammar_symbol.clone(),
            artifact_digest: request.artifact_digest.clone(),
        }
    }

    fn from_derive(request: &DeriveSnapshot) -> Self {
        Self {
            parser_path: request
                .parser_path
                .canonicalize()
                .unwrap_or_else(|_| request.parser_path.clone()),
            grammar_symbol: request.grammar_symbol.clone(),
            artifact_digest: request.artifact_digest.clone(),
        }
    }

    fn from_asset(asset: &WorkerLanguageAsset) -> Self {
        Self {
            parser_path: asset
                .parser_path
                .canonicalize()
                .unwrap_or_else(|_| asset.parser_path.clone()),
            grammar_symbol: asset.grammar_symbol.clone(),
            artifact_digest: asset.artifact_digest.clone(),
        }
    }
}

impl PartialEq for GrammarKey {
    fn eq(&self, other: &Self) -> bool {
        self.grammar_symbol == other.grammar_symbol && self.artifact_digest == other.artifact_digest
    }
}

impl Eq for GrammarKey {}

impl std::hash::Hash for GrammarKey {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.grammar_symbol.hash(state);
        self.artifact_digest.hash(state);
    }
}

struct WorkerThreadState {
    loader: crate::language::loader::ParserLoader,
    languages: HashMap<GrammarKey, tree_sitter::Language>,
    parsers: HashMap<GrammarKey, tree_sitter::Parser>,
}

pub struct LocalDeriver {
    state: WorkerThreadState,
}

pub struct LocalDocumentReplica {
    state: WorkerThreadState,
    replica: Option<DocumentReplica>,
}

impl Default for LocalDeriver {
    fn default() -> Self {
        Self::new()
    }
}

impl LocalDeriver {
    pub fn new() -> Self {
        Self {
            state: WorkerThreadState::new(),
        }
    }

    pub fn derive(&mut self, request: DeriveSnapshot) -> Response {
        self.state.derive(request, Duration::ZERO)
    }
}

impl Default for LocalDocumentReplica {
    fn default() -> Self {
        Self::new()
    }
}

impl LocalDocumentReplica {
    pub fn new() -> Self {
        Self {
            state: WorkerThreadState::new(),
            replica: None,
        }
    }

    pub fn sync_document(&mut self, request: SyncDocument) -> Response {
        let context = request.context.clone();
        let grammar_key = GrammarKey::from_sync(&request);
        let Self { state, replica } = self;
        if let Some(existing) = replica.as_ref()
            && let Err(message) = existing.validate_replacement(
                &context,
                &request.language,
                &grammar_key,
                &request.queries,
            )
        {
            return Response::Error(WorkerError {
                context: Some(context),
                message,
            });
        }
        state.with_parser(
            grammar_key,
            context.clone(),
            |parser, _| match DocumentReplica::sync(request, parser) {
                Ok((document, ack)) => {
                    *replica = Some(document);
                    Response::DocumentAck(ack)
                }
                Err(message) => Response::Error(WorkerError {
                    context: Some(context),
                    message,
                }),
            },
        )
    }

    pub fn apply_document_edits(&mut self, request: ApplyDocumentEdits) -> Response {
        let context = request.context.clone();
        let Some(grammar_key) = self
            .replica
            .as_ref()
            .map(|replica| replica.grammar_key.clone())
        else {
            return Response::Error(WorkerError {
                context: Some(context),
                message: "document replica is missing; full sync required".into(),
            });
        };
        let Self { state, replica } = self;
        state.with_parser(grammar_key, context.clone(), |parser, _| {
            match replica
                .as_mut()
                .expect("replica presence checked before parser checkout")
                .apply(request, parser, None)
            {
                Ok(ack) => Response::DocumentAck(ack),
                Err(message) => Response::Error(WorkerError {
                    context: Some(context),
                    message,
                }),
            }
        })
    }

    pub fn derive_document_snapshot(&self, request: DeriveDocumentSnapshot) -> Response {
        let context = request.context;
        let Some(replica) = &self.replica else {
            return Response::Error(WorkerError {
                context: Some(context),
                message: "document replica is missing; full sync required".into(),
            });
        };
        match replica.derive(context.clone()) {
            Ok(snapshot) => Response::Snapshot(snapshot),
            Err(message) => Response::Error(WorkerError {
                context: Some(context),
                message,
            }),
        }
    }
}

impl WorkerThreadState {
    fn new() -> Self {
        Self {
            loader: crate::language::loader::ParserLoader::new(),
            languages: HashMap::new(),
            parsers: HashMap::new(),
        }
    }

    fn derive(&mut self, request: DeriveSnapshot, queue_wait: Duration) -> Response {
        let request_context = request.context.clone();
        let key = GrammarKey::from_derive(&request);
        self.with_parser(key, request_context, |parser, parser_cache_hit| {
            derive_snapshot_with_parser(request, parser, parser_cache_hit, queue_wait)
        })
    }

    fn with_parser(
        &mut self,
        key: GrammarKey,
        request_context: RequestContext,
        operation: impl FnOnce(&mut tree_sitter::Parser, bool) -> Response,
    ) -> Response {
        let language = match self.languages.get(&key).cloned() {
            Some(language) => language,
            None => {
                let actual_digest = match artifact_digest(&key.parser_path) {
                    Ok(digest) => digest,
                    Err(error) => {
                        return Response::Error(WorkerError {
                            context: Some(request_context),
                            message: format!("cannot verify parser artifact: {error}"),
                        });
                    }
                };
                if actual_digest != key.artifact_digest {
                    return Response::Error(WorkerError {
                        context: Some(request_context),
                        message: format!(
                            "parser artifact digest mismatch: expected {} got {actual_digest}",
                            key.artifact_digest
                        ),
                    });
                }
                match self
                    .loader
                    .load_language(&key.parser_path, &key.grammar_symbol)
                {
                    Ok(language) => {
                        self.languages.insert(key.clone(), language.clone());
                        language
                    }
                    Err(error) => {
                        return Response::Error(WorkerError {
                            context: Some(request_context),
                            message: error.to_string(),
                        });
                    }
                }
            }
        };
        let parser_cache_hit = self.parsers.contains_key(&key);
        let mut parser = if let Some(parser) = self.parsers.remove(&key) {
            parser
        } else {
            let mut parser = tree_sitter::Parser::new();
            if let Err(error) = parser.set_language(&language) {
                return Response::Error(WorkerError {
                    context: Some(request_context),
                    message: format!("incompatible grammar: {error}"),
                });
            }
            parser
        };
        let response = operation(&mut parser, parser_cache_hit);
        self.parsers.insert(key, parser);
        response
    }
}

thread_local! {
    static WORKER_THREAD_STATE: RefCell<WorkerThreadState> =
        RefCell::new(WorkerThreadState::new());
}

fn derive_snapshot(request: DeriveSnapshot, queue_wait: Duration) -> Response {
    WORKER_THREAD_STATE.with(|state| state.borrow_mut().derive(request, queue_wait))
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct DocumentKey {
    uri: String,
}

impl From<&RequestContext> for DocumentKey {
    fn from(context: &RequestContext) -> Self {
        Self {
            uri: context.uri.clone(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct CacheEvictionCandidate {
    key: DocumentKey,
    result_bytes: usize,
    auxiliary_bytes: usize,
    current: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum CacheEviction {
    Auxiliary(DocumentKey),
    Result(DocumentKey),
}

fn cache_eviction_plan(
    mut candidates: Vec<CacheEvictionCandidate>,
    target_bytes: usize,
) -> Vec<CacheEviction> {
    let mut retained = candidates.iter().fold(0_usize, |bytes, candidate| {
        bytes
            .saturating_add(candidate.result_bytes)
            .saturating_add(candidate.auxiliary_bytes)
    });
    let mut plan = Vec::new();
    candidates.sort_by(|left, right| {
        right
            .auxiliary_bytes
            .cmp(&left.auxiliary_bytes)
            .then_with(|| left.key.uri.cmp(&right.key.uri))
    });
    for candidate in &candidates {
        if retained <= target_bytes {
            return plan;
        }
        if candidate.auxiliary_bytes > 0 {
            retained = retained.saturating_sub(candidate.auxiliary_bytes);
            plan.push(CacheEviction::Auxiliary(candidate.key.clone()));
        }
    }
    candidates.sort_by(|left, right| {
        left.current
            .cmp(&right.current)
            .then_with(|| right.result_bytes.cmp(&left.result_bytes))
            .then_with(|| left.key.uri.cmp(&right.key.uri))
    });
    for candidate in candidates {
        if retained <= target_bytes {
            break;
        }
        if candidate.result_bytes > 0 {
            retained = retained.saturating_sub(candidate.result_bytes);
            plan.push(CacheEviction::Result(candidate.key));
        }
    }
    plan
}

fn enforce_derived_memory_budget(
    documents: &DocumentStore,
    pressure_lock: &Mutex<()>,
    current: &DocumentKey,
    target_bytes: usize,
) -> Result<(), String> {
    let _pressure = pressure_lock
        .lock()
        .map_err(|_| "worker memory-pressure lock is poisoned".to_string())?;
    let mut candidates = Vec::with_capacity(documents.len());
    for entry in documents.iter() {
        let key = entry.key().clone();
        let replica = Arc::clone(entry.value());
        drop(entry);
        let estimate = replica
            .lock()
            .map_err(|_| "document replica lock is poisoned during memory pressure".to_string())?
            .estimated_memory();
        candidates.push(CacheEvictionCandidate {
            current: key == *current,
            key,
            result_bytes: estimate.result_cache_bytes,
            auxiliary_bytes: estimate.auxiliary_cache_bytes,
        });
    }
    for eviction in cache_eviction_plan(candidates, target_bytes) {
        let key = match &eviction {
            CacheEviction::Auxiliary(key) | CacheEviction::Result(key) => key,
        };
        let Some(replica) = documents.get(key).map(|entry| Arc::clone(entry.value())) else {
            continue;
        };
        let mut replica = replica
            .lock()
            .map_err(|_| "document replica lock is poisoned during cache eviction".to_string())?;
        match eviction {
            CacheEviction::Auxiliary(_) => {
                replica.clear_auxiliary_caches();
            }
            CacheEviction::Result(_) => {
                replica.clear_result_cache();
            }
        }
    }
    Ok(())
}

type DocumentStore = Arc<dashmap::DashMap<DocumentKey, Arc<Mutex<DocumentReplica>>>>;
type ClosedDocuments = Arc<dashmap::DashMap<DocumentKey, u64>>;
type DocumentCapacity = Arc<Mutex<usize>>;
type RetainedDocumentBytes = Arc<AtomicUsize>;
type RetainedEstimatedDocumentBytes = Arc<AtomicUsize>;
type LaneJob = Box<dyn FnOnce() -> LaneAction + Send + 'static>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WorkPriority {
    Foreground,
    Background,
}

#[derive(Clone, Copy)]
enum LaneAction {
    Keep,
    Retire,
}

struct DocumentLane {
    state: Mutex<DocumentLaneState>,
    key: Option<DocumentKey>,
    registry: Weak<LaneRegistry>,
}

#[derive(Default)]
struct DocumentLaneState {
    running: bool,
    retire_when_idle: bool,
    pending: VecDeque<LaneJob>,
}

impl DocumentLane {
    fn new(key: DocumentKey, registry: Weak<LaneRegistry>) -> Self {
        Self {
            state: Mutex::new(DocumentLaneState {
                retire_when_idle: true,
                ..DocumentLaneState::default()
            }),
            key: Some(key),
            registry,
        }
    }

    #[cfg(test)]
    fn detached() -> Self {
        Self {
            state: Mutex::new(DocumentLaneState::default()),
            key: None,
            registry: Weak::new(),
        }
    }

    fn submit(self: &Arc<Self>, pool: &Arc<rayon::ThreadPool>, job: LaneJob) {
        let should_start = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.pending.push_back(job);
            if state.running {
                false
            } else {
                state.running = true;
                true
            }
        };
        if should_start {
            let lane = Arc::clone(self);
            pool.spawn(move || lane.drain());
        }
    }

    fn drain(&self) {
        loop {
            let job = {
                let mut state = self
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                match state.pending.pop_front() {
                    Some(job) => job,
                    None => {
                        state.running = false;
                        let retire = state.retire_when_idle;
                        drop(state);
                        if retire {
                            self.remove_if_retired();
                        }
                        return;
                    }
                }
            };
            let action = job();
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match action {
                LaneAction::Keep => state.retire_when_idle = false,
                LaneAction::Retire => state.retire_when_idle = true,
            }
        }
    }

    fn remove_if_retired(&self) {
        let (Some(key), Some(registry)) = (&self.key, self.registry.upgrade()) else {
            return;
        };
        if let dashmap::mapref::entry::Entry::Occupied(entry) = registry.entry(key.clone()) {
            let state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if !state.running
                && state.pending.is_empty()
                && state.retire_when_idle
                && std::ptr::eq(Arc::as_ptr(entry.get()), self)
            {
                drop(state);
                entry.remove();
            }
        }
    }
}

type LaneRegistry = dashmap::DashMap<DocumentKey, Arc<DocumentLane>>;
type DocumentLanes = Arc<LaneRegistry>;

#[derive(Default)]
struct RunnableDocuments {
    counts: dashmap::DashMap<DocumentKey, RunnableDocumentCounts>,
}

#[derive(Default)]
struct RunnableDocumentCounts {
    total: usize,
    foreground: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DocumentCompetition {
    None,
    Background,
    Foreground,
}

impl RunnableDocuments {
    fn enter(self: &Arc<Self>, key: DocumentKey, priority: WorkPriority) -> RunnableDocumentGuard {
        self.counts
            .entry(key.clone())
            .and_modify(|counts| {
                counts.total += 1;
                counts.foreground += usize::from(priority == WorkPriority::Foreground);
            })
            .or_insert(RunnableDocumentCounts {
                total: 1,
                foreground: usize::from(priority == WorkPriority::Foreground),
            });
        RunnableDocumentGuard {
            runnable: Arc::clone(self),
            key,
            priority,
        }
    }

    fn competition_for(&self, current: &DocumentKey) -> DocumentCompetition {
        if self.counts.len() <= 1 {
            return DocumentCompetition::None;
        }
        if self
            .counts
            .iter()
            .any(|entry| entry.key() != current && entry.value().foreground > 0)
        {
            DocumentCompetition::Foreground
        } else {
            DocumentCompetition::Background
        }
    }
}

struct RunnableDocumentGuard {
    runnable: Arc<RunnableDocuments>,
    key: DocumentKey,
    priority: WorkPriority,
}

impl Drop for RunnableDocumentGuard {
    fn drop(&mut self) {
        if let dashmap::mapref::entry::Entry::Occupied(mut entry) =
            self.runnable.counts.entry(self.key.clone())
        {
            if entry.get().total == 1 {
                entry.remove();
            } else {
                let counts = entry.get_mut();
                counts.total -= 1;
                counts.foreground -= usize::from(self.priority == WorkPriority::Foreground);
            }
        }
    }
}

fn request_priority(request: &Request) -> WorkPriority {
    match request {
        Request::DeriveSnapshot(_)
        | Request::DeriveDocumentSnapshot(_)
        | Request::DeriveInjectionRegions(_)
        | Request::DeriveSemanticTokens(_) => WorkPriority::Background,
        Request::Handshake(_)
        | Request::Cancel(_)
        | Request::SyncDocument(_)
        | Request::ApplyDocumentEdits(_)
        | Request::ApplyDocumentEditsAndDerive(_)
        | Request::ResolveNode(_)
        | Request::NavigateNode(_)
        | Request::RunNodeScalar(_)
        | Request::DeriveSelectionRanges(_)
        | Request::RunCaptures(_)
        | Request::DeriveNativeBindings(_)
        | Request::ConfigureLanguages(_)
        | Request::CloseDocument(_) => WorkPriority::Foreground,
    }
}

fn submit_document_job(
    lanes: &DocumentLanes,
    key: DocumentKey,
    pool: &Arc<rayon::ThreadPool>,
    job: LaneJob,
) {
    let lane = {
        let entry = lanes
            .entry(key.clone())
            .or_insert_with(|| Arc::new(DocumentLane::new(key, Arc::downgrade(lanes))));
        Arc::clone(entry.value())
    };
    lane.submit(pool, job);
}

#[allow(clippy::too_many_arguments)]
fn handle_work(
    request: Request,
    analysis: &Arc<crate::language::LanguageCoordinator>,
    documents: &DocumentStore,
    closed_documents: &ClosedDocuments,
    document_capacity: &DocumentCapacity,
    retained_document_bytes: &RetainedDocumentBytes,
    retained_estimated_document_bytes: &RetainedEstimatedDocumentBytes,
    derived_pressure_lock: &Mutex<()>,
    queue_wait: Duration,
    cancellation: &crate::cancel::CancelToken,
    nested_work_policy: &(dyn Fn() -> crate::analysis::semantic::NestedWorkPolicy + Sync),
) -> Response {
    let context = request_context(&request)
        .expect("scheduled worker request must have context")
        .clone();
    if cancellation.is_cancelled() {
        return Response::RequestCancelled(RequestCancelled { context });
    }
    let started = Instant::now();
    let may_grow_derived_state = request_may_grow_derived_state(&request);
    let response = match request {
        Request::Handshake(_) | Request::Cancel(_) => {
            unreachable!("control requests are not scheduled")
        }
        Request::DeriveSnapshot(request) => derive_snapshot(request, queue_wait),
        Request::SyncDocument(request) => sync_document_with_analysis(
            request,
            analysis,
            documents,
            closed_documents,
            document_capacity,
            retained_document_bytes,
            retained_estimated_document_bytes,
        ),
        Request::ApplyDocumentEdits(request) => apply_document_edits(
            request,
            documents,
            retained_document_bytes,
            retained_estimated_document_bytes,
        ),
        Request::ApplyDocumentEditsAndDerive(request) => apply_document_edits_and_derive(
            request,
            documents,
            retained_document_bytes,
            retained_estimated_document_bytes,
            queue_wait,
            started,
        ),
        Request::DeriveDocumentSnapshot(request) => {
            derive_document_snapshot(request, documents, queue_wait, started)
        }
        Request::ResolveNode(request) => resolve_node(request, documents),
        Request::NavigateNode(request) => navigate_node(request, documents),
        Request::RunNodeScalar(request) => run_node_scalar(request, documents),
        Request::DeriveSelectionRanges(request) => derive_selection_ranges(request, documents),
        Request::DeriveInjectionRegions(request) => derive_injection_regions(request, documents),
        Request::DeriveSemanticTokens(request) => derive_semantic_tokens(
            request,
            documents,
            queue_wait,
            started,
            cancellation,
            nested_work_policy,
        ),
        Request::RunCaptures(request) => run_captures(request, documents),
        Request::DeriveNativeBindings(request) => derive_native_bindings(request, documents),
        Request::ConfigureLanguages(request) => configure_languages(request, analysis),
        Request::CloseDocument(request) => close_document(
            request,
            documents,
            closed_documents,
            retained_document_bytes,
            retained_estimated_document_bytes,
        ),
    };
    if pressure_checkpoint_required(may_grow_derived_state, &response)
        && let Err(reason) = enforce_derived_memory_budget(
            documents,
            derived_pressure_lock,
            &DocumentKey::from(&context),
            MAX_RETAINED_DERIVED_BYTES,
        )
    {
        return Response::WorkerRestartRequired(WorkerRestartRequired { context, reason });
    }
    if cancellation.is_cancelled() {
        Response::RequestCancelled(RequestCancelled { context })
    } else {
        response
    }
}

fn pressure_checkpoint_required(may_grow_derived_state: bool, response: &Response) -> bool {
    may_grow_derived_state
        && !matches!(
            response,
            Response::SemanticTokens(result) if result.cache_hit
        )
}

fn request_may_grow_derived_state(request: &Request) -> bool {
    matches!(
        request,
        Request::ResolveNode(_)
            | Request::DeriveSelectionRanges(_)
            | Request::DeriveInjectionRegions(_)
            | Request::DeriveSemanticTokens(_)
            | Request::RunCaptures(_)
            | Request::DeriveNativeBindings(_)
    )
}

fn request_context(request: &Request) -> Option<&RequestContext> {
    match request {
        Request::Handshake(_) | Request::Cancel(_) => None,
        Request::DeriveSnapshot(request) => Some(&request.context),
        Request::SyncDocument(request) => Some(&request.context),
        Request::ApplyDocumentEdits(request) => Some(&request.context),
        Request::ApplyDocumentEditsAndDerive(request) => Some(&request.context),
        Request::DeriveDocumentSnapshot(request) => Some(&request.context),
        Request::ResolveNode(request) => Some(&request.context),
        Request::NavigateNode(request) => Some(&request.context),
        Request::RunNodeScalar(request) => Some(&request.context),
        Request::DeriveSelectionRanges(request) => Some(&request.context),
        Request::DeriveInjectionRegions(request) => Some(&request.context),
        Request::DeriveSemanticTokens(request) => Some(&request.context),
        Request::RunCaptures(request) => Some(&request.context),
        Request::DeriveNativeBindings(request) => Some(&request.context),
        Request::ConfigureLanguages(request) => Some(&request.context),
        Request::CloseDocument(request) => Some(&request.context),
    }
}

fn document_key(request: &Request) -> Option<DocumentKey> {
    match request {
        Request::SyncDocument(request) => Some(DocumentKey::from(&request.context)),
        Request::ApplyDocumentEdits(request) => Some(DocumentKey::from(&request.context)),
        Request::ApplyDocumentEditsAndDerive(request) => Some(DocumentKey::from(&request.context)),
        Request::DeriveDocumentSnapshot(request) => Some(DocumentKey::from(&request.context)),
        Request::ResolveNode(request) => Some(DocumentKey::from(&request.context)),
        Request::NavigateNode(request) => Some(DocumentKey::from(&request.context)),
        Request::RunNodeScalar(request) => Some(DocumentKey::from(&request.context)),
        Request::DeriveSelectionRanges(request) => Some(DocumentKey::from(&request.context)),
        Request::DeriveInjectionRegions(request) => Some(DocumentKey::from(&request.context)),
        Request::DeriveSemanticTokens(request) => Some(DocumentKey::from(&request.context)),
        Request::RunCaptures(request) => Some(DocumentKey::from(&request.context)),
        Request::DeriveNativeBindings(request) => Some(DocumentKey::from(&request.context)),
        Request::CloseDocument(request) => Some(DocumentKey::from(&request.context)),
        Request::Handshake(_)
        | Request::Cancel(_)
        | Request::DeriveSnapshot(_)
        | Request::ConfigureLanguages(_) => None,
    }
}

fn is_cancellable_reader_request(request: &Request) -> bool {
    matches!(
        request,
        Request::DeriveSnapshot(_)
            | Request::DeriveDocumentSnapshot(_)
            | Request::ResolveNode(_)
            | Request::NavigateNode(_)
            | Request::RunNodeScalar(_)
            | Request::DeriveSelectionRanges(_)
            | Request::DeriveInjectionRegions(_)
            | Request::DeriveSemanticTokens(_)
            | Request::RunCaptures(_)
            | Request::DeriveNativeBindings(_)
    )
}

fn resolve_node(request: ResolveNode, documents: &DocumentStore) -> Response {
    let context = request.context.clone();
    let Some(replica) = documents
        .get(&DocumentKey::from(&context))
        .map(|entry| Arc::clone(entry.value()))
    else {
        return Response::Error(WorkerError {
            context: Some(context),
            message: "document replica is missing; full sync required".into(),
        });
    };
    match replica.lock() {
        Ok(mut replica) => match replica.resolve_node(request) {
            Ok(result) => Response::Nodes(result),
            Err(message) => Response::Error(WorkerError {
                context: Some(context),
                message,
            }),
        },
        Err(_) => poisoned_document_restart(context),
    }
}

fn navigate_node(request: NavigateNode, documents: &DocumentStore) -> Response {
    let context = request.context.clone();
    let Some(replica) = documents
        .get(&DocumentKey::from(&context))
        .map(|entry| Arc::clone(entry.value()))
    else {
        return Response::Error(WorkerError {
            context: Some(context),
            message: "document replica is missing; full sync required".into(),
        });
    };
    match replica.lock() {
        Ok(mut replica) => match replica.navigate_node(request) {
            Ok(result) => Response::Nodes(result),
            Err(message) => Response::Error(WorkerError {
                context: Some(context),
                message,
            }),
        },
        Err(_) => poisoned_document_restart(context),
    }
}

fn run_node_scalar(request: RunNodeScalar, documents: &DocumentStore) -> Response {
    let context = request.context.clone();
    let Some(replica) = documents
        .get(&DocumentKey::from(&context))
        .map(|entry| Arc::clone(entry.value()))
    else {
        return Response::Error(WorkerError {
            context: Some(context),
            message: "document replica is missing; full sync required".into(),
        });
    };
    match replica.lock() {
        Ok(mut replica) => match replica.run_node_scalar(request) {
            Ok(result) => Response::NodeScalar(result),
            Err(message) => Response::Error(WorkerError {
                context: Some(context),
                message,
            }),
        },
        Err(_) => poisoned_document_restart(context),
    }
}

fn derive_selection_ranges(request: DeriveSelectionRanges, documents: &DocumentStore) -> Response {
    let context = request.context.clone();
    let Some(replica) = documents
        .get(&DocumentKey::from(&context))
        .map(|entry| Arc::clone(entry.value()))
    else {
        return Response::Error(WorkerError {
            context: Some(context),
            message: "document replica is missing; full sync required".into(),
        });
    };
    match replica.lock() {
        Ok(replica) => match replica.derive_selection_ranges(request) {
            Ok(result) => Response::SelectionRanges(result),
            Err(message) => Response::Error(WorkerError {
                context: Some(context),
                message,
            }),
        },
        Err(_) => poisoned_document_restart(context),
    }
}

fn derive_injection_regions(
    request: DeriveInjectionRegions,
    documents: &DocumentStore,
) -> Response {
    let context = request.context.clone();
    let Some(replica) = documents
        .get(&DocumentKey::from(&context))
        .map(|entry| Arc::clone(entry.value()))
    else {
        return Response::Error(WorkerError {
            context: Some(context),
            message: "document replica is missing; full sync required".into(),
        });
    };
    match replica.lock() {
        Ok(mut replica) => match replica.derive_injection_regions(request) {
            Ok(result) => Response::InjectionRegions(result),
            Err(message) => Response::Error(WorkerError {
                context: Some(context),
                message,
            }),
        },
        Err(_) => poisoned_document_restart(context),
    }
}

fn derive_semantic_tokens(
    request: DeriveSemanticTokens,
    documents: &DocumentStore,
    queue_wait: Duration,
    started: Instant,
    cancellation: &crate::cancel::CancelToken,
    nested_work_policy: &(dyn Fn() -> crate::analysis::semantic::NestedWorkPolicy + Sync),
) -> Response {
    let context = request.context.clone();
    let Some(replica) = documents
        .get(&DocumentKey::from(&context))
        .map(|entry| Arc::clone(entry.value()))
    else {
        return Response::Error(WorkerError {
            context: Some(context),
            message: "document replica is missing; full sync required".into(),
        });
    };
    match replica.lock() {
        Ok(mut replica) => {
            match replica.derive_semantic_tokens_with_telemetry(
                request,
                queue_wait,
                started,
                rayon::current_num_threads(),
                Some(cancellation),
                Some(nested_work_policy),
            ) {
                Ok(result) => Response::SemanticTokens(result),
                Err(message) => Response::Error(WorkerError {
                    context: Some(context),
                    message,
                }),
            }
        }
        Err(_) => poisoned_document_restart(context),
    }
}

fn run_captures(request: RunCaptures, documents: &DocumentStore) -> Response {
    let context = request.context.clone();
    let Some(replica) = documents
        .get(&DocumentKey::from(&context))
        .map(|entry| Arc::clone(entry.value()))
    else {
        return Response::Error(WorkerError {
            context: Some(context),
            message: "document replica is missing; full sync required".into(),
        });
    };
    match replica.lock() {
        Ok(replica) => match replica.run_captures(request) {
            Ok(result) => Response::Captures(result),
            Err(message) => Response::Error(WorkerError {
                context: Some(context),
                message,
            }),
        },
        Err(_) => poisoned_document_restart(context),
    }
}

fn derive_native_bindings(request: DeriveNativeBindings, documents: &DocumentStore) -> Response {
    let context = request.context.clone();
    let Some(replica) = documents
        .get(&DocumentKey::from(&context))
        .map(|entry| Arc::clone(entry.value()))
    else {
        return Response::Error(WorkerError {
            context: Some(context),
            message: "document replica is missing; full sync required".into(),
        });
    };
    match replica.lock() {
        Ok(replica) => match replica.derive_native_bindings(request) {
            Ok(result) => Response::NativeBindings(result),
            Err(message) => Response::Error(WorkerError {
                context: Some(context),
                message,
            }),
        },
        Err(_) => poisoned_document_restart(context),
    }
}

fn configure_languages(
    request: ConfigureLanguages,
    analysis: &Arc<crate::language::LanguageCoordinator>,
) -> Response {
    let context = request.context;
    analysis.configure_worker_runtime(
        request.catalog.search_paths,
        request.catalog.capture_mappings,
    );
    let mut configured = 0;
    for asset in request.catalog.assets {
        let key = GrammarKey::from_asset(&asset);
        let language_name = asset.language;
        let queries = asset.queries;
        analysis.query_store().remove_queries(&language_name);
        let response = WORKER_THREAD_STATE.with(|state| {
            state
                .borrow_mut()
                .with_parser(key, context.clone(), |parser, _| {
                    let Some(language) = parser.language().map(|language| (*language).clone())
                    else {
                        return Response::Error(WorkerError {
                            context: Some(context.clone()),
                            message: "parser has no configured language".into(),
                        });
                    };
                    match worker_analysis(
                        analysis,
                        &language_name,
                        language,
                        queries,
                        WorkerQuerySet::All,
                    ) {
                        Ok(()) => Response::LanguageCatalogAck(LanguageCatalogAck {
                            context: context.clone(),
                            languages: configured + 1,
                        }),
                        Err(message) => Response::Error(WorkerError {
                            context: Some(context.clone()),
                            message,
                        }),
                    }
                })
        });
        if matches!(
            response,
            Response::Error(_) | Response::WorkerRestartRequired(_)
        ) {
            return response;
        }
        configured += 1;
    }
    Response::LanguageCatalogAck(LanguageCatalogAck {
        context,
        languages: configured,
    })
}

fn poisoned_document_restart(context: RequestContext) -> Response {
    Response::WorkerRestartRequired(WorkerRestartRequired {
        context,
        reason: "document replica lock is poisoned; restart and resync".into(),
    })
}

#[cfg(test)]
fn sync_document(
    request: SyncDocument,
    documents: &DocumentStore,
    closed_documents: &ClosedDocuments,
    document_capacity: &DocumentCapacity,
    retained_document_bytes: &RetainedDocumentBytes,
) -> Response {
    let retained_estimated_document_bytes = Arc::new(AtomicUsize::new(0));
    sync_document_with_analysis(
        request,
        &Arc::new(crate::language::LanguageCoordinator::new()),
        documents,
        closed_documents,
        document_capacity,
        retained_document_bytes,
        &retained_estimated_document_bytes,
    )
}

fn sync_document_with_analysis(
    request: SyncDocument,
    analysis: &Arc<crate::language::LanguageCoordinator>,
    documents: &DocumentStore,
    closed_documents: &ClosedDocuments,
    document_capacity: &DocumentCapacity,
    retained_document_bytes: &RetainedDocumentBytes,
    retained_estimated_document_bytes: &RetainedEstimatedDocumentBytes,
) -> Response {
    let context = request.context.clone();
    let request_text_len = request.text.len();
    if let Err(message) = validate_document_size(request_text_len) {
        return Response::Error(WorkerError {
            context: Some(context),
            message,
        });
    }
    let document_key = DocumentKey::from(&context);
    let grammar_key = GrammarKey::from_sync(&request);
    let (replaced_bytes, replaced_estimated_bytes) = if let Some(existing) = documents
        .get(&document_key)
        .map(|entry| Arc::clone(entry.value()))
    {
        match existing.lock() {
            Ok(replica) => {
                match replica.validate_replacement(
                    &context,
                    &request.language,
                    &grammar_key,
                    &request.queries,
                ) {
                    Ok(()) => (
                        replica.text.len(),
                        replica.estimated_memory().non_evictable_bytes,
                    ),
                    Err(message) => {
                        return Response::Error(WorkerError {
                            context: Some(context),
                            message,
                        });
                    }
                }
            }
            Err(_) => {
                return poisoned_document_restart(context);
            }
        }
    } else {
        if let Some(closed_incarnation) = closed_documents.get(&document_key)
            && context.incarnation <= *closed_incarnation
        {
            return Response::Error(WorkerError {
                context: Some(context),
                message: "full sync targets a closed document incarnation".into(),
            });
        }
        (0, 0)
    };
    let reserves_slot =
        !documents.contains_key(&document_key) && !closed_documents.contains_key(&document_key);
    if reserves_slot {
        let mut known_documents = document_capacity
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if *known_documents >= MAX_DOCUMENT_REPLICAS {
            return Response::WorkerRestartRequired(WorkerRestartRequired {
                context,
                reason: format!(
                    "document identity limit {MAX_DOCUMENT_REPLICAS} reached; restart and resync"
                ),
            });
        }
        *known_documents += 1;
    }
    let reserved_growth =
        match reserve_retained_growth(retained_document_bytes, replaced_bytes, request_text_len) {
            Ok(reserved) => reserved,
            Err(message) => {
                if reserves_slot {
                    let mut known_documents = document_capacity
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    *known_documents = known_documents.saturating_sub(1);
                }
                return Response::Error(WorkerError {
                    context: Some(context),
                    message,
                });
            }
        };
    let response = WORKER_THREAD_STATE.with(|state| {
        state
            .borrow_mut()
            .with_parser(grammar_key, context.clone(), |parser, _| {
                match DocumentReplica::sync_with_analysis(request, parser, Arc::clone(analysis)) {
                    Ok((replica, ack)) => {
                        let estimated_bytes = replica.estimated_memory().non_evictable_bytes;
                        if let Err(message) = replace_retained_estimated_bytes(
                            retained_estimated_document_bytes,
                            replaced_estimated_bytes,
                            estimated_bytes,
                            MAX_RETAINED_ESTIMATED_DOCUMENT_BYTES,
                        ) {
                            return Response::Error(WorkerError {
                                context: Some(context),
                                message,
                            });
                        }
                        documents.insert(document_key, Arc::new(Mutex::new(replica)));
                        closed_documents.remove(&DocumentKey::from(&ack.context));
                        Response::DocumentAck(ack)
                    }
                    Err(message) => Response::Error(WorkerError {
                        context: Some(context),
                        message,
                    }),
                }
            })
    });
    if reserves_slot && !matches!(&response, Response::DocumentAck(_)) {
        let mut known_documents = document_capacity
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *known_documents = known_documents.saturating_sub(1);
    }
    if matches!(&response, Response::DocumentAck(_)) && request_text_len < replaced_bytes {
        replace_retained_bytes(retained_document_bytes, replaced_bytes, request_text_len)
            .expect("committing a document shrink cannot exceed the retained-byte budget");
    } else if !matches!(&response, Response::DocumentAck(_)) && reserved_growth {
        replace_retained_bytes(retained_document_bytes, request_text_len, replaced_bytes)
            .expect("rolling back retained growth cannot exceed the retained-byte budget");
    }
    response
}

fn apply_document_edits(
    request: ApplyDocumentEdits,
    documents: &DocumentStore,
    retained_document_bytes: &RetainedDocumentBytes,
    retained_estimated_document_bytes: &RetainedEstimatedDocumentBytes,
) -> Response {
    let context = request.context.clone();
    let document_key = DocumentKey::from(&context);
    let Some(replica) = documents
        .get(&document_key)
        .map(|entry| Arc::clone(entry.value()))
    else {
        return Response::Error(WorkerError {
            context: Some(context),
            message: "document replica is missing; full sync required".into(),
        });
    };
    let grammar_key = match replica.lock() {
        Ok(replica) => replica.grammar_key.clone(),
        Err(_) => {
            return poisoned_document_restart(context);
        }
    };
    WORKER_THREAD_STATE.with(|state| {
        state
            .borrow_mut()
            .with_parser(grammar_key, context.clone(), |parser, _| {
                let Ok(mut replica) = replica.lock() else {
                    return poisoned_document_restart(context);
                };
                match replica.apply(
                    request,
                    parser,
                    Some((retained_document_bytes, retained_estimated_document_bytes)),
                ) {
                    Ok(ack) => Response::DocumentAck(ack),
                    Err(message) => Response::Error(WorkerError {
                        context: Some(context),
                        message,
                    }),
                }
            })
    })
}

fn apply_document_edits_and_derive(
    request: ApplyDocumentEditsAndDerive,
    documents: &DocumentStore,
    retained_document_bytes: &RetainedDocumentBytes,
    retained_estimated_document_bytes: &RetainedEstimatedDocumentBytes,
    queue_wait: Duration,
    started: Instant,
) -> Response {
    let context = request.context.clone();
    let document_key = DocumentKey::from(&context);
    let Some(replica) = documents
        .get(&document_key)
        .map(|entry| Arc::clone(entry.value()))
    else {
        return Response::Error(WorkerError {
            context: Some(context),
            message: "document replica is missing; full sync required".into(),
        });
    };
    let grammar_key = match replica.lock() {
        Ok(replica) => replica.grammar_key.clone(),
        Err(_) => {
            return poisoned_document_restart(context);
        }
    };
    WORKER_THREAD_STATE.with(|state| {
        state
            .borrow_mut()
            .with_parser(grammar_key, context.clone(), |parser, parser_cache_hit| {
                let Ok(mut replica) = replica.lock() else {
                    return poisoned_document_restart(context);
                };
                let edits = ApplyDocumentEdits {
                    context: request.context,
                    base_version: request.base_version,
                    edits: request.edits,
                };
                if let Err(message) = replica.apply(
                    edits,
                    parser,
                    Some((retained_document_bytes, retained_estimated_document_bytes)),
                ) {
                    return Response::Error(WorkerError {
                        context: Some(context),
                        message,
                    });
                }
                match replica.derive_with_telemetry(
                    context.clone(),
                    Some(parser_cache_hit),
                    queue_wait,
                    started,
                ) {
                    Ok(snapshot) => Response::Snapshot(snapshot),
                    Err(message) => Response::Error(WorkerError {
                        context: Some(context),
                        message,
                    }),
                }
            })
    })
}

fn derive_document_snapshot(
    request: DeriveDocumentSnapshot,
    documents: &DocumentStore,
    queue_wait: Duration,
    started: Instant,
) -> Response {
    let context = request.context;
    let document_key = DocumentKey::from(&context);
    let Some(replica) = documents
        .get(&document_key)
        .map(|entry| Arc::clone(entry.value()))
    else {
        return Response::Error(WorkerError {
            context: Some(context),
            message: "document replica is missing; full sync required".into(),
        });
    };
    match replica.lock() {
        Ok(replica) => {
            match replica.derive_with_telemetry(context.clone(), None, queue_wait, started) {
                Ok(snapshot) => Response::Snapshot(snapshot),
                Err(message) => Response::Error(WorkerError {
                    context: Some(context),
                    message,
                }),
            }
        }
        Err(_) => poisoned_document_restart(context),
    }
}

fn close_document(
    request: CloseDocument,
    documents: &DocumentStore,
    closed_documents: &ClosedDocuments,
    retained_document_bytes: &RetainedDocumentBytes,
    retained_estimated_document_bytes: &RetainedEstimatedDocumentBytes,
) -> Response {
    let context = request.context;
    let document_key = DocumentKey::from(&context);
    let Some(replica) = documents
        .get(&document_key)
        .map(|entry| Arc::clone(entry.value()))
    else {
        return Response::Error(WorkerError {
            context: Some(context),
            message: "document replica is missing".into(),
        });
    };
    let (removed_bytes, removed_estimated_bytes) = match replica.lock() {
        Ok(replica) => {
            if let Err(message) = replica.validate_lifetime(&context) {
                return Response::Error(WorkerError {
                    context: Some(context),
                    message,
                });
            }
            (
                replica.text.len(),
                replica.estimated_memory().non_evictable_bytes,
            )
        }
        Err(_) => return poisoned_document_restart(context),
    };
    documents.remove(&document_key);
    replace_retained_bytes(retained_document_bytes, removed_bytes, 0)
        .expect("removing a document cannot exceed the retained-byte budget");
    replace_retained_estimated_bytes(
        retained_estimated_document_bytes,
        removed_estimated_bytes,
        0,
        MAX_RETAINED_ESTIMATED_DOCUMENT_BYTES,
    )
    .expect("removing a document cannot exceed the estimated non-evictable budget");
    closed_documents.insert(document_key, context.incarnation);
    Response::DocumentClosed(DocumentClosed { context })
}

pub fn run<R, W>(reader: R, writer: W, compute_threads: usize) -> io::Result<()>
where
    R: Read,
    W: Write + Send + 'static,
{
    run_with_build_id(reader, writer, compute_threads, BUILD_ID)
}

fn run_with_build_id<R, W>(
    mut reader: R,
    writer: W,
    compute_threads: usize,
    build_id: &str,
) -> io::Result<()>
where
    R: Read,
    W: Write + Send + 'static,
{
    if compute_threads == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "worker compute thread count must be positive",
        ));
    }
    let pool = Arc::new(
        rayon::ThreadPoolBuilder::new()
            .num_threads(compute_threads)
            .thread_name(|index| format!("kakehashi-tree-worker-{index}"))
            .build()
            .map_err(io::Error::other)?,
    );
    let max_inflight = compute_threads.saturating_mul(2).max(1);
    let (permits, permit_rx) = mpsc::sync_channel(max_inflight);
    for _ in 0..max_inflight {
        permits
            .send(())
            .map_err(|_| io::Error::other("worker admission queue stopped"))?;
    }
    let permit_rx = Arc::new(Mutex::new(permit_rx));
    let (responses, response_rx) =
        mpsc::sync_channel::<(Response, Option<AdmissionPermit>)>(max_inflight);
    let writer_thread = std::thread::spawn(move || -> io::Result<()> {
        let mut writer = writer;
        for (response, _permit) in response_rx {
            encode_frame(&mut writer, &response)?;
        }
        Ok(())
    });

    let handshake = match decode_frame::<_, Request>(&mut reader)? {
        Some(Request::Handshake(handshake)) => handshake,
        None => {
            drop(responses);
            return join_writer(writer_thread);
        }
        Some(_) => {
            drop(responses);
            join_writer(writer_thread)?;
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "tree worker requires handshake as its first message",
            ));
        }
    };
    if handshake.protocol_version != PROTOCOL_VERSION || handshake.build_id != build_id {
        responses
            .send((
                Response::Error(WorkerError {
                    context: None,
                    message: "worker protocol/build identity mismatch".into(),
                }),
                None,
            ))
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "worker writer stopped"))?;
        drop(responses);
        join_writer(writer_thread)?;
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "worker protocol/build identity mismatch",
        ));
    }
    let worker_generation = handshake.worker_generation;
    responses
        .send((
            Response::HandshakeReady(HandshakeReady {
                protocol_version: PROTOCOL_VERSION,
                build_id: build_id.into(),
                worker_generation,
                compute_threads,
            }),
            None,
        ))
        .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "worker writer stopped"))?;
    #[cfg(feature = "e2e")]
    inject_idle_worker_crash_once();

    let documents = Arc::new(dashmap::DashMap::new());
    let analysis = Arc::new(crate::language::LanguageCoordinator::new());
    let closed_documents = Arc::new(dashmap::DashMap::new());
    let document_capacity = Arc::new(Mutex::new(0));
    let retained_document_bytes = Arc::new(AtomicUsize::new(0));
    let retained_estimated_document_bytes = Arc::new(AtomicUsize::new(0));
    let derived_pressure_lock = Arc::new(Mutex::new(()));
    let document_lanes: DocumentLanes = Arc::new(dashmap::DashMap::new());
    let runnable_documents = Arc::new(RunnableDocuments::default());
    let cancellations = Arc::new(dashmap::DashMap::<u64, crate::cancel::CancelToken>::new());
    // The client permits at most four requests per compute thread. Decode one
    // frame beyond this pending window so cancellation stays observable under
    // saturation, but wait before scheduling any excess ordinary work.
    let max_pending = compute_threads.saturating_mul(4).max(1);
    let (completion_tx, completion_rx) = mpsc::channel::<()>();
    let mut pending = 0_usize;
    while let Some(request) = decode_frame::<_, Request>(&mut reader)? {
        if let Request::Cancel(cancel) = request {
            if cancel.worker_generation == worker_generation {
                cancellations.entry(cancel.request_id).or_default().cancel();
            }
            continue;
        }
        let Some(context) = request_context(&request) else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "handshake may only be sent once",
            ));
        };
        if context.worker_generation != worker_generation {
            responses
                .send((
                    Response::Error(WorkerError {
                        context: Some(context.clone()),
                        message: "stale worker generation".into(),
                    }),
                    None,
                ))
                .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "worker writer stopped"))?;
            continue;
        }
        #[cfg(feature = "e2e")]
        if let Some(response) = inject_worker_failure_once(&request) {
            responses
                .send((response, None))
                .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "worker writer stopped"))?;
            continue;
        }
        let context = context.clone();
        let request_id = context.request_id;
        let cancellation = if is_cancellable_reader_request(&request) {
            cancellations.entry(request_id).or_default().clone()
        } else {
            cancellations.remove(&request_id);
            crate::cancel::CancelToken::default()
        };
        if pending == max_pending {
            completion_rx
                .recv()
                .map_err(|_| io::Error::other("worker completion queue stopped"))?;
            pending -= 1;
        }
        pending += 1;
        let enqueued = Instant::now();
        let responses = responses.clone();
        let permits = permits.clone();
        let permit_rx = Arc::clone(&permit_rx);
        let completion_tx = completion_tx.clone();
        let documents = Arc::clone(&documents);
        let analysis = Arc::clone(&analysis);
        let closed_documents = Arc::clone(&closed_documents);
        let document_capacity = Arc::clone(&document_capacity);
        let retained_document_bytes = Arc::clone(&retained_document_bytes);
        let retained_estimated_document_bytes = Arc::clone(&retained_estimated_document_bytes);
        let derived_pressure_lock = Arc::clone(&derived_pressure_lock);
        let job_cancellations = Arc::clone(&cancellations);
        let lane_key = document_key(&request);
        let priority = request_priority(&request);
        let runnable_guard = lane_key
            .as_ref()
            .map(|key| runnable_documents.enter(key.clone(), priority));
        #[cfg(feature = "e2e")]
        if lane_key.as_ref().is_some_and(|key| {
            std::env::var("KAKEHASHI_TREE_WORKER_RUNNABLE_ENTERED_URI_SUFFIX")
                .ok()
                .is_some_and(|suffix| key.uri.ends_with(&suffix))
        }) && let Ok(path) = std::env::var("KAKEHASHI_TREE_WORKER_RUNNABLE_ENTERED_FILE")
        {
            let _ = std::fs::write(path, b"runnable");
        }
        let job_runnable_documents = Arc::clone(&runnable_documents);
        let action_key = lane_key.clone();
        let job: LaneJob = Box::new(move || {
            #[cfg(feature = "e2e")]
            if action_key.as_ref().is_some_and(|key| {
                std::env::var("KAKEHASHI_TREE_WORKER_ADMISSION_DELAY_URI_SUFFIX")
                    .ok()
                    .is_some_and(|suffix| key.uri.ends_with(&suffix))
            }) && let Ok(milliseconds) =
                std::env::var("KAKEHASHI_TREE_WORKER_ADMISSION_DELAY_MS")
                    .unwrap_or_default()
                    .parse::<u64>()
            {
                std::thread::sleep(Duration::from_millis(milliseconds));
                if let Ok(path) = std::env::var("KAKEHASHI_TREE_WORKER_ADMISSION_RELEASE_FILE") {
                    let deadline = Instant::now() + Duration::from_secs(10);
                    while !std::path::Path::new(&path).exists() && Instant::now() < deadline {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                }
            }
            permit_rx
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .recv()
                .expect("worker admission queue stopped before its jobs");
            let permit = AdmissionPermit(permits);
            #[cfg(feature = "e2e")]
            let nested_parallelism_was_allowed = AtomicBool::new(false);
            #[cfg(feature = "e2e")]
            let nested_parallelism_yield_was_reported = AtomicBool::new(false);
            let nested_work_policy = || {
                use crate::analysis::semantic::NestedWorkPolicy;

                #[cfg(feature = "e2e")]
                let force_nested =
                    std::env::var_os("KAKEHASHI_TREE_WORKER_FORCE_NESTED_PARALLELISM").is_some();
                #[cfg(not(feature = "e2e"))]
                let force_nested = false;
                let policy = if force_nested {
                    NestedWorkPolicy::Parallel
                } else {
                    match action_key
                        .as_ref()
                        .map(|key| job_runnable_documents.competition_for(key))
                        .unwrap_or(DocumentCompetition::None)
                    {
                        DocumentCompetition::None => NestedWorkPolicy::Parallel,
                        DocumentCompetition::Background => NestedWorkPolicy::Sequential,
                        DocumentCompetition::Foreground => NestedWorkPolicy::Yield,
                    }
                };
                #[cfg(feature = "e2e")]
                let trace_fairness = action_key.as_ref().is_some_and(|key| {
                    std::env::var("KAKEHASHI_TREE_WORKER_FAIRNESS_TRACE_URI_SUFFIX")
                        .ok()
                        .is_some_and(|suffix| key.uri.ends_with(&suffix))
                });
                #[cfg(feature = "e2e")]
                if policy == NestedWorkPolicy::Parallel {
                    if trace_fairness
                        && let Ok(path) =
                            std::env::var("KAKEHASHI_TREE_WORKER_FAIRNESS_ALLOWED_FILE")
                    {
                        let _ = std::fs::write(path, b"allowed");
                    }
                    nested_parallelism_was_allowed.store(true, Ordering::Relaxed);
                } else if nested_parallelism_was_allowed.load(Ordering::Relaxed)
                    && !nested_parallelism_yield_was_reported.swap(true, Ordering::Relaxed)
                    && trace_fairness
                    && let Ok(path) = std::env::var("KAKEHASHI_TREE_WORKER_FAIRNESS_YIELDED_FILE")
                {
                    let _ = std::fs::write(path, b"yielded");
                }
                policy
            };
            let response = handle_work(
                request,
                &analysis,
                &documents,
                &closed_documents,
                &document_capacity,
                &retained_document_bytes,
                &retained_estimated_document_bytes,
                &derived_pressure_lock,
                enqueued.elapsed(),
                &cancellation,
                &nested_work_policy,
            );
            job_cancellations.remove(&request_id);
            let action = if action_key
                .as_ref()
                .is_some_and(|key| documents.contains_key(key))
            {
                LaneAction::Keep
            } else {
                LaneAction::Retire
            };
            let _ = responses.send((response, Some(permit)));
            let _ = completion_tx.send(());
            drop(runnable_guard);
            action
        });
        #[cfg(feature = "e2e")]
        let scheduled_key = lane_key.clone();
        #[cfg(feature = "e2e")]
        if scheduled_key.as_ref().is_some_and(|key| {
            std::env::var("KAKEHASHI_TREE_WORKER_SCHEDULE_RELEASE_URI_SUFFIX")
                .ok()
                .is_some_and(|suffix| key.uri.ends_with(&suffix))
        }) && let Ok(path) = std::env::var("KAKEHASHI_TREE_WORKER_SCHEDULE_RELEASE_FILE")
        {
            let deadline = Instant::now() + Duration::from_secs(10);
            while !std::path::Path::new(&path).exists() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        if let Some(lane_key) = lane_key {
            submit_document_job(&document_lanes, lane_key, &pool, job);
        } else {
            pool.spawn(move || {
                let _ = job();
            });
        }
        #[cfg(feature = "e2e")]
        if scheduled_key.as_ref().is_some_and(|key| {
            std::env::var("KAKEHASHI_TREE_WORKER_SCHEDULED_URI_SUFFIX")
                .ok()
                .is_some_and(|suffix| key.uri.ends_with(&suffix))
        }) && let Ok(path) = std::env::var("KAKEHASHI_TREE_WORKER_SCHEDULED_FILE")
        {
            let _ = std::fs::write(path, b"scheduled");
        }
    }
    for _ in 0..pending {
        completion_rx
            .recv()
            .map_err(|_| io::Error::other("worker completion queue stopped"))?;
    }
    drop(responses);
    join_writer(writer_thread)
}

#[cfg(feature = "e2e")]
fn inject_worker_failure_once(request: &Request) -> Option<Response> {
    let Request::SyncDocument(sync) = request else {
        return None;
    };
    if let Ok(marker) = std::env::var("KAKEHASHI_TREE_WORKER_HANG_ONCE_FILE")
        && std::env::var("KAKEHASHI_TREE_WORKER_HANG_URI_SUFFIX")
            .ok()
            .is_none_or(|suffix| sync.context.uri.ends_with(&suffix))
        && std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(marker)
            .is_ok()
    {
        loop {
            std::thread::park();
        }
    }
    if let Ok(marker) = std::env::var("KAKEHASHI_TREE_WORKER_CRASH_ONCE_FILE")
        && std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(marker)
            .is_ok()
    {
        std::process::exit(86);
    }
    if let Ok(marker) = std::env::var("KAKEHASHI_TREE_WORKER_RESTART_ONCE_FILE")
        && std::env::var("KAKEHASHI_TREE_WORKER_RESTART_URI_SUFFIX")
            .ok()
            .is_none_or(|suffix| sync.context.uri.ends_with(&suffix))
        && std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(marker)
            .is_ok()
    {
        return Some(Response::WorkerRestartRequired(WorkerRestartRequired {
            context: sync.context.clone(),
            reason: "injected systemic restart".into(),
        }));
    }
    if std::env::var("KAKEHASHI_TREE_WORKER_ERROR_URI_SUFFIX")
        .ok()
        .is_some_and(|suffix| sync.context.uri.ends_with(&suffix))
    {
        return Some(Response::Error(WorkerError {
            context: Some(sync.context.clone()),
            message: "injected document rejection".into(),
        }));
    }
    None
}

#[cfg(feature = "e2e")]
fn inject_idle_worker_crash_once() {
    let Ok(marker) = std::env::var("KAKEHASHI_TREE_WORKER_IDLE_CRASH_ONCE_FILE") else {
        return;
    };
    if std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(marker)
        .is_err()
    {
        return;
    }
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_millis(100));
        std::process::exit(87);
    });
}

fn join_writer(thread: std::thread::JoinHandle<io::Result<()>>) -> io::Result<()> {
    thread
        .join()
        .map_err(|_| io::Error::other("worker writer panicked"))?
}

struct AdmissionPermit(mpsc::SyncSender<()>);

impl Drop for AdmissionPermit {
    fn drop(&mut self) {
        let _ = self.0.send(());
    }
}

pub fn run_stdio(compute_threads: usize) -> io::Result<()> {
    let build_id = artifact_digest(&std::env::current_exe()?)?;
    run_with_build_id(
        std::io::stdin(),
        std::io::stdout(),
        compute_threads,
        &build_id,
    )
}

pub fn artifact_digest(path: &std::path::Path) -> io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(format!("sha256:{:x}", digest.finalize()))
}

pub struct Client {
    child: Arc<Mutex<Child>>,
    terminated_by_transport: Arc<AtomicBool>,
    outbound: Mutex<Option<mpsc::Sender<Request>>>,
    routes: Arc<Mutex<HashMap<u64, Route>>>,
    reader: Option<std::thread::JoinHandle<()>>,
    writer: Option<std::thread::JoinHandle<()>>,
    ready: HandshakeReady,
    max_inflight: usize,
}

const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

#[cfg(any(test, feature = "e2e"))]
fn parse_request_timeout(value: Option<&str>) -> Duration {
    value
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|milliseconds| *milliseconds > 0)
        .map(Duration::from_millis)
        .unwrap_or(DEFAULT_REQUEST_TIMEOUT)
}

fn request_timeout(_uri: &str) -> Duration {
    #[cfg(feature = "e2e")]
    {
        if std::env::var("KAKEHASHI_TREE_WORKER_REQUEST_TIMEOUT_URI_SUFFIX")
            .ok()
            .is_some_and(|suffix| !_uri.ends_with(&suffix))
        {
            return DEFAULT_REQUEST_TIMEOUT;
        }
        parse_request_timeout(
            std::env::var("KAKEHASHI_TREE_WORKER_REQUEST_TIMEOUT_MS")
                .ok()
                .as_deref(),
        )
    }
    #[cfg(not(feature = "e2e"))]
    DEFAULT_REQUEST_TIMEOUT
}

struct Route {
    expected: RequestContext,
    sender: mpsc::Sender<io::Result<Response>>,
}

impl Client {
    pub fn spawn(
        executable: &std::path::Path,
        compute_threads: usize,
        worker_generation: u64,
    ) -> io::Result<Self> {
        if compute_threads == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "worker compute thread count must be positive",
            ));
        }
        let build_id = artifact_digest(executable)?;
        let mut child = Command::new(executable)
            .args(["__tree-worker", "--threads", &compute_threads.to_string()])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()?;
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("worker stdin was not piped"))?;
        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("worker stdout was not piped"))?;
        let child = Arc::new(Mutex::new(child));
        let terminated_by_transport = Arc::new(AtomicBool::new(false));
        let routes = Arc::new(Mutex::new(HashMap::<u64, Route>::new()));
        let reader_routes = Arc::clone(&routes);
        let reader_child = Arc::clone(&child);
        let reader_terminated_by_transport = Arc::clone(&terminated_by_transport);
        let (handshake_tx, handshake_rx) = mpsc::channel();
        let reader = std::thread::spawn(move || {
            let mut handshake_tx = Some(handshake_tx);
            loop {
                match decode_frame::<_, Response>(&mut stdout) {
                    Ok(Some(response)) => {
                        if let Some(handshake) = handshake_tx.take() {
                            let _ = handshake.send(Ok(response));
                            continue;
                        }
                        if let Err(error) = route_response(&reader_routes, response) {
                            let kind = error.kind();
                            let message = error.to_string();
                            fail_routes(None, &reader_routes, kind, &message);
                            break;
                        }
                    }
                    Ok(None) => {
                        fail_routes(
                            handshake_tx.take(),
                            &reader_routes,
                            io::ErrorKind::UnexpectedEof,
                            "tree worker closed its response stream",
                        );
                        break;
                    }
                    Err(error) => {
                        let kind = error.kind();
                        let message = error.to_string();
                        fail_routes(handshake_tx.take(), &reader_routes, kind, &message);
                        break;
                    }
                }
            }
            if let Ok(mut child) = reader_child.lock() {
                terminate_by_transport(
                    &mut child,
                    &reader_terminated_by_transport,
                    Duration::from_secs(1),
                );
            }
        });
        if let Err(error) = encode_frame(
            &mut stdin,
            &Request::Handshake(Handshake {
                protocol_version: PROTOCOL_VERSION,
                build_id: build_id.clone(),
                worker_generation,
            }),
        ) {
            return failed_spawn(child, stdin, reader, error);
        }
        let ready = match handshake_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(Response::HandshakeReady(ready)))
                if ready.protocol_version == PROTOCOL_VERSION
                    && ready.build_id == build_id
                    && ready.worker_generation == worker_generation
                    && ready.compute_threads == compute_threads =>
            {
                ready
            }
            Ok(Ok(response)) => {
                return failed_spawn(
                    child,
                    stdin,
                    reader,
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("invalid worker handshake response: {response:?}"),
                    ),
                );
            }
            Ok(Err(error)) => {
                return failed_spawn(child, stdin, reader, error);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                return failed_spawn(
                    child,
                    stdin,
                    reader,
                    io::Error::new(io::ErrorKind::TimedOut, "tree worker handshake timed out"),
                );
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return failed_spawn(
                    child,
                    stdin,
                    reader,
                    io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "tree worker handshake channel closed",
                    ),
                );
            }
        };
        let max_inflight = compute_threads.saturating_mul(4).max(1);
        // The route table is the bounded admission control. Keep the transport
        // channel unbounded so a cancellation control frame never competes
        // with work for the final queue slot.
        let (outbound, outbound_rx) = mpsc::channel::<Request>();
        let writer_routes = Arc::clone(&routes);
        let writer_child = Arc::clone(&child);
        let writer_terminated_by_transport = Arc::clone(&terminated_by_transport);
        let writer = std::thread::spawn(move || {
            for request in outbound_rx {
                if let Err(error) = encode_frame(&mut stdin, &request) {
                    let kind = error.kind();
                    let message = error.to_string();
                    fail_routes(None, &writer_routes, kind, &message);
                    if let Ok(mut child) = writer_child.lock() {
                        terminate_by_transport(
                            &mut child,
                            &writer_terminated_by_transport,
                            Duration::from_secs(1),
                        );
                    }
                    break;
                }
            }
        });
        Ok(Self {
            child,
            terminated_by_transport,
            outbound: Mutex::new(Some(outbound)),
            routes,
            reader: Some(reader),
            writer: Some(writer),
            ready,
            max_inflight,
        })
    }

    pub fn try_wait(&self) -> io::Result<Option<std::process::ExitStatus>> {
        self.child
            .lock()
            .map_err(|_| io::Error::other("worker child lock is poisoned"))?
            .try_wait()
    }

    pub fn was_terminated_by_transport(&self) -> bool {
        self.terminated_by_transport.load(Ordering::Acquire)
    }

    pub fn compute_threads(&self) -> usize {
        self.ready.compute_threads
    }

    pub fn worker_generation(&self) -> u64 {
        self.ready.worker_generation
    }

    /// Idempotently cancel queued or cooperatively running work in this worker
    /// generation. The original request route remains until its terminal
    /// response arrives.
    pub fn cancel_request(&self, request_id: u64) -> io::Result<()> {
        let outbound = self
            .outbound
            .lock()
            .map_err(|_| io::Error::other("worker outbound lock is poisoned"))?;
        let outbound = outbound
            .as_ref()
            .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "worker is shut down"))?;
        outbound
            .send(Request::Cancel(CancelRequest {
                request_id,
                worker_generation: self.ready.worker_generation,
            }))
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "tree worker writer stopped"))
    }

    /// Stops this generation without requiring unique ownership of the client.
    ///
    /// Supervisors use this after atomically removing a generation from the
    /// read router. In-flight requests then observe transport failure while a
    /// replacement generation can be published independently.
    pub fn terminate_shared(&self, timeout: Duration) {
        self.outbound
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Ok(mut child) = self.child.lock() {
            terminate_by_transport(&mut child, &self.terminated_by_transport, timeout);
        }
    }

    pub fn derive(&self, request: DeriveSnapshot) -> io::Result<Response> {
        let timeout = request_timeout(&request.context.uri);
        self.derive_with_timeout(request, timeout)
    }

    /// Sends one request with a process-level recovery deadline.
    ///
    /// A timeout terminates the shared worker because native parser code may be
    /// non-cooperative. All concurrent requests consequently fail, and this
    /// client cannot be reused; the supervisor must create a new generation.
    pub fn derive_with_timeout(
        &self,
        request: DeriveSnapshot,
        timeout: Duration,
    ) -> io::Result<Response> {
        let context = request.context.clone();
        self.request_with_timeout(Request::DeriveSnapshot(request), context, timeout)
    }

    pub fn sync_document(&self, request: SyncDocument) -> io::Result<Response> {
        let context = request.context.clone();
        let timeout = request_timeout(&context.uri);
        self.request_with_timeout(Request::SyncDocument(request), context, timeout)
    }

    pub fn apply_document_edits(&self, request: ApplyDocumentEdits) -> io::Result<Response> {
        let context = request.context.clone();
        let timeout = request_timeout(&context.uri);
        self.request_with_timeout(Request::ApplyDocumentEdits(request), context, timeout)
    }

    pub fn apply_document_edits_and_derive(
        &self,
        request: ApplyDocumentEditsAndDerive,
    ) -> io::Result<Response> {
        let context = request.context.clone();
        let timeout = request_timeout(&context.uri);
        self.request_with_timeout(
            Request::ApplyDocumentEditsAndDerive(request),
            context,
            timeout,
        )
    }

    pub fn derive_document_snapshot(
        &self,
        request: DeriveDocumentSnapshot,
    ) -> io::Result<Response> {
        let context = request.context.clone();
        let timeout = request_timeout(&context.uri);
        self.request_with_timeout(Request::DeriveDocumentSnapshot(request), context, timeout)
    }

    pub fn resolve_node(&self, request: ResolveNode) -> io::Result<Response> {
        let context = request.context.clone();
        let timeout = request_timeout(&context.uri);
        self.request_with_timeout(Request::ResolveNode(request), context, timeout)
    }

    pub fn navigate_node(&self, request: NavigateNode) -> io::Result<Response> {
        let context = request.context.clone();
        let timeout = request_timeout(&context.uri);
        self.request_with_timeout(Request::NavigateNode(request), context, timeout)
    }

    pub fn run_node_scalar(&self, request: RunNodeScalar) -> io::Result<Response> {
        let context = request.context.clone();
        let timeout = request_timeout(&context.uri);
        self.request_with_timeout(Request::RunNodeScalar(request), context, timeout)
    }

    pub fn derive_selection_ranges(&self, request: DeriveSelectionRanges) -> io::Result<Response> {
        let context = request.context.clone();
        let timeout = request_timeout(&context.uri);
        self.request_with_timeout(Request::DeriveSelectionRanges(request), context, timeout)
    }

    pub fn derive_injection_regions(
        &self,
        request: DeriveInjectionRegions,
    ) -> io::Result<Response> {
        let context = request.context.clone();
        let timeout = request_timeout(&context.uri);
        self.request_with_timeout(Request::DeriveInjectionRegions(request), context, timeout)
    }

    pub fn derive_semantic_tokens(&self, request: DeriveSemanticTokens) -> io::Result<Response> {
        let context = request.context.clone();
        let timeout = request_timeout(&context.uri);
        self.request_with_timeout(Request::DeriveSemanticTokens(request), context, timeout)
    }

    pub fn run_captures(&self, request: RunCaptures) -> io::Result<Response> {
        let context = request.context.clone();
        let timeout = request_timeout(&context.uri);
        self.request_with_timeout(Request::RunCaptures(request), context, timeout)
    }

    pub fn derive_native_bindings(&self, request: DeriveNativeBindings) -> io::Result<Response> {
        let context = request.context.clone();
        let timeout = request_timeout(&context.uri);
        self.request_with_timeout(Request::DeriveNativeBindings(request), context, timeout)
    }

    pub fn configure_languages(&self, request: ConfigureLanguages) -> io::Result<Response> {
        let context = request.context.clone();
        let timeout = request_timeout(&context.uri);
        self.request_with_timeout(Request::ConfigureLanguages(request), context, timeout)
    }

    pub fn close_document(&self, request: CloseDocument) -> io::Result<Response> {
        let context = request.context.clone();
        let timeout = request_timeout(&context.uri);
        self.request_with_timeout(Request::CloseDocument(request), context, timeout)
    }

    fn request_with_timeout(
        &self,
        request: Request,
        expected: RequestContext,
        timeout: Duration,
    ) -> io::Result<Response> {
        let started = Instant::now();
        if expected.worker_generation != self.ready.worker_generation {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "request targets a stale worker generation",
            ));
        }
        let request_id = expected.request_id;
        let (response_tx, response_rx) = mpsc::channel();
        {
            let mut routes = self
                .routes
                .lock()
                .map_err(|_| io::Error::other("worker response router is poisoned"))?;
            if routes.contains_key(&request_id) {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("tree worker request {request_id} is already active"),
                ));
            }
            if routes.len() >= self.max_inflight {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "tree worker in-flight limit reached",
                ));
            }
            routes.insert(
                request_id,
                Route {
                    expected,
                    sender: response_tx,
                },
            );
        }
        {
            let outbound = self
                .outbound
                .lock()
                .map_err(|_| io::Error::other("worker outbound lock is poisoned"))?;
            let outbound = outbound
                .as_ref()
                .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "worker is shut down"))?;
            if outbound.send(request).is_err() {
                self.remove_route(request_id);
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "tree worker writer stopped",
                ));
            }
        }
        let remaining = timeout.saturating_sub(started.elapsed());
        let response = match response_rx.recv_timeout(remaining) {
            Ok(response) => response?,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                self.remove_route(request_id);
                if let Ok(mut child) = self.child.lock() {
                    terminate(&mut child, Duration::from_secs(1));
                }
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("tree worker request {request_id} timed out"),
                ));
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "tree worker response channel closed",
                ));
            }
        };
        Ok(response)
    }

    fn remove_route(&self, request_id: u64) {
        if let Ok(mut routes) = self.routes.lock() {
            routes.remove(&request_id);
        }
    }

    pub fn shutdown(mut self) -> io::Result<()> {
        self.outbound
            .get_mut()
            .map_err(|_| io::Error::other("worker outbound lock is poisoned"))?
            .take();
        let status = {
            let mut child = self
                .child
                .lock()
                .map_err(|_| io::Error::other("worker child lock is poisoned"))?;
            wait_until(&mut child, Duration::from_secs(2))?
        };
        if let Some(writer) = self.writer.take() {
            let _ = writer.join();
        }
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
        if status.success() {
            Ok(())
        } else {
            Err(io::Error::other(format!(
                "tree worker exited with {status}"
            )))
        }
    }
}

fn failed_spawn(
    child: Arc<Mutex<Child>>,
    stdin: ChildStdin,
    reader: std::thread::JoinHandle<()>,
    error: io::Error,
) -> io::Result<Client> {
    drop(stdin);
    if let Ok(mut child) = child.lock() {
        terminate(&mut child, Duration::from_secs(1));
    }
    let _ = reader.join();
    Err(error)
}

impl Drop for Client {
    fn drop(&mut self) {
        if let Ok(outbound) = self.outbound.get_mut() {
            outbound.take();
        }
        if let Ok(mut child) = self.child.lock() {
            terminate(&mut child, Duration::from_secs(1));
        }
        if let Some(writer) = self.writer.take() {
            let _ = writer.join();
        }
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

fn response_request_id(response: &Response) -> Option<u64> {
    match response {
        Response::DocumentAck(ack) => Some(ack.context.request_id),
        Response::DocumentClosed(closed) => Some(closed.context.request_id),
        Response::WorkerRestartRequired(required) => Some(required.context.request_id),
        Response::RequestCancelled(cancelled) => Some(cancelled.context.request_id),
        Response::Snapshot(snapshot) => Some(snapshot.context.request_id),
        Response::Nodes(result) => Some(result.context.request_id),
        Response::NodeScalar(result) => Some(result.context.request_id),
        Response::SelectionRanges(result) => Some(result.context.request_id),
        Response::InjectionRegions(result) => Some(result.context.request_id),
        Response::SemanticTokens(result) => Some(result.context.request_id),
        Response::Captures(result) => Some(result.context.request_id),
        Response::NativeBindings(result) => Some(result.context.request_id),
        Response::LanguageCatalogAck(ack) => Some(ack.context.request_id),
        Response::Error(error) => error.context.as_ref().map(|context| context.request_id),
        Response::HandshakeReady(_) => None,
    }
}

fn response_context(response: &Response) -> Option<&RequestContext> {
    match response {
        Response::DocumentAck(ack) => Some(&ack.context),
        Response::DocumentClosed(closed) => Some(&closed.context),
        Response::WorkerRestartRequired(required) => Some(&required.context),
        Response::RequestCancelled(cancelled) => Some(&cancelled.context),
        Response::Snapshot(snapshot) => Some(&snapshot.context),
        Response::Nodes(result) => Some(&result.context),
        Response::NodeScalar(result) => Some(&result.context),
        Response::SelectionRanges(result) => Some(&result.context),
        Response::InjectionRegions(result) => Some(&result.context),
        Response::SemanticTokens(result) => Some(&result.context),
        Response::Captures(result) => Some(&result.context),
        Response::NativeBindings(result) => Some(&result.context),
        Response::LanguageCatalogAck(ack) => Some(&ack.context),
        Response::Error(error) => error.context.as_ref(),
        Response::HandshakeReady(_) => None,
    }
}

fn route_response(routes: &Mutex<HashMap<u64, Route>>, response: Response) -> io::Result<()> {
    let Some(request_id) = response_request_id(&response) else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unexpected response without request id",
        ));
    };
    let route = routes
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&request_id)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("response has unknown request id {request_id}"),
            )
        })?;
    if response_context(&response) != Some(&route.expected) {
        let error = io::Error::new(
            io::ErrorKind::InvalidData,
            format!("response context mismatch for request {request_id}"),
        );
        let _ = route
            .sender
            .send(Err(io::Error::new(error.kind(), error.to_string())));
        return Err(error);
    }
    let _ = route.sender.send(Ok(response));
    Ok(())
}

fn fail_routes(
    handshake: Option<mpsc::Sender<io::Result<Response>>>,
    routes: &Mutex<HashMap<u64, Route>>,
    kind: io::ErrorKind,
    message: &str,
) {
    if let Some(handshake) = handshake {
        let _ = handshake.send(Err(io::Error::new(kind, message.to_string())));
    }
    let routes = std::mem::take(
        &mut *routes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    );
    for route in routes.into_values() {
        let _ = route
            .sender
            .send(Err(io::Error::new(kind, message.to_string())));
    }
}

fn wait_until(child: &mut Child, timeout: Duration) -> io::Result<std::process::ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            let kill_error = child.kill().err();
            let reap_deadline = Instant::now() + timeout;
            loop {
                if let Some(status) = child.try_wait()? {
                    return Ok(status);
                }
                if Instant::now() >= reap_deadline {
                    return Err(kill_error.unwrap_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::TimedOut,
                            "tree worker did not exit after kill",
                        )
                    }));
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn terminate(child: &mut Child, timeout: Duration) {
    if child.try_wait().ok().flatten().is_some() {
        return;
    }
    let _ = child.kill();
    let _ = wait_until(child, timeout);
}

fn terminate_by_transport(child: &mut Child, marker: &AtomicBool, timeout: Duration) {
    if child.try_wait().ok().flatten().is_some() {
        return;
    }
    marker.store(true, Ordering::Release);
    terminate(child, timeout);
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::io::{Cursor, Read, Write};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Condvar, Mutex};
    use std::time::{Duration, Instant};

    use super::{
        ApplyDocumentEdits, BUILD_ID, ByteEdit, CacheEviction, CacheEvictionCandidate,
        CachedInjectionLayer, CancelRequest, CapturesResult, CloseDocument, DeriveInjectionRegions,
        DeriveNativeBindings, DeriveSelectionRanges, DeriveSemanticTokens, DeriveSnapshot,
        DocumentCompetition, DocumentKey, DocumentLane, DocumentReplica, GrammarKey, Handshake,
        HandshakeReady, LaneAction, LaneRegistry, MAX_DOCUMENT_BYTES, MAX_DOCUMENT_REPLICAS,
        MAX_RETAINED_DOCUMENT_BYTES, MAX_RETAINED_ESTIMATED_DOCUMENT_BYTES, NavigateNode,
        NodeLayerSelector, NodeNavigation, NodeScalarOperation, NodeScalarValue, OpaqueNodeId,
        PROTOCOL_VERSION, Request, RequestCancelled, RequestContext, ResolveNode, Response, Route,
        RunCaptures, RunNodeScalar, RunnableDocuments,
        SEMANTIC_CACHE_AUXILIARY_TRIM_THRESHOLD_BYTES, SyncDocument, WireByteRange, WirePosition,
        WireRange, WireSemanticToken, WorkPriority, WorkerError, WorkerQuerySources,
        cache_eviction_plan, close_document, decode_frame, derive_snapshot_with_language,
        encode_frame, enforce_derived_memory_budget, named_node_count, parse_request_timeout,
        pressure_checkpoint_required, replace_retained_bytes, replace_retained_estimated_bytes,
        request_may_grow_derived_state, request_priority, reserve_retained_growth, route_response,
        run, submit_document_job, sync_document, terminate_by_transport, validate_document_size,
    };

    #[derive(Clone, Default)]
    struct SharedWriter(Arc<Mutex<Vec<u8>>>);

    impl Write for SharedWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().write(bytes)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[cfg(unix)]
    #[test]
    fn transport_termination_marker_excludes_already_exited_workers() {
        let marker = AtomicBool::new(false);
        let mut running = std::process::Command::new("sh")
            .args(["-c", "sleep 10"])
            .spawn()
            .unwrap();
        terminate_by_transport(&mut running, &marker, Duration::from_secs(1));
        assert!(marker.load(Ordering::Acquire));

        let marker = AtomicBool::new(false);
        let mut exited = std::process::Command::new("sh")
            .args(["-c", "exit 0"])
            .spawn()
            .unwrap();
        exited.wait().unwrap();
        terminate_by_transport(&mut exited, &marker, Duration::from_secs(1));
        assert!(!marker.load(Ordering::Acquire));
    }

    #[derive(Clone, Default)]
    struct GatedWriter(Arc<(Mutex<GateState>, Condvar)>);

    #[derive(Default)]
    struct GateState {
        bytes: Vec<u8>,
        flushes: usize,
        blocked: bool,
        released: bool,
    }

    impl Write for GatedWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            let (state, _) = &*self.0;
            state.lock().unwrap().bytes.write(bytes)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            let (state, changed) = &*self.0;
            let mut state = state.lock().unwrap();
            state.flushes += 1;
            if state.flushes >= 2 && !state.released {
                state.blocked = true;
                changed.notify_all();
                state = changed.wait_while(state, |state| !state.released).unwrap();
            }
            state.bytes.flush()
        }
    }

    impl GatedWriter {
        fn wait_until_blocked(&self) {
            let (state, changed) = &*self.0;
            let state = state.lock().unwrap();
            let (state, timeout) = changed
                .wait_timeout_while(state, Duration::from_secs(5), |state| !state.blocked)
                .unwrap();
            assert!(
                !timeout.timed_out() && state.blocked,
                "writer never blocked"
            );
        }

        fn release(&self) {
            let (state, changed) = &*self.0;
            state.lock().unwrap().released = true;
            changed.notify_all();
        }
    }

    struct CountingReader {
        cursor: Cursor<Vec<u8>>,
        progress: Arc<(AtomicUsize, Condvar, Mutex<()>)>,
    }

    impl Read for CountingReader {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            let read = self.cursor.read(buffer)?;
            self.progress.0.fetch_add(read, Ordering::Relaxed);
            self.progress.1.notify_all();
            Ok(read)
        }
    }

    fn wait_for_read_progress(progress: &Arc<(AtomicUsize, Condvar, Mutex<()>)>, minimum: usize) {
        let guard = progress.2.lock().unwrap();
        let (_guard, timeout) = progress
            .1
            .wait_timeout_while(guard, Duration::from_secs(5), |_| {
                progress.0.load(Ordering::Relaxed) < minimum
            })
            .unwrap();
        assert!(
            !timeout.timed_out(),
            "reader did not reach bounded lookahead"
        );
    }

    #[derive(Default)]
    struct FailAfterFirstFlush {
        bytes: Vec<u8>,
        flushes: usize,
    }

    impl Write for FailAfterFirstFlush {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.bytes.write(bytes)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            self.flushes += 1;
            if self.flushes > 1 {
                Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "injected writer failure",
                ))
            } else {
                Ok(())
            }
        }
    }

    fn framed(requests: &[Request]) -> Vec<u8> {
        let mut bytes = Vec::new();
        for request in requests {
            encode_frame(&mut bytes, request).unwrap();
        }
        bytes
    }

    fn handshake(version: u32) -> Request {
        Request::Handshake(Handshake {
            protocol_version: version,
            build_id: BUILD_ID.into(),
            worker_generation: 3,
        })
    }

    fn request() -> Request {
        Request::DeriveSnapshot(DeriveSnapshot {
            context: RequestContext {
                request_id: 7,
                worker_generation: 3,
                uri: "file:///example.rs".into(),
                incarnation: 11,
                content_version: 5,
                configuration_generation: 2,
            },
            language: "rust".into(),
            grammar_symbol: "rust".into(),
            parser_path: "/unused/in/unit-test".into(),
            artifact_digest: "sha256:rust-v1".into(),
            text: "fn main() {}".into(),
        })
    }

    #[test]
    fn frame_round_trip_preserves_request() {
        let request = request();
        let mut bytes = Vec::new();
        encode_frame(&mut bytes, &request).unwrap();

        assert_eq!(
            decode_frame(&mut Cursor::new(bytes)).unwrap(),
            Some(request)
        );
    }

    #[test]
    fn frame_round_trip_preserves_idempotent_cancellation() {
        let request = Request::Cancel(CancelRequest {
            request_id: 42,
            worker_generation: 7,
        });
        let mut bytes = Vec::new();
        encode_frame(&mut bytes, &request).unwrap();

        assert_eq!(
            decode_frame(&mut Cursor::new(bytes)).unwrap(),
            Some(request)
        );
    }

    #[test]
    fn request_timeout_override_is_positive_milliseconds() {
        assert_eq!(
            parse_request_timeout(Some("125")),
            Duration::from_millis(125)
        );
        assert_eq!(parse_request_timeout(Some("0")), Duration::from_secs(60));
        assert_eq!(
            parse_request_timeout(Some("invalid")),
            Duration::from_secs(60)
        );
        assert_eq!(parse_request_timeout(None), Duration::from_secs(60));
    }

    #[test]
    fn frame_round_trip_preserves_capture_json_without_self_describing_codec() {
        let Request::DeriveSnapshot(snapshot_request) = request() else {
            unreachable!()
        };
        let response = Response::Captures(CapturesResult {
            context: snapshot_request.context,
            available: true,
            matches: vec![serde_json::json!({
                "node": {"id": "opaque", "named": true},
                "captures": [1, null, "text"]
            })],
            skipped: vec![serde_json::json!({"reason": "missing query"})],
        });
        let mut bytes = Vec::new();
        encode_frame(&mut bytes, &response).unwrap();

        assert_eq!(
            decode_frame(&mut Cursor::new(bytes)).unwrap(),
            Some(response)
        );
    }

    #[test]
    fn oversized_frame_is_rejected_before_payload_read() {
        let bytes = (64_u32 * 1024 * 1024 + 1).to_be_bytes();
        let error = decode_frame::<_, Request>(&mut Cursor::new(bytes)).unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("frame exceeds"));
    }

    #[test]
    fn derive_snapshot_returns_only_guarded_serializable_data() {
        let Request::DeriveSnapshot(request) = request() else {
            unreachable!()
        };

        let response = derive_snapshot_with_language(request, tree_sitter_rust::LANGUAGE.into());

        let Response::Snapshot(snapshot) = response else {
            panic!("expected snapshot response");
        };
        assert_eq!(snapshot.context.request_id, 7);
        assert_eq!(snapshot.context.content_version, 5);
        assert_eq!(snapshot.language, "rust");
        assert_eq!(snapshot.root_kind, "source_file");
        assert!(!snapshot.has_error);
        assert!(snapshot.named_node_count >= 2);
    }

    #[test]
    fn cursor_named_node_count_visits_each_node_once() {
        fn recursive_count(node: tree_sitter::Node<'_>) -> usize {
            usize::from(node.is_named())
                + (0..node.child_count() as u32)
                    .filter_map(|index| node.child(index))
                    .map(recursive_count)
                    .sum::<usize>()
        }

        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        let tree = parser
            .parse(
                "fn main() { let answer = if true { 42 } else { 0 }; }",
                None,
            )
            .unwrap();
        let root = tree.root_node();

        assert_eq!(named_node_count(root), recursive_count(root));
    }

    #[test]
    fn worker_requires_matching_handshake_before_work() {
        let output = SharedWriter::default();
        let captured = output.clone();

        let error = run(
            Cursor::new(framed(&[handshake(PROTOCOL_VERSION + 1)])),
            output,
            2,
        )
        .unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        let bytes = captured.0.lock().unwrap().clone();
        let response = decode_frame::<_, Response>(&mut Cursor::new(bytes))
            .unwrap()
            .unwrap();
        assert!(matches!(response, Response::Error(_)));
    }

    #[test]
    fn worker_rejects_work_before_handshake() {
        let output = SharedWriter::default();

        let error = run(Cursor::new(framed(&[request()])), output, 2).unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("handshake"));
    }

    #[test]
    fn response_router_rejects_unknown_request_id() {
        let routes = Mutex::new(HashMap::new());
        let response = Response::Error(WorkerError {
            context: Some(RequestContext {
                request_id: 99,
                worker_generation: 3,
                uri: "file:///unknown.rs".into(),
                incarnation: 1,
                content_version: 1,
                configuration_generation: 1,
            }),
            message: "unexpected".into(),
        });

        let error = route_response(&routes, response).unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("unknown request id 99"));
    }

    #[test]
    fn response_router_rejects_stale_context() {
        let expected = RequestContext {
            request_id: 7,
            worker_generation: 3,
            uri: "file:///example.rs".into(),
            incarnation: 11,
            content_version: 5,
            configuration_generation: 2,
        };
        let (sender, _receiver) = std::sync::mpsc::channel();
        let routes = Mutex::new(HashMap::from([(
            7,
            Route {
                expected: expected.clone(),
                sender,
            },
        )]));
        let mut stale = expected;
        stale.content_version += 1;

        let error = route_response(
            &routes,
            Response::Error(WorkerError {
                context: Some(stale),
                message: "stale".into(),
            }),
        )
        .unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("context mismatch"));
    }

    #[test]
    fn worker_reports_budget_and_routes_guarded_request_errors() {
        let output = SharedWriter::default();
        let captured = output.clone();

        run(
            Cursor::new(framed(&[handshake(PROTOCOL_VERSION), request()])),
            output,
            2,
        )
        .unwrap();

        let bytes = captured.0.lock().unwrap().clone();
        let mut bytes = Cursor::new(bytes);
        assert_eq!(
            decode_frame::<_, Response>(&mut bytes).unwrap(),
            Some(Response::HandshakeReady(HandshakeReady {
                protocol_version: PROTOCOL_VERSION,
                build_id: BUILD_ID.into(),
                worker_generation: 3,
                compute_threads: 2,
            }))
        );
        let response = decode_frame::<_, Response>(&mut bytes).unwrap().unwrap();
        let Response::Error(error) = response else {
            panic!("missing parser path must return a routed request error");
        };
        assert_eq!(
            error.context.as_ref().map(|context| context.request_id),
            Some(7)
        );
    }

    #[test]
    fn worker_honors_cancellation_that_arrives_before_reader_work() {
        let output = SharedWriter::default();
        let captured = output.clone();
        let cancel = Request::Cancel(CancelRequest {
            request_id: 7,
            worker_generation: 3,
        });

        run(
            Cursor::new(framed(&[handshake(PROTOCOL_VERSION), cancel, request()])),
            output,
            1,
        )
        .unwrap();

        let bytes = captured.0.lock().unwrap().clone();
        let mut bytes = Cursor::new(bytes);
        assert!(matches!(
            decode_frame::<_, Response>(&mut bytes).unwrap(),
            Some(Response::HandshakeReady(_))
        ));
        let response = decode_frame::<_, Response>(&mut bytes).unwrap().unwrap();
        assert!(matches!(
            response,
            Response::RequestCancelled(RequestCancelled { context })
                if context.request_id == 7
        ));
    }

    #[test]
    fn worker_backpressures_until_the_response_writer_resumes() {
        let writer = GatedWriter::default();
        let gate = writer.clone();
        let mut requests = vec![handshake(PROTOCOL_VERSION)];
        for request_id in 1..=12 {
            let Request::DeriveSnapshot(mut request) = request() else {
                unreachable!()
            };
            request.context.request_id = request_id;
            requests.push(Request::DeriveSnapshot(request));
        }
        let admitted_prefix_bytes = framed(&requests[..8]).len();
        let framed_requests = framed(&requests);
        let progress = Arc::new((AtomicUsize::new(0), Condvar::new(), Mutex::new(())));
        let reader = CountingReader {
            cursor: Cursor::new(framed_requests.clone()),
            progress: Arc::clone(&progress),
        };
        let (completed, completed_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = completed.send(run(reader, writer, 1));
        });

        gate.wait_until_blocked();
        wait_for_read_progress(&progress, admitted_prefix_bytes);
        assert!(
            progress.0.load(Ordering::Relaxed) <= admitted_prefix_bytes,
            "worker read past its pending, response, and lookahead bounds"
        );
        assert!(
            progress.0.load(Ordering::Relaxed) < framed_requests.len(),
            "worker consumed the complete request stream while stdout was stalled"
        );
        assert!(
            completed_rx
                .recv_timeout(Duration::from_millis(50))
                .is_err(),
            "worker unexpectedly completed while stdout was stalled"
        );

        gate.release();
        completed_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("worker did not resume after stdout was released")
            .unwrap();
    }

    #[test]
    fn writer_failure_releases_all_admission_permits() {
        let mut requests = vec![handshake(PROTOCOL_VERSION)];
        for request_id in 1..=8 {
            let Request::DeriveSnapshot(mut request) = request() else {
                unreachable!()
            };
            request.context.request_id = request_id;
            requests.push(Request::DeriveSnapshot(request));
        }
        let (completed, completed_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = completed.send(run(
                Cursor::new(framed(&requests)),
                FailAfterFirstFlush::default(),
                1,
            ));
        });

        let error = completed_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("worker deadlocked after response writer failure")
            .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe);
    }

    #[test]
    fn document_lane_runs_jobs_in_submission_order() {
        let pool = Arc::new(
            rayon::ThreadPoolBuilder::new()
                .num_threads(4)
                .build()
                .unwrap(),
        );
        let lane = Arc::new(DocumentLane::detached());
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let order = Arc::new(Mutex::new(Vec::new()));
        let (completed, completed_rx) = std::sync::mpsc::channel();

        let first_gate = Arc::clone(&gate);
        let first_order = Arc::clone(&order);
        lane.submit(
            &pool,
            Box::new(move || {
                let (released, changed) = &*first_gate;
                let released = released.lock().unwrap();
                drop(changed.wait_while(released, |released| !*released).unwrap());
                first_order.lock().unwrap().push(1);
                LaneAction::Retire
            }),
        );
        let second_order = Arc::clone(&order);
        lane.submit(
            &pool,
            Box::new(move || {
                second_order.lock().unwrap().push(2);
                completed.send(()).unwrap();
                LaneAction::Retire
            }),
        );

        assert!(completed_rx.try_recv().is_err());
        *gate.0.lock().unwrap() = true;
        gate.1.notify_all();
        completed_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(*order.lock().unwrap(), [1, 2]);
    }

    #[test]
    fn runnable_documents_count_distinct_uris_not_backlog_depth() {
        let runnable = Arc::new(RunnableDocuments::default());
        let a = DocumentKey {
            uri: "file:///a.rs".into(),
        };
        let b = DocumentKey {
            uri: "file:///b.rs".into(),
        };

        let first_a = runnable.enter(a.clone(), WorkPriority::Background);
        let second_a = runnable.enter(a.clone(), WorkPriority::Foreground);
        assert_eq!(runnable.competition_for(&a), DocumentCompetition::None);

        let first_b = runnable.enter(b, WorkPriority::Background);
        assert_eq!(
            runnable.competition_for(&DocumentKey {
                uri: "file:///a.rs".into()
            }),
            DocumentCompetition::Background
        );
        drop(first_b);
        assert_eq!(
            runnable.competition_for(&DocumentKey {
                uri: "file:///a.rs".into()
            }),
            DocumentCompetition::None
        );

        drop(second_a);
        drop(first_a);
        assert_eq!(
            runnable.competition_for(&DocumentKey {
                uri: "file:///a.rs".into()
            }),
            DocumentCompetition::None
        );
    }

    #[test]
    fn runnable_documents_track_foreground_competitors_separately() {
        let runnable = Arc::new(RunnableDocuments::default());
        let current = DocumentKey {
            uri: "file:///a.rs".into(),
        };
        let first = runnable.enter(current.clone(), WorkPriority::Background);
        assert_eq!(
            runnable.competition_for(&current),
            DocumentCompetition::None
        );

        let background = runnable.enter(
            DocumentKey {
                uri: "file:///b.rs".into(),
            },
            WorkPriority::Background,
        );
        assert_eq!(
            runnable.competition_for(&current),
            DocumentCompetition::Background
        );

        let foreground = runnable.enter(
            DocumentKey {
                uri: "file:///c.rs".into(),
            },
            WorkPriority::Foreground,
        );
        assert_eq!(
            runnable.competition_for(&current),
            DocumentCompetition::Foreground
        );
        drop(foreground);
        assert_eq!(
            runnable.competition_for(&current),
            DocumentCompetition::Background
        );
        drop(background);
        drop(first);
    }

    #[test]
    fn request_priority_separates_bulk_derivation_from_interactive_reads() {
        let context = RequestContext {
            request_id: 1,
            worker_generation: 1,
            uri: "file:///priority.rs".into(),
            incarnation: 1,
            content_version: 1,
            configuration_generation: 1,
        };
        assert_eq!(
            request_priority(&Request::DeriveSemanticTokens(DeriveSemanticTokens {
                context: context.clone(),
                supports_multiline: false,
            })),
            WorkPriority::Background
        );
        assert_eq!(
            request_priority(&Request::ResolveNode(ResolveNode {
                context,
                byte_offset: 0,
                named: true,
                layer: NodeLayerSelector::Host,
            })),
            WorkPriority::Foreground
        );
    }

    #[test]
    fn idle_document_lane_removes_its_registry_key() {
        let pool = Arc::new(rayon::ThreadPoolBuilder::new().build().unwrap());
        let registry = Arc::new(LaneRegistry::new());
        let key = DocumentKey {
            uri: "file:///closed.rs".into(),
        };
        let (completed, completed_rx) = std::sync::mpsc::channel();
        submit_document_job(
            &registry,
            key,
            &pool,
            Box::new(move || {
                completed.send(()).unwrap();
                LaneAction::Retire
            }),
        );
        completed_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !registry.is_empty() && std::time::Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert!(registry.is_empty(), "idle lane retained its URI key");
    }

    #[test]
    fn live_document_lane_stays_registered_between_requests() {
        let pool = Arc::new(rayon::ThreadPoolBuilder::new().build().unwrap());
        let registry = Arc::new(LaneRegistry::new());
        let key = DocumentKey {
            uri: "file:///live.rs".into(),
        };
        let (completed, completed_rx) = std::sync::mpsc::channel();
        submit_document_job(
            &registry,
            key,
            &pool,
            Box::new(move || {
                completed.send(()).unwrap();
                LaneAction::Keep
            }),
        );
        completed_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while registry
            .iter()
            .any(|entry| entry.value().state.lock().unwrap().running)
            && std::time::Instant::now() < deadline
        {
            std::thread::yield_now();
        }
        assert_eq!(registry.len(), 1, "live lane was recreated after idle");
    }

    #[test]
    fn document_replica_applies_an_incremental_edit_at_the_expected_base_version() {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        let context = RequestContext {
            request_id: 1,
            worker_generation: 3,
            uri: "file:///example.rs".into(),
            incarnation: 4,
            content_version: 1,
            configuration_generation: 2,
        };
        let (mut replica, sync_ack) = DocumentReplica::sync(
            SyncDocument {
                context: context.clone(),
                language: "rust".into(),
                grammar_symbol: "rust".into(),
                source_path: "/unused/static-language".into(),
                parser_path: "/unused/static-language".into(),
                artifact_digest: "sha256:rust-v1".into(),
                queries: WorkerQuerySources::default(),
                text: "fn main() { 1 }".into(),
            },
            &mut parser,
        )
        .unwrap();
        assert!(!sync_ack.incremental);

        let mut edited_context = context;
        edited_context.request_id = 2;
        edited_context.content_version = 2;
        let ack = replica
            .apply(
                ApplyDocumentEdits {
                    context: edited_context.clone(),
                    base_version: 1,
                    edits: vec![ByteEdit {
                        start_byte: 12,
                        old_end_byte: 13,
                        new_text: "value + 2".into(),
                    }],
                },
                &mut parser,
                None,
            )
            .unwrap();

        assert!(ack.incremental);
        assert_eq!(ack.context.content_version, 2);
        let snapshot = replica.derive(edited_context).unwrap();
        assert_eq!(snapshot.root_kind, "source_file");
        assert_eq!(snapshot.root_end_byte, "fn main() { value + 2 }".len());
        assert_eq!(snapshot.parser_cache_hit, None);
        assert!(snapshot.compute_ns > 0);
    }

    #[test]
    fn node_operations_return_generation_fenced_owned_data() {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        let context = RequestContext {
            request_id: 1,
            worker_generation: 3,
            uri: "file:///example.rs".into(),
            incarnation: 4,
            content_version: 1,
            configuration_generation: 2,
        };
        let (mut replica, _) = DocumentReplica::sync(
            SyncDocument {
                context: context.clone(),
                language: "rust".into(),
                grammar_symbol: "rust".into(),
                source_path: "/unused/static-language".into(),
                parser_path: "/unused/static-language".into(),
                artifact_digest: "sha256:rust-v1".into(),
                queries: WorkerQuerySources {
                    injections: Some("(identifier) @injection.content".into()),
                    ..WorkerQuerySources::default()
                },
                text: "fn main() {}".into(),
            },
            &mut parser,
        )
        .unwrap();
        assert!(replica.analysis.injection_query("rust").is_some());

        let resolved = replica
            .resolve_node(ResolveNode {
                context: context.clone(),
                byte_offset: 3,
                named: true,
                layer: NodeLayerSelector::Host,
            })
            .unwrap();
        assert_eq!(resolved.nodes.len(), 1);
        assert_eq!(resolved.nodes[0].kind, "identifier");
        assert_eq!(resolved.nodes[0].id.worker_generation, 3);
        let node_id = resolved.nodes[0].id.clone();

        let scalar = replica
            .run_node_scalar(RunNodeScalar {
                context: context.clone(),
                node_id: node_id.clone(),
                operation: NodeScalarOperation::Text,
            })
            .unwrap();
        assert_eq!(scalar.value, Some(NodeScalarValue::String("main".into())));

        let range = replica
            .run_node_scalar(RunNodeScalar {
                context: context.clone(),
                node_id: node_id.clone(),
                operation: NodeScalarOperation::Range,
            })
            .unwrap();
        assert_eq!(
            range.value,
            Some(NodeScalarValue::Range {
                start: WirePosition {
                    line: 0,
                    character: 3,
                },
                end: WirePosition {
                    line: 0,
                    character: 7,
                },
            })
        );

        let parent = replica
            .navigate_node(NavigateNode {
                context: context.clone(),
                node_id: node_id.clone(),
                operation: NodeNavigation::Parent,
            })
            .unwrap();
        assert_eq!(parent.nodes.len(), 1);
        assert_eq!(parent.nodes[0].kind, "function_item");

        let named = replica
            .navigate_node(NavigateNode {
                context: context.clone(),
                node_id: parent.nodes[0].id.clone(),
                operation: NodeNavigation::ChildByFieldName {
                    name: "name".into(),
                },
            })
            .unwrap();
        assert_eq!(named.nodes.len(), 1);
        assert_eq!(named.nodes[0].kind, "identifier");
        assert_eq!(named.nodes[0].start_byte, 3);

        let descendant = replica
            .navigate_node(NavigateNode {
                context: context.clone(),
                node_id: parent.nodes[0].id.clone(),
                operation: NodeNavigation::DescendantForByteRange {
                    start_byte: 3,
                    end_byte: 7,
                    named: true,
                },
            })
            .unwrap();
        assert_eq!(descendant.nodes.len(), 1);
        assert_eq!(descendant.nodes[0].kind, "identifier");

        let point_descendant = replica
            .navigate_node(NavigateNode {
                context: context.clone(),
                node_id: parent.nodes[0].id.clone(),
                operation: NodeNavigation::DescendantForPointRange {
                    start: WirePosition {
                        line: 0,
                        character: 3,
                    },
                    end: WirePosition {
                        line: 0,
                        character: 7,
                    },
                    named: true,
                },
            })
            .unwrap();
        assert_eq!(point_descendant.nodes.len(), 1);
        assert_eq!(point_descendant.nodes[0].kind, "identifier");

        let child_with_descendant = replica
            .navigate_node(NavigateNode {
                context: context.clone(),
                node_id: parent.nodes[0].id.clone(),
                operation: NodeNavigation::ChildWithDescendant {
                    descendant_id: node_id.clone(),
                },
            })
            .unwrap();
        assert_eq!(child_with_descendant.nodes.len(), 1);
        assert_eq!(child_with_descendant.nodes[0].kind, "identifier");

        let selections = replica
            .derive_selection_ranges(DeriveSelectionRanges {
                context: context.clone(),
                positions: vec![
                    WirePosition {
                        line: 0,
                        character: 3,
                    },
                    WirePosition {
                        line: 99,
                        character: 0,
                    },
                ],
            })
            .unwrap();
        assert_eq!(selections.ranges.len(), 2);
        assert_eq!(
            selections.ranges[0].range.start,
            WirePosition {
                line: 0,
                character: 3,
            }
        );
        assert_eq!(
            selections.ranges[0].range.end,
            WirePosition {
                line: 0,
                character: 7,
            }
        );
        assert!(selections.ranges[0].parent.is_some());
        assert_eq!(
            selections.ranges[1].range.start,
            WirePosition {
                line: 99,
                character: 0,
            }
        );
        assert!(selections.ranges[1].parent.is_none());

        let mut edited = context.clone();
        edited.request_id = 2;
        edited.content_version = 2;
        replica
            .apply(
                ApplyDocumentEdits {
                    context: edited.clone(),
                    base_version: 1,
                    edits: vec![ByteEdit {
                        start_byte: 0,
                        old_end_byte: 0,
                        new_text: "pub ".into(),
                    }],
                },
                &mut parser,
                None,
            )
            .unwrap();
        let shifted = replica
            .navigate_node(NavigateNode {
                context: edited.clone(),
                node_id: node_id.clone(),
                operation: NodeNavigation::Parent,
            })
            .unwrap();
        assert_eq!(shifted.nodes.len(), 1);
        assert_eq!(shifted.nodes[0].kind, "function_item");

        let stale = replica
            .navigate_node(NavigateNode {
                context: edited,
                node_id: OpaqueNodeId {
                    worker_generation: 2,
                    local_id: node_id.local_id,
                },
                operation: NodeNavigation::Parent,
            })
            .unwrap();
        assert!(stale.nodes.is_empty());
    }

    #[test]
    fn node_operations_stay_inside_worker_owned_injection_layer() {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_md::LANGUAGE.into())
            .unwrap();
        let context = RequestContext {
            request_id: 1,
            worker_generation: 3,
            uri: "file:///node.md".into(),
            incarnation: 4,
            content_version: 1,
            configuration_generation: 2,
        };
        let analysis = Arc::new(crate::language::LanguageCoordinator::new());
        super::worker_analysis(
            &analysis,
            "lua",
            tree_sitter_lua::LANGUAGE.into(),
            WorkerQuerySources::default(),
            super::WorkerQuerySet::Injections,
        )
        .unwrap();
        let text = "# t\n\n```lua\nlocal v = 1\nprint(v)\n```\n";
        let (mut replica, _) = DocumentReplica::sync_with_analysis(
            SyncDocument {
                context: context.clone(),
                language: "markdown".into(),
                grammar_symbol: "markdown".into(),
                source_path: "/unused/static-language".into(),
                parser_path: "/unused/static-language".into(),
                artifact_digest: "sha256:markdown-v1".into(),
                queries: WorkerQuerySources {
                    injections: Some(
                        r#"
                        (fenced_code_block
                          (info_string (language) @injection.language)
                          (code_fence_content) @injection.content)
                        "#
                        .into(),
                    ),
                    ..WorkerQuerySources::default()
                },
                text: text.into(),
            },
            &mut parser,
            analysis,
        )
        .unwrap();

        let byte_offset = text.find("v)").unwrap();
        let resolved = replica
            .resolve_node(ResolveNode {
                context: context.clone(),
                byte_offset,
                named: true,
                layer: NodeLayerSelector::Deepest,
            })
            .unwrap();
        assert_eq!(resolved.nodes.len(), 1);
        assert_eq!(resolved.nodes[0].kind, "identifier");
        assert_eq!(resolved.nodes[0].start_byte, byte_offset);
        assert_eq!(resolved.nodes[0].layer, 1);

        let scalar = replica
            .run_node_scalar(RunNodeScalar {
                context: context.clone(),
                node_id: resolved.nodes[0].id.clone(),
                operation: NodeScalarOperation::Text,
            })
            .unwrap();
        assert_eq!(scalar.value, Some(NodeScalarValue::String("v".into())));

        let parent = replica
            .navigate_node(NavigateNode {
                context: context.clone(),
                node_id: resolved.nodes[0].id.clone(),
                operation: NodeNavigation::Parent,
            })
            .unwrap();
        assert_eq!(parent.nodes.len(), 1);
        assert_eq!(parent.nodes[0].kind, "arguments");

        for (selector, expected_kind) in [
            (NodeLayerSelector::Index(0), "code_fence_content"),
            (NodeLayerSelector::Index(1), "identifier"),
            (NodeLayerSelector::Index(-1), "identifier"),
            (NodeLayerSelector::Index(-2), "code_fence_content"),
        ] {
            let result = replica
                .resolve_node(ResolveNode {
                    context: context.clone(),
                    byte_offset,
                    named: false,
                    layer: selector,
                })
                .unwrap();
            assert_eq!(result.nodes[0].kind, expected_kind);
        }
        let out_of_bounds = replica
            .resolve_node(ResolveNode {
                context,
                byte_offset,
                named: false,
                layer: NodeLayerSelector::Index(2),
            })
            .unwrap();
        assert!(out_of_bounds.nodes.is_empty());
    }

    #[test]
    fn worker_query_compilation_preserves_tolerant_parent_semantics() {
        let analysis = crate::language::LanguageCoordinator::new();

        super::worker_analysis(
            &analysis,
            "rust",
            tree_sitter_rust::LANGUAGE.into(),
            WorkerQuerySources {
                highlights: Some("(identifier) @variable\n(nonexistent_node) @invalid\n".into()),
                ..WorkerQuerySources::default()
            },
            super::WorkerQuerySet::Semantic,
        )
        .unwrap();

        assert!(analysis.highlight_query("rust").is_some());
        let first = analysis.highlight_query("rust").unwrap();
        super::worker_analysis(
            &analysis,
            "rust",
            tree_sitter_rust::LANGUAGE.into(),
            WorkerQuerySources {
                highlights: Some("(identifier) @variable\n(nonexistent_node) @invalid\n".into()),
                ..WorkerQuerySources::default()
            },
            super::WorkerQuerySet::Semantic,
        )
        .unwrap();
        let second = analysis.highlight_query("rust").unwrap();
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(
            analysis
                .query_store()
                .query_input(crate::config::settings::QueryKind::Highlights, "rust")
                .as_deref(),
            Some("(identifier) @variable\n(nonexistent_node) @invalid\n")
        );
        let compiled = analysis
            .query_store()
            .query_source(crate::config::settings::QueryKind::Highlights, "rust")
            .unwrap();
        assert!(compiled.contains("(identifier) @variable"));
        assert!(!compiled.contains("nonexistent_node"));
    }

    #[test]
    fn selection_ranges_descend_into_worker_owned_injections() {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        let context = RequestContext {
            request_id: 1,
            worker_generation: 3,
            uri: "file:///injected.rs".into(),
            incarnation: 4,
            content_version: 1,
            configuration_generation: 2,
        };
        let (mut replica, _) = DocumentReplica::sync(
            SyncDocument {
                context: context.clone(),
                language: "rust".into(),
                grammar_symbol: "rust".into(),
                source_path: "/unused/static-language".into(),
                parser_path: "/unused/static-language".into(),
                artifact_digest: "sha256:rust-v1".into(),
                queries: WorkerQuerySources {
                    highlights: Some("(identifier) @variable".into()),
                    injections: Some(
                        r#"((line_comment) @injection.content
                            (#set! injection.language "rust")
                            (#offset! @injection.content 0 3 0 0))"#
                            .into(),
                    ),
                    ..WorkerQuerySources::default()
                },
                text: "// fn inner() {}\n".into(),
            },
            &mut parser,
        )
        .unwrap();
        assert!(replica.analysis.highlight_query("rust").is_none());
        assert!(replica.analysis.injection_query("rust").is_some());

        let result = replica
            .derive_selection_ranges(DeriveSelectionRanges {
                context,
                positions: vec![WirePosition {
                    line: 0,
                    character: 6,
                }],
            })
            .unwrap();

        assert_eq!(result.ranges.len(), 1);
        assert_eq!(
            result.ranges[0].range,
            WireRange {
                start: WirePosition {
                    line: 0,
                    character: 6,
                },
                end: WirePosition {
                    line: 0,
                    character: 11,
                },
            }
        );
        assert!(result.ranges[0].parent.is_some());

        let regions = replica
            .derive_injection_regions(DeriveInjectionRegions {
                context: RequestContext {
                    request_id: 2,
                    worker_generation: 3,
                    uri: "file:///injected.rs".into(),
                    incarnation: 4,
                    content_version: 1,
                    configuration_generation: 2,
                },
            })
            .unwrap();
        assert_eq!(regions.regions.len(), 1);
        assert_eq!(regions.regions[0].language, "rust");
        assert_eq!(regions.regions[0].virtual_content, "fn inner() {}");

        let tokens = replica
            .derive_semantic_tokens(DeriveSemanticTokens {
                context: RequestContext {
                    request_id: 3,
                    worker_generation: 3,
                    uri: "file:///injected.rs".into(),
                    incarnation: 4,
                    content_version: 1,
                    configuration_generation: 2,
                },
                supports_multiline: false,
            })
            .unwrap();
        assert!(
            tokens
                .tokens
                .iter()
                .any(|token| token.delta_start == 6 && token.length == 5)
        );
    }

    #[test]
    fn semantic_tokens_are_derived_from_worker_owned_queries() {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        let context = RequestContext {
            request_id: 1,
            worker_generation: 3,
            uri: "file:///semantic.rs".into(),
            incarnation: 4,
            content_version: 1,
            configuration_generation: 2,
        };
        let (mut replica, _) = DocumentReplica::sync(
            SyncDocument {
                context: context.clone(),
                language: "rust".into(),
                grammar_symbol: "rust".into(),
                source_path: "/unused/static-language".into(),
                parser_path: "/unused/static-language".into(),
                artifact_digest: "sha256:rust-v1".into(),
                queries: WorkerQuerySources {
                    highlights: Some("(identifier) @variable".into()),
                    ..WorkerQuerySources::default()
                },
                text: "fn main() {}".into(),
            },
            &mut parser,
        )
        .unwrap();

        let result = replica
            .derive_semantic_tokens(DeriveSemanticTokens {
                context,
                supports_multiline: false,
            })
            .unwrap();

        assert_eq!(result.tokens.len(), 1);
        assert_eq!(result.tokens[0].delta_start, 3);
        assert_eq!(result.tokens[0].length, 4);
        assert!(result.compute_ns > 0);
        assert_eq!(result.queue_wait_ns, 0);
        assert!(!result.cache_hit);

        let cached = replica
            .derive_semantic_tokens(DeriveSemanticTokens {
                context: RequestContext {
                    request_id: 2,
                    worker_generation: 3,
                    uri: "file:///semantic.rs".into(),
                    incarnation: 4,
                    content_version: 1,
                    configuration_generation: 2,
                },
                supports_multiline: false,
            })
            .unwrap();
        assert!(
            cached.cache_hit,
            "same-version semantic requests should reuse worker facts"
        );
    }

    #[test]
    fn oversized_semantic_cache_trims_only_recomputable_auxiliary_state() {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        let context = RequestContext {
            request_id: 1,
            worker_generation: 3,
            uri: "file:///memory-pressure.rs".into(),
            incarnation: 4,
            content_version: 1,
            configuration_generation: 2,
        };
        let (mut replica, _) = DocumentReplica::sync(
            SyncDocument {
                context,
                language: "rust".into(),
                grammar_symbol: "rust".into(),
                source_path: "/unused/static-language".into(),
                parser_path: "/unused/static-language".into(),
                artifact_digest: "sha256:rust-v1".into(),
                queries: WorkerQuerySources::default(),
                text: "fn main() {}".into(),
            },
            &mut parser,
        )
        .unwrap();
        let token = WireSemanticToken {
            delta_line: 0,
            delta_start: 0,
            length: 1,
            token_type: 0,
            token_modifiers_bitset: 0,
        };
        let token_count = SEMANTIC_CACHE_AUXILIARY_TRIM_THRESHOLD_BYTES
            / std::mem::size_of::<WireSemanticToken>()
            + 1;
        replica.cached_semantic_tokens = Some((2, false, Arc::new(vec![token; token_count])));
        replica.semantic_discovery = Some(Arc::new(crate::document::DiscoveredInjections {
            generation: 2,
            complete: true,
            regions: Vec::new(),
        }));

        assert!(replica.trim_auxiliary_caches_for_pressure());
        assert!(replica.cached_semantic_tokens.is_some());
        assert!(replica.semantic_discovery.is_none());
        assert_eq!(replica.context.content_version, 1);
        assert_eq!(replica.tree.root_node().kind(), "source_file");
    }

    #[test]
    fn small_semantic_cache_keeps_auxiliary_state() {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        let context = RequestContext {
            request_id: 1,
            worker_generation: 3,
            uri: "file:///small-cache.rs".into(),
            incarnation: 4,
            content_version: 1,
            configuration_generation: 2,
        };
        let (mut replica, _) = DocumentReplica::sync(
            SyncDocument {
                context,
                language: "rust".into(),
                grammar_symbol: "rust".into(),
                source_path: "/unused/static-language".into(),
                parser_path: "/unused/static-language".into(),
                artifact_digest: "sha256:rust-v1".into(),
                queries: WorkerQuerySources::default(),
                text: "fn main() {}".into(),
            },
            &mut parser,
        )
        .unwrap();
        replica.cached_semantic_tokens = Some((2, false, Arc::new(Vec::new())));
        replica.semantic_discovery = Some(Arc::new(crate::document::DiscoveredInjections {
            generation: 2,
            complete: true,
            regions: Vec::new(),
        }));

        assert!(!replica.trim_auxiliary_caches_for_pressure());
        assert!(replica.semantic_discovery.is_some());
    }

    #[test]
    fn semantic_cache_capacity_is_reported_as_evictable_memory() {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        let context = RequestContext {
            request_id: 1,
            worker_generation: 3,
            uri: "file:///memory-accounting.rs".into(),
            incarnation: 4,
            content_version: 1,
            configuration_generation: 2,
        };
        let (mut replica, _) = DocumentReplica::sync(
            SyncDocument {
                context,
                language: "rust".into(),
                grammar_symbol: "rust".into(),
                source_path: "/unused/static-language".into(),
                parser_path: "/unused/static-language".into(),
                artifact_digest: "sha256:rust-v1".into(),
                queries: WorkerQuerySources::default(),
                text: "fn main() {}".into(),
            },
            &mut parser,
        )
        .unwrap();
        let before = replica.estimated_memory();
        let token = WireSemanticToken {
            delta_line: 0,
            delta_start: 0,
            length: 1,
            token_type: 0,
            token_modifiers_bitset: 0,
        };
        let tokens = vec![token; 16];
        let semantic_bytes = tokens.capacity() * std::mem::size_of::<WireSemanticToken>();
        replica.cached_semantic_tokens = Some((2, false, Arc::new(tokens)));

        let after = replica.estimated_memory();
        assert_eq!(after.non_evictable_bytes, before.non_evictable_bytes);
        assert_eq!(
            after.evictable_bytes(),
            before.evictable_bytes() + semantic_bytes
        );
    }

    #[test]
    fn parsed_tree_contributes_to_non_evictable_memory() {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        let (replica, _) = DocumentReplica::sync(
            SyncDocument {
                context: RequestContext {
                    request_id: 1,
                    worker_generation: 3,
                    uri: "file:///tree-accounting.rs".into(),
                    incarnation: 4,
                    content_version: 1,
                    configuration_generation: 2,
                },
                language: "rust".into(),
                grammar_symbol: "rust".into(),
                source_path: "/unused/static-language".into(),
                parser_path: "/unused/static-language".into(),
                artifact_digest: "sha256:rust-v1".into(),
                queries: WorkerQuerySources::default(),
                text: "fn main() { let answer = 42; }".into(),
            },
            &mut parser,
        )
        .unwrap();

        assert!(replica.estimated_memory().non_evictable_bytes > replica.text.capacity());
    }

    #[test]
    fn cache_eviction_prefers_largest_auxiliary_then_non_current_results() {
        let candidates = vec![
            CacheEvictionCandidate {
                key: DocumentKey {
                    uri: "file:///a.rs".into(),
                },
                result_bytes: 40,
                auxiliary_bytes: 20,
                current: false,
            },
            CacheEvictionCandidate {
                key: DocumentKey {
                    uri: "file:///b.rs".into(),
                },
                result_bytes: 50,
                auxiliary_bytes: 30,
                current: true,
            },
        ];

        assert_eq!(
            cache_eviction_plan(candidates, 50),
            vec![
                CacheEviction::Auxiliary(DocumentKey {
                    uri: "file:///b.rs".into(),
                }),
                CacheEviction::Auxiliary(DocumentKey {
                    uri: "file:///a.rs".into(),
                }),
                CacheEviction::Result(DocumentKey {
                    uri: "file:///a.rs".into(),
                }),
            ]
        );
    }

    #[test]
    fn worker_pressure_evicts_auxiliary_then_non_current_result_state() {
        let documents: super::DocumentStore = Arc::new(dashmap::DashMap::new());
        let make_replica = |uri: &str| {
            let mut parser = tree_sitter::Parser::new();
            parser
                .set_language(&tree_sitter_rust::LANGUAGE.into())
                .unwrap();
            DocumentReplica::sync(
                SyncDocument {
                    context: RequestContext {
                        request_id: 1,
                        worker_generation: 3,
                        uri: uri.into(),
                        incarnation: 4,
                        content_version: 1,
                        configuration_generation: 2,
                    },
                    language: "rust".into(),
                    grammar_symbol: "rust".into(),
                    source_path: "/unused/static-language".into(),
                    parser_path: "/unused/static-language".into(),
                    artifact_digest: "sha256:rust-v1".into(),
                    queries: WorkerQuerySources::default(),
                    text: "fn main() {}".into(),
                },
                &mut parser,
            )
            .unwrap()
            .0
        };
        for uri in ["file:///a.rs", "file:///b.rs"] {
            let mut replica = make_replica(uri);
            replica.cached_semantic_tokens = Some((
                2,
                false,
                Arc::new(vec![
                    WireSemanticToken {
                        delta_line: 0,
                        delta_start: 0,
                        length: 1,
                        token_type: 0,
                        token_modifiers_bitset: 0,
                    };
                    16
                ]),
            ));
            replica.semantic_discovery = Some(Arc::new(crate::document::DiscoveredInjections {
                generation: 2,
                complete: true,
                regions: Vec::with_capacity(16),
            }));
            documents.insert(
                DocumentKey { uri: uri.into() },
                Arc::new(Mutex::new(replica)),
            );
        }
        let current = DocumentKey {
            uri: "file:///b.rs".into(),
        };
        let get = |key: &DocumentKey| Arc::clone(documents.get(key).unwrap().value());
        let current_replica = get(&current);
        let target = current_replica
            .lock()
            .unwrap()
            .estimated_memory()
            .result_cache_bytes;

        enforce_derived_memory_budget(&documents, &Mutex::new(()), &current, target).unwrap();

        let a_key = DocumentKey {
            uri: "file:///a.rs".into(),
        };
        let a_replica = get(&a_key);
        let b_replica = get(&current);
        let a = a_replica.lock().unwrap();
        let b = b_replica.lock().unwrap();
        assert!(a.semantic_discovery.is_none());
        assert!(b.semantic_discovery.is_none());
        assert!(a.cached_semantic_tokens.is_none());
        assert!(b.cached_semantic_tokens.is_some());
    }

    #[test]
    fn only_cache_growing_requests_run_memory_pressure_checkpoint() {
        let context = RequestContext {
            request_id: 1,
            worker_generation: 3,
            uri: "file:///pressure.rs".into(),
            incarnation: 4,
            content_version: 1,
            configuration_generation: 2,
        };
        assert!(request_may_grow_derived_state(
            &Request::DeriveSemanticTokens(DeriveSemanticTokens {
                context: context.clone(),
                supports_multiline: false,
            })
        ));
        assert!(request_may_grow_derived_state(
            &Request::DeriveInjectionRegions(DeriveInjectionRegions {
                context: context.clone(),
            })
        ));
        assert!(!request_may_grow_derived_state(&Request::NavigateNode(
            NavigateNode {
                context,
                node_id: OpaqueNodeId {
                    worker_generation: 3,
                    local_id: "node-1".into(),
                },
                operation: NodeNavigation::Parent,
            }
        )));
    }

    #[test]
    fn semantic_cache_hits_skip_memory_pressure_checkpoint() {
        let result = |cache_hit| {
            Response::SemanticTokens(super::DerivedSemanticTokens {
                context: RequestContext {
                    request_id: 1,
                    worker_generation: 3,
                    uri: "file:///pressure-hit.rs".into(),
                    incarnation: 4,
                    content_version: 1,
                    configuration_generation: 2,
                },
                tokens: Vec::new(),
                queue_wait_ns: 0,
                compute_ns: 0,
                cache_hit,
            })
        };

        assert!(!pressure_checkpoint_required(true, &result(true)));
        assert!(pressure_checkpoint_required(true, &result(false)));
    }

    #[test]
    fn semantic_discovery_capacity_is_reported_as_auxiliary_memory() {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        let context = RequestContext {
            request_id: 1,
            worker_generation: 3,
            uri: "file:///discovery-accounting.rs".into(),
            incarnation: 4,
            content_version: 1,
            configuration_generation: 2,
        };
        let (mut replica, _) = DocumentReplica::sync(
            SyncDocument {
                context,
                language: "rust".into(),
                grammar_symbol: "rust".into(),
                source_path: "/unused/static-language".into(),
                parser_path: "/unused/static-language".into(),
                artifact_digest: "sha256:rust-v1".into(),
                queries: WorkerQuerySources::default(),
                text: "fn main() {}".into(),
            },
            &mut parser,
        )
        .unwrap();
        let before = replica.estimated_memory();
        let regions = Vec::with_capacity(32);
        let discovery_bytes = std::mem::size_of::<crate::document::DiscoveredInjections>()
            + regions.capacity() * std::mem::size_of::<crate::document::DiscoveredRegion>();
        replica.semantic_discovery = Some(Arc::new(crate::document::DiscoveredInjections {
            generation: 2,
            complete: true,
            regions,
        }));

        let after = replica.estimated_memory();
        assert_eq!(
            after.auxiliary_cache_bytes,
            before.auxiliary_cache_bytes + discovery_bytes
        );
    }

    #[test]
    fn resolved_injection_capacity_is_reported_as_auxiliary_memory() {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        let context = RequestContext {
            request_id: 1,
            worker_generation: 3,
            uri: "file:///resolved-accounting.rs".into(),
            incarnation: 4,
            content_version: 1,
            configuration_generation: 2,
        };
        let (mut replica, _) = DocumentReplica::sync(
            SyncDocument {
                context,
                language: "rust".into(),
                grammar_symbol: "rust".into(),
                source_path: "/unused/static-language".into(),
                parser_path: "/unused/static-language".into(),
                artifact_digest: "sha256:rust-v1".into(),
                queries: WorkerQuerySources::default(),
                text: "fn main() {}".into(),
            },
            &mut parser,
        )
        .unwrap();
        let before = replica.estimated_memory();
        let resolved = Vec::with_capacity(16);
        let resolved_bytes =
            std::mem::size_of::<Vec<crate::language::injection::ResolvedInjection>>()
                + resolved.capacity()
                    * std::mem::size_of::<crate::language::injection::ResolvedInjection>();
        replica.resolved_injections = Some((2, Arc::new(resolved)));

        let after = replica.estimated_memory();
        assert_eq!(
            after.auxiliary_cache_bytes,
            before.auxiliary_cache_bytes + resolved_bytes
        );
    }

    #[test]
    fn injection_layer_capacity_is_reported_as_auxiliary_memory() {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        let context = RequestContext {
            request_id: 1,
            worker_generation: 3,
            uri: "file:///layer-accounting.rs".into(),
            incarnation: 4,
            content_version: 1,
            configuration_generation: 2,
        };
        let (mut replica, _) = DocumentReplica::sync(
            SyncDocument {
                context,
                language: "rust".into(),
                grammar_symbol: "rust".into(),
                source_path: "/unused/static-language".into(),
                parser_path: "/unused/static-language".into(),
                artifact_digest: "sha256:rust-v1".into(),
                queries: WorkerQuerySources::default(),
                text: "fn main() {}".into(),
            },
            &mut parser,
        )
        .unwrap();
        let before = replica.estimated_memory();
        replica.injection_layers = Vec::with_capacity(8);
        let layer_bytes =
            replica.injection_layers.capacity() * std::mem::size_of::<CachedInjectionLayer>();

        let after = replica.estimated_memory();
        assert_eq!(
            after.auxiliary_cache_bytes,
            before.auxiliary_cache_bytes + layer_bytes
        );
    }

    #[test]
    fn cancelled_semantic_tokens_are_not_published_or_cached() {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        let context = RequestContext {
            request_id: 1,
            worker_generation: 3,
            uri: "file:///cancelled-semantic.rs".into(),
            incarnation: 4,
            content_version: 1,
            configuration_generation: 2,
        };
        let (mut replica, _) = DocumentReplica::sync(
            SyncDocument {
                context: context.clone(),
                language: "rust".into(),
                grammar_symbol: "rust".into(),
                source_path: "/unused/static-language".into(),
                parser_path: "/unused/static-language".into(),
                artifact_digest: "sha256:rust-v1".into(),
                queries: WorkerQuerySources {
                    highlights: Some("(identifier) @variable".into()),
                    ..WorkerQuerySources::default()
                },
                text: "fn main() {}".into(),
            },
            &mut parser,
        )
        .unwrap();
        let cancellation = crate::cancel::CancelToken::default();
        cancellation.cancel_after_polls(1);

        let result = replica.derive_semantic_tokens_with_telemetry(
            DeriveSemanticTokens {
                context,
                supports_multiline: false,
            },
            Duration::ZERO,
            Instant::now(),
            rayon::current_num_threads(),
            Some(&cancellation),
            None,
        );

        assert_eq!(
            result.unwrap_err(),
            "worker semantic token derivation was cancelled"
        );
        assert!(
            replica.cached_semantic_tokens.is_none(),
            "cancelled results must never become reusable worker facts"
        );
    }

    #[test]
    fn native_bindings_are_resolved_from_the_worker_owned_tree() {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        let context = RequestContext {
            request_id: 1,
            worker_generation: 3,
            uri: "file:///bindings.rs".into(),
            incarnation: 4,
            content_version: 1,
            configuration_generation: 2,
        };
        let (replica, _) = DocumentReplica::sync(
            SyncDocument {
                context: context.clone(),
                language: "rust".into(),
                grammar_symbol: "rust".into(),
                source_path: "/unused/static-language".into(),
                parser_path: "/unused/static-language".into(),
                artifact_digest: "sha256:rust-v1".into(),
                queries: WorkerQuerySources {
                    bindings: Some(
                        r#"
                        (block) @scope
                        (let_declaration pattern: (identifier) @definition)
                        (identifier) @reference
                        "#
                        .into(),
                    ),
                    ..WorkerQuerySources::default()
                },
                text: "fn main() { let target = 1; target; }".into(),
            },
            &mut parser,
        )
        .unwrap();

        let result = replica
            .derive_native_bindings(DeriveNativeBindings {
                context,
                position: WirePosition {
                    line: 0,
                    character: 28,
                },
            })
            .unwrap();
        let facts = result.facts.expect("reference must resolve");

        assert_eq!(facts.definition, Some(WireByteRange { start: 16, end: 22 }));
        assert_eq!(facts.references, vec![WireByteRange { start: 28, end: 34 }]);
        assert_eq!(
            facts.definitions,
            vec![WireByteRange { start: 16, end: 22 }]
        );
    }

    #[test]
    fn native_bindings_are_resolved_in_the_innermost_injected_layer() {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_md::LANGUAGE.into())
            .unwrap();
        let context = RequestContext {
            request_id: 1,
            worker_generation: 3,
            uri: "file:///bindings.md".into(),
            incarnation: 4,
            content_version: 1,
            configuration_generation: 2,
        };
        let analysis = Arc::new(crate::language::LanguageCoordinator::new());
        super::worker_analysis(
            &analysis,
            "lua",
            tree_sitter_lua::LANGUAGE.into(),
            WorkerQuerySources {
                bindings: Some(include_str!("../assets/queries/lua/bindings.scm").into()),
                ..WorkerQuerySources::default()
            },
            super::WorkerQuerySet::Bindings,
        )
        .unwrap();
        let (replica, _) = DocumentReplica::sync_with_analysis(
            SyncDocument {
                context: context.clone(),
                language: "markdown".into(),
                grammar_symbol: "markdown".into(),
                source_path: "/unused/static-language".into(),
                parser_path: "/unused/static-language".into(),
                artifact_digest: "sha256:markdown-v1".into(),
                queries: WorkerQuerySources {
                    injections: Some(
                        r#"
                        (fenced_code_block
                          (info_string (language) @injection.language)
                          (code_fence_content) @injection.content)
                        "#
                        .into(),
                    ),
                    ..WorkerQuerySources::default()
                },
                text: "# t\n\n```lua\nlocal v = 1\nprint(v)\n```\n".into(),
            },
            &mut parser,
            analysis,
        )
        .unwrap();

        let result = replica
            .derive_native_bindings(DeriveNativeBindings {
                context,
                position: WirePosition {
                    line: 4,
                    character: 6,
                },
            })
            .unwrap();
        let facts = result.facts.expect("injected reference must resolve");

        assert_eq!(facts.definition, Some(WireByteRange { start: 18, end: 19 }));
        assert_eq!(facts.references, vec![WireByteRange { start: 30, end: 31 }]);
        assert_eq!(
            facts.definitions,
            vec![WireByteRange { start: 18, end: 19 }]
        );
    }

    #[test]
    fn captures_are_derived_from_worker_owned_tree_and_query_paths() {
        let directory = tempfile::tempdir().unwrap();
        let query_dir = directory.path().join("queries/rust");
        std::fs::create_dir_all(&query_dir).unwrap();
        std::fs::write(query_dir.join("context.scm"), "(identifier) @name").unwrap();

        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        let context = RequestContext {
            request_id: 1,
            worker_generation: 3,
            uri: "file:///captures.rs".into(),
            incarnation: 4,
            content_version: 1,
            configuration_generation: 2,
        };
        let analysis = Arc::new(crate::language::LanguageCoordinator::new());
        analysis.configure_worker_runtime(vec![directory.path().into()], Default::default());
        let (replica, _) = DocumentReplica::sync_with_analysis(
            SyncDocument {
                context: context.clone(),
                language: "rust".into(),
                grammar_symbol: "rust".into(),
                source_path: "/unused/static-language".into(),
                parser_path: "/unused/static-language".into(),
                artifact_digest: "sha256:rust-v1".into(),
                queries: WorkerQuerySources::default(),
                text: "fn main() {}".into(),
            },
            &mut parser,
            analysis,
        )
        .unwrap();

        let result = replica
            .run_captures(RunCaptures {
                context,
                kind: "context".into(),
                range: None,
                injection: false,
            })
            .unwrap();

        assert_eq!(result.matches.len(), 1);
        assert_eq!(result.matches[0]["language"], "rust");
        assert_eq!(result.matches[0]["captures"][0]["name"], "name");
        assert!(result.skipped.is_empty());
    }

    #[test]
    fn document_replica_rejects_an_edit_with_a_stale_base_version() {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        let context = RequestContext {
            request_id: 1,
            worker_generation: 3,
            uri: "file:///example.rs".into(),
            incarnation: 4,
            content_version: 3,
            configuration_generation: 2,
        };
        let (mut replica, _) = DocumentReplica::sync(
            SyncDocument {
                context: context.clone(),
                language: "rust".into(),
                grammar_symbol: "rust".into(),
                source_path: "/unused/static-language".into(),
                parser_path: "/unused/static-language".into(),
                artifact_digest: "sha256:rust-v1".into(),
                queries: WorkerQuerySources::default(),
                text: "fn main() {}".into(),
            },
            &mut parser,
        )
        .unwrap();
        let mut target = context;
        target.request_id = 2;
        target.content_version = 4;

        let error = replica
            .apply(
                ApplyDocumentEdits {
                    context: target,
                    base_version: 2,
                    edits: Vec::new(),
                },
                &mut parser,
                None,
            )
            .unwrap_err();

        assert!(error.contains("base version 2 does not match 3"));
    }

    #[test]
    fn document_replica_rejects_a_stale_full_sync() {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        let context = RequestContext {
            request_id: 1,
            worker_generation: 3,
            uri: "file:///stale-sync.rs".into(),
            incarnation: 4,
            content_version: 3,
            configuration_generation: 2,
        };
        let (replica, _) = DocumentReplica::sync(
            SyncDocument {
                context: context.clone(),
                language: "rust".into(),
                grammar_symbol: "rust".into(),
                source_path: "/unused/static-language".into(),
                parser_path: "/unused/static-language".into(),
                artifact_digest: "sha256:rust-v1".into(),
                queries: WorkerQuerySources::default(),
                text: "fn current() {}".into(),
            },
            &mut parser,
        )
        .unwrap();
        let mut stale = context;
        stale.request_id = 2;
        stale.content_version = 2;

        assert!(
            replica
                .validate_replacement(
                    &stale,
                    &replica.language,
                    &replica.grammar_key,
                    &replica.queries,
                )
                .unwrap_err()
                .contains("stale content version")
        );
    }

    #[test]
    fn document_replica_allows_same_version_query_asset_refresh() {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        let context = RequestContext {
            request_id: 1,
            worker_generation: 3,
            uri: "file:///query-refresh.rs".into(),
            incarnation: 4,
            content_version: 3,
            configuration_generation: 2,
        };
        let (replica, _) = DocumentReplica::sync(
            SyncDocument {
                context: context.clone(),
                language: "rust".into(),
                grammar_symbol: "rust".into(),
                source_path: "/unused/static-language".into(),
                parser_path: "/unused/static-language".into(),
                artifact_digest: "sha256:rust-v1".into(),
                queries: WorkerQuerySources::default(),
                text: "fn current() {}".into(),
            },
            &mut parser,
        )
        .unwrap();
        let refreshed = WorkerQuerySources {
            bindings: Some("(identifier) @reference".into()),
            ..WorkerQuerySources::default()
        };

        replica
            .validate_replacement(
                &context,
                &replica.language,
                &replica.grammar_key,
                &refreshed,
            )
            .expect("query-only refresh must be accepted at the same content version");
        assert!(
            replica
                .validate_replacement(
                    &context,
                    &replica.language,
                    &replica.grammar_key,
                    &replica.queries,
                )
                .unwrap_err()
                .contains("must advance")
        );
    }

    #[test]
    fn document_replica_fences_language_and_grammar_replacement() {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        let context = RequestContext {
            request_id: 1,
            worker_generation: 3,
            uri: "file:///replacement.rs".into(),
            incarnation: 4,
            content_version: 3,
            configuration_generation: 2,
        };
        let (replica, _) = DocumentReplica::sync(
            SyncDocument {
                context: context.clone(),
                language: "rust".into(),
                grammar_symbol: "rust".into(),
                source_path: "/unused/rust".into(),
                parser_path: "/unused/rust".into(),
                artifact_digest: "sha256:rust-v1".into(),
                queries: WorkerQuerySources::default(),
                text: "fn current() {}".into(),
            },
            &mut parser,
        )
        .unwrap();
        let mut replacement = context;
        replacement.request_id = 2;
        replacement.content_version = 4;

        assert!(
            replica
                .validate_replacement(
                    &replacement,
                    "python",
                    &replica.grammar_key,
                    &replica.queries,
                )
                .unwrap_err()
                .contains("language change")
        );
        let alias_grammar = GrammarKey {
            parser_path: "/unused/other-rust".into(),
            grammar_symbol: "rust".into(),
            artifact_digest: "sha256:rust-v1".into(),
        };
        replica
            .validate_replacement(&replacement, "rust", &alias_grammar, &replica.queries)
            .unwrap();

        let different_grammar = GrammarKey {
            parser_path: "/unused/rust".into(),
            grammar_symbol: "rust".into(),
            artifact_digest: "sha256:rust-v2".into(),
        };
        assert!(
            replica
                .validate_replacement(&replacement, "rust", &different_grammar, &replica.queries,)
                .unwrap_err()
                .contains("grammar change")
        );
    }

    #[test]
    fn document_identity_limit_requests_a_worker_restart() {
        let documents = Arc::new(dashmap::DashMap::new());
        let closed_documents = Arc::new(dashmap::DashMap::new());
        let capacity = Arc::new(Mutex::new(MAX_DOCUMENT_REPLICAS));
        let response = sync_document(
            SyncDocument {
                context: RequestContext {
                    request_id: 1,
                    worker_generation: 3,
                    uri: "file:///over-limit.rs".into(),
                    incarnation: 5_000,
                    content_version: 1,
                    configuration_generation: 0,
                },
                language: "rust".into(),
                grammar_symbol: "rust".into(),
                source_path: "/unused/rust".into(),
                parser_path: "/unused/rust".into(),
                artifact_digest: "sha256:rust-v1".into(),
                queries: WorkerQuerySources::default(),
                text: "fn main() {}".into(),
            },
            &documents,
            &closed_documents,
            &capacity,
            &Arc::new(AtomicUsize::new(0)),
        );

        assert!(matches!(response, Response::WorkerRestartRequired(_)));
    }

    #[test]
    fn poisoned_document_replica_requires_restart_instead_of_reusing_uncertain_state() {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        let context = RequestContext {
            request_id: 1,
            worker_generation: 3,
            uri: "file:///poisoned.rs".into(),
            incarnation: 1,
            content_version: 1,
            configuration_generation: 0,
        };
        let text = "fn main() {}";
        let (replica, _) = DocumentReplica::sync(
            SyncDocument {
                context: context.clone(),
                language: "rust".into(),
                grammar_symbol: "rust".into(),
                source_path: "/unused/rust".into(),
                parser_path: "/unused/rust".into(),
                artifact_digest: "sha256:rust-v1".into(),
                queries: WorkerQuerySources::default(),
                text: text.into(),
            },
            &mut parser,
        )
        .unwrap();
        let documents = Arc::new(dashmap::DashMap::new());
        let key = DocumentKey::from(&context);
        let replica = Arc::new(Mutex::new(replica));
        documents.insert(key.clone(), Arc::clone(&replica));
        let poison_target = Arc::clone(&replica);
        let _ = std::thread::spawn(move || {
            let _guard = poison_target.lock().unwrap();
            panic!("poison replica for restart test");
        })
        .join();
        let closed_documents = Arc::new(dashmap::DashMap::new());
        let retained = Arc::new(AtomicUsize::new(text.len()));

        let mut replacement_context = context.clone();
        replacement_context.request_id = 2;
        replacement_context.content_version = 2;
        let sync_response = sync_document(
            SyncDocument {
                context: replacement_context,
                language: "rust".into(),
                grammar_symbol: "rust".into(),
                source_path: "/unused/rust".into(),
                parser_path: "/unused/rust".into(),
                artifact_digest: "sha256:rust-v1".into(),
                queries: WorkerQuerySources::default(),
                text: "fn replacement() {}".into(),
            },
            &documents,
            &closed_documents,
            &Arc::new(Mutex::new(1)),
            &retained,
        );
        assert!(matches!(sync_response, Response::WorkerRestartRequired(_)));

        let mut close_context = context;
        close_context.request_id = 3;
        close_context.content_version = 2;
        let retained_estimated = Arc::new(AtomicUsize::new(0));
        let close_response = close_document(
            CloseDocument {
                context: close_context,
            },
            &documents,
            &closed_documents,
            &retained,
            &retained_estimated,
        );
        assert!(matches!(close_response, Response::WorkerRestartRequired(_)));
        assert!(documents.contains_key(&key));
        assert_eq!(retained.load(Ordering::Relaxed), text.len());
        assert_eq!(retained_estimated.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn document_size_limit_rejects_unbounded_replica_growth() {
        assert!(validate_document_size(MAX_DOCUMENT_BYTES).is_ok());
        assert!(
            validate_document_size(MAX_DOCUMENT_BYTES + 1)
                .unwrap_err()
                .contains("retained size limit")
        );
    }

    #[test]
    fn retained_document_budget_is_transactional() {
        let retained = AtomicUsize::new(MAX_RETAINED_DOCUMENT_BYTES - 8);

        assert!(
            replace_retained_bytes(&retained, 4, 13)
                .unwrap_err()
                .contains("retained document budget")
        );
        assert_eq!(
            retained.load(Ordering::Relaxed),
            MAX_RETAINED_DOCUMENT_BYTES - 8
        );

        replace_retained_bytes(&retained, 4, 12).unwrap();
        assert_eq!(
            retained.load(Ordering::Relaxed),
            MAX_RETAINED_DOCUMENT_BYTES
        );
    }

    #[test]
    fn estimated_non_evictable_budget_is_transactional() {
        let retained = AtomicUsize::new(MAX_RETAINED_ESTIMATED_DOCUMENT_BYTES - 8);

        assert!(replace_retained_estimated_bytes(
            &retained,
            0,
            9,
            MAX_RETAINED_ESTIMATED_DOCUMENT_BYTES,
        )
        .is_err());
        assert_eq!(
            retained.load(Ordering::Relaxed),
            MAX_RETAINED_ESTIMATED_DOCUMENT_BYTES - 8
        );
        replace_retained_estimated_bytes(
            &retained,
            0,
            8,
            MAX_RETAINED_ESTIMATED_DOCUMENT_BYTES,
        )
        .unwrap();
        assert_eq!(
            retained.load(Ordering::Relaxed),
            MAX_RETAINED_ESTIMATED_DOCUMENT_BYTES
        );
    }

    #[test]
    fn estimated_non_evictable_accounting_obeys_injected_hard_budget() {
        let retained = AtomicUsize::new(0);

        assert!(replace_retained_estimated_bytes(&retained, 0, 9, 8).is_err());
        assert_eq!(retained.load(Ordering::Relaxed), 0);
        replace_retained_estimated_bytes(&retained, 0, 8, 8).unwrap();
        assert_eq!(retained.load(Ordering::Relaxed), 8);
    }

    #[test]
    fn worker_memory_budgets_reject_zero_limits() {
        assert!(super::WorkerMemoryBudgets::new(0, 1).is_err());
        assert!(super::WorkerMemoryBudgets::new(1, 0).is_err());
    }

    #[test]
    fn retained_document_shrink_does_not_release_budget_before_commit() {
        let retained = AtomicUsize::new(MAX_RETAINED_DOCUMENT_BYTES);

        assert!(!reserve_retained_growth(&retained, 12, 4).unwrap());
        assert_eq!(
            retained.load(Ordering::Relaxed),
            MAX_RETAINED_DOCUMENT_BYTES
        );

        replace_retained_bytes(&retained, 12, 4).unwrap();
        assert_eq!(
            retained.load(Ordering::Relaxed),
            MAX_RETAINED_DOCUMENT_BYTES - 8
        );
    }

    #[test]
    fn retained_document_growth_reservation_can_always_roll_back() {
        let retained = AtomicUsize::new(MAX_RETAINED_DOCUMENT_BYTES - 8);

        assert!(reserve_retained_growth(&retained, 4, 12).unwrap());
        assert_eq!(
            retained.load(Ordering::Relaxed),
            MAX_RETAINED_DOCUMENT_BYTES
        );

        replace_retained_bytes(&retained, 12, 4).unwrap();
        assert_eq!(
            retained.load(Ordering::Relaxed),
            MAX_RETAINED_DOCUMENT_BYTES - 8
        );
    }

    #[test]
    fn document_replica_keeps_the_previous_version_when_an_edit_batch_is_invalid() {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        let context = RequestContext {
            request_id: 1,
            worker_generation: 3,
            uri: "file:///transaction.rs".into(),
            incarnation: 1,
            content_version: 1,
            configuration_generation: 0,
        };
        let original = "fn main() { 1 }";
        let (mut replica, _) = DocumentReplica::sync(
            SyncDocument {
                context: context.clone(),
                language: "rust".into(),
                grammar_symbol: "rust".into(),
                source_path: "/unused/static-language".into(),
                parser_path: "/unused/static-language".into(),
                artifact_digest: "sha256:rust-v1".into(),
                queries: WorkerQuerySources::default(),
                text: original.into(),
            },
            &mut parser,
        )
        .unwrap();
        let mut target = context.clone();
        target.request_id = 2;
        target.content_version = 2;

        replica
            .apply(
                ApplyDocumentEdits {
                    context: target,
                    base_version: 1,
                    edits: vec![
                        ByteEdit {
                            start_byte: 12,
                            old_end_byte: 13,
                            new_text: "2".into(),
                        },
                        ByteEdit {
                            start_byte: 999,
                            old_end_byte: 999,
                            new_text: "invalid".into(),
                        },
                    ],
                },
                &mut parser,
                None,
            )
            .unwrap_err();

        let mut derive_context = context;
        derive_context.request_id = 3;
        let snapshot = replica.derive(derive_context).unwrap();
        assert_eq!(snapshot.root_end_byte, original.len());
        assert_eq!(replica.text, original);
    }

    #[test]
    fn document_replica_applies_edit_ranges_to_each_intermediate_text() {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        let context = RequestContext {
            request_id: 1,
            worker_generation: 3,
            uri: "file:///sequential-edits.rs".into(),
            incarnation: 1,
            content_version: 1,
            configuration_generation: 0,
        };
        let (mut replica, _) = DocumentReplica::sync(
            SyncDocument {
                context: context.clone(),
                language: "rust".into(),
                grammar_symbol: "rust".into(),
                source_path: "/unused/static-language".into(),
                parser_path: "/unused/static-language".into(),
                artifact_digest: "sha256:rust-v1".into(),
                queries: WorkerQuerySources::default(),
                text: "fn main() { 1 }".into(),
            },
            &mut parser,
        )
        .unwrap();
        let mut target = context;
        target.request_id = 2;
        target.content_version = 2;

        replica
            .apply(
                ApplyDocumentEdits {
                    context: target,
                    base_version: 1,
                    edits: vec![
                        ByteEdit {
                            start_byte: 12,
                            old_end_byte: 12,
                            new_text: "value + ".into(),
                        },
                        ByteEdit {
                            start_byte: 20,
                            old_end_byte: 21,
                            new_text: "2".into(),
                        },
                    ],
                },
                &mut parser,
                None,
            )
            .unwrap();

        assert_eq!(replica.text, "fn main() { value + 2 }");
    }
}
