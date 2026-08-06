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

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Mutex;

use super::ConnectionKey;
use crate::error::LockResultExt;
use crate::lsp::bridge::protocol::encode_command;

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
    /// The connections whose ENCODED form of this name the editor was asked to
    /// hold. Per connection, unlike `registered_upstream`: an encoded entry
    /// names one connection, so each gets its own palette row and its own
    /// retirement.
    ///
    /// "Asked to hold", not "advertises": a connection is added when its
    /// handshake advertises the name AND the routed entry goes out, removed
    /// when it stops advertising it, when its server leaves the config, or when
    /// the request could not be sent at all. So it tracks the editor's side, not
    /// the downstream's.
    encoded_registered: HashSet<ConnectionKey>,
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
    /// Raw names the editor should now be told about.
    pub(crate) newly_advertised: Vec<String>,
    /// Raw names this handshake left with no advertiser at all, so the editor
    /// should stop offering them.
    pub(crate) orphaned: Vec<String>,
    /// Encoded, connection-specific names the editor should now be told about —
    /// the unambiguous sibling of every raw entry this connection contributes.
    pub(crate) newly_encoded: Vec<String>,
    /// Encoded names this connection no longer backs.
    pub(crate) retired_encoded: Vec<String>,
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
        let (orphaned, retired_encoded) = Self::reconcile(&mut origins, key, &commands);
        let mut newly_advertised = Vec::new();
        let mut newly_encoded = Vec::new();
        for command in commands {
            // Never ANNOUNCE a name whose text is already claimed by the other
            // kind — checked against the WHOLE registry, because the two owners
            // need not come from the same connection or the same handshake.
            //
            // The registration id is derived from the final text, so two owners
            // of one string share one id, and either retiring would unregister
            // what the other still needs. Nothing usable is lost by withholding:
            // dispatch refuses that exact text anyway, since it cannot tell which
            // of the two readings the user picked.
            let encoded = encode_command(key, &command);
            let routed_text_is_a_known_raw_name = origins.contains_key(&encoded);
            let raw_text_is_a_held_routed_name =
                crate::lsp::bridge::protocol::decode_command(&command).is_some_and(|route| {
                    origins
                        .get(route.command)
                        .is_some_and(|entry| entry.encoded_registered.contains(&route.key))
                });

            let entry = origins.entry(command.clone()).or_default();
            entry.servers.insert(key.server().to_string());
            // Recording a non-reconnectable key would only ever subtract: it
            // cannot be revived, but it could make the reconnectable set look
            // ambiguous and veto the key that works.
            if key.is_client_fallback() && !entry.reconnectable.contains(key) {
                entry.reconnectable.push(key.clone());
            }
            if entry.encoded_registered.insert(key.clone()) && !routed_text_is_a_known_raw_name {
                newly_encoded.push(encoded);
            }
            if !entry.registered_upstream && !raw_text_is_a_held_routed_name {
                entry.registered_upstream = true;
                newly_advertised.push(command);
            }
        }
        RegistrationOutcome {
            newly_advertised,
            orphaned,
            newly_encoded,
            retired_encoded,
        }
    }

    /// Undo the registration bookkeeping for names whose request was never sent.
    ///
    /// The flag means "the editor was asked to hold this", so it must not be set
    /// when nothing went out — a client that cannot accept dynamic registration,
    /// or a forwarding loop that has gone away. Otherwise the registry would
    /// later try to retire an id the editor never held, and would refuse to
    /// re-offer the name if the client's capability arrived by another route.
    /// Take back the bookkeeping for an announcement that never reached the
    /// editor — an unsendable request, or a client that cannot accept one.
    ///
    /// The two halves are taken back SEPARATELY because they are independent.
    /// A second connection advertising a name the first already announced
    /// contributes a routed entry and NO raw one, so keying the rollback off the
    /// raw list would silently retain a routed row the editor never received —
    /// and nothing would ever re-offer it. Conversely a raw name may be shared,
    /// so clearing it from this connection's failure must not be inferred from a
    /// routed rollback either.
    pub(crate) fn forget_registration(&self, outcome: &RegistrationOutcome) {
        let mut origins = self
            .origins
            .lock()
            .recover_poison("CommandOriginRegistry::forget_registration");
        for command in &outcome.newly_advertised {
            if let Some(entry) = origins.get_mut(command) {
                entry.registered_upstream = false;
            }
        }
        for encoded in &outcome.newly_encoded {
            // The routed name carries its own owner, so it needs no separate
            // key argument: decoding gives back exactly the (connection, command)
            // pair that minted it.
            if let Some(route) = crate::lsp::bridge::protocol::decode_command(encoded)
                && let Some(entry) = origins.get_mut(route.command)
            {
                entry.encoded_registered.remove(&route.key);
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
    ) -> (Vec<String>, Vec<String>) {
        // One set, not a linear scan per known name: this runs under the lock on
        // every handshake, against a map that keeps every name for the session.
        let advertised: std::collections::HashSet<&str> =
            commands.iter().map(String::as_str).collect();
        let mut orphaned = Vec::new();
        let mut retired_encoded = Vec::new();
        for (command, entry) in origins.iter_mut() {
            if advertised.contains(command.as_str()) {
                continue;
            }
            entry.reconnectable.retain(|recorded| recorded != key);
            if entry.encoded_registered.remove(key) {
                retired_encoded.push(encode_command(key, command));
            }
            // Drop the SERVER only once none of its connections advertise the
            // name any more. One connection's handshake speaks for itself, not
            // for its siblings: the same server rooted at two workspace folders
            // is two connections, and the first to drop a command must not
            // retire an entry the second still backs.
            if !entry
                .encoded_registered
                .iter()
                .any(|live| live.server() == key.server())
            {
                entry.servers.remove(key.server());
            }
            if entry.registered_upstream && entry.servers.is_empty() {
                entry.registered_upstream = false;
                orphaned.push(command.clone());
            }
        }
        (orphaned, retired_encoded)
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
        let mut spawnable: HashMap<String, bool> = HashMap::new();
        for entry in origins.values() {
            let servers = entry
                .servers
                .iter()
                .map(String::as_str)
                .chain(entry.encoded_registered.iter().map(ConnectionKey::server));
            for server in servers {
                if !spawnable.contains_key(server) {
                    spawnable.insert(server.to_string(), is_spawnable(server));
                }
            }
        }
        let mut retired = Vec::new();
        for (command, entry) in origins.iter_mut() {
            // Routed entries name a connection, so each dies with ITS OWN
            // server. They are retired independently of the raw name, which
            // survives as long as any advertiser does — otherwise a removed
            // server's routed row would be left visible and broken whenever a
            // second server kept the raw name alive.
            let dead_keys: Vec<_> = entry
                .encoded_registered
                .iter()
                .filter(|key| !spawnable.get(key.server()).copied().unwrap_or(false))
                .cloned()
                .collect();
            for key in dead_keys {
                entry.encoded_registered.remove(&key);
                retired.push(encode_command(&key, command));
            }
            if entry.registered_upstream
                && !entry.servers.is_empty()
                && !entry
                    .servers
                    .iter()
                    .any(|server| spawnable.get(server.as_str()).copied().unwrap_or(false))
            {
                entry.registered_upstream = false;
                retired.push(command.clone());
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

    /// Whether `name` is a ROUTED entry kakehashi minted and the editor still
    /// holds — the exact question, not "does this string happen to decode".
    ///
    /// A downstream is free to advertise a raw id shaped like a routed name.
    /// That is only a collision if a routed entry of the same text actually
    /// exists; otherwise there is one reading and it routes fine, which is what
    /// happened before these entries were registered.
    pub(crate) fn holds_encoded(&self, name: &str) -> bool {
        let Some(route) = crate::lsp::bridge::protocol::decode_command(name) else {
            return false;
        };
        self.origins
            .lock()
            .recover_poison("CommandOriginRegistry::holds_encoded")
            .get(route.command)
            .is_some_and(|entry| entry.encoded_registered.contains(&route.key))
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

        // Config drops ruff: nothing can advertise the name any more, so BOTH
        // the raw entry and ruff's routed one die.
        let mut retired = reg.retire_unadvertisable(|_| false);
        retired.sort();
        assert_eq!(
            retired,
            vec![
                "kakehashi|c|ruff||ruff.fix".to_string(),
                "ruff.fix".to_string()
            ]
        );
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
    fn every_connection_gets_its_own_encoded_entry() {
        // The escape hatch a refusal leaves the user without: a raw name shared
        // by two roots is unresolvable, but each root's encoded entry names its
        // own connection, so picking one IS the disambiguation.
        let reg = CommandOriginRegistry::default();
        let a = marker("ruff", "file:///repo-a");
        let b = marker("ruff", "file:///repo-b");

        let first = reg.register(&a, vec!["ruff.fix".to_string()]);
        let second = reg.register(&b, vec!["ruff.fix".to_string()]);

        assert_eq!(first.newly_advertised, vec!["ruff.fix".to_string()]);
        assert!(
            second.newly_advertised.is_empty(),
            "the raw name is still announced exactly once"
        );
        assert_eq!(first.newly_encoded.len(), 1);
        assert_eq!(second.newly_encoded.len(), 1);
        assert_ne!(
            first.newly_encoded, second.newly_encoded,
            "two roots must get two distinguishable entries"
        );
        for encoded in first.newly_encoded.iter().chain(&second.newly_encoded) {
            assert!(encoded.contains("ruff.fix"), "{encoded}");
        }
    }

    #[test]
    fn a_routed_only_announcement_that_was_never_sent_is_taken_back() {
        // The rollback shape that keying off the RAW list misses entirely: the
        // second connection contributes a routed entry and no raw one, because
        // the first already announced the raw name.
        let reg = CommandOriginRegistry::default();
        let a = marker("ruff", "file:///repo-a");
        let b = marker("ruff", "file:///repo-b");
        reg.register(&a, vec!["ruff.fix".to_string()]);

        let second = reg.register(&b, vec!["ruff.fix".to_string()]);
        assert!(
            second.newly_advertised.is_empty(),
            "the raw name was already announced by repo-a"
        );
        assert_eq!(second.newly_encoded.len(), 1);

        reg.forget_registration(&second);

        assert_eq!(
            reg.register(&b, vec!["ruff.fix".to_string()]).newly_encoded,
            second.newly_encoded,
            "a routed entry the editor never received must be offered again"
        );
    }

    #[test]
    fn a_text_two_owners_could_claim_is_announced_by_at_most_one() {
        // One registration id is derived from the final text, so two owners of
        // one string would share it and either retiring would unregister what
        // the other still needs. At most one owner is the property that matters;
        // WHICH one depends on advertisement order and does not.
        //
        // The survivor is unusable either way — dispatch refuses that exact text,
        // since it cannot tell the two readings apart — so nothing the user could
        // have run is lost.
        let reg = CommandOriginRegistry::default();
        let srv = fallback("srv");
        let collide = encode_command(&srv, "foo");

        let outcome = reg.register(&srv, vec!["foo".to_string(), collide.clone()]);

        let owners = outcome
            .newly_advertised
            .iter()
            .chain(&outcome.newly_encoded)
            .filter(|name| **name == collide)
            .count();
        assert_eq!(
            owners, 1,
            "two owners would share one registration id: {outcome:?}"
        );
        assert!(
            reg.is_registered(&collide),
            "it is still a name the decode gate has to recognise"
        );
    }

    #[test]
    fn re_advertising_from_the_same_connection_adds_no_second_encoded_entry() {
        let reg = CommandOriginRegistry::default();
        let a = marker("ruff", "file:///repo-a");
        reg.register(&a, vec!["ruff.fix".to_string()]);

        assert!(
            reg.register(&a, vec!["ruff.fix".to_string()])
                .newly_encoded
                .is_empty(),
            "a respawn under the same key must not duplicate its palette row"
        );
    }

    #[test]
    fn one_connection_dropping_a_command_retires_only_its_own_encoded_entry() {
        let reg = CommandOriginRegistry::default();
        let a = marker("ruff", "file:///repo-a");
        let b = marker("ruff", "file:///repo-b");
        reg.register(&a, vec!["ruff.fix".to_string()]);
        let b_entry = reg.register(&b, vec!["ruff.fix".to_string()]).newly_encoded;

        let outcome = reg.register(&b, Vec::new());

        assert_eq!(outcome.retired_encoded, b_entry);
        assert!(
            outcome.orphaned.is_empty(),
            "the raw name survives — repo-a still advertises it"
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

        reg.forget_registration(&outcome);

        let again = reg.register(&ruff, vec!["ruff.fix".to_string()]);
        assert_eq!(
            again.newly_advertised,
            vec!["ruff.fix".to_string()],
            "a name that was never announced must be announceable again"
        );
        assert_eq!(
            again.newly_encoded, outcome.newly_encoded,
            "the routed entry rode the same unsent request, so it must come back too"
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

        // Deleting ruff leaves the RAW name real — eslint still advertises it —
        // but ruff's routed entry names a connection that is gone, so it must
        // not be left visible and broken.
        assert_eq!(
            reg.retire_unadvertisable(|server| server == "eslint"),
            vec!["kakehashi|c|ruff||source.fixAll".to_string()],
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
