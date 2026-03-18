# ADR-0006: LSP Bridge Architecture for Injection Regions

## Status

Proposed (Phase 1 complete: infrastructure with go-to-definition working)

## Context

Markdown code blocks and other injection regions (e.g., JavaScript inside HTML `<script>` tags, SQL in string literals) currently only receive Tree-sitter-based features from kakehashi. While Tree-sitter provides excellent syntax highlighting via semantic tokens, injection regions lack access to full LSP capabilities such as:

- Go-to-definition with cross-file resolution
- Completion with type information
- Hover documentation
- Diagnostics from language-specific analyzers

Modern editors can only attach one LSP server per buffer, meaning users must choose between kakehashi (fast semantic tokens for the host document) and a language-specific server (full features but only for the primary language).

The key insight is: **kakehashi already knows where injection regions are and what languages they contain**. It can act as an LSP Bridge, connecting injection regions to appropriate language servers with position translation.

### Language Server Constraints

Language servers have requirements beyond the LSP protocol that affect this architecture:

**Project Context**: Many language servers require project structure to function. rust-analyzer returns `null` for go-to-definition on standalone `.rs` files—it needs a project definition (via `Cargo.toml` or `rust-project.json`/`linkedProjects`) to build its symbol index.

**Real Files on Disk**: Some servers index from the filesystem rather than relying solely on `didOpen` content. Virtual URIs (`file:///virtual/...`) are insufficient.

**Indexing Time**: Language servers need time to index after `didOpen` before responding to queries. The `publishDiagnostics` notification often signals indexing completion, though this is not guaranteed for all servers.

These constraints mean bridging is not simply "forward request, return response"—servers may need specific initialization options and real files on disk.

## Decision

**Implement LSP Bridge capability in kakehashi to connect injection regions to configured language servers, with position translation, user-provided initialization options, and connection pooling.**

### Architecture Overview

```
┌─────────────────────────────────────────────────────────────────┐
│                        kakehashi                            │
├─────────────────────────────────────────────────────────────────┤
│                                                                 │
│  ┌──────────────┐    ┌─────────────────┐    ┌────────────────┐  │
│  │ LSP Handler  │───▶│  BridgeRouter   │───▶│ PositionMapper │  │
│  │ (lsp_impl)   │    │                 │    │                │  │
│  └──────────────┘    └────────┬────────┘    └────────────────┘  │
│                               │                                 │
│                               ▼                                 │
│                      ┌─────────────────┐                        │
│                      │   ServerPool    │                        │
│                      │ ┌─────────────┐ │                        │
│                      │ │rust-analyzer│ │  (on-demand spawn,     │
│                      │ ├─────────────┤ │   connection reuse)    │
│                      │ │  pyright    │ │                        │
│                      │ ├─────────────┤ │                        │
│                      │ │   gopls     │ │                        │
│                      │ └─────────────┘ │                        │
│                      └────────┬────────┘                        │
│                               │                                 │
│                               ▼                                 │
│                      ┌─────────────────┐                        │
│                      │ TempFileManager │                        │
│                      │ (per-injection) │                        │
│                      └─────────────────┘                        │
│                                                                 │
└─────────────────────────────────────────────────────────────────┘
```

### Security Model

**Only explicitly configured servers are spawned.** kakehashi does not auto-discover or execute arbitrary language servers based on injection content. A malicious code block cannot trigger execution of unregistered commands.

- Servers must be listed in user configuration with explicit `cmd` field
- No shell expansion or command interpolation in server commands
- Temp files contain only extracted source code, never executable content

### Server Connection Pool

**Critical for production**: Spawning a language server per request is unacceptable (multi-second latency). Connections must be pooled and reused.

```
┌─────────────────────────────────────────────────────────┐
│                    ServerPool                           │
├─────────────────────────────────────────────────────────┤
│                                                         │
│  get_connection("rust")                                 │
│       │                                                 │
│       ▼                                                 │
│  ┌─────────────────┐    ┌─────────────────────────────┐ │
│  │ Connection      │ NO │ Spawn new server            │ │
│  │ exists?         │───▶│ Wait for initialization     │ │
│  └────────┬────────┘    │ Store in pool               │ │
│           │ YES         └─────────────────────────────┘ │
│           ▼                                             │
│  ┌─────────────────┐                                    │
│  │ Return existing │                                    │
│  │ connection      │                                    │
│  └─────────────────┘                                    │
│                                                         │
└─────────────────────────────────────────────────────────┘
```

#### Spawn Strategy

