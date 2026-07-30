pub mod defaults;
pub(crate) mod deprecation;
pub(crate) mod expand;
pub(crate) mod merge;
pub mod settings;

use std::collections::HashMap;

#[cfg(test)]
pub(crate) use expand::make_env;
pub(crate) mod user;

pub use expand::{set_config_file_override, set_data_dir_override};
pub(crate) use merge::{
    is_server_spawnable, merge_aggregation_configs, merge_bridge_language_configs,
    merge_bridge_server_configs, merge_layer_aggregation_configs, merge_workspace_settings,
    resolve_with_wildcard,
};
pub(crate) use settings::{CaptureMappings, DEFAULT_DEBOUNCE_MS, QueryTypeMappings};
pub use settings::{LanguageSettings, RawWorkspaceSettings, WorkspaceSettings, json_schema};
pub(crate) use user::load_user_config;

/// Wildcard key for default configurations in HashMap-based settings.
/// Used in capture_mappings, languages, and language_servers for fallback values.
pub(crate) const WILDCARD_KEY: &str = "_";

/// Returns the default search paths for parsers and queries.
/// Uses the platform-specific data directory (via `dirs` crate):
/// - Linux: ~/.local/share/kakehashi
/// - macOS: ~/Library/Application Support/kakehashi
/// - Windows: %APPDATA%/kakehashi
///
/// Note: Returns the base directory only. The resolver functions append
/// "parser/" or "queries/" subdirectories as needed.
fn default_search_paths() -> Vec<String> {
    crate::install::default_data_dir()
        .map(|d| vec![d.to_string_lossy().to_string()])
        .unwrap_or_default()
}

/// Convert `RawWorkspaceSettings` to `WorkspaceSettings` without expanding
/// environment variables or tilde. This is the base conversion used
/// internally by `try_from_settings`.
fn base_convert(settings: &RawWorkspaceSettings) -> WorkspaceSettings {
    let languages = settings.languages.clone();
    let capture_mappings = settings
        .capture_mappings
        .iter()
        .map(|(lang, mappings)| {
            (
                lang.clone(),
                QueryTypeMappings {
                    highlights: mappings.highlights.clone(),
                    folds: mappings.folds.clone(),
                },
            )
        })
        .collect();

    // Use explicit search_paths if provided, otherwise use platform defaults
    let search_paths = settings
        .search_paths
        .clone()
        .unwrap_or_else(default_search_paths);

    WorkspaceSettings {
        search_paths,
        languages,
        capture_mappings,
        auto_install: settings.auto_install.unwrap_or(true),
        diagnostics_debounce_ms: settings
            .diagnostics_debounce_ms
            .unwrap_or(DEFAULT_DEBOUNCE_MS),
        features: settings.features.as_ref().map_or_else(
            settings::ResolvedFeatureSettings::default,
            settings::FeatureSettings::resolve,
        ),
        language_servers: settings.language_servers.clone().unwrap_or_default(),
    }
}

fn strip_inherited_languages(
    languages: &HashMap<String, LanguageSettings>,
) -> HashMap<String, LanguageSettings> {
    languages
        .iter()
        .map(|(name, language)| {
            let inherited = inherited_language_settings(languages, name, language);

            let stripped = match inherited {
                Some(base) => strip_inherited_language_settings(base, language),
                None => language.clone(),
            };

            (name.clone(), stripped)
        })
        .collect()
}

fn inherited_language_settings<'a>(
    languages: &'a HashMap<String, LanguageSettings>,
    name: &str,
    language: &LanguageSettings,
) -> Option<&'a LanguageSettings> {
    if language.base.as_deref() == Some(name) {
        return None;
    }

    language
        .base
        .as_deref()
        .and_then(|base| languages.get(base))
        .or_else(|| {
            (name != WILDCARD_KEY)
                .then(|| languages.get(WILDCARD_KEY))
                .flatten()
        })
}

/// Drop fields whose value equals the one they would inherit, so a
/// reconstructed raw config shows only genuine overrides.
///
/// Not behavior-preserving for a CIRCULAR `base` chain: both nodes resolve to
/// the same folded value, so each looks inherited from the other and the
/// cycle's single explicit value is stripped from both — re-resolving then
/// falls through to the top-level default. Unreachable today (every production
/// path carries the original raw settings; only the `None` arm in
/// `apply_shared_settings` would land here, and no production caller passes
/// `None`), and fixing it properly needs cycle-aware stripping. Recorded so a
/// future caller of that arm knows what it inherits.
fn strip_inherited_language_settings(
    inherited: &LanguageSettings,
    current: &LanguageSettings,
) -> LanguageSettings {
    LanguageSettings {
        base: current.base.clone(),
        parser: (current.parser != inherited.parser)
            .then(|| current.parser.clone())
            .flatten(),
        queries: (current.queries != inherited.queries)
            .then(|| current.queries.clone())
            .flatten(),
        bridge: strip_inherited_bridge_map(inherited.bridge.as_ref(), current.bridge.as_ref()),
        // Whole-field equality strip (like queries/aliases): a layers map
        // that differs from the inherited one at all is kept verbatim. The
        // per-key deep strip used for bridge is not mirrored here until the
        // display duplication it avoids proves to matter for layers.
        layers: (current.layers != inherited.layers)
            .then(|| current.layers.clone())
            .flatten(),
        aliases: (current.aliases != inherited.aliases)
            .then(|| current.aliases.clone())
            .flatten(),
        auto_install: (current.auto_install != inherited.auto_install)
            .then_some(current.auto_install)
            .flatten(),
    }
}

fn strip_inherited_bridge_map(
    inherited: Option<&HashMap<String, settings::BridgeLanguageConfig>>,
    current: Option<&HashMap<String, settings::BridgeLanguageConfig>>,
) -> Option<HashMap<String, settings::BridgeLanguageConfig>> {
    let current = current?;

    if current.is_empty() {
        return Some(current.clone());
    }

    let mut stripped = HashMap::new();
    for (name, current_config) in current {
        let inherited_config = inherited.and_then(|base| {
            merge::resolve_with_wildcard(base, name, merge::merge_bridge_language_configs)
        });

        let stripped_config = match inherited_config {
            Some(base) => strip_inherited_bridge_language_config(&base, current_config),
            None => current_config.clone(),
        };

        if stripped_config != settings::BridgeLanguageConfig::default() {
            stripped.insert(name.clone(), stripped_config);
        }
    }

    (!stripped.is_empty()).then_some(stripped)
}

fn strip_inherited_bridge_language_config(
    inherited: &settings::BridgeLanguageConfig,
    current: &settings::BridgeLanguageConfig,
) -> settings::BridgeLanguageConfig {
    settings::BridgeLanguageConfig {
        enabled: (current.enabled != inherited.enabled)
            .then_some(current.enabled)
            .flatten(),
        aggregation: strip_inherited_aggregation_map(
            inherited.aggregation.as_ref(),
            current.aggregation.as_ref(),
        ),
    }
}

fn strip_inherited_aggregation_map(
    inherited: Option<&HashMap<String, settings::AggregationConfig>>,
    current: Option<&HashMap<String, settings::AggregationConfig>>,
) -> Option<HashMap<String, settings::AggregationConfig>> {
    let current = current?;

    if current.is_empty() {
        return Some(current.clone());
    }

    let mut stripped = HashMap::new();

    for (method, current_config) in current {
        let inherited_config = inherited.and_then(|base| {
            merge::resolve_with_wildcard(base, method, merge::merge_aggregation_configs)
        });

        let stripped_config = match inherited_config {
            Some(base) => strip_inherited_aggregation_config(&base, current_config),
            None => current_config.clone(),
        };

        if stripped_config != settings::AggregationConfig::default() {
            stripped.insert(method.clone(), stripped_config);
        }
    }

    (!stripped.is_empty()).then_some(stripped)
}

fn strip_inherited_aggregation_config(
    inherited: &settings::AggregationConfig,
    current: &settings::AggregationConfig,
) -> settings::AggregationConfig {
    settings::AggregationConfig {
        priorities: (current.priorities != inherited.priorities)
            .then(|| current.priorities.clone())
            .flatten(),
        strategy: (current.strategy != inherited.strategy)
            .then_some(current.strategy)
            .flatten(),
        max_fan_out: (current.max_fan_out != inherited.max_fan_out)
            .then_some(current.max_fan_out)
            .flatten(),
        pull_fallback: (current.pull_fallback != inherited.pull_fallback)
            .then_some(current.pull_fallback)
            .flatten(),
        push_fallback: (current.push_fallback != inherited.push_fallback)
            .then_some(current.push_fallback)
            .flatten(),
    }
}

