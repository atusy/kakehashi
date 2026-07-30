# Execute Command Routing Token

**Related Decisions**:
[language-server-bridge-request-strategies](language-server-bridge-request-strategies.md),
[ls-bridge-server-pool-coordination](ls-bridge-server-pool-coordination.md),
[host-document-bridge](host-document-bridge.md),
[lazy-node-identity-tracking](lazy-node-identity-tracking.md)

## Context

`workspace/executeCommand` carries `command`, `arguments?`, and
`workDoneToken?` — no `textDocument`. An ordinary language server needs none:
it already knows its own workspace and its own open documents, so a command
name is a complete instruction. The document knowledge is not absent, it is
*implicit*.

Kakehashi has no such implicit knowledge. It multiplexes N downstream servers
across M workspace roots, so "which process runs this command" is not
determined by the method and must be carried explicitly. `codeAction/resolve`
can route through the action's `data` envelope; `workspace/executeCommand` has
no envelope, so the command **name** is the only available channel. Rewriting a
name the bridge itself minted is legitimate; the question is what to put in it.

The name currently encodes `(origin, host_uri, command)`, and `host_uri` serves
two unrelated purposes:

| Purpose | What it actually needs | How `host_uri` supplies it |
|---------|------------------------|----------------------------|
| **Routing** | the target *connection* — `(server, root)` | root is derived by walking workspace markers up from the document path |
| **State repair** | the *host document* whose virtual documents must be re-opened | directly |

Repair exists because a downstream respawn purges that connection's document
tracker, while `workspace/executeCommand` — unlike the request paths — has no
`ensure_document_opened` step. The command's `arguments` typically reference a
virtual document that the replacement process never opened, so the bridge
re-opens it just before dispatching.

Conflating the two costs three things:

- **The command set is unbounded.** A per-document name can be neither
  statically advertised nor dynamically registered, so
  `executeCommandProvider.commands` is empty. Clients that dispatch an action's
  command only when its id appears in a registered list (vscode-languageclient)
  display such an action and silently do nothing; clients that dispatch on
  provider presence (Neovim's built-in client) work.
- **Repair is user-latency-bound.** It runs inside the user-facing request under
  a 2 s timeout, and dispatches unrepaired when that expires.
- **The routing token depends on a mutable document.** The name outlives the
  edit, close, or reparse of the document it names.

## Decision

Separate the two concerns.

### 1. The routing token identifies a connection, not a document

```
kakehashi|{root_tag}|{server}|{root}|{command}
```

`root_tag` is a single letter for the connection's rooting mode (marker,
client-root fallback, shared instance); `server` is the config key; `root` is
the marker root URI (empty for the two document-less modes); `command` is the
downstream's own id, taken as the remainder so it needs no constraint.

`server` and `root` are escaped — `%` first, then the `|` separator — which
makes the split boundaries separator-free **by construction**. The previous
encoding could only *enforce* that invariant by refusing to mint a name
(dropping the command) when a config key contained the separator; escaping
removes that failure mode entirely. Only those two characters are escaped, so a
root that already carries percent escapes gets them doubled (`%20` → `%2520`) —
correct, if not pretty.

The separator itself moved from the 0x1f control character to `|`. That is not
what makes the encoding unambiguous (escaping is), but these names reach logs,
editor UIs, and JSON — where RFC 8259 forces a `\u001f` escape for the old
separator — so a printable byte is easier to transport and to read.

This is exactly the `(server, root)` identity the pool already keys every
connection by, and the by-key acquisition path already exists:
`ready_connection_by_key` for a live connection, and
`acquire_resolved_wait_ready(.., connection_key, marker, ..)` for a
reconnect.

### 2. State repair belongs to respawn, not to the request path

When a connection is purged the document tracker already computes the virtual
URIs that connection had open, and holds a host→virtual index besides. Those
resolve back to host documents. The re-open is therefore driven by the purge
set and runs when the replacement connection reaches `Ready` — before any
request needs it, and off the user-facing path.

Re-opening needs document *content*, which only the host document and its
injection regions can supply, so the work is performed on the server side
(which owns the document store and injection resolution) and reached through
the existing pool→editor upward request channel. The pool decides *when*; the
server side decides *what*.

That split makes the repair **asynchronous**, where the inline version was
awaited — and being awaited was load-bearing, not incidental. The outbound
queue to a downstream is FIFO, so a request enqueued before the re-open's
`didOpen` reaches the server first and asks about a document that server has
not opened. Two things restore the ordering:

- The re-open itself must **await the open**, not merely start it. The eager
  batch path spawns a detached task per server and returns, so completing it
  proves nothing; the re-open drives the awaited per-server open instead. That
  also scopes the work to the server that actually respawned.
