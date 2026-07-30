# kakehashi Documentation

kakehashi is a Language Server Protocol (LSP) server that uses Tree-sitter for fast, accurate parsing. It provides semantic highlighting and selection ranges for any language with a Tree-sitter grammar, and can bridge embedded regions to language-specific LSP servers for richer editor features.

## Features

### Semantic Tokens (Syntax Highlighting)

Provides LSP semantic tokens based on Tree-sitter `highlights.scm` queries. Works with any editor that supports LSP semantic tokens.

- Supports language injection (e.g., SQL in JavaScript template strings, code blocks in Markdown)
- Uses nvim-treesitter query files for compatibility
- Supports query inheritance (e.g., TypeScript inherits from `ecma`)

### Selection Range

Expand/shrink selection based on AST structure. Select increasingly larger syntax nodes with each invocation.

### LSP Bridge

Bridge embedded regions to language-specific servers. For example, get Python completions and hover documentation inside Markdown code blocks.

Current bridge-backed requests include:
- Completion
- Signature Help
- Go to Definition / Type Definition / Implementation / Declaration
- Hover
- Find References
- Rename / Prepare Rename
- Document Highlight / Document Symbol / Document Link
- Moniker / Inlay Hint
- Code Lens (incl. `codeLens/resolve` routed back to the origin server for injection-layer lenses — host-layer lenses pass through unrouted; resolution fails soft when the region was moved or invalidated since the lens was produced, and always in `#offset!`-adjusted regions such as frontmatter)
- Code Action (incl. `codeAction/resolve` routed back to the origin server, host-layer actions via `bridge._self`, and a merged menu across every injection region a multi-fence range overlaps; advertised only to clients with `codeActionLiteralSupport`)
- `workspace/executeCommand` (commands surfaced through bridged actions route back to their origin server by name; palette-fired commands — those that a downstream advertised in its initialize result — route via dynamically registered names when the client supports dynamic registration (a downstream's own later dynamic command registrations are not routed). Known limitations: action-embedded command names are per-document encoded and never registered, so clients that only dispatch command ids from registered lists — VS Code's vscode-languageclient — show such actions without running their command; and the palette registry is session-global by raw command id, so a name advertised by several servers routes to the latest advertiser)
- `workspace/applyEdit` from downstream servers (virtual-document edits are translated to the host document and relayed to the editor; untranslatable edits answer `applied: false`)
- Pull Diagnostics
- On Type Formatting (config-driven; see `onTypeFormattingTriggers`)

**Limitations:**
- **No cross-region results within the host document**: on the goto/references/rename transforms, a result addressed to a *different* region's virtual URI is filtered out (that URI would be meaningless to the editor; document-link targets are the exception — they pass through untouched). A code action touching another region keeps a visible-but-disabled entry for `disabledSupport` clients (payload stripped); without that capability it is dropped from the initial response, or returned unresolved on `codeAction/resolve` (a response cannot be dropped). Results in real files — an external definition, a cross-file rename edit — pass through unchanged; for navigation/references/rename, host-URI results are not containment-checked (injection-layer code actions, and applyEdit requests that also touch a virtual document, constrain host-URI edits to the region).

See [Bridge Configuration](#bridge-configuration) for setup instructions.

## Prerequisites

kakehashi automatically compiles Tree-sitter parsers from source, which requires these external tools:

### Required Dependencies

| Dependency | Purpose | Installation |
|------------|---------|--------------|
| **C Compiler** | Compiles parser grammars into shared libraries | See platform-specific instructions |

### Optional Dependencies

| Dependency | Purpose | Installation |
|------------|---------|--------------|
| **Git** | Fallback for non-GitHub parser repositories | Usually pre-installed |

Parser source code is downloaded via HTTP from GitHub archives. Git is only needed as a fallback for parsers hosted outside GitHub.

### C Compiler Installation

| Platform | Command |
|----------|---------|
| **macOS** | `xcode-select --install` |
| **Debian/Ubuntu** | `sudo apt install build-essential` |
| **Fedora/RHEL** | `sudo dnf install gcc` |
| **Arch Linux** | `sudo pacman -S base-devel` |
| **Windows** | Install [Visual Studio Build Tools](https://visualstudio.microsoft.com/visual-cpp-build-tools/) |

### Verifying Installation

```bash
# Check C compiler
cc --version  # or gcc --version / clang --version
```

If any command fails, install the missing dependency before using kakehashi.

## Zero-Configuration Usage

kakehashi works out of the box with no configuration required:

1. Start the LSP server
2. Open any file with a supported language
3. The parser and queries are automatically downloaded and installed

### Default Data Directories

| Platform | Path |
|----------|------|
| Linux | `~/.local/share/kakehashi/` |
| macOS | `~/Library/Application Support/kakehashi/` |
| Windows | `%APPDATA%/kakehashi/` |

You can override the data directory by setting the `KAKEHASHI_DATA_DIR` environment variable or using the `--data-dir` global CLI flag. The precedence order is:

1. `--data-dir` global CLI flag (highest — sets `KAKEHASHI_DATA_DIR` in-process)
2. `KAKEHASHI_DATA_DIR` environment variable
3. Platform-specific default (lowest)

**Note:** Tilde (`~`) in `KAKEHASHI_DATA_DIR` is **not** expanded — use absolute paths.

Parsers are stored in `{data_dir}/parser/` and queries in `{data_dir}/queries/`.

## Configuration

Configuration is provided via LSP `initializationOptions`. All options are optional.

This section is a practical reference. For the exhaustive field list and types, see `kakehashi config schema`.

### Configuration Options

Workspace-wide client-facing policies live under top-level `features`, keyed by
the LSP method they govern. Publish scheduling is global policy with independent
state per host URI; diagnostic refresh uses one scheduler shared by all downstream
servers in the workspace:

```toml
[features."textDocument/publishDiagnostics"]
debounceMs = 100
maxWaitMs = 1000

[features."window/logMessage"]
logLevel = "info"

[features."workspace/diagnostic/refresh"]
debounceMs = 100
maxWaitMs = 1000
```

Both schedulers send the first activity after idle immediately. Later activity is
released after `debounceMs` of quiet, with `maxWaitMs` bounding when a continuous
burst must attempt a flush. If the cache changes while a publish set is being
assembled, Kakehashi discards that stale attempt and retries at a bounded rate so
it never sends an internally superseded set; therefore `maxWaitMs` bounds the
attempt, not completion under uninterrupted concurrent mutation.
Publish diagnostics keep only the latest merged set per URI, and different URIs
never delay one another. Pull diagnostics are unaffected. The values
apply to cycles admitted after a live configuration update; an active cycle keeps
the timing snapshot it started with. `debounceMs` may be zero; `maxWaitMs` must be
positive and at least `debounceMs`. Both values are limited to 86,400,000 ms
(24 hours).

`features."window/logMessage".logLevel` is one workspace-wide threshold for
both messages forwarded from downstream servers and messages emitted by
kakehashi itself. Values are `error`, `warning`, `info`, `log`, and `off`; the
default `info` forwards Error, Warning, and Info while suppressing LSP `Log` and
`Debug`. The `log` threshold passes through Log, LSP 3.18 Debug, and future
`MessageType` values. Live updates affect subsequent messages.
`window/showMessage` is never filtered by this policy.

```json
{
  "searchPaths": ["$HOME/.local/share/kakehashi", "/another/path"],
  "languages": {
    "_": {
      "autoInstall": true
    },
    "typescript": {
      "base": "ecma",
      "queries": [
        {"path": "~/queries/highlights.scm", "kind": "highlights"},
        {"path": "/path/to/custom.scm", "kind": "highlights"}
      ]
    },
    "markdown": {
      "bridge": {
        "python": {
          "aggregation": {
            "textDocument/completion": { "priorities": ["pyright"], "maxFanOut": 1 }
          }
        }
      }
    }
  },
  "languageServers": {
    "pyright": {
      "cmd": ["pyright-langserver", "--stdio"],
      "languages": ["python"],
      "initializationOptions": {
        "python": { "analysis": { "typeCheckingMode": "basic" } }
      }
    }
  },
  "captureMappings": {
    "_": {
      "highlights": {
        "variable.builtin": "variable.defaultLibrary"
      }
    }
  }
}
```

### Environment Variable Expansion

Path fields support environment variable expansion and tilde (`~`) expansion, making configurations portable across machines.

**Supported syntax:**
- `$VAR` or `${VAR}` — expands to the value of environment variable `VAR`
- `~` — expands to the user's home directory
- `$$` — produces a literal `$` (escape mechanism)

**Supported fields:**
- `searchPaths[*]`
- `languages[*].parser`
- `languages[*].queries[*].path`

**Behavior on undefined variables:** If a referenced environment variable is not defined during ordinary startup loading, the merged configuration is discarded and programmed defaults are used. A runtime `workspace/didChangeConfiguration` update is discarded while the previous settings remain active. Explicit `--config-file` inputs are stricter: an expansion failure in one of them rejects LSP initialization or makes the CLI command exit with status 2, rather than falling back to defaults. `initializationOptions` sent by the client keep the ordinary non-fatal behavior even in a `--config-file` session. The exception is `KAKEHASHI_DATA_DIR`, which automatically falls back to the platform-specific default when unset (see [Default Data Directories](#default-data-directories)).

### Option Reference

#### `searchPaths`

Array of base directories to search for parsers and queries. If not specified, uses platform-specific defaults:
- Linux: `~/.local/share/kakehashi`
- macOS: `~/Library/Application Support/kakehashi`
- Windows: `%APPDATA%/kakehashi`

**Important:** Specify base directories, not subdirectories. The resolver automatically appends `parser/` and `queries/` subdirectories.

Parsers are searched as `{searchPath}/parser/{language}.{so,dylib,dll}`.
Queries are searched as `{searchPath}/queries/{language}/{query_type}.scm`.

#### `autoInstall` (deprecated)

**Deprecated:** use [`languages[*].autoInstall`](#languagesautoinstall) instead.
The top-level key still works — it answers whenever no per-language value is
set — but kakehashi shows a one-time migration notice when it is present, and
it may be removed in a future release.

Move `autoInstall = true` to `[languages._] autoInstall = true` — equivalent in
every case but one, see the migration caveat under
[`languages[*].autoInstall`](#languagesautoinstall) — then override per language
as needed.

#### `languages`

Per-language configuration. Usually not needed as kakehashi auto-detects languages.

| Field | Description |
|-------|-------------|
| `base` | Inherit parser, queries, bridge, and `autoInstall` configuration from another language |
| `parser` | Explicit path to the parser library (`.so`, `.dylib`, `.dll`) |
| `queries` | Array of query configurations with `path` and `kind` (highlights, bindings, injections) |
| `bridge` | Per-injection-language bridge filter and aggregation settings |
| `autoInstall` | Whether missing parsers/queries for this language may be auto-installed |
| `aliases` | Deprecated alternative language IDs. Prefer `base` on the derived language instead. |

##### `languages[*].autoInstall`

Whether kakehashi may download and install a missing parser/queries for this
language when a file is opened.

Resolved most-specific-wins: the language's own value, then each entry in its
`base` chain, then the `"_"` wildcard's, then the deprecated top-level
`autoInstall`, defaulting to `true`. Unset is the default at every level, so a
language inherits through `base` and `"_"` exactly like the other fields — for
example `[languages.rmd] base = "markdown"` picks up markdown's `autoInstall`
before `"_"` is consulted.

**Which name to use.** The key names the language kakehashi would *install*, not
the token in your document. A host document uses the `languageId` your editor
sends — except `plaintext`, where the language is inferred from the path and
content instead, so a `.rs` file opened as `plaintext` is governed by
`languages.rust.autoInstall`. An injected region uses its resolved language, so a
```` ```py ```` fence is governed by `languages.python.autoInstall`, and an `rmd`
region whose own parser is absent by whatever its `base` resolves to. Set the key
on the resolved name.

Enable everywhere except one language:

```toml
[languages._]
autoInstall = true

[languages.python]
autoInstall = false   # never auto-install the python parser
```

Or the reverse — off by default, with opt-in exceptions:

```toml
[languages._]
autoInstall = false

[languages.lua]
autoInstall = true
```

When auto-install is off for a language and its parser is missing, kakehashi
says so in the log alongside the manual `kakehashi language install` hint, and
points at the config that decided it. For a language with no entry of its own it
names the exact key (`` `languages._.autoInstall` is false ``); for one that
does have an entry it names the overriding key and the places the value may have
come from, since `base`-chain and `_` inheritance are folded together by then.

**Migration caveat:** moving the top-level key to `[languages._]` is
equivalence-preserving for every language *except* one whose `base` chain
terminates before reaching `"_"` — a self-referential `base`
(`[languages.foo] base = "foo"`) or a cycle that never visits `"_"`. Such a
language inherits nothing from `"_"` — by design, for every field — so it falls
through to the top-level default instead. (A chain that *does* reach `"_"`,
including one that gets there via an explicit `[languages._] base`, inherits
normally.) Give those
languages an explicit `autoInstall` when migrating.

**Precedence note:** a `languages.*.autoInstall` value outranks the top-level
`autoInstall` even when the top-level one is set at a higher-precedence source.
That is deliberate — the top-level key is only a fallback for an unset
per-language value — but it means moving the key to `[languages._]` in a
low-precedence file will shadow a top-level `autoInstall` pushed via
`initializationOptions`. Prefer setting one or the other, not both.

##### `languages[*].base`

Use `base` when one language should reuse another language's parser, queries, and bridge settings while still allowing language-specific overrides.

```json
{
  "languages": {
    "rmd": {
      "base": "markdown",
      "bridge": {
        "r": {
          "aggregation": {
            "textDocument/completion": { "priorities": ["languageserver"] }
          }
        }
      }
    },
    "qmd": {
      "base": "markdown",
      "bridge": {
        "julia": { "enabled": false }
      }
    }
  }
}
```

For `rmd`, kakehashi will try `rmd`-specific parser/query settings first and fall back through `markdown` and then `_`. Fields set on the derived language override inherited fields. Omitted fields inherit from the base chain; `queries: []` and `bridge: {}` explicitly clear inherited query and bridge settings.

Set `base` to the same language name to make a self-contained language that does not inherit from `_`:

```json
{
  "languages": {
    "my_custom_lang": {
      "base": "my_custom_lang",
      "parser": "/path/to/my_custom_lang.so"
    }
  }
}
```

#### `captureMappings`

Remap Tree-sitter capture names to LSP semantic token types. Use `_` as a wildcard for all languages.

```json
{
  "captureMappings": {
    "_": {
      "highlights": {
        "variable.builtin": "variable.defaultLibrary",
        "function.builtin": "function.defaultLibrary"
      }
    },
    "python": {
      "highlights": {
        "type.builtin": "type.defaultLibrary"
      }
    }
  }
}
```

### Bridge Configuration

#### `languageServers`

Configure language servers for bridging LSP requests in injection regions.

```json
{
  "languageServers": {
    "pyright": {
      "cmd": ["pyright-langserver", "--stdio"],
      "languages": ["python"]
    },
    "lua-language-server": {
      "cmd": ["lua-language-server"],
      "languages": ["lua"]
    }
  },
  "languages": {
    "markdown": {
      "bridge": {
        "python": {
          "aggregation": {
            "textDocument/completion": { "priorities": ["pyright"], "maxFanOut": 1 }
          }
        },
        "lua": {
          "aggregation": {
            "_": { "priorities": ["lua-language-server"] }
          }
        }
      }
    },
    "quarto": {
      "bridge": {
        "r": { "enabled": false }
      }
    }
  }
}
```

**Server Configuration:**

| Field | Description |
|-------|-------------|
| `cmd` | Command and arguments to start the language server |
| `languages` | Languages this server handles. The element `"*"` means **any language**, for servers that are not tied to one (spell/grammar/typo checkers, AI completion) — see below. |
| `initializationOptions` | Optional initialization options forwarded during the downstream server's `initialize` request |
| `workspaceMarkers` | Marker files/directories locating the workspace root the server is initialized with, following Neovim's `vim.fs.root` `(string\|string[])[]` shape. (The pre-rename key `rootMarkers` is still accepted as a deprecated alias.) Entries are tried **in list order** (earlier = higher priority): each entry is searched up the triggering document's ancestors nearest-first before the next entry is tried, so a higher-priority marker in a far ancestor outranks a lower-priority one sitting next to the document. A **nested array** is one equal-priority group where the nearest ancestor containing any of its names wins — e.g. `[["stylua.toml", ".luarc.json"], ".git"]` means "nearest of stylua.toml/.luarc.json, otherwise .git". The first matching entry's directory becomes the server's `rootUri` and sole workspace folder. Default: `[".git"]`. No marker hit falls back to the client-supplied root; an explicit `[]` disables the search. The connection pool is keyed by `(server, resolved root)`, so in a multi-root monorepo documents under different marker roots get their own downstream process, each rooted correctly; documents sharing a root (or the no-marker fallback) share one process. Trade-off: process count grows with the number of distinct roots opened, and there is currently no idle-eviction — a long session touching many roots keeps one process per root alive until shutdown. Servers that operate purely on `workspaceFolders` can opt out of this growth with `preferSharedInstance` (below). |
| `onTypeFormattingTriggers` | Trigger characters for bridged `textDocument/onTypeFormatting` (e.g. `["}", ";"]`). kakehashi advertises the sorted union across all servers at initialize and forwards a request to a downstream server only when that server's own capabilities declare the typed character. Unset everywhere (default) → the capability is not advertised. |
| `preferSharedInstance` | Prefer reusing **one** downstream process across every workspace root for this server instead of the default one-process-per-marker-root (above). Default `false`. It is a *preference*, honored only when the downstream server advertises `workspace.workspaceFolders.{supported, changeNotifications}`: when it does, kakehashi routes all roots to a single connection and announces each new root with `workspace/didChangeWorkspaceFolders`; when it does not, kakehashi logs once and silently falls back to the per-root-instance model. Because that fallback is universal, a blanket `languageServers._.preferSharedInstance = true` is safe across a mixed set of servers. Use it to bound process count and get cross-root navigation for servers that key purely off `workspaceFolders`; leave it `false` for servers needing per-root isolation (per-root virtualenv, conflicting tool/package versions) or that key behavior off the immutable `rootUri`. Note: removal/idle-eviction of folders is not modeled yet — the set only grows. |

> **Migration note**: `workspaceMarkers` was previously named `rootMarkers`
> (aligning with the LSP spec's `workspaceFolders`). The old `rootMarkers` key
> still works as a deprecated alias, so existing configs need no change; new
> configs should prefer `workspaceMarkers`. When a config still uses
> `rootMarkers`, kakehashi shows a one-time deprecation notice per session as a
> visible `window/showMessage` popup.

A `languageServers._` wildcard entry supplies defaults that every server
inherits field-by-field (wildcard-config-inheritance) — e.g. set
`workspaceMarkers` or `preferSharedInstance` once for all servers. A concrete
server's explicit value overrides the wildcard, so `_.preferSharedInstance =
true` can still be opted out of per server with `preferSharedInstance =
false`. A wildcard-only entry is never spawned itself, and a concrete server
whose merged `cmd` is still empty is skipped.

**Servers for any language (`languages = ["*"]`)**

Some servers are not tied to a language at all — grammar and spell checkers,
typo linters, AI completion. Give them the single element `"*"` instead of an
enumeration:

```toml
[languageServers.harper-ls]
cmd = ["harper-ls", "--stdio"]
languages = ["*"]
```

`"*"` is a list element for the same reason `priorities` uses one: `_` keys
carry field-level inheritance, list elements do not. Note that an **empty or
omitted** `languages` does *not* mean "any" — it means "not specified here", so
it falls through to the same server's entry in a lower config layer and, only
if no layer specified one, to the `_` entry (wildcard-config-inheritance).
Either way it can only ever *defer*, which is why widening needs its own
marker. Keeping it in the list also leaves room for future set algebra such as
an `"!markdown"` exclusion.

**Budget it against regions, not languages.** A `"*"` server is one process per
workspace root by default — the connection pool has no language dimension, and
`preferSharedInstance` can collapse the roots too — but it gets a
virtual `didOpen` for every injection *region* it matches, and joins every
region's fan-out. That number is larger than it looks in markdown: the shipped
injection query emits a `markdown_inline` region per inline node and per table
cell, so a prose file yields regions in the hundreds. Nothing bounds it
(`maxFanOut` caps servers per region, not regions). Use the per-host bridge
filter to exclude what the server should not see:

```toml
# harper-ls checks the prose; it does not need the inline sub-regions too.
[languages.markdown.bridge.markdown_inline]
enabled = false
```

Also note that diagnostics and code actions are concatenated without span
dedup, so a `"*"` server can report the same finding once per matching region —
and, with `bridge._self.enabled = true`, once more from the host document.

Two things `"*"` does **not** do:

- It does not enable bridging for a language the host blocks. `"*"` widens
  *which servers may answer*, not *which injections a host bridges at all* —
  a `languages.markdown.bridge.rust.enabled = false` filter still applies.
- It does not opt the server into the host-document bridge. A `"*"` server
  becomes a host candidate for every language, but the host path remains
  gated on the explicit `bridge._self.enabled = true` opt-in.

Because `languages` is inheritable, `languageServers._.languages = ["*"]`
attaches **every** server that omits `languages` to **every** language. That
is occasionally what you want, but it is rarely what you mean — prefer
declaring `"*"` on the concrete servers that need it. Note the reach crosses
config *files*: layers collapse before the `_` entry is resolved, so a `_`
wildcard in your user config widens servers declared in any project's config
too — servers you never saw when you wrote it.

Opting a single server back out is `enabled = false`, not an empty
`languages`: `[]` means "not specified here", so it resolves to whatever the
next source supplies — that same server's list from a lower config layer if it
has one, and otherwise the `_` wildcard, i.e. right back to `["*"]`. A concrete
server's own non-empty `languages` does override the wildcard, so listing real
languages narrows it as expected.

**Bridge Language Configuration:**

Each entry in the `bridge` map configures bridging for one injection language:

| Field | Description |
|-------|-------------|
| `enabled` | Whether bridging is enabled (`true`/`false`). Omit to inherit from the `_` wildcard (defaults to `true`). |
| `aggregation` | Per-method aggregation config. Key = LSP method name (e.g., `textDocument/completion`) or `_` for default. |

**Host bridging (`bridge._self`):**

The reserved `_self` key makes the host language its own bridge target: with
it enabled, requests on the host document are forwarded to servers whose
`languages` matches the **host** language (including a `"*"` server), with the real URI and no
coordinate translation. All bridged request methods are wired (exceptions:
semantic tokens; document color stays injection-only; host completion-item
and code-lens resolves pass through unrouted); by default the host layer is
tried after
`virt` (see `layers` above), so for `preferred` methods injections keep
winning inside code fences while the host server answers everywhere else —
diagnostics/code actions (`concatenated` default) and the formatting
pipeline combine BOTH layers instead. For formatting, combine
fence formatters with a whole-document formatter via
`layers.aggregation."textDocument/formatting".strategy = "concatenated"`.

```toml
[languages.markdown.bridge._self]
enabled = true                       # opt-in: REQUIRED, never inherited from `_`

[languageServers.marksman]
cmd = ["marksman", "server"]
languages = ["markdown"]             # host candidate for markdown documents
```

Unlike injection entries, `_self.enabled` does **not** inherit from the `_`
wildcard — a server listing the host language is a *capability*, not consent
to use it. `_self.aggregation` (priorities/strategy/maxFanOut) inherits from
`_` as usual.

**Aggregation Configuration:**

When multiple language servers can handle the same injection language, `aggregation` controls which server's response is preferred. Each entry contains:

| Field | Description |
|-------|-------------|
| `priorities` | Ordered **allowlist** of server names: listed servers are queried in this order, and servers absent from the list do not run. A `"*"` element stands for every configured-but-unlisted server (first-win among themselves), so `["pyright", "*"]` means "prefer pyright, fall back to the rest" and `["*", "pylsp"]` demotes pylsp below everyone else. Omitted = `["*"]` (all servers, first-win). An explicit `[]` disables the method for this bridge entry. Note: the sequential `concatenated` formatting pipeline requires explicit names and ignores `"*"`. |
| `strategy` | `"preferred"` or `"concatenated"`. Default depends on the LSP method: `"concatenated"` for the diagnostics methods (`textDocument/diagnostic`, `textDocument/publishDiagnostics`) and `textDocument/codeAction` (every server's actions appear in one menu), `"preferred"` for everything else. `"preferred"` uses the first non-empty response; `"concatenated"` collects and merges responses from all servers. Note: only the diagnostics methods, `textDocument/codeAction`, and full `textDocument/formatting` (sequential pipeline) consume `"concatenated"` — every other method dispatches `"preferred"` regardless of this field. |
| `maxFanOut` | Maximum number of servers to query. `null` or omitted = no limit (default). `0` = disable fan-out entirely. Positive integer = cap at N servers. Priority servers are selected first when limiting. Negative values are treated as no limit. |

> **Migration note**: `priorities` used to be a preference order only — unlisted
> servers still participated as fallback. It is now an allowlist: `["pyright"]`
> runs *only* pyright. Append `"*"` (`["pyright", "*"]`) to keep the old
> fallback behavior.

Example with per-method priorities, strategy, and maxFanOut:

```json
{
  "bridge": {
    "python": {
      "aggregation": {
        "textDocument/completion": { "priorities": ["pyright", "pylsp"], "maxFanOut": 1 },
        "textDocument/diagnostic": { "strategy": "preferred", "priorities": ["pyright", "*"] },
        "_": { "priorities": ["pylsp", "*"] }
      }
    }
  }
}
```

**Cross-Layer Aggregation (`layers`):**

A request to kakehashi can be answered by up to three *result layers*:
`virt` (the injection bridges above), `host` (a host-document language
server — opt-in via `bridge._self` above), and `native` (kakehashi's
own features). `layers.aggregation` prioritizes which layer contributions are
selected and staged per LSP method, mirroring the
`bridge.<lang>.aggregation` nesting. Methods with a deterministic wire-order
contract, such as diagnostics, normalize those staged contributions before
returning the response:

```json
{
  "languages": {
    "markdown": {
      "layers": {
        "aggregation": {
          "textDocument/hover": { "priorities": ["virt", "native"] },
          "_": { "priorities": ["virt", "host", "native"] }
        }
      }
    }
  }
}
```

| Field | Description |
|-------|-------------|
| `priorities` | Ordered allowlist of layers, highest priority first (same allowlist rule as the server-name `priorities` above, but over the closed set `virt`/`host`/`native` — no `"*"`). Layers omitted from the list do not participate; `[]` disables the method entirely. Default: `["virt", "host", "native"]`. Omitting `"virt"` turns off injection bridging for that method. |
| `strategy` | Cross-layer combine strategy: `"preferred"` (first non-empty layer wins) or `"concatenated"`. Consumed by `textDocument/formatting` (default `"concatenated"`: a sequential pipeline — injection regions format first (`virt`), then the host formatter (`host`, see `bridge._self`) formats the resulting text, collapsing into one whole-document edit), by the diagnostics methods (default `"concatenated"`: the `virt` regions' diagnostics and the host servers' diagnostics for the real document merge into one report/publish; `"preferred"` returns the first non-empty layer instead), by `textDocument/codeAction` (default `"concatenated"`: the injection region's actions and the host servers' actions appear in one menu, with at most one `isPreferred` action kept), and by list-shaped whole-document methods such as `textDocument/documentLink`, `textDocument/foldingRange`, and `textDocument/codeLens` when explicitly configured. Every other method combines with `"preferred"` regardless of this field. |

Details:

- **Key**: under `aggregation`, the LSP method name or `_` for the method
  wildcard (same convention as `bridge.<lang>.aggregation`).
- **Formatting**: `textDocument/rangeFormatting` shares the
  `textDocument/formatting` key.
- **Diagnostics**: two keys, mirroring their aggregation keying — pull
  diagnostics under `textDocument/diagnostic`, push diagnostics under
  `textDocument/publishDiagnostics`. Each layer is gated independently by
  `priorities` membership (host additionally by `bridge._self.enabled`);
  omit both layers (or use `_` with `priorities = []`) to fully turn
  bridge-driven diagnostics off. With host bridging opted in, host servers
  are pulled with the real document URI and their diagnostics merge with
  the injection regions' per the layer `strategy`. Caveat: SPONTANEOUS
  pushes a downstream server sends on its own bypass the
  priorities/strategy machinery when proactively republished — cached and
  concatenated across servers, except that push slots from pull-capable
  servers are suppressed in favor of pull results while the pull layer is
  active (in mixed configurations this can suppress a push whose server was
  not itself pulled; host `_self` pushes keep their real host URIs and
  ranges as-is). When cached pushes later answer a client pull
  (`pushFallback`), only push-driven servers' slots fold in (pull-capable
  servers excluded), under the CROSS-LAYER priorities/strategy only —
  server-level `priorities`/`maxFanOut` are not reapplied.
- **Current effect**: the `virt` layer answers inside injection regions, and
  the `host` layer answers on the host document itself for the bridged
  request methods — including pull/push diagnostics, with the `bridge._self`
  exceptions noted above — when host bridging is opted in. The `native`
  layer additionally computes definition/references/document highlight/
  rename from Tree-sitter bindings under `KAKEHASHI_EXPERIMENTAL=true` (for
  languages shipping a `bindings.scm`; `#offset!`-shifted regions declined).
  Semantic tokens stay native-only for now.

> **Migration note**: the layer list was renamed `order` →
> `priorities` (and, one change earlier, the method map moved under
> `aggregation`: `layers.<method>` → `layers.aggregation.<method>`). Old
> keys are silently ignored — rewrite `layers.aggregation.<method>.order`
> (or the older `layers.<method>.order`) as
> `layers.aggregation.<method>.priorities`. The default `strategy` for
> `textDocument/formatting` also changed from `"preferred"` to
> `"concatenated"`; set it back explicitly if you relied on
> first-non-empty-layer-wins formatting with host bridging enabled.

**Bridge Filter Semantics:**

The `bridge` map in language configuration controls which injection languages are bridged:

| Value | Meaning |
|-------|---------|
| `{ "_": { "enabled": false }, "python": { "enabled": true } }` | Bridge only Python injections |
| `{ "r": { "enabled": false } }` | Bridge every configured injection language except R |
| `{}` | Disable bridging entirely for this host language |
| `null` or omitted | Bridge all configured languages (default) |

### Configuration Files

kakehashi loads configuration from `~/.config/kakehashi/kakehashi.toml` (user config) and `./kakehashi.toml` (project config). Both use the same TOML format:

```toml
[captureMappings._.highlights]
"variable.builtin" = "variable.defaultLibrary"

[languages.custom_lang]
queries = [
    { path = "./queries/highlights.scm", kind = "highlights" }
]
```

Configuration files are merged with LSP initialization options (which take highest precedence).

You can override the default locations with `--config-file`:

```bash
# Use a single custom config file (skips default user and project configs)
kakehashi --config-file /path/to/custom.toml

# Use multiple config files (merged in order; later files override earlier)
kakehashi --config-file /path/to/base.toml --config-file /path/to/overrides.toml

# Use an empty file for test isolation (only programmed defaults apply)
kakehashi --config-file /path/to/empty.toml
```

When `--config-file` is specified:
- Default user config (`~/.config/kakehashi/kakehashi.toml`) is **skipped**
- Default project config (`./kakehashi.toml`) is **skipped**
- A file that is **present but unusable** aborts startup: unreadable, malformed
  TOML, larger than 8 MiB, or carrying a path that cannot be expanded. LSP initialization returns a
  `RequestFailed` error naming the first such file; `format` and `diagnose`
  print it to stderr and exit with status 2. Nothing is re-read while the
  session runs, so correcting the file means restarting the server
  (`:LspRestart`, or reloading the window). A client that responds to the
  rejected handshake by sending `initialize` again is also served correctly —
  the retry re-reads the files and is not contaminated by the failed attempt —
  but most editors do not, so treat restart as the recovery path.
- A path that expands badly is judged **per file**, so a later layer cannot mask
  an earlier layer's undefined variable — the merged result would never mention
  the mistake, because a later layer replaces path fields wholesale.
- A file that is **absent** is skipped rather than treated as an error, so
  `--config-file base.toml --config-file overrides.toml` works in a repository
  that has no overlay. Note that a relative path resolves against the process
  working directory, which for an editor-spawned server is the editor's rather
  than the workspace root. The skip is reported to the LSP client as a warning;
  `format` and `diagnose` report only the hard errors above, so a mistyped path
  is silent there.
- A path whose metadata cannot be read at all — an ancestor directory denying
  traversal, say — counts as unusable, not absent.
- A key kakehashi does not recognise is **reported but not fatal**, so a file
  can carry a key a newer version understands without the older one refusing to
  start. Serde would otherwise drop `autoInstal = false` silently and leave you
  wondering why the default applied. Two caveats: keys inside `features` are an
  exception — the parser rejects them outright, so an unknown one there *is*
  fatal — and in `format`/`diagnose` the warning has nowhere to go, since CLI
  mode surfaces only hard errors. (`workspace/didChangeConfiguration` is
  stricter still and rejects the whole update — that one is a live edit, not a
  file you may share across versions.)
- Cross-field invariants (e.g. `debounceMs` ≤ `maxWaitMs`) are judged on the
  merged explicit configuration, so splitting the two halves across two files is
  fine — but a combination that is invalid only once merged still aborts.
- `initializationOptions` from the LSP client still apply on top, and keep their
  ordinary non-fatal behavior — but "non-fatal" covers two different outcomes.
  An override that fails to *parse* is warned about and dropped, leaving the
  config files in effect. An override that parses and then makes the merged
  configuration invalid — an unexpandable path, a violated invariant — discards
  the *whole* merge in favour of programmed defaults, so the config files do not
  survive it either. Only the abort is avoided, not the loss.

Implicitly discovered configuration is deliberately laxer. A
`~/.config/kakehashi/kakehashi.toml` or `./kakehashi.toml` that fails to parse
is reported as a warning and skipped, so a stray file cannot stop the server
from starting. Only paths you name explicitly are strict.

## CLI Commands

The CLI uses a hierarchical subcommand structure: `kakehashi <resource> <action>`.

### Language Management

```bash
# Install a language (parser + queries)
kakehashi language install lua

# Install with verbose output
kakehashi language install lua --verbose

# Force reinstall
kakehashi language install python --force

# Custom data directory (--data-dir is a global flag, works at any position)
kakehashi --data-dir /custom/path language install go

# Bypass metadata cache
kakehashi language install ruby --no-cache

# List supported languages
kakehashi language list

# Show installed languages and their status
kakehashi language status

# Show status with file paths
kakehashi language status --verbose

# Show status for custom data directory
kakehashi --data-dir /custom/path language status

# Uninstall a language (parser + queries)
kakehashi language uninstall lua

# Uninstall without confirmation prompt
kakehashi language uninstall lua --force

# Uninstall all installed languages
kakehashi language uninstall --all --force
```

### Configuration Management

```bash
# Print a default configuration template to stdout
kakehashi config init

# Write a template to a file
kakehashi config init --output ./kakehashi.toml

# Overwrite an existing file
kakehashi config init --output ./kakehashi.toml --force
```

`--force` only applies when `--output` is used.

### Formatting

`kakehashi format` formats files through the same downstream language servers
the LSP bridge uses: each injection region (e.g. a fenced code block in
Markdown) is sent to the servers configured for its language, and the edits
are applied back to the host file. Configuration comes from the usual config
files (`./kakehashi.toml` etc.) or `--config-file`.

```bash
# Format files in place
kakehashi format README.md docs/

# Directories are walked recursively, respecting .gitignore;
# explicitly listed files are formatted even when gitignored
kakehashi format .

# Exclude paths (gitignore-style pattern, repeatable)
kakehashi format . --excludes vendor/ --excludes "*.gen.md"

# CI: don't write, exit 1 if anything would change
kakehashi format . --check

# Write changes but exit 1 if anything changed
kakehashi format . --fail-on-change

# Format stdin (result goes to stdout); the filename drives language detection
cat README.md | kakehashi format --stdin-filename README.md

# Indentation hints forwarded as LSP FormattingOptions (servers may ignore
# them in favor of their own config; defaults: --tab-size 4 --insert-spaces true)
kakehashi format . --tab-size 2 --insert-spaces false
```

Exit codes: `0` nothing to change (or changes written without
`--fail-on-change`), `1` changes detected under `--check`/`--fail-on-change`,
`2` usage error, I/O error, a configuration file given with `--config-file`
that could not be loaded, or downstream formatter failure (a configured
server failed to start, errored on the request, timed out, or returned a
protocol-invalid response). Only *request-time* failures are strategy-aware:
under `preferred` — the bridge-level default for formatting, where several
servers of one target produce competing whole-document edits — the winner is
authoritative, so a non-winning server's failed request does not count, while
`concatenated` counts every server's. A server that fails to **start** always
counts, whichever strategy is in force and even if a fallback then formatted
the document.

A configuration failure exits `2` before anything is written, so stdout stays
empty in `--stdin-filename` mode. A *downstream* failure does not: the content
is written first and the exit code reports the failure afterwards, so a caller
that pipes stdin through `format` must check the status rather than assume
empty output means failure.

### Diagnostics

`kakehashi diagnose` pulls diagnostics for files through the same bridge the
LSP server uses (injection regions via `virt`, the host document via
`bridge._self`, aggregated per `layers.aggregation`) and prints them in a
machine-readable format. File selection matches `format` (directories walked
respecting `.gitignore`, explicit paths win, `--excludes` filters every path
under the current directory — including explicitly listed ones).

Only **pull** diagnostics (`textDocument/diagnostic`) are reported. **Push**
diagnostics (`textDocument/publishDiagnostics`) are not collected, so a
downstream server that only publishes diagnostics — and does not answer a pull
request — contributes nothing to `kakehashi diagnose`.

```bash
# Report diagnostics (default format: file:line:col: severity: message [source])
kakehashi diagnose README.md docs/

# Output formats: default (the above) or jsonl (one JSON object per line)
kakehashi diagnose . --output-format jsonl

# CI gate: errors always exit 1; --fail-on-warning makes warnings fail too.
# To never fail on diagnostics, append `|| true`.
kakehashi diagnose . --fail-on-warning

# Diagnose stdin; the filename drives language detection and config resolution
cat README.md | kakehashi diagnose --stdin-filename README.md
```

The default format is presentation-oriented and intentionally lossy. The file
field preserves ordinary characters but renders terminal/bidirectional controls
as visible Rust-style escapes (for example, `\u{1b}`). In message and source
fields, leading and trailing whitespace is removed and internal runs become
single spaces before remaining controls are escaped. Use
`--output-format jsonl` when consumers need the original field values. JSON
escapes controls on the wire, but parsing each JSONL record recovers the exact
emitted Unicode field values. In path mode, `file` is cwd-relative when it is
under the current directory and absolute otherwise. It uses Rust's display
representation, which can be lossy for a non-UTF-8 filename.

Diagnostics go to stdout; the one-line summary, any errors, and `RUST_LOG`
output go to stderr — so stdout stays a clean data channel for `| jq` / `| head`
(redirect or ignore stderr in CI if the summary is unwanted).

Line and column are 1-based; a diagnostic with no severity is treated as an
error (so it can never silently slip past the gate). Exit codes: `0` no failing
diagnostic; `1` a failing diagnostic — any error always, plus warnings with
`--fail-on-warning` (info/hint never fail); `2` an operational error (a file
could not be read, a path could not be opened or fully enumerated, a
configuration file given with `--config-file` could not be loaded, or a
configured downstream server failed — including one that answered the
`textDocument/diagnostic` pull with an error response or a
present-but-malformed payload, matching `kakehashi format`'s strictness) —
independent of the diagnostics, so it surfaces a broken run rather than
looking clean to CI.

> Under the non-default `preferred` aggregation strategy, the winning server's
> result is authoritative, so a *non-winning* server's request-time failure is
> deliberately **not** counted toward exit `2` — a failure surfaces only when
> no server won (no non-empty result) *and* a contender actually failed. The
> default `concatenated` strategy counts every server's failure.

Diagnostics stream to stdout as each file is processed. Every file is always
scanned so the exit code reflects the whole set; if stdout is closed before the
scan finishes (e.g. `kakehashi diagnose . | head`), further writes are
suppressed but the scan still completes.

## Editor Integration

### Neovim

Using Neovim's built-in LSP client (0.11+):

```lua
vim.lsp.config.kakehashi = {
  cmd = { "kakehashi" },
  init_options = {
    -- LSP Bridge configuration (optional)
    languageServers = {
      pyright = {
        cmd = { "pyright-langserver", "--stdio" },
        languages = { "python" },
      },
      ["lua-language-server"] = {
        cmd = { "lua-language-server" },
        languages = { "lua" },
      },
    },
    languages = {
      _ = { autoInstall = true },
      markdown = {
        bridge = {
          python = {
            aggregation = {
              ["textDocument/completion"] = { priorities = { "pyright" }, maxFanOut = 1 },
            },
          },
          lua = {
            aggregation = {
              _ = { priorities = { "lua-language-server" } },
            },
          },
        },
      },
    },
  },
}
vim.lsp.enable("kakehashi")

-- Disable built-in treesitter highlighting to avoid conflicts
vim.api.nvim_create_autocmd("FileType", {
  callback = function()
    vim.treesitter.stop()
  end,
})
```

With nvim-lspconfig:

```lua
require("lspconfig").kakehashi.setup({
  init_options = {
    languages = { _ = { autoInstall = true } },
  },
})
```

### VS Code

kakehashi does not currently ship a first-party VS Code extension. If you use VS Code, register `kakehashi` through a generic LSP client extension and pass the same `initializationOptions` shown above.

### Other Editors

Any editor supporting LSP can use kakehashi. Configure it as a language server with the `kakehashi` command.

## Supported Languages

kakehashi supports any language with a Tree-sitter grammar available in nvim-treesitter. Common languages include:

- Lua, Python, Rust, Go, C, C++
- JavaScript, TypeScript, TSX, JSX
- HTML, CSS, JSON, YAML, TOML
- Markdown, LaTeX
- Bash, Fish, Zsh
- SQL, GraphQL
- And many more...

Run `kakehashi language list` for the complete list.

## Query Inheritance

Some languages inherit queries from base languages:

| Language | Inherits From |
|----------|---------------|
| TypeScript | ecma |
| JavaScript | ecma, jsx |
| TSX | typescript, jsx |

When you install a language with inheritance, the base queries are automatically downloaded.

## Logging

kakehashi uses Rust's standard logging with `env_logger`. Configure logging via the `RUST_LOG` environment variable.

### Log Targets

| Target | Level | Description |
|--------|-------|-------------|
| `kakehashi::lock_recovery` | warn | Thread synchronization recovery events |
| `kakehashi::crash_recovery` | error | Panics escaping compute-pool work units |
| `kakehashi::query` | info | Query syntax/validation issues |

### Examples

```bash
# Enable all kakehashi logs at debug level
RUST_LOG=kakehashi=debug kakehashi

# Only show crash events (most severe)
RUST_LOG=kakehashi::crash_recovery=error kakehashi

# Show query issues (helpful for query authors)
RUST_LOG=kakehashi::query=info kakehashi

# Show lock recovery events (for debugging thread issues)
RUST_LOG=kakehashi::lock_recovery=warn kakehashi
```

**Note:** Logs are written to stderr. Stdout is reserved for LSP JSON-RPC protocol messages.

## Troubleshooting

### Parser fails to load

1. Check if the parser exists: `ls ~/.local/share/kakehashi/parser/`
2. Reinstall: `kakehashi language install <language> --force`
3. Check for ABI compatibility with your Tree-sitter version

### No syntax highlighting

1. Verify queries exist: `ls ~/.local/share/kakehashi/queries/<language>/`
2. Check LSP logs for errors
3. Ensure your editor has semantic tokens enabled

### Queries not working for TypeScript/JavaScript

These languages use query inheritance. Ensure base queries are installed:

```bash
kakehashi language install typescript --force
# This automatically installs 'ecma' queries
```
