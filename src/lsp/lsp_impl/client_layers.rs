//! The client-supplied configuration layers retained for replay.
//!
//! The effective settings are a left fold of every layer, and the fold is not
//! associative once an empty container clears the layer below (see
//! `fold_layers` in `src/lsp/settings.rs`). Rebuilding them — after a
//! workspace-root change, say — therefore needs the client's layers as they
//! arrived, in order, rather than only the merged result.

use crate::config::RawWorkspaceSettings;

/// How a retained layer reached kakehashi, which decides what a later layer
/// does to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Provenance {
    /// `initializationOptions`, or a pushed `didChangeConfiguration`: layers
    /// that accumulate, each one above the ones before (#734).
    Accumulated,
    /// An answer to kakehashi's own `workspace/configuration` pull: the
    /// client's whole configuration for the scope asked, so a newer answer
    /// supersedes it.
    Pulled,
}

#[derive(Debug, Clone)]
struct ClientLayer {
    provenance: Provenance,
    raw: RawWorkspaceSettings,
}

/// The client's layers in arrival order, stored unanchored so a replay can
/// anchor them to whichever workspace root is then in effect.
#[derive(Debug, Default, Clone)]
pub(super) struct ClientLayers {
    layers: Vec<ClientLayer>,
}

impl ClientLayers {
    /// The layers a session starts with: its `initializationOptions`, when it
    /// sent any that were accepted.
    pub(super) fn from_initialization_options(options: Option<RawWorkspaceSettings>) -> Self {
        Self {
            layers: options.into_iter().map(ClientLayer::accumulated).collect(),
        }
    }

    /// Record a pushed `workspace/didChangeConfiguration` layer. Pushes
    /// accumulate: each one is a layer above the ones before (#734).
    pub(super) fn append_pushed(&mut self, layer: RawWorkspaceSettings) {
        self.layers.push(ClientLayer::accumulated(layer));
    }

    /// Record a pull answer in place of the previous one: `Some` is the
    /// client's configuration for the scope asked, `None` an answer that holds
    /// nothing for kakehashi. Returns whether the layers changed.
    ///
    /// The answer lands at its own arrival, above every push before it, not
    /// where the previous answer sat: it is the newest statement of the
    /// client's configuration, and a push it would sit beneath is older.
    pub(super) fn replace_pulled(&mut self, answer: Option<RawWorkspaceSettings>) -> bool {
        let before = self.layers.len();
        self.layers
            .retain(|layer| layer.provenance != Provenance::Pulled);
        let withdrew = self.layers.len() != before;
        match answer {
            Some(raw) => {
                self.layers.push(ClientLayer {
                    provenance: Provenance::Pulled,
                    raw,
                });
                true
            }
            None => withdrew,
        }
    }

    /// The layers in fold order, for a replay.
    pub(super) fn to_fold_order(&self) -> Vec<RawWorkspaceSettings> {
        self.layers.iter().map(|layer| layer.raw.clone()).collect()
    }
}

impl ClientLayer {
    fn accumulated(raw: RawWorkspaceSettings) -> Self {
        Self {
            provenance: Provenance::Accumulated,
            raw,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn search_paths(paths: &[&str]) -> RawWorkspaceSettings {
        RawWorkspaceSettings {
            search_paths: Some(paths.iter().map(|path| path.to_string()).collect()),
            ..Default::default()
        }
    }

    fn fold_order(layers: &ClientLayers) -> Vec<Option<Vec<String>>> {
        layers
            .to_fold_order()
            .into_iter()
            .map(|layer| layer.search_paths)
            .collect()
    }

    #[test]
    fn a_pull_answer_supersedes_only_the_previous_answer() {
        let mut layers = ClientLayers::from_initialization_options(Some(search_paths(&["/init"])));
        assert!(layers.replace_pulled(Some(search_paths(&["/pulled-first"]))));
        layers.append_pushed(search_paths(&["/pushed"]));
        assert!(layers.replace_pulled(Some(search_paths(&["/pulled-second"]))));

        assert_eq!(
            fold_order(&layers),
            vec![
                Some(vec!["/init".to_string()]),
                Some(vec!["/pushed".to_string()]),
                Some(vec!["/pulled-second".to_string()]),
            ],
            "initializationOptions and pushes stay, in order; only the older \
             answer goes, and the newer one lands last"
        );
    }

    #[test]
    fn an_empty_answer_changes_the_layers_only_when_it_withdraws_one() {
        let mut layers = ClientLayers::default();
        assert!(
            !layers.replace_pulled(None),
            "no answer to withdraw: nothing changed"
        );

        assert!(layers.replace_pulled(Some(search_paths(&["/pulled"]))));
        assert!(
            layers.replace_pulled(None),
            "withdrawing an answer is a change"
        );
        assert!(fold_order(&layers).is_empty());
    }
}
