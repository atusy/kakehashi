# Virtual Document Prepare Protocol

**Related Decisions**:
- [language-server-bridge-virtual-document-model](language-server-bridge-virtual-document-model.md) — the virtual documents this protocol presents, including `injection.combined` and its coordinate-preserving masks
- [bridge-routing-protocol](bridge-routing-protocol.md) — the sibling kakehashi→downstream request and its `capabilities.experimental` discovery convention
- [concatenated-formatting-pipeline](concatenated-formatting-pipeline.md) — the formatting pipeline whose whole-region result is mapped back the same way
- [ls-bridge-message-ordering](ls-bridge-message-ordering.md) — the per-connection ordering the held-document rule keeps intact

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

kakehashi asks a configured peer, `kakehashi/virtualDocument/prepare`, how
to present each virtual document before any downstream server sees it, and
keeps all coordinate bookkeeping itself. The peer is any language server
that advertises the request; tsudoi-language-server is the intended one,
since its handlers are user code.

### Configuration and Discovery

`languages.<host>.bridge.<injection>.prepare` names a `languageServers`
entry; the field inherits through the `_` wildcards like the other bridge
fields. The named server must advertise
`capabilities.experimental.kakehashi.virtualDocumentPrepare: true` in its
`initialize` result. It needs no `languages` of its own, and it receives
only prepare requests unless its `languages` also select it as a downstream
server. The feature is dormant unless `KAKEHASHI_EXPERIMENTAL=true`.

Only pairs that are bridged and have a downstream server for the injection
language are prepared — a document nobody receives is not worth a peer
round-trip per edit. An empty name (`prepare = ""`) opts one language out of
a wildcard's peer.

### The Request

