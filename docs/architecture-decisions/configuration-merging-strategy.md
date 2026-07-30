# Configuration Merging Strategy

## Context

kakehashi needs to support multiple configuration sources to accommodate different use cases:

1. **Programmed defaults**: Built-in defaults for zero-config usage
2. **User-wide defaults**: Settings that apply across all projects for a user
3. **Project-specific settings**: Configuration local to a specific project/directory
4. **Session-specific overrides**: Settings passed directly from the LSP client at initialization

The limitations of the current system are:

- Missing **User-wide defaults**
- **Project-specific settings** are only based on `./kakehashi.toml`
- Complex `captureMappings` overrides must be duplicated in each project's `kakehashi.toml`

The standard pattern in many language servers and CLI tools is layered configuration with clear precedence rules. This decision proposes adding a **user configuration layer** between programmed defaults and project config.

## Decision

**Implement a four-layer configuration system with "later sources override earlier ones" semantics.**

### Query Configuration Schema

kakehashi introduces a unified `queries` field to simplify query file configuration:

```toml
[languages.python]
queries = [
    { path = "/usr/share/python/highlights.scm" },
    { path = "/usr/share/python-bindings.scm", kind = "bindings" },
    { path = "./custom-injections.scm", kind = "injections" }
]
```

**QueryItem structure:**

| Field  | Type   | Required | Description                                      |
|--------|--------|----------|--------------------------------------------------|
| `path` | string | Yes      | Path to the `.scm` query file                    |
| `kind` | string | No       | Query type: `"highlights"`, `"bindings"`, `"injections"` |

**Type inference rules (when `kind` is omitted):**

1. If the filename is exactly `highlights.scm`, `bindings.scm`, or `injections.scm`, use the corresponding type
2. Otherwise, return `None` and the query item is skipped

**Examples of type inference:**

| Path                              | Inferred `kind` |
|-----------------------------------|-----------------|
| `/usr/share/python/highlights.scm` | `highlights`    |
| `./queries/highlights.scm`         | `highlights`    |
| `/usr/share/python/bindings.scm`     | `bindings`        |
| `./my-custom/injections.scm`       | `injections`    |
| `./python-highlights.scm`          | (skipped)       |
| `./custom-queries.scm`             | (skipped)       |

### Configuration Sources (Lowest to Highest Precedence)

1. **Programmed defaults** (lowest precedence)
   - Source: `src/config.rs` (`default_search_paths()`, implicit `autoInstall: true`)
   - Purpose: Sensible out-of-the-box behavior; enables zero-config experience

2. **User configuration file**
   - Location: `$XDG_CONFIG_HOME/kakehashi/kakehashi.toml`
   - Falls back to `~/.config/kakehashi/kakehashi.toml` on most Unix systems
   - Purpose: User-wide defaults (e.g., default `searchPaths`, global `captureMappings` overrides)

3. **Project configuration file**
   - Location: `./kakehashi.toml` in workspace root (loaded via `load_toml_settings()`)
   - `--config-file` CLI option to specify alternative path(s)
   - Purpose: Project-specific settings, version-controlled with the project

4. **Session-specific overrides** (highest precedence)
   - Sources:
     - `initializationOptions` in the LSP `initialize` request (at startup)
     - `workspace/didChangeConfiguration` notification (at runtime)
   - Purpose: Per-session overrides from the editor/client configuration
   - Note: Runtime changes via `didChangeConfiguration` re-trigger the merge process

### Path Anchoring Precedes the Merge

Relative path fields (`searchPaths`, `languages[*].parser`,
`languages[*].queries[*].path`) are rewritten to sit under their source layer's
directory *before* the layers are merged, by
`crate::config::expand::anchor_settings_paths`. Doing it afterwards is not
possible: the merge replaces path fields wholesale, so a surviving `./queries`
no longer records which file asked for it, and the only base left is the server
process's working directory — which for an editor-spawned server belongs to
whoever launched the editor.

