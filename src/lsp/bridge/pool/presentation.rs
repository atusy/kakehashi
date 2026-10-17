//! How coordinates a downstream server computed for a virtual document
//! translate to the host: through the region's offset alone, through the
//! map of the prepared text the server holds first, or not at all.
//!
//! Every path that reads coordinates a connection sent unprompted — a
//! pushed diagnostic, an inbound edit, a showDocument selection — and the
//! check a request makes before it goes out decide it here, from what the
//! connection holds now (see [`HeldText`]), what it held when its
//! coordinates were read ([`Observed`]), and what the lifecycle pass knows of
//! the region ([`RegionPrepare`]).

use std::sync::Arc;

use super::document_tracker::HeldText;
use crate::lsp::bridge::protocol::{PreparedMap, RegionOffset};

/// How coordinates a connection computed for a region translate to the host.
#[derive(Debug, Clone)]
pub(crate) enum Presentation {
    /// Through the region's offset alone: the connection holds the virtual
    /// text as is.
    AsIs,
    /// Through this map first: the connection holds the text a peer
    /// prepared from the region's current virtual text.
    Prepared(Arc<PreparedMap>),
    /// Not at all: the coordinates describe no current host position.
    Unavailable,
}

impl Presentation {
    /// `offset` as coordinates under this presentation follow it, `None`
    /// when unavailable.
    pub(crate) fn offset(self, offset: RegionOffset) -> Option<RegionOffset> {
        match self {
            Self::AsIs => Some(offset),
            Self::Prepared(map) => Some(offset.with_prepared(map)),
            Self::Unavailable => None,
        }
    }
}

/// What a connection held when kakehashi read coordinates it sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Observed {
    /// Read while no connection had ever been sent a prepared text: the
    /// connection held its text as is.
    BeforeAnyPrepared,
    /// Read when the connection held no text newer than this generation
    /// (see [`HeldText::generation`]): a text it holds with a newer one was
    /// sent after the coordinates were computed.
    Through(u64),
    /// Computed for another text than the connection was last sent (a push
    /// naming an older document version).
    Stale,
}

/// What the lifecycle pass knows of a region's current virtual document.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct RegionPrepare {
    /// The lifecycle pass holds it on a prepare answer (or one it still
    /// waits for, or one that failed): it is bridged only through a prepared
    /// text, never one held as is.
    pub(crate) prepared: bool,
    /// The peer answered its current input unusably: it is not bridged.
    pub(crate) failed: bool,
}

/// The one rule deciding how a connection's coordinates translate.
///
/// - A region whose current input the peer answered unusably is not bridged.
/// - A text the connection was sent after the coordinates were read is not
///   the one they were computed for.
/// - A text held as is (or nothing on record) translates through the offset
///   alone — unless the lifecycle pass prepares the region, whose servers
///   are bridged only through a prepared text.
/// - A prepared text translates through its map while that map was built
///   from the region's current virtual text; one built from another virtual
///   text describes no current host position.
pub(super) fn presentation(
    held: Option<&HeldText>,
    observed: Observed,
    region: RegionPrepare,
    virtual_text: &str,
) -> Presentation {
    if region.failed {
        return Presentation::Unavailable;
    }
    let newer = match observed {
        Observed::Stale => true,
        Observed::BeforeAnyPrepared => held.is_some_and(|held| held.map.is_some()),
        Observed::Through(generation) => held.is_some_and(|held| held.generation > generation),
    };
    if newer {
        return Presentation::Unavailable;
    }
    match held.and_then(|held| held.map.as_ref()) {
        None if region.prepared => Presentation::Unavailable,
        None => Presentation::AsIs,
        Some(map) if map.virtual_text() == virtual_text => Presentation::Prepared(Arc::clone(map)),
        Some(_) => Presentation::Unavailable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lsp::bridge::protocol::{VirtualLayout, apply_prepare_result};

    fn map(virtual_text: &str) -> Arc<PreparedMap> {
        apply_prepare_result(virtual_text, &VirtualLayout::single(virtual_text), None)
            .unwrap()
            .map
    }

    fn held(generation: u64, map: Option<Arc<PreparedMap>>) -> HeldText {
        HeldText {
            fingerprint: 0,
            map,
            generation,
            version: 1,
        }
    }

    const PREPARED: RegionPrepare = RegionPrepare {
        prepared: true,
        failed: false,
    };

    #[test]
    fn a_text_held_as_is_translates_through_the_offset_unless_the_region_is_prepared() {
        let as_is = held(3, None);
        assert!(matches!(
            presentation(
                Some(&as_is),
                Observed::Through(3),
                RegionPrepare::default(),
                "v"
            ),
            Presentation::AsIs
        ));
        assert!(matches!(
            presentation(None, Observed::Through(3), RegionPrepare::default(), "v"),
            Presentation::AsIs
        ));
        // A region the lifecycle pass prepares is bridged only through a
        // prepared text: one held as is was sent before its pair gained its
        // peer, and nothing on record says no map.
        assert!(matches!(
            presentation(Some(&as_is), Observed::Through(3), PREPARED, "v"),
            Presentation::Unavailable
        ));
        assert!(matches!(
            presentation(None, Observed::Through(3), PREPARED, "v"),
            Presentation::Unavailable
        ));
    }

    #[test]
    fn a_prepared_text_translates_through_its_map_only_for_the_current_virtual_text() {
        let prepared = held(3, Some(map("v")));
        assert!(matches!(
            presentation(Some(&prepared), Observed::Through(3), PREPARED, "v"),
            Presentation::Prepared(_)
        ));
        // Prepared from another virtual text: no current host position.
        assert!(matches!(
            presentation(Some(&prepared), Observed::Through(3), PREPARED, "w"),
            Presentation::Unavailable
        ));
        // Right after its pair lost its peer, the server still holds it.
        assert!(matches!(
            presentation(
                Some(&prepared),
                Observed::Through(3),
                RegionPrepare::default(),
                "v"
            ),
            Presentation::Prepared(_)
        ));
    }

    #[test]
    fn a_text_sent_after_the_coordinates_were_read_is_not_theirs() {
        let prepared = held(4, Some(map("v")));
        assert!(matches!(
            presentation(Some(&prepared), Observed::Through(3), PREPARED, "v"),
            Presentation::Unavailable
        ));
        let as_is = held(4, None);
        assert!(matches!(
            presentation(
                Some(&as_is),
                Observed::Through(3),
                RegionPrepare::default(),
                "v"
            ),
            Presentation::Unavailable
        ));
        // Read before anything was prepared: only a prepared text is newer
        // than what the connection held then.
        assert!(matches!(
            presentation(
                Some(&as_is),
                Observed::BeforeAnyPrepared,
                RegionPrepare::default(),
                "v"
            ),
            Presentation::AsIs
        ));
        let prepared = held(1, Some(map("v")));
        assert!(matches!(
            presentation(Some(&prepared), Observed::BeforeAnyPrepared, PREPARED, "v"),
            Presentation::Unavailable
        ));
        // Computed for an earlier version of the text.
        assert!(matches!(
            presentation(Some(&prepared), Observed::Stale, PREPARED, "v"),
            Presentation::Unavailable
        ));
    }

    #[test]
    fn a_region_answered_unusably_is_not_bridged() {
        let prepared = held(3, Some(map("v")));
        let failed = RegionPrepare {
            prepared: true,
            failed: true,
        };
        assert!(matches!(
            presentation(Some(&prepared), Observed::Through(3), failed, "v"),
            Presentation::Unavailable
        ));
    }
}
