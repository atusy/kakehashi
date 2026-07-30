//! Detection of deprecated configuration keys, for one-time migration warnings.
//!
//! Serde's `#[serde(alias = "...")]` collapses the deprecated and canonical
//! spellings into the same field, so the parsed [`RawWorkspaceSettings`] carries
//! no trace of which key the user actually wrote. To warn on the deprecated
//! spelling we must inspect the *raw* config value before that collapse. These
//! helpers are pure — they read a value and return whether the deprecated key is
//! present — so the once-per-session policy can live entirely at the call site.
//!
//! [`RawWorkspaceSettings`]: crate::config::RawWorkspaceSettings

use serde_json::Value as JsonValue;

/// Which deprecated config keys any loaded layer actually spelled.
///
/// Serde collapses deprecated and canonical spellings (via `alias`, or by the
/// key simply still being accepted), so the parsed settings carry no trace of
/// what the user wrote — each flag is detected from the raw config value
/// before that collapse. Kept OUT of `events` because many callers re-load
/// settings and would re-warn; the `initialize` and `didChangeConfiguration`
/// handlers instead surface each one once per session via `SettingsManager`'s
/// claim guards.
///
/// An aggregate rather than one `&mut bool` per key: every loader has to
/// thread all of them, and the count grows with each deprecation.
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DeprecatedKeysSeen {
    /// `[languageServers.*] rootMarkers`, superseded by `workspaceMarkers`.
    pub(crate) root_markers: bool,
    /// Top-level `autoInstall`, superseded by `[languages.*] autoInstall`.
    pub(crate) auto_install: bool,
}

impl DeprecatedKeysSeen {
    pub(crate) fn merge(&mut self, other: Self) {
        self.root_markers |= other.root_markers;
        self.auto_install |= other.auto_install;
    }
}

/// User-facing text for the one-per-session `rootMarkers` deprecation notice,
/// shared by every path that can surface it (initialize, didChangeConfiguration).
pub(crate) const ROOT_MARKERS_DEPRECATION_NOTICE: &str = "kakehashi: the `rootMarkers` config key is deprecated; rename it to \
     `workspaceMarkers`. `rootMarkers` still works for now but may be removed \
     in a future release.";

/// User-facing text for runtime client-pushed configuration that still uses the
/// old unwrapped/flat `workspace/didChangeConfiguration` shape.
pub(crate) const UNWRAPPED_DIDCHANGE_CONFIGURATION_NOTICE: &str = "kakehashi: unwrapped `workspace/didChangeConfiguration` settings are deprecated; \
     send runtime settings in the notification's `settings.kakehashi` object. \
     Flat didChange settings still work for now but may be removed in a future release.";

/// User-facing text for the one-per-session top-level `autoInstall` notice.
pub(crate) const AUTO_INSTALL_DEPRECATION_NOTICE: &str = "kakehashi: the top-level `autoInstall` config key is deprecated; move it to \
     `[languages._] autoInstall` (and override per language with \
     `[languages.<lang>] autoInstall`). The top-level key still works for now \
     but may be removed in a future release.";

/// Which deprecated keys the raw TOML text spells.
///
/// Returns all-false for unparseable input: this is a best-effort migration
/// nudge, and a genuine parse error is surfaced through the normal load-error
/// path.
pub(crate) fn toml_deprecated_keys(contents: &str) -> DeprecatedKeysSeen {
    let Ok(value) = toml::from_str::<toml::Value>(contents) else {
        return DeprecatedKeysSeen::default();
    };
    DeprecatedKeysSeen {
        root_markers: value
            .get("languageServers")
            .and_then(|servers| servers.as_table())
            .is_some_and(|servers| {
                servers.values().any(|config| {
                    config
                        .as_table()
                        .is_some_and(|table| table.contains_key("rootMarkers"))
                })
            }),
        // Only the TOP-LEVEL spelling is deprecated. A `[languages.*]` table
        // carries the canonical key of the same name, so scanning by name alone
        // would warn about the very config we are telling people to write.
        auto_install: value
            .as_table()
            .is_some_and(|table| table.contains_key("autoInstall")),
    }
}

