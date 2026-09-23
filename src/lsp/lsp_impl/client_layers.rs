//! The client-supplied configuration layers retained for replay.
//!
//! The effective settings are a left fold of every layer, and the fold is not
//! associative once an empty container clears the layer below (see
//! `fold_layers` in `src/lsp/settings.rs`). Rebuilding them — after a
//! workspace-root change, say — therefore needs the client's layers as they
//! arrived, in order, rather than only the merged result.

use crate::config::RawWorkspaceSettings;

/// The client's layers in arrival order, stored unanchored so a replay can
/// anchor them to whichever workspace root is then in effect.
#[derive(Debug, Default)]
pub(super) struct ClientLayers {
    layers: Vec<RawWorkspaceSettings>,
}

impl ClientLayers {
    /// The layers a session starts with: its `initializationOptions`, when it
    /// sent any that were accepted.
    pub(super) fn from_initialization_options(options: Option<RawWorkspaceSettings>) -> Self {
        Self {
            layers: options.into_iter().collect(),
        }
    }

    /// Record a pushed `workspace/didChangeConfiguration` layer. Pushes
    /// accumulate: each one is a layer above the ones before (#734).
    pub(super) fn append_pushed(&mut self, layer: RawWorkspaceSettings) {
        self.layers.push(layer);
    }

    /// The layers in fold order, for a replay.
    pub(super) fn to_fold_order(&self) -> Vec<RawWorkspaceSettings> {
        self.layers.clone()
    }
}
