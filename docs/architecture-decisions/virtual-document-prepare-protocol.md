# Virtual Document Prepare Protocol

**Related Decisions**:
- [language-server-bridge-virtual-document-model](language-server-bridge-virtual-document-model.md) — the virtual documents this protocol presents, including `injection.combined` and its coordinate-preserving masks
- [bridge-routing-protocol](bridge-routing-protocol.md) — the sibling kakehashi→downstream request and its `capabilities.experimental` discovery convention
- [ls-bridge-message-ordering](ls-bridge-message-ordering.md) — the per-connection ordering the held-document rule keeps intact
- [push-propagation-diagnostic-forwarding](push-propagation-diagnostic-forwarding.md) — the pushed diagnostics placed through what their connection held

## Context

Embedded code is often not valid in its own language as written. A Nix
indented string strips its common indentation, so the bash inside
`''…''` is indented by the host, not by the script; and its interpolations
(`${toString cfg.port}`) leave holes that a JSON or Python server sees as
syntax errors. Two contributions proposed teaching kakehashi each fix through
new query directives: `#set! injection.dedent` (#1082) and
`@injection.gap` with `injection.gap-placeholder` (#1083).

Both are presentation policies, and the right policy depends on the host
language, the embedded language, and the syntactic context — Nix's dedent
rule is not Python's `textwrap.dedent`, and the right placeholder for
`${…}` is `0` in JSON, `None` in Python, `$1` in SQL. Baking each policy
into kakehashi grows query vocabulary that also reads as if it affected
every injection consumer (highlighting included), and still covers only
the cases someone thought of.

## Decision

kakehashi asks a peer, `kakehashi/virtualDocument/prepare`, how to present
each virtual document before any downstream server sees it, and keeps all
coordinate bookkeeping itself. The peer is any language server that
advertises the request; tsudoi-language-server is the intended one, since
its handlers are user code.

Edits are not mapped back through a prepared document: methods whose
answers are edits are not available on one (see Coordinates).

### Configuration and Discovery

The peer is chosen like the servers of any other method, through the
per-method aggregation map under the method key
`kakehashi/virtualDocument/prepare`:

```toml
[languages.nix.bridge._.aggregation."kakehashi/virtualDocument/prepare"]
priorities = ["tsudoi"]
```

- The candidates are the servers bridged for the injection language — the
  ones its `languages` select, as for any request — so a peer lists the
  languages it prepares (or `"*"`), and receives their documents like any
  downstream server; `priorities` on the other methods (or the `"_"`
  method entry) keep it out of their fan-out.
- `priorities` follows aggregation-priorities-wildcard and inherits from
  the `"_"` method entry like every LSP method: an ordered allowlist,
  `"*"` for the unlisted rest (by server name), `["*"]` by default, and
  `[]` opts a language out.
- The strategy is always `preferred`: the first candidate, in priority
  order, that advertises
  `capabilities.experimental.kakehashi.virtualDocumentPrepare: true` in its
  `initialize` result prepares the document. A candidate still starting is
  waited for, so the choice does not depend on which server comes up
  first; one that does not advertise the request is passed over. Only that
  candidate is asked, and its answer stands (see Failure). A configured
  `concatenated` is ignored with a warning.
- When no candidate advertises the request, the document is sent as is.
  Each connection's advertisement is remembered from its last handshake;
  one that answered `MethodNotFound` counts as not advertising until its
  next one. A server advertises the request while any of its connections
  (one per workspace root) does, so a pair none of whose candidates
  advertises it is not held back per edit to find that out again. A
  connection replaced after a crash keeps its record until the
  replacement's handshake answers; one retired (its root removed, its
  replacement's handshake failed or timed out) loses it.
- A candidate that cannot start is passed over for that host's documents
  only — its connection for another workspace root may be healthy — until
  a handshake of it advertises the request, a settings change, or the host
  closing. A failure an attempt saw before such a clearing is not
  recorded.

Every answer stands under one **prepare epoch**. The epoch moves when a
connection's advertisement record changes or goes, or its server's does,
when start failures are cleared (but for one host's, on its close), when
settings change, and when runtime settings are pushed to servers once
anything was looked up. A first record moves nothing (a candidate
not yet recorded is waited for, so no answer was given without it), nor
does a replaced connection's handshake answering what its key already
advertised. Answers are keyed by
the input and its candidates' launch configurations, and are read only
under the epoch they were asked in; when the epoch moves, every open host
is synced again without waiting for an edit — a server that starts
advertising later has its pairs' documents prepared, and one that withdraws
it has them asked of the next candidate (or sent as is).

The feature is present only with `KAKEHASHI_EXPERIMENTAL=true`; without it
no advertisement is recorded and nothing is looked up. Only pairs that are
bridged and have a server for the injection language are prepared.

### The Request

```ts
interface VirtualDocumentPrepareParams {
  // The virtual document as kakehashi built it; `version` increases with
  // every request kakehashi makes (requests, even for one document, may
  // still arrive in either order).
  textDocument: { uri: DocumentUri; languageId: string; version: integer };
  hostTextDocument: { uri: DocumentUri; languageId: string };
  segments: { type: "content" | "gap"; content: string }[];
}

type VirtualDocumentPrepareResult = {
  segments: (
    | { type: "content"; changes?: TextEdit[] }
    | { type: "gap"; content?: string }
  )[];
} | null;
```

The segments cover the virtual document in order. A **content** segment is
injected text, and its `content` is that text. A **gap** is host text inside
the document's span that is not injected — the text between captures of an
`injection.combined` pattern — and its `content` is the original host text,
which the virtual document itself only carries as coordinate-preserving
whitespace. Host text the virtual document strips altogether (a blockquote's
`> `) is not presented on its own: between content, downstream servers never
see it, so there is nothing to replace; next to a gap it is part of that
gap's `content`. An isolated injection is a single content segment.

The answer has the same length and the same `type` order:

- A content segment's `changes` are segment-relative `TextEdit`s that may
  only **delete whitespace**, each change in one of two shapes. Omitted
  means unchanged.
  - Leading whitespace of a line: the dedent case.
  - Whole blank lines, line start to line start, at the **document's
    edges**: the lines opening the first segment, or those ending the last.
    When the last ends in a line holding only indentation (the indent
    before a closing `''`), deleting the lines before it takes deleting
    all of that indentation too, as a change of its own. That is what a
    host string drops from its value: the line break after a Nix `''`, Lua
    `[[` or `indoc!`, the closing line, and the blank lines a YAML `|` block
    clips. Without it, a shebang, a Dockerfile parser directive or an XML
    declaration sits on line 2. Blank lines beside a gap are inside the
    document, so they stay. A blank line and the next line's indent are two
    changes, not one.
- A gap's `content` is its replacement, of **any length** — a placeholder,
  nothing, or the default. Omitted keeps the coordinate-preserving
  whitespace.
- `null` keeps every segment.

kakehashi validates the whole answer; a wrong length or order, or a content
change of any other shape, refuses it. That includes a blank line between
lines that stay, and blank lines ending or opening a segment beside a gap
(the line break opening the second of two joined strings, say). So is an
answer whose prepared text would join a lone CR and a LF the virtual
document keeps apart into one line break (a dedent down to the line break
after a lone CR, or a placeholder completing a CRLF): P would have a line
fewer than V there. A change's position follows LSP: a `character` past
its line's length defaults back to that length (so none falls between a
CRLF's CR and LF), while a `line` past the segment's last refuses the
answer.

### Coordinates

Downstream servers receive the **prepared** document (P). kakehashi derives
a map between P and the virtual document (V), and composes it with V's
existing host translation, so every position, range, diagnostic, folding
range and `window/showDocument` selection a server sends or receives is
translated through P ↔ V ↔ host. Deleted edge blank lines stay in V: P's
start maps after the leading ones and P's end before the trailing ones. A
request at a host position P has no place for — strictly inside a gap, or
on a deleted edge line — is not made at all, since the server would answer
for another place.

No edit is mapped back through a prepared document. The methods closed to
a non-contiguous combined document — codeAction, colorPresentation,
completion, formatting, inlayHint, linkedEditingRange, onTypeFormatting,
prepareRename, rangeFormatting, rename — are closed to a prepared one too,
as is an inbound `workspace/applyEdit` computed for a prepared text, but
for inlay hints: they stay available on a contiguous prepared document
(they are display), with each hint's `textEdits` dropped.
A `*/resolve` envelope minted from a prepared document never resolves,
nor does a call or type hierarchy item minted from one answer its
follow-ups (`callHierarchy/incomingCalls`, `outgoingCalls`,
`typeHierarchy/supertypes`, `subtypes`): the map it was minted through is
not carried.

### Failure

A document the chosen peer could not prepare is **not sent** — never sent
unprepared. Every failure of that peer counts: an error response, a
malformed or refused answer, a timeout. (A pair with no peer at all — no
candidate advertising the request, or able to start — is a different case:
there is nothing to prepare the document, and it is sent as is.) Requests on
such a document get no answer from the virt layer, its pushed diagnostics
are dropped, and the one-shot CLI (`format`, `diagnose`) counts it as a
failed request, exiting 2. A peer that prefers a fallback answers `null`
(or catches its own errors and does so); kakehashi does not choose one on
its behalf.

An unusable answer (an error response, a malformed or refused result) is
final for that input under the current epoch. When it is the input the
lifecycle pass holds the document on, and the epoch has not moved, it
re-syncs its host as it arrives, and the host's diagnostics are collected
again (through the edit's debounce when the answer is an edit's, as for a
usable one). A missing answer from the chosen peer (crashing, timing out, or
answering with one of LSP's retryable cancellation codes) is retried with
backoff — one second, doubling to a minute — without waiting for an edit,
and requests do not pile further attempts onto a backing-off one.

### Lifecycle

The prepare request is made for each input (the virtual text and its
segments, under the pair's candidates) until it is answered, and its answer
is shared by every downstream server and every translation path. `didChange`
to downstream servers stays a full-text sync of the prepared document, so
open and change need no separate hooks. While an input's answer is
pending, the document is held: not opened or changed downstream, nor
closed — unless a candidate is known to prepare the pair and a server still
holds text sent as is (before the pair gained its peer) or nothing on
record, which is closed, since a prepared pair never bridges unprepared
text. While the candidates' handshakes have yet to tell whether any of them
prepares the pair, its open documents stay as they are.

When the answer the lifecycle pass holds a document on lands, its host is
synced again: the prepared text is sent, the host's diagnostics are
collected again (through the edit's debounce when the answer is an edit's
under an unmoved epoch, at once otherwise), an editor pull answered
without the document while it waited is owed a
`workspace/diagnostic/refresh`, and a save the held document missed is
forwarded, if the host is still at the saved version. A pass whose sends
could not all be queued, or whose regions could not be looked at (a reload
in progress), is retried with backoff, a bounded number of times. A
request opening a document lazily opens the newer of its own text and the
latest the lifecycle pass recorded.

## Invariants

> The invariants below are normative; the mechanisms that satisfy them are
> deliberately unspecified.

- **Every downstream-facing text is the prepared text.** didOpen, didChange,
  didSave text, the content a lazy open substitutes, and the fingerprints
  that decide whether a change is sent all describe P. Mixing V into any of
  them hands a server text whose coordinates no map describes.
- **What a connection holds decides how its coordinates translate.** The
  record of the text each connection was last sent, kept by the paths that
  send it and by nothing else, carries the map that text was sent with. It
  is made before the send is queued, so a push answering it already finds
  it, and taken back if the send is not queued. Coordinates a connection
  computed — a response, a push, an inbound edit or selection — translate
  through that map, never one chosen by URI or by region: right after a
  send, connections of one region can hold different texts, and two inputs
  can share their virtual text (they differ only inside gaps, which it
  masks) or even their prepared text while mapping it back differently. A
  held prepared text built from another virtual text than the region's
  current one describes no current host position, and a region the
  lifecycle pass prepares is bridged only through a prepared text: one held
  as is, or none on record yet, translates nothing. A connection ever sent
  a prepared text of a document stays suspect for it after its record goes,
  until the host closes. One rule decides all of this, for every path.
- **A request reads only the text its coordinates describe.** Right after an
  edit, a request is prepared for the new text while an open document still
  holds the previous one until the lifecycle pass sends it; reading the old
  answer through the new map misplaces it. A request on a prepared document
  waits for its server to hold exactly its text (or fails); one on an
  unprepared document waits only while its server still holds a prepared
  text, right after its pair lost its peer. The check and the request's
  enqueue cannot be separated by a recorded send. The request must not send
  a text to a document its server already has open — it could overtake a
  newer text the lifecycle pass already sent; it only opens one not open
  there (see Lifecycle).
- **A push follows the text its connection held when kakehashi read it.**
  The push is stamped as it is read, before it waits to be published.
  Where preparation is involved (by the rule above), it is placed only while
  the connection still holds that very text, and one naming another
  document version than the one its connection was last sent was computed
  for an earlier text and places nowhere. Elsewhere — and for a push read
  while its connection had never held a prepared text of the document — it
  places as on any unprepared path.
- **Cached answers follow the configuration.** An answer is the peer's for
  one launch configuration and its runtime settings, read only under the
  epoch it was asked in. The epoch moves once the new settings are in, so
  no answer given for a lookup resolved from the old ones is read after the
  change.
- **The answer that unblocks a held document always re-syncs its host**,
  whoever started the attempt that got it; requests may read older answers
  but never move what the lifecycle pass holds the document on.
- **The peer is not awaited under a document's edit lock.** A slow or
  starting peer would otherwise stall the host's edit processing.
- **Without experimental features nothing of this runs.** No record is
  stamped, no region text is copied for placement, and no request waits.

## Considered Options

### Query directives (#1082, #1083)

Rejected as described in Context: one directive per policy, each fixed per
query, and the names suggest an effect on every injection consumer. Queries
still decide *what* is injected and combined; the peer decides *how* it is
presented.

### Answer with the whole prepared text

kakehashi would have to infer the P ↔ V correspondence by diffing, which is
ambiguous wherever the peer's text repeats the original. Segment-shaped
answers state the correspondence instead.

### Deleting any blank line, or inserting text into content

Deleting a blank line between lines that stay would leave P a line break
the host has with no P position of its own between them; no host string
drops interior blank lines anyway, and in some embedded languages (HTTP,
Markdown) they carry meaning. Inserting text would add P text no V
position stands for.

### Gap replacements of the original width

Preserving each gap's line count and width would let the existing
per-line column offsets translate P directly, but it pads placeholders with
spaces the downstream server then sees (trailing whitespace after a token,
columns shifted in what it reports), and it cannot express removing a Nix
escape or collapsing a fence.

### The downstream server prepares its own documents

A server that wants dedented input can dedent internally — but then it also
has to translate its own coordinates back, and off-the-shelf servers cannot.
The protocol exists so the peer that presents is not the server that
analyzes.

### Fall through to the next advertising candidate

Asking the next candidate when the chosen one fails would let a flaky
peer switch which preparation servers see from one version to the next.
The first advertising candidate is the peer, as `preferred` picks one
answer.

### Send unprepared text when the peer fails

A server would alternate between two shapes of the same document, and
nothing on the wire tells it which one it holds. Fail-closed keeps every
document's coordinates describable; a peer that wants the fallback can
answer `null`.

### Forgetting or revoking answers when something changes

Each change that can alter an answer would need its own hook to find and
drop the answers it affects, and an attempt in flight across the change to
be revoked. One epoch, read with every answer, makes every such change one
move: an answer from before it is simply not read.

## Consequences

### Positive

- Dedent, placeholders and other presentation policies live in user code
  (tsudoi handlers) instead of kakehashi and its query vocabulary.
- Read-only features — hover, definitions, diagnostics, folding, inlay
  hints — work on embedded code its servers could not parse as written.

### Negative

- Every new document version waits for a peer round-trip before downstream
  servers see it.
- With experimental features on, a language's first document waits for its
  servers' handshakes to learn whether any prepares it, even when none
  does.
- Edit-producing methods (completion, code actions, rename, formatting and
  the others listed in Coordinates) and inbound `workspace/applyEdit` are
  not available on prepared documents; inlay hints lose their edits.
- A peer must be bridged for the languages it prepares, so it receives
  their documents too; keeping it out of other methods takes `priorities`.
- A peer that cannot start leaves its documents unprepared rather than
  unsent, so servers may see unprepared text until it is up.
- A peer bridged for every language (`"*"`) prepares every injection it
  can under the default `priorities`, and declining with `null` still
  makes a document prepared; restricting it takes `priorities = []` where
  it should not apply.
- `*/resolve` for items from prepared documents is refused (stale), as are
  call and type hierarchy follow-ups for their items.
- Host text the injection query includes in content cannot be protected as
  a gap; only text outside the captured content is.
- A request issued right after an edit waits for the new prepared text to
  reach its server, and fails if a newer edit supersedes it first. A request
  on an unprepared document waits the same way while its server still holds
  a prepared text (right after its pair lost its peer).
- A save made while a document's answer is pending reaches no server that
  has not started opening the document by the time the answer arrives.
- A push without a document version, computed for one prepared text but
  read after the next was recorded under another map, is placed through
  the newer map: without a version nothing tells the two apart.

### Neutral

- The peer sees only the segments, not the whole host document; a peer that
  needs more context can be configured as a host-document server as well.
