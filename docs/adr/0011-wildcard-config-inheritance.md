# ADR-0011: Wildcard Config Inheritance

## Status

Implemented (Sprints 121-123)

## Context

ADR-0010 defines how configuration merges across layers (user → project → InitializationOptions). However, there's another dimension of merging: **within a single config**, the `_` (wildcard) key serves as defaults that should be inherited by specific entries.

Currently in `captureMappings`:
- `captureMappings._` defines default capture-to-token mappings for all languages
- `captureMappings.rust` can define rust-specific overrides

The expected behavior is that `rust` inherits all mappings from `_` and only overrides what it explicitly specifies. This is similar to how CSS cascading works with wildcards.

This pattern could also apply to other HashMaps:
- `languages._` could define default language settings (e.g., default `bridge` filter)
- `languageServers._` could define default server settings

## Decision

**The `_` key in HashMaps serves as a wildcard that provides defaults inherited by all specific keys.**

### Inheritance Rules

When resolving configuration for a specific key (e.g., `rust`):

```
effective_config[rust] = merge(config["_"], config["rust"])
```

1. Start with `_` (wildcard) values as the base
2. Override with specific key values
3. Missing specific key → use `_` entirely
4. Missing `_` → use specific key entirely
5. Both missing → no config for that key

### Application Order

Wildcard inheritance happens **after** cross-layer merging (ADR-0010):

```
1. Merge across layers:    final_config = merge_all([user, project, InitializationOptions])
2. Resolve wildcards:      effective[rust] = merge(final_config["_"], final_config["rust"])
```

This means:
- Layer merging produces a single `captureMappings` with both `_` and language-specific entries
- At resolution time, `_` is applied as defaults for each language

### Affected HashMaps

| HashMap | Wildcard Key | Use Case |
|---------|--------------|----------|
| `captureMappings` | `_` | Default capture-to-token mappings for all languages |
| `languages` | `_` | Default language settings (see nested wildcards below) |
| `languages.{lang}.bridge` | `_` | Default bridge settings for all injection targets |
| `languageServers` | `_` | Default server settings |

### Nested Wildcards

Wildcards can be nested when HashMaps contain other HashMaps. Resolution applies recursively:

```
effective[python].bridge[rust] = merge(
    merge(languages["_"], languages["python"]).bridge["_"],
    merge(languages["_"], languages["python"]).bridge["rust"]
)
```

**Resolution order:**
1. Resolve outer wildcard: `languages._` → `languages.python`
2. Resolve inner wildcard: `bridge._` → `bridge.rust`

### Example: languages._.bridge._ (Nested Wildcards)

```toml
# Default bridge settings for ALL languages and ALL injection targets
[languages._.bridge._]
enabled = true
isolation = true

# Python-specific: disable bridging to JavaScript
[languages.python.bridge.javascript]
enabled = false

# Effective config for languages.python.bridge.rust:
# enabled = true    # inherited from languages._.bridge._
# isolation = true   # inherited from languages._.bridge._

# Effective config for languages.python.bridge.javascript:
# enabled = false   # overridden by python-specific setting
# isolation = true   # inherited from languages._.bridge._
```

### Example: captureMappings

```toml
[captureMappings._.highlights]
"variable" = "variable"
"variable.builtin" = "variable.defaultLibrary"
"function" = "function"
"function.builtin" = "function.defaultLibrary"

[captureMappings.rust.highlights]
"type.builtin" = "type.defaultLibrary"  # rust-specific addition

# Effective config for rust:
# "variable" = "variable"                    # inherited from _
# "variable.builtin" = "variable.defaultLibrary"  # inherited from _
# "function" = "function"                    # inherited from _
# "function.builtin" = "function.defaultLibrary"  # inherited from _
# "type.builtin" = "type.defaultLibrary"    # rust-specific
```

### Example: Overriding a wildcard value

```toml
[captureMappings._.highlights]
"comment" = "comment"

[captureMappings.lua.highlights]
"comment" = ""  # suppress comments for lua specifically

# Effective config for lua:
# "comment" = ""  # overridden (empty string suppresses the token)
```

## Consequences

### Positive

- **DRY configuration**: Define common mappings once in `_`, override only exceptions
- **Intuitive**: Matches user expectation of "wildcard = default"
- **Extensible**: Pattern can be applied to other HashMaps as needed
- **Consistent with CSS/TOML conventions**: `_` as wildcard is familiar

