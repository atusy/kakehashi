//! `window/logMessage` notification forwarder.
//!
//! Inbound (downstream → editor). The live workspace threshold is checked before
//! enqueue so suppressed logs cannot consume bounded queue capacity; the shared
//! client-facing delivery boundary rechecks it for configuration races and uses
//! the same policy for kakehashi's own messages. A downstream flood cannot harm
//! the bridge because the window channel is bounded and drop-on-full (see [`UpstreamNotification`]). The text
//! is prefixed with the originating server name so output from multiple bridged
//! servers stays distinguishable.
//!
//! [`UpstreamNotification`]: crate::lsp::bridge::actor::UpstreamNotification

use log::debug;
use serde::Deserialize;
use tower_lsp_server::ls_types::LogMessageParams;

use super::{prefixed_message, send_window_notification};
use crate::lsp::bridge::actor::{ServerRequestDeps, UpstreamNotification};

#[derive(Deserialize)]
struct LogMessageType {
    #[serde(rename = "type")]
    typ: tower_lsp_server::ls_types::MessageType,
}

/// Forward a `window/logMessage` notification to the editor.
pub(in crate::lsp::bridge) fn forward(
    message: &serde_json::Value,
    server_prefix: &str,
    deps: &ServerRequestDeps,
) {
    let Ok(log_type) = LogMessageType::deserialize(&message["params"]) else {
        debug!(
            target: "kakehashi::bridge::reader",
            "{}Dropping window/logMessage with invalid params",
            server_prefix
        );
        return;
    };
    if !deps.dynamic_capabilities.allows_log_message(log_type.typ) {
        return;
    }

    // Deserialize the message text only after the cheap severity gate, so a
    // suppressed downstream flood does not allocate one String per message.
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
