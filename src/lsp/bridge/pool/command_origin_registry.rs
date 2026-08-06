//! Remembers which raw `workspace/executeCommand` names downstream servers
//! advertised, and which of those can be RECONNECTED (#628 palette commands).
//!
//! A command surfaced in a bridged code action routes by its NAME-encoded
//! envelope (`command_routing`). But a command the client fires WITHOUT an
//! action context — from the command palette, keyed off the advertised
//! `executeCommandProvider.commands` list — arrives as the RAW downstream name.
//! Bridged action commands embed their exact origin and do not use this registry.
//!
//! Two questions, two shapes:
//!
//! - **"Does the editor know this name?"** — entry PRESENCE. `dispatch_execute_command`
//!   asks before decoding, because a downstream's advertised ids are arbitrary
//!   strings and one could be shaped like a routed name. Names are never removed:
//!   nothing un-registers them with the editor, so the palette can fire one for
//!   the whole session and the gate has to keep recognising it.
//! - **"If nothing is live, what can I reconnect?"** — the entry's VALUE. Only
//!   `ConnectionKey::is_client_fallback` keys are recorded, because those are the
//!   only ones the dispatcher's reconnect branch can revive: acquiring with
//!   `document_uri: None` round-trips to that exact key, while a marker-rooted or
//!   shared key cannot be rebuilt without a document. Storing a key the branch
//!   will reject anyway would let it out-vote the one key that works.
//!
//! Which connection actually RUNS a live command is not decided here — the
//! dispatcher scans the connections map for handles whose exact advertised list
//! contains the name, which is the only view that cannot lag a handshake.

use std::collections::HashMap;
use std::sync::Mutex;

use super::ConnectionKey;
use crate::error::LockResultExt;

#[derive(Default)]
pub(crate) struct CommandOriginRegistry {
    /// Command name → the client-fallback connections that advertised it, in
    /// first-advertisement order. An EMPTY vec is meaningful: the name is known
    /// to the editor, but every advertiser was marker-rooted or shared and so
    /// cannot be reconnected from the name alone.
    origins: Mutex<HashMap<String, Vec<ConnectionKey>>>,
}

impl CommandOriginRegistry {
    /// Record `commands` as advertised by the connection `key`, returning the
    /// subset that is NEWLY seen (never registered before).
    ///
    /// Only genuinely new command names are returned: the name is already
    /// registered with the editor after its first advertisement, so
    /// re-advertising it — by the same connection, a respawn, or a second
    /// server — must not produce a duplicate registration.
    pub(crate) fn register(&self, key: &ConnectionKey, commands: Vec<String>) -> Vec<String> {
        let mut origins = self
            .origins
            .lock()
            .recover_poison("CommandOriginRegistry::register");
        let mut added = Vec::new();
        for command in commands {
            // Recording a non-reconnectable key would only ever subtract: it
            // cannot be revived, but it could make the reconnectable set look
            // ambiguous and veto the key that works.
            let candidate = key.is_client_fallback();
            match origins.get_mut(&command) {
                Some(existing) => {
                    if candidate && !existing.contains(key) {
                        existing.push(key.clone());
                    }
                }
                None => {
                    let seed = if candidate {
                        vec![key.clone()]
                    } else {
                        Vec::new()
                    };
                    // Clone only for the map key; move the name itself into `added`.
                    origins.insert(command.clone(), seed);
                    added.push(command);
                }
            }
        }
        added
    }

    /// The connections that advertised `command` AND can be reconnected from the
    /// name alone, in first-advertisement order.
    ///
    /// Empty both when the name is unknown and when every advertiser was
    /// marker-rooted — the dispatcher must not read emptiness as "unknown"; that
    /// is [`Self::is_registered`]'s question.
    pub(crate) fn reconnectable_origins(&self, command: &str) -> Vec<ConnectionKey> {
        self.origins
            .lock()
            .recover_poison("CommandOriginRegistry::reconnectable_origins")
            .get(command)
            .cloned()
            .unwrap_or_default()
    }

