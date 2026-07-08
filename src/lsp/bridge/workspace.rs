//! Inbound `workspace/*` server-request handlers.
//!
//! The `workspace` LSP namespace is inbound here (downstream → bridge): a
//! downstream server asks the client (the bridge) to apply a workspace edit
//! or re-pull diagnostics, or pulls the workspace folders it serves. Each
//! method has its own file so the directory listing maps to the supported
//! surface, mirroring [`text_document`](super::text_document).
//!
//! [`folder_set`] holds the per-connection [`WorkspaceFolderSet`] *data type*
//! that [`workspace_folders`] answers pulls from; keeping the type and its
//! handler in separate files avoids a name collision while both stay under the
//! namespace they belong to.
//!
//! The dispatcher in [`actor::reader`](super::actor) owns the shared transport
//! and routes each method here.

pub(in crate::lsp::bridge) mod apply_edit;
pub(in crate::lsp::bridge) mod configuration;
pub(in crate::lsp::bridge) mod diagnostic_refresh;
// The one OUTBOUND method in this namespace (editor → bridge → downstream):
// routes a bridged command back to its origin server. Kept here so the
// `workspace/*` surface stays in one directory.
pub(in crate::lsp::bridge) mod execute_command;
mod folder_set;
pub(in crate::lsp::bridge) mod workspace_folders;

pub(crate) use folder_set::WorkspaceFolderSet;
