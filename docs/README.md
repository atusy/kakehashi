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
- Pull Diagnostics

The server does not currently advertise `textDocument/codeAction`.

**Limitations:**
- **Same-region navigation only**: Cross-region jumps/edits (e.g., go to Definition, rename, ...) are not supported—these results are filtered out.

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
        "python": { "enabled": true }
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
| `queries` | Array of query configurations with `path` and `kind` (highlights, locals, injections) |
| `bridge` | Per-injection-language bridge filter and aggregation settings |
| `aliases` | Deprecated alternative language IDs. Prefer `base` on the derived language instead. |

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
        "python": { "enabled": true },
        "lua": {
          "enabled": true,
          "aggregation": {
            "_": { "priorities": ["lua-language-server"] }
          }
        }
      }
    },
    "quarto": {
      "bridge": {
        "python": { "enabled": true },
        "r": { "enabled": true }
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

**Bridge Language Configuration:**

Each entry in the `bridge` map configures bridging for one injection language:

| Field | Description |
|-------|-------------|
| `enabled` | Whether bridging is enabled (`true`/`false`). Omit to inherit from the `_` wildcard (defaults to `true`). |
| `aggregation` | Per-method aggregation config. Key = LSP method name (e.g., `textDocument/completion`) or `_` for default. |

**Aggregation Configuration:**

When multiple language servers can handle the same injection language, `aggregation` controls which server's response is preferred. Each entry contains:

| Field | Description |
|-------|-------------|
| `priorities` | Ordered list of server names. The first server that returns a valid response wins. Empty list falls back to arrival-order (first-win) behavior. |
| `strategy` | `"preferred"` or `"concatenated"`. Default depends on the LSP method: `"concatenated"` for `textDocument/diagnostic`, `"preferred"` for everything else. `"preferred"` uses the first non-empty response; `"concatenated"` collects and merges responses from all servers. |
| `maxFanOut` | Maximum number of servers to query. `null` or omitted = no limit (default). `0` = disable fan-out entirely. Positive integer = cap at N servers. Priority servers are selected first when limiting. Negative values are treated as no limit. |

Example with per-method priorities, strategy, and maxFanOut:

```json
{
  "bridge": {
    "python": {
      "enabled": true,
      "aggregation": {
        "textDocument/completion": { "priorities": ["pyright", "pylsp"], "maxFanOut": 1 },
        "textDocument/diagnostic": { "strategy": "preferred", "priorities": ["pyright"] },
        "_": { "priorities": ["pylsp"] }
      }
    }
  }
}
```

**Bridge Filter Semantics:**

The `bridge` map in language configuration controls which injection languages are bridged:

| Value | Meaning |
|-------|---------|
| `{ "python": { "enabled": true } }` | Bridge only enabled languages |
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
      markdown = { bridge = { python = { enabled = true }, lua = { enabled = true } } },
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
