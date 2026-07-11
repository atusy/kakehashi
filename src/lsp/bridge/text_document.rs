//! Text document request handlers for bridge connections.
//!
//! This module provides LSP text document request functionality (hover, completion, etc.)
//! for downstream language servers via the bridge architecture.
//!
//! The structure mirrors `lsp_impl/text_document/` for consistency.

mod code_action;
mod code_lens;
mod color_presentation;

pub(crate) use code_action::{
    CodeActionEnvelope, UpstreamCodeActionCaps, bridge_code_actions, extract_code_action_envelope,
    parse_code_actions_leniently,
};
pub(crate) use code_lens::{CodeLensEnvelope, extract_code_lens_envelope};
mod completion;
pub(crate) use completion::{KakehashiEnvelope, extract_envelope};
mod completion_item;
mod declaration;
mod definition;
mod diagnostic;
mod did_change;
mod did_close;
mod did_open;
mod document_color;
mod document_highlight;
mod document_link;
mod document_symbol;
mod folding_range;
mod formatting;
pub(super) mod host;
mod hover;
mod implementation;
mod inlay_hint;
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
