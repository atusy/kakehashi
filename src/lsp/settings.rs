use crate::config::deprecation::DeprecatedKeysSeen;
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
    /// Hard error normally surfaced via `window/showMessage`.
    ///
    /// Fatal explicit-config errors instead reject initialization before
    /// settings events are sent, so the initialize response is their sole
    /// client-facing report.
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
    /// True if any loaded config layer used the deprecated `rootMarkers` key
    /// (superseded by `workspaceMarkers`). Serde's alias erases which spelling
    /// was written, so this is detected from the raw config value. The
    /// `initialize` handler reads this field and surfaces a one-per-session
    /// deprecation notice (gated by `SettingsManager`'s claim guard, shared
    /// with the didChangeConfiguration path); it is intentionally kept out of
    /// `events` so the many callers that re-load settings do not re-warn.
    pub deprecated_keys: DeprecatedKeysSeen,
    /// Fatal error from an explicitly requested configuration source.
    ///
    /// A `--config-file` path that is *present* but unusable — unreadable,
    /// malformed, or carrying a path that cannot be expanded — represents a
    /// user mistake that must not be papered over with defaults. An *absent*
    /// explicit file stays optional (see `load_toml_file`), as do all
    /// implicitly discovered user/project files.
    pub(crate) fatal_error: Option<String>,
}

pub fn load_settings(
    root_path: Option<&Path>,
    override_settings: Option<(SettingsSource, Value)>,
    home: Option<&str>,
    env_fn: impl Fn(&str) -> Option<String>,
) -> SettingsLoadOutcome {
    let env_fn = crate::config::expand::with_kakehashi_defaults(env_fn);
    let mut events = Vec::new();
    let mut deprecated_keys = DeprecatedKeysSeen::default();
    let mut fatal_error = None;
    let explicit_config_requested = crate::config::expand::config_file_override().is_some();

    // Layer 1: Programmed defaults (configuration-merging-strategy: lowest precedence)
    let defaults = Some(default_settings());

    // Layers 2+3: config files (either explicit --config-file or default locations)
    let explicit_files = crate::config::expand::config_file_override();
    let config_layers: Vec<Option<RawWorkspaceSettings>> = if let Some(files) = explicit_files {
        events.push(SettingsEvent::info(format!(
            "Using {} explicit config file(s); default config locations skipped",
            files.len()
        )));
        let mut layers = Vec::with_capacity(files.len());
        for path in files {
            let layer = match load_toml_file(path, &mut events, &mut deprecated_keys) {
                Ok(layer) => layer,
                Err(message) => {
                    events.push(SettingsEvent::error(message.clone()));
                    fatal_error.get_or_insert(message);
                    None
                }
            };
            // Judge each layer's *paths* on its own so a later layer cannot
            // mask an earlier one's undefined variable: path fields are
            // replaced wholesale by the overlay, so the merged result would
            // never mention the mistake. Cross-field invariants are excluded
            // here (see `ExpandErrors::path_error_summary`) — their operands
            // merge independently, so they are only meaningful once every
            // layer has been folded together, and `expand_merged_settings`
            // catches them there.
            if let Some(raw_settings) = layer.as_ref()
                && let Err(errs) = WorkspaceSettings::try_from_settings(raw_settings, home, &env_fn)
                && let Some(details) = errs.path_error_summary()
            {
                let message = format!("Path expansion failed in {}: {details}", path.display());
                events.push(SettingsEvent::error(message.clone()));
                fatal_error.get_or_insert(message);
            }
            layers.push(layer);
        }
        layers
    } else {
        vec![
            // Layer 2: User config from XDG_CONFIG_HOME (~/.config/kakehashi/kakehashi.toml)
            load_user_config_with_events(&mut events, &mut deprecated_keys),
            // Layer 3: Project config from root_path/kakehashi.toml
            load_toml_settings(root_path, &mut events, &mut deprecated_keys),
        ]
    };

    // Layer 4: Override settings from initialization options or client configuration
    let override_settings = override_settings.and_then(|(source, value)| {
        parse_override_settings(source, value, &mut events, &mut deprecated_keys)
    });

    // Merge all layers: defaults < config_layers < override (later layers override earlier)
    let mut layers = vec![defaults];
    layers.extend(config_layers);
    layers.push(override_settings);
    let merged = layers
        .into_iter()
        .reduce(merge_workspace_settings)
        .flatten();
    let raw_settings = merged.clone();
    let settings = expand_merged_settings(
        merged,
        home,
        &env_fn,
        &mut events,
        &mut fatal_error,
        explicit_files.is_some(),
    );

    SettingsLoadOutcome {
        settings,
        raw_settings,
        events,
        deprecated_keys,
        fatal_error,
    }
}

