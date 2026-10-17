//! The bridge's `kakehashi/virtualDocument/prepare` layer: which pairs are
//! prepared, by which candidates, and the answers' registry.
//!
//! Present on a [`super::BridgeCoordinator`] only with experimental features
//! on (`KAKEHASHI_EXPERIMENTAL=true`); without it no document is prepared and
//! no path asks anything of it.

use std::sync::Arc;

use dashmap::{DashMap, DashSet};
use url::Url;

use super::ResolvedServerConfig;
use super::pool::{LanguageServerPool, RegionPrepare};
use super::prepare_registry::{PrepareInput, PrepareLookup, PrepareMiss, PrepareRegistry, Resync};
use crate::config::WorkspaceSettings;
use crate::text::terminal::escape_terminal_controls;

/// A pair's candidates, `None` for a pair with none.
type Candidates = Option<Arc<[ResolvedServerConfig]>>;

/// The settings prepare targets are resolved from, with the targets resolved
/// so far (this runs per bridged region in every lifecycle pass and request).
struct Targets {
    settings: Arc<WorkspaceSettings>,
    /// Host language → injection language → the pair's candidates: nested,
    /// so a hit borrows both names instead of allocating a key.
    resolved: DashMap<String, DashMap<String, Candidates>>,
}

pub(crate) struct PrepareLayer {
    registry: PrepareRegistry,
    targets: arc_swap::ArcSwap<Targets>,
    /// Pairs already warned about a configured strategy other than
    /// `preferred`, per settings.
    warned_strategy: DashSet<(String, String)>,
    /// Hosts an editor's pull answered without documents then waiting for
    /// their prepare answers: owed a refresh once they are sent. One per
    /// host: each refresh's re-pull records it again while anything waits.
    pull_debts: DashSet<Url>,
    /// Saves held virtual documents missed: forwarded to each once its
    /// prepared text is sent, while the host is still at the saved version.
    held_saves: DashMap<Url, HeldSave>,
}

/// A save that reached a host while some of its virtual documents were held
/// back for their prepare answers.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct HeldSave {
    pub(crate) incarnation: u64,
    pub(crate) content_version: u64,
    pub(crate) region_ids: std::collections::HashSet<String>,
}

impl PrepareLayer {
    pub(crate) fn new() -> Self {
        Self {
            registry: PrepareRegistry::default(),
            targets: arc_swap::ArcSwap::new(Arc::new(Targets {
                settings: Arc::default(),
                resolved: DashMap::new(),
            })),
            warned_strategy: DashSet::new(),
            pull_debts: DashSet::new(),
            held_saves: DashMap::new(),
        }
    }
}

impl PrepareLayer {
    /// Take `settings` as what prepare targets resolve from, moving the
    /// prepare epoch when they differ from those before: a pair's peer, or
    /// its launch, may have changed, and the answers given under the old
    /// ones stand no more.
    pub(crate) fn settings_changed(
        &self,
        pool: &LanguageServerPool,
        settings: &Arc<WorkspaceSettings>,
    ) {
        let current = self.targets.load();
        if *current.settings == **settings {
            return;
        }
        self.targets.store(Arc::new(Targets {
            settings: Arc::clone(settings),
            resolved: DashMap::new(),
        }));
        self.warned_strategy.clear();
        // After the settings are in: a target resolved under the epoch read
        // before this move asks nothing (see `target`).
        pool.forget_prepare_start_failures(None);
        pool.advance_prepare_epoch();
    }
}

