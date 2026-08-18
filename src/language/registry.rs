use dashmap::DashMap;
use std::sync::Arc;
use tree_sitter::Language;

/// Registry for managing loaded Tree-sitter languages
#[derive(Clone)]
pub(crate) struct LanguageRegistry {
    languages: Arc<DashMap<String, Language>>,
}

impl Default for LanguageRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl LanguageRegistry {
    pub(crate) fn new() -> Self {
        Self {
            languages: Arc::new(DashMap::new()),
        }
    }

    /// Register a language with the given ID
    pub(crate) fn register(&self, language_id: String, language: Language) {
        self.languages.insert(language_id, language);
    }

    /// Remove a language registration by ID.
    pub(crate) fn unregister(&self, language_id: &str) {
        self.languages.remove(language_id);
    }

    /// Get a language by ID
    pub(crate) fn get(&self, language_id: &str) -> Option<Language> {
        self.languages
            .get(language_id)
            .map(|entry| entry.value().clone())
    }

    /// Check if a language is registered
    pub(crate) fn contains(&self, language_id: &str) -> bool {
        self.languages.contains_key(language_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Helper to create a dummy language for testing
    fn dummy_language() -> Language {
        // Use tree-sitter-rust as a test language since it's commonly available
        tree_sitter_rust::LANGUAGE.into()
    }

    #[test]
    fn test_contains_when_loaded() {
        let registry = LanguageRegistry::new();
        registry.register("rust".to_string(), dummy_language());

        assert!(registry.contains("rust"));
    }

    #[test]
    fn test_contains_when_not_loaded() {
        let registry = LanguageRegistry::new();
        // Don't register anything

        assert!(!registry.contains("nonexistent"));
    }
}
