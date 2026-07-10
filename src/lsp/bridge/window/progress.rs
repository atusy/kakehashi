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
use crate::lsp::bridge::progress_registry::{ProgressAdmission, ProgressSignal};

/// Translate and forward a downstream `$/progress` notification to the editor.
///
/// Only progress reported against a token the downstream declared via
/// `window/workDoneProgress/create` is forwarded: its token is rewritten to the
/// bridge-minted upstream token and sent on the upstream channel. Progress for
/// any other token (e.g. a client-provided `workDoneToken`) is **dropped** — it
/// is out of scope for token remapping (see [`ProgressRegistry`] module docs),
/// and forwarding it verbatim risks duplicates under request fan-out.
///
/// Announcement is lazy: the editor-facing `window/workDoneProgress/create` is
/// enqueued here, immediately before the first renderable `begin` (same FIFO
/// channel, so create-before-progress ordering holds), while blank progress
/// lifecycles are swallowed (a later renderable `begin` reusing the token
/// still upgrades it) — see [`ProgressRegistry`] module docs for why
/// (per-request downstream progress storms).
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

    // Client-progress routing: if the token is one the bridge minted for a fanned-out request,
    // route it to that request's aggregator, which relays a single lifecycle onto the editor's
    // own `workDoneToken` and emits it ungated (ls-bridge-client-progress).
    if let Some(aggregator) = deps.client_progress_registry.route(&params.token) {
        // The incoming token identifies the source; the aggregator picks the first
        // source to `Begin` as the winner and relays only its progress. The shared
        // helper enqueues the relay under the aggregator lock so it is ordered
        // strictly before/after the teardown's terminal `End` (guards the cancel
        // race; ls-bridge-client-progress).
        crate::lsp::bridge::client_progress::relay_to_aggregator(
            &aggregator,
            &deps.upstream_tx,
            &params.token,
            params.value,
        );
        return;
    }

    let signal = match &params.value {
        ProgressParamsValue::WorkDone(WorkDoneProgress::Begin(begin)) => {
            // A begin is renderable when ANY of title/message/percentage gives
            // the editor something to show — an empty title alone is legal
            // (title is a required field, "" a legal value) and clients render
            // message/percentage without one. Only a fully blank begin (the
            // per-analysis-pass storm shape, `{"kind":"begin","title":""}`)
            // is swallowed.
            let renderable = !begin.title.is_empty()
                || begin
                    .message
                    .as_deref()
                    .is_some_and(|message| !message.is_empty())
                || begin.percentage.is_some();
            if renderable {
                ProgressSignal::RenderableBegin
            } else {
                ProgressSignal::BlankBegin
            }
        }
        _ => ProgressSignal::Other,
    };
    let is_end = matches!(
        &params.value,
        ProgressParamsValue::WorkDone(WorkDoneProgress::End(_))
    );

    let admission =
        deps.progress_registry
            .admit(deps.progress_connection_id, &params.token, signal);
    let Some(admission) = admission else {
        // Unknown token: not a downstream-declared progress we remapped.
        debug!(
            target: "kakehashi::bridge::reader",
            "{}Dropping $/progress for unmapped token {:?}",
            server_prefix, params.token
        );
        return;
    };

    match admission {
        ProgressAdmission::Announce(upstream_token) => {
            // First renderable begin: ask the editor to create the token, then
            // forward the begin. Same FIFO channel, and the forwarding loop
            // awaits the create inline, so ordering holds.
            let _ = deps
                .upstream_tx
                .send(UpstreamNotification::CreateWorkDoneProgress {
                    token: upstream_token.clone(),
                });
            params.token = upstream_token;
            let _ = deps
                .upstream_tx
                .send(UpstreamNotification::Progress { params });
        }
        ProgressAdmission::Forward(upstream_token) => {
            let downstream_token = std::mem::replace(&mut params.token, upstream_token);
            let _ = deps
                .upstream_tx
                .send(UpstreamNotification::Progress { params });
            if is_end {
                deps.progress_registry
                    .complete(deps.progress_connection_id, &downstream_token);
            }
        }
        ProgressAdmission::Drop => {
            // Swallowed lifecycle (blank begin) or report/end without a begin:
            // nothing reaches the editor. An End still clears the mapping.
            if is_end {
                deps.progress_registry
                    .complete(deps.progress_connection_id, &params.token);
            }
        }
    }
}
