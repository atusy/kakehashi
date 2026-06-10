# Query Execution Protocol

**Related Decisions**:
- [node-reference-protocol](node-reference-protocol.md) — Node identity, `NodeInfo`, and the `kakehashi/node/*` accessor family this protocol composes with
- [lazy-node-identity-tracking](lazy-node-identity-tracking.md) — Underlying ULID identity tracking reused to make query results addressable

## Context and Problem Statement

node-reference-protocol opened kakehashi as a platform for syntax-aware extensions by exposing tree-sitter's [`Node`](https://docs.rs/tree-sitter/latest/tree_sitter/struct.Node.html) API one-for-one. That family lets a client identify a node and walk the tree, but it deliberately stops at the node boundary: there is **no way to run a tree-sitter query** against a document from the client side.

Queries are tree-sitter's headline capability. The whole point of an `.scm` query is to declare — per language — *which* structural shapes matter and *which* sub-spans to extract, without the client hard-coding grammar knowledge. The motivating consumer is a [nvim-treesitter-context](https://github.com/nvim-treesitter/nvim-treesitter-context)-style "sticky context" feature: given the cursor, find the enclosing context nodes (`@context`) and the range each one's header occupies (`@context.start` / `@context.end` / `@context.final`), then render them in a floating window. The same primitive also powers structural search, symbol extraction, fold-range computation, and lint-like queries.

This cannot be reconstructed from the `kakehashi/node/*` primitives. Reproducing it client-side would mean re-implementing tree-sitter's query engine **and** its predicate evaluation (`#eq?`, `#match?`, `#any-of?`, plus the Neovim-flavored `#lua-match?`, `#has-ancestor?`, …) in the client — exactly the "don't bundle tree-sitter on the client" burden node-reference-protocol set out to remove. Walking the tree node-by-node and matching kinds by hand also discards the declarative, per-language `.scm` assets the ecosystem already maintains.

Kakehashi already runs queries internally for semantic tokens and injection discovery, and already evaluates predicates. The gap is purely that this power is not reachable over the wire.

## Decision Drivers

* **Reuse, don't reinvent**: lean on the existing query loader/compiler (`src/language/query_loader.rs`), predicate evaluator (`src/language/query_predicates.rs::filter_captures`), ULID minting (`mint_node_info`), and coordinate conversion (`PositionMapper`).
* **Round-trip economy**: query results are inherently bulk. A consumer that gets N matches must not then make N follow-up requests to learn each capture's range — the N+1 cost node-reference-protocol explicitly flagged as its weakness.
* **Composes with node-reference-protocol**: every captured node should carry a trackable `NodeInfo` so clients can hand it straight back to `kakehashi/node/*`.
* **Client-supplied queries**: the consumer's `.scm` assets (e.g. `context.scm`) live on the client, not in kakehashi. The method must accept an arbitrary query string, not only kakehashi's bundled queries.
* **Faithful predicate semantics**: built-in and custom predicates must be evaluated server-side so clients see the same matches tree-sitter/Neovim would.
* **Incremental scope**: ship the single-language (host) case first — it already covers the overwhelming majority of context/search usage — and defer cross-injection querying, mirroring node-reference-protocol's PR-1 (host) → PR-4 (injection) progression.

## Decision Outcome

**Chosen approach**: Introduce a single custom LSP method, `kakehashi/query`, that compiles a client-supplied query string against the **host document's** language grammar, runs it over the whole tree, evaluates predicates server-side, and returns the matches — grouped by match, each capture carrying a trackable `NodeInfo` **and** its LSP `Range` inline.

### Method

| Method | Input | Output | Purpose |
|---|---|---|---|
| `kakehashi/query` | `{ textDocument, query, matchLimit? }` | `QueryResult \| null` | Run a tree-sitter query over the document and return its matches |

### Request shape

```typescript
type QueryParams = {
  textDocument: TextDocumentIdentifier;
  query: string;          // tree-sitter S-expression query (e.g. the body of a context.scm)
  matchLimit?: number;    // optional cap on matches returned (default: server-defined safety cap)
};
```

### Result shape

```typescript
type QueryResult = {
  matches: QueryMatch[];
  skipped: SkippedPattern[];   // patterns dropped by tolerant compilation (see below)
};

type QueryMatch = {
  patternIndex: number;        // which pattern in the query produced this match
  captures: QueryCapture[];
};

type QueryCapture = {
  name: string;                // capture name without '@', e.g. "context", "context.end"
  node: NodeInfo;              // { id, kind } — trackable via kakehashi/node/*
  range: Range;                // LSP Range (UTF-16), inline to avoid N+1
};

type SkippedPattern = {
  patternIndex: number;
  reason: string;
};
```

`NodeInfo` and `Range` are exactly the node-reference-protocol types. Returning both per capture is the deliberate N+1 fix: the bulk nature of a query makes inline ranges the correct trade-off, unlike the per-node accessors where node-reference-protocol kept `range` behind its own endpoint.

### Grouping by match (not a flat capture list)

Captures are returned **grouped under their match**, because correlated captures within one pattern carry the semantics. In a context query, `@context` and its sibling `@context.end` belong to the same match; a flat capture stream would lose which `.end` bounds which `@context`. `patternIndex` is included so clients that branch on pattern identity (common in `context.scm`) can do so.

### Predicate evaluation

