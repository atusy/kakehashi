# ADR-0024: Base Language Inheritance

## Status

Proposed (Supersedes `aliases` field from [ADR-0005](0005-language-detection-fallback-chain.md))

## Context

The current system uses an `aliases` field to map alternative language names to a canonical language:

```toml
[languages.markdown]
aliases = ["rmd", "qmd"]
```

This approach has three problems:

1. **Duplicate alias conflict**: Multiple languages can claim the same alias (e.g., both `markdown` and `unexpected` define `aliases = ["rmd"]`). The current behavior is last-wins with a warning log, which is fragile and order-dependent.

2. **No per-alias configuration**: Aliases are pure identity mappings — `rmd` becomes `markdown` with no ability to customize. For example, `rmd` embeds R code while `qmd` embeds Python/R/Julia, so they need different bridge configurations, but aliases share the parent's config entirely.

3. **Ownership is inverted**: The parent language declares its aliases, rather than each derived language declaring its parent. This means adding a new alias requires modifying the parent's config.

### What We Want

```toml
[languages.rmd]
base = "markdown"
# rmd-specific bridge config for R code blocks
[languages.rmd.bridge.r]
enabled = true

[languages.qmd]
base = "markdown"
# qmd-specific bridge config for Python
[languages.qmd.bridge.python.aggregation]
"textDocument/completion" = { priorities = ["basedpyright"] }
```

Each derived language declares its own parent and can override any configuration field independently.

## Terminology

| Term | Definition |
|---|---|
| **Base language** | The language named in the `base` field (e.g., `"markdown"` for `rmd`) |
| **Derived language** | A language that declares a `base` (e.g., `rmd` with `base = "markdown"`) |
| **Base chain** | The ordered list of languages from derived to root, following `base` links (e.g., `rmd -> markdown -> _`) |
| **Most-specific-wins** | Merge rule: later (more specific) entries in the base chain override earlier (more general) entries at the field level |
| **Effective config** | The result of merging all languages in the base chain using the most-specific-wins rule |
| **Layer merge** | Cross-layer config merging (ADR-0010): programmed defaults → user → project → `initializationOptions` |
| **Nested wildcard** | A `_` key within a sub-dictionary of the effective config (e.g., `bridge._`), resolved after base chain merging |
| **Overlay semantics** | When merging configs, only fields present in the higher-priority layer override; absent fields are preserved from lower-priority layers (ADR-0010) |

## Decision

**Replace the `aliases` field with a `base` field that enables single-parent configuration inheritance with chain resolution.**

### Resolution Order (Three Phases)

Configuration resolution proceeds in three phases. Each phase depends on the output of the previous one, so this ordering is mandatory:

1. **Layer merge** (ADR-0010): programmed defaults → user config → project config → `initializationOptions`. Produces a single merged config per language, including `base` field values.
2. **Base chain resolution** (this ADR): walk the `base` chain across languages, merge most-specific-wins. This merges configs *across languages* (vertical), whereas Phase 1 merges *within each language across layers* (horizontal). Merge order is lowest to highest priority: `_ ← markdown ← rmd`. For multi-level chains: `_ ← markdown ← markdown_custom ← rmd`. Produces the effective config for each language.
3. **Nested wildcard resolution** (ADR-0011): resolve `_` keys within sub-dictionaries of the effective config (e.g., `bridge._`). Operates on the output of Phase 2.

### The `base` Field

The `base` field participates in cross-layer merging (Phase 1) like any other `LanguageConfig` field: a later layer's `base` value overrides an earlier layer's value. If a layer defines `[languages.rmd]` without specifying `base`, the `base` from a lower-priority layer survives the merge (overlay semantics — see Terminology).

Every language config gains an optional `base` field (default: not set, implicitly `"_"`):

| `base` value | Meaning | Typical use |
|---|---|---|
| `None` (omitted) | Inherit from `_` | Most languages |
| `""` (empty string) | **No inheritance** — chain stops here | `_` itself; self-contained languages |
| `"_"` | Explicitly inherit from `_` | Equivalent to `None`; for clarity |
| `"markdown"` etc. | Inherit from named language | Derived languages (`rmd`, `qmd`) |

