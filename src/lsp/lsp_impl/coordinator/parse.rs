use crate::document::DocumentStore;
use crate::language::{DocumentParserPool, LanguageCoordinator};
use crate::lsp::auto_install::AutoInstallManager;
use crate::lsp::bridge::BridgeCoordinator;
use crate::lsp::cache::CacheCoordinator;
use crate::lsp::client::ClientNotifier;
use tower_lsp_server::Client;
use tree_sitter::InputEdit;
use url::Url;

use crate::lsp::lsp_impl::{Kakehashi, build_notifier};
use crate::lsp::settings_manager::SettingsManager;

/// Timeout for spawn_blocking parse operations to prevent hangs on pathological inputs.
/// Shared across all parse-with-pool call sites (didChange, semantic tokens, selection range).
const PARSE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

pub(super) struct ParseCoordinatorDeps {
    pub(super) client: Client,
    pub(super) language: std::sync::Arc<LanguageCoordinator>,
    pub(super) parser_pool: std::sync::Arc<tokio::sync::Mutex<DocumentParserPool>>,
    pub(super) documents: std::sync::Arc<DocumentStore>,
    pub(super) cache: std::sync::Arc<CacheCoordinator>,
    pub(super) settings_manager: std::sync::Arc<SettingsManager>,
    pub(super) auto_install: AutoInstallManager,
    pub(super) bridge: std::sync::Arc<BridgeCoordinator>,
}

pub(crate) struct ParseCoordinator {
    client: Client,
    language: std::sync::Arc<LanguageCoordinator>,
    parser_pool: std::sync::Arc<tokio::sync::Mutex<DocumentParserPool>>,
    documents: std::sync::Arc<DocumentStore>,
    cache: std::sync::Arc<CacheCoordinator>,
    settings_manager: std::sync::Arc<SettingsManager>,
    auto_install: AutoInstallManager,
    bridge: std::sync::Arc<BridgeCoordinator>,
}

impl ParseCoordinator {
    pub(crate) fn new(server: &Kakehashi) -> Self {
        Self::from_parts(ParseCoordinatorDeps {
            client: server.client.clone(),
            language: std::sync::Arc::clone(&server.language),
            parser_pool: std::sync::Arc::clone(&server.parser_pool),
            documents: std::sync::Arc::clone(&server.documents),
            cache: std::sync::Arc::clone(&server.cache),
            settings_manager: std::sync::Arc::clone(&server.settings_manager),
            auto_install: server.auto_install.clone(),
            bridge: std::sync::Arc::clone(&server.bridge),
        })
    }

    pub(super) fn from_parts(deps: ParseCoordinatorDeps) -> Self {
        Self {
            client: deps.client,
            language: deps.language,
            parser_pool: deps.parser_pool,
            documents: deps.documents,
            cache: deps.cache,
            settings_manager: deps.settings_manager,
            auto_install: deps.auto_install,
            bridge: deps.bridge,
        }
    }

    /// Shared parsing orchestration: acquire parser from pool, run parse logic in
    /// `spawn_blocking` with timeout, release parser back to pool.
    ///
    /// The caller provides the actual parse logic via `parse_fn`, which receives a
    /// `tree_sitter::Parser` and must return it along with an optional result.
    /// On normal completion, this ensures the parser is returned to the pool.
    /// The parser is not returned if the blocking task times out (it keeps
    /// running) or if the task fails or is cancelled and yields a `JoinError`.
    ///
    /// Returns `None` if:
    /// - No parser is available for the language
    /// - The parse task panicked or was cancelled (JoinError; parser not returned)
    /// - The parse timed out after `PARSE_TIMEOUT` (parser not returned)
    /// - The closure returned `None`
    pub(crate) async fn parse_with_pool<T, F>(
        &self,
        language_name: &str,
        uri: &Url,
        text_len: usize,
        parse_fn: F,
    ) -> Option<T>
    where
        F: FnOnce(tree_sitter::Parser) -> (tree_sitter::Parser, Option<T>) + Send + 'static,
        T: Send + 'static,
    {
        let parser = {
            let mut pool = self.parser_pool.lock().await;
            pool.acquire(language_name)
        };

        let parser = parser?;

        let result = tokio::time::timeout(
            PARSE_TIMEOUT,
            tokio::task::spawn_blocking(move || parse_fn(parser)),
        )
        .await;

        match result {
            Ok(Ok((parser, value))) => {
                let mut pool = self.parser_pool.lock().await;
                pool.release(language_name.to_string(), parser);
                value
            }
            Ok(Err(join_error)) => {
                if join_error.is_panic() {
                    log::error!(
                        "Parse task panicked for language '{}' on document {}: {}",
                        language_name,
                        uri,
                        join_error
                    );
                } else {
                    log::warn!(
                        "Parse task was cancelled for language '{}' on document {}: {}",
                        language_name,
                        uri,
                        join_error
                    );
                }
                None
            }
            Err(_timeout) => {
                log::warn!(
                    "Parse timeout after {:?} for language '{}' on document {} ({} bytes)",
                    PARSE_TIMEOUT,
                    language_name,
                    uri,
                    text_len
                );
                None
            }
        }
    }