| Strategy | Trigger | First Request Latency | Resource Usage |
|----------|---------|----------------------|----------------|
| **Eager** | Injection detected during parse | Low (server pre-warmed) | Higher (may spawn unused) |
| **Lazy** | First LSP request to injection | High (spawn + index) | Lower (only when needed) |

**Recommended: Eager spawn** when injection is detected during document parsing or semantic token calculation. This eliminates user-perceived latency on first go-to-definition or hover.

```
Document Open/Edit
       │
       ▼
┌─────────────────┐
│ Parse document  │
│ Detect injects  │
└────────┬────────┘
         │
         ▼
┌─────────────────┐     ┌─────────────────────────────┐
│ For each new    │────▶│ Background: spawn server    │
│ injection lang  │     │ Write temp file             │
└─────────────────┘     │ Wait for ready signal       │
                        └─────────────────────────────┘
         │
         ▼
  (User makes request)
         │
         ▼
┌─────────────────┐
│ Server ready    │ ──▶ Immediate response
│ (already warm)  │
└─────────────────┘
```

Injection detection already happens during:
- `textDocument/semanticTokens` (we scan all injections for highlighting)
- Incremental parsing on `textDocument/didChange`

Spawning can piggyback on these existing code paths.

#### Lifecycle

- **Spawn on injection detection**: Background spawn when new language injection is found
- **Reuse**: All subsequent requests use warm connection
- **Crash recovery**: Detect dead servers (broken pipe, exit code) and respawn immediately

### Server Registry and Configuration

The bridge requires knowing which server to use for each language. Language servers are configured at the root level of `initializationOptions` under `languageServers`:

```json
{
  "languageServers": {
    "rust-analyzer": {
      "cmd": ["rust-analyzer"],
      "languages": ["rust"],
      "initializationOptions": {
        "linkedProjects": ["~/.config/kakehashi/rust-project.json"]
      }
    },
    "pyright": {
      "cmd": ["pyright-langserver", "--stdio"],
      "languages": ["python"]
    },
    "gopls": {
      "cmd": ["gopls"],
      "languages": ["go"]
    }
  }
}
```

| Field | Required | Description |
|-------|----------|-------------|
| `languageServers` | Yes | Server configurations keyed by server name (at root level) |
| `languageServers.*.cmd` | Yes | Command array: first element is program, rest are arguments |
| `languageServers.*.languages` | Yes | Languages this server handles |
| `languageServers.*.initializationOptions` | No | Passed to server's `initialize` request |

#### Per-Host Language Bridge Configuration

The `languages` section can specify which injection languages to bridge for each host filetype using a map with `enabled` flags:

```json
{
  "languages": {
    "quarto": {
      "bridge": {
        "python": { "enabled": true },
        "r": { "enabled": true }
      }
    },
    "rmd": {
      "bridge": {
        "r": { "enabled": true }
      }
    },
    "markdown": {
      "bridge": {}
    }
  }
}
```

| Value | Meaning |
|-------|---------|
| `{ "python": { "enabled": true } }` | Bridge only languages with `enabled: true` |
| `{}` (empty map) | Bridge no languages (disable bridging for this host) |
| `null` or omitted | Bridge all configured languages (default) |

This enables scenarios like:
- **Quarto files**: Bridge both Python and R to their respective servers
- **R Markdown files**: Bridge only R (no Python bridge needed)
- **Plain Markdown**: Disable all bridging (use Tree-sitter only)

The bridge filtering happens at request time: when a request targets an injection region, kakehashi checks if the injection language has `enabled: true` in the host's `bridge` map before routing to a server.

#### Multiple Servers Per Language

When multiple servers are configured for the same language (e.g., `pyright` + `ruff` for Python), requests are only routed to servers with the required capability. The routing strategy among capable servers is **implementation-defined**:

| Strategy | Description | Trade-off |
|----------|-------------|-----------|
| **First** | Route to first capable server | Simple, low latency, but loses information from other servers |
| **Aggregation** | Query all capable servers, merge responses | Richer results, but higher latency and merge complexity |

The appropriate strategy may vary by request type. For example, diagnostics benefit from aggregation (show warnings from all linters), while completion may prefer first (avoid duplicates).

This enables complementary servers: `pyright` for type checking, `ruff` for linting.

#### Why Server-Centric Configuration

| Concern | `languages` field | `languageServers` field |
|---------|-------------------|-------------------------|
| **Purpose** | Tree-sitter parser/query config + bridge filtering | LSP server connection |
| **Primary key** | Language name | Server name |
| **Scope** | One language per entry (with bridge filter) | One server → multiple languages |
| **Example** | Parser paths, query sources, `bridge` filter | `typos-lsp` for markdown + asciidoc |

