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

use std::collections::{BTreeSet, HashMap};
use std::sync::Mutex;

use super::ConnectionKey;
use crate::error::LockResultExt;

/// What is known about one advertised raw command name.
#[derive(Default)]
struct CommandOrigins {
    /// Every server that has advertised this name. Server names, not connection
    /// keys, because the question they answer is "could ANY currently-configured
    /// server still produce this command?" — which survives respawns, re-roots,
    /// and the loss of every connection.
    servers: BTreeSet<String>,
    /// The client-fallback connections that advertised it, in first-advertisement
    /// order. Only these, because only these can be revived from the name alone.
    reconnectable: Vec<ConnectionKey>,
    /// Whether the editor currently holds a registration for this name.
    ///
    /// Distinct from the entry existing. The entry outlives the registration:
    /// once retired, the name must still be recognised by the decode gate (a
    /// client can fire a name it cached or a user bound), and must be
    /// re-registered rather than skipped if a server advertises it again.
    registered_upstream: bool,
}

#[derive(Default)]
pub(crate) struct CommandOriginRegistry {
    origins: Mutex<HashMap<String, CommandOrigins>>,
}

/// What a handshake's advertisement changed.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct RegistrationOutcome {
    /// Names the editor should now be told about.
    pub(crate) newly_advertised: Vec<String>,
    /// Names this handshake left with no advertiser at all, so the editor should
    /// stop offering them.
    pub(crate) orphaned: Vec<String>,
}

impl CommandOriginRegistry {
    /// Reconcile `key`'s advertisement against what it used to advertise, and
    /// record the result.
    ///
    /// `commands` is the WHOLE list this handshake advertised, empty included: a
    /// server that came back without `executeCommandProvider` has stopped
    /// offering everything, and treating "no list" as "no news" would leave the
    /// previous incarnation's claims standing.
    ///
    /// Re-advertising a name the editor already knows returns nothing, so no
    /// duplicate registration is produced. A name whose registration was RETIRED
    /// does come back, because a server advertising it again is exactly the
    /// event that makes it real once more.
    pub(crate) fn register(
        &self,
        key: &ConnectionKey,
        commands: Vec<String>,
    ) -> RegistrationOutcome {
        let mut origins = self
            .origins
            .lock()
            .recover_poison("CommandOriginRegistry::register");
        // Reconcile BEFORE recording: this handshake is the current truth about
        // what `key` serves.
        let orphaned = Self::reconcile(&mut origins, key, &commands);
        let mut newly_advertised = Vec::new();
        for command in commands {
            let entry = origins.entry(command.clone()).or_default();
            entry.servers.insert(key.server().to_string());
            // Recording a non-reconnectable key would only ever subtract: it
            // cannot be revived, but it could make the reconnectable set look
            // ambiguous and veto the key that works.
            if key.is_client_fallback() && !entry.reconnectable.contains(key) {
                entry.reconnectable.push(key.clone());
            }
            if !entry.registered_upstream {
                entry.registered_upstream = true;
                newly_advertised.push(command);
            }
        }
        RegistrationOutcome {
            newly_advertised,
            orphaned,
        }
    }

    /// Undo the registration bookkeeping for names whose request was never sent.
    ///
    /// The flag means "the editor was asked to hold this", so it must not be set
    /// when nothing went out — a client that cannot accept dynamic registration,
    /// or a forwarding loop that has gone away. Otherwise the registry would
    /// later try to retire an id the editor never held, and would refuse to
    /// re-offer the name if the client's capability arrived by another route.
    pub(crate) fn forget_registration(&self, commands: &[String]) {
        let mut origins = self
            .origins
            .lock()
            .recover_poison("CommandOriginRegistry::forget_registration");
        for command in commands {
            if let Some(entry) = origins.get_mut(command) {
                entry.registered_upstream = false;
            }
        }
    }