    /// Whether `command` is a raw name some downstream advertised.
    ///
    /// Separate from [`Self::reconnectable_origins`] because this runs on EVERY
    /// `workspace/executeCommand` as the decode gate, and because a name with no
    /// reconnectable origin is still a name the editor knows.
    pub(crate) fn is_registered(&self, command: &str) -> bool {
        self.origins
            .lock()
            .recover_poison("CommandOriginRegistry::is_registered")
            .contains_key(command)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fallback(server: &str) -> ConnectionKey {
        ConnectionKey::new(server, None)
    }

    fn marker(server: &str, root: &str) -> ConnectionKey {
        ConnectionKey::new(server, Some(root.to_string()))
    }

    #[test]
    fn register_reports_only_new_names_and_same_origin_is_idempotent() {
        let reg = CommandOriginRegistry::default();
        let ruff = fallback("ruff");

        assert_eq!(
            reg.register(&ruff, vec!["ruff.fix".to_string(), "ruff.sort".to_string()]),
            vec!["ruff.fix".to_string(), "ruff.sort".to_string()]
        );
        assert_eq!(reg.reconnectable_origins("ruff.fix"), vec![ruff.clone()]);

        // The same connection advertising again neither duplicates the editor
        // registration nor makes its route ambiguous.
        assert!(reg.register(&ruff, vec!["ruff.fix".to_string()]).is_empty());
        assert_eq!(reg.reconnectable_origins("ruff.fix"), vec![ruff]);
    }

    #[test]
    fn an_unknown_name_is_neither_registered_nor_reconnectable() {
        let reg = CommandOriginRegistry::default();
        assert!(!reg.is_registered("unknown.cmd"));
        assert!(reg.reconnectable_origins("unknown.cmd").is_empty());
    }

    #[test]
    fn collision_registers_the_name_once_and_retains_both_fallback_origins() {
        let reg = CommandOriginRegistry::default();
        let ruff = fallback("ruff");
        let eslint = fallback("eslint");

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
            reg.reconnectable_origins("source.fixAll"),
            vec![ruff, eslint]
        );
    }

    #[test]
    fn re_advertisement_inside_a_collision_neither_duplicates_nor_reorders() {
        let reg = CommandOriginRegistry::default();
        let ruff = fallback("ruff");
        let eslint = fallback("eslint");
        reg.register(&ruff, vec!["source.fixAll".to_string()]);
        reg.register(&eslint, vec!["source.fixAll".to_string()]);

        assert!(
            reg.register(&ruff, vec!["source.fixAll".to_string()])
                .is_empty()
        );

        assert_eq!(
            reg.reconnectable_origins("source.fixAll"),
            vec![ruff, eslint],
            "a respawn must not append a second entry nor move itself to the back"
        );
    }

    #[test]
    fn a_marker_rooted_advertiser_registers_the_name_without_becoming_a_candidate() {
        let reg = CommandOriginRegistry::default();
        let marker_rooted = marker("ruff", "file:///w/a");

        assert_eq!(
            reg.register(&marker_rooted, vec!["ruff.fix".to_string()]),
            vec!["ruff.fix"],
            "the editor is still told about the name"
        );
        assert!(reg.is_registered("ruff.fix"));
        assert!(
            reg.reconnectable_origins("ruff.fix").is_empty(),
            "a marker-rooted key cannot be rebuilt without a document, so it is \
             not a reconnect candidate"
        );
    }

    #[test]
    fn a_marker_rooted_advertiser_cannot_out_vote_the_reconnectable_one() {
        // The regression this shape exists to prevent: a key the reconnect
        // branch would reject anyway must not make the set look ambiguous and
        // cost the one key that works.
        let reg = CommandOriginRegistry::default();
        reg.register(&marker("ruff", "file:///w/a"), vec!["ruff.fix".to_string()]);
        let reconnectable = fallback("ruff");
        reg.register(&reconnectable, vec!["ruff.fix".to_string()]);

        assert_eq!(
            reg.reconnectable_origins("ruff.fix"),
            vec![reconnectable],
            "the sole revivable origin must survive alongside a dead marker root"
        );
    }
}