- When `base` is `None` (or omitted), the language inherits from `_` (wildcard), preserving current ADR-0011 behavior. `base = "_"` is equivalent to `None`.
- When `base` is `""` (empty string), the language has **no base** — it does not inherit from `_` or any other language. This is useful for fully self-contained language configs that should not pick up wildcard defaults.
- When `base` is explicitly set to a non-empty value (e.g., `"markdown"`), the language inherits from that language instead of directly from `_`.
- **`_` defaults to `base = ""`** — it is the root of all chains and does not inherit from anything. This means `_` is not special-cased; it simply has `base = ""` as its default, and the uniform termination rule is "stop when `base == ""`". Users may override `_`'s `base` (e.g., `base = "some_language"`), but this can create unexpected inheritance chains.
- The chain always terminates at `base = ""`: `rmd -> markdown -> _` (where `_` has `base = ""`).

### Undefined Languages in the Chain

Languages referenced in the `base` field do not need to be explicitly defined in configuration. If `base` points to an undefined language, the chain continues through it toward `_`, and parser/query resolution still searches `searchPaths` using that language's name. No error is raised — this is the normal case for languages discovered via `searchPaths` without explicit config.

### Chain Termination and Error Handling

The **only** termination condition is `base = ""`. When `base` is `None`, the chain does not terminate — it continues by resolving `None` to `"_"`, which itself has `base = ""` by default, terminating the chain there. There is no special case for `_`; it terminates simply because its default `base` is `""`.

**Error conditions:**

| Condition | Behavior |
|---|---|
| **Circular reference** (`a.base = "b"`, `b.base = "a"`) | Detected and terminated at the cycle point. `_` is **not** appended — the language loses wildcard defaults. The misconfiguration is reported to the user. |
| **Self-reference** (`a.base = "a"`) | Special case of circular reference. Same behavior. |
| **Undefined base language** | Chain continues through it (see "Undefined Languages in the Chain" above). Not an error. |

### Nested Wildcard Resolution (Phase 3)

Base chain resolution (Phase 2) **replaces** ADR-0011's outer `languages._` wildcard resolution. Previously, ADR-0011 unconditionally merged `_` with every language. With base chains, `_` is already the root of every default chain (since chains terminate at `base = ""`), producing the same result. Additionally, base chains introduce a new capability: a language can **opt out** of `_` inheritance by setting `base = ""`, which was not possible under ADR-0011's unconditional merge.

Phase 3 applies only to **nested wildcards** — `_` keys within sub-dictionaries of the effective config (e.g., `bridge._`). After Phase 2 produces the effective config for a language, any nested `_` entries within that config are merged with their sibling entries, with the specific sibling overriding the wildcard.

Note: `base = ""` on an intermediate language (e.g., `markdown.base = ""`) intentionally prevents `_` inheritance for all languages derived from it. This is a deliberate design choice — if a language opts out of wildcard defaults, its derived languages respect that decision.

### Parser Resolution

Parser resolution walks the base chain from most-specific to least-specific. At each level, an explicit parser path in the config is checked before searching `searchPaths`. The first match is used.

When a derived language resolves to a base language's parser, the parser is registered under the derived language's name, ensuring all subsequent lookups by language name work correctly.

### Query Resolution

Query file discovery (highlights.scm, locals.scm, injections.scm) follows the same base chain pattern as parser resolution. The first level in the chain that provides queries wins (queries are replaced entirely, not merged, per ADR-0010).

Query files may also contain `; inherits: <language>` directives, which is a separate query-level composition mechanism independent from config-level `base` inheritance. These two mechanisms operate at different levels and do not conflict.

### Impact on Language Detection (ADR-0005)

The language detection fallback chain from ADR-0005 changes:

**Before** (alias-based): Detection finds `"rmd"` is not a registered parser, looks up the alias map, discovers it maps to `"markdown"`, and uses the markdown parser.

**After** (base-based): Detection finds `"rmd"` directly in the languages config. The base chain resolves its parser (finding `markdown.so`), and the parser is registered under `"rmd"`.

The key difference: with `base`, the language `rmd` is a first-class entry in the config, not a reverse lookup in an alias map. Detection finds `rmd` directly, and the base chain handles parser resolution. This also applies to injection language resolution — injected language identifiers are looked up directly in the config, and the base chain handles parser fallback.