Bases per layer: a config file uses its own directory (each `--config-file`
layer its own), `initializationOptions` and `didChangeConfiguration` use the
initialized workspace root, and the programmed defaults have no base.

Anchoring is **syntactic and does not expand**. A value beginning with `/`, `~`,
or `$` is left as written, so `WorkspaceSettings::try_from_settings` remains the
only place expansion happens. That single-pass property is load-bearing in three
ways: the `$$` literal-dollar escape cannot be consumed twice, the cross-field
invariant checks that live in `try_from_settings` cannot be bypassed by a second
conversion entry point, and the defaults' `${KAKEHASHI_DATA_DIR}` template
survives into the raw settings that `kakehashi/internal/effectiveConfiguration`
reports.

Because each layer is anchored while raw, a language `base` chain that spans
layers inherits values that are *already* absolute — `resolve_base_configs`
folds the chain later, so an inherited `parser` keeps the base of the layer that
wrote it rather than the layer that inherits it.

### Merge Algorithm

Layers are merged pairwise via `merge_workspace_settings` using `reduce` and `flatten`:

```rust
fn merge_workspace_settings(base: Option<RawWorkspaceSettings>, overlay: Option<RawWorkspaceSettings>) -> Option<RawWorkspaceSettings>
```

Configs are applied in order (earlier = lower precedence, later = higher precedence):

```
final_config = [defaults, user_config, project_config, InitializationOptions]
    .into_iter().reduce(merge_workspace_settings).flatten()
```

**Scalar values and Option types** (`searchPaths`, `autoInstall`):
- Later sources completely replace earlier values (via `overlay.or(base)`)
- Example: `autoInstall: false` in InitializationOptions overrides `autoInstall: true` from project config

**Exception — key specificity can outrank source order.** The top-level
`autoInstall` is deprecated in favour of `languages.<lang>.autoInstall` (and
`languages._.autoInstall`), and `WorkspaceSettings::auto_install_for` consults the
per-language keys first *regardless of which source supplied them*. So a
`[languages._] autoInstall = true` in project config wins over an
`autoInstall: false` from InitializationOptions — the merge above still runs
normally per key, but the two keys are then read in specificity order, not source
order. This is why `default_settings()` (merge layer 1) and the `config init`
template both leave `languages._.autoInstall` unset: a built-in value there would
silently shadow every user-supplied top-level opt-out.

**Languages HashMap** (`languages`):
- **Deep merge at language level**: Keys from later sources override same keys from earlier sources
- **Deep merge within each language**: Individual fields (`parser`, `queries`, `bridge`, etc.) are merged
- The `queries` array is **replaced entirely**, not concatenated
- Example:
  ```toml
  # user config
  [languages.python]
  parser = "/usr/lib/python.so"
  queries = [
      { path = "/usr/share/python/highlights.scm" },
      { path = "/usr/share/python-bindings.scm", kind = "bindings" }
  ]
  bridge = { rust = { enabled = true }, javascript = { enabled = true } }

  # project config
  [languages.python]
  queries = [
      { path = "./queries/python-highlights.scm" }  # replaces user's queries entirely
  ]
  # bridge not specified

  # final (deep merge)
  [languages.python]
  parser = "/usr/lib/python.so"                           # inherited from user
  queries = [{ path = "./queries/python-highlights.scm" }] # replaced by project (user's bindings lost!)
  bridge = { rust = { enabled = true }, javascript = { enabled = true } }  # inherited from user
  ```

  **Note**: If the project only wants to override highlights while keeping the user's bindings, it must include both:
  ```toml
  # project config (preserving user's bindings)
  [languages.python]
  queries = [
      { path = "./queries/python-highlights.scm" },
      { path = "/usr/share/python-bindings.scm", kind = "bindings" }  # must repeat user's bindings
  ]
  ```

