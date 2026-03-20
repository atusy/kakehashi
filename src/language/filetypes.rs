use crate::error::LockResultExt;
use std::collections::HashMap;
use std::sync::RwLock;

/// Resolves file types to language identifiers
pub(crate) struct FiletypeResolver {
    filetype_map: RwLock<HashMap<String, String>>,
}

impl FiletypeResolver {
    pub(crate) fn new() -> Self {
        Self {
            filetype_map: RwLock::new(HashMap::new()),
        }
    }

    /// Get language for a document path (URI path or file path)
    pub(crate) fn get_language_for_path(&self, path: &str) -> Option<String> {
        let extension = Self::extract_extension(path);
        self.filetype_map
            .read()
            .recover_poison("FiletypeResolver::get_language_for_path")
            .get(extension)
            .cloned()
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

    impl FiletypeResolver {
        fn add_mapping(&self, extension: String, language: String) {
            self.filetype_map
                .write()
                .recover_poison("FiletypeResolver::add_mapping")
                .insert(extension, language);
        }

        fn get_language_for_extension(&self, extension: &str) -> Option<String> {
            self.filetype_map
                .read()
                .recover_poison("FiletypeResolver::get_language_for_extension")
                .get(extension)
                .cloned()
        }
    }

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
}
