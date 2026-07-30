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
}

/// Ceiling on a single config file read.
///
/// A kakehashi configuration is a hand-written TOML file; megabytes of one is a
/// mistake, not a use case. Without a bound, a `--config-file` naming an
/// endless source — `/dev/zero`, most obviously — would allocate until the
/// process died rather than reporting a bad path.
const MAX_CONFIG_FILE_BYTES: u64 = 8 * 1024 * 1024;

/// The `--config-file` inputs, read and judged exactly once.
///
/// Read *once* is a contract, not an optimisation. A file replaced between two
/// reads would have its second verdict either ignored or discovered too late to
/// matter, and a path that happens to name a stream would be found empty the
/// second time. (Naming a stream is not a supported workflow — one with no
/// writer will simply block initialization — but reading once is what keeps the
/// failure mode that of the path the user chose.)
pub(crate) struct ExplicitConfig {
    layers: Vec<Option<RawWorkspaceSettings>>,
    events: Vec<SettingsEvent>,
    deprecated_keys: DeprecatedKeysSeen,
    /// Why this configuration cannot be used, if it cannot. `initialize` must
    /// reject the session when this is set.
    pub(crate) fatal_error: Option<String>,
}

/// Read and judge the `--config-file` inputs, or `None` if there are none.
///
/// `initialize` calls this *before* it latches any once-only state from the
/// request. `tower-lsp-server` resets to `Uninitialized` after an error
/// response, so a client may fix the file and retry — and a retry carrying
/// different capabilities or workspace folders would otherwise be served with
/// the failed attempt's values, since those are first-write-wins. The result is
/// then handed to [`load_settings`], so the files are not read again.
pub(crate) fn load_explicit_config(
    home: Option<&str>,
    env_fn: impl Fn(&str) -> Option<String>,
) -> Option<ExplicitConfig> {
    let files = crate::config::expand::config_file_override()?;
    Some(read_explicit_layers(files, home, env_fn))
}

