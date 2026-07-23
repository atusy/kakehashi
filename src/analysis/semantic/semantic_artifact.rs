use crate::analysis::SemanticSnapshotIdentity;
use futures::FutureExt;
use futures::future::{BoxFuture, Shared};
use std::sync::Arc;
use tower_lsp_server::ls_types::{SemanticTokens, SemanticTokensResult};
use url::Url;

/// All request-independent inputs that identify one semantic-token artifact.
///
/// The artifact remains scoped to one immutable parse snapshot. Response-local
/// state such as an LSP `resultId` is deliberately not part of this identity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SemanticArtifactIdentity {
    uri: Url,
    language: String,
    snapshot: SemanticSnapshotIdentity,
    supports_multiline: bool,
}

impl SemanticArtifactIdentity {
    pub(crate) fn new(
        uri: Url,
        language: String,
        snapshot: SemanticSnapshotIdentity,
        supports_multiline: bool,
    ) -> Self {
        Self {
            uri,
            language,
            snapshot,
            supports_multiline,
        }
    }

    pub(crate) fn expected<'a>(
        uri: &'a Url,
        language: &'a str,
        snapshot: SemanticSnapshotIdentity,
        supports_multiline: bool,
    ) -> SemanticArtifactIdentityRef<'a> {
        SemanticArtifactIdentityRef {
            uri,
            language,
            snapshot,
            supports_multiline,
        }
    }

    #[cfg(test)]
    fn as_ref(&self) -> SemanticArtifactIdentityRef<'_> {
        Self::expected(
            &self.uri,
            &self.language,
            self.snapshot,
            self.supports_multiline,
        )
    }

    fn matches(&self, expected: SemanticArtifactIdentityRef<'_>) -> bool {
        self.uri == *expected.uri
            && self.language == expected.language
            && self.snapshot == expected.snapshot
            && self.supports_multiline == expected.supports_multiline
    }
}

/// Allocation-free request view used to validate a reusable artifact.
///
/// Stage 2 constructs the artifact locally, so this comparison is expected to
/// succeed. Keeping the authoritative request inputs separate makes the same
/// check non-tautological when Stage 3 retrieves an artifact from a snapshot
/// slot, without cloning its URI or language for every lookup.
#[derive(Clone, Copy)]
pub(crate) struct SemanticArtifactIdentityRef<'a> {
    uri: &'a Url,
    language: &'a str,
    snapshot: SemanticSnapshotIdentity,
    supports_multiline: bool,
}

impl SemanticArtifactIdentityRef<'_> {
    pub(crate) fn to_owned(self) -> SemanticArtifactIdentity {
        SemanticArtifactIdentity::new(
            self.uri.clone(),
            self.language.to_owned(),
            self.snapshot,
            self.supports_multiline,
        )
    }
}

/// Complete immutable semantic output for one [`SemanticArtifactIdentity`].
///
/// Construction accepts only a complete full result. The data stays private
/// until a request materializes it and supplies its own LSP `resultId`.
pub(crate) struct SemanticArtifact {
    identity: SemanticArtifactIdentity,
    tokens: SemanticTokens,
}

/// One generation-aware single-flight slot owned by a [`ParseSnapshot`].
///
/// The slot retains either the in-progress shared future or its completed
/// immutable artifact. A different identity atomically replaces and cancels
/// the previous attempt. Failed/cancelled attempts compare-and-remove
/// themselves so the same identity can be retried; an old attempt can never
/// clear its replacement.
///
/// [`ParseSnapshot`]: crate::document::snapshot::ParseSnapshot
pub(crate) struct SemanticArtifactSlot {
    attempt: std::sync::Mutex<Option<SemanticArtifactAttempt>>,
    minimum_generation: std::sync::atomic::AtomicU64,
    retired: std::sync::atomic::AtomicBool,
    #[cfg(test)]
    producer_starts: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    joins: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    fail_next_producer: std::sync::atomic::AtomicBool,
}

struct SemanticArtifactAttempt {
    identity: SemanticArtifactIdentity,
    control: Arc<SemanticArtifactAttemptControl>,
    future: SharedArtifactFuture,
}

