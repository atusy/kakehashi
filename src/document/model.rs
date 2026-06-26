use std::sync::Arc;

use tree_sitter::Tree;

/// Immutable snapshot of document state for lock-free processing
pub(crate) struct DocumentSnapshot {
    text: Arc<str>,
    tree: Tree,
}

impl DocumentSnapshot {
    /// Get the text content
    pub(crate) fn text(&self) -> &str {
        &self.text
    }

    /// Cheaply clone the text as a shared `Arc<str>` (a refcount bump, no copy)
    /// — for callers that need an owned handle to the snapshot's text, e.g. the
    /// host bridge's `HostRequestContext` (#498).
    pub(crate) fn text_arc(&self) -> Arc<str> {
        Arc::clone(&self.text)
    }

    /// Get the parse tree
    pub(crate) fn tree(&self) -> &Tree {
        &self.tree
    }
}

/// Unified document structure combining text, parsing, and LSP state
pub struct Document {
    /// Stored as `Arc<str>` so cloning the text — on every `snapshot()` and on
    /// each host-bridge live read — is a refcount bump rather than a full copy
    /// (#498). The cost is one `String → Arc<str>` reallocation per construct /
    /// edit, paid back by the many cheap clones per edit.
    text: Arc<str>,
    language_id: Option<String>,
    tree: Option<Tree>,
    /// Previous tree for changed_ranges comparison during incremental parsing
    previous_tree: Option<Tree>,
    /// Previous text for line delta calculation during incremental tokenization
    previous_text: Option<Arc<str>>,
}

impl Document {
    /// Create a new document with just text
    pub(crate) fn new(text: String) -> Self {
        Self {
            text: Arc::from(text),
            language_id: None,
            tree: None,
            previous_tree: None,
            previous_text: None,
        }
    }

    /// Create with language but no tree yet (for early document registration)
    pub(crate) fn with_language(text: String, language_id: String) -> Self {
        Self {
            text: Arc::from(text),
            language_id: Some(language_id),
            tree: None,
            previous_tree: None,
            previous_text: None,
        }
    }

    /// Create with language and tree
    pub(crate) fn with_tree(text: String, language_id: String, tree: Tree) -> Self {
        Self {
            text: Arc::from(text),
            language_id: Some(language_id),
            tree: Some(tree),
            previous_tree: None,
            previous_text: None,
        }
    }

    /// Get the text content
    pub(crate) fn text(&self) -> &str {
        &self.text
    }

    /// Cheaply clone the text as a shared `Arc<str>` (a refcount bump, no copy).
    /// Used by the host-bridge live read so reading the document's current text
    /// under the lock no longer full-copies it (#498).
    pub(crate) fn text_arc(&self) -> Arc<str> {
        Arc::clone(&self.text)
    }

    /// Get the language ID
    pub(crate) fn language_id(&self) -> Option<&str> {
        self.language_id.as_deref()
    }

    /// Get the tree
    pub fn tree(&self) -> Option<&Tree> {
        self.tree.as_ref()
    }

    /// Get a position mapper for this document
    pub(crate) fn position_mapper(&self) -> crate::text::PositionMapper {
        crate::text::PositionMapper::new(self.text())
    }

    /// Create an immutable snapshot of current document state
    ///
    /// Returns None if document is not fully initialized (missing tree).
    /// The snapshot clones text and tree to enable lock-free processing.
    pub(crate) fn snapshot(&self) -> Option<DocumentSnapshot> {
        Some(DocumentSnapshot {
            text: self.text.clone(),
            tree: self.tree.as_ref()?.clone(),
        })
    }

    /// Update tree and text together for incremental tokenization support
    ///
    /// This preserves both previous tree and previous text for:
    /// - changed_ranges comparison (tree)
    /// - line delta calculation (text)
    ///
    /// Note: For proper `changed_ranges()` support, prefer `update_with_edited_tree`
    /// which accepts the edited previous tree (after `tree.edit()` was called).
    pub(crate) fn update_tree_and_text(&mut self, new_tree: Tree, new_text: String) {
        self.previous_tree = self.tree.take();
        self.previous_text = Some(std::mem::replace(&mut self.text, Arc::from(new_text)));
        self.tree = Some(new_tree);
    }

    /// Update tree and text with an explicit edited previous tree.
    ///
    /// Preferred when the previous tree was edited via `tree.edit()` before
    /// parsing: the edited tree lets `changed_ranges()` accurately compute the
    /// byte ranges changed between the old and new parse trees.
    pub(crate) fn update_with_edited_tree(
        &mut self,
        new_tree: Tree,
        new_text: String,
        edited_previous_tree: Tree,
    ) {
        self.previous_tree = Some(edited_previous_tree);
        self.previous_text = Some(std::mem::replace(&mut self.text, Arc::from(new_text)));
        self.tree = Some(new_tree);
    }

    /// Attach a parsed tree **without** touching the text.
    ///
    /// For an on-demand reader parse whose parsed text already equals the stored
    /// text: there is no content-version transition to record, so `previous_tree`
    /// / `previous_text` are left as-is and the text is neither re-cloned nor
    /// replaced.
    pub(crate) fn set_tree(&mut self, tree: Tree) {
        self.tree = Some(tree);
    }