impl super::BridgeCoordinator {
    /// The servers that may prepare `injection_language` virtual documents
    /// in `host_language` hosts: those the pair's
    /// `kakehashi/virtualDocument/prepare` aggregation `priorities` admit
    /// among the servers bridged for the language, in that order (`"*"` by
    /// name), under the current prepare epoch. `None` without the layer,
    /// with no candidate, or when every candidate is known not to advertise
    /// the request — the pair is then not prepared at all, rather than each
    /// revision being held back to find that out again.
    pub(crate) fn prepare_target(
        &self,
        host_language: &str,
        injection_language: &str,
    ) -> Option<super::PrepareTarget> {
        let layer = self.prepare.as_ref()?;
        // Before the settings: a settings change stores them, then moves the
        // epoch, so a target never pairs older candidates with a newer epoch.
        let epoch = self.pool.prepare_epoch();
        let targets = layer.targets.load();
        // The first resolution wins for every caller: answers are keyed by
        // the candidates' launch configs by identity, which two resolutions
        // racing each other would not share.
        let hit = targets
            .resolved
            .get(host_language)
            .and_then(|pairs| pairs.get(injection_language).map(|hit| hit.clone()));
        let candidates = match hit {
            Some(hit) => hit,
            None => targets
                .resolved
                .entry(host_language.to_string())
                .or_default()
                .entry(injection_language.to_string())
                .or_insert_with(|| {
                    self.resolve_prepare_candidates(
                        layer,
                        &targets.settings,
                        host_language,
                        injection_language,
                    )
                })
                .clone(),
        }?;
        candidates
            .iter()
            .any(|candidate| self.pool.prepare_advertisement(&candidate.server_name) != Some(false))
            .then_some(super::PrepareTarget { candidates, epoch })
    }

    /// Whether some candidate of the pair is known to prepare its documents:
    /// it advertised the request in its last handshake.
    pub(crate) fn prepare_peer_known(&self, host_language: &str, injection_language: &str) -> bool {
        self.prepare_target(host_language, injection_language)
            .is_some_and(|target| {
                target.candidates.iter().any(|candidate| {
                    self.pool.prepare_advertisement(&candidate.server_name) == Some(true)
                })
            })
    }

    fn resolve_prepare_candidates(
        &self,
        layer: &PrepareLayer,
        settings: &Arc<WorkspaceSettings>,
        host_language: &str,
        injection_language: &str,
    ) -> Option<Arc<[ResolvedServerConfig]>> {
        use crate::lsp::aggregation::server::priority::{entry_names, expand_priorities};
        let configs =
            self.cached_configs_for_injection_language(settings, host_language, injection_language);
        if configs.is_empty() {
            return None;
        }
        let aggregation =
            crate::lsp::lsp_impl::bridge_context::resolve_aggregation_config_from_settings(
                settings,
                host_language,
                injection_language,
                super::protocol::PREPARE_METHOD,
            );
        if aggregation.strategy != crate::config::settings::AggregationStrategy::Preferred
            && layer
                .warned_strategy
                .insert((host_language.to_string(), injection_language.to_string()))
        {
            log::warn!(
                target: "kakehashi::bridge::prepare",
                "{} in {} hosts: {} is always aggregated as preferred; the configured strategy is ignored",
                escape_terminal_controls(injection_language),
                escape_terminal_controls(host_language),
                super::protocol::PREPARE_METHOD,
            );
        }
        let candidates: Arc<[ResolvedServerConfig]> =
            entry_names(&expand_priorities(&aggregation.priorities, &configs))
                .into_iter()
                .filter_map(|name| configs.iter().find(|config| config.server_name == name))
                .cloned()
                .collect();
        (!candidates.is_empty()).then_some(candidates)
    }
}

impl super::BridgeCoordinator {
    /// Take `settings` as what prepare targets resolve from (see
    /// [`PrepareLayer::settings_changed`]).
    pub(crate) fn prepare_settings_changed(&self, settings: &Arc<WorkspaceSettings>) {
        if let Some(layer) = &self.prepare {
            layer.settings_changed(&self.pool, settings);
        }
    }

    /// Servers were sent other runtime settings: a peer may now prepare
    /// differently, so the answers given under the ones before stand no
    /// more. Nothing to do while nothing was ever prepared.
    pub(crate) fn prepare_runtime_settings_pushed(&self) {
        if self
            .prepare
            .as_ref()
            .is_some_and(|layer| !layer.registry.unused())
        {
            self.pool.advance_prepare_epoch();
        }
    }

    /// The lifecycle pass's lookup of a virtual document's answer, without
    /// waiting: on a miss an attempt starts in the background, and the host
    /// is synced again once it lands.
    pub(crate) fn prepared_document_now(
        &self,
        target: &super::PrepareTarget,
        input: PrepareInput<'_>,
    ) -> PrepareLookup {
        match &self.prepare {
            Some(layer) => layer.registry.lookup_or_start(&self.pool, target, input),
            None => PrepareLookup::Pending,
        }
    }

