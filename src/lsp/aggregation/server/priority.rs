//! Priority-list expansion for server aggregation
//! (aggregation-priorities-wildcard).
//!
//! A `priorities` list is an **ordered allowlist** over the configured
//! servers: servers absent from the list do not run. The `"*"` element
//! stands for every configured server not named elsewhere in the list, as a
//! single first-win group at that position. [`expand_priorities`] turns the
//! raw config list into [`PriorityEntry`] values that both fan-out
//! (membership + spawn order) and fan-in (decision order) consume, so the
//! two sides can never disagree about which servers participate.

use std::collections::HashSet;

use crate::config::settings::PRIORITIES_WILDCARD;
use crate::lsp::bridge::ResolvedServerConfig;

/// One step in the resolved priority walk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PriorityEntry {
    /// A single named server at a strict priority position.
    Server(String),
    /// The `"*"` group: configured-but-unlisted servers. First-win amongst
    /// themselves — the inner order is the spawn order (config order), not a
    /// ranking.
    Rest(Vec<String>),
}

/// Expand a raw `priorities` list against the configured servers.
///
/// - Named servers keep their position (configured-only, deduplicated;
///   unknown names are logged and dropped — under allowlist semantics a typo
///   would otherwise silently exclude the intended server).
/// - The first `"*"` becomes a [`PriorityEntry::Rest`] of the configured
///   servers not named anywhere in the list (config order). A second `"*"`
///   is logged and ignored. An empty rest group is dropped.
/// - An empty `priorities` expands to no entries: fan-out is disabled for
///   this target (`[]` is the per-method kill switch).
pub(crate) fn expand_priorities(
    priorities: &[String],
    configs: &[ResolvedServerConfig],
) -> Vec<PriorityEntry> {
    let configured: HashSet<&str> = configs.iter().map(|c| c.server_name.as_str()).collect();

    let mut entries: Vec<PriorityEntry> = Vec::with_capacity(priorities.len());
    let mut seen: HashSet<&str> = HashSet::new();
    let mut rest_index: Option<usize> = None;

    for name in priorities {
        if name == PRIORITIES_WILDCARD {
            if rest_index.is_some() {
                // debug, not warn: expansion runs on every dispatch
                // (completion fires per keystroke), so a persistent config
                // condition would flood the log.
                log::debug!(
                    "aggregation priorities contain more than one '{}'; only the first is honored",
                    PRIORITIES_WILDCARD
                );
                continue;
            }
            rest_index = Some(entries.len());
            entries.push(PriorityEntry::Rest(Vec::new()));
        } else if !configured.contains(name.as_str()) {
            // debug, not warn: an unmatched name here is routine, not only a
            // typo — priorities inherited from a `bridge._` wildcard entry
            // legitimately name servers that exist for other injection
            // languages of the same host.
            log::debug!(
                "aggregation priorities name '{name}' matches no configured server for this \
                 target; ignored (priorities is an allowlist)"
            );
        } else if seen.insert(name.as_str()) {
            entries.push(PriorityEntry::Server(name.clone()));
        }
    }

    if let Some(index) = rest_index {
        let rest: Vec<String> = configs
            .iter()
            .filter(|c| !seen.contains(c.server_name.as_str()))
            .map(|c| c.server_name.clone())
            .collect();
        if rest.is_empty() {
            entries.remove(index);
        } else {
            entries[index] = PriorityEntry::Rest(rest);
        }
    }

    entries
}

/// Cap the total number of servers across all entries to `max_fan_out`.
///
/// Counts flattened names in walk order; a cap landing inside a
/// [`PriorityEntry::Rest`] truncates the group (a group truncated to empty
/// is dropped), and entries past the cap are dropped.
pub(crate) fn truncate_entries(
    entries: Vec<PriorityEntry>,
    max_fan_out: Option<usize>,
) -> Vec<PriorityEntry> {
    let Some(limit) = max_fan_out else {
        return entries;
    };
    let mut remaining = limit;
    let mut result = Vec::with_capacity(entries.len());
    for entry in entries {
        if remaining == 0 {
            break;
        }
        match entry {
            PriorityEntry::Server(name) => {
                remaining -= 1;
                result.push(PriorityEntry::Server(name));
            }
            PriorityEntry::Rest(mut names) => {
                names.truncate(remaining);
                remaining -= names.len();
                if !names.is_empty() {
                    result.push(PriorityEntry::Rest(names));
                }
            }
        }
    }
    result
}

