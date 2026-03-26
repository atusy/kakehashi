# ADR-0024: Base Language Inheritance

## Status

Accepted (Supersedes `aliases` field from [ADR-0005](0005-language-detection-fallback-chain.md))

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

## Decision

**Replace the `aliases` field with a `base` field that enables single-parent configuration inheritance with chain resolution.**

### Resolution Order (Three Phases)

Configuration resolution proceeds in three phases. Each phase depends on the output of the previous one, so this ordering is mandatory:

1. **Layer merge** (ADR-0010): programmed defaults → user config → project config → `initializationOptions`. Produces a single merged config per language, including `base` field values.
2. **Base chain resolution** (this ADR): walk the `base` chain across languages, merge most-specific-wins. This merges configs *across languages* (vertical), whereas Phase 1 merges *within each language across layers* (horizontal). Merge order (lowest → highest priority, shown with `←`): `_ ← markdown ← rmd`. For multi-level chains: `_ ← markdown ← markdown_custom ← rmd`. Produces the effective config for each language.
3. **Nested wildcard resolution** (ADR-0011): resolve `_` keys within sub-dictionaries of the effective config (e.g., `bridge._`). Operates on the output of Phase 2.

### The `base` Field

The `base` field participates in cross-layer merging (Phase 1) like any other `LanguageSettings` field: a later layer's `base` value overrides an earlier layer's value. If a layer defines `[languages.rmd]` without specifying `base`, the `base` from a lower-priority layer survives the merge (absent fields are preserved per ADR-0010).

Every language config gains an optional `base` field (default: not set, implicitly `"_"`):

| `base` value | Meaning | Typical use |
|---|---|---|
| `None` (omitted) | Inherit from `_` | Most languages |
| `"_"` | Explicitly inherit from `_` | Equivalent to `None`; for clarity |
| `"markdown"` etc. | Inherit from named language | Derived languages (`rmd`, `qmd`) |
| Same as own name | **Self-reference** — chain terminates here | `_` itself; self-contained languages |

- **`_` defaults to `base = "_"`** (self-reference) — it is the root of all chains and does not inherit from anything. Self-reference is the uniform termination condition: the chain stops when a language's `base` equals its own name. Users may override `_`'s `base` (e.g., `base = "some_language"`), but this can create unexpected inheritance chains.

### Chain Termination and Error Handling

The **primary** termination condition is **self-reference** (`base == own name`). When `base` is `None`, the chain does not terminate — it continues by resolving `None` to `"_"`, which itself has `base = "_"` (self-reference) by default, terminating the chain there. There is no special case for `_`; it terminates simply because its `base` equals its own name.

| Condition | Behavior |
|---|---|
| **Self-reference** (`a.base = "a"`) | Normal termination. The chain stops here. This is not an error — it is the standard way to declare a chain root. |
| **Undefined base language** | Not an error. The chain continues through it toward `_`, and parser/query resolution still searches `searchPaths` using that language's name. This is the normal case for languages discovered via `searchPaths` without explicit config. |
| **Circular reference** (`a.base = "b"`, `b.base = "a"`) | Detected and terminated at the cycle point. `_` is **not** appended — the language loses wildcard defaults. The misconfiguration is reported to the user. |

### How Phase 2 Subsumes ADR-0011's Outer Wildcard

Base chain resolution (Phase 2) **replaces** ADR-0011's outer `languages._` wildcard resolution. Previously, ADR-0011 unconditionally merged `_` with every language. With base chains, `_` is already the root of every default chain (since chains terminate at self-reference), producing the same result. Additionally, base chains introduce a new capability: a language can **opt out** of `_` inheritance by setting `base` to its own name (self-reference), which was not possible under ADR-0011's unconditional merge.

Note: self-reference on an intermediate language (e.g., `markdown.base = "markdown"`) intentionally prevents `_` inheritance for all languages derived from it — derived languages respect that decision.

### Nested Wildcard Resolution (Phase 3)

Phase 3 applies only to **nested wildcards** — `_` keys within sub-dictionaries of the effective config (e.g., `bridge._`). After Phase 2 produces the effective config for a language, any nested `_` entries within that config are merged with their sibling entries, with the specific sibling overriding the wildcard.

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

The `aliases` field is removed from language configuration with/without a deprecation period. Breaking changes can be introduced without deprecation periods until the product releases v1.0.

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