/// Which configuration level decided `autoInstall` for a language.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AutoInstallSource {
    /// `[languages.<lang>] autoInstall`
    Language,
    /// `[languages._] autoInstall`
    Wildcard,
    /// The deprecated top-level `autoInstall`.
    TopLevel,
}

impl WorkspaceSettings {
    /// Convert `RawWorkspaceSettings` to `WorkspaceSettings`, expanding environment
    /// variables (`$VAR`, `${VAR}`) and tilde (`~`) in path fields.
    ///
    /// Path fields expanded: `search_paths`, `languages[*].parser`, `languages[*].queries[*].path`.
    ///
    /// `home` is the pre-computed home directory (from `dirs::home_dir()`),
    /// passed in so the caller computes it once and tests can inject `None`.
    ///
    /// Uses `base_convert` for the structural conversion, then expands only the
    /// path fields. This avoids duplicating conversion logic.
    pub fn try_from_settings(
        settings: &RawWorkspaceSettings,
        home: Option<&str>,
        env_fn: impl Fn(&str) -> Option<String>,
    ) -> Result<Self, expand::ExpandErrors> {
        let mut ws = base_convert(settings);
        let mut errors = Vec::new();

        let publish = ws.features.text_document_publish_diagnostics;
        if publish.max_wait_ms == 0
            || publish.debounce_ms > settings::MAX_FEATURE_TIMING_MS
            || publish.max_wait_ms > settings::MAX_FEATURE_TIMING_MS
            || publish.max_wait_ms < publish.debounce_ms
        {
            errors.push(expand::ExpandError::InvalidSetting {
                message: format!(
                    "features.\"textDocument/publishDiagnostics\" requires 0 <= debounceMs <= maxWaitMs <= {} and maxWaitMs > 0 (got debounceMs={}, maxWaitMs={})",
                    settings::MAX_FEATURE_TIMING_MS, publish.debounce_ms, publish.max_wait_ms
                ),
            });
        }

        let refresh = ws.features.workspace_diagnostic_refresh;
        if refresh.max_wait_ms == 0
            || refresh.debounce_ms > settings::MAX_FEATURE_TIMING_MS
            || refresh.max_wait_ms > settings::MAX_FEATURE_TIMING_MS
            || refresh.max_wait_ms < refresh.debounce_ms
        {
            errors.push(expand::ExpandError::InvalidSetting {
                message: format!(
                    "features.\"workspace/diagnostic/refresh\" requires 0 <= debounceMs <= maxWaitMs <= {} and maxWaitMs > 0 (got debounceMs={}, maxWaitMs={})",
                    settings::MAX_FEATURE_TIMING_MS, refresh.debounce_ms, refresh.max_wait_ms
                ),
            });
        }

        // Resolve base configs first so expansion only sees effective parser/query paths.
        ws.languages = merge::resolve_base_configs(&ws.languages);

        for p in &mut ws.search_paths {
            match expand::expand_path(p, home, &env_fn) {
                Ok(expanded) => *p = expanded,
                Err(e) => errors.push(e),
            }
        }

        // Sort keys for deterministic error reporting (HashMap iteration is unordered)
        let mut lang_names: Vec<_> = ws.languages.keys().cloned().collect();
        lang_names.sort();
        for name in lang_names {
            let Some(lang) = ws.languages.get_mut(&name) else {
                continue;
            };
            if let Some(parser) = lang.parser.as_mut() {
                match expand::expand_path(parser, home, &env_fn) {
                    Ok(expanded) => *parser = expanded,
                    Err(e) => errors.push(e),
                }
            }
            if let Some(queries) = lang.queries.as_mut() {
                for q in queries.iter_mut() {
                    match expand::expand_path(&q.path, home, &env_fn) {
                        Ok(expanded) => q.path = expanded,
                        Err(e) => errors.push(e),
                    }
                }
            }
        }

        if errors.is_empty() {
            Ok(ws)
        } else {
            Err(expand::ExpandErrors(errors))
        }
    }

    /// Look up language settings for a host language, falling back to the
    /// wildcard (`"_"`) entry when the host has no explicit configuration.
    pub(crate) fn resolve_host_language_settings(
        &self,
        host_language: &str,
    ) -> Option<&LanguageSettings> {
        self.languages
            .get(host_language)
            .or_else(|| self.languages.get(WILDCARD_KEY))
    }

    /// Whether missing parsers/queries for `language` may be auto-installed.
    ///
    /// Per-ENTRY fallback, not per-key: the language's resolved entry answers if
    /// it has a value, else the `"_"` entry, else the deprecated top-level
    /// `autoInstall` (which itself defaults to enabled). Because
    /// [`merge::resolve_base_configs`] has already folded `"_"` and the whole
    /// `base` chain into every present entry, "the entry has no value" means
    /// nothing in that chain set one — including a language that deliberately
    /// escaped wildcard inheritance with a self-referential `base`. See
    /// [`Self::auto_install_decision`] for why that is the right reading.
    pub(crate) fn auto_install_for(&self, language: &str) -> bool {
        self.auto_install_decision(language).1
    }

    /// Why auto-install is OFF for `language`, or `None` when it is enabled —
    /// so a user-facing message can point at config rather than just saying
    /// "disabled".
    ///
    /// Deliberately vague on the language arm. `resolve_base_configs` copies
    /// the whole `base` chain (and `"_"`) into every present entry, so a folded
    /// `Some(false)` does not tell us whether the user wrote it on the language,
    /// on its base, or on the wildcard. Naming `languages.<lang>.autoInstall`
    /// outright would misdirect someone hunting for the global switch; the
    /// phrasing here names the key that overrides it, which is true either way.
    ///
    /// Formats lazily: [`Self::auto_install_for`] shares the same decision
    /// without allocating, since the gate runs per `didOpen` and per injected
    /// language while this runs only on the disabled path.
    pub(crate) fn auto_install_disabled_reason(&self, language: &str) -> Option<String> {
        let (source, enabled) = self.auto_install_decision(language);
        if enabled {
            return None;
        }
        Some(match source {
            AutoInstallSource::Language => format!(
                "`languages.{language}.autoInstall` resolves to false (set on \
                 the language, its `base` chain, or `languages._`)"
            ),
            AutoInstallSource::Wildcard => {
                format!("`languages.{WILDCARD_KEY}.autoInstall` is false")
            }
            AutoInstallSource::TopLevel => "`autoInstall` is false".to_string(),
        })
    }

    /// Which level answers auto-install for `language`, and what it says.
    /// Single source of the precedence order so the boolean and the message
    /// can never disagree.
    ///
    /// A PRESENT entry owns the answer outright: `resolve_base_configs` has
    /// already folded `"_"` and the `base` chain into it, so `None` there means
    /// nothing in the chain set the key — including the case where the language
    /// deliberately escaped wildcard inheritance with a self-referential `base`
    /// (`merge::build_base_chain`), where re-consulting `"_"` here would apply a
    /// wildcard value every other field on that language ignores.
    ///
    /// The `"_"` lookup is for languages with NO entry: the fold maps over
    /// `languages.keys()`, so it never ran for them.
    fn auto_install_decision(&self, language: &str) -> (AutoInstallSource, bool) {
        let resolved = match self.languages.get(language) {
            Some(settings) => settings
                .auto_install
                .map(|enabled| (AutoInstallSource::Language, enabled)),
            None => self
                .languages
                .get(WILDCARD_KEY)
                .and_then(|wildcard| wildcard.auto_install)
                .map(|enabled| (AutoInstallSource::Wildcard, enabled)),
        };
        resolved.unwrap_or((AutoInstallSource::TopLevel, self.auto_install))
    }

