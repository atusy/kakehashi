//! didChangeConfiguration notification handler for Kakehashi.

use crate::config::unknown_keys::{
    KNOWN_FEATURE_SETTING_KEYS, is_workspace_setting_key_or_typo, sort_and_dedup_unknown_keys,
    unknown_workspace_setting_keys,
};
use serde_json::Value;
use tower_lsp_server::ls_types::{ConfigurationItem, DidChangeConfigurationParams};

use crate::config::{RawWorkspaceSettings, WorkspaceSettings, merge_workspace_settings};

use super::super::{Kakehashi, lock_settings_reload};

fn settings_payload(settings: Value) -> (Value, Vec<String>) {
    let Value::Object(mut object) = settings else {
        return (settings, Vec::new());
    };

    if object.contains_key("kakehashi") {
        let kakehashi = object
            .remove("kakehashi")
            .expect("kakehashi key should exist after object lookup");
        if !kakehashi.is_object() {
            return (kakehashi, Vec::new());
        }

        let mut unknown_keys = object
            .into_iter()
            .filter_map(|(key, value)| is_kakehashi_workspace_entry(&key, &value).then_some(key))
            .collect::<Vec<_>>();
        unknown_keys.extend(unknown_workspace_setting_keys(&kakehashi));
        sort_and_dedup_unknown_keys(&mut unknown_keys);
        return (kakehashi, unknown_keys);
    }

    let settings = kakehashi_targeted_payload(object);
    let mut unknown_keys = unknown_workspace_setting_keys(&settings);
    sort_and_dedup_unknown_keys(&mut unknown_keys);
    (settings, unknown_keys)
}

fn uses_deprecated_unwrapped_didchange_shape(settings: &Value) -> bool {
    let Some(object) = settings.as_object() else {
        return false;
    };
    if object.contains_key("kakehashi") {
        return false;
    }

    kakehashi_targeted_payload(object.clone())
        .as_object()
        .is_some_and(|object| !object.is_empty())
}

fn kakehashi_targeted_payload(object: serde_json::Map<String, Value>) -> serde_json::Value {
    let object = object
        .into_iter()
        .filter_map(|(key, mut value)| {
            if !is_kakehashi_workspace_entry(&key, &value) {
                return None;
            }
            if key == "features"
                && let Some(features) = value.as_object_mut()
            {
                features
                    .retain(|feature, _| KNOWN_FEATURE_SETTING_KEYS.contains(&feature.as_str()));
            }
            Some((key, value))
        })
        .collect();
    Value::Object(object)
}

/// Whether a pushed `settings` says nothing at all — `null`, or an object with
/// no keys. Distinct from a payload that mentions only settings kakehashi does
/// not own, which is a statement about other servers and still says nothing
/// about this one, but which the unknown-key path reports on.
fn carries_no_payload(settings: &Value) -> bool {
    match settings {
        Value::Null => true,
        Value::Object(object) => object.is_empty(),
        _ => false,
    }
}