/// The body of [`load_explicit_config`], taking the paths directly so it can be
/// exercised without the process-global `--config-file` override.
fn read_explicit_layers(
    files: &[std::path::PathBuf],
    home: Option<&str>,
    env_fn: impl Fn(&str) -> Option<String>,
) -> ExplicitConfig {
    let env_fn = crate::config::expand::with_kakehashi_defaults(env_fn);
    let mut events = vec![SettingsEvent::info(format!(
        "Using {} explicit config file(s); default config locations skipped",
        files.len()
    ))];
    let mut used_deprecated_root_markers = false;
    let mut fatal_error = None;
    let mut layers = Vec::with_capacity(files.len());

    for path in files {
        // The verdict cannot change once a layer has failed, and reading on is
        // not free: a later path could be a FIFO that blocks forever, so an
        // already-doomed session would hang instead of reporting the failure it
        // already knows about.
        if fatal_error.is_some() {
            break;
        }
        let layer = match load_toml_file(path, &mut events, &mut used_deprecated_root_markers) {
            Ok(layer) => layer,
            Err(message) => {
                events.push(SettingsEvent::error(message.clone()));
                fatal_error.get_or_insert(message);
                None
            }
        };
        // Judging a layer in isolation also resolves its language `base`
        // chains, so a cycle an overlay later removes is still warned about
        // once here. Accepted: the alternative is threading a "stay quiet"
        // flag through base resolution to silence a warning that names a
        // real cycle in a file the user wrote.
        //
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

    // An explicit configuration also has to be valid *as a whole*, or startup
    // would silently continue on programmed defaults. This is the only place a
    // cross-layer invariant can be judged: one file supplying `debounceMs` and
    // another `maxWaitMs` is valid in neither file alone. It is judged without
    // `initializationOptions`, which keep a non-fatal policy of their own — a
    // client sending a bad override must not be reported as a mistake in the
    // user's file.
    //
    // Skipped once a layer has already failed: the verdict cannot change, and
    // every `try_from_settings` re-resolves language `base` chains, whose cycle
    // detector logs as it goes.
    if fatal_error.is_none() {
        // Only the explicit layers. Programmed defaults carry
        // `${KAKEHASHI_DATA_DIR}`, which cannot expand on a host with no
        // discoverable data directory — blaming the user's file for that would
        // reject a session over a valid, even empty, config. Their absence
        // costs nothing here: `FeatureSettings::resolve` already fills an
        // unset half of a timing pair from the same defaults, so a lone
        // `debounceMs` is still judged against the default `maxWaitMs`.
        let merged = layers
            .iter()
            .cloned()
            .reduce(merge_workspace_settings)
            .flatten();
        if let Some(raw_settings) = merged.as_ref()
            && let Err(errs) = WorkspaceSettings::try_from_settings(raw_settings, home, &env_fn)
        {
            fatal_error = Some(format!("Invalid configuration from --config-file: {errs}"));
        }
    }

    ExplicitConfig {
        layers,
        events,
        used_deprecated_root_markers,
        fatal_error,
    }
}

/// Merge every configuration layer into the settings the session will use.
///
/// `explicit` carries the already-read `--config-file` inputs; when it is
/// `Some`, the implicitly discovered user and project files are skipped and the
/// strict gate applies. Callers must obtain it from [`load_explicit_config`]
/// rather than reading the files themselves — passing `None` while
/// `--config-file` is set would silently fall back to implicit discovery.
pub fn load_settings(
    root_path: Option<&Path>,
    override_settings: Option<(SettingsSource, Value)>,
    home: Option<&str>,
    env_fn: impl Fn(&str) -> Option<String>,
    explicit: Option<ExplicitConfig>,
) -> SettingsLoadOutcome {
    let env_fn = crate::config::expand::with_kakehashi_defaults(env_fn);
    let mut events = Vec::new();
    let mut deprecated_keys = DeprecatedKeysSeen::default();
    let explicit_config_requested = crate::config::expand::config_file_override().is_some();

    // Layer 1: Programmed defaults (configuration-merging-strategy: lowest precedence)
    let defaults = Some(default_settings());

    // Layers 2+3: config files (either explicit --config-file or default locations)
    let config_layers: Vec<Option<RawWorkspaceSettings>> = if let Some(explicit) = explicit {
        events.extend(explicit.events);
        deprecated_keys.merge(explicit.deprecated_keys);
        explicit.layers
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
    let settings = expand_merged_settings(merged, home, &env_fn, &mut events);

    SettingsLoadOutcome {
        settings,
        raw_settings,
        events,
        deprecated_keys,
    }
}

/// Expand and validate the fully merged configuration, `initializationOptions`
/// included.
///
/// Failure here is never fatal on its own: it is the last, most permissive
/// gate, and the layers that *are* strict have already been judged by the
/// caller. Discarding the merge leaves `initialize` to start on programmed
/// defaults, which is the documented policy for a client-supplied override.
fn expand_merged_settings(
    merged: Option<RawWorkspaceSettings>,
    home: Option<&str>,
    env_fn: impl Fn(&str) -> Option<String>,
    events: &mut Vec<SettingsEvent>,
) -> Option<WorkspaceSettings> {
    merged.and_then(|settings| {
        match WorkspaceSettings::try_from_settings(&settings, home, env_fn) {
            Ok(settings) => Some(settings),
            Err(errs) => {
                events.push(SettingsEvent::error(format!(
                    "Invalid configuration: {errs}. \
                     This configuration has been discarded. \
                     Please correct the invalid settings or remove them from your config.",
                )));
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
    // Classify by opening, not by probing first: a separate `exists` check
    // would answer for a different moment than the open, and would have to
    // decide what "no" means without the kernel's reason for it. `NotFound` is
    // the only answer that can mean the optional-overlay case; everything else
    // — a denied ancestor directory, a path that is a directory — is a file the
    // user named and cannot use.
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            // Opening follows symlinks, so a link whose target is gone also
            // reports `NotFound`. That is not the optional-overlay case: the
            // path the user named does exist, it just does not lead to a
            // config. `symlink_metadata` does not follow, so it still sees the
            // link itself.
            //
            // This describes the path as of the probe, which is a moment after
            // the failed open. Something appearing there in between is
            // misreported — a config loader does not serialize against the
            // filesystem, and the alternative (probe first) only moves the
            // window.
            match path.symlink_metadata() {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    return Err(format!(
                        "Failed to read {}: broken symbolic link",
                        path.display()
                    ));
                }
                // Something appeared between the failed open and this probe.
                // Nothing was read, so report the absence the open saw rather
                // than guessing at what is there now.
                Ok(_) => {}
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                // The probe itself failed — a denied parent, an I/O error.
                // That is a path the user named and cannot use, not an absent
                // one, and falling through would skip it silently.
                Err(err) => {
                    return Err(format!("Failed to read {}: {}", path.display(), err));
                }
            }
            events.push(SettingsEvent::warning(format!(
                "Config file not found, skipping: {}",
                path.display()
            )));
            return Ok(None);
        }
        Err(err) => {
            return Err(format!("Failed to read {}: {}", path.display(), err));
        }
    };

    events.push(SettingsEvent::info(format!(
        "Loading config file: {}",
        path.display()
    )));

    // Read one byte past the ceiling so hitting it is distinguishable from a
    // file that merely ends there. Bytes, then length, then decoding: the
    // cutoff can land mid-character, and an oversized file should be reported
    // as oversized rather than as invalid UTF-8 the truncation invented.
    let mut bytes = Vec::new();
    use std::io::Read as _;
    file.take(MAX_CONFIG_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|err| format!("Failed to read {}: {}", path.display(), err))?;
    if bytes.len() as u64 > MAX_CONFIG_FILE_BYTES {
        return Err(format!(
            "Failed to read {}: larger than the {} MiB configuration limit",
            path.display(),
            MAX_CONFIG_FILE_BYTES / (1024 * 1024)
        ));
    }
    let contents = String::from_utf8(bytes)
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
        let outcome = load_settings(
            Some(project_dir.path()),
            None,
            home.as_deref(),
            |var| std::env::var(var).ok(),
            None,
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
            None,
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
        let outcome = load_settings(
            None,
            None,
            home.as_deref(),
            |var| std::env::var(var).ok(),
            None,
        );

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
            None,
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

    /// The last expansion gate stays non-fatal: it also carries
    /// `initializationOptions`, whose failures must not abort startup.
    #[test]
    fn merged_expansion_failure_discards_settings_without_aborting() {
        let merged = RawWorkspaceSettings {
            search_paths: Some(vec!["$UNDEFINED_VAR/parsers".to_string()]),
            ..Default::default()
        };
        let mut events = Vec::new();

        let settings = expand_merged_settings(
            Some(merged),
            None,
            crate::config::make_env(&[]),
            &mut events,
        );

        assert!(settings.is_none());
        assert!(
            events
                .iter()
                .any(|event| event.kind == SettingsEventKind::Error
                    && event.message.contains("UNDEFINED_VAR")),
            "the discarded configuration must still be reported: {events:?}"
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
            None,
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
            None,
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
        // A child of a fresh temp dir, so "absent" cannot depend on what the
        // host happens to have at a fixed absolute path.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        let mut events = Vec::new();
        let mut ignored_deprecation = DeprecatedKeysSeen::default();
        let result = load_toml_file(&path, &mut events, &mut ignored_deprecation);

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

    /// load_toml_file: a path whose metadata cannot even be reached is fatal,
    /// not "absent". `Path::exists()` answers `false` for both, which would
    /// silently skip a file the user cannot traverse to.
    #[cfg(unix)]
    #[test]
    fn test_load_toml_file_unreachable_parent() {
        use std::os::unix::fs::PermissionsExt;

        let dir = TempDir::new().unwrap();
        let locked_parent = dir.path().join("locked");
        std::fs::create_dir(&locked_parent).unwrap();
        let path = locked_parent.join("config.toml");
        std::fs::write(&path, "autoInstall = false\n").unwrap();
        std::fs::set_permissions(&locked_parent, std::fs::Permissions::from_mode(0o000)).unwrap();

        let mut events = Vec::new();
        let mut ignored_deprecation = false;
        // root ignores the permission bits, so probe before deciding to assert.
        let permissions_enforced = std::fs::read_to_string(&path).is_err();
        let result = load_toml_file(&path, &mut events, &mut ignored_deprecation);

        // Restore before asserting so a failure cannot leave an unremovable dir.
        std::fs::set_permissions(&locked_parent, std::fs::Permissions::from_mode(0o755)).unwrap();

        if !permissions_enforced {
            // Running as root (common in container CI): the bits do not apply,
            // so there is no denial to observe. Still assert the path is not
            // misread as absent, which is the regression this guards.
            assert!(
                matches!(result, Ok(Some(_))),
                "a reachable file must load: {result:?}"
            );
            return;
        }
        let message = result.expect_err("an unreachable explicit file must be fatal");
        assert!(
            message.contains(&path.display().to_string()) && !message.contains("not found"),
            "an unreachable path must not be reported as merely absent: {message}"
        );
    }

    /// Every explicit layer is read, in order, while the configuration is
    /// still usable.
    #[test]
    fn read_explicit_layers_reads_each_file_in_order() {
        let dir = TempDir::new().unwrap();
        let first = dir.path().join("first.toml");
        let second = dir.path().join("second.toml");
        std::fs::write(&first, "autoInstall = false\n").unwrap();
        std::fs::write(&second, "autoInstall = true\n").unwrap();

        let explicit = read_explicit_layers(
            &[first.clone(), second.clone()],
            None,
            crate::config::make_env(&[]),
        );

        assert!(explicit.fatal_error.is_none());
        assert_eq!(explicit.layers.len(), 2);
        assert!(
            explicit
                .events
                .iter()
                .any(|event| event.message.contains(&second.display().to_string())),
            "the second file must be read: {:?}",
            explicit.events
        );
    }

    /// A programmed default that cannot expand is not the explicit file's
    /// fault. `${KAKEHASHI_DATA_DIR}` has no value on a host with no
    /// discoverable data directory, and rejecting a session over that would
    /// turn a valid — even empty — `--config-file` into a startup failure.
    #[test]
    fn read_explicit_layers_does_not_blame_a_file_for_the_defaults() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("empty.toml");
        std::fs::write(&path, "").unwrap();

        // An environment where the data-directory fallback is unavailable, so
        // the default `searchPaths` entry cannot expand.
        let explicit = read_explicit_layers(&[path], None, |var| match var {
            "KAKEHASHI_DATA_DIR" => None,
            _ => std::env::var(var).ok(),
        });

        assert!(
            explicit.fatal_error.is_none(),
            "a valid explicit file must not inherit the defaults' failure: {:?}",
            explicit.fatal_error
        );
    }

    /// Once a layer has failed the verdict is settled, so later paths are not
    /// touched at all. This is what keeps a `--config-file` naming a FIFO from
    /// hanging a session over a failure already known — a property no
    /// finite-file test can observe, hence the assertion on the events.
    #[test]
    fn read_explicit_layers_stops_at_the_first_failure() {
        let dir = TempDir::new().unwrap();
        let broken = dir.path().join("broken.toml");
        let later = dir.path().join("later.toml");
        std::fs::write(&broken, "this is not [valid toml").unwrap();
        std::fs::write(&later, "autoInstall = true\n").unwrap();

        let explicit = read_explicit_layers(
            &[broken.clone(), later.clone()],
            None,
            crate::config::make_env(&[]),
        );

        assert!(
            explicit
                .fatal_error
                .as_deref()
                .is_some_and(|message| message.contains(&broken.display().to_string())),
            "the first failure must be reported: {:?}",
            explicit.fatal_error
        );
        assert!(
            explicit
                .events
                .iter()
                .all(|event| !event.message.contains(&later.display().to_string())),
            "nothing after the failure may be read: {:?}",
            explicit.events
        );
    }

    /// load_toml_file: a file past the size ceiling fails instead of being read
    /// to exhaustion. The ceiling is what keeps `--config-file /dev/zero` from
    /// allocating until the process dies.
    #[test]
    fn test_load_toml_file_over_size_limit() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("huge.toml");
        // A comment body, so the file is only rejected for its size — a valid
        // parse would otherwise be indistinguishable from a missing check.
        let mut oversized = String::with_capacity(MAX_CONFIG_FILE_BYTES as usize + 16);
        oversized.push_str("# ");
        oversized.extend(std::iter::repeat_n('a', MAX_CONFIG_FILE_BYTES as usize));
        std::fs::write(&path, &oversized).unwrap();

        let mut events = Vec::new();
        let mut ignored_deprecation = false;
        let result = load_toml_file(&path, &mut events, &mut ignored_deprecation);

        let message = result.expect_err("an oversized explicit file must be fatal");
        assert!(
            message.contains("configuration limit")
                && message.contains(&path.display().to_string()),
            "the limit must be named, and so must the file: {message}"
        );
    }

    /// The size check runs before decoding, so a file whose oversize happens to
    /// put a multi-byte character across the cutoff is still reported as
    /// oversized rather than as invalid UTF-8 the truncation itself created.
    #[test]
    fn test_load_toml_file_over_size_limit_reports_size_not_encoding() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("huge-utf8.toml");
        let mut oversized = String::from("# ");
        oversized.extend(std::iter::repeat_n('a', MAX_CONFIG_FILE_BYTES as usize - 2));
        oversized.push('é'); // straddles the ceiling
        std::fs::write(&path, &oversized).unwrap();

        let mut events = Vec::new();
        let mut ignored_deprecation = false;
        let result = load_toml_file(&path, &mut events, &mut ignored_deprecation);

        let message = result.expect_err("an oversized explicit file must be fatal");
        assert!(
            message.contains("configuration limit"),
            "size must outrank the encoding error the cutoff invented: {message}"
        );
    }

    /// load_toml_file: a symlink whose target is gone is present-but-unusable,
    /// not absent. `try_exists` follows the link and reports the *target*, so
    /// the two cases look identical without an explicit check.
    #[cfg(unix)]
    #[test]
    fn test_load_toml_file_broken_symlink() {
        let dir = TempDir::new().unwrap();
        let target = dir.path().join("generated.toml");
        let link = dir.path().join("config.toml");
        std::fs::write(&target, "autoInstall = false\n").unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        std::fs::remove_file(&target).unwrap();

        let mut events = Vec::new();
        let mut ignored_deprecation = false;
        let result = load_toml_file(&link, &mut events, &mut ignored_deprecation);

        let message = result.expect_err("a dangling explicit symlink must be fatal");
        assert!(
            message.contains(&link.display().to_string()) && !message.contains("not found"),
            "a dangling symlink must not be reported as merely absent: {message}"
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
            // Running as root: the bits do not apply. Assert the file still
            // loads rather than being misclassified as absent.
            assert!(
                matches!(result, Ok(Some(_))),
                "a readable file must load: {result:?}"
            );
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
