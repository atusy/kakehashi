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

## Decision

**Replace the `aliases` field with a `base` field that enables single-parent configuration inheritance with chain resolution.**

### The `base` Field

Every language config gains an optional `base` field:

```rust
pub struct LanguageConfig {
    pub base: Option<String>,  // default: None (implicitly "_")
    // ... existing fields
}
```

| `base` value | Meaning | Typical use |
|---|---|---|
| `None` (omitted) | Inherit from `_` | Most languages |
| `""` (empty string) | **No inheritance** — chain stops here | `_` itself; self-contained languages |
| `"markdown"` etc. | Inherit from named language | Derived languages (`rmd`, `qmd`) |

- When `base` is `None` (or omitted), the language inherits from `_` (wildcard), preserving current ADR-0011 behavior.
- When `base` is `""` (empty string), the language has **no base** — it does not inherit from `_` or any other language. This is useful for fully self-contained language configs that should not pick up wildcard defaults.
- When `base` is explicitly set to a non-empty value (e.g., `"markdown"`), the language inherits from that language instead of directly from `_`.
- **`_` defaults to `base = ""`** — it is the root of all chains and does not inherit from anything. This means `_` is not special-cased in code; it simply has `base = ""` as its default, and the uniform termination rule is "stop when `base == ""`".
- The chain always terminates at `base = ""`: `rmd -> markdown -> _` (where `_` has `base = ""`).

### Resolution Chain

Configuration for a language is resolved by walking the `base` chain and merging (most specific wins):

```
effective_config[rmd] = merge_chain([
    config["_"],        // wildcard defaults (base of chain)
    config["markdown"], // rmd's base
    config["rmd"],      // most specific
])
```

For multi-level chains (`rmd -> markdown_custom -> markdown -> _`):

```
effective_config[rmd] = merge_chain([
    config["_"],
    config["markdown"],
    config["markdown_custom"],
    config["rmd"],
])
```

Later entries in the chain override earlier entries, consistent with ADR-0011's "specific overrides wildcard" semantics.

### Circular Reference Detection

The chain terminates when:
- `base` is `""` — no inheritance, chain stops here (this is the **only** termination condition)
- `base` is `None` — implicitly resolves to `"_"`, so the chain continues to `_` (which has `base = ""` by default)

There is no special case for `_` — it terminates the chain simply because its default `base` is `""`.

Circular `base` chains (e.g., `a.base = "b"`, `b.base = "a"`) are detected at resolution time using a visited set, following the same pattern as query `; inherits:` resolution. A circular chain is treated as an error (logged as warning, chain terminates at the cycle point).

### Nested Wildcard Resolution

The `base` chain resolution generalizes ADR-0011's wildcard resolution for `languages`. After the chain produces an effective config, inner wildcards (e.g., `bridge._`) are still resolved per ADR-0011:

```
effective_config[rmd].bridge[python] = merge(
    effective_config[rmd].bridge["_"],
    effective_config[rmd].bridge["python"]
)
```

### Parser Fallback Chain

When loading a parser for a language, the `base` chain determines the search order:

```
load_parser("rmd"):
    1. config["rmd"].parser           -- explicit parser path in rmd config
    2. searchPaths/parser/rmd.{so,dylib,dll}  -- search paths with rmd name
    3. config["markdown"].parser      -- explicit parser path in base config
    4. searchPaths/parser/markdown.{so,dylib,dll}  -- search paths with base name
    5. (continue up the chain until "_")
```

**Critical**: When loading a parser library from a base language (e.g., `markdown.so` for `rmd`), the **symbol name** must use the base language name (`tree_sitter_markdown`), not the derived name. The existing `load_language(path, lang_name)` API already supports this separation.

**Registration**: The loaded parser is registered under the derived language name (`rmd`), pointing to the same `Language` grammar object. This ensures all lookups by language name work correctly.

### Query Fallback Chain

Query files (highlights.scm, locals.scm, injections.scm) follow the same chain:

```
load_queries("rmd"):
    1. config["rmd"].queries          -- explicit query paths in rmd config
    2. searchPaths/queries/rmd/*.scm  -- search paths with rmd name
    3. config["markdown"].queries     -- explicit query paths in base config
    4. searchPaths/queries/markdown/*.scm  -- search paths with base name
    5. (continue up the chain)
```

The first level in the chain that provides queries wins (queries are replaced entirely, not merged, per ADR-0010).

### Impact on Language Detection (ADR-0005)

The language detection fallback chain from ADR-0005 changes:

**Before** (alias-based):
```
try_with_alias_fallback("rmd"):
    1. Is "rmd" a registered parser? -> No
    2. Is "rmd" an alias? -> Yes, maps to "markdown"
    3. Is "markdown" a registered parser? -> Yes, use it
```

**After** (base-based):
```
try_with_base_fallback("rmd"):
    1. Is "rmd" in languages config? -> Yes
    2. Load parser for "rmd" via base chain -> finds markdown.so
    3. Register as "rmd" parser -> use it
```

The key difference: with `base`, the language `rmd` is a first-class entry in the config, not a reverse lookup in an alias map. Detection finds `rmd` directly, and the base chain handles parser resolution.

Syntect-based token normalization (e.g., `py` -> `python`) remains unchanged — it operates before the base chain.

### Configuration Examples

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

### Removed: `aliases` Field

The `aliases` field is removed from `LanguageConfig` and `LanguageSettings`. The `alias_map`, `build_alias_map()`, and `resolve_alias()` in `LanguageCoordinator` are also removed.

**Migration**: Each `aliases = ["x", "y"]` on language `L` becomes:

```toml
[languages.x]
base = "L"

[languages.y]
base = "L"
```

This is a breaking change. The project is in beta (per CLAUDE.md), so this is acceptable.

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
- **Circular reference risk**: Must validate or detect cycles (mitigated by visited-set detection)
- **Longer resolution chains**: Multi-level chains add resolution complexity, though cached at load time
- **Parser symbol name coupling**: Loading a base language's `.so` requires knowing the base language name for the symbol lookup
- **Verbose for pure aliases**: `[languages.rmd]\nbase = "markdown"` is more verbose than `aliases = ["rmd"]` when no customization is needed

### Neutral

- **`_` is not special-cased in code**: It terminates the chain via `base = ""` (its default), not via name-based branching. However, `_` remains reserved as a key name (unchanged from ADR-0011)
- **`base` chain is linear**: Single parent only — no multi-inheritance complexity
- **Lazy resolution**: Chain resolved at access time, consistent with ADR-0011 wildcard pattern
- **Auto-install interaction**: When walking the chain, auto-install attempts use the language name at each level (e.g., try to auto-install `rmd`, then `markdown`)

## Related Decisions

- [ADR-0005](0005-language-detection-fallback-chain.md): Language detection fallback chain (alias resolution replaced by base resolution)
- [ADR-0010](0010-configuration-merging-strategy.md): Cross-layer configuration merging (base chain operates after layer merging)
- [ADR-0011](0011-wildcard-config-inheritance.md): Wildcard config inheritance (`base` generalizes `_` inheritance)