/// Expand and validate the fully merged configuration.
///
/// `explicit_config` marks a session driven by `--config-file`. Those paths
/// represent user intent, so a merged configuration that cannot be expanded is
/// fatal rather than silently degrading to programmed defaults. This is also
/// the only place a cross-layer invariant violation can be caught — one file
/// supplying `debounceMs` and another `maxWaitMs` is valid in neither file
/// alone, yet the pair must still be checked once merged.
fn expand_merged_settings(
    merged: Option<RawWorkspaceSettings>,
    home: Option<&str>,
    env_fn: impl Fn(&str) -> Option<String>,
    events: &mut Vec<SettingsEvent>,
    fatal_error: &mut Option<String>,
    explicit_config: bool,
) -> Option<WorkspaceSettings> {
    merged.and_then(|settings| {
        match WorkspaceSettings::try_from_settings(&settings, home, env_fn) {
            Ok(settings) => Some(settings),
            Err(errs) => {
                let message = format!(
                    "Invalid configuration: {errs}. \
                     This configuration has been discarded. \
                     Please correct the invalid settings or remove them from your config.",
                );
                events.push(SettingsEvent::error(message.clone()));
                if explicit_config {
                    fatal_error.get_or_insert(message);
                }
                None
            }
        }
    })
}

/// Load user config and add appropriate events to the events vector.
fn load_user_config_with_events(
    events: &mut Vec<SettingsEvent>,
    deprecated_keys: &mut DeprecatedKeysSeen,
) -> Option<RawWorkspaceSettings> {
    match load_user_config() {
        Ok(Some(config)) => {
            events.push(SettingsEvent::info(
                "Loaded user config from XDG_CONFIG_HOME",
            ));
            deprecated_keys.merge(config.deprecated_keys);
            Some(config.settings)
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
/// Every `Err` returned here is fatal to the session: unlike
/// `load_toml_settings`, a file the user named explicitly and that is actually
/// present must not be skipped in favour of defaults
/// (configuration-merging-strategy).
///
/// An absent file is `Ok(None)`, not an error. Layered invocations
/// (`--config-file base.toml --config-file overrides.toml`) rely on the overlay
/// being optional, and a relative path resolves against the process working
/// directory — for an editor-spawned server that is the editor's, not the
/// workspace root — so absence is too easily accidental to be worth aborting
/// over. It is still reported as a warning so the skip is visible.
fn load_toml_file(
    path: &Path,
    events: &mut Vec<SettingsEvent>,
    deprecated_keys: &mut DeprecatedKeysSeen,
) -> Result<Option<RawWorkspaceSettings>, String> {
    if !path.exists() {
        events.push(SettingsEvent::warning(format!(
            "Config file not found, skipping: {}",
            path.display()
        )));
        return Ok(None);
    }

    events.push(SettingsEvent::info(format!(
        "Loading config file: {}",
        path.display()
    )));

    let contents = fs::read_to_string(path)
        .map_err(|err| format!("Failed to read {}: {}", path.display(), err))?;
    deprecated_keys.merge(crate::config::deprecation::toml_deprecated_keys(&contents));
    let settings = toml::from_str::<RawWorkspaceSettings>(&contents)
        .map_err(|err| format!("Failed to parse {}: {}", path.display(), err))?;

    events.push(SettingsEvent::info(format!(
        "Successfully loaded {}",
        path.display()
    )));
    Ok(Some(settings))
}

fn load_toml_settings(
    root_path: Option<&Path>,
    events: &mut Vec<SettingsEvent>,
    deprecated_keys: &mut DeprecatedKeysSeen,
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
        Ok(contents) => {
            deprecated_keys.merge(crate::config::deprecation::toml_deprecated_keys(&contents));
            match toml::from_str::<RawWorkspaceSettings>(&contents) {
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
            }
        }
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
    deprecated_keys: &mut DeprecatedKeysSeen,
) -> Option<RawWorkspaceSettings> {
    deprecated_keys.merge(crate::config::deprecation::json_deprecated_keys(&value));
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

    /// load_settings() merges 4 layers via reduce(merge_workspace_settings):
    /// defaults < user (XDG_CONFIG_HOME) < project < InitializationOptions.
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

    /// override_settings (InitializationOptions) has highest precedence.
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

    /// User config loading logs appropriate events.
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

        let has_error_event = outcome.events.iter().any(|e| {
            e.kind == SettingsEventKind::Error
                && e.message.contains("Invalid configuration")
                && e.message.contains("UNDEFINED_VAR")
        });

        assert!(
            has_error_event,
            "Should have an Error event about expansion failure. Events: {:?}",
            outcome
                .events
                .iter()
                .map(|e| format!("{:?}: {}", e.kind, &e.message))
                .collect::<Vec<_>>()
        );
        assert!(
            outcome
                .events
                .iter()
                .all(|event| !event.message.contains("previous settings")),
            "initialization-time errors must not claim that previous settings exist"
        );
    }

    /// A merged configuration that fails expansion is fatal only when the user
    /// named the files explicitly; implicit discovery keeps degrading to
    /// defaults so the zero-config experience survives a stray config file.
    #[test]
    fn merged_expansion_failure_is_fatal_only_for_explicit_config() {
        let broken = || RawWorkspaceSettings {
            search_paths: Some(vec!["$UNDEFINED_VAR/parsers".to_string()]),
            ..Default::default()
        };

        let mut explicit_events = Vec::new();
        let mut explicit_fatal = None;
        let explicit = expand_merged_settings(
            Some(broken()),
            None,
            crate::config::make_env(&[]),
            &mut explicit_events,
            &mut explicit_fatal,
            true,
        );

        let mut implicit_events = Vec::new();
        let mut implicit_fatal = None;
        let implicit = expand_merged_settings(
            Some(broken()),
            None,
            crate::config::make_env(&[]),
            &mut implicit_events,
            &mut implicit_fatal,
            false,
        );

        assert!(explicit.is_none());
        assert!(implicit.is_none());
        assert!(
            explicit_fatal
                .as_deref()
                .is_some_and(|message| message.contains("UNDEFINED_VAR")),
            "explicit config must abort startup: {explicit_fatal:?}"
        );
        assert!(
            implicit_fatal.is_none(),
            "implicit config must keep falling back to defaults: {implicit_fatal:?}"
        );
    }

    /// A config layer using the deprecated `rootMarkers` key sets the outcome
    /// flag (surfaced once per session by `initialize`), while the canonical
    /// `workspaceMarkers` key does not. XDG is pointed at an empty temp dir so
    /// the developer's real user config cannot pollute the result.
    #[test]
    #[serial(xdg_env)]
    fn load_settings_flags_deprecated_root_markers_in_override() {
        use crate::config::make_env;
        use std::env;

        let original_xdg = env::var("XDG_CONFIG_HOME").ok();
        let empty_config = TempDir::new().expect("failed to create temp dir");
        // SAFETY: #[serial(xdg_env)] prevents concurrent modification.
        unsafe {
            env::set_var("XDG_CONFIG_HOME", empty_config.path());
        }

        let deprecated = serde_json::json!({
            "languageServers": { "rust-analyzer": { "rootMarkers": [".git"] } }
        });
        let flagged = load_settings(
            None,
            Some((SettingsSource::InitializationOptions, deprecated)),
            None,
            make_env(&[]),
        )
        .deprecated_keys
        .root_markers;

        let canonical = serde_json::json!({
            "languageServers": { "rust-analyzer": { "workspaceMarkers": [".git"] } }
        });
        let unflagged = load_settings(
            None,
            Some((SettingsSource::InitializationOptions, canonical)),
            None,
            make_env(&[]),
        )
        .deprecated_keys
        .root_markers;

        // SAFETY: #[serial(xdg_env)] prevents concurrent modification.
        unsafe {
            match original_xdg {
                Some(val) => env::set_var("XDG_CONFIG_HOME", val),
                None => env::remove_var("XDG_CONFIG_HOME"),
            }
        }

        assert!(
            flagged,
            "rootMarkers in the override layer should set the deprecation flag"
        );
        assert!(
            !unflagged,
            "workspaceMarkers must not set the deprecation flag"
        );
    }

    /// load_toml_file: valid TOML parses correctly.
    #[test]
    fn test_load_toml_file_valid() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.toml");
        std::fs::write(&path, "autoInstall = false\n").unwrap();

        let mut events = Vec::new();
        let mut ignored_deprecation = DeprecatedKeysSeen::default();
        let result = load_toml_file(&path, &mut events, &mut ignored_deprecation);

        let settings = result.expect("valid TOML should parse");
        assert_eq!(
            settings
                .expect("present file should yield a layer")
                .auto_install,
            Some(false)
        );
        assert!(
            events
                .iter()
                .any(|e| e.kind == SettingsEventKind::Info
                    && e.message.contains("Successfully loaded")),
            "should log success"
        );
    }

    /// load_toml_file: an absent explicit path is an optional layer, not an
    /// error, so a layered invocation whose overlay does not exist still starts.
    #[test]
    fn test_load_toml_file_missing() {
        let mut events = Vec::new();
        let mut ignored_deprecation = DeprecatedKeysSeen::default();
        let result = load_toml_file(
            Path::new("/nonexistent/config.toml"),
            &mut events,
            &mut ignored_deprecation,
        );

        assert!(
            matches!(result, Ok(None)),
            "missing file should be skipped, not fatal: {result:?}"
        );
        assert!(
            events
                .iter()
                .any(|e| e.kind == SettingsEventKind::Warning && e.message.contains("not found")),
            "the skip must still be visible as a warning"
        );
    }

    /// load_toml_file: a present file with invalid TOML is fatal.
    #[test]
    fn test_load_toml_file_invalid_toml() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("bad.toml");
        std::fs::write(&path, "this is not [valid toml").unwrap();

        let mut events = Vec::new();
        let mut ignored_deprecation = DeprecatedKeysSeen::default();
        let result = load_toml_file(&path, &mut events, &mut ignored_deprecation);

        let message = result.expect_err("invalid TOML in an explicit file must be fatal");
        assert!(
            message.contains("Failed to parse") && message.contains(&path.display().to_string()),
            "the fatal message must name the offending file: {message}"
        );
    }

    /// load_toml_file: a file that exists but cannot be read is fatal, and is
    /// reported differently from a file that is simply absent.
    #[cfg(unix)]
    #[test]
    fn test_load_toml_file_unreadable() {
        use std::os::unix::fs::PermissionsExt;

        let dir = TempDir::new().unwrap();
        let path = dir.path().join("locked.toml");
        std::fs::write(&path, "autoInstall = false\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();

        let mut events = Vec::new();
        let mut ignored_deprecation = false;
        // root ignores the permission bits, so probe before deciding to assert.
        let permissions_enforced = std::fs::read_to_string(&path).is_err();
        let result = load_toml_file(&path, &mut events, &mut ignored_deprecation);

        // Restore before asserting so a failure cannot leave an unremovable dir.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        if !permissions_enforced {
            return;
        }
        let message = result.expect_err("an unreadable explicit file must be fatal");
        assert!(
            message.contains("Failed to read") && message.contains(&path.display().to_string()),
            "the fatal message must name the offending file: {message}"
        );
    }

    /// load_toml_file (the --config-file layer) sets the deprecation flag when
    /// the file uses `rootMarkers`; a regression dropping the detector `|=` in
    /// this layer would otherwise slip through.
    #[test]
    fn load_toml_file_flags_deprecated_root_markers() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("kakehashi.toml");
        std::fs::write(&path, "[languageServers.x]\nrootMarkers = [\".git\"]\n").unwrap();

        let mut events = Vec::new();
        let mut used_deprecated = DeprecatedKeysSeen::default();
        let result = load_toml_file(&path, &mut events, &mut used_deprecated);

        assert!(
            matches!(result, Ok(Some(_))),
            "valid TOML should parse: {result:?}"
        );
        assert!(used_deprecated.root_markers, "rootMarkers should set the flag");
    }

    /// load_toml_settings (the project kakehashi.toml layer) sets the flag when
    /// the file uses `rootMarkers`.
    #[test]
    fn load_toml_settings_flags_deprecated_root_markers() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("kakehashi.toml"),
            "[languageServers.x]\nrootMarkers = [\".git\"]\n",
        )
        .unwrap();

        let mut events = Vec::new();
        let mut used_deprecated = DeprecatedKeysSeen::default();
        let result = load_toml_settings(Some(dir.path()), &mut events, &mut used_deprecated);

        assert!(result.is_some(), "valid project config should parse");
        assert!(
            used_deprecated.root_markers,
            "rootMarkers should set the flag"
        );
    }
}
