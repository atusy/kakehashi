use serde::Deserialize;
use serde_json::Value;
use tower_lsp_server::jsonrpc::Result;

use crate::lsp::lsp_impl::Kakehashi;

#[derive(Deserialize)]
pub struct EffectiveConfigurationParams {}

impl Kakehashi {
    /// Handler for `kakehashi/internal/effectiveConfiguration`.
    ///
    /// Returns `{ settings: <RawWorkspaceSettings> }` where the settings value
    /// matches the `kakehashi.toml` schema.
    pub async fn effective_configuration(
        &self,
        _params: EffectiveConfigurationParams,
    ) -> Result<Value> {
        let raw_settings = self.settings_manager.load_raw_settings();
        let settings_value = serde_json::to_value(raw_settings.as_ref()).map_err(|e| {
            log::error!(
                target: "kakehashi::effective_configuration",
                "Failed to serialize effective configuration: {}",
                e
            );
            tower_lsp_server::jsonrpc::Error::internal_error()
        })?;
        Ok(serde_json::json!({ "settings": settings_value }))
    }
}

#[cfg(test)]
mod tests {
    use crate::config::{LanguageSettings, RawWorkspaceSettings, WILDCARD_KEY, WorkspaceSettings};
    use std::collections::HashMap;
    use tower_lsp_server::LspService;

    use super::*;

    #[tokio::test]
    async fn test_effective_configuration_returns_current_settings() {
        let (service, _socket) = LspService::new(|client| {
            let kakehashi = Kakehashi::new(client);
            // Apply custom settings to verify they're returned
            kakehashi
                .settings_manager
                .apply_settings(WorkspaceSettings {
                    search_paths: vec!["/test/path".to_string()],
                    auto_install: false,
                    ..Default::default()
                });
            kakehashi
        });

        let kakehashi = service.inner();
        let result = kakehashi
            .effective_configuration(EffectiveConfigurationParams {})
            .await
            .unwrap();

        // Verify the result is wrapped in { settings: ... }
        let settings = &result["settings"];
        assert_eq!(
            settings["searchPaths"],
            serde_json::json!(["/test/path"]),
            "searchPaths should reflect applied settings"
        );
        assert_eq!(
            settings["autoInstall"],
            serde_json::json!(false),
            "autoInstall should reflect applied settings"
        );
    }

    #[tokio::test]
    async fn test_effective_configuration_returns_defaults_when_no_settings_applied() {
        let (service, _socket) = LspService::new(Kakehashi::new);

        let kakehashi = service.inner();
        let result = kakehashi
            .effective_configuration(EffectiveConfigurationParams {})
            .await
            .unwrap();

        // Before initialize(), SettingsManager holds WorkspaceSettings::default()
        // which has auto_install = true (matching the zero-config default).
        let settings = &result["settings"];
        assert!(settings.is_object(), "settings should be a JSON object");
        assert_eq!(
            settings["autoInstall"],
            serde_json::json!(true),
            "Pre-initialize default autoInstall should be true"
        );
    }

    #[tokio::test]
    async fn test_effective_configuration_preserves_explicit_matching_override() {
        let (service, _socket) = LspService::new(|client| {
            let kakehashi = Kakehashi::new(client);
            let raw_settings = RawWorkspaceSettings {
                languages: HashMap::from([
                    (
                        WILDCARD_KEY.to_string(),
                        LanguageSettings {
                            bridge: Some(HashMap::from([(
                                "python".to_string(),
                                crate::config::settings::BridgeLanguageConfig {
                                    enabled: Some(true),
                                    ..Default::default()
                                },
                            )])),
                            ..Default::default()
                        },
                    ),
                    (
                        "r".to_string(),
                        LanguageSettings {
                            bridge: Some(HashMap::from([(
                                "python".to_string(),
                                crate::config::settings::BridgeLanguageConfig {
                                    enabled: Some(true),
                                    ..Default::default()
                                },
                            )])),
                            ..Default::default()
                        },
                    ),
                ]),
                ..Default::default()
            };
            let settings = WorkspaceSettings::try_from_settings(&raw_settings, None, |_| None)
                .expect("raw settings should resolve");
            kakehashi
                .settings_manager
                .apply_settings_with_raw(raw_settings, settings);
            kakehashi
        });

        let kakehashi = service.inner();
        let result = kakehashi
            .effective_configuration(EffectiveConfigurationParams {})
            .await
            .unwrap();

        let settings = &result["settings"];
        assert_eq!(
            settings["languages"]["r"]["bridge"]["python"]["enabled"],
            serde_json::json!(true),
            "explicit override should remain visible in effective raw settings"
        );
    }
}
