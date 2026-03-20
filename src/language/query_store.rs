use crate::error::LockResultExt;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use tree_sitter::Query;

/// Stores and manages Tree-sitter queries for different languages
pub(crate) struct QueryStore {
    highlight_queries: RwLock<HashMap<String, Arc<Query>>>,
    locals_queries: RwLock<HashMap<String, Arc<Query>>>,
    injection_queries: RwLock<HashMap<String, Arc<Query>>>,
}

impl QueryStore {
    pub(crate) fn new() -> Self {
        Self {
            highlight_queries: RwLock::new(HashMap::new()),
            locals_queries: RwLock::new(HashMap::new()),
            injection_queries: RwLock::new(HashMap::new()),
        }
    }

    pub(crate) fn insert_highlight_query(&self, lang_name: String, query: Arc<Query>) {
        self.highlight_queries
            .write()
            .recover_poison(format_args!(
                "QueryStore::insert_highlight_query({})",
                lang_name
            ))
            .insert(lang_name, query);
    }

    pub(crate) fn highlight_query(&self, lang_name: &str) -> Option<Arc<Query>> {
        self.highlight_queries
            .read()
            .recover_poison(format_args!("QueryStore::highlight_query({})", lang_name))
            .get(lang_name)
            .cloned()
    }

    pub(crate) fn has_highlight_query(&self, lang_name: &str) -> bool {
        self.highlight_queries
            .read()
            .recover_poison(format_args!(
                "QueryStore::has_highlight_query({})",
                lang_name
            ))
            .contains_key(lang_name)
    }

    pub(crate) fn insert_locals_query(&self, lang_name: String, query: Arc<Query>) {
        self.locals_queries
            .write()
            .recover_poison(format_args!(
                "QueryStore::insert_locals_query({})",
                lang_name
            ))
            .insert(lang_name, query);
    }

    pub(crate) fn insert_injection_query(&self, lang_name: String, query: Arc<Query>) {
        self.injection_queries
            .write()
            .recover_poison(format_args!(
                "QueryStore::insert_injection_query({})",
                lang_name
            ))
            .insert(lang_name, query);
    }

    pub(crate) fn injection_query(&self, lang_name: &str) -> Option<Arc<Query>> {
        self.injection_queries
            .read()
            .recover_poison(format_args!("QueryStore::injection_query({})", lang_name))
            .get(lang_name)
            .cloned()
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

        let query_str = "(identifier) @variable";
        let query = Arc::new(Query::new(&lang, query_str).unwrap());

        // Test highlight queries
        assert!(!store.has_highlight_query("rust"));
        store.insert_highlight_query("rust".to_string(), query.clone());
        assert!(store.has_highlight_query("rust"));
        assert_eq!(store.highlight_query("rust").unwrap(), query);

        // Test locals queries - verify insert doesn't panic
        store.insert_locals_query("rust".to_string(), query.clone());
    }

    #[test]
    fn test_poison_recovery_on_read() {
        let store = Arc::new(QueryStore::new());
        let lang: tree_sitter::Language = tree_sitter_rust::LANGUAGE.into();
        let query = Arc::new(Query::new(&lang, "(identifier) @variable").unwrap());
        store.insert_highlight_query("rust".to_string(), query.clone());

        // Poison the RwLock by panicking while holding a write guard
        let store_clone = Arc::clone(&store);
        let handle = std::thread::spawn(move || {
            let _guard = store_clone.highlight_queries.write().unwrap();
            panic!("intentional panic to poison the lock");
        });
        let _ = handle.join();

        // Verify the lock is poisoned
        assert!(store.highlight_queries.read().is_err());

        // highlight_query should recover from the poisoned lock
        let retrieved = store.highlight_query("rust");
        assert_eq!(retrieved.unwrap(), query);
    }

    #[test]
    fn test_poison_recovery_on_write() {
        let store = Arc::new(QueryStore::new());

        // Poison the RwLock by panicking while holding a write guard
        let store_clone = Arc::clone(&store);
        let handle = std::thread::spawn(move || {
            let _guard = store_clone.highlight_queries.write().unwrap();
            panic!("intentional panic to poison the lock");
        });
        let _ = handle.join();

        // Verify the lock is poisoned
        assert!(store.highlight_queries.write().is_err());

        // insert_highlight_query should recover from the poisoned lock
        let lang: tree_sitter::Language = tree_sitter_rust::LANGUAGE.into();
        let query = Arc::new(Query::new(&lang, "(identifier) @variable").unwrap());
        store.insert_highlight_query("rust".to_string(), query.clone());

        // Verify the query was stored despite the poisoned lock
        assert!(store.has_highlight_query("rust"));
    }
}