    /// [`Self::prepared_document_now`] for a request that cannot wait (see
    /// `PrepareRegistry::lookup_for_request`).
    pub(crate) fn prepared_document_now_for_request(
        &self,
        target: &super::PrepareTarget,
        input: PrepareInput<'_>,
    ) -> PrepareLookup {
        match &self.prepare {
            Some(layer) => layer.registry.lookup_for_request(&self.pool, target, input),
            None => PrepareLookup::Pending,
        }
    }

    /// Note that an editor's pull for `host` was answered without documents
    /// still waiting for their prepare answers: it is owed a
    /// `workspace/diagnostic/refresh` once a resync pass sends them. When
    /// none is pending any more — the answer came while the pull ran, and
    /// its resync may already have looked for the debt — the host is synced
    /// again, so a pass that reaches every server settles it.
    pub(crate) fn owe_pull_after_prepare(&self, host: &Url) {
        let Some(layer) = &self.prepare else {
            return;
        };
        // Closed meanwhile: no editor shows it.
        if layer.registry.host_closed(host) {
            return;
        }
        layer.pull_debts.insert(host.clone());
        if !self.prepare_host_has_pending(host) {
            layer.registry.requeue_resync(
                Resync {
                    host_uri: host.clone(),
                    ready: false,
                    recollect: false,
                },
                std::time::Duration::ZERO,
            );
        }
    }

    /// Take `host`'s debt of a pull answered without documents then waiting
    /// for their prepare answers (see [`Self::owe_pull_after_prepare`]).
    pub(crate) fn take_pull_debt_after_prepare(&self, host: &Url) -> bool {
        self.prepare
            .as_ref()
            .is_some_and(|layer| layer.pull_debts.remove(host).is_some())
    }

    /// A request's wait for a virtual document's answer.
    pub(crate) async fn prepared_document(
        &self,
        target: &super::PrepareTarget,
        input: PrepareInput<'_>,
    ) -> Result<super::prepare_registry::Answer, PrepareMiss> {
        match &self.prepare {
            Some(layer) => layer.registry.prepare(&self.pool, target, input).await,
            None => Err(PrepareMiss::Unanswered),
        }
    }

    /// Settle `input`'s answer as an attempt would (see
    /// `PrepareRegistry::answer_for_test`).
    #[cfg(test)]
    pub(crate) fn answer_prepare_for_test(
        &self,
        target: &super::PrepareTarget,
        input: PrepareInput<'_>,
        answer: super::prepare_registry::Answer,
        held: bool,
    ) {
        self.prepare
            .as_ref()
            .expect("the prepare layer")
            .registry
            .answer_for_test(target, input, answer, held);
    }

    /// What the lifecycle pass knows of a region's current document (see
    /// [`RegionPrepare`]).
    pub(crate) fn prepare_region(
        &self,
        host_uri: &Url,
        injection_language: &str,
        region_id: &str,
    ) -> RegionPrepare {
        match &self.prepare {
            Some(layer) => layer.registry.region(
                host_uri,
                injection_language,
                region_id,
                self.pool.prepare_epoch(),
            ),
            None => RegionPrepare::default(),
        }
    }

    /// How coordinates a connection sent about a region — a cached push, a
    /// showDocument selection, an applyEdit's edits — place while the
    /// region's virtual text is `virtual_text`: through the text the
    /// connection held when they were read (`held`), by the one rule (see
    /// `Presentation`). Unstamped coordinates place as is — unless the
    /// region is prepared, whose servers are bridged only through a
    /// prepared text.
    pub(crate) fn place_held(
        &self,
        host_uri: &Url,
        injection_language: &str,
        region_id: &str,
        virtual_text: &str,
        held: Option<&crate::lsp::diagnostic_cache::HeldStamp>,
    ) -> super::Presentation {
        let region = self.prepare_region(host_uri, injection_language, region_id);
        // The rule's shortcut, before any URI is built: nothing involves a
        // prepared text.
        if !self.pool.held_texts_matter(region) {
            return super::Presentation::AsIs;
        }
        // Unstamped: read while the pool stamped nothing, which a pool that
        // prepares never does. Asked through the rule all the same, so a
        // region the lifecycle pass prepares is never placed as is.
        let Some(stamp) = held else {
            return self.pool.unrecorded_presentation(region);
        };
        let Ok(host) = crate::lsp::lsp_impl::url_to_uri(host_uri) else {
            return super::Presentation::Unavailable;
        };
        let virtual_uri =
            super::protocol::VirtualDocumentUri::new(&host, injection_language, region_id);
        self.pool.presentation(
            &virtual_uri.to_uri_string(),
            &stamp.connection,
            stamp.observed,
            region,
            virtual_text,
        )
    }

