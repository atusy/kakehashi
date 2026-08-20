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
    /// Top-level `captureMappings`, superseded by the semantic-token feature.
    pub(crate) capture_mappings: bool,
}

impl DeprecatedKeysSeen {
    pub(crate) fn merge(&mut self, other: Self) {
        self.root_markers |= other.root_markers;
        self.auto_install |= other.auto_install;
        self.capture_mappings |= other.capture_mappings;
    }
}

crate::deprecation::declare_deprecation_notice!(
    /// User-facing text for the one-per-session `rootMarkers` deprecation notice,
    /// shared by every path that can surface it (initialize, didChangeConfiguration).
    pub(crate) const ROOT_MARKERS_DEPRECATION_NOTICE;
    name = "languageServers.*.rootMarkers",
    deprecated_in = 0,
    remove_in = 2,
    message = "kakehashi: the `rootMarkers` config key is deprecated; rename it to \
         `workspaceMarkers`. `rootMarkers` still works for now but will be removed \
         in kakehashi v"
);

crate::deprecation::declare_deprecation_notice!(
    /// User-facing text for runtime client-pushed configuration that still uses the
    /// old unwrapped/flat `workspace/didChangeConfiguration` shape.
    pub(crate) const UNWRAPPED_DIDCHANGE_CONFIGURATION_NOTICE;
    name = "unwrapped workspace/didChangeConfiguration settings",
    deprecated_in = 0,
    remove_in = 2,
    message = "kakehashi: unwrapped `workspace/didChangeConfiguration` settings are deprecated; \
         send runtime settings in the notification's `settings.kakehashi` object. \
         Flat didChange settings still work for now but will be removed in kakehashi v"
);

crate::deprecation::declare_deprecation_notice!(
    /// User-facing text for the one-per-session top-level `autoInstall` notice.
    ///
    /// Dotted key paths, not TOML table syntax: this notice also fires for JSON
    /// runtime settings (`initializationOptions`, `didChangeConfiguration`), where
    /// `[languages._]` would name a shape the user cannot write.
    pub(crate) const AUTO_INSTALL_DEPRECATION_NOTICE;
    name = "top-level autoInstall",
    deprecated_in = 0,
    remove_in = 2,
    message = "kakehashi: the top-level `autoInstall` config key is deprecated; move it to \
         `languages._.autoInstall` (and override per language with \
         `languages.<lang>.autoInstall`). A language with a self-referential \
         `base` inherits nothing from `_`, so give those an explicit value. The \
         top-level key still works for now but will be removed in kakehashi v"
);

crate::deprecation::declare_deprecation_notice!(
    /// User-facing text for the one-per-session top-level `captureMappings`
    /// notice. The dotted path is usable for both TOML and JSON configuration.
    pub(crate) const CAPTURE_MAPPINGS_DEPRECATION_NOTICE;
    name = "top-level captureMappings",
    deprecated_in = 0,
    remove_in = 2,
    message = "kakehashi: the top-level `captureMappings` config key is deprecated; move highlight mappings to \
         `features.\"textDocument/semanticTokens\".captureMappings` in TOML (or \
         `features[\"textDocument/semanticTokens\"].captureMappings` in JSON) and remove the \
         intermediate `highlights` key. The top-level key still works for now but \
         will be removed in kakehashi v"
);

