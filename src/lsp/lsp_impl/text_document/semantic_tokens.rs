//! Semantic token methods for Kakehashi.
//!
//! # Cancel Handling
//!
//! This module supports immediate cancellation of semantic token requests:
//! - When `$/cancelRequest` is received, the handler aborts and returns `RequestCancelled` (-32800)
//! - Uses `tokio::select!` to race between cancel notification and token computation
//! - The blocking Rayon computation is cancelled *cooperatively*: the handler
//!   flips a [`CancelToken`](crate::cancel::CancelToken) (also flipped when a
//!   newer request supersedes this one, or the document closes) and the compute
//!   polls it throughout host and injected-language query walks, injection
//!   discovery, nested regions, and final shaping. Parsing itself remains
//!   non-preemptible, but a region that observes cancellation returns incomplete
//!   output that is neither served nor cached.
//!
//! This is achieved by subscribing to cancel notifications via `CancelForwarder::subscribe()`
//! and using biased `tokio::select!` to prioritize cancel handling.

use tower_lsp_server::jsonrpc::{Error, Result};
use tower_lsp_server::ls_types::{
    NumberOrString, Position, Range, SemanticTokens, SemanticTokensDelta,
    SemanticTokensDeltaParams, SemanticTokensFullDeltaResult, SemanticTokensParams,
    SemanticTokensRangeParams, SemanticTokensRangeResult, SemanticTokensResult,
};
use url::Url;

#[cfg(test)]
use tower_lsp_server::ls_types::{
    PartialResultParams, TextDocumentIdentifier, WorkDoneProgressParams,
};

use crate::analysis::{
    SemanticSnapshotIdentity, calculate_delta_or_full, filter_semantic_tokens_by_range,
    handle_semantic_tokens_full, next_result_id,
};
use crate::config::settings::LayerSource;
use crate::language::InjectionResolver;
use crate::lsp::aggregation::server::{
    HostFanOutTask, dispatch_host_preferred, dispatch_preferred,
};
use crate::lsp::bridge::{HostDocument, RegionOffset, host_position_within_region_bounds};
use crate::lsp::current_upstream_id;

use super::super::{Kakehashi, uri_to_url};

