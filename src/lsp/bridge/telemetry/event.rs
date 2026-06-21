//! `telemetry/event` notification forwarder.
//!
//! Inbound (downstream → editor). `telemetry/event` has no client capability
//! gating it, so it is always passed through; the raw `params` (arbitrary
//! LSPAny) is forwarded to the editor verbatim. It rides the same bounded
//! best-effort channel as `window/*` (see [`send_window_notification`]) because
//! telemetry is high-volume and loss-tolerant: dropping events under a flood is
//! preferable to unbounded memory growth or starving loss-intolerant
//! notifications.
//!
//! [`send_window_notification`]: crate::lsp::bridge::window::send_window_notification

use crate::lsp::bridge::actor::{ServerRequestDeps, UpstreamNotification};
use crate::lsp::bridge::window::send_window_notification;

/// Forward a `telemetry/event` notification to the editor.
pub(in crate::lsp::bridge) fn forward(
    message: &serde_json::Value,
    server_prefix: &str,
    deps: &ServerRequestDeps,
) {
    // `params` is arbitrary LSPAny; forward it verbatim (absent → `null`).
    let data = message
        .get("params")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    send_window_notification(
        deps,
        server_prefix,
        "telemetry/event",
        UpstreamNotification::TelemetryEvent { data },
    );
}