* **Built-in text predicates** (`#eq?`, `#match?`, `#any-of?`, …) are applied automatically by tree-sitter's `QueryCursor::matches()` (0.26+).
* **Custom/general predicates** (`#lua-match?`, `#not-lua-match?`, `#contains?`, `#has-parent?`, `#has-ancestor?`, and their negations) are applied via the existing `filter_captures`/`check_predicate` path, so query results match what semantic-token highlighting and Neovim would compute. Unknown predicates are permissively ignored (logged), matching current behavior.

### Tolerant compilation

Compilation reuses `query_loader::parse_query`'s tolerant path: a query whose individual patterns are valid but some reference symbols absent from this grammar yields the matches from the valid patterns plus a `skipped` list describing the dropped ones, rather than failing wholesale. This matters for shared `.scm` files written against a grammar superset. A query that cannot be parsed at all (true syntax error, no isolable patterns) is a client error and is returned as a JSON-RPC `InvalidParams` error — distinct from `null`.

### Null vs. error semantics

* `null` — the document is unresolvable for the **same** reasons as node-reference-protocol: unknown URI, document not yet parsed, no host language detected. This keeps the "not currently valid" signal uniform across the platform.
* JSON-RPC `InvalidParams` error — the `query` string itself is unparseable. A malformed query is an actionable client bug; collapsing it to `null`/empty would silently hide it.

### Scope: host layer only (v1)

`kakehashi/query` v1 compiles and runs the query against the **host** tree. Injection-layer querying is deferred (see Considered Options) because a query string is compiled against one specific grammar, so cross-language injection querying additionally requires the client to learn each layer's language — a separate capability. Host-layer querying already serves single-language buffers (Rust, Python, Lua, …), which is the bulk of context/search usage.

## Considered Options

### A. Generic `kakehashi/query` vs. a purpose-built `kakehashi/context`

A fat, context-specific endpoint would walk the ancestor chain server-side and return only the final context ranges, leaving the client to render. Rejected for v1: it bakes one consumer's algorithm into the protocol, and it still has to own the per-language `context.scm` assets (which language layer? whose copy?). The generic query method follows node-reference-protocol's "minimal composable primitives" driver — context becomes one client-side recipe on top of it (run the context query once, intersect captures with the cursor's ancestor chain obtained via `kakehashi/node` + `parent`). A purpose-built endpoint remains a possible future addition if profiling shows the round-trips or the client-side ancestor walk dominate.

### B. Arbitrary query string vs. a named-query catalog

Restricting clients to queries kakehashi ships would make the method useless for the motivating consumer, whose `context.scm` lives in the plugin. Accepting an arbitrary string is required. (A named-query convenience could be layered on later without changing this decision.)

### C. Grouped matches vs. flat captures

A flat `{name, node, range}[]` is simpler but cannot express intra-match capture correlation (`@context` ↔ `@context.end`). Grouping is required for the motivating use case and is strictly more expressive; clients that don't care can flatten trivially.

### D. Inline `range` vs. `NodeInfo`-only (force follow-up calls)

Returning only `NodeInfo` per capture would keep the wire shape minimal and consistent with the per-node accessors, but force an N+1 storm (`kakehashi/node/range` per capture) for the inherently-bulk query result. Inline ranges win here precisely because the bulk context inverts the trade-off node-reference-protocol made for single-node accessors.

### E. Whole-tree run vs. node-scoped run (`scope: {id}`)

nvim-treesitter-context runs the query per ancestor node with `max_start_depth = 0`. The equivalent over the wire — a `scope: {id}` that restricts the query to a subtree — was considered but deferred: running once over the whole tree and letting the client filter by the cursor's ancestor chain needs only one round-trip and no new resolution path. A `scope` parameter remains a clean future extension (resolve the id via the existing `with_node_by_id`, run the cursor on that node) if a consumer needs subtree-scoped queries.

### F. Host-only vs. injection-aware querying (v1)

Running the query across all injection layers (tagging each match with its layer) would serve Markdown-with-embedded-code context out of the box. Deferred because a query string is grammar-specific: a single call cannot serve both the Markdown and the Python layers, so injection support also needs a way for the client to discover each layer's language (a `kakehashi/tree/layers`-style capability). Sequencing host-first keeps v1 small and matches the node-reference-protocol PR-1 → PR-4 rollout. The `injection` selector name is reserved for that follow-up.

## Consequences

### Positive

* Unlocks tree-sitter's headline capability over the wire: structural search, symbol/fold extraction, and the motivating sticky-context feature, all without bundling tree-sitter or grammars on the client.
* Bulk results with inline ranges eliminate the N+1 round-trips that the per-node accessors would otherwise force on a query consumer.
* Captures return trackable `NodeInfo`, so query output flows directly back into `kakehashi/node/*` — the platform composes.
* Almost entirely built from existing infrastructure (loader, predicate filter, minter, position mapper), keeping the new surface thin.

### Negative

* v1 cannot answer queries inside injected languages (e.g. a Python block in Markdown) — the common cross-language context case waits on a follow-up.
* Inline `range` per capture makes the wire payload larger than a node-only shape; large queries over large files can produce sizable responses (mitigated by `matchLimit`).
* Accepting arbitrary client query strings means compilation cost and error handling are now part of the request path (mitigated by reusing tolerant `parse_query` and by query-compilation caching where the same string recurs — a possible optimization, not required for v1).

### Neutral

* Predicate semantics are inherited from the existing evaluator, so query results match semantic-token behavior — including its permissive handling of unknown predicates.
* `skipped` surfaces partial-compilation outcomes explicitly rather than hiding them, consistent with how kakehashi treats its own bundled queries.
