//! Settings management abstraction for LSP server.
//!
//! This module provides `SettingsManager`, which consolidates workspace settings,
//! client capabilities, and root path management into a single cohesive struct.
//!
//! # Design Rationale
//!
//! Extracting settings management into a dedicated struct provides:
//! - **Single Responsibility**: All configuration state in one place
//! - **Testability**: Methods can be unit tested without full LSP setup
//! - **Thread Safety**: Uses `ArcSwap` and `OnceLock` for concurrent access
//!
//! # Initialization Lifecycle
//!
//! The `SettingsManager` manages state that is set during `initialize()`:
//! - `client_capabilities`: Set once via `set_capabilities()`
//! - `root_path`: Set once via `set_root_path()`
//! - `settings`: Can be updated via `apply_settings()`

use arc_swap::ArcSwap;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use tower_lsp_server::ls_types::{
    ClientCapabilities, GotoCapability, TextDocumentClientCapabilities,
};

use crate::config::WorkspaceSettings;
#[cfg(test)]
use crate::lsp::client::check_semantic_tokens_refresh_support;

/// Centralized manager for workspace settings, capabilities, and configuration.
///
/// `SettingsManager` encapsulates all configuration state, providing a clean API for:
/// - Storing and retrieving workspace settings
/// - Managing client capabilities (set once during initialize)
/// - Tracking workspace root path
/// - Checking capability-dependent features (e.g., semantic tokens refresh)
/// - Auto-install configuration validation
///
/// # Thread Safety
///
/// `SettingsManager` is thread-safe:
/// - `ArcSwap` for atomic updates to settings and root_path
/// - `OnceLock` for one-time initialization of capabilities
pub(crate) struct SettingsManager {
    root_path: ArcSwap<Option<PathBuf>>,
    settings: ArcSwap<WorkspaceSettings>,
    /// Client capabilities from initialize() - immutable after initialization.
    /// Uses OnceLock to enforce "set once, read many" semantics per LSP protocol.
    client_capabilities: OnceLock<ClientCapabilities>,
}

impl std::fmt::Debug for SettingsManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SettingsManager")
            .field("root_path", &"ArcSwap<Option<PathBuf>>")
            .field("settings", &"ArcSwap<WorkspaceSettings>")
            .field("client_capabilities", &"OnceLock<ClientCapabilities>")
            .finish()
    }
}

impl Default for SettingsManager {
    fn default() -> Self {
        Self::new()
    }
}

impl SettingsManager {
    /// Create a new `SettingsManager` with default settings.
    ///
    /// The manager starts with:
    /// - Empty root path
    /// - Default workspace settings
    /// - Unset client capabilities (until initialize() calls set_capabilities)
    pub(crate) fn new() -> Self {
        Self {
            root_path: ArcSwap::new(Arc::new(None)),
            settings: ArcSwap::new(Arc::new(WorkspaceSettings::default())),
            client_capabilities: OnceLock::new(),
        }
    }

    /// Store client capabilities from initialize().
    ///
    /// This should be called exactly once during the LSP initialize handshake.
    /// Subsequent calls are ignored (OnceLock semantics).
    ///
    /// # Arguments
    /// * `caps` - The client capabilities received in InitializeParams
    pub(crate) fn set_capabilities(&self, caps: ClientCapabilities) {
        // OnceLock::set() returns Err if already set - ignore since LSP spec guarantees
        // initialize() is called exactly once per session.
        let _ = self.client_capabilities.set(caps);
    }

    /// Get a reference to the stored client capabilities (test helper).
    ///
    /// Returns `None` if `set_capabilities()` hasn't been called yet.
    #[cfg(test)]
    pub(crate) fn client_capabilities(&self) -> Option<&ClientCapabilities> {
        self.client_capabilities.get()
    }

    /// Get a reference to the underlying OnceLock for client capabilities.
    ///
    /// This is needed for constructing `ClientNotifier` which requires
    /// a reference to the OnceLock (not just the capabilities value).
    pub(crate) fn client_capabilities_lock(&self) -> &OnceLock<ClientCapabilities> {
        &self.client_capabilities
    }

    /// Set the workspace root path.
    ///
    /// Called during initialize() to store the workspace root derived from
    /// `workspace_folders`, `root_uri` (deprecated but supported for backward
    /// compatibility), or the current working directory.
    ///
    /// # Arguments
    /// * `path` - The workspace root path, or None if it couldn't be determined
    pub(crate) fn set_root_path(&self, path: Option<PathBuf>) {
        self.root_path.store(Arc::new(path));
    }

