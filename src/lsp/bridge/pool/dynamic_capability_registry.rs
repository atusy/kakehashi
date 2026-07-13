use std::collections::{HashMap, HashSet};
use std::sync::RwLock;

use tower_lsp_server::ls_types::{Registration, Unregistration};

use crate::error::LockResultExt;

/// Thread-safe store for dynamically registered LSP capabilities.
///
/// Downstream language servers (e.g., Pyright) register capabilities dynamically
/// via `client/registerCapability` after the initialize handshake. This registry
/// tracks those registrations so the bridge can check capability support.
///
/// The LSP spec allows multiple registrations per method (with different document
/// selectors and IDs). We key by registration ID, allowing multiple same-method
/// registrations to coexist (e.g., two `textDocument/diagnostic` registrations
/// with different document selectors).
pub(crate) struct DynamicCapabilityRegistry {
    registrations: RwLock<CapabilityRegistrationState>,
    /// Serializes capability acknowledgement/publication with outbound actions
    /// whose validity depends on the published registry state.
    workspace_folder_ordering: std::sync::Arc<tokio::sync::Mutex<()>>,
}

#[derive(Default)]
struct CapabilityRegistrationState {
    active: HashMap<String, Registration>,
    revoked_methods_by_id: HashMap<String, HashSet<String>>,
}