struct SemanticArtifactAttemptControl {
    id: u64,
    cancel: crate::cancel::CancelToken,
    consumers: std::sync::atomic::AtomicUsize,
    complete: std::sync::atomic::AtomicBool,
    slot: std::sync::Weak<SemanticArtifactSlot>,
}

type SharedArtifactFuture = Shared<BoxFuture<'static, Option<Arc<SemanticArtifact>>>>;

/// One request-local interest in a shared artifact attempt.
///
/// Dropping the last consumer cancels a still-running request-driven producer.
/// The slot's retained future does not count as interest: it exists only so a
/// concurrent consumer can join without duplicating work.
pub(crate) struct SemanticArtifactConsumer {
    future: SharedArtifactFuture,
    control: Arc<SemanticArtifactAttemptControl>,
}

impl std::future::Future for SemanticArtifactConsumer {
    type Output = Option<Arc<SemanticArtifact>>;

    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        std::pin::Pin::new(&mut self.future).poll(cx)
    }
}

impl Drop for SemanticArtifactConsumer {
    fn drop(&mut self) {
        if self
            .control
            .consumers
            .fetch_sub(1, std::sync::atomic::Ordering::AcqRel)
            == 1
            && !self
                .control
                .complete
                .load(std::sync::atomic::Ordering::Acquire)
        {
            if let Some(slot) = self.control.slot.upgrade() {
                slot.cancel_unobserved_attempt(&self.control);
            } else {
                self.control.cancel.cancel();
            }
        }
    }
}

static NEXT_ATTEMPT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

impl SemanticArtifactConsumer {
    fn unavailable() -> Self {
        let cancel = crate::cancel::CancelToken::default();
        cancel.cancel();
        Self {
            future: futures::future::ready(None).boxed().shared(),
            control: Arc::new(SemanticArtifactAttemptControl {
                id: 0,
                cancel,
                consumers: std::sync::atomic::AtomicUsize::new(1),
                complete: std::sync::atomic::AtomicBool::new(true),
                slot: std::sync::Weak::new(),
            }),
        }
    }
}

impl SemanticArtifactSlot {
    pub(crate) fn new() -> Self {
        Self {
            attempt: std::sync::Mutex::new(None),
            minimum_generation: std::sync::atomic::AtomicU64::new(0),
            retired: std::sync::atomic::AtomicBool::new(false),
            #[cfg(test)]
            producer_starts: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(test)]
            joins: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(test)]
            fail_next_producer: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Join the attempt for `identity`, or install one lazy producer.
    ///
    /// `produce` is called only by the elected producer and receives the
    /// revision-owned cancellation token. Dropping one returned future merely
    /// detaches that consumer: the slot retains another clone, so another
    /// consumer can still join the current-revision work.
    pub(crate) fn claim_or_join<F, Fut>(
        self: &Arc<Self>,
        identity: SemanticArtifactIdentity,
        produce: F,
    ) -> SemanticArtifactConsumer
    where
        F: FnOnce(crate::cancel::CancelToken, SemanticArtifactIdentity) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Option<SemanticArtifact>> + Send + 'static,
    {
        let mut attempt = self.attempt.lock().unwrap_or_else(|e| e.into_inner());
        if self.retired.load(std::sync::atomic::Ordering::Acquire)
            || identity.snapshot.generation
                < self
                    .minimum_generation
                    .load(std::sync::atomic::Ordering::Acquire)
        {
            return SemanticArtifactConsumer::unavailable();
        }
        if let Some(current) = attempt.as_ref()
            && current.identity == identity
        {
            current
                .control
                .consumers
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            #[cfg(test)]
            self.joins.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            return SemanticArtifactConsumer {
                future: current.future.clone(),
                control: Arc::clone(&current.control),
            };
        }

        if attempt.as_ref().is_some_and(|current| {
            identity.snapshot.generation < current.identity.snapshot.generation
        }) {
            return SemanticArtifactConsumer::unavailable();
        }

        if let Some(previous) = attempt.take() {
            previous.control.cancel.cancel();
        }

        let id = NEXT_ATTEMPT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let cancel = crate::cancel::CancelToken::default();
        let producer_cancel = cancel.clone();
        let weak_slot = Arc::downgrade(self);
        let control = Arc::new(SemanticArtifactAttemptControl {
            id,
            cancel,
            consumers: std::sync::atomic::AtomicUsize::new(1),
            complete: std::sync::atomic::AtomicBool::new(false),
            slot: std::sync::Weak::clone(&weak_slot),
        });
        let producer_control = Arc::clone(&control);
        let producer_identity = identity.clone();
        let future = async move {
            #[cfg(test)]
            let force_failure = weak_slot.upgrade().is_some_and(|slot| {
                slot.producer_starts
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                slot.fail_next_producer
                    .swap(false, std::sync::atomic::Ordering::SeqCst)
            });
            #[cfg(not(test))]
            let force_failure = false;
            let artifact = if force_failure {
                None
            } else {
                match std::panic::AssertUnwindSafe(produce(producer_cancel, producer_identity))
                    .catch_unwind()
                    .await
                {
                    Ok(artifact) => artifact.map(Arc::new),
                    Err(_) => {
                        log::error!(
                            target: "kakehashi::crash_recovery",
                            "semantic artifact producer future panicked"
                        );
                        None
                    }
                }
            };
            producer_control
                .complete
                .store(true, std::sync::atomic::Ordering::Release);
            if artifact.is_none()
                && let Some(slot) = weak_slot.upgrade()
            {
                slot.remove_failed_attempt(id);
            }
            artifact
        }
        .boxed()
        .shared();

        *attempt = Some(SemanticArtifactAttempt {
            identity,
            control: Arc::clone(&control),
            future: future.clone(),
        });
        SemanticArtifactConsumer { future, control }
    }

