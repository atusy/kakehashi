# kakehashi Documentation

kakehashi is a Language Server Protocol (LSP) server that uses Tree-sitter for fast, accurate parsing. It provides semantic highlighting, selection ranges, and code actions for any language with a Tree-sitter grammar.

## Features

### Semantic Tokens (Syntax Highlighting)

Provides LSP semantic tokens based on Tree-sitter `highlights.scm` queries. Works with any editor that supports LSP semantic tokens.

- Supports language injection (e.g., SQL in JavaScript template strings, code blocks in Markdown)
- Uses nvim-treesitter query files for compatibility
- Supports query inheritance (e.g., TypeScript inherits from `ecma`)

### Selection Range

Expand/shrink selection based on AST structure. Select increasingly larger syntax nodes with each invocation.

### Code Actions

- **Swap Parameters**: Reorder function parameters

### LSP Bridge

Full LSP features in injection regions by bridging to language-specific servers. For example, get Rust completions and hover documentation inside Markdown code blocks.

**Supported Features:**
- Completion
- Signature Help
- Go to Definition / Type Definition / Implementation / Declaration
- Hover
- Find References

**Limitations:**
- **Same-region navigation only**: Cross-region jumps/edits (e.g., go to Definition, rename, ...) are not supported—these results are filtered out.

See [Configuration: Bridge](#bridge) for setup instructions.

## Prerequisites

kakehashi automatically compiles Tree-sitter parsers from source, which requires these external tools:

### Required Dependencies

| Dependency | Purpose | Installation |
|------------|---------|--------------|
| **tree-sitter CLI** | Compiles parser grammars into shared libraries | `cargo install tree-sitter-cli` |
| **Git** | Clones parser repositories during installation | Usually pre-installed |
| **C Compiler** | Required by tree-sitter CLI for compilation | See platform-specific instructions |

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
# Check tree-sitter CLI
tree-sitter --version

# Check C compiler
cc --version  # or gcc --version / clang --version

# Check git
git --version
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

Parsers are stored in `{data_dir}/parser/` and queries in `{data_dir}/queries/`.

## Configuration

Configuration is provided via LSP `initializationOptions`. All options are optional.

### Configuration Options

```json
{
  "searchPaths": ["/custom/path/to/parsers", "/another/path"],
  "autoInstall": true,
  "languages": {
    "lua": {
      "parser": "/path/to/lua.so",
      "queries": [
        {"path": "/path/to/highlights.scm", "kind": "highlights"},
        {"path": "/path/to/custom.scm", "kind": "highlights"},
        {"path": "/path/to/injections.scm", "kind": "injections"}
      ]
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
| `parser` | Explicit path to the parser library (`.so`, `.dylib`, `.dll`) |
| `queries` | Array of query configurations with `path` and `kind` (highlights, locals, injections) |

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
    "rust": {
      "highlights": {
        "type.builtin": "type.defaultLibrary"
      }
    }
  }
}
```

#### `languageServers`

Configure language servers for bridging LSP requests in injection regions.

```json
{
  "languageServers": {
    "rust-analyzer": {
      "cmd": ["rust-analyzer"],
      "languages": ["rust"],
    },
    "pyright": {
      "cmd": ["pyright-langserver", "--stdio"],
      "languages": ["python"]
    }
  },
  "languages": {
    "markdown": {
      "bridge": {
        "rust": { "enabled": true },
        "python": {
          "enabled": true,
          "aggregation": {
            "_": { "priorities": ["pyright"] }
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

Example with per-method priorities and strategy:

```json
{
  "bridge": {
    "python": {
      "enabled": true,
      "aggregation": {
        "textDocument/completion": { "priorities": ["pyright", "pylsp"] },
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
| `{ "rust": { "enabled": true } }` | Bridge only enabled languages |
| `{}` | Disable bridging entirely for this host language |
| `null` or omitted | Bridge all configured languages (default) |

### Project Configuration File

You can also use a `kakehashi.toml` file in your project root:

```toml
[captureMappings._.highlights]
"variable.builtin" = "variable.defaultLibrary"

[languages.custom_lang]
queries = [
    { path = "./queries/highlights.scm", kind = "highlights" }
]
```

Project configuration is merged with LSP initialization options.

## CLI Commands

The CLI uses a hierarchical subcommand structure: `kakehashi <resource> <action>`.

### Language Management

```bash
# Install a language (parser + queries)
kakehashi language install lua

# Install with verbose output
kakehashi language install rust --verbose

# Force reinstall
kakehashi language install python --force

# Custom data directory
kakehashi language install go --data-dir /custom/path

# Bypass metadata cache
kakehashi language install ruby --no-cache

# List supported languages
kakehashi language list

# Show installed languages and their status
kakehashi language status

# Show status with file paths
kakehashi language status --verbose

# Show status for custom data directory
kakehashi language status --data-dir /custom/path

# Uninstall a language (parser + queries)
kakehashi language uninstall lua

# Uninstall without confirmation prompt
kakehashi language uninstall rust --force

# Uninstall all installed languages
kakehashi language uninstall --all --force
```

### Configuration Management

```bash
# Generate a default configuration file in current directory
kakehashi config init

# Overwrite existing configuration file
kakehashi config init --force
```

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
      ["rust-analyzer"] = {
        cmd = { "rust-analyzer" },
        languages = { "rust" },
      },
      pyright = {
        cmd = { "pyright-langserver", "--stdio" },
        languages = { "python" },
      },
    },
    languages = {
      markdown = { bridge = { rust = { enabled = true }, python = { enabled = true } } },
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

(Configuration for VS Code LSP clients would go here)

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
