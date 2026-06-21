//! `window/logMessage` notification forwarder.
//!
//! Inbound (downstream → editor). Forwarded unconditionally so the bridge stays
//! transparent: messages a direct connection would surface are not silently
//! swallowed. A downstream flood cannot harm the bridge because the window
//! channel is bounded and drop-on-full (see [`UpstreamNotification`]). The text
//! is prefixed with the originating server name so output from multiple bridged
//! servers stays distinguishable.
//!
//! [`UpstreamNotification`]: crate::lsp::bridge::actor::UpstreamNotification

use log::debug;
use serde::Deserialize;
use tower_lsp_server::ls_types::LogMessageParams;

use super::{prefixed_message, send_window_notification};
use crate::lsp::bridge::actor::{ServerRequestDeps, UpstreamNotification};

/// Forward a `window/logMessage` notification to the editor.
pub(in crate::lsp::bridge) fn forward(
    message: &serde_json::Value,
    server_prefix: &str,
    deps: &ServerRequestDeps,
) {
    // Deserialize from a reference to avoid cloning the params value.
    match LogMessageParams::deserialize(&message["params"]) {
        Ok(params) => send_window_notification(
            deps,
            server_prefix,
            "window/logMessage",
            UpstreamNotification::LogMessage {
                typ: params.typ,
                message: prefixed_message(deps.server_name.as_deref(), &params.message),
            },
        ),
        Err(e) => debug!(
            target: "kakehashi::bridge::reader",
            "{}Dropping window/logMessage with invalid params: {}",
            server_prefix,
            e
        ),
    }
}
