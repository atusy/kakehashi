//! Inbound `telemetry/*` notifications.
//!
//! The `telemetry` LSP namespace is inbound here (downstream → editor): a
//! downstream server emits `telemetry/event`, which the bridge forwards to the
//! editor verbatim. One file per method keeps the directory listing a map of
//! the supported surface, mirroring [`text_document`](super::text_document).
//!
//! The dispatcher in [`actor::reader`](super::actor) owns the shared transport
//! and routes the method here.

pub(in crate::lsp::bridge) mod event;
