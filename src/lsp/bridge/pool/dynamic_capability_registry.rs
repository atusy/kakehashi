use std::collections::HashMap;
use std::sync::RwLock;
use std::sync::atomic::{AtomicU8, Ordering};

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
    registrations: RwLock<HashMap<String, Registration>>,
    /// Live workspace policy copied into every connection. The reader checks
    /// it before a suppressed log can consume bounded window-queue capacity.
    log_message_level: AtomicU8,
}

impl DynamicCapabilityRegistry {
    pub(crate) fn new() -> Self {
        Self {
            registrations: RwLock::new(HashMap::new()),
            log_message_level: AtomicU8::new(
                crate::config::settings::LogMessageLevel::Info.as_u8(),
            ),
        }
    }

    pub(crate) fn register(&self, registrations: Vec<Registration>) {
        let mut guard = self
            .registrations
            .write()
            .recover_poison("DynamicCapabilityRegistry::register");
        for reg in registrations {
            guard.insert(reg.id.clone(), reg);
        }
    }

    pub(crate) fn unregister(&self, unregistrations: Vec<Unregistration>) {
        let mut guard = self
            .registrations
            .write()
            .recover_poison("DynamicCapabilityRegistry::unregister");
        for unreg in unregistrations {
            guard.remove(&unreg.id);
        }
    }

    pub(crate) fn has_registration(&self, method: &str) -> bool {
        self.registrations
            .read()
            .recover_poison("DynamicCapabilityRegistry::has_registration")
            .values()
            .any(|r| r.method == method)
    }

    pub(crate) fn store_log_message_level(&self, level: crate::config::settings::LogMessageLevel) {
        self.log_message_level
            .store(level.as_u8(), Ordering::Release);
    }

    pub(crate) fn allows_log_message(
        &self,
        message_type: tower_lsp_server::ls_types::MessageType,
    ) -> bool {
        crate::config::settings::LogMessageLevel::from_u8(
            self.log_message_level.load(Ordering::Acquire),
        )
        .allows(message_type)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::thread;

    use tower_lsp_server::ls_types::{MessageType, Registration, Unregistration};

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
        assert_eq!(guard.get("1").unwrap().id, "1");
        assert_eq!(guard.get("2").unwrap().id, "2");
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
    fn downstream_log_gate_matches_every_global_level() {
        use crate::config::settings::LogMessageLevel;

        let registry = DynamicCapabilityRegistry::new();
        let debug: MessageType = serde_json::from_str("5").unwrap();
        let message_types = [
            MessageType::ERROR,
            MessageType::WARNING,
            MessageType::INFO,
            MessageType::LOG,
            debug,
        ];
        for level in [
            LogMessageLevel::Error,
            LogMessageLevel::Warning,
            LogMessageLevel::Info,
            LogMessageLevel::Log,
            LogMessageLevel::Off,
        ] {
            registry.store_log_message_level(level);
            for message_type in message_types {
                assert_eq!(
                    registry.allows_log_message(message_type),
                    level.allows(message_type),
                    "downstream atomic gate diverged at {level:?}"
                );
            }
        }
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
