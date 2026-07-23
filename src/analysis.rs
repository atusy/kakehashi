pub(crate) mod bindings;
pub(crate) mod offset_calculator;
pub(crate) mod result_id;
pub(crate) mod selection;
pub(crate) mod semantic;
pub(crate) mod semantic_cache;

// Re-export crate-internal types and functions
pub(crate) use result_id::next_result_id;
pub(crate) use selection::handle_selection_range;
pub(crate) use semantic::{LEGEND_MODIFIERS, LEGEND_TYPES, calculate_delta_or_full};
pub(crate) use semantic_cache::{
    InjectionMap, InjectionTokenCache, SemanticSnapshotIdentity, SemanticTokenCache,
    SemanticTokenRangeCache,
};

// Re-export crate-internal functions used by LSP layer
pub(crate) use semantic::{
    SemanticArtifact, SemanticArtifactIdentity, filter_semantic_tokens_by_range,
    handle_semantic_tokens_full,
};
