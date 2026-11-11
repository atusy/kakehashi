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
through the same map (see Edits); on a prepared document with gaps, where
they are not, edit-producing methods are not available (see
Coordinates).

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

A peer does not see the virtual document, whose lines the rules below
name, but the segments spell it. Its text is each segment's in order: a
content segment's `content` as it is, and for a gap what stands for its
host text there —

- a space for each UTF-16 code unit of the text up to the gap's first line
  break, when the gap continues a line of content (the segment before it
  is content that does not end in a line break), and nothing for that
  text when the gap opens a line (it is the first segment, or follows
  content that ends in a line break);
- then each of its line breaks as the host has it (LF, CRLF or a lone CR),
  and nothing for the text after one. Host text before the first injected
  character of its line is stripped, not masked: a blockquote's `> `, and
  just as much the `}` closing an interpolation opened on a line above.

So content after a gap that holds a line break starts a line of the
virtual document, and content after a gap on one line goes on with the
line the gap is on. (One document is spelled apart: where its span ends
between a CR and its LF, that CR — the last character of a gap ending
the document — stands as a space, since no line ends there.)

A gap that opens a line and has content after it on that line is the one
case with nothing left to present: an interpolation with nothing injected
before it on its host line (`${pkgs.hello}/bin/hello` written at column
0) is stripped as a line prefix is, so it is no segment, takes no
placeholder, and its line reaches servers as `/bin/hello` — as it does
without a peer. kakehashi knows which text is injected, not what the text
before it is. Where the host injects the line's indentation too, as a Nix
string indented in its file does, the interpolation continues a line and
is a gap like any other.

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
that line. A range-formatting selection of only what the peer removed
(indentation, deleted edge blank lines) is empty in P: it is not sent,
as a selection lying only in a host prefix is not.

The methods closed to a non-contiguous combined document — codeAction,
colorPresentation, completion, formatting, inlayHint, linkedEditingRange,
onTypeFormatting, prepareRename, rangeFormatting, rename — stay available
on a contiguous prepared document, their edits mapped as Edits describes,
and so does an inbound `workspace/applyEdit` computed for a prepared text.
A non-contiguous prepared document (one with gaps) stays closed to all of
them, and an inbound edit to one is refused: on a map with gaps the
indentation lines regain is not exact in every shape.
A `*/resolve` envelope minted from a prepared document never resolves —
but for a code action's (see Workspace Edits) — nor does a call or type
hierarchy item minted from one answer its follow-ups
(`callHierarchy/incomingCalls`, `outgoingCalls`,
`typeHierarchy/supertypes`, `subtypes`): the map it was minted through is
not carried.
A completion item's resolve is the exception for what holds no position: it
is forwarded while the item's server holds the text prepared from the
region's current one, the item going without its edits, and only the
`documentation` and `detail` of the answer are taken. An edit the server
adds on resolve (an auto-import) is not.

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
that line, its whitespace after it; a formatting answer is read as the
text it leaves instead, see below) — provided every indentation the peer removed from the segment's content
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
concatenated pipeline's result) is read as the text it leaves, not edit for
edit: its edits are applied to P (an end at the start of the line just
past P's last naming P's end, as whole-document answers do), the region's
edge layout of P restored as for any document (but for an on-type answer,
which keeps the edges as it writes them, typing being an edit, not a
reformatting of the region: one dropping P's final line break is refused
where the peer deleted closing lines after it, and dropped whole
otherwise, as any answer still removing the line break that ends a
region is: what follows the region, a closing fence say, would join its
last line). The text V takes for the edited P is then written, line by
line:

- A line the answer left as it was — whole lines alike at either end of P
  and the edited P, then a longest run of lines alike between them — stays
  as V has it, whatever the peer removed from it: a whitespace-only line
  keeps the host's whitespace.
- Any other line is written as the edited P has it, after the indentation
  the peer removed from the content. Every line the answer wrote regains
  it, a whitespace-only one too; an empty line carries none.
- A last line written with no line break, where P ends with one, takes
  the place of the indentation-only last line the peer removed.