    /// True if any configured language opts into host bridging
    /// (`bridge._self.enabled = true`), including the `"_"` wildcard entry —
    /// an unconfigured document falls back to it wholesale via
    /// [`Self::resolve_host_language_settings`], so a wildcard opt-in really
    /// does enable host forwarding for those documents.
    ///
    /// Gates the willSave/willSaveWaitUntil capabilities at initialize (#357):
    /// those methods forward only to host-bridge servers, so advertising them
    /// when no language enables host bridging would make every save block on a
    /// no-op round trip that can only ever return "no edits".
    pub(crate) fn any_host_bridging_enabled(&self) -> bool {
        self.languages
            .values()
            .any(LanguageSettings::is_host_bridging_enabled)
    }

    /// True if any configured language server has a runnable command AND is
    /// enabled. The built-in `_` wildcard defaults entry carries an empty
    /// `cmd` and is thus excluded, so this is false on a blank config but
    /// true once a real server (host- or virt-capable) is configured.
    ///
    /// Gates willSave advertisement (#357): willSave now fans out to both host
    /// and virt bridges, so a single runnable bridge server is a potential
    /// consumer — but a config with only the empty defaults entry has none.
    /// A server disabled via `enabled: false` (directly or inherited from the
    /// wildcard) is likewise not a consumer: it never spawns, so counting it
    /// would advertise willSave for a save that can only block on a no-op.
    pub(crate) fn any_bridge_server_runnable(&self) -> bool {
        let wildcard = self.language_servers.get(WILDCARD_KEY);
        self.language_servers
            .iter()
            .filter(|(name, _)| name.as_str() != WILDCARD_KEY)
            .any(|(_, server)| server.is_spawnable_with_wildcard(wildcard))
    }
}

impl From<&WorkspaceSettings> for RawWorkspaceSettings {
    fn from(settings: &WorkspaceSettings) -> Self {
        let languages = strip_inherited_languages(&settings.languages);
        let capture_mappings = settings
            .capture_mappings
            .iter()
            .map(|(lang, mappings)| {
                (
                    lang.clone(),
                    QueryTypeMappings {
                        highlights: mappings.highlights.clone(),
                        folds: mappings.folds.clone(),
                    },
                )
            })
            .collect();

        let search_paths = Some(settings.search_paths.clone());

        RawWorkspaceSettings {
            search_paths,
            languages,
            capture_mappings,
            auto_install: Some(settings.auto_install),
            diagnostics_debounce_ms: Some(settings.diagnostics_debounce_ms),
            features: Some(settings::FeatureSettings {
                text_document_publish_diagnostics: Some(settings::DebounceFeatureSettings {
                    debounce_ms: Some(
                        settings
                            .features
                            .text_document_publish_diagnostics
                            .debounce_ms,
                    ),
                    max_wait_ms: Some(
                        settings
                            .features
                            .text_document_publish_diagnostics
                            .max_wait_ms,
                    ),
                }),
                window_log_message: Some(settings::LogMessageFeatureSettings {
                    log_level: Some(settings.features.window_log_message),
                }),
                workspace_diagnostic_refresh: Some(settings::DebounceFeatureSettings {
                    debounce_ms: Some(settings.features.workspace_diagnostic_refresh.debounce_ms),
                    max_wait_ms: Some(settings.features.workspace_diagnostic_refresh.max_wait_ms),
                }),
            }),
            language_servers: Some(settings.language_servers.clone()),
        }
    }
}