    /// Whether every cached push places as is (see [`Self::place_held`]),
    /// so the per-slot placement can be skipped: the rule's shortcut (see
    /// `DocumentTracker::held_texts_matter`) for every region at once — no
    /// prepared text was ever sent, and the lifecycle pass prepares no
    /// region now (answers given as is, as with no peer, involve nothing).
    pub(crate) fn places_pushes_as_is(&self) -> bool {
        !self.pool.held_texts_matter(RegionPrepare::default())
            && self.prepare.as_ref().is_none_or(|layer| {
                !layer
                    .registry
                    .any_region_prepared(self.pool.prepare_epoch())
            })
    }

    /// Whether coordinates a server computed for a region may be a prepared
    /// text's: the lifecycle pass prepares its document (or holds it back
    /// for its peer), or a server still holds a prepared text of it (right
    /// after its pair lost its peer). What resolving through the region's
    /// offset alone is refused by; edit methods close by the lifecycle pass
    /// alone (see `method_closed_to`).
    pub(crate) fn region_coordinates_may_be_prepared(
        &self,
        host_uri: &Url,
        injection_language: &str,
        region_id: &str,
    ) -> bool {
        if self.prepare.is_none() {
            return false;
        }
        if self
            .prepare_region(host_uri, injection_language, region_id)
            .prepared
        {
            return true;
        }
        // No prepared text was ever sent: none is held.
        if !self.pool.held_texts_matter(RegionPrepare::default()) {
            return false;
        }
        let Ok(host) = crate::lsp::lsp_impl::url_to_uri(host_uri) else {
            return false;
        };
        let virtual_uri =
            super::protocol::VirtualDocumentUri::new(&host, injection_language, region_id);
        let uri = virtual_uri.to_uri_string();
        self.pool
            .get_all_connections_for_virtual_uri(&virtual_uri)
            .iter()
            .any(|connection| {
                self.pool
                    .held_text(&uri, connection)
                    .is_some_and(|held| held.map.is_some())
            })
    }

    /// Whether any of `host_uri`'s documents still waits for its answer.
    pub(crate) fn prepare_host_has_pending(&self, host_uri: &Url) -> bool {
        self.prepare.as_ref().is_some_and(|layer| {
            layer
                .registry
                .host_has_pending(host_uri, self.pool.prepare_epoch())
        })
    }

    /// Drop one region's answers (see `PrepareRegistry::forget_region`).
    pub(crate) fn forget_prepared_region(
        &self,
        host_uri: &Url,
        injection_language: Option<&str>,
        region_id: &str,
    ) {
        if let Some(layer) = &self.prepare {
            layer
                .registry
                .forget_region(host_uri, injection_language, region_id);
        }
    }

    /// Drop invalidated regions' answers under every language (see
    /// `PrepareRegistry::forget_regions`).
    pub(crate) fn forget_prepared_regions(&self, host_uri: &Url, region_ids: &[String]) {
        if let Some(layer) = &self.prepare {
            layer.registry.forget_regions(host_uri, region_ids);
        }
    }

    /// Drop a replaced region's answers except under the language it
    /// resolves to now (see `PrepareRegistry::forget_region_except`).
    pub(crate) fn forget_replaced_prepared_region(
        &self,
        host_uri: &Url,
        region_id: &str,
        current_language: Option<&str>,
    ) {
        if let Some(layer) = &self.prepare {
            layer
                .registry
                .forget_region_except(host_uri, region_id, current_language);
        }
    }

