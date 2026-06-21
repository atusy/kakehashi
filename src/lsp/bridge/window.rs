//! Inbound `window/*` server messages, plus the bridged `$/progress` stream.
//!
//! The `window` LSP namespace is entirely inbound (downstream → editor): a
//! downstream server surfaces log/show messages or declares work-done progress,
//! and the bridge forwards them upstream. Each method has its own file so the
//! directory listing maps to the supported surface, mirroring
//! [`text_document`](super::text_document).
//!
//! `$/progress` lives here too ([`progress`]) even though it is `$`-namespaced:
//! it is bridged as the data half of `window/workDoneProgress/create`
//! ([`work_done_progress_create`]), sharing the same [`ProgressRegistry`] token
//! remapping. Splitting the two by namespace would scatter one feature, so they
//! sit together under `window/` with this note.
//!
//! The dispatcher in [`actor::reader`](super::actor) owns the shared transport
//! and routes each method here; the `window/*` forwarders reach back for the
//! bounded best-effort window channel via [`send_window_notification`].
//!
//! [`ProgressRegistry`]: crate::lsp::bridge::ProgressRegistry

use log::debug;
use tokio::sync::mpsc;

use crate::lsp::bridge::actor::{ServerRequestDeps, UpstreamNotification};

pub(in crate::lsp::bridge) mod log_message;
pub(in crate::lsp::bridge) mod progress;
pub(in crate::lsp::bridge) mod show_document;
pub(in crate::lsp::bridge) mod show_message;
pub(in crate::lsp::bridge) mod show_message_request;
pub(in crate::lsp::bridge) mod work_done_progress_create;

/// Enqueue a notification on the bounded best-effort channel.
///
/// Shared by the `window/*` forwarders and [`telemetry::event`] (which rides the
/// same loss-tolerant channel); hence `pub(in crate::lsp::bridge)`.
///
/// `try_send` keeps the reader task non-blocking: a full queue (editor slower
/// than a downstream notification flood) drops the message instead of growing
/// memory or stalling stdout reads; a closed queue (shutdown) has no one left
/// to deliver to. Both are deliberate per the loss-tolerance split documented
/// on [`UpstreamNotification`].
///
/// [`telemetry::event`]: crate::lsp::bridge::telemetry::event
pub(in crate::lsp::bridge) fn send_window_notification(
    deps: &ServerRequestDeps,
    server_prefix: &str,
    method: &str,
    notification: UpstreamNotification,
) {
    if let Err(e) = deps.window_tx.try_send(notification) {
        debug!(
            target: "kakehashi::bridge::reader",
            "{}Dropping {} ({})",
            server_prefix,
            method,
            match e {
                mpsc::error::TrySendError::Full(_) => "window notification queue full",
                mpsc::error::TrySendError::Closed(_) => "forwarding loop gone",
            }
        );
    }
}

/// `[kakehashi:<server>] <message>` — tells the user which downstream server
/// a forwarded `window/*` notification came from (#378). Falls back to
/// `[kakehashi] <message>` when the server name is absent (test-only spawns;
/// production spawns always carry the name, see pool.rs).
fn prefixed_message(server_name: Option<&str>, message: &str) -> String {
    match server_name {
        Some(name) => format!("[kakehashi:{name}] {message}"),
        None => format!("[kakehashi] {message}"),
    }
}