impl From<WorkspaceSettings> for RawWorkspaceSettings {
    fn from(settings: WorkspaceSettings) -> Self {
        RawWorkspaceSettings::from(&settings)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    /// Build a [`WorkspaceSettings`] whose `languages` map binds `key` to a
    /// language whose host bridging is `enabled`.
    fn settings_with_host_bridge(key: &str, enabled: bool) -> WorkspaceSettings {
        use crate::config::settings::{BridgeLanguageConfig, HOST_BRIDGE_KEY};
        let lang = LanguageSettings {
            bridge: Some(HashMap::from([(
                HOST_BRIDGE_KEY.to_string(),
                BridgeLanguageConfig {
                    enabled: Some(enabled),
                    aggregation: None,
                },
            )])),
            ..Default::default()
        };
        WorkspaceSettings {
            languages: HashMap::from([(key.to_string(), lang)]),
            ..Default::default()
        }
    }

    /// Build a [`WorkspaceSettings`] with the given per-language `autoInstall`
    /// values and top-level (deprecated) value, WITHOUT running
    /// `resolve_base_configs` — so each case exercises the lookup-time
    /// precedence in isolation from the base-chain fold.
    fn settings_with_auto_install(
        entries: &[(&str, Option<bool>)],
        top_level: bool,
    ) -> WorkspaceSettings {
        WorkspaceSettings {
            languages: entries
                .iter()
                .map(|(key, auto_install)| {
                    (
                        key.to_string(),
                        LanguageSettings {
                            auto_install: *auto_install,
                            ..Default::default()
                        },
                    )
                })
                .collect(),
            auto_install: top_level,
            ..Default::default()
        }
    }

    #[test]
    fn stripping_keeps_an_auto_install_that_differs_from_the_inherited_value() {
        use crate::config::settings::LanguageSettings;
        let inherited = LanguageSettings {
            auto_install: Some(true),
            ..Default::default()
        };
        // Differs → kept, so the effective-config dump shows the override.
        let differing = LanguageSettings {
            auto_install: Some(false),
            ..Default::default()
        };
        assert_eq!(
            strip_inherited_language_settings(&inherited, &differing).auto_install,
            Some(false)
        );
        // Equal → stripped as redundant, like every neighbouring field.
        assert_eq!(
            strip_inherited_language_settings(&inherited, &inherited).auto_install,
            None
        );
    }

    #[test]
    fn auto_install_for_honors_the_language_entry() {
        let settings = settings_with_auto_install(&[("python", Some(false))], true);
        assert!(!settings.auto_install_for("python"));
        // A sibling language is unaffected by python's exception.
        assert!(settings.auto_install_for("rust"));
    }

    #[test]
    fn auto_install_for_lets_a_present_entry_own_the_answer() {
        // A present entry has already been through `resolve_base_configs`, which
        // folds `_` and the whole `base` chain into it. `None` there therefore
        // means nothing in the chain set the key — notably including a language
        // that escaped wildcard inheritance via a self-referential `base`, where
        // re-applying `_` would contradict every other field on that language.
        let settings =
            settings_with_auto_install(&[(WILDCARD_KEY, Some(false)), ("python", None)], true);
        assert!(
            settings.auto_install_for("python"),
            "an entry the fold left unset must not pick `_` up again at lookup time"
        );
    }

    #[test]
    fn auto_install_for_falls_back_to_the_wildcard_with_no_language_entry() {
        // The case the base-chain fold does NOT cover: no entry for the
        // language at all, so only a lookup-time `_` fallback can answer.
        let settings = settings_with_auto_install(&[(WILDCARD_KEY, Some(false))], true);
        assert!(!settings.auto_install_for("python"));
    }

    #[test]
    fn auto_install_for_falls_back_to_the_deprecated_top_level_key() {
        let settings = settings_with_auto_install(&[], false);
        assert!(!settings.auto_install_for("python"));
    }

    #[test]
    fn auto_install_for_defaults_to_enabled() {
        // Nothing configured anywhere: the zero-config default.
        assert!(WorkspaceSettings::default().auto_install_for("python"));
    }

    #[test]
    fn auto_install_for_lets_a_language_opt_back_in_over_a_global_off() {
        // The point of the feature: `autoInstall = false` globally, with a
        // per-language exception that re-enables it.
        let settings = settings_with_auto_install(&[("python", Some(true))], false);
        assert!(settings.auto_install_for("python"));
        assert!(!settings.auto_install_for("rust"));
    }

    #[test]
    fn auto_install_for_prefers_the_language_entry_over_the_wildcard() {
        let settings = settings_with_auto_install(
            &[(WILDCARD_KEY, Some(false)), ("python", Some(true))],
            false,
        );
        assert!(settings.auto_install_for("python"));
        assert!(!settings.auto_install_for("rust"));
    }

    #[test]
    fn auto_install_precedence_holds_through_the_real_merge() {
        // The synthetic `settings_with_auto_install` fixtures construct an
        // ALREADY-resolved map, so they cannot catch a reversed
        // `merge_language_settings` arm (`base.or(overlay)`): the wildcard's
        // value would win in production while those tests stayed green. This
        // runs the real fold with every level in conflict.
        let raw = crate::config::RawWorkspaceSettings {
            auto_install: Some(true),
            languages: HashMap::from([
                (
                    WILDCARD_KEY.to_string(),
                    LanguageSettings {
                        auto_install: Some(false),
                        ..Default::default()
                    },
                ),
                (
                    "python".to_string(),
                    LanguageSettings {
                        auto_install: Some(true),
                        ..Default::default()
                    },
                ),
                (
                    "markdown".to_string(),
                    LanguageSettings {
                        auto_install: Some(true),
                        ..Default::default()
                    },
                ),
                (
                    "rmd".to_string(),
                    LanguageSettings {
                        base: Some("markdown".to_string()),
                        auto_install: Some(false),
                        ..Default::default()
                    },
                ),
                ("lua".to_string(), LanguageSettings::default()),
            ]),
            ..Default::default()
        };
        let settings =
            WorkspaceSettings::try_from_settings(&raw, None, |_| None).expect("settings expand");

        // Own value beats the wildcard...
        assert!(settings.auto_install_for("python"));
        // ...and beats an inherited base value.
        assert!(!settings.auto_install_for("rmd"));
        // An entry with nothing of its own takes the wildcard through the fold.
        assert!(!settings.auto_install_for("lua"));
        // No entry at all: the wildcard answers at lookup time.
        assert!(!settings.auto_install_for("unconfigured"));
    }

    #[test]
    fn auto_install_inherits_through_the_base_chain() {
        // Discriminating on purpose: only `merge_language_settings`'s
        // `auto_install` arm can carry markdown's value onto `rmd`. Without it
        // `rmd`'s entry stays unset and, since a present entry owns the answer,
        // resolution falls straight through to the top-level default.
        let raw = crate::config::RawWorkspaceSettings {
            languages: HashMap::from([
                (
                    "markdown".to_string(),
                    LanguageSettings {
                        auto_install: Some(false),
                        ..Default::default()
                    },
                ),
                (
                    "rmd".to_string(),
                    LanguageSettings {
                        base: Some("markdown".to_string()),
                        ..Default::default()
                    },
                ),
            ]),
            ..Default::default()
        };
        let settings =
            WorkspaceSettings::try_from_settings(&raw, None, |_| None).expect("settings expand");

        assert!(
            !settings.auto_install_for("rmd"),
            "rmd must inherit markdown's autoInstall through the base chain"
        );
    }

    #[test]
    fn auto_install_respects_a_self_referential_base_through_the_real_fold() {
        // Built through `try_from_settings` so `resolve_base_configs` actually
        // runs: `base = "foo"` is the supported escape from wildcard
        // inheritance, and `autoInstall` must honor it like every other field.
        let raw = crate::config::RawWorkspaceSettings {
            languages: HashMap::from([
                (
                    WILDCARD_KEY.to_string(),
                    LanguageSettings {
                        auto_install: Some(false),
                        ..Default::default()
                    },
                ),
                (
                    "foo".to_string(),
                    LanguageSettings {
                        base: Some("foo".to_string()),
                        ..Default::default()
                    },
                ),
                ("bar".to_string(), LanguageSettings::default()),
            ]),
            ..Default::default()
        };
        let settings =
            WorkspaceSettings::try_from_settings(&raw, None, |_| None).expect("settings expand");

        assert!(
            settings.auto_install_for("foo"),
            "a self-referential base opts out of wildcard inheritance entirely"
        );
        assert!(
            !settings.auto_install_for("bar"),
            "an ordinary entry still inherits the wildcard through the fold"
        );
        assert!(
            !settings.auto_install_for("unconfigured"),
            "a language with no entry falls back to the wildcard at lookup time"
        );
    }

    #[test]
    fn auto_install_disabled_reason_points_at_the_config() {
        // Language arm: the fold makes "who set it" unknowable, so the message
        // names the overriding key and says where it may have come from rather
        // than asserting the user wrote it on the language.
        let language = settings_with_auto_install(&[("python", Some(false))], true);
        // Exact, not `contains`: this string reaches the client verbatim inside
        // `notify_parser_missing`'s sentence, and a `contains` triple let a
        // line-continuation slip that shipped 18 literal spaces mid-message.
        assert_eq!(
            language.auto_install_disabled_reason("python").as_deref(),
            Some(
                "`languages.python.autoInstall` resolves to false \
                 (set on the language, its `base` chain, or `languages._`)"
            )
        );

        // Wildcard and top-level arms ARE reliable, so they name the key flatly.
        let wildcard = settings_with_auto_install(&[(WILDCARD_KEY, Some(false))], true);
        assert_eq!(
            wildcard.auto_install_disabled_reason("python").as_deref(),
            Some("`languages._.autoInstall` is false")
        );

        let top_level = settings_with_auto_install(&[], false);
        assert_eq!(
            top_level.auto_install_disabled_reason("python").as_deref(),
            Some("`autoInstall` is false")
        );

        // Discriminates the ORDER, not just each arm in isolation: with the
        // arms swapped, the wildcard's `false` would win and this would report
        // a reason instead of `None`.
        let overridden = settings_with_auto_install(
            &[(WILDCARD_KEY, Some(false)), ("python", Some(true))],
            true,
        );
        assert_eq!(overridden.auto_install_disabled_reason("python"), None);
    }

    #[test]
    fn auto_install_disabled_reason_is_none_when_enabled() {
        assert_eq!(
            WorkspaceSettings::default().auto_install_disabled_reason("python"),
            None
        );
        // Re-enabled per-language over a global off: nothing is disabling it,
        // so there is nothing to report even though the top level says false.
        let exception = settings_with_auto_install(&[("python", Some(true))], false);
        assert_eq!(exception.auto_install_disabled_reason("python"), None);
        assert_eq!(
            exception.auto_install_disabled_reason("rust").as_deref(),
            Some("`autoInstall` is false")
        );
    }

    #[test]
    fn any_host_bridging_enabled_is_false_for_default_settings() {
        assert!(!WorkspaceSettings::default().any_host_bridging_enabled());
    }

    #[test]
    fn any_host_bridging_enabled_true_for_explicit_language() {
        let settings = settings_with_host_bridge("markdown", true);
        assert!(settings.any_host_bridging_enabled());
    }

    #[test]
    fn any_host_bridging_enabled_false_when_explicitly_disabled() {
        let settings = settings_with_host_bridge("markdown", false);
        assert!(!settings.any_host_bridging_enabled());
    }

    #[test]
    fn any_host_bridging_enabled_true_for_wildcard_language() {
        // An unconfigured document falls back wholesale to the `"_"` entry via
        // `resolve_host_language_settings`, so a wildcard opt-in must count.
        let settings = settings_with_host_bridge(WILDCARD_KEY, true);
        assert!(settings.any_host_bridging_enabled());
    }

    #[test]
    fn any_bridge_server_runnable_excludes_empty_cmd_defaults() {
        use crate::config::settings::BridgeServerConfig;

        let server = |cmd: Vec<&str>| BridgeServerConfig {
            cmd: cmd.into_iter().map(String::from).collect(),
            languages: vec![],
            initialization_options: None,
            workspace_markers: None,
            on_type_formatting_triggers: None,
            prefer_shared_instance: None,
            enabled: None,
            settings: None,
        };

        // Only the built-in `_` defaults entry (empty cmd): not runnable.
        let settings = WorkspaceSettings {
            language_servers: HashMap::from([(WILDCARD_KEY.to_string(), server(vec![]))]),
            ..Default::default()
        };
        assert!(
            !settings.any_bridge_server_runnable(),
            "only the empty defaults entry → no runnable server"
        );

        // A real server with a command counts.
        let settings = WorkspaceSettings {
            language_servers: HashMap::from([
                (WILDCARD_KEY.to_string(), server(vec![])),
                ("lua_ls".to_string(), server(vec!["lua-language-server"])),
            ]),
            ..Default::default()
        };
        assert!(settings.any_bridge_server_runnable());
    }

    #[test]
    fn any_bridge_server_runnable_excludes_disabled_server() {
        use crate::config::settings::BridgeServerConfig;

        let server = |cmd: Vec<&str>, enabled: Option<bool>| BridgeServerConfig {
            cmd: cmd.into_iter().map(String::from).collect(),
            languages: vec![],
            initialization_options: None,
            workspace_markers: None,
            on_type_formatting_triggers: None,
            prefer_shared_instance: None,
            enabled,
            settings: None,
        };

        // Directly disabled: not a willSave consumer even with a real cmd.
        let settings = WorkspaceSettings {
            language_servers: HashMap::from([(
                "lua_ls".to_string(),
                server(vec!["lua-language-server"], Some(false)),
            )]),
            ..Default::default()
        };
        assert!(
            !settings.any_bridge_server_runnable(),
            "a disabled server never spawns, so it must not count as runnable"
        );

        // Disabled via the wildcard's inherited default.
        let settings = WorkspaceSettings {
            language_servers: HashMap::from([
                (WILDCARD_KEY.to_string(), server(vec![], Some(false))),
                (
                    "lua_ls".to_string(),
                    server(vec!["lua-language-server"], None),
                ),
            ]),
            ..Default::default()
        };
        assert!(
            !settings.any_bridge_server_runnable(),
            "a server disabled via the wildcard default must not count as runnable"
        );

        // A server can opt back in over a disabled wildcard.
        let settings = WorkspaceSettings {
            language_servers: HashMap::from([
                (WILDCARD_KEY.to_string(), server(vec![], Some(false))),
                (
                    "lua_ls".to_string(),
                    server(vec!["lua-language-server"], Some(true)),
                ),
            ]),
            ..Default::default()
        };
        assert!(settings.any_bridge_server_runnable());
    }

    #[test]
    fn test_capture_mapping_handles_at_prefix() {
        // Create capture mappings with "@" prefix
        let mut capture_mappings = CaptureMappings::new();

        let mut highlights = HashMap::new();
        highlights.insert("@module".to_string(), "@namespace".to_string());
        highlights.insert(
            "@module.builtin".to_string(),
            "@namespace.defaultLibrary".to_string(),
        );

        let query_type_mappings = QueryTypeMappings {
            highlights,
            folds: HashMap::new(),
        };

        capture_mappings.insert(WILDCARD_KEY.to_string(), query_type_mappings);

        // Verify the mapping exists and contains expected values
        assert!(capture_mappings.contains_key(WILDCARD_KEY));
        let wildcard_mappings = capture_mappings.get(WILDCARD_KEY).unwrap();
        assert_eq!(
            wildcard_mappings.highlights.get("@module"),
            Some(&"@namespace".to_string())
        );
        assert_eq!(
            wildcard_mappings.highlights.get("@module.builtin"),
            Some(&"@namespace.defaultLibrary".to_string())
        );
    }

    #[test]
    fn test_default_search_paths_used_when_none_configured() {
        // When search_paths is None in RawWorkspaceSettings, WorkspaceSettings
        // should use the default data directory paths (not an empty vector)
        let settings = RawWorkspaceSettings {
            search_paths: None,
            languages: HashMap::new(),
            capture_mappings: HashMap::new(),
            auto_install: None,
            diagnostics_debounce_ms: None,
            features: None,
            language_servers: None,
        };

        let workspace: WorkspaceSettings = base_convert(&settings);

        // Default paths should be populated (not empty)
        assert!(
            !workspace.search_paths.is_empty(),
            "search_paths should contain default data directory paths when not configured"
        );

        // Should contain parser and queries subdirectories
        let paths_str = workspace.search_paths.join("|");
        assert!(
            paths_str.contains("kakehashi"),
            "Default paths should include kakehashi directory: {:?}",
            workspace.search_paths
        );
    }

    #[test]
    fn test_explicit_search_paths_override_default() {
        // When search_paths is explicitly set, it should be used as-is
        let settings = RawWorkspaceSettings {
            search_paths: Some(vec!["/custom/path".to_string()]),
            languages: HashMap::new(),
            capture_mappings: HashMap::new(),
            auto_install: None,
            diagnostics_debounce_ms: None,
            features: None,
            language_servers: None,
        };

        let workspace: WorkspaceSettings = base_convert(&settings);

        // Should use explicit paths, not default
        assert_eq!(workspace.search_paths, vec!["/custom/path".to_string()]);
    }

    #[test]
    fn test_search_paths_can_include_default() {
        // Users can extend default paths by including them explicitly
        let default_paths = default_search_paths();
        let mut paths = vec!["/custom/path".to_string()];
        paths.extend(default_paths.clone());

        let settings = RawWorkspaceSettings {
            search_paths: Some(paths.clone()),
            languages: HashMap::new(),
            capture_mappings: HashMap::new(),
            auto_install: None,
            diagnostics_debounce_ms: None,
            features: None,
            language_servers: None,
        };

        let workspace: WorkspaceSettings = base_convert(&settings);

        // Should use the combined paths
        assert_eq!(workspace.search_paths.len(), 2); // 1 custom + 1 default (base dir only)
        assert_eq!(workspace.search_paths[0], "/custom/path");
        // Default paths follow
        for (i, default_path) in default_paths.iter().enumerate() {
            assert_eq!(&workspace.search_paths[i + 1], default_path);
        }
    }

    #[rstest]
    #[case::default_true(None, true)]
    #[case::explicit_true(Some(true), true)]
    #[case::explicit_false(Some(false), false)]
    fn test_auto_install(#[case] auto_install: Option<bool>, #[case] expected: bool) {
        // autoInstall defaults to true for zero-config; explicit values honored
        let settings = RawWorkspaceSettings {
            search_paths: None,
            languages: HashMap::new(),
            capture_mappings: HashMap::new(),
            auto_install,
            diagnostics_debounce_ms: None,
            features: None,
            language_servers: None,
        };

        let workspace: WorkspaceSettings = base_convert(&settings);
        assert_eq!(workspace.auto_install, expected);
    }

    #[rstest]
    #[case::default(None, DEFAULT_DEBOUNCE_MS)]
    #[case::explicit(Some(50), 50)]
    #[case::explicit_zero(Some(0), 0)]
    fn test_diagnostics_debounce_ms(#[case] raw: Option<u64>, #[case] expected: u64) {
        // Unset resolves to the runtime default; explicit values (incl. 0) honored.
        let settings = RawWorkspaceSettings {
            search_paths: None,
            languages: HashMap::new(),
            capture_mappings: HashMap::new(),
            auto_install: None,
            diagnostics_debounce_ms: raw,
            features: None,
            language_servers: None,
        };

        let workspace: WorkspaceSettings = base_convert(&settings);
        assert_eq!(workspace.diagnostics_debounce_ms, expected);
    }

    #[test]
    fn workspace_diagnostic_refresh_feature_resolves_defaults_and_custom_values() {
        let defaults = base_convert(&RawWorkspaceSettings::default());
        assert_eq!(
            defaults.features.workspace_diagnostic_refresh,
            settings::ResolvedDebounceFeatureSettings::default()
        );

        let raw: RawWorkspaceSettings = toml::from_str(
            r#"
            [features."workspace/diagnostic/refresh"]
            debounceMs = 25
            maxWaitMs = 250
            "#,
        )
        .unwrap();
        let custom = WorkspaceSettings::try_from_settings(&raw, None, |_| None).unwrap();
        assert_eq!(custom.features.workspace_diagnostic_refresh.debounce_ms, 25);
        assert_eq!(
            custom.features.workspace_diagnostic_refresh.max_wait_ms,
            250
        );
    }

    #[test]
    fn publish_diagnostics_feature_resolves_defaults_and_custom_values() {
        let defaults = base_convert(&RawWorkspaceSettings::default());
        assert_eq!(
            defaults
                .features
                .text_document_publish_diagnostics
                .debounce_ms,
            settings::DEFAULT_PUBLISH_DIAGNOSTICS_DEBOUNCE_MS
        );
        assert_eq!(
            defaults
                .features
                .text_document_publish_diagnostics
                .max_wait_ms,
            settings::DEFAULT_PUBLISH_DIAGNOSTICS_MAX_WAIT_MS
        );
        let raw: RawWorkspaceSettings = toml::from_str(
            r#"
            [features."textDocument/publishDiagnostics"]
            debounceMs = 30
            maxWaitMs = 300
            "#,
        )
        .unwrap();
        let custom = WorkspaceSettings::try_from_settings(&raw, None, |_| None).unwrap();
        assert_eq!(
            custom
                .features
                .text_document_publish_diagnostics
                .debounce_ms,
            30
        );
        assert_eq!(
            custom
                .features
                .text_document_publish_diagnostics
                .max_wait_ms,
            300
        );
    }

    #[test]
    fn publish_diagnostics_rejects_invalid_timing() {
        let raw: RawWorkspaceSettings = toml::from_str(
            r#"
            [features."textDocument/publishDiagnostics"]
            debounceMs = 101
            maxWaitMs = 100
            "#,
        )
        .unwrap();
        assert!(WorkspaceSettings::try_from_settings(&raw, None, |_| None).is_err());
    }

    #[test]
    fn workspace_diagnostic_refresh_rejects_invalid_timing() {
        let raw: RawWorkspaceSettings = toml::from_str(
            r#"
            [features."workspace/diagnostic/refresh"]
            debounceMs = 101
            maxWaitMs = 100
            "#,
        )
        .unwrap();
        let error = WorkspaceSettings::try_from_settings(&raw, None, |_| None).unwrap_err();
        assert!(error.to_string().contains("debounceMs=101, maxWaitMs=100"));
    }

    #[test]
    fn workspace_diagnostic_refresh_rejects_zero_and_unbounded_max_wait() {
        for max_wait_ms in [0, settings::MAX_FEATURE_TIMING_MS + 1] {
            let raw = RawWorkspaceSettings {
                features: Some(settings::FeatureSettings {
                    text_document_publish_diagnostics: None,
                    window_log_message: None,
                    workspace_diagnostic_refresh: Some(settings::DebounceFeatureSettings {
                        debounce_ms: Some(0),
                        max_wait_ms: Some(max_wait_ms),
                    }),
                }),
                ..Default::default()
            };
            assert!(WorkspaceSettings::try_from_settings(&raw, None, |_| None).is_err());
        }
    }

    #[test]
    fn feature_policy_rejects_unknown_methods_and_fields() {
        for invalid in [
            r#"[features."workspace/diagnostic/refresh"]
               debouncMs = 100"#,
            r#"[features."workspace/diagnostics/refresh"]
               debounceMs = 100"#,
        ] {
            assert!(toml::from_str::<RawWorkspaceSettings>(invalid).is_err());
        }
    }

    #[test]
    fn test_default_search_paths_format() {
        // resolve_library_path() appends "parser/" itself, so default_search_paths()
        // must return the base directory (not "/parser" or "/queries" subdirectories).
        let paths = default_search_paths();

        // Should have exactly one path (the base directory)
        assert_eq!(
            paths.len(),
            1,
            "default_search_paths should return single base directory, got {:?}",
            paths
        );

        // The path should NOT end with "/parser" or "/queries"
        let path = &paths[0];
        assert!(
            !path.ends_with("/parser") && !path.ends_with("/queries"),
            "Path should be base directory, not subdirectory: {}",
            path
        );

        // The path should end with "kakehashi" (the base directory name)
        assert!(
            path.ends_with("kakehashi"),
            "Path should end with 'kakehashi': {}",
            path
        );
    }

    #[test]
    fn test_bridge_router_respects_host_filter() {
        // Bridge filtering is applied at request time before routing to language servers.
        use settings::BridgeLanguageConfig;

        // Host markdown with bridge filter: only python and r enabled
        let mut bridge_filter = HashMap::new();
        bridge_filter.insert(
            "python".to_string(),
            BridgeLanguageConfig {
                enabled: Some(true),
                ..Default::default()
            },
        );
        bridge_filter.insert(
            "r".to_string(),
            BridgeLanguageConfig {
                enabled: Some(true),
                ..Default::default()
            },
        );
        let markdown_settings = LanguageSettings {
            bridge: Some(bridge_filter),
            ..Default::default()
        };

        // Router should allow python (enabled in filter)
        assert!(
            markdown_settings.is_language_bridgeable("python"),
            "Bridge router should allow python for markdown"
        );

        // Router should allow r (enabled in filter)
        assert!(
            markdown_settings.is_language_bridgeable("r"),
            "Bridge router should allow r for markdown"
        );

        // Router should block rust (not in filter)
        assert!(
            !markdown_settings.is_language_bridgeable("rust"),
            "Bridge router should block rust for markdown"
        );

        // Host quarto with no bridge filter (default: all)
        let quarto_settings = LanguageSettings::default();

        // Router should allow all languages
        assert!(
            quarto_settings.is_language_bridgeable("python"),
            "Bridge router should allow python for quarto (no filter)"
        );
        assert!(
            quarto_settings.is_language_bridgeable("rust"),
            "Bridge router should allow rust for quarto (no filter)"
        );

        // Host rmd with empty bridge filter (disable all)
        let rmd_settings = LanguageSettings {
            bridge: Some(HashMap::new()),
            ..Default::default()
        };

        // Router should block all languages
        assert!(
            !rmd_settings.is_language_bridgeable("r"),
            "Bridge router should block r for rmd (empty filter)"
        );
        assert!(
            !rmd_settings.is_language_bridgeable("python"),
            "Bridge router should block python for rmd (empty filter)"
        );
    }
}

#[cfg(test)]
mod strip_inherited_tests {
    use super::*;
    use settings::{AggregationConfig, AggregationStrategy, BridgeLanguageConfig};