- A request that depends on the repair synchronizes on it explicitly. The
  connection's pending re-open is claimed *before* the connection is published
  as `Ready`, so a request unblocked by that transition can see it, and requests
  wait on it under the same bound the inline heal used. Dropping the completion
  signal releases the waiters, so a failed re-open degrades to the old lazy
  behaviour instead of stalling.

The claim is reversible: a handshake that dies after claiming the set restores
it, because the purge that recorded it already emptied the tracker and no later
purge would report those documents again.

This is sound across intervening edits because region ids are position-keyed
and shifted by edits rather than re-minted
(lazy-node-identity-tracking), so a virtual URI captured at purge
time still names the same region afterwards.

Repair then benefits every request path, not just `workspace/executeCommand`,
and the routing token no longer has to name a document.

## Considered Options

### Keep `host_uri` in the token (status quo)

Rejected. It works, but it is what makes the command set unbounded, and it
leaves repair on the user-facing path. The document is carried only because
repair needs it — routing needs a root, and a root is a property of the
connection.

### Encode the connection key (chosen)

Document-free, stateless, and collision-safe for the case that motivated the
original design (two servers advertising the same command id). It also makes
the set of routable names finite: one entry per `(server, root, command)`,
which is the precondition for advertising them.

### Opaque handle plus a mint table

`kakehashi.cmd.7` with a session table holding `(ConnectionKey, command)`.
Shorter names, and no separator or escaping rules at all — once names are
enumerated for registration, the table is already being kept, so statelessness
buys less than it appears to.

Rejected because it trades away self-description. A stale encoded name fails
with a specific diagnosis (`origin is not spawnable`, `no resolvable config`);
a stale handle resolves to a *different* command or to nothing, which is the
one outcome this path must avoid — it is user-invoked, where the standing rule
is fail soft but never silently.

### Recover the host document from `arguments`

The virtual URI embedded in a command's arguments does encode its host. But
`arguments` are opaque by contract and forwarded verbatim, and not every
command carries a URI, so this is a heuristic dressed as a lookup. Rejected as
a routing mechanism; it remains a legitimate fallback nowhere in this design.

### Re-open every open host document on respawn

Correct but wasteful: it re-resolves injections for documents the dead
connection never held. Unnecessary, since the purge already knows the exact
set. Rejected in favour of the captured set.

## Consequences

### Positive

- `workspace/executeCommand` routing no longer references a document, matching
  the shape the protocol assumes.
- The set of routable command names becomes finite and enumerable — the
  prerequisite for advertising action-embedded commands to clients that
  dispatch only registered ids.
- Repair moves off the user-facing request and covers all request paths. The
  ordering barrier is taken by both `workspace/executeCommand` routes (the
  encoded one and the palette one); other request paths benefit from the repair
  without waiting on it, since they open their own documents first.
- The "separator in a config key drops this server's commands" failure mode is
  gone; escaping makes the invariant structural.
- Command names no longer grow with document path length.

### Negative

- Two mechanisms change at once for a single user-visible behaviour, so a
  regression in repair now surfaces as a stale-virtual-document error on
  whichever path happens to run first, rather than on `executeCommand` alone.
- Repair is asynchronous, so ordering that the inline `await` gave for free is
  now an explicit barrier. A future request path that depends on repaired
  document state must remember to wait on it; forgetting is silent, and shows
  up only as a downstream error about an unopened document.
- A respawn re-opens the dead connection's whole document set at once, where
  the previous design re-opened one document lazily. Large workspaces on a
  shared instance pay this as a burst.
- Encoded names minted by an older build no longer decode. Harmless in
  practice — names are minted fresh in each `textDocument/codeAction` response
  and never persisted — but a client that cached an action across a kakehashi
  upgrade gets one fail-soft null.

### Neutral

- The token grows a root segment and loses a document segment; net length is
  usually shorter, since a marker root is a prefix of the document path.
- `arguments` handling is untouched: still forwarded verbatim in the
  downstream's own coordinate system.

## Decision–Implementation Gap

**Dynamic registration of encoded names is deferred.** Making the name set
finite is the precondition, not the feature. Two questions must be settled
empirically before registering them, and neither can be answered from the
protocol text:

- Roots are discovered lazily as documents open, so the registerable set grows
  during a session. The existing palette registration mints a fresh
  registration id per batch and never unregisters; whether real clients
  tolerate repeated `workspace/executeCommand` registrations with overlapping
  command lists is untested.
- Whether a client that filters on registered ids also expects a
  human-readable command id, given these are machine-generated.

Until that is settled, `executeCommandProvider.commands` stays empty and the
vscode-languageclient limitation stands.

**A DEAD shared-instance connection cannot be re-rooted from the token alone.**
A `preferSharedInstance` connection is keyed without a root, and announcing a
workspace folder to it requires a marker resolved from a document. Routing to a
LIVE shared connection works — including one still mid-handshake, which the
by-key path waits out rather than dropping — but reviving a dead one and
restoring its folder set does not, and fails soft as before.