    /// Retire this slot because the owning snapshot is no longer current.
    ///
    /// Retirement is sticky: a request that captured the snapshot before an
    /// edit/replacement cannot install fresh work after the invalidation.
    pub(crate) fn cancel(&self) {
        let attempt = self.attempt.lock().unwrap_or_else(|e| e.into_inner());
        self.retired
            .store(true, std::sync::atomic::Ordering::Release);
        if let Some(attempt) = attempt.as_ref() {
            attempt.control.cancel.cancel();
        }
    }

    /// Reject attempts from generations older than `minimum_generation`.
    ///
    /// Unlike snapshot retirement, a settings reload keeps the parse snapshot
    /// usable. It only cancels/removes an older-generation producer and allows a
    /// request using the new settings generation to install its replacement.
    pub(crate) fn advance_minimum_generation(&self, minimum_generation: u64) {
        let mut attempt = self.attempt.lock().unwrap_or_else(|e| e.into_inner());
        let current_minimum = self
            .minimum_generation
            .load(std::sync::atomic::Ordering::Acquire);
        if minimum_generation <= current_minimum {
            return;
        }
        self.minimum_generation
            .store(minimum_generation, std::sync::atomic::Ordering::Release);
        if attempt
            .as_ref()
            .is_some_and(|attempt| attempt.identity.snapshot.generation < minimum_generation)
            && let Some(previous) = attempt.take()
        {
            previous.control.cancel.cancel();
        }
    }

    fn remove_failed_attempt(&self, id: u64) {
        let mut attempt = self.attempt.lock().unwrap_or_else(|e| e.into_inner());
        if attempt
            .as_ref()
            .is_some_and(|attempt| attempt.control.id == id)
        {
            *attempt = None;
        }
    }

    fn cancel_unobserved_attempt(&self, control: &Arc<SemanticArtifactAttemptControl>) {
        let mut attempt = self.attempt.lock().unwrap_or_else(|e| e.into_inner());
        let should_cancel = attempt.as_ref().is_some_and(|attempt| {
            Arc::ptr_eq(&attempt.control, control)
                && control.consumers.load(std::sync::atomic::Ordering::Acquire) == 0
                && !control.complete.load(std::sync::atomic::Ordering::Acquire)
        });
        if should_cancel {
            control.cancel.cancel();
            *attempt = None;
        }
    }

