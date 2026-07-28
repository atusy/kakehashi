# Any-Language Server Wildcard

**Related Decisions**:
- [language-server-bridge](language-server-bridge.md) — The `languageServers.<name>.languages` field this decision extends
- [wildcard-config-inheritance](wildcard-config-inheritance.md) — The `_` key inheritance that occupies the empty/absent spelling
- [aggregation-priorities-wildcard](aggregation-priorities-wildcard.md) — The sigil discipline (`_` for keys, `"*"` for list elements) this decision follows
- [host-document-bridge](host-document-bridge.md) — The second axis that selects servers through the same `languages` list

## Implementation Status

Implemented. `LANGUAGES_WILDCARD` and the `BridgeServerConfig::handles_language`
predicate live in `src/config/settings.rs`; the two selection sites that consume
it — `get_all_configs_for_language` (injections) and
`get_host_configs_for_language` (host documents) — are in
`src/lsp/bridge/coordinator.rs`.

## Context

`languageServers.<name>.languages` is an enumeration of the languages a
downstream server answers for. Selection is plain membership: a server is a
bridge candidate for an injected region when its `languages` contains that
region's language.

Enumeration fits servers that *are* a language's server — `rust-analyzer` for
rust, `pyright` for python. It does not fit the cross-cutting ones, which
language-server-bridge already anticipated as a motivating case: grammar and
spell checkers (`harper-ls`), typo linters (`typos-lsp`), AI completion. These
have no finite language list. Enumerating "every language I might ever open" is
both unwritable in advance and wrong the moment a new injection language
appears in a document.

The obvious spellings for "any" are already taken:

- **`languages = []`** — `merge_bridge_server_configs`
  (`src/config/merge.rs`) treats an empty overlay list as "inherit `languages`
  from the `_` entry" (wildcard-config-inheritance).
- **`languages` omitted** — `#[serde(default)]` on `Vec<String>` produces that
  same empty vec, so it lands on the identical inherit path. (`null` is not a
  third spelling: TOML has no `null`, and an explicit JSON `null` from
  `initializationOptions` fails to deserialize into `Vec<String>` rather than
  defaulting — the field is neither `Option` nor `default_on_null`.)

Both therefore mean *defer*. A concrete server has no way to **widen** past an
inherited narrow list — only to accept it. That is precisely the gap a
language-agnostic server needs to fill.

## Decision Drivers

- **Widening must be expressible.** The empty/absent spellings can only defer;
  something must be able to say "more than what I inherited".
- **Sigil discipline** (aggregation-priorities-wildcard): `_` is for wildcard
  *keys* carrying field-level inheritance. A list *element* meaning
  "everything" carries no inheritance and must not reuse that sigil.
- **Open set.** Injected languages are discovered from documents at runtime;
  "every language" cannot be enumerated statically at config-merge time.
- **Room to grow.** Whatever shape is chosen should not foreclose refinement
  ("any *except* markdown").

## Decision Outcome

**Chosen approach**: a `"*"` element in `languages` matches every language.

```toml
[languageServers.harper-ls]
cmd = ["harper-ls", "--stdio"]
languages = ["*"]
```

### Resolution Rules

| Configured value | Meaning |
|---|---|
| `["rust"]` | matches rust only |
| `["*"]` | matches every language |
| `["rust", "*"]` | matches every language — membership has no ordering, so the named entry is redundant, not restrictive |
| `[]` / omitted | **inherit from `languageServers._`** (unchanged); if nothing is inherited the server matches nothing |

- Resolved at **match time** (`handles_language`), not expanded during config
  merging. "Every language" is an open set with no static enumeration.
- Resolution order is unchanged: wildcard *inheritance* (`_`) is applied first,
  then the resolved `languages` list is tested. `languageServers._.languages =
  ["*"]` is therefore legal and inheritable.