    /// Update text and clear layers/state
    pub(crate) fn update_text(&mut self, text: String) {
        self.text = Arc::from(text);
        // Note: Tree needs to be rebuilt after text change
        self.tree = None;
        self.previous_tree = None;
        self.previous_text = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_document_creation() {
        let doc = Document::new("hello world".to_string());
        assert_eq!(doc.text(), "hello world");
        assert_eq!(doc.text().len(), 11);
        assert!(!doc.text().is_empty());
    }

    #[test]
    fn test_document_with_layer() {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        let tree = parser.parse("fn main() {}", None).unwrap();

        let doc = Document::with_tree("fn main() {}".to_string(), "rust".to_string(), tree);

        assert_eq!(doc.text(), "fn main() {}");
        assert!(doc.tree().is_some());
        assert_eq!(doc.language_id(), Some("rust"));
    }

    #[test]
    fn test_update_text() {
        let mut doc = Document::new("initial".to_string());
        doc.update_text("updated".to_string());
        assert_eq!(doc.text(), "updated");
        assert!(doc.tree().is_none());
    }

    #[test]
    fn test_document_preserves_previous_tree() {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        let tree1 = parser.parse("fn main() {}", None).unwrap();

        let mut doc = Document::with_tree("fn main() {}".to_string(), "rust".to_string(), tree1);

        // Initially no previous tree
        assert!(doc.previous_tree.is_none());

        // Update tree and text - old should become previous
        let tree2 = parser.parse("fn main() { let x = 1; }", None).unwrap();
        doc.update_tree_and_text(tree2, "fn main() { let x = 1; }".to_string());

        // Now previous tree should exist
        assert!(doc.previous_tree.is_some());
        // Current tree should be the new one
        assert!(doc.tree().is_some());
    }

    #[test]
    fn test_document_preserves_previous_text() {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();

        let old_text = "fn main() {}".to_string();
        let new_text = "fn main() { let x = 1; }".to_string();

        let tree1 = parser.parse(&old_text, None).unwrap();
        let mut doc = Document::with_tree(old_text.clone(), "rust".to_string(), tree1);

        // Initially no previous text
        assert!(doc.previous_text.is_none());

        // Update tree and text together
        let tree2 = parser.parse(&new_text, None).unwrap();
        doc.update_tree_and_text(tree2, new_text.clone());

        // Now previous text should exist and match old text
        assert_eq!(doc.previous_text.as_deref(), Some("fn main() {}"));
        // Current text should be new text
        assert_eq!(doc.text(), "fn main() { let x = 1; }");
        // Previous tree should also exist
        assert!(doc.previous_tree.is_some());
    }

    #[test]
    fn test_document_snapshot() {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        let tree = parser.parse("fn main() {}", None).unwrap();

        let doc = Document::with_tree("fn main() {}".to_string(), "rust".to_string(), tree);

        // Snapshot should succeed for fully initialized document
        let snapshot = doc.snapshot();
        assert!(snapshot.is_some());

        let snapshot = snapshot.unwrap();
        assert_eq!(snapshot.text(), "fn main() {}");
        assert_eq!(snapshot.tree().root_node().kind(), "source_file");
    }

    #[test]
    fn test_document_snapshot_none_when_no_tree() {
        let doc = Document::new("test".to_string());
        // No tree, so snapshot should be None
        assert!(doc.snapshot().is_none());
    }

    #[test]
    fn test_document_snapshot_clones_independently() {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        let tree = parser.parse("fn main() {}", None).unwrap();

        let doc = Document::with_tree("fn main() {}".to_string(), "rust".to_string(), tree);

        // Create snapshot
        let snapshot = doc.snapshot().unwrap();

        // Snapshot content matches the document. The text now shares the
        // document's `Arc<str>` allocation (a cheap clone — see
        // `snapshot_text_shares_the_document_allocation`); it stays a valid
        // immutable snapshot because any edit installs a *new* `Arc` on the
        // document rather than mutating this one.
        assert_eq!(snapshot.text(), doc.text());
        assert_eq!(
            snapshot.tree().root_node().kind(),
            doc.tree().unwrap().root_node().kind()
        );
    }

    #[test]
    fn text_arc_is_a_cheap_shared_clone() {
        let doc = Document::new("shared text".to_string());
        let a = doc.text_arc();
        let b = doc.text_arc();
        // Both handles point to the SAME allocation — a refcount bump, not a
        // copy (#498).
        assert!(Arc::ptr_eq(&a, &b));
        assert_eq!(&*a, "shared text");
    }

    #[test]
    fn snapshot_text_shares_the_document_allocation() {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        let tree = parser.parse("fn main() {}", None).unwrap();
        let doc = Document::with_tree("fn main() {}".to_string(), "rust".to_string(), tree);

        let snapshot = doc.snapshot().unwrap();

        // `snapshot()` clones the text `Arc` (refcount bump) rather than copying
        // the bytes — the whole point of #498.
        assert!(Arc::ptr_eq(&doc.text_arc(), &snapshot.text_arc()));
    }

    #[test]
    fn update_text_installs_a_fresh_allocation() {
        // An edit replaces the `Arc` (so prior snapshots keep their bytes); the
        // new text is correct.
        let mut doc = Document::new("v1".to_string());
        let before = doc.text_arc();
        doc.update_text("v2".to_string());
        let after = doc.text_arc();
        assert_eq!(&*after, "v2");
        assert!(!Arc::ptr_eq(&before, &after), "edit installs a new Arc");
        assert_eq!(&*before, "v1", "the prior Arc still holds the old text");
    }
}
