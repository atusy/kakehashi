# Respawn Re-open Derives Its Targets

**Related Decisions**:
[execute-command-routing-token](execute-command-routing-token.md),
[ls-bridge-server-pool-coordination](ls-bridge-server-pool-coordination.md),
[language-server-bridge-virtual-document-model](language-server-bridge-virtual-document-model.md),
[host-document-bridge](host-document-bridge.md),
[bridge-routing-protocol](bridge-routing-protocol.md)

## Context

When a bridged downstream is replaced, the fresh process has nothing open. The
replacement must be told about the virtual documents it is expected to serve,
or it answers requests about documents it has never seen.

execute-command-routing-token established *when* that happens (at respawn,
signalled by a barrier) and *who* does it (the server side, which owns document
content). It answered *which documents* by capturing them: the purge returned
the host documents the dead connection had held, and the replacement replayed
that list.

Capturing seemed forced by the situation — the purge is the moment the
information is destroyed, so it looked like the last moment it was knowable.
But a captured list is a claim about a state that has already stopped being
true, and every way it could drift from the present needed its own repair:

| Drift | Repair it needed |
|-------|------------------|
| a document closed after the purge | the claim DRAINS the list |
| a second purge before any replacement lands | the record UNIONS instead of replacing |
| the handshake dies after claiming | restore the claimed list |
| the hand-off send fails | restore the claimed list again |
| a config change re-roots a host | carry the claimed key and acquire by it |

Four restore-shaped repairs against one cause. And one divergence had no repair
available at all: a connection that died before opening anything held nothing,
so its purge captured an empty list, which was skipped — nothing was recorded
and nothing was scheduled. Its replacement was never repaired by anyone.
Restoring could not help, because the list had never existed. That case is not
exotic: it is precisely a connection that fails during startup and is replaced,
which is when a replacement most needs the repair.

The pattern points at the premise. The question "what did the dead process
hold?" is not the question that needs answering. The question is "what should
this connection hold?" — and that has an answer that is true now.

## Decision

**Derive the re-open set from current state; remember only that a connection
owes one.**

A purge ARMS its key. The replacement's handshake CLAIMS it. The re-open then
asks, of every currently open document, whether it belongs to this connection,
and opens the ones that do.

No captured re-open *target list* is remembered, so no such list can go
stale. (Per-document route bindings — bridge-routing-protocol — *are*
remembered for each binding's lifetime — the decided document's close;
a virtual document closes when its last region leaves — and are consulted by the
belongs-here question below; their staleness is that decision's recorded
trade-off, not a captured-list resurrection.)

### Arming is unconditional, and that is the load-bearing part

The previous design armed only when the captured list was non-empty, which is
what left a young connection's replacement unrepaired. Arming records that a
connection was replaced — a fact about the connection, not about its contents —
so there is nothing to be empty. An ordinary first-ever spawn is still free:
no prior purge, no armed key, no re-open. Crash recovery also arms a newly
diverted per-root key before acquiring it when a replacement shared instance
lacks folder-change support (#1126). That process has no predecessor under its
own key, but inherits current document demand from the crashed shared instance;
its handshake claims the debt and derives its documents in the same way.

Symmetrically, a handshake that finds an armed key always emits the re-open
request. Both halves must be unconditional; leaving either gated on a captured
set would preserve the hole while appearing to fix it.

### Belonging is decided per host, against current settings

Which documents are a connection's is not a property of language alone. A
connection is a `(server, root)` pair, so a document that bridges to the right
server but sits under a different root is not its document.

So each candidate host is screened in stages, cheapest first, and the ordering
is a correctness property rather than a micro-optimization. Three bounds are
in play and none is the sweep's lifetime: the sweep WAITS for pending parses
against one shared deadline (`REOPEN_WAIT`), each command waiting on `done`
applies its own `REOPEN_WAIT` and fails soft when it passes, and the sweep
itself keeps walking — without waiting — until every candidate has been
looked at, however long that takes. Work done per candidate before the
cheap screen is therefore work charged against every command on that
connection while the sweep runs; screening after the expensive steps would
make a command's wait scale with workspace size instead of with the work
that belongs to the connection.

1. Could a document in this HOST language bridge to this server at all? Pure
   configuration, answered from the per-snapshot memo — no parse, no tree, no
   pool lookup, no filesystem access. The screen accepts the **union** of
   injection reachability and the host layer's own candidacy (`_self`
   enabled with a host-language-matching server — bridge-routing-protocol's
   host units), so a host-only route is not pre-rejected by an
   injection-shaped filter. It rejects hosts whose configured `bridge`
   filter blocks every language the server declares, and servers no longer
   configured. How much that narrows depends entirely on the configuration: on
   the shipped defaults the bridge filter allows everything, so a workspace of
   same-host-language documents is barely thinned, and the later stages carry
   the load. It must still run before the parse wait and the injection
   resolution, not merely before the open.
