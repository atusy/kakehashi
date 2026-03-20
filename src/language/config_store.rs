use crate::config::{CaptureMappings, WorkspaceSettings};
use crate::error::LockResultExt;
use path_clean::PathClean;
use std::path::PathBuf;
use std::sync::RwLock;

/// Thread-safe cache of workspace settings for the language subsystem.
///
/// Search paths are cleaned/normalized (e.g. `..` resolved) on write.
pub(crate) struct ConfigStore {
    capture_mappings: RwLock<CaptureMappings>,
    search_paths: RwLock<Vec<PathBuf>>,
}

impl ConfigStore {
    pub(crate) fn new() -> Self {
        Self {
            capture_mappings: RwLock::new(CaptureMappings::default()),
            search_paths: RwLock::new(Vec::new()),
        }
    }

    pub(crate) fn update_from_settings(&self, settings: &WorkspaceSettings) {
        self.set_capture_mappings(settings.capture_mappings.clone());
        self.set_search_paths(settings.search_paths.clone());
    }

    fn set_capture_mappings(&self, mappings: CaptureMappings) {
        *self
            .capture_mappings
            .write()
            .recover_poison("ConfigStore::set_capture_mappings") = mappings;
    }

    pub(crate) fn capture_mappings(&self) -> CaptureMappings {
        self.capture_mappings
            .read()
            .recover_poison("ConfigStore::capture_mappings")
            .clone()
    }

    fn set_search_paths(&self, paths: Vec<String>) {
        *self
            .search_paths
            .write()
            .recover_poison("ConfigStore::set_search_paths") = paths
            .into_iter()
            .map(|p| PathBuf::from(p).clean())
            .collect();
    }

    pub(crate) fn search_paths(&self) -> Vec<PathBuf> {
        self.search_paths
            .read()
            .recover_poison("ConfigStore::search_paths")
            .clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::LanguageSettings;
    use std::collections::HashMap;
    use std::sync::Arc;

    #[test]
    fn test_config_store_update_from_settings() {
        let store = ConfigStore::new();

        let settings = WorkspaceSettings {
            languages: HashMap::from([("python".to_string(), LanguageSettings::default())]),
            search_paths: vec!["/search/path".to_string()],
            ..Default::default()
        };

        store.update_from_settings(&settings);

        assert_eq!(store.search_paths(), vec![PathBuf::from("/search/path")]);
        assert_eq!(store.capture_mappings(), settings.capture_mappings);
    }

    #[test]
    fn test_search_paths_normalized_on_update() {
        let store = ConfigStore::new();
        let settings = WorkspaceSettings {
            search_paths: vec!["/path/one".to_string(), "/path/with/../dots".to_string()],
            ..Default::default()
        };
        store.update_from_settings(&settings);
        assert_eq!(
            store.search_paths(),
            vec![PathBuf::from("/path/one"), PathBuf::from("/path/dots")]
        );
    }

    #[test]
    fn test_poison_recovery_on_read() {
        let store = Arc::new(ConfigStore::new());
        store.set_search_paths(vec!["/path/one".to_string()]);

        // Poison the RwLock by panicking while holding a write guard
        let store_clone = Arc::clone(&store);
        let handle = std::thread::spawn(move || {
            let _guard = store_clone.search_paths.write().unwrap();
            panic!("intentional panic to poison the lock");
        });
        let _ = handle.join();

        // Verify the lock is poisoned
        assert!(store.search_paths.read().is_err());

        // search_paths should recover from the poisoned lock
        assert_eq!(store.search_paths(), vec![PathBuf::from("/path/one")]);
    }

    #[test]
    fn test_poison_recovery_on_write() {
        let store = Arc::new(ConfigStore::new());

        // Poison the RwLock by panicking while holding a write guard
        let store_clone = Arc::clone(&store);
        let handle = std::thread::spawn(move || {
            let _guard = store_clone.search_paths.write().unwrap();
            panic!("intentional panic to poison the lock");
        });
        let _ = handle.join();

        // Verify the lock is poisoned
        assert!(store.search_paths.write().is_err());

        // set_search_paths should recover from the poisoned lock
        store.set_search_paths(vec!["/path/one".to_string()]);

        // Verify the data was stored despite the poisoned lock
        assert_eq!(store.search_paths(), vec![PathBuf::from("/path/one")]);
    }
}