fn format_rejected_keys(keys: &[String]) -> String {
    keys.iter()
        .map(|key| format!("`{key}`"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn is_kakehashi_workspace_entry(key: &str, value: &Value) -> bool {
    if key == "features" {
        return value.as_object().is_some_and(|features| {
            KNOWN_FEATURE_SETTING_KEYS
                .iter()
                .any(|key| features.contains_key(*key))
        });
    }
    is_workspace_setting_key_or_typo(key)
}

/// How long to wait for the client to answer `workspace/configuration`.
///
/// The await lives in a service future the server joins on, so an answer that
/// never comes must not be waited for indefinitely. Generous, because the cost
/// of giving up early is a session running on file settings alone.
const CONFIGURATION_PULL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// How a client-supplied configuration reached kakehashi.
///
/// Only used to describe it back to the user: the two arrive by different
/// routes, and a message naming the wrong one sends people looking for a
/// notification they never sent.
#[derive(Clone, Copy)]
pub(crate) enum ConfigurationIngress {
    /// The client pushed `workspace/didChangeConfiguration`.
    Push,
    /// kakehashi asked, via `workspace/configuration`.
    Pull,
}

impl ConfigurationIngress {
    fn describe(self) -> &'static str {
        match self {
            Self::Push => "workspace/didChangeConfiguration",
            Self::Pull => "the configuration read from the client",
        }
    }

    /// What to say once the layer is in effect. A pull runs at startup for
    /// every capable client, where "Configuration updated!" would describe an
    /// event the user did not cause.
    fn applied_message(self) -> &'static str {
        match self {
            Self::Push => "Configuration updated!",
            Self::Pull => "Applied the configuration read from the client",
        }
    }
}

impl Kakehashi {
    /// Ask the client for its `kakehashi` section and apply what comes back.
    ///
    /// Editors that send `didChangeConfiguration` with no usable `settings`
    /// (VS Code most prominently) expect the server to pull instead. The
    /// answer is not a delta and not a snapshot of kakehashi's own state — it
    /// is the client's configuration, which is one layer among the rest, so it
    /// is applied exactly as a push of the same section would be. Nothing
    /// supersedes anything: the layer is appended in arrival order like any
    /// other (#734).
    ///
    /// The item carries no `scopeUri`, which asks for the client's global
    /// settings. kakehashi resolves one effective settings snapshot for the
    /// whole process, so naming a scope and then applying the answer
    /// process-wide would silently promote one folder's configuration to
    /// global. Asking unscoped asks for exactly what the single layer means;
    /// scoped pull is the separate half of #952.
    ///
    /// The answer is trusted as authored: a field written as an empty container
    /// clears the layer below, exactly as the same spelling would in a config
    /// file. An editor that materializes `{}` as the default for a setting it
    /// registers would therefore clear it for a user who configured nothing —
    /// worth knowing before registering such a default, and the reason a pull
    /// answer is not merged more leniently than a push.
    ///
    /// Those global settings are still anchored against the workspace root, as
    /// a push is. Relative paths in a client's global configuration therefore
    /// resolve against whichever workspace is open — accepted, because the
    /// alternative leaves them resolving against the launch directory.
    pub(crate) async fn pull_client_configuration(&self) {
        if !self.settings_manager.supports_configuration_pull() {
            return;
        }

        let items = vec![ConfigurationItem {
            scope_uri: None,
            section: Some("kakehashi".to_string()),
        }];
        // Bounded by shutdown: this await lives in the `initialized` service
        // future, which the server joins on, so a client that never answers
        // would keep `serve` from returning after `exit`. The SIGTERM handler
        // rescues that on Unix and nothing does on Windows.
        let answered = match tokio::select! {
            result = self.client.configuration(items) => result,
            () = self.shutdown_token.cancelled() => return,
            () = tokio::time::sleep(CONFIGURATION_PULL_TIMEOUT) => {
                self.notifier()
                    .log_warning(
                        "The client did not answer workspace/configuration in time; \
                         keeping the settings already in effect"
                            .to_string(),
                    )
                    .await;
                return;
            }
        } {
            Ok(values) => values,
            Err(error) => {
                // Leave the settings in effect alone: a failed pull is no
                // answer, not an empty one.
                self.notifier()
                    .log_warning(format!(
                        "Could not read configuration from the client: {error}"
                    ))
                    .await;
                return;
            }
        };

        // A missing element or `null` is the client saying it cannot provide a
        // value for this item — no answer, so nothing changes. Anything else is
        // an answer, including a malformed one: routing it through says so,
        // where swallowing it would leave the user with a client that is
        // answering wrongly and a server that looks like it never asked.
        let section = match answered.into_iter().next() {
            None | Some(Value::Null) => return,
            Some(section) => section,
        };

        self.apply_client_configuration(
            serde_json::json!({ "kakehashi": section }),
            ConfigurationIngress::Pull,
        )
        .await;
    }

    /// Handle workspace/didChangeConfiguration notification.
    ///
    /// A client that carries no usable payload is telling kakehashi that
    /// something changed, not what — vscode-languageclient's canonical push is
    /// `{"settings": null}`. When it can answer a pull, the notification is a
    /// trigger and the answer is the content; when it cannot, there is nothing
    /// to apply and nothing worth warning about.
    ///
    /// The trigger lives here rather than in `apply_client_configuration`, so
    /// that applying a pulled answer cannot pull again.
    pub(crate) async fn did_change_configuration_impl(&self, params: DidChangeConfigurationParams) {
        if carries_no_payload(&params.settings) {
            if self.settings_manager.supports_configuration_pull() {
                self.pull_client_configuration().await;
            }
            return;
        }
        self.apply_client_configuration(params.settings, ConfigurationIngress::Push)
            .await;
    }

    /// Apply a client-supplied configuration layer, however it arrived.
    async fn apply_client_configuration(&self, settings: Value, ingress: ConfigurationIngress) {
        let params = DidChangeConfigurationParams { settings };
        let uses_deprecated_unwrapped_shape =
            uses_deprecated_unwrapped_didchange_shape(&params.settings);
        let (settings_value, unknown_keys) = settings_payload(params.settings);

        // Nudge users off any deprecated key this push carries. Detected from
        // the raw value before parsing collapses the deprecated and canonical
        // spellings; each claim guard shares its once-per-session slot with the
        // initialize path.
        let pushed_deprecated_keys =
            crate::config::deprecation::json_deprecated_keys(&settings_value);
        if pushed_deprecated_keys.root_markers
            && self
                .settings_manager
                .claim_root_markers_deprecation_warning()
        {
            self.notifier()
                .show_warning(crate::config::deprecation::ROOT_MARKERS_DEPRECATION_NOTICE)
                .await;
        }
        if pushed_deprecated_keys.auto_install
            && self
                .settings_manager
                .claim_auto_install_deprecation_warning()
        {
            self.notifier()
                .show_warning(crate::config::deprecation::AUTO_INSTALL_DEPRECATION_NOTICE)
                .await;
        }

        if uses_deprecated_unwrapped_shape
            && self
                .settings_manager
                .claim_unwrapped_didchange_deprecation_warning()
        {
            self.notifier()
                .show_warning(crate::config::deprecation::UNWRAPPED_DIDCHANGE_CONFIGURATION_NOTICE)
                .await;
        }

        if !unknown_keys.is_empty() {
            match ingress {
                ConfigurationIngress::Push => {
                    self.notifier()
                        .log_warning(format!(
                            "{} rejected configuration update containing unknown or mixed-format \
                             key(s): {}",
                            ingress.describe(),
                            format_rejected_keys(&unknown_keys)
                        ))
                        .await;
                    return;
                }
                // kakehashi asked for the whole section, so it also gets the
                // keys the editor keeps there — `trace.server` is
                // vscode-languageclient's near-universal convention, inside
                // the very section being pulled. Rejecting the layer over one
                // of those would make the pull useless for the editors it
                // exists for; the keys are dropped by parsing instead.
                ConfigurationIngress::Pull => {
                    self.notifier()
                        .log_info(format!(
                            "Ignoring {} in the configuration read from the client",
                            format_rejected_keys(&unknown_keys)
                        ))
                        .await;
                }
            }
        }

        if settings_value
            .as_object()
            .is_some_and(serde_json::Map::is_empty)
        {
            return;
        }

        // Parse the incoming settings.
        let mut parsed = match serde_json::from_value::<RawWorkspaceSettings>(settings_value) {
            Ok(settings) => settings,
            Err(err) => {
                self.notifier()
                    .log_warning(format!("Failed to parse client configuration: {}", err))
                    .await;
                return;
            }
        };

        // Snapshot read, derivation, and publication must share the same reload
        // transaction as post-install search-path updates, or either path can
        // overwrite a concurrent update derived from a stale snapshot. The root
        // read below is part of that: `didChangeWorkspaceFolders` stores a new
        // root inside this same transaction, and anchoring against a root read
        // outside it pins this push to a workspace the session may already have
        // left — permanently, since anchoring yields absolute paths that no
        // later reload re-bases.
        let reload = lock_settings_reload().await;

        // A pushed path is workspace-local, matching `initializationOptions`:
        // the client knows the workspace it opened, not the directory the server
        // was launched from. Anchored here, while this push is still a layer of
        // its own — after the merge below it is indistinguishable from a value
        // that arrived from a config file and was already anchored to that
        // file's directory.
        let root_path = self.settings_manager.root_path();
        // A base that cannot be represented leaves the pushed values as written,
        // which is the pre-#732 meaning; `anchor_settings_paths` warns about it.
        // Not fatal, matching this handler's existing posture of keeping the
        // previous settings in effect rather than taking the session down.
        let _ =
            crate::config::paths::anchor_settings_paths(&mut parsed, root_path.as_ref().as_deref());

        // Checked on the pushed layer, not the merge below: an empty container
        // inherited from a file layer would otherwise be re-announced on every
        // push. Latched session-wide, like the deprecated-key notices.
        if let Some(notice) = crate::lsp::settings::emptied_container_notice(Some(&parsed))
            && self
                .settings_manager
                .claim_empty_container_migration_warning()
        {
            self.notifier().show_warning(notice).await;
        }

        // Merge onto current effective settings (not from scratch).
        // The current settings already reflect defaults < user < project < initializationOptions,
        // so merging preserves languages and other fields set during initialize.
        let current_ts = self.settings_manager.load_raw_settings();
        // SAFETY: merge_workspace_settings(Some, Some) always returns Some, so unwrap_or_return is
        // defensive only — the None branch is unreachable under the current implementation.
        let Some(merged_ts) =
            merge_workspace_settings(Some((*current_ts).clone()), Some(parsed.clone()))
        else {
            log::warn!(
                "merge_workspace_settings returned None despite two Some inputs; skipping configuration update"
            );
            return;
        };

        match WorkspaceSettings::try_from_settings(
            &merged_ts,
            self.home_dir.as_deref(),
            crate::config::expand::with_kakehashi_defaults(|var| std::env::var(var).ok()),
        ) {
            Ok(settings) => {
                // Remembered under the same reload transaction that publishes
                // the merged settings, so a concurrent workspace-root change
                // cannot rebuild the layers from a half-updated override.
                let client_override = self.client_settings_override.load_full();
                let merged_override =
                    merge_workspace_settings(client_override.as_deref().cloned(), Some(parsed));
                self.client_settings_override
                    .store(merged_override.map(std::sync::Arc::new));
                let warnings = Self::misconfigured_settings_warnings(&settings);
                self.apply_raw_settings_locked(&reload, merged_ts, settings)
                    .await;
                drop(reload);
                self.warn_on_misconfigured_settings(&warnings).await;
                self.notifier().log_info(ingress.applied_message()).await;
            }
            Err(errs) => {
                drop(reload);
                let event = crate::lsp::SettingsEvent::error(format!(
                    "Invalid configuration: {errs}. \
                     This configuration has been discarded; previous settings remain in effect. \
                     Please correct the invalid settings or remove them from your config.",
                ));
                self.notifier().log_settings_events(&[event]).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::future::Future;
    use std::task::Poll;
    use tower_lsp_server::LspService;

    #[test]
    fn settings_payload_ignores_unrelated_features_section_sibling() {
        let (payload, unknown_keys) = settings_payload(serde_json::json!({
            "kakehashi": {
                "autoInstall": true
            },
            "features": {
                "someOtherClientFeature": true
            }
        }));

        assert_eq!(payload, serde_json::json!({ "autoInstall": true }));
        assert!(unknown_keys.is_empty());
    }

    #[test]
    fn settings_payload_rejects_kakehashi_features_section_sibling() {
        let (_payload, unknown_keys) = settings_payload(serde_json::json!({
            "kakehashi": {
                "autoInstall": true
            },
            "features": {
                "workspace/diagnostic/refresh": {
                    "debounceMs": 20
                }
            }
        }));

        assert_eq!(unknown_keys, ["features"]);
    }

    #[test]
    fn settings_payload_ignores_unrelated_unwrapped_features() {
        let (payload, unknown_keys) = settings_payload(serde_json::json!({
            "autoInstall": true,
            "features": {
                "someOtherClientFeature": true
            }
        }));

        assert_eq!(payload, serde_json::json!({ "autoInstall": true }));
        assert!(unknown_keys.is_empty());
    }

    #[test]
    fn settings_payload_keeps_unwrapped_publish_diagnostics_features() {
        let features = serde_json::json!({
            "textDocument/publishDiagnostics": {
                "debounceMs": 30,
                "maxWaitMs": 300
            }
        });
        let (payload, unknown_keys) = settings_payload(serde_json::json!({
            "features": features
        }));

        assert_eq!(payload, serde_json::json!({ "features": features }));
        assert!(unknown_keys.is_empty());
    }

    #[test]
    fn settings_payload_keeps_unwrapped_log_message_feature() {
        let features = serde_json::json!({
            "window/logMessage": {
                "logLevel": "off"
            }
        });
        let (payload, unknown_keys) = settings_payload(serde_json::json!({
            "features": features
        }));

        assert_eq!(payload, serde_json::json!({ "features": features }));
        assert!(unknown_keys.is_empty());
    }

    #[test]
    fn settings_payload_projects_mixed_unwrapped_features() {
        let (payload, unknown_keys) = settings_payload(serde_json::json!({
            "features": {
                "textDocument/publishDiagnostics": {
                    "debounceMs": 30,
                    "maxWaitMs": 300
                },
                "someOtherClientFeature": true
            }
        }));

        assert_eq!(
            payload,
            serde_json::json!({
                "features": {
                    "textDocument/publishDiagnostics": {
                        "debounceMs": 30,
                        "maxWaitMs": 300
                    }
                }
            })
        );
        assert!(unknown_keys.is_empty());
    }

    #[test]
    fn non_object_kakehashi_wrapper_reaches_parse_error_path() {
        let (payload, unknown_keys) = settings_payload(serde_json::json!({
            "kakehashi": null,
            "autoInstall": false
        }));

        assert_eq!(payload, serde_json::Value::Null);
        assert!(unknown_keys.is_empty());
        assert!(serde_json::from_value::<RawWorkspaceSettings>(payload).is_err());
    }

    #[test]
    fn settings_payload_deduplicates_unknown_keys() {
        let (_payload, unknown_keys) = settings_payload(serde_json::json!({
            "kakehashi": {
                "autoInstal": true
            },
            "autoInstal": false
        }));

        assert_eq!(unknown_keys, ["autoInstal"]);
    }

    /// A path pushed by the client is workspace-local. Without anchoring it
    /// would reach the filesystem relative to the server's working directory,
    /// which for an editor-spawned server is the editor's — the same
    /// launch-directory dependence issue #732 removed from the file layers.
    ///
    /// `searchPaths` stands in for every path field: which fields are anchored
    /// is settled by `anchor_settings_paths`' own tests. Pushing a `languages`
    /// entry here would additionally register a language and reach for its
    /// parser, which is not what this test is about.
    #[tokio::test]
    async fn pushed_relative_paths_anchor_to_the_workspace_root() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        server
            .settings_manager
            .set_root_path(Some(std::path::PathBuf::from("/workspace")));

        server
            .did_change_configuration_impl(DidChangeConfigurationParams {
                settings: serde_json::json!({
                    "kakehashi": { "searchPaths": ["./runtime"] }
                }),
            })
            .await;

        let snapshot = server.settings_manager.load_settings_pair();
        assert_eq!(
            snapshot.settings.search_paths,
            vec!["/workspace/runtime".to_string()]
        );
        assert_eq!(
            snapshot.raw_settings.search_paths,
            Some(vec!["/workspace/runtime".to_string()]),
            "the stored raw settings carry the anchored value, so a later push \
             merging onto them does not re-base it"
        );
    }

    /// A push arriving before `initialize` stored a root has no workspace to
    /// anchor to. The value is left as written rather than guessed at, so it
    /// keeps the pre-#732 meaning instead of being anchored to a wrong base.
    #[tokio::test]
    async fn pushed_relative_paths_survive_an_unknown_workspace_root() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        server.settings_manager.set_root_path(None);

        server
            .did_change_configuration_impl(DidChangeConfigurationParams {
                settings: serde_json::json!({
                    "kakehashi": { "searchPaths": ["./runtime"] }
                }),
            })
            .await;

        assert_eq!(
            server.settings_manager.load_settings().search_paths,
            vec!["./runtime".to_string()]
        );
    }

    #[tokio::test]
    async fn did_change_configuration_merges_into_settings_published_while_waiting() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        server.settings_manager.apply_settings_with_raw(
            RawWorkspaceSettings {
                auto_install: Some(true),
                search_paths: Some(vec!["/initial".to_string()]),
                ..Default::default()
            },
            WorkspaceSettings {
                auto_install: true,
                search_paths: vec!["/initial".to_string()],
                ..Default::default()
            },
        );

        // This payload uses the deprecated top-level `autoInstall` on purpose
        // (the merge behavior under test is about that key). Consume the notice
        // slot first: the warning would otherwise `show_warning` into a socket
        // this test never drains, parking the future for a reason unrelated to
        // the settings-reload lock it is meant to park on.
        server
            .settings_manager
            .claim_auto_install_deprecation_warning();

        let reload_guard = crate::lsp::lsp_impl::lock_settings_reload().await;
        let mut update = Box::pin(server.did_change_configuration_impl(
            DidChangeConfigurationParams {
                settings: serde_json::json!({
                    "kakehashi": { "autoInstall": false }
                }),
            },
        ));
        std::future::poll_fn(|cx| {
            assert!(update.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;

        server.settings_manager.apply_settings_with_raw(
            RawWorkspaceSettings {
                auto_install: Some(true),
                search_paths: Some(vec!["/newer".to_string()]),
                ..Default::default()
            },
            WorkspaceSettings {
                auto_install: true,
                search_paths: vec!["/newer".to_string()],
                ..Default::default()
            },
        );
        drop(reload_guard);
        update.await;

        let snapshot = server.settings_manager.load_settings_pair();
        assert_eq!(snapshot.raw_settings.auto_install, Some(false));
        assert_eq!(
            snapshot.raw_settings.search_paths,
            Some(vec!["/newer".to_string()])
        );
        assert!(!snapshot.settings.auto_install);
        assert_eq!(snapshot.settings.search_paths, vec!["/newer".to_string()]);
    }
}
