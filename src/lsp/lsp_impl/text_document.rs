//! Text document related LSP methods.

mod code_action;
mod code_lens;
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
mod folding_range;
mod formatting;
mod hover;
mod implementation;
mod inlay_hint;
mod linked_editing_range;
mod moniker;
mod on_type_formatting;
mod prepare_rename;
pub(crate) mod publish_diagnostic;
mod range_formatting;
mod references;
mod rename;
mod selection_range;
mod semantic_tokens;
mod signature_help;
mod type_definition;
mod will_save;
mod will_save_wait_until;

// Re-export the methods (they are implemented as impl blocks on Kakehashi)
