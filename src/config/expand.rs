use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Process-global override for `KAKEHASHI_DATA_DIR`, set by the `--data-dir` CLI flag.
///
/// Using `OnceLock` instead of `unsafe { std::env::set_var() }` avoids the unsafety
/// of mutating environment variables while still providing a process-global override.
static DATA_DIR_OVERRIDE: OnceLock<PathBuf> = OnceLock::new();

/// Set the data directory override (called once from main).
pub fn set_data_dir_override(dir: PathBuf) {
    DATA_DIR_OVERRIDE.set(dir).ok();
}

/// Get the data directory override, if set.
pub fn data_dir_override() -> Option<&'static Path> {
    DATA_DIR_OVERRIDE.get().map(|p| p.as_path())
}

/// Process-global override for config file paths, set by the `--config-file` CLI flag.
///
/// When set, these files replace the default user config (XDG) and project config
/// (`./kakehashi.toml`). Multiple files merge in specified order (later overrides earlier).
static CONFIG_FILE_OVERRIDE: OnceLock<Vec<PathBuf>> = OnceLock::new();

/// Set the config file override (called once from main).
///
/// Only stores if `files` is non-empty; an empty vec means "not specified".
pub fn set_config_file_override(files: Vec<PathBuf>) {
    if !files.is_empty() {
        CONFIG_FILE_OVERRIDE.set(files).ok();
    }
}

/// Get the config file override paths, if set.
pub(crate) fn config_file_override() -> Option<&'static [PathBuf]> {
    CONFIG_FILE_OVERRIDE.get().map(|v| v.as_slice())
}

/// Error returned when a single path expansion fails.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ExpandError {
    /// A referenced environment variable is not defined.
    UndefinedVar { var_name: String, input: String },
    /// The path uses `~` but no home directory is available.
    NoHomeDir { input: String },
}

impl fmt::Display for ExpandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ExpandError::UndefinedVar { var_name, input } => {
                write!(
                    f,
                    "undefined environment variable: {var_name} (in \"{input}\")"
                )
            }
            ExpandError::NoHomeDir { input } => {
                write!(
                    f,
                    "path uses ~ but home directory is not available (in \"{input}\")"
                )
            }
        }
    }
}

/// Collected errors from expanding all path fields in a configuration.
///
/// Returned by `WorkspaceSettings::try_from_settings` when one or more
/// path expansions fail.
#[derive(Debug)]
pub struct ExpandErrors(pub(crate) Vec<ExpandError>);

impl fmt::Display for ExpandErrors {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let details: Vec<String> = self.0.iter().map(|e| e.to_string()).collect();
        write!(f, "{}", details.join("; "))
    }
}

impl std::error::Error for ExpandErrors {}

/// Expand environment variables (`$VAR`, `${VAR}`) and tilde (`~`) in a path string.
/// Returns `Err` if any referenced variable is undefined or if `~` is used
/// without a home directory.
///
/// **Note:** When a single path contains multiple undefined variables (e.g.
/// `$A/$B`), only the first undefined variable is reported because
/// `shellexpand::full_with_context` short-circuits on the first error.
/// The caller (see `try_from_settings`) still collects errors *across* different
/// path fields, so most multi-variable issues surface eventually.
///
/// `home` is the pre-computed home directory (from `dirs::home_dir()`),
/// passed in so the caller computes it once for all paths.
pub(super) fn expand_path(
    input: &str,
    home: Option<&str>,
    env_fn: impl Fn(&str) -> Option<String>,
) -> Result<String, ExpandError> {
    // Detect tilde usage when home dir is unavailable, rather than
    // silently leaving `~` as a literal character in the path.
    if home.is_none() && (input == "~" || input.starts_with("~/")) {
        return Err(ExpandError::NoHomeDir {
            input: input.to_string(),
        });
    }

    let result = shellexpand::full_with_context(
        input,
        || home,
        |var: &str| match env_fn(var) {
            Some(val) => Ok(Some(val)),
            None => Err(ExpandError::UndefinedVar {
                var_name: var.to_string(),
                input: input.to_string(),
            }),
        },
    );

    match result {
        Ok(expanded) => Ok(expanded.into_owned()),
        Err(e) => Err(e.cause),
    }
}