Syntect-based token normalization (e.g., `py` -> `python`) remains unchanged — it operates before the base chain.

### Removed: `aliases` Field

The `aliases` field is removed from language configuration. This is a breaking change. The project is in beta (per CLAUDE.md), so this is acceptable.

**Migration**: Each `aliases = ["x", "y"]` on language `L` becomes:

```toml
[languages.x]
base = "L"

[languages.y]
base = "L"
```

**Key behavioral differences from aliases:**

- **Language identity**: With aliases, `rmd` was transparent — only `markdown` appeared in the language registry. With `base`, `rmd` becomes a first-class config entry.
- **Config inheritance**: With aliases, derived languages shared the parent's entire config. With `base`, derived languages inherit the parent's config via the base chain, and can selectively override any field (bridge settings, parser, queries).
- **Bridge config**: Bridge settings defined on the base language are inherited automatically by derived languages (via base chain merging). No need to redeclare them unless overriding.

## Consequences

### Positive

- **Per-language customization**: Each derived language (`rmd`, `qmd`) can override bridge settings, queries, or parser independently
- **No conflict**: Each language name is unique in the config map — no duplicate alias ambiguity
- **Correct ownership**: Derived languages declare their own parent, not the other way around
- **Composable**: Multi-level chains enable layered customization (`rmd -> markdown_custom -> markdown -> _`)
- **Consistent with ADR-0011**: Extends the wildcard pattern naturally — `_` is just the default base
- **Simpler coordinator**: No reverse alias map to maintain; language names are direct keys

### Negative

- **Breaking change**: Existing `aliases` configurations must be migrated
- **Circular reference risk**: Must detect and report cycles in the base chain
- **Longer resolution chains**: Multi-level chains add resolution complexity at access time
- **Parser symbol name coupling**: Loading a base language's parser requires knowing the base language name for the symbol lookup
- **Verbose for pure aliases**: `[languages.rmd]\nbase = "markdown"` is more verbose than `aliases = ["rmd"]` when no customization is needed

### Neutral

- **`_` is not special-cased**: It terminates the chain via `base = ""` (its default), not via name-based branching. However, `_` remains reserved as a key name (unchanged from ADR-0011)
- **`base` chain is linear**: Single parent only — no multi-inheritance complexity
- **Lazy resolution**: Chain resolved at access time, consistent with ADR-0011 wildcard pattern
- **Auto-install interaction**: When walking the chain, auto-install attempts use the language name at each level (e.g., try to auto-install `rmd`, then `markdown`)

## Related Decisions

- [ADR-0005](0005-language-detection-fallback-chain.md): Language detection fallback chain (alias resolution replaced by base resolution)
- [ADR-0010](0010-configuration-merging-strategy.md): Cross-layer configuration merging (base chain operates after layer merging)
- [ADR-0011](0011-wildcard-config-inheritance.md): Wildcard config inheritance (`base` generalizes `_` inheritance)

## Appendix: Configuration Examples

#### Basic: rmd inherits from markdown

```toml
[languages.markdown]
# markdown config (parser auto-discovered or auto-installed)

[languages.rmd]
base = "markdown"
# Uses markdown's parser and queries, but can override bridge settings

[languages.qmd]
base = "markdown"
[languages.qmd.bridge.python.aggregation]
"textDocument/completion" = { priorities = ["basedpyright"] }
```

#### Multi-level chain

```toml
[languages._]
# global defaults

[languages.markdown]
# standard markdown config

[languages.markdown_custom]
base = "markdown"
# custom markdown with extra query overrides

[languages.rmd]
base = "markdown_custom"
# rmd -> markdown_custom -> markdown -> _
```

#### Override parser but inherit queries

```toml
[languages.rmd]
base = "markdown"
parser = "/custom/path/to/rmd_parser.so"
# Uses custom parser but inherits markdown's queries
```

#### Self-contained language (no inheritance)

```toml
[languages.my_custom_lang]
base = ""
parser = "/path/to/my_parser.so"
queries = [{ path = "/path/to/highlights.scm" }]
# Does NOT inherit from "_" — fully self-contained
```

#### Circular reference (misconfiguration)

```toml
# BAD: circular reference — both languages lose wildcard defaults
[languages.a]
base = "b"

[languages.b]
base = "a"
```