    /// Get the current workspace root path.
    ///
    /// Returns an Arc containing the optional path for efficient sharing.
    pub(crate) fn root_path(&self) -> Arc<Option<PathBuf>> {
        self.root_path.load_full()
    }

    /// Load the current workspace settings.
    ///
    /// Returns an Arc containing the settings for efficient sharing.
    pub(crate) fn load_settings(&self) -> Arc<WorkspaceSettings> {
        self.settings.load_full()
    }

    /// Apply new workspace settings.
    ///
    /// This stores the settings for later retrieval via `load_settings()`.
    ///
    /// # Arguments
    /// * `settings` - The new workspace settings to apply
    pub(crate) fn apply_settings(&self, settings: WorkspaceSettings) {
        self.settings.store(Arc::new(settings));
    }

    /// Returns true only if client declared workspace.semanticTokens.refreshSupport.
    /// Returns false if initialize() hasn't been called yet (OnceLock is empty).
    #[cfg(test)]
    pub(crate) fn supports_semantic_tokens_refresh(&self) -> bool {
        self.client_capabilities
            .get()
            .map(check_semantic_tokens_refresh_support)
            .unwrap_or(false)
    }

    /// Returns true if client declared textDocument.semanticTokens.multilineTokenSupport.
    /// Returns false if initialize() hasn't been called yet (OnceLock is empty).
    ///
    /// Per LSP 3.16.0+, multiline tokens allow a single semantic token to span
    /// multiple lines. When not supported, tokens spanning multiple lines must
    /// be split into per-line tokens.
    pub(crate) fn supports_multiline_tokens(&self) -> bool {
        self.client_capabilities
            .get()
            .and_then(|caps| caps.text_document.as_ref())
            .and_then(|td| td.semantic_tokens.as_ref())
            .and_then(|st| st.multiline_token_support)
            .unwrap_or(false)
    }

    /// Returns true if client declared textDocument.documentSymbol.hierarchicalDocumentSymbolSupport.
    /// Returns false if initialize() hasn't been called yet (OnceLock is empty).
    ///
    /// Per LSP 3.18, when true the server can return DocumentSymbol[] (hierarchical).
    /// When false, the server should return SymbolInformation[] (flat).
    pub(crate) fn supports_hierarchical_document_symbol(&self) -> bool {
        self.client_capabilities
            .get()
            .and_then(|caps| caps.text_document.as_ref())
            .and_then(|td| td.document_symbol.as_ref())
            .and_then(|ds| ds.hierarchical_document_symbol_support)
            .unwrap_or(false)
    }

    /// Check if auto-install is enabled.
    ///
    /// Returns `false` if:
    /// - `autoInstall` is explicitly set to `false` in settings
    /// - `searchPaths` doesn't include the default data directory (auto-install
    ///   would install to a location that isn't being searched)
    pub(crate) fn is_auto_install_enabled(&self) -> bool {
        let settings = self.settings.load();

        // If explicitly disabled, return false
        if !settings.auto_install {
            return false;
        }

        // Check if searchPaths includes the default data directory
        // If not, auto-install would be useless (installed parsers wouldn't be found)
        self.search_paths_include_default_data_dir(&settings.search_paths)
    }

    /// Check if the given search paths include the default data directory.
    pub(crate) fn search_paths_include_default_data_dir(&self, search_paths: &[String]) -> bool {
        let Some(default_dir) = crate::install::default_data_dir() else {
            // Can't determine default dir - allow auto-install anyway
            return true;
        };

        let default_str = default_dir.to_string_lossy();
        search_paths.iter().any(|p| p == default_str.as_ref())
    }

    /// Check if client supports LocationLink[] for a specific goto capability.
    ///
    /// Per LSP spec, when linkSupport is true, the server can return LocationLink[]
    /// which provides richer information (origin selection range, target range, and
    /// target selection range). When false or missing, the server should return
    /// Location[] for compatibility.
    ///
    /// Returns false if initialize() hasn't been called yet (OnceLock is empty).
    fn supports_goto_link(
        &self,
        get_capability: fn(&TextDocumentClientCapabilities) -> &Option<GotoCapability>,
    ) -> bool {
        self.client_capabilities
            .get()
            .and_then(|caps| caps.text_document.as_ref())
            .and_then(|td| get_capability(td).as_ref())
            .and_then(|cap| cap.link_support)
            .unwrap_or(false)
    }

    /// Returns true if client declared textDocument.definition.linkSupport.
    pub(crate) fn supports_definition_link(&self) -> bool {
        self.supports_goto_link(|td| &td.definition)
    }

    /// Check if client supports LocationLink[] for type definition responses.
    pub(crate) fn supports_type_definition_link(&self) -> bool {
        self.supports_goto_link(|td| &td.type_definition)
    }