    // --- strip_inherited_language_settings ---

    #[test]
    fn strips_all_fields_when_matching_inherited() {
        let inherited = LanguageSettings {
            parser: Some("/path/to/parser".to_string()),
            queries: Some(vec![]),
            ..Default::default()
        };
        let current = inherited.clone();
        let result = strip_inherited_language_settings(&inherited, &current);
        assert_eq!(result.parser, None);
        assert_eq!(result.queries, None);
    }

    #[test]
    fn preserves_differing_fields() {
        let inherited = LanguageSettings {
            parser: Some("/path/to/base".to_string()),
            ..Default::default()
        };
        let current = LanguageSettings {
            parser: Some("/path/to/custom".to_string()),
            ..Default::default()
        };
        let result = strip_inherited_language_settings(&inherited, &current);
        assert_eq!(result.parser, Some("/path/to/custom".to_string()));
    }

    #[test]
    fn preserves_base_field_always() {
        let inherited = LanguageSettings::default();
        let current = LanguageSettings {
            base: Some("markdown".to_string()),
            ..Default::default()
        };
        let result = strip_inherited_language_settings(&inherited, &current);
        assert_eq!(result.base, Some("markdown".to_string()));
    }

    // --- strip_inherited_bridge_map ---

    #[test]
    fn bridge_map_none_returns_none() {
        let result = strip_inherited_bridge_map(None, None);
        assert_eq!(result, None);
    }

