# Lexical Name Resolution

**Related Decisions**:
- [captures-protocol](captures-protocol.md) — query asset resolution (`queries/<lang>/<kind>.scm` across `searchPaths`), tolerant compilation, and the static `#set!` metadata conventions this spec builds on
- [cross-layer-aggregation](cross-layer-aggregation.md) — how per-layer native results combine with bridge results into one LSP response
- [language-server-bridge](language-server-bridge.md) — the high-accuracy delegation path this feature complements, never competes with
- [language-server-bridge-virtual-document-model](language-server-bridge-virtual-document-model.md) — the concatenated-region model a future cross-region resolution mode would align with

## Context

kakehashi delegates definition/references/hover to external language servers
per injected layer (language-server-bridge). That is the right accuracy story,
but it leaves a gap: layers whose language has **no bridge server configured**
(or none installed on the user's machine) get nothing, even though the
tree-sitter tree already contains enough structure to answer most
within-document lookups. A native, tree-sitter-only resolver can serve
`textDocument/definition`, `references`, and `documentHighlight` for those
layers — best-effort by design, since the bridge remains the 100%-accuracy
path.

Two constraints shape the design:

1. **kakehashi must contain no language knowledge.** "Python class bodies are
   invisible to nested functions", "PHP functions don't capture outer locals",
   "Lua's `local x = x` reads the outer `x`" — none of this may live in Rust.
   The engine implements a universal lexical-scoping model; everything
   language-specific is declared in per-language query assets, exactly as
   highlights/injections already work.
2. **Wrong answers are worse than no answers.** A native result that
   contradicts a bridge result (or plain reality) erodes trust in both. The
   resolver must be able to say "unresolved" and stay silent.

The obvious prior art is nvim-treesitter's `locals.scm`
(`@local.scope` / `@local.definition.<kind>` / `@local.reference` +
`(#set! definition.<kind>.scope ...)`), but it is a dead end:

- nvim-treesitter (main) itself **no longer uses locals.scm at all**; the
  files ship for "limited backwards compatibility" and receive no maintenance
  pressure toward correctness.
- The vocabulary cannot express **name visibility start** (hoisting vs.
  declare-before-use vs. Lua's `local x = x`), **namespaces** (a type and a
  variable with the same name collide on text equality), or **scope-inheritance
  control** (the Python-class and PHP-function rules above).
- References are a blanket `(identifier) @local.reference` with text-equality
  matching and no way to constrain what they may bind to.

A compatibility-superset was considered and rejected (see Considered Options);
this record defines a kakehashi-owned spec instead.

Current code state: `QueryKind::Locals` exists end-to-end (config inference,
coordinator loading, `QueryStore` storage, auto-install via `QUERY_FILES`)
but **no feature consumes it** — `QueryStore::locals_query()` has no callers
outside the store. The slot is dead weight from an earlier intent, and its
installed content is ecosystem `locals.scm` files written against the
nvim-treesitter vocabulary.

## Decision

Define a new query kind, **`bindings.scm`**, with a kakehashi-owned capture
vocabulary, and implement a **generic lexical resolution engine** that knows
only the spec below — never a language. Languages gain native
definition/references support by dropping a `queries/<lang>/bindings.scm`
into a search path; resolution accuracy is improved per language by editing
that asset, never by editing Rust.

The existing `QueryKind::Locals` pipeline (loading, storage, auto-install of
`locals.scm`) is **removed** in the same change — it is consumerless today,
and its semantics are not ours to define.

### Capture vocabulary

| Capture | Meaning |
|---|---|
| `@scope` | A node that opens a lexical scope. The layer's document root is always an implicit root scope. |
| `@definition` / `@definition.<label>` | A name-introducing node (the identifier itself, not the whole declaration). The optional `<label>` is an **opaque string**: the engine attaches no semantics to it. It serves as a property-targeting key within a pattern (below) and is surfaced in results for future use (e.g. `SymbolKind` mapping). |
| `@reference` / `@reference.<label>` | A name-using node. The blanket form `(identifier) @reference` is expected and supported: any node also captured as a definition in the same layer is automatically excluded from references. |

### Properties

All language semantics are declared with `#set!` (static, parsed once per
pattern — the same machinery captures-protocol documents). A property key may
be prefixed with a capture's full name suffix to target one capture in a
multi-capture pattern: bare `definition.visibility` applies to every
`@definition*` capture in the pattern; `definition.parameter.visibility`
applies only to `@definition.parameter`.

| Property | Values | Declares |
|---|---|---|
| `definition.scope` | `local` (default) / `parent` / `global` | Which scope the binding registers in: the innermost enclosing `@scope`, its parent, or the layer root. `parent` expresses "a function's name belongs outside the function" when one pattern captures both the scope and the name. |
| `definition.visibility` | `scope` (default) / `after` | When the binding becomes visible. `scope` = the whole scope (function hoisting, Python assignments). `after` = from the **end byte of the pattern's match** onward — so in Lua's `local x = x` the right-hand `x` precedes visibility and correctly resolves outward, while `local function f` patterns (where `f` is visible in its own body) simply keep `scope`. |
| `definition.namespace` | string, default `default` | The binding's namespace. |
| `reference.namespace` | space-separated strings, default `default` | Namespaces this reference may bind to, searched in order within each scope. A type-position reference declares `"type"`; an ambiguous-position reference declares `"type default"`. Matching is by equality — an unannotated reference does **not** match an annotated definition, which is the conservative direction (silence over a wrong answer). |
| `scope.inherits` | `true` (default) / `false` | When `false`, lookups from inside this scope (and everything nested in it) stop here — they never see outer bindings. Expresses PHP-style functions that do not capture enclosing locals. |
| `scope.visible-to-nested` | `true` (default) / `false` | When `false`, this scope's bindings are skipped when the lookup walk arrives **from a nested scope**; references directly in the scope still see them. Expresses Python class bodies (methods and comprehensions inside the class do not see class-level names; statements in the body do). |

### Resolution algorithm (the engine's entire language model)

Per layer, per parsed version:

1. Run the layer language's `bindings.scm` over the layer tree. Build the
   scope tree from `@scope` captures (implicit root = the layer), nested by
   node containment.
2. Register each `@definition` as a binding `(name text, namespace, label,
   visibility start)` in its scope after applying the `definition.scope`
   lift. `visibility start` is the scope start for `scope` visibility, the
   pattern-match end byte for `after`.
3. For a `@reference` with name `N`, namespaces `NS`, at position `P`: walk
   scopes innermost → outermost. In each scope, for each namespace in `NS`
   order, the candidates are bindings named `N` in that namespace. Pick the
   candidate with the **latest visibility start that does not exceed `P`**
   (natural shadowing and re-binding); if none precedes `P` but a
   `scope`-visibility candidate exists, pick the first one (hoisting). Skip a
   scope's bindings when it has `visible-to-nested false` and is not the
   innermost scope containing `P`; stop the walk entirely after a scope with
   `inherits false`.
4. No candidate anywhere → the reference is **unresolved**. Unresolved is a
   first-class outcome, not an error.

Everything else — which nodes are scopes, what hoists, what shadows what —
is the query author's statement about the language, not the engine's.

### Miss policy: silence

When the cursor's identifier is unresolved (or not captured at all), the
native resolver contributes **nothing** to the response and the normal
bridge/aggregation path decides the answer (cross-layer-aggregation). There
is no fuzzy fallback to same-name text search: a guessed location shown next
to (or instead of) a bridge server's correct one is exactly the trust-eroding
outcome the second design constraint forbids.

### LSP feature mapping

| Feature | Native behavior |
|---|---|
| `textDocument/definition` | Resolved binding → its definition node's range. On a definition node itself → that node. |
| `textDocument/references` | All references in the layer that resolve to the same binding; the definition included per `includeDeclaration`. |
| `textDocument/documentHighlight` | Same set as references, kind `Text`. (`Read`/`Write` distinction would require knowing which syntactic positions write — expressible later as a `reference.write` property, not as engine knowledge.) |
| `textDocument/rename` | **Included, best-effort.** Resolved binding → a `WorkspaceEdit` renaming the definition node and every reference resolving to it, layer-confined by construction. `prepareRename` answers only when the cursor's identifier resolves natively; otherwise it returns nothing and the bridge/aggregation path owns the request — so the native resolver never offers a rename it cannot ground. The residual risk is accepted and documented: an identifier the query failed to capture (or that binds dynamically) survives the rename, softening rename's all-or-nothing contract into best-effort — the same accuracy posture as every other feature here. |

How a native answer and a bridge answer for the same request combine (order,
dedup, precedence) is governed by cross-layer-aggregation, not duplicated
here; the native resolver is one more per-layer producer.

### Layering

Scope trees are **per injection layer**: each region's tree gets its own
root scope, and resolution never crosses layer boundaries. Cross-region
resolution for same-language regions (two Python blocks in one Markdown
document referencing each other) is deliberately out of scope for v1 — if
added, it must align with the concatenated view of
language-server-bridge-virtual-document-model rather than invent a second
region-joining model.

### Out of scope, permanently

Member access (`a.b`), type-based method resolution, and cross-file
navigation are not lexical problems; they are the bridge's domain. The spec
intentionally has no vocabulary for them, so query authors are not tempted
to approximate them badly. Dynamic scoping is likewise unresolvable
statically, but unlike the above it has a sanctioned in-spec approximation
— `definition.scope "global"` — discussed under Considered Options.

## Considered Options

### A. Adopt the nvim-treesitter `locals.scm` vocabulary (compatibility superset)

The ~200 existing ecosystem files would work day one. Rejected: the files
are explicitly legacy (the plugin itself no longer reads them), their
vocabulary cannot carry visibility/namespace/inheritance semantics without
contortions, and compatibility would freeze kakehashi to a spec whose
maintenance pressure is zero. Decided with the explicit position that
compatibility is a non-goal.

### B. New vocabulary under the old `locals.scm` filename

Rejected on a concrete failure mode: the install pipeline fetches ecosystem
`locals.scm` files, and `searchPaths` commonly contain nvim-treesitter
runtime directories. First-hit-wins resolution would load a file in the old
vocabulary, produce zero captures, and the feature would be **silently dead
per language** with no diagnosable error. A new kind name makes "no asset"
explicit and leaves ecosystem files untouched where they lie.

### C. tree-sitter upstream locals spec (`@local.scope`/`@local.definition`/`@local.reference` + `local.scope-inherits`)

Smaller than nvim-treesitter's and the only prior art with inheritance
control. Rejected as a base — no namespaces, no visibility start, no
definition-scope lift — but its `scope-inherits` concept is adopted in
spirit as `scope.inherits`.

### D. `tags.scm`-style flat definition/reference pairs (GitHub code-nav)

No scope tree at all; matching is document-global by text. That is precisely
the fuzzy baseline this decision exists to beat. Rejected.

### E. Per-language resolution logic in Rust

Highest possible native accuracy (real Python scoping rules, etc.).
Rejected outright by the first design constraint: language knowledge lives
in query assets so that adding or fixing a language never touches the
engine, and so the engine's test surface is the property semantics alone.

### F. Concatenated virtual-document scope trees in v1

Running resolution over the same concatenated region view the bridge's
virtual documents use would give cross-block resolution in Markdown for
free. Deferred, not rejected: per-layer trees are strictly simpler, and the
concatenation model carries its own offset-mapping machinery that should be
shared, not duplicated, when this is attempted.

### G. Fuzzy fallback (same-name text search) on unresolved references

More answers, lower trust. Rejected; see Miss policy. The bridge is the
designated fallback.

### H. Dynamic scope resolution (Bash variables, Emacs Lisp `defvar`/special variables, Perl `local`)

Under dynamic scoping a reference binds to whichever **caller** most
recently bound the name — a property of the runtime call stack, not of the
program text. Resolving it statically requires interprocedural call-graph
and data-flow analysis, which is approximate even with full language
semantics in hand, contradicts the no-language-knowledge constraint
outright, and crosses file boundaries the moment call chains do. Rejected
as **impossible in this architecture**, not merely deferred — no future
property vocabulary fixes this, because the missing input is runtime
behavior, not query expressiveness.

The practical approximation needs no new vocabulary: declare
dynamically-scoped definitions with `definition.scope "global"`, registering
them at the layer root so every reference in the document resolves to the
textual definition site. That is what users actually want from
goto-definition on a Bash variable or a `defvar` — jump to where it is
introduced, not to a runtime frame — and it is the same flattening every
static navigation tool (ctags, GitHub code-nav) applies to such languages.
What this approximation gets wrong is call-path-dependent shadowing
(two functions `local`-binding the same name before a shared callee reads
it); those references resolve to the global site instead of staying
unresolved, an accepted deviation from the strict silence-over-guessing
posture because the definition *site* shown is still textually real.

## Consequences

### Positive

* Languages without a configured bridge server get definition / references /
  documentHighlight from the tree-sitter tree alone — and the path to better
  accuracy is editing a `.scm` asset, with no Rust release.
* The engine's correctness surface is small and language-free: scope-tree
  construction plus six property semantics. It can be tested exhaustively
  with synthetic fixtures, while per-language assets are validated separately
  with fixture documents and expected definition↔reference pairs — adding a
  language adds no Rust tests.
* The dead `QueryKind::Locals` pipeline is retired instead of accumulating
  semantics by accident.
* Query loading reuses the proven captures-protocol machinery wholesale:
  `searchPaths` resolution, `; inherits:`, tolerant per-pattern compilation,
  static `#set!` parsing.

### Negative

* Every supported language needs a hand-written `bindings.scm`; nothing is
  inherited from the ecosystem. Coverage grows language by language, starting
  from zero.
* Authoring requires understanding the property semantics, which are richer
  (and stricter — equality-matched namespaces) than nvim-treesitter's
  forgiving legacy spec. Misannotation degrades to silence, which is safe but
  can read as "feature doesn't work".
* Accuracy is bounded by lexical scoping: dynamic constructs (Python
  `globals()`, Lua `_ENV`, JS `with`) and anything member- or type-shaped
  stay unresolved by design.
* Native rename is best-effort, not all-or-nothing: occurrences the query
  fails to capture survive a rename and must be found by the user (or a
  bridge server). The `prepareRename` gate bounds *when* a rename is offered,
  not *how complete* it is.

### Neutral

* `@definition` labels are opaque today; mapping them to LSP `SymbolKind`
  (for a future `documentSymbol`) is a separate decision.
* `reference.write` (for `documentHighlight` Read/Write kinds) is a
  deliberate later phase on the same vocabulary.
* Whether `bindings.scm` is added to the auto-install `QUERY_FILES` set is
  an install-source question (there is no upstream corpus to fetch — assets
  are kakehashi-authored), tracked with the install pipeline, not here.

## Decision–Implementation Gap

Nothing is implemented: no `bindings.scm` kind, no engine, and
`QueryKind::Locals` (slated for removal above) still loads and installs
`locals.scm` today. This record precedes implementation.
