//! Text document request handlers for bridge connections.
//!
//! This module provides LSP text document request functionality (hover, completion, etc.)
//! for downstream language servers via the bridge architecture.
//!
//! The structure mirrors `lsp_impl/text_document/` for consistency.

#[cfg(feature = "experimental")]
mod color_presentation;
mod completion;
mod completion_item;
mod declaration;
mod definition;
mod diagnostic;
mod did_change;
mod did_close;
mod did_open;
#[cfg(feature = "experimental")]
mod document_color;
mod document_highlight;
mod document_link;
mod document_symbol;
mod hover;
mod implementation;
mod inlay_hint;
mod moniker;
mod prepare_rename;
mod references;
mod rename;
mod signature_help;
#[cfg(test)]
pub(in crate::lsp::bridge) mod test_helpers;
mod type_definition;
