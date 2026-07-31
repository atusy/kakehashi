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
   preceded it on the wire. An `ingress_order` test pins that old names reach
   the gate already rewritten, so the ordering dependency is not merely a
   comment.
2. **The mapping is an explicit allowlist, not a prefix rule.** A blanket
   `kakehashi/` → `kakehashi/textDocument/` rewrite would corrupt
   `internal/effectiveConfiguration`, and would invent deprecated spellings for
   methods that never had one. The list is frozen at the rename: methods added
   afterwards never had an old name to alias.
3. **The warning is log-only.** The natural alternative,
   `window/showMessage` as the config-key deprecations use, would mean up to 41
   popups in a session; the middleware also has no `Client` handle, sitting
   above the service that owns one. A client author reads the LSP log.

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
  divergence is correct.