### Negative

- **Two-phase resolution**: Must merge layers first, then resolve wildcards
- **Complexity**: Users must understand both cross-layer and wildcard merging
- **Nested wildcards add depth**: `languages._.bridge._` requires recursive resolution
- **Order matters**: `_` must be processed before specific keys during resolution
- **Infrastructure-integration gap**: Phases 1-3 (Sprints 121-123) built wildcard resolution APIs but delivered ZERO user value until Sprint 124 wired them into application lookups. Lesson: infrastructure sprints must be followed by integration sprints within 1-2 sprints to realize value.

### Neutral

- **`_` is reserved**: Cannot use `_` as an actual language/server name
- **Explicit over implicit**: Language must be listed to get non-wildcard config
- **Lazy resolution**: Wildcards resolved at access time, not at load time

## Implementation Notes

### Two Wildcard Resolution Strategies

During PBI-152 (Sprint 121), we discovered two valid approaches to wildcard inheritance:

**1. Eager Merge (resolve_with_wildcard)**
```rust
fn resolve_with_wildcard<V: Merge>(
    map: &HashMap<String, V>,
    key: &str,
) -> Option<V> {
    let wildcard = map.get("_");
    let specific = map.get(key);

    match (wildcard, specific) {
        (Some(w), Some(s)) => Some(w.merge(s)),
        (Some(w), None) => Some(w.clone()),
        (None, Some(s)) => Some(s.clone()),
        (None, None) => None,
    }
}
```

**2. Lazy Fallback (resolve_capture pattern)**
```rust
fn resolve_capture(
    capture_name: &str,
    filetype: Option<&str>,
    capture_mappings: Option<&CaptureMappings>,
) -> String {
    if let Some(mappings) = capture_mappings {
        // Try filetype-specific mapping first
        if let Some(ft) = filetype
            && let Some(lang_mappings) = mappings.get(ft)
            && let Some(mapped) = lang_mappings.highlights.get(capture_name)
        {
            return mapped.clone();
        }

        // Try wildcard mapping
        if let Some(wildcard_mappings) = mappings.get("_")
            && let Some(mapped) = wildcard_mappings.highlights.get(capture_name)
        {
            return mapped.clone();
        }
    }
    capture_name.to_string()
}
```

Both approaches produce identical user-facing behavior:
- Specific values override wildcard values
- Missing specific key falls back to wildcard
- Each capture name resolved independently

**Trade-offs:**
- **Eager merge**: Creates merged config upfront, single HashMap lookup per capture
- **Lazy fallback**: Two HashMap lookups per capture (specific then wildcard), but avoids creating intermediate merged structure

The existing `resolve_capture()` in `semantic.rs` already implements lazy fallback for runtime semantic token resolution. The new `resolve_with_wildcard()` provides eager merge for configuration preprocessing when needed.

## Implementation Phases

### Phase 1: captureMappings Wildcard (Completed - Sprint 121, PBI-152)
- [x] Implement `resolve_with_wildcard()` function (commit 2c62805)
- [x] Apply wildcard resolution to `captureMappings` lookup (existing `resolve_capture()` already implements lazy fallback)
- [x] Unit tests for wildcard resolution with various combinations (3 tests: wildcard-only, merge, override)

### Phase 2: languages Wildcard (Completed - Sprint 122, PBI-153)
- [x] Apply wildcard resolution to `languages._` for default language settings (commit 5e4796b)
- [x] Apply wildcard resolution to `languages.{lang}.bridge._` for default bridge settings (commit 79e3047)
- [x] Unit tests for nested wildcard resolution (outer then inner) (commit 15604de)
- [x] Unit tests for specific values overriding wildcards at both levels (commit 8a501cc)

### Phase 3: languageServers Wildcard (Completed - Sprint 123, PBI-154)
- [x] Apply wildcard resolution to `languageServers._` for default server settings (commit 4a410a8)
- [x] Unit tests for wildcard resolution with various combinations (commits 215ce9e, c5fd55b)
- [x] Integration tests verifying wildcard inheritance (commit 677465b)

## Related Decisions

- [ADR-0010](0010-configuration-merging-strategy.md): Cross-layer configuration merging strategy