- `"*"` is safe as a marker in practice — no real tree-sitter language is named
  `*` — but that is an observation, not an enforced invariant. Injection
  language ids are taken verbatim from `#set! injection.language` properties and
  from `@injection.language` capture *text*, and canonicalization falls through
  to the raw identifier, so a hand-authored query or a `` ```* `` fence can
  produce the literal id `*`. The consequence is benign: such a region is
  matched by exactly the servers that already accept every language.

### Scope: `priorities` Still Excludes an Unlisted Server

A third limit, easy to miss because it lives on a different axis:
`aggregation.<method>.priorities` is an ordered **allowlist over server names**
(aggregation-priorities-wildcard), and its `"*"` element means "the configured
servers not named elsewhere in this list" — a different thing from a `"*"`
*language*. A `"*"` server passes `handles_language` and reaches the candidate
set, but is still dropped from any target whose `priorities` enumerates server
names without a `"*"` element.

```toml
# harper-ls.languages = ["*"] answers in every fence EXCEPT python,
# because this list names servers explicitly and omits it.
[languages.markdown.bridge.python.aggregation."_"]
priorities = ["pyright", "ruff"]
```

The default (absent `priorities` ≡ `["*"]`) includes it everywhere, so this
only bites configs that have already opted into explicit priority lists.

### Scope: What `"*"` Does Not Widen

Both bridge axes select servers through this one list, so `"*"` reaches both.
Two limits keep that from becoming a blanket opt-in:

1. **It does not enable a blocked language.** `"*"` widens *which servers may
   answer*, not *which injections a host bridges at all*. The host's
   `languages.<host>.bridge.<lang>.enabled` filter is evaluated before server
   selection and still applies.
2. **It does not opt into the host-document bridge.** A `"*"` server becomes a
   host candidate for every host language, but candidacy is not consent: the
   host path stays gated on the explicit `bridge._self.enabled = true` opt-in
   (host-document-bridge).

Restricting `"*"` to the injection axis only was considered and rejected: both
axes ask the same question ("does this server handle language L?") through the
same field, and a marker that silently means "any" on one axis and "none" on
the other is harder to document than the `_self` gate that already exists.

## Consequences

### Positive

- **Cross-cutting servers become configurable** without enumerating a language
  set that cannot be known in advance.
- **Widening becomes expressible** at all, closing the defer-only gap left by
  the inheritance semantics of the empty list.
- **No behavior change for existing configs**: `"*"` is opt-in, and no existing
  config could have used it meaningfully — it was a language name matching
  nothing.

### Negative

- **`languageServers._.languages = ["*"]` is a footgun**: it attaches every
  server that omits `languages` to every language, silently and totally. It is
  the same hazard any `_.languages` value carries, but the blast radius is
  maximal — and it crosses config *files*, since layers collapse before `_` is
  resolved at match time, so a `_` wildcard in the user config also widens
  servers declared in a project's config. Documented with a recommendation to
  declare `"*"` on concrete servers instead.
- **Opting a single server back out is not spellable in `languages`.** Under a
  `_` wildcard, `[]` means "inherit" and resolves back to `["*"]`; the escape
  hatch is `enabled = false` (which the selection sites check first, via
  `is_spawnable`). Narrowing to a real language list does work — a concrete
  non-empty `languages` overrides the wildcard.
- **Cost scales with injection *regions*, not with languages — and that is a
  much bigger number than it sounds.** The process count does not grow at all:
  the connection pool is keyed by `(server, resolved root)` with no language
  component, so one `"*"` server is one process per root. What grows is
  per-region work — a virtual `didOpen` per region, a task and a downstream
  request per region on whole-document methods, a context per region per
  diagnostics cycle.

  The worst case is the wildcard's own headline use case. The shipped markdown
  injection query emits a `markdown_inline` region for *every* inline node and
  every table cell, so a prose document produces regions on the order of
  hundreds. Before this change nothing matched `markdown_inline` and every
  consumer dropped those regions at an `is_empty()` check; a `"*"` grammar
  checker matches all of them. Nothing bounds this: `maxFanOut` caps servers
  per region, not regions, and the capability prefilter cannot help for methods
  the server *does* advertise (diagnostics, hover, codeAction — exactly the
  ones such a server exists to answer). The prefilter also only acts once the
  server is `Ready`, so the first request in any language still pays the spawn
  and initialize.

  The mitigation is the per-host bridge filter, which is evaluated before
  server selection: disable the injection languages the wildcard server should
  not see, e.g. `languages.markdown.bridge.markdown_inline.enabled = false`.
- **Results can duplicate rather than merely multiply.** Diagnostics and
  codeAction default to `concatenated` and neither path dedups by span. Two
  shapes follow: same-span alternate-language regions each contribute
  (a `"*"` server is a candidate for every alternate), and — when combined with
  `bridge._self.enabled = true` — one process holds both the host document and
  its virtual regions, so a finding inside an injected region is reported once
  by the host answer and once by the region answer, at the same host
  coordinates.
- **A `"*"` server joins first-win races it would previously have sat out.**
  Under the default `priorities = ["*"]` every matching server lands in one
  `Rest` group, and the default `preferred` strategy decides that group by
  arrival time. For methods where the `"*"` server returns a non-empty result
  (hover, definition, completion), it can beat the language-specific server
  nondeterministically. Users who care about the ordering must name servers
  explicitly in `priorities` — which, per the `priorities` scope note above,
  then also excludes the `"*"` server unless `"*"` is in that list too.
- **The eager open had to stop picking one server per language.** It resolved a
  single server and gave it the virtual `didOpen`, which was survivable while
  overlapping servers were a deliberate config. A `"*"` server overlaps
  *everything*, and the starved case is not merely a slower warm start: a
  **push-only** server — one that publishes diagnostics instead of answering
  pulls — issues no request, so nothing opens the document lazily and the pull
  path returns on its capability check before `ensure_document_opened`. Missing
  the eager open meant never seeing the region at all, for exactly the
  grammar/spell-checker shape this wildcard exists to support. The eager open
  now fans out to every matching server, like every other selection site, and
  the single-pick resolver is gone.

  This is a behavior change for existing non-wildcard configs too: two servers
  on one language now both spawn and open at `didOpen` where one did before.
  Note it does *not* multiply the region worst case above — nothing but the
  `"*"` server matches `markdown_inline`, so the multiplication lands only on
  languages that already have a real server, where the server count is small.

### Neutral

- `"*"` here means "everything" whereas the `priorities` `"*"` means "the
  unlisted rest, at this position". On an unordered membership predicate the
  two collapse to the same thing ("anything not named"), so the sigil stays
  consistent even though the surrounding list semantics differ.

## Alternatives Considered

### A. `languages = []` means "any"

**Rejected because**: already means "inherit from `_`"
(`merge_bridge_server_configs`). Redefining it would silently convert every
server that currently defers to the wildcard entry into an attach-to-everything
server. Secondary: a multi-line list whose entries are all commented out
collapses to `[]`, so the intent would be unrecoverable from the text.

### B. `languages` omitted means "any"

**Rejected because**: `#[serde(default)]` maps omission onto the same empty vec
as A, inheriting A's problem exactly.

