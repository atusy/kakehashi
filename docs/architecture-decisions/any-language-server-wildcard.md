# Any-Language Server Wildcard

**Related Decisions**:
- [language-server-bridge](language-server-bridge.md) — The `languageServers.<name>.languages` field this decision extends
- [wildcard-config-inheritance](wildcard-config-inheritance.md) — The `_` key inheritance that occupies the empty/absent spelling
- [aggregation-priorities-wildcard](aggregation-priorities-wildcard.md) — The sigil discipline (`_` for keys, `"*"` for list elements) this decision follows
- [host-document-bridge](host-document-bridge.md) — The second axis that selects servers through the same `languages` list

## Implementation Status

Implemented. `LANGUAGES_WILDCARD` and the `BridgeServerConfig::handles_language`
predicate live in `src/config/settings.rs`; the three selection sites that
consume it (injection single-pick, injection fan-out, host fan-out) are in
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
- **`languages` omitted / `null`** — `#[serde(default)]` on `Vec<String>`
  produces the same empty vec, so it lands on the identical inherit path. TOML
  has no `null` regardless.

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
- `"*"` is unambiguous as a marker: tree-sitter language identifiers never
  contain `*`.

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
- **No behavior change for existing configs**: `"*"` is opt-in and was
  previously a language name matching nothing.

### Negative

- **`languageServers._.languages = ["*"]` is a footgun**: it attaches every
  server that omits `languages` to every language, silently and totally. It is
  the same hazard any `_.languages` value carries, but the blast radius is
  maximal. Documented with a recommendation to declare `"*"` on concrete
  servers instead.
- **Fan-out grows** with each `"*"` server, which now participates in every
  injection region. Mitigated — not eliminated — by the capability prefilter,
  which drops servers that have advertised no support for the method.

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

### B. `languages = null` / omitted means "any"

**Rejected because**: `#[serde(default)]` maps it onto the same empty vec as A,
inheriting A's problem exactly. TOML has no `null` literal, so it is not even
spellable in the primary config format.

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