    /// Remember that a save of `host_uri` at `(incarnation, content_version)`
    /// reached it while these regions were held, so it is forwarded to them
    /// once they are sent. A newer save supersedes what an older one missed.
    pub(crate) fn hold_save(
        &self,
        host_uri: &Url,
        incarnation: u64,
        content_version: u64,
        region_ids: impl IntoIterator<Item = String>,
    ) {
        let Some(layer) = &self.prepare else {
            return;
        };
        let mut held = layer.held_saves.entry(host_uri.clone()).or_default();
        if (held.incarnation, held.content_version) != (incarnation, content_version) {
            *held = HeldSave {
                incarnation,
                content_version,
                region_ids: Default::default(),
            };
        }
        held.region_ids.extend(region_ids);
    }

    /// Take the save `host_uri`'s held regions missed, if any.
    pub(crate) fn take_held_save(&self, host_uri: &Url) -> Option<HeldSave> {
        self.prepare
            .as_ref()?
            .held_saves
            .remove(host_uri)
            .map(|(_, held)| held)
    }

    /// A host opened (again): its documents are prepared as any.
    pub(crate) fn prepare_host_opened(&self, host_uri: &Url) {
        if let Some(layer) = &self.prepare {
            layer.registry.host_opened(host_uri);
        }
    }

    /// Drop a closed host's answers and the start failures recorded for it.
    pub(crate) fn forget_prepared_host(&self, host_uri: &Url) {
        if let Some(layer) = &self.prepare {
            layer.pull_debts.remove(host_uri);
            layer.held_saves.remove(host_uri);
            layer.registry.forget_host(host_uri);
            self.pool.forget_prepare_start_failures(Some(host_uri));
        }
    }

    /// Hosts to sync again (see `PrepareRegistry::take_resync_rx`), with the
    /// prepare epoch to watch: when it moves, every open host is synced
    /// again, its documents prepared (or sent as is) under the new one.
    /// Taken once; `None` without the layer.
    pub(crate) fn take_prepare_resyncs(
        &self,
    ) -> Option<(
        tokio::sync::mpsc::UnboundedReceiver<Resync>,
        tokio::sync::watch::Receiver<u64>,
    )> {
        let layer = self.prepare.as_ref()?;
        Some((
            layer.registry.take_resync_rx()?,
            self.pool.subscribe_prepare_epoch()?,
        ))
    }

    /// Whether nothing was ever looked up for a prepare peer: no answer was
    /// given, and no region is prepared.
    pub(crate) fn prepare_unused(&self) -> bool {
        self.prepare
            .as_ref()
            .is_none_or(|layer| layer.registry.unused())
    }

    /// Sync `resync`'s host again after `delay` (see
    /// `PrepareRegistry::requeue_resync`).
    pub(crate) fn requeue_prepare_resync(&self, resync: Resync, delay: std::time::Duration) {
        if let Some(layer) = &self.prepare {
            layer.registry.requeue_resync(resync, delay);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use url::Url;

    use super::super::pool::test_helpers::TEST_ULID_LUA_0;
    use super::super::pool::{ConnectionKey, LanguageServerPool, SentText};
    use super::super::protocol::{VirtualDocumentUri, VirtualLayout, apply_prepare_result};
    use super::super::{BridgeCoordinator, Observed, Presentation};
    use crate::lsp::CancelForwarder;
    use crate::lsp::diagnostic_cache::HeldStamp;

    fn dedented(virtual_text: &str) -> super::super::protocol::PreparedDocument {
        let answer = serde_json::from_value(serde_json::json!({"segments": [{"type": "content", "changes": [
            {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 2}}, "newText": ""}
        ]}]}))
        .unwrap();
        apply_prepare_result(
            virtual_text,
            &VirtualLayout::single(virtual_text),
            Some(answer),
        )
        .unwrap()
    }