    #[test]
    fn bridge_map_empty_current_preserved() {
        let result = strip_inherited_bridge_map(Some(&HashMap::new()), Some(&HashMap::new()));
        assert_eq!(result, Some(HashMap::new()));
    }

    #[test]
    fn bridge_map_strips_matching_keys_keeps_differing() {
        let inherited = HashMap::from([(
            "python".to_string(),
            BridgeLanguageConfig {
                enabled: Some(true),
                ..Default::default()
            },
        )]);
        let current = HashMap::from([
            (
                "python".to_string(),
                BridgeLanguageConfig {
                    enabled: Some(true),
                    ..Default::default()
                },
            ),
            (
                "lua".to_string(),
                BridgeLanguageConfig {
                    enabled: Some(false),
                    ..Default::default()
                },
            ),
        ]);

        let result = strip_inherited_bridge_map(Some(&inherited), Some(&current));
        let result = result.unwrap();
        assert!(
            !result.contains_key("python"),
            "python should be stripped (matches inherited)"
        );
        assert!(
            result.contains_key("lua"),
            "lua should be preserved (not in inherited)"
        );
    }

    // --- strip_inherited_aggregation_map ---

    #[test]
    fn aggregation_map_strips_matching_preserves_differing() {
        let inherited = HashMap::from([(
            WILDCARD_KEY.to_string(),
            AggregationConfig {
                strategy: Some(AggregationStrategy::Preferred),
                ..Default::default()
            },
        )]);
        let current = HashMap::from([
            (
                WILDCARD_KEY.to_string(),
                AggregationConfig {
                    strategy: Some(AggregationStrategy::Preferred),
                    ..Default::default()
                },
            ),
            (
                "textDocument/diagnostic".to_string(),
                AggregationConfig {
                    strategy: Some(AggregationStrategy::Concatenated),
                    ..Default::default()
                },
            ),
        ]);

        let result = strip_inherited_aggregation_map(Some(&inherited), Some(&current));
        let result = result.unwrap();
        assert!(
            !result.contains_key(WILDCARD_KEY),
            "wildcard should be stripped (matches inherited)"
        );
        assert!(
            result.contains_key("textDocument/diagnostic"),
            "diagnostic should be preserved (strategy differs)"
        );
    }

