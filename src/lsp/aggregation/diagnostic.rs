use std::borrow::Cow;
use std::sync::Arc;

use tower_lsp_server::ls_types::Diagnostic;

use crate::config::settings::{AggregationStrategy, LayerSource, ResolvedLayerConfig};

/// An uncollected layer differs from a collected empty result.
#[derive(Debug, Clone)]
pub(crate) enum PullContribution {
    Pending,
    NotPulled,
    /// Result of eligible collection, including an empty result. Host events
    /// retain their best-effort policy: request errors do not switch a
    /// pull-driven server to its push cache. Refresh prefetch separately
    /// counts errors and vetoes the cache commit on failure.
    Pulled(Arc<[Diagnostic]>),
}

impl PullContribution {
    fn retain_pending(self, previous: Option<&Self>) -> Self {
        match self {
            // Retain source ownership along with the cached items. Coverage
            // describes the retained pull source, not current-version freshness;
            // exposing push alongside these items could duplicate or contradict
            // them before the next virtual pull replaces the retained result.
            Self::Pending => previous.cloned().unwrap_or(Self::NotPulled),
            collected => collected,
        }
    }

    fn items(&self) -> &[Diagnostic] {
        match self {
            Self::Pulled(items) => items,
            Self::Pending | Self::NotPulled => &[],
        }
    }

    fn was_pulled(&self) -> bool {
        matches!(self, Self::Pulled(_))
    }
}

/// Uncombined results are retained even when a preferred layer hides them.
#[derive(Debug, Clone)]
pub(crate) struct PullLayerComponents {
    pub(crate) virt: PullContribution,
    pub(crate) host: PullContribution,
    pub(crate) layer_cfg: ResolvedLayerConfig,
}

impl PullLayerComponents {
    pub(crate) fn retain_pending(self, previous: Option<&Self>) -> Self {
        Self {
            virt: self.virt.retain_pending(previous.map(|old| &old.virt)),
            host: self.host.retain_pending(previous.map(|old| &old.host)),
            layer_cfg: self.layer_cfg,
        }
    }

    pub(crate) fn coverage(&self) -> (bool, bool) {
        (self.virt.was_pulled(), self.host.was_pulled())
    }

    pub(crate) fn combine(&self) -> Vec<Diagnostic> {
        combine_layer_diagnostics(&self.layer_cfg, self.virt.items(), self.host.items())
    }

    pub(crate) fn len(&self) -> usize {
        [&self.virt, &self.host]
            .into_iter()
            .map(|layer| match layer {
                PullContribution::Pulled(items) => items.len(),
                _ => 0,
            })
            .sum()
    }
}

/// Combine per-layer diagnostic results by the cross-layer strategy
/// (cross-layer-aggregation): `concatenated` merges every participating
/// layer's items in `priorities` order; `preferred` returns the first
/// layer with a non-empty result. Native has no diagnostics contributor.
pub(crate) fn combine_layer_diagnostics<'a>(
    layer_cfg: &ResolvedLayerConfig,
    virt: impl Into<Cow<'a, [Diagnostic]>>,
    host: impl Into<Cow<'a, [Diagnostic]>>,
) -> Vec<Diagnostic> {
    let mut virt = Some(virt.into());
    let mut host = Some(host.into());
    let mut merged = Vec::new();
    for layer in &layer_cfg.priorities {
        let items = match layer {
            LayerSource::Virt => virt.take(),
            LayerSource::Host => host.take(),
            LayerSource::Native => None,
        };
        let Some(items) = items else { continue };
        match layer_cfg.strategy {
            AggregationStrategy::Concatenated => merged.extend(items.into_owned()),
            AggregationStrategy::Preferred => {
                if !items.is_empty() {
                    return items.into_owned();
                }
            }
        }
    }
    merged
}
