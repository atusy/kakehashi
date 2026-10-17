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

/// The settings prepare targets are resolved from, with the targets resolved
/// so far (this runs per bridged region in every lifecycle pass and request).
struct Targets {
    settings: Arc<WorkspaceSettings>,
    /// `(host language, injection language)` → the pair's candidates, `None`
    /// for a pair with none.
    resolved: DashMap<(String, String), Option<Arc<[ResolvedServerConfig]>>>,
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
        let key = (host_language.to_string(), injection_language.to_string());
        // The first resolution wins for every caller: answers are keyed by
        // the candidates' launch configs by identity, which two resolutions
        // racing each other would not share.
        let candidates = match targets.resolved.get(&key) {
            Some(hit) => hit.clone(),
            None => targets
                .resolved
                .entry(key)
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

    /// How a region's cached push places while the region's virtual text is
    /// `virtual_text`: through the text its connection held when it pushed
    /// (`held`), by the one rule (see `Presentation`). A slot not stamped
    /// with a connection places as is — unless the region is prepared, whose
    /// servers are bridged only through a prepared text.
    pub(crate) fn place_push(
        &self,
        host_uri: &Url,
        injection_language: &str,
        region_id: &str,
        virtual_text: &str,
        held: Option<&crate::lsp::diagnostic_cache::HeldStamp>,
    ) -> super::Presentation {
        let region = self.prepare_region(host_uri, injection_language, region_id);
        let Some(stamp) = held else {
            return if region == RegionPrepare::default() {
                super::Presentation::AsIs
            } else {
                super::Presentation::Unavailable
            };
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

    /// Whether every cached push places as is (see [`Self::place_push`]):
    /// nothing was ever prepared, so the per-slot placement can be skipped.
    pub(crate) fn places_pushes_as_is(&self) -> bool {
        self.prepare_unused() && !self.pool.prepares_anything()
    }

    /// Whether a region is prepared: the lifecycle pass prepares its document
    /// (or holds it back for its peer), or a server still holds a prepared
    /// text of it (right after its pair lost its peer).
    pub(crate) fn region_is_prepared(
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

    /// Drop a closed host's answers and the start failures recorded for it.
    pub(crate) fn forget_prepared_host(&self, host_uri: &Url) {
        if let Some(layer) = &self.prepare {
            layer.pull_debts.remove(host_uri);
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

    /// Whether nothing was ever looked up for a prepare peer: an epoch move
    /// then changes nothing any document holds.
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
