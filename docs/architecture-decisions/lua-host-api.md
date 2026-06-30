# Lua Host API

## Context

workspace-resolver introduces a user-authored Lua function that decides whether
to attach a document to a bridged server and what workspace to use. To make
real decisions (Deno vs. Node/tsgo, content-dependent attach), the resolver
needs to inspect paths, look at files on disk, and find a project root.

Two boundaries shape what kakehashi must provide:

1. **The resolver runs in a restricted sandbox.** For determinism, safety, and
   the worker-thread/timeout execution model (see workspace-resolver), the Lua
   environment is stripped: `string`, `table`, `math` stay; `io`, `os`,
   `package`, `require`, `load`/`loadstring`, `dofile`, `debug`, and FFI are
   removed (also `string.dump`; `collectgarbage` left read-only or removed).
   `debug` is on the list deliberately — `debug.getupvalue`/`setupvalue`/
   `sethook`/`getregistry` are introspection and sandbox-escape vectors. So
   *any* host access — filesystem, working directory — must be an explicit
   kakehashi-provided function, not Lua's stdlib.

2. **Lua's `string` library is already powerful.** Splitting, matching, and
   substituting path-like strings is trivial with `string.match`/`gsub`. We do
   not want to wrap what Lua already does well.

The risk is an ever-growing host API. This decision fixes a **minimal surface**
and the **inclusion criteria** that gate future additions.

## Decision

### Inclusion criteria

A `kakehashi.*` function is provided only if it meets at least one of:

1. **Correctness Lua stdlib cannot guarantee** — e.g. cross-platform path
   semantics matching Rust's `std::path`, URI percent-decoding.
2. **Sandboxed host access** the stripped environment otherwise denies —
   filesystem reads, working directory.
3. **Reuse of non-trivial kakehashi Rust logic** the resolver should not
   reimplement — root finding / marker resolution.

Pure string conveniences that Lua's `string` library does trivially are
**excluded** on principle.

### Submodules only, mirroring Rust `std`

`kakehashi.*` exposes **only submodules** — no bare top-level functions — and
each submodule mirrors a Rust `std` module, keeping one naming language:

- `kakehashi.path` ← `std::path` — **pure**: deterministic string functions, no I/O.
- `kakehashi.fs` ← `std::fs` — **impure**: anything that touches the disk,
  including the `find_ancestor` walk.
- `kakehashi.env` ← `std::env` — **impure**: ambient process environment
  (`current_dir`; env-var reads if later promoted from deferred).
- `kakehashi.log` — diagnostics output. The one deliberate exception to the
  std-mirror rule: there is no `std::log` (logging is the `log` crate's
  facade), but a sanctioned output channel is needed since `print`/`io` are
  stripped.

The `path`/`fs` divide is the **purity line**, which keeps "any `path.X` is
side-effect-free" a guarantee a reader can rely on.

### Surface (initial)

All functions are snake_case (matching Rust and the resolver examples). A
return of `nil` corresponds to Rust's `None`.

#### `kakehashi.path` — pure path manipulation (no I/O)

Thin wrappers over `std::path::Path`, operating on OS-native path strings so
separator and root handling are correct on every platform. Justified by
criterion 1 (and supplies the `parent`/`ancestors` primitives root-finding
needs); deliberately pure and side-effect-free.

| Function | Returns | Rust analogue |
|---|---|---|
| `path.is_absolute(p)` | `boolean` | `Path::is_absolute` |
| `path.is_relative(p)` | `boolean` | `Path::is_relative` |
| `path.parent(p)` | `string \| nil` | `Path::parent` |
| `path.ancestors(p)` | `string[]` (self first → root last) | `Path::ancestors` |
| `path.file_name(p)` | `string \| nil` | `Path::file_name` |
| `path.file_stem(p)` | `string \| nil` | `Path::file_stem` |
| `path.file_prefix(p)` | `string \| nil` | `Path::file_prefix` |
| `path.extension(p)` | `string \| nil` | `Path::extension` |
| `path.join(base, ...)` | `string` | `Path::join` (variadic for convenience) |

`nil`-returning cases mirror Rust exactly: `parent("/")` → `nil`,
`extension("README")` → `nil`, etc. All wrapped methods are stable since Rust
1.0 (`ancestors` since 1.28) **except `Path::file_prefix`, stabilized in 1.91**
— including it sets an implicit MSRV floor of 1.91. The project currently pins
no MSRV and builds on ≥1.95, so this is a recorded caveat, not a blocker; drop
`file_prefix` (it overlaps `file_stem`) if an older toolchain must be supported.

#### `kakehashi.fs` — read-only host filesystem (criteria 2 and 3)

The sandbox removes `io`/`os`, so content-dependent decisions (does
`deno.json` exist? what is in `package.json`?) require explicit, **read-only**
accessors. Root finding lives here too: it is fundamentally an upward walk of
the filesystem, so it belongs with the other impure disk operations rather than
in the pure `path` module.