    #[test]
    fn aggregation_config_strips_matching_priorities() {
        let inherited = AggregationConfig {
            priorities: Some(vec!["pyright".to_string()]),
            strategy: Some(AggregationStrategy::Preferred),
            max_fan_out: Some(2),
            ..Default::default()
        };
        let current = AggregationConfig {
            priorities: Some(vec!["pyright".to_string()]),
            strategy: Some(AggregationStrategy::Concatenated),
            max_fan_out: Some(2),
            ..Default::default()
        };
        let result = strip_inherited_aggregation_config(&inherited, &current);
        assert_eq!(result.priorities, None, "priorities match → stripped");
        assert_eq!(
            result.strategy,
            Some(AggregationStrategy::Concatenated),
            "strategy differs → preserved"
        );
        assert_eq!(result.max_fan_out, None, "max_fan_out matches → stripped");
    }
}

#[cfg(test)]
mod try_from_settings_tests {
    use super::*;
    use expand::make_env;
    use settings::{QueryItem, QueryKind};

    #[test]
    fn expands_search_paths() {
        let settings = RawWorkspaceSettings {
            search_paths: Some(vec!["$TEST_VAR/parsers".to_string()]),
            languages: HashMap::new(),
            capture_mappings: HashMap::new(),
            auto_install: None,
            diagnostics_debounce_ms: None,
            features: None,
            language_servers: None,
        };
        let env = make_env(&[("TEST_VAR", "/home/user")]);
        let ws = WorkspaceSettings::try_from_settings(&settings, None, env).unwrap();
        assert_eq!(ws.search_paths, vec!["/home/user/parsers"]);
    }

    #[test]
    fn expands_parser_path() {
        let mut languages = HashMap::new();
        languages.insert(
            "lua".to_string(),
            LanguageSettings {
                parser: Some("$TEST_VAR/lua.so".to_string()),
                ..Default::default()
            },
        );
        let settings = RawWorkspaceSettings {
            search_paths: None,
            languages,
            capture_mappings: HashMap::new(),
            auto_install: None,
            diagnostics_debounce_ms: None,
            features: None,
            language_servers: None,
        };
        let env = make_env(&[("TEST_VAR", "/opt/parsers")]);
        let ws = WorkspaceSettings::try_from_settings(&settings, None, env).unwrap();
        assert_eq!(
            ws.languages.get("lua").unwrap().parser.as_deref(),
            Some("/opt/parsers/lua.so")
        );
    }

    #[test]
    fn expands_query_path() {
        let mut languages = HashMap::new();
        languages.insert(
            "lua".to_string(),
            LanguageSettings {
                queries: Some(vec![QueryItem {
                    path: "${TEST_VAR}/highlights.scm".to_string(),
                    kind: Some(QueryKind::Highlights),
                }]),
                ..Default::default()
            },
        );
        let settings = RawWorkspaceSettings {
            search_paths: None,
            languages,
            capture_mappings: HashMap::new(),
            auto_install: None,
            diagnostics_debounce_ms: None,
            features: None,
            language_servers: None,
        };
        let env = make_env(&[("TEST_VAR", "/queries")]);
        let ws = WorkspaceSettings::try_from_settings(&settings, None, env).unwrap();
        let queries = ws.languages.get("lua").unwrap().queries.as_ref().unwrap();
        assert_eq!(queries[0].path, "/queries/highlights.scm");
    }

    #[test]
    fn resolves_base_before_expanding_derived_paths() {
        // With most-specific-wins, derived parser is kept and expanded.
        // If derived has no parser, it inherits base's parser path,
        // which must be expandable.
        let mut languages = HashMap::new();
        languages.insert(
            "markdown".to_string(),
            LanguageSettings {
                parser: Some("/opt/parsers/markdown.so".to_string()),
                ..Default::default()
            },
        );
        languages.insert(
            "rmd".to_string(),
            LanguageSettings {
                base: Some("markdown".to_string()),
                // No parser → inherits markdown's parser
                ..Default::default()
            },
        );
        let settings = RawWorkspaceSettings {
            search_paths: None,
            languages,
            capture_mappings: HashMap::new(),
            auto_install: None,
            diagnostics_debounce_ms: None,
            features: None,
            language_servers: None,
        };

        let env = make_env(&[]);
        let ws = WorkspaceSettings::try_from_settings(&settings, None, env)
            .expect("inherited parser path should be expanded successfully");

        assert_eq!(
            ws.languages.get("rmd").unwrap().parser.as_deref(),
            Some("/opt/parsers/markdown.so")
        );
    }

    #[test]
    fn undefined_var_returns_error() {
        let settings = RawWorkspaceSettings {
            search_paths: Some(vec!["$UNDEFINED/path".to_string()]),
            languages: HashMap::new(),
            capture_mappings: HashMap::new(),
            auto_install: None,
            diagnostics_debounce_ms: None,
            features: None,
            language_servers: None,
        };
        let env = make_env(&[]);
        let errs = WorkspaceSettings::try_from_settings(&settings, None, env).unwrap_err();
        assert_eq!(
            errs.0,
            vec![expand::ExpandError::UndefinedVar {
                var_name: "UNDEFINED".to_string(),
                input: "$UNDEFINED/path".to_string(),
            }]
        );
    }

    #[test]
    fn collects_all_expansion_errors() {
        let mut languages = HashMap::new();
        languages.insert(
            "lua".to_string(),
            LanguageSettings {
                parser: Some("$ALSO_MISSING/lua.so".to_string()),
                ..Default::default()
            },
        );
        let settings = RawWorkspaceSettings {
            search_paths: Some(vec!["$MISSING_ONE/parsers".to_string()]),
            languages,
            capture_mappings: HashMap::new(),
            auto_install: None,
            diagnostics_debounce_ms: None,
            features: None,
            language_servers: None,
        };
        let env = make_env(&[]);
        let errs = WorkspaceSettings::try_from_settings(&settings, None, env).unwrap_err();
        assert_eq!(
            errs.0.len(),
            2,
            "Should collect errors from all path fields"
        );
    }

    #[test]
    fn tilde_without_home_dir_returns_error() {
        let settings = RawWorkspaceSettings {
            search_paths: Some(vec!["~/parsers".to_string()]),
            languages: HashMap::new(),
            capture_mappings: HashMap::new(),
            auto_install: None,
            diagnostics_debounce_ms: None,
            features: None,
            language_servers: None,
        };
        let env = make_env(&[]);
        let errs = WorkspaceSettings::try_from_settings(&settings, None, env).unwrap_err();
        assert_eq!(
            errs.0,
            vec![expand::ExpandError::NoHomeDir {
                input: "~/parsers".to_string(),
            }]
        );
    }

    #[test]
    fn base_config_most_specific_wins() {
        let mut languages = HashMap::new();
        languages.insert(
            "markdown".to_string(),
            LanguageSettings {
                parser: Some("/opt/markdown.so".to_string()),
                queries: Some(vec![crate::config::settings::QueryItem {
                    path: "/opt/markdown/highlights.scm".to_string(),
                    kind: Some(crate::config::settings::QueryKind::Highlights),
                }]),
                ..Default::default()
            },
        );
        languages.insert(
            "rmd".to_string(),
            LanguageSettings {
                base: Some("markdown".to_string()),
                parser: Some("/opt/rmd.so".to_string()),
                ..Default::default()
            },
        );
        let settings = RawWorkspaceSettings {
            languages,
            ..Default::default()
        };
        let env = make_env(&[]);
        let ws = WorkspaceSettings::try_from_settings(&settings, None, env).unwrap();

        // rmd's own parser wins (most-specific-wins)
        assert_eq!(ws.languages["rmd"].parser.as_deref(), Some("/opt/rmd.so"));
        // queries inherited from markdown (rmd didn't set them)
        assert!(ws.languages["rmd"].queries.is_some());
        // base field should be preserved
        assert_eq!(ws.languages["rmd"].base, Some("markdown".to_string()));
    }

