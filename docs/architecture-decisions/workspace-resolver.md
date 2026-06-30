# Workspace Resolver

## Context

A bridged language server needs a workspace root to base its `rootUri`/
`workspaceFolders` on at initialization. Today this is decided statically by
`languageServers.<name>.rootMarkers` (see `src/config/settings.rs`, default
`[".git"]` in `src/config/defaults.rs`), resolved upward from the document path
by `src/lsp/bridge/root_markers.rs`.

Two forces push beyond static markers:

1. **Naming drift from the LSP spec.** The spec deprecates `rootPath` in favor
   of `rootUri`, and `rootUri` in favor of `workspaceFolders`. `rootMarkers`
   (a name inherited from Neovim's `vim.fs.root`) reads as the deprecated
   vocabulary.

2. **Static markers cannot make content-dependent decisions.** For
   ecosystems like Deno vs. Node (`deno`/`tsserver`/`tsgo`), *which* server to
   attach — and *whether* to attach at all — can depend on more than the
   presence of a marker file: it may require inspecting the document text or
   running custom logic (e.g. "`package.json` present but no `deno.json`").
   Markers alone, even with priority groups, cannot express this.

kakehashi is **file-first**: bridged servers are spawned on demand when a
document that needs them is opened. There is no document-less activation path —
every workspace decision happens with a concrete triggering document in hand.

## Decision

### 1. Rename `rootMarkers` → `workspaceMarkers`

Adopt `workspaceMarkers` as the configuration key, aligning vocabulary with the
LSP spec's `workspaceFolders`. `rootMarkers` remains a **deprecated serde
alias** (`#[serde(alias = "rootMarkers")]`) for backward compatibility. The
default is unchanged: `[".git"]`. Resolution semantics (upward search, nested
arrays = equal-priority groups) are unchanged.

### 2. Introduce `workspaceResolver` — a Lua function

A new optional key holds a Lua function that dynamically decides both **whether
to attach** and **what workspace** to use:

```toml
[languageServers.<serverName>]
workspaceMarkers = [ ".git" ]            # used only when workspaceResolver is unset
workspaceResolver = """
---@param server_info { name: string, config: table }
---@param document_info { uri: string, path: string, language_id: string, text: string } -- fields resolved lazily via metatable
---@return boolean, nil | string
--- first return  — attach? if false, do not attach this document to this server.
--- second return — workspace folder, a filesystem path (ignored when first is false):
---   nil:    attach without adding a workspace folder.
---   string: attach with the given folder; add it if missing.
--- To resolve a folder from markers, call kakehashi.fs.find_ancestor and return
--- its result, e.g. `return true, kakehashi.fs.find_ancestor(document_info.path, {"deno.json"})`.
return function(server_info, document_info)
    ...
end
"""
```

Key properties:

- **Idempotent function `(server_info, document_info) -> (attach, workspace)`.**
  Not pure — it reads the filesystem and cwd through `kakehashi.fs`/`env` (see
  lua-host-api). The point is statelessness w.r.t. spawn-vs-attach: there is no
  `event` parameter, the resolver does not know — and must not care — whether
  this document is spawning the server or attaching to a running one, and it is
  expected to return the **same result for a given `document_info`** (modulo
  on-disk changes). kakehashi decides how to apply the result from the server's
  current state (below).
- **`document_info` is always present** (file-first model), so it is
  non-optional. Its fields — including `text` — are materialized **lazily via a
  metatable** to avoid copying large documents into Lua when the resolver never
  reads them.
- **`workspaceMarkers` is the fallback**, used only when `workspaceResolver` is
  unset. They are not layered; a configured resolver fully governs the decision.
- **The second return is always a filesystem path (or nil)** — never raw
  markers. A resolver that wants marker-based resolution calls
  `kakehashi.fs.find_ancestor` itself (see lua-host-api), so there is one
  folder-from-markers entry point (`root_markers.rs`), reached either by the
  `workspaceMarkers` fallback or by an explicit `find_ancestor` call — not by a
  second, return-value-shaped path.

### 3. Execution model: synchronous return-style on a worker thread

The resolver runs **synchronously** (return-style, not continuation-passing) off
the tokio runtime, so a blocking resolver cannot stall the LSP event loop — the
async-friendliness is achieved at the Rust↔Lua boundary, not inside Lua.

- **Thread ownership.** `mlua::Lua` is `!Send` by default, so the VM cannot be
  moved across threads or held across an `await`. A reusable compiled VM
  therefore lives on a **dedicated thread** that owns it and receives requests
  over a channel (not an ad-hoc `spawn_blocking` per call, which would force
  re-creating the VM and a `Send` bound on every `kakehashi.*` callback).
- **Timeout is in-VM, not wall-clock-only.** Wrapping the call in
  `tokio::time::timeout` (or dropping the join handle) only stops *awaiting* the
  result; the Lua code keeps running and keeps occupying the worker thread. Real
  interruption requires an **in-VM hook** — `Lua::set_hook` with
  `HookTriggers::every_nth_instruction` (or Luau `set_interrupt`) that checks
  elapsed time and raises. This bounds Lua-level loops.
  - **Caveat:** an instruction hook cannot interrupt time spent inside a host
    C-call (`kakehashi.fs.find_ancestor`'s filesystem walk, `read_to_string`)
    or the lazy `document_info` metatable `__index`. Those host operations must
    carry **their own bounds** (e.g. a capped `read_to_string`, see
    lua-host-api) — the §6 "must not tie up a worker thread" guarantee depends
    on both the hook and these host-side limits.
- The `text` snapshot is **owned in Rust** (`Arc<str>` or equivalent) before
  entering Lua; the metatable only defers *materialization into Lua*, never
  performs async I/O. A metatable `__index` must not await (and, per the caveat,
  must itself be cheap/bounded).

### 4. Veto and wiring

- **Veto (`attach = false`)** is applied as a filter **after** the
  language-based server selection in `src/lsp/bridge/coordinator.rs` (the
  `c.languages.iter().any(...)` filters). Language match first, resolver
  refinement second.
- **Result application is inferred from server state**, not signaled by the
  resolver:
  - Server not yet running, this document triggers the spawn → resolved
    workspace **seeds `InitializeParams`** via `build_initialize_request`
    (`protocol/lifecycle.rs`), replacing the `marker` argument that
    `workspace_from_marker` (defined in `root_markers.rs`, called from `pool.rs`)
    would otherwise supply.
  - Server already running, a new document arrives → a returned folder is added
    via `workspace/didChangeWorkspaceFolders` (the `WorkspaceFolderSet` in
    `pool.rs`). **Precondition:** this only reaches a server that advertised
    `workspace.workspaceFolders.changeNotifications` (or dynamically registered
    it); the implementation already gates on `supports_workspace_folder_changes`,
    so for a non-supporting running server a resolver's returned folder is
    **silently dropped**. See Open Questions.

### 5. Cache contract

The resolver is evaluated **once, at didOpen attach time**, and the resolved
workspace is fixed to the connection-pool key. It is **not re-evaluated on
didChange** — server routing does not change mid-edit. This bounds resolver
cost to O(documents attached), not O(edits), despite the resolver depending on
document text.

### 6. Error policy

A Lua error **or** a timeout overrun is treated as **fail-closed**: do not
attach the document to that server, and log. A broken or slow resolver must not
silently misroute. "Tie up a worker thread indefinitely" is prevented by the
in-VM hook **plus** the host-side bounds of §3 — a hook alone cannot stop a
runaway host call, so both are load-bearing for this guarantee.

### 7. Open questions

Deferred to implementation, recorded so they are not lost:

- **Timeout default value.** Not fixed here; must be generous enough for a
  filesystem walk yet bound a misbehaving resolver.
- **`read_to_string` size cap.** Per §3 the in-VM hook cannot interrupt a host
  read, so `kakehashi.fs.read_to_string` needs its own large-file bound (see
  lua-host-api); the exact cap is open.
- **Non-`file://` documents.** The "file-first" framing assumes a filesystem
  path. What `document_info.path` (and the path-based host API) yield for
  `untitled:` and other non-`file://` URIs is undefined; likely the resolver
  should be skipped (fall back to `workspaceMarkers`/no folder) for those.
- **Folder dropped on a non-supporting running server** (§4): whether to fall
  back (e.g. respawn with the folder, or surface a warning) instead of silently
  dropping is open.

## Considered Options

- **Pure declarative extensions (negative markers / `requireWorkspace`).**
  Rejected as the sole mechanism: it can express "`package.json` but not
  `deno.json`" but cannot inspect document *content*, which is a stated
  requirement. `workspaceMarkers` is retained as the declarative common case;
  the resolver is the escape hatch for what declarations cannot express.

- **Continuation-passing `on_dir(root_dir)` callback (Neovim's `root_dir`
  form).** Neovim uses CPS because its Lua runs on the editor's single-threaded
  main loop, where blocking freezes the UI; calling `on_dir` later lets the
  resolver do async work, and *not* calling it doubles as the skip signal.
  kakehashi has neither constraint: Lua runs on a worker thread, so a
  synchronous resolver does not block the event loop, and an explicit
  `attach = false` is a safer veto than Neovim's "forgot to call `on_dir` →
  silent no-attach" footgun. The single lesson imported from Neovim is the
  **timeout**, not the callback shape.

- **Single overloaded return (`false | nil | string | table`).** An earlier
  shape folded attach-or-not and the workspace value into one return.
  Rejected for the explicit 2-tuple `(attach, workspace)`: four return shapes
  on one value are error-prone to author in Lua.

- **Marker-array second return (`(string | string[])[]`).** The 2-tuple's
  workspace slot once also accepted raw markers, which kakehashi would resolve
  via `root_markers.rs`. Dropped once `kakehashi.fs.find_ancestor` exposed that
  same resolution to Lua: the return form was a redundant second entry point to
  one implementation. Folding it away makes the second return uniformly a path
  (or nil), simplifies the boundary, and lets the resolver author control the
  no-marker-found fallback explicitly instead of relying on an implicit
  kakehashi rule.

- **`event: "initialize" | "didOpen"` parameter.** Considered so the resolver
  could branch on spawn vs. attach. Rejected: in the file-first model both
  carry a document, and folding the spawn/attach distinction into kakehashi's
  state (rather than the resolver's logic) keeps the resolver an idempotent,
  stateless function and removes a state-management responsibility from
  resolver authors.

- **`workspaceFallback` (e.g. `"$PWD"`) when neither markers nor resolver
  resolve.** Deferred — not part of this decision. A resolver can already
  return a literal string for the same effect; revisit only if a purely
  declarative fallback proves needed.

## Consequences

### Positive

- Vocabulary aligns with the LSP spec (`workspaceFolders`), with a no-break
  deprecation alias.
- Content-dependent server selection (Deno vs. Node/tsgo) becomes expressible.
- The resolver is an idempotent, stateless function; spawn-vs-attach routing
  stays in kakehashi, not in user Lua.
- One folder-from-markers implementation, shared by markers and resolver.

### Negative

- **Introduces a Lua VM as a new dependency** (e.g. `mlua`). Today only
  `lua-pattern` (Lua-pattern→regex) is present; there is no general Lua
  runtime. This changes the nature of config from purely declarative data to
  embedded executable code, with the attendant sandboxing, sync/async-boundary,
  and timeout machinery.
- Resolver correctness depends on author discipline (idempotence) that the type
  signature cannot enforce.

### Neutral

- Resolver evaluation is off the request hot path (attach time only) and capped
  by timeout, so it does not affect semantic-token or diagnostic latency.
- `workspaceResolver` and `workspaceMarkers` are either/or per server, not
  layered.

## Decision–Implementation Gap

Not yet implemented. As of this record:

- The config key is still `rootMarkers`; the rename and `#[serde(alias)]` are
  pending.
- No Lua VM dependency is present; `workspaceResolver` evaluation, the lazy
  `document_info` metatable, the worker-thread/timeout harness, the
  coordinator-level veto, and the cache contract are all unbuilt.

## Related Decisions

- [lua-host-api](lua-host-api.md): the `kakehashi.*` Lua functions the resolver calls (`path`, `fs` incl. `find_ancestor`, `env.current_dir`) and the path-vs-URI boundary.
- [language-server-bridge](language-server-bridge.md): the bridge this resolver feeds.
- [ls-bridge-server-pool-coordination](ls-bridge-server-pool-coordination.md): pool keys and spawn-time workspace seeding the resolver result must integrate with.
- [wildcard-config-inheritance](wildcard-config-inheritance.md): how `languageServers._` defaults (including `workspaceMarkers`/`workspaceResolver`) are inherited.
