use crate::config::deprecation::DeprecatedKeysSeen;
use crate::config::paths::anchor_settings_paths;
use crate::config::{
    RawWorkspaceSettings, WorkspaceSettings, defaults::default_settings, load_user_config,
    merge_workspace_settings,
};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

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
    /// Which deprecated keys the loaded layers spelled — see
    /// [`DeprecatedKeysSeen`] for why this is not an `events` entry.
    pub(crate) deprecated_keys: DeprecatedKeysSeen,
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

/// Keys in a TOML config file that kakehashi does not recognise.
///
/// Reuses the walker the `workspace/didChangeConfiguration` path uses, via a
/// JSON round-trip, so both routes judge a key by the same schema. An empty
/// result when the TOML cannot be re-read as a generic value is deliberate:
/// this only ever *reports*, so a limitation of the conversion must not be
/// louder than the settings it is describing.
fn unknown_config_keys(contents: &str) -> Vec<String> {
    let Ok(value) = toml::from_str::<Value>(contents) else {
        return Vec::new();
    };
    let mut keys = crate::config::unknown_keys::unknown_workspace_setting_keys(&value);
    crate::config::unknown_keys::sort_and_dedup_unknown_keys(&mut keys);
    keys
}

/// The explicit layers merged with nothing beneath them — the subject of the
/// strict gate.
///
/// Programmed defaults are deliberately absent. They carry
/// `${KAKEHASHI_DATA_DIR}`, which cannot expand on a host with no discoverable
/// data directory, and blaming the user's file for that would reject a session
/// over a valid — even empty — config. Their absence costs the gate nothing:
/// `FeatureSettings::resolve` already fills an unset half of a timing pair from
/// the same defaults, so a lone `debounceMs` is still judged against the
/// default `maxWaitMs`.
fn merge_explicit_layers(layers: &[Option<RawWorkspaceSettings>]) -> Option<RawWorkspaceSettings> {
    layers
        .iter()
        .cloned()
        .reduce(merge_workspace_settings)
        .flatten()
}