```ts
interface VirtualDocumentPrepareParams {
  // The virtual document as kakehashi built it; `version` increases
  // whenever its text, segments or peer change, and is never reused.
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
  only **delete leading whitespace** of a line (the dedent case). Omitted
  means unchanged.
- A gap's `content` is its replacement, of **any length** — a placeholder,
  nothing, or the default. Omitted keeps the coordinate-preserving
  whitespace.
- `null` keeps every segment.

kakehashi validates the whole answer; a wrong length or order, or a content
change other than a leading-whitespace deletion, refuses it.

### Coordinates

Downstream servers receive the **prepared** document (P). kakehashi derives
a map between P and the virtual document (V), and composes it with V's
existing host translation, so every position, range, diagnostic and folding
range a server sends or receives is translated through P ↔ V ↔ host.

Edits are mapped as edits, not as two endpoints. A formatting result is
replayed against P and diffed, so a whole-document replacement maps as
precisely as small edits; any other edit (completion, rename, code action,
inlay hint, color presentation) is mapped whole, keeping its extent. Either
way a line the edit creates inside dedented content regains the content's
removed indentation, and an edit replacing a whole dedented line covers (and
restores) that line's own indent. A change is **refused** when it touches a
gap — the host text a gap stands for is never edited through a downstream
server — or when it creates lines in content whose removed indentation is
not one uniform string. Formatting fails the request; other edit carriers
drop the affected edit, item or edit set under their existing
all-or-nothing rules.

Because the map is what keeps edits off gaps, a non-contiguous combined
document becomes available to edit-producing methods once it is prepared,
and every prepared document carries a map, an identity one when the answer
changed nothing. Two exceptions stay contiguous-only: linkedEditingRange and
prepareRename answer with bare ranges the client edits verbatim, which no
map can keep off a gap. An inbound `workspace/applyEdit` on a prepared
non-contiguous document is likewise still refused: kakehashi cannot verify
that the server sending it holds the text the map describes.

### Failure

A document the peer could not prepare is **not sent** — never sent
unprepared. Every failure counts: no advertisement, an error response, a
malformed or refused answer, a timeout. Requests on such a document get no
answer from the virt layer, its pushed diagnostics are dropped, and the
one-shot CLI (`format`, `diagnose`) counts it as a failed request. A peer
that prefers a fallback answers `null` (or catches its own errors and does
so); kakehashi does not choose one on its behalf.

An unusable answer (an error response, a malformed or refused result) or a
peer that does not advertise the request is final for that document
version. A missing answer (the peer not starting in time, crashing, timing
out, or answering with one of LSP's retryable cancellation codes) is retried
with backoff — one second, doubling to a minute — without waiting for an
edit, and requests do not pile further attempts onto a backing-off one.

### Lifecycle

The prepare request is made once per document version and its answer is
shared by every downstream server and every translation path. `didChange`
to downstream servers stays a full-text sync of the prepared document, so
open and change need no separate hooks.

## Invariants

> The invariants below are normative; the mechanisms that satisfy them are
> deliberately unspecified.

- **Every downstream-facing text is the prepared text.** didOpen, didChange,
  didSave text, the content a lazy open substitutes, and the fingerprints
  that decide whether a change is sent all describe P. Mixing V into any of
  them hands a server text whose coordinates no map describes.
- **A map is chosen by the exact text it was built for**, never by URI
  alone. Paths that translate a stored region (pushed diagnostics, resolve
  gates, inbound edits) can see newer text than the server holds; pairing
  that text with an older map misplaces every coordinate silently.
- **A request reads only the prepared text the server holds.** Right after
  an edit, a request is prepared for the new text while an open document
  still holds the previous one until the lifecycle pass sends it; reading
  the old answer through the new map misplaces it. The request must wait
  for that send (or fail), and must not send the text itself — it could
  overtake a newer text the lifecycle pass already sent.
- **Cached state follows the configuration.** A settings change that drops
  or retargets a pair's peer must drop what was prepared for it; otherwise
  paths that look a map up by text keep translating unprepared text through
  it.
- **The peer is not awaited under a document's edit lock.** A slow or
  starting peer would otherwise stall the host's edit processing; a document
  whose answer is pending is held — neither opened nor changed, and not
  closed either — and the host is synced again when the answer arrives.
- **A resolve envelope cannot carry a map.** An item produced from a prepared
  document is treated as stale at resolve time rather than translated with
  the envelope's map-less offset.

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

### Per-method post hooks for edit-producing requests

Every position-bearing response — hover ranges, diagnostics, locations,
symbols — needs translating, not only edits, so post hooks would multiply
per method. Restricting content changes to leading-whitespace deletion makes
every edit mappable without asking the peer again.

### Gap replacements of the original width

Preserving each gap's line count and width would let the existing
per-line column offsets translate P directly, but it makes placeholders pad
with spaces that formatters then rewrite (touching the gap), and it cannot
express removing a Nix escape or collapsing a fence.

### The downstream server prepares its own documents

A server that wants dedented input can dedent internally — but then it also
has to translate its own coordinates back, and off-the-shelf servers cannot.
The protocol exists so the peer that presents is not the server that
analyzes.

### Send unprepared text when the peer fails

A server would alternate between two shapes of the same document, and
nothing on the wire tells it which one it holds. Fail-closed keeps every
document's coordinates describable; a peer that wants the fallback can
answer `null`.

## Consequences

### Positive

- Dedent, placeholders and other presentation policies live in user code
  (tsudoi handlers) instead of kakehashi and its query vocabulary.
- Combined documents gain formatting and other edit-producing methods
  without risking the host text between captures.

### Negative

- Every new document version waits for a peer round-trip before downstream
  servers see it.
- `*/resolve` for items from prepared documents is refused (stale).
- A blockquoted (line-prefixed) region refuses any result of the
  concatenated formatting pipeline when prepared, since prefixes are not
  re-applied on that path.
- Host text the injection query includes in content cannot be protected as
  a gap; only text outside the captured content is.
- A request issued right after an edit waits for the new prepared text to
  reach its server, and fails if a newer edit supersedes it first.
- linkedEditingRange, prepareRename and inbound `workspace/applyEdit` stay
  unavailable on prepared non-contiguous documents.

### Neutral

- The peer sees only the segments, not the whole host document; a peer that
  needs more context can be configured as a host-document server as well.