crate::deprecation::declare_deprecation!(
    pub(crate) const ALIASES_DEPRECATION;
    name = "languages.*.aliases",
    deprecated_in = 0,
    remove_in = 2,
);
pub(crate) fn aliases_deprecation_notice(language: &str, aliases: &[String]) -> String {
    let aliases = aliases
        .iter()
        .map(String::as_str)
        .collect::<std::collections::BTreeSet<_>>();
    if aliases.is_empty() {
        return format!(
            "Language '{language}' uses an empty deprecated 'aliases' field. \
             Remove the empty 'aliases' field. The 'aliases' field will be removed in \
             kakehashi v{}.",
            ALIASES_DEPRECATION.remove_in_major()
        );
    }
    let has_reserved_alias = aliases.contains("_");
    let has_self_alias = aliases.contains(language);
    let aliases = aliases
        .into_iter()
        .filter(|alias| *alias != "_" && *alias != language)
        .collect::<Vec<_>>();
    let special_guidance = match (has_reserved_alias, has_self_alias) {
        (true, true) => {
            "The `_` alias is reserved and must not become a base entry. \
             A self-alias needs no base entry.\n"
        }
        (true, false) => "The `_` alias is reserved and must not become a base entry.\n",
        (false, true) => "A self-alias needs no base entry.\n",
        (false, false) => "",
    };
    if aliases.is_empty() {
        return format!(
            "Language '{language}' uses deprecated 'aliases' field. Remove the 'aliases' field. \
             {special_guidance}The 'aliases' field will be removed in kakehashi v{}.",
            ALIASES_DEPRECATION.remove_in_major()
        );
    }
    let language_toml = toml::Value::String(language.to_owned()).to_string();
    let toml_examples = aliases
        .iter()
        .map(|alias| {
            let alias = toml::Value::String((*alias).to_owned()).to_string();
            format!("[languages.{alias}]\nbase = {language_toml}")
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    let mut derived_languages = serde_json::Map::new();
    for alias in aliases {
        derived_languages.insert(alias.to_owned(), serde_json::json!({ "base": language }));
    }
    let json = serde_json::json!({ "languages": derived_languages });
    format!(
        "Language '{language}' uses deprecated 'aliases' field. \
         Use 'base' on each derived language instead. Edit each existing language entry in \
         place; do not add a duplicate table or object key.\n\
         TOML:\n{toml_examples}\n\
         JSON:\n{json}\n\
         {special_guidance}The 'aliases' field will be removed in kakehashi v{}.",
        ALIASES_DEPRECATION.remove_in_major()
    )
}

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
        capture_mappings: value
            .as_table()
            .is_some_and(|table| table.contains_key("captureMappings")),
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
        capture_mappings: value
            .as_object()
            .is_some_and(|object| object.contains_key("captureMappings")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v0_deprecation_notices() -> [&'static str; 4] {
        [
            ROOT_MARKERS_DEPRECATION_NOTICE,
            UNWRAPPED_DIDCHANGE_CONFIGURATION_NOTICE,
            AUTO_INSTALL_DEPRECATION_NOTICE,
            CAPTURE_MAPPINGS_DEPRECATION_NOTICE,
        ]
    }

    #[test]
    fn merge_ors_flags_so_a_clean_later_layer_cannot_clear_them() {
        // The regression: `=` instead of `|=` would let a clean higher layer
        // (e.g. `initializationOptions: {}`, which every session runs through
        // `parse_override_settings`) clobber a flag a config FILE set, and the
        // notice would never fire for file-config users.
        let mut seen = DeprecatedKeysSeen {
            root_markers: true,
            auto_install: true,
            capture_mappings: true,
        };
        seen.merge(DeprecatedKeysSeen::default());
        assert!(seen.root_markers, "a later clean layer must not clear this");
        assert!(seen.auto_install, "a later clean layer must not clear this");
        assert!(
            seen.capture_mappings,
            "a later clean layer must not clear this"
        );

        // And it must still pick flags UP from a later layer.
        let mut none = DeprecatedKeysSeen::default();
        none.merge(DeprecatedKeysSeen {
            root_markers: false,
            auto_install: true,
            capture_mappings: true,
        });
        assert!(none.auto_install);
        assert!(none.capture_mappings);
        assert!(!none.root_markers);
    }

    #[test]
    fn toml_detects_the_deprecated_top_level_auto_install() {
        assert!(toml_deprecated_keys("autoInstall = true").auto_install);
        assert!(toml_deprecated_keys("autoInstall = false").auto_install);
    }

    #[test]
    fn toml_distinguishes_deprecated_and_canonical_capture_mappings() {
        let deprecated = toml_deprecated_keys(
            r#"
            [captureMappings._.highlights]
            variable = "variable"
            "#,
        );
        assert!(deprecated.capture_mappings);

        let canonical = toml_deprecated_keys(
            r#"
            [features."textDocument/semanticTokens".captureMappings._]
            variable = "variable"
            "#,
        );
        assert!(!canonical.capture_mappings);
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
    fn json_distinguishes_deprecated_and_canonical_capture_mappings() {
        let deprecated = serde_json::json!({
            "captureMappings": { "_": { "highlights": { "variable": "variable" } } }
        });
        assert!(json_deprecated_keys(&deprecated).capture_mappings);

        let canonical = serde_json::json!({
            "features": {
                "textDocument/semanticTokens": {
                    "captureMappings": { "_": { "variable": "variable" } }
                }
            }
        });
        assert!(!json_deprecated_keys(&canonical).capture_mappings);
    }

    #[test]
    fn capture_mapping_notice_gives_valid_toml_and_json_paths() {
        assert!(
            CAPTURE_MAPPINGS_DEPRECATION_NOTICE
                .contains("features.\"textDocument/semanticTokens\".captureMappings")
        );
        assert!(
            CAPTURE_MAPPINGS_DEPRECATION_NOTICE
                .contains("features[\"textDocument/semanticTokens\"].captureMappings")
        );
    }

    #[test]
    fn every_v0_notice_names_the_v2_removal() {
        for notice in v0_deprecation_notices() {
            assert!(
                notice.contains("removed in kakehashi v2"),
                "v0 deprecation notice must name its removal deadline: {notice}"
            );
        }
    }

    #[test]
    fn aliases_notice_gives_valid_toml_and_json_migrations_with_declared_deadline() {
        let notice = aliases_deprecation_notice("markdown", &["rmd".to_owned()]);
        let expected = format!(
            "Language 'markdown' uses deprecated 'aliases' field. Use 'base' on each derived \
             language instead. Edit each existing language entry in place; do not add a \
             duplicate table or object key.\nTOML:\n[languages.\"rmd\"]\nbase = \"markdown\"\nJSON:\n\
             {{\"languages\":{{\"rmd\":{{\"base\":\"markdown\"}}}}}}\n\
             The 'aliases' field will be removed in kakehashi v{}.",
            ALIASES_DEPRECATION.remove_in_major()
        );
        assert_eq!(notice, expected);
    }

    #[test]
    fn aliases_notice_escapes_language_ids_in_both_migration_formats() {
        let notice = aliases_deprecation_notice("mark\"down", &["r.md\"x".to_owned()]);
        let toml_example = notice
            .split_once("TOML:\n")
            .and_then(|(_, examples)| examples.split_once("\nJSON:\n"))
            .map(|(toml, _)| toml)
            .expect("TOML migration example");
        let parsed_toml: toml::Value = toml::from_str(toml_example).expect("valid TOML example");
        assert_eq!(
            parsed_toml["languages"]["r.md\"x"]["base"].as_str(),
            Some("mark\"down")
        );

        let json_example = notice
            .split_once("\nJSON:\n")
            .and_then(|(_, rest)| rest.split_once("\nThe 'aliases' field"))
            .map(|(json, _)| json)
            .expect("JSON migration example");
        let parsed_json: serde_json::Value =
            serde_json::from_str(json_example).expect("valid JSON example");
        assert_eq!(
            parsed_json["languages"]["r.md\"x"]["base"].as_str(),
            Some("mark\"down")
        );
    }

    #[test]
    fn empty_aliases_notice_only_requests_removing_the_field() {
        let notice = aliases_deprecation_notice("markdown", &[]);
        assert!(
            notice.contains("Remove the empty 'aliases' field"),
            "{notice}"
        );
        assert!(!notice.contains("<derived>"), "{notice}");
        assert!(!notice.contains("TOML:"), "{notice}");
        assert!(!notice.contains("JSON:"), "{notice}");
    }

    #[test]
    fn aliases_notice_migrates_every_distinct_alias() {
        let aliases = vec!["rmd".to_owned(), "qmd".to_owned(), "rmd".to_owned()];
        let notice = aliases_deprecation_notice("markdown", &aliases);
        assert_eq!(notice.matches("[languages.\"rmd\"]").count(), 1);
        assert_eq!(notice.matches("[languages.\"qmd\"]").count(), 1);

        let json_example = notice
            .split_once("\nJSON:\n")
            .and_then(|(_, rest)| rest.split_once("\nThe 'aliases' field"))
            .map(|(json, _)| json)
            .expect("JSON migration example");
        let parsed_json: serde_json::Value =
            serde_json::from_str(json_example).expect("valid JSON example");
        assert_eq!(
            parsed_json["languages"]["rmd"]["base"].as_str(),
            Some("markdown")
        );
        assert_eq!(
            parsed_json["languages"]["qmd"]["base"].as_str(),
            Some("markdown")
        );
    }

    #[test]
    fn aliases_notice_does_not_migrate_reserved_or_self_aliases() {
        let aliases = vec!["_".to_owned(), "markdown".to_owned(), "rmd".to_owned()];
        let notice = aliases_deprecation_notice("markdown", &aliases);
        assert!(notice.contains("[languages.\"rmd\"]"), "{notice}");
        assert!(!notice.contains("[languages.\"_\"]"), "{notice}");
        assert!(!notice.contains("[languages.\"markdown\"]"), "{notice}");
        assert!(notice.contains("`_` alias is reserved"), "{notice}");
        assert!(
            notice.contains("self-alias needs no base entry"),
            "{notice}"
        );
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
