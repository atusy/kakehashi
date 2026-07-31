pub mod auto_install;
mod bridge;
mod cache;
mod client;
mod debounced_diagnostics;
mod diagnostic_cache;
mod diagnostic_order;
pub(crate) mod in_progress_set;
mod settings_manager;
mod synthetic_diagnostics;
mod text_sync;
mod wire_repair;

mod aggregation;
mod ingress_order;
mod lsp_impl;
mod method_alias;
mod progress;
mod request_id;
mod semantic_request_tracker;
mod settings;

pub use bridge::LanguageServerPool;
use ingress_order::IngressOrderGate;
pub(crate) use ingress_order::current_writer_ticket;
pub use lsp_impl::Kakehashi;
use method_alias::DeprecatedMethodAlias;

/// Compose the ingress middleware in the single order that is correct, so the
/// order is a property of the library rather than of the call site.
///
/// `DeprecatedMethodAlias` **must** wrap `IngressOrderGate`, not the reverse.
/// The gate classifies requests by method name to assign per-document
/// wire-order tickets and knows only the canonical scope-first spellings. With
/// the gate on the outside, a deprecated name would reach it unrecognized,
/// classify as `Role::None`, pass through ungated, and answer from a tree
/// missing an edit that preceded it on the wire — for exactly the un-migrated
/// clients the alias exists to serve.
///
/// Both orders compile and both leave every request answerable, so the mistake
/// is invisible at the call site. Wrapping it here means the unit test that
/// drives this function is what pins the order, rather than a comment in
/// `main.rs` that nothing checks.
///
/// The `Future: Send` bound must be restated on the return type: `Server::serve`
/// requires it, and an opaque `impl Trait` would otherwise hide that the
/// composed future is `Send`.
///
/// `client` carries the deprecation notice to the editor's LSP log. It is a
/// `OnceLock` because `LspService::build` only hands out the `Client` inside
/// the factory closure that also builds `inner`, so the caller fills the slot
/// there and passes it here.
pub fn ingress_stack<S>(
    inner: S,
    client: std::sync::Arc<std::sync::OnceLock<tower_lsp_server::Client>>,
) -> impl tower::Service<
    tower_lsp_server::jsonrpc::Request,
    Response = S::Response,
    Error = S::Error,
    Future: Send + 'static,
>
where
    S: tower::Service<
            tower_lsp_server::jsonrpc::Request,
            Response = Option<tower_lsp_server::jsonrpc::Response>,
        >,
    S::Future: Send + 'static,
    S::Error: Send + 'static,
{
    DeprecatedMethodAlias::new(IngressOrderGate::new(inner), client)
}
pub(crate) use request_id::current_upstream_id;
pub use request_id::{CancelForwarder, RequestIdCapture};
pub(crate) use settings::{SettingsEvent, SettingsEventKind, SettingsSource, load_settings};
pub use wire_repair::repair_inbound_frames;