/// Which deprecated keys a raw JSON value spells (the `initializationOptions`
/// and `didChangeConfiguration` override paths).
pub(crate) fn json_deprecated_keys(value: &JsonValue) -> DeprecatedKeysSeen {
    DeprecatedKeysSeen {
        root_markers: value
            .get("languageServers")
            .and_then(|servers| servers.as_object())
            .is_some_and(|servers| {
                servers.values().any(|config| {
                    config
                        .as_object()
                        .is_some_and(|object| object.contains_key("rootMarkers"))
                })
            }),
        auto_install: value
            .as_object()
            .is_some_and(|object| object.contains_key("autoInstall")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toml_detects_the_deprecated_top_level_auto_install() {
        assert!(toml_deprecated_keys("autoInstall = true").auto_install);
        assert!(toml_deprecated_keys("autoInstall = false").auto_install);
    }

    #[test]
    fn toml_does_not_flag_the_canonical_per_language_auto_install() {
        // The regression this guards: scanning by key name alone would warn
        // about the exact config the notice tells people to write.
        let canonical = r#"
[languages._]
autoInstall = true

[languages.python]
autoInstall = false
"#;
        assert!(!toml_deprecated_keys(canonical).auto_install);
    }

    #[test]
    fn toml_flags_both_keys_independently() {
        let both = r#"
autoInstall = false

[languageServers.rust-analyzer]
rootMarkers = [".git"]
"#;
        let seen = toml_deprecated_keys(both);
        assert!(seen.auto_install);
        assert!(seen.root_markers);

        let neither = toml_deprecated_keys("[languages._]\nautoInstall = true\n");
        assert!(!neither.auto_install);
        assert!(!neither.root_markers);
    }

    #[test]
    fn json_detects_the_deprecated_top_level_auto_install() {
        let deprecated = serde_json::json!({ "autoInstall": false });
        assert!(json_deprecated_keys(&deprecated).auto_install);

        let canonical = serde_json::json!({
            "languages": { "_": { "autoInstall": true } }
        });
        assert!(!json_deprecated_keys(&canonical).auto_install);
    }

    #[test]
    fn unparseable_toml_flags_nothing() {
        let seen = toml_deprecated_keys("this is not = = toml");
        assert_eq!(seen, DeprecatedKeysSeen::default());
    }

    #[test]
    fn toml_detects_deprecated_key() {
        let contents = r#"
            [languageServers.rust-analyzer]
            rootMarkers = [".git"]
        "#;
        assert!(toml_deprecated_keys(contents).root_markers);
    }

    #[test]
    fn toml_ignores_canonical_key() {
        let contents = r#"
            [languageServers.rust-analyzer]
            workspaceMarkers = [".git"]
        "#;
        assert!(!toml_deprecated_keys(contents).root_markers);
    }

    #[test]
    fn toml_ignores_absent_key_and_unparseable_input() {
        assert!(!toml_deprecated_keys("autoInstall = false").root_markers);
        assert!(!toml_deprecated_keys("this is not [valid toml").root_markers);
    }

    #[test]
    fn toml_does_not_false_positive_on_the_word_in_a_comment() {
        // A string-scan would trip on this; a structured walk does not.
        let contents = r#"
            # rootMarkers used to be the key name
            [languageServers.rust-analyzer]
            workspaceMarkers = ["rootMarkers"]
        "#;
        assert!(!toml_deprecated_keys(contents).root_markers);
    }

    #[test]
    fn json_detects_deprecated_key() {
        let value = serde_json::json!({
            "languageServers": { "rust-analyzer": { "rootMarkers": [".git"] } }
        });
        assert!(json_deprecated_keys(&value).root_markers);
    }

    #[test]
    fn json_ignores_canonical_and_absent() {
        let canonical = serde_json::json!({
            "languageServers": { "rust-analyzer": { "workspaceMarkers": [".git"] } }
        });
        assert!(!json_deprecated_keys(&canonical).root_markers);
        assert!(!json_deprecated_keys(&serde_json::json!({})).root_markers);
    }

    #[test]
    fn scope_is_limited_to_languageservers_entries() {
        // The walk is deliberately narrow: a `rootMarkers` key elsewhere (top
        // level, an unrelated table, or a server's opaque passthrough blob) must
        // not trip the deprecation warning. This pins that scope against a
        // future change that broadens the walk and starts false-warning.
        assert!(!toml_deprecated_keys("rootMarkers = [\".git\"]").root_markers);
        assert!(!toml_deprecated_keys("[other]\nrootMarkers = [\".git\"]").root_markers);
        assert!(
            !json_deprecated_keys(&serde_json::json!({
                "rootMarkers": [".git"],
                "other": { "rootMarkers": [".git"] }
            }))
            .root_markers
        );
        // A downstream server's opaque `initializationOptions` blob may itself
        // carry an unrelated `rootMarkers` key — must not be walked into.
        assert!(
            !json_deprecated_keys(&serde_json::json!({
                "languageServers": {
                    "x": { "initializationOptions": { "rootMarkers": [".git"] } }
                }
            }))
            .root_markers
        );
    }

    #[test]
    fn non_table_languageservers_is_ignored() {
        assert!(!toml_deprecated_keys("languageServers = \"oops\"").root_markers);
        assert!(
            !json_deprecated_keys(&serde_json::json!({
                "languageServers": ["oops"]
            }))
            .root_markers
        );
    }

    #[test]
    fn deprecated_key_is_flagged_regardless_of_value_shape() {
        // Empty array and a both-keys table (which serde later rejects as a
        // duplicate field) still count as "the deprecated key was written".
        assert!(toml_deprecated_keys("[languageServers.x]\nrootMarkers = []").root_markers);
        assert!(
            toml_deprecated_keys(
                "[languageServers.x]\nrootMarkers = [\".git\"]\nworkspaceMarkers = [\".git\"]"
            )
            .root_markers
        );
    }
}