2. Which of its route units bridge to this server? This stage produces
   **units**, not a host-level verdict (bridge-routing-protocol): each
   resolved injection language whose `bridge` filter admits the server is an
   injection unit, and the host layer is a unit of its own when `_self` is
   enabled with a host-language match — a host-only `_self` route survives
   with no matching injection at all. Pure configuration, but the injection
   half needs the injections, so it is paid only by hosts that survived (1).
3. Does the unit route to *this* connection? A marker resolution, paid per
   unit surviving (2). Read-only: it never spawns, so asking about a document
   belonging to another root cannot bring that root's server up.
   bridge-routing-protocol amends this stage (target state, landing with
   that protocol's implementation): when the document holds an
   active route binding for the server, the binding answers instead of the
   marker walk — a suppressed server is "not applicable", a bound key must
   match this connection's — and an entry-less server (this exact (document, server) entry has no
   record, whatever the document's other servers settled) falls through
   to the marker rule above. Bindings
   are keyed per decided document (the host, and each virtual document),
   so stages 2-3 evaluate each unit **independently**: every current
   virtual document is its own unit with its own binding, the host layer
   is a unit screened by `_self`/host-language candidacy rather than
   injections, and each unit enqueues its own `didOpen`; any
   required-open failure fails the applicable host, while a host with
   zero applicable units reads NotApplicable. One host can suppress a
   server for one injection language while another language — or the
   host layer — routes to a different key, and the sweep re-opens only
   the units whose own **server entry** (within the unit's decision
   tuple) names this connection. A server entry still *pending* at the
   sweep's bounded wait is applicable-but-unsettled, not "not
   applicable": the barrier's fail-soft path applies, never a
   successful omission; a sibling entry's state never decides this
   server's applicability. This membership check stays read-only;
   completing an absent host routing decision before synchronization is the
   separate exception described below.

Stage 1 is deliberately conservative — a server declaring the `*` wildcard is
never pre-rejected, and inheritance from the `_` template is resolved before the
list is read, because a server that omits `languages` reads as declaring nothing
until the template is merged in.

It is also advisory, in that it reads the host language before the parse wait
while the authoritative language is re-read with the injections. That asymmetry
runs one way only, and the safe direction is the ACCEPTING one: a wrong accept
costs an unnecessary stage 2, while a wrong reject skips the document and still
reports success. Any future narrowing of stage 1 has to be judged against the
reject direction, which no test can observe from the outside.

Only then is the connection acquired, and acquired BY KEY rather than by what
the host resolves to. Both are needed and they are not the same check. The
routing question decides whether this host belongs here; acquiring by key
decides that the open lands on the connection the barrier signals for. A by-key
lookup succeeds whichever host asked, so without the routing question a sweep
over every open document would cross-open one root's documents onto another
root's process.

### Host synchronization is part of every re-open

The host layer is restored before waiting for the host's injection parse. It
uses the real URI and current text, read together with its language and
revision, and only an existing Ready connection under the named key. Shared
workspace announcements precede its `didOpen`, as for injected regions.

If a cancelled eager batch left the host's routing decision absent, the
re-open first finishes ordinary provider selection over the full host-server
candidate set. That step may acquire candidates: a sibling provider must still
be able to suppress the target. It is serialized by the host lifecycle lock
and guarded by the current settings and document lifetime, including before
acquisition and before recording the answer. Hosts already routed elsewhere
are excluded before waiting for that lock. The subsequent exact-key sync still
uses only an existing Ready connection and rechecks routing before `didOpen`.

This work is awaited directly rather than delegated to edit-driven eager
batches. Superseding or cancelling an eager batch therefore cannot release the
re-open barrier before its host `didOpen` is enqueued. This also applies when a
replacement's handshake claims a consolidation's debt (#1116). An applicable
host sync failure contributes `false` to the barrier; a closed or differently
routed host supplies nothing for that key. Host-only demand can trigger crash
recovery without a tree or an editor request (#1127).

### The third outcome is "not applicable", not "wrong"

An open reports one of three things: it happened; it was not this connection's
document; or it was and it failed.

The middle case is the common one under derivation, and it is not a failure.
Only an applicable host that failed to open may mark the connection as not
caught up. Conflating them would report failure on essentially every respawn,
holding the barrier shut so that every command pays the full wait and then
fails soft — a correctness mechanism turned into a latency tax that also
withholds correct results.

### What the barrier now means

`done` reports whether every host this re-open judged applicable was opened on
this connection — a per-connection property, matching what the barrier is keyed
by and what a routing token names. Under the captured-list design it reported
whether N remembered hosts had been restored: a per-host property forced into a
per-connection signal, which is why its granularity never quite fit.

It is a report on the sweep, not a proof of completeness. A host whose tree does
not settle inside the budget IS reported — it marks the connection not caught
up, because an empty resolution from a document with no tree says nothing about
that document. What stays invisible is a host misjudged as not-applicable:
skipping is indistinguishable from having nothing to do, by construction. That
asymmetry is the price of the three-way outcome — it buys a barrier that is not
permanently shut, and it makes every future misclassification silent. See
"Known limits of `done`" below for the cases that remain.

## Considered Options

### Keep the captured list and add a bounded wait for the Initializing case

The unrepaired-replacement hole can be closed by having the re-open wait for a
still-initializing replacement instead of giving up on it. It works, and it
would have been a fifth repair against the same cause — arriving after four
others, in a mechanism where each one had made the next harder to see. Rejected
in favour of removing the cause. Under derivation the case dissolves rather
than being handled: nothing is lost when a re-open gives up, because the next
one re-derives.

### Derive by language only, without the root check

Simpler, and wrong. Two roots each running the same server would repair each
other: a respawn under root A would open root B's documents onto A's process.

### Derive, but resolve each host's connection instead of acquiring by key

Resolving from the host is how the pre-#927 design worked and it re-introduces
that bug: it finds whichever connection the host routes to now, which after a
re-rooting is not the one the barrier signals for. It also spawns, so a sweep
would start servers for roots nobody asked about.

### Keep remembering, but recompute the list at claim time

A middle path: capture at purge, then filter against current state before
replaying. This is derivation with a redundant input — the filter is doing all
the work, and the captured list only narrows what the filter would have found
anyway, incorrectly, since it cannot include documents opened since the purge.

## Consequences

### Positive

- A replacement of a connection that died before opening anything is now
  repaired. Previously it never was, silently.
- `purge_connection`'s return value, the remembered host map, and the
  record/take/restore lifecycle are gone, along with the class of bug where a
  claimed set is dropped on a failure path.
- The barrier's signal is per-connection in meaning as well as in keying.
- Documents opened *since* the purge are now included; the captured list could
  only ever shrink.
- The re-open no longer touches documents the editor has closed, without
  needing a drain to arrange it.

### Negative

- A host re-rooted away from the connection being repaired is no longer
  re-opened onto it. This is a real regression against what
  execute-command-routing-token's follow-up deliberately built: it made the
  re-open acquire the CLAIMED connection precisely so a re-rooted host would
  still be restored there. Current settings are the authority now, so that
  connection's correct contents are nothing — but a command already in flight
  against the
  old root now fails downstream rather than being served. It needs BOTH a
  respawn and a re-rooting — a live connection already holds its documents, and
  the barrier is a no-op for it. Re-rooting is not only a configuration change:
  marker resolution walks the live filesystem uncached, so creating a marker
  (`git init` in a subdirectory, a submodule checkout, scaffolding a nested
  project) re-roots a host with settings untouched. Both apply only to
  (document, server) entries *without* an active route
  binding record — a terminally deleted entry falls through to marker
  resolution even while sibling entries stay bound: a bound entry
  (bridge-routing-protocol) keeps its key for the binding's lifetime —
  the decided document's close (a virtual document closes when its
  last region leaves) — and is not re-rooted by live marker changes while it
  lives.
- The re-open considers every open document rather than a pre-narrowed set. The
  configuration question is answered first and from a memo, so the cost is a
  map lookup per open document, but it does scale with the workspace rather
  than with what one connection held. The ordering above is what keeps that
  cost off the commands' wait (the sweep itself may outlive it); a future
  change that moves work ahead of the stage-1 screen re-couples them, and
  the symptom is every command on a respawned connection failing soft on a
  large workspace.
- Marker resolution now runs during the re-open for the server entries that
  lack a route binding record; bound entries answer from the binding
  instead (per exact (document, server) entry — one host's units can
  hold both kinds at once). The pre-existing eager path already resolves markers
  per open, so this is not a new kind of work, but it is work the
  captured-list design skipped.

### Empty resolutions retain their input revision

The sweep captures the document's incarnation and content version before
resolving injections. An empty result is confirmed against that exact revision
in one snapshot lookup, which distinguishes a closed document, an unchanged
current parse, and a changed document. A closed document is no longer owed a
repair. A newer current parse or a reopened lifetime cannot confirm the old
empty result; the sweep reports incomplete instead. The snapshot language is
also checked before cached regions or a missing injection query can establish
that the host has no injections. A nonempty resolution that routes to no units
on this connection must pass the same revision check after routing: an edit
while routing is pending may introduce a unit that does belong here.

### Undeterminable parses fail soft

`invalidate_parse` publishes a tree-less snapshot whose `parsed_version`
equals the content version, so the parse wait alone can classify it as settled.
The sweep does not use that currency as proof of an empty injection set:
`bridge_injections` reports an undeterminable result for the placeholder, and
`done` reports incomplete. This already prevented the false-success placeholder
case described in #929 before the revision and content checks above were added.

Commands observing this incomplete result fail soft. Once the sender drops,
the registry retires that completed barrier when a waiter observes it; a
permanently tree-less document therefore does not leave that barrier blocking
all later commands. A subsequent parse re-opens its regions eagerly. A tree-less
snapshot still cannot distinguish a pending parse from a document that will
never produce a tree, so the conservative failure remains an availability
trade-off. It is not evidence that the downstream already holds its documents.

### Required opens use revision-validated content

A nonempty resolution carries the same captured incarnation and content
version into the open. After routing completes, each injection takes the host
edit lock, checks that revision, and retains the lock through enqueue. An edit
that supersedes the resolved text makes the repair incomplete. Routing runs
before this lock is acquired because it may wait on downstream queries.

A required open uses the verified snapshot text directly: the latest forwarded
virtual-content cache may still lag the completed parse. If another request
already opened the virtual document, the repair sends a full `didChange` when
its content differs. A failed enqueue reports incomplete and leaves the sent
content fingerprint unchanged, so a later attempt can retry. Thus the repair
cannot report success merely because a document was already opened with older
text. Ordinary deferred eager opens continue to refresh from the forwarded
cache, since they do not carry this revision guarantee.

### Neutral

- The barrier itself is unchanged: same claim-before-Ready ordering, same
  bound, same fail-soft-on-unsettled rule. Only what it is a barrier *for*
  changed.
- Arming a key whose connection is never replaced leaves one entry until the
  key is next claimed. Bounded by the number of distinct `(server, root)` pairs.
