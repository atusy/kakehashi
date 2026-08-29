use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, RwLock, RwLockReadGuard};

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
    revision: AtomicU64,
    workspace_diagnostic_revision: AtomicU64,
    changes: tokio::sync::watch::Sender<u64>,
    diagnostic_registration_settled: tokio::sync::OnceCell<()>,
    static_workspace_diagnostic_provider: AtomicBool,
    workspace_diagnostic_pull: Mutex<WorkspaceDiagnosticPull>,
    workspace_diagnostic_lifecycle: Mutex<WorkspaceDiagnosticLifecycle>,
}

#[derive(Default)]
struct WorkspaceDiagnosticPull {
    active: bool,
    accepted_once: bool,
    registration_refresh_pending: bool,
}

#[derive(Default)]
struct WorkspaceDiagnosticLifecycle {
    contributed: bool,
    reader_exited: bool,
}

pub(crate) struct WorkspaceDiagnosticLifecycleGuard<'a>(
    MutexGuard<'a, WorkspaceDiagnosticLifecycle>,
);

impl WorkspaceDiagnosticLifecycleGuard<'_> {
    pub(crate) fn reader_exited(&self) -> bool {
        self.0.reader_exited
    }

    pub(crate) fn set_contributed(&mut self, contributed: bool) {
        self.0.contributed = contributed;
    }
}

impl DynamicCapabilityRegistry {
    fn registration_participates_in_workspace_diagnostics(registration: &Registration) -> bool {
        registration.method == "textDocument/diagnostic"
            && registration
                .register_options
                .as_ref()
                .and_then(|options| options.get("workspaceDiagnostics"))
                .and_then(serde_json::Value::as_bool)
                == Some(true)
    }

    pub(crate) fn new() -> Self {
        let (changes, _receiver) = tokio::sync::watch::channel(0);
        Self {
            registrations: RwLock::new(HashMap::new()),
            log_message_level: AtomicU8::new(
                crate::config::settings::LogMessageLevel::Info.as_u8(),
            ),
            revision: AtomicU64::new(0),
            workspace_diagnostic_revision: AtomicU64::new(0),
            changes,
            diagnostic_registration_settled: tokio::sync::OnceCell::new(),
            static_workspace_diagnostic_provider: AtomicBool::new(false),
            workspace_diagnostic_pull: Mutex::new(WorkspaceDiagnosticPull::default()),
            workspace_diagnostic_lifecycle: Mutex::new(WorkspaceDiagnosticLifecycle::default()),
        }
    }

    fn notify_change(&self, revision: u64) {
        self.changes.send_replace(revision);
    }

    pub(crate) fn register(&self, registrations: Vec<Registration>) -> bool {
        let mut guard = self
            .registrations
            .write()
            .recover_poison("DynamicCapabilityRegistry::register");
        let mut workspace_diagnostic_changed = false;
        for reg in registrations {
            let is_workspace_diagnostic =
                Self::registration_participates_in_workspace_diagnostics(&reg);
            let replaced = guard.insert(reg.id.clone(), reg);
            workspace_diagnostic_changed |= is_workspace_diagnostic
                || replaced
                    .as_ref()
                    .is_some_and(Self::registration_participates_in_workspace_diagnostics);
        }
        let revision = self.revision.fetch_add(1, Ordering::AcqRel) + 1;
        if workspace_diagnostic_changed {
            self.workspace_diagnostic_revision
                .fetch_add(1, Ordering::AcqRel);
        }
        drop(guard);
        self.notify_change(revision);
        workspace_diagnostic_changed
    }

    pub(crate) fn unregister(&self, unregistrations: Vec<Unregistration>) {
        let mut guard = self
            .registrations
            .write()
            .recover_poison("DynamicCapabilityRegistry::unregister");
        let mut workspace_diagnostic_changed = false;
        for unreg in unregistrations {
            let removed = guard.remove(&unreg.id);
            workspace_diagnostic_changed |= removed
                .as_ref()
                .is_some_and(Self::registration_participates_in_workspace_diagnostics);
        }
        let revision = self.revision.fetch_add(1, Ordering::AcqRel) + 1;
        if workspace_diagnostic_changed {
            self.workspace_diagnostic_revision
                .fetch_add(1, Ordering::AcqRel);
        }
        drop(guard);
        self.notify_change(revision);
    }

