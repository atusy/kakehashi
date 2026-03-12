//! User configuration loading for kakehashi.
//!
//! This module handles loading user-wide configuration from the XDG config directory.
//! User config location: $XDG_CONFIG_HOME/kakehashi/kakehashi.toml
//! Fallback: ~/.config/kakehashi/kakehashi.toml

use crate::config::RawWorkspaceSettings;
use log::warn;
use std::path::PathBuf;

/// Result type for user config loading operations.
pub type UserConfigResult<T> = Result<T, UserConfigError>;

/// Error type for user config loading.
#[derive(Debug)]
pub enum UserConfigError {
    /// I/O error reading the config file
    IoError {
        path: PathBuf,
        source: std::io::Error,
    },
    /// TOML parse error with context
    ParseError {
        path: PathBuf,
        source: toml::de::Error,
    },
}

impl std::fmt::Display for UserConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UserConfigError::IoError { path, source } => {
                write!(
                    f,
                    "Failed to read user config at {}: {}",
                    path.display(),
                    source
                )
            }
            UserConfigError::ParseError { path, source } => {
                write!(
                    f,
                    "Failed to parse user config at {}: {}",
                    path.display(),
                    source
                )
            }
        }
    }
}

impl std::error::Error for UserConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            UserConfigError::IoError { source, .. } => Some(source),
            UserConfigError::ParseError { source, .. } => Some(source),
        }
    }
}

/// Loads user configuration from the XDG config directory.
///
/// Returns:
/// - `Ok(Some(settings))` if the file exists and is valid TOML
/// - `Ok(None)` if the file does not exist (zero-config experience preserved)
/// - `Err(UserConfigError)` if the file exists but contains invalid TOML
pub fn load_user_config() -> UserConfigResult<Option<RawWorkspaceSettings>> {
    let path = match user_config_path() {
        Some(p) => p,
        None => return Ok(None), // No home directory, silently ignore
    };

    // Check if file exists
    if !path.exists() {
        return Ok(None); // Missing file is silently ignored (zero-config)
    }

    // Read and parse the file
    let contents = std::fs::read_to_string(&path).map_err(|e| UserConfigError::IoError {
        path: path.clone(),
        source: e,
    })?;

    let settings = toml::from_str::<RawWorkspaceSettings>(&contents)
        .map_err(|e| UserConfigError::ParseError { path, source: e })?;

    Ok(Some(settings))
}

