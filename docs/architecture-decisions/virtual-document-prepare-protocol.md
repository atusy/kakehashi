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

Edits a downstream server makes to a prepared document are mapped back
through the same map (see Edits); methods whose edits are not mapped that
way are not available on one (see Coordinates).

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

The epoch is one of two validity axes. It decides which answers a lookup
may read, but it cannot reach a map already given out, which a connection
holds or a request is built on. The second axis is **revocation**:
- A failure of an input revokes every map answered for that same input of
  that same region before the failed attempt's input was taken. "The same
  input" means the same text, segments and host language, whatever the
  epoch or candidates.
- A revoked map translates nothing more, for any path that reads it.
- A map whose input was taken after the failure is not revoked, whether it
  is for a newer text or for the same text asked again.

An answer is read only while both hold: its epoch is current, and its map
is not revoked.

The feature is present only with `KAKEHASHI_EXPERIMENTAL=true`. Without
it, a server's `initialize` result is still parsed, and its advertisement
is noted on its connection, but it is never recorded for its pairs, so
nothing is looked up or asked. Only pairs that are
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
request is not made at all at a host position with no corresponding
position in P, such as one strictly inside a gap or on a deleted edge line,
since the server would answer it for another place. A position inside
indentation the peer removed names the start of its line in P (completion
alone refuses it, see Edits): on-type formatting typed there answers for
that line.

The methods closed to a non-contiguous combined document — codeAction,
colorPresentation, completion, formatting, inlayHint, linkedEditingRange,
onTypeFormatting, prepareRename, rangeFormatting, rename — are closed to a
prepared one too, as is an inbound `workspace/applyEdit` computed for a
prepared text, but for completion, inlay hints, color presentations,
formatting, range formatting and on-type formatting: they stay available on
a contiguous prepared document, their edits mapped as Edits describes. A
non-contiguous prepared document (one with gaps) stays closed to all of
them: on a map with gaps the indentation lines regain is not exact in every
shape.
A `*/resolve` envelope minted from a prepared document never resolves,
nor does a call or type hierarchy item minted from one answer its
follow-ups (`callHierarchy/incomingCalls`, `outgoingCalls`,
`typeHierarchy/supertypes`, `subtypes`): the map it was minted through is
not carried.

### Edits

An edit a server makes to P maps onto V keeping its extent: its range maps
as positions do, except that a change starting at a line start that
replaces, clears or deletes whole lines, or opens with a line break, also
covers the indentation the peer removed from that line, and a change
ending at a line start keeps that line's own removed indentation (the
change takes it along and gives it back). Every line the edit's text
begins regains the indentation the peer removed from the segment it lands
in — but text joined at the start of a whitespace line the peer kept,
including a line the edit begins there, gains none (it is text typed on
that line, its whitespace after it; a formatting answer writing there is
refused instead, see below) — provided every indentation the peer removed from the segment's content
lines is the same (lines it left alone do not count, nor do
whitespace-only lines, but in a segment holding nothing else); otherwise
an edit creating a line of text that would regain it there is refused,
since no single indentation is right (a blank line needs none, and the
existing line an edit ends before keeps its own). A segment that says
nothing of its indentation (no indentation removed, and no block of text
opening a line of its own: the rest of a line after an interpolation,
blank lines) regains the segment's before it.
The document's deleted indentation-only last line (the indentation before
a closing `''`) counts only in a segment of whitespace alone, with no
other indentation removed and no closing lines deleted with it: a created
line never takes host layout's indentation over the content's (nixfmt's
shallower closing indent), nor any beside content left at column 0. Lines
inserted at that last line's start are content lines: they go before its
indentation, which the text after their last line break, if any, joins.

A set of edits the client applies together (a hint's `textEdits`, a
completion item's or color presentation's main edit with its
`additionalTextEdits`) is mapped one by one and checked as a whole on the
edited V. It is refused, and dropped like any unsafe set, when:

- an edit is reversed, or reaches a line past P's last; edits overlap in
  P or once mapped; or edits touch
  (one ending where another starts, or two inserting at one place) at a
  line start, or with a line break or no text on the side where they meet:
  how each regains a removed indent, or where a CR and an LF meet, depends
  on the other's text — but for lines inserted at a line start before an
  edit starting there with text (an import above the line a completion
  rewrites), which keep to their own lines, provided the host has a line
  start there too and both map to the same place;
- an edge of a mapped edit falls between a CR and an LF of V, where no LSP
  position is;
- an edit reaches into a gap's text (or, for a gap the peer emptied,
  spans its position);
- host text starting a line in V — a gap such as the fences between two
  combined blocks, or the document's deleted closing lines — would no
  longer start one;
- a line break P hides — a deleted edge line's, or a replaced gap's — would
  fuse with one beside it into a single CRLF;
- a CR and an LF would meet at an edit's edge in one document but not in
  the other (a CR inserted before an LF P shows right after a gap it
  emptied: one line break in P, two in V).

No edit in P reaches into the deleted edge lines or the closing indent
deleted with them: P has no position inside them.

A formatter's answer (formatting, range and on-type formatting, and the
concatenated pipeline's result) is read as what it changes, not edit for
edit: its edits are applied to P (an end at the start of the line just
past P's last naming P's end, as whole-document answers do), the region's
edge layout of P restored as for any document (but for an on-type answer,
which keeps the edges as it writes them, typing being an edit, not a
reformatting of the region: one dropping P's final line break is refused
where the peer deleted closing lines after it, and dropped whole
otherwise, as any answer still removing the line break that ends a
region is: what follows the region, a closing fence say, would join its
last line), and the result diffed against P. An answer
changing a region whose peer dedented it unevenly — a line of text (not
whitespace alone) the peer left alone, or
dedented otherwise than the rest of its segment — is refused before any
pairing: every line of a whole answer is rewritten text, and which old
line each is, and so which indentation it regains, only a pairing reads;
with one indentation for every line of the content, no reading decides an
indentation. (Edits that keep their extents state where they go, and
still map there.) Lines are paired first: a line of text with the line
it was rewritten from (alike to the byte, then alike but for the whitespace
around its text, a line found once in each text anchoring the rest; a line
alike but for whitespace to several, as many on each side, is not paired
to the byte, since reindented one copy may read as another did, but in
order with the lines alike to several below), then one alike but for
whitespace anywhere in it, the same way; then, between those, lines found as many
times on each side (a `}` among several), by the fewest changes; then a
line other lines were joined onto (found by looking up the old lines'
texts, not comparing every pair); then blank lines with blank lines — as
many as can pair, and of the alignments pairing as many, the one keeping
most whitespace lines with ones holding the same whitespace, so that text
the formatter writes beside blank lines is a line of its own whenever a
blank line of the answer could be the line it would otherwise be written
on, not text written on one (which, on a whitespace line the peer kept,
would regain no indentation and lose the formatter's relative
indentation); then lines of text in order from either end of what is left
while their letters and digits are alike (a line whose quotes or
punctuation a formatter rewrote is the line it was). Each pair, and
each run of lines left between pairs, is then diffed by character. A run
holding lines of text on P's side is diffed without its blank lines, which
go whole: its new text is that text rewritten, never text written on a
blank line the answer removed (which would take that line's indentation,
not the content's). A run of blank lines takes the answer's lines line for
line where it holds as many there (not ending with a line break P lacks);
with more or fewer, the blank lines went and the other lines are the
content's. Text where an empty line was, or one the peer emptied, takes
the content's indentation. Text where a whitespace line the peer kept was
is refused, even typed right after that whitespace: in a whole-text
answer, "typed on that line" is only how the diff aligned the text, which
reads as well as a line written in its place, the kept line removed, and
the two readings give different host text. (Edits that keep their extents,
a completion's say, state where they go: text inserted after a kept line's
whitespace is typed on that line, gaining no indentation.) Every stretch
the result differs from P in maps as one edit does above; stretches
touching at a line start stay apart where the later replaces some of that
line's start with text, or where the earlier ends with a line break of P
and the later replaces no line break. Of two stretches touching there, the
first leaves the line's removed indentation to the second rather than take
it along and give it back (the same text), so they do not both claim it.
The set is then checked as a whole, as above. Which of several blank lines
alike a line was written on, or which went, no answer tells: the run's
diff decides.

The diff has a budget counted in work, not time: each step of its searches
(a comparison, a cell of a table, a diagonal followed) spends one, about a
million in all and 16 more for each byte of the texts (pairing every line
and diffing each on its own is linear work no fixed budget covers for a
large region), so an answer is accepted or refused alike on any machine
and under any load. The character diff pairs the characters other than
whitespace first and diffs only what lies between them, so an answer that
mostly moves whitespace (a pretty-printer's) costs work linear in the
texts. An answer whose diff runs out of work is refused, and logged apart
from the refusals its shape decides. The tables quadratic in what they
search (the line diff's trace, and pairing blank lines, 4 bytes a step)
are spent before they are taken, so their memory is linear in the budget,
which itself grows with the texts but stops at 2^24 steps: some 64 MiB at
most. The rest (the line diff's frontier, the lines' keys and indexes,
and the characters the character diff compares) is linear in the texts. Whole lines alike
at either end of P and the edited P are set aside first, uncharged, so the
work follows what the answer changes, not the region's size: a one-line
answer costs its line in any region, and a 20,000-line region of half a
megabyte rewritten whole maps in some 30 to 100 ms (release build, on a
loaded machine), so the
diff runs where the answer arrives. The budget decides only which answers
are refused, never what an accepted one writes; a large region heavily
rewritten may be refused.

An answer's edits are mapped only while they would land where the server
meant them. This is read where an answer is chosen, not as each reply
arrives: the servers' fan-in or the layer race can hold an answer while
the host moves, and a reply refused on arrival would count as no answer,
so a server ranked below it whose reply came once the host settled (with
the region's content unchanged, after an edit to another fence or a hold
clearing) would win against the configured order. A reply is mapped
through its request's map whatever happened since. Completion, and color
presentations under the `preferred` strategy, read it as the layer race
decides on the injected layer's answer: a stale one is no answer of that
layer, and the race falls through to the host layer's (waited for if
still running), a stale incomplete completion list still asking to
re-query; they read it once more as they return the answer, with no await
since the race decided. Under `concatenated`, color presentations read it
once, after every layer answered: a stale prepared part is dropped from
the merged answer, the host layer's part kept. Inlay hints read it only
once the answer is chosen: a stale answer's hints stay shown without
their edits, which keeps more than the host layer's answer would give.
The answer is checked against:

- the host was edited since the request's text was read (anywhere: an
  edit above the region moves the host coordinates its edits were
  translated to, though the region's text and map stay the same) —
  dropped, when the final answer is chosen;
- the region is no longer live (no longer tracked under the host
  lifetime the request's content version was read under, as it was when
  the answer was made: deleted, invalidated by an edit over it, its host
  closed or reopened, before or after the request's text was read) —
  dropped;
- the host's last edit has not been settled yet (its parse or lifecycle
  pass still running, so the recorded text may predate it), or the
  host's newer text waits for its prepare answer — dropped, but for the
  formatting methods and color presentations, which wait for that to
  clear (see below);
- the peer failed the request's input (see Failure; a reply arriving
  after that is refused whole, and one the failure lands under keeps no
  edit) — dropped;
- live, and nothing recorded for the region yet (opened, no pass has
  recorded its text) — kept;
- recorded with the request's very text in the very same coordinates —
  kept;
- anything else recorded (typed on, re-indented, or, with the host
  unedited, sent as is or answered anew after an epoch move, or the
  region rerouted to another language) — dropped.

Dropped, completion answers nothing (an empty incomplete list when it
asked to re-query), color presentations answer nothing, and inlay hints
stay shown without their edits. Formatting reads whether the host moved
under its answer only when the final answer is chosen, not as each reply
arrives: refused there, a reply would count as its server failing, and the
fan-in would fall through to a server ranked below it whose reply came
once the host settled. Nor do formatting and color presentations drop an answer for the host's
last edit not having settled yet, or a region held for its prepare answer:
formatting, range and on-type formatting each come once per action (an
on-type trigger, a save), and a color presentation once per picker
interaction, so an answer dropped for that would not be asked again. The final check waits, as a request waits for its text (up to the
prepare timeout), until the edit settles and the hold clears, then judges
the answer as above; the host edited meanwhile, it is dropped at once as
moved, and once nothing is left to settle or clear, an answer still not
current (its region re-prepared to other coordinates, no longer tracked,
its host reopened, its map revoked) is dropped at once as stale; one that
never settles is dropped when the wait ends. The warning says which.
Dropped (moved, stale or unsettled), the whole answer is dropped (the host's own formatter's edits and
those of regions still current too) and the request is counted failed, so
`kakehashi format` exits 2 and writes nothing (range and on-type
formatting, which no one-shot run makes, answer nothing). A reply its map
refuses counts as that server failing, as any failed reply does:
`kakehashi format` exits 2 when no other server formats the region.
On-type formatting is the exception: an answer refused, or dropped at the
final check, is no answer, logged at debug, so that trigger keystrokes do
not warn the user each time.

Unlike on an unprepared document, an answer overtaken by typing is
dropped, not kept stale. How an edit maps back depends on the whole text
— which lines are blank, what an inserted line joins, which indentation
a created line regains — so no text but the request's is known to place
it as the server meant, and a stale edit could land in indentation the
host has and P never showed. Completion answers dropped this way come back
as an empty incomplete list, which the client asks again for, and inlay
hints stay shown with only their edits stripped: the answer for the latest
text still arrives, the cost availability (an answer within a keystroke's
parse and prepare round-trip), never a misplaced edit. Color presentations
are asked once per picker interaction, and the formatting methods once per
action: they wait for the host to settle instead, as the final check
described above does.

A color presentation without a `textEdit` replaces the request range with
its label; on a prepared document that edit is made explicit and mapped
like any other. A request range reaching into a gap stands for host text no
presentation may replace (with the gap emptied, its prepared range is even
empty, and an edit there would only insert beside it), and one right after
an emptied gap reaches the server where the gap's start does, an edit there
mapping back before the gap: for either, no presentation is returned.
Additional edits refused with the main edit are dropped, the main edit kept
as it maps alone.

Completion keeps to what the client inserts where it asked:

- A caret inside indentation or blank lines the peer removed (or from
  where the deleted closing lines start) gets no completion: the server
  sees it elsewhere, and its items would edit away from it.
- An item's edit must contain the caret once mapped (a caret right after a
  gap the peer emptied reaches the server where the gap's start does); an
  insert/replace edit maps through both ranges, which must start at the
  same position as sent, map the shared text alike and keep their shared
  start.
- A snippet is kept only when it is literal text: what a client expands
  tabstops, placeholders, variables or escapes to is not the text checked.
  Kept, it is sent as plain text, which no snippet engine re-indents.
- An item without a `textEdit` is inserted at the caret, outside the map:
  it is dropped when its text breaks a line; where the same text as an
  edit at the caret regains an indent (a blank line among dedented ones,
  or one the peer emptied of other whitespace, which that edit replaces
  on the caret's line), it goes out as that edit, its additional edits
  checked with it; inserted at the caret as is, it drops them, having no
  main edit to be checked with.
- A mapped multi-line edit already regained the removed indentation: the
  item asks a client that supports it to insert as is, and is dropped for
  one that always adjusts indentation.
- An incomplete list without items (the server's own, or one whose every
  item did not map back) is no result: it would win over another server's
  items. Its ask to re-query still stands when the server ranks above the
  layer's winning server, since the next keystroke may move the caret to
  where its items map: the winning list is marked incomplete. With no
  items in the layer, the host layer's answer is marked incomplete when it
  came after the injected layer finished, and nothing at all answers an
  empty incomplete list.

### Failure

A document the chosen peer could not prepare is **not sent** — never sent
unprepared. Every failure of that peer counts: an error response, a
malformed or refused answer, a timeout. (A pair with no peer at all — no
candidate advertising the request, or able to start — is a different case:
there is nothing to prepare the document, and it is sent as is.) Requests on
such a document get no answer from the virt layer, its pushed diagnostics
are dropped, and the one-shot CLI (`format`, `diagnose`) counts it as a
failed request, exiting 2 — `format` counts a host span left unformatted:
a same-range alternative language that answers for the span (an empty
answer, already formatted, included) makes up for one whose peer could not
prepare it; one that takes the span but gets no answer (none of its servers
formats, or none answers) does not. A peer that prefers a fallback answers `null`
(or catches its own errors and does so); kakehashi does not choose one on
its behalf.

An unusable answer (an error response, a malformed or refused result) is
final for that input under the current epoch. When it is the input the
lifecycle pass holds the document on, and the epoch has not moved, it
re-syncs its host as it arrives, and the host's diagnostics are collected
again (through the edit's debounce when the answer is an edit's, as for a
usable one). Its servers keep the prepared text they last held, but no
coordinates are read through it any more — a request built on an earlier
answer for that same input (under an earlier epoch) is not admitted, nor
opens the document, as no push is placed. A request built on an answer for
another text (an edit since, or an undo back to it) is left to the
held-text check. A
missing answer from the chosen peer (crashing, timing out, or
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
  there (see Lifecycle). `completionItem/resolve` is the one exception: it
  syncs its document's text before resolving, under the host's edit lock
  so no newer text can be sent meanwhile, and only for an unprepared
  document: never over a prepared text or a document held for its peer.
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
- **Without experimental features none of the preparing runs.** No peer is
  asked, no record is stamped, no region text is copied for placement, and
  no request waits. What runs regardless is small: each send's text is
  recorded as what its connection holds, and the advertisement in a
  server's `initialize` result is parsed and noted on its connection. It is
  never recorded for its pairs (see Configuration and Discovery), so
  nothing reads it.

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

### Forgetting answers when something changes

Each change that can alter an answer would need its own hook to find and
drop the answers it affects, and an attempt in flight across the change to
be revoked. One epoch, read with every answer, makes every such change one
move: an answer from before it is simply not read.

The epoch cannot reach a map already given out, though. So a failure, the
one change that has to stop coordinates from being read through answers in
use, also revokes those maps (see Configuration and Discovery). Revocation
is kept per input and is triggered only by that input's failure. It is not
a hook per kind of change.

## Consequences

### Positive

- Dedent, placeholders and other presentation policies live in user code
  (tsudoi handlers) instead of kakehashi and its query vocabulary.
- Read-only features — hover, definitions, diagnostics, folding, inlay
  hints — work on embedded code its servers could not parse as written.
- So do completion and formatting, their edits placed back with the
  indentation the peer removed.

### Negative

- Every new document version waits for a peer round-trip before downstream
  servers see it.
- With experimental features on, a language's first document waits for its
  servers' handshakes to learn whether any prepares it, even when none
  does.
- Edit-producing methods other than completion, inlay hints, color
  presentations and the formatting methods (code actions, rename and the
  others listed in Coordinates) and inbound `workspace/applyEdit` are not
  available on prepared documents, nor is any on a prepared document with
  gaps; completion items there are snippets only as literal text, and are
  not resolved.
- A formatting answer whose diff runs out of its work budget is refused, so a
  large region heavily reformatted may be left as it is (counted a failure
  by `kakehashi format`). The common case costs work linear in the region:
  an answer that keeps most lines, mostly moves whitespace, respaces every
  line (`x=1` to `x = 1`), or rewrites the punctuation of every line
  (`f('a')` to `f("a");`, 20,000 lines). Pairing blank lines takes a table
  quadratic in the blank lines of one stretch no line of text anchors: an
  answer changing the letters of every line of text, with whitespace lines
  between them, is refused from about 1,100 such text lines (some 2,200
  lines in all); one that also collapses the blank lines between them (two
  to one), from about 770 text lines. Hashing or comparing a line's text
  costs a step for every 64 bytes of it (lines are numbered once, then
  compared as numbers), so the budget bounds the diff's time with long
  lines too, and the answer's positions and the result's convert to bytes
  and UTF-16 columns in one pass over each line they fall on, each line
  found in a few lookups, uncharged:
  1 MB of 1 KB lines, each rewritten at its end, maps in some 60 ms; 4 MB
  of them is refused in some 200 ms, and a 4 MB answer to a 500 KB region
  in some 140 ms (release build, measured on a loaded machine).
- The concatenated formatting pipeline does not re-apply host line prefixes
  on a prepared document: a prefixed (blockquoted) prepared region fails
  the request instead.
- Formatting, range and on-type formatting refuse a region whose peer
  dedented it unevenly (a line of text left alone, or dedented otherwise
  than the rest), whatever the answer changes; completions, hints and
  other edits that keep their extents still map there. Reading a whole
  answer back onto lines that kept indentations of their own met one
  ambiguity after another (copies crossing, a line moved off its own
  indentation, one copy reindented into another's text), several mapped
  to wrong text before they were found; a refusal is the safe side. The
  usual peers dedent evenly: a Nix `''` string strips the least
  indentation of its lines (a line opening with an interpolation counts,
  so one at column 0 strips none), `textwrap.dedent`, `indoc!` and `<<~`
  the common prefix, and a YAML block scalar requires its indentation. A
  CommonMark fence strips up to its opener's own indentation from each
  line, so a line less indented than an indented fence's opener is
  dedented unevenly, and that block is not formatted through its peer.
- Text a formatter writes where a whitespace-only line the peer kept was is
  refused, typed after its whitespace or not: a whole-text answer does not
  tell text typed on that line from a line written in its place. That is
  text in the kept line's place line for line, or, where the answer holds
  more or fewer lines there, text starting with exactly that line's
  whitespace; text indented otherwise is a line of the content. Where an
  answer reads as well as a line of its own beside the blank lines, that
  reading is taken. The exact-text property, which writes on kept
  whitespace lines on purpose, sees such refusals in about 1.4% of its
  answers changing evenly dedented documents.
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
- A save one connection refuses, because a newer answer reached it first,
  is owed for the region, not for that connection: the resync the newer
  answer brings replays it to every server holding the region. A server
  that already got the save before then gets a second didSave, after the
  didChange to the newer text, so each didSave still describes the text
  that server holds.
- A whole-document request (folding ranges, document links, colors, code
  lenses) waits for each region's attempt in flight. A region still without
  an answer after that is left out of the reply, and nothing refreshes the
  reply once the answer arrives. This happens when its peer gave no answer
  and is backing off, or when the epoch moved meanwhile. The editor's next
  request covers the region. Only diagnostics are refreshed
  (`workspace/diagnostic/refresh`): LSP offers no refresh for links or
  colors.
- A push without a document version, computed for one prepared text but
  read after the next was recorded under another map, is placed through
  the newer map: without a version nothing tells the two apart.

### Neutral

- The peer sees only the segments, not the whole host document; a peer that
  needs more context can be configured as a host-document server as well.