    pub(crate) fn subscribe_changes(&self) -> tokio::sync::watch::Receiver<u64> {
        self.changes.subscribe()
    }

    pub(crate) fn diagnostic_registration_settle(&self) -> &tokio::sync::OnceCell<()> {
        &self.diagnostic_registration_settled
    }

    pub(crate) fn set_static_workspace_diagnostic_provider(&self, supported: bool) {
        self.static_workspace_diagnostic_provider
            .store(supported, Ordering::Release);
    }

    pub(crate) fn has_workspace_diagnostic_provider(&self) -> bool {
        self.static_workspace_diagnostic_provider
            .load(Ordering::Acquire)
            || self.registration_options_flag("textDocument/diagnostic", "workspaceDiagnostics")
    }

    #[cfg(test)]
    pub(crate) fn try_mark_workspace_diagnostic_contributed(&self) -> Option<bool> {
        let mut lifecycle = self.workspace_diagnostic_lifecycle_lock();
        if lifecycle.reader_exited() {
            return None;
        }
        lifecycle.set_contributed(true);
        drop(lifecycle);
        Some(self.mark_workspace_diagnostic_pull_completed())
    }

    pub(crate) fn mark_workspace_diagnostic_reader_exited(&self) -> bool {
        let mut lifecycle = self
            .workspace_diagnostic_lifecycle
            .lock()
            .recover_poison("DynamicCapabilityRegistry::mark_workspace_diagnostic_reader_exited");
        lifecycle.reader_exited = true;
        lifecycle.contributed
    }

    pub(crate) fn has_workspace_diagnostic_reader_exited(&self) -> bool {
        self.workspace_diagnostic_lifecycle_lock().reader_exited()
    }

