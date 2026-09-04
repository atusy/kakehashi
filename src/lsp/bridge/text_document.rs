//! Text document request handlers for bridge connections.
//!
//! This module provides LSP text document request functionality (hover, completion, etc.)
//! for downstream language servers via the bridge architecture.
//!
//! The structure mirrors `lsp_impl/text_document/` for consistency.

mod call_hierarchy;
mod code_action;
mod code_lens;
pub(crate) use call_hierarchy::{
    CallHierarchyDocumentRevision, CallHierarchyEnvelope, envelope_host_call_hierarchy_items,
    extract_call_hierarchy_envelope,
};
mod color_presentation;

pub(crate) use code_action::{
    CodeActionEnvelope, UpstreamCodeActionCaps, bridge_code_actions, extract_code_action_envelope,
    parse_code_actions_leniently,
};
pub(crate) use code_lens::{envelope_host_code_lenses, extract_code_lens_envelope};
mod completion;
pub(crate) use completion::{
    EnvelopeOffset, KakehashiEnvelope, bridge_host_completion_items, extract_envelope,
};
mod completion_item;
mod declaration;
mod definition;
mod diagnostic;
mod did_change;
mod did_close;
mod did_open;
pub(crate) use did_open::{OpenExpectation, OpenOutcome};
mod document_color;
mod document_highlight;
mod document_link;
pub(crate) use document_link::{envelope_host_document_links, extract_document_link_envelope};
mod document_symbol;
mod folding_range;
mod formatting;
pub(super) mod host;
mod hover;
mod implementation;
mod inlay_hint;
pub(crate) use inlay_hint::{
    InlayHintDocumentRevision, InlayHintEnvelope, envelope_host_inlay_hints,
    extract_inlay_hint_envelope,
};
mod linked_editing_range;
mod moniker;
mod on_type_formatting;
mod prepare_rename;
pub(in crate::lsp::bridge) mod publish_diagnostics;
mod range_formatting;
mod references;
mod rename;
mod save;
mod signature_help;
#[cfg(test)]
pub(in crate::lsp::bridge) mod test_helpers;
mod type_definition;
mod type_hierarchy;
pub(crate) use type_hierarchy::{
    TypeHierarchyDocumentRevision, TypeHierarchyEnvelope, envelope_host_type_hierarchy_items,
    extract_type_hierarchy_envelope, parse_type_hierarchy_items,
};
