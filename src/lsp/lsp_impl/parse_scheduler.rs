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

use std::sync::Arc;

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
        // Hot path (a live document being edited): `get_mut` borrows the key, so no
        // `Url` clone; only a first-edit miss pays for the owned key via `entry`.
        if let Some(mut state) = self.states.get_mut(uri) {
            // Keep the highest ticket: writer tickets are monotonic, and a later
            // schedule carrying `None` (or, defensively, an out-of-order lower one)
            // must not regress the watermark target. `None < Some(_)` in `Option`'s
            // ordering, so `max` keeps any real ticket.
            state.ticket = state.ticket.max(ticket);
            return if state.parsing {
                state.dirty = true;
                false
            } else {
                state.parsing = true;
                state.dirty = false;
                true
            };
        }
        match self.states.entry(uri.clone()) {
            // Re-check: a concurrent `schedule` may have inserted between the
            // `get_mut` miss and here.
            Entry::Occupied(mut entry) => {
                let state = entry.get_mut();
                state.ticket = state.ticket.max(ticket);
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

    /// Start a parse pass: **clear `dirty`** and return the latest ticket. Called
    /// at the top of each loop iteration, *before* reading the document text, so an
    /// edit arriving after this re-sets `dirty` and [`finish`](Self::finish) loops
    /// again — while an edit already folded into this pass's text does **not**
    /// trigger a redundant reparse of the same text. The outer `Option` is `None`
    /// only if the entry is somehow gone (defensive); the inner is the ticket.
    pub(crate) fn start_pass(&self, uri: &Url) -> Option<Option<u64>> {
        self.states.get_mut(uri).map(|mut state| {
            state.dirty = false;
            state.ticket
        })
    }

    /// Called by the parse loop after one pass (parse + downstream) completes, to
    /// decide whether to loop again. Returns `true` when an edit landed during the
    /// pass (`dirty`) — keep the entry and loop (the next [`start_pass`] clears it);
    /// `false` when nothing is pending — the entry is removed and the loop exits.
    ///
    /// Uses `remove_if`, which holds the shard write lock across the predicate
    /// **and** the removal, so it is atomic against a concurrent `schedule`: that
    /// `schedule` either sets `dirty` before the predicate runs (entry kept → we
    /// continue) or after the removal (entry re-created → it spawns a fresh loop).
    /// A `get_mut`-then-`remove` split would instead let `schedule` set `dirty`
    /// between the two and then be deleted — a lost wakeup. `remove_if` also borrows
    /// the key (no `Url` clone).
    ///
    /// `continue_loop` is set **inside** the predicate so it distinguishes
    /// kept-because-dirty (continue) from a missing entry (stop). A bare
    /// `remove_if(...).is_none()` would return `true` for a missing entry too,
    /// trapping the loop in an infinite, CPU-burning cycle.
    pub(crate) fn finish(&self, uri: &Url) -> bool {
        let mut continue_loop = false;
        self.states.remove_if(uri, |_, state| {
            if state.dirty {
                // Keep the entry and loop again; the next `start_pass` clears dirty.
                continue_loop = true;
                false
            } else {
                // Nothing pending — remove the entry and stop.
                true
            }
        });
        continue_loop
    }

    /// Clear `uri`'s scheduling entry unconditionally, leaving the next
    /// `did_change` free to spawn a fresh loop. Used when a parse loop stops
    /// **without** the normal `finish()` removal — a panic (via [`ParseLoopGuard`]),
    /// a server shutdown, or the document being closed mid-parse — all of which
    /// would otherwise leave the entry stuck at `parsing: true` and wedge the URI
    /// (every later edit only marks it `dirty`, never re-spawning).
    pub(crate) fn clear(&self, uri: &Url) {
        self.states.remove(uri);
    }
}

/// Re-arms the scheduler if the spawned parse loop is torn down without finishing
/// normally — i.e. a panic in the loop glue (`reparse_latest` / `process_injections`
/// / `republish`; the blocking parse itself is already panic-isolated by
/// `spawn_blocking`). On a clean exit the loop calls [`disarm`](Self::disarm) (its
/// `finish()` already removed the entry); on a panic the guard's `Drop` runs during
/// unwind and clears the stuck entry so the next `did_change` re-spawns.
pub(crate) struct ParseLoopGuard {
    scheduler: Arc<ParseScheduler>,
    uri: Url,
    armed: bool,
}

impl ParseLoopGuard {
    pub(crate) fn new(scheduler: Arc<ParseScheduler>, uri: Url) -> Self {
        Self {
            scheduler,
            uri,
            armed: true,
        }
    }

    /// Mark a normal loop exit so `Drop` does not clear the entry (the loop's
    /// final `finish()` already removed it, and a fresh loop may have been spawned).
    pub(crate) fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ParseLoopGuard {
    fn drop(&mut self) {
        if self.armed {
            self.scheduler.clear(&self.uri);
            log::warn!(
                target: "kakehashi::parse_actor",
                "parse loop for {} aborted abnormally; cleared its schedule so the next edit re-spawns",
                self.uri
            );
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
    fn start_pass_clears_dirty_and_finish_continues_then_exits() {
        let s = ParseScheduler::default();
        let u = uri();
        assert!(s.schedule(&u, Some(1)));

        // Pass 1 starts: clears dirty, takes the latest ticket (1).
        assert_eq!(s.start_pass(&u), Some(Some(1)));
        // An edit lands during the pass → dirty, ticket=5.
        assert!(!s.schedule(&u, Some(5)), "edit during parse marks dirty");
        // finish() sees dirty → continue (entry kept).
        assert!(s.finish(&u), "dirty during the pass → loop continues");

        // Pass 2 starts: clears dirty, takes the latest ticket (5).
        assert_eq!(s.start_pass(&u), Some(Some(5)));
        // No edit during pass 2 → finish() removes the entry and stops.
        assert!(!s.finish(&u), "no dirty → loop exits and entry removed");
        assert_eq!(s.start_pass(&u), None, "entry removed after exit");
    }

    #[test]
    fn no_redundant_pass_when_edit_arrived_before_the_first_start_pass() {
        let s = ParseScheduler::default();
        let u = uri();
        // Edit 1 spawns; edit 2 lands before the loop's first start_pass (marks
        // dirty). The first start_pass clears that dirty (its text already covers
        // edit 2), so finish() does NOT trigger a redundant same-text pass.
        assert!(s.schedule(&u, Some(1)));
        assert!(!s.schedule(&u, Some(2)));

        assert_eq!(s.start_pass(&u), Some(Some(2)), "first pass covers edit 2");
        assert!(
            !s.finish(&u),
            "dirty was cleared by start_pass → no redundant pass"
        );
    }

    #[test]
    fn after_exit_a_new_schedule_spawns_again() {
        let s = ParseScheduler::default();
        let u = uri();
        assert!(s.schedule(&u, Some(1)));
        let _ = s.start_pass(&u);
        assert!(!s.finish(&u), "no dirty → exit");
        assert!(
            s.schedule(&u, Some(2)),
            "a fresh edit after the loop exited spawns a new loop"
        );
    }

    #[test]
    fn guard_clears_a_stuck_entry_on_abnormal_drop() {
        let scheduler = Arc::new(ParseScheduler::default());
        let u = uri();
        assert!(scheduler.schedule(&u, Some(1)), "spawned: entry is parsing");

        // Loop panicked: the guard was never disarmed. Its Drop must clear the
        // stuck `parsing` entry so the next edit re-spawns.
        {
            let _guard = ParseLoopGuard::new(Arc::clone(&scheduler), u.clone());
            // dropped here without disarm() → abnormal-exit path
        }
        assert!(
            scheduler.schedule(&u, Some(2)),
            "after an aborted loop, the next edit must spawn a fresh loop (not just mark dirty)"
        );
    }

    #[test]
    fn disarmed_guard_leaves_a_freshly_spawned_entry_alone() {
        let scheduler = Arc::new(ParseScheduler::default());
        let u = uri();
        assert!(scheduler.schedule(&u, Some(1)));

        // Normal exit: finish() removed the entry, guard disarmed. A new loop may
        // have been spawned meanwhile; the disarmed guard's Drop must NOT clear it.
        let mut guard = ParseLoopGuard::new(Arc::clone(&scheduler), u.clone());
        let _ = scheduler.start_pass(&u);
        assert!(!scheduler.finish(&u), "no dirty → entry removed");
        guard.disarm();
        assert!(scheduler.schedule(&u, Some(2)), "a fresh loop is spawned");
        drop(guard); // disarmed → must not abort the fresh entry
        assert!(
            !scheduler.schedule(&u, Some(3)),
            "the fresh entry survived the disarmed guard's drop (still parsing)"
        );
    }

    #[test]
    fn finish_on_a_missing_entry_stops_the_loop() {
        let s = ParseScheduler::default();
        let u = uri();
        // No entry present (e.g. it was already removed). `finish` must return
        // false (stop) — a `true` here would spin the parse loop forever.
        assert!(
            !s.finish(&u),
            "finish on a missing entry must stop the loop, not continue"
        );
    }

    #[test]
    fn schedule_keeps_the_highest_ticket() {
        let s = ParseScheduler::default();
        let u = uri();
        assert!(s.schedule(&u, Some(5)));
        // A later schedule with no ticket (or a lower one) must not regress it.
        assert!(!s.schedule(&u, None));
        assert!(!s.schedule(&u, Some(2)));
        assert_eq!(
            s.start_pass(&u),
            Some(Some(5)),
            "the highest ticket is kept"
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