    #[test]
    fn a_cached_push_places_through_the_text_its_connection_held() {
        let pool = Arc::new(LanguageServerPool::preparing());
        let coordinator = BridgeCoordinator::with_cancel_forwarder(
            Arc::clone(&pool),
            CancelForwarder::new(Arc::clone(&pool)),
        );
        let host = Url::parse("file:///test/place.md").unwrap();
        let virtual_uri = VirtualDocumentUri::new(
            &crate::lsp::lsp_impl::url_to_uri(&host).unwrap(),
            "lua",
            TEST_ULID_LUA_0,
        );
        let key = ConnectionKey::for_server("lua");
        let place = |virtual_text: &str, held: Option<&HeldStamp>| {
            coordinator.place_held(&host, "lua", TEST_ULID_LUA_0, virtual_text, held)
        };
        let stamp = |generation| HeldStamp {
            connection: key.clone(),
            observed: Observed::Read {
                generation: Some(generation),
                prepared: true,
                stale_version: false,
            },
        };

        // Not stamped, in a region the lifecycle pass does not prepare.
        assert!(matches!(place("  a", None), Presentation::AsIs));

        // Stamped with the prepared text the connection holds: through its
        // map, while the region's virtual text is the one it was built from.
        let first = dedented("  a");
        pool.record_sent_for_test(
            &virtual_uri,
            &key,
            SentText {
                text: &first.text,
                map: Some(&first.map),
            },
        );
        match place("  a", Some(&stamp(1))) {
            Presentation::Prepared(map) => assert!(Arc::ptr_eq(&map, &first.map)),
            other => panic!("expected the held map, got {other:?}"),
        }
        assert!(matches!(
            place("  b", Some(&stamp(1))),
            Presentation::Unavailable
        ));

        // A newer text sent since the push was read: it places nowhere.
        let second = dedented("  b");
        pool.record_sent_for_test(
            &virtual_uri,
            &key,
            SentText {
                text: &second.text,
                map: Some(&second.map),
            },
        );
        assert!(matches!(
            place("  b", Some(&stamp(1))),
            Presentation::Unavailable
        ));
        assert!(matches!(
            place("  b", Some(&stamp(2))),
            Presentation::Prepared(_)
        ));
    }

    #[tokio::test]
    async fn a_pull_owed_a_refresh_is_settled_once_and_forgotten_with_its_host() {
        let pool = Arc::new(LanguageServerPool::preparing());
        let coordinator = BridgeCoordinator::with_cancel_forwarder(
            Arc::clone(&pool),
            CancelForwarder::new(Arc::clone(&pool)),
        );
        let (mut resyncs, _epochs) = coordinator.take_prepare_resyncs().unwrap();
        let host = Url::parse("file:///test/owed.md").unwrap();

        // Nothing pending any more: the host is synced again, so a pass
        // reaches the debt.
        coordinator.owe_pull_after_prepare(&host);
        let resync = resyncs.recv().await.unwrap();
        assert_eq!(resync.host_uri, host);
        assert!(coordinator.take_pull_debt_after_prepare(&host));
        assert!(!coordinator.take_pull_debt_after_prepare(&host));

        // A closed host owes nothing. (One of its documents was looked up,
        // as any owing a pull.)
        coordinator.prepare_settings_changed(&Arc::new(crate::config::WorkspaceSettings {
            auto_install: false,
            language_servers: [(
                "peer".to_string(),
                super::super::pool::test_helpers::devnull_config_for_language("lua"),
            )]
            .into_iter()
            .collect(),
            ..Default::default()
        }));
        let target = coordinator.prepare_target("markdown", "lua").unwrap();
        coordinator.answer_prepare_for_test(
            &target,
            super::PrepareInput {
                host_uri: &host,
                host_language: "markdown",
                injection_language: "lua",
                region_id: TEST_ULID_LUA_0,
                virtual_text: "a",
                gaps: &[],
            },
            super::super::prepare_registry::Answer::AsIs,
            true,
        );
        coordinator.owe_pull_after_prepare(&host);
        coordinator.forget_prepared_host(&host);
        assert!(!coordinator.take_pull_debt_after_prepare(&host));
        // Nor does a pull still running past the close.
        coordinator.owe_pull_after_prepare(&host);
        assert!(!coordinator.take_pull_debt_after_prepare(&host));
    }

