use crate::error::LockResultExt;
use std::collections::HashMap;
use std::sync::RwLock;

/// Resolves file types to language identifiers
pub struct FiletypeResolver {
    filetype_map: RwLock<HashMap<String, String>>,
}

impl FiletypeResolver {
    pub fn new() -> Self {
        Self {
            filetype_map: RwLock::new(HashMap::new()),
        }
    }

    // build_from_settings and build_from_configs removed in PBI-061
    // Language detection now relies on languageId from DidOpen, not config filetypes

    /// Set the filetype map directly
    pub fn set_filetype_map(&self, map: HashMap<String, String>) {
        *self
            .filetype_map
            .write()
            .recover_poison("FiletypeResolver::set_filetype_map") = map;
    }

    /// Get language for a document path (URI path or file path)
    pub fn get_language_for_path(&self, path: &str) -> Option<String> {
        let extension = Self::extract_extension(path);
        self.filetype_map
            .read()
            .recover_poison("FiletypeResolver::get_language_for_path")
            .get(extension)
            .cloned()
    }

    /// Get language for a file extension
    pub fn get_language_for_extension(&self, extension: &str) -> Option<String> {
        self.filetype_map
            .read()
            .recover_poison("FiletypeResolver::get_language_for_extension")
            .get(extension)
            .cloned()
    }

    /// Get a copy of the entire filetype map
    pub fn get_filetype_map(&self) -> HashMap<String, String> {
        self.filetype_map
            .read()
            .recover_poison("FiletypeResolver::get_filetype_map")
            .clone()
    }

    /// Add a single filetype mapping
    pub fn add_mapping(&self, extension: String, language: String) {
        self.filetype_map
            .write()
            .recover_poison("FiletypeResolver::add_mapping")
            .insert(extension, language);
    }

    /// Remove a filetype mapping
    pub fn remove_mapping(&self, extension: &str) -> Option<String> {
        self.filetype_map
            .write()
            .recover_poison("FiletypeResolver::remove_mapping")
            .remove(extension)
    }

    /// Clear all mappings
    pub fn clear(&self) {
        self.filetype_map
            .write()
            .recover_poison("FiletypeResolver::clear")
            .clear();
    }

    /// Check if a language is registered for any extension
    pub fn has_language(&self, language: &str) -> bool {
        self.filetype_map
            .read()
            .recover_poison("FiletypeResolver::has_language")
            .values()
            .any(|l| l == language)
    }

    /// Get all extensions for a language
    pub fn get_extensions_for_language(&self, language: &str) -> Vec<String> {
        self.filetype_map
            .read()
            .recover_poison("FiletypeResolver::get_extensions_for_language")
            .iter()
            .filter_map(|(ext, lang)| {
                if lang == language {
                    Some(ext.clone())
                } else {
                    None
                }
            })
            .collect()
    }

    /// Extract file extension from a path
    fn extract_extension(path: &str) -> &str {
        path.split('.').next_back().unwrap_or("")
    }
}

impl Default for FiletypeResolver {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn test_filetype_resolver_basic() {
        let resolver = FiletypeResolver::new();

        resolver.add_mapping("rs".to_string(), "rust".to_string());
        resolver.add_mapping("py".to_string(), "python".to_string());

        assert_eq!(
            resolver.get_language_for_extension("rs"),
            Some("rust".to_string())
        );
        assert_eq!(
            resolver.get_language_for_extension("py"),
            Some("python".to_string())
        );
        assert_eq!(resolver.get_language_for_extension("txt"), None);
    }

    // test_filetype_resolver_from_settings removed in PBI-061 S48.4
    // Method build_from_settings no longer exists

    #[test]
    fn test_poison_recovery_on_read() {
        let resolver = Arc::new(FiletypeResolver::new());
        resolver.add_mapping("rs".to_string(), "rust".to_string());

        // Poison the RwLock by panicking while holding a write guard
        let resolver_clone = Arc::clone(&resolver);
        let handle = std::thread::spawn(move || {
            let _guard = resolver_clone.filetype_map.write().unwrap();
            panic!("intentional panic to poison the lock");
        });
        let _ = handle.join();

        // Verify the lock is poisoned
        assert!(resolver.filetype_map.read().is_err());

        // get_language_for_extension should recover from the poisoned lock
        assert_eq!(
            resolver.get_language_for_extension("rs"),
            Some("rust".to_string())
        );
    }

    #[test]
    fn test_poison_recovery_on_write() {
        let resolver = Arc::new(FiletypeResolver::new());

        // Poison the RwLock by panicking while holding a write guard
        let resolver_clone = Arc::clone(&resolver);
        let handle = std::thread::spawn(move || {
            let _guard = resolver_clone.filetype_map.write().unwrap();
            panic!("intentional panic to poison the lock");
        });
        let _ = handle.join();

        // Verify the lock is poisoned
        assert!(resolver.filetype_map.write().is_err());

        // add_mapping should recover from the poisoned lock
        resolver.add_mapping("rs".to_string(), "rust".to_string());

        // Verify the mapping was stored despite the poisoned lock
        assert_eq!(
            resolver.get_language_for_extension("rs"),
            Some("rust".to_string())
        );
    }

    #[test]
    fn test_filetype_resolver_document_path() {
        let resolver = FiletypeResolver::new();
        resolver.add_mapping("rs".to_string(), "rust".to_string());

        assert_eq!(
            resolver.get_language_for_path("/path/to/file.rs"),
            Some("rust".to_string())
        );

        assert_eq!(resolver.get_language_for_path("/path/to/file"), None);
    }

    #[test]
    fn test_filetype_resolver_reverse_lookup() {
        let resolver = FiletypeResolver::new();

        resolver.add_mapping("rs".to_string(), "rust".to_string());
        resolver.add_mapping("rust".to_string(), "rust".to_string());
        resolver.add_mapping("py".to_string(), "python".to_string());

        assert!(resolver.has_language("rust"));
        assert!(resolver.has_language("python"));
        assert!(!resolver.has_language("javascript"));

        let rust_exts = resolver.get_extensions_for_language("rust");
        assert_eq!(rust_exts.len(), 2);
        assert!(rust_exts.contains(&"rs".to_string()));
        assert!(rust_exts.contains(&"rust".to_string()));
    }

    #[test]
    fn test_filetype_resolver_remove_and_clear() {
        let resolver = FiletypeResolver::new();

        resolver.add_mapping("rs".to_string(), "rust".to_string());
        resolver.add_mapping("py".to_string(), "python".to_string());

        assert_eq!(resolver.remove_mapping("rs"), Some("rust".to_string()));
        assert_eq!(resolver.get_language_for_extension("rs"), None);

        resolver.clear();
        assert_eq!(resolver.get_language_for_extension("py"), None);
        assert_eq!(resolver.get_filetype_map().len(), 0);
    }
}