`null` is not a separable variant of this: TOML has no `null` literal, and an
explicit JSON `null` from `initializationOptions` is a deserialization *error*
for a bare `Vec<String>` — `serde(default)` covers a missing field, not a null
one. So it could only mean "any" by first adding null handling that does not
exist, which is strictly more work than A for the same outcome.

### C. A separate boolean, e.g. `anyLanguage = true`

**Rejected because**: it creates a second knob whose interaction with
`languages` must then be defined (what does `anyLanguage = true` alongside
`languages = ["rust"]` mean?), and it cannot grow into exclusions without a
third. Keeping the statement inside the list keeps one field authoritative.

### D. `languages = "*"` as a scalar (untagged string-or-array)

**Rejected because**: two wire shapes for one field, and the scalar form is a
dead end — `"any except markdown"` has no scalar spelling, so exclusions would
force a return to the list anyway.

### E. `languages = ["_"]`

**Rejected because**: `_` is reserved for wildcard *keys* carrying field-level
inheritance (aggregation-priorities-wildcard, alternative C). Reusing it as a
list element would suggest inheritance where none exists.

### F. Invert the axis — declare it as `languages._.bridge.<server>.enabled = true`

Express "this server applies everywhere" from the language side, reusing the
`languages._` wildcard that already exists.

**Rejected because**: it does not actually work without a code change. The
`bridge` map gates *whether a language is bridged*; the server must still pass
the `languages` membership filter to be selected at all, so an inverted opt-in
would need the same predicate change plus a second mechanism to reconcile. Given
a code change either way, the server-side marker is the smaller one and keeps
"which languages does this server handle" answerable from the server's own
entry.

### G. Full glob matching (`languages = ["ts*"]`)

**Rejected as premature**: a strict superset of `["*"]` with no current
requirement behind it. `["*"]` is forward-compatible with adding globs later,
so nothing is foreclosed.