/// Flatten entries to server names in walk order.
///
/// This is the fan-out membership and spawn order, and the result ordering
/// for the concatenated strategy.
pub(crate) fn entry_names(entries: &[PriorityEntry]) -> Vec<String> {
    entries
        .iter()
        .flat_map(|entry| match entry {
            PriorityEntry::Server(name) => std::slice::from_ref(name),
            PriorityEntry::Rest(names) => names.as_slice(),
        })
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::config::settings::BridgeServerConfig;

    fn make_config(name: &str) -> ResolvedServerConfig {
        ResolvedServerConfig {
            server_name: name.to_string(),
            config: Arc::new(BridgeServerConfig {
                cmd: vec![name.to_string()],
                languages: vec![],
                initialization_options: None,
                workspace_markers: None,
                on_type_formatting_triggers: None,
                prefer_shared_instance: None,
                settings: None,
            }),
        }
    }

    fn configs(names: &[&str]) -> Vec<ResolvedServerConfig> {
        names.iter().map(|n| make_config(n)).collect()
    }

    fn prios(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| n.to_string()).collect()
    }

    #[test]
    fn expand_wildcard_only_is_one_rest_group_of_all_servers() {
        let entries = expand_priorities(&prios(&["*"]), &configs(&["alpha", "beta", "gamma"]));
        assert_eq!(
            entries,
            vec![PriorityEntry::Rest(prios(&["alpha", "beta", "gamma"]))]
        );
    }

    #[test]
    fn expand_named_only_is_strict_allowlist() {
        // No '*': unlisted servers (beta) are excluded entirely.
        let entries = expand_priorities(
            &prios(&["gamma", "alpha"]),
            &configs(&["alpha", "beta", "gamma"]),
        );
        assert_eq!(
            entries,
            vec![
                PriorityEntry::Server("gamma".into()),
                PriorityEntry::Server("alpha".into()),
            ]
        );
    }

    #[test]
    fn expand_empty_priorities_disables_fan_out() {
        let entries = expand_priorities(&[], &configs(&["alpha", "beta"]));
        assert!(entries.is_empty(), "[] is the per-method kill switch");
    }

    #[test]
    fn expand_wildcard_position_demotes_later_names_below_rest() {
        // ["foo", "*", "zzz"]: zzz ranks BELOW the unlisted rest — the
        // expressiveness a boolean exclusive-flag cannot provide (ADR).
        let entries = expand_priorities(
            &prios(&["foo", "*", "zzz"]),
            &configs(&["bar", "foo", "qux", "zzz"]),
        );
        assert_eq!(
            entries,
            vec![
                PriorityEntry::Server("foo".into()),
                PriorityEntry::Rest(prios(&["bar", "qux"])),
                PriorityEntry::Server("zzz".into()),
            ]
        );
    }

    #[test]
    fn expand_rest_excludes_names_listed_after_wildcard() {
        // zzz appears after '*' but must not also be in the rest group.
        let entries = expand_priorities(&prios(&["*", "zzz"]), &configs(&["alpha", "zzz"]));
        assert_eq!(
            entries,
            vec![
                PriorityEntry::Rest(prios(&["alpha"])),
                PriorityEntry::Server("zzz".into()),
            ]
        );
    }

    #[test]
    fn expand_drops_unknown_names() {
        let entries =
            expand_priorities(&prios(&["unknown", "alpha"]), &configs(&["alpha", "beta"]));
        assert_eq!(entries, vec![PriorityEntry::Server("alpha".into())]);
    }

    #[test]
    fn expand_dedupes_repeated_names() {
        let entries = expand_priorities(
            &prios(&["alpha", "alpha", "beta"]),
            &configs(&["alpha", "beta"]),
        );
        assert_eq!(
            entries,
            vec![
                PriorityEntry::Server("alpha".into()),
                PriorityEntry::Server("beta".into()),
            ]
        );
    }

    #[test]
    fn expand_ignores_second_wildcard() {
        let entries = expand_priorities(&prios(&["alpha", "*", "*"]), &configs(&["alpha", "beta"]));
        assert_eq!(
            entries,
            vec![
                PriorityEntry::Server("alpha".into()),
                PriorityEntry::Rest(prios(&["beta"])),
            ]
        );
    }

    #[test]
    fn expand_drops_empty_rest_group() {
        // Every configured server is explicitly named: '*' has nothing left.
        let entries = expand_priorities(
            &prios(&["alpha", "beta", "*"]),
            &configs(&["alpha", "beta"]),
        );
        assert_eq!(
            entries,
            vec![
                PriorityEntry::Server("alpha".into()),
                PriorityEntry::Server("beta".into()),
            ]
        );
    }

    #[test]
    fn expand_rest_preserves_config_order() {
        let entries = expand_priorities(&prios(&["*"]), &configs(&["gamma", "alpha", "beta"]));
        assert_eq!(
            entries,
            vec![PriorityEntry::Rest(prios(&["gamma", "alpha", "beta"]))]
        );
    }

    #[test]
    fn truncate_none_keeps_all() {
        let entries = expand_priorities(&prios(&["*"]), &configs(&["a", "b", "c"]));
        let truncated = truncate_entries(entries.clone(), None);
        assert_eq!(truncated, entries);
    }

    #[test]
    fn truncate_zero_disables_fan_out() {
        let entries = expand_priorities(&prios(&["*"]), &configs(&["a", "b"]));
        assert!(truncate_entries(entries, Some(0)).is_empty());
    }

    #[test]
    fn truncate_cuts_inside_rest_group() {
        let entries = expand_priorities(&prios(&["*"]), &configs(&["a", "b", "c"]));
        assert_eq!(
            truncate_entries(entries, Some(2)),
            vec![PriorityEntry::Rest(prios(&["a", "b"]))]
        );
    }

    #[test]
    fn truncate_drops_entries_past_cap() {
        let entries = expand_priorities(&prios(&["a", "*", "c"]), &configs(&["a", "b", "c"]));
        // flatten order: a, b (rest), c — cap 2 keeps a + rest[b], drops c.
        assert_eq!(
            truncate_entries(entries, Some(2)),
            vec![
                PriorityEntry::Server("a".into()),
                PriorityEntry::Rest(prios(&["b"])),
            ]
        );
    }

    #[test]
    fn entry_names_flattens_in_walk_order() {
        let entries = expand_priorities(
            &prios(&["foo", "*", "zzz"]),
            &configs(&["bar", "foo", "zzz"]),
        );
        assert_eq!(entry_names(&entries), prios(&["foo", "bar", "zzz"]));
    }
}
