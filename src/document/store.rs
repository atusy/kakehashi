use crate::document::Document;
use dashmap::DashMap;
use dashmap::mapref::entry::Entry;
use dashmap::mapref::one::Ref;
use std::ops::Deref;
use tokio::sync::watch;
use tree_sitter::{InputEdit, Tree};
use url::Url;

// The central store for all document-related information.
pub struct DocumentStore {
    documents: DashMap<Url, Document>,
    parse_states: DashMap<Url, watch::Sender<ParseState>>,
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
    /// This method should be called when parsing was done with an edited old tree (via tree.edit()).
    /// The edited_previous_tree allows tree-sitter's changed_ranges() to accurately compute
    /// the byte ranges that changed between versions.
    ///
    /// Uses entry() API for atomic check-and-update/insert operations.
    ///
    /// # Arguments
    /// * `uri` - Document URI
    /// * `text` - New document text
    /// * `new_tree` - Newly parsed tree
    /// * `edited_previous_tree` - The previous tree after tree.edit() was applied
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

    // Lock safety: map() consumes Ref, returning owned String clone - no read lock held after return
    pub fn get_document_text(&self, uri: &Url) -> Option<String> {
        self.documents.get(uri).map(|doc| doc.text().to_string())
    }

    // Lock safety: Single remove() call - no read lock held before or during write
    pub(crate) fn remove(&self, uri: &Url) -> Option<Document> {
        self.parse_states.remove(uri);
        self.documents.remove(uri).map(|(_, doc)| doc)
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