    /// Drop `key`'s claims on every name NOT in `commands`, returning the names
    /// that lost their LAST advertiser as a result.
    ///
    /// A recorded origin is a claim that this connection serves the name, and a
    /// handshake is the only moment that claim is checked against reality. A
    /// server that comes back after an upgrade without a command falsifies it.
    /// Left in place the stale candidate cannot be reached — the exact-name check
    /// rejects it — but it still COUNTS, so it can make an otherwise-revivable
    /// name look ambiguous and refuse it for the rest of the session.
    ///
    /// The SERVER is dropped too, not just the connection key, which is what
    /// lets an in-place upgrade retire a name: every instance of one server runs
    /// the same binary and so advertises the same list, and the config-driven
    /// sweep cannot see a name disappear while its server stays configured.
    fn reconcile(
        origins: &mut HashMap<String, CommandOrigins>,
        key: &ConnectionKey,
        commands: &[String],
    ) -> Vec<String> {
        // One set, not a linear scan per known name: this runs under the lock on
        // every handshake, against a map that keeps every name for the session.
        let advertised: std::collections::HashSet<&str> =
            commands.iter().map(String::as_str).collect();
        let mut orphaned = Vec::new();
        for (command, entry) in origins.iter_mut() {
            if advertised.contains(command.as_str()) {
                continue;
            }
            entry.reconnectable.retain(|recorded| recorded != key);
            entry.servers.remove(key.server());
            if entry.registered_upstream && entry.servers.is_empty() {
                entry.registered_upstream = false;
                orphaned.push(command.clone());
            }
        }
        orphaned
    }

    /// The names whose every advertiser is gone from `is_spawnable`, marking them
    /// unregistered so a later advertisement re-registers them.
    ///
    /// Called on a settings change, which is the only event that can retire a
    /// name. Connection purges cannot: every removal site in the pool arms a
    /// re-open for the same key, so the connection is expected back and
    /// unregistering there would churn the editor's palette on each respawn.
    ///
    /// A name with NO recorded server is left alone rather than retired — that
    /// is a shape `register` cannot produce, and guessing at it would drop a
    /// registration the editor still holds.
    pub(crate) fn retire_unadvertisable(&self, is_spawnable: impl Fn(&str) -> bool) -> Vec<String> {
        let mut origins = self
            .origins
            .lock()
            .recover_poison("CommandOriginRegistry::retire_unadvertisable");
        // Ask about each distinct SERVER once, before touching any entry. The
        // caller answers from a full wildcard merge of the config, and one
        // server typically advertises many commands — asking per (name, server)
        // pair would pay for that merge once per command on every reload.
        let mut spawnable: HashMap<&str, bool> = HashMap::new();
        for entry in origins.values() {
            for server in &entry.servers {
                if !spawnable.contains_key(server.as_str()) {
                    spawnable.insert(server.as_str(), is_spawnable(server));
                }
            }
        }
        let retired: Vec<String> = origins
            .iter()
            .filter(|(_, entry)| {
                entry.registered_upstream
                    && !entry.servers.is_empty()
                    && !entry
                        .servers
                        .iter()
                        .any(|server| spawnable[server.as_str()])
            })
            .map(|(command, _)| command.clone())
            .collect();
        for command in &retired {
            if let Some(entry) = origins.get_mut(command) {
                entry.registered_upstream = false;
            }
        }
        retired
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
            .map(|entry| entry.reconnectable.clone())
            .unwrap_or_default()
    }