**Bridge servers HashMap** (`languageServers`):
- **Deep merge at server level**: Keys (server names) from later sources override same keys from earlier sources
- **Deep merge within each server**: Individual fields (`cmd`, `languages`, `initializationOptions`, `settings`, `onTypeFormattingTriggers`) are merged (JSON-object fields `initializationOptions` and `settings` deep-merge; list options like `onTypeFormattingTriggers` are overlay-wins-when-present, not unioned). `settings` carries downstream workspace configuration propagated post-initialize — see downstream-settings-propagation.
- Example:
  ```toml
  # user config
  [languageServers.rust-analyzer]
  cmd = ["rust-analyzer"]
  languages = ["rust"]

  # project config
  [languageServers.rust-analyzer]
  initializationOptions = { linkedProjects = ["./Cargo.toml"] }

  # final (deep merge)
  [languageServers.rust-analyzer]
  cmd = ["rust-analyzer"]                                        # inherited
  languages = ["rust"]                                           # inherited
  initializationOptions = { linkedProjects = ["./Cargo.toml"] }  # added by project
  ```

**Capture mappings** (`captureMappings`):
- **Deep merge**: Individual capture mappings are merged per-language, per-query-type
- Later sources override specific keys while preserving unmentioned keys from earlier sources
- Example:
  ```toml
  # user config
  [captureMappings._.highlights]
  "variable.builtin" = "fallback.variable"
  "function.builtin" = "fallback.function"

  # project config
  [captureMappings._.highlights]
  "variable.builtin" = "project.variable"

  # final (deep merge)
  [captureMappings._.highlights]
  "variable.builtin" = "project.variable"  # overridden
  "function.builtin" = "fallback.function" # inherited
  ```

### File Loading Behavior

Strictness follows *how the path was chosen*, not what went wrong with it. A
path the user typed carries intent; a path kakehashi went looking for does not.

1. **Implicitly discovered files degrade quietly**
   - User config doesn't exist: proceed with empty user config
   - Project config doesn't exist: proceed with empty project config
   - Either one exists but fails to parse: warn and skip that layer
   - This is what keeps zero-config startup working: a stray or half-edited
     `kakehashi.toml` must never leave the user without a server

2. **An explicit `--config-file` that is present but unusable fails startup**
   - Unreadable, malformed TOML, larger than the 8 MiB read ceiling, or
     carrying a path that cannot be expanded
   - LSP `initialize` returns `RequestFailed` (-32803) naming the first such
     file; `format` and `diagnose` print it and exit 2
   - Silently dropping the layer and continuing on defaults is what hides
     configuration mistakes, and an explicit path is where the user is most
     entitled to be told

3. **An explicit `--config-file` that is absent is skipped**
   - Layered invocations (`--config-file base.toml --config-file overrides.toml`)
     depend on the overlay being allowed not to exist, and a relative path
     resolves against the process working directory — for an editor-spawned
     server, the editor's rather than the workspace root. Absence is too easily
     accidental to be worth refusing to start over; being unusable is not.
   - A path whose metadata cannot be read at all counts as unusable, not
     absent: `exists()` answers "no" to both, and only one of them is the
     optional-overlay case.
   - The skip is a `SettingsEvent::warning`, so an editor sees it as
     `window/logMessage`. `format` and `diagnose` show nothing: CLI mode has no
     channel for non-fatal settings events at all (the stub client pump
     discards them), which is a pre-existing gap rather than a decision here.

