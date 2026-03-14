use crate::error::LspResult;
use dashmap::DashMap;
use std::sync::Arc;
use tree_sitter::Language;

/// Registry for managing loaded Tree-sitter languages
#[derive(Clone)]
pub struct LanguageRegistry {
    languages: Arc<DashMap<String, Language>>,
}

impl Default for LanguageRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl LanguageRegistry {
    pub fn new() -> Self {
        Self {
            languages: Arc::new(DashMap::new()),
        }
    }

    /// Register a language with the given ID
    pub fn register(&self, language_id: String, language: Language) -> LspResult<()> {
        self.languages.insert(language_id, language);
        Ok(())
    }

    /// Register a language with the given ID (compatibility version)
    pub fn register_unchecked(&self, language_id: String, language: Language) {
        let _ = self.register(language_id, language);
    }

    /// Get a language by ID
    pub fn get(&self, language_id: &str) -> Option<Language> {
        self.languages
            .get(language_id)
            .map(|entry| entry.value().clone())
    }

    /// Check if a language is registered
    pub fn contains(&self, language_id: &str) -> bool {
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
        registry.register_unchecked("rust".to_string(), dummy_language());

        assert!(registry.contains("rust"));
    }

    #[test]
    fn test_contains_when_not_loaded() {
        let registry = LanguageRegistry::new();
        // Don't register anything

        assert!(!registry.contains("nonexistent"));
    }
}
