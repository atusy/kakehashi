use crate::config::{CaptureMappings, LanguageSettings, WorkspaceSettings};
use crate::error::LockResultExt;
use std::collections::HashMap;
use std::sync::RwLock;

/// Thread-safe cache of workspace settings for the language subsystem.
pub struct ConfigStore {
    language_configs: RwLock<HashMap<String, LanguageSettings>>,
    capture_mappings: RwLock<CaptureMappings>,
    search_paths: RwLock<Vec<String>>,
}

impl ConfigStore {
    pub fn new() -> Self {
        Self {
            language_configs: RwLock::new(HashMap::new()),
            capture_mappings: RwLock::new(CaptureMappings::default()),
            search_paths: RwLock::new(Vec::new()),
        }
    }

    // ========== Language Configs ==========
    pub fn set_language_configs(&self, configs: HashMap<String, LanguageSettings>) {
        *self
            .language_configs
            .write()
            .recover_poison("ConfigStore::set_language_configs") = configs;
    }

    pub fn update_from_settings(&self, settings: &WorkspaceSettings) {
        self.set_language_configs(settings.languages.clone());
        self.set_capture_mappings(settings.capture_mappings.clone());
        self.set_search_paths(settings.search_paths.clone());
    }

    // ========== Capture Mappings ==========
    pub fn set_capture_mappings(&self, mappings: CaptureMappings) {
        *self
            .capture_mappings
            .write()
            .recover_poison("ConfigStore::set_capture_mappings") = mappings;
    }

    pub fn get_capture_mappings(&self) -> CaptureMappings {
        self.capture_mappings
            .read()
            .recover_poison("ConfigStore::get_capture_mappings")
            .clone()
    }

    // ========== Search Paths ==========
    pub fn set_search_paths(&self, paths: Vec<String>) {
        *self
            .search_paths
            .write()
            .recover_poison("ConfigStore::set_search_paths") = paths;
    }

    pub fn get_search_paths(&self) -> Vec<String> {
        self.search_paths
            .read()
            .recover_poison("ConfigStore::get_search_paths")
            .clone()
    }
}

impl Default for ConfigStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    impl ConfigStore {
        fn get_language_config(&self, lang_name: &str) -> Option<LanguageSettings> {
            self.language_configs
                .read()
                .recover_poison("ConfigStore::get_language_config")
                .get(lang_name)
                .cloned()
        }
    }

    #[test]
    fn test_config_store_language_configs() {
        use crate::config::settings::{QueryItem, QueryKind};

        let store = ConfigStore::new();

        let mut configs = HashMap::new();
        configs.insert(
            "rust".to_string(),
            LanguageSettings {
                parser: Some("/path/to/rust.so".to_string()),
                queries: Some(vec![QueryItem {
                    path: "/path/to/highlights.scm".to_string(),
                    kind: Some(QueryKind::Highlights),
                }]),
                ..Default::default()
            },
        );

        store.set_language_configs(configs.clone());

        let rust_config = store.get_language_config("rust").unwrap();
        assert_eq!(rust_config.parser, Some("/path/to/rust.so".to_string()));
    }

    #[test]
    fn test_poison_recovery_on_read() {
        let store = Arc::new(ConfigStore::new());
        let mut configs = HashMap::new();
        configs.insert("rust".to_string(), LanguageSettings::default());
        store.set_language_configs(configs);

        // Poison the RwLock by panicking while holding a write guard
        let store_clone = Arc::clone(&store);
        let handle = std::thread::spawn(move || {
            let _guard = store_clone.language_configs.write().unwrap();
            panic!("intentional panic to poison the lock");
        });
        let _ = handle.join();

        // Verify the lock is poisoned
        assert!(store.language_configs.read().is_err());

        // get_language_config should recover from the poisoned lock
        assert!(store.get_language_config("rust").is_some());
    }

    #[test]
    fn test_poison_recovery_on_write() {
        let store = Arc::new(ConfigStore::new());

        // Poison the RwLock by panicking while holding a write guard
        let store_clone = Arc::clone(&store);
        let handle = std::thread::spawn(move || {
            let _guard = store_clone.language_configs.write().unwrap();
            panic!("intentional panic to poison the lock");
        });
        let _ = handle.join();

        // Verify the lock is poisoned
        assert!(store.language_configs.write().is_err());

        // set_language_configs should recover from the poisoned lock
        let mut configs = HashMap::new();
        configs.insert("rust".to_string(), LanguageSettings::default());
        store.set_language_configs(configs);

        // Verify the config was stored despite the poisoned lock
        assert!(store.get_language_config("rust").is_some());
    }

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
        store.set_search_paths(paths.clone());

        let retrieved = store.get_search_paths();
        assert_eq!(retrieved.len(), 2);
        assert_eq!(retrieved[0], "/path/one");
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

        assert!(store.get_language_config("python").is_some());
        assert_eq!(store.get_search_paths(), vec!["/search/path".to_string()]);
    }
}