4. **Where each class of failure is judged**
   - Path expansion: per file, because a later layer replaces path fields
     wholesale, so the merged result would never mention an earlier layer's
     undefined variable
   - Cross-field invariants (e.g. `debounceMs` ≤ `maxWaitMs`): on the merged
     explicit configuration only, because their operands merge independently —
     one file may legitimately supply just one half
   - Unrecognised key names: reported as a warning on an explicit file, not
     fatal. Serde drops an unknown field silently, so a typo otherwise reads as
     "not specified"; but a key this version does not know may be one the next
     one does, and rejecting the file would version-lock it.
     `workspace/didChangeConfiguration` rejects the whole update instead —
     a live edit is not a file shared across versions.
   - Two known inconsistencies, both pre-existing and both worth closing
     separately: `FeatureSettings` and its children carry
     `deny_unknown_fields`, so an unknown key *inside* `features` fails typed
     deserialization and is fatal before the warning walker ever sees it; and
     CLI mode has no channel for non-fatal settings events at all, so
     `format`/`diagnose` users never see these warnings.
   - `initializationOptions`: never fatal, and judged last. A client-supplied
     override that fails to expand does not abort — but "non-fatal" only means
     the session starts: the *whole* merged configuration is discarded in
     favour of programmed defaults, explicit files included. Only the abort is
     avoided, not the loss.

5. **When the strict gate runs**
   - Before `initialize` stores anything derived from the request. Several of
     those stores are first-write-wins and `tower-lsp-server` accepts a retry
     after an error response, so a client that fixes the file and re-sends
     `initialize` would otherwise get the corrected settings alongside the
     failed attempt's capabilities and workspace folders.
   - Each `--config-file` is read exactly once and the result carried into the
     merge: a file swapped between two reads would slip past whichever check
     ran first. Reads are bounded, so a path naming an endless source fails
     instead of exhausting memory; a path naming a stream with no writer still
     blocks, which is the failure mode of the path the user chose.

### Implementation Notes

**Config loading order:**

`--config-file` is not an alternative *path* for the project layer — it replaces
the whole implicit pair, and accepts any number of files that merge in flag
order. It is also the only layer with a verdict of its own, reached before
`initialize` stores anything, so the files are read once and carried into the
merge rather than re-read:

```rust
// `initialize`, before any request-derived state is stored:
let explicit = load_explicit_config(home, env_fn);   // None when the flag is absent
if let Some(error) = explicit.as_ref().and_then(|c| c.fatal_error.clone()) {
    return Err(configuration_load_error(error));
}

fn load_settings(root, override_settings, home, env_fn, explicit) -> SettingsLoadOutcome {
    let defaults = Some(default_settings());          // src/config/defaults.rs
    let files = match explicit {
        Some(explicit) => explicit.layers,            // --config-file, in order
        None => vec![load_user_config(), load_project_config(root)],
    };
    // initializationOptions merge last, and are never fatal
}
```

**XDG Base Directory compliance:**
- Use `$XDG_CONFIG_HOME` if set
- Fall back to `$HOME/.config` otherwise
- Consider using the `dirs` or `directories` crate for cross-platform support

## Consequences

### Positive

- **Layered flexibility**: Users can set sensible defaults globally while projects customize as needed
- **Editor-agnostic defaults**: User config works regardless of which editor/client is used
- **Version control friendly**: Project configs can be committed to repos
- **Zero-config still works**: All layers are optional; empty config results in auto-install behavior
- **Precedence is intuitive**: "Closer to the action" = higher priority (session > project > user)
- **Unified queries format**: Single `queries` field with type inference reduces config verbosity
- **Self-documenting paths**: Filenames like `highlights.scm` convey intent without explicit `kind`

### Negative

- **Complexity increase**: Four config sources to understand and debug
- **Arrays replace, not merge**: `queries` arrays are replaced entirely, not concatenated; overriding one query type requires repeating all
- **No "unset" mechanism**: Cannot explicitly remove a field inherited from earlier layers (would need `null` support)
- **File I/O at startup**: Reading the config files adds latency (minimal in practice) — two implicit files, or however many `--config-file` arguments were given
- **Infrastructure-integration gap**: Phases 1-3 (Sprints 118-120) built infrastructure (schema, merging, user config loading) but delivered ZERO user value until Sprint 124 wired APIs into application. Lesson: infrastructure sprints must be followed by integration sprints within 1-2 sprints to realize value.

