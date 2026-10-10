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

kakehashi asks a peer, `kakehashi/virtualDocument/prepare`, how to present
each virtual document before any downstream server sees it, and keeps all
coordinate bookkeeping itself. The peer is any language server that
advertises the request; tsudoi-language-server is the intended one, since
its handlers are user code.

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
  first; one that cannot start, or does not advertise the request, is
  passed over. Only that candidate is asked, and its answer stands (see
  Failure). A configured `concatenated` is ignored with a warning.
- When no candidate advertises the request, the document is sent as is.
  Each server's advertisement is remembered from its last handshake, so a
  pair none of whose candidates advertises it is not held back per edit to
  find that out again.

The feature is dormant unless `KAKEHASHI_EXPERIMENTAL=true`. Only pairs
that are bridged and have a server for the injection language are
prepared.

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
  only **delete whitespace**, each change in one of two shapes. Omitted
  means unchanged.
  - Leading whitespace of a line: the dedent case.
  - Whole blank lines, line start to line start, at the **document's
    edges**: the lines opening the first segment, or those ending the last
    (before a last line holding only indentation). That is what a host
    string drops from its value: the line break after a Nix `''`, Lua `[[`
    or `indoc!`, and the blank lines a YAML `|` block clips. Without it, a
    shebang, a Dockerfile parser directive or an XML declaration sits on
    line 2. Blank lines beside a gap are inside the document, so they stay.
    A blank line and the next line's indent are two changes, not one.
- A gap's `content` is its replacement, of **any length** — a placeholder,
  nothing, or the default. Omitted keeps the coordinate-preserving
  whitespace.
- `null` keeps every segment.

kakehashi validates the whole answer; a wrong length or order, or a content
change of any other shape, refuses it. That includes a blank line between
lines that stay, and blank lines ending or opening a segment beside a gap
(the line break opening the second of two joined strings, say).

### Coordinates

Downstream servers receive the **prepared** document (P). kakehashi derives
a map between P and the virtual document (V), and composes it with V's
existing host translation, so every position, range, diagnostic and folding
range a server sends or receives is translated through P ↔ V ↔ host.

Edits are mapped as edits, not as two endpoints. A formatting result is
replayed against P and diffed, so a whole-document replacement maps as
precisely as small edits. A workspace edit's edits to one document (rename,
code action) are replayed together too — mapped one by one, adjacent edits
could both claim a dedented line's removed indent — but each is first
checked, as sent, against the gaps. Any other edit (completion, inlay hint,
color presentation) is mapped whole, keeping its extent. In every case a
line the edit creates inside dedented content regains the content's removed
indentation, and an edit replacing a whole dedented line covers (and
restores) that line's own indent. Deleted edge blank lines stay in V: P's
start maps after the leading ones, and P's end, a range ending where the
trailing ones were or an insertion there before them, so a change at
either edge lands on the content (a closing indent the peer kept keeps its
own line). A change is **refused** when:

- it touches a gap (for formatting, when the diff does: a formatter's
  whole-document replacement rewrites every gap's replacement unchanged) —
  the host text a gap stands for is never edited through a downstream
  server;
- it would join text onto a gap that starts a line (a closing fence
  between combined blocks), by the same rule as at the region's own end;
- it creates lines in content whose removed indentation is not one uniform
  string.

Formatting fails the request; other edit carriers drop the affected edit,
item or edit set under their existing all-or-nothing rules.

Because the map is what keeps edits off gaps, a non-contiguous combined
document becomes available to edit-producing methods once it is prepared,
and every prepared document carries a map, an identity one when the answer
changed nothing. Two exceptions stay contiguous-only: linkedEditingRange and
prepareRename answer with bare ranges the client edits verbatim, which no
map can keep off a gap. An inbound `workspace/applyEdit` on a prepared
non-contiguous document is likewise still refused: kakehashi cannot verify
that the server sending it holds the text the map describes.

### Failure

A document the chosen peer could not prepare is **not sent** — never sent
unprepared. Every failure of that peer counts: an error response, a
malformed or refused answer, a timeout. (A pair with no peer at all — no
candidate advertising the request, or able to start — is a different case:
there is nothing to prepare the document, and it is sent as is.) Requests on such a document get no
answer from the virt layer, its pushed diagnostics are dropped, and the
one-shot CLI (`format`, `diagnose`) counts it as a failed request. A peer
that prefers a fallback answers `null` (or catches its own errors and does
so); kakehashi does not choose one on its behalf.

An unusable answer (an error response, a malformed or refused result) is
final for that document version. A missing answer (the peer not starting in time, crashing, timing
out, or answering with one of LSP's retryable cancellation codes) is retried
with backoff — one second, doubling to a minute — without waiting for an
edit, and requests do not pile further attempts onto a backing-off one.

### Lifecycle

The prepare request is made once per document version and its answer is
shared by every downstream server and every translation path. `didChange`
to downstream servers stays a full-text sync of the prepared document, so
open and change need no separate hooks. While a version's answer is
pending, the document is held: not opened or changed downstream, nor
closed — unless it still holds the unprepared text sent before its pair
gained a peer, which is closed, since a prepared pair never bridges
unprepared text. A save a held document misses is forwarded once its
prepared text is sent, if the host is still at the saved version, to the
servers that have the document open or opening by then. A server that
opens it later opens the saved text and gets no `didSave`, as with any
server that starts after a save.

## Invariants

> The invariants below are normative; the mechanisms that satisfy them are
> deliberately unspecified.

- **Every downstream-facing text is the prepared text.** didOpen, didChange,
  didSave text, the content a lazy open substitutes, and the fingerprints
  that decide whether a change is sent all describe P. Mixing V into any of
  them hands a server text whose coordinates no map describes.
- **A map is the one servers were sent, for the exact text it was built
  for**, never chosen by URI alone. An answer that is in but not sent yet
  describes nothing servers hold, and two inputs can share their virtual
  text (they differ only inside gaps, which it masks) or even their
  prepared text while mapping it back differently. Paths
  that translate a stored region (pushed diagnostics, resolve gates,
  inbound edits) can see newer text than the server holds; pairing
  that text with an older map misplaces every coordinate silently.
- **A request reads only the text the server holds.** Right after an edit,
  a request is prepared for the new text while an open document still
  holds the previous one until the lifecycle pass sends it; reading the old
  answer through the new map misplaces it. The same holds right after a
  settings change adds or drops a pair's peer, between prepared and
  unprepared text, so once anything is prepared every bridged request
  checks. The request must wait for that send (or fail), and must not send
  the text itself — it could overtake a newer text the lifecycle pass
  already sent.
- **A push is kept only from a server holding the text last sent.** The
  sent text is recorded before its didChange reaches every connection (and
  a send can fail); a push from a connection still holding another text —
  prepared differently, or not at all — is in coordinates the recorded
  answer does not describe. A push computed for the previous text but
  arriving after the new one was sent cannot be told apart, as for any
  unprepared document.
- **Cached state follows the configuration.** A settings change that drops
  or retargets a pair's peer must drop what was prepared for it; otherwise
  paths that look a map up by text keep translating unprepared text through
  it. Diagnostics a server pushed go once it is sent a different text than
  the one they describe — prepared differently, or no longer prepared —
  even when the virtual text itself is unchanged.
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
per method. Restricting content changes to deleting whitespace — leading
whitespace, and blank lines at the edges — makes every edit mappable
without asking the peer again.

### Deleting any blank line, or inserting text into content

Deleting a blank line between lines that stay leaves a join in P where a
formatter may add a blank line back, which the host would then hold twice;
no host string drops interior blank lines anyway, and in some embedded
languages (HTTP, Markdown) they carry meaning. Inserting text (a module
docstring, a shebang) adds P-only text that a formatter's diff can
attribute either way, and an edit beside it may belong to it or to the
content. Silencing diagnostics that way belongs to filtering them (a peer
pulling diagnostics through `kakehashi/bridge/peer/request`), and removing
non-whitespace prefixes (rustdoc's `# `, doctest's `>>> `) to the parser
and queries, as a blockquote's `> ` already is.

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

### A `prepare` field naming the peer

The first version configured the peer as
`languages.<host>.bridge.<injection>.prepare = "<server>"`, a field of its
own, and held a document unsent while the named server could not start.
Every other per-language server choice is an aggregation `priorities`
list; a second mechanism for one method had its own inheritance, opt-out
(`prepare = ""`) and failure rules to learn. Choosing among the
language's servers by advertisement also means a configuration that never
mentions preparation still gets it from a server that offers it.

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

## Consequences

### Positive

- Dedent, placeholders and other presentation policies live in user code
  (tsudoi handlers) instead of kakehashi and its query vocabulary.
- Combined documents gain formatting and other edit-producing methods
  without risking the host text between captures.

### Negative

- Every new document version waits for a peer round-trip before downstream
  servers see it; with experimental features on, a language's first
  document also waits for its servers' handshakes to learn whether any
  prepares it.
- A peer must be bridged for the languages it prepares, so it receives
  their documents too; keeping it out of other methods takes `priorities`.
- A peer that cannot start leaves its documents unprepared rather than
  unsent, so servers may see unprepared text until it is up.
- A peer bridged for every language (`"*"`) prepares every injection it
  can under the default `priorities`, and declining with `null` still
  makes a document prepared; restricting it takes `priorities = []` where
  it should not apply.
- `*/resolve` for items from prepared documents is refused (stale).
- A blockquoted (line-prefixed) region refuses any result of the
  concatenated formatting pipeline when prepared, since prefixes are not
  re-applied on that path.
- Host text the injection query includes in content cannot be protected as
  a gap; only text outside the captured content is.
- A request issued right after an edit waits for the new prepared text to
  reach its server, and fails if a newer edit supersedes it first. Once any
  document has been prepared, requests on unprepared documents wait the
  same way.
- linkedEditingRange, prepareRename and inbound `workspace/applyEdit` stay
  unavailable on prepared non-contiguous documents.
- A save made while a document's first answer is pending reaches no server
  that has not started opening the document by the time the answer
  arrives; a server whose open is still being scheduled then opens the
  saved text without a `didSave`.

### Neutral

- The peer sees only the segments, not the whole host document; a peer that
  needs more context can be configured as a host-document server as well.
