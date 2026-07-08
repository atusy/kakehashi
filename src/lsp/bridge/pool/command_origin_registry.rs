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
    /// subset that was NEWLY added (not already registered under any connection).
    /// The caller advertises only the new names upstream, so a server respawn or
    /// a second root re-advertises nothing (dedup by construction).
    pub(crate) fn register(&self, key: &ConnectionKey, commands: &[String]) -> Vec<String> {
        let mut origins = self
            .origins
            .lock()
            .recover_poison("CommandOriginRegistry::register");
        let mut added = Vec::new();
        for command in commands {
            if !origins.contains_key(command) {
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
    fn register_dedups_and_reports_only_new_names() {
        let reg = CommandOriginRegistry::default();
        let ruff = ConnectionKey::new("ruff", Some("/w".to_string()));
        let pyright = ConnectionKey::new("pyright", Some("/w".to_string()));

        assert_eq!(
            reg.register(&ruff, &["ruff.fix".to_string(), "ruff.sort".to_string()]),
            vec!["ruff.fix".to_string(), "ruff.sort".to_string()]
        );
        // A respawn / second root re-registers nothing new.
        assert!(reg.register(&ruff, &["ruff.fix".to_string()]).is_empty());
        // A different server contributes only its unseen names.
        assert_eq!(
            reg.register(
                &pyright,
                &["ruff.fix".to_string(), "pyright.org".to_string()]
            ),
            vec!["pyright.org".to_string()]
        );
        assert_eq!(reg.route("ruff.fix").as_ref(), Some(&ruff));
        assert_eq!(reg.route("pyright.org").as_ref(), Some(&pyright));
        assert_eq!(reg.route("unknown.cmd"), None);
    }
}