    /// Parse `uri`'s text and publish the result into the store.
    ///
    /// `ticket` is the ingress writer ticket of the mutation that scheduled this
    /// parse (plumbed from `IngressOrderGate` via the handler), or `None` for a
    /// caller outside the ingress sequence (a post-install reparse, or a test
    /// driving the coordinator directly). On every resolution path — a tree, a
    /// parsed-to-nothing, a previously-crashed parser, or no detectable language —
    /// the parse advances the store's per-document **watermark** to `ticket`, so a
    /// virt/native reader waiting on the watermark is released once the parse
    /// covering its tail edit has resolved. Threading the ticket as an explicit
    /// value (rather than reading a task-local here) keeps this signature stable
    /// when the per-document parse actor later runs this off the ingress task.
    pub(crate) async fn parse_document(
        &self,
        uri: Url,
        text: String,
        language_id: Option<&str>,
        edits: Vec<InputEdit>,
        ticket: Option<u64>,
    ) {
        let parse_generation = self.documents.mark_parse_started(&uri);
        let mut events = Vec::new();

        // Publish the watermark on whichever path resolves the parse below.
        let advance_watermark = || {
            if let Some(ticket) = ticket {
                self.documents.advance_watermark(&uri, ticket);
            }
        };

        let language_name = self
            .language
            .detect_language(uri.path(), &text, None, language_id);

        if let Some(language_name) = language_name {
            if self.auto_install.is_parser_failed(&language_name) {
                log::warn!(
                    target: "kakehashi::crash_recovery",
                    "Skipping parsing for '{}' - parser previously crashed",
                    language_name
                );
                self.documents
                    .insert(uri.clone(), text, Some(language_name), None);
                self.documents
                    .mark_parse_finished(&uri, parse_generation, false);
                advance_watermark();
                self.notifier().log_language_events(&events).await;
                return;
            }

            let load_result = self.language.ensure_language_loaded(&language_name);
            events.extend(load_result.events);

            let (base_tree, pre_edit_tree) = if !edits.is_empty() {
                let edited = self.documents.get_edited_tree(&uri, &edits);
                let for_store = edited.clone();
                (edited, for_store)
            } else {
                // Full-text sync (and any other no-edits reparse): do NOT seed
                // the parse with the stored tree. tree-sitter's incremental
                // contract requires every text change to be applied to the old
                // tree via ts_tree_edit first; reusing an unedited tree against
                // different text makes external scanners deserialize state at
                // wrong positions and (for the markdown grammar) overflow in
                // scanner serialize — heap corruption that killed the whole
                // server (#348). A full parse is the correct cost of full-text
                // sync: without InputEdits there is nothing to be incremental
                // about.
                (None, None)
            };

            let text_clone = text.clone();
            let auto_install = self.auto_install.clone();
            let language_name_clone = language_name.clone();

            let parsed_tree = self
                .parse_with_pool(&language_name, &uri, text.len(), move |mut parser| {
                    let _ = auto_install.begin_parsing(&language_name_clone);
                    let parse_result = parser.parse(&text_clone, base_tree.as_ref());
                    let _ = auto_install.end_parsing(&language_name_clone);
                    (parser, parse_result.map(|tree| (tree, pre_edit_tree)))
                })
                .await;

            if let Some((tree, pre_edit_tree)) = parsed_tree {
                self.cache.populate_injections(
                    &uri,
                    &text,
                    &tree,
                    &language_name,
                    &self.language,
                    self.bridge.node_tracker(),
                );

                if let Some(edited_tree) = pre_edit_tree {
                    self.documents.update_document_with_edited_tree(
                        uri.clone(),
                        text,
                        tree,
                        edited_tree,
                    );
                } else {
                    self.documents.insert(
                        uri.clone(),
                        text,
                        Some(language_name.clone()),
                        Some(tree),
                    );
                }

                self.documents
                    .mark_parse_finished(&uri, parse_generation, true);
                advance_watermark();
                self.notifier().log_language_events(&events).await;
                return;
            }
        }

        self.documents.insert(uri.clone(), text, None, None);
        self.documents
            .mark_parse_finished(&uri, parse_generation, false);
        advance_watermark();
        self.notifier().log_language_events(&events).await;
    }

