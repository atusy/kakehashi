use crate::config::{CaptureMappings, WorkspaceSettings};
use crate::error::LockResultExt;
use path_clean::PathClean;
use std::path::PathBuf;
use std::sync::RwLock;

/// Thread-safe cache of workspace settings for the language subsystem.
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

    // ========== Capture Mappings ==========
    fn set_capture_mappings(&self, mappings: CaptureMappings) {
        *self
            .capture_mappings
            .write()
            .recover_poison("ConfigStore::set_capture_mappings") = mappings;
    }

    pub(crate) fn get_capture_mappings(&self) -> CaptureMappings {
        self.capture_mappings
            .read()
            .recover_poison("ConfigStore::get_capture_mappings")
            .clone()
    }

    // ========== Search Paths ==========
    fn set_search_paths(&self, paths: Vec<String>) {
        *self
            .search_paths
            .write()
            .recover_poison("ConfigStore::set_search_paths") = paths
            .into_iter()
            .map(|p| PathBuf::from(p).clean())
            .collect();
    }

    pub(crate) fn get_search_paths(&self) -> Vec<PathBuf> {
        self.search_paths
            .read()
            .recover_poison("ConfigStore::get_search_paths")
            .clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::LanguageSettings;
    use std::collections::HashMap;

    #[test]
    fn test_config_store_capture_mappings() {
        let store = ConfigStore::new();

        let mappings = CaptureMappings::default();
        store.set_capture_mappings(mappings.clone());

        let retrieved = store.get_capture_mappings();
        assert_eq!(retrieved.len(), mappings.len());
    }

    #[test]
    fn test_config_store_search_paths() {
        let store = ConfigStore::new();

        let paths = vec!["/path/one".to_string(), "/path/two".to_string()];
        store.set_search_paths(paths);

        let retrieved = store.get_search_paths();
        assert_eq!(retrieved.len(), 2);
        assert_eq!(retrieved[0], PathBuf::from("/path/one"));
    }

    #[test]
    fn test_config_store_update_from_settings() {
        let store = ConfigStore::new();

        let settings = WorkspaceSettings {
            languages: {
                let mut langs = HashMap::new();
                langs.insert("python".to_string(), LanguageSettings::default());
                langs
            },
            search_paths: vec!["/search/path".to_string()],
            capture_mappings: CaptureMappings::default(),
            auto_install: true,
            language_servers: HashMap::new(),
        };

        store.update_from_settings(&settings);

        assert_eq!(
            store.get_search_paths(),
            vec![PathBuf::from("/search/path")]
        );
    }

    #[test]
    fn test_search_paths_string_to_pathbuf_round_trip() {
        let store = ConfigStore::new();
        let input = vec!["/path/one".to_string(), "/path/with/../dots".to_string()];
        store.set_search_paths(input);
        let retrieved = store.get_search_paths();
        assert_eq!(
            retrieved,
            vec![PathBuf::from("/path/one"), PathBuf::from("/path/dots"),]
        );
    }
}