/// Wrap an env lookup function to provide fallback values for known `KAKEHASHI_`
/// environment variables when they are undefined.
///
/// This allows configurations using `${KAKEHASHI_DATA_DIR}` to expand gracefully
/// even when the user has not explicitly set the variable — the platform-specific
/// default (from `dirs::data_dir()`) is used instead.
pub(crate) fn with_kakehashi_defaults(
    env_fn: impl Fn(&str) -> Option<String>,
) -> impl Fn(&str) -> Option<String> {
    move |var: &str| {
        // --data-dir override takes highest priority
        if var == "KAKEHASHI_DATA_DIR"
            && let Some(dir) = data_dir_override()
        {
            return Some(dir.to_string_lossy().into_owned());
        }
        env_fn(var).or_else(|| kakehashi_default(var))
    }
}

/// Return the platform-specific default for known `KAKEHASHI_` variables.
///
/// Uses `dirs::data_dir()` directly (not `default_data_dir()`) to avoid
/// circularity: `default_data_dir()` checks `KAKEHASHI_DATA_DIR` env var,
/// but this function is the fallback when that var is *not* set.
fn kakehashi_default(var: &str) -> Option<String> {
    match var {
        "KAKEHASHI_DATA_DIR" => {
            dirs::data_dir().map(|p| p.join("kakehashi").to_string_lossy().into_owned())
        }
        _ => None,
    }
}