impl DynamicCapabilityRegistry {
    pub(crate) fn new() -> Self {
        Self {
            registrations: RwLock::new(CapabilityRegistrationState::default()),
            workspace_folder_ordering: std::sync::Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    pub(crate) fn workspace_folder_ordering(&self) -> std::sync::Arc<tokio::sync::Mutex<()>> {
        std::sync::Arc::clone(&self.workspace_folder_ordering)
    }

    pub(crate) fn register(&self, registrations: Vec<Registration>) {
        let mut guard = self
            .registrations
            .write()
            .recover_poison("DynamicCapabilityRegistry::register");
        for reg in registrations {
            let clear_id = guard
                .revoked_methods_by_id
                .get_mut(&reg.id)
                .is_some_and(|methods| {
                    methods.remove(&reg.method);
                    methods.is_empty()
                });
            if clear_id {
                guard.revoked_methods_by_id.remove(&reg.id);
            }
            guard.active.insert(reg.id.clone(), reg);
        }
    }

    pub(crate) fn unregister(&self, unregistrations: Vec<Unregistration>) {
        let mut guard = self
            .registrations
            .write()
            .recover_poison("DynamicCapabilityRegistry::unregister");
        for unreg in unregistrations {
            guard.active.remove(&unreg.id);
            guard
                .revoked_methods_by_id
                .entry(unreg.id)
                .or_default()
                .insert(unreg.method);
        }
    }

    pub(crate) fn has_registration(&self, method: &str) -> bool {
        self.registrations
            .read()
            .recover_poison("DynamicCapabilityRegistry::has_registration")
            .active
            .values()
            .any(|r| r.method == method)
    }

    /// Whether a registration ID/method pair was explicitly revoked and has
    /// not since been registered again. This also covers registrations
    /// advertised in initialize capabilities rather than through
    /// `client/registerCapability`.
    pub(crate) fn is_registration_revoked(&self, id: &str, method: &str) -> bool {
        self.registrations
            .read()
            .recover_poison("DynamicCapabilityRegistry::is_registration_revoked")
            .revoked_methods_by_id
            .get(id)
            .is_some_and(|methods| methods.contains(method))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::thread;

    use tower_lsp_server::ls_types::{Registration, Unregistration};

    use super::DynamicCapabilityRegistry;

    fn make_registration(id: &str, method: &str) -> Registration {
        Registration {
            id: id.to_string(),
            method: method.to_string(),
            register_options: None,
        }
    }

    fn make_unregistration(id: &str, method: &str) -> Unregistration {
        Unregistration {
            id: id.to_string(),
            method: method.to_string(),
        }
    }

    #[test]
    fn register_stores_capability() {
        let registry = DynamicCapabilityRegistry::new();
        let reg = make_registration("1", "textDocument/completion");

        registry.register(vec![reg]);

        assert!(registry.has_registration("textDocument/completion"));
    }

    #[test]
    fn unregister_removes_capability() {
        let registry = DynamicCapabilityRegistry::new();
        let reg = make_registration("1", "textDocument/completion");
        registry.register(vec![reg]);

        let unreg = make_unregistration("1", "textDocument/completion");
        registry.unregister(vec![unreg]);

        assert!(!registry.has_registration("textDocument/completion"));
        assert!(registry.is_registration_revoked("1", "textDocument/completion"));
    }

    #[test]
    fn registering_an_id_clears_its_revocation() {
        let registry = DynamicCapabilityRegistry::new();
        let unreg = make_unregistration("1", "textDocument/completion");
        registry.unregister(vec![unreg]);
        assert!(registry.is_registration_revoked("1", "textDocument/completion"));

        registry.register(vec![make_registration("1", "textDocument/completion")]);

        assert!(!registry.is_registration_revoked("1", "textDocument/completion"));
        assert!(registry.has_registration("textDocument/completion"));
    }

    #[test]
    fn reusing_an_id_for_another_method_preserves_revocation() {
        let registry = DynamicCapabilityRegistry::new();
        registry.unregister(vec![make_unregistration(
            "wf-id",
            "workspace/didChangeWorkspaceFolders",
        )]);

        registry.register(vec![make_registration("wf-id", "textDocument/hover")]);

        assert!(registry.is_registration_revoked("wf-id", "workspace/didChangeWorkspaceFolders"));
        assert!(registry.has_registration("textDocument/hover"));
    }

    #[test]
    fn has_registration_returns_false_for_unknown() {
        let registry = DynamicCapabilityRegistry::new();

        assert!(!registry.has_registration("textDocument/hover"));
    }

    #[test]
    fn register_coexists_same_method_different_ids() {
        let registry = DynamicCapabilityRegistry::new();
        let reg1 = make_registration("1", "textDocument/completion");
        let reg2 = make_registration("2", "textDocument/completion");

        registry.register(vec![reg1]);
        registry.register(vec![reg2]);

        assert!(registry.has_registration("textDocument/completion"));
        // Verify both registrations are stored (keyed by ID)
        let guard = registry.registrations.read().unwrap();
        assert_eq!(guard.active.get("1").unwrap().id, "1");
        assert_eq!(guard.active.get("2").unwrap().id, "2");
    }

    #[test]
    fn unregister_removes_by_id_not_method() {
        let registry = DynamicCapabilityRegistry::new();
        let reg1 = make_registration("diag-1", "textDocument/diagnostic");
        let reg2 = make_registration("diag-2", "textDocument/diagnostic");

        registry.register(vec![reg1, reg2]);

        // Unregister only "diag-1"
        let unreg = make_unregistration("diag-1", "textDocument/diagnostic");
        registry.unregister(vec![unreg]);

        // "diag-2" should still be registered
        assert!(registry.has_registration("textDocument/diagnostic"));
    }

    #[test]
    fn poison_recovery_on_read() {
        let registry = Arc::new(DynamicCapabilityRegistry::new());
        let reg = make_registration("1", "textDocument/completion");
        registry.register(vec![reg]);

        // Poison the RwLock by panicking while holding a write guard
        let registry_clone = Arc::clone(&registry);
        let handle = thread::spawn(move || {
            let _guard = registry_clone.registrations.write().unwrap();
            panic!("intentional panic to poison the lock");
        });
        let _ = handle.join(); // Wait for thread to finish (it panicked)

        // Verify the lock is poisoned
        assert!(registry.registrations.read().is_err());

        // has_registration should recover from the poisoned lock
        assert!(registry.has_registration("textDocument/completion"));
    }

    #[test]
    fn poison_recovery_on_write() {
        let registry = Arc::new(DynamicCapabilityRegistry::new());

        // Poison the RwLock by panicking while holding a write guard
        let registry_clone = Arc::clone(&registry);
        let handle = thread::spawn(move || {
            let _guard = registry_clone.registrations.write().unwrap();
            panic!("intentional panic to poison the lock");
        });
        let _ = handle.join(); // Wait for thread to finish (it panicked)

        // Verify the lock is poisoned
        assert!(registry.registrations.write().is_err());

        // register should recover from the poisoned lock
        let reg = make_registration("1", "textDocument/hover");
        registry.register(vec![reg]);

        // Verify the registration was stored despite the poisoned lock
        assert!(registry.has_registration("textDocument/hover"));
    }
}
