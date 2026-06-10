use crate::document::Document;
use dashmap::DashMap;
use dashmap::mapref::entry::Entry;
use dashmap::mapref::one::Ref;
use std::ops::Deref;
use std::sync::Arc;
use tokio::sync::{Mutex, watch};
use tree_sitter::{InputEdit, Tree};
use url::Url;

// The central store for all document-related information.
pub struct DocumentStore {
    documents: DashMap<Url, Document>,
    parse_states: DashMap<Url, watch::Sender<ParseState>>,
    /// Per-document serialization lock for `didChange` application.
    ///
    /// `didChange` handlers read the current text, apply incremental ranges, and
    /// only persist the result after an async reparse. Without serialization,
    /// concurrently-dispatched handlers for the same document all read the same
    /// stale base text, so a later edit's range (authored against a newer state)
    /// is applied to an older one — corrupting the text and, before clamping,
    /// panicking in `replace_range`. Holding this per-URI async lock across a
    /// document's edit critical section gives each edit mutual exclusion and lets
    /// it see the previous one's persisted result. Handlers take the lock as their
    /// first `.await`, so acquisition follows first-poll order — a practical
    /// mitigation, not a hard JSON-RPC wire-order guarantee (tracked in
    /// <https://github.com/atusy/kakehashi/issues/342>). Different documents keep
    /// their own locks and run concurrently.
    edit_locks: DashMap<Url, Arc<Mutex<()>>>,
    /// Monotonic "open generation" per URI, lazily assigned on first ask and
    /// cleared by [`remove`](Self::remove). A consumer that captures the
    /// generation alongside a snapshot can later detect that the document was
    /// closed and reopened in between — the reopened document draws a fresh
    /// generation even though the URI looks alive again. See
    /// `Kakehashi::store_lineage` (captures-protocol §"Delta semantics").
    open_generations: DashMap<Url, u64>,
    open_counter: std::sync::atomic::AtomicU64,
}

#[derive(Clone, Copy, Debug, Default)]
struct ParseState {
    generation: u64,
    in_progress: bool,
    has_tree: bool,
}

pub struct DocumentHandle<'a> {
    inner: Ref<'a, Url, Document>,
}

impl<'a> DocumentHandle<'a> {
    fn new(inner: Ref<'a, Url, Document>) -> Self {
        Self { inner }
    }
}

impl<'a> Deref for DocumentHandle<'a> {
    type Target = Document;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl Default for DocumentStore {
    fn default() -> Self {
        Self {
            documents: DashMap::new(),
            parse_states: DashMap::new(),
            edit_locks: DashMap::new(),
            open_generations: DashMap::new(),
            open_counter: std::sync::atomic::AtomicU64::new(0),
        }
    }
}

impl DocumentStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Update tree availability without affecting parse-in-progress tracking.
    /// The `in_progress` state is owned exclusively by mark_parse_started/mark_parse_finished.
    fn update_tree_availability(&self, uri: &Url, has_tree: bool) {
        let sender = self.parse_sender(uri);
        let mut state = *sender.borrow();
        state.has_tree = has_tree;
        sender.send_replace(state);
    }

    fn parse_sender(&self, uri: &Url) -> watch::Sender<ParseState> {
        match self.parse_states.entry(uri.clone()) {
            Entry::Occupied(entry) => entry.get().clone(),
            Entry::Vacant(entry) => {
                let (sender, _receiver) = watch::channel(ParseState::default());
                entry.insert(sender.clone());
                sender
            }
        }
    }

    pub fn mark_parse_started(&self, uri: &Url) -> u64 {
        let sender = self.parse_sender(uri);
        let mut state = *sender.borrow();
        state.generation = state.generation.saturating_add(1);
        state.in_progress = true;
        state.has_tree = false;
        sender.send_replace(state);
        state.generation
    }

    pub fn mark_parse_finished(&self, uri: &Url, generation: u64, has_tree: bool) {
        let sender = self.parse_sender(uri);
        let mut state = *sender.borrow();
        if state.generation != generation {
            return;
        }
        state.in_progress = false;
        state.has_tree = has_tree;
        sender.send_replace(state);
    }

    pub async fn wait_for_parse_completion(&self, uri: &Url, timeout: std::time::Duration) {
        let mut receiver = self.parse_sender(uri).subscribe();

        let wait_future = async {
            loop {
                let state = *receiver.borrow();

                // Already have a tree - done waiting
                if state.has_tree {
                    return;
                }

                // No tree yet - wait for state change
                // (either parse starts, or parse finishes with a tree)
                if receiver.changed().await.is_err() {
                    return; // Channel closed
                }
            }
        };

        let _ = tokio::time::timeout(timeout, wait_future).await;
    }

