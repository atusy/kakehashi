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
- **No cross-region results within the host document**: on the goto/references/rename transforms, a result addressed to a *different* region's virtual URI is filtered out (that URI would be meaningless to the editor; document-link targets are the exception — they pass through untouched). A code action touching another region keeps a visible-but-disabled entry for `disabledSupport` clients (payload stripped); without that capability it is dropped from the initial response, or returned unresolved on `codeAction/resolve` (a response cannot be dropped). Results in real files — an external definition, a cross-file rename edit — pass through unchanged; for navigation/references/rename, host-URI results are not containment-checked (INJECTION-layer code actions, and applyEdit requests that also touch a virtual document, DO region-bound host-URI edits).

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

```json
{
  "searchPaths": ["$HOME/.local/share/kakehashi", "/another/path"],
  "autoInstall": true,
  "languages": {
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

**Behavior on undefined variables:** If a referenced environment variable is not defined, configuration loading fails with an error notification and falls back to previous settings (or defaults). The exception is `KAKEHASHI_DATA_DIR`, which automatically falls back to the platform-specific default when unset (see [Default Data Directories](#default-data-directories)).

### Option Reference

#### `searchPaths`

Array of base directories to search for parsers and queries. If not specified, uses platform-specific defaults:
- Linux: `~/.local/share/kakehashi`
- macOS: `~/Library/Application Support/kakehashi`
- Windows: `%APPDATA%/kakehashi`

**Important:** Specify base directories, not subdirectories. The resolver automatically appends `parser/` and `queries/` subdirectories.

Parsers are searched as `{searchPath}/parser/{language}.{so,dylib,dll}`.
Queries are searched as `{searchPath}/queries/{language}/{query_type}.scm`.

#### `autoInstall`

- `true` (default): Automatically download and install missing parsers/queries when a file is opened
- `false`: Require manual installation via CLI

#### `languages`

Per-language configuration. Usually not needed as kakehashi auto-detects languages.

| Field | Description |
|-------|-------------|
| `base` | Inherit parser, queries, and bridge configuration from another language |
| `parser` | Explicit path to the parser library (`.so`, `.dylib`, `.dll`) |
| `queries` | Array of query configurations with `path` and `kind` (highlights, bindings, injections) |
| `bridge` | Per-injection-language bridge filter and aggregation settings |
| `aliases` | Deprecated alternative language IDs. Prefer `base` on the derived language instead. |

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
| `languages` | Languages this server handles |
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

**Bridge Language Configuration:**

Each entry in the `bridge` map configures bridging for one injection language:

| Field | Description |
|-------|-------------|
| `enabled` | Whether bridging is enabled (`true`/`false`). Omit to inherit from the `_` wildcard (defaults to `true`). |
| `aggregation` | Per-method aggregation config. Key = LSP method name (e.g., `textDocument/completion`) or `_` for default. |

**Host bridging (`bridge._self`):**

The reserved `_self` key makes the host language its own bridge target: with
it enabled, requests on the host document are forwarded to servers whose
`languages` contains the **host** language, with the real URI and no
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
own features). `layers.aggregation` orders them per LSP method, mirroring
the `bridge.<lang>.aggregation` nesting:

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
- Non-existent files produce an error
- `initializationOptions` from the LSP client still apply on top

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
`2` usage error, I/O error, or downstream formatter failure (a configured
server failed to start, errored on the request, timed out, or returned a
protocol-invalid response — even when a fallback server still produced
output).

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

Diagnostics go to stdout; the one-line summary, any errors, and `RUST_LOG`
output go to stderr — so stdout stays a clean data channel for `| jq` / `| head`
(redirect or ignore stderr in CI if the summary is unwanted).

Line and column are 1-based; a diagnostic with no severity is treated as an
error (so it can never silently slip past the gate). Exit codes: `0` no failing
diagnostic; `1` a failing diagnostic — any error always, plus warnings with
`--fail-on-warning` (info/hint never fail); `2` an operational error (a file
could not be read, a path could not be opened or fully enumerated, or a
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
    autoInstall = true,
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
    autoInstall = true,
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
| `kakehashi::crash_recovery` | error | Parser crash detection and recovery |
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
