# Respawn Re-open Derives Its Targets

**Related Decisions**:
[execute-command-routing-token](execute-command-routing-token.md),
[ls-bridge-server-pool-coordination](ls-bridge-server-pool-coordination.md),
[language-server-bridge-virtual-document-model](language-server-bridge-virtual-document-model.md),
[host-document-bridge](host-document-bridge.md)

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

Nothing is remembered about documents, so nothing about them can go stale.

### Arming is unconditional, and that is the load-bearing part

The previous design armed only when the captured list was non-empty, which is
what left a young connection's replacement unrepaired. Arming records that a
connection was replaced — a fact about the connection, not about its contents —
so there is nothing to be empty. A first-ever spawn is still free: no prior
purge, no armed key, no re-open.

Symmetrically, a handshake that finds an armed key always emits the re-open
request. Both halves must be unconditional; leaving either gated on a captured
set would preserve the hole while appearing to fix it.

### Belonging is decided per host, against current settings

Which documents are a connection's is not a property of language alone. A
connection is a `(server, root)` pair, so a document that bridges to the right
server but sits under a different root is not its document.

So each candidate host is screened in stages, cheapest first, and the ordering
is a correctness property rather than a micro-optimization: the re-open runs
inside a fixed budget that `done` must signal within, so work done per candidate
is work charged against every command on that connection. Screening after the
expensive steps would make that budget scale with workspace size instead of
with the work that belongs to the connection.

1. Could a document in this HOST language bridge to this server at all? Pure
   configuration, answered from the per-snapshot memo — no parse, no tree, no
   pool lookup, no filesystem access. It rejects hosts whose configured `bridge`
   filter blocks every language the server declares, and servers no longer
   configured. How much that narrows depends entirely on the configuration: on
   the shipped defaults the bridge filter allows everything, so a workspace of
   same-host-language documents is barely thinned, and the later stages carry
   the load. It must still run before the parse wait and the injection
   resolution, not merely before the open.
2. Do its resolved injections actually bridge to this server? Also pure
   configuration, but it needs the injections, so it is paid only by hosts that
   survived (1).
3. Does it route to *this* connection? A marker resolution, paid only by hosts
   that survived (2). Read-only: it never spawns, so asking about a document
   belonging to another root cannot bring that root's server up.

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
  project) re-roots a host with settings untouched.
- The re-open considers every open document rather than a pre-narrowed set. The
  configuration question is answered first and from a memo, so the cost is a
  map lookup per open document, but it does scale with the workspace rather
  than with what one connection held. The ordering above is what keeps that
  cost off the barrier's budget; a future change that moves work ahead of the
  stage-1 screen re-couples them, and the symptom is every command on a
  respawned connection failing soft on a large workspace.
- Marker resolution now runs during the re-open for hosts that bridge to the
  respawned server. The pre-existing eager path already resolves markers per
  open, so this is not a new kind of work, but it is work the captured-list
  design skipped.

### Known limits of `done`

Four ways the sweep's report can be wrong — mostly by claiming success for a
connection that is not caught up, and in the last case by claiming failure for a
document nobody is owed. All are narrow, all degrade to the pre-existing lazy
heal (the next parse's eager open), and none is introduced here — but the
barrier's contract is stated in terms of `done`, so they belong written down
rather than implied.

**An invalidation placeholder reads as a current parse.** `invalidate_parse`
publishes a tree-less snapshot whose `parsed_version` equals the content
version, so both the parse wait and the currency re-check classify it as
settled. The sweep then resolves no injections — because there is no tree, not
because the host has no regions — and reports success. This matters most on the
settings-reload path, which invalidates every parse and purges connections in
the same pass. Distinguishing a placeholder from a legitimately tree-less parse
(no parser loaded, parse produced nothing) needs a discriminator the snapshot
does not currently carry, and rejecting every tree-less snapshot instead would
wedge the barrier shut while a parser is still being installed. Tracked as the
same class as the reload-placeholder issue in the parse-snapshot work.

**An empty resolution can be confirmed against a NEWER version.** If a
`didChange` clears the tree and its reparse publishes before the currency
re-check runs, the check passes on version N+1 while the emptiness came from N.
The new version's own `process_injections` opens the documents, so the
connection is repaired — just not by this sweep, and possibly after the barrier
released.

**A `didOpen` can carry superseded content.** `ensure_document_opened` re-reads
the latest virtual content immediately before enqueue, but that cache is
refreshed when a `didChange` is FORWARDED, which happens after the reparse the
edit scheduled. A sweep that claims the document in between reads the older
content and enqueues it. The open claim does order the eventual
`didChange` after this `didOpen`, so the downstream converges — but it does not
order either of them against the command the barrier is about to release, so a
command can arrive between them.

**The incarnation checks are not atomic with what they guard.** A close landing
between the liveness check and the currency re-check still reports failure, and
a close+reopen can validate an empty result from one lifetime against a snapshot
from the next. Same shape as the version case above: a two-step check over a
value that can move between the steps. Closing it properly needs one lookup
returning gone / exactly-current / changed rather than two booleans.

### Neutral

- The barrier itself is unchanged: same claim-before-Ready ordering, same
  bound, same fail-soft-on-unsettled rule. Only what it is a barrier *for*
  changed.
- Arming a key whose connection is never replaced leaves one entry until the
  key is next claimed. Bounded by the number of distinct `(server, root)` pairs.