    /// A region reads prepared for resolve paths while any connection still
    /// holds a prepared text of it, though the lifecycle pass no longer
    /// prepares it (its pair just lost its peer): resolving through the
    /// offset alone would misplace what that server computed.
    #[tokio::test]
    async fn a_region_reads_prepared_while_a_server_holds_a_prepared_text() {
        let pool = Arc::new(LanguageServerPool::preparing());
        let coordinator = BridgeCoordinator::with_cancel_forwarder(
            Arc::clone(&pool),
            CancelForwarder::new(Arc::clone(&pool)),
        );
        let host = Url::parse("file:///test/resolve.md").unwrap();
        let virtual_uri = VirtualDocumentUri::new(
            &crate::lsp::lsp_impl::url_to_uri(&host).unwrap(),
            "lua",
            TEST_ULID_LUA_0,
        );
        let key = ConnectionKey::for_server("lua");
        pool.register_opened_document(&host, &virtual_uri, &key)
            .await;
        let prepared = |coordinator: &BridgeCoordinator| {
            coordinator.region_coordinates_may_be_prepared(&host, "lua", TEST_ULID_LUA_0)
        };

        pool.record_sent_for_test(&virtual_uri, &key, "  a".into());
        assert!(!prepared(&coordinator));
        let text = dedented("  a");
        pool.record_sent_for_test(
            &virtual_uri,
            &key,
            SentText {
                text: &text.text,
                map: Some(&text.map),
            },
        );
        assert!(prepared(&coordinator));
        pool.record_sent_for_test(&virtual_uri, &key, "  a".into());
        assert!(!prepared(&coordinator));

        // Without the layer nothing is prepared.
        let off = Arc::new(LanguageServerPool::new());
        let off = BridgeCoordinator::with_cancel_forwarder(
            Arc::clone(&off),
            CancelForwarder::new(Arc::clone(&off)),
        );
        assert!(!prepared(&off));
    }

    /// A pair's candidates resolve once per settings: every caller gets the
    /// same launch configs (answers are keyed by their identity), and only
    /// other settings resolve them again.
    #[test]
    fn a_pairs_candidates_resolve_once_per_settings() {
        let pool = Arc::new(LanguageServerPool::preparing());
        let coordinator = BridgeCoordinator::with_cancel_forwarder(
            Arc::clone(&pool),
            CancelForwarder::new(Arc::clone(&pool)),
        );
        let settings_with = |servers: &[&str]| {
            Arc::new(crate::config::WorkspaceSettings {
                auto_install: false,
                language_servers: servers
                    .iter()
                    .map(|name| {
                        (
                            name.to_string(),
                            super::super::pool::test_helpers::devnull_config_for_language("lua"),
                        )
                    })
                    .collect(),
                ..Default::default()
            })
        };
        coordinator.prepare_settings_changed(&settings_with(&["peer"]));
        let first = coordinator.prepare_target("markdown", "lua").unwrap();
        let again = coordinator.prepare_target("markdown", "lua").unwrap();
        assert!(Arc::ptr_eq(&first.candidates, &again.candidates));
        assert!(Arc::ptr_eq(
            &first.candidates[0].config,
            &again.candidates[0].config
        ));

        // Equal settings: nothing resolves again, the epoch stays.
        coordinator.prepare_settings_changed(&settings_with(&["peer"]));
        let same = coordinator.prepare_target("markdown", "lua").unwrap();
        assert!(Arc::ptr_eq(&first.candidates, &same.candidates));
        assert_eq!(same.epoch, first.epoch);

        // Other settings: resolved again, under a new epoch.
        coordinator.prepare_settings_changed(&settings_with(&["peer", "other"]));
        let other = coordinator.prepare_target("markdown", "lua").unwrap();
        assert!(!Arc::ptr_eq(&first.candidates, &other.candidates));
        assert!(other.epoch > first.epoch);
    }

