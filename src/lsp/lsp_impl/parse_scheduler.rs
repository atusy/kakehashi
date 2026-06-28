//! Per-document coalescing scheduler for the **off-ingress parse**
//! (per-document-parse-actor ADR).
//!
//! `did_change` applies the edit to the store and clears the tree synchronously
//! (under the edit lock), then asks this scheduler to (re)parse off the ingress
//! ticket. The scheduler guarantees **one** in-flight parse loop per document:
//! edits arriving while a parse runs only set a `dirty` bit, so a burst collapses
//! to a single follow-up reparse over the latest text rather than one parse per
//! edit — the coalescing the cost table makes worthwhile. The parse loop itself
//! lives in `Kakehashi::schedule_reparse`; this type owns only the
//! spawn-or-mark-dirty decision and is the one piece that must be race-tight.

use dashmap::DashMap;
use dashmap::mapref::entry::Entry;
use url::Url;

/// Per-document scheduling state.
#[derive(Default)]
struct SchedState {
    /// A parse loop is currently running for this document.
    parsing: bool,
    /// An edit landed while the loop was parsing; it must reparse again.
    dirty: bool,
    /// The highest ingress writer ticket the next parse must cover (for the
    /// watermark). The latest scheduled edit wins; a single coalesced parse
    /// covers the whole contiguous run up to here.
    ticket: Option<u64>,
}

/// Coalescing scheduler: at most one parse loop runs per document at a time.
#[derive(Default)]
pub(crate) struct ParseScheduler {
    states: DashMap<Url, SchedState>,
}

impl ParseScheduler {
    /// Record a pending reparse for `uri` at `ticket`. Returns `true` when the
    /// caller should **spawn** the parse loop (none was running); `false` when a
    /// loop is already running — it has now been marked `dirty` so it reparses the
    /// latest text once its current parse finishes.
    ///
    /// `entry` holds the shard write lock across the whole decision, so two racing
    /// `did_change`s can never both spawn nor drop the `dirty` bit.
    pub(crate) fn schedule(&self, uri: &Url, ticket: Option<u64>) -> bool {
        match self.states.entry(uri.clone()) {
            Entry::Occupied(mut entry) => {
                let state = entry.get_mut();
                state.ticket = ticket;
                if state.parsing {
                    state.dirty = true;
                    false
                } else {
                    state.parsing = true;
                    state.dirty = false;
                    true
                }
            }
            Entry::Vacant(entry) => {
                entry.insert(SchedState {
                    parsing: true,
                    dirty: false,
                    ticket,
                });
                true
            }
        }
    }

    /// Peek the highest ticket scheduled for `uri` so the loop's *first* reparse
    /// covers the latest edit (an edit can land between the spawn and the first
    /// parse, raising the ticket past the one the spawn was created with). `None`
    /// if there is no pending entry.
    pub(crate) fn latest_ticket(&self, uri: &Url) -> Option<u64> {
        self.states.get(uri).and_then(|state| state.ticket)
    }

    /// Called by the parse loop after one parse completes, to decide whether to
    /// loop again. Returns `Some(ticket)` when an edit landed during the parse
    /// (`dirty`) — the loop keeps running and reparses the latest text at `ticket`
    /// (itself `Option<u64>`, since a reparse may carry no ingress ticket); `None`
    /// when nothing is pending — the entry is removed and the loop exits. The outer
    /// `Option` is the continue/stop signal, distinct from the inner ticket, so a
    /// pending reparse whose ticket is `None` still continues the loop. Atomic
    /// under the shard write lock so it cannot race a concurrent `schedule`: a
    /// `schedule` arriving just before this either set `dirty` (we continue) or
    /// finds no entry afterward and spawns a fresh loop.
    pub(crate) fn next(&self, uri: &Url) -> Option<Option<u64>> {
        match self.states.entry(uri.clone()) {
            Entry::Occupied(mut entry) => {
                if entry.get().dirty {
                    let state = entry.get_mut();
                    state.dirty = false;
                    Some(state.ticket)
                } else {
                    entry.remove();
                    None
                }
            }
            Entry::Vacant(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uri() -> Url {
        Url::parse("file:///s.rs").unwrap()
    }

    #[test]
    fn first_schedule_spawns_and_second_marks_dirty() {
        let s = ParseScheduler::default();
        let u = uri();
        assert!(s.schedule(&u, Some(1)), "first schedule spawns the loop");
        assert!(
            !s.schedule(&u, Some(2)),
            "a second schedule while parsing only marks dirty"
        );
    }

    #[test]
    fn next_continues_when_dirty_with_latest_ticket_then_exits() {
        let s = ParseScheduler::default();
        let u = uri();
        assert!(s.schedule(&u, Some(1)));
        assert!(
            !s.schedule(&u, Some(5)),
            "edit during parse marks dirty, ticket=5"
        );

        // Loop finishes its first parse: dirty was set → continue at the latest ticket.
        assert_eq!(s.next(&u), Some(Some(5)));
        // Nothing more pending → loop exits and the entry is cleared.
        assert_eq!(s.next(&u), None);
    }

    #[test]
    fn after_exit_a_new_schedule_spawns_again() {
        let s = ParseScheduler::default();
        let u = uri();
        assert!(s.schedule(&u, Some(1)));
        assert_eq!(s.next(&u), None, "no dirty → exit");
        assert!(
            s.schedule(&u, Some(2)),
            "a fresh edit after the loop exited spawns a new loop"
        );
    }

    #[test]
    fn documents_are_independent() {
        let s = ParseScheduler::default();
        let a = Url::parse("file:///a.rs").unwrap();
        let b = Url::parse("file:///b.rs").unwrap();
        assert!(s.schedule(&a, Some(1)));
        assert!(
            s.schedule(&b, Some(1)),
            "a different document spawns its own loop"
        );
    }
}