/// The directory a config file lives in, as an absolute path.
///
/// A relative `--config-file` argument is resolved against the working
/// directory first: its *parent* would otherwise be relative too, and anchoring
/// a layer to a relative base leaves the layer relative to the working
/// directory — the dependence anchoring is meant to remove.
///
/// A path with no parent at all is reported as an error rather than as "no base
/// to anchor to". Only a filesystem root lacks one, which cannot be read as a
/// config file — so the caller never sees this — but answering `None` would make
/// the one unanchored outcome indistinguishable from success.
fn config_file_base(path: &Path) -> std::io::Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    absolute.parent().map(Path::to_path_buf).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "path has no parent directory",
        )
    })
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
    let mut deprecated_keys = DeprecatedKeysSeen::default();
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
        let mut layer = match load_toml_file(path, &mut events, &mut deprecated_keys) {
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
        //
        // Judged *before* anchoring, so the value the error quotes is the one
        // the user can find in their file. The verdict is the same either way:
        // anchoring cannot introduce an expansion error, since it prepends an
        // absolute base whose own `$` it escapes, and cannot remove one, since
        // it declines to fold a value carrying a variable. So this costs
        // nothing and reads better.
        if let Some(raw_settings) = layer.as_ref()
            && let Err(errs) = WorkspaceSettings::try_from_settings(raw_settings, home, &env_fn)
            && let Some(details) = errs.path_error_summary()
        {
            let message = format!("Path expansion failed in {}: {details}", path.display());
            events.push(SettingsEvent::error(message.clone()));
            fatal_error.get_or_insert(message);
        }
        // Anchor each file's relative paths to that file's own directory, so
        // `--config-file base.toml --config-file team/overrides.toml` reads
        // `./queries` as relative to whichever file wrote it.
        //
        // Failing to anchor is fatal rather than skipped, both when the
        // directory cannot be resolved and when it cannot be represented in a
        // `String` path field: falling through would silently resolve that
        // layer's paths against the working directory, which is the
        // launch-directory dependence this anchoring exists to remove. The
        // layers that degrade instead of failing are the implicit ones, whose
        // contract has always been to fall back rather than abort.
        //
        // An unusable directory is only reported when the layer actually has
        // something to anchor. A file naming only absolute paths does not care
        // where it lives, and rejecting the session over it would be a failure
        // the user cannot act on.
        if let Some(raw_settings) = layer.as_mut() {
            match config_file_base(path) {
                Ok(base) => {
                    let unanchored = anchor_settings_paths(raw_settings, Some(&base));
                    if !unanchored.is_empty() {
                        let message = format!(
                            "Cannot resolve {} in {} against that file's directory, so {} would \
                             silently resolve against the working directory instead. Either the \
                             directory's name is not valid UTF-8, or the path names a drive \
                             without a root (`C:lib`); write the path in full to fix it.",
                            if unanchored.len() == 1 {
                                "a path".to_string()
                            } else {
                                format!("{} paths", unanchored.len())
                            },
                            path.display(),
                            unanchored.join(", ")
                        );
                        events.push(SettingsEvent::error(message.clone()));
                        fatal_error.get_or_insert(message);
                    }
                }
                Err(error) => {
                    let message = format!(
                        "Failed to resolve the directory of {}: {error}",
                        path.display()
                    );
                    events.push(SettingsEvent::error(message.clone()));
                    fatal_error.get_or_insert(message);
                }
            }
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
        let merged = merge_explicit_layers(&layers);
        if let Some(raw_settings) = merged.as_ref()
            && let Err(errs) = WorkspaceSettings::try_from_settings(raw_settings, home, &env_fn)
        {
            let message = format!("Invalid configuration from --config-file: {errs}");
            events.push(SettingsEvent::error(message.clone()));
            fatal_error = Some(message);
        }
    }

    ExplicitConfig {
        layers,
        events,
        deprecated_keys,
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

    // Layer 1: Programmed defaults (configuration-merging-strategy: lowest precedence)
    //
    // No source directory, so nothing to anchor against: the defaults carry
    // `${KAKEHASHI_DATA_DIR}`, which is meant to reach expansion as written.
    let defaults = Some(default_settings());

    // Layers 2+3: config files (either explicit --config-file or default locations)
    //
    // Each layer's relative paths are anchored to the directory that layer came
    // from, while that is still known. Explicit layers were already anchored by
    // `read_explicit_layers`, which is the only place their per-file parents are
    // in scope.
    let config_layers: Vec<Option<RawWorkspaceSettings>> = if let Some(explicit) = explicit {
        events.extend(explicit.events);
        deprecated_keys.merge(explicit.deprecated_keys);
        explicit.layers
    } else {
        vec![
            // Layer 2: User config from XDG_CONFIG_HOME (~/.config/kakehashi/kakehashi.toml)
            //
            // A base that cannot be represented leaves values as written, which
            // is the pre-#732 meaning. Deliberately not fatal here: an implicit
            // layer's contract is to degrade rather than take the session down,
            // and `anchor_settings_paths` warns about what it skipped.
            load_user_config_with_events(&mut events, &mut deprecated_keys).map(
                |(mut settings, path)| {
                    let _ = anchor_settings_paths(&mut settings, path.parent());
                    settings
                },
            ),
            // Layer 3: Project config from root_path/kakehashi.toml
            load_toml_settings(root_path, &mut events, &mut deprecated_keys).map(|mut settings| {
                // `load_toml_settings` reads `root_path/kakehashi.toml`, so the
                // file's directory is `root_path` itself.
                let _ = anchor_settings_paths(&mut settings, root_path);
                settings
            }),
        ]
    };

    // Layer 4: Override settings from initialization options or client configuration.
    //
    // Client-supplied paths are workspace-local: the client knows the workspace
    // it opened, not the directory the server was launched from.
    let override_settings = override_settings
        .and_then(|(source, value)| {
            parse_override_settings(source, value, &mut events, &mut deprecated_keys)
        })
        .map(|mut settings| {
            let _ = anchor_settings_paths(&mut settings, root_path);
            settings
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
///
/// Reports the file it was read from alongside the settings, so the caller can
/// anchor the layer's relative paths to that file's directory.
fn load_user_config_with_events(
    events: &mut Vec<SettingsEvent>,
    deprecated_keys: &mut DeprecatedKeysSeen,
) -> Option<(RawWorkspaceSettings, PathBuf)> {
    match load_user_config() {
        Ok(Some(config)) => {
            events.push(SettingsEvent::info(
                "Loaded user config from XDG_CONFIG_HOME",
            ));
            deprecated_keys.merge(config.deprecated_keys);
            Some((config.settings, config.path))
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
            // Only the final component is inspected. A path whose *ancestor*
            // is a dangling link resolves to nothing at all, so nothing is
            // there to call unusable — that is the absent case, the same as a
            // missing directory. The line is "does the path the user named
            // exist?", and a link does while a path beneath a broken one does
            // not.
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

    // Serde drops an unrecognised field silently, so `autoInstal = false` reads
    // as "not specified" and the user gets defaults without being told why.
    // Warn rather than reject: a key kakehashi does not recognise may be a
    // typo, but it may equally be one this version has not learned yet, and
    // refusing to start over it would make the file version-locked.
    //
    // Not reached for a key inside `features`: those structs carry
    // `deny_unknown_fields`, so the parse above already failed and this file is
    // fatal. Inconsistent with the rule stated here, pre-existing, and pinned
    // by `test_load_toml_file_unknown_feature_key_is_fatal_today` so a future
    // change to it is a deliberate one.
    for key in unknown_config_keys(&contents) {
        events.push(SettingsEvent::warning(format!(
            "Unknown configuration key in {}: {key}",
            path.display()
        )));
    }

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

    /// Restores `XDG_CONFIG_HOME` when dropped, so a panic inside the test body
    /// cannot leave the variable pointing at a deleted `TempDir` and turn one
    /// failure into a cascade across the other `#[serial(xdg_env)]` tests.
    struct XdgConfigHome(Option<String>);

    impl Drop for XdgConfigHome {
        fn drop(&mut self) {
            // SAFETY: #[serial(xdg_env)] prevents concurrent modification.
            unsafe {
                match self.0.take() {
                    Some(value) => std::env::set_var("XDG_CONFIG_HOME", value),
                    None => std::env::remove_var("XDG_CONFIG_HOME"),
                }
            }
        }
    }

    /// Run `f` with `XDG_CONFIG_HOME` pointed at `config_home`, restoring the
    /// original afterwards. Callers must carry `#[serial(xdg_env)]`.
    fn with_xdg_config_home<T>(config_home: &Path, f: impl FnOnce() -> T) -> T {
        let _restore = XdgConfigHome(std::env::var("XDG_CONFIG_HOME").ok());
        // SAFETY: #[serial(xdg_env)] prevents concurrent modification.
        unsafe { std::env::set_var("XDG_CONFIG_HOME", config_home) };
        f()
    }

    /// A user config file at `<config_home>/kakehashi/kakehashi.toml`, returning
    /// the directory it was written to — the base its relative paths anchor to.
    fn write_user_config(config_home: &Path, contents: &str) -> PathBuf {
        let dir = config_home.join("kakehashi");
        std::fs::create_dir_all(&dir).expect("failed to create user config dir");
        std::fs::write(dir.join("kakehashi.toml"), contents).expect("failed to write user config");
        dir
    }

    /// A `--config-file` given as a bare filename anchors to the working
    /// directory, and one with no parent at all is an error rather than a base
    /// of "nowhere" — otherwise an unanchored layer would be indistinguishable
    /// from a successfully anchored one.
    #[test]
    fn config_file_base_resolves_a_bare_filename_and_rejects_a_parentless_path() {
        let cwd = std::env::current_dir().expect("a working directory");
        assert_eq!(
            config_file_base(Path::new("kakehashi.toml")).expect("a bare filename has a parent"),
            cwd
        );
        assert_eq!(
            config_file_base(Path::new("/"))
                .expect_err("a filesystem root has no parent")
                .kind(),
            std::io::ErrorKind::InvalidInput
        );
    }

    /// A project-local path resolves against the project config's directory, not
    /// the directory the server process happens to be running in (issue #732).
    #[test]
    #[serial(xdg_env)]
    fn project_relative_path_anchors_to_the_project_config() {
        let empty_config_home = TempDir::new().expect("failed to create user config temp dir");
        let project = TempDir::new().expect("failed to create project temp dir");
        std::fs::write(
            project.path().join("kakehashi.toml"),
            "searchPaths = [\"./runtime\"]\n",
        )
        .expect("failed to write project config");

        let outcome = with_xdg_config_home(empty_config_home.path(), || {
            load_settings(Some(project.path()), None, None, |_| None, None)
        });

        assert_eq!(
            outcome.settings.expect("settings should load").search_paths,
            [project.path().join("runtime").to_string_lossy()],
            "a project-relative searchPath belongs under the project config"
        );
    }

    /// The point of anchoring per layer rather than after the merge: each
    /// surviving field keeps the base of the file that wrote it, even when three
    /// layers each contribute one.
    #[test]
    #[serial(xdg_env)]
    fn merged_paths_keep_their_source_layer_base() {
        let config_home = TempDir::new().expect("failed to create user config temp dir");
        let user_dir = write_user_config(
            config_home.path(),
            "[languages.lua]\nparser = './parser/lua.so'\n",
        );
        let project = TempDir::new().expect("failed to create project temp dir");
        std::fs::write(
            project.path().join("kakehashi.toml"),
            "[languages.lua]\nqueries = [{ path = './queries/highlights.scm' }]\n",
        )
        .expect("failed to write project config");

        let outcome = with_xdg_config_home(config_home.path(), || {
            load_settings(
                Some(project.path()),
                Some((
                    SettingsSource::InitializationOptions,
                    serde_json::json!({ "searchPaths": ["./runtime"] }),
                )),
                None,
                |_| None,
                None,
            )
        });

        let settings = outcome.settings.expect("settings should load");
        assert_eq!(
            settings.search_paths,
            [project.path().join("runtime").to_string_lossy()],
            "a client-supplied path is workspace-local"
        );
        assert_eq!(
            settings.languages["lua"].parser.as_deref(),
            Some(user_dir.join("parser/lua.so").to_string_lossy().as_ref()),
            "the parser came from the user config, so it anchors there"
        );
        assert_eq!(
            settings.languages["lua"].queries.as_ref().unwrap()[0].path,
            project
                .path()
                .join("queries/highlights.scm")
                .to_string_lossy(),
            "the query came from the project config, so it anchors there"
        );
    }

    /// Anchoring must not consume the expansion syntax it runs ahead of.
    /// `effectiveConfiguration` reports these raw settings verbatim, and the
    /// programmed defaults' `${KAKEHASHI_DATA_DIR}` has to reach the expansion
    /// pass to pick up its platform default at all.
    #[test]
    #[serial(xdg_env)]
    fn expansion_syntax_reaches_expansion_unconsumed() {
        let config_home = TempDir::new().expect("failed to create user config temp dir");
        let project = TempDir::new().expect("failed to create project temp dir");

        let outcome = with_xdg_config_home(config_home.path(), || {
            load_settings(
                Some(project.path()),
                Some((
                    SettingsSource::InitializationOptions,
                    serde_json::json!({
                        "languages": { "lua": { "parser": "$LUA_PARSER" } },
                    }),
                )),
                None,
                |var| (var == "LUA_PARSER").then(|| "/opt/lua.so".to_string()),
                None,
            )
        });

        let raw = outcome.raw_settings.expect("raw settings should load");
        assert!(
            raw.search_paths
                .as_ref()
                .is_some_and(|paths| paths.iter().any(|path| path == "${KAKEHASHI_DATA_DIR}")),
            "the defaults' data-dir template must survive into the raw settings: {:?}",
            raw.search_paths
        );
        assert_eq!(
            raw.languages["lua"].parser.as_deref(),
            Some("$LUA_PARSER"),
            "a variable-led value is reported as the client wrote it"
        );
        assert_eq!(
            outcome.settings.expect("settings should load").languages["lua"]
                .parser
                .as_deref(),
            Some("/opt/lua.so"),
            "and still expands to an absolute path, unanchored"
        );
    }

    /// A `base` chain crosses layers: the inherited value is anchored by the
    /// layer that *wrote* it, not the layer that inherits it. Anchoring runs on
    /// each raw layer, before `resolve_base_configs` folds the chain, so every
    /// value is already absolute by the time it is copied.
    #[test]
    #[serial(xdg_env)]
    fn inherited_paths_anchor_to_the_layer_that_wrote_them() {
        let config_home = TempDir::new().expect("failed to create user config temp dir");
        let user_dir = write_user_config(
            config_home.path(),
            "[languages.shared]\nparser = './parser/shared.so'\n",
        );
        let project = TempDir::new().expect("failed to create project temp dir");
        std::fs::write(
            project.path().join("kakehashi.toml"),
            "[languages.derived]\nbase = 'shared'\n",
        )
        .expect("failed to write project config");

        let outcome = with_xdg_config_home(config_home.path(), || {
            load_settings(Some(project.path()), None, None, |_| None, None)
        });

        assert_eq!(
            outcome.settings.expect("settings should load").languages["derived"]
                .parser
                .as_deref(),
            Some(user_dir.join("parser/shared.so").to_string_lossy().as_ref()),
            "inheriting a parser must not re-base it onto the inheriting layer"
        );
    }

    /// Each `--config-file` layer anchors to its own parent directory, so two
    /// files that both say `./queries` mean two different directories.
    #[test]
    fn explicit_layers_anchor_to_their_own_directories() {
        let first_dir = TempDir::new().expect("failed to create first temp dir");
        let second_dir = TempDir::new().expect("failed to create second temp dir");
        let first = first_dir.path().join("base.toml");
        let second = second_dir.path().join("overrides.toml");
        std::fs::write(&first, "searchPaths = ['./runtime']\n").expect("failed to write first");
        std::fs::write(&second, "[languages.lua]\nparser = './parser/lua.so'\n")
            .expect("failed to write second");

        let explicit = read_explicit_layers(&[first, second], None, |_| None);
        assert!(
            explicit.fatal_error.is_none(),
            "both layers are valid: {:?}",
            explicit.fatal_error
        );

        let merged = merge_explicit_layers(&explicit.layers).expect("layers should merge");
        assert_eq!(
            merged.search_paths,
            Some(vec![
                first_dir
                    .path()
                    .join("runtime")
                    .to_string_lossy()
                    .into_owned()
            ])
        );
        assert_eq!(
            merged.languages["lua"].parser.as_deref(),
            Some(
                second_dir
                    .path()
                    .join("parser/lua.so")
                    .to_string_lossy()
                    .as_ref()
            )
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
        let mut ignored_deprecation = DeprecatedKeysSeen::default();
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

    /// A typo is reported rather than silently read as "not specified", which
    /// is what serde does with an unrecognised field. It stays a warning: an
    /// unrecognised key may be a mistake, or may be one a newer kakehashi
    /// understands, and rejecting the file would version-lock it.
    #[test]
    fn test_load_toml_file_warns_about_unknown_keys() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("typo.toml");
        std::fs::write(&path, "autoInstal = false\n").unwrap();

        let mut events = Vec::new();
        let mut ignored_deprecation = DeprecatedKeysSeen::default();
        let result = load_toml_file(&path, &mut events, &mut ignored_deprecation);

        assert!(
            matches!(result, Ok(Some(_))),
            "an unknown key must not reject the file: {result:?}"
        );
        assert!(
            events
                .iter()
                .any(|event| event.kind == SettingsEventKind::Warning
                    && event.message.contains("autoInstal")
                    && event.message.contains(&path.display().to_string())),
            "the typo and its file must both be named: {events:?}"
        );
    }

    /// Records, rather than endorses, an inconsistency: `FeatureSettings` and
    /// its children carry `deny_unknown_fields`, so an unknown key *inside*
    /// `features` fails typed deserialization and is fatal — while the same
    /// mistake anywhere else is a warning. Pinned so that changing it is a
    /// deliberate act with a test to update, not a silent drift.
    #[test]
    fn test_load_toml_file_unknown_feature_key_is_fatal_today() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("feature-typo.toml");
        std::fs::write(
            &path,
            "[features.\"textDocument/publishDiagnostics\"]\nfutureOption = 1\n",
        )
        .unwrap();

        let mut events = Vec::new();
        let mut ignored_deprecation = DeprecatedKeysSeen::default();
        let result = load_toml_file(&path, &mut events, &mut ignored_deprecation);

        let message = result
            .expect_err("today an unknown key under `features` is fatal, unlike anywhere else");
        assert!(
            message.contains("futureOption"),
            "the rejected key must be named: {message}"
        );
    }

    /// The recognised spelling must stay silent, or the warning is noise.
    #[test]
    fn test_load_toml_file_accepts_known_keys_without_warning() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("fine.toml");
        std::fs::write(
            &path,
            "autoInstall = false\n[languages.rust]\nparser = \"/p.so\"\n",
        )
        .unwrap();

        let mut events = Vec::new();
        let mut ignored_deprecation = DeprecatedKeysSeen::default();
        let result = load_toml_file(&path, &mut events, &mut ignored_deprecation);

        assert!(matches!(result, Ok(Some(_))), "{result:?}");
        assert!(
            events
                .iter()
                .all(|event| !event.message.contains("Unknown configuration key")),
            "a well-formed file must not warn: {events:?}"
        );
    }

    /// The strict gate judges the user's files, not the defaults beneath them.
    /// Programmed defaults carry `${KAKEHASHI_DATA_DIR}`, which has no value on
    /// a host with no discoverable data directory; merging them in would reject
    /// a session over a valid — even empty — `--config-file`.
    ///
    /// Asserted on the merge rather than on a verdict: whether the default
    /// expands depends on the host, so a test that ran the gate would pass on
    /// any developer machine even with the defaults merged back in.
    #[test]
    fn merge_explicit_layers_excludes_the_programmed_defaults() {
        let layers = vec![
            Some(RawWorkspaceSettings {
                auto_install: Some(false),
                ..Default::default()
            }),
            None,
        ];

        let merged = merge_explicit_layers(&layers).expect("the explicit layer should survive");

        assert_eq!(merged.auto_install, Some(false));
        assert!(
            merged.search_paths.is_none(),
            "the defaults' searchPaths must not join the subject of the gate: {:?}",
            merged.search_paths
        );
        assert!(
            default_settings().search_paths.is_some(),
            "this test is only meaningful while the defaults do carry searchPaths"
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

    /// A `--config-file` under a directory whose name is not valid UTF-8 cannot
    /// have its relative paths anchored, and the strict layer must not quietly
    /// accept that: the values would resolve against the working directory,
    /// which is precisely the dependence anchoring removes. A file with nothing
    /// to anchor is unaffected — it does not care where it lives.
    ///
    /// Runs where the filesystem allows such a name — Linux, and so CI. APFS
    /// rejects it outright, so on macOS the setup cannot be built and the test
    /// reports that rather than failing for an unrelated reason.
    #[cfg(unix)]
    #[test]
    fn read_explicit_layers_rejects_a_base_it_cannot_represent() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let parent = TempDir::new().unwrap();
        let dir = parent
            .path()
            .join(OsStr::from_bytes(b"proj-\xFF").to_os_string());
        if std::fs::create_dir(&dir).is_err() {
            eprintln!(
                "skipping: this filesystem rejects non-UTF-8 directory names, so the case \
                 under test cannot be constructed here"
            );
            return;
        }

        let relative = dir.join("relative.toml");
        std::fs::write(&relative, "searchPaths = ['./runtime']\n").unwrap();
        let rejected =
            read_explicit_layers(&[relative.clone()], None, crate::config::make_env(&[]));
        assert!(
            rejected
                .fatal_error
                .as_deref()
                .is_some_and(|message| message.contains("./runtime")),
            "a path needing this base must abort and name itself: {:?}",
            rejected.fatal_error
        );

        let absolute = dir.join("absolute.toml");
        std::fs::write(&absolute, "searchPaths = ['/opt/kakehashi']\n").unwrap();
        let accepted = read_explicit_layers(&[absolute], None, crate::config::make_env(&[]));
        assert_eq!(
            accepted.fatal_error, None,
            "a file with nothing to anchor does not care where it lives"
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
        let mut ignored_deprecation = DeprecatedKeysSeen::default();
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
        let mut ignored_deprecation = DeprecatedKeysSeen::default();
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
        let mut ignored_deprecation = DeprecatedKeysSeen::default();
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
        let mut ignored_deprecation = DeprecatedKeysSeen::default();
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
        assert!(
            used_deprecated.root_markers,
            "rootMarkers should set the flag"
        );
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