    /// A save held regions missed accumulates their ids per saved version;
    /// a newer save supersedes it, taking it is one-shot, and a closed host
    /// owes none. Without the layer nothing is held.
    #[test]
    fn a_save_held_regions_missed_is_kept_for_the_saved_version() {
        let pool = Arc::new(LanguageServerPool::preparing());
        let coordinator = BridgeCoordinator::with_cancel_forwarder(
            Arc::clone(&pool),
            CancelForwarder::new(Arc::clone(&pool)),
        );
        let host = Url::parse("file:///test/save.md").unwrap();
        let ids = |ids: &[&str]| ids.iter().map(|id| id.to_string()).collect::<Vec<_>>();
        let held = |incarnation, content_version, region_ids: &[&str]| super::HeldSave {
            incarnation,
            content_version,
            region_ids: region_ids.iter().map(|id| id.to_string()).collect(),
        };

        coordinator.hold_save(&host, 1, 2, ids(&["a"]));
        coordinator.hold_save(&host, 1, 2, ids(&["b"]));
        assert_eq!(
            coordinator.take_held_save(&host),
            Some(held(1, 2, &["a", "b"]))
        );
        assert_eq!(coordinator.take_held_save(&host), None);

        coordinator.hold_save(&host, 1, 2, ids(&["a"]));
        coordinator.hold_save(&host, 1, 3, ids(&["c"]));
        assert_eq!(coordinator.take_held_save(&host), Some(held(1, 3, &["c"])));

        coordinator.hold_save(&host, 1, 3, ids(&["c"]));
        coordinator.forget_prepared_host(&host);
        assert_eq!(coordinator.take_held_save(&host), None);

        let off = Arc::new(LanguageServerPool::new());
        let off = BridgeCoordinator::with_cancel_forwarder(
            Arc::clone(&off),
            CancelForwarder::new(Arc::clone(&off)),
        );
        off.hold_save(&host, 1, 2, ids(&["a"]));
        assert_eq!(off.take_held_save(&host), None);
    }

    /// With a peer configured but nothing prepared — every answer given as
    /// is, as before a handshake or with no peer — pushes place as is
    /// without per-slot work, however many lookups were made.
    #[tokio::test]
    async fn pushes_place_as_is_until_a_region_is_prepared() {
        let pool = Arc::new(LanguageServerPool::preparing());
        let coordinator = BridgeCoordinator::with_cancel_forwarder(
            Arc::clone(&pool),
            CancelForwarder::new(Arc::clone(&pool)),
        );
        coordinator.prepare_settings_changed(&Arc::new(crate::config::WorkspaceSettings {
            auto_install: false,
            language_servers: [(
                "peer".to_string(),
                super::super::pool::test_helpers::devnull_config_for_language("lua"),
            )]
            .into_iter()
            .collect(),
            ..Default::default()
        }));
        let host = Url::parse("file:///test/as-is.md").unwrap();
        let target = coordinator.prepare_target("markdown", "lua").unwrap();
        let input = |region_id, virtual_text| super::PrepareInput {
            host_uri: &host,
            host_language: "markdown",
            injection_language: "lua",
            region_id,
            virtual_text,
            gaps: &[],
        };
        assert!(coordinator.places_pushes_as_is());

        // Looked up, answered as is.
        coordinator.answer_prepare_for_test(
            &target,
            input(TEST_ULID_LUA_0, "a"),
            super::super::prepare_registry::Answer::AsIs,
            false,
        );
        assert!(coordinator.places_pushes_as_is());
        assert!(!coordinator.region_coordinates_may_be_prepared(&host, "lua", TEST_ULID_LUA_0));
        assert!(matches!(
            coordinator.place_held(&host, "lua", TEST_ULID_LUA_0, "a", None),
            Presentation::AsIs
        ));
        // The lifecycle pass holds the region on that answer, as it holds
        // every region with no peer.
        assert!(matches!(
            coordinator.prepared_document_now(&target, input(TEST_ULID_LUA_0, "a")),
            super::PrepareLookup::Answered(super::super::prepare_registry::Answer::AsIs)
        ));
        assert!(coordinator.places_pushes_as_is());

        // The lifecycle pass holds a region on an answer it waits for.
        let _ = coordinator.prepared_document_now(&target, input(TEST_ULID_LUA_0, "b"));
        assert!(!coordinator.places_pushes_as_is());
    }

    #[test]
    fn an_unrecorded_text_places_as_is_only_outside_prepared_regions() {
        let pool = LanguageServerPool::preparing();
        let prepared = super::RegionPrepare {
            prepared: true,
            failed: false,
        };
        assert!(matches!(
            pool.unrecorded_presentation(super::RegionPrepare::default()),
            Presentation::AsIs
        ));
        assert!(matches!(
            pool.unrecorded_presentation(prepared),
            Presentation::Unavailable
        ));
    }
}