The edits sent are what V differs from that text in. The lines not alike
are replaced; within them, lines reading alike in their letters and digits
are paired (a line a formatter respaced or requoted is the line it was),
and each pair is narrowed to the characters that differ. So is each run
of lines between pairs: line for line where the answer writes as many
lines as V holds there, as one stretch otherwise. The set is then checked
as a whole, as above.
Any diff of V against the same text leaves the same host text, so how
lines pair decides only how small the edits are, and which of several
whitespace-only lines P shows alike keeps the host's whitespace — never
where text goes, nor which indentation it regains.

An answer changing a region whose peer dedented it unevenly — a line of
text (not whitespace alone) the peer left alone, or dedented otherwise
than the rest of its segment — is refused: every line an answer writes
regains the one indentation of the content, which such a region does not
have. (Edits that keep their extents state where they go, and still map
there.) So is an answer rewriting a line that holds a gap's text, or the
line after one a gap's text ends: V holds other text there than P shows.
Lines it leaves alike may hold gaps, and the lines around them map.

Text where a whitespace line the peer kept was is a line of the content,
like any other the answer wrote: the server formatted P, and the host
takes each of its lines as far from the content's left edge as the server
wrote it. Whether a diff would read that text as typed on the kept line
or as a line written in its place does not come into it.