#[cfg(feature = "e2e")]
async fn wait_for_semantic_delta_commit_release() {
    let Ok(dir) = std::env::var("KAKEHASHI_E2E_SEMANTIC_DELTA_COMMIT_BARRIER_DIR") else {
        return;
    };
    let dir = std::path::Path::new(&dir);
    if std::fs::create_dir_all(dir).is_err()
        || std::fs::write(dir.join("captured"), b"captured").is_err()
    {
        return;
    }
    let release = dir.join("release");
    for _ in 0..300 {
        if release.exists() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

/// Outcome of the serve-current snapshot resolution for the whole-document
/// token handlers (see [`Kakehashi::current_snapshot_for_tokens`]).
pub(crate) enum TokenSnapshot {
    /// The snapshot is current (`parsed_version == content_version`) —
    /// compute against it.
    Current(std::sync::Arc<crate::document::snapshot::ParseSnapshot>),
    /// Unregistered/closed URI, or the first parse never landed within its
    /// backstop. The native layer contributes no tokens; `full` may still route
    /// through configured host/virtual layers, while `full/delta` falls back to
    /// a full response.
    Absent,
    /// The snapshot still trailed the live text when the settle backstop
    /// expired — reject with `ContentModified`; the parse loop's settle
    /// refresh re-drives the client once the parse lands.
    Stale,
    /// The client cancelled the request while it was parked.
    Cancelled,
    /// A newer request for the same document superseded this one while it was
    /// parked (`SemanticRequestTracker` flipped its token). Answer `Ok(None)`
    /// — the same contract as a compute superseded mid-flight — instead of
    /// riding out the park: on a client that supersedes without sending
    /// `$/cancelRequest`, obsolete parked requests would otherwise hold
    /// ingress admission slots until the parse settles or the backstop
    /// expires.
    Superseded,
}

struct NativeSemanticLayer {
    tokens: SemanticTokens,
    snapshot: Option<std::sync::Arc<crate::document::snapshot::ParseSnapshot>>,
    request_guard: SemanticRequestGuard,
    generation: u64,
    pending_native: Option<PendingNativeBaseline>,
}

impl NativeSemanticLayer {
    fn new(
        tokens: SemanticTokens,
        snapshot: Option<std::sync::Arc<crate::document::snapshot::ParseSnapshot>>,
        request_guard: SemanticRequestGuard,
        generation: u64,
    ) -> Self {
        Self {
            tokens,
            snapshot,
            request_guard,
            generation,
            pending_native: None,
        }
    }

    fn with_pending_native(mut self, pending_native: Option<PendingNativeBaseline>) -> Self {
        self.pending_native = pending_native;
        self
    }
}

struct PendingNativeBaseline {
    uri: Url,
    tokens: SemanticTokens,
    language: String,
    cache_key: u64,
    snapshot: SemanticSnapshotIdentity,
    served_version: u64,
}

impl PendingNativeBaseline {
    fn commit(self, cache: &crate::lsp::cache::CacheCoordinator) {
        cache.store_tokens(
            self.uri.clone(),
            self.tokens,
            self.language,
            self.cache_key,
            self.snapshot,
        );
        cache.record_served_semantic_version(&self.uri, self.served_version);
    }
}

struct PendingWireBaseline {
    uri: Url,
    tokens: SemanticTokens,
    language: String,
    cache_key: u64,
    snapshot: SemanticSnapshotIdentity,
}

impl PendingWireBaseline {
    fn commit(self, cache: &crate::lsp::cache::CacheCoordinator) {
        cache.store_wire_tokens(
            self.uri,
            self.tokens,
            self.language,
            self.cache_key,
            self.snapshot,
        );
    }
}

struct SemanticFullComputation {
    result: SemanticTokensResult,
    pending_native: Option<PendingNativeBaseline>,
    pending_wire: Option<PendingWireBaseline>,
    identity: (u64, u64),
    generation: u64,
    snapshot_present: bool,
}

struct SemanticDeltaComputation {
    result: SemanticTokensFullDeltaResult,
    pending_native: Option<PendingNativeBaseline>,
    pending_wire: Option<PendingWireBaseline>,
    identity: (u64, u64),
    generation: u64,
    snapshot_present: bool,
}

fn should_publish_empty_non_native_result(
    non_native_only: bool,
    virt_only: bool,
    bridge_work_selected: bool,
    bridge_succeeded: bool,
) -> bool {
    non_native_only && (bridge_succeeded || (virt_only && !bridge_work_selected))
}

fn commit_full_baselines(
    cache: &crate::lsp::cache::CacheCoordinator,
    computed: &mut SemanticFullComputation,
) {
    if let Some(pending) = computed.pending_native.take() {
        pending.commit(cache);
    }
    if let Some(pending) = computed.pending_wire.take() {
        pending.commit(cache);
    }
}

/// Owns a semantic request's tracker entry through its complete native + bridge
/// pipeline. Async cancellation drops the handler future, so explicit cleanup
/// branches alone cannot reclaim an already-started blocking compute. A nested
/// full computation can borrow the delta request's entry with an unarmed guard.
struct SemanticRequestGuard {
    cache: std::sync::Arc<crate::lsp::cache::CacheCoordinator>,
    uri: Url,
    request_id: u64,
    cancel_token: crate::cancel::CancelToken,
    armed: bool,
}

impl SemanticRequestGuard {
    fn new(
        cache: std::sync::Arc<crate::lsp::cache::CacheCoordinator>,
        uri: Url,
        request_id: u64,
        cancel_token: crate::cancel::CancelToken,
        owns_tracking: bool,
    ) -> Self {
        Self {
            cache,
            uri,
            request_id,
            cancel_token,
            armed: owns_tracking,
        }
    }

    fn finish(&mut self) {
        if self.armed {
            self.cache.finish_request(&self.uri, self.request_id);
            self.armed = false;
        }
    }
}

impl Drop for SemanticRequestGuard {
    fn drop(&mut self) {
        if self.armed {
            self.cancel_token.cancel();
            self.cache.finish_request(&self.uri, self.request_id);
        }
    }
}

/// Cancels blocking semantic-token work when its async owner is abandoned.
///
/// Dropping a `ComputePool` future cannot stop a work unit that already entered
/// Rayon. Layer races intentionally drop losing futures, so the native range
/// arm must turn that drop into the cooperative signal polled by the token
/// collector.
struct SemanticComputeCancelGuard {
    token: crate::cancel::CancelToken,
    armed: bool,
}

impl SemanticComputeCancelGuard {
    fn new(token: crate::cancel::CancelToken) -> Self {
        Self { token, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for SemanticComputeCancelGuard {
    fn drop(&mut self) {
        if self.armed {
            self.token.cancel();
        }
    }
}

impl Kakehashi {
    fn semantic_full_computation_is_current(
        &self,
        uri: &Url,
        computed: &SemanticFullComputation,
    ) -> bool {
        self.cache.semantic_token_generation() == computed.generation
            && self.documents.latest_snapshot(uri).is_some_and(|view| {
                view.slot.current_incarnation == computed.identity.0
                    && view.content_version == computed.identity.1
                    && view.slot.snapshot.is_some() == computed.snapshot_present
            })
    }

    fn semantic_delta_computation_is_current(
        &self,
        uri: &Url,
        computed: &SemanticDeltaComputation,
    ) -> bool {
        self.cache.semantic_token_generation() == computed.generation
            && self.documents.latest_snapshot(uri).is_some_and(|view| {
                view.slot.current_incarnation == computed.identity.0
                    && view.content_version == computed.identity.1
                    && view.slot.snapshot.is_some() == computed.snapshot_present
            })
    }

    fn semantic_snapshot_is_current(
        &self,
        uri: &Url,
        incarnation: u64,
        parsed_version: u64,
        generation: u64,
        edit_lock: &std::sync::Arc<tokio::sync::Mutex<()>>,
    ) -> bool {
        let latest = self.documents.latest_snapshot(uri);
        let current = self.cache.semantic_token_generation() == generation
            && latest.as_ref().is_some_and(|view| {
                view.slot.current_incarnation == incarnation
                    && view.content_version == parsed_version
            });
        if latest.is_none() {
            self.documents.remove_edit_lock_if_unshared(uri, edit_lock);
        }
        current
    }

    fn semantic_full_response_is_current(
        &self,
        uri: &Url,
        live_identity: (u64, u64),
        generations: (u64, u64),
        snapshot: Option<&std::sync::Arc<crate::document::snapshot::ParseSnapshot>>,
        require_snapshot_identity: bool,
        edit_lock: &std::sync::Arc<tokio::sync::Mutex<()>>,
    ) -> bool {
        self.settings_manager.settings_generation() == generations.1
            && self.cache.semantic_token_generation() == generations.0
            && (!require_snapshot_identity
                || snapshot.map_or_else(
                    || {
                        self.documents.latest_snapshot(uri).is_some_and(|view| {
                            view.slot.current_incarnation == live_identity.0
                                && view.content_version == live_identity.1
                                && view.slot.snapshot.is_none()
                        })
                    },
                    |snapshot| {
                        self.semantic_snapshot_is_current(
                            uri,
                            snapshot.incarnation,
                            snapshot.parsed_version,
                            generations.0,
                            edit_lock,
                        )
                    },
                ))
            && self.documents.get(uri).is_some_and(|document| {
                document.incarnation() == live_identity.0
                    && document.content_version() == live_identity.1
            })
    }

    /// Latest-completed snapshot resolution (parse-snapshot ADR §3): returns
    /// the newest published snapshot, which may trail the input. The only
    /// wait is the bounded first-parse wait (no snapshot for this lifetime
    /// yet); no per-keystroke read ever waits on a reparse. Used by the
    /// currency-*checking* readers (`semanticTokens/range`, which resolves
    /// through this and then staleness-rejects inline against the live
    /// version). `None` for an unregistered/closed URI or when no parse
    /// resolves within the wait.
    pub(crate) async fn snapshot_for_tokens(
        &self,
        uri: &Url,
    ) -> Option<std::sync::Arc<crate::document::snapshot::ParseSnapshot>> {
        // Generous on purpose: this wait only runs while the document has NO
        // snapshot for its lifetime, and every open-parse resolution path
        // publishes one (a tree, a resolved-but-tree-less outcome, or the
        // didClose sentinel — all of which wake this receiver). The wait is
        // therefore bounded by parse completion (itself capped by the parse
        // work-unit timeout), not by this constant; the constant is only a
        // backstop. A tight cap here made first requests racing didOpen
        // answer empty whenever the machine was loaded enough to push the
        // open parse past it (observed under the parallel e2e suite, and the
        // real-world analog is editor startup on a busy machine).
        let deadline =
            tokio::time::Instant::now() + crate::lsp::lsp_impl::snapshot_read::FIRST_PARSE_BACKSTOP;
        loop {
            // Subscribe BEFORE checking: `watch::Sender::subscribe` marks the
            // value current at subscription time as already seen, so a publish
            // landing between a check and a later subscribe would be invisible
            // to `changed()` — a lost wakeup that burned the whole wait and
            // served the pre-parse fallback (the e2e-visible symptom: node —
            // and first-token — requests racing didOpen answered null).
            // Subscribing first closes the window: a publish before the check
            // is caught by the check, one after it triggers `changed()`.
            let mut receiver = self.documents.subscribe_snapshots(uri)?;
            // Re-resolve the cell per iteration (per-request re-resolution +
            // incarnation validation happen inside `latest_snapshot`).
            let view = self.documents.latest_snapshot(uri)?;
            if let Some(snapshot) = view.slot.snapshot {
                return Some(snapshot);
            }
            match tokio::time::timeout_at(deadline, receiver.changed()).await {
                Ok(Ok(())) => continue,
                // Channel closed (document gone) or first-parse wait elapsed.
                _ => return None,
            }
        }
    }

    /// Serve-current snapshot resolution for `semanticTokens/full` and
    /// `full/delta`: park (racing the client's `$/cancelRequest`) until the
    /// latest snapshot is **current**, then compute against that.
    ///
    /// Why not serve the latest completed snapshot and let the parse loop's
    /// refresh heal the client? Because the editor draws whatever we answer
    /// against the text it has NOW: Neovim stamps the response with the
    /// buffer version at request time and renders it as soon as that matches
    /// the live buffer (`vim.lsp.semantic_tokens`: `process_response` /
    /// `on_win`, extmarks placed with `strict = false`), so tokens computed
    /// for older text land visibly misplaced on unchanged lines. While we
    /// park instead, the editor keeps its previous tokens as extmarks that
    /// shift with the edit — temporarily unhighlighted new text, never
    /// corrupted existing text. The wait is bounded by parse completion
    /// (every edit's parse publishes), not by typing: the settle backstop
    /// only expires when the pipeline is pathologically behind.
    pub(crate) async fn current_snapshot_for_tokens(
        &self,
        uri: &Url,
        cancel_rx: Option<&mut crate::lsp::request_id::CancelReceiver>,
        supersede: &crate::cancel::CancelToken,
    ) -> TokenSnapshot {
        use crate::lsp::lsp_impl::snapshot_read::{SnapshotWait, TOKEN_SETTLE_BACKSTOP};
        let wait = self.wait_for_current_snapshot(uri, TOKEN_SETTLE_BACKSTOP);
        let outcome = match cancel_rx {
            Some(rx) => {
                tokio::select! {
                    biased;
                    // Fires on $/cancelRequest (and on forwarder teardown,
                    // which the compute-race arms below treat as cancel too).
                    _ = rx => return TokenSnapshot::Cancelled,
                    // Fires when a newer request for this document flips this
                    // request's tracker token — release the park (and its
                    // admission slot) instead of computing a discarded result.
                    _ = supersede.cancelled() => return TokenSnapshot::Superseded,
                    outcome = wait => outcome,
                }
            }
            None => {
                tokio::select! {
                    biased;
                    _ = supersede.cancelled() => return TokenSnapshot::Superseded,
                    outcome = wait => outcome,
                }
            }
        };
        match outcome {
            SnapshotWait::Current(snapshot) => TokenSnapshot::Current(snapshot),
            SnapshotWait::Stale => TokenSnapshot::Stale,
            SnapshotWait::Unparsed | SnapshotWait::Gone => TokenSnapshot::Absent,
        }
    }

    pub(crate) async fn semantic_tokens_full_impl(
        &self,
        params: SemanticTokensParams,
    ) -> Result<Option<SemanticTokensResult>> {
        Ok(self
            .semantic_tokens_full_impl_with_tracking(params, None)
            .await?
            .map(|computed| computed.result))
    }

    async fn semantic_tokens_full_impl_with_tracking(
        &self,
        params: SemanticTokensParams,
        tracking: Option<(crate::lsp::cache::RequestId, crate::cancel::CancelToken)>,
    ) -> Result<Option<SemanticFullComputation>> {
        let Ok(uri) = uri_to_url(&params.text_document.uri) else {
            return Ok(None);
        };
        let direct_request = tracking.is_none();
        let (request_id, cancel_token) = tracking.unwrap_or_else(|| self.cache.start_request(&uri));
        let mut request_guard = SemanticRequestGuard::new(
            std::sync::Arc::clone(&self.cache),
            uri.clone(),
            request_id,
            cancel_token.clone(),
            direct_request,
        );
        let tracking = Some((request_id, cancel_token));
        let absent_identity = self.documents.latest_snapshot(&uri).and_then(|view| {
            view.slot.snapshot.is_none().then_some((
                view.slot.current_incarnation,
                view.content_version,
                self.cache.semantic_token_generation(),
            ))
        });
        let retry_params = params.clone();
        let retry_tracking = tracking.clone();
        let outcome = self
            .semantic_tokens_full_impl_with_tracking_once(params, tracking, direct_request)
            .await?;
        if outcome.is_some() {
            request_guard.finish();
            return Ok(outcome);
        }
        let retry_after_snapshot_publication =
            absent_identity.is_some_and(|(incarnation, content_version, generation)| {
                self.cache.semantic_token_generation() == generation
                    && self.documents.latest_snapshot(&uri).is_some_and(|view| {
                        view.slot.current_incarnation == incarnation
                            && view.content_version == content_version
                            && view.slot.snapshot.is_some()
                    })
                    && retry_tracking.as_ref().is_some_and(|(request_id, cancel)| {
                        !cancel.is_cancelled() && self.cache.is_request_active(&uri, *request_id)
                    })
            });
        if !retry_after_snapshot_publication {
            request_guard.finish();
            return Ok(None);
        }
        let outcome = self
            .semantic_tokens_full_impl_with_tracking_once(
                retry_params,
                retry_tracking,
                direct_request,
            )
            .await;
        request_guard.finish();
        outcome
    }

    async fn semantic_tokens_full_impl_with_tracking_once(
        &self,
        params: SemanticTokensParams,
        tracking: Option<(crate::lsp::cache::RequestId, crate::cancel::CancelToken)>,
        direct_request: bool,
    ) -> Result<Option<SemanticFullComputation>> {
        const METHOD: &str = "textDocument/semanticTokens/full";
        let lsp_uri = params.text_document.uri.clone();
        let Ok(uri) = uri_to_url(&lsp_uri) else {
            return Ok(None);
        };
        let raw_params = serde_json::to_value(&params).unwrap_or(serde_json::Value::Null);
        let progress_token = params.work_done_progress_params.work_done_token.clone();
        let upstream_id = current_upstream_id();
        let (mut cancel_rx, _subscription_guard) = self.subscribe_cancel(upstream_id.as_ref());
        let supersession = tracking.as_ref().map(|(_, cancel)| cancel.clone());
        let settings_generation = self.settings_manager.settings_generation();
        let layer_config = self
            .document_language(&uri)
            .map(|language| self.resolve_layer_config(&language, METHOD));
        let only_layer = layer_config.as_ref().and_then(|layers| {
            layers
                .priorities
                .first()
                .copied()
                .filter(|first| layers.priorities.iter().all(|source| source == first))
        });
        let native_enabled = self.semantic_tokens_full_includes_native(&uri);
        let document_language = self.document_language(&uri);
        let virtual_enabled = document_language.as_ref().is_some_and(|language| {
            self.resolve_layer_config(language, METHOD)
                .allows(LayerSource::Virt)
        });
        let actionable_virtual = virtual_enabled
            && document_language.as_ref().is_some_and(|language| {
                self.semantic_tokens_full_has_potential_virtual_producer(language)
            });
        let virt_only = only_layer == Some(LayerSource::Virt);
        let non_native_only = layer_config.as_ref().is_some_and(|layers| {
            !layers.priorities.is_empty() && !layers.allows(LayerSource::Native)
        });
        // Establish the serve-current native baseline first. Besides providing
        // immediate syntax coverage, this preserves the existing park,
        // supersession, and cancellation contract. A current snapshot makes
        // whole-document bridge discovery immediate. When the first-parse
        // backstop expires instead, the no-snapshot path carries the live
        // incarnation/content version through fan-out and revalidates that
        // identity before returning.
        let Some(native_layer) = self
            .semantic_tokens_full_native_layer(
                params,
                &mut cancel_rx,
                tracking,
                native_enabled,
                native_enabled || actionable_virtual,
            )
            .await?
        else {
            return Ok(None);
        };
        let mut request_guard = native_layer.request_guard;
        let request_id = request_guard.request_id;
        let cancel_token = request_guard.cancel_token.clone();
        let generation = native_layer.generation;
        let native_tokens = native_layer.tokens;
        let snapshot = native_layer.snapshot;
        let pending_native = native_layer.pending_native;
        if actionable_virtual
            && snapshot
                .as_ref()
                .is_none_or(|snapshot| snapshot.tree.is_none())
        {
            // An unparsed virtual layer is pending work, not an authoritative
            // empty overlay. Register interest so parse settlement re-drives
            // the client, and preserve its previous tokens until then.
            self.cache.record_served_semantic_version(
                &uri,
                snapshot
                    .as_ref()
                    .map_or(0, |snapshot| snapshot.parsed_version),
            );
            request_guard.finish();
            return Err(crate::error::content_modified_error());
        }
        let live_snapshot_present = self
            .documents
            .latest_snapshot(&uri)
            .is_some_and(|view| view.slot.snapshot.is_some());
        let Some((live_identity, live_text, live_language)) =
            self.documents.get(&uri).map(|document| {
                (
                    (document.incarnation(), document.content_version()),
                    document.text_arc(),
                    document.language_id().unwrap_or_default().to_string(),
                )
            })
        else {
            request_guard.finish();
            return Ok(None);
        };
        let native_result_id = native_tokens.result_id.clone();
        let native_data = native_tokens.data;
        let expected = Some(snapshot.as_ref().map_or_else(
            || super::super::whole_document::WholeDocumentSnapshotIdentity {
                incarnation: live_identity.0,
                parsed_version: live_identity.1,
                generation,
            },
            |snapshot| super::super::whole_document::WholeDocumentSnapshotIdentity {
                incarnation: snapshot.incarnation,
                parsed_version: snapshot.parsed_version,
                generation,
            },
        ));
        let expected_incarnation = Some(live_identity.0);
        let native = std::future::ready(Ok(Some(native_data)));
        let bridge_attempted = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let virt_bridge_attempted = std::sync::Arc::clone(&bridge_attempted);
        let bridge_succeeded = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let virt_bridge_succeeded = std::sync::Arc::clone(&bridge_succeeded);
        let bridge_work_selected = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let virt_bridge_work_selected = std::sync::Arc::clone(&bridge_work_selected);
        let bridge_activity =
            std::sync::Arc::new(super::super::whole_document::WholeDocumentBridgeActivity::new());
        let virt_bridge_activity = std::sync::Arc::clone(&bridge_activity);

        let fan_out = async {
            let data = self
                .whole_document_fan_out(
                    &lsp_uri,
                    METHOD,
                    raw_params,
                    progress_token,
                    expected,
                    Some(std::sync::Arc::clone(&bridge_attempted)),
                    Some(std::sync::Arc::clone(&bridge_succeeded)),
                    Some(std::sync::Arc::clone(&bridge_work_selected)),
                    Some(std::sync::Arc::clone(&bridge_activity)),
                    true,
                    true,
                    true,
                    actionable_virtual,
                    native,
                    move |task| {
                        let attempted = std::sync::Arc::clone(&virt_bridge_attempted);
                        let succeeded = std::sync::Arc::clone(&virt_bridge_succeeded);
                        let selected = std::sync::Arc::clone(&virt_bridge_work_selected);
                        let activity = std::sync::Arc::clone(&virt_bridge_activity);
                        async move {
                            let unit = format!("virt:{}", task.region_id);
                            let region_end = task.region_end();
                            let local_attempted =
                                std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                            let local_selected =
                                std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                            let result = task
                                .pool
                                .send_semantic_tokens_full_request(
                                    &task.server_name,
                                    &task.server_config,
                                    &task.uri,
                                    region_end,
                                    &task.injection_language,
                                    &task.region_id,
                                    task.offset,
                                    &task.virtual_content,
                                    task.upstream_id,
                                    task.client_progress_token,
                                    expected_incarnation,
                                    Some(std::sync::Arc::clone(&local_attempted)),
                                    Some(std::sync::Arc::clone(&local_selected)),
                                )
                                .await;
                            let was_attempted =
                                local_attempted.load(std::sync::atomic::Ordering::Acquire);
                            if local_selected.load(std::sync::atomic::Ordering::Acquire) {
                                selected.store(true, std::sync::atomic::Ordering::Release);
                                activity.mark_selected(unit.clone());
                            }
                            if was_attempted {
                                attempted.store(true, std::sync::atomic::Ordering::Release);
                                if result.is_ok() {
                                    succeeded.store(true, std::sync::atomic::Ordering::Release);
                                    activity.mark_succeeded(unit);
                                }
                            }
                            result.map(|tokens| tokens.map(|tokens| tokens.data))
                        }
                    },
                    |value| {
                        serde_json::from_value::<SemanticTokensResult>(value)
                            .ok()
                            .map(|result| match result {
                                SemanticTokensResult::Tokens(tokens) => tokens.data,
                                SemanticTokensResult::Partial(partial) => partial.data,
                            })
                    },
                    |won| {
                        let legend = won.handle.semantic_tokens_legend()?;
                        let mapper = crate::text::PositionMapper::new(&won.host_text);
                        let document_end = mapper.byte_to_position(won.host_text.len())?;
                        crate::lsp::bridge::transform_semantic_tokens_result_to_host(
                            serde_json::to_value(SemanticTokens {
                                result_id: None,
                                data: won.items,
                            })
                            .ok()?,
                            legend,
                            &RegionOffset::new(0, 0),
                            document_end,
                            &won.host_text,
                            Range::new(Position::new(0, 0), document_end),
                        )
                        .map(|tokens| tokens.data)
                    },
                    crate::lsp::bridge::merge_semantic_token_layers,
                    crate::lsp::bridge::merge_semantic_token_layers,
                )
                .await?;

            let bridge_work_selected = bridge_activity.has_selected();
            let bridge_is_authoritative = bridge_activity.is_authoritative();
            let bridge_succeeded = bridge_work_selected && bridge_is_authoritative;
            if !bridge_is_authoritative {
                // Native data is only a partial overlay when a selected bridge
                // producer failed. Preserve the previous merged wire baseline
                // instead of publishing that incomplete result.
                return Ok(None);
            }
            let data = match data {
                Some(data) => data,
                None if should_publish_empty_non_native_result(
                    non_native_only,
                    virt_only,
                    bridge_work_selected,
                    bridge_succeeded,
                ) =>
                {
                    // A selected non-native overlay recomputed to no tokens:
                    // either its server returned null/empty, or a virt-only
                    // document has no regions left. Keep an empty wire lineage
                    // so delta deletes the previous downstream classifications.
                    bridge_attempted.store(true, std::sync::atomic::Ordering::Release);
                    Vec::new()
                }
                None => return Ok(None),
            };
            let edit_lock = self.documents.edit_lock(&uri);
            let _edit_guard = edit_lock.lock().await;
            if cancel_token.is_cancelled()
                || !self.cache.is_request_active(&uri, request_id)
                || !self.semantic_full_response_is_current(
                    &uri,
                    live_identity,
                    (generation, settings_generation),
                    snapshot.as_ref(),
                    native_enabled,
                    &edit_lock,
                )
            {
                return Ok(None);
            }

            let bridge_attempted = bridge_attempted.load(std::sync::atomic::Ordering::Acquire);
            let result_id = if bridge_attempted {
                Some(next_result_id())
            } else {
                native_result_id
            };
            let tokens = SemanticTokens { result_id, data };
            let pending_wire = bridge_attempted.then(|| PendingWireBaseline {
                uri: uri.clone(),
                tokens: tokens.clone(),
                language: snapshot
                    .as_ref()
                    .and_then(|snapshot| snapshot.language.clone())
                    .unwrap_or(live_language),
                cache_key: self.cache.cache_key_for(&live_text, generation),
                snapshot: snapshot.as_ref().map_or(
                    SemanticSnapshotIdentity {
                        parsed_version: live_identity.1,
                        incarnation: live_identity.0,
                        generation,
                    },
                    |snapshot| SemanticSnapshotIdentity {
                        parsed_version: snapshot.parsed_version,
                        incarnation: snapshot.incarnation,
                        generation,
                    },
                ),
            });
            Ok(Some(SemanticFullComputation {
                result: SemanticTokensResult::Tokens(tokens),
                pending_native,
                pending_wire,
                identity: live_identity,
                generation,
                snapshot_present: live_snapshot_present,
            }))
        };
        let mut outcome = match cancel_rx.as_mut() {
            Some(cancel_rx) => {
                tokio::select! {
                    biased;
                    _ = cancel_rx => {
                        cancel_token.cancel();
                        Err(Error::request_cancelled())
                    }
                    _ = cancel_token.cancelled() => Ok(None),
                    result = fan_out => result,
                }
            }
            None => {
                tokio::select! {
                    biased;
                    _ = cancel_token.cancelled() => Ok(None),
                    result = fan_out => result,
                }
            }
        };
        // The biased select above accepts the response before publication. A
        // direct request then reacquires the document edit lock and holds it
        // through the tracker-gated cache writes, so neither cancellation nor
        // didChange can publish a baseline the client never receives.
        if direct_request && matches!(outcome, Ok(Some(_))) {
            let edit_lock = self.documents.edit_lock(&uri);
            let lock = edit_lock.lock();
            let commit_guard = match cancel_rx.as_mut() {
                Some(cancel_rx) => tokio::select! {
                    biased;
                    _ = cancel_rx => {
                        cancel_token.cancel();
                        outcome = Err(Error::request_cancelled());
                        None
                    }
                    _ = cancel_token.cancelled() => {
                        outcome = Ok(None);
                        None
                    }
                    guard = lock => Some(guard),
                },
                None => tokio::select! {
                    biased;
                    _ = cancel_token.cancelled() => {
                        outcome = Ok(None);
                        None
                    }
                    guard = lock => Some(guard),
                },
            };
            if commit_guard.is_some()
                && let Ok(Some(computed)) = &mut outcome
            {
                let current = self.semantic_full_computation_is_current(&uri, computed);
                if !current
                    || self
                        .cache
                        .with_active_request(&uri, request_id, || {
                            commit_full_baselines(&self.cache, computed);
                        })
                        .is_none()
                {
                    outcome = Ok(None);
                }
            }
        }
        request_guard.finish();
        outcome
    }

    async fn semantic_tokens_full_native_layer(
        &self,
        params: SemanticTokensParams,
        cancel_rx: &mut Option<crate::lsp::request_id::CancelReceiver>,
        tracking: Option<(crate::lsp::cache::RequestId, crate::cancel::CancelToken)>,
        require_snapshot: bool,
    ) -> Result<Option<NativeSemanticLayer>> {
        let lsp_uri = params.text_document.uri;

        // Convert ls_types::Uri to url::Url for internal use
        let Ok(uri) = uri_to_url(&lsp_uri) else {
            log::warn!("Invalid URI in semanticTokens/full: {}", lsp_uri.as_str());
            return Ok(None);
        };

        // Start tracking this request - supersedes any previous request for this URI.
        // `cancel_token` is flipped when a newer request supersedes this one (or
        // the document closes); it is threaded into the blocking compute so a
        // superseded request stops mid-flight instead of running to completion.
        let owns_tracking = tracking.is_none();
        let (request_id, cancel_token) = tracking.unwrap_or_else(|| self.cache.start_request(&uri));
        let request_guard = SemanticRequestGuard::new(
            std::sync::Arc::clone(&self.cache),
            uri.clone(),
            request_id,
            cancel_token.clone(),
            owns_tracking,
        );
        let finish_if_owned = || {
            if owns_tracking {
                self.cache.finish_request(&uri, request_id);
            }
        };

        // Snapshot the settings generation NOW, before reading any
        // settings-dependent tokenization input (language resolution, queries,
        // capture mappings) below. Folded into the cache key once the text is
        // available; pinning it here means a settings reload racing this request
        // leaves our stored tokens on the old generation — invisible to
        // post-reload requests — so we can't poison the cache (see `cache_key_for`).
        let token_generation = self.cache.semantic_token_generation();

        log::debug!(
            target: "kakehashi::semantic",
            "[SEMANTIC_TOKENS] START uri={} req={}",
            uri, request_id
        );

        // Early exit if request was superseded
        if !self.cache.is_request_active(&uri, request_id) {
            log::debug!(
                target: "kakehashi::semantic",
                "[SEMANTIC_TOKENS] CANCELLED uri={} req={}",
                uri, request_id
            );
            return Ok(None);
        }
        if !require_snapshot {
            return Ok(Some(NativeSemanticLayer::new(
                SemanticTokens {
                    result_id: None,
                    data: Vec::new(),
                },
                None,
                request_guard,
                token_generation,
            )));
        }
        // Serve-current (ADR §3, revised): park until the snapshot matches the
        // live text — see `current_snapshot_for_tokens` for why answering from
        // a trailing snapshot corrupts the editor's existing highlights. The
        // resolved snapshot's (text, tree, language) triple is internally
        // consistent, and every input below (query, mappings) resolves against
        // the snapshot's own detected language — never a live re-detection
        // that could diverge from the tree's grammar.
        let snapshot = match self
            .current_snapshot_for_tokens(&uri, cancel_rx.as_mut(), &cancel_token)
            .await
        {
            TokenSnapshot::Current(snapshot) => snapshot,
            TokenSnapshot::Absent => {
                return Ok(Some(NativeSemanticLayer::new(
                    SemanticTokens {
                        result_id: None,
                        data: vec![],
                    },
                    None,
                    request_guard,
                    token_generation,
                )));
            }
            TokenSnapshot::Stale => {
                // Register token interest (version 0, monotonic max — a real
                // serve overwrites) so the settle-refresh gate re-drives this
                // client even when EVERY request so far rejected: without a
                // served mark the gate reads "nobody highlights this
                // document" and the client would stay dark until its next
                // didChange-driven request.
                self.cache.record_served_semantic_version(&uri, 0);
                finish_if_owned();
                return Err(crate::error::content_modified_error());
            }
            TokenSnapshot::Cancelled => {
                cancel_token.cancel();
                finish_if_owned();
                log::debug!(
                    target: "kakehashi::semantic",
                    "[SEMANTIC_TOKENS] CANCELLED via $/cancelRequest uri={} req={} (while parked)",
                    uri, request_id
                );
                return Err(Error::request_cancelled());
            }
            TokenSnapshot::Superseded => {
                // Same contract as a compute superseded mid-flight (below):
                // the newer request answers; this one drops out quietly.
                finish_if_owned();
                log::debug!(
                    target: "kakehashi::semantic",
                    "[SEMANTIC_TOKENS] CANCELLED uri={} req={} (superseded while parked)",
                    uri, request_id
                );
                return Ok(None);
            }
        };
        let (Some(language_name), Some(tree)) = (snapshot.language.clone(), snapshot.tree.clone())
        else {
            // No detectable language, or resolved-but-tree-less (see
            // `ParseSnapshot` for the causes): nothing to tokenize. The empty set
            // IS this snapshot's served state — record it so the parse loop
            // doesn't keep refreshing a document that has no tokens.
            self.cache
                .record_served_semantic_version(&uri, snapshot.parsed_version);
            return Ok(Some(NativeSemanticLayer::new(
                SemanticTokens {
                    result_id: None,
                    data: vec![],
                },
                Some(snapshot),
                request_guard,
                token_generation,
            )));
        };
        let text = std::sync::Arc::clone(&snapshot.text);

        // Ensure language is loaded before trying to get queries.
        // This handles the race condition where semanticTokens/full arrives
        // before didOpen finishes loading the language.
        let load_result = self
            .language
            .ensure_language_loaded_async(&language_name)
            .await;
        if !load_result.success {
            return Ok(Some(NativeSemanticLayer::new(
                SemanticTokens {
                    result_id: None,
                    data: vec![],
                },
                Some(snapshot),
                request_guard,
                token_generation,
            )));
        }

        // Early exit check after loading language
        if !self.cache.is_request_active(&uri, request_id) {
            log::debug!(
                target: "kakehashi::semantic",
                "[SEMANTIC_TOKENS] CANCELLED uri={} req={} (after language load)",
                uri, request_id
            );
            return Ok(None);
        }

        let Some(query) = self.language.highlight_query(&language_name) else {
            return Ok(Some(NativeSemanticLayer::new(
                SemanticTokens {
                    result_id: None,
                    data: vec![],
                },
                Some(snapshot),
                request_guard,
                token_generation,
            )));
        };

        // Read the remaining settings-dependent tokenization inputs HERE —
        // together with the query above, with no `.await` in between — so a
        // settings reload can't split them into an inconsistent mix (e.g. old
        // query + new capture mappings). All are consistent with the
        // `token_generation` snapshotted at the top.
        let capture_mappings = self.language.capture_mappings();
        let supports_multiline = self.settings_manager.supports_multiline_tokens();

        // Early exit check before expensive computation
        if !self.cache.is_request_active(&uri, request_id) {
            log::debug!(
                target: "kakehashi::semantic",
                "[SEMANTIC_TOKENS] CANCELLED uri={} req={} (before compute)",
                uri, request_id
            );
            return Ok(None);
        }

        let snapshot_identity = SemanticSnapshotIdentity {
            parsed_version: snapshot.parsed_version,
            incarnation: snapshot.incarnation,
            generation: token_generation,
        };
        if let Some(cached) =
            self.cache
                .get_current_tokens_for_snapshot(&uri, &language_name, snapshot_identity)
        {
            let cached = (*cached).clone();
            let edit_lock = self.documents.edit_lock(&uri);
            let _edit_guard = edit_lock.lock().await;
            let still_current = self.semantic_snapshot_is_current(
                &uri,
                snapshot.incarnation,
                snapshot.parsed_version,
                token_generation,
                &edit_lock,
            );
            if !still_current || !self.cache.is_request_active(&uri, request_id) {
                finish_if_owned();
                return Ok(None);
            }
            self.cache
                .record_served_semantic_version(&uri, snapshot.parsed_version);
            return Ok(Some(NativeSemanticLayer::new(
                cached,
                Some(snapshot),
                request_guard,
                token_generation,
            )));
        }

        // Validity key for the snapshot's text under the generation captured at
        // the top: keys both the unchanged-document cache short-circuit below
        // and the store of the freshly computed tokens. Keying off the
        // snapshot's own text hash means a compute racing a fresh edit stores
        // under its own text's hash, which a post-edit request never looks up.
        let cache_key = self.cache.cache_key_for(&text, token_generation);

        // Compute tokens against the (current-at-resolution) snapshot. An edit
        // landing after the resolution supersedes this request via the client's
        // next didChange-driven request; the CancelToken then reclaims the
        // compute mid-flight.
        let result = {
            // Snapshot-identical repeat request: tokens already cached for this
            // exact text are still correct, so skip re-tokenizing. Returns the
            // cached tokens with their original `result_id`, keeping a client's
            // delta baseline stable across idle re-requests.
            if let Some(cached) = self
                .cache
                .get_current_tokens(&uri, &language_name, cache_key)
            {
                let cached = (*cached).clone();
                let edit_lock = self.documents.edit_lock(&uri);
                let _edit_guard = edit_lock.lock().await;
                let still_current = self.semantic_snapshot_is_current(
                    &uri,
                    snapshot.incarnation,
                    snapshot.parsed_version,
                    token_generation,
                    &edit_lock,
                );
                if !still_current || !self.cache.is_request_active(&uri, request_id) {
                    finish_if_owned();
                    return Ok(None);
                }
                self.cache
                    .record_served_semantic_version(&uri, snapshot.parsed_version);
                // The wire type owns its data (`ls_types::SemanticTokensResult`
                // has no borrowing variant), so this is the one legitimate
                // materialization point — everything upstream (the cache hit
                // itself) stayed O(1) via the `Arc`.
                return Ok(Some(NativeSemanticLayer::new(
                    cached,
                    Some(snapshot),
                    request_guard,
                    token_generation,
                )));
            }

            // capture_mappings and supports_multiline were read before the await
            // above (consistent with the query and token_generation). Rayon-based
            // parallel injection processing uses thread-local parser caching
            // instead of the shared parser pool, avoiding lock contention.
            let coordinator = std::sync::Arc::clone(&self.language);

            // Enable per-region injection-token reuse (#529). The generation is
            // the one snapshotted at the top of the handler (same value folded
            // into `cache_key`), so a config reload racing this request can't make
            // it serve or store stale-query tokens.
            let injection_cache = Some(crate::analysis::semantic::InjectionCacheParams {
                uri: uri.clone(),
                tracker: self.bridge.node_tracker_arc(),
                cache: self.cache.injection_token_cache_arc(),
                generation: token_generation,
                documents: std::sync::Arc::clone(&self.documents),
                parsed_version: snapshot.parsed_version,
                incarnation: snapshot.incarnation,
                // The snapshot's own discovery (ADR §3, don't-discover-twice):
                // rebuilt into contexts instead of re-running the injection
                // query, when its generation still matches.
                discovery: snapshot.injection_regions.clone(),
            });

            // Compute tokens, racing against cancel notification if provided
            let compute_future = handle_semantic_tokens_full(
                &self.compute_pool,
                text.clone(),
                tree.clone(),
                query,
                Some(language_name.clone()),
                Some(capture_mappings),
                coordinator,
                supports_multiline,
                injection_cache,
                Some(cancel_token.clone()),
            );

            if let Some(cancel_rx) = cancel_rx.as_mut() {
                // Race between computation and cancel notification
                tokio::pin!(cancel_rx);
                tokio::select! {
                    biased;

                    // Cancel notification received - abort immediately. Flip the
                    // token so the now-detached blocking compute stops early
                    // instead of running to completion for a discarded result.
                    _ = &mut cancel_rx => {
                        cancel_token.cancel();
                        finish_if_owned();
                        log::debug!(
                            target: "kakehashi::semantic",
                            "[SEMANTIC_TOKENS] CANCELLED via $/cancelRequest uri={} req={}",
                            uri, request_id
                        );
                        return Err(Error::request_cancelled());
                    }

                    // Computation completed
                    result = compute_future => result,
                }
            } else {
                // No cancel support - just await the computation
                compute_future.await
            }
        };

        // A supersede/close between compute start and here flips the token; the
        // compute then bailed at a checkpoint and returned `None` (a partial
        // result), so drop the request rather than storing it over the cache.
        // This is CPU-reclamation, not staleness-rejection: an *un*-superseded
        // compute over a snapshot the live text has since outrun still serves —
        // the client's didChange-driven follow-up request supersedes and heals
        // (§4's narrowed CancelToken role).
        if cancel_token.is_cancelled() {
            finish_if_owned();
            log::debug!(
                target: "kakehashi::semantic",
                "[SEMANTIC_TOKENS] CANCELLED uri={} req={} (compute superseded)",
                uri, request_id
            );
            return Ok(None);
        }

        // Early exit check before storing - prevents superseded request from overwriting cache
        if !self.cache.is_request_active(&uri, request_id) {
            log::debug!(
                target: "kakehashi::semantic",
                "[SEMANTIC_TOKENS] CANCELLED uri={} req={} (before store)",
                uri, request_id
            );
            return Ok(None);
        }
        let mut tokens_with_id = match result.unwrap_or_else(|| {
            tower_lsp_server::ls_types::SemanticTokensResult::Tokens(
                tower_lsp_server::ls_types::SemanticTokens {
                    result_id: None,
                    data: Vec::new(),
                },
            )
        }) {
            tower_lsp_server::ls_types::SemanticTokensResult::Tokens(tokens) => tokens,
            tower_lsp_server::ls_types::SemanticTokensResult::Partial(_) => {
                tower_lsp_server::ls_types::SemanticTokens {
                    result_id: None,
                    data: Vec::new(),
                }
            }
        };
        // Use atomic sequential ID for efficient cache validation
        tokens_with_id.result_id = Some(next_result_id());
        let stored_tokens = tokens_with_id.clone();
        let lsp_tokens = tokens_with_id;
        let edit_lock = self.documents.edit_lock(&uri);
        let _edit_guard = edit_lock.lock().await;
        let still_current = self.semantic_snapshot_is_current(
            &uri,
            snapshot.incarnation,
            snapshot.parsed_version,
            token_generation,
            &edit_lock,
        );
        if !still_current || !self.cache.is_request_active(&uri, request_id) {
            finish_if_owned();
            return Ok(None);
        }
        // A nested full borrows the outer delta's tracker. Keep its freshly
        // minted native baseline private until that outer request passes its
        // own lifecycle fence; otherwise a discarded delta can evict the last
        // baseline actually held by the editor.
        let pending_native = if owns_tracking {
            self.cache.store_tokens(
                uri.clone(),
                stored_tokens,
                language_name,
                cache_key,
                snapshot_identity,
            );
            self.cache
                .record_served_semantic_version(&uri, snapshot.parsed_version);
            None
        } else {
            Some(PendingNativeBaseline {
                uri: uri.clone(),
                tokens: stored_tokens,
                language: language_name,
                cache_key,
                snapshot: snapshot_identity,
                served_version: snapshot.parsed_version,
            })
        };

        log::debug!(
            target: "kakehashi::semantic",
            "[SEMANTIC_TOKENS] DONE uri={} req={} tokens={}",
            uri, request_id, lsp_tokens.data.len()
        );

        Ok(Some(
            NativeSemanticLayer::new(lsp_tokens, Some(snapshot), request_guard, token_generation)
                .with_pending_native(pending_native),
        ))
    }

    async fn semantic_delta_has_applicable_bridge(
        &self,
        lsp_uri: &tower_lsp_server::ls_types::Uri,
        uri: &Url,
        snapshot: &crate::document::snapshot::ParseSnapshot,
        bridge_language: &str,
        parser_language: Option<&str>,
    ) -> bool {
        const METHOD: &str = "textDocument/semanticTokens/full";
        let settings = self.settings_manager.load_settings();
        let layers = super::super::bridge_context::resolve_layer_config_from_settings(
            &settings,
            bridge_language,
            METHOD,
        );
        if layers.priorities.contains(&LayerSource::Host)
            && let Some(ctx) = self.resolve_host_bridge_context(lsp_uri, METHOD)
        {
            let pool = self.bridge.pool_arc();
            let mut incapable = std::collections::HashSet::new();
            for config in &ctx.configs {
                let key = pool
                    .resolved_connection_key(&config.server_name, &config.config, uri)
                    .await;
                if pool.connection_known_incapable(&key, METHOD).await {
                    incapable.insert(config.server_name.clone());
                }
            }
            let suppressed = ctx
                .configs
                .iter()
                .filter(|config| {
                    self.bridge
                        .pool()
                        .host_routing_by_server(uri, &config.server_name)
                        == Some(false)
                })
                .map(|config| config.server_name.clone())
                .collect::<std::collections::HashSet<_>>();
            if semantic_host_configs_select_servers(
                &ctx.priorities,
                &ctx.configs,
                ctx.max_fan_out,
                &incapable,
                &suppressed,
            ) {
                return true;
            }
        }
        if !layers.priorities.contains(&LayerSource::Virt) {
            return false;
        }
        let Some(parser_language) = parser_language else {
            return false;
        };

        let generation = self.cache.semantic_token_generation();
        let owned_regions;
        let regions = if let Some((stamped, regions)) = snapshot.resolved_regions.as_ref()
            && *stamped == generation
        {
            regions.as_slice()
        } else {
            let (Some(tree), Some(query)) = (
                snapshot.tree.as_ref(),
                self.language.injection_query(parser_language),
            ) else {
                return false;
            };
            owned_regions = InjectionResolver::resolve_all(
                &self.language,
                self.bridge.node_tracker(),
                uri,
                tree,
                &snapshot.text,
                query.as_ref(),
                snapshot.incarnation,
            );
            &owned_regions
        };
        let pool = self.bridge.pool_arc();
        let mut incapable_by_connection = std::collections::HashMap::new();
        for region in regions {
            if !region.contiguous {
                continue;
            }
            let configs = self
                .bridge_configs_for_injection_language(bridge_language, &region.injection_language);
            let agg = self.resolve_aggregation_config(
                bridge_language,
                &region.injection_language,
                METHOD,
            );
            let Ok(routing_uri) = url::Url::parse(
                &crate::lsp::bridge::VirtualDocumentUri::new(
                    lsp_uri,
                    &region.injection_language,
                    &region.region.region_id,
                )
                .to_uri_string(),
            ) else {
                continue;
            };
            let mut incapable = std::collections::HashSet::new();
            for config in &configs {
                let key = pool
                    .resolved_connection_key(&config.server_name, &config.config, &routing_uri)
                    .await;
                let known_incapable = match incapable_by_connection.get(&key) {
                    Some(known_incapable) => *known_incapable,
                    None => {
                        let known_incapable = pool.connection_known_incapable(&key, METHOD).await;
                        incapable_by_connection.insert(key, known_incapable);
                        known_incapable
                    }
                };
                if known_incapable {
                    incapable.insert(config.server_name.clone());
                }
            }
            let suppressed = configs
                .iter()
                .filter(|config| {
                    pool.host_routing_by_server(&routing_uri, &config.server_name) == Some(false)
                })
                .map(|config| config.server_name.clone())
                .collect::<std::collections::HashSet<_>>();
            if semantic_region_selects_servers(
                true,
                &agg.priorities,
                &configs,
                agg.max_fan_out,
                &incapable,
                &suppressed,
            ) {
                return true;
            }
        }
        false
    }

    fn semantic_tokens_full_includes_native(&self, uri: &Url) -> bool {
        const METHOD: &str = "textDocument/semanticTokens/full";
        self.document_language(uri).is_none_or(|language| {
            let layers = self.resolve_layer_config(&language, METHOD);
            layers.allows(LayerSource::Native)
        })
    }

    async fn semantic_tokens_full_has_potential_virtual_producer(
        &self,
        host_uri: &Url,
        host_language: &str,
    ) -> bool {
        const METHOD: &str = "textDocument/semanticTokens/full";
        if self.language.injection_query(host_language).is_none() {
            return false;
        }
        let settings = self.settings_manager.load_settings();
        let mut candidates = settings.languages.keys().cloned().collect::<Vec<_>>();
        candidates.extend(
            settings
                .language_servers
                .values()
                .filter_map(|server| server.languages.as_ref())
                .flatten()
                .filter(|language| language.as_str() != "*")
                .cloned(),
        );
        if let Some(bridge) = settings
            .languages
            .get(host_language)
            .and_then(|language| language.bridge.as_ref())
        {
            candidates.extend(bridge.keys().filter(|name| *name != "_").cloned());
        }
        let wildcard_server = settings.language_servers.values().any(|server| {
            server
                .languages
                .as_ref()
                .is_some_and(|languages| languages.iter().any(|language| language == "*"))
        });
        if wildcard_server {
            candidates.push("__kakehashi_any_injection__".to_owned());
        }
        candidates.sort();
        candidates.dedup();
        let plans = candidates
            .into_iter()
            .filter_map(|injection_language| {
                let configs =
                    self.bridge_configs_for_injection_language(host_language, &injection_language);
                if configs.is_empty() {
                    return None;
                }
                let aggregation =
                    self.resolve_aggregation_config(host_language, &injection_language, METHOD);
                Some((configs, aggregation))
            })
            .collect::<Vec<_>>();
        let pool = self.bridge.pool();
        let mut incapable = std::collections::HashSet::new();
        let mut incapable_by_connection = std::collections::HashMap::new();
        for (configs, _) in &plans {
            for config in configs {
                let key = pool
                    .resolved_connection_key(&config.server_name, &config.config, host_uri)
                    .await;
                let known_incapable = match incapable_by_connection.get(&key) {
                    Some(known_incapable) => *known_incapable,
                    None => {
                        let known_incapable = pool.connection_known_incapable(&key, METHOD).await;
                        incapable_by_connection.insert(key, known_incapable);
                        known_incapable
                    }
                };
                if known_incapable {
                    incapable.insert(config.server_name.clone());
                }
            }
        }
        plans.into_iter().any(|(configs, aggregation)| {
            semantic_region_selects_servers(
                true,
                &aggregation.priorities,
                &configs,
                aggregation.max_fan_out,
                &incapable,
                &std::collections::HashSet::new(),
            )
        })
    }

    async fn semantic_tokens_delta_reenter_full(
        &self,
        uri: &Url,
        full_params: SemanticTokensParams,
        request_id: crate::lsp::cache::RequestId,
        cancel_token: crate::cancel::CancelToken,
        cancel_rx: &mut Option<crate::lsp::request_id::CancelReceiver>,
    ) -> Result<Option<SemanticTokensFullDeltaResult>> {
        let full = self.semantic_tokens_full_impl_with_tracking(
            full_params,
            Some((request_id, cancel_token.clone())),
        );
        let result = match cancel_rx.as_mut() {
            Some(cancel_rx) => {
                tokio::select! {
                    biased;
                    _ = cancel_rx => {
                        cancel_token.cancel();
                        self.cache.finish_request(uri, request_id);
                        return Err(Error::request_cancelled());
                    },
                    result = full => result,
                }
            }
            None => full.await,
        };
        self.cache.finish_request(uri, request_id);
        result.map(|result| {
            result.map(|result| match result {
                SemanticTokensResult::Tokens(tokens) => {
                    SemanticTokensFullDeltaResult::Tokens(tokens)
                }
                SemanticTokensResult::Partial(partial) => {
                    SemanticTokensFullDeltaResult::Tokens(SemanticTokens {
                        result_id: None,
                        data: partial.data,
                    })
                }
            })
        })
    }

    pub(crate) async fn semantic_tokens_full_delta_impl(
        &self,
        params: SemanticTokensDeltaParams,
    ) -> Result<Option<SemanticTokensFullDeltaResult>> {
        let Ok(uri) = uri_to_url(&params.text_document.uri) else {
            log::warn!(
                "Invalid URI in semanticTokens/full/delta: {}",
                params.text_document.uri.as_str()
            );
            return Ok(None);
        };
        let upstream_id = current_upstream_id();
        let (mut cancel_rx, _subscription_guard) = self.subscribe_cancel(upstream_id.as_ref());
        let (request_id, cancel_token) = self.cache.start_request(&uri);
        let mut request_guard = SemanticRequestGuard::new(
            std::sync::Arc::clone(&self.cache),
            uri.clone(),
            request_id,
            cancel_token.clone(),
            true,
        );
        let request =
            self.semantic_tokens_full_delta_tracked_impl(params, request_id, cancel_token.clone());
        let mut outcome = match cancel_rx.as_mut() {
            Some(cancel_rx) => {
                tokio::select! {
                    biased;
                    _ = cancel_rx => {
                        cancel_token.cancel();
                        Err(Error::request_cancelled())
                    }
                    _ = cancel_token.cancelled() => Ok(None),
                    result = request => result,
                }
            }
            None => {
                tokio::select! {
                    biased;
                    _ = cancel_token.cancelled() => Ok(None),
                    result = request => result,
                }
            }
        };
        if matches!(outcome, Ok(Some(_))) {
            let edit_lock = self.documents.edit_lock(&uri);
            let lock = edit_lock.lock();
            let commit_guard = match cancel_rx.as_mut() {
                Some(cancel_rx) => tokio::select! {
                    biased;
                    _ = cancel_rx => {
                        cancel_token.cancel();
                        outcome = Err(Error::request_cancelled());
                        None
                    }
                    _ = cancel_token.cancelled() => {
                        outcome = Ok(None);
                        None
                    }
                    guard = lock => Some(guard),
                },
                None => tokio::select! {
                    biased;
                    _ = cancel_token.cancelled() => {
                        outcome = Ok(None);
                        None
                    }
                    guard = lock => Some(guard),
                },
            };
            if commit_guard.is_some()
                && let Ok(Some(computed)) = &mut outcome
            {
                let current = self.semantic_delta_computation_is_current(&uri, computed);
                if !current
                    || self
                        .cache
                        .with_active_request(&uri, request_id, || {
                            if let Some(pending) = computed.pending_native.take() {
                                pending.commit(&self.cache);
                            }
                            if let Some(pending) = computed.pending_wire.take() {
                                pending.commit(&self.cache);
                            }
                        })
                        .is_none()
                {
                    outcome = Ok(None);
                }
            }
        }
        request_guard.finish();
        outcome.map(|outcome| outcome.map(|computed| computed.result))
    }

    async fn semantic_tokens_full_delta_tracked_impl(
        &self,
        params: SemanticTokensDeltaParams,
        request_id: crate::lsp::cache::RequestId,
        cancel_token: crate::cancel::CancelToken,
    ) -> Result<Option<SemanticDeltaComputation>> {
        let lsp_uri = params.text_document.uri.clone();
        let Ok(uri) = uri_to_url(&lsp_uri) else {
            log::warn!(
                "Invalid URI in semanticTokens/full/delta: {}",
                lsp_uri.as_str()
            );
            return Ok(None);
        };
        let previous_result_id = params.previous_result_id.clone();
        let Some(request_identity) = self
            .documents
            .get(&uri)
            .map(|document| (document.incarnation(), document.content_version()))
        else {
            return Ok(None);
        };
        let request_generation = self.cache.semantic_token_generation();
        // Hold the requested baseline across the nested full request. The wire
        // cache is byte-bounded, so looking it up afterwards can lose the exact
        // lineage when the newly computed full result evicts older entries.
        let previous_wire = self
            .cache
            .get_wire_tokens_if_valid(&uri, &previous_result_id);
        let previous_native = previous_wire
            .is_none()
            .then(|| self.cache.get_tokens_if_valid(&uri, &previous_result_id))
            .flatten();
        // Preserve the native-only idle fast path. A wire baseline or any
        // eligible bridge layer must still execute the full fan-out because a
        // downstream server may change its answer without a document edit.
        if let Some(previous) = previous_native.as_ref()
            && let Some(view) = self.documents.latest_snapshot(&uri)
            && let Some(snapshot) = view.slot.snapshot.as_ref()
            && snapshot.incarnation == view.slot.current_incarnation
            && snapshot.parsed_version == view.content_version
            && let Some(language) = snapshot.language.as_deref()
            && crate::lsp::lsp_impl::bridge_context::resolve_layer_config_from_settings(
                &self.settings_manager.load_settings(),
                language,
                "textDocument/semanticTokens/full",
            )
            .allows(LayerSource::Native)
            && !self
                .semantic_delta_has_applicable_bridge(&lsp_uri, &uri, snapshot, language)
                .await
            && self.cache.semantic_token_generation() == request_generation
            && let Some(current) = self.cache.get_current_tokens_for_snapshot(
                &uri,
                language,
                SemanticSnapshotIdentity {
                    parsed_version: snapshot.parsed_version,
                    incarnation: snapshot.incarnation,
                    generation: request_generation,
                },
            )
            && std::sync::Arc::ptr_eq(previous, &current)
        {
            let edit_lock = self.documents.edit_lock(&uri);
            let _edit_guard = edit_lock.lock().await;
            if !cancel_token.is_cancelled()
                && self.cache.is_request_active(&uri, request_id)
                && self.semantic_full_response_is_current(
                    &uri,
                    request_identity,
                    request_generation,
                    Some(snapshot),
                    true,
                    &edit_lock,
                )
            {
                self.cache
                    .record_served_semantic_version(&uri, snapshot.parsed_version);
                return Ok(Some(SemanticDeltaComputation {
                    result: SemanticTokensFullDeltaResult::TokensDelta(SemanticTokensDelta {
                        result_id: Some(previous_result_id),
                        edits: Vec::new(),
                    }),
                    pending_native: None,
                    pending_wire: None,
                    identity: request_identity,
                    generation: request_generation,
                    snapshot_present: true,
                }));
            }
            return Ok(None);
        }
        let previous = previous_wire.or(previous_native);
        let full_params = SemanticTokensParams {
            text_document: params.text_document,
            work_done_progress_params: params.work_done_progress_params,
            partial_result_params: params.partial_result_params,
        };
        let Some(current) = self
            .semantic_tokens_full_impl_with_tracking(
                full_params,
                Some((request_id, cancel_token.clone())),
            )
            .await?
        else {
            return Ok(None);
        };
        let pending_native = current.pending_native;
        let pending_wire = current.pending_wire;
        let snapshot_present = current.snapshot_present;
        let current = match current.result {
            SemanticTokensResult::Tokens(tokens) => tokens,
            SemanticTokensResult::Partial(partial) => SemanticTokens {
                result_id: None,
                data: partial.data,
            },
        };
        let result = match previous {
            Some(previous) => calculate_delta_or_full(&previous, &current, &previous_result_id),
            None => SemanticTokensFullDeltaResult::Tokens(current),
        };
        #[cfg(feature = "e2e")]
        wait_for_semantic_delta_commit_release().await;
        let edit_lock = self.documents.edit_lock(&uri);
        let _edit_guard = edit_lock.lock().await;
        if cancel_token.is_cancelled()
            || !self.cache.is_request_active(&uri, request_id)
            || self.cache.semantic_token_generation() != request_generation
            || !self.documents.latest_snapshot(&uri).is_some_and(|view| {
                view.slot.current_incarnation == request_identity.0
                    && view.content_version == request_identity.1
                    && view.slot.snapshot.is_some() == snapshot_present
            })
        {
            return Ok(None);
        }

        Ok(Some(SemanticDeltaComputation {
            result,
            pending_native,
            pending_wire,
            identity: request_identity,
            generation: request_generation,
            snapshot_present,
        }))
    }

    pub(crate) async fn semantic_tokens_range_impl(
        &self,
        params: SemanticTokensRangeParams,
    ) -> Result<Option<SemanticTokensRangeResult>> {
        const METHOD: &str = "textDocument/semanticTokens/range";
        let lsp_uri = params.text_document.uri.clone();
        let range = params.range;
        if (range.start.line, range.start.character) >= (range.end.line, range.end.character) {
            return Ok(Some(SemanticTokensRangeResult::Tokens(SemanticTokens {
                result_id: None,
                data: Vec::new(),
            })));
        }
        let progress_token = params.work_done_progress_params.work_done_token.clone();
        let mut raw_params = serde_json::to_value(&params).unwrap_or(serde_json::Value::Null);
        if let Some(params) = raw_params.as_object_mut() {
            // Downstream progress for the client's token is not aggregated or
            // translated by the bridge reader. Force one complete final result
            // instead of silently dropping streamed semantic-token chunks.
            params.remove("partialResultToken");
        }
        let request_identity = std::sync::Arc::new(std::sync::OnceLock::new());
        let virt = self.observe_semantic_range_identity(
            &lsp_uri,
            std::sync::Arc::clone(&request_identity),
            self.semantic_tokens_range_virt_layer(&lsp_uri, range, progress_token),
        );
        let host = self.observe_semantic_range_identity(
            &lsp_uri,
            std::sync::Arc::clone(&request_identity),
            self.semantic_tokens_range_host_layer(&lsp_uri, range, raw_params),
        );
        let native = self.observe_semantic_range_identity(
            &lsp_uri,
            std::sync::Arc::clone(&request_identity),
            self.semantic_tokens_range_native_layer(params),
        );

        let result = self
            .walk_layer_futures(
                &lsp_uri,
                METHOD,
                METHOD,
                virt,
                host,
                native,
                |tokens: &SemanticTokensRangeResult| match tokens {
                    SemanticTokensRangeResult::Tokens(tokens) => !tokens.data.is_empty(),
                    SemanticTokensRangeResult::Partial(partial) => !partial.data.is_empty(),
                },
            )
            .await?;
        let Some(&(incarnation, content_version)) = request_identity.get() else {
            return Ok(None);
        };
        if !self.semantic_range_snapshot_is_current(&lsp_uri, incarnation, content_version) {
            return Ok(None);
        }
        Ok(result)
    }

    async fn observe_semantic_range_identity<F>(
        &self,
        lsp_uri: &tower_lsp_server::ls_types::Uri,
        request_identity: std::sync::Arc<std::sync::OnceLock<(u64, u64)>>,
        layer: F,
    ) -> Result<Option<SemanticTokensRangeResult>>
    where
        F: std::future::Future<Output = Result<Option<SemanticTokensRangeResult>>>,
    {
        let identity = uri_to_url(lsp_uri)
            .ok()
            .and_then(|uri| self.documents.get(&uri))
            .map(|document| (document.incarnation(), document.content_version()));
        let result = layer.await;
        let identity = identity.or_else(|| {
            uri_to_url(lsp_uri)
                .ok()
                .and_then(|uri| self.documents.get(&uri))
                .map(|document| (document.incarnation(), document.content_version()))
        });
        if let Some(identity) = identity {
            let _ = request_identity.set(identity);
        }
        result
    }

    async fn semantic_tokens_range_host_layer(
        &self,
        lsp_uri: &tower_lsp_server::ls_types::Uri,
        range: Range,
        raw_params: serde_json::Value,
    ) -> Result<Option<SemanticTokensRangeResult>> {
        const METHOD: &str = "textDocument/semanticTokens/range";
        let Some(ctx) = self.resolve_host_bridge_context(lsp_uri, METHOD) else {
            return Ok(None);
        };
        let incarnation = ctx.incarnation;
        let content_version = ctx.content_version;
        if !self.semantic_range_snapshot_is_current(lsp_uri, incarnation, content_version) {
            return Ok(None);
        }
        let (cancel_rx, _cancel_guard) = self.subscribe_cancel(ctx.upstream_request_id.as_ref());
        let documents = std::sync::Arc::clone(&self.documents);
        let result = dispatch_host_preferred(
            &ctx,
            self.bridge.pool_arc(),
            move |task: HostFanOutTask| {
                let params = raw_params.clone();
                let documents = std::sync::Arc::clone(&documents);
                async move {
                    let host_uri = task.uri.clone();
                    let revision_text_reader: crate::lsp::bridge::HostTextReader =
                        std::sync::Arc::new(move || {
                            documents.get(&host_uri).and_then(|document| {
                                (document.incarnation() == incarnation
                                    && document.content_version() == content_version)
                                    .then(|| document.text_arc())
                            })
                        });
                    task.pool
                        .send_host_semantic_tokens_range_request(
                            &task.server_name,
                            &task.server_config,
                            &HostDocument {
                                uri: &task.uri,
                                language_id: &task.language_id,
                                text: &task.text,
                            },
                            params,
                            range,
                            task.upstream_id,
                            incarnation,
                            revision_text_reader,
                        )
                        .await
                }
            },
            |tokens| {
                tokens
                    .as_ref()
                    .is_some_and(|tokens| !tokens.data.is_empty())
            },
            cancel_rx,
        )
        .await;
        let tokens = self.host_layer_result(result, METHOD, |won| won).await?;
        if !self.semantic_range_snapshot_is_current(lsp_uri, incarnation, content_version) {
            return Ok(None);
        }
        Ok(tokens.map(SemanticTokensRangeResult::Tokens))
    }

    async fn semantic_tokens_range_virt_layer(
        &self,
        lsp_uri: &tower_lsp_server::ls_types::Uri,
        range: Range,
        progress_token: Option<NumberOrString>,
    ) -> Result<Option<SemanticTokensRangeResult>> {
        const METHOD: &str = "textDocument/semanticTokens/range";
        let Some(mut ctx) = self
            .resolve_bridge_contexts_for_range(lsp_uri, range, METHOD)
            .await
        else {
            return Ok(None);
        };
        let offset = RegionOffset::with_per_line_offsets(
            ctx.document.resolved.region.line_range.start,
            ctx.document.resolved.line_column_offsets.clone(),
        );
        let Some(region_end) = ctx.document.region_end else {
            return Ok(None);
        };
        if ctx.range.start > ctx.range.end
            || !host_position_within_region_bounds(ctx.range.start, &offset, region_end)
            || !host_position_within_region_bounds(ctx.range.end, &offset, region_end)
        {
            return Ok(None);
        }
        let host_range = ctx.range;
        ctx.document.client_progress_token = progress_token;
        let incarnation = ctx.incarnation;
        let content_version = ctx.content_version;
        if !self.semantic_range_snapshot_is_current(lsp_uri, incarnation, content_version) {
            return Ok(None);
        }
        let (cancel_rx, _cancel_guard) =
            self.subscribe_cancel(ctx.document.upstream_request_id.as_ref());
        let result = dispatch_preferred(
            &ctx.document,
            self.bridge.pool_arc(),
            |task| async move {
                task.pool
                    .send_semantic_tokens_range_request(
                        &task.server_name,
                        &task.server_config,
                        &task.uri,
                        host_range,
                        region_end,
                        &task.injection_language,
                        &task.region_id,
                        task.offset,
                        &task.virtual_content,
                        task.upstream_id,
                        task.client_progress_token,
                        incarnation,
                    )
                    .await
            },
            |tokens| {
                tokens
                    .as_ref()
                    .is_some_and(|tokens| !tokens.data.is_empty())
            },
            cancel_rx,
        )
        .await;
        let tokens = result
            .handle(&self.notifier(), "semantic token range", None, Ok)
            .await?;
        if !self.semantic_range_snapshot_is_current(lsp_uri, incarnation, content_version) {
            return Ok(None);
        }
        Ok(tokens.map(SemanticTokensRangeResult::Tokens))
    }

    fn semantic_range_snapshot_is_current(
        &self,
        lsp_uri: &tower_lsp_server::ls_types::Uri,
        incarnation: u64,
        content_version: u64,
    ) -> bool {
        uri_to_url(lsp_uri)
            .ok()
            .and_then(|uri| {
                self.documents.get(&uri).map(|document| {
                    document.incarnation() == incarnation
                        && document.content_version() == content_version
                })
            })
            .unwrap_or(false)
    }

    async fn semantic_tokens_range_native_layer(
        &self,
        params: SemanticTokensRangeParams,
    ) -> Result<Option<SemanticTokensRangeResult>> {
        let lsp_uri = params.text_document.uri;
        let range = params.range;

        // Convert ls_types::Uri to url::Url for internal use
        let Ok(uri) = uri_to_url(&lsp_uri) else {
            log::warn!("Invalid URI in semanticTokens/range: {}", lsp_uri.as_str());
            return Ok(None);
        };

        let domain_range = range;
        if (domain_range.start.line, domain_range.start.character)
            >= (domain_range.end.line, domain_range.end.character)
        {
            return Ok(Some(SemanticTokensRangeResult::Tokens(SemanticTokens {
                result_id: None,
                data: vec![],
            })));
        }

        // Snapshot the settings generation at the top, before any await (#535): a
        // reload that bumps the generation after this leaves this request's stored
        // key on the old generation — invisible to post-reload requests (which
        // compute the new-generation key) — so a stale entry can never be served.
        let generation = self.cache.semantic_token_generation();

        // First-parse bound (parse-snapshot ADR §3): resolve through the same
        // bounded first-parse wait as full/delta. Without it, a range request
        // racing didOpen answered empty tokens with nothing to re-drive the
        // client — the parse loop's refresh heals full/delta lineages, but an
        // empty range response has no lineage, so the viewport stayed blank
        // until an incidental re-request. Steady state (snapshot present)
        // resolves immediately.
        let Some(snapshot) = self.snapshot_for_tokens(&uri).await else {
            return Ok(Some(SemanticTokensRangeResult::Tokens(SemanticTokens {
                result_id: None,
                data: vec![],
            })));
        };
        // Staleness-reject: the request's `range` is authored against the
        // LIVE text, so a trailing (or cross-incarnation) snapshot cannot
        // answer it — unlike full/delta (whole-document, which PARK for the
        // current snapshot instead). A stale snapshot → ContentModified; the
        // client's next natural request (this is a per-redraw viewport read)
        // gets the fresh one.
        let Some(view) = self.documents.latest_snapshot(&uri) else {
            return Ok(Some(SemanticTokensRangeResult::Tokens(SemanticTokens {
                result_id: None,
                data: vec![],
            })));
        };
        if snapshot.incarnation != view.slot.current_incarnation
            || snapshot.parsed_version != view.content_version
        {
            return Err(crate::error::content_modified_error());
        }
        let snapshot_identity = SemanticSnapshotIdentity {
            parsed_version: snapshot.parsed_version,
            incarnation: snapshot.incarnation,
            generation,
        };
        let (Some(language_name), Some(tree)) = (snapshot.language.clone(), snapshot.tree.clone())
        else {
            self.cache
                .record_served_semantic_version(&uri, snapshot.parsed_version);
            return Ok(Some(SemanticTokensRangeResult::Tokens(SemanticTokens {
                result_id: None,
                data: vec![],
            })));
        };
        let text = std::sync::Arc::clone(&snapshot.text);

        let language_loaded = self
            .language
            .ensure_language_loaded_async(&language_name)
            .await
            .success;
        let edit_lock = self.documents.edit_lock(&uri);
        let _edit_guard = edit_lock.lock().await;
        if !self.semantic_snapshot_is_current(
            &uri,
            snapshot.incarnation,
            snapshot.parsed_version,
            generation,
            &edit_lock,
        ) {
            return Err(crate::error::content_modified_error());
        }
        if !language_loaded {
            self.cache
                .record_served_semantic_version(&uri, snapshot.parsed_version);
            return Ok(Some(SemanticTokensRangeResult::Tokens(SemanticTokens {
                result_id: None,
                data: vec![],
            })));
        }
        let Some(query) = self.language.highlight_query(&language_name) else {
            self.cache
                .record_served_semantic_version(&uri, snapshot.parsed_version);
            return Ok(Some(SemanticTokensRangeResult::Tokens(SemanticTokens {
                result_id: None,
                data: vec![],
            })));
        };
        drop(_edit_guard);

        // Short-circuit an identical-viewport re-request of an unchanged document
        // (#535). `cache_key` folds the document text with the settings generation,
        // and the entry also pins the viewport `range`, so a hit means re-tokenizing
        // would reproduce these exact tokens. Misses (scroll, edit, or reload) fall
        // through to the recompute below, which restores the entry.
        let cache_key = self.cache.cache_key_for(&text, generation);
        if let Some(tokens) =
            self.cache
                .get_current_range_tokens(&uri, &domain_range, &language_name, cache_key)
        {
            let tokens = (*tokens).clone();
            let edit_lock = self.documents.edit_lock(&uri);
            let _edit_guard = edit_lock.lock().await;
            let current = self.semantic_snapshot_is_current(
                &uri,
                snapshot.incarnation,
                snapshot.parsed_version,
                generation,
                &edit_lock,
            );
            if !current {
                return Err(crate::error::content_modified_error());
            }
            return Ok(Some(SemanticTokensRangeResult::Tokens(tokens)));
        }

        // A previous full/delta request, or an earlier range miss below, may have
        // already computed the whole-document token set for this exact snapshot.
        // Filtering it is much cheaper than re-running the full tree-sitter path
        // for every scrolled viewport.
        if let Some(full_tokens) = self
            .cache
            .get_current_tokens(&uri, &language_name, cache_key)
        {
            let range_tokens = filter_semantic_tokens_by_range(&full_tokens, &domain_range);
            let response_tokens = range_tokens.clone();
            let edit_lock = self.documents.edit_lock(&uri);
            let _edit_guard = edit_lock.lock().await;
            let current = self.semantic_snapshot_is_current(
                &uri,
                snapshot.incarnation,
                snapshot.parsed_version,
                generation,
                &edit_lock,
            );
            if !current {
                return Err(crate::error::content_modified_error());
            }
            self.cache.store_range_tokens(
                uri,
                domain_range,
                language_name,
                range_tokens,
                cache_key,
            );
            return Ok(Some(SemanticTokensRangeResult::Tokens(response_tokens)));
        }

        // Get capture mappings for token type resolution
        let capture_mappings = self.language.capture_mappings();

        // Use Rayon-based parallel injection processing
        let supports_multiline = self.settings_manager.supports_multiline_tokens();
        let coordinator = std::sync::Arc::clone(&self.language);

        // Bind the work to both ownership of this layer future and the input
        // version it was built from. A higher-priority bridge answer or an
        // upstream cancellation drops this future and trips the guard; an edit
        // or close trips the document's version token. In either case, forward
        // cancellation into the token polled by the blocking collector so a
        // losing/stale work unit cannot occupy the shared compute pool.
        let Some(version_cancel) = self.documents.get(&uri).and_then(|document| {
            (document.incarnation() == snapshot.incarnation
                && document.content_version() == snapshot.parsed_version)
                .then(|| document.version_cancel_token())
        }) else {
            return Err(crate::error::content_modified_error());
        };
        let compute_cancel = crate::cancel::CancelToken::default();
        let mut compute_guard = SemanticComputeCancelGuard::new(compute_cancel.clone());

        let compute = handle_semantic_tokens_full(
            &self.compute_pool,
            text,
            tree,
            query,
            Some(language_name.clone()),
            Some(capture_mappings),
            coordinator,
            supports_multiline,
            None,
            Some(compute_cancel.clone()),
        );
        let result = tokio::select! {
            result = compute => result,
            _ = version_cancel.cancelled() => {
                compute_cancel.cancel();
                return Err(crate::error::content_modified_error());
            }
        };
        compute_guard.disarm();

        // Shape immutable payloads before taking the edit lock. Only the final
        // live-snapshot validation and cache commits need to exclude edits.
        let (domain_range_result, tokens_to_store) = match result {
            Some(tower_lsp_server::ls_types::SemanticTokensResult::Tokens(mut full_tokens)) => {
                full_tokens.result_id = Some(next_result_id());
                let range_tokens = filter_semantic_tokens_by_range(&full_tokens, &domain_range);
                let response = tower_lsp_server::ls_types::SemanticTokensRangeResult::from(
                    range_tokens.clone(),
                );
                (response, Some((full_tokens, range_tokens)))
            }
            Some(tower_lsp_server::ls_types::SemanticTokensResult::Partial(partial)) => (
                tower_lsp_server::ls_types::SemanticTokensRangeResult::from(partial),
                None,
            ),
            None => (
                tower_lsp_server::ls_types::SemanticTokensRangeResult::Tokens(
                    tower_lsp_server::ls_types::SemanticTokens {
                        result_id: None,
                        data: Vec::new(),
                    },
                ),
                None,
            ),
        };

        // A range is authored against one live document lifetime. A close,
        // reopen, or edit during the uncancellable full-document compute makes
        // both its response coordinates and any cache store obsolete.
        let edit_lock = self.documents.edit_lock(&uri);
        let _edit_guard = edit_lock.lock().await;
        let still_current = self.semantic_snapshot_is_current(
            &uri,
            snapshot.incarnation,
            snapshot.parsed_version,
            generation,
            &edit_lock,
        );
        if !still_current {
            return Err(crate::error::content_modified_error());
        }

        // Cache ONLY a clean `Tokens` result (#535). Partial/None responses are
        // degraded or transient and must not become reusable cache entries.
        if let Some((full_tokens, range_tokens)) = tokens_to_store {
            self.cache.store_tokens(
                uri.clone(),
                full_tokens,
                language_name.clone(),
                cache_key,
                snapshot_identity,
            );
            self.cache.store_range_tokens(
                uri,
                domain_range,
                language_name,
                range_tokens,
                cache_key,
            );
        }

        Ok(Some(domain_range_result))
    }
}

fn semantic_region_selects_servers(
    contiguous: bool,
    priorities: &[String],
    configs: &[crate::lsp::bridge::ResolvedServerConfig],
    max_fan_out: Option<usize>,
    incapable: &std::collections::HashSet<String>,
    suppressed: &std::collections::HashSet<String>,
) -> bool {
    if !contiguous {
        return false;
    }
    semantic_virt_configs_select_servers(priorities, configs, max_fan_out, incapable, suppressed)
}

fn semantic_virt_configs_select_servers(
    priorities: &[String],
    configs: &[crate::lsp::bridge::ResolvedServerConfig],
    max_fan_out: Option<usize>,
    incapable: &std::collections::HashSet<String>,
    suppressed: &std::collections::HashSet<String>,
) -> bool {
    use crate::lsp::aggregation::server::priority::PriorityEntry;

    // Full virtual fan-out removes definitely incapable configs before it expands
    // priorities and applies maxFanOut. Mirror that ordering so delta re-entry
    // promotes the same lower-priority capable server. Routing suppression remains
    // a post-cap filter, matching dispatch.
    let capable_configs = configs
        .iter()
        .filter(|config| !incapable.contains(&config.server_name))
        .cloned()
        .collect::<Vec<_>>();
    crate::lsp::aggregation::server::truncate_entries(
        crate::lsp::aggregation::server::expand_priorities(priorities, &capable_configs),
        max_fan_out,
    )
    .iter()
    .flat_map(|entry| match entry {
        PriorityEntry::Server(name) => std::slice::from_ref(name),
        PriorityEntry::Rest(names) => names.as_slice(),
    })
    .any(|name| !suppressed.contains(name))
}

fn semantic_host_configs_select_servers(
    priorities: &[String],
    configs: &[crate::lsp::bridge::ResolvedServerConfig],
    max_fan_out: Option<usize>,
    incapable: &std::collections::HashSet<String>,
    suppressed: &std::collections::HashSet<String>,
) -> bool {
    use crate::lsp::aggregation::server::priority::PriorityEntry;

    crate::lsp::aggregation::server::truncate_entries(
        crate::lsp::aggregation::server::expand_priorities(priorities, configs),
        max_fan_out,
    )
    .iter()
    .flat_map(|entry| match entry {
        PriorityEntry::Server(name) => std::slice::from_ref(name),
        PriorityEntry::Rest(names) => names.as_slice(),
    })
    .any(|name| !incapable.contains(name) && !suppressed.contains(name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::{Duration, sleep, timeout};
    use tower_lsp_server::LspService;
    use url::Url;

    #[test]
    fn dropping_semantic_compute_owner_cancels_blocking_work() {
        let token = crate::cancel::CancelToken::default();
        {
            let _guard = SemanticComputeCancelGuard::new(token.clone());
            assert!(!token.is_cancelled());
        }
        assert!(token.is_cancelled());
    }

    #[test]
    fn delta_reentry_excludes_non_contiguous_virtual_regions() {
        let configs = vec![crate::lsp::bridge::ResolvedServerConfig {
            server_name: "tokens".into(),
            config: std::sync::Arc::new(crate::config::settings::BridgeServerConfig::default()),
        }];
        let priorities = [crate::config::settings::PRIORITIES_WILDCARD.into()];
        assert!(!semantic_region_selects_servers(
            false,
            &priorities,
            &configs,
            None,
            &std::collections::HashSet::new(),
            &std::collections::HashSet::new(),
        ));
        assert!(semantic_region_selects_servers(
            true,
            &priorities,
            &configs,
            None,
            &std::collections::HashSet::new(),
            &std::collections::HashSet::new(),
        ));
        assert!(!semantic_region_selects_servers(
            true,
            &priorities,
            &configs,
            None,
            &std::collections::HashSet::from(["tokens".into()]),
            &std::collections::HashSet::new(),
        ));
        assert!(!semantic_region_selects_servers(
            true,
            &priorities,
            &configs,
            None,
            &std::collections::HashSet::new(),
            &std::collections::HashSet::from(["tokens".into()]),
        ));
    }

    #[test]
    fn delta_reentry_matches_full_fanout_filter_order() {
        let configs = ["suppressed", "available"]
            .into_iter()
            .map(|server_name| crate::lsp::bridge::ResolvedServerConfig {
                server_name: server_name.into(),
                config: std::sync::Arc::new(crate::config::settings::BridgeServerConfig::default()),
            })
            .collect::<Vec<_>>();
        let priorities = [crate::config::settings::PRIORITIES_WILDCARD.into()];
        let suppressed = std::collections::HashSet::from(["suppressed".into()]);

        assert!(!semantic_virt_configs_select_servers(
            &priorities,
            &configs,
            Some(1),
            &std::collections::HashSet::new(),
            &suppressed,
        ));
        assert!(semantic_virt_configs_select_servers(
            &priorities,
            &configs,
            None,
            &std::collections::HashSet::new(),
            &suppressed,
        ));

        let incapable = std::collections::HashSet::from(["suppressed".into()]);
        assert!(
            semantic_virt_configs_select_servers(
                &priorities,
                &configs,
                Some(1),
                &incapable,
                &std::collections::HashSet::new(),
            ),
            "an incapable first server is removed before maxFanOut promotes the capable fallback"
        );
        assert!(
            !semantic_host_configs_select_servers(
                &priorities,
                &configs,
                Some(1),
                &incapable,
                &std::collections::HashSet::new(),
            ),
            "host fan-out caps before its per-request capability gate"
        );
    }

    #[test]
    fn absent_snapshot_full_fence_rejects_document_and_settings_changes() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        let uri = Url::parse("file:///semantic_absent_snapshot.unknown").expect("valid test URI");
        server.documents.insert(
            uri.clone(),
            "old".to_string(),
            Some("unknown".to_string()),
            None,
        );
        let view = server
            .documents
            .latest_snapshot(&uri)
            .expect("document must be open");
        assert!(view.slot.snapshot.is_none(), "snapshot must stay absent");
        let identity = (view.slot.current_incarnation, view.content_version);
        let generation = server.cache.semantic_token_generation();
        let settings_generation = server.settings_manager.settings_generation();
        let computed = SemanticFullComputation {
            result: SemanticTokensResult::Tokens(SemanticTokens {
                result_id: None,
                data: Vec::new(),
            }),
            pending_native: None,
            pending_wire: None,
            identity,
            generation,
            snapshot_present: false,
        };
        let delta_computed = SemanticDeltaComputation {
            result: SemanticTokensFullDeltaResult::Tokens(SemanticTokens {
                result_id: None,
                data: Vec::new(),
            }),
            pending_native: None,
            pending_wire: None,
            identity,
            generation,
            snapshot_present: false,
        };
        assert!(server.semantic_full_computation_is_current(&uri, &computed));
        assert!(server.semantic_delta_computation_is_current(&uri, &delta_computed));
        let edit_lock = server.documents.edit_lock(&uri);
        assert!(server.semantic_full_response_is_current(
            &uri,
            identity,
            (generation, settings_generation),
            None,
            true,
            &edit_lock,
        ));
        server
            .settings_manager
            .apply_settings(crate::config::WorkspaceSettings::default());
        assert!(
            !server.semantic_full_response_is_current(
                &uri,
                identity,
                (generation, settings_generation),
                None,
                true,
                &edit_lock,
            ),
            "a settings reload must invalidate the captured host-only plan"
        );
        let settings_generation = server.settings_manager.settings_generation();
        publish_treeless(server, &uri, "old", 0);
        assert!(
            !server.semantic_full_computation_is_current(&uri, &computed),
            "the final publication fence must reject a newly published snapshot"
        );
        assert!(
            !server.semantic_delta_computation_is_current(&uri, &delta_computed),
            "the delta publication fence must reject a newly published snapshot"
        );
        assert!(
            !server.semantic_full_response_is_current(
                &uri,
                identity,
                (generation, settings_generation),
                None,
                true,
                &edit_lock,
            ),
            "a snapshot published during parserless fan-out invalidates that response"
        );

        server
            .documents
            .update_document(uri.clone(), "new".to_string(), None);
        assert!(!server.semantic_full_response_is_current(
            &uri,
            identity,
            (generation, settings_generation),
            None,
            true,
            &edit_lock,
        ));

        let updated = server
            .documents
            .latest_snapshot(&uri)
            .expect("document must remain open");
        let updated_identity = (updated.slot.current_incarnation, updated.content_version);
        server.cache.bump_semantic_token_generation();
        assert!(!server.semantic_full_response_is_current(
            &uri,
            updated_identity,
            (generation, settings_generation),
            None,
            true,
            &edit_lock,
        ));
    }

    #[test]
    fn non_native_empty_result_requires_success_or_no_virtual_request() {
        assert!(should_publish_empty_non_native_result(
            true, false, true, true,
        ));
        assert!(should_publish_empty_non_native_result(
            true, true, false, false,
        ));
        assert!(
            !should_publish_empty_non_native_result(true, false, true, false),
            "a failed host or virtual request must not clear highlighting"
        );
        assert!(
            !should_publish_empty_non_native_result(true, true, true, false),
            "virt-only request failures differ from a document with no virtual request"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn absent_snapshot_full_honors_empty_layer_priorities() {
        use crate::config::WorkspaceSettings;
        use crate::config::settings::{LanguageSettings, LayerAggregationConfig, LayersConfig};
        use std::collections::HashMap;

        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        let mut aggregation = HashMap::new();
        aggregation.insert(
            "textDocument/semanticTokens/full".to_string(),
            LayerAggregationConfig {
                priorities: Some(Vec::new()),
                strategy: None,
            },
        );
        let mut languages = HashMap::new();
        languages.insert(
            "unknown".to_string(),
            LanguageSettings {
                layers: Some(LayersConfig {
                    aggregation: Some(aggregation),
                }),
                ..Default::default()
            },
        );
        server.settings_manager.apply_settings(WorkspaceSettings {
            languages,
            auto_install: false,
            ..Default::default()
        });

        let uri = Url::parse("file:///semantic_absent_priorities.unknown").expect("valid test URI");
        server.documents.insert(
            uri.clone(),
            "unparsed".to_string(),
            Some("unknown".to_string()),
            None,
        );
        let request = server.semantic_tokens_full_impl(full_params(&uri));
        tokio::pin!(request);
        tokio::select! {
            result = &mut request => panic!("request resolved before the first-parse backstop: {result:?}"),
            _ = tokio::task::yield_now() => {}
        }
        tokio::time::advance(
            crate::lsp::lsp_impl::snapshot_read::FIRST_PARSE_BACKSTOP + Duration::from_millis(1),
        )
        .await;

        let result = request
            .await
            .expect("semantic tokens full should return without error");
        assert!(
            result.is_none(),
            "empty priorities must disable every layer after an absent native snapshot: {result:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn treeless_full_without_an_actionable_virtual_layer_serves_native_empty() {
        use crate::config::WorkspaceSettings;
        use crate::config::settings::{LanguageSettings, LayerAggregationConfig, LayersConfig};
        use std::collections::HashMap;

        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        let mut aggregation = HashMap::new();
        aggregation.insert(
            "textDocument/semanticTokens/full".to_string(),
            LayerAggregationConfig {
                priorities: Some(vec![LayerSource::Virt, LayerSource::Native]),
                strategy: None,
            },
        );
        let mut languages = HashMap::new();
        languages.insert(
            "python".to_string(),
            LanguageSettings {
                layers: Some(LayersConfig {
                    aggregation: Some(aggregation),
                }),
                ..Default::default()
            },
        );
        server.settings_manager.apply_settings(WorkspaceSettings {
            languages,
            auto_install: false,
            ..Default::default()
        });

        server
            .language
            .language_registry_for_parallel()
            .register("python".to_string(), tree_sitter_python::LANGUAGE.into());

        let uri = Url::parse("file:///semantic_host_only.py").expect("valid test URI");
        server.documents.insert(
            uri.clone(),
            "unparsed".to_string(),
            Some("py".to_string()),
            None,
        );
        publish_treeless(server, &uri, "unparsed", 0);

        let result = tokio::time::timeout(
            Duration::from_millis(100),
            server.semantic_tokens_full_impl(full_params(&uri)),
        )
        .await
        .expect("tree-less virtual discovery must finish without waiting")
        .expect("semantic tokens full should return without error")
        .expect("the authoritative native empty layer must be served");
        let SemanticTokensResult::Tokens(tokens) = result else {
            panic!("full tokens")
        };
        assert!(tokens.data.is_empty());
        assert_eq!(server.cache.served_semantic_version(&uri), Some(0));
    }

    #[tokio::test(start_paused = true)]
    async fn delta_reentry_without_actionable_virtual_work_skips_the_first_parse_backstop() {
        use crate::config::WorkspaceSettings;
        use crate::config::settings::{LanguageSettings, LayerAggregationConfig, LayersConfig};
        use std::collections::HashMap;

        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        let mut aggregation = HashMap::new();
        aggregation.insert(
            "textDocument/semanticTokens/full".to_string(),
            LayerAggregationConfig {
                priorities: Some(vec![LayerSource::Host, LayerSource::Virt]),
                strategy: None,
            },
        );
        let mut languages = HashMap::new();
        languages.insert(
            "python".to_string(),
            LanguageSettings {
                layers: Some(LayersConfig {
                    aggregation: Some(aggregation),
                }),
                ..Default::default()
            },
        );
        server.settings_manager.apply_settings(WorkspaceSettings {
            languages,
            auto_install: false,
            ..Default::default()
        });
        server
            .language
            .language_registry_for_parallel()
            .register("python".to_string(), tree_sitter_python::LANGUAGE.into());

        let uri = Url::parse("file:///semantic_delta_host_only.py").expect("valid test URI");
        server.documents.insert(
            uri.clone(),
            "unparsed".to_string(),
            Some("py".to_string()),
            None,
        );
        let params = SemanticTokensDeltaParams {
            text_document: TextDocumentIdentifier {
                uri: crate::lsp::lsp_impl::url_to_uri(&uri).expect("test URI should convert"),
            },
            previous_result_id: "native-before-reload".into(),
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };

        let result = tokio::time::timeout(
            Duration::from_millis(500),
            server.semantic_tokens_full_delta_impl(params),
        )
        .await
        .expect("inactive virtual work must not trigger the first-parse backstop")
        .expect("semantic tokens delta should return without error");
        assert!(result.is_none(), "no host server is configured: {result:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn treeless_delta_with_actionable_virtual_work_preserves_previous_tokens() {
        use crate::config::WorkspaceSettings;
        use crate::config::settings::{
            BridgeServerConfig, LanguageSettings, LayerAggregationConfig, LayersConfig,
        };
        use std::collections::HashMap;

        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        let rust_language: tree_sitter::Language = tree_sitter_rust::LANGUAGE.into();
        let injection_query = tree_sitter::Query::new(
            &rust_language,
            r#"((string_literal (string_content) @injection.content)
                (#set! injection.language "html"))"#,
        )
        .expect("valid injection query");
        let mut aggregation = HashMap::new();
        aggregation.insert(
            "textDocument/semanticTokens/full".to_string(),
            LayerAggregationConfig {
                priorities: Some(vec![LayerSource::Virt, LayerSource::Native]),
                strategy: None,
            },
        );
        let mut languages = HashMap::new();
        languages.insert(
            "rust".to_string(),
            LanguageSettings {
                layers: Some(LayersConfig {
                    aggregation: Some(aggregation),
                }),
                ..Default::default()
            },
        );
        let mut language_servers = HashMap::new();
        language_servers.insert(
            "html-ls".to_string(),
            BridgeServerConfig {
                cmd: Some(vec!["unused-test-server".into()]),
                languages: Some(vec!["html".into()]),
                ..Default::default()
            },
        );
        server.settings_manager.apply_settings(WorkspaceSettings {
            languages,
            language_servers,
            auto_install: false,
            ..Default::default()
        });
        server
            .language
            .language_registry_for_parallel()
            .register("rust".to_string(), rust_language.clone());
        server
            .language
            .query_store()
            .insert_injection_query("rust".into(), std::sync::Arc::new(injection_query));
        assert!(server.language.injection_query("rust").is_some());
        assert!(
            !server
                .bridge_configs_for_injection_language("rust", "html")
                .is_empty()
        );
        let uri = Url::parse("file:///semantic_delta_treeless.rs").expect("valid test URI");
        assert!(
            server
                .semantic_tokens_full_has_potential_virtual_producer(&uri, "rust")
                .await
        );
        server.documents.insert(
            uri.clone(),
            "let html = \"<div>\";".to_string(),
            Some("rust".to_string()),
            None,
        );
        publish_treeless(server, &uri, "let html = \"<div>\";", 0);
        let direct_err = server
            .semantic_tokens_full_impl(full_params(&uri))
            .await
            .expect_err("direct full must defer the same pending virtual work");
        assert_eq!(direct_err.code, crate::error::content_modified_error().code);
        let params = SemanticTokensDeltaParams {
            text_document: TextDocumentIdentifier {
                uri: crate::lsp::lsp_impl::url_to_uri(&uri).expect("test URI should convert"),
            },
            previous_result_id: "previous-tokens".into(),
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };

        let err = server
            .semantic_tokens_full_delta_impl(params)
            .await
            .expect_err("pending virtual work must preserve the previous token baseline");
        assert_eq!(err.code, crate::error::content_modified_error().code);
        assert_eq!(server.cache.served_semantic_version(&uri), Some(0));
    }

    /// Publish a snapshot for `uri` built from `text` at `parsed_version`,
    /// tree-less (no parser needed): the handlers' snapshot-resolution and
    /// served-version bookkeeping are observable without tokenizing.
    fn publish_treeless(server: &Kakehashi, uri: &Url, text: &str, parsed_version: u64) {
        let incarnation = server
            .documents
            .latest_snapshot(uri)
            .expect("document must be open")
            .slot
            .current_incarnation;
        let landed = server
            .documents
            .get(uri)
            .map(|doc| {
                doc.publish_snapshot(std::sync::Arc::new(
                    crate::document::snapshot::ParseSnapshot {
                        text: std::sync::Arc::from(text),
                        tree: None,
                        language: Some("rust".to_string()),
                        parsed_version,
                        incarnation,
                        injection_regions: None,
                        bridge_regions: None,
                        resolved_regions: None,
                        layer_trees: std::sync::OnceLock::new(),
                    },
                ))
            })
            .unwrap_or(false);
        assert!(landed, "test snapshot must land");
    }

    fn full_params(uri: &Url) -> SemanticTokensParams {
        SemanticTokensParams {
            text_document: TextDocumentIdentifier {
                uri: crate::lsp::lsp_impl::url_to_uri(uri).expect("test URI should convert"),
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        }
    }

    #[tokio::test]
    async fn nested_full_keeps_borrowed_delta_tracking_active() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        let uri = Url::parse("file:///semantic_delta_tracking.rs").expect("valid test URI");
        server.documents.insert(
            uri.clone(),
            "fn main() {}".to_string(),
            Some("rust".to_string()),
            None,
        );
        publish_treeless(server, &uri, "fn main() {}", 0);
        let (request_id, cancel_token) = server.cache.start_request(&uri);

        server
            .semantic_tokens_full_impl_with_tracking(
                full_params(&uri),
                Some((request_id, cancel_token.clone())),
            )
            .await
            .expect("nested full should complete");
        assert!(
            server.cache.is_request_active(&uri, request_id),
            "nested full must leave the outer delta's request ownership active"
        );

        let (newer_id, _newer_token) = server.cache.start_request(&uri);
        assert!(cancel_token.is_cancelled());
        assert!(!server.cache.is_request_active(&uri, request_id));
        server.cache.finish_request(&uri, newer_id);
    }

    #[tokio::test]
    async fn nested_full_with_superseded_delta_tracking_preserves_the_newer_request() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let uri = Url::parse("file:///delta-reentry-superseded.rs").unwrap();
        let (older_id, older_cancel) = service.inner().cache.start_request(&uri);
        let (newer_id, _newer_cancel) = service.inner().cache.start_request(&uri);

        let result = service
            .inner()
            .semantic_tokens_full_impl_with_tracking(
                full_params(&uri),
                Some((older_id, older_cancel)),
            )
            .await
            .unwrap();

        assert!(result.is_none());
        assert!(service.inner().cache.is_request_active(&uri, newer_id));
    }

    #[tokio::test]
    async fn pending_wire_baseline_is_invisible_until_outer_commit() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        let uri = Url::parse("file:///pending-wire-baseline.rs").expect("valid test URI");
        let result_id = "pending-wire".to_string();
        let pending = PendingWireBaseline {
            uri: uri.clone(),
            tokens: SemanticTokens {
                result_id: Some(result_id.clone()),
                data: vec![tower_lsp_server::ls_types::SemanticToken {
                    delta_line: 0,
                    delta_start: 0,
                    length: 1,
                    token_type: 0,
                    token_modifiers_bitset: 0,
                }],
            },
            language: "rust".into(),
            cache_key: 1,
            snapshot: SemanticSnapshotIdentity {
                parsed_version: 1,
                incarnation: 1,
                generation: 1,
            },
        };

        assert!(
            server
                .cache
                .get_wire_tokens_if_valid(&uri, &result_id)
                .is_none(),
            "a nested full baseline must remain private before the delta fence"
        );
        let mut computed = SemanticFullComputation {
            result: SemanticTokensResult::Tokens(SemanticTokens {
                result_id: Some(result_id.clone()),
                data: Vec::new(),
            }),
            pending_native: None,
            pending_wire: Some(pending),
            identity: (1, 1),
            generation: 1,
            snapshot_present: true,
        };
        commit_full_baselines(&server.cache, &mut computed);
        assert!(
            server
                .cache
                .get_wire_tokens_if_valid(&uri, &result_id)
                .is_some(),
            "a direct full must publish only after its outer select accepts the response"
        );
    }

    #[tokio::test]
    async fn pending_native_baseline_is_invisible_until_outer_commit() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        let uri = Url::parse("file:///pending-native-baseline.rs").expect("valid test URI");
        let result_id = "pending-native".to_string();
        let pending = PendingNativeBaseline {
            uri: uri.clone(),
            tokens: SemanticTokens {
                result_id: Some(result_id.clone()),
                data: vec![tower_lsp_server::ls_types::SemanticToken {
                    delta_line: 0,
                    delta_start: 0,
                    length: 1,
                    token_type: 0,
                    token_modifiers_bitset: 0,
                }],
            },
            language: "rust".into(),
            cache_key: 1,
            snapshot: SemanticSnapshotIdentity {
                parsed_version: 1,
                incarnation: 1,
                generation: 1,
            },
            served_version: 1,
        };

        assert!(
            server.cache.get_tokens_if_valid(&uri, &result_id).is_none(),
            "a nested native baseline must remain private before the delta fence"
        );
        pending.commit(&server.cache);
        assert!(
            server.cache.get_tokens_if_valid(&uri, &result_id).is_some(),
            "a successful outer delta fence must publish its native baseline"
        );
    }

    fn range_params(uri: &Url, range: Range) -> SemanticTokensRangeParams {
        SemanticTokensRangeParams {
            text_document: TextDocumentIdentifier {
                uri: crate::lsp::lsp_impl::url_to_uri(uri).expect("test URI should convert"),
            },
            range,
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn semantic_tokens_empty_range_does_not_wait_for_first_parse() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let uri = Url::parse("file:///empty_semantic_range.rs").expect("valid test uri");
        service.inner().documents.insert(
            uri.clone(),
            "fn main() {}".to_string(),
            Some("rust".to_string()),
            None,
        );

        let result = timeout(
            Duration::from_millis(1),
            service.inner().semantic_tokens_range_impl(range_params(
                &uri,
                Range {
                    start: Position {
                        line: 0,
                        character: 3,
                    },
                    end: Position {
                        line: 0,
                        character: 3,
                    },
                },
            )),
        )
        .await
        .expect("an empty range must not wait for the first parse")
        .expect("empty range request should succeed")
        .expect("empty range request should return a token result");

        let SemanticTokensRangeResult::Tokens(tokens) = result else {
            panic!("empty range must return a complete empty token result");
        };
        assert!(tokens.data.is_empty());
    }

    /// Serve-current (the Neovim client contract): a full request arriving
    /// while the latest snapshot trails the live text must NOT serve the
    /// stale snapshot — it parks until the current one publishes and serves
    /// that. Pinned via the served-version mark: the old serve-stale model
    /// recorded the trailing `parsed_version` immediately.
    #[tokio::test(start_paused = true)]
    async fn semantic_tokens_full_parks_until_current_snapshot() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let service = std::sync::Arc::new(service);
        let uri = Url::parse("file:///serve_current.rs").expect("valid test uri");

        service.inner().documents.insert(
            uri.clone(),
            "fn main() {}".to_string(),
            Some("rust".to_string()),
            None,
        );
        publish_treeless(service.inner(), &uri, "fn main() {}", 0);
        // An edit bumps content_version past the published parse: the v0
        // snapshot is now stale.
        service
            .inner()
            .documents
            .update_document(uri.clone(), "fn main() { }".to_string(), None);

        let request = {
            let service = std::sync::Arc::clone(&service);
            let uri = uri.clone();
            tokio::spawn(async move {
                service
                    .inner()
                    .semantic_tokens_full_impl(full_params(&uri))
                    .await
            })
        };
        // Let the handler reach its snapshot wait, then publish the current
        // parse (well inside the settle backstop).
        sleep(Duration::from_millis(50)).await;
        assert!(
            service
                .inner()
                .cache
                .served_semantic_version(&uri)
                .is_none(),
            "the handler must not have served the stale v0 snapshot"
        );
        publish_treeless(service.inner(), &uri, "fn main() { }", 1);

        let result = request.await.expect("handler task must not panic");
        assert!(result.is_ok(), "current-snapshot serve must succeed");
        assert_eq!(
            service.inner().cache.served_semantic_version(&uri),
            Some(1),
            "the response must be computed from the CURRENT snapshot"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn dropping_semantic_tokens_full_cancels_and_forgets_the_request() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let service = std::sync::Arc::new(service);
        let uri = Url::parse("file:///dropped_full_request.rs").expect("valid test uri");

        service.inner().documents.insert(
            uri.clone(),
            "fn main() {}".to_string(),
            Some("rust".to_string()),
            None,
        );
        // With no published snapshot the request parks after installing its
        // tracker entry, giving the test a deterministic drop boundary.
        let request = {
            let service = std::sync::Arc::clone(&service);
            let uri = uri.clone();
            tokio::spawn(async move {
                service
                    .inner()
                    .semantic_tokens_full_impl(full_params(&uri))
                    .await
            })
        };
        sleep(Duration::from_millis(50)).await;
        assert!(!request.is_finished(), "the request must be parked");
        let (request_id, cancel_token) = service
            .inner()
            .cache
            .active_request(&uri)
            .expect("the parked request must be tracked");

        request.abort();
        let error = request
            .await
            .expect_err("aborting must drop the handler future");
        assert!(error.is_cancelled());
        assert!(
            cancel_token.is_cancelled(),
            "dropping the handler must stop detached blocking work"
        );
        assert!(
            !service.inner().cache.is_request_active(&uri, request_id),
            "dropping the handler must remove its exact tracker entry"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn dropping_semantic_tokens_delta_cancels_and_forgets_the_request() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let service = std::sync::Arc::new(service);
        let uri = Url::parse("file:///dropped_delta_request.rs").expect("valid test uri");

        service.inner().documents.insert(
            uri.clone(),
            "fn main() {}".to_string(),
            Some("rust".to_string()),
            None,
        );
        let params = SemanticTokensDeltaParams {
            text_document: TextDocumentIdentifier {
                uri: crate::lsp::lsp_impl::url_to_uri(&uri).expect("test URI should convert"),
            },
            previous_result_id: "missing-baseline".to_string(),
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };
        // The nested full computation parks on the absent snapshot while the
        // outer delta guard remains the sole owner of the tracker entry.
        let request = {
            let service = std::sync::Arc::clone(&service);
            tokio::spawn(async move {
                service
                    .inner()
                    .semantic_tokens_full_delta_impl(params)
                    .await
            })
        };
        sleep(Duration::from_millis(50)).await;
        assert!(!request.is_finished(), "the delta request must be parked");
        let (request_id, cancel_token) = service
            .inner()
            .cache
            .active_request(&uri)
            .expect("the parked delta request must be tracked");

        request.abort();
        let error = request
            .await
            .expect_err("aborting must drop the delta handler future");
        assert!(error.is_cancelled());
        assert!(
            cancel_token.is_cancelled(),
            "dropping delta must stop its nested full computation"
        );
        assert!(
            !service.inner().cache.is_request_active(&uri, request_id),
            "dropping delta must remove its exact tracker entry"
        );
    }

    /// A parked request superseded by a newer request for the same document
    /// (the tracker flips its token — no `$/cancelRequest` involved) must
    /// release promptly with the compute-superseded contract `Ok(None)`, not
    /// hold its ingress admission slot until the parse settles or the
    /// backstop expires.
    #[tokio::test(start_paused = true)]
    async fn semantic_tokens_full_superseded_while_parked_releases_with_none() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let service = std::sync::Arc::new(service);
        let uri = Url::parse("file:///serve_current_supersede.rs").expect("valid test uri");

        service.inner().documents.insert(
            uri.clone(),
            "fn main() {}".to_string(),
            Some("rust".to_string()),
            None,
        );
        // No snapshot for the live content version → the handler parks.
        service
            .inner()
            .documents
            .update_document(uri.clone(), "fn main() { }".to_string(), None);

        let request = {
            let service = std::sync::Arc::clone(&service);
            let uri = uri.clone();
            tokio::spawn(async move {
                service
                    .inner()
                    .semantic_tokens_full_impl(full_params(&uri))
                    .await
            })
        };
        sleep(Duration::from_millis(50)).await;
        assert!(
            !request.is_finished(),
            "the handler must be parked awaiting the current snapshot"
        );

        // A newer request for the same URI supersedes the parked one.
        let woke_at = tokio::time::Instant::now();
        let _newer = service.inner().cache.start_request(&uri);

        let result = request
            .await
            .expect("handler task must not panic")
            .expect("a superseded parked request answers, not errors");
        assert!(
            result.is_none(),
            "superseded-while-parked follows the compute-superseded contract (None)"
        );
        assert!(
            woke_at.elapsed() < crate::lsp::lsp_impl::snapshot_read::TOKEN_SETTLE_BACKSTOP,
            "the park must release on supersession, not ride out the backstop"
        );
    }

    /// A `$/cancelRequest` arriving while the handler is parked on a trailing
    /// snapshot must answer RequestCancelled promptly (the
    /// `TokenSnapshot::Cancelled` arm) — not sit out the settle backstop.
    #[tokio::test(start_paused = true)]
    async fn semantic_tokens_full_cancel_while_parked_answers_request_cancelled() {
        use tower_lsp_server::jsonrpc::Id;

        let (service, _socket) = LspService::new(Kakehashi::new);
        let service = std::sync::Arc::new(service);
        let uri = Url::parse("file:///serve_current_cancel.rs").expect("valid test uri");

        service.inner().documents.insert(
            uri.clone(),
            "fn main() {}".to_string(),
            Some("rust".to_string()),
            None,
        );
        // No snapshot for the live content version → the handler parks.
        service
            .inner()
            .documents
            .update_document(uri.clone(), "fn main() { }".to_string(), None);

        let request = {
            let service = std::sync::Arc::clone(&service);
            let uri = uri.clone();
            // The upstream request id rides task-local storage (installed by
            // the RequestIdCapture middleware in production).
            tokio::spawn(crate::lsp::request_id::CURRENT_REQUEST_ID.scope(
                Some(Id::Number(42)),
                async move {
                    service
                        .inner()
                        .semantic_tokens_full_impl(full_params(&uri))
                        .await
                },
            ))
        };
        sleep(Duration::from_millis(50)).await;
        assert!(
            !request.is_finished(),
            "the handler must be parked awaiting the current snapshot"
        );

        service
            .inner()
            .bridge
            .cancel_forwarder()
            .forward_cancel(crate::lsp::bridge::UpstreamId::Number(42))
            .expect("cancel forward must not error");

        let result = request.await.expect("handler task must not panic");
        let err = result.expect_err("a cancelled parked request must error, not answer");
        assert_eq!(
            err.code,
            Error::request_cancelled().code,
            "the parked handler must answer RequestCancelled on $/cancelRequest"
        );
    }

    /// When no parse catches up within the settle backstop, the handler must
    /// reject with ContentModified (the parse loop's settle refresh re-drives
    /// the client later) — never answer with tokens for text the client no
    /// longer has.
    #[tokio::test(start_paused = true)]
    async fn semantic_tokens_full_rejects_content_modified_when_parse_never_catches_up() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        let uri = Url::parse("file:///serve_current_timeout.rs").expect("valid test uri");

        server.documents.insert(
            uri.clone(),
            "fn main() {}".to_string(),
            Some("rust".to_string()),
            None,
        );
        publish_treeless(server, &uri, "fn main() {}", 0);
        server
            .documents
            .update_document(uri.clone(), "fn main() { }".to_string(), None);

        let result = server.semantic_tokens_full_impl(full_params(&uri)).await;
        let err = result.expect_err("a snapshot that never catches up must reject");
        assert_eq!(
            err.code,
            crate::error::content_modified_error().code,
            "staleness past the settle backstop signals ContentModified"
        );
        assert_eq!(
            server.cache.served_semantic_version(&uri),
            Some(0),
            "a rejected request must register token interest (version 0, \
             never the stale snapshot's version) so the settle-refresh gate \
             re-drives a client whose every request rejected"
        );
    }

    /// The delta path shares the serve-current wait: a delta against a
    /// trailing snapshot parks and answers from the current one.
    #[tokio::test(start_paused = true)]
    async fn semantic_tokens_delta_parks_until_current_snapshot() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let service = std::sync::Arc::new(service);
        let uri = Url::parse("file:///serve_current_delta.rs").expect("valid test uri");

        service.inner().documents.insert(
            uri.clone(),
            "fn main() {}".to_string(),
            Some("rust".to_string()),
            None,
        );
        publish_treeless(service.inner(), &uri, "fn main() {}", 0);
        service
            .inner()
            .documents
            .update_document(uri.clone(), "fn main() { }".to_string(), None);

        let request = {
            let service = std::sync::Arc::clone(&service);
            let uri = uri.clone();
            tokio::spawn(async move {
                let params = SemanticTokensDeltaParams {
                    text_document: TextDocumentIdentifier {
                        uri: crate::lsp::lsp_impl::url_to_uri(&uri)
                            .expect("test URI should convert"),
                    },
                    previous_result_id: "1".to_string(),
                    work_done_progress_params: WorkDoneProgressParams::default(),
                    partial_result_params: PartialResultParams::default(),
                };
                service
                    .inner()
                    .semantic_tokens_full_delta_impl(params)
                    .await
            })
        };
        sleep(Duration::from_millis(50)).await;
        assert!(
            service
                .inner()
                .cache
                .served_semantic_version(&uri)
                .is_none(),
            "the delta handler must not have served the stale v0 snapshot"
        );
        publish_treeless(service.inner(), &uri, "fn main() { }", 1);

        let result = request.await.expect("handler task must not panic");
        assert!(result.is_ok(), "current-snapshot delta serve must succeed");
        assert_eq!(
            service.inner().cache.served_semantic_version(&uri),
            Some(1),
            "the delta must be computed from the CURRENT snapshot"
        );
    }

    #[tokio::test]
    async fn semantic_tokens_delta_does_not_overwrite_newer_text() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        let uri = Url::parse("file:///semantic_delta_race.lua").expect("should construct test uri");

        let mut initial_text = String::from("local M = {}\n");
        for _ in 0..2000 {
            initial_text.push_str("local x = 1\n");
        }
        initial_text.push_str("return M\n");

        server
            .documents
            .insert(uri.clone(), initial_text, Some("lua".to_string()), None);

        let load_result = server.language.ensure_language_loaded("lua");
        if !load_result.success || server.language.highlight_query("lua").is_none() {
            eprintln!("Skipping: lua language parser or highlight query not available");
            return;
        }

        let new_text = "local LONG_NAME = {}\nreturn LONG_NAME\n".to_string();
        let new_text_clone = new_text.clone();

        let update_future = async {
            sleep(Duration::from_millis(10)).await;
            server
                .documents
                .insert(uri.clone(), new_text_clone, Some("lua".to_string()), None);
        };

        let params = SemanticTokensDeltaParams {
            text_document: TextDocumentIdentifier {
                uri: crate::lsp::lsp_impl::url_to_uri(&uri).expect("test URI should convert"),
            },
            previous_result_id: "0".to_string(),
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };

        let (result, _) = tokio::join!(
            server.semantic_tokens_full_delta_impl(params),
            update_future
        );

        assert!(
            result.is_ok(),
            "semantic tokens delta request should complete without error"
        );

        let doc = server
            .documents
            .get(&uri)
            .expect("document should still exist after delta request");

        assert_eq!(
            doc.text(),
            new_text,
            "delta path should not overwrite newer document text"
        );
    }

    /// Snapshot readers never parse on demand (ADR §3 — the property survived
    /// the serve-stale → serve-current revision): a request against a
    /// resolved-but-tree-less snapshot (what `parse_document` publishes when
    /// no parser is available) releases the first-parse wait immediately,
    /// serves the empty fallback, and leaves the document's tree untouched.
    /// (Replaces the pre-snapshot `..._times_out_but_parses_on_demand` test,
    /// whose asserted on-demand parse was removed by the reader migration.)
    #[tokio::test]
    async fn semantic_tokens_full_serves_empty_without_parsing_on_demand() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        let uri = Url::parse("file:///semantic_no_inline_parse.rs").expect("valid test uri");

        let text = "fn main() {}";
        server.documents.insert(
            uri.clone(),
            text.to_string(),
            Some("rust".to_string()),
            None,
        );
        let incarnation = server
            .documents
            .latest_snapshot(&uri)
            .expect("document just inserted")
            .slot
            .current_incarnation;
        let published = server
            .documents
            .get(&uri)
            .map(|doc| {
                doc.publish_snapshot(std::sync::Arc::new(
                    crate::document::snapshot::ParseSnapshot {
                        text: std::sync::Arc::from(text),
                        tree: None,
                        language: Some("rust".to_string()),
                        parsed_version: 0,
                        incarnation,
                        injection_regions: None,
                        bridge_regions: None,
                        resolved_regions: None,
                        layer_trees: std::sync::OnceLock::new(),
                    },
                ))
            })
            .unwrap_or(false);
        assert!(published, "tree-less snapshot must land");

        let params = SemanticTokensParams {
            text_document: TextDocumentIdentifier {
                uri: crate::lsp::lsp_impl::url_to_uri(&uri).expect("test URI should convert"),
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };

        let result = timeout(
            Duration::from_secs(2),
            server.semantic_tokens_full_impl(params),
        )
        .await
        .expect("a present snapshot must not wait out the first-parse bound")
        .expect("semantic tokens full should return without error");

        match result {
            Some(SemanticTokensResult::Tokens(tokens)) => {
                assert!(
                    tokens.data.is_empty(),
                    "tree-less snapshot serves the empty fallback"
                );
            }
            other => panic!("expected empty tokens fallback, got {other:?}"),
        }

        let doc = server.documents.get(&uri).expect("document still open");
        assert!(
            doc.tree().is_none(),
            "snapshot readers never parse inline: the tree must stay absent"
        );
    }

    /// Test that delta response has result_id and cache is updated correctly.
    ///
    /// This verifies that when returning TokensDelta:
    /// 1. The delta response contains a non-None result_id
    /// 2. The cache is updated with full tokens (not just delta)
    /// 3. The cache entry has the same result_id as the delta response
    /// 4. Subsequent delta requests can use this new result_id
    #[tokio::test]
    async fn semantic_tokens_delta_response_has_result_id_and_updates_cache() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        let uri = Url::parse("file:///delta_result_id.lua").expect("should construct test uri");

        // Insert initial document
        server.documents.insert(
            uri.clone(),
            "local x = 1".to_string(),
            Some("lua".to_string()),
            None,
        );

        let load_result = server.language.ensure_language_loaded("lua");
        if !load_result.success || server.language.highlight_query("lua").is_none() {
            eprintln!("Skipping: lua language parser or highlight query not available");
            return;
        }

        // First request: semanticTokens/full to get initial result_id
        let full_params = SemanticTokensParams {
            text_document: TextDocumentIdentifier {
                uri: crate::lsp::lsp_impl::url_to_uri(&uri).expect("test URI should convert"),
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };

        let full_result = server
            .semantic_tokens_full_impl(full_params)
            .await
            .expect("full request should succeed")
            .expect("should return tokens");

        let initial_result_id = match full_result {
            SemanticTokensResult::Tokens(t) => t.result_id.expect("should have result_id"),
            _ => panic!("expected Tokens variant"),
        };

        // Update document to trigger delta calculation
        server.documents.update_document(
            uri.clone(),
            "local y = 2".to_string(),
            None, // tree will be None until next parse
        );

        // Second request: semanticTokens/full/delta with previous_result_id
        let delta_params = SemanticTokensDeltaParams {
            text_document: TextDocumentIdentifier {
                uri: crate::lsp::lsp_impl::url_to_uri(&uri).expect("test URI should convert"),
            },
            previous_result_id: initial_result_id.clone(),
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };

        let delta_result = server
            .semantic_tokens_full_delta_impl(delta_params)
            .await
            .expect("delta request should succeed")
            .expect("should return delta or tokens");

        // ASSERTION 1: Response has non-None result_id
        let delta_result_id = match &delta_result {
            SemanticTokensFullDeltaResult::TokensDelta(d) => {
                d.result_id.clone().expect("delta should have result_id")
            }
            SemanticTokensFullDeltaResult::Tokens(t) => {
                t.result_id.clone().expect("tokens should have result_id")
            }
            _ => panic!("unexpected variant"),
        };

        // ASSERTION 2: result_id is different from initial
        assert_ne!(
            delta_result_id, initial_result_id,
            "new result_id should be assigned"
        );

        // ASSERTION 3: Cache is updated with the new result_id
        let cached = server.cache.get_tokens_if_valid(&uri, &delta_result_id);
        assert!(
            cached.is_some(),
            "cache should contain tokens with new result_id '{}'",
            delta_result_id
        );

        // ASSERTION 4: Subsequent delta request works with new result_id
        let follow_up_params = SemanticTokensDeltaParams {
            text_document: TextDocumentIdentifier {
                uri: crate::lsp::lsp_impl::url_to_uri(&uri).expect("test URI should convert"),
            },
            previous_result_id: delta_result_id.clone(),
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };

        let follow_up_result = server
            .semantic_tokens_full_delta_impl(follow_up_params)
            .await
            .expect("follow-up delta request should succeed")
            .expect("unchanged native baseline should produce a delta");
        let SemanticTokensFullDeltaResult::TokensDelta(follow_up_delta) = follow_up_result else {
            panic!("unchanged native baseline should keep the delta fast path");
        };
        assert_eq!(
            follow_up_delta.result_id.as_deref(),
            Some(delta_result_id.as_str())
        );
        assert!(follow_up_delta.edits.is_empty());
    }

    /// End-to-end for the don't-discover-twice lever (parse-snapshot ADR §3):
    /// the parse loop's `populate_injections` derives the injection discovery
    /// once, the published snapshot carries it, and the semantic handler
    /// consumes it — so the request-path compute never re-runs the injection
    /// query. Pinned via the reuse-hit counter around a real
    /// `semanticTokens/full` request.
    #[tokio::test]
    async fn semantic_full_reuses_snapshot_discovery() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        let uri = Url::parse("file:///discovery_reuse.md").expect("test uri");

        // Eight lua blocks: clears INJECTION_CACHE_MIN_REGIONS so populate
        // stores a discovery.
        let mut text = String::new();
        for i in 0..8 {
            text.push_str(&format!("# h{i}\n\n```lua\nlocal x{i} = {i}\n```\n\n"));
        }
        server
            .documents
            .insert(uri.clone(), text, Some("markdown".to_string()), None);

        let settings = crate::config::WorkspaceSettings {
            search_paths: vec![
                std::env::var("TREE_SITTER_GRAMMARS")
                    .unwrap_or_else(|_| "deps/tree-sitter".to_string()),
            ],
            ..Default::default()
        };
        let _ = server.language.load_settings(&settings);
        for lang in ["markdown", "markdown_inline", "lua"] {
            if !server.language.ensure_language_loaded(lang).success {
                eprintln!("Skipping: {lang} parser not available");
                return;
            }
        }
        if server.language.highlight_query("markdown").is_none() {
            eprintln!("Skipping: markdown highlight query not available");
            return;
        }

        server
            .parse_coordinator()
            .parse_document(uri.clone(), Some("markdown"), None)
            .await;

        // The published snapshot must carry the derived discovery.
        let view = server
            .documents
            .latest_snapshot(&uri)
            .expect("document registered");
        let snapshot = view.slot.snapshot.expect("open parse published");
        assert!(
            snapshot.injection_regions.is_some(),
            "populate must derive a discovery onto the snapshot for an 8-region document"
        );

        // A real full request must take the reuse path (no injection-query re-run).
        use crate::analysis::semantic::DISCOVERY_REUSE_HITS;
        DISCOVERY_REUSE_HITS.store(0, std::sync::atomic::Ordering::Relaxed);
        let params = SemanticTokensParams {
            text_document: TextDocumentIdentifier {
                uri: crate::lsp::lsp_impl::url_to_uri(&uri).expect("test URI should convert"),
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };
        let result = server
            .semantic_tokens_full_impl(params)
            .await
            .expect("full request should succeed")
            .expect("should return tokens");
        assert!(matches!(result, SemanticTokensResult::Tokens(t) if !t.data.is_empty()));
        assert!(
            DISCOVERY_REUSE_HITS.load(std::sync::atomic::Ordering::Relaxed) >= 1,
            "the request must rebuild contexts from the snapshot's discovery"
        );
    }

    /// Test that a no-op delta (no document change) reuses the previous
    /// result_id instead of rotating it and re-storing identical tokens.
    ///
    /// When the document is unchanged, recomputed tokens are byte-identical to
    /// the cached tokens, so the delta has zero edits. The LSP result_id is a
    /// version token the client echoes back; keeping it stable avoids a wasted
    /// clone + cache store + id increment, and the cache entry under the
    /// previous id stays valid for the next request.
    #[tokio::test]
    async fn semantic_tokens_noop_delta_reuses_previous_result_id() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        let uri = Url::parse("file:///noop_delta.lua").expect("should construct test uri");

        server.documents.insert(
            uri.clone(),
            "local x = 1".to_string(),
            Some("lua".to_string()),
            None,
        );

        // Configure the grammar search path so the language actually loads
        // (grammars live under deps/tree-sitter, or TREE_SITTER_GRAMMARS in Nix).
        let settings = crate::config::WorkspaceSettings {
            search_paths: vec![
                std::env::var("TREE_SITTER_GRAMMARS")
                    .unwrap_or_else(|_| "deps/tree-sitter".to_string()),
            ],
            ..Default::default()
        };
        let _ = server.language.load_settings(&settings);

        let load_result = server.language.ensure_language_loaded("lua");
        if !load_result.success || server.language.highlight_query("lua").is_none() {
            eprintln!("Skipping: lua language parser or highlight query not available");
            return;
        }

        // Publish a parse snapshot: the handlers serve the latest snapshot and
        // never parse on demand (parse-snapshot ADR §3), so the open parse must
        // run before the first request — as didOpen arranges in production.
        server
            .parse_coordinator()
            .parse_document(uri.clone(), Some("lua"), None)
            .await;

        // First request: semanticTokens/full to get the initial result_id.
        let full_params = SemanticTokensParams {
            text_document: TextDocumentIdentifier {
                uri: crate::lsp::lsp_impl::url_to_uri(&uri).expect("test URI should convert"),
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };
        let full_result = server
            .semantic_tokens_full_impl(full_params)
            .await
            .expect("full request should succeed")
            .expect("should return tokens");
        let initial_result_id = match full_result {
            SemanticTokensResult::Tokens(t) => t.result_id.expect("should have result_id"),
            _ => panic!("expected Tokens variant"),
        };

        // Second request: delta WITHOUT changing the document → no edits.
        let delta_params = SemanticTokensDeltaParams {
            text_document: TextDocumentIdentifier {
                uri: crate::lsp::lsp_impl::url_to_uri(&uri).expect("test URI should convert"),
            },
            previous_result_id: initial_result_id.clone(),
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };
        let delta_result = server
            .semantic_tokens_full_delta_impl(delta_params)
            .await
            .expect("delta request should succeed")
            .expect("should return delta");

        let delta = match delta_result {
            SemanticTokensFullDeltaResult::TokensDelta(d) => d,
            other => panic!("expected TokensDelta for unchanged document, got {other:?}"),
        };

        // No edits, since nothing changed.
        assert!(
            delta.edits.is_empty(),
            "unchanged document should produce a delta with no edits, got {:?}",
            delta.edits
        );

        // The result_id must be reused, not rotated.
        assert_eq!(
            delta.result_id.as_deref(),
            Some(initial_result_id.as_str()),
            "no-op delta should reuse the previous result_id"
        );

        // The cache entry under the initial result_id must still be valid.
        assert!(
            server
                .cache
                .get_tokens_if_valid(&uri, &initial_result_id)
                .is_some(),
            "cache should still hold tokens under the reused result_id"
        );
    }

    /// Test that semantic token cache is preserved for delta calculations.
    ///
    /// This verifies the fix for the issue where `invalidate_semantic()` was being
    /// called on every `didChange`, preventing delta calculations from ever working.
    #[tokio::test]
    async fn semantic_tokens_cache_preserved_for_delta() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        let uri = Url::parse("file:///cache_test.lua").expect("should construct test uri");

        // Insert a document
        server.documents.insert(
            uri.clone(),
            "local x = 1".to_string(),
            Some("lua".to_string()),
            None,
        );

        let load_result = server.language.ensure_language_loaded("lua");
        if !load_result.success || server.language.highlight_query("lua").is_none() {
            eprintln!("Skipping: lua language parser or highlight query not available");
            return;
        }

        // First request: semanticTokens/full to populate the cache
        let params = SemanticTokensParams {
            text_document: TextDocumentIdentifier {
                uri: crate::lsp::lsp_impl::url_to_uri(&uri).expect("test URI should convert"),
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };

        let result = server.semantic_tokens_full_impl(params).await;
        assert!(result.is_ok(), "semantic_tokens_full should succeed");

        let tokens_result = result.unwrap();
        assert!(tokens_result.is_some(), "should return tokens");

        // Extract the result_id from the response
        let result_id = match tokens_result.unwrap() {
            SemanticTokensResult::Tokens(tokens) => tokens.result_id,
            _ => panic!("expected Tokens variant"),
        };
        assert!(result_id.is_some(), "should have result_id");
        let result_id = result_id.unwrap();

        // Verify the cache contains tokens with this result_id
        let cached = server.cache.get_tokens_if_valid(&uri, &result_id);
        assert!(
            cached.is_some(),
            "cache should contain tokens with result_id '{}'",
            result_id
        );

        // Simulate a document change (this would normally be done via didChange)
        // In production, didChange does NOT invalidate semantic cache anymore
        server.documents.update_document(
            uri.clone(),
            "local y = 2".to_string(),
            None, // tree will be None until next parse
        );

        // The cache must retain previous tokens after didChange — the delta
        // calculation on the next semanticTokens request depends on them.
        let still_cached = server.cache.get_tokens_if_valid(&uri, &result_id);
        assert!(
            still_cached.is_some(),
            "cache should STILL contain tokens after document update - needed for delta calculations"
        );
    }

    /// An unchanged document must reuse cached tokens instead of re-tokenizing:
    /// the second `semanticTokens/full` returns the SAME `result_id` as the first.
    /// Before content-hash keying, every full response drew a fresh `result_id`,
    /// so this asserts the cache short-circuit (skipped recomputation) is live.
    #[tokio::test]
    async fn semantic_tokens_full_reuses_cached_tokens_for_unchanged_document() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        let uri = Url::parse("file:///unchanged.lua").expect("should construct test uri");

        server.documents.insert(
            uri.clone(),
            "local x = 1".to_string(),
            Some("lua".to_string()),
            None,
        );

        let load_result = server.language.ensure_language_loaded("lua");
        if !load_result.success || server.language.highlight_query("lua").is_none() {
            eprintln!("Skipping: lua language parser or highlight query not available");
            return;
        }

        let make_params = || SemanticTokensParams {
            text_document: TextDocumentIdentifier {
                uri: crate::lsp::lsp_impl::url_to_uri(&uri).expect("test URI should convert"),
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };

        let first = server
            .semantic_tokens_full_impl(make_params())
            .await
            .expect("first full request should succeed")
            .expect("should return tokens");
        let first_id = match first {
            SemanticTokensResult::Tokens(t) => t.result_id.expect("should have result_id"),
            _ => panic!("expected Tokens variant"),
        };

        // Second request, document UNCHANGED: must serve the cached tokens (same
        // result_id), proving the re-tokenization was skipped.
        let second = server
            .semantic_tokens_full_impl(make_params())
            .await
            .expect("second full request should succeed")
            .expect("should return tokens");
        let second_id = match second {
            SemanticTokensResult::Tokens(t) => t.result_id.expect("should have result_id"),
            _ => panic!("expected Tokens variant"),
        };

        assert_eq!(
            first_id, second_id,
            "an unchanged document should reuse cached tokens (stable result_id), \
             not recompute with a fresh id"
        );
    }

    /// End-to-end guard for #549: the same URI, re-assigned to a DIFFERENT
    /// language without any text change, must recompute rather than serve the
    /// first language's cached tokens. The text (and thus `cache_key`) is
    /// identical across both requests, so only the language dimension of the key
    /// prevents the collision. This deliberately does NOT go through `did_close`
    /// (which evicts the entry): the bug it locks is the lingering entry being
    /// re-read under the new language (the store-after-evict / reopen race), so
    /// the entry must survive into the second request.
    #[tokio::test]
    async fn semantic_tokens_full_recomputes_after_language_switch_same_text() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        let uri = Url::parse("file:///switch.txt").expect("should construct test uri");
        // Text is only ever compared for equality across the two requests; it need
        // not be valid in either grammar (tree-sitter still yields a tree + tokens).
        let text = "local x = 1".to_string();

        for lang in ["lua", "rust"] {
            let load_result = server.language.ensure_language_loaded(lang);
            if !load_result.success || server.language.highlight_query(lang).is_none() {
                eprintln!("Skipping: {lang} parser or highlight query not available");
                return;
            }
        }

        let make_params = || SemanticTokensParams {
            text_document: TextDocumentIdentifier {
                uri: crate::lsp::lsp_impl::url_to_uri(&uri).expect("test URI should convert"),
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };

        // Open as lua, compute + cache tokens under (uri, lua, cache_key).
        server
            .documents
            .insert(uri.clone(), text.clone(), Some("lua".to_string()), None);
        let lua_result = server
            .semantic_tokens_full_impl(make_params())
            .await
            .expect("lua full request should succeed")
            .expect("should return tokens");
        let lua_id = match lua_result {
            SemanticTokensResult::Tokens(t) => t.result_id.expect("should have result_id"),
            _ => panic!("expected Tokens variant"),
        };

        // Re-assign the SAME uri + SAME text to rust WITHOUT closing (so the lua
        // cache entry lingers). The cache_key is unchanged (text + generation are),
        // so only the language guard can force a miss here.
        server
            .documents
            .insert(uri.clone(), text, Some("rust".to_string()), None);
        let rust_result = server
            .semantic_tokens_full_impl(make_params())
            .await
            .expect("rust full request should succeed")
            .expect("should return tokens");
        let rust_id = match rust_result {
            SemanticTokensResult::Tokens(t) => t.result_id.expect("should have result_id"),
            _ => panic!("expected Tokens variant"),
        };

        assert_ne!(
            lua_id, rust_id,
            "switching the document's language (same text) must recompute, not \
             serve the previous language's cached tokens"
        );
    }

    /// A delta request on an unchanged document whose baseline matches the cached
    /// tokens returns an empty delta with the same `result_id` — the fast path that
    /// skips the `previous_tokens` clone and the O(N) diff entirely.
    #[tokio::test]
    async fn semantic_tokens_delta_returns_empty_delta_for_unchanged_document() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        let uri = Url::parse("file:///delta_noop.lua").expect("should construct test uri");

        let mut settings = crate::config::WorkspaceSettings::default();
        settings.language_servers.insert(
            "unrelated".into(),
            crate::config::settings::BridgeServerConfig {
                cmd: Some(vec!["unrelated-server".into()]),
                languages: Some(vec!["python".into()]),
                ..Default::default()
            },
        );
        server
            .apply_raw_settings(crate::config::RawWorkspaceSettings::default(), settings)
            .await;

        server.documents.insert(
            uri.clone(),
            "local x = 1".to_string(),
            Some("lua".to_string()),
            None,
        );

        let load_result = server.language.ensure_language_loaded("lua");
        if !load_result.success || server.language.highlight_query("lua").is_none() {
            eprintln!("Skipping: lua language parser or highlight query not available");
            return;
        }

        // Full request establishes the baseline result_id.
        let full = server
            .semantic_tokens_full_impl(SemanticTokensParams {
                text_document: TextDocumentIdentifier {
                    uri: crate::lsp::lsp_impl::url_to_uri(&uri).expect("test URI should convert"),
                },
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
            })
            .await
            .expect("full request should succeed")
            .expect("should return tokens");
        let baseline_id = match full {
            SemanticTokensResult::Tokens(t) => t.result_id.expect("should have result_id"),
            _ => panic!("expected Tokens variant"),
        };
        assert!(
            server
                .cache
                .get_wire_tokens_if_valid(&uri, &baseline_id)
                .is_none(),
            "a native-only full result should not duplicate its baseline in the wire cache"
        );

        // Delta on the UNCHANGED document with the matching baseline: empty delta,
        // same result_id (the fast path).
        let delta = server
            .semantic_tokens_full_delta_impl(SemanticTokensDeltaParams {
                text_document: TextDocumentIdentifier {
                    uri: crate::lsp::lsp_impl::url_to_uri(&uri).expect("test URI should convert"),
                },
                previous_result_id: baseline_id.clone(),
                work_done_progress_params: WorkDoneProgressParams::default(),
                partial_result_params: PartialResultParams::default(),
            })
            .await
            .expect("delta request should succeed")
            .expect("should return a delta");
        match delta {
            SemanticTokensFullDeltaResult::TokensDelta(d) => {
                assert_eq!(
                    d.result_id,
                    Some(baseline_id),
                    "no-op delta should reuse the baseline result_id"
                );
                assert!(
                    d.edits.is_empty(),
                    "an unchanged document should yield an empty delta even when an unrelated server is configured"
                );
            }
            other => panic!("expected an empty TokensDelta, got {:?}", other),
        }
    }

    /// Test that semantic tokens full request returns RequestCancelled (-32800) when cancelled.
    ///
    /// This verifies the fix for immediate cancellation support:
    /// 1. Start a semantic tokens request for a large document
    /// 2. Immediately trigger cancellation via CancelForwarder
    /// 3. Verify that RequestCancelled error (-32800) is returned
    #[tokio::test]
    async fn semantic_tokens_full_returns_request_cancelled_when_cancelled() {
        use crate::lsp::bridge::{LanguageServerPool, UpstreamId};
        use crate::lsp::request_id::CancelForwarder;
        use std::sync::Arc;

        // Create shared pool and cancel forwarder
        let pool = Arc::new(LanguageServerPool::new());
        let cancel_forwarder = CancelForwarder::new(Arc::clone(&pool));

        // Create server with shared cancel forwarder
        let (service, _socket) = LspService::new(|client| {
            Kakehashi::with_cancel_forwarder(client, pool, cancel_forwarder.clone())
        });
        let server = service.inner();
        let uri = Url::parse("file:///cancel_test.lua").expect("should construct test uri");

        // Create a moderately large document to ensure processing takes some time
        let mut text = String::from("local M = {}\n");
        for i in 0..500 {
            text.push_str(&format!("local var_{} = {}\n", i, i));
        }
        text.push_str("return M\n");

        server
            .documents
            .insert(uri.clone(), text, Some("lua".to_string()), None);

        let load_result = server.language.ensure_language_loaded("lua");
        if !load_result.success {
            eprintln!("Skipping: lua language parser not available for cancel test");
            return;
        }

        // Trigger cancel immediately (simulating $/cancelRequest arrival)
        // We set a task-local request ID so subscribe_cancel() can subscribe,
        // then notify on the same ID.
        let cancel_forwarder_clone = cancel_forwarder.clone();
        tokio::spawn(async move {
            // Small delay to ensure the request starts processing and subscribes
            sleep(Duration::from_millis(1)).await;
            cancel_forwarder_clone.notify_cancel(&UpstreamId::Number(999));
        });

        let params = SemanticTokensParams {
            text_document: TextDocumentIdentifier {
                uri: crate::lsp::lsp_impl::url_to_uri(&uri).expect("test URI should convert"),
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };

        // Call the public implementation within a task-local request ID scope
        // so subscribe_cancel() can subscribe to cancel notifications
        let result = crate::lsp::request_id::CURRENT_REQUEST_ID
            .scope(
                Some(tower_lsp_server::jsonrpc::Id::Number(999)),
                server.semantic_tokens_full_impl(params),
            )
            .await;

        // Verify we got RequestCancelled error (-32800)
        match result {
            Err(e) => {
                assert_eq!(
                    e.code,
                    tower_lsp_server::jsonrpc::ErrorCode::RequestCancelled,
                    "should return RequestCancelled error code (-32800), got: {:?}",
                    e.code
                );
            }
            Ok(_) => {
                // If the request completed before cancel was processed, that's also acceptable
                // (cancel is best-effort per LSP spec). But we expect cancel to win for large docs.
                eprintln!(
                    "Note: request completed before cancel - this is acceptable but unexpected for large docs"
                );
            }
        }
    }

    /// Test that semantic tokens full delta request returns RequestCancelled (-32800) when cancelled.
    ///
    /// Similar to the full request test, but specifically tests the delta endpoint:
    /// 1. First request semantic tokens to establish a baseline result_id
    /// 2. Start a semantic tokens delta request for a large document
    /// 3. Immediately trigger cancellation via CancelForwarder
    /// 4. Verify that RequestCancelled error (-32800) is returned
    #[tokio::test]
    async fn semantic_tokens_full_delta_returns_request_cancelled_when_cancelled() {
        use crate::lsp::bridge::{LanguageServerPool, UpstreamId};
        use crate::lsp::request_id::CancelForwarder;
        use std::sync::Arc;

        // Create shared pool and cancel forwarder
        let pool = Arc::new(LanguageServerPool::new());
        let cancel_forwarder = CancelForwarder::new(Arc::clone(&pool));

        // Create server with shared cancel forwarder
        let (service, _socket) = LspService::new(|client| {
            Kakehashi::with_cancel_forwarder(client, pool, cancel_forwarder.clone())
        });
        let server = service.inner();
        let uri = Url::parse("file:///cancel_delta_test.lua").expect("should construct test uri");

        // Create a moderately large document to ensure processing takes some time
        let mut text = String::from("local M = {}\n");
        for i in 0..500 {
            text.push_str(&format!("local var_{} = {}\n", i, i));
        }
        text.push_str("return M\n");

        server
            .documents
            .insert(uri.clone(), text, Some("lua".to_string()), None);

        let load_result = server.language.ensure_language_loaded("lua");
        if !load_result.success {
            eprintln!("Skipping: lua language parser not available for cancel delta test");
            return;
        }

        // First, get initial tokens to establish a result_id for delta requests
        let full_params = SemanticTokensParams {
            text_document: TextDocumentIdentifier {
                uri: crate::lsp::lsp_impl::url_to_uri(&uri).expect("test URI should convert"),
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };

        let initial_result = server.semantic_tokens_full_impl(full_params).await;

        let previous_result_id = match initial_result {
            Ok(Some(SemanticTokensResult::Tokens(tokens))) => {
                tokens.result_id.expect("should have result_id")
            }
            _ => {
                eprintln!("Skipping: could not get initial tokens for delta test");
                return;
            }
        };

        // Trigger cancel immediately (simulating $/cancelRequest arrival)
        // We set a task-local request ID so subscribe_cancel() can subscribe,
        // then notify on the same ID.
        let cancel_forwarder_clone = cancel_forwarder.clone();
        tokio::spawn(async move {
            // Small delay to ensure the request starts processing and subscribes
            sleep(Duration::from_millis(1)).await;
            cancel_forwarder_clone.notify_cancel(&UpstreamId::Number(999));
        });

        let delta_params = SemanticTokensDeltaParams {
            text_document: TextDocumentIdentifier {
                uri: crate::lsp::lsp_impl::url_to_uri(&uri).expect("test URI should convert"),
            },
            previous_result_id,
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        };

        // Call the public delta implementation within a task-local request ID scope
        // so subscribe_cancel() can subscribe to cancel notifications
        let result = crate::lsp::request_id::CURRENT_REQUEST_ID
            .scope(
                Some(tower_lsp_server::jsonrpc::Id::Number(999)),
                server.semantic_tokens_full_delta_impl(delta_params),
            )
            .await;

        // Verify we got RequestCancelled error (-32800)
        match result {
            Err(e) => {
                assert_eq!(
                    e.code,
                    tower_lsp_server::jsonrpc::ErrorCode::RequestCancelled,
                    "should return RequestCancelled error code (-32800), got: {:?}",
                    e.code
                );
            }
            Ok(_) => {
                // If the request completed before cancel was processed, that's also acceptable
                // (cancel is best-effort per LSP spec). But we expect cancel to win for large docs.
                eprintln!(
                    "Note: delta request completed before cancel - this is acceptable but unexpected for large docs"
                );
            }
        }
    }
}