    /// Check if client supports LocationLink[] for implementation responses.
    pub(crate) fn supports_implementation_link(&self) -> bool {
        self.supports_goto_link(|td| &td.implementation)
    }

    /// Check if client supports LocationLink[] for declaration responses.
    pub(crate) fn supports_declaration_link(&self) -> bool {
        self.supports_goto_link(|td| &td.declaration)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;
    use tower_lsp_server::ls_types::{
        DocumentSymbolClientCapabilities, SemanticTokensWorkspaceClientCapabilities,
        TextDocumentClientCapabilities, WorkspaceClientCapabilities,
    };

    #[test]
    fn test_new_creates_default_state() {
        let manager = SettingsManager::new();

        // Verify initial state
        assert!(manager.root_path().is_none());
        assert!(manager.client_capabilities().is_none());
        assert!(!manager.supports_semantic_tokens_refresh());
    }

    #[test]
    fn test_set_and_get_root_path() {
        let manager = SettingsManager::new();
        let path = PathBuf::from("/test/path");

        manager.set_root_path(Some(path.clone()));

        assert_eq!(manager.root_path().as_ref(), &Some(path));
    }

    #[test]
    fn test_set_root_path_none() {
        let manager = SettingsManager::new();

        // First set a path
        manager.set_root_path(Some(PathBuf::from("/initial")));
        // Then set to None
        manager.set_root_path(None);

        assert!(manager.root_path().is_none());
    }

    #[test]
    fn test_set_capabilities_once() {
        let manager = SettingsManager::new();
        let caps = ClientCapabilities::default();

        manager.set_capabilities(caps);

        assert!(manager.client_capabilities().is_some());
    }

    #[test]
    fn test_set_capabilities_idempotent() {
        let manager = SettingsManager::new();

        // Set with refresh support = true
        let caps1 = ClientCapabilities {
            workspace: Some(WorkspaceClientCapabilities {
                semantic_tokens: Some(SemanticTokensWorkspaceClientCapabilities {
                    refresh_support: Some(true),
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        manager.set_capabilities(caps1);

        // Try to set again with different value - should be ignored
        let caps2 = ClientCapabilities::default();
        manager.set_capabilities(caps2);

        // First value should be retained
        assert!(manager.supports_semantic_tokens_refresh());
    }

    #[test]
    fn test_apply_and_load_settings() {
        let manager = SettingsManager::new();
        let settings = WorkspaceSettings {
            auto_install: false,
            ..Default::default()
        };

        manager.apply_settings(settings);
        let loaded = manager.load_settings();

        assert!(!loaded.auto_install);
    }

    /// Tests for supports_semantic_tokens_refresh capability checking.
    ///
    /// Arguments control what's present at each level:
    /// - `workspace`: Whether workspace field is Some
    /// - `semantic_tokens`: Whether semantic_tokens is Some (requires workspace)
    /// - `refresh_support`: The actual refresh_support value (requires semantic_tokens)
    /// - `expected`: The expected result of refresh support
    #[rstest]
    #[case::refresh_support_true(true, true, Some(true), true)]
    #[case::refresh_support_false(true, true, Some(false), false)]
    #[case::refresh_support_none(true, true, None, false)]
    #[case::semantic_tokens_none(true, false, None, false)]
    #[case::workspace_empty(true, false, None, false)]
    #[case::workspace_none(false, false, None, false)]
    fn test_supports_semantic_tokens_refresh(
        #[case] workspace: bool,
        #[case] semantic_tokens: bool,
        #[case] refresh_support: Option<bool>,
        #[case] expected: bool,
    ) {
        let manager = SettingsManager::new();
        let caps = ClientCapabilities {
            workspace: if workspace {
                Some(WorkspaceClientCapabilities {
                    semantic_tokens: if semantic_tokens {
                        Some(SemanticTokensWorkspaceClientCapabilities { refresh_support })
                    } else {
                        None
                    },
                    ..Default::default()
                })
            } else {
                None
            },
            ..Default::default()
        };
        manager.set_capabilities(caps);
        assert_eq!(manager.supports_semantic_tokens_refresh(), expected);
    }

    #[test]
    fn test_supports_semantic_tokens_refresh_before_init() {
        // Before initialize() is called, capabilities are not set
        let manager = SettingsManager::new();
        // Should return false (safe default)
        assert!(!manager.supports_semantic_tokens_refresh());
    }

    #[test]
    fn test_supports_hierarchical_document_symbol_before_init() {
        let manager = SettingsManager::new();
        assert!(!manager.supports_hierarchical_document_symbol());
    }

    /// Parameterized tests for supports_hierarchical_document_symbol().
    ///
    /// Arguments control what's present at each level:
    /// - `text_document`: Whether text_document field is Some
    /// - `document_symbol`: Whether document_symbol is Some
    /// - `hierarchical`: The actual hierarchical_document_symbol_support value
    /// - `expected`: The expected result
    #[rstest]
    #[case::hierarchical_true(true, true, Some(true), true)]
    #[case::hierarchical_false(true, true, Some(false), false)]
    #[case::hierarchical_none(true, true, None, false)]
    #[case::document_symbol_none(true, false, None, false)]
    #[case::text_document_none(false, false, None, false)]
    fn test_supports_hierarchical_document_symbol(
        #[case] text_document: bool,
        #[case] document_symbol: bool,
        #[case] hierarchical: Option<bool>,
        #[case] expected: bool,
    ) {
        let manager = SettingsManager::new();
        let caps = ClientCapabilities {
            text_document: if text_document {
                Some(TextDocumentClientCapabilities {
                    document_symbol: if document_symbol {
                        Some(DocumentSymbolClientCapabilities {
                            hierarchical_document_symbol_support: hierarchical,
                            ..Default::default()
                        })
                    } else {
                        None
                    },
                    ..Default::default()
                })
            } else {
                None
            },
            ..Default::default()
        };
        manager.set_capabilities(caps);
        assert_eq!(manager.supports_hierarchical_document_symbol(), expected);
    }

    #[test]
    fn test_supports_definition_link_before_init() {
        // Before initialize() is called, capabilities are not set
        let manager = SettingsManager::new();
        // Should return false (safe default - use Location[] for compatibility)
        assert!(!manager.supports_definition_link());
    }

    #[test]
    fn test_supports_type_definition_link_before_init() {
        // Before initialize() is called, capabilities are not set
        let manager = SettingsManager::new();
        // Should return false (safe default - use Location[] for compatibility)
        assert!(!manager.supports_type_definition_link());
    }

    #[test]
    fn test_supports_implementation_link_before_init() {
        // Before initialize() is called, capabilities are not set
        let manager = SettingsManager::new();
        // Should return false (safe default - use Location[] for compatibility)
        assert!(!manager.supports_implementation_link());
    }

    #[test]
    fn test_supports_declaration_link_before_init() {
        // Before initialize() is called, capabilities are not set
        let manager = SettingsManager::new();
        // Should return false (safe default - use Location[] for compatibility)
        assert!(!manager.supports_declaration_link());
    }

    /// Parameterized tests for supports_*_link() capability checking.
    ///
    /// Each test case varies:
    /// - `text_document`: Whether text_document field is Some
    /// - `goto_cap`: Whether the specific goto capability is Some (requires text_document)
    /// - `link_support`: The actual link_support value (requires goto_cap)
    /// - `expected`: The expected result
    ///
    /// All four goto link methods (definition, type_definition, implementation,
    /// declaration) use the same GotoCapability struct, so one parameterized test
    /// covers all four by calling each method.
    #[rstest]
    #[case::link_support_true(true, true, Some(true), true)]
    #[case::link_support_false(true, true, Some(false), false)]
    #[case::link_support_none(true, true, None, false)]
    #[case::goto_cap_none(true, false, None, false)]
    #[case::text_document_none(false, false, None, false)]
    fn test_supports_goto_link(
        #[case] text_document: bool,
        #[case] goto_cap: bool,
        #[case] link_support: Option<bool>,
        #[case] expected: bool,
    ) {
        use tower_lsp_server::ls_types::{GotoCapability, TextDocumentClientCapabilities};

        let goto = if goto_cap {
            Some(GotoCapability {
                dynamic_registration: None,
                link_support,
            })
        } else {
            None
        };

        let manager = SettingsManager::new();
        let caps = ClientCapabilities {
            text_document: if text_document {
                Some(TextDocumentClientCapabilities {
                    definition: goto,
                    type_definition: goto,
                    implementation: goto,
                    declaration: goto,
                    ..Default::default()
                })
            } else {
                None
            },
            ..Default::default()
        };
        manager.set_capabilities(caps);

        assert_eq!(
            manager.supports_definition_link(),
            expected,
            "supports_definition_link"
        );
        assert_eq!(
            manager.supports_type_definition_link(),
            expected,
            "supports_type_definition_link"
        );
        assert_eq!(
            manager.supports_implementation_link(),
            expected,
            "supports_implementation_link"
        );
        assert_eq!(
            manager.supports_declaration_link(),
            expected,
            "supports_declaration_link"
        );
    }
}
