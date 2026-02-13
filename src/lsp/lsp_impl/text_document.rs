//! Text document related LSP methods.

#[cfg(feature = "experimental")]
mod color_presentation;
mod completion;
mod declaration;
mod definition;
pub(crate) mod diagnostic;
#[cfg(feature = "experimental")]
mod document_color;
mod document_highlight;
mod document_link;
mod document_symbol;
mod first_win;
mod hover;
mod implementation;
mod inlay_hint;
mod moniker;
mod publish_diagnostic;
mod references;
mod rename;
mod selection_range;
mod semantic_tokens;
mod signature_help;
mod type_definition;

// Re-export the methods (they are implemented as impl blocks on Kakehashi)