    #[cfg(test)]
    pub(crate) fn test_producer_starts(&self) -> usize {
        self.producer_starts
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    #[cfg(test)]
    pub(crate) fn test_joins(&self) -> usize {
        self.joins.load(std::sync::atomic::Ordering::SeqCst)
    }

    #[cfg(test)]
    pub(crate) fn test_fail_next_producer(&self) {
        self.fail_next_producer
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    #[cfg(test)]
    pub(crate) fn test_minimum_generation(&self) -> u64 {
        self.minimum_generation
            .load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl Default for SemanticArtifactSlot {
    fn default() -> Self {
        Self::new()
    }
}

impl SemanticArtifact {
    pub(crate) fn from_full_result(
        identity: SemanticArtifactIdentity,
        result: SemanticTokensResult,
    ) -> Option<Self> {
        let SemanticTokensResult::Tokens(mut tokens) = result else {
            return None;
        };
        tokens.result_id = None;
        Some(Self { identity, tokens })
    }

    #[cfg(test)]
    fn into_full(
        mut self,
        expected_identity: SemanticArtifactIdentityRef<'_>,
        result_id: Option<String>,
    ) -> Option<SemanticTokens> {
        if !self.identity.matches(expected_identity) {
            return None;
        }
        self.tokens.result_id = result_id;
        Some(self.tokens)
    }

    /// Materialize one request response while retaining the artifact for other
    /// consumers of the same snapshot slot.
    ///
    /// Shared-slot consumers pay the wire-payload clone required for each
    /// independent LSP response while retaining one immutable artifact.
    pub(crate) fn materialize_full(
        &self,
        expected_identity: SemanticArtifactIdentityRef<'_>,
        result_id: Option<String>,
    ) -> Option<SemanticTokens> {
        if !self.identity.matches(expected_identity) {
            return None;
        }
        let mut tokens = self.tokens.clone();
        tokens.result_id = result_id;
        Some(tokens)
    }
}

#[cfg(test)]
mod tests {
    use super::{SemanticArtifact, SemanticArtifactIdentity, SemanticArtifactSlot};
    use crate::analysis::SemanticSnapshotIdentity;
    use tower_lsp_server::ls_types::{
        SemanticToken, SemanticTokens, SemanticTokensPartialResult, SemanticTokensResult,
    };
    use url::Url;

    fn identity() -> SemanticArtifactIdentity {
        SemanticArtifactIdentity::new(
            Url::parse("file:///workspace/main.rs").unwrap(),
            "rust".into(),
            SemanticSnapshotIdentity {
                parsed_version: 7,
                incarnation: 3,
                generation: 11,
            },
            true,
        )
    }

    fn identity_at_generation(generation: u64) -> SemanticArtifactIdentity {
        let mut identity = identity();
        identity.snapshot.generation = generation;
        identity
    }

    fn artifact(identity: SemanticArtifactIdentity, token_type: u32) -> SemanticArtifact {
        SemanticArtifact::from_full_result(
            identity,
            SemanticTokensResult::Tokens(SemanticTokens {
                result_id: None,
                data: vec![SemanticToken {
                    delta_line: 0,
                    delta_start: 0,
                    length: 1,
                    token_type,
                    token_modifiers_bitset: 0,
                }],
            }),
        )
        .expect("complete result")
    }

    #[test]
    fn artifact_identity_includes_every_output_input() {
        let identity = identity();

        assert_eq!(identity.uri.as_str(), "file:///workspace/main.rs");
        assert_eq!(identity.language, "rust");
        assert_eq!(identity.snapshot.parsed_version, 7);
        assert_eq!(identity.snapshot.incarnation, 3);
        assert_eq!(identity.snapshot.generation, 11);
        assert!(identity.supports_multiline);
    }

    #[test]
    fn artifact_discards_compute_local_result_id() {
        let token = SemanticToken {
            delta_line: 1,
            delta_start: 2,
            length: 3,
            token_type: 4,
            token_modifiers_bitset: 5,
        };
        let artifact = SemanticArtifact::from_full_result(
            identity(),
            SemanticTokensResult::Tokens(SemanticTokens {
                result_id: Some("compute-local".into()),
                data: vec![token],
            }),
        )
        .expect("complete result");

        let materialized = artifact
            .into_full(identity().as_ref(), None)
            .expect("matching identity");
        assert_eq!(materialized.result_id, None);
        assert_eq!(materialized.data, vec![token]);
    }

    #[test]
    fn materialization_assigns_request_result_id() {
        let artifact = SemanticArtifact::from_full_result(
            identity(),
            SemanticTokensResult::Tokens(SemanticTokens {
                result_id: None,
                data: vec![],
            }),
        )
        .expect("complete result");

        let materialized = artifact
            .into_full(identity().as_ref(), Some("request-42".into()))
            .expect("matching identity");
        assert_eq!(materialized.result_id.as_deref(), Some("request-42"));
    }

    #[test]
    fn every_identity_component_discriminates_materialization() {
        let base = identity();
        let other_uri = Url::parse("file:///workspace/other.rs").unwrap();
        let different_parsed_version = SemanticSnapshotIdentity {
            parsed_version: base.snapshot.parsed_version + 1,
            ..base.snapshot
        };
        let different_incarnation = SemanticSnapshotIdentity {
            incarnation: base.snapshot.incarnation + 1,
            ..base.snapshot
        };
        let different_generation = SemanticSnapshotIdentity {
            generation: base.snapshot.generation + 1,
            ..base.snapshot
        };
        let mismatches = [
            SemanticArtifactIdentity::expected(
                &other_uri,
                &base.language,
                base.snapshot,
                base.supports_multiline,
            ),
            SemanticArtifactIdentity::expected(
                &base.uri,
                "python",
                base.snapshot,
                base.supports_multiline,
            ),
            SemanticArtifactIdentity::expected(
                &base.uri,
                &base.language,
                different_parsed_version,
                base.supports_multiline,
            ),
            SemanticArtifactIdentity::expected(
                &base.uri,
                &base.language,
                different_incarnation,
                base.supports_multiline,
            ),
            SemanticArtifactIdentity::expected(
                &base.uri,
                &base.language,
                different_generation,
                base.supports_multiline,
            ),
            SemanticArtifactIdentity::expected(
                &base.uri,
                &base.language,
                base.snapshot,
                !base.supports_multiline,
            ),
        ];

        for mismatch in mismatches {
            let shared_artifact = SemanticArtifact::from_full_result(
                identity(),
                SemanticTokensResult::Tokens(SemanticTokens {
                    result_id: None,
                    data: vec![],
                }),
            )
            .expect("complete result");
            assert!(shared_artifact.materialize_full(mismatch, None).is_none());

            let artifact = SemanticArtifact::from_full_result(
                identity(),
                SemanticTokensResult::Tokens(SemanticTokens {
                    result_id: None,
                    data: vec![],
                }),
            )
            .expect("complete result");

            assert!(artifact.into_full(mismatch, None).is_none());
        }
    }

    #[test]
    fn shared_artifact_materializes_independent_responses() {
        let artifact = SemanticArtifact::from_full_result(
            identity(),
            SemanticTokensResult::Tokens(SemanticTokens {
                result_id: None,
                data: vec![],
            }),
        )
        .expect("complete result");

        let first = artifact
            .materialize_full(identity().as_ref(), Some("request-1".into()))
            .expect("matching identity");
        let second = artifact
            .materialize_full(identity().as_ref(), Some("request-2".into()))
            .expect("artifact remains available");

        assert_eq!(first.result_id.as_deref(), Some("request-1"));
        assert_eq!(second.result_id.as_deref(), Some("request-2"));
    }

    #[test]
    fn partial_result_cannot_become_visible_artifact() {
        let artifact = SemanticArtifact::from_full_result(
            identity(),
            SemanticTokensResult::Partial(SemanticTokensPartialResult { data: vec![] }),
        );

        assert!(artifact.is_none());
    }

    #[tokio::test]
    async fn same_identity_joins_one_producer() {
        let slot = std::sync::Arc::new(SemanticArtifactSlot::new());
        let producers = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let release = std::sync::Arc::new(tokio::sync::Notify::new());

        let first = slot.claim_or_join(identity(), {
            let producers = std::sync::Arc::clone(&producers);
            let release = std::sync::Arc::clone(&release);
            move |_cancel, identity| async move {
                producers.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                release.notified().await;
                Some(artifact(identity, 1))
            }
        });
        let second = slot.claim_or_join(identity(), {
            let producers = std::sync::Arc::clone(&producers);
            move |_cancel, identity| async move {
                producers.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Some(artifact(identity, 2))
            }
        });

        let joined = tokio::spawn(second);
        tokio::task::yield_now().await;
        assert_eq!(producers.load(std::sync::atomic::Ordering::SeqCst), 1);
        release.notify_waiters();

        let (first, second) = tokio::join!(first, joined);
        let first = first.expect("producer completes");
        let second = second.expect("join task").expect("join completes");
        assert!(std::sync::Arc::ptr_eq(&first, &second));
        assert_eq!(producers.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn failed_attempt_is_retryable() {
        let slot = std::sync::Arc::new(SemanticArtifactSlot::new());
        let failed = slot.claim_or_join(identity(), |_cancel, _identity| async { None });
        assert!(failed.await.is_none());

        let retried = slot.claim_or_join(identity(), |_cancel, identity| async move {
            Some(artifact(identity, 7))
        });
        assert!(retried.await.is_some());
    }

    #[tokio::test]
    async fn panicked_attempt_is_contained_and_retryable() {
        let slot = std::sync::Arc::new(SemanticArtifactSlot::new());
        let panicked = slot.claim_or_join(identity(), |_cancel, _identity| async {
            panic!("test producer panic");
        });
        assert!(panicked.await.is_none());

        let retried = slot.claim_or_join(identity(), |_cancel, identity| async move {
            Some(artifact(identity, 8))
        });
        assert!(retried.await.is_some());
    }

    #[tokio::test]
    async fn dropping_one_consumer_does_not_cancel_a_joiner() {
        let slot = std::sync::Arc::new(SemanticArtifactSlot::new());
        let started = std::sync::Arc::new(tokio::sync::Notify::new());
        let release = std::sync::Arc::new(tokio::sync::Notify::new());
        let (token_tx, token_rx) = tokio::sync::oneshot::channel();
        let first = slot.claim_or_join(identity(), {
            let started = std::sync::Arc::clone(&started);
            let release = std::sync::Arc::clone(&release);
            move |cancel, identity| async move {
                token_tx.send(cancel.clone()).ok();
                started.notify_one();
                release.notified().await;
                Some(artifact(identity, 4))
            }
        });
        let second = slot.claim_or_join(identity(), |_cancel, identity| async move {
            Some(artifact(identity, 5))
        });

        let cancelled_consumer = tokio::spawn(first);
        started.notified().await;
        let producer_token = token_rx.await.expect("producer exposes its token");
        cancelled_consumer.abort();
        let cancelled = cancelled_consumer.await;
        assert!(matches!(cancelled, Err(error) if error.is_cancelled()));
        assert!(
            !producer_token.is_cancelled(),
            "one detached consumer must not cancel a joined producer"
        );
        release.notify_one();

        let artifact = second.await.expect("joined consumer still completes");
        let tokens = artifact
            .materialize_full(identity().as_ref(), None)
            .expect("matching identity");
        assert_eq!(tokens.data[0].token_type, 4);
    }

    #[tokio::test]
    async fn dropping_last_consumer_cancels_and_vacates_attempt() {
        let slot = std::sync::Arc::new(SemanticArtifactSlot::new());
        let (token_tx, token_rx) = tokio::sync::oneshot::channel();
        let first = slot.claim_or_join(identity(), move |cancel, _identity| async move {
            token_tx.send(cancel.clone()).ok();
            cancel.cancelled().await;
            None
        });
        let first = tokio::spawn(first);
        let token = token_rx.await.expect("producer exposes its token");
        first.abort();
        token.cancelled().await;

        let retry = slot.claim_or_join(identity(), |_cancel, identity| async move {
            Some(artifact(identity, 6))
        });
        let retry = retry.await.expect("next consumer becomes producer");
        let tokens = retry
            .materialize_full(identity().as_ref(), None)
            .expect("matching identity");
        assert_eq!(tokens.data[0].token_type, 6);
    }

    #[tokio::test]
    async fn snapshot_cancellation_stops_current_attempt() {
        let slot = std::sync::Arc::new(SemanticArtifactSlot::new());
        let started = std::sync::Arc::new(tokio::sync::Notify::new());
        let future = slot.claim_or_join(identity(), {
            let started = std::sync::Arc::clone(&started);
            move |cancel, _identity| async move {
                started.notify_one();
                cancel.cancelled().await;
                None
            }
        });
        let task = tokio::spawn(future);
        started.notified().await;
        slot.cancel();
        assert!(task.await.expect("consumer task").is_none());

        let starts = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let late = slot.claim_or_join(identity(), {
            let starts = std::sync::Arc::clone(&starts);
            move |_cancel, identity| async move {
                starts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Some(artifact(identity, 9))
            }
        });
        assert!(
            late.await.is_none(),
            "retired snapshot must reject late work"
        );
        assert_eq!(starts.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn obsolete_generation_cannot_replace_newer_attempt() {
        let slot = std::sync::Arc::new(SemanticArtifactSlot::new());
        let release = std::sync::Arc::new(tokio::sync::Notify::new());
        let (token_tx, token_rx) = tokio::sync::oneshot::channel();
        let current_identity = identity_at_generation(12);
        let current = slot.claim_or_join(current_identity.clone(), {
            let release = std::sync::Arc::clone(&release);
            move |cancel, identity| async move {
                token_tx.send(cancel.clone()).ok();
                release.notified().await;
                Some(artifact(identity, 12))
            }
        });
        let current = tokio::spawn(current);
        let current_token = token_rx.await.expect("current producer token");

        let obsolete = slot
            .claim_or_join(identity_at_generation(11), |_cancel, identity| async move {
                Some(artifact(identity, 11))
            });
        assert!(obsolete.await.is_none());
        assert!(
            !current_token.is_cancelled(),
            "obsolete claim must not cancel the newer producer"
        );

        release.notify_one();
        let current = current
            .await
            .expect("current consumer task")
            .expect("current artifact");
        assert!(
            current
                .materialize_full(current_identity.as_ref(), None)
                .is_some()
        );
    }

    #[tokio::test]
    async fn generation_floor_cancels_old_attempt_and_allows_new_generation() {
        let slot = std::sync::Arc::new(SemanticArtifactSlot::new());
        let old = slot.claim_or_join(identity_at_generation(11), |cancel, _identity| async move {
            cancel.cancelled().await;
            None
        });
        let old = tokio::spawn(old);
        tokio::task::yield_now().await;

        slot.advance_minimum_generation(12);
        assert!(old.await.expect("old consumer task").is_none());
        assert!(
            slot.claim_or_join(identity_at_generation(11), |_cancel, identity| async move {
                Some(artifact(identity, 11))
            },)
                .await
                .is_none()
        );

        let current_identity = identity_at_generation(12);
        let current = slot
            .claim_or_join(current_identity.clone(), |_cancel, identity| async move {
                Some(artifact(identity, 12))
            })
            .await
            .expect("new generation remains usable");
        assert!(
            current
                .materialize_full(current_identity.as_ref(), None)
                .is_some()
        );
    }

    #[tokio::test]
    async fn replacement_cancels_old_attempt_without_removing_new_one() {
        let slot = std::sync::Arc::new(SemanticArtifactSlot::new());
        let old_started = std::sync::Arc::new(tokio::sync::Notify::new());
        let old = slot.claim_or_join(identity(), {
            let old_started = std::sync::Arc::clone(&old_started);
            move |cancel, _identity| async move {
                old_started.notify_one();
                cancel.cancelled().await;
                None
            }
        });
        let old_task = tokio::spawn(old);
        old_started.notified().await;

        let mut replacement_identity = identity();
        replacement_identity.snapshot.generation += 1;
        let replacement = slot.claim_or_join(
            replacement_identity.clone(),
            |_cancel, identity| async move { Some(artifact(identity, 9)) },
        );
        assert!(old_task.await.expect("old task").is_none());
        assert!(replacement.await.is_some());

        let joined = slot.claim_or_join(replacement_identity, |_cancel, identity| async move {
            Some(artifact(identity, 10))
        });
        let joined = joined.await.expect("replacement remains installed");
        let tokens = joined
            .materialize_full(
                SemanticArtifactIdentity::expected(
                    &Url::parse("file:///workspace/main.rs").unwrap(),
                    "rust",
                    SemanticSnapshotIdentity {
                        parsed_version: 7,
                        incarnation: 3,
                        generation: 12,
                    },
                    true,
                ),
                None,
            )
            .expect("matching replacement identity");
        assert_eq!(tokens.data[0].token_type, 9);
    }
}