This separation allows:
- **Cross-cutting servers**: `typos-lsp` provides diagnostics for multiple languages
- **Multiple servers per language**: `pyright` + `ruff` for Python (both in `languageServers`)
- **Independent lifecycle**: Tree-sitter config doesn't affect server spawning
- **Per-host filtering**: Each host language can selectively enable/disable bridging via `bridge` field

### Temporary File Management

Injection content must be written to disk for servers that require real files.

#### File Naming Strategy

Temp files use deterministic, unique paths to support multiple concurrent injections:

```
{temp_dir}/kakehashi/{document_hash}/{language}_{injection_index}.{ext}
```

Example:
```
/tmp/kakehashi/a1b2c3d4/rust_0.rs
/tmp/kakehashi/a1b2c3d4/rust_1.rs
/tmp/kakehashi/e5f6g7h8/python_0.py
```

| Component | Source |
|-----------|--------|
| `{temp_dir}` | `std::env::temp_dir()` (cross-platform) |
| `{document_hash}` | Hash of host document URI |
| `{language}` | Injection language name |
| `{injection_index}` | 0-based index within document |
| `{ext}` | Language-appropriate extension |

#### Cleanup Strategy

| Event | Action |
|-------|--------|
| Document closed | Delete temp files for that document |
| kakehashi startup | Clean stale files from previous sessions |
| kakehashi shutdown | Delete all temp files |

Startup cleanup handles crash recovery: scan `{temp_dir}/kakehashi/` and remove directories older than 24 hours.

### Workspace Provisioning

Different language servers have different project structure requirements:

| Server | Requirement | Solution |
|--------|-------------|----------|
| rust-analyzer | Project context | `linkedProjects` pointing to user's `rust-project.json` |
| gopls | Module context | v0.15.0+ has improved standalone file support |
| pyright | None | Works with virtual documents via `didOpen` |
| typescript-language-server | None | Works with virtual documents via `didOpen` |

#### Design: User-Configured LSP + Minimal File Creation

kakehashi should be as simple as possible:
1. **Create only the source file** with injection content
2. **Pass user-provided settings** to the language server via `initializationOptions`
3. **Let the language server** use its own configuration mechanisms

For rust-analyzer, users maintain a `rust-project.json` that defines a virtual crate:

```json
// ~/.config/kakehashi/rust-project.json
{
  "sysroot_src": "~/.rustup/toolchains/stable-x86_64-apple-darwin/lib/rustlib/src/rust/library",
  "crates": [{
    "root_module": "/tmp/kakehashi/injection.rs",
    "edition": "2021",
    "deps": []
  }]
}
```

> **Note**: The `root_module` path should match the temp file location used by kakehashi. For multiple injections, consider using a glob pattern if rust-analyzer supports it, or configure multiple crate entries.

kakehashi configuration points rust-analyzer to this file:

```json
{
  "languageServers": {
    "rust-analyzer": {
      "cmd": ["rust-analyzer"],
      "languages": ["rust"],
      "initializationOptions": {
        "linkedProjects": ["~/.config/kakehashi/rust-project.json"]
      }
    }
  }
}
```

**Benefits**:
- kakehashi has zero language-specific knowledge
- Users leverage familiar LSP configuration patterns
- Full flexibility for any language server
- Simpler implementation (just write source files)

#### Provisioning Flow

1. Create temporary file with injection content using deterministic path
2. Initialize server with user-provided `initializationOptions`
3. Send `didOpen` notification
4. Wait for ready signal (see below)
5. Clean up temp files on document close or shutdown

#### Ready Detection

Detecting when a server has finished indexing and is ready for queries:

| Method | Reliability | Timeout |
|--------|-------------|---------|
| `publishDiagnostics` received | Medium (some servers don't send for valid code) | 5s |
| `window/workDoneProgress` completion | High (when supported) | 10s |
| Configurable delay | Low (guessing) | N/A |
| Timeout fallback | Always | 5s default |

Implementation uses a multi-signal approach:
1. Start timeout timer (configurable, default 5s)
2. Listen for `publishDiagnostics` or `workDoneProgress/end`
3. Mark ready on first signal OR timeout expiration
4. Log warning if timeout was hit (suggests misconfiguration)

### Async Communication and Error Handling

Language server communication uses synchronous stdio which blocks the thread. In kakehashi's async runtime (tokio), this requires `spawn_blocking` to avoid stalling other tasks.

```rust
let result = tokio::time::timeout(
    Duration::from_secs(request_timeout_secs),
    tokio::task::spawn_blocking(move || {
        let conn = pool.get_connection("rust")?;
        conn.request(method, params)
    })
).await;

match result {
    Ok(Ok(response)) => response,
    Ok(Err(e)) => {
        // Server error (e.g., invalid request)
        log::warn!("Bridge request failed: {}", e);
        None
    }
    Err(_) => {
        // Timeout - server may be hung
        log::warn!("Bridge request timed out after {}s", request_timeout_secs);
        None
    }
}
```

#### Error Handling Strategy

| Error Type | Detection | Recovery |
|------------|-----------|----------|
| Server crash | Broken pipe on read/write | Mark connection dead, respawn immediately |
| Request timeout | `tokio::time::timeout` | Return `None`, log warning |
| Malformed response | JSON parse error | Return `None`, log error |
| Server busy | No response within timeout | Return `None`, consider increasing timeout |

Cancellation: When the user moves the cursor before a response arrives, the LSP client typically sends a new request. kakehashi should:
1. Not block waiting for the old response
2. Allow the old request to complete in background (result discarded)
3. Process the new request immediately

### Position Translation

Injection regions exist at specific byte offsets within the host document. The bridge must translate positions bidirectionally:

```
Host Document (Markdown)          Virtual Document (Rust)
┌─────────────────────────┐       ┌─────────────────────────┐
│ # Title                 │       │                         │
│                         │       │                         │
│ ```rust                 │       │fn main() {              │
│ fn main() {             │ ────▶ │    println!("hi");      │
│     println!("hi");     │       │}                        │
│ }                       │       │                         │
│ ```                     │       └─────────────────────────┘
│                         │
│ More text...            │
└─────────────────────────┘

Cursor at line 4, col 5 in host ──▶ line 1, col 5 in virtual
```

#### Translation Details

For a single injection region starting at host line `H` and column `C`:

| Direction | Formula |
|-----------|---------|
| Host → Virtual | `virtual_line = host_line - H`, `virtual_col = host_col - C` (first line only) |
| Virtual → Host | `host_line = virtual_line + H`, `host_col = virtual_col + C` (first line only) |

For multiple injections of the same language in one document, see [ADR-0007](0007-language-server-bridge-virtual-document-model.md) for virtual document strategies.

Translation is straightforward for positions within a single injection. See [ADR-0008](0008-language-server-bridge-request-strategies.md) for complex cases involving cross-file references.

## Consequences

### Positive

- **Full LSP in injections**: Users get completion, hover, diagnostics in code blocks
- **No editor configuration**: Works transparently; editor only talks to kakehashi
- **Leverages existing detection**: Reuses injection detection from Tree-sitter queries
- **Progressive enhancement**: Falls back gracefully to Tree-sitter when servers unavailable
- **Low latency**: Connection pooling enables fast responses after initial spawn
- **Secure by design**: Only user-configured servers are spawned

### Negative

- **Resource overhead**: Multiple language server processes consume memory
- **Complexity**: kakehashi becomes both server and client; protocol translation adds complexity
- **Initial latency**: First request to a language incurs server spawn time (mitigated by eager spawn)
- **Debugging difficulty**: Multi-hop request/response makes troubleshooting harder
- **Configuration burden**: Some servers (rust-analyzer) require non-trivial setup

### Neutral

- **Configuration optional**: Some servers (pyright) work out-of-the-box; others (rust-analyzer) benefit from `initializationOptions` for full functionality
- **Partial feature support**: Not all LSP methods will be bridged (see [ADR-0008](0008-language-server-bridge-request-strategies.md))
- **Server availability**: Graceful degradation when servers not installed

## Implementation Phases

### Phase 1: Infrastructure (Complete)

- [x] Basic LSP client implementation
- [x] Temporary source file creation
- [x] Offset translation
- [x] Go-to-definition working
- [x] `languageServers` configuration at root level (PBI-119)
- [x] Per-host `bridge` filter with map format (PBI-120)

### Phase 2: Connection Pool

- [ ] Server connection pooling
- [ ] Crash recovery and respawn

### Phase 3: Configuration System

- [x] `initializationOptions` passthrough
- [ ] Support for multiple language servers
- [ ] Multi-server routing by capability

### Phase 4: Robustness

- [ ] Ready detection with multiple signals
- [ ] Request timeout handling
- [ ] Startup cleanup of stale temp files

### Phase 5+: Feature Expansion

See [ADR-0008](0008-language-server-bridge-request-strategies.md) for per-method implementation details.

## Related Decisions

- [ADR-0005](0005-language-detection-fallback-chain.md): Language detection applies to both host documents and injection regions
- [ADR-0007](0007-language-server-bridge-virtual-document-model.md): How multiple injections are represented as virtual documents
- [ADR-0008](0008-language-server-bridge-request-strategies.md): Per-method bridge strategies