### Neutral

- **TOML format**: Consistent with project config; JSON would work but TOML is more readable for humans
- **XDG compliance**: Standard for Unix tools; Windows path handling needs separate consideration
- **Future extensibility**: Additional layers (e.g., workspace-level) could be added with same merge rules

## Implementation Phases

**Overall Progress**: Phases 1-3 completed. Core configuration loading infrastructure is in place. Remaining work: CLI options and end-to-end testing.

### Phase 1: Query Configuration Schema (Completed - Sprint 118, PBI-151)
- [x] Add `QueryItem` struct with `path` (required) and `kind` (optional) fields
- [x] Add `queries: Option<Vec<QueryItem>>` field to `LanguageSettings`
- [x] Implement `QueryKind` enum (`Highlights`, `Bindings`, `Injections`) with default `Highlights`
- [x] Implement type inference from exact filename (`highlights.scm`, `bindings.scm`, `injections.scm`)

### Phase 2: Core Merging (Completed - Sprint 119, PBI-150)
- [x] Implement `merge_workspace_settings()` function for layered config merging
- [x] Deep merge for `languages` HashMap
- [x] Deep merge for `languageServers` HashMap
- [x] Deep merge for `captureMappings`

### Phase 3: User Configuration File (Completed - Sprint 120, PBI-149)
- [x] XDG Base Directory compliance for config path
- [x] Load user config from `$XDG_CONFIG_HOME/kakehashi/kakehashi.toml`
- [x] Silent ignore for missing user config file

### Phase 4: Project Configuration (Completed)
- [x] Load project config from `./kakehashi.toml`
- [x] `--config-file` CLI option for alternative path(s)
- [x] Fail startup on an explicit file that is present but unusable; skip an
      absent one with a warning

### Phase 5: Testing
- [ ] Unit tests for `QueryItem` parsing and type inference
- [ ] Unit tests for `merge()` function covering all value types
- [ ] Integration tests loading actual files from XDG and project paths
- [ ] E2E Neovim tests verifying InitializationOptions override file-based config

## Alternatives Considered

### 1. Shallow merge for `languages` HashMap (current implementation)
- Pro: Simple to implement and understand
- Con: Users must repeat all fields when overriding a single field (e.g., must specify `parser` again just to change `queries`)
- Con: Less intuitive — users expect inheritance
- Decision: **Change to deep merge** for `languages` to match `captureMappings` behavior; arrays within language config (e.g., `queries`) are replaced, not merged

### 2. Prepend arrays instead of replace
- Pro: Allow extending `searchPaths` from earlier layers
- Con: Current `primary.or(fallback)` is simpler and predictable
- Con: Users can manually include default paths if they want extension
- Decision: Keep current replace behavior for simplicity

### 3. Single config file with includes
- Pro: Simpler loading logic
- Con: Requires inventing include syntax; less conventional
- Decision: Rejected; layered files are standard in the ecosystem

### 4. Environment variable overrides
- Pro: Easy CI/CD integration
- Con: Not useful for complex settings like `languages` config
- Decision: Deferred; could be added later for specific scalar settings like `autoInstall`

### 5. Keep separate `highlights`, `locals`, `injections` fields
- Pro: Explicit, no type inference needed
- Pro: No new data structure to learn
- Con: Verbose configuration—three separate arrays to manage
- Con: Adding new query types (e.g., `folds`, `indents`) requires schema changes
- Decision: **Rejected**; use unified `queries` field with type inference instead

### 6. Merge queries per-kind instead of replacing entire array
- Pro: Override only highlights while inheriting locals from user config
- Con: Significantly more complex merge logic
- Decision: Keep simple array replacement; users can use wildcard-config-inheritance wildcard inheritance for shared queries

## Related Decisions

- [wildcard-config-inheritance](wildcard-config-inheritance.md): Wildcard inheritance within a single config layer
