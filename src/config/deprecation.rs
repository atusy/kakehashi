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

/// User-facing text for the one-per-session `rootMarkers` deprecation notice,
/// shared by every path that can surface it (initialize, didChangeConfiguration).
pub(crate) const ROOT_MARKERS_DEPRECATION_NOTICE: &str = "kakehashi: the `rootMarkers` config key is deprecated; rename it to \
     `workspaceMarkers`. `rootMarkers` still works for now but may be removed \
     in a future release.";

/// True if any `[languageServers.*]` entry declares the deprecated `rootMarkers`
/// key (superseded by `workspaceMarkers`), scanning raw TOML text.
///
/// Returns `false` for unparseable input: this is a best-effort migration nudge,
/// and a genuine parse error is surfaced through the normal load-error path.
pub(crate) fn toml_uses_deprecated_root_markers(contents: &str) -> bool {
    toml::from_str::<toml::Value>(contents)
        .ok()
        .as_ref()
        .and_then(|value| value.get("languageServers"))
        .and_then(|servers| servers.as_table())
        .is_some_and(|servers| {
            servers.values().any(|config| {
                config
                    .as_table()
                    .is_some_and(|table| table.contains_key("rootMarkers"))
            })
        })
}

/// True if any `languageServers.*` entry declares the deprecated `rootMarkers`
/// key, scanning a raw JSON value (the `initializationOptions` override path).
pub(crate) fn json_uses_deprecated_root_markers(value: &JsonValue) -> bool {
    value
        .get("languageServers")
        .and_then(|servers| servers.as_object())
        .is_some_and(|servers| {
            servers.values().any(|config| {
                config
                    .as_object()
                    .is_some_and(|object| object.contains_key("rootMarkers"))
            })
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toml_detects_deprecated_key() {
        let contents = r#"
            [languageServers.rust-analyzer]
            rootMarkers = [".git"]
        "#;
        assert!(toml_uses_deprecated_root_markers(contents));
    }

    #[test]
    fn toml_ignores_canonical_key() {
        let contents = r#"
            [languageServers.rust-analyzer]
            workspaceMarkers = [".git"]
        "#;
        assert!(!toml_uses_deprecated_root_markers(contents));
    }

    #[test]
    fn toml_ignores_absent_key_and_unparseable_input() {
        assert!(!toml_uses_deprecated_root_markers("autoInstall = false"));
        assert!(!toml_uses_deprecated_root_markers(
            "this is not [valid toml"
        ));
    }

    #[test]
    fn toml_does_not_false_positive_on_the_word_in_a_comment() {
        // A string-scan would trip on this; a structured walk does not.
        let contents = r#"
            # rootMarkers used to be the key name
            [languageServers.rust-analyzer]
            workspaceMarkers = ["rootMarkers"]
        "#;
        assert!(!toml_uses_deprecated_root_markers(contents));
    }

    #[test]
    fn json_detects_deprecated_key() {
        let value = serde_json::json!({
            "languageServers": { "rust-analyzer": { "rootMarkers": [".git"] } }
        });
        assert!(json_uses_deprecated_root_markers(&value));
    }

    #[test]
    fn json_ignores_canonical_and_absent() {
        let canonical = serde_json::json!({
            "languageServers": { "rust-analyzer": { "workspaceMarkers": [".git"] } }
        });
        assert!(!json_uses_deprecated_root_markers(&canonical));
        assert!(!json_uses_deprecated_root_markers(&serde_json::json!({})));
    }
}
