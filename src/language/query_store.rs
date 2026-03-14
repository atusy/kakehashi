use crate::error::LockResultExt;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use tree_sitter::Query;

/// Stores and manages Tree-sitter queries for different languages
pub struct QueryStore {
    highlight_queries: RwLock<HashMap<String, Arc<Query>>>,
    locals_queries: RwLock<HashMap<String, Arc<Query>>>,
    injection_queries: RwLock<HashMap<String, Arc<Query>>>,
}

impl QueryStore {
    pub fn new() -> Self {
        Self {
            highlight_queries: RwLock::new(HashMap::new()),
            locals_queries: RwLock::new(HashMap::new()),
            injection_queries: RwLock::new(HashMap::new()),
        }
    }

    // ========== Highlight Queries ==========
    pub fn insert_highlight_query(&self, lang_name: String, query: Arc<Query>) {
        self.highlight_queries
            .write()
            .recover_poison("QueryStore::insert_highlight_query")
            .insert(lang_name, query);
    }

    pub fn get_highlight_query(&self, lang_name: &str) -> Option<Arc<Query>> {
        self.highlight_queries
            .read()
            .recover_poison("QueryStore::get_highlight_query")
            .get(lang_name)
            .cloned()
    }

    pub fn has_highlight_query(&self, lang_name: &str) -> bool {
        self.highlight_queries
            .read()
            .recover_poison("QueryStore::has_highlight_query")
            .contains_key(lang_name)
    }

    // ========== Locals Queries ==========
    pub fn insert_locals_query(&self, lang_name: String, query: Arc<Query>) {
        self.locals_queries
            .write()
            .recover_poison("QueryStore::insert_locals_query")
            .insert(lang_name, query);
    }

    pub fn get_locals_query(&self, lang_name: &str) -> Option<Arc<Query>> {
        self.locals_queries
            .read()
            .recover_poison("QueryStore::get_locals_query")
            .get(lang_name)
            .cloned()
    }

    // ========== Injection Queries ==========
    pub fn insert_injection_query(&self, lang_name: String, query: Arc<Query>) {
        self.injection_queries
            .write()
            .recover_poison("QueryStore::insert_injection_query")
            .insert(lang_name, query);
    }

    pub fn get_injection_query(&self, lang_name: &str) -> Option<Arc<Query>> {
        self.injection_queries
            .read()
            .recover_poison("QueryStore::get_injection_query")
            .get(lang_name)
            .cloned()
    }

    /// Clear all queries for a specific language
    pub fn clear_language(&self, lang_name: &str) {
        self.highlight_queries
            .write()
            .recover_poison("QueryStore::clear_language(highlight)")
            .remove(lang_name);
        self.locals_queries
            .write()
            .recover_poison("QueryStore::clear_language(locals)")
            .remove(lang_name);
        self.injection_queries
            .write()
            .recover_poison("QueryStore::clear_language(injection)")
            .remove(lang_name);
    }

    /// Clear all queries
    pub fn clear_all(&self) {
        self.highlight_queries
            .write()
            .recover_poison("QueryStore::clear_all(highlight)")
            .clear();
        self.locals_queries
            .write()
            .recover_poison("QueryStore::clear_all(locals)")
            .clear();
        self.injection_queries
            .write()
            .recover_poison("QueryStore::clear_all(injection)")
            .clear();
    }
}

impl Default for QueryStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_query_store_operations() {
        let store = QueryStore::new();
        let lang: tree_sitter::Language = tree_sitter_rust::LANGUAGE.into();

        // Create a simple query
        let query_str = "(identifier) @variable";
        let query = Arc::new(Query::new(&lang, query_str).unwrap());

        // Test highlight queries
        assert!(!store.has_highlight_query("rust"));
        store.insert_highlight_query("rust".to_string(), query.clone());
        assert!(store.has_highlight_query("rust"));
        assert_eq!(store.get_highlight_query("rust").unwrap(), query);

        // Test locals queries
        store.insert_locals_query("rust".to_string(), query.clone());
        assert_eq!(store.get_locals_query("rust").unwrap(), query);

        // Test clear language
        store.clear_language("rust");
        assert!(!store.has_highlight_query("rust"));
        assert!(store.get_locals_query("rust").is_none());
    }

    #[test]
    fn test_query_store_clear_all() {
        let store = QueryStore::new();
        let lang: tree_sitter::Language = tree_sitter_rust::LANGUAGE.into();

        let query = Arc::new(Query::new(&lang, "(identifier) @variable").unwrap());

        store.insert_highlight_query("rust".to_string(), query.clone());
        store.insert_highlight_query("python".to_string(), query.clone());
        store.insert_locals_query("rust".to_string(), query.clone());

        store.clear_all();

        assert!(!store.has_highlight_query("rust"));
        assert!(!store.has_highlight_query("python"));
        assert!(store.get_locals_query("rust").is_none());
    }
}
