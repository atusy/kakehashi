//! `$/progress` notification forwarder (the data half of work-done progress).
//!
//! Inbound (downstream → editor). Although `$/progress` is `$`-namespaced, it is
//! bridged as part of `window/workDoneProgress/create`
//! ([`work_done_progress_create`](super::work_done_progress_create)) and shares
//! its [`ProgressRegistry`] token remapping, so it lives under `window/`.
//!
//! [`ProgressRegistry`]: crate::lsp::bridge::ProgressRegistry

use log::debug;
use serde::Deserialize;
use tower_lsp_server::ls_types::{ProgressParams, ProgressParamsValue, WorkDoneProgress};

use crate::lsp::bridge::actor::{ServerRequestDeps, UpstreamNotification};

/// Translate and forward a downstream `$/progress` notification to the editor.
///
/// Only progress reported against a token the downstream declared via
/// `window/workDoneProgress/create` is forwarded: its token is rewritten to the
/// bridge-minted upstream token and sent on the upstream channel. Progress for
/// any other token (e.g. a client-provided `workDoneToken`) is **dropped** — it
/// is out of scope for token remapping (see [`ProgressRegistry`] module docs),
/// and forwarding it verbatim risks duplicates under request fan-out.
///
/// A terminating `WorkDoneProgress::End` clears the mapping after forwarding.
///
/// [`ProgressRegistry`]: crate::lsp::bridge::ProgressRegistry
pub(in crate::lsp::bridge) fn forward(
    message: &serde_json::Value,
    server_prefix: &str,
    deps: &ServerRequestDeps,
) {
    // Deserialize from the borrowed `params` value (indexing yields `Null` for a
    // missing key, which fails to parse and is dropped) — no clone of the Value.
    let Ok(mut params) = ProgressParams::deserialize(&message["params"]) else {
        debug!(
            target: "kakehashi::bridge::reader",
            "{}Dropping $/progress with missing/unparseable params",
            server_prefix
        );
        return;
    };

    let Some(upstream_token) = deps
        .progress_registry
        .translate(deps.progress_connection_id, &params.token)
    else {
        // Unknown token: not a downstream-declared progress we remapped.
        debug!(
            target: "kakehashi::bridge::reader",
            "{}Dropping $/progress for unmapped token {:?}",
            server_prefix, params.token
        );
        return;
    };

    let is_end = matches!(
        &params.value,
        ProgressParamsValue::WorkDone(WorkDoneProgress::End(_))
    );

    let downstream_token = std::mem::replace(&mut params.token, upstream_token);
    let _ = deps
        .upstream_tx
        .send(UpstreamNotification::Progress { params });

    if is_end {
        deps.progress_registry
            .complete(deps.progress_connection_id, &downstream_token);
    }
}