Pairing lines and narrowing the changes has a budget counted in work, not
time: each step of its searches (a diagonal followed, a cell of a table)
spends one, each line indexed or kept to pair eight, and each byte
compared by character sixteen — about a million in all and 16 more for
each byte of the texts, 2^24 at most. So an answer maps to the same edits
on any machine and under any load. The budget decides only how small the
edits are: out of work, the lines not yet paired are replaced whole (a
whitespace-only line among them as the answer wrote it), and no answer
is refused for its size. Whole lines alike at either end of P and the
edited P are set aside first, uncharged, so the work follows what the
answer changes, not the region's size: a one-line answer costs its line
in any region. The character diff pairs the characters other than
whitespace first and diffs only what lies between them, so an answer that
mostly moves whitespace (a pretty-printer's) is narrowed in work linear
in the texts. What the searches keep of the answer is spent before it is
taken, some four bytes a step: 64 MiB at most. Only P's lines are
indexed, and of the answer's only those alike to one of them are kept, so
beside that the mapping holds what is linear in the texts (the edited P,
the lines V takes, the edits): a 15 MB answer to a 590-byte region maps
to one edit in 90 to 190 ms within 87 MB, and a 33 MB one in 210 to 420
ms within 175 MB (release build, on a loaded machine). What the mapping
does beyond its budget is linear in the answer, as reading and parsing
the answer already were, so it runs where the answer arrives, not on a
blocking thread.

An earlier design mapped each stretch of a diff of P against the edited P
as one edit, so the indentation a stretch regained depended on how the
diff had aligned the lines. Making the alignment safe took eight stages
of line pairing and a refusal wherever two readings gave different host
text (text where a kept whitespace line was), and its keys for every
line, kept as text, took 1.5 GB for that 15 MB answer before refusing it
as out of work. Over the same generated answers (253,804 changing P) the
two designs leave the same host text but on whitespace-only lines (8% of
the answers); writing the text first maps another 3% the earlier one
refused, and refuses none it mapped.

### Workspace Edits

A rename's or a code action's workspace edit, and a server's
`workspace/applyEdit`, are one set of edits per document the client
applies together. The request's virtual document's edits are **replayed**
(an end at the start of the line just past P's last naming P's end, as in
a formatting answer: a whole-document replacement's):
taken in P's order and grouped where they touch (one ending where the next
starts, two inserting at one place). A group within one line (neither what
it replaces nor its text holds a line break) creates no line and regains no
indentation: it maps as the one edit it makes, keeping its extent, as a
completion's does — on any region, an unevenly dedented one included. Any
other group is replayed on the whole lines it reaches (groups whose lines
meet are one), and those lines are rewritten in V exactly as a formatting
answer's are, with its budget and refusals: what the group leaves alike
stays as the host has it (a whitespace line's whitespace), and the lines
it writes regain the content's indentation. Such a group is refused, with
the whole edit, on a region the peer dedented unevenly, as a formatting
answer is. Groups are rewritten apart, never as one text: across the whole
text a line the edits left alone could be read as another reading the same
in P (the first of two alike lines deleted, a whitespace line below the
second going with it), against what the edits state. The mapped set is
then checked as a whole, as above.
A `documentChanges` entry holding annotated edits, which must keep their
identity and count, is taken only where replaying maps one edit for each:
every edit within one line and touching no other, each then mapped
keeping its extent; an entry whose edits add, remove or rewrite lines, or
touch, is refused with the whole edit, as no edit of a replayed group is
any one of them.

The choice was made on an exact-text property built without the map:
documents generated as lines of text over one indentation (some left at
column 0, blank lines emptied or kept, LF and CRLF), edits as a server
sends them for a rename, an import with a rename, a block wrapped or
unwrapped (the header and the first indent one edit or two, blank lines in
the block left alone), a line deleted, each also as one edit rewriting the
whole document; the expected host text is the same change made to those
lines. Its default run (6000 cases) gives, on evenly dedented documents:
replayed, 1835 exact, none wrong or refused; each edit keeping its extent,
318 wrong (every whole-document form rewriting a blank or left-alone line
as created) and 8 refused (touching edits at a line start); rewritten as
one whole text, 2 wrong (a blank line deleted by name, the pairing picking
another P shows alike). On unevenly dedented documents replay maps every in-line
change and refuses every one reaching across lines; extent maps many of
those, but is wrong on 548. Which of several blank lines alike a whole
document rewritten deletes no edit of that shape tells, and is left out.

Beside the region's edits, the edit is refused whole when it cannot be read
as one set of host edits: an edit made to the host URI directly reaching
into the region's host text (meeting it at an edge is not reaching in) —
it never went through the map, so nothing keeps it off the text P hides,
nor tells which text it was computed on; more than one `documentChanges`
entry with edits for the virtual document, or one beside an entry with
edits for the host URI — entries apply in turn, each to the text the ones
before left, while the region's edits map as the server's text was; or, in
`changes`, a direct host edit and a mapped one where one inserts at the
other's start, at the region's edge (a host insertion where the region
ends, beside a line appended at P's end) — which text goes first there no
edit states (one ending where the other starts is kept). A file
operation on a virtual document refuses the whole edit, and other regions'
virtual documents are dropped, as on any document.

A rename whose edit the map refuses counts as its server not answering:
a server ranked below it, or another layer, may rename. When none does,
the rename is answered with a `RequestFailed` error naming the reason,
not with `null`: that reads as nothing to rename, while a server did
rename, and asking again would be refused again. (A file operation on a
virtual document, no rename on any document, stays no answer.)

Snippet edits never reach a prepared document. LSP 3.18's
`SnippetTextEdit` in `documentChanges` is not among the types kakehashi
parses LSP with: the editor's `workspace.workspaceEdit.snippetEditSupport`
is dropped as `initialize` is read, so no downstream server is invited to
send one, and an entry holding one fails the parse of the whole edit (a
rename answers null, the code action is skipped, `workspace/applyEdit` is
answered `InvalidParams`), as on an unprepared document. A server sending
snippet syntax in a plain `TextEdit` under an editor's `experimental`
capability (rust-analyzer's `snippetTextEdit`, marking the edit with
`insertTextFormat`) loses the mark in that parse: the editor inserts the
text as is, which is the text the map checks.

prepareRename's range, which the client shows as what a rename replaces,
and linked editing ranges, which it types into together, are offered only
where they cover in the host exactly the text they cover in P (no removed
indentation, no hidden line inside): otherwise no answer (for linked
ranges, none of them).

A code action's diagnostics and range go host to V to P as positions do. A
lazy action from a prepared document is resolved through the map its
server holds when it is picked, only while it holds the very text the
action was listed on: its envelope records that text's generation (not the
map); the reply must be computed on that text (noted, with the map, as the
resolve is queued, and unchanged as it answers), and the final check below
must hold for the region. Otherwise the action comes back unresolved. For
an editor that cannot resolve actions, they are resolved within the list
request, through its map, the same way. An action whose edit the map
refuses comes back disabled, for an editor that shows disabled actions
(dropped for one that does not): under `preferred`, among servers or
layers, an answer of only such actions does not win over one ranked below
that answers, and is the answer when none does.

A server's `workspace/applyEdit` maps through the map of the text its
connection held as the request was read: a text recorded after that is
not the one the edit was computed on, and the edit is refused; so is a
versioned edit naming another version than the one its connection was
sent. Decided at once, never waited on: refused, the server is answered
`applied: false` with the reason, and the editor is never asked.

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
the merged answer, the host layer's part kept while the host is still the
text the request read. Inlay hints read it only
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
  formatting methods, renames, color presentations, code actions,
  linked editing ranges and prepareRename, which wait for that to clear
  (see below);
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
asked to re-query), color presentations answer nothing, inlay hints
stay shown without their edits, code actions answer without the stale
region's actions (the host layer's and other regions' stay when
concatenated; under a cross-layer `preferred` a virt answer already
moved or stale as the layers' race decides is no answer of its layer, and
the race falls through to the host layer's, while one chosen and then
dropped — settling as the race decided, or moved since — leaves none
until asked again), and prepareRename and linked editing ranges answer
nothing. Under a cross-layer `preferred`, a rename's,
prepareRename's or linked editing ranges' virt answer already moved or
stale as the race decides is likewise no answer of its layer: the host
layer's wins. A rename's does so only while the host is still the text
the request read (neither edited nor closed and reopened), as a
formatting answer's stand-in does (below): read for the text the host
moved from, its edits would be as misplaced as the ones passed over, and
the rename is answered `ContentModified`. Code actions, prepareRename and linked editing ranges
wait, as formatting does (below), while the host's last edit settles or a
region's hold clears: an editor may ask them once right after an edit
elsewhere (Neovim asks code actions once per keypress), and an answer
dropped only for that would be asked again only on the next keypress.
Completion and inlay hints, asked again as the caret moves, do not wait.
A rename comes once per action, like formatting: it waits as formatting
does, and dropped, it is answered as `ContentModified` rather than as
nothing to rename: an editor that shows the error lets the user rename
again; how it is shown is the client's (VS Code's language client reads
it as no result, as it would `null`), and none retries it on its own
unless it lists the method to.
A prepareRename range is judged as code actions are (waiting while the
host's last edit settles, none once moved or stale). Formatting reads whether the host moved
under its answer only when the final answer is chosen, not as each reply
arrives: refused there, a reply would count as its server failing, and the
fan-in would fall through to a server ranked below it whose reply came
once the host settled. Nor do formatting, renames and color presentations drop an answer for the host's
last edit not having settled yet, or a region held for its prepare answer:
formatting, range and on-type formatting and renames each come once per
action (an on-type trigger, a save, a rename), and a color presentation once per picker
interaction, so an answer dropped for that would not be asked again. The final check waits, as a request waits for its text (up to the
prepare timeout), until the edit settles and the hold clears, then judges
the answer as above; the host edited meanwhile, it is dropped at once as
moved, and once nothing is left to settle or clear, an answer still not
current (its region re-prepared to other coordinates, no longer tracked,
its host reopened, its map revoked) is dropped at once as stale; one that
never settles is dropped when the wait ends. The warning says which.
Dropped (moved, stale or unsettled), the whole answer is dropped (the host's own formatter's edits and
those of regions still current too) and the request is counted failed, so
`kakehashi format` exits 2 and writes nothing. To an editor, each of the
three formatting methods then answers `ContentModified` (-32801), the
error LSP provides for a result a change made invalid, its message saying
which verdict dropped the answer: `null` would read as nothing to format,
and the action would be lost without a word. Under the
`preferred` strategy, an answer of any of the three formatting methods
dropped as stale or unsettled gives way to the host layer's answer when that layer is in the race
(waited for if still running) and the host is still the text the request
read (not edited, nor closed and reopened): read for that text, it lost
only to the answer dropped, so it answers (an empty answer, already
formatted, too) and the request is not counted failed. Dropped as moved, nothing stands in: the host layer's edits were
read for the text the host moved from too. Color presentations hold the
same rule for an answer the race passed over while it settled: the host
layer's answer stands in for it, dropped as it settles, only while the
host is still the text the request read. A reply its map
refuses counts as that server failing, as any failed reply does:
`kakehashi format` exits 2 when no other server formats the region.
On-type formatting is the exception in what it logs: an answer refused
(no answer), or dropped at the final check with nothing standing in
(`ContentModified`, as above), is logged at debug, so that trigger
keystrokes do not warn the user each time. An answer its map refuses
stays `null` for every method: asking again would be refused again.

Formatting outcomes follow one rule, the one `kakehashi format` counts by.
An answer, an empty one (already formatted) included, is a success. A
failure is something owed an answer and left without one: a request whose
servers all failed (under `preferred`, only when none answered), a span
left unformatted for want of its prepared text (see Failure), a chosen
answer dropped at the final check, an edit its map refuses. Only an answer
for the same thing, read for the same text, makes up for one: another
same-span alternative's for a span, the host layer's for a dropped
`preferred` answer (above). A server not asked (not configured, or not
advertising the method) is neither. A failure inside a layer the race
waits for counts whichever answer it then chooses, as a failed region
request does without the prepare layer; a layer dropped once a higher one
wins counts nothing it had not counted yet. A span whose peer failed and
whose taker failed too counts both: two operations failed. Range and
on-type formatting choose their answer by the same rule and count
nothing.

Unlike on an unprepared document, an answer overtaken by typing is
dropped, not kept stale. How an edit maps back depends on the whole text
— which lines are blank, what an inserted line joins, which indentation
a created line regains — so no text but the request's is known to place
it as the server meant, and a stale edit could land in indentation the
host has and P never showed. Completion answers dropped this way come back
as an empty incomplete list, which the client asks again for, and inlay
hints stay shown with only their edits stripped: the answer for the latest
text still arrives, the cost availability (an answer within a keystroke's
parse and prepare round-trip), never a misplaced edit; code actions,
prepareRename and linked editing ranges likewise, though they first wait for the host's last
edit to settle (see above). Color presentations are asked once per picker
interaction, and the formatting methods and renames once per action: they
wait for the host to settle instead, as the final check described above
does.

A color presentation without a `textEdit` replaces the request range with
its label; on a prepared document that edit is made explicit and mapped
like any other. A request range reaching into a gap stands for host text no
presentation may replace (with the gap emptied, its prepared range is even
empty, and an edit there would only insert beside it), and one right after
an emptied gap reaches the server where the gap's start does, an edit there
mapping back before the gap: for either, no presentation is returned.
Additional edits refused with the main edit are dropped, the main edit kept
as it maps alone.

A document the peer left as it is — it answered `null`, or an answer
changing nothing, and the document has no gap — has P for V: nothing was
removed that an edit could regain, and its server sees the caret where
it is. Its completion items go as a document as is sends them, snippets
and line-breaking edits included; the rules that follow are for a
document the peer changed.

Completion keeps to what the client inserts where it asked:

- A caret inside indentation or blank lines the peer removed (or from
  where the deleted closing lines start) gets no completion: the server
  sees it elsewhere, and its items would edit away from it.
- An item's edit must contain the caret once mapped (a caret right after a
  gap the peer emptied reaches the server where the gap's start does); an
  insert/replace edit maps through both ranges, which must start at the
  same position as sent, map the shared text alike and keep their shared
  start.
- A snippet is checked as the client expands it, every tabstop at its
  default, and kept only in the forms every client expands alike: literal
  text escaping nothing but `$`, `}` or `\`; tabstops `$n` or `${n}`, the
  number written without a leading zero and at most 9999, the final
  tabstop at most once; at most one placeholder `${n:…}` per tabstop, on
  its first occurrence (later ones mirror it), its default literal text
  escaping only the same three; all on one line, with no tab, and read as
  a snippet by its text (a `$` opening a word after anything but a
  backslash: mini.completion inserts other text as is). Anything else is
  refused: choices (VS Code inserts the first option, Neovim's
  `vim.snippet.expand` nothing until the user reaches it), variables (the
  editor resolves them at insert time), transforms (not computed here),
  nested placeholders (which clients resolve differently), a default after
  its tabstop's first occurrence (mini.snippets mirrors the first), other
  escapes (Neovim reads them into a pattern), a `$` opening nothing (which
  Neovim's grammar does not read), and tabstop numbers with a leading zero
  (mini.snippets reads `01` as another tabstop than `1`) or past 9999 (a
  bound every client reads alike: Lua numbers lose integers past 2^53).
  The expansion is what the map and the region's
  checks read, with the additional edits, through each range the client
  may apply the main edit with; the snippet goes out with the indentation
  the expansion regained written before it — spaces only — and is dropped
  otherwise. One
  expanding to spaces alone is dropped: the user types at its tabstop,
  which on a blank line among dedented ones regains no indent. A snippet
  without a tabstop (literal text, or only escapes) is sent as the plain
  text it stands for, which clients insert as is (for an item of the
  snippet kind, see below): mini.completion expands an item as a snippet
  only when it holds a tabstop, a tab or a line break, and would insert
  `x\$` with its backslash.
- An item of the snippet kind going out as plain text is dropped where
  its text holds a line break, a tab or a `$` opening a word:
  mini.completion inserts such an item as a snippet whatever its format
  says.
- An item without a `textEdit` is inserted at the caret, outside the map:
  it is dropped when its text breaks a line; where the same text as an
  edit at the caret regains an indent (a blank line among dedented ones,
  or one the peer emptied of other whitespace, which that edit replaces
  on the caret's line), it goes out as that edit, its additional edits
  checked with it; inserted at the caret as is, it drops them, having no
  main edit to be checked with.
- A mapped multi-line edit already regained the removed indentation: the
  item asks a client that supports it to insert as is, and is dropped for
  one that always adjusts indentation. An edit the client would have
  adjusted — the item asks for `adjustIndentation`, or names no mode and
  the client declares that its default (`completion.insertTextMode`) — was
  written to get the indentation of the line it starts on, up to its
  start, on every line below: it is given that over P before it maps
  back, so the lines end up with what P shows of that indentation after
  what the peer removed. A client declaring no default is taken to insert
  as is.
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
  current one describes no current host position (a push read under it
  stays placed, see below), and a region the lifecycle pass prepares is
  bridged only through a prepared text: one held as is, or none on record
  yet, translates nothing. A connection ever sent
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
  An item minted from a prepared document resolves without a sync: its
  request is queued only while its server holds the text prepared from the
  region's current one, checked as any request's text is.
- **A push follows the text its connection held when kakehashi read it.**
  The push is stamped as it is read, before it waits to be published.
  Where preparation is involved (by the rule above), it is placed through
  the map that very text was last held with, and through no other: one
  naming another document version than the one its connection was last sent
  was computed for an earlier text and places nowhere. It stays placed that
  way once the text is replaced — while an edit waits for its answer, and
  after newer texts are sent — until a push read under a newer text takes
  its place, as on any unprepared path. A server that publishes only what
  changed sends nothing after an edit its diagnostics stay the same
  through, so taking the push down with the text would lose them for good.
  Until then its positions are the earlier virtual text's, under the
  region's current offset. It is taken down when the document closes on its
  connection, when that text's map is revoked, while the region's peer
  fails its current input, and — read under a text held as is — once the
  region is prepared. Elsewhere — and for a push read while its connection
  had never held a prepared text of the document — it places as on any
  unprepared path.
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

### Presenting stripped host text as gaps

Stripped text presented as a gap of no width could take a placeholder,
which an interpolation opening a host line cannot (see The Request). But
kakehashi cannot present those interpolations alone: text stripped before
a line's first injected character is as often a line prefix (a
blockquote's `> `, a list item's indentation), and nothing in the
injection tells the two apart. Presenting both would hand a peer every
prefix of a quoted document as a gap to leave alone, and split that
document's content at each line: no blank line would be at the
document's edge any more, each lying beside a gap, so the edge rule would
have to tell gaps of no width from the others. The cost falls on every
prefixed document for the sake of a line that starts with an
interpolation, which reads to servers as it does without a peer.

### The downstream server prepares its own documents

A server that wants dedented input can dedent internally — but then it also
has to translate its own coordinates back, and off-the-shelf servers cannot.
The protocol exists so the peer that presents is not the server that
analyzes.

### Naming the peer at the prepare key alone

Exempting the key from the `"_"` method entry and defaulting it to `[]`
would hold no document for a handshake until a peer was named: with
experimental features on and no peer configured, a language's first
document would not wait for its servers to start. But the key would have
rules no other method has — a second exemption beside
`kakehashi/bridge/routing`, a default of its own, and an
`aggregation."_"` allowlist reaching every method but this one. The key
reads like every LSP method instead, and `[]` opts a pair out.

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

### Snippets breaking lines for an editor declaring it inserts as is

A multi-line snippet could regain the removed indentation on its lines
(written into its literal text, or into a default its tabstop holds once)
and go to an editor declaring `insertTextModeSupport` with `asIs`, marked
so. The declaration does not reach the snippet engine: mini.completion
declares it and expands through mini.snippets or `vim.snippet.expand`,
which indent the lines after the first by the cursor line's anyway, so
the regained indentation would be doubled. No declaration tells which
editors keep a snippet as is.

## Consequences

### Positive

- Dedent, placeholders and other presentation policies live in user code
  (tsudoi handlers) instead of kakehashi and its query vocabulary.
- Read-only features — hover, definitions, diagnostics, folding, inlay
  hints — work on embedded code its servers could not parse as written.
- So do completion, formatting, rename and code actions, their edits
  placed back with the indentation the peer removed.

### Negative

- Every new document version waits for a peer round-trip before downstream
  servers see it.
- With experimental features on, a language's first document waits for its
  servers' handshakes to learn whether any prepares it, even when none
  does.
- No edit-producing method (those listed in Coordinates) nor inbound
  `workspace/applyEdit` is available on a prepared document with gaps;
  completion items resolve only their `documentation` and `detail`.
- A snippet completion breaking a line or holding a tab is refused on a
  document the peer changed, whatever the editor declares (a literal one
  goes out as plain text; a document the peer left as it is keeps its
  snippets): snippet engines rewrite both whatever `insertTextMode`
  asks. Neovim's `vim.snippet.expand` (`runtime/lua/vim/snippet.lua`)
  prefixes every line after the first with the cursor line's indentation
  and, under `expandtab`, turns every tab into spaces; mini.snippets
  indents the later lines too, behind mini.completion, which declares
  `insertTextModeSupport` with `asIs`. The indentation a prepared map
  regains would be doubled, or a tab shortened. One-line snippets with
  tabstops and placeholders still map, and literal snippets go out as
  plain text.
- A rename's, code action's or server's own `workspace/applyEdit` edit
  adding, removing or rewriting lines on a
  region the peer dedented unevenly is refused whole (in-line edits, a
  rename's, still map): the property above refuses every such change
  there, where mapping each edit keeping its extent would map many of them
  rightly but few of the whole-document forms.
- A `documentChanges` entry of annotated edits (an editor supporting
  change annotations) whose edits add, remove or rewrite lines is refused:
  its edits cannot be replayed one for each.
- A workspace edit editing the host document directly inside a prepared
  region, holding a `documentChanges` entry with edits for the host
  beside its region's (or more than one for the region), or in `changes`
  inserting on the host at the region's edge where a mapped edit starts
  (or a mapped insertion where a host edit starts), is refused; edits a
  server makes to another region's virtual document are dropped, as on
  any document.
- An unversioned `workspace/applyEdit` computed on a text before one
  already sent but not yet read by its server (no notification is
  acknowledged) maps through the newer text's map: nothing tells the two
  apart. A versioned edit is refused instead, as on any document, where a
  stale unversioned edit lands as stale.
- A lazy code action whose server was sent another text since it was
  listed (a re-prepare, an edit) resolves to nothing; it is listed again on
  the next request.
- A formatting answer whose diff runs out of its work budget maps to
  coarser edits, not to other text: the lines not yet paired are replaced
  whole, so an editor loses the marks and folds it kept on them, and the
  whitespace-only lines among them take the answer's whitespace. The
  common case is narrowed in work linear in the region: an answer that
  keeps most lines, mostly moves whitespace, respaces every line (`x=1`
  to `x = 1`), or rewrites the punctuation of every line (`f('a')` to
  `f("a");`, 20,000 lines, each changed where it differs). Past the
  budget's cap (2^24 steps, some megabyte of text compared by character),
  lines are replaced whole, one by one where the answer writes as many
  lines as it replaces. The answer's positions and the result's
  convert to bytes and UTF-16 columns in one pass over each line they
  fall on, each line found in a few lookups, uncharged.
- The concatenated formatting pipeline does not re-apply host line prefixes
  on a prepared document: a prefixed (blockquoted) prepared region fails
  the request instead.
- Formatting, range and on-type formatting refuse a region whose peer
  dedented it unevenly (a line of text left alone, or dedented otherwise
  than the rest), whatever the answer changes; completions, hints and
  other edits that keep their extents still map there. Every line an
  answer writes regains the content's one indentation, and such a region
  has none; reading which line was which from a pairing instead, as an
  earlier design did, met one ambiguity after another (copies crossing, a
  line moved off its own indentation, one copy reindented into another's
  text), several mapped to wrong text before they were found. A refusal
  is the safe side. The
  usual peers dedent evenly: a Nix `''` string strips the least
  indentation of its lines (a line opening with an interpolation counts,
  so one at column 0 strips none), `textwrap.dedent`, `indoc!` and `<<~`
  the common prefix, and a YAML block scalar requires its indentation. A
  CommonMark fence strips up to its opener's own indentation from each
  line, so a line less indented than an indented fence's opener is
  dedented unevenly, and that block is not formatted through its peer.
- A whitespace-only line keeps the host's whitespace only where the
  answer leaves the line as it was and the diff pairs it with itself. One
  the answer rewrites (other whitespace, another line break), one of
  several alike the pairing reads as another, and any not yet paired when
  the diff runs out of work, is written as the answer has it: empty, or
  after the content's indentation. A peer that strips the indentation
  from every line shows that line back as the server wrote it; one that
  keeps whitespace-only lines as they are shows it indented once more.
  Formatters leave no whitespace on blank lines, so in practice such a
  line is emptied.
- A formatting answer rewriting the line a gap's text is on, or the line
  after one a gap's text ends, is refused, as is a workspace edit creating
  or removing lines there. Lines holding gaps map when left alike, but
  edit carriers stay closed on prepared documents with gaps (see
  Coordinates).
- A peer must be bridged for the languages it prepares, so it receives
  their documents too; keeping it out of other methods takes `priorities`.
- A peer that cannot start leaves its documents unprepared rather than
  unsent, so servers may see unprepared text until it is up.
- A document the peer leaves as it is costs less than one it changes,
  not nothing: it waits for the peer's answer like any other, and what
  is listed from it resolves as from a prepared document. Only its
  completion items go as a document as is sends them (see Edits).
- A peer bridged for every language (`"*"`) prepares every injection it
  can under the default `priorities`, and declining with `null` still
  makes a document prepared; restricting it takes `priorities = []` where
  it should not apply.
- `*/resolve` for items from prepared documents is refused (stale), as are
  call and type hierarchy follow-ups for their items. A completion item
  resolves its `documentation` and `detail` only: edits its server defers
  to resolve, as the editor's `resolveSupport` lets it, are lost.
- Host text the injection query includes in content cannot be protected as
  a gap; only text outside the captured content is.
- An interpolation with nothing injected before it on its host line takes
  no placeholder: stripped like a line prefix, it is no gap (see The
  Request).
- A request issued right after an edit waits for the new prepared text to
  reach its server, and fails if a newer edit supersedes it first: as soon
  as its server is sent another text instead. Only a text sent while the
  request waits tells: one its server held already may be the text before
  the request's own, or, after an undo, an answer given later for the text
  the undo left. So a superseded request that starts waiting after the
  newer text was sent (its own answer came late) waits five seconds for
  nothing, unless the editor cancels it. A request on an unprepared
  document waits the same way while its server still holds a prepared text
  (right after its pair lost its peer).
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
- Between an edit of a region and its server's next publish, a pushed
  diagnostic is shown where the text before the edit had it, as on an
  unprepared document.

### Neutral

- The peer sees only the segments, not the whole host document; a peer that
  needs more context can be configured as a host-document server as well.
