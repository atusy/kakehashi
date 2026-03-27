use crate::config::{
    RawWorkspaceSettings, WorkspaceSettings, defaults::default_settings, load_user_config,
    merge_workspace_settings,
};
use serde_json::Value;
use std::fs;
use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettingsEventKind {
    Info,
    Warning,
    /// Hard error surfaced via `window/showMessage` so the user cannot miss it.
    Error,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SettingsEvent {
    pub kind: SettingsEventKind,
    pub message: String,
}

impl SettingsEvent {
    pub fn info(message: impl Into<String>) -> Self {
        Self {
            kind: SettingsEventKind::Info,
            message: message.into(),
        }
    }

    pub fn warning(message: impl Into<String>) -> Self {
        Self {
            kind: SettingsEventKind::Warning,
            message: message.into(),
        }
    }

    pub fn error(message: impl Into<String>) -> Self {
        Self {
            kind: SettingsEventKind::Error,
            message: message.into(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettingsSource {
    InitializationOptions,
}

impl SettingsSource {
    fn description(self) -> &'static str {
        match self {
            SettingsSource::InitializationOptions => "initialization options",
        }
    }
}

#[derive(Default, Debug)]
pub struct SettingsLoadOutcome {
    pub settings: Option<WorkspaceSettings>,
    pub raw_settings: Option<RawWorkspaceSettings>,
    pub events: Vec<SettingsEvent>,
}

pub fn load_settings(
    root_path: Option<&Path>,
    override_settings: Option<(SettingsSource, Value)>,
    home: Option<&str>,
    env_fn: impl Fn(&str) -> Option<String>,
) -> SettingsLoadOutcome {
    let env_fn = crate::config::expand::with_kakehashi_defaults(env_fn);
    let mut events = Vec::new();

    // Layer 1: Programmed defaults (ADR-0010: lowest precedence)
    let defaults = Some(default_settings());

    // Layers 2+3: config files (either explicit --config-file or default locations)
    let config_layers: Vec<Option<RawWorkspaceSettings>> =
        if let Some(files) = crate::config::expand::config_file_override() {
            events.push(SettingsEvent::info(format!(
                "Using {} explicit config file(s); default config locations skipped",
                files.len()
            )));
            files
                .iter()
                .map(|p| load_toml_file(p, &mut events))
                .collect()
        } else {
            vec![
                // Layer 2: User config from XDG_CONFIG_HOME (~/.config/kakehashi/kakehashi.toml)
                load_user_config_with_events(&mut events),
                // Layer 3: Project config from root_path/kakehashi.toml
                load_toml_settings(root_path, &mut events),
            ]
        };

    // Layer 4: Override settings from initialization options or client configuration
    let override_settings = override_settings
        .and_then(|(source, value)| parse_override_settings(source, value, &mut events));

    // Merge all layers: defaults < config_layers < override (later layers override earlier)
    let mut layers = vec![defaults];
    layers.extend(config_layers);
    layers.push(override_settings);
    let merged = layers
        .into_iter()
        .reduce(merge_workspace_settings)
        .flatten();
    let raw_settings = merged.clone();
    let settings =
        merged.and_then(
            |m| match WorkspaceSettings::try_from_settings(&m, home, &env_fn) {
                Ok(ws) => Some(ws),
                Err(errs) => {
                    events.push(SettingsEvent::error(format!(
                        "Path expansion failed: {errs}. \
                     This configuration has been discarded; previous settings remain in effect. \
                     Please correct the affected paths and environment variables or remove them from your config.",
                    )));
                    None
                }
            },
        );

    SettingsLoadOutcome {
        settings,
        raw_settings,
        events,
    }
}

/// Load user config and add appropriate events to the events vector.
fn load_user_config_with_events(events: &mut Vec<SettingsEvent>) -> Option<RawWorkspaceSettings> {
    match load_user_config() {
        Ok(Some(settings)) => {
            events.push(SettingsEvent::info(
                "Loaded user config from XDG_CONFIG_HOME",
            ));
            Some(settings)
        }
        Ok(None) => {
            // No user config file exists - this is fine (zero-config experience)
            None
        }
        Err(err) => {
            events.push(SettingsEvent::warning(format!(
                "Failed to load user config: {}",
                err
            )));
            None
        }
    }
}

/// Load a TOML config file from an explicit path (used with `--config-file`).
///
/// Unlike `load_toml_settings`, non-existent files are treated as errors
/// because explicit paths represent user intent (ADR-0010).
fn load_toml_file(path: &Path, events: &mut Vec<SettingsEvent>) -> Option<RawWorkspaceSettings> {
    if !path.exists() {
        events.push(SettingsEvent::error(format!(
            "Config file not found: {}",
            path.display()
        )));
        return None;
    }

    events.push(SettingsEvent::info(format!(
        "Loading config file: {}",
        path.display()
    )));

    match fs::read_to_string(path) {
        Ok(contents) => match toml::from_str::<RawWorkspaceSettings>(&contents) {
            Ok(settings) => {
                events.push(SettingsEvent::info(format!(
                    "Successfully loaded {}",
                    path.display()
                )));
                Some(settings)
            }
            Err(err) => {
                events.push(SettingsEvent::error(format!(
                    "Failed to parse {}: {}",
                    path.display(),
                    err
                )));
                None
            }
        },
        Err(err) => {
            events.push(SettingsEvent::error(format!(
                "Failed to read {}: {}",
                path.display(),
                err
            )));
            None
        }
    }
}

fn load_toml_settings(
    root_path: Option<&Path>,
    events: &mut Vec<SettingsEvent>,
) -> Option<RawWorkspaceSettings> {
    let root = root_path?;
    let config_path = root.join("kakehashi.toml");
    if !config_path.exists() {
        return None;
    }

    events.push(SettingsEvent::info(format!(
        "Found config file: {}",
        config_path.display()
    )));

    match fs::read_to_string(&config_path) {
        Ok(contents) => match toml::from_str::<RawWorkspaceSettings>(&contents) {
            Ok(settings) => {
                events.push(SettingsEvent::info("Successfully loaded kakehashi.toml"));
                Some(settings)
            }
            Err(err) => {
                events.push(SettingsEvent::warning(format!(
                    "Failed to parse kakehashi.toml: {}",
                    err
                )));
                None
            }
        },
        Err(err) => {
            events.push(SettingsEvent::warning(format!(
                "Failed to read kakehashi.toml: {}",
                err
            )));
            None
        }
    }
}

fn parse_override_settings(
    source: SettingsSource,
    value: Value,
    events: &mut Vec<SettingsEvent>,
) -> Option<RawWorkspaceSettings> {
    match serde_json::from_value::<RawWorkspaceSettings>(value) {
        Ok(settings) => {
            events.push(SettingsEvent::info(format!(
                "Parsed {} as RawWorkspaceSettings",
                source.description()
            )));
            Some(settings)
        }
        Err(err) => {
            events.push(SettingsEvent::warning(format!(
                "Failed to parse {}: {}",
                source.description(),
                err
            )));
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use tempfile::TempDir;

    /// PBI-155 Subtask 1: Verify load_settings() uses 4-layer merge
    ///
    /// This test verifies that load_settings():
    /// 1. Loads user config from XDG_CONFIG_HOME
    /// 2. Merges 4 layers via reduce(merge_workspace_settings): defaults < user < project < InitializationOptions
    #[test]
    #[serial(xdg_env)]
    fn test_load_settings_merges_user_config_with_project_and_override() {
        use std::env;
        use std::fs;

        // Save original XDG_CONFIG_HOME
        let original_xdg = env::var("XDG_CONFIG_HOME").ok();

        // Create temp directories for user config and project
        let user_config_dir = TempDir::new().expect("failed to create user config temp dir");
        let project_dir = TempDir::new().expect("failed to create project temp dir");

        // Set up user config with unique searchPath
        let kakehashi_config_dir = user_config_dir.path().join("kakehashi");
        fs::create_dir_all(&kakehashi_config_dir).expect("failed to create config dir");
        let user_config_content = r#"
            searchPaths = ["/user/search/path"]
            autoInstall = false
        "#;
        fs::write(
            kakehashi_config_dir.join("kakehashi.toml"),
            user_config_content,
        )
        .expect("failed to write user config");

        // Set up project config with different setting
        let project_config_content = r#"
            autoInstall = true
        "#;
        fs::write(
            project_dir.path().join("kakehashi.toml"),
            project_config_content,
        )
        .expect("failed to write project config");

        // Point XDG_CONFIG_HOME to our temp directory
        // SAFETY: #[serial(xdg_env)] prevents concurrent modification of XDG_CONFIG_HOME
        unsafe {
            env::set_var("XDG_CONFIG_HOME", user_config_dir.path());
        }

        // Load settings with project path
        let home = dirs::home_dir().map(|p| p.to_string_lossy().into_owned());
        let outcome = load_settings(Some(project_dir.path()), None, home.as_deref(), |var| {
            std::env::var(var).ok()
        });

        // Restore original XDG_CONFIG_HOME
        // SAFETY: #[serial(xdg_env)] prevents concurrent modification of XDG_CONFIG_HOME
        unsafe {
            match original_xdg {
                Some(val) => env::set_var("XDG_CONFIG_HOME", val),
                None => env::remove_var("XDG_CONFIG_HOME"),
            }
        }

        // Verify: settings should exist
        assert!(
            outcome.settings.is_some(),
            "load_settings should return settings when configs exist"
        );
        let settings = outcome.settings.unwrap();

        // Verify: user config's searchPath should be present (inherited from user layer)
        assert!(
            settings
                .search_paths
                .iter()
                .any(|p| p == "/user/search/path"),
            "User config searchPath should be inherited. Got: {:?}",
            settings.search_paths
        );

        // Verify: project config's autoInstall should override user config
        assert!(
            settings.auto_install,
            "Project config autoInstall=true should override user config autoInstall=false"
        );
    }

    /// PBI-155: Verify override_settings (InitializationOptions) has highest precedence
    #[test]
    #[serial(xdg_env)]
    fn test_load_settings_override_has_highest_precedence() {
        use std::env;
        use std::fs;

        // Save original XDG_CONFIG_HOME
        let original_xdg = env::var("XDG_CONFIG_HOME").ok();

        // Create temp directories
        let user_config_dir = TempDir::new().expect("failed to create user config temp dir");
        let project_dir = TempDir::new().expect("failed to create project temp dir");

        // Set up user config
        let kakehashi_config_dir = user_config_dir.path().join("kakehashi");
        fs::create_dir_all(&kakehashi_config_dir).expect("failed to create config dir");
        let user_config_content = r#"
            autoInstall = false
        "#;
        fs::write(
            kakehashi_config_dir.join("kakehashi.toml"),
            user_config_content,
        )
        .expect("failed to write user config");

        // Set up project config
        let project_config_content = r#"
            autoInstall = false
        "#;
        fs::write(
            project_dir.path().join("kakehashi.toml"),
            project_config_content,
        )
        .expect("failed to write project config");

        // Point XDG_CONFIG_HOME to our temp directory
        // SAFETY: #[serial(xdg_env)] prevents concurrent modification of XDG_CONFIG_HOME
        unsafe {
            env::set_var("XDG_CONFIG_HOME", user_config_dir.path());
        }

        // Create override settings via InitializationOptions with autoInstall = true
        let override_json = serde_json::json!({
            "autoInstall": true
        });

        // Load settings with override
        let home = dirs::home_dir().map(|p| p.to_string_lossy().into_owned());
        let outcome = load_settings(
            Some(project_dir.path()),
            Some((SettingsSource::InitializationOptions, override_json)),
            home.as_deref(),
            |var| std::env::var(var).ok(),
        );

        // Restore original XDG_CONFIG_HOME
        // SAFETY: #[serial(xdg_env)] prevents concurrent modification of XDG_CONFIG_HOME
        unsafe {
            match original_xdg {
                Some(val) => env::set_var("XDG_CONFIG_HOME", val),
                None => env::remove_var("XDG_CONFIG_HOME"),
            }
        }

        // Verify: settings should exist
        assert!(
            outcome.settings.is_some(),
            "load_settings should return settings"
        );
        let settings = outcome.settings.unwrap();

        // Verify: override's autoInstall=true should win over user and project's autoInstall=false
        assert!(
            settings.auto_install,
            "Override (InitializationOptions) autoInstall=true should have highest precedence"
        );
    }

    /// PBI-155: Verify user config loading logs appropriate events
    #[test]
    #[serial(xdg_env)]
    fn test_load_settings_logs_user_config_events() {
        use std::env;
        use std::fs;

        // Save original XDG_CONFIG_HOME
        let original_xdg = env::var("XDG_CONFIG_HOME").ok();

        // Create temp directory for user config
        let user_config_dir = TempDir::new().expect("failed to create user config temp dir");

        // Set up user config
        let kakehashi_config_dir = user_config_dir.path().join("kakehashi");
        fs::create_dir_all(&kakehashi_config_dir).expect("failed to create config dir");
        let user_config_content = r#"
            autoInstall = false
        "#;
        fs::write(
            kakehashi_config_dir.join("kakehashi.toml"),
            user_config_content,
        )
        .expect("failed to write user config");

        // Point XDG_CONFIG_HOME to our temp directory
        // SAFETY: #[serial(xdg_env)] prevents concurrent modification of XDG_CONFIG_HOME
        unsafe {
            env::set_var("XDG_CONFIG_HOME", user_config_dir.path());
        }

        // Load settings (no project path, just user config)
        let home = dirs::home_dir().map(|p| p.to_string_lossy().into_owned());
        let outcome = load_settings(None, None, home.as_deref(), |var| std::env::var(var).ok());

        // Restore original XDG_CONFIG_HOME
        // SAFETY: #[serial(xdg_env)] prevents concurrent modification of XDG_CONFIG_HOME
        unsafe {
            match original_xdg {
                Some(val) => env::set_var("XDG_CONFIG_HOME", val),
                None => env::remove_var("XDG_CONFIG_HOME"),
            }
        }

        // Verify: should have logged info event about loading user config
        let has_user_config_event = outcome
            .events
            .iter()
            .any(|e| e.kind == SettingsEventKind::Info && e.message.contains("user config"));

        assert!(
            has_user_config_event,
            "Should log info event about loading user config. Events: {:?}",
            outcome
                .events
                .iter()
                .map(|e| &e.message)
                .collect::<Vec<_>>()
        );
    }

    /// Verify that undefined env vars in override settings produce an Error event
    /// and discard the settings (returning None).
    #[test]
    fn test_load_settings_expansion_error_discards_settings() {
        use crate::config::make_env;

        let override_json = serde_json::json!({
            "searchPaths": ["$UNDEFINED_VAR/parsers"]
        });

        // Use a deterministic empty env so the test does not depend on
        // any particular variable being absent from the real environment.
        let env = make_env(&[]);
        let outcome = load_settings(
            None,
            Some((SettingsSource::InitializationOptions, override_json)),
            None,
            env,
        );

        assert!(
            outcome.settings.is_none(),
            "Settings should be None when expansion fails"
        );

        let has_error_event = outcome
            .events
            .iter()
            .any(|e| e.kind == SettingsEventKind::Error && e.message.contains("expansion failed"));

        assert!(
            has_error_event,
            "Should have an Error event about expansion failure. Events: {:?}",
            outcome
                .events
                .iter()
                .map(|e| format!("{:?}: {}", e.kind, &e.message))
                .collect::<Vec<_>>()
        );
    }

    /// load_toml_file: valid TOML parses correctly.
    #[test]
    fn test_load_toml_file_valid() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.toml");
        std::fs::write(&path, "autoInstall = false\n").unwrap();

        let mut events = Vec::new();
        let result = load_toml_file(&path, &mut events);

        assert!(result.is_some(), "valid TOML should parse");
        assert_eq!(result.unwrap().auto_install, Some(false));
        assert!(
            events
                .iter()
                .any(|e| e.kind == SettingsEventKind::Info
                    && e.message.contains("Successfully loaded")),
            "should log success"
        );
    }

    /// load_toml_file: non-existent path returns None + error event (ADR-0010).
    #[test]
    fn test_load_toml_file_missing() {
        let mut events = Vec::new();
        let result = load_toml_file(Path::new("/nonexistent/config.toml"), &mut events);

        assert!(result.is_none(), "missing file should return None");
        assert!(
            events
                .iter()
                .any(|e| e.kind == SettingsEventKind::Error && e.message.contains("not found")),
            "should emit error event for missing file"
        );
    }

    /// load_toml_file: invalid TOML returns None + warning event.
    #[test]
    fn test_load_toml_file_invalid_toml() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("bad.toml");
        std::fs::write(&path, "this is not [valid toml").unwrap();

        let mut events = Vec::new();
        let result = load_toml_file(&path, &mut events);

        assert!(result.is_none(), "invalid TOML should return None");
        assert!(
            events.iter().any(
                |e| e.kind == SettingsEventKind::Error && e.message.contains("Failed to parse")
            ),
            "should emit error for invalid TOML in explicit config file"
        );
    }
}
