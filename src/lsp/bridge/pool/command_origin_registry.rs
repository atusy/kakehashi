//! Maps a downstream server's advertised `workspace/executeCommand` command
//! names to the CONNECTION that advertised them (#628 palette-fired commands).
//!
//! A command surfaced in a bridged code action routes by its NAME-encoded
//! envelope (`command_routing`). But a command the client fires WITHOUT an
//! action context — from the command palette, keyed off the advertised
//! `executeCommandProvider.commands` list — arrives as the RAW downstream name.
//! This registry lets `dispatch_execute_command` resolve that raw name back to
//! the exact `(server, root)` connection that advertised it, so the command runs
//! in the same workspace context (not a fresh client-root connection).
//!
//! Global to the editor↔Kakehashi session (one instance on the pool) and keyed
//! by command name. Every distinct advertising connection is retained: a raw
//! palette command is routable only while its origin is unambiguous. Bridged
//! action commands embed their exact origin and do not use this registry.

use std::collections::HashMap;
use std::sync::Mutex;

use super::ConnectionKey;
use crate::error::LockResultExt;

#[derive(Default)]
pub(crate) struct CommandOriginRegistry {
    origins: Mutex<HashMap<String, Vec<ConnectionKey>>>,
}

impl CommandOriginRegistry {
    /// Record `commands` as advertised by the connection `key`, returning the
    /// subset that is NEWLY seen (never registered before).
    ///
    /// Re-registering from the same connection is idempotent. A distinct origin
    /// is retained alongside the first so routing can fail soft on ambiguity
    /// instead of selecting whichever handshake completed last. Only genuinely
    /// new command names are returned: the name is already registered with the
    /// editor after its first advertisement.
    pub(crate) fn register(&self, key: &ConnectionKey, commands: Vec<String>) -> Vec<String> {
        let mut origins = self
            .origins
            .lock()
            .recover_poison("CommandOriginRegistry::register");
        let mut added = Vec::new();
        for command in commands {
            if let Some(existing) = origins.get_mut(&command) {
                if !existing.contains(key) {
                    existing.push(key.clone());
                }
            } else {
                // Clone only for the map key; move the name itself into `added`.
                origins.insert(command.clone(), vec![key.clone()]);
                added.push(command);
            }
        }
        added
    }

    /// The sole connection that advertised a palette command name. Returns
    /// `None` for both unknown and ambiguous names; callers log either as a
    /// fail-soft refusal rather than execute against an arbitrary workspace.
    pub(crate) fn route(&self, command: &str) -> Option<ConnectionKey> {
        self.origins
            .lock()
            .recover_poison("CommandOriginRegistry::route")
            .get(command)
            .and_then(|origins| match origins.as_slice() {
                [origin] => Some(origin.clone()),
                [] | [_, ..] => None,
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_reports_only_new_names_and_same_origin_is_idempotent() {
        let reg = CommandOriginRegistry::default();
        let ruff_a = ConnectionKey::new("ruff", Some("/w/a".to_string()));

        assert_eq!(
            reg.register(
                &ruff_a,
                vec!["ruff.fix".to_string(), "ruff.sort".to_string()]
            ),
            vec!["ruff.fix".to_string(), "ruff.sort".to_string()]
        );
        assert_eq!(reg.route("ruff.fix").as_ref(), Some(&ruff_a));

        // The same connection advertising again neither duplicates the editor
        // registration nor makes its route ambiguous.
        assert!(
            reg.register(&ruff_a, vec!["ruff.fix".to_string()])
                .is_empty()
        );
        assert_eq!(
            reg.route("ruff.fix").as_ref(),
            Some(&ruff_a),
            "re-advertising from one origin stays uniquely routable"
        );

        assert_eq!(reg.route("unknown.cmd"), None);
    }

    #[test]
    fn colliding_command_name_has_no_arbitrary_route() {
        let reg = CommandOriginRegistry::default();
        let ruff = ConnectionKey::new("ruff", Some("/w/a".to_string()));
        let eslint = ConnectionKey::new("eslint", Some("/w/b".to_string()));

        assert_eq!(
            reg.register(&ruff, vec!["source.fixAll".to_string()]),
            vec!["source.fixAll"]
        );
        assert!(
            reg.register(&eslint, vec!["source.fixAll".to_string()])
                .is_empty(),
            "the editor command name is registered only once"
        );
        assert_eq!(
            reg.route("source.fixAll"),
            None,
            "an ambiguous raw command must fail soft instead of choosing the last handshake"
        );
    }
}
