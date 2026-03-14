//! Text document related LSP methods.

#[cfg(feature = "experimental")]
mod color_presentation;
mod completion;
mod completion_item;
mod declaration;
mod definition;
pub(crate) mod diagnostic;
mod did_change;
mod did_close;
mod did_open;
mod did_save;
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
pub(crate) mod publish_diagnostic;
mod references;
mod rename;
mod selection_range;
mod semantic_tokens;
mod signature_help;
mod type_definition;

// Re-export the methods (they are implemented as impl blocks on Kakehashi)