- **No conflict**: Each language name is unique in the config map — no duplicate alias ambiguity
- **Composable**: Multi-level chains enable layered customization (`rmd -> markdown_custom -> markdown -> _`)
- **Consistent with ADR-0011**: Extends the wildcard pattern naturally — `_` is just the default base
- **Simpler coordinator**: No reverse alias map to maintain; language names are direct keys
- **Opt-out capability**: Languages can use self-reference (`base = "<own name>"`) to avoid inheriting from `_`, which was not possible under ADR-0011's unconditional merge

### Negative

- **Silent breaking change**: Existing `aliases` fields are silently ignored — languages that relied on aliases will stop working without warning. Users must migrate to `base` declarations.
- **Longer resolution chains**: Multi-level chains add resolution complexity and make it harder to debug "where did this config value come from?"
- **Parser symbol name coupling**: Loading a base language's parser requires knowing the base language name for the `tree_sitter_<lang>` symbol lookup in the shared library
- **Verbose for pure aliases**: `[languages.rmd]\nbase = "markdown"` is more verbose than `aliases = ["rmd"]` when no customization is needed
- **Cumulative resolution complexity**: This adds a third resolution phase (after ADR-0010 layer merge and before ADR-0011 nested wildcards), increasing the total number of phases a reader must understand to predict effective config

### Neutral

- **`_` is not special-cased**: It terminates the chain via self-reference (`base = "_"`), the same mechanism any language can use. However, `_` remains reserved as a key name (unchanged from ADR-0011)
- **Auto-install chain walking**: When walking the chain, auto-install attempts use the language name at each level (e.g., try to auto-install `rmd`, then `markdown`), which may add latency for deeply nested chains

## Alternatives Considered

### 1. Keep `aliases` alongside `base`

- Pro: Non-breaking; `aliases` remains as syntactic sugar for the common case (pure identity mapping)
- Pro: Less verbose when no customization is needed
- Con: Two mechanisms for the same concept; users must learn when to use which
- Con: Alias conflicts (the original problem #1) remain for languages still using `aliases`
- Decision: **Rejected**; a single mechanism is simpler to reason about, and the project is in beta so breaking changes are acceptable

### 2. Multi-inheritance (`base = ["markdown", "r"]`)

- Pro: Languages with multiple conceptual parents (e.g., R Markdown is both Markdown and R) could inherit from both
- Pro: More expressive for complex language relationships
- Con: Significantly more complex merge semantics — field conflicts between parents require a resolution strategy (C3 linearization, explicit priority, etc.)
- Con: Harder to debug; "where did this config value come from?" becomes a diamond-problem question
- Con: No concrete use case requires it — bridge config already handles the "embedded language" dimension separately
- Decision: **Rejected**; single inheritance with bridge config covers all current use cases without the complexity

### 3. Value-based chain termination (`base = ""`)

- Pro: Simple sentinel value — empty string clearly means "stop"
- Pro: Does not require knowing the language's own name
- Con: Introduces a magic value (`""`) that is not a valid language name
- Con: Less discoverable — users must know the empty string convention
- Con: Conflicts with `None` semantics — both `""` and `None` could mean "no base"
- Decision: **Rejected** in favor of self-reference termination; self-reference (`base = "<own name>"`) is uniform, self-documenting, and enables both opt-out and custom root templates without magic values

### 4. Hard error on circular references

- Pro: Fail-fast; misconfiguration is immediately visible
- Con: A single circular reference in one language would prevent the entire server from starting
- Con: Disproportionate impact — other languages unrelated to the cycle would be affected
- Decision: **Rejected**; detect and report the cycle, but allow unaffected languages to function normally

## Related Decisions

- [ADR-0005](0005-language-detection-fallback-chain.md): Language detection fallback chain (alias resolution replaced by base resolution)
- [ADR-0010](0010-configuration-merging-strategy.md): Cross-layer configuration merging (base chain operates after layer merging)
- [ADR-0011](0011-wildcard-config-inheritance.md): Wildcard config inheritance (`base` generalizes `_` inheritance)

## Appendix: Configuration Examples

### Multi-level chain

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

### Self-contained language (no inheritance)

```toml
[languages.my_custom_lang]
base = "my_custom_lang"
parser = "/path/to/my_parser.so"
queries = [{ path = "/path/to/highlights.scm" }]
# Self-reference terminates the chain — does NOT inherit from "_"
```