| Function | Returns |
|---|---|
| `fs.exists(p)` | `boolean` |
| `fs.is_file(p)` | `boolean` |
| `fs.is_dir(p)` | `boolean` |
| `fs.read_to_string(p)` | `string \| nil` (nil on missing / unreadable / non-UTF-8 / over size cap) |
| `fs.find_ancestor(start, markers)` | `string \| nil` (the matching directory) |

`fs.read_to_string` is **size-capped**: the workspace-resolver in-VM timeout
hook cannot interrupt a host read, so an unbounded read of a huge file could
block the worker thread or OOM. Over the cap it returns `nil` (treated like
unreadable). The cap value is an open question (see workspace-resolver).

`fs.find_ancestor` is a **general upward-search primitive**, not a
workspace-specific function — finding a project root is just one use. It reuses
`src/lsp/bridge/root_markers.rs` (criterion 3) rather than having every caller
reimplement the walk:

- `start`: a path. The search's **base directory** is `start` itself when it is
  an existing directory, or `start`'s parent otherwise — including when `start`
  does **not** exist (e.g. an unsaved/new document path), which is treated as a
  file path, matching `find_marker_root`. The walk checks the base directory
  **and** each ancestor.
- `markers`: `(string | string[])[]` — a nested array is an equal-priority
  group. This is the **same shape** as `workspaceMarkers`, but the function
  does not assume the result is a workspace. (The resolver's return no longer
  accepts raw markers; it calls this function instead — see workspace-resolver.)
- Returns the first directory (base, then ancestors) containing a marker, or
  `nil`.

> **Implementation note:** the reused `root_markers.rs::find_marker_root` takes
> a *document file* and starts at `parent().ancestors()`, so it never inspects
> the passed path itself. Generalizing it to `fs.find_ancestor` must check the
> base directory when `start` is an existing directory — otherwise a marker
> sitting *in* a directory `start` (e.g.
> `find_ancestor(kakehashi.env.current_dir(), …)`) is missed. The file-vs-dir
> test reads filesystem metadata; a nonexistent `start` skips the probe and is
> treated as a file path (base = parent).

Named for what it returns (a directory among the base + ancestors) and the
mechanism, **not** for the workspace use case — `find_workspace`/`find_root`
would over-narrow a reusable primitive to one consumer. A resolver can combine
it with content checks imperatively — e.g. find the `package.json` directory,
then branch on whether `deno.json` sits beside it.

No writes, no spawning. Reads and walks are **point-in-time**, consistent with
the evaluate-once-at-didOpen cache contract in workspace-resolver: a later
change to a file the resolver read does not re-trigger resolution.

#### `kakehashi.env` — ambient process environment (criterion 2)

Mirrors `std::env`. Initially one function:

| Function | Returns |
|---|---|
| `env.current_dir()` | `string` — the process working directory, wrapping `std::env::current_dir()` (`getcwd(2)`) |

`env.current_dir()` subsumes the `workspaceFallback`/`"$PWD"` idea deferred in
workspace-resolver: a resolver can simply `return true, kakehashi.env.current_dir()` as
its own fallback, so no separate declarative key is needed. Grouping it under
`env` (rather than a bare `kakehashi.current_dir()`) keeps `kakehashi.*` submodule-only
and gives the deferred env-var reads (`env.var("DENO_DIR")`) a natural home —
exactly the `current_dir`/`var` pairing `std::env` itself uses.

`env.current_dir()` cannot be replaced by `os.getenv("PWD")`: `os` is stripped from the sandbox,
and even unsandboxed `$PWD` is an unreliable proxy — a shell convention that may
be unset when the server is exec'd directly, is not updated by `chdir()` (so it
can be stale), and does not exist on Windows. `getcwd(2)` is the correct,
cross-platform source.

#### `kakehashi.log.{debug,info,warn,error}(msg)` — diagnostics

Because the resolver fails closed (a Lua error or timeout silently does not
attach, per workspace-resolver), authors need a way to see why. These route to
the server log. Included as the one concession to debuggability; not for
control flow.

### Paths, not URIs, at the Lua boundary

`document_info.uri` is a `file://` URI, but the host API speaks **OS paths**.
To keep resolvers in path-space and avoid a URI module:

- `document_info.path` is exposed (lazily, via the same metatable as `text`) as
  the already-decoded OS path of the document.
- The resolver's **string workspace return is interpreted as a filesystem
  path**; kakehashi converts it to a `file://` `WorkspaceFolder` URI at the
  boundary — the same conversion marker resolution already performs. This
  pins down the otherwise-ambiguous "folder" string in workspace-resolver.

Percent-decoding and `file://` handling thus happen once, in Rust, not in every
resolver.

#### Path encoding contract

Lua strings are arbitrary **byte** strings, but Rust `Path`/`OsStr` are not
universally byte-representable, so the boundary needs a defined encoding:

- **Unix:** an `OsStr` path *is* bytes, so paths cross the boundary losslessly
  as raw byte strings — including non-UTF-8 paths (which Lua holds fine, though
  `string.*` pattern functions may behave oddly on them).
- **Windows:** paths are UTF-16 and are passed as UTF-8 (WTF-8). A path that is
  not representable (an unpaired surrogate) **fails closed** — the host function
  returns `nil`/error rather than handing back a lossy, non-round-trippable
  string. Callers that pass a path string back to `kakehashi.fs`/`path` get
  byte-for-byte the value they received, so round-tripping holds for every path
  that crossed the boundary in the first place.