/// Returns the path to the user configuration file.
///
/// The path is determined by:
/// 1. If $XDG_CONFIG_HOME is set and valid: $XDG_CONFIG_HOME/kakehashi/kakehashi.toml
/// 2. Otherwise: ~/.config/kakehashi/kakehashi.toml
///
/// Security: XDG_CONFIG_HOME is validated to prevent path traversal attacks:
/// - Must be an absolute path (not relative)
/// - Must not contain .. components (path traversal)
/// - Invalid paths trigger a warning and fall back to ~/.config
///
/// Returns None if the home directory cannot be determined.
pub fn user_config_path() -> Option<PathBuf> {
    use std::path::{Component, Path};

    // Check XDG_CONFIG_HOME first
    if let Ok(xdg_config) = std::env::var("XDG_CONFIG_HOME") {
        let xdg_path = Path::new(&xdg_config);

        // Security check: XDG_CONFIG_HOME must be an absolute path
        if !xdg_path.is_absolute() {
            warn!(
                "XDG_CONFIG_HOME is not an absolute path: '{}'. Falling back to ~/.config",
                xdg_config
            );
            // Fall through to use ~/.config
        } else {
            // Security check: Reject paths with .. components (traversal attempts)
            let has_parent_component = xdg_path.components().any(|c| c == Component::ParentDir);
            if has_parent_component {
                warn!(
                    "XDG_CONFIG_HOME contains path traversal (..) components: '{}'. Falling back to ~/.config",
                    xdg_config
                );
                // Fall through to use ~/.config
            } else {
                // Path is absolute and doesn't contain .. components
                return Some(xdg_path.join("kakehashi").join("kakehashi.toml"));
            }
        }
    }

    // Fallback to ~/.config/kakehashi/kakehashi.toml
    // dirs::home_dir() returns the user's home directory
    dirs::home_dir().map(|home| {
        home.join(".config")
            .join("kakehashi")
            .join("kakehashi.toml")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::env;

    #[test]
    #[serial(xdg_env)]
    fn user_config_path_uses_xdg_config_home_when_set() {
        // Save original value
        let original = env::var("XDG_CONFIG_HOME").ok();

        // Set XDG_CONFIG_HOME to a custom path
        // SAFETY: #[serial(xdg_env)] prevents concurrent modification of XDG_CONFIG_HOME
        unsafe {
            env::set_var("XDG_CONFIG_HOME", "/custom/config");
        }

        let path = user_config_path();

        // Restore original value
        // SAFETY: #[serial(xdg_env)] prevents concurrent modification of XDG_CONFIG_HOME
        unsafe {
            match original {
                Some(val) => env::set_var("XDG_CONFIG_HOME", val),
                None => env::remove_var("XDG_CONFIG_HOME"),
            }
        }

        assert!(
            path.is_some(),
            "user_config_path should return Some when XDG_CONFIG_HOME is set"
        );
        let path = path.unwrap();
        assert_eq!(
            path,
            PathBuf::from("/custom/config/kakehashi/kakehashi.toml"),
            "should use XDG_CONFIG_HOME/kakehashi/kakehashi.toml"
        );
    }

    #[test]
    #[serial(xdg_env)]
    fn user_config_path_falls_back_to_home_config_when_xdg_unset() {
        // Save original value
        let original = env::var("XDG_CONFIG_HOME").ok();

        // Remove XDG_CONFIG_HOME to test fallback
        // SAFETY: #[serial(xdg_env)] prevents concurrent modification of XDG_CONFIG_HOME
        unsafe {
            env::remove_var("XDG_CONFIG_HOME");
        }

        let path = user_config_path();

        // Restore original value
        // SAFETY: #[serial(xdg_env)] prevents concurrent modification of XDG_CONFIG_HOME
        unsafe {
            if let Some(val) = original {
                env::set_var("XDG_CONFIG_HOME", val);
            }
        }

        // On a normal system, we should get a path even without XDG_CONFIG_HOME
        // It should fall back to $HOME/.config/kakehashi/kakehashi.toml
        assert!(
            path.is_some(),
            "user_config_path should return Some even when XDG_CONFIG_HOME is unset"
        );
        let path = path.unwrap();

        // Verify the path structure - should end with kakehashi/kakehashi.toml
        // and contain ".config" in the path (the fallback behavior)
        let path_str = path.to_string_lossy();
        assert!(
            path_str.ends_with("kakehashi/kakehashi.toml"),
            "path should end with kakehashi/kakehashi.toml, got: {}",
            path_str
        );
        assert!(
            path_str.contains(".config"),
            "fallback path should contain .config, got: {}",
            path_str
        );
    }

    #[test]
    #[serial(xdg_env)]
    fn load_user_config_returns_none_for_missing_file() {
        use tempfile::TempDir;

        // Save original XDG_CONFIG_HOME
        let original = env::var("XDG_CONFIG_HOME").ok();

        // Point to a temp directory where no config file exists
        let temp_dir = TempDir::new().expect("failed to create temp dir");
        // SAFETY: #[serial(xdg_env)] prevents concurrent modification of XDG_CONFIG_HOME
        unsafe {
            env::set_var("XDG_CONFIG_HOME", temp_dir.path());
        }

        let result = load_user_config();

        // Restore original
        unsafe {
            match original {
                Some(val) => env::set_var("XDG_CONFIG_HOME", val),
                None => env::remove_var("XDG_CONFIG_HOME"),
            }
        }

        // Should succeed with None (no error, just missing file)
        assert!(
            result.is_ok(),
            "load_user_config should return Ok for missing file"
        );
        assert!(
            result.unwrap().is_none(),
            "load_user_config should return None when config file is missing"
        );
    }

    #[test]
    #[serial(xdg_env)]
    fn load_user_config_loads_valid_toml_file() {
        use std::fs;
        use tempfile::TempDir;

        // Save original XDG_CONFIG_HOME
        let original = env::var("XDG_CONFIG_HOME").ok();

        // Create temp directory with valid config file
        let temp_dir = TempDir::new().expect("failed to create temp dir");
        let config_dir = temp_dir.path().join("kakehashi");
        fs::create_dir_all(&config_dir).expect("failed to create config dir");

        let config_path = config_dir.join("kakehashi.toml");
        let config_content = r#"
            autoInstall = false
            searchPaths = ["/user/custom/path"]
        "#;
        fs::write(&config_path, config_content).expect("failed to write config file");

        // SAFETY: #[serial(xdg_env)] prevents concurrent modification of XDG_CONFIG_HOME
        unsafe {
            env::set_var("XDG_CONFIG_HOME", temp_dir.path());
        }

        let result = load_user_config();

        // Restore original
        // SAFETY: #[serial(xdg_env)] prevents concurrent modification of XDG_CONFIG_HOME
        unsafe {
            match original {
                Some(val) => env::set_var("XDG_CONFIG_HOME", val),
                None => env::remove_var("XDG_CONFIG_HOME"),
            }
        }

        // Should succeed with Some(settings)
        assert!(
            result.is_ok(),
            "load_user_config should return Ok for valid file"
        );
        let settings = result.unwrap();
        assert!(
            settings.is_some(),
            "load_user_config should return Some for existing file"
        );

        let settings = settings.unwrap();
        assert_eq!(
            settings.auto_install,
            Some(false),
            "should parse autoInstall"
        );
        assert_eq!(
            settings.search_paths,
            Some(vec!["/user/custom/path".to_string()]),
            "should parse searchPaths"
        );
    }

    #[test]
    #[serial(xdg_env)]
    fn load_user_config_returns_descriptive_error_for_invalid_toml() {
        use std::fs;
        use tempfile::TempDir;

        // Save original XDG_CONFIG_HOME
        let original = env::var("XDG_CONFIG_HOME").ok();

        // Create temp directory with invalid TOML config file
        let temp_dir = TempDir::new().expect("failed to create temp dir");
        let config_dir = temp_dir.path().join("kakehashi");
        fs::create_dir_all(&config_dir).expect("failed to create config dir");

        let config_path = config_dir.join("kakehashi.toml");
        // Invalid TOML: missing closing bracket
        let invalid_content = r#"
            autoInstall = true
            searchPaths = ["/path/one"
        "#;
        fs::write(&config_path, invalid_content).expect("failed to write config file");

        // SAFETY: #[serial(xdg_env)] prevents concurrent modification of XDG_CONFIG_HOME
        unsafe {
            env::set_var("XDG_CONFIG_HOME", temp_dir.path());
        }

        let result = load_user_config();

        // Restore original
        // SAFETY: #[serial(xdg_env)] prevents concurrent modification of XDG_CONFIG_HOME
        unsafe {
            match original {
                Some(val) => env::set_var("XDG_CONFIG_HOME", val),
                None => env::remove_var("XDG_CONFIG_HOME"),
            }
        }

        // Should return an error with descriptive message
        assert!(
            result.is_err(),
            "load_user_config should return Err for invalid TOML"
        );
        let error = result.unwrap_err();

        // Error message should include the file path
        let error_message = error.to_string();
        assert!(
            error_message.contains("kakehashi.toml"),
            "error should include file path, got: {}",
            error_message
        );

        // Error message should describe the parse issue
        assert!(
            error_message.contains("Failed to parse") || error_message.contains("parse"),
            "error should describe the parse failure, got: {}",
            error_message
        );
    }

    #[test]
    #[serial(xdg_env)]
    fn user_config_path_rejects_relative_xdg_config_home() {
        // Save original value
        let original = env::var("XDG_CONFIG_HOME").ok();

        // Set XDG_CONFIG_HOME to a relative path (security vulnerability)
        // SAFETY: #[serial(xdg_env)] prevents concurrent modification of XDG_CONFIG_HOME
        unsafe {
            env::set_var("XDG_CONFIG_HOME", "relative/path");
        }

        let path = user_config_path();

        // Restore original value
        // SAFETY: #[serial(xdg_env)] prevents concurrent modification of XDG_CONFIG_HOME
        unsafe {
            match original {
                Some(val) => env::set_var("XDG_CONFIG_HOME", val),
                None => env::remove_var("XDG_CONFIG_HOME"),
            }
        }

        // Should return None (falls back to ~/.config) for relative paths
        assert!(
            path.is_some(),
            "user_config_path should fall back to ~/.config for relative XDG_CONFIG_HOME"
        );
        let path = path.unwrap();
        let path_str = path.to_string_lossy();
        // Should fall back to ~/.config, not use the relative path
        assert!(
            path_str.contains(".config"),
            "should fall back to ~/.config for relative XDG_CONFIG_HOME, got: {}",
            path_str
        );
        assert!(
            !path_str.contains("relative/path"),
            "should not use relative path from XDG_CONFIG_HOME, got: {}",
            path_str
        );
    }

    #[test]
    #[serial(xdg_env)]
    fn user_config_path_rejects_path_traversal_in_xdg_config_home() {
        use std::fs;
        use tempfile::TempDir;

        // Save original value
        let original = env::var("XDG_CONFIG_HOME").ok();

        // Create a temp directory to use as a "safe" base
        let temp_dir = TempDir::new().expect("failed to create temp dir");
        let safe_path = temp_dir.path().join("safe");
        fs::create_dir_all(&safe_path).expect("failed to create safe dir");

        // Set XDG_CONFIG_HOME to a path with traversal (security vulnerability)
        // This attempts to escape to /etc or similar
        let traversal_path = format!("{}/../../../etc", safe_path.display());

        // SAFETY: #[serial(xdg_env)] prevents concurrent modification of XDG_CONFIG_HOME
        unsafe {
            env::set_var("XDG_CONFIG_HOME", &traversal_path);
        }

        let path = user_config_path();

        // Restore original value
        // SAFETY: #[serial(xdg_env)] prevents concurrent modification of XDG_CONFIG_HOME
        unsafe {
            match original {
                Some(val) => env::set_var("XDG_CONFIG_HOME", val),
                None => env::remove_var("XDG_CONFIG_HOME"),
            }
        }

        // Should fall back to ~/.config and NOT resolve to /etc
        assert!(
            path.is_some(),
            "user_config_path should fall back to ~/.config for traversal XDG_CONFIG_HOME"
        );
        let path = path.unwrap();
        let path_str = path.to_string_lossy();
        // Should fall back to ~/.config
        assert!(
            path_str.contains(".config"),
            "should fall back to ~/.config for traversal XDG_CONFIG_HOME, got: {}",
            path_str
        );
        // Should NOT contain /etc
        assert!(
            !path_str.contains("/etc/"),
            "should not resolve to /etc for traversal XDG_CONFIG_HOME, got: {}",
            path_str
        );
    }
}