    #[test]
    fn raw_workspace_settings_from_preserves_implicit_wildcard_inheritance_on_reload() {
        let initial = RawWorkspaceSettings {
            languages: HashMap::from([
                (
                    WILDCARD_KEY.to_string(),
                    LanguageSettings {
                        bridge: Some(HashMap::from([(
                            "python".to_string(),
                            settings::BridgeLanguageConfig {
                                aggregation: Some(HashMap::from([(
                                    WILDCARD_KEY.to_string(),
                                    settings::AggregationConfig {
                                        priorities: Some(vec!["pyright".to_string()]),
                                        ..Default::default()
                                    },
                                )])),
                                ..Default::default()
                            },
                        )])),
                        ..Default::default()
                    },
                ),
                ("r".to_string(), LanguageSettings::default()),
            ]),
            ..Default::default()
        };

        let current = WorkspaceSettings::try_from_settings(&initial, None, |_| None).unwrap();
        let current_raw = RawWorkspaceSettings::from(&current);

        assert_eq!(
            current_raw.languages["r"].bridge, None,
            "implicit wildcard bridge settings should stay inherited in raw settings"
        );

        let update = RawWorkspaceSettings {
            languages: HashMap::from([(
                WILDCARD_KEY.to_string(),
                LanguageSettings {
                    bridge: Some(HashMap::from([(
                        "python".to_string(),
                        settings::BridgeLanguageConfig {
                            aggregation: Some(HashMap::from([(
                                WILDCARD_KEY.to_string(),
                                settings::AggregationConfig {
                                    priorities: Some(vec!["ruff".to_string()]),
                                    ..Default::default()
                                },
                            )])),
                            ..Default::default()
                        },
                    )])),
                    ..Default::default()
                },
            )]),
            ..Default::default()
        };

        let merged = merge::merge_workspace_settings(Some(current_raw), Some(update)).unwrap();
        let reloaded = WorkspaceSettings::try_from_settings(&merged, None, |_| None).unwrap();

        let priorities = reloaded.languages["r"].bridge.as_ref().unwrap()["python"]
            .aggregation
            .as_ref()
            .unwrap()[WILDCARD_KEY]
            .priorities
            .clone();
        assert_eq!(priorities, Some(vec!["ruff".to_string()]));
    }

    #[test]
    fn raw_workspace_settings_from_preserves_wildcard_inheritance_when_base_is_missing() {
        let initial = RawWorkspaceSettings {
            languages: HashMap::from([
                (
                    WILDCARD_KEY.to_string(),
                    LanguageSettings {
                        bridge: Some(HashMap::from([(
                            "python".to_string(),
                            settings::BridgeLanguageConfig {
                                aggregation: Some(HashMap::from([(
                                    WILDCARD_KEY.to_string(),
                                    settings::AggregationConfig {
                                        priorities: Some(vec!["pyright".to_string()]),
                                        ..Default::default()
                                    },
                                )])),
                                ..Default::default()
                            },
                        )])),
                        ..Default::default()
                    },
                ),
                (
                    "r".to_string(),
                    LanguageSettings {
                        base: Some("missing".to_string()),
                        ..Default::default()
                    },
                ),
            ]),
            ..Default::default()
        };

        let current = WorkspaceSettings::try_from_settings(&initial, None, |_| None).unwrap();
        let current_raw = RawWorkspaceSettings::from(&current);

        assert_eq!(
            current_raw.languages["r"].bridge, None,
            "wildcard bridge settings should stay inherited even when the named base is absent"
        );

        let update = RawWorkspaceSettings {
            languages: HashMap::from([(
                WILDCARD_KEY.to_string(),
                LanguageSettings {
                    bridge: Some(HashMap::from([(
                        "python".to_string(),
                        settings::BridgeLanguageConfig {
                            aggregation: Some(HashMap::from([(
                                WILDCARD_KEY.to_string(),
                                settings::AggregationConfig {
                                    priorities: Some(vec!["ruff".to_string()]),
                                    ..Default::default()
                                },
                            )])),
                            ..Default::default()
                        },
                    )])),
                    ..Default::default()
                },
            )]),
            ..Default::default()
        };

        let merged = merge::merge_workspace_settings(Some(current_raw), Some(update)).unwrap();
        let reloaded = WorkspaceSettings::try_from_settings(&merged, None, |_| None).unwrap();

        let priorities = reloaded.languages["r"].bridge.as_ref().unwrap()["python"]
            .aggregation
            .as_ref()
            .unwrap()[WILDCARD_KEY]
            .priorities
            .clone();
        assert_eq!(priorities, Some(vec!["ruff".to_string()]));
    }

    #[test]
    fn raw_workspace_settings_from_preserves_explicit_empty_aggregation_map() {
        let current = WorkspaceSettings {
            languages: HashMap::from([
                (
                    WILDCARD_KEY.to_string(),
                    LanguageSettings {
                        bridge: Some(HashMap::from([(
                            "python".to_string(),
                            settings::BridgeLanguageConfig {
                                aggregation: Some(HashMap::from([(
                                    WILDCARD_KEY.to_string(),
                                    settings::AggregationConfig {
                                        strategy: Some(settings::AggregationStrategy::Preferred),
                                        ..Default::default()
                                    },
                                )])),
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
                            settings::BridgeLanguageConfig {
                                aggregation: Some(HashMap::new()),
                                ..Default::default()
                            },
                        )])),
                        ..Default::default()
                    },
                ),
            ]),
            ..Default::default()
        };

        let current_raw = RawWorkspaceSettings::from(&current);

        assert_eq!(
            current_raw.languages["r"].bridge.as_ref().unwrap()["python"].aggregation,
            Some(HashMap::new())
        );
    }

    #[test]
    fn raw_workspace_settings_from_keeps_self_referential_language_as_blank_slate_root() {
        let current = WorkspaceSettings {
            languages: HashMap::from([
                (
                    WILDCARD_KEY.to_string(),
                    LanguageSettings {
                        bridge: Some(HashMap::from([(
                            "python".to_string(),
                            settings::BridgeLanguageConfig {
                                enabled: Some(true),
                                ..Default::default()
                            },
                        )])),
                        ..Default::default()
                    },
                ),
                (
                    "_blank".to_string(),
                    LanguageSettings {
                        base: Some("_blank".to_string()),
                        bridge: Some(HashMap::new()),
                        ..Default::default()
                    },
                ),
            ]),
            ..Default::default()
        };

        let current_raw = RawWorkspaceSettings::from(&current);

        assert_eq!(current_raw.languages["_blank"].bridge, Some(HashMap::new()));
    }

    #[test]
    fn raw_workspace_settings_from_preserves_explicit_bridge_override_for_self_referential_root() {
        let current = WorkspaceSettings {
            languages: HashMap::from([
                (
                    WILDCARD_KEY.to_string(),
                    LanguageSettings {
                        bridge: Some(HashMap::from([(
                            "python".to_string(),
                            settings::BridgeLanguageConfig {
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
                        base: Some("r".to_string()),
                        bridge: Some(HashMap::from([(
                            "python".to_string(),
                            settings::BridgeLanguageConfig {
                                enabled: Some(false),
                                ..Default::default()
                            },
                        )])),
                        ..Default::default()
                    },
                ),
            ]),
            ..Default::default()
        };

        let current_raw = RawWorkspaceSettings::from(&current);

        assert_eq!(
            current_raw.languages["r"].bridge.as_ref().unwrap()["python"].enabled,
            Some(false)
        );
    }

    #[test]
    fn raw_workspace_settings_from_preserves_explicit_bridge_value_matching_wildcard_for_self_referential_root()
     {
        let current = WorkspaceSettings {
            languages: HashMap::from([
                (
                    WILDCARD_KEY.to_string(),
                    LanguageSettings {
                        bridge: Some(HashMap::from([(
                            "python".to_string(),
                            settings::BridgeLanguageConfig {
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
                        base: Some("r".to_string()),
                        bridge: Some(HashMap::from([(
                            "python".to_string(),
                            settings::BridgeLanguageConfig {
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

        let current_raw = RawWorkspaceSettings::from(&current);

        assert_eq!(
            current_raw.languages["r"].bridge.as_ref().unwrap()["python"].enabled,
            Some(true)
        );
    }
}