This is a recorded contract, not yet implemented; the exact Windows policy is
an open question (see workspace-resolver Open Questions).

## Considered Options

- **A `kakehashi.uri` module (`to_path`/`from_path`).** Rejected for the
  initial surface: exposing `document_info.path` and accepting path returns
  moves all URI↔path conversion to the kakehashi boundary, so typical resolvers
  never touch URIs. Revisit only if a resolver must synthesize sibling URIs
  directly.

- **No path module; rely on Lua `string`.** Rejected. `string.match` can fake
  `parent`/`extension`, but gets platform separators, trailing slashes, and
  root edge cases wrong, and `ancestors` (the root-finding workhorse) is fiddly
  to write correctly. Matching `std::path` semantics once, in Rust, is worth
  the thin wrapper.

- **Folding `fs` (and `find_ancestor`) into `path`.** Rejected. The split is
  on purity, not topic: `path.*` is pure and side-effect-free, which makes it
  independently testable and lets a reader assume `path.X` never touches the
  disk. Mixing in `exists`/`read_to_string`/`find_ancestor` would destroy that
  invariant. `find_root` *was* merged — but into `fs` as `fs.find_ancestor`,
  because it is a filesystem walk, not a string operation.

- **`os.getenv("PWD")` instead of `kakehashi.env.current_dir()`.** Rejected — see the
  env section: `os` is stripped from the sandbox, and `$PWD` is an unreliable,
  non-portable proxy for `getcwd(2)`.

- **`kakehashi.uv.cwd()` (mirroring Neovim's `vim.uv`).** Rejected. `vim.uv` is
  a libuv binding; naming a module `uv` promises the libuv surface
  (timers, spawn, tcp, fs_event) that the sandbox deliberately withholds, and
  it breaks from this API's Rust-`std` naming language (`path`/`fs`/`env`).
  `cwd` lives where Rust itself puts it — `std::env::current_dir` →
  `kakehashi.env.current_dir()`.

- **Bare `kakehashi.current_dir()` (top-level function).** Rejected in favor of
  `kakehashi.env.current_dir()`: keeping `kakehashi.*` submodule-only avoids a grab-bag
  of loose functions and gives env-var reads a home next to `current_dir`.

- **Naming the walk `find_workspace` / `find_marker` / `find_root`.** Rejected
  for `find_ancestor`. The function is a general upward-search primitive that
  returns an ancestor *directory*; finding a workspace root is one use, not its
  definition. `find_workspace`/`find_root` over-narrow it to a single consumer,
  and `find_marker` misreads as returning the marker. `find_ancestor` names the
  return type and pairs with `path.ancestors`.

- **Writable `fs` / `os.execute` / env-var reads.** Rejected for now. Read-only
  fs covers the known use cases; writes and subprocess spawning enlarge the
  attack and nondeterminism surface against the timeout/cache model. General
  env-var access (`kakehashi.env.var(...)`, e.g. `DENO_DIR`) is plausible future
  work — the `env` module already exists for `current_dir`, so promoting it is a small
  addition — but is deferred under the minimal-surface principle.

- **Leaving the Lua stdlib intact (`io`/`os` available).** Rejected. It would
  make resolvers nondeterministic, unsandboxed, and able to block or escape the
  worker-thread model. All host access must be explicit and gated.

## Consequences

### Positive

- Small, principled surface; the inclusion criteria give a clear test for
  future additions.
- Resolvers stay in path-space; URI handling is centralized and correct.
- `fs.find_ancestor` reuse means one upward-search implementation shared by
  markers, resolvers, and any future caller.
- Two modules split on purity (`path` pure, `fs` impure) — fewer to learn, and
  `path.*` stays a side-effect-free, independently testable layer.
- `kakehashi.env.current_dir()` retires the deferred `workspaceFallback` cleanly.
- `kakehashi.*` is submodule-only and each submodule mirrors a Rust `std`
  module (`path`/`fs`/`env`; `log` is the one documented exception), so there
  is one naming language and no bare top-level functions to accrete.

### Negative

- Every host capability a resolver needs must be anticipated and wrapped; an
  unforeseen need means a new ADR-gated API rather than reaching for Lua stdlib.
- Read-only fs reads make resolution disk-dependent (already true of marker
  checking); combined with evaluate-once caching, stale on-disk state is not
  re-observed until re-attach.

### Neutral

- The `kakehashi.*` namespace is introduced for workspace resolving but is
  designed to host future Lua-extensible features under the same criteria.
- `path.*` is pure and could be unit-tested independently of the resolver.

## Decision–Implementation Gap

Not yet implemented. No Lua VM dependency exists yet (only `lua-pattern`), so
the sandbox, the `kakehashi.*` modules, `document_info.path`, and the
path-return conversion are all unbuilt. This ADR fixes the intended surface
ahead of that work.

## Related Decisions

- [workspace-resolver](workspace-resolver.md): the consumer of this API; defines the resolver signature, sandbox/timeout execution model, and cache contract this surface assumes.
