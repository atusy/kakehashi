//! Inbound `client/*` server-request handlers.
//!
//! The `client` LSP namespace is entirely inbound (downstream → bridge): a
//! downstream server asks the client (the bridge, here) to register or
//! unregister dynamic capabilities. Each method has its own file so the
//! directory listing maps to the supported surface, mirroring
//! [`text_document`](super::text_document).
//!
//! The dispatcher in [`actor::reader`](super::actor) owns the shared transport
//! and routes each method here.

pub(in crate::lsp::bridge) mod register_capability;
pub(in crate::lsp::bridge) mod unregister_capability;