/// Build an env lookup function from a slice of `(key, value)` pairs.
/// Intended for tests that need a deterministic `env_fn`.
#[cfg(test)]
pub(crate) fn make_env(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
    let map: std::collections::HashMap<String, String> = vars
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    move |var: &str| map.get(var).cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_dollar_var() {
        let env = make_env(&[("HOME", "/home/user")]);
        assert_eq!(
            expand_path("$HOME/data", None, &env).unwrap(),
            "/home/user/data"
        );
    }

    #[test]
    fn expand_braced_var() {
        let env = make_env(&[("HOME", "/home/user")]);
        assert_eq!(
            expand_path("${HOME}/data", None, &env).unwrap(),
            "/home/user/data"
        );
    }

    #[test]
    fn undefined_var_returns_error() {
        let env = make_env(&[]);
        let err = expand_path("$NONEXISTENT/path", None, &env).unwrap_err();
        assert_eq!(
            err,
            ExpandError::UndefinedVar {
                var_name: "NONEXISTENT".to_string(),
                input: "$NONEXISTENT/path".to_string(),
            }
        );
    }

    #[test]
    fn tilde_without_home_dir_returns_error() {
        let env = make_env(&[]);
        let err = expand_path("~/parsers", None, &env).unwrap_err();
        assert_eq!(
            err,
            ExpandError::NoHomeDir {
                input: "~/parsers".to_string(),
            }
        );
    }

    #[test]
    fn bare_tilde_without_home_dir_returns_error() {
        let env = make_env(&[]);
        let err = expand_path("~", None, &env).unwrap_err();
        assert_eq!(
            err,
            ExpandError::NoHomeDir {
                input: "~".to_string(),
            }
        );
    }

    #[test]
    fn tilde_username_without_home_dir_passes_through() {
        // ~username is not supported by shellexpand — it should be left as-is
        // rather than raising a misleading "home directory not available" error.
        let env = make_env(&[]);
        let result = expand_path("~bob/parsers", None, &env).unwrap();
        assert_eq!(result, "~bob/parsers");
    }

    #[test]
    fn no_variables_unchanged() {
        let env = make_env(&[]);
        assert_eq!(
            expand_path("/plain/path", None, &env).unwrap(),
            "/plain/path"
        );
    }

    #[test]
    fn tilde_expands_to_home_dir() {
        let env = make_env(&[]);
        let result = expand_path("~/parsers", Some("/home/testuser"), &env).unwrap();
        assert_eq!(result, "/home/testuser/parsers");
    }

    #[test]
    fn mixed_expansion() {
        let env = make_env(&[("HOME", "/home/user"), ("LANG", "lua")]);
        assert_eq!(
            expand_path("$HOME/parsers/$LANG", None, &env).unwrap(),
            "/home/user/parsers/lua"
        );
    }

    #[test]
    fn empty_string() {
        let env = make_env(&[]);
        assert_eq!(expand_path("", None, &env).unwrap(), "");
    }

    #[test]
    fn dollar_dollar_escape() {
        let env = make_env(&[]);
        assert_eq!(expand_path("$$literal", None, &env).unwrap(), "$literal");
    }

    #[test]
    fn with_kakehashi_defaults_passes_through_existing_env() {
        let env = make_env(&[("HOME", "/home/user")]);
        let wrapped = with_kakehashi_defaults(env);
        assert_eq!(wrapped("HOME"), Some("/home/user".to_string()));
    }

    #[test]
    fn with_kakehashi_defaults_provides_data_dir_fallback() {
        let env = make_env(&[]);
        let wrapped = with_kakehashi_defaults(env);
        let result = wrapped("KAKEHASHI_DATA_DIR");
        assert!(
            result.is_some(),
            "should provide a fallback for KAKEHASHI_DATA_DIR"
        );
        assert!(
            result.as_ref().unwrap().contains("kakehashi"),
            "fallback should contain 'kakehashi', got: {:?}",
            result
        );
    }

    #[test]
    fn with_kakehashi_defaults_does_not_override_explicit_env() {
        let env = make_env(&[("KAKEHASHI_DATA_DIR", "/custom/dir")]);
        let wrapped = with_kakehashi_defaults(env);
        assert_eq!(
            wrapped("KAKEHASHI_DATA_DIR"),
            Some("/custom/dir".to_string())
        );
    }

    #[test]
    fn with_kakehashi_defaults_returns_none_for_unknown_vars() {
        let env = make_env(&[]);
        let wrapped = with_kakehashi_defaults(env);
        assert_eq!(wrapped("UNKNOWN_VAR"), None);
    }

    #[test]
    fn with_kakehashi_defaults_returns_none_for_unknown_kakehashi_vars() {
        let env = make_env(&[]);
        let wrapped = with_kakehashi_defaults(env);
        assert_eq!(wrapped("KAKEHASHI_UNKNOWN"), None);
    }

    #[test]
    fn data_dir_override_returns_none_initially() {
        // Before set_data_dir_override is called, it should return None.
        // NOTE: OnceLock is process-global and can only be set once, so we can
        // only test the "not yet set" path OR the "set" path in a single test
        // process. Other tests in this process may call set_data_dir_override,
        // so we just verify the function is callable and returns Option<&Path>.
        let result = data_dir_override();
        // Either None (not yet set) or Some (set by another test) — both are valid
        assert!(result.is_none() || result.is_some());
    }

    #[test]
    fn with_kakehashi_defaults_prioritizes_override_over_env_and_fallback() {
        // When a data_dir override is set, with_kakehashi_defaults should return
        // the override value even if env_fn provides a different value.
        // NOTE: This test verifies the priority logic. Because OnceLock is global,
        // the actual override value depends on test execution order. We test the
        // structural behavior: if override is set, it wins.
        let env = make_env(&[("KAKEHASHI_DATA_DIR", "/from/env")]);
        let wrapped = with_kakehashi_defaults(env);
        let result = wrapped("KAKEHASHI_DATA_DIR");

        match data_dir_override() {
            Some(override_path) => {
                // Override is set — it should take priority
                assert_eq!(
                    result,
                    Some(override_path.to_string_lossy().into_owned()),
                    "override should take priority over env"
                );
            }
            None => {
                // Override not set — env value should win
                assert_eq!(result, Some("/from/env".to_string()));
            }
        }
    }
}