    /// Whether `command` is a raw name some downstream advertised.
    ///
    /// Deliberately entry PRESENCE, not `registered_upstream`: this is the decode
    /// gate, and its job is to stop a downstream id shaped like a routing token
    /// from being decoded and misrouted. A retired name is still such an id, and
    /// a client can still fire one from a keybind or a stale cache.
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
            reg.register(&ruff, vec!["ruff.fix".to_string(), "ruff.sort".to_string()])
                .newly_advertised,
            vec!["ruff.fix".to_string(), "ruff.sort".to_string()]
        );
        assert_eq!(reg.reconnectable_origins("ruff.fix"), vec![ruff.clone()]);

        // The same connection advertising again neither duplicates the editor
        // registration nor makes its route ambiguous.
        assert!(
            reg.register(&ruff, vec!["ruff.fix".to_string()])
                .newly_advertised
                .is_empty()
        );
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
            reg.register(&ruff, vec!["source.fixAll".to_string()])
                .newly_advertised,
            vec!["source.fixAll"]
        );
        assert!(
            reg.register(&eslint, vec!["source.fixAll".to_string()])
                .newly_advertised
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
                .newly_advertised
                .is_empty()
        );

        assert_eq!(
            reg.reconnectable_origins("source.fixAll"),
            vec![ruff, eslint],
            "a respawn must not append a second entry nor move itself to the back"
        );
    }

    #[test]
    fn a_name_is_registered_once_until_it_is_retired() {
        let reg = CommandOriginRegistry::default();
        let ruff = fallback("ruff");
        assert_eq!(
            reg.register(&ruff, vec!["ruff.fix".to_string()])
                .newly_advertised,
            vec!["ruff.fix"]
        );
        assert!(
            reg.register(&ruff, vec!["ruff.fix".to_string()])
                .newly_advertised
                .is_empty()
        );

        // Config drops ruff: nothing can advertise the name any more.
        assert_eq!(reg.retire_unadvertisable(|_| false), vec!["ruff.fix"]);
        assert!(
            reg.is_registered("ruff.fix"),
            "the decode gate must keep recognising a retired name — a client can \
             still fire one from a keybind or a stale cache"
        );
        assert!(
            reg.retire_unadvertisable(|_| false).is_empty(),
            "retiring twice would unregister an id the editor no longer holds"
        );

        // ruff comes back, so the name becomes real again and must be re-offered.
        assert_eq!(
            reg.register(&ruff, vec!["ruff.fix".to_string()])
                .newly_advertised,
            vec!["ruff.fix"],
            "a retired name must be re-registered, not skipped as already known"
        );
    }

    #[test]
    fn a_server_that_came_back_with_no_provider_at_all_is_reconciled() {
        // The shape an upgrade takes when it drops `executeCommandProvider`
        // entirely. Treating "no list" as "no news" would leave the previous
        // incarnation's claims standing forever.
        let reg = CommandOriginRegistry::default();
        let ruff = fallback("ruff");
        reg.register(&ruff, vec!["ruff.fix".to_string()]);

        let outcome = reg.register(&ruff, Vec::new());

        assert_eq!(
            outcome.orphaned,
            vec!["ruff.fix".to_string()],
            "its last advertiser stopped offering it, so the editor must stop too"
        );
        assert!(reg.reconnectable_origins("ruff.fix").is_empty());
        assert!(
            reg.is_registered("ruff.fix"),
            "the decode gate keeps recognising it; only the registration is gone"
        );
    }

    #[test]
    fn an_in_place_upgrade_retires_a_command_its_server_no_longer_has() {
        // The config-driven sweep cannot see this: the server is still perfectly
        // well configured, it just stopped offering the command.
        let reg = CommandOriginRegistry::default();
        let ruff = fallback("ruff");
        reg.register(&ruff, vec!["old.fix".to_string(), "ruff.fix".to_string()]);
        assert!(
            reg.retire_unadvertisable(|_| true).is_empty(),
            "while the server is configured, nothing config-driven can retire it"
        );

        let outcome = reg.register(&ruff, vec!["ruff.fix".to_string()]);

        assert_eq!(outcome.orphaned, vec!["old.fix".to_string()]);
        assert!(
            outcome.newly_advertised.is_empty(),
            "ruff.fix was already registered and must not be re-announced"
        );
    }

    #[test]
    fn one_server_dropping_a_command_another_still_has_retires_nothing() {
        let reg = CommandOriginRegistry::default();
        let ruff = fallback("ruff");
        reg.register(&ruff, vec!["source.fixAll".to_string()]);
        reg.register(&fallback("eslint"), vec!["source.fixAll".to_string()]);

        let outcome = reg.register(&ruff, Vec::new());

        assert!(
            outcome.orphaned.is_empty(),
            "eslint still advertises it, so the palette entry is still real"
        );
        assert!(reg.is_registered("source.fixAll"));
    }

    #[test]
    fn a_registration_that_was_never_sent_is_not_remembered_as_sent() {
        // The client cannot accept dynamic registration, or the forwarding loop
        // is gone: nothing went out, so the editor holds nothing. Believing
        // otherwise would later retire an id it never had, and would refuse to
        // offer the name if the capability arrived by another route.
        let reg = CommandOriginRegistry::default();
        let ruff = fallback("ruff");
        let outcome = reg.register(&ruff, vec!["ruff.fix".to_string()]);
        assert_eq!(outcome.newly_advertised, vec!["ruff.fix".to_string()]);

        reg.forget_registration(&outcome.newly_advertised);

        assert_eq!(
            reg.register(&ruff, vec!["ruff.fix".to_string()])
                .newly_advertised,
            vec!["ruff.fix".to_string()],
            "a name that was never announced must be announceable again"
        );
    }

    #[test]
    fn each_server_is_asked_about_once_per_reload() {
        // The answer comes from a full wildcard merge, so asking per
        // (name, server) pair makes a reload cost O(commands) merges per server.
        let reg = CommandOriginRegistry::default();
        let ruff = fallback("ruff");
        reg.register(
            &ruff,
            vec![
                "ruff.fix".to_string(),
                "ruff.sort".to_string(),
                "ruff.format".to_string(),
            ],
        );
        reg.register(&fallback("eslint"), vec!["eslint.fix".to_string()]);

        let asked = std::cell::RefCell::new(Vec::new());
        reg.retire_unadvertisable(|server| {
            asked.borrow_mut().push(server.to_string());
            false
        });

        let mut asked = asked.into_inner();
        asked.sort();
        assert_eq!(asked, vec!["eslint".to_string(), "ruff".to_string()]);
    }

    #[test]
    fn a_name_one_surviving_server_still_advertises_is_not_retired() {
        let reg = CommandOriginRegistry::default();
        reg.register(&fallback("ruff"), vec!["source.fixAll".to_string()]);
        reg.register(&fallback("eslint"), vec!["source.fixAll".to_string()]);

        assert!(
            reg.retire_unadvertisable(|server| server == "eslint")
                .is_empty(),
            "deleting one of two advertisers leaves the name real"
        );
        assert!(
            reg.retire_unadvertisable(|_| false)
                .contains(&"source.fixAll".to_string())
        );
    }

    #[test]
    fn a_respawn_that_dropped_a_command_stops_being_a_candidate_for_it() {
        // The false-ambiguity shape: two fallback servers advertise a name, one
        // comes back after an upgrade without it, the other dies. The name has
        // exactly one revivable origin left and must not read as ambiguous.
        let reg = CommandOriginRegistry::default();
        let ruff = fallback("ruff");
        let eslint = fallback("eslint");
        reg.register(&ruff, vec!["source.fixAll".to_string()]);
        reg.register(&eslint, vec!["source.fixAll".to_string()]);
        assert_eq!(
            reg.reconnectable_origins("source.fixAll"),
            vec![ruff.clone(), eslint.clone()]
        );

        // eslint re-handshakes advertising something else entirely.
        reg.register(&eslint, vec!["eslint.restart".to_string()]);

        assert_eq!(
            reg.reconnectable_origins("source.fixAll"),
            vec![ruff],
            "a server that no longer advertises the name must not keep a vote on it"
        );
        assert!(
            reg.is_registered("source.fixAll"),
            "the name stays registered — another server may still advertise it"
        );
    }

    #[test]
    fn a_marker_rooted_advertiser_registers_the_name_without_becoming_a_candidate() {
        let reg = CommandOriginRegistry::default();
        let marker_rooted = marker("ruff", "file:///w/a");

        assert_eq!(
            reg.register(&marker_rooted, vec!["ruff.fix".to_string()])
                .newly_advertised,
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
