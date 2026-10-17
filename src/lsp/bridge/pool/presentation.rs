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
    #[allow(
        dead_code,
        reason = "pushes and inbound requests read coordinates through it, next"
    )]
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

/// What a connection held when kakehashi read coordinates it sent (or is
/// about to send it a request on).
#[allow(
    dead_code,
    reason = "pushes and inbound requests read coordinates through it, next"
)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Observed {
    /// Read while no connection had ever been sent a prepared text: the
    /// connection held a text as is (or nothing).
    AsIsThen,
    /// Read when the connection held the text of this generation (`None`:
    /// nothing on record), prepared or not — or, had it ever, a prepared
    /// text of the document before (`prepared`). `stale_version`: the
    /// coordinates name another document version than that text's (a push
    /// computed for an earlier text).
    Read {
        generation: Option<u64>,
        prepared: bool,
        stale_version: bool,
    },
    /// Read when no text newer than this generation was held by any
    /// connection (see `DocumentTracker::held_through`): for coordinates
    /// about several documents, whose records are read later.
    Through(u64),
    /// A request about to be sent: the coordinates it will get are computed
    /// for whatever the connection holds now.
    Now,
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
/// `held` is what the connection holds for the document now, `history`
/// whether it was ever sent a prepared text of it (kept past the record
/// itself).
///
/// - A region whose current input the peer answered unusably is not bridged.
/// - Where preparation is involved — the region is prepared, or the
///   connection holds (or ever held, or held when the coordinates were read)
///   a prepared text of the document — coordinates are those of the text the
///   connection held when they were read, and that text only: a text sent
///   since, a document closed since, or an earlier version named, places
///   nowhere. Elsewhere coordinates translate as on any unprepared path,
///   whatever was sent meanwhile.
/// - A text held as is (or nothing on record) translates through the offset
///   alone — unless the lifecycle pass prepares the region, whose servers
///   are bridged only through a prepared text.
/// - A prepared text translates through its map while that map was built
///   from the region's current virtual text; one built from another virtual
///   text describes no current host position.
pub(super) fn presentation(
    held: Option<&HeldText>,
    history: bool,
    observed: Observed,
    region: RegionPrepare,
    virtual_text: &str,
) -> Presentation {
    if region.failed {
        return Presentation::Unavailable;
    }
    let involved = region.prepared
        || history
        || held.is_some_and(|held| held.map.is_some())
        || matches!(observed, Observed::Read { prepared: true, .. });
    if involved {
        // Nothing on record: the document was closed since (every send is
        // recorded before it is queued).
        let newer = held.is_none() && observed != Observed::Now
            || match observed {
                Observed::AsIsThen => held.is_none_or(|held| held.map.is_some()),
                Observed::Read {
                    generation,
                    stale_version,
                    ..
                } => stale_version || generation != held.map(|held| held.generation),
                Observed::Through(generation) => {
                    held.is_none_or(|held| held.generation > generation)
                }
                Observed::Now => false,
            };
        if newer {
            return Presentation::Unavailable;
        }
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

    fn read(generation: u64, prepared: bool) -> Observed {
        Observed::Read {
            generation: Some(generation),
            prepared,
            stale_version: false,
        }
    }

    const AS_IS: RegionPrepare = RegionPrepare {
        prepared: false,
        failed: false,
    };

    #[test]
    fn a_text_held_as_is_translates_through_the_offset_unless_the_region_is_prepared() {
        let as_is = held(3, None);
        for held in [Some(&as_is), None] {
            assert!(matches!(
                presentation(held, false, read(3, false), AS_IS, "v"),
                Presentation::AsIs
            ));
            // A region the lifecycle pass prepares is bridged only through a
            // prepared text: one held as is was sent before its pair gained
            // its peer, and nothing on record says no map.
            assert!(matches!(
                presentation(held, false, Observed::Now, PREPARED, "v"),
                Presentation::Unavailable
            ));
        }
    }

    #[test]
    fn a_prepared_text_translates_through_its_map_only_for_the_current_virtual_text() {
        let prepared = held(3, Some(map("v")));
        assert!(matches!(
            presentation(Some(&prepared), true, read(3, true), PREPARED, "v"),
            Presentation::Prepared(_)
        ));
        // Prepared from another virtual text: no current host position.
        assert!(matches!(
            presentation(Some(&prepared), true, read(3, true), PREPARED, "w"),
            Presentation::Unavailable
        ));
        // Right after its pair lost its peer, the server still holds it.
        assert!(matches!(
            presentation(Some(&prepared), true, Observed::Now, AS_IS, "v"),
            Presentation::Prepared(_)
        ));
    }

    #[test]
    fn where_nothing_is_prepared_coordinates_translate_whatever_was_sent_since() {
        // As on any unprepared path: a push racing an edit, or naming an
        // earlier version, is placed through the current offset.
        let as_is = held(4, None);
        for observed in [
            read(3, false),
            Observed::Read {
                generation: Some(4),
                prepared: false,
                stale_version: true,
            },
            Observed::Through(3),
            Observed::AsIsThen,
        ] {
            assert!(
                matches!(
                    presentation(Some(&as_is), false, observed, AS_IS, "v"),
                    Presentation::AsIs
                ),
                "{observed:?}"
            );
        }
    }

    #[test]
    fn where_preparation_is_involved_coordinates_are_the_held_texts_only() {
        let prepared = held(4, Some(map("v")));
        // A text sent after the coordinates were read.
        assert!(matches!(
            presentation(Some(&prepared), true, read(3, true), PREPARED, "v"),
            Presentation::Unavailable
        ));
        assert!(matches!(
            presentation(Some(&prepared), true, Observed::Through(3), PREPARED, "v"),
            Presentation::Unavailable
        ));
        assert!(matches!(
            presentation(Some(&prepared), true, Observed::AsIsThen, PREPARED, "v"),
            Presentation::Unavailable
        ));
        // An earlier version named.
        let stale = Observed::Read {
            generation: Some(4),
            prepared: true,
            stale_version: true,
        };
        assert!(matches!(
            presentation(Some(&prepared), true, stale, PREPARED, "v"),
            Presentation::Unavailable
        ));
        // The pair lost its peer: the text held as is now was sent after the
        // prepared one the coordinates were computed for.
        let as_is = held(5, None);
        assert!(matches!(
            presentation(Some(&as_is), true, read(4, true), AS_IS, "v"),
            Presentation::Unavailable
        ));
        // Read about several documents, while the connection holds as is a
        // document it was once sent a prepared text of: that history alone
        // involves preparation.
        assert!(matches!(
            presentation(Some(&as_is), true, Observed::Through(4), AS_IS, "v"),
            Presentation::Unavailable
        ));
        assert!(matches!(
            presentation(Some(&as_is), false, Observed::Through(4), AS_IS, "v"),
            Presentation::AsIs
        ));
        // Closed since: nothing on record now.
        assert!(matches!(
            presentation(None, true, read(4, true), AS_IS, "v"),
            Presentation::Unavailable
        ));
        // Read while it was held, and still held: placed through its map.
        assert!(matches!(
            presentation(Some(&prepared), true, read(4, true), PREPARED, "v"),
            Presentation::Prepared(_)
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
            presentation(Some(&prepared), true, read(3, true), failed, "v"),
            Presentation::Unavailable
        ));
    }
}
