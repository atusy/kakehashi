//! Maps a downstream server's advertised `workspace/executeCommand` command
//! names to their origin server (#628 palette-fired commands).
//!
//! A command surfaced in a bridged code action routes by its NAME-encoded
//! envelope (`command_routing`). But a command the client fires WITHOUT an
//! action context — from the command palette, keyed off the advertised
//! `executeCommandProvider.commands` list — arrives as the RAW downstream name.
//! This registry lets `dispatch_execute_command` resolve that raw name back to
//! the server that advertised it.
//!
//! Global to the editor↔Kakehashi session (one instance on the pool) and keyed
//! by command name. Two servers advertising the same command name collide to a
//! single origin — an accepted limitation (bridged-action commands, which embed
//! the origin, don't have this ambiguity).

use std::collections::HashMap;
use std::sync::Mutex;

use crate::error::LockResultExt;

#[derive(Default)]
pub(crate) struct CommandOriginRegistry {
    origins: Mutex<HashMap<String, String>>,
}

impl CommandOriginRegistry {
    /// Record `commands` as owned by `server_name`, returning the subset that was
    /// NEWLY added (not already registered under any server). The caller
    /// advertises only the new names upstream, so a server respawn or a second
    /// root for the same server re-advertises nothing (dedup by construction).
    pub(crate) fn register(&self, server_name: &str, commands: &[String]) -> Vec<String> {
        let mut origins = self
            .origins
            .lock()
            .recover_poison("CommandOriginRegistry::register");
        let mut added = Vec::new();
        for command in commands {
            if !origins.contains_key(command) {
                origins.insert(command.clone(), server_name.to_string());
                added.push(command.clone());
            }
        }
        added
    }

    /// The origin server for a palette command name, if one advertised it.
    pub(crate) fn origin(&self, command: &str) -> Option<String> {
        self.origins
            .lock()
            .recover_poison("CommandOriginRegistry::origin")
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
        assert_eq!(
            reg.register("ruff", &["ruff.fix".to_string(), "ruff.sort".to_string()]),
            vec!["ruff.fix".to_string(), "ruff.sort".to_string()]
        );
        // A respawn / second root re-registers nothing new.
        assert!(reg.register("ruff", &["ruff.fix".to_string()]).is_empty());
        // A different server contributes only its unseen names.
        assert_eq!(
            reg.register(
                "pyright",
                &["ruff.fix".to_string(), "pyright.org".to_string()]
            ),
            vec!["pyright.org".to_string()]
        );
        assert_eq!(reg.origin("ruff.fix").as_deref(), Some("ruff"));
        assert_eq!(reg.origin("pyright.org").as_deref(), Some("pyright"));
        assert_eq!(reg.origin("unknown.cmd"), None);
    }
}
