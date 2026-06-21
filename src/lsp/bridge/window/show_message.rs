//! `window/showMessage` notification forwarder.
//!
//! Inbound (downstream → editor). Forwarded unconditionally so the bridge stays
//! transparent, prefixed with the originating server name, on the same bounded
//! drop-on-full channel as [`log_message`](super::log_message). See that module
//! and [`UpstreamNotification`] for the loss-tolerance rationale.
//!
//! [`UpstreamNotification`]: crate::lsp::bridge::actor::UpstreamNotification

use log::debug;
use serde::Deserialize;
use tower_lsp_server::ls_types::ShowMessageParams;

use super::{prefixed_message, send_window_notification};
use crate::lsp::bridge::actor::{ServerRequestDeps, UpstreamNotification};

/// Forward a `window/showMessage` notification to the editor.
pub(in crate::lsp::bridge) fn forward(
    message: &serde_json::Value,
    server_prefix: &str,
    deps: &ServerRequestDeps,
) {
    match ShowMessageParams::deserialize(&message["params"]) {
        Ok(params) => send_window_notification(
            deps,
            server_prefix,
            "window/showMessage",
            UpstreamNotification::ShowMessage {
                typ: params.typ,
                message: prefixed_message(deps.server_name.as_deref(), &params.message),
            },
        ),
        Err(e) => debug!(
            target: "kakehashi::bridge::reader",
            "{}Dropping window/showMessage with invalid params: {}",
            server_prefix,
            e
        ),
    }
}