    /// Re-parse a document after its parser finished installing, **off the
    /// ingress path** and **resurrection-safely**.
    ///
    /// Called from the spawned auto-install task (see `did_open`), so by the time
    /// it runs the originating `didOpen` writer ticket has already completed.
    /// Unlike [`parse_document`](Self::parse_document) it:
    ///
    /// - re-reads the **latest** store text rather than the open-time text (a
    ///   `didChange` may have landed while the install ran), and
    /// - persists through the **non-inserting** `update_tree_if_text_unchanged`
    ///   CAS, so a `didClose` during the install leaves the document gone instead
    ///   of resurrecting it (the install/parse resurrection vector the actor ADR
    ///   calls out), and a `didChange` between the read and the write drops the
    ///   now-stale tree.
    ///
    /// No watermark advance: the originating `didOpen`'s skip-parse branch already
    /// resolved that ticket's watermark, and this reparse carries no ticket.
    ///
    /// Because the install is now off-ingress, a `didChange` can run *concurrently*
    /// with this reparse (it is no longer gated behind the install). A `didChange`
    /// that lands while the parser is still loading stores its new text with **no
    /// tree** (the parser wasn't available), and would then CAS-reject this
    /// reparse's now-stale tree — leaving the document tree-less. To converge, this
    /// re-reads the latest text and retries a bounded number of times until the
    /// tree lands (or another parse wins). Sustained editing falls back to the
    /// reader's on-demand parse; the parse actor replaces this with a proper
    /// coalescing loop.
    pub(crate) async fn reparse_installed_document(&self, uri: Url, language_id: Option<String>) {
        /// Bound on the convergence retries (a burst of edits landing exactly as
        /// the install completes); past this the reader on-demand parse covers it.
        const MAX_REPARSE_ATTEMPTS: usize = 8;

        // Resolve the language once from the current text; a missing document means
        // a `didClose` ran during the install — do not resurrect it. Detect while
        // borrowing the stored text (a synchronous call, no `.await` and no
        // document write under the `Ref`) rather than cloning it.
        let Some(language_name) = ({
            let Some(doc) = self.documents.get(&uri) else {
                return;
            };
            self.language
                .detect_language(uri.path(), doc.text(), None, language_id.as_deref())
        }) else {
            return;
        };
        if self.auto_install.is_parser_failed(&language_name) {
            return;
        }
        let load_result = self.language.ensure_language_loaded(&language_name);
        let events = load_result.events;

        for _ in 0..MAX_REPARSE_ATTEMPTS {
            // Re-read the latest text each attempt. Gone => closed (no resurrect);
            // already has a tree => a concurrent parse won, nothing to do.
            let text = {
                let Some(doc) = self.documents.get(&uri) else {
                    break;
                };
                if doc.tree().is_some() {
                    break;
                }
                doc.text().to_string()
            };

            let text_clone = text.clone();
            let auto_install = self.auto_install.clone();
            let language_name_clone = language_name.clone();
            let parsed_tree = self
                .parse_with_pool(&language_name, &uri, text.len(), move |mut parser| {
                    let _ = auto_install.begin_parsing(&language_name_clone);
                    let result = parser.parse(&text_clone, None);
                    let _ = auto_install.end_parsing(&language_name_clone);
                    (parser, result)
                })
                .await;

            let Some(tree) = parsed_tree else { break };

            // Persist FIRST through the non-inserting CAS: a closed (Vacant)
            // document or one whose text moved on (a concurrent `didChange`) drops
            // the tree. Only populate the injection caches when the tree actually
            // landed, so a `didClose` racing this reparse can't leave stale
            // injection entries for a gone document. (`Tree` clone is a cheap
            // refcount bump.)
            let stored = self
                .documents
                .update_tree_if_text_unchanged(&uri, &text, tree.clone());
            if stored {
                self.cache.populate_injections(
                    &uri,
                    &text,
                    &tree,
                    &language_name,
                    &self.language,
                    self.bridge.node_tracker(),
                );
                break;
            }
            // CAS rejected: the text moved under us (a concurrent `didChange`).
            // Loop to re-read the latest text and try again.
        }

        self.notifier().log_language_events(&events).await;
    }

    fn notifier(&self) -> ClientNotifier<'_> {
        build_notifier(&self.client, &self.settings_manager)
    }
}