    pub(crate) fn workspace_diagnostic_lifecycle_lock(
        &self,
    ) -> WorkspaceDiagnosticLifecycleGuard<'_> {
        WorkspaceDiagnosticLifecycleGuard(
            self.workspace_diagnostic_lifecycle
                .lock()
                .recover_poison("DynamicCapabilityRegistry::workspace_diagnostic_lifecycle_lock"),
        )
    }

    pub(crate) fn mark_workspace_diagnostic_pull_completed(&self) -> bool {
        let mut pull = self
            .workspace_diagnostic_pull
            .lock()
            .recover_poison("DynamicCapabilityRegistry::mark_workspace_diagnostic_pull_completed");
        pull.active = false;
        pull.accepted_once = true;
        std::mem::take(&mut pull.registration_refresh_pending)
    }

    pub(crate) fn mark_workspace_diagnostic_pull_active(&self) {
        self.workspace_diagnostic_pull
            .lock()
            .recover_poison("DynamicCapabilityRegistry::mark_workspace_diagnostic_pull_active")
            .active = true;
    }

    pub(crate) fn workspace_diagnostic_pull_active(&self) -> bool {
        self.workspace_diagnostic_pull
            .lock()
            .recover_poison("DynamicCapabilityRegistry::workspace_diagnostic_pull_active")
            .active
    }

    pub(crate) fn mark_workspace_diagnostic_pull_aborted(&self, started_by_pull: bool) -> bool {
        let mut pull = self
            .workspace_diagnostic_pull
            .lock()
            .recover_poison("DynamicCapabilityRegistry::mark_workspace_diagnostic_pull_aborted");
        if pull.active {
            pull.active = false;
            if pull.accepted_once || !started_by_pull {
                return std::mem::take(&mut pull.registration_refresh_pending);
            }
            pull.registration_refresh_pending = false;
        }
        false
    }

    pub(crate) fn take_workspace_diagnostic_registration_refresh(&self) -> bool {
        std::mem::take(
            &mut self
                .workspace_diagnostic_pull
                .lock()
                .recover_poison(
                    "DynamicCapabilityRegistry::take_workspace_diagnostic_registration_refresh",
                )
                .registration_refresh_pending,
        )
    }

    pub(crate) fn has_workspace_diagnostic_contributed(&self) -> bool {
        self.workspace_diagnostic_lifecycle
            .lock()
            .recover_poison("DynamicCapabilityRegistry::has_workspace_diagnostic_contributed")
            .contributed
    }

    pub(crate) fn request_or_defer_workspace_diagnostic_registration_refresh(&self) -> bool {
        let mut pull = self.workspace_diagnostic_pull.lock().recover_poison(
            "DynamicCapabilityRegistry::request_or_defer_workspace_diagnostic_registration_refresh",
        );
        if pull.active {
            pull.registration_refresh_pending = true;
            false
        } else {
            true
        }
    }

    pub(crate) fn has_registration(&self, method: &str) -> bool {
        self.registrations
            .read()
            .recover_poison("DynamicCapabilityRegistry::has_registration")
            .values()
            .any(|r| r.method == method)
    }

    /// Run `f` while a matching registration remains protected from
    /// unregistration. The read lease linearizes request admission before the
    /// unregister writer can remove the capability and acknowledge it.
    pub(crate) fn with_registration<R>(&self, method: &str, f: impl FnOnce() -> R) -> Option<R> {
        let guard = self
            .registrations
            .read()
            .recover_poison("DynamicCapabilityRegistry::with_registration");
        guard
            .values()
            .any(|registration| registration.method == method)
            .then(f)
    }

    pub(crate) fn registrations_for_method(&self, method: &str) -> Vec<Registration> {
        self.registrations
            .read()
            .recover_poison("DynamicCapabilityRegistry::registrations_for_method")
            .values()
            .filter(|registration| registration.method == method)
            .cloned()
            .collect()
    }

    /// Snapshot matching registrations and the mutation revision while the
    /// registration map is read-locked. Writers advance the revision before
    /// releasing their write lock, so the pair cannot hide an unregister /
    /// re-register ABA transition.
    pub(crate) fn registrations_for_method_with_revision(
        &self,
        method: &str,
    ) -> (Vec<Registration>, u64) {
        let guard = self
            .registrations
            .read()
            .recover_poison("DynamicCapabilityRegistry::registrations_for_method_with_revision");
        let registrations = guard
            .values()
            .filter(|registration| registration.method == method)
            .cloned()
            .collect();
        let revision = self.workspace_diagnostic_revision.load(Ordering::Acquire);
        (registrations, revision)
    }

    pub(crate) fn workspace_diagnostic_revision(&self) -> u64 {
        self.workspace_diagnostic_revision.load(Ordering::Acquire)
    }

    pub(crate) fn registrations_read(&self) -> RwLockReadGuard<'_, HashMap<String, Registration>> {
        self.registrations
            .read()
            .recover_poison("DynamicCapabilityRegistry::registrations_read")
    }

    /// Run `f` with every still-live registration from `ids`, while all of
    /// them remain protected from unregistration or replacement.
    pub(crate) fn with_registrations_by_id<R>(
        &self,
        ids: &[String],
        method: &str,
        f: impl FnOnce(Vec<&Registration>) -> R,
    ) -> R {
        let guard = self
            .registrations
            .read()
            .recover_poison("DynamicCapabilityRegistry::with_registrations_by_id");
        f(ids
            .iter()
            .filter_map(|id| guard.get(id))
            .filter(|registration| registration.method == method)
            .collect())
    }

    /// Whether any dynamic registration of `method` sets the boolean
    /// `registerOptions.<flag>`.
    ///
    /// Sub-capabilities that live INSIDE another method's options have no
    /// method of their own to register — for example, `completionItem/resolve`
    /// is `textDocument/completion`'s `resolveProvider`, not a registrable
    /// method — so [`Self::has_registration`] can never see them.
    pub(crate) fn registration_options_flag(&self, method: &str, flag: &str) -> bool {
        self.registrations
            .read()
            .recover_poison("DynamicCapabilityRegistry::registration_options_flag")
            .values()
            .filter(|r| r.method == method)
            .any(|r| {
                r.register_options
                    .as_ref()
                    .and_then(|options| options.get(flag))
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false)
            })
    }

    /// Snapshot whether a method is registered and whether any matching
    /// registration enables `flag`, then run `f` while both facts remain
    /// protected from registration changes.
    pub(crate) fn with_registration_snapshot<R>(
        &self,
        method: &str,
        flag: &str,
        f: impl FnOnce(bool, bool) -> R,
    ) -> R {
        let guard = self
            .registrations
            .read()
            .recover_poison("DynamicCapabilityRegistry::with_registration_snapshot");
        let mut registered = false;
        let mut flag_enabled = false;
        for registration in guard
            .values()
            .filter(|registration| registration.method == method)
        {
            registered = true;
            flag_enabled |= registration
                .register_options
                .as_ref()
                .and_then(|options| options.get(flag))
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
        }
        f(registered, flag_enabled)
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
    fn registration_read_lease_orders_unregistration_after_admission() {
        let registry = DynamicCapabilityRegistry::new();
        registry.register(vec![make_registration("1", "textDocument/completion")]);
        let write_is_excluded = registry.with_registration("textDocument/completion", || {
            registry.registrations.try_write().is_err()
        });
        assert_eq!(
            write_is_excluded,
            Some(true),
            "admission callback must execute while the unregister write lock is excluded"
        );
        registry.unregister(vec![make_unregistration("1", "textDocument/completion")]);
        assert!(!registry.has_registration("textDocument/completion"));
    }

    #[test]
    fn exact_registration_read_lease_exposes_options_and_orders_unregistration() {
        let registry = DynamicCapabilityRegistry::new();
        registry.register(vec![Registration {
            id: "diagnostics".into(),
            method: "textDocument/diagnostic".into(),
            register_options: Some(serde_json::json!({
                "identifier": "rust",
                "workspaceDiagnostics": true
            })),
        }]);

        let snapshot = registry.with_registrations_by_id(
            &["diagnostics".into()],
            "textDocument/diagnostic",
            |registrations| {
                (
                    registrations[0].register_options.clone(),
                    registry.registrations.try_write().is_err(),
                )
            },
        );

        assert_eq!(
            snapshot,
            (
                Some(serde_json::json!({
                    "identifier": "rust",
                    "workspaceDiagnostics": true
                })),
                true
            )
        );
    }

    #[test]
    fn registrations_for_method_returns_each_provider_registration() {
        let registry = DynamicCapabilityRegistry::new();
        registry.register(vec![
            Registration {
                id: "rust".into(),
                method: "textDocument/diagnostic".into(),
                register_options: Some(serde_json::json!({ "identifier": "rust" })),
            },
            Registration {
                id: "lua".into(),
                method: "textDocument/diagnostic".into(),
                register_options: Some(serde_json::json!({ "identifier": "lua" })),
            },
            make_registration("hover", "textDocument/hover"),
        ]);

        let mut ids: Vec<_> = registry
            .registrations_for_method("textDocument/diagnostic")
            .into_iter()
            .map(|registration| registration.id)
            .collect();
        ids.sort();
        assert_eq!(ids, ["lua", "rust"]);
    }

    #[test]
    fn workspace_diagnostic_revision_ignores_unrelated_capabilities() {
        let registry = DynamicCapabilityRegistry::new();
        let (_, initial_revision) =
            registry.registrations_for_method_with_revision("textDocument/diagnostic");

        registry.register(vec![make_registration("hover", "textDocument/hover")]);
        let (_, unrelated_revision) =
            registry.registrations_for_method_with_revision("textDocument/diagnostic");
        assert_eq!(unrelated_revision, initial_revision);

        registry.register(vec![Registration {
            id: "document-diagnostics".into(),
            method: "textDocument/diagnostic".into(),
            register_options: Some(serde_json::json!({ "workspaceDiagnostics": false })),
        }]);
        let (_, document_only_revision) =
            registry.registrations_for_method_with_revision("textDocument/diagnostic");
        assert_eq!(document_only_revision, unrelated_revision);

        registry.register(vec![Registration {
            id: "workspace-diagnostics".into(),
            method: "textDocument/diagnostic".into(),
            register_options: Some(serde_json::json!({ "workspaceDiagnostics": true })),
        }]);
        let (_, diagnostic_revision) =
            registry.registrations_for_method_with_revision("textDocument/diagnostic");
        assert!(diagnostic_revision > document_only_revision);
    }

    #[test]
    fn replacing_workspace_diagnostics_with_document_only_is_a_workspace_change() {
        let registry = DynamicCapabilityRegistry::new();
        assert!(registry.register(vec![Registration {
            id: "diagnostics".into(),
            method: "textDocument/diagnostic".into(),
            register_options: Some(serde_json::json!({ "workspaceDiagnostics": true })),
        }]));

        assert!(registry.register(vec![Registration {
            id: "diagnostics".into(),
            method: "textDocument/diagnostic".into(),
            register_options: Some(serde_json::json!({ "workspaceDiagnostics": false })),
        }]));
        assert!(!registry.has_workspace_diagnostic_provider());
    }

    #[test]
    fn registration_snapshot_keeps_resolve_state_stable_during_admission() {
        let registry = DynamicCapabilityRegistry::new();
        registry.register(vec![Registration {
            id: "1".into(),
            method: "workspace/symbol".into(),
            register_options: Some(serde_json::json!({ "resolveProvider": true })),
        }]);
        let snapshot = registry.with_registration_snapshot(
            "workspace/symbol",
            "resolveProvider",
            |registered, resolves| {
                (
                    registered,
                    resolves,
                    registry.registrations.try_write().is_err(),
                )
            },
        );
        assert_eq!(snapshot, (true, true, true));
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
    fn diagnostic_registration_refresh_waits_for_an_accepted_aggregate() {
        let registry = DynamicCapabilityRegistry::new();
        registry.mark_workspace_diagnostic_pull_active();

        assert!(
            !registry.request_or_defer_workspace_diagnostic_registration_refresh(),
            "a cold registration must not immediately trigger another pull"
        );
        assert!(
            registry.try_mark_workspace_diagnostic_contributed() == Some(true),
            "the first accepted aggregate must release the deferred retry"
        );
        assert!(
            registry.request_or_defer_workspace_diagnostic_registration_refresh(),
            "later registrations invalidate an already accepted aggregate immediately"
        );
        assert!(
            registry.try_mark_workspace_diagnostic_contributed() == Some(false),
            "an immediate retry must not leave duplicate deferred work"
        );
    }

    #[test]
    fn reader_exit_and_contribution_use_an_atomic_handoff() {
        let exited_first = DynamicCapabilityRegistry::new();
        assert!(!exited_first.mark_workspace_diagnostic_reader_exited());
        assert_eq!(
            exited_first.try_mark_workspace_diagnostic_contributed(),
            None,
            "an aggregate must not be accepted after reader exit"
        );

        let contributed_first = DynamicCapabilityRegistry::new();
        assert_eq!(
            contributed_first.try_mark_workspace_diagnostic_contributed(),
            Some(false)
        );
        assert!(
            contributed_first.mark_workspace_diagnostic_reader_exited(),
            "reader exit must observe an accepted contribution and request refresh"
        );
    }

    #[test]
    fn cold_pull_stays_incomplete_until_success_is_recorded() {
        let registry = DynamicCapabilityRegistry::new();
        registry.mark_workspace_diagnostic_pull_active();

        assert!(!registry.request_or_defer_workspace_diagnostic_registration_refresh());
        assert!(!registry.request_or_defer_workspace_diagnostic_registration_refresh());
        assert!(!registry.has_workspace_diagnostic_contributed());
        assert!(registry.mark_workspace_diagnostic_pull_completed());
        assert!(registry.request_or_defer_workspace_diagnostic_registration_refresh());
    }

    #[test]
    fn empty_pull_releases_registration_refresh_without_arming_exit_refresh() {
        let registry = DynamicCapabilityRegistry::new();
        registry.mark_workspace_diagnostic_pull_active();

        assert!(!registry.request_or_defer_workspace_diagnostic_registration_refresh());
        assert!(registry.mark_workspace_diagnostic_pull_completed());
        assert!(!registry.has_workspace_diagnostic_contributed());
        assert!(registry.request_or_defer_workspace_diagnostic_registration_refresh());
    }

    #[test]
    fn warm_registration_refreshes_without_waiting_for_a_pull() {
        let registry = DynamicCapabilityRegistry::new();

        assert!(registry.request_or_defer_workspace_diagnostic_registration_refresh());
        assert!(!registry.take_workspace_diagnostic_registration_refresh());
    }

    #[test]
    fn aborted_cold_pull_does_not_release_its_deferred_refresh() {
        let registry = DynamicCapabilityRegistry::new();
        registry.mark_workspace_diagnostic_pull_active();
        assert!(!registry.request_or_defer_workspace_diagnostic_registration_refresh());

        assert!(!registry.mark_workspace_diagnostic_pull_aborted(true));

        assert!(!registry.take_workspace_diagnostic_registration_refresh());
        assert!(
            registry.request_or_defer_workspace_diagnostic_registration_refresh(),
            "the registration deferred before abort is resolved by the abort"
        );
    }

    #[test]
    fn aborted_cold_pull_does_not_suppress_a_later_registration() {
        let registry = DynamicCapabilityRegistry::new();
        registry.mark_workspace_diagnostic_pull_active();
        registry.mark_workspace_diagnostic_pull_aborted(true);

        assert!(
            registry.request_or_defer_workspace_diagnostic_registration_refresh(),
            "a registration not observed during the pull must refresh normally"
        );
    }

    #[test]
    fn aborted_first_pull_releases_refresh_for_a_preexisting_connection() {
        let registry = DynamicCapabilityRegistry::new();
        registry.mark_workspace_diagnostic_pull_active();
        assert!(!registry.request_or_defer_workspace_diagnostic_registration_refresh());

        assert!(registry.mark_workspace_diagnostic_pull_aborted(false));
        assert!(!registry.take_workspace_diagnostic_registration_refresh());
    }

    #[test]
    fn aborted_warm_pull_releases_its_deferred_refresh() {
        let registry = DynamicCapabilityRegistry::new();
        assert_eq!(
            registry.try_mark_workspace_diagnostic_contributed(),
            Some(false)
        );
        registry.mark_workspace_diagnostic_pull_active();
        assert!(!registry.request_or_defer_workspace_diagnostic_registration_refresh());

        assert!(registry.mark_workspace_diagnostic_pull_aborted(true));
        assert!(!registry.take_workspace_diagnostic_registration_refresh());
    }

    #[test]
    fn aborted_pull_after_an_accepted_empty_result_releases_its_deferred_refresh() {
        let registry = DynamicCapabilityRegistry::new();
        registry.mark_workspace_diagnostic_pull_active();
        assert!(!registry.mark_workspace_diagnostic_pull_completed());
        assert!(
            !registry.has_workspace_diagnostic_contributed(),
            "an accepted empty result must remain distinct from visible contribution"
        );

        registry.mark_workspace_diagnostic_pull_active();
        assert!(!registry.request_or_defer_workspace_diagnostic_registration_refresh());

        assert!(registry.mark_workspace_diagnostic_pull_aborted(true));
        assert!(!registry.take_workspace_diagnostic_registration_refresh());
    }

    #[test]
    fn completed_pull_is_not_reclassified_as_aborted_by_guard_cleanup() {
        let registry = DynamicCapabilityRegistry::new();
        registry.mark_workspace_diagnostic_pull_active();
        assert!(!registry.mark_workspace_diagnostic_pull_completed());

        registry.mark_workspace_diagnostic_pull_aborted(true);

        assert!(registry.request_or_defer_workspace_diagnostic_registration_refresh());
    }

    #[test]
    fn registration_read_lease_blocks_unregistration() {
        let registry = std::sync::Arc::new(DynamicCapabilityRegistry::new());
        registry.register(vec![make_registration("diag-1", "textDocument/diagnostic")]);
        let lease = registry.registrations_read();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let writer = std::sync::Arc::clone(&registry);
        let task = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            writer.unregister(vec![make_unregistration(
                "diag-1",
                "textDocument/diagnostic",
            )]);
            done_tx.send(()).unwrap();
        });

        started_rx.recv().unwrap();
        assert!(done_rx.try_recv().is_err());
        drop(lease);
        done_rx.recv().unwrap();
        task.join().unwrap();
        assert!(!registry.has_registration("textDocument/diagnostic"));
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
