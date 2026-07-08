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
//! by command name. Two servers advertising the same command name collide to a
//! single origin — an accepted limitation (bridged-action commands, which embed
//! the origin, don't have this ambiguity).

use std::collections::HashMap;
use std::sync::Mutex;

use super::ConnectionKey;
use crate::error::LockResultExt;

#[derive(Default)]
pub(crate) struct CommandOriginRegistry {
    origins: Mutex<HashMap<String, ConnectionKey>>,
}

impl CommandOriginRegistry {
    /// Record `commands` as advertised by the connection `key`, returning the
    /// subset that is NEWLY seen (never registered before).
    ///
    /// The routing target is ALWAYS updated to `key` — a server respawned under a
    /// different root, or a different server that later advertises the same name,
    /// must route to the CURRENT live connection, not a stale one. Only the
    /// genuinely-new names are returned, though: the command NAME is already
    /// registered with the editor, so re-advertising it would be a duplicate
    /// registration.
    pub(crate) fn register(&self, key: &ConnectionKey, commands: &[String]) -> Vec<String> {
        let mut origins = self
            .origins
            .lock()
            .recover_poison("CommandOriginRegistry::register");
        let mut added = Vec::new();
        for command in commands {
            if let Some(existing) = origins.get_mut(command) {
                // Re-point routing to the current owner; don't re-advertise (and
                // don't clone the key string — the entry already exists).
                *existing = key.clone();
            } else {
                origins.insert(command.clone(), key.clone());
                added.push(command.clone());
            }
        }
        added
    }

    /// The connection that advertised a palette command name, if one did.
    pub(crate) fn route(&self, command: &str) -> Option<ConnectionKey> {
        self.origins
            .lock()
            .recover_poison("CommandOriginRegistry::route")
            .get(command)
            .cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_reports_only_new_names_but_always_updates_routing() {
        let reg = CommandOriginRegistry::default();
        let ruff_a = ConnectionKey::new("ruff", Some("/w/a".to_string()));
        let ruff_b = ConnectionKey::new("ruff", Some("/w/b".to_string()));

        assert_eq!(
            reg.register(&ruff_a, &["ruff.fix".to_string(), "ruff.sort".to_string()]),
            vec!["ruff.fix".to_string(), "ruff.sort".to_string()]
        );
        assert_eq!(reg.route("ruff.fix").as_ref(), Some(&ruff_a));

        // A respawn under a DIFFERENT root re-advertises nothing new (the name is
        // already registered with the editor) but RE-POINTS routing to the live
        // connection, so a palette command reaches the current process.
        assert!(reg.register(&ruff_b, &["ruff.fix".to_string()]).is_empty());
        assert_eq!(
            reg.route("ruff.fix").as_ref(),
            Some(&ruff_b),
            "routing must follow the current owner, not stay on the stale root"
        );

        assert_eq!(reg.route("unknown.cmd"), None);
    }
}
