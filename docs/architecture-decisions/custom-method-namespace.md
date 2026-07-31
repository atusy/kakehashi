# Custom Method Namespace

**Related Decisions**:
- [node-reference-protocol](node-reference-protocol.md) — The `kakehashi/textDocument/node/*` family this names
- [captures-protocol](captures-protocol.md) — The `kakehashi/textDocument/captures/*` triple this names

## Context

Kakehashi exposes 42 custom LSP methods. They accumulated under a flat
vendor-first shape — `kakehashi/<feature>` — matching what most servers do
(`rust-analyzer/expandMacro`, `$ccls/vars`, `metals/…`).

Forty-one of them are document-scoped: every one takes a `textDocument`
parameter, including the id-based node accessors, which carry it to scope the
ULID lookup. One is not: `kakehashi/internal/effectiveConfiguration` takes
empty params and reports server state.

That asymmetry is not a curiosity — it is the beginning of a second axis. A
captures-kind discovery method (see captures-protocol § Kind resolution) and
anything else that reports on the server rather than a document will land on
the non-document side. Under a flat scheme those methods sit as siblings of the
document-scoped ones with nothing in the name distinguishing them.

LSP itself resolved the same tension by putting **scope first and feature
second**: `textDocument/diagnostic` vs `workspace/diagnostic`,
`textDocument/documentSymbol` vs `workspace/symbol`,
`textDocument/semanticTokens/full` vs `workspace/semanticTokens/refresh`. The
scope segment is a constant across most spec methods and is not considered
waste — it is what lets the two sides of a feature coexist without one of them
being renamed later.

## Decision

Adopt LSP's scope-first axis inside the vendor namespace:
`kakehashi/<scope>/<feature>`.

- The 41 document-scoped methods move under `textDocument/`:
  `kakehashi/node` → `kakehashi/textDocument/node`,
  `kakehashi/node/parent` → `kakehashi/textDocument/node/parent`,
  `kakehashi/captures/full` → `kakehashi/textDocument/captures/full`, and so on.
- Future server-scoped methods take `kakehashi/workspace/<feature>`.
- `kakehashi/internal/effectiveConfiguration` keeps its name. `internal/`
  marks *visibility* — "not public API" — which is a different axis from
  scope, and a method outside the public protocol does not owe the public
  protocol's naming rule.

**The vendor prefix stays outermost.** `kakehashi/textDocument/node`, not
`textDocument/node`.

**Deprecation window.** The old names remain callable. A tower middleware
(`src/lsp/method_alias.rs`) rewrites a deprecated name to its canonical
spelling before any other layer sees the request, and warns once per distinct
old name under the `kakehashi::deprecated` log target. They may be removed in a
future release.

Three properties of that middleware are load-bearing:

1. **It is the outermost layer, above `IngressOrderGate`.** The gate assigns
   per-document wire-order tickets by matching on the method name and knows
   only the canonical spellings. Below the gate, a deprecated call would arrive
   unrecognized, fall through ungated, and read a tree missing edits that
   preceded it on the wire.

   Both orders compile and both leave every request answerable, so the mistake
   is invisible at the call site. The composition therefore lives in one
   library function, `lsp::ingress_stack`, which `main.rs` calls and a unit
   test drives — the shipped order is the one under assertion. A test that
   assembled its own stack would have pinned the property while leaving the
   wiring free to regress.
2. **The mapping is an explicit allowlist, not a prefix rule.** A blanket
   `kakehashi/` → `kakehashi/textDocument/` rewrite would corrupt
   `internal/effectiveConfiguration`, and would invent deprecated spellings for
   methods that never had one. The list is frozen at the rename: methods added
   afterwards never had an old name to alias.

   Freezing it opens one hazard the list cannot close by itself. Nothing
   couples it to the registrations in `main.rs`, so renaming or removing a
   canonical method would leave its alias rewriting into a name that no longer
   exists — and the client would get `MethodNotFound` naming a method it never
   sent, which is worse than the error it would have got without the alias. No
   unit test can see both sides: the registrations live in the binary and
   tower-lsp's method map is private. An e2e test therefore walks all 41 old
   spellings over the wire and asserts none answers `-32601`.

3. **The notice goes to `window/logMessage`, not just the server log.** An
   earlier revision emitted `log::warn!` only, which is invisible in a default
   run: env_logger takes its level from `RUST_LOG` and filters at `Error` when
   it is unset, and it writes to the server's stderr rather than the editor's
   LSP output. The audience is client authors reading that output.

   `window/showMessage`, which the config-key deprecations use, is the wrong
   instrument here — up to 41 popups in a session. `logMessage` is the LSP log
   itself and carries no popup. The `Client` reaches the layer through a
   `OnceLock` filled inside `LspService::build`'s factory closure, since that
   closure is the only place the handle exists; the notification is spawned
   rather than awaited so the request path stays synchronous into the ordering
   gate.

## Considered Options

### Keep the flat `kakehashi/<feature>` shape

Shortest names, and matches the common vendor-extension convention. Rejected
because it has no place to put the server-scoped side of a feature: a
document-scoped `captures/full` and a server-scoped `captures/kinds` would be
siblings, and the distinction would live only in documentation.

The argument that `textDocument/` is "a constant, not a namespace" because it
would appear on 41 of 42 methods proves too much — it equally condemns LSP's
own `textDocument/`, which covers the large majority of spec methods.

### Extend the spec namespace directly: `textDocument/captures/full`

Shortest of all, and there is real precedent — clangd ships
`textDocument/switchSourceHeader`, `textDocument/ast`, and
`textDocument/symbolInfo`. Rejected because an unprefixed name can be claimed
by a future LSP version, leaving a vendor method and a spec method with the
same name and different shapes. The risk is worst exactly where this proposal
would reach next: `workspace/` is densely used by the spec.

Keeping `kakehashi/` outermost also preserves a rule clients can apply without
a lookup table: everything under `kakehashi/` is a kakehashi extension.

### Rename with no compatibility window

The project is in beta and breaking changes are allowed without deprecation,
so this was available. Rejected because the cost of the alias is one small
middleware and a frozen list, while the benefit is that no client breaks on
upgrade — and these methods exist specifically to be built on by third-party
clients (node-reference-protocol § Context). Charging an integration cost to
the very audience the protocol is courting is a poor trade for the code it
saves.

## Consequences

### Positive

- Server-scoped methods have an obvious home before the first one is written,
  so no second rename is needed when one appears.
- Every existing client keeps working; migration is at the client's pace.
- The naming rule is mechanical, so new methods need no case-by-case debate.

### Negative

- Names are 13 characters longer. `textDocument/semanticTokens/full/delta`
  shows the spec already tolerates four segments, so this is a readability
  cost, not a structural one.
- Two spellings exist for the same method until the window closes, which
  documentation and examples must not drift between.

### Neutral

- The rename is a **behavioral** change by this project's definition (the LSP
  protocol interface is the only API that matters), not a Tidy First
  structural one — even though no client breaks, the wire surface changed.
- The frozen alias list is deliberately not derived from the live registration
  list in `src/bin/main.rs`. They will diverge as methods are added; that
  divergence is correct, and it is one-directional — registered-but-not-aliased
  is the intended state, aliased-but-not-registered is the bug the e2e sweep
  catches. (Deriving it is also not cheap: `custom_method` takes a distinct
  closure type per call, so the registrations cannot be a loop over a shared
  table without a macro, and the table would then live in the binary where no
  lib test can read it.)