    // Lock safety: Single insert() call - no read lock held before or during write
    pub fn insert(&self, uri: Url, text: String, language_id: Option<String>, tree: Option<Tree>) {
        let has_tree = tree.is_some();
        let document = match (language_id, tree) {
            (Some(lang), Some(t)) => Document::with_tree(text, lang, t),
            (Some(lang), None) => Document::with_language(text, lang),
            _ => Document::new(text),
        };

        self.documents.insert(uri.clone(), document);
        self.update_tree_availability(&uri, has_tree);
    }

    // Lock safety: Returns DocumentHandle wrapping Ref - caller holds read lock until drop
    // Callers must not call write methods while holding the returned handle
    pub fn get(&self, uri: &Url) -> Option<DocumentHandle<'_>> {
        self.documents.get(uri).map(DocumentHandle::new)
    }

    // Lock safety: Uses entry() API for atomic check-and-update/insert operations,
    // eliminating race conditions between get_mut and insert.
    pub fn update_document(&self, uri: Url, text: String, new_tree: Option<Tree>) {
        // Use entry API for atomic operations to prevent race conditions
        // between checking if document exists and inserting/updating.
        let has_tree = match self.documents.entry(uri.clone()) {
            Entry::Occupied(mut entry) => {
                // Document exists - update in place to preserve previous_tree and previous_text
                let doc = entry.get_mut();
                if let Some(tree) = new_tree {
                    doc.update_tree_and_text(tree, text);
                    true
                } else {
                    // No new tree provided - clear existing tree and update text only.
                    // This path is used when text changes without re-parsing (rare edge case).
                    doc.update_text(text);
                    false
                }
            }
            Entry::Vacant(entry) => {
                // Document doesn't exist - create new one
                if let Some(tree) = new_tree {
                    entry.insert(Document::with_tree(text, "unknown".to_string(), tree));
                    true
                } else {
                    entry.insert(Document::new(text));
                    false
                }
            }
        };
        self.update_tree_availability(&uri, has_tree);
    }

    /// Update document with an edited previous tree for proper changed_ranges() support.
    ///
    /// Call when parsing used an edited old tree (via `tree.edit()`): the
    /// `edited_previous_tree` lets tree-sitter's `changed_ranges()` accurately
    /// compute the byte ranges that changed between versions.
    pub fn update_document_with_edited_tree(
        &self,
        uri: Url,
        text: String,
        new_tree: Tree,
        edited_previous_tree: Tree,
    ) {
        match self.documents.entry(uri.clone()) {
            Entry::Occupied(mut entry) => {
                // Document exists - update with edited tree to preserve change history
                entry
                    .get_mut()
                    .update_with_edited_tree(new_tree, text, edited_previous_tree);
            }
            Entry::Vacant(entry) => {
                // Document doesn't exist - create new one (edited_previous_tree is lost,
                // but this is expected for newly created documents)
                entry.insert(Document::with_tree(text, "unknown".to_string(), new_tree));
            }
        }
        self.update_tree_availability(&uri, true);
    }

    /// Get the existing tree and apply edits for incremental parsing
    /// Returns the edited tree without updating the document store
    // Lock safety: and_then() consumes Ref, returning owned Tree clone - no read lock held after return
    pub fn get_edited_tree(&self, uri: &Url, edits: &[InputEdit]) -> Option<Tree> {
        self.documents.get(uri).and_then(|doc| {
            doc.tree().map(|tree| {
                let mut tree = tree.clone();
                for edit in edits {
                    tree.edit(edit);
                }
                tree
            })
        })
    }

    /// Return the per-document `didChange` serialization lock, creating it on
    /// first use. Callers hold the guard across the document's edit critical
    /// section so concurrent edits to the same document apply in order. See the
    /// `edit_locks` field docs for why this is required.
    pub(crate) fn edit_lock(&self, uri: &Url) -> Arc<Mutex<()>> {
        self.edit_locks
            .entry(uri.clone())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    /// Drop the per-URI edit lock entry without touching the document itself.
    ///
    /// `edit_lock` get-or-inserts, so a handler that takes the lock and then
    /// finds the document missing (a `didChange`/semantic request for a never-
    /// opened or already-closed URI, e.g. a reordered notification) would leave a
    /// lock entry behind forever. Such handlers call this on their miss path to
    /// keep the map bounded. Safe to call while holding the lock's `Arc` guard —
    /// the guard keeps the mutex alive; only the map entry is removed.
    pub(crate) fn remove_edit_lock(&self, uri: &Url) {
        self.edit_locks.remove(uri);
    }

    // Lock safety: Single remove() call - no read lock held before or during write
    pub(crate) fn remove(&self, uri: &Url) -> Option<Document> {
        self.parse_states.remove(uri);
        self.edit_locks.remove(uri);
        self.open_generations.remove(uri);
        self.documents.remove(uri).map(|(_, doc)| doc)
    }

    /// The current open generation for `uri`, lazily assigned. Two reads
    /// straddling a [`remove`](Self::remove) (didClose) return different
    /// values, because the removal clears the entry and the next ask draws a
    /// fresh number — letting callers detect a close-then-reopen even when the
    /// URI looks alive on both sides. Fetch it **before** snapshotting: if a
    /// reopen races in between, the stale generation makes the later
    /// comparison fail (conservative), whereas the reverse order could pair an
    /// old snapshot with the new document's generation.
    pub(crate) fn open_generation(&self, uri: &Url) -> u64 {
        *self.open_generations.entry(uri.clone()).or_insert_with(|| {
            self.open_counter
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        })
    }

    /// Drop the lazily-created generation entry for a URI that turned out to
    /// have no document — the cleanup for an [`open_generation`](Self::open_generation)
    /// ask that raced a `didClose`. Safety does not depend on this (the
    /// counter is monotonic, so a pre-close generation can never equal any
    /// later one); it only prevents entries accumulating for closed URIs. The
    /// removal re-checks absence so a concurrent reopen's live entry is left
    /// alone; losing that race merely hands the reopened document a fresh
    /// number on its next ask, which is the conservative direction.
    pub(crate) fn forget_open_generation_if_closed(&self, uri: &Url) {
        self.open_generations
            .remove_if(uri, |_, _| self.documents.get(uri).is_none());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;
    use tokio::time::timeout;

    #[test]
    fn test_concurrent_update_and_get_no_deadlock() {
        // This test verifies that concurrent update_document and get operations
        // do not cause deadlock. The test uses a timeout to detect deadlock.
        let store = Arc::new(DocumentStore::new());
        let uri = Url::parse("file:///test.rs").unwrap();

        // Insert initial document
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        let initial_text = "fn main() {}".to_string();
        let tree = parser.parse(&initial_text, None).unwrap();
        store.insert(
            uri.clone(),
            initial_text,
            Some("rust".to_string()),
            Some(tree),
        );

        let num_threads = 10;
        let iterations_per_thread = 100;
        let mut handles = vec![];

        // Spawn writer threads
        for i in 0..num_threads {
            let store_clone = store.clone();
            let uri_clone = uri.clone();
            let mut parser = tree_sitter::Parser::new();
            parser
                .set_language(&tree_sitter_rust::LANGUAGE.into())
                .unwrap();

            let handle = thread::spawn(move || {
                for j in 0..iterations_per_thread {
                    let text =
                        format!("fn main() {{ let x = {}; }}", i * iterations_per_thread + j);
                    let tree = parser.parse(&text, None).unwrap();
                    store_clone.update_document(uri_clone.clone(), text, Some(tree));
                }
            });
            handles.push(handle);
        }

        // Spawn reader threads
        for _ in 0..num_threads {
            let store_clone = store.clone();
            let uri_clone = uri.clone();

            let handle = thread::spawn(move || {
                for _ in 0..iterations_per_thread {
                    // get() returns a Ref which holds a read lock
                    if let Some(doc) = store_clone.get(&uri_clone) {
                        // Access the document while holding the lock
                        let _ = doc.text();
                        let _ = doc.tree();
                    }
                }
            });
            handles.push(handle);
        }

        // Wait for all threads with timeout (5 seconds)
        // If deadlock occurs, this will hang and the test will fail
        let timeout = Duration::from_secs(5);
        let start = std::time::Instant::now();

        for handle in handles {
            let remaining = timeout.saturating_sub(start.elapsed());
            if remaining.is_zero() {
                panic!("Test timed out - possible deadlock detected");
            }

            // Use a channel to implement join with timeout
            let (tx, rx) = std::sync::mpsc::channel();
            let join_handle = thread::spawn(move || {
                let result = handle.join();
                let _ = tx.send(result);
            });

            match rx.recv_timeout(remaining) {
                Ok(Ok(())) => {}
                Ok(Err(_)) => panic!("Thread panicked"),
                Err(_) => panic!("Test timed out - possible deadlock detected"),
            }

            // Clean up the join wrapper thread
            let _ = join_handle.join();
        }

        // If we get here, no deadlock occurred
        // Verify final state is consistent
        let doc = store.get(&uri).expect("Document should exist");
        assert!(!doc.text().is_empty());
    }

    #[test]
    fn test_add_and_get_document() {
        let store = DocumentStore::new();
        let uri = Url::parse("file:///test.txt").unwrap();
        let text = "hello world".to_string();

        store.insert(uri.clone(), text.clone(), None, None);
        let doc = store.get(&uri).unwrap();
        assert_eq!(doc.text(), &text);
    }

    #[test]
    fn test_update_document_preserves_language() {
        let store = DocumentStore::new();
        let uri = Url::parse("file:///test.rs").unwrap();
        let text1 = "fn main() {}".to_string();
        let text2 = "fn main() { println!(\"hello\"); }".to_string();

        // Create a fake tree for testing
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        let tree = parser.parse(&text1, None).unwrap();

        store.insert(uri.clone(), text1, Some("rust".to_string()), Some(tree));

        // Update text
        store.update_document(uri.clone(), text2.clone(), None);

        // Language info should be preserved
        let doc = store.get(&uri).unwrap();
        assert_eq!(doc.text(), text2);
        assert_eq!(doc.language_id(), Some("rust"));
    }

    #[test]
    fn test_edit_lock_is_stable_per_uri_and_cleared_on_remove() {
        let store = DocumentStore::new();
        let uri_a = Url::parse("file:///a.rs").unwrap();
        let uri_b = Url::parse("file:///b.rs").unwrap();

        // Same URI yields the same lock instance, so concurrent didChange
        // handlers for one document serialize on a shared mutex.
        let a1 = store.edit_lock(&uri_a);
        let a2 = store.edit_lock(&uri_a);
        assert!(Arc::ptr_eq(&a1, &a2), "same URI must share one edit lock");

        // Different URIs get distinct locks so unrelated documents stay parallel.
        let b1 = store.edit_lock(&uri_b);
        assert!(
            !Arc::ptr_eq(&a1, &b1),
            "different URIs must not share an edit lock"
        );

        // Removing the document drops its lock entry; a later edit_lock call
        // mints a fresh, distinct lock (open/close cycles don't reuse a stale
        // mutex). Compare pointer identity directly — `a1` is kept alive so the
        // check doesn't depend on incidental Arc clone counts.
        store.remove(&uri_a);
        let a3 = store.edit_lock(&uri_a);
        assert!(
            !Arc::ptr_eq(&a1, &a3),
            "lock after remove must be a fresh instance, not the cleared one"
        );

        // remove_edit_lock drops the entry on the document-missing path, so the
        // next lock for the same URI is again a fresh instance.
        store.remove_edit_lock(&uri_a);
        let a4 = store.edit_lock(&uri_a);
        assert!(
            !Arc::ptr_eq(&a3, &a4),
            "lock after remove_edit_lock must be a fresh instance"
        );
    }

    #[test]
    fn test_document_layer_preservation() {
        let store = DocumentStore::new();
        let uri = Url::parse("file:///test.rs").unwrap();
        let text = "let x = 1;".to_string();

        // Create document with tree
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        let tree = parser.parse(&text, None).unwrap();

        store.insert(
            uri.clone(),
            text.clone(),
            Some("rust".to_string()),
            Some(tree.clone()),
        );

        // Update with new tree
        let new_text = "let x = 2;".to_string();
        let new_tree = parser.parse(&new_text, Some(&tree)).unwrap();
        store.update_document(uri.clone(), new_text.clone(), Some(new_tree));

        let doc = store.get(&uri).unwrap();
        assert_eq!(doc.text(), new_text);
        assert!(doc.tree().is_some());
    }

    #[tokio::test]
    async fn wait_for_parse_completion_blocks_until_finished() {
        let store = DocumentStore::new();
        let uri = Url::parse("file:///parse-wait.lua").unwrap();

        let generation = store.mark_parse_started(&uri);
        let wait_future = store.wait_for_parse_completion(&uri, Duration::from_secs(1));
        let mut wait_future = Box::pin(wait_future);

        assert!(
            timeout(Duration::from_millis(10), &mut wait_future)
                .await
                .is_err(),
            "wait should block while parse is in progress"
        );

        store.mark_parse_finished(&uri, generation, true);

        assert!(
            timeout(Duration::from_millis(200), wait_future)
                .await
                .is_ok(),
            "wait should complete after parse finishes"
        );
    }

    #[tokio::test]
    async fn wait_for_parse_completion_returns_when_tree_available() {
        let store = DocumentStore::new();
        let uri = Url::parse("file:///parse-ready.rs").unwrap();

        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        let text = "fn main() {}".to_string();
        let tree = parser.parse(&text, None).unwrap();

        store.insert(uri.clone(), text, Some("rust".to_string()), Some(tree));

        let wait_future = store.wait_for_parse_completion(&uri, Duration::from_secs(1));
        assert!(
            timeout(Duration::from_millis(10), wait_future)
                .await
                .is_ok(),
            "wait should return immediately when a tree is already available"
        );
    }
}
