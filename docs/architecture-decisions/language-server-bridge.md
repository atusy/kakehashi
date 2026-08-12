# Language Server Bridge

## Decision–Implementation Gap

As recorded, only Phase 1 (bridge infrastructure with working go-to-definition)
was complete. Per-method coverage has since expanded well beyond this; see
[docs/README.md](../README.md) for the current list of bridge-backed requests.

Two of this record's original mechanisms never shipped, and the architecture
diagram, the eager-spawn diagram, and the Phase 1 checklist below still show
them. Injection content goes to **virtual document URIs**, not temporary
files on disk (language-server-bridge-virtual-document-model), so the
`TempFileManager` those diagrams name does not exist; and readiness is the
**LSP handshake**, not a multi-signal indexing detector. § Provisioning Flow
and § Ready Detection are corrected; the diagrams are left as the historical
record this file is.

## Context

*Framing, as of when this decision was taken.* Markdown code blocks and other injection regions (e.g., JavaScript inside HTML `<script>` tags, SQL in string literals) received only Tree-sitter-based features from kakehashi. While Tree-sitter provides excellent syntax highlighting via semantic tokens, injection regions lacked access to full LSP capabilities such as:

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
- **Crash recovery**: Detect dead servers (broken pipe, exit code) and mark the connection `Failed`; the next acquire respawns it (not a timer — bridge-client-control-protocol)

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
| `languageServers.*.languages` | No (inherits from `_`) | Languages this server handles. The element `"*"` matches every language, for servers not tied to one (any-language-server-wildcard). An omitted list is *not* "any": it inherits from the `_` entry, and an empty one says the server handles nothing (wildcard-config-inheritance) |
| `languageServers.*.initializationOptions` | No | Passed to server's `initialize` request |
| `languageServers.*.onTypeFormattingTriggers` | No | Trigger characters for bridged `textDocument/onTypeFormatting`; the sorted union is advertised at initialize, and requests are forwarded only when the downstream also declares the typed character (#354) |
| `languageServers.*.enabled` | No | Whether this server is eligible to spawn/use at all. Default `true`; inheritable via the `_` wildcard, so `_.enabled: false` disables every server by default while individual servers opt back in with `enabled: true` |

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
- **Cross-cutting servers**: `typos-lsp` provides diagnostics for multiple languages — or for *any* language via `languages = ["*"]` (any-language-server-wildcard)
- **Multiple servers per language**: `pyright` + `ruff` for Python (both in `languageServers`)
- **Independent lifecycle**: Tree-sitter config doesn't affect server spawning
- **Per-host filtering**: Each host language can selectively enable/disable bridging via `bridge` field

### Temporary File Management

> **Superseded, non-normative — retained as history.** This whole section
> describes a design that never shipped. Injection content is not written to
> disk: the bridge mints virtual document URIs and sends in-memory content
> (§ Provisioning Flow, language-server-bridge-virtual-document-model).
> Nothing below is a current requirement.

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

> **Superseded in its mechanism, non-normative — retained as history.** The
> *problem* is real and still shapes the design: servers differ in what
> project context they need. The *solution* recorded below routes through
> temp files kakehashi no longer writes (§ Provisioning Flow). Treat the
> per-server requirements as accurate and the temp-path instructions as
> historical.

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

Both halves of this record's original provisioning design were **superseded
before they shipped**, and the sections above still describe the world they
assumed — see § Decision–Implementation Gap.

1. Initialize the server with user-provided `initializationOptions`.
2. Complete the **LSP handshake** — the initialize response processed and
   `initialized` sent — and only then treat the connection as ready. No
   document notification may precede this (ls-bridge-message-ordering), and
   there is no indexing detector beyond it.
3. Mint a **virtual document URI** for the injection region and send its
   current in-memory content in `didOpen`. Regions are not written to
   temporary files, and there is nothing to clean up on close beyond the
   `didClose`. language-server-bridge-virtual-document-model carries this
   decision; the servers that genuinely need a real path on disk are its
   subject, not this one's.

#### Ready Detection

A server is ready when its handshake completes, and not by any later signal.
The original design instead tried to detect *indexing* completion by waiting
on `publishDiagnostics`, `window/workDoneProgress` end, or a timeout
fallback. That was rejected in practice: the signals are per-server
unreliable (many servers publish no diagnostics for valid code), and a
readiness gate built on them delays every first request by a timeout
whenever the guess is wrong. Requests instead go out as soon as the
handshake completes and are bounded by the timeout tiers of
ls-bridge-timeout-hierarchy.

### Async Communication and Error Handling

A bridged request never waits on a downstream server indefinitely: a server
error and an expired wait each log a warning and yield no result *from that
server*, so a hung server costs one timeout, never a stalled handler.

Whether that becomes a failed host request depends on the aggregation
strategy, and is cross-layer-aggregation's and
ls-bridge-server-pool-coordination's subject rather than this one's. In
outline: a `preferred` fan-in falls through to the next candidate and only
surfaces an error when nothing answered; a `concatenated` aggregation fails
if a layer it required fails; and a fan-out whose downstreams all fail
answers `REQUEST_FAILED`. Degradation is per-server, not per-request. (Which bound applies — the Tier-1
per-request timeout, which is Phase 3 and only engages for a multi-server
fan-out, the Tier-2 liveness timeout otherwise — is
ls-bridge-timeout-hierarchy's subject, along with the requests deliberately
exempt from both. The lifecycle handshake is exempt in the other direction:
the `shutdown` request's response wait is bounded by the shutdown deadline,
not by any request timeout.)

Communication is **pure async** — no blocking stdio, and no OS thread per
connection. ls-bridge-async-connection carries that decision, its reader and
writer task patterns, and the timeout tiers this bound sits in.

#### Error Handling Strategy

| Error Type | Detection | Recovery |
|------------|-----------|----------|
| Server crash | Broken pipe on read/write | Mark connection `Failed`; the next acquire respawns |
| Request timeout | Response wait expires | Return `None`, log warning |
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

For multiple injections of the same language in one document, see language-server-bridge-virtual-document-model for virtual document strategies.

Translation is straightforward for positions within a single injection. See language-server-bridge-request-strategies for complex cases involving cross-file references.

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
- **Partial feature support**: Not all LSP methods will be bridged (see language-server-bridge-request-strategies)
- **Server availability**: Graceful degradation when servers not installed

## Implementation Phases

### Phase 1: Infrastructure (Complete)

> **Historical ledger, not a status report.** It records what was done at the
> time of writing, including the temporary-file approach later superseded by
> virtual documents, and it predates the pooling and crash recovery that have
> since shipped. See § Decision–Implementation Gap.

- [x] Basic LSP client implementation
- [x] Temporary source file creation (superseded by virtual documents)
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

See language-server-bridge-request-strategies for per-method implementation details.

## Related Decisions

- [language-detection-fallback-chain](language-detection-fallback-chain.md): Language detection applies to both host documents and injection regions
- [language-server-bridge-virtual-document-model](language-server-bridge-virtual-document-model.md): How multiple injections are represented as virtual documents
- [language-server-bridge-request-strategies](language-server-bridge-request-strategies.md): Per-method bridge strategies
- [any-language-server-wildcard](any-language-server-wildcard.md): The `languages = ["*"]` marker for servers not tied to one language
- [bridge-routing-protocol](bridge-routing-protocol.md): Extends the security model's trust boundary — a configured downstream provider may influence routing within recorded bounds
