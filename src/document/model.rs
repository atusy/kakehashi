use tree_sitter::Tree;

/// Immutable snapshot of document state for lock-free processing
pub(crate) struct DocumentSnapshot {
    text: String,
    tree: Tree,
}

impl DocumentSnapshot {
    /// Get the text content
    pub(crate) fn text(&self) -> &str {
        &self.text
    }

    /// Get the parse tree
    pub(crate) fn tree(&self) -> &Tree {
        &self.tree
    }
}

/// Unified document structure combining text, parsing, and LSP state
pub struct Document {
    text: String,
    language_id: Option<String>,
    tree: Option<Tree>,
    /// Previous tree for changed_ranges comparison during incremental parsing
    previous_tree: Option<Tree>,
    /// Previous text for line delta calculation during incremental tokenization
    previous_text: Option<String>,
}

impl Document {
    /// Create a new document with just text
    pub(crate) fn new(text: String) -> Self {
        Self {
            text,
            language_id: None,
            tree: None,
            previous_tree: None,
            previous_text: None,
        }
    }

    /// Create with language but no tree yet (for early document registration)
    pub(crate) fn with_language(text: String, language_id: String) -> Self {
        Self {
            text,
            language_id: Some(language_id),
            tree: None,
            previous_tree: None,
            previous_text: None,
        }
    }

    /// Create with language and tree
    pub(crate) fn with_tree(text: String, language_id: String, tree: Tree) -> Self {
        Self {
            text,
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
        self.previous_text = Some(std::mem::replace(&mut self.text, new_text));
        self.tree = Some(new_tree);
    }

    /// Update tree and text with an explicit edited previous tree.
    ///
    /// This is the preferred method when the previous tree was edited via `tree.edit()`
    /// before parsing. The edited tree allows `changed_ranges()` to accurately compute
    /// the byte ranges that changed between the old and new parse trees.
    ///
    /// # Arguments
    /// * `new_tree` - The newly parsed tree
    /// * `new_text` - The new document text
    /// * `edited_previous_tree` - The previous tree after `tree.edit()` was applied
    pub(crate) fn update_with_edited_tree(
        &mut self,
        new_tree: Tree,
        new_text: String,
        edited_previous_tree: Tree,
    ) {
        self.previous_tree = Some(edited_previous_tree);
        self.previous_text = Some(std::mem::replace(&mut self.text, new_text));
        self.tree = Some(new_tree);
    }

    /// Update text and clear layers/state
    pub(crate) fn update_text(&mut self, text: String) {
        self.text = text;
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

        // Verify snapshot is independent (different addresses would be ideal, but we can verify content)
        assert_eq!(snapshot.text(), doc.text());
        assert_eq!(
            snapshot.tree().root_node().kind(),
            doc.tree().unwrap().root_node().kind()
        );
    }
}
