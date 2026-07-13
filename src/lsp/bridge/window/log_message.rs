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

use super::{prefixed_message, send_window_notification};
use crate::lsp::bridge::actor::{ServerRequestDeps, UpstreamNotification};

#[derive(Deserialize)]
struct BorrowedLogMessageParams<'a> {
    #[serde(rename = "type")]
    typ: tower_lsp_server::ls_types::MessageType,
    message: &'a str,
}

/// Forward a `window/logMessage` notification to the editor.
pub(in crate::lsp::bridge) fn forward(
    message: &serde_json::Value,
    server_prefix: &str,
    deps: &ServerRequestDeps,
) {
    let Ok(params) = BorrowedLogMessageParams::deserialize(&message["params"]) else {
        debug!(
            target: "kakehashi::bridge::reader",
            "{}Dropping window/logMessage with invalid params",
            server_prefix
        );
        return;
    };
    if !deps.dynamic_capabilities.allows_log_message(params.typ) {
        return;
    }

    send_window_notification(
        deps,
        server_prefix,
        "window/logMessage",
        UpstreamNotification::LogMessage {
            typ: params.typ,
            message: prefixed_message(deps.server_name.as_deref(), params.message),
        },
    );
}
